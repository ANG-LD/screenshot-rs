//! 自动滚动注入器（滚动截屏把滚轮事件投递给指针下窗口）。
//!
//! - Linux：XTest 扩展伪造滚轮事件（button 5 = 滚轮下）。XCBConnection 不是
//!   Sync，只能在同一线程内使用，因此注入器全程在滚动循环所在线程创建/持有。
//! - Windows：Win32 `SetCursorPos` + `mouse_event(MOUSEEVENTF_WHEEL)` 走真实输入
//!   队列，投递给指针下窗口，无需手动设焦点。
//! - 其它平台不编译真实实现：`XtestInjector` 只保留一个桩类型，`open()` 恒报错，
//!   使滚动截屏在这些平台优雅失败（见 `scroll::new_injector`）。

/// XTest 滚轮注入器（真实实现，仅 Linux/X11 编译）。
///
/// 依赖 x11rb 的 `xcb` 特性链接系统 libxcb，而 libxcb 只存在于 X11 环境，
/// 因此整个实现必须按平台隔离，否则 Windows 链接器会找 `xcb.lib`。
#[cfg(target_os = "linux")]
mod imp {
    use std::time::Duration;

    use x11rb::connection::{Connection, RequestConnection};
    use x11rb::protocol::xproto::{self, ConnectionExt as _};
    use x11rb::protocol::xtest::{self, ConnectionExt as _};
    use x11rb::xcb_ffi::XCBConnection;

    use crate::error::{AppError, AppResult};

    /// 单 tick 间间隔（复刻 xdotool 滚轮注入的有效间隔；太短会被 Chromium 当 fling 合并）
    const TICK_DELAY: Duration = Duration::from_millis(80);

    /// XTest 滚轮注入器
    pub struct XtestInjector {
        conn: XCBConnection,
        root: xproto::Window,
    }

    impl XtestInjector {
        /// 打开一个独立的 X 连接并确认 XTEST 扩展可用
        pub fn open() -> AppResult<Self> {
            let (conn, screen_num) = XCBConnection::connect(None)
                .map_err(|e| AppError::Window(format!("X 连接失败: {e}")))?;
            let has_xtest = conn
                .extension_information(xtest::X11_EXTENSION_NAME)
                .map_err(|e| AppError::Window(format!("XTEST 扩展查询失败: {e}")))?
                .is_some();
            if !has_xtest {
                return Err(AppError::Window(
                    "当前 X server 不支持 XTEST 扩展，无法自动滚动".into(),
                ));
            }
            let root = conn.setup().roots[screen_num].root;
            // 声明使用 XTEST 的意图，部分 server/WM 对此有不同的事件可信度策略
            let _ = conn.xtest_grab_control(true);
            conn.flush().ok();
            Ok(Self { conn, root })
        }

        /// 把指针移到屏幕绝对坐标 (abs_x, abs_y)（滚轮事件投递给指针下窗口）
        pub fn warp_to(&self, abs_x: i16, abs_y: i16) {
            if let Err(e) = self.conn.warp_pointer(
                x11rb::NONE,
                self.root,
                0,
                0,
                0,
                0,
                abs_x,
                abs_y,
            ) {
                tracing::warn!("[scroll] warp_pointer failed: {e}");
            }
            self.conn.flush().ok();
            // 往返等待 server 完成 warp + EnterNotify，确保后续 XTest 事件
            // 投递给指针新位置下的窗口
            self.sync();
        }

        /// 把 X 输入焦点设到指针下方的窗口。
        ///
        /// Chromium/Electron 会忽略投给「未聚焦/被遮挡窗口」的合成滚轮事件；
        /// 滚动前先把焦点给目标窗口，它们才会响应 XTEST 注入。
        pub fn focus_under_pointer(&self) {
            let Ok(cookie) = self.conn.query_pointer(self.root) else {
                return;
            };
            let Ok(reply) = cookie.reply() else {
                return;
            };
            if reply.child == x11rb::NONE {
                return;
            }
            if let Err(e) = self.conn.set_input_focus(
                xproto::InputFocus::POINTER_ROOT,
                reply.child,
                x11rb::CURRENT_TIME,
            ) {
                tracing::warn!("[scroll] set_input_focus failed: {e}");
            }
            self.conn.flush().ok();
            // 等待 server 完成焦点转移，Chromium 需要确认焦点后才接受合成滚轮
            self.sync();
        }

        /// 当前指针到 (x, y) 的欧氏距离（物理像素）。
        ///
        /// 用于检测用户是否把鼠标移离注入目标（比如想去点进度窗按钮），
        /// 此时应暂停注入、让指针自由。查询失败返回 0.0（当作在目标附近，
        /// 避免连接抖动导致引擎永久暂停）。
        pub fn pointer_distance_from(&self, x: i16, y: i16) -> f64 {
            let Ok(cookie) = self.conn.query_pointer(self.root) else {
                return 0.0;
            };
            let Ok(reply) = cookie.reply() else {
                return 0.0;
            };
            let dx = reply.root_x as f64 - x as f64;
            let dy = reply.root_y as f64 - y as f64;
            (dx * dx + dy * dy).sqrt()
        }

        /// 查询当前 X 输入焦点所在的窗口标题（诊断用）。
        ///
        /// 用于确认注入滚轮时目标窗口是否处于聚焦状态。
        pub fn describe_focus(&self) -> String {
            let Ok(cookie) = self.conn.get_input_focus() else {
                return "get_input_focus failed".into();
            };
            let Ok(reply) = cookie.reply() else {
                return "get_input_focus reply failed".into();
            };
            let win = reply.focus;
            if win == x11rb::NONE {
                return format!("focus={win:#x}");
            }
            let title = self
                .window_title(win)
                .unwrap_or_else(|| "(no title)".into());
            format!("focus={win:#x} title={title:?}")
        }

        /// 查询当前指针位置及所在窗口标题（诊断用）。
        ///
        /// 用于确认 warp 坐标是否正确落在目标窗口上（多显示器下主屏原点可能不是
        /// root (0,0)，此时滚轮事件会投递给别的窗口）。
        pub fn describe_pointer(&self) -> String {
            let Ok(cookie) = self.conn.query_pointer(self.root) else {
                return "query_pointer failed".into();
            };
            let Ok(reply) = cookie.reply() else {
                return "query_pointer reply failed".into();
            };
            let child = reply.child;
            if child == x11rb::NONE {
                return format!("root=({},{}) child=root", reply.root_x, reply.root_y);
            }
            let title = self
                .window_title(child)
                .unwrap_or_else(|| "(no title)".into());
            format!(
                "root=({},{}) child={:#x} title={:?}",
                reply.root_x, reply.root_y, child, title
            )
        }

        /// 读取窗口 `_NET_WM_NAME` 标题
        fn window_title(&self, window: xproto::Window) -> Option<String> {
            let name_atom = self
                .conn
                .intern_atom(false, b"_NET_WM_NAME")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let reply = self
                .conn
                .get_property(false, window, name_atom, x11rb::NONE, 0, 1024)
                .ok()?
                .reply()
                .ok()?;
            if reply.value_len == 0 {
                return None;
            }
            String::from_utf8(reply.value.to_vec()).ok()
        }

        /// 在当前指针位置向下滚动：连续注入 `ticks` 个滚轮下 tick。
        ///
        /// 每 tick = ButtonPress + ButtonRelease for button 5（滚轮下）。
        /// 显式传 self.root 作为 root window，避免 NONE(0) 时部分 server/WM 将事件
        /// 投递到错误窗口。event type 使用 xproto 命名常量而非裸数字。
        /// 每次注入后做 XSync 往返确保 server 已完成事件递送，再等 TICK_DELAY。
        pub fn scroll_down(&self, ticks: u8) {
            for _ in 0..ticks {
                let press = self.conn.xtest_fake_input(
                    xproto::BUTTON_PRESS_EVENT,
                    5, // button 5 = scroll down
                    0,
                    self.root,
                    0,
                    0,
                    0,
                );
                let release = self.conn.xtest_fake_input(
                    xproto::BUTTON_RELEASE_EVENT,
                    5,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                );
                if let Err(e) = press {
                    tracing::warn!("[scroll] fake press failed: {e}");
                }
                if let Err(e) = release {
                    tracing::warn!("[scroll] fake release failed: {e}");
                }
                self.conn.flush().ok();
                // XSync 往返：确保 server 已处理 ButtonPress/Release 生成的事件，
                // 避免事件被缓冲导致 Chromium 在下一个 tick 前未收到上一轮 scroll
                self.sync();
                std::thread::sleep(TICK_DELAY);
            }
        }

        /// XSync 等价操作：发送一个需要回复的请求并等待回复，强制与 server 往返。
        /// 保证此前所有已 flush 的请求都已被 server 处理。
        fn sync(&self) {
            if let Ok(c) = self.conn.get_input_focus() {
                let _ = c.reply();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::XtestInjector;

/// Win32 自动滚动注入器（真实实现，Windows 编译）。
///
/// Windows 没有 XTest；用 `SetCursorPos` 移指针 + `mouse_event(MOUSEEVENTF_WHEEL)`
/// 发送真实滚轮输入（投递给指针下窗口）。与 X11 不同，无需手动设焦点——
/// 合成滚轮事件走真实输入队列，窗口按指针位置接收。
#[cfg(target_os = "windows")]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_WHEEL};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowTextW, SetCursorPos, WindowFromPoint,
    };

    use crate::error::AppResult;

    /// 单 tick 间间隔（与 X11 实现同节奏：太快的连续滚轮会被 Chromium 当 fling 合并）
    const TICK_DELAY: Duration = Duration::from_millis(80);
    /// 滚轮每格刻度（Windows WHEEL_DELTA）
    const WHEEL_DELTA: i32 = 120;
    /// warp 后等待系统完成指针移动与命中窗口刷新
    const WARP_SETTLE: Duration = Duration::from_millis(16);

    /// Win32 滚轮注入器
    pub struct XtestInjector;

    impl XtestInjector {
        /// Win32 注入无需连接，直接可用。
        pub fn open() -> AppResult<Self> {
            Ok(Self)
        }

        /// 把指针移到屏幕绝对坐标 (abs_x, abs_y)（滚轮事件投递给指针下窗口）。
        pub fn warp_to(&self, abs_x: i16, abs_y: i16) {
            if unsafe { SetCursorPos(abs_x as i32, abs_y as i32) } == 0 {
                tracing::warn!("[scroll] SetCursorPos({abs_x},{abs_y}) 失败");
            }
            // 等系统完成指针移动与命中窗口刷新，避免滚轮投给旧位置窗口
            std::thread::sleep(WARP_SETTLE);
        }

        /// Windows 合成滚轮走真实输入队列，投递给指针下窗口，无需手动设焦点。
        pub fn focus_under_pointer(&self) {}

        /// 当前指针到 (x, y) 的欧氏距离（物理像素）。查询失败返回 0.0。
        pub fn pointer_distance_from(&self, x: i16, y: i16) -> f64 {
            let mut pt: POINT = unsafe { std::mem::zeroed() };
            if unsafe { GetCursorPos(&mut pt) } == 0 {
                return 0.0;
            }
            let dx = pt.x as f64 - x as f64;
            let dy = pt.y as f64 - y as f64;
            (dx * dx + dy * dy).sqrt()
        }

        /// 诊断：指针下方窗口标题。
        pub fn describe_focus(&self) -> String {
            self.window_under_pointer()
                .unwrap_or_else(|| "(none)".into())
        }

        /// 诊断：指针位置 + 下方窗口标题。
        pub fn describe_pointer(&self) -> String {
            let mut pt: POINT = unsafe { std::mem::zeroed() };
            let _ = unsafe { GetCursorPos(&mut pt) };
            format!(
                "pos=({},{}) {}",
                pt.x,
                pt.y,
                self.window_under_pointer()
                    .unwrap_or_else(|| "(none)".into())
            )
        }

        /// 指针下窗口标题
        fn window_under_pointer(&self) -> Option<String> {
            let mut pt: POINT = unsafe { std::mem::zeroed() };
            if unsafe { GetCursorPos(&mut pt) } == 0 {
                return None;
            }
            let hwnd = unsafe { WindowFromPoint(pt) };
            self.window_title(hwnd)
        }

        fn window_title(&self, hwnd: HWND) -> Option<String> {
            if hwnd.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 512];
            let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
            if n <= 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        }

        /// 在当前指针位置向下滚动：连续注入 `ticks` 个滚轮格。
        /// 每格 = MOUSEEVENTF_WHEEL 事件、dwData = -WHEEL_DELTA（向下）。
        pub fn scroll_down(&self, ticks: u8) {
            for _ in 0..ticks {
                unsafe {
                    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, -WHEEL_DELTA, 0);
                }
                std::thread::sleep(TICK_DELAY);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::XtestInjector;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
use crate::error::{AppError, AppResult};

/// 非 Linux/Windows 平台桩：没有 X11/XTest、也没有 Win32，构造恒失败。
///
/// 保留同名方法与真实实现一致的接口，让 `scroll` 引擎在各平台统一编译；
/// `new_injector()` 在 macOS 等平台返回错误，这些方法实际上永远不会被调用。
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub struct XtestInjector;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl XtestInjector {
    /// 非 Linux/Windows 平台没有注入能力，恒返回错误。
    pub fn open() -> AppResult<Self> {
        Err(AppError::Window(
            "滚动截屏仅支持 Linux/X11 或 Windows 会话".into(),
        ))
    }

    pub fn warp_to(&self, _abs_x: i16, _abs_y: i16) {}

    pub fn focus_under_pointer(&self) {}

    pub fn pointer_distance_from(&self, _x: i16, _y: i16) -> f64 {
        0.0
    }

    pub fn describe_focus(&self) -> String {
        "滚动截屏当前平台不支持".into()
    }

    pub fn describe_pointer(&self) -> String {
        "滚动截屏当前平台不支持".into()
    }

    pub fn scroll_down(&self, _ticks: u8) {}
}
