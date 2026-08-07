//! Linux 平台屏幕捕获实现
//!
//! MVP 阶段要求 X11 会话（XWayland fallback 也可）。纯 Wayland 原生支持作为 v0.2 任务。
//!
//! ## 背景说明
//!
//! Linux 上的图形栈并不统一，常见的显示服务器协议有：
//!
//! - **X11**：历史悠久，API 稳定，几乎所有抓屏方案都能直接抓 Root Window。
//! - **Wayland**：现代协议，强调安全隔离；普通进程无法直接读取其他窗口/桌面像素，
//!   必须通过 XDG Desktop Portal（Screenshot 接口）经合成器（如 Mutter / KWin）
//!   中转才能完成截屏。
//! - **XWayland**：Wayland 下的 X11 兼容层，绝大多数抓屏 crate 在 XWayland 下
//!   仍然可以工作，因此 `screenshots` crate 在 Linux 上其实是默认走 X11/XWayland 路径。
//!
//! `screenshots` crate 内部根据编译特性选择后端：在 Linux 上它链接 `x11rb` 并调用
//! `XGetImage` / `XShmGetImage` 抓取根窗口。Wayland 原生会话下，未配置 XWayland
//! 或 Portal 时通常会失败——这是预期行为，会在 v0.2 中通过 Portal 方式补齐。
//!
//! ## 实现策略
//!
//! - 主屏抓取：调用 `Screen::all()` 获取全部屏幕，再取第一个作为"主屏"。
//! - 显示器枚举：同样通过 `Screen::all()`，并利用每个 `Screen` 自带的
//!   `display_info` 字段（分辨率 + HiDPI 缩放因子）。
//! - 错误处理：所有来自 `screenshots` crate 的错误统一映射为
//!   `AppError::Capture`，便于上层统一日志/上报。

use screenshots::Screen;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::AppResult;

/// Linux 平台 ScreenCapture 实现
///
/// `screenshots` crate 已封装底层 X11/XWayland 调用，这里仅做"取主屏 /
/// 枚举显示器"的薄封装。该结构体无字段，零成本抽象。
pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    /// 构造一个 Linux 平台捕获器实例
    ///
    /// 当前没有需要保存的状态，所以直接返回单元结构体。
    pub fn new() -> Self {
        Self
    }
}

impl ScreenCapture for PlatformScreenCapture {
    /// 抓取主显示器全屏画面
    ///
    /// 步骤：
    /// 1. 通过 `Screen::all()` 枚举系统上所有可用屏幕；
    /// 2. 取第一个作为主屏（与 `screenshots` crate 在多屏环境下"主屏"语义对齐）；
    /// 3. 调用 `screen.capture()` 获取 RGBA 像素；
    /// 4. 转换为统一的 `CapturedFrame`（width / height / RGBA bytes）。
    ///
    /// ## 错误情况
    /// - 无法访问 X Server（如纯 Wayland 无 XWayland fallback）→ `AppError::Capture`
    /// - 系统未检测到任何显示器 → `AppError::Window`
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        // 1) 枚举所有屏幕；任何 X11 协议错误（无法连接 X Server、权限不足等）
        //    都通过 `AppError::Capture` 暴露给调用方。
        let screens = Screen::all().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;

        // 2) 取第一块屏幕作为主屏。注意：这里没有按坐标或 role 排序，
        //    因为 MVP 不要求多屏语义；后续若需要"主屏 = 用户桌面"，
        //    应改用 `_XROOTMAP_WINDOW` 或 Portal 报告的 primary output。
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into()))?;

        // 3) 抓取该屏幕全屏像素，screenshots crate 内部完成 X11 同步调用。
        let image = screen.capture().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;

        // 4) 包装成项目统一的 CapturedFrame。
        //    screenshots crate 返回的 Image 实现 `Into<Vec<u8>>`，且像素顺序
        //    为 RGBA，可直接 move 进我们的结构体（零拷贝）。
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into(),
        })
    }

    /// 捕获主屏上指定区域（滚动截屏用）
    ///
    /// 与 `capture_primary` 一样每次调用重新取主屏 `Screen`，避免把连接状态
    /// 存进实现里跨线程共享（trait 是 Send + Sync）。
    ///
    /// 坐标语义：frame 物理像素 = 主屏相对坐标；`capture_area` 内部会再加
    /// `display_info.x/y`。越界区域会被 clamp，返回尺寸可能小于请求值。
    fn capture_area(&self, x: i32, y: i32, w: u32, h: u32) -> AppResult<CapturedFrame> {
        let screens = Screen::all().map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into()))?;
        let image = screen
            .capture_area(x, y, w, h)
            .map_err(|e| crate::error::AppError::Capture(e.to_string()))?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into(),
        })
    }

    /// 列出系统上所有可用显示器
    ///
    /// `Screen::all()` 失败时（例如没有可用的 X Server）返回空 Vec 而不是错误，
    /// 这样 UI 层仍可正常启动、仅显示"无显示器"提示，不会因为枚举失败导致
    /// 整个应用 panic。
    fn list_displays(&self) -> Vec<DisplayInfo> {
        Screen::all()
            .map(|screens| {
                screens
                    .into_iter()
                    .enumerate()
                    .map(|(id, s)| DisplayInfo {
                        // 用枚举顺序作为稳定 id（MVP 范围够用）。
                        id: id as u32,
                        width: s.display_info.width,
                        height: s.display_info.height,
                        // HiDPI 缩放因子：X11 下通常为 1.0，Wayland 下可能 >1.0。
                        scale_factor: s.display_info.scale_factor,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}