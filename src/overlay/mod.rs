//! 截图覆盖窗口模块
//!
//! 状态机：`Idle → Selecting → Editing → Idle`
//! GPUI 渲染层入口 `run_overlay` 在 `selection.rs` 中实现（Task 14）。

pub mod commands;
pub mod drawing;
pub mod font;
pub mod palette;
pub mod selection;
pub mod toolbar;
pub mod window;
