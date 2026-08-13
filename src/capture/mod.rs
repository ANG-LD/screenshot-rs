//! 屏幕捕获模块：定义跨平台 trait 和数据结构。
//!
//! 平台实现见 `windows.rs`、`linux.rs` 和 `macos.rs`。

pub mod linux;
pub mod macos;
pub mod windows;

use crate::error::{AppError, AppResult};

/// 一帧屏幕像素数据（RGBA 格式，每像素 4 字节连续存储）
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>, // RGBA
}

impl CapturedFrame {
    /// 从 (x, y) 坐标开始裁剪 (w, h) 大小的子区域
    ///
    /// 用于在 EDITING 阶段只取选区对应的像素，丢弃不必要的数据。
    pub fn clip_region(&self, x: u32, y: u32, w: u32, h: u32) -> AppResult<CapturedFrame> {
        if x + w > self.width || y + h > self.height {
            return Err(AppError::Window(format!(
                "裁剪区域 ({}x{} @ {},{}) 超出图像尺寸 {}x{}",
                w, h, x, y, self.width, self.height
            )));
        }
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for row in y..(y + h) {
            let start = (row * self.width + x) as usize * 4;
            let end = start + w as usize * 4;
            pixels.extend_from_slice(&self.pixels[start..end]);
        }
        Ok(CapturedFrame {
            width: w,
            height: h,
            pixels,
        })
    }
}

/// 显示器信息（用于多屏支持预留）
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

/// 屏幕捕获 trait：所有平台实现都暴露此接口
pub trait ScreenCapture: Send + Sync {
    /// 捕获主显示器全屏
    fn capture_primary(&self) -> AppResult<CapturedFrame>;

    /// 捕获主显示器上 (x, y) 起 (w, h) 的区域（物理像素，主屏相对坐标）
    ///
    /// 用于滚动截屏：反复抓取同一视口并拼接。越界部分会被底层 clamp，
    /// 返回的实际尺寸可能小于请求值，调用方需校验。
    fn capture_area(&self, x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame>;

    /// 列出所有可用显示器
    fn list_displays(&self) -> Vec<DisplayInfo>;
}

#[cfg(target_os = "windows")]
pub use windows::PlatformScreenCapture;

#[cfg(target_os = "linux")]
pub use linux::PlatformScreenCapture;

#[cfg(target_os = "macos")]
pub use macos::PlatformScreenCapture;

/// 根据当前平台返回默认实现
pub fn platform_capture() -> Box<dyn ScreenCapture> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::PlatformScreenCapture::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::PlatformScreenCapture::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::PlatformScreenCapture::new())
    }
}
