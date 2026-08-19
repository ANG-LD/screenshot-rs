//! Linux 平台屏幕捕获实现（x11rb 直连 + 缓存连接 + XShm 共享内存加速）
//!
//! 使用 x11rb 直接调用 GetImage，连接在首次访问时创建并缓存，避免每次截图
//! 都重新建立 X11 连接的开销（`screenshots` crate 每次都会打开新连接）。
//!
//! X11 GetImage Z_PIXMAP 在 x86-64 上返回 BGRA 字节序（B 在低字节），
//! 转换为 RGBA 后存入 CapturedFrame。
//!
//! XShm：让 X 服务器把屏幕像素直接写进共享内存，免去 8MB 像素经 socket
//! 传输（GetImage 全屏约 190ms 主要是这个传输开销）。初始化失败或单次
//! 捕获失败都会自动回退到 GetImage 路径，保证可用性。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as ShmExt;
use x11rb::protocol::xproto::{query_extension, ConnectionExt as XprotoExt, ImageFormat};
use x11rb::xcb_ffi::XCBConnection;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::{AppError, AppResult};

/// XShm 共享内存段状态。地址用 `usize` 存储以保持 `Send`（CachedConn 放进
/// static Mutex 需要），使用时再转回指针。
struct ShmState {
    /// 服务器端 segment 资源 id（`generate_id` 生成，XShmAttach 用）
    shmseg: u32,
    /// mmap 起始地址
    addr: usize,
    /// 段大小（字节），按全屏尺寸分配，覆盖任意捕获区域
    size: usize,
}

struct CachedConn {
    conn: XCBConnection,
    root: u32,
    width: u32,
    height: u32,
    shm: Option<ShmState>,
}

impl CachedConn {
    fn open() -> AppResult<Self> {
        let (conn, screen_num) = XCBConnection::connect(None)
            .map_err(|e| AppError::Capture(format!("X 连接失败: {e}")))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| AppError::Window("找不到 X 屏幕".into()))?;
        let w = screen.width_in_pixels as u32;
        let h = screen.height_in_pixels as u32;
        let shm = init_shm(&conn, w, h);
        if shm.is_none() {
            tracing::debug!("[capture] XShm 不可用，使用 GetImage 传输路径");
        }
        Ok(Self {
            root: screen.root,
            width: w,
            height: h,
            conn,
            shm,
        })
    }
}

/// 初始化 XShm：检测 MIT-SHM 扩展 + 创建共享内存段 + attach 到 X 连接。
/// 任一步失败返回 None（调用方回退 GetImage）。
fn init_shm(conn: &XCBConnection, w: u32, h: u32) -> Option<ShmState> {
    // 1) 扩展检测（x11rb 无便捷方法，直接用 QueryExtension 请求）
    let ext = query_extension(conn, b"MIT-SHM").ok()?.reply().ok()?;
    if !ext.present {
        return None;
    }
    // 2) SysV 共享内存段：尺寸 = 全屏像素 + 余量（覆盖任意 capture_area 区域）
    let size = (w as usize) * (h as usize) * 4 + 4096;
    let shmid = unsafe {
        libc::shmget(
            libc::IPC_PRIVATE,
            size as libc::size_t,
            libc::IPC_CREAT | 0o600,
        )
    };
    if shmid < 0 {
        return None;
    }
    // 创建即标记 IPC_RMID：最后一个 attach 释放后内核回收，进程退出不泄漏
    unsafe {
        libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
    }
    let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
    if addr == libc::MAP_FAILED {
        return None;
    }
    // 3) 服务器端 attach（read_only=false：服务器要写入共享内存）。
    //    shmseg 是客户端生成的资源 id（xcb_generate_id 语义），shmid 是 SysV id
    let seg = conn.generate_id().ok()?;
    if conn.shm_attach(seg, shmid as u32, false).is_err() {
        return None;
    }
    Some(ShmState {
        shmseg: seg,
        addr: addr as usize,
        size,
    })
}

/// 用 XShm 捕获区域像素；失败返回 None（调用方回退 GetImage）。
fn get_image_shm(guard: &CachedConn, x: i32, y: i32, w: u32, h: u32) -> Option<CapturedFrame> {
    let shm = guard.shm.as_ref()?;
    let w = w.max(1);
    let h = h.max(1);
    let len = (w as usize) * (h as usize) * 4;
    if len > shm.size {
        return None; // 区域超过段大小（异常），回退
    }
    let cookie = guard
        .conn
        .shm_get_image(
            guard.root,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            u32::MAX,
            ImageFormat::Z_PIXMAP.into(),
            shm.shmseg,
            0,
        )
        .ok()?;
    // reply 返回时服务器已把像素写入共享内存（同一连接上请求按序处理）
    let _reply = cookie.reply().ok()?;

    let mut rgba = Vec::with_capacity(len);
    unsafe {
        std::ptr::copy_nonoverlapping(shm.addr as *const u8, rgba.as_mut_ptr(), len);
        rgba.set_len(len);
    }
    bgra_to_rgba(&mut rgba);
    Some(CapturedFrame {
        width: w,
        height: h,
        pixels: rgba,
    })
}

/// BGRA → RGBA 并强制 A=255（X11 ZPixmap 的 alpha 通道无意义）。
/// u32 批量位运算，一次处理 4 字节：迭代数是逐字节 swap 的 1/4，debug 未优化
/// 构建下也快得多。
fn bgra_to_rgba(pixels: &mut [u8]) {
    debug_assert_eq!(pixels.len() % 4, 0);
    if (pixels.as_ptr() as usize).is_multiple_of(4) {
        let words: &mut [u32] = unsafe {
            std::slice::from_raw_parts_mut(pixels.as_mut_ptr() as *mut u32, pixels.len() / 4)
        };
        for v in words {
            // 输入 BGRA(LE u32): B | G<<8 | R<<16 | A<<24
            // → 输出 RGBA: R | G<<8 | B<<16 | 255<<24
            *v = ((*v & 0x0000_00FF) << 16)
                | (*v & 0x0000_FF00)
                | ((*v & 0x00FF_0000) >> 16)
                | 0xFF00_0000;
        }
    } else {
        // 慢路径：缓冲区未 4 字节对齐时退回避让（罕见）
        for c in pixels.chunks_exact_mut(4) {
            c.swap(0, 2);
            c[3] = 255;
        }
    }
}

fn get_image(x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame> {
    let guard = CACHED
        .lock()
        .map_err(|e| AppError::Capture(format!("CachedConn 锁中毒: {e}")))?;

    // 优先 XShm：服务器直写共享内存，省掉 8MB 像素的 socket 传输
    if let Some(frame) = get_image_shm(&guard, x, y, w, h) {
        return Ok(frame);
    }

    let c = &guard.conn;
    let w = w.max(1);
    let h = h.max(1);

    let cookie = c
        .get_image(
            ImageFormat::Z_PIXMAP,
            guard.root,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            u32::MAX,
        )
        .map_err(|e| AppError::Capture(format!("GetImage 失败: {e}")))?;

    let reply = cookie
        .reply()
        .map_err(|e| AppError::Capture(format!("GetImage 响应失败: {e}")))?;

    let raw = reply.data;
    let expected = (w as usize) * (h as usize) * 4;
    if raw.len() != expected {
        tracing::warn!(
            "[capture] size mismatch: {}x{} depth={} got={} expected={}",
            w, h, reply.depth, raw.len(), expected
        );
    }

    // 诊断用原始帧转储：默认关闭（1080p PNG 编码约 1s，会拖慢首次截图）。
    // 需要时设置环境变量 SCREENSHOT_RS_DEBUG_DUMP=1 再启动应用，首次捕获会
    // 保存 /tmp/capture_raw_bgra.png（BGRA 原始字节，共 1 次）。
    if debug_dump_enabled() {
        static DEBUG_DUMP: AtomicBool = AtomicBool::new(true);
        if DEBUG_DUMP.swap(false, Ordering::Relaxed) {
            if let Some(img) = image::RgbaImage::from_raw(w, h, raw.clone()) {
                let _ = img.save("/tmp/capture_raw_bgra.png");
                tracing::info!("[capture] saved /tmp/capture_raw_bgra.png for debug");
            }
        }
    }

    // BGRA → RGBA（u32 批量位运算 + 强制 A=255）
    let mut rgba = raw;
    bgra_to_rgba(&mut rgba);

    Ok(CapturedFrame {
        width: w,
        height: h,
        pixels: rgba,
    })
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        let (cw, ch) = {
            let guard = CACHED
                .lock()
                .map_err(|e| AppError::Capture(format!("CachedConn 锁中毒: {e}")))?;
            (guard.width, guard.height)
        };
        get_image(0, 0, cw, ch)
    }

    fn capture_area(&self, x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame> {
        get_image(x, y, w, h)
    }

    fn list_displays(&self) -> Vec<DisplayInfo> {
        // 只读宽高，不依赖连接状态；锁中毒时恢复数据而非级联 panic
        let guard = CACHED.lock().unwrap_or_else(|e| {
            tracing::warn!("[capture] CachedConn 锁中毒，恢复宽高数据");
            e.into_inner()
        });
        vec![DisplayInfo {
            id: 0,
            width: guard.width,
            height: guard.height,
            scale_factor: 1.0,
        }]
    }
}

/// 是否启用首次捕获的原始帧转储（环境变量 SCREENSHOT_RS_DEBUG_DUMP=1）。
/// 一次性读取环境变量并缓存，避免每次捕获都查环境。
fn debug_dump_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SCREENSHOT_RS_DEBUG_DUMP")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

static CACHED: LazyLock<Mutex<CachedConn>> =
    LazyLock::new(|| Mutex::new(CachedConn::open().expect("无法连接 X Server")));

pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        LazyLock::force(&CACHED);
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_to_rgba_swaps_and_forces_alpha() {
        let mut px: Vec<u8> = vec![
            0x11, 0x22, 0x33, 0x00, // B=0x11 G=0x22 R=0x33 A=0x00（alpha 强制 255）
            0xAA, 0xBB, 0xCC, 0x12, // 半透明通道也强制 255
            0x00, 0x00, 0x00, 0x00, // 全零
            0xFF, 0x80, 0x40, 0xFF, // 混合值
        ];
        bgra_to_rgba(&mut px);
        assert_eq!(
            px,
            vec![
                0x33, 0x22, 0x11, 0xFF, // RGBA
                0xCC, 0xBB, 0xAA, 0xFF,
                0x00, 0x00, 0x00, 0xFF,
                0x40, 0x80, 0xFF, 0xFF,
            ]
        );
    }
}
