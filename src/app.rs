//! 应用主状态：聚合所有服务，控制生命周期。
//!
//! MVP 阶段 `AppState` 仅做服务容器与事件循环分发；
//! GPUI 窗口的创建/销毁由 `overlay/mod.rs` 中的 `run_overlay` 入口负责。

use crate::capture::platform_capture;
use crate::clipboard::ClipboardService;
use crate::error::AppResult;
use crate::hotkey::{HotkeyEvent, HotkeyService};
use crate::tray::{TrayMenuEvent, TrayService};

/// 应用主状态
pub struct AppState {
    pub capture: Box<dyn crate::capture::ScreenCapture>,
    pub clipboard: ClipboardService,
    pub hotkey: HotkeyService,
    pub tray: TrayService,
}

impl AppState {
    pub fn new() -> AppResult<Self> {
        Ok(Self {
            capture: platform_capture(),
            clipboard: ClipboardService::new(),
            hotkey: HotkeyService::new()?,
            tray: TrayService::new()?,
        })
    }

    /// 主事件循环（MVP 简化版）
    ///
    /// 监听热键和托盘事件，触发截图。
    /// 实际 GPUI 窗口创建在 `run_overlay` 中处理（本任务只搭骨架）。
    pub fn run(&self) -> AppResult<()> {
        loop {
            // 优先处理热键事件
            if let Some(event) = self.hotkey.try_recv() {
                match event {
                    HotkeyEvent::TriggerScreenshot => {
                        tracing::info!("热键触发：开始截图");
                        // TODO Task 13+: 调用 overlay::run_overlay
                        // 临时仅打印日志
                    }
                }
            }

            // 处理托盘事件
            if let Some(event) = self.tray.try_recv() {
                match event {
                    TrayMenuEvent::TriggerScreenshot => {
                        tracing::info!("托盘触发：开始截图");
                    }
                    TrayMenuEvent::Quit => {
                        tracing::info!("托盘触发：退出");
                        return Ok(());
                    }
                }
            }

            // 避免空转
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
