//! 剪贴板模块
//!
//! 负责将截图结果写入系统剪贴板。
//! 使用 `arboard` crate 提供跨平台支持（Linux X11/Wayland、macOS、Windows）。
//!
//! Task 8 将实现：
//! - 写入图片到剪贴板
//! - 错误处理与平台差异兜底