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

use arboard::ImageData;

use crate::capture::CapturedFrame;
use crate::error::{AppError, AppResult};

/// 用帧的宽高与像素切片构建 arboard ImageData。
///
/// `bytes` 借用调用方像素（Cow::Borrowed）：`set_image` 在调用期间同步完成
/// 编码（Linux 编 PNG / Windows 建 DIB），不会在返回后持有数据，因此
/// 成功路径零拷贝。
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

/// 进程级剪贴板服务单例。
///
/// GPUI 覆盖线程的 OCR 复制与主线程的图像写入可能在不同线程调用；arboard 的
/// `Clipboard` 底层是进程内全局单例（各实例共享同一 X11 连接/窗口），因此这里
/// 也用一个全局 `ClipboardService` 保证实例长存、X11 剪贴板所有权不丢。
pub fn global() -> &'static ClipboardService {
    static SVC: OnceLock<ClipboardService> = OnceLock::new();
    SVC.get_or_init(ClipboardService::new)
}