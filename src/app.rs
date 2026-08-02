//! 应用主状态：聚合所有服务，控制生命周期。
//!
//! MVP 阶段 `AppState` 仅做服务容器与事件循环分发；
//! GPUI 窗口的创建/销毁由 `overlay/mod.rs` 中的 `run_overlay` 入口负责。

use crate::capture::platform_capture;
use crate::clipboard::ClipboardService;
use crate::error::AppResult;
use crate::hotkey::{HotkeyEvent, HotkeyService};
use crate::overlay::window::OverlayService;
use crate::tray::{TrayMenuEvent, TrayService};
use crate::utils::bounds::{Bounds, Point};

/// 应用主状态
pub struct AppState {
    pub capture: Box<dyn crate::capture::ScreenCapture>,
    pub clipboard: ClipboardService,
    pub hotkey: HotkeyService,
    pub tray: TrayService,
    /// 常驻 GPUI 服务（懒启动，进程级单例）
    pub overlay: OverlayService,
}

impl AppState {
    pub fn new() -> AppResult<Self> {
        Ok(Self {
            capture: platform_capture(),
            clipboard: ClipboardService::new(),
            hotkey: HotkeyService::new()?,
            tray: TrayService::new()?,
            overlay: OverlayService::new(),
        })
    }

    /// 主事件循环（MVP 简化版）
    pub fn run(&self) -> AppResult<()> {
        loop {
            // Windows 下必须先泵取 Win32 消息：
            // global-hotkey / tray-icon 各自创建了隐藏窗口并注册到当前线程的消息队列，
            // 只有 PeekMessage + DispatchMessage 才会触发它们的窗口过程，
            // WM_HOTKEY（热键）与托盘回调事件才能送达。两个 crate 都要求
            // 「创建管理器/图标的线程必须运行 Win32 消息循环」。
            #[cfg(target_os = "windows")]
            pump_windows_messages();

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

        // GPUI 覆盖层（阻塞直到用户选完/取消；常驻应用内开窗）
        let result = self.overlay.open_overlay(frame.clone(), screen_bounds);
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

        // 用户点了「固定」：把裁剪+标注好的帧交给常驻应用开 Pin 窗口（同 app，不复制剪贴板）
        if let Some(payload) = result.pin {
            tracing::info!("用户固定截图");
            self.overlay.open_pin(payload);
        }

        // 裁剪并写入剪贴板（Pin 固定时跳过）
        if !result.no_clipboard {
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
        }

        Ok(())
    }
}

/// 泵取当前线程的全部 Win32 待处理消息（PM_REMOVE）。
///
/// global-hotkey 的 `WM_HOTKEY`、tray-icon 的托盘回调消息都投递到各自隐藏窗口，
/// 归属于本线程（创建线程）的消息队列；不执行 GetMessage/PeekMessage +
/// DispatchMessage 时，窗口过程不会被调用，事件永远不会发出。
#[cfg(target_os = "windows")]
fn pump_windows_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        // 取出队列中所有已到达的消息并分发；无消息时返回 0，结束本轮泵取。
        // hwnd 传 null 表示泵取当前线程消息队列的所有消息（热键/托盘窗口都属于本线程）。
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
