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
                        if let Err(e) = self.trigger_screenshot() {
                            tracing::error!("截图失败：{e}");
                        }
                    }
                }
            }

            if let Some(event) = self.tray.try_recv() {
                match event {
                    TrayMenuEvent::TriggerScreenshot => {
                        tracing::info!("托盘触发：开始截图");
                        if let Err(e) = self.trigger_screenshot() {
                            tracing::error!("截图失败：{e}");
                        }
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
        tracing::info!("捕获到 {}x{} 帧", frame.width, frame.height);

        // GPUI 覆盖层（阻塞直到用户选完/取消）
        let result = crate::overlay::window::run_blocking(frame.clone(), screen_bounds);
        let Some(region) = result.selection else {
            tracing::info!("用户取消截图");
            return Ok(());
        };
        tracing::info!(
            "选区 origin=({}, {}) size={}x{}；标注 {} 笔",
            region.origin.x,
            region.origin.y,
            region.size.x,
            region.size.y,
            result.commands.len()
        );

        // 裁剪并写入剪贴板
        let mut clipped = frame.clip_region(
            region.origin.x as u32,
            region.origin.y as u32,
            region.size.x as u32,
            region.size.y as u32,
        )?;

        // 把可见的 DrawCommand 应用到裁剪后的 frame 上
        // 命令的坐标是屏幕坐标，需要平移到 clipped 局部坐标
        if !result.commands.is_empty() {
            tracing::info!(
                "apply_commands: clipped={}x{} region_origin=({},{}) commands={}",
                clipped.width, clipped.height, region.origin.x, region.origin.y,
                result.commands.len()
            );
            crate::overlay::commands::apply_commands(
                &mut clipped,
                region.origin.x,
                region.origin.y,
                &result.commands,
            )?;
        }

        self.clipboard.write_frame(&clipped)?;
        tracing::info!("截图已复制到剪贴板（{}x{}）", clipped.width, clipped.height);

        Ok(())
    }
}
