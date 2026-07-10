//! 应用统一错误类型
//!
//! 所有模块的错误通过 `AppError` 向上传播。库代码使用 `Result<T, AppError>`，
//! 入口 `main.rs` 用 `anyhow::Result` 兜底。

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("屏幕捕获失败：{0}")]
    Capture(String),

    #[error("剪贴板写入失败：{0}")]
    Clipboard(#[from] arboard::Error),

    #[error("热键注册失败：{0}")]
    Hotkey(String),

    #[error("托盘创建失败：{0}")]
    Tray(String),

    #[error("窗口操作失败：{0}")]
    Window(String),

    #[error("GPUI 错误：{0}")]
    Gpui(String),
}

/// 应用统一 Result 别名
pub type AppResult<T> = Result<T, AppError>;
