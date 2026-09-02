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
        /// `_NET_ACTIVE_WINDOW` atom（预取，避免每轮 intern_atom 往返）
        net_active_atom: xproto::Atom,
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
            // 预取 `_NET_ACTIVE_WINDOW` atom：窗口激活要用，避免每轮查一次
            let net_active_atom = conn
                .intern_atom(false, b"_NET_ACTIVE_WINDOW")
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.atom)
                .unwrap_or(x11rb::NONE);
            Ok(Self { conn, root, net_active_atom })
        }

        /// 把指针移到屏幕绝对坐标 (abs_x, abs_y)（滚轮事件投递给指针下窗口）。
        ///
        /// 用单次 `warp_pointer` 绝对跳变即可（实测：一次 warp + 激活目标窗口 +
        /// 合成滚轮就能让 Chromium 滚动，无需平滑走位）。真正让合成滚轮生效的是
        /// ——目标窗口必须是对应 WM 的**活动窗口**（见 `focus_under_pointer`），
        /// 否则 Chromium/Electron 会丢弃投给「非活动窗口」的合成滚轮。
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

        /// 把 X 输入焦点设到指针下方的顶层窗口。
        ///
        /// Chromium/Electron 会忽略投给「未聚焦/被遮挡窗口」的合成滚轮事件。但仅靠
        /// X 层的 `set_input_focus` 在很多 EWMH/WM（GNOME/KDE）下**不会**把窗口标示为
        /// 「活动窗口」（WM 看的是 `_NET_ACTIVE_WINDOW`），Chrome 仍以为自己在后台而
        /// 丢弃合成滚轮——这是自动滚动「只拼一页、relocate 全部 differ=false」的另一
        /// 主因。因此先发送 `_NET_ACTIVE_WINDOW` 客户端消息到 WM（等价于 xdotool
        /// `windowactivate`），再补一个 X 层焦点，确保窗口真正获得输入。
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
            let top = self.toplevel_of(reply.child);
            // 每次注入前都强制发 `_NET_ACTIVE_WINDOW` 把目标窗口激活为 WM 的活动窗口：
            //
            // 不要用「已经是活动窗口就跳过」的优化。实测自动滚动起步时（热键按下、
            // overlay 关闭后）mutter 往往**仍把 Chrome 标为活动窗口**，若此处提前
            // return 跳过了 `_NET_ACTIVE_WINDOW`，就只剩 `set_input_focus`——而
            // `set_input_focus` 单独**不能**让 Chromium 接受合成滚轮（只有 EWMH
            // 激活才行），滚轮照旧被丢弃。所以每次滚动前都要显式 EWMH 激活。
            self.activate_window(top);
        }

        /// 从窗口向上走到顶层（父为 root 或 NONE）。
        fn toplevel_of(&self, child: xproto::Window) -> xproto::Window {
            let mut win = child;
            loop {
                let Ok(cookie) = self.conn.query_tree(win) else {
                    return win;
                };
                let Ok(reply) = cookie.reply() else {
                    return win;
                };
                if reply.parent == self.root || reply.parent == x11rb::NONE {
                    return win;
                }
                win = reply.parent;
            }
        }

        /// 发送 `_NET_ACTIVE_WINDOW` 客户端消息给 WM，把顶层窗口激活为活动窗口。
        fn activate_window(&self, win: xproto::Window) {
            let atom = self.net_active_atom;
            if atom == x11rb::NONE {
                // 拿不到 atom（异常环境）：退化为直接的 X 层焦点
                let _ = self.conn.set_input_focus(
                    xproto::InputFocus::NONE,
                    win,
                    x11rb::CURRENT_TIME,
                );
                self.conn.flush().ok();
                self.sync();
                return;
            }
            let event = xproto::ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: 32,
                sequence: 0,
                // IMPORTANT: `window` 字段 = 要激活的**目标窗口**（Chrome）。
                // 消息发送**到 root**（send_event 的 destination），但 event.window
                // 必须是目标窗口——这点和 xdotool `windowactivate`（xdo_activate_window）
                // 完全一致：`xev.xclient.window = wid`。之前写成 self.root 会让 WM
                // 「去激活 root」，Chrome 从未成为活动窗口，合成滚轮被丢弃。
                window: win,
                type_: atom,
                // data[0]=2 (source: pager/app)，data[1]=0 (CurrentTime)，其余 0
                data: xproto::ClientMessageData::from([2, 0, 0, 0, 0]),
            };
            let _ = self.conn.send_event(
                false,
                self.root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT
                    | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            );
            self.conn.flush().ok();
            // 再补一个直接的 X 层焦点（部分去 WM 的环境仍需要）
            let _ = self.conn.set_input_focus(
                xproto::InputFocus::NONE,
                win,
                x11rb::CURRENT_TIME,
            );
            self.conn.flush().ok();
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
            self.scroll_button(5, ticks);
        }

        /// 每 tick = ButtonPress + ButtonRelease for button 4（滚轮上）
        pub fn scroll_up(&self, ticks: u8) {
            self.scroll_button(4, ticks);
        }

        fn scroll_button(&self, button: u8, ticks: u8) {
            for _ in 0..ticks {
                let press = self.conn.xtest_fake_input(
                    xproto::BUTTON_PRESS_EVENT,
                    button,
                    0,
                    self.root,
                    0,
                    0,
                    0,
                );
                let release = self.conn.xtest_fake_input(
                    xproto::BUTTON_RELEASE_EVENT,
                    button,
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
            self.scroll_wheel(-WHEEL_DELTA, ticks);
        }

        /// 在当前指针位置向上滚动：每格 = +WHEEL_DELTA（向上）
        pub fn scroll_up(&self, ticks: u8) {
            self.scroll_wheel(WHEEL_DELTA, ticks);
        }

        fn scroll_wheel(&self, delta: i32, ticks: u8) {
            for _ in 0..ticks {
                unsafe {
                    mouse_event(MOUSEEVENTF_WHEEL, 0, 0, delta, 0);
                }
                std::thread::sleep(TICK_DELAY);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::XtestInjector;

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    use core_graphics::display::CGDisplay;
    use core_graphics::event::{CGEvent, CGEventTapLocation, ScrollEventUnit};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    use crate::error::AppResult;

    const TICK_DELAY: Duration = Duration::from_millis(80);
    const WARP_SETTLE: Duration = Duration::from_millis(16);
    /// 每个滚轮 tick 的滚动量（kCGScrollEventUnitLine：3 行 ≈ 一次滚轮刻度，与 Linux/
    /// Windows 的"一格"语义对齐）。引擎按实测 delta 校准，此值只影响每次滚动步长、
    /// 不影响拼接正确性。若在某些 app 下 LINE 不响应，可改用 PIXEL(=0) 且加大 delta。
    const LINES_PER_TICK: i32 = 3;

    /// 主显示器 scale_factor。注入器拿到的坐标是物理像素，而 CGEvent/CGDisplay 用
    /// 逻辑点（Retina 为 2×），必须换算；取不到时回退 1.0（非 Retina）。
    fn scale() -> f32 {
        screenshots::Screen::all()
            .ok()
            .and_then(|s| s.into_iter().find(|s| s.display_info.is_primary))
            .map(|s| s.display_info.scale_factor.max(0.001))
            .unwrap_or(1.0)
    }

    /// CoreGraphics 滚轮注入器（macOS）。
    ///
    /// 没有 XTest；用 `CGEvent::new_scroll_event` 合成滚轮事件并经 `CGEventPost`
    /// 投递到当前会话——事件走系统输入队列，滚轮交给指针下窗口，无需手动设焦点
    /// （与 Windows 用真实输入队列思路一致）。
    pub struct XtestInjector;

    impl XtestInjector {
        /// 无需连接，直接可用。
        pub fn open() -> AppResult<Self> {
            Ok(Self)
        }

        /// 把指针移到屏幕绝对坐标（滚轮事件投递给该位置下窗口）。物理像素→逻辑点。
        pub fn warp_to(&self, abs_x: i16, abs_y: i16) {
            let s = scale() as f64;
            let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(
                abs_x as f64 / s,
                abs_y as f64 / s,
            ));
            std::thread::sleep(WARP_SETTLE);
        }

        /// 合成滚轮走系统输入队列，投递给指针下窗口，无需手动设焦点。
        pub fn focus_under_pointer(&self) {}

        /// 当前指针到 (x, y) 的欧氏距离（物理像素）。查询失败返回 0.0。
        pub fn pointer_distance_from(&self, x: i16, y: i16) -> f64 {
            let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
                return 0.0;
            };
            let Ok(ev) = CGEvent::new(src) else {
                return 0.0;
            };
            let s = scale() as f64;
            let loc = ev.location();
            let dx = loc.x * s - x as f64;
            let dy = loc.y * s - y as f64;
            (dx * dx + dy * dy).sqrt()
        }

        /// 诊断：指针下窗口标题（macOS 需 CGWindowListCopyWindowInfo 较繁琐，此处
        /// 简化为说明文字；仅供日志诊断，不影响拼接）。
        pub fn describe_focus(&self) -> String {
            "macOS：滚轮按指针下窗口投递（无需设焦点）".into()
        }

        /// 诊断：指针位置（物理像素）。
        pub fn describe_pointer(&self) -> String {
            let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
                return "pos=(?,?)".into();
            };
            let Ok(ev) = CGEvent::new(src) else {
                return "pos=(?,?)".into();
            };
            let s = scale() as f64;
            let loc = ev.location();
            format!("pos=({:.0},{:.0})", loc.x * s, loc.y * s)
        }

        /// 在当前指针位置向下滚动：连续注入 `ticks` 个滚轮格（每格 3 行）。
        pub fn scroll_down(&self, ticks: u8) {
            self.scroll(-LINES_PER_TICK, ticks);
        }

        /// 在当前指针位置向上滚动。
        pub fn scroll_up(&self, ticks: u8) {
            self.scroll(LINES_PER_TICK, ticks);
        }

        fn scroll(&self, delta_line: i32, ticks: u8) {
            let Ok(src) = CGEventSource::new(CGEventSourceStateID::CombinedSessionState) else {
                return;
            };
            for _ in 0..ticks {
                if let Ok(ev) = CGEvent::new_scroll_event(
                    src.clone(),
                    ScrollEventUnit::LINE,
                    1,
                    delta_line,
                    0,
                    0,
                ) {
                    // Session tap（kCGSessionEventTap）：投递到当前会话输入流，模拟真实输入。
                    ev.post(CGEventTapLocation::Session);
                }
                std::thread::sleep(TICK_DELAY);
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::XtestInjector;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
use crate::error::{AppError, AppResult};

/// 其他平台桩（无 X11/XTest、Win32、CoreGraphics）：构造恒失败。
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub struct XtestInjector;

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
impl XtestInjector {
    /// 其他平台没有注入能力，恒返回错误。
    pub fn open() -> AppResult<Self> {
        Err(AppError::Window(
            "滚动截屏仅支持 Linux/X11、Windows 或 macOS 会话".into(),
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

    pub fn scroll_up(&self, _ticks: u8) {}
}

