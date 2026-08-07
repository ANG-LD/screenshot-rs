//! X11 自动滚动注入（XTest 扩展，仅 X11/XWayland 会话可用）
//!
//! 通过 XTest 伪造滚轮事件（button 5 = 滚轮下）把滚动传给指针所在窗口。
//! XCBConnection 不是 Sync，只能在同一线程内使用，因此 XtestInjector
//! 全程在滚动循环所在线程上创建/持有。

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
