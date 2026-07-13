//! 全局热键监听服务
//!
//! 使用 `global-hotkey` crate。注册 alt+s 作为截图触发键。
//! 跨平台支持：Windows (RegisterHotKey) / Linux X11 (XGrabKey) / macOS (不需实现).
//!
//! 设计要点：
//! - `HotkeyService::new()` 在创建时同步注册 alt+s 全局快捷键；
//! - 启动一个独立线程持续轮询 `GlobalHotKeyEvent::receiver()`，
//!   将底层的 `GlobalHotKeyEvent` 转换为更上层的 `HotkeyEvent`，通过自有 mpsc 通道下发；
//! - 调用方（一般是 `app` 模块）通过 `try_recv()` 或 `recv()` 拿到事件，
//!   进而触发截图流程。

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::sync::mpsc::{Receiver, Sender};

use crate::error::{AppError, AppResult};

/// 应用层可识别的热键事件枚举
///
/// 与底层 `global_hotkey::GlobalHotKeyEvent` 解耦，方便业务侧
/// 根据事件触发相应的截图流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// 用户按下 alt+s，请求触发截图
    TriggerScreenshot,
}

/// 全局热键服务
///
/// 负责：
/// 1. 创建 `GlobalHotKeyManager` 并注册 alt+s；
/// 2. 启动后台监听线程，把底层事件转换为 `HotkeyEvent` 后通过 mpsc 发出；
/// 3. 提供 `try_recv()` / `recv()` 让上层主动轮询或阻塞等待事件。
pub struct HotkeyService {
    /// 全局热键管理器（保留字段，未来可用于注销/重新注册）
    #[allow(dead_code)]
    manager: GlobalHotKeyManager,
    /// 监听线程 → 业务侧的事件通道发送端（保留字段，便于将来扩展）
    #[allow(dead_code)]
    event_tx: Sender<HotkeyEvent>,
    /// 业务侧接收热键事件的通道接收端
    event_rx: Receiver<HotkeyEvent>,
    /// 当前注册的截图热键 ID（保留字段，便于将来注销/重新注册）
    #[allow(dead_code)]
    screenshot_id: u32,
}

impl HotkeyService {
    /// 创建并启动全局热键服务
    ///
    /// 流程：
    /// 1. 创建 `GlobalHotKeyManager`；
    /// 2. 创建自有 mpsc 通道；
    /// 3. 注册 alt+s 作为截图触发键；
    /// 4. 启动后台监听线程，把底层 `GlobalHotKeyEvent` 转为 `HotkeyEvent`。
    pub fn new() -> AppResult<Self> {
        // 1. 创建底层全局热键管理器
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| AppError::Hotkey(format!("创建全局热键管理器失败：{e}")))?;
        tracing::info!("全局热键管理器创建成功");

        // 2. 创建自有事件通道（监听线程 → 业务侧）
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        // 3. 构造并注册 alt+s
        let hotkey = HotKey::new(Some(Modifiers::ALT), Code::KeyS);
        let screenshot_id = hotkey.id();
        manager
            .register(hotkey)
            .map_err(|e| AppError::Hotkey(format!("注册 alt+s 失败：{e}")))?;
        tracing::info!("已注册全局热键：alt+s（hotkey id = {}）", screenshot_id);

        // 4. 启动监听线程
        //    把底层 global-hotkey 事件转成我们自己的 HotkeyEvent
        //    使用 try_recv + sleep 的轮询模式，避免长时间占用 CPU
        //    注意：克隆一份 Sender 给线程，原 Sender 保留在 HotkeyService 中
        //    以便将来扩展（例如动态注册/注销热键时使用）。
        let tx_for_thread = event_tx.clone();
        std::thread::spawn(move || {
            let event_tx = tx_for_thread;
            loop {
                if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                    tracing::debug!(
                        "热键事件：id={:?} state={:?}",
                        event.id,
                        event.state
                    );
                    // 只关心按键按下事件（松开不重复触发）
                    if event.state == HotKeyState::Pressed {
                        // 这里目前只有一个热键（alt+s），因此直接发出截图事件；
                        // 将来若有多个热键，可根据 event.id() 进行匹配分发。
                        let _ = event_tx.send(HotkeyEvent::TriggerScreenshot);
                    }
                }
                // 50ms 轮询间隔：兼顾响应速度与 CPU 占用
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        Ok(Self {
            manager,
            event_tx,
            event_rx,
            screenshot_id,
        })
    }

    /// 非阻塞检查是否有热键事件
    ///
    /// 若通道里有事件则返回 `Some(HotkeyEvent)`；否则返回 `None`。
    /// 适合在主循环里轮询使用。
    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.try_recv().ok()
    }

    /// 阻塞接收下一个热键事件
    ///
    /// 若监听线程已结束（通道断开），返回 `None`。
    pub fn recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.recv().ok()
    }
}