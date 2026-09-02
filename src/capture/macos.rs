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

    /// 取主显示器（`Screen::all()` 顺序不保证主屏在前；多显示器用 `is_primary` 精确定位，
    /// 保证 `capture_primary` 与 `capture_area` 一致地从同一屏幕取样）。
    fn primary_screen(&self) -> AppResult<screenshots::Screen> {
        let screens = Screen::all()
            .map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        let primary = screens.iter().find(|s| s.display_info.is_primary).copied();
        match primary {
            Some(s) => Ok(s),
            None => screens
                .into_iter()
                .next()
                .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into())),
        }
    }
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        let screen = self.primary_screen()?;
        let image = screen.capture().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into(), // screenshots crate 输出 RGBA（通过 Into<Vec<u8>>）
        })
    }

    /// 捕获主显示器上 (x, y) 起 (w, h) 的区域（物理像素，主屏相对坐标）。
    ///
    /// `screenshots` 在 macOS（darwin.rs）把入参当作**points（逻辑坐标）**从显示器原点
    /// 加偏移，并产出 sf×这么大的物理像素图。滚动引擎传的是**物理像素**；Retina 下
    /// 必须**除以 scale_factor**，否则区域放大了 sf 倍、位置偏移 → 滚动拼接错位。
    /// 除以 sf 后返回的帧仍是物理像素，与调用方预期一致。
    fn capture_area(&self, x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame> {
        let screen = self.primary_screen()?;
        let sf = screen.display_info.scale_factor.max(0.001);
        let image = screen
            .capture_area(
                (x as f32 / sf).round() as i32,
                (y as f32 / sf).round() as i32,
                (w as f32 / sf).round() as u32,
                (h as f32 / sf).round() as u32,
            )
            .map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into(), // screenshots crate 输出 RGBA
        })
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
