//! macOS 平台屏幕捕获实现
//!
//! 使用 `screenshots` crate，底层走 CGDisplayCreateImage（系统原生 API）。
//! 首次使用时 macOS 会弹「屏幕录制」权限提示，需要在 系统设置 → 隐私与安全性 →
//! 屏幕录制 中授权。

use screenshots::Screen;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::AppResult;

/// macOS 平台屏幕捕获器
pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        Self
    }
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        let screens = Screen::all().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into()))?;

        let image = screen.capture().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into(), // screenshots crate 输出 RGBA（通过 Into<Vec<u8>>）
        })
    }

    /// 滚动截屏暂不支持 macOS（区域截屏可行，但自动滚动注入仅实现于 Linux/X11）
    fn capture_area(&self, _x: i32, _y: i32, _w: u32, _h: u32) -> AppResult<CapturedFrame> {
        Err(crate::error::AppError::Window(
            "滚动截屏暂不支持 macOS".into(),
        ))
    }

    fn list_displays(&self) -> Vec<DisplayInfo> {
        Screen::all()
            .map(|screens| {
                screens
                    .into_iter()
                    .enumerate()
                    .map(|(id, s)| DisplayInfo {
                        id: id as u32,
                        width: s.display_info.width,
                        height: s.display_info.height,
                        scale_factor: s.display_info.scale_factor,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
