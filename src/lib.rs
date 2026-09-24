//! screenshot-rs 库入口
//!
//! 暴露所有子模块给集成测试和外部调用者。

pub mod single_instance;
pub mod app;
pub mod assets;
pub mod capture;
pub mod clipboard;
pub mod config;
pub mod error;
pub mod hotkey;
pub mod ocr;
pub mod overlay;
pub mod scroll;
pub mod translate;
pub mod tray;
pub mod update;
pub mod utils;
