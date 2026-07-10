//! 应用主逻辑模块
//!
//! 负责 GPUI App 生命周期管理：初始化、组件装配、事件分发。
//! 作为其他子模块（capture / clipboard / hotkey / tray / overlay）的协调层。
//!
//! 占位 - Task 11 填充完整实现：
//! - 构造 `AppState` 聚合状态
//! - 初始化各子系统
//! - 实现 GPUI `Application` 入口