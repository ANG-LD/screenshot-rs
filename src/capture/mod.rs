//! 屏幕捕获模块
//!
//! 通过 `screenshots` crate 提供跨平台截图能力。
//! 当前包含平台子模块：
//! - `linux`：X11 / Wayland 后端实现（Task 7）
//! - `windows`：Windows GDI / DXGI 后端实现（Task 6）
//!
//! 平台无关的 trait 与类型定义见 Task 5。

pub mod linux;
pub mod windows;