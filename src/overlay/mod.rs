//! 截图覆盖窗口模块
//!
//! 状态机：`Idle → Selecting → Editing → Idle`
//! GPUI 渲染层入口 `run_overlay` 在 `selection.rs` 中实现（Task 14）。

pub mod commands;
pub mod drawing;
pub mod palette;
pub mod selection;
pub mod toolbar;
pub mod window;

/// 覆盖窗口状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    /// 待命：窗口未创建
    Idle,
    /// 选区拖拽中：显示 dim 背景 + 选区矩形
    Selecting,
    /// 工具栏编辑：显示选区边框 + 工具栏
    Editing,
    /// 关闭中：清理资源
    Closing,
}
