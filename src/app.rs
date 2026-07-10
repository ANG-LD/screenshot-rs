//! 应用主逻辑模块
//!
//! 负责 GPUI App 生命周期管理：初始化、组件装配、事件分发。
//! 作为其他子模块（capture / clipboard / hotkey / tray / overlay）的协调层。
//!
//! 占位 - Task 11 填充完整实现：
//! - 构造 `AppState` 聚合状态
//! - 初始化各子系统
//! - 实现 GPUI `Application` 入口

use crate::error::AppResult;

/// 应用全局状态聚合（占位实现）
///
/// 当前仅提供一个空壳，Task 11 将替换为完整实现。
#[derive(Debug, Default)]
pub struct AppState {
    // 后续任务会填充：
    // - capture_engine: capture::Engine
    // - clipboard: clipboard::Manager
    // - hotkey_manager: hotkey::Manager
    // - tray: tray::Tray
    // - overlay_state: overlay::State
}

impl AppState {
    /// 构造一个空的 AppState（占位实现）
    pub fn new() -> AppResult<Self> {
        Ok(Self::default())
    }
}
