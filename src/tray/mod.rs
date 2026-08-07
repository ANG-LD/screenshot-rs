//! 系统托盘服务模块
//!
//! 本模块基于 `tray-icon` crate（其内部封装了跨平台的托盘 API：
//! Windows 使用 win32 Shell_NotifyIcon，macOS 使用 NSStatusBar，
//! Linux 使用 libappindicator + GTK）实现应用托盘图标与菜单。
//!
//! ## 提供的功能
//!
//! - 在系统托盘区显示应用图标，并附带提示文本（tooltip）。
//! - 提供右键菜单：
//!   - 「截图」：向主应用发送 `TriggerScreenshot` 事件，相当于按下全局热键 `Alt+S`。
//!   - 「退出」：向主应用发送 `Quit` 事件，请求结束应用。
//!
//! ## 平台注意事项
//!
//! - **Windows / Linux**：必须在创建托盘图标的同一线程上运行事件循环。
//!   在 Windows 上是 win32 消息循环；在 Linux 上是 GTK 主循环。
//! - **macOS**：事件循环必须在主线程上运行，因此托盘也必须在主线程上创建。
//! - **Linux**：必须安装 `libappindicator3-dev` 或 `libayatana-appindicator3-dev`，
//!   编译期通过 `pkg-config` 链接；否则运行时会因找不到共享库而失败。
//!
//! ## 实现要点
//!
//! - `tray-icon` 通过静态全局 channel 派发菜单事件；我们用一个独立工作线程轮询该 channel，
//!   将匹配的 `MenuId` 转换为自定义的 `TrayMenuEvent` 并通过本地 mpsc 转发给主应用。
//! - 这样可以避免直接依赖 `tray-icon` 的事件类型，保持上层模块解耦。

use std::sync::mpsc::{Receiver, Sender};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::error::{AppError, AppResult};

/// 托盘图标尺寸（物理像素）
const TRAY_ICON_SIZE: u32 = 48;

/// 内嵌 48x48 PNG 图标数据
static TRAY_ICON_PNG: &[u8] = include_bytes!("../../assets/icons/tray-48.png");

/// 解码内嵌 PNG 为 RGBA Vec
fn load_tray_icon_rgba() -> AppResult<Vec<u8>> {
    let img = image::load_from_memory(TRAY_ICON_PNG)
        .map_err(|e| AppError::Tray(format!("托盘图标解码失败: {e}")))?;
    Ok(img.to_rgba8().into_raw())
}

/// 托盘菜单事件枚举
///
/// 由 `TrayService` 的后台监听线程在用户点击托盘菜单项时产生，
/// 通过内部 mpsc channel 转发给主应用消费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuEvent {
    /// 用户点击了「截图」菜单项，触发与全局热键 Alt+S 相同的区域截图流程。
    TriggerScreenshot,
    /// 用户点击了「退出」菜单项，请求结束应用。
    Quit,
}

/// 系统托盘服务
///
/// 负责：
/// 1. 在系统托盘区创建并维护应用图标；
/// 2. 维护右键菜单（截图 / 退出）；
/// 3. 监听菜单事件并转换为内部 `TrayMenuEvent` 转发给上层。
///
/// 注意：内部 `_icon` 字段必须保持存活，否则 `tray-icon` 的引用计数归零，
/// 托盘图标会被立即从系统移除。`_` 前缀既表达「有意保留」又避免未使用警告。
pub struct TrayService {
    /// 托盘图标实例；只要本结构体存活，托盘图标就一直显示。
    _icon: TrayIcon,
    /// 从后台监听线程接收 `TrayMenuEvent` 的 channel 接收端。
    event_rx: Receiver<TrayMenuEvent>,
}

impl TrayService {
    /// 创建托盘服务，注册系统托盘图标和菜单，并启动后台事件转发线程。
    ///
    /// # 流程
    /// 1. 创建菜单并添加「截图」「退出」两个 `MenuItem`。
    /// 2. 通过 `TrayIconBuilder` 构建托盘图标，设置菜单与提示文本。
    /// 3. 创建本地 mpsc channel，用于把 `tray-icon` 的全局菜单事件
    ///    转换为业务层的 `TrayMenuEvent`。
    /// 4. 启动一个独立工作线程，轮询 `MenuEvent::receiver()`，
    ///    根据 `MenuId` 匹配菜单项并通过 channel 发出对应事件。
    ///
    /// # 错误
    /// - 任一菜单项添加失败（理论上仅 Linux GTK 初始化失败时发生）。
    /// - 托盘图标创建失败（缺少 libappindicator、权限不足等）。
    pub fn new() -> AppResult<Self> {
        // 创建根菜单容器
        let menu = Menu::new();

        // 创建菜单项。注意 `tray-icon 0.11` 内部的 `muda 0.11` 中
        // `MenuItem::new` 的第三个参数是 `Option<Accelerator>`（快捷键文本），
        // 我们不需要全局快捷键（已有 alt+s），传 `None` 即可。
        let screenshot_item = MenuItem::new("截图", true, None);
        let quit_item = MenuItem::new("退出", true, None);

        // 将菜单项追加到菜单。`append` 在 Linux GTK 初始化失败时返回错误。
        menu.append(&screenshot_item)
            .map_err(|e| AppError::Tray(e.to_string()))?;
        menu.append(&quit_item)
            .map_err(|e| AppError::Tray(e.to_string()))?;

        // 构建托盘图标：设置图标、菜单、tooltip。
        let rgba = load_tray_icon_rgba()?;
        let tray_icon = Icon::from_rgba(rgba, TRAY_ICON_SIZE, TRAY_ICON_SIZE)
            .map_err(|e| AppError::Tray(e.to_string()))?;
        let icon = TrayIconBuilder::new()
            .with_icon(tray_icon)
            .with_menu(Box::new(menu))
            .with_tooltip("screenshot-rs")
            .build()
            .map_err(|e| AppError::Tray(e.to_string()))?;
        tracing::info!("系统托盘图标创建成功（菜单：截图 / 退出）");

        // 创建业务层 mpsc channel，用于把 tray-icon 的全局 MenuEvent
        // 转换成自定义的 TrayMenuEvent。
        let (event_tx, event_rx): (Sender<TrayMenuEvent>, Receiver<TrayMenuEvent>) =
            std::sync::mpsc::channel();

        // 提前克隆两个菜单项的 MenuId，移到工作线程里做匹配比较。
        // 这里使用 clone 而不是引用，是因为 MenuItem 内部 id 是 Rc<MenuId>，
        // 而我们这里 clone 出独立的 MenuId 用于线程间比较。
        let screenshot_id = screenshot_item.id().clone();
        let quit_id = quit_item.id().clone();

        // 启动后台监听线程：不断轮询 tray-icon 的全局 MenuEvent 通道，
        // 匹配到对应 MenuId 后通过本地 channel 转发。
        // 注意：这里使用 `try_recv` + `sleep` 的简单轮询而不是阻塞接收，
        // 是为了让循环能在出错（例如 receiver 关闭）时优雅退出。
        std::thread::spawn(move || {
            loop {
                // try_recv 非阻塞：若无事件立即返回 Err(Empty)。
                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == screenshot_id {
                        // 忽略发送失败（主应用可能已退出）
                        let _ = event_tx.send(TrayMenuEvent::TriggerScreenshot);
                    } else if event.id == quit_id {
                        let _ = event_tx.send(TrayMenuEvent::Quit);
                    }
                }
                // 短暂休眠，降低 CPU 占用；同时避免忙等阻塞其它线程调度。
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        Ok(Self {
            _icon: icon,
            event_rx,
        })
    }

    /// 非阻塞地尝试获取一个菜单事件。
    ///
    /// 没有事件时立即返回 `None`，适合在主循环中轮询调用。
    pub fn try_recv(&self) -> Option<TrayMenuEvent> {
        self.event_rx.try_recv().ok()
    }

    /// 阻塞地等待下一个菜单事件。
    ///
    /// 当发送端被关闭（`TrayService` 自身被销毁）时返回 `None`，
    /// 调用方应据此退出处理循环。
    pub fn recv(&self) -> Option<TrayMenuEvent> {
        self.event_rx.recv().ok()
    }
}