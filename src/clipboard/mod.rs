//! 系统剪贴板写入服务
//!
//! 使用 `arboard` crate 跨平台抽象。截图完成时调用 `write_frame` 把 RGBA
//! 数据写入剪贴板，粘贴到任意位置（Slack/编辑器/浏览器）都能看到图像。
//!
//! ## 长存 Clipboard 的必要性（X11 平台）
//!
//! 在 X11 上，`arboard::Clipboard` 内部持有一个 X server 连接和它注册的窗口。
//! 当 `Clipboard` 被 drop 时，该窗口被销毁，剪贴板所有权随之释放，
//! 其他应用（gimp、chrome、编辑器等）再来读就只能拿到空。
//!
//! `ClipboardService` 因此必须把 `Clipboard` 实例存为字段，长存于整个进程
//! 生命周期内。Mutex 保护是为了在第一次写入失败后能 lazy 重连。

use std::sync::{Mutex, OnceLock};

#[cfg(not(target_os = "windows"))]
use arboard::ImageData;

#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::{
    DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    },
    Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    Ole::{CF_BITMAP, CF_DIB, CF_DIBV5},
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BITMAPV5HEADER, BI_BITFIELDS, CBM_INIT, CreateCompatibleDC,
    CreateDIBitmap, DeleteDC, DeleteObject, DIB_RGB_COLORS, LCS_GM_IMAGES, RGBQUAD,
};

use crate::capture::CapturedFrame;
use crate::error::{AppError, AppResult};

/// 用帧的宽高与像素切片构建 arboard ImageData。
///
/// `bytes` 借用调用方像素（Cow::Borrowed）：`set_image` 在调用期间同步完成
/// 编码（Linux 编 PNG / Windows 建 DIB），不会在返回后持有数据，因此
/// 成功路径零拷贝。Windows 图像写入走原生多格式，不走 arboard。
#[cfg(not(target_os = "windows"))]
fn image_data<'a>(frame: &CapturedFrame, pixels: &'a [u8]) -> ImageData<'a> {
    ImageData {
        width: frame.width as usize,
        height: frame.height as usize,
        bytes: std::borrow::Cow::Borrowed(pixels),
    }
}

/// 跨平台剪贴板服务
///
/// 持有长生命周期的 `arboard::Clipboard` 实例。`write_frame` 首次调用时会
/// lazy 初始化连接；之后所有写入复用同一连接，确保 X11 上剪贴板所有权不丢。
pub struct ClipboardService {
    /// 长存的 arboard Clipboard。None 表示还没连过。
    inner: Mutex<Option<arboard::Clipboard>>,
}

impl ClipboardService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 把捕获的帧写入剪贴板
    ///
    /// 第一次调用时会 lazy 创建 arboard 连接（允许显示服务暂时不可用，
    /// 服务启动不会因此失败）。后续调用复用同一连接。
    ///
    /// Linux X11/Wayland 偶发"could not be converted to the appropriate format"
    /// —— 通常不是图像数据真的有问题，而是 arboard 内部持有的剪贴板所有权
    /// 被其他应用抢走（用户中途切到其他剪贴板管理工具、或 X server 处理大图
    /// 时连接被 server 端断开）。失败时把连接 drop 重连一次再试，
    /// 避免一次偶发错误让本次截图直接丢。
    pub fn write_frame(&self, frame: &CapturedFrame) -> AppResult<()> {
        #[cfg(target_os = "windows")]
        {
            // Windows 用原生多格式写入（CF_DIBV5 + CF_PNG），不经过 arboard：
            // arboard 在 Windows 只写 CF_DIBV5 单一格式，部分目标应用不接受；
            // 且 arboard Clipboard 有线程亲和性，跨线程共享可能损坏剪贴板状态。
            return write_frame_windows(frame);
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut guard = self
                .inner
                .lock()
                .map_err(|e| AppError::Window(format!("ClipboardService Mutex poisoned: {e}")))?;
            if guard.is_none() {
                *guard = Some(arboard::Clipboard::new().map_err(AppError::Clipboard)?);
            }

            // 校验像素长度与 width*height*4 一致：arboard set_image 内部会
            // 把 RGBA8 编码为 PNG，长度不匹配直接 ConversionFailure，
            // 报出来的错误是"could not be converted to the appropriate format"——
            // 看不出来是长度问题，反而像格式问题，让人误以为是颜色通道顺序。
            let expected = frame.width as usize * frame.height as usize * 4;
            if frame.pixels.len() != expected {
                tracing::error!(
                    "CapturedFrame 长度不一致：width={} height={} pixels.len()={} expected={}",
                    frame.width, frame.height, frame.pixels.len(), expected
                );
                return Err(AppError::Window(format!(
                    "CapturedFrame pixels 长度不匹配：得到 {}，期望 {}",
                    frame.pixels.len(),
                    expected
                )));
            }

            // 按需构建 ImageData：借用帧像素（Cow::Borrowed）。arboard 的 set_image
            // 在调用期间同步编码（Linux 编 PNG / Windows 建 DIB），不会持有数据，
            // 因此成功路径零拷贝；重连重试路径同样只是借用，仍无复制。
            let clipboard = guard
                .as_mut()
                .expect("刚 ensure 完不应为 None");
            match clipboard.set_image(image_data(frame, &frame.pixels)) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!("剪贴板写入失败，重连重试一次：{e}");
                    *guard = None;
                    let mut new_clipboard =
                        arboard::Clipboard::new().map_err(AppError::Clipboard)?;
                    new_clipboard
                        .set_image(image_data(frame, &frame.pixels))
                        .map_err(AppError::Clipboard)?;
                    *guard = Some(new_clipboard);
                    Ok(())
                }
            }
        }
    }

    /// 把文本写入剪贴板（OCR 结果复制用）
    ///
    /// 与 `write_frame` 一样复用长存的 arboard 连接：X11 上 `Clipboard` drop
    /// 即释放剪贴板所有权、粘贴拿到空，所以必须长存实例。失败时重连重试一次。
    pub fn write_text(&self, text: &str) -> AppResult<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| AppError::Window(format!("ClipboardService Mutex poisoned: {e}")))?;
        if guard.is_none() {
            *guard = Some(arboard::Clipboard::new().map_err(AppError::Clipboard)?);
        }
        let clipboard = guard.as_mut().expect("刚 ensure 完不应为 None");
        match clipboard.set_text(text) {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!("剪贴板文本写入失败，重连重试一次：{e}");
                *guard = None;
                let mut new_clipboard = arboard::Clipboard::new().map_err(AppError::Clipboard)?;
                new_clipboard.set_text(text).map_err(AppError::Clipboard)?;
                *guard = Some(new_clipboard);
                Ok(())
            }
        }
    }
}

/// Windows 原生把帧写入剪贴板：一次会话里同时提供 CF_DIBV5 与 CF_PNG，
/// 最大化目标应用的粘贴兼容性。
///
/// - CF_DIBV5（BITMAPV5HEADER + 32bit BGRA，底部向上）：Word / Paint / 编辑器等
///   标准 Windows 应用。
/// - CF_PNG：浏览器 / 聊天工具等多认 PNG。
///
/// arboard 在 Windows 只写 CF_DIBV5 单一格式，部分应用（尤其只认 PNG 或
/// CF_BITMAP 的）粘贴失败；且 arboard 的 Clipboard 有线程亲和性，覆盖线程
/// OCR 复制与主线程图像写入共用实例时可能损坏状态。这里直接走 Win32 原生，
/// 与本进程其它剪贴板操作解耦。
/// 分配一块 HGLOBAL 并写入字节，返回句柄；失败返回 null（内部已释放）。
#[cfg(target_os = "windows")]
unsafe fn global_from_bytes(data: &[u8]) -> *mut core::ffi::c_void {
    let h = GlobalAlloc(GMEM_MOVEABLE, data.len());
    if h.is_null() {
        return h;
    }
    let ptr = GlobalLock(h) as *mut u8;
    if ptr.is_null() {
        GlobalFree(h);
        return std::ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    GlobalUnlock(h);
    h
}

#[cfg(target_os = "windows")]
fn write_frame_windows(frame: &CapturedFrame) -> AppResult<()> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || frame.pixels.len() != w * h * 4 {
        return Err(AppError::Window(format!(
            "CapturedFrame pixels 长度不匹配：得到 {}，期望 {}",
            frame.pixels.len(),
            w * h * 4
        )));
    }

    // RGBA → BGRA，并垂直翻转（DIB 底部向上，与 arboard 一致：MS Word 不接受
    // 负高度顶向下的 DIB）。
    let mut bgra = Vec::with_capacity(w * h * 4);
    for row in (0..h).rev() {
        let base = row * w * 4;
        for i in 0..w {
            let p = base + i * 4;
            bgra.extend_from_slice(&[
                frame.pixels[p + 2],
                frame.pixels[p + 1],
                frame.pixels[p],
                frame.pixels[p + 3],
            ]);
        }
    }

    // 编码 PNG（CF_PNG 用；编码失败不致命，仍有其它格式兜底）。
    // 用 `PngEncoder::write_image` 直接编码**借用**的像素切片，省掉一整帧 clone
    // （原来 `RgbaImage::from_raw` 需要所有权，被迫 `frame.pixels.clone()` 拷贝一整帧）。
    let mut png_buf = Vec::new();
    {
        use image::ImageEncoder;
        use image::codecs::png::PngEncoder;
        let mut cursor = std::io::Cursor::new(&mut png_buf);
        let _ = PngEncoder::new(&mut cursor).write_image(
            &frame.pixels,
            frame.width,
            frame.height,
            image::ExtendedColorType::Rgba8,
        );
    }

    // "PNG" 剪贴板格式 ID 由系统按名称运行时分配，不能硬编码：硬编码 0x8017
    // 在部分系统上与真实 ID 不一致，要找 PNG 的应用（含 arboard 读取路径，它
    // 优先读 PNG）会按注册返回的 ID 找不到我们的数据，回退读 DIBV5。
    // RegisterClipboardFormatW 返回 0 表示失败，此时退化为只提供 DIB。
    let png_format = unsafe {
        let wide: Vec<u16> = "PNG".encode_utf16().chain(std::iter::once(0)).collect();
        RegisterClipboardFormatW(wide.as_ptr())
    };

    unsafe {
        // 1) CF_DIBV5：BITMAPV5HEADER + 32bit BGRA（掩码在头内，GDI 类应用可读）
        #[allow(non_upper_case_globals)]
        const LCS_sRGB: u32 = 0x7352_4742;
        let header_size = std::mem::size_of::<BITMAPV5HEADER>();
        let header = BITMAPV5HEADER {
            bV5Size: header_size as u32,
            bV5Width: frame.width as i32,
            bV5Height: frame.height as i32,
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS,
            bV5SizeImage: (4 * w * h) as u32,
            bV5XPelsPerMeter: 0,
            bV5YPelsPerMeter: 0,
            bV5ClrUsed: 0,
            bV5ClrImportant: 0,
            bV5RedMask: 0x00ff0000,
            bV5GreenMask: 0x0000ff00,
            bV5BlueMask: 0x000000ff,
            bV5AlphaMask: 0xff000000,
            bV5CSType: LCS_sRGB,
            bV5Endpoints: std::mem::zeroed(),
            bV5GammaRed: 0,
            bV5GammaGreen: 0,
            bV5GammaBlue: 0,
            bV5Intent: LCS_GM_IMAGES as u32,
            bV5ProfileData: 0,
            bV5ProfileSize: 0,
            bV5Reserved: 0,
        };
        let mut dibv5_data = Vec::with_capacity(header_size + bgra.len());
        dibv5_data.extend_from_slice(std::slice::from_raw_parts(
            (&header as *const BITMAPV5HEADER).cast::<u8>(),
            header_size,
        ));
        dibv5_data.extend_from_slice(&bgra);

        // 2) CF_DIB：BITMAPINFOHEADER + BI_BITFIELDS + 头后 3 个 R/G/B 掩码 + 像素。
        //    image crate 对 V4/V5 头 + BI_BITFIELDS 有"像素区额外跳过 12 字节"的解析
        //    怪癖，会把 V5 头内掩码当作像素偏移前的数据；改用 Info 头 + 掩码在头后
        //    的经典 DIB 布局，GDI 与 image crate 都能正确解析。
        let bih = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: frame.width as i32,
            biHeight: frame.height as i32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_BITFIELDS,
            biSizeImage: (4 * w * h) as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut dib_data = Vec::with_capacity(40 + 12 + bgra.len());
        dib_data.extend_from_slice(std::slice::from_raw_parts(
            (&bih as *const BITMAPINFOHEADER).cast::<u8>(),
            40,
        ));
        dib_data.extend_from_slice(&0x00ff0000u32.to_le_bytes()); // RedMask
        dib_data.extend_from_slice(&0x0000ff00u32.to_le_bytes()); // GreenMask
        dib_data.extend_from_slice(&0x000000ffu32.to_le_bytes()); // BlueMask
        dib_data.extend_from_slice(&bgra);

        // 3) CF_BITMAP：GDI HBITMAP（最老的通用图像剪贴板格式，部分旧应用只认它）
        let bmi = BITMAPINFO {
            bmiHeader: bih,
            bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }],
        };
        let hdc = CreateCompatibleDC(std::ptr::null_mut());
        let hbmp = CreateDIBitmap(
            hdc,
            &bih,
            CBM_INIT as u32,
            bgra.as_ptr() as *const core::ffi::c_void,
            &bmi,
            DIB_RGB_COLORS,
        );
        DeleteDC(hdc);

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            let code = GetLastError();
            tracing::error!("OpenClipboard 失败，last_error={code}");
            return Err(AppError::Window(format!(
                "OpenClipboard 失败 (error {code})"
            )));
        }
        EmptyClipboard();

        // SetClipboardData 失败时句柄仍归调用方，必须释放（GlobalFree / DeleteObject），
        // 否则泄漏；成功则所有权移交剪贴板。任一格式成功即算成功。
        let mut any_set = false;

        let h = global_from_bytes(&dibv5_data);
        if !h.is_null() {
            if !SetClipboardData(CF_DIBV5 as u32, h as _).is_null() {
                any_set = true;
            } else {
                GlobalFree(h);
            }
        }

        let h = global_from_bytes(&dib_data);
        if !h.is_null() {
            if !SetClipboardData(CF_DIB as u32, h as _).is_null() {
                any_set = true;
            } else {
                GlobalFree(h);
            }
        }

        if !hbmp.is_null() {
            if !SetClipboardData(CF_BITMAP as u32, hbmp as _).is_null() {
                any_set = true;
            } else {
                DeleteObject(hbmp as _);
            }
        }

        if png_format != 0 && !png_buf.is_empty() {
            let h = global_from_bytes(&png_buf);
            if !h.is_null() {
                if !SetClipboardData(png_format, h as _).is_null() {
                    any_set = true;
                } else {
                    GlobalFree(h);
                }
            }
        }

        CloseClipboard();

        if !any_set {
            tracing::error!(
                "剪贴板图像格式写入全部失败：DIBV5/DIB/BITMAP/PNG 均未设置成功"
            );
            return Err(AppError::Window("剪贴板图像格式写入全部失败".into()));
        }
        tracing::info!(
            "剪贴板图像写入成功（{}x{}，DIBV5/DIB/BITMAP/PNG 至少一项）",
            frame.width,
            frame.height
        );
    }
    Ok(())
}

/// 进程级剪贴板服务单例。
///
/// GPUI 覆盖线程的 OCR 复制与主线程的图像写入可能在不同线程调用；arboard 的
/// `Clipboard` 底层是进程内全局单例（各实例共享同一 X11 连接/窗口），因此这里
/// 也用一个全局 `ClipboardService` 保证实例长存、X11 剪贴板所有权不丢。
pub fn global() -> &'static ClipboardService {
    static SVC: OnceLock<ClipboardService> = OnceLock::new();
    SVC.get_or_init(ClipboardService::new)
}