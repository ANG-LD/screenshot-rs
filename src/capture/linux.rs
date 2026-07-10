// Linux 平台屏幕捕获实现（X11 / Wayland）。
//
// 计划（Task 7）：
// - 探测运行环境：X11 还是 Wayland
// - 在 X11 下通过 `screenshots` crate 直接抓取
// - 在 Wayland 下借助 XDG Portal（未来扩展点）
// - 处理多显示器与 HiDPI 缩放
//
// 当前为占位：仅提供 PlatformScreenCapture 的最小外壳，让 mod.rs 中的
// `pub use` 与 `platform_capture()` 在 Linux 上能编译通过。
// 真正的实现见 Task 7。

use crate::capture::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::{AppError, AppResult};

/// Linux 平台 ScreenCapture 占位实现
///
/// 真正的抓取逻辑在 Task 7 完成。当前所有调用都会 panic，开发期间可见。
pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        Self
    }
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        // Task 7 替换为真实抓取实现
        Err(AppError::Capture(
            "Linux 平台屏幕捕获尚未实现（Task 7）".to_string(),
        ))
    }

    fn list_displays(&self) -> Vec<DisplayInfo> {
        // Task 7 替换为通过 `screenshots::Screen::all()` 查询
        Vec::new()
    }
}
