//! 统一错误类型模块
//!
//! 使用 `thiserror` 定义项目统一的错误枚举 `AppError`，
//! 覆盖截图、剪贴板、文件 IO、配置等领域的失败场景。
//!
//! 占位 - Task 2 填充完整实现：
//! - `AppError` 枚举与 `Result<T>` 类型别名
//! - 各子模块（capture / clipboard / tray 等）的错误转换
//! - 与 `anyhow` 的协同使用策略