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
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        // 获取所有屏幕，取第一个作为主屏
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

    /// 滚动截屏暂不支持 Windows（区域截屏可行，但自动滚动注入未实现）
    fn capture_area(&self, _x: i32, _y: i32, _w: u32, _h: u32) -> AppResult<CapturedFrame> {
        Err(crate::error::AppError::Window(
            "滚动截屏暂不支持 Windows".into(),
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
