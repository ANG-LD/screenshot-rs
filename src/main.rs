//! screenshot-rs 应用入口
//!
//! 本文件是 screenshot-rs 桌面截图工具的程序入口。
//! 当前为 Task 1 占位实现，仅打印启动信息；后续任务将逐步填充：
//! - 初始化日志系统（tracing）
//! - 启动 GPUI 应用
//! - 注册全局快捷键
//! - 初始化系统托盘
//! - 进入主事件循环

// 模块声明：按职责划分的顶层模块
// 每个模块对应一个功能域，后续任务中按需填充实现
mod app;       // 应用主逻辑（GPUI App 生命周期管理）- Task 11
mod capture;   // 屏幕捕获（跨平台抽象，Linux/Windows 子模块）- Task 6/7
mod clipboard; // 剪贴板集成（将图片写入系统剪贴板）- Task 8
mod error;     // 统一错误类型定义（thiserror）- Task 2
mod hotkey;    // 全局快捷键监听与绑定 - Task 9
mod overlay;   // 截图选区遮罩 / 工具栏浮层 - Task 12
mod tray;      // 系统托盘图标与菜单 - Task 10
mod utils;     // 通用工具模块（bounds / color / image）- Task 3/4

fn main() {
    // 启动占位输出：后续将由 GPUI 应用接管
    println!("screenshot-rs starting...");
}