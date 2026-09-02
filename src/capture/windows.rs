//! Windows 平台屏幕捕获实现
//!
//! 使用 `screenshots` crate，底层走 GDI（兼容性最好）。
//! 后续可优化为 DXGI Output Duplication 提升性能（不在 MVP 范围）。

use screenshots::Screen;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::AppResult;

/// Windows 平台屏幕捕获器
///
/// `screenshots` crate 内部根据平台自动选择 API；在 Windows 上
/// 默认使用 GDI（BitBlt）抓取全屏，兼容性好但效率一般。
/// 后续可改为 DXGI Output Duplication 提升性能。
pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        Self
    }

    /// 取主显示器（`Screen::all()` 的顺序不保证主屏在前；多显示器下用 `is_primary`
    /// 精确定位，兼容 `capture_primary` 与 `capture_area` 一致地从同一屏幕取样）。
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
    /// `screenshots` 的 `Screen::capture_area` 以「显示器的逻辑(DIP)坐标」解释入参，
    /// 并**按 `scale_factor` 放大**（win32.rs）。滚动引擎传的是主屏相对**物理像素**；
    /// 在非 100% DPI（Win 125/150/200% 或 Retina）下必须**除以 scale_factor**，
    /// 否则区域被二次放大、偏移 → 滚动拼接抓错区 / 触发 size_changed。
    /// 除以 sf 后 `capture_area` 内部再乘回 sf，返回的帧仍是物理像素，与调用方预期一致。
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
