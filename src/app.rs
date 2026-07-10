//! 应用主状态：聚合所有服务，控制生命周期。
//!
//! MVP 阶段 `AppState` 仅做服务容器与事件循环分发；
//! GPUI 窗口的创建/销毁由 `overlay/mod.rs` 中的 `run_overlay` 入口负责。

use crate::capture::platform_capture;
use crate::clipboard::ClipboardService;
use crate::error::AppResult;
use crate::hotkey::{HotkeyEvent, HotkeyService};
use crate::tray::{TrayMenuEvent, TrayService};
use crate::utils::bounds::{Bounds, Point};

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
    pub fn run(&self) -> AppResult<()> {
        loop {
            if let Some(event) = self.hotkey.try_recv() {
                match event {
                    HotkeyEvent::TriggerScreenshot => {
                        tracing::info!("热键触发：开始截图");
                        self.trigger_screenshot()?;
                    }
                }
            }

            if let Some(event) = self.tray.try_recv() {
                match event {
                    TrayMenuEvent::TriggerScreenshot => {
                        tracing::info!("托盘触发：开始截图");
                        self.trigger_screenshot()?;
                    }
                    TrayMenuEvent::Quit => {
                        tracing::info!("托盘触发：退出");
                        return Ok(());
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// 触发一次截图：捕获屏幕 → 打开覆盖窗口 → 取选区 → 复制到剪贴板
    fn trigger_screenshot(&self) -> AppResult<()> {
        let frame = self.capture.capture_primary()?;
        let screen_bounds = Bounds::new(
            Point::ZERO,
            Point::new(frame.width as f32, frame.height as f32),
        );

        // 真实实现需要在 GPUI 主线程中运行 run_overlay
        // MVP 阶段：仅记录日志，暂未集成 GPUI 窗口
        tracing::info!(
            "捕获到 {}x{} 帧，覆盖窗口 bounds={:?}",
            frame.width,
            frame.height,
            screen_bounds
        );

        // TODO: 接入 GPUI 事件循环
        // let region = run_overlay(...);
        // if let Some(r) = region {
        //     let clipped = frame.clip_region(r.origin.x as u32, r.origin.y as u32, r.size.x as u32, r.size.y as u32)?;
        //     self.clipboard.write_frame(&clipped)?;
        // }

        Ok(())
    }
}
