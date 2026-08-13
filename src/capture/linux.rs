//! Linux 平台屏幕捕获实现（x11rb 直连 + 缓存连接）
//!
//! 使用 x11rb 直接调用 GetImage，连接在首次访问时创建并缓存，避免每次截图
//! 都重新建立 X11 连接的开销（`screenshots` crate 每次都会打开新连接）。
//!
//! X11 GetImage Z_PIXMAP 在 x86-64 上返回 BGRA 字节序（B 在低字节），
//! 转换为 RGBA 后存入 CapturedFrame。

use std::sync::{LazyLock, Mutex};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as XprotoExt, ImageFormat};
use x11rb::xcb_ffi::XCBConnection;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::{AppError, AppResult};

struct CachedConn {
    conn: XCBConnection,
    root: u32,
    width: u32,
    height: u32,
}

impl CachedConn {
    fn open() -> AppResult<Self> {
        let (conn, screen_num) = XCBConnection::connect(None)
            .map_err(|e| AppError::Capture(format!("X 连接失败: {e}")))?;
        let screen = conn
            .setup()
            .roots
            .iter()
            .nth(screen_num)
            .ok_or_else(|| AppError::Window("找不到 X 屏幕".into()))?;
        Ok(Self {
            root: screen.root,
            width: screen.width_in_pixels as u32,
            height: screen.height_in_pixels as u32,
            conn,
        })
    }
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

fn get_image(x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame> {
    let guard = CACHED
        .lock()
        .map_err(|e| AppError::Capture(format!("CachedConn 锁中毒: {e}")))?;
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

    // 保存一份原始 BGRA 数据用于诊断（首次捕获时）
    use std::sync::atomic::{AtomicBool, Ordering};
    static DEBUG_DUMP: AtomicBool = AtomicBool::new(true);
    if DEBUG_DUMP.swap(false, Ordering::Relaxed) {
        if let Some(img) = image::RgbaImage::from_raw(w, h, raw.clone()) {
            let _ = img.save("/tmp/capture_raw_bgra.png");
            tracing::info!("[capture] saved /tmp/capture_raw_bgra.png for debug");
        }
    }

    // BGRA → RGBA
    let mut rgba = raw;
    for chunk in rgba.chunks_exact_mut(4) {
        chunk.swap(0, 2);
        chunk[3] = 255;
    }

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
