//! 马赛克笔刷：把**系统光标本身**隐藏掉，让自绘的圆盘当指针。
//!
//! 用户要求"鼠标指针变成实心圆"，而实测现状是"实心圆 + 箭头光标压在上面"。
//! GPUI 这边做不到：
//! - `CursorStyle` 的 21 个变体里没有任何"隐藏/空光标"，只有 Arrow/IBeam/Crosshair…；
//! - 平台层确实有 `hide_cursor_until_mouse_moves()`（X11 后端会给我们窗口设一个
//!   1×1 全透明的 cursor），但 `App::platform` 是 `pub(crate)`，且
//!   `CursorHideMode` 只有 `Never/OnTyping/OnTypingAndAction`，够不到"一直隐藏"。
//!
//! 所以这里直接用 x11rb（本项目已有依赖，见 scroll/xtest.rs 的同一套用法）给
//! **指针所在的那个顶层窗口**设 1×1 全透明 cursor。X11 的 cursor 是 per-window
//! 属性，只影响被设的那一个窗口，系统里其它窗口不受影响；恢复时设回
//! `XCB_NONE`（= 继承父窗口/root 的默认箭头）。
//!
//! 失败一律静默降级：非 X11（如 Wayland）、连接失败、找不到窗口时什么都不做，
//! 保持"箭头 + 圆盘"的现状，绝不因为光标功能影响截图主流程。

/// 指针所在顶层窗口必须至少占屏幕这么大，才认为是我们的全屏覆盖层。
///
/// 防止"指针其实在别的应用的小窗口上"时把人家窗口的光标改掉。
/// 覆盖层按定义铺满整块屏幕，取 0.9 留出窗口边框/任务栏的余量。
const FULLSCREEN_COVER_RATIO: f64 = 0.9;

/// 窗口是否"几乎铺满屏幕"——用于确认指针下那个顶层窗口是我们的全屏覆盖层。
pub fn covers_screen(win_w: u16, win_h: u16, screen_w: u16, screen_h: u16) -> bool {
    f64::from(win_w) >= f64::from(screen_w) * FULLSCREEN_COVER_RATIO
        && f64::from(win_h) >= f64::from(screen_h) * FULLSCREEN_COVER_RATIO
}

#[cfg(target_os = "linux")]
mod imp {
    use super::covers_screen;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt, Window};
    use x11rb::xcb_ffi::XCBConnection;

    /// 隐形的马赛克笔刷光标（X11）。
    pub struct BrushCursor {
        /// 惰性建立：不用马赛克时完全不碰 X 连接
        x: Option<Xcb>,
        /// 当前是否已隐藏（只有状态翻转时才发 X 请求，鼠标移动不会反复发）
        hidden: bool,
    }

    struct Xcb {
        conn: XCBConnection,
        root: Window,
        /// root 的像素尺寸（校验"铺满屏幕"用）
        screen: (u16, u16),
        /// 1×1 全透明 cursor（source/mask 都是未填充的 1×1 pixmap）
        invisible: u32,
        /// 被我们改过 cursor 的窗口，恢复时按原样还回去
        applied: Vec<Window>,
    }

    impl BrushCursor {
        pub fn new() -> Self {
            Self {
                x: None,
                hidden: false,
            }
        }

        /// 需要时隐藏系统光标 / 不需要时恢复。幂等：状态没变就完全不碰 X。
        pub fn set_hidden(&mut self, want_hidden: bool) {
            if want_hidden == self.hidden {
                return;
            }
            if want_hidden {
                // 只有真的设上了才算"已隐藏"：失败时保持 false，下次还会再试
                let ok = self.hide();
                self.hidden = ok;
                if !ok {
                    tracing::debug!("笔刷光标：隐藏系统光标失败（非 X11 或无窗口），保持默认箭头");
                }
            } else {
                self.restore();
                self.hidden = false;
            }
        }
    }

    impl Default for BrushCursor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for BrushCursor {
        fn drop(&mut self) {
            // 兜底：绝不能把用户的光标留在"隐形"状态
            if self.hidden {
                self.restore();
            }
        }
    }

    impl BrushCursor {
        fn hide(&mut self) -> bool {
            // 惰性建连：连接失败就保持 None，下次切换再来（不影响主流程）
            if self.x.is_none() {
                self.x = Xcb::connect();
            }
            let Some(x) = self.x.as_mut() else {
                return false;
            };
            let Some(win) = x.top_level_under_pointer() else {
                return false;
            };
            match x.set_cursor(win, Some(x.invisible)) {
                Ok(()) => {
                    x.applied.clear();
                    x.applied.push(win);
                    // 只在状态翻转时打（不是每帧），开着 debug 就能确认接线是否生效
                    tracing::debug!("笔刷光标：已隐藏系统光标（窗口 {win}）");
                    true
                }
                Err(e) => {
                    tracing::debug!("笔刷光标：设置隐形 cursor 失败：{e}");
                    false
                }
            }
        }

        fn restore(&mut self) {
            let Some(x) = self.x.as_mut() else { return };
            // 先取出待恢复的窗口列表：避免同时可变/不可变借用 x
            let windows = std::mem::take(&mut x.applied);
            for win in windows {
                // XCB_NONE = 继承父窗口（root）的默认光标，即普通箭头
                match x.set_cursor(win, None) {
                    Ok(()) => tracing::debug!("笔刷光标：已恢复系统光标（窗口 {win}）"),
                    Err(e) => tracing::debug!("笔刷光标：恢复系统光标失败：{e}"),
                }
            }
        }
    }

    impl Xcb {
        fn connect() -> Option<Self> {
            let (conn, screen_num) = XCBConnection::connect(None).ok()?;
            let screen = conn.setup().roots.get(screen_num)?;
            let (root, screen) = (screen.root, (screen.width_in_pixels, screen.height_in_pixels));
            let invisible = create_invisible_cursor(&conn).ok()?;
            Some(Self {
                conn,
                root,
                screen,
                invisible,
                applied: Vec::new(),
            })
        }

        /// 指针所在的顶层窗口，且必须"几乎铺满屏幕"（= 我们的全屏覆盖层）。
        fn top_level_under_pointer(&self) -> Option<Window> {
            let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
            if reply.child == x11rb::NONE {
                return None; // 指针直接在 root 上
            }
            // 向上走到 root 的直接子窗口
            let mut win = reply.child;
            for _ in 0..8 {
                let tree = self.conn.query_tree(win).ok()?.reply().ok()?;
                if tree.parent == self.root || tree.parent == x11rb::NONE {
                    break;
                }
                win = tree.parent;
            }
            // 尺寸校验：避免把别的应用的小窗口当成覆盖层
            let geom = self.conn.get_geometry(win).ok()?.reply().ok()?;
            let (sw, sh) = self.screen;
            if !covers_screen(geom.width, geom.height, sw, sh) {
                tracing::debug!(
                    "笔刷光标：指针下窗口 {}x{} 未铺满屏幕（{}x{}），不动它的光标",
                    geom.width, geom.height, sw, sh
                );
                return None;
            }
            Some(win)
        }

        fn set_cursor(&self, win: Window, cursor: Option<u32>) -> Result<(), x11rb::errors::ConnectionError> {
            let aux = ChangeWindowAttributesAux::default().cursor(Some(cursor.unwrap_or(x11rb::NONE)));
            self.conn.change_window_attributes(win, &aux)?;
            self.conn.flush()?;
            Ok(())
        }
    }

    /// 建一个 1×1 全透明 cursor：source 与 mask 都是**未填充**的 1×1 pixmap，
    /// 于是整体 alpha 全 0、画出来什么都不显示。
    ///
    /// 与 gpui_linux 自己的 `create_invisible_cursor` 完全同一套做法
    /// （crates/gpui_linux/src/linux/x11/client.rs），只是我们拿不到它的 API。
    pub(super) fn create_invisible_cursor(conn: &XCBConnection) -> Result<u32, Box<dyn std::error::Error>> {
        let root = conn.setup().roots[0].root;
        let pixmap = conn.generate_id()?;
        conn.create_pixmap(1, pixmap, root, 1, 1)?;
        let cursor = conn.generate_id()?;
        conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
        conn.free_pixmap(pixmap)?;
        conn.flush()?;
        Ok(cursor)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    /// 非 Linux：不做任何事（保持默认光标 + 自绘圆盘）。
    pub struct BrushCursor;

    impl BrushCursor {
        pub fn new() -> Self {
            Self
        }

        pub fn set_hidden(&mut self, _want_hidden: bool) {}
    }

    impl Default for BrushCursor {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub use imp::BrushCursor;


#[cfg(test)]
mod tests {
    use super::*;

    /// "铺满屏幕"判据：全屏覆盖层通过，小窗口/弹层不通过（防止误改别的应用的窗口）。
    #[test]
    fn covers_screen_only_accepts_fullscreen_windows() {
        assert!(covers_screen(1920, 1080, 1920, 1080));
        // 1080 × 0.9 = 972：留出任务栏余量仍算铺满，明显矮于屏幕则不算
        assert!(covers_screen(1920, 1000, 1920, 1080), "留出任务栏余量也算铺满");
        assert!(!covers_screen(1920, 900, 1920, 1080));
        assert!(!covers_screen(400, 300, 1920, 1080), "小窗口不能算覆盖层");
        assert!(!covers_screen(1920, 300, 1920, 1080), "只有一条不算");
        assert!(!covers_screen(0, 0, 1920, 1080));
    }

    /// 真连 X 服务器走一遍隐形 cursor 的完整流程。
    ///
    /// 系统光标不进截图，所以"看不见鼠标了"无法用截图验证；但这条能验证**机制**
    /// 本身在我们的环境里真的可用：建连、造 1×1 透明 cursor、把它设到窗口上、
    /// 再设回 XCB_NONE。全部请求用 `.check()` 收 X 协议错误（否则错了也静默）。
    ///
    /// 按需运行：`DISPLAY=:1 cargo test x11_invisible_cursor -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn x11_invisible_cursor_roundtrip() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{
            ChangeWindowAttributesAux, ConnectionExt, CreateWindowAux, WindowClass,
        };
        use x11rb::xcb_ffi::XCBConnection;

        let (conn, screen_num) = XCBConnection::connect(None).expect("需要可用的 DISPLAY");
        let root = conn.setup().roots[screen_num].root;

        // 自建一个窗口当靶子（不映射，避免在你屏幕上闪一个窗）
        let win = conn.generate_id().expect("generate_id");
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            100,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::default(),
        )
        .expect("create_window")
        .check()
        .expect("create_window 被 X 拒绝");

        let cursor = super::imp::create_invisible_cursor(&conn).expect("创建隐形 cursor");

        // 设上隐形 cursor
        conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::default().cursor(Some(cursor)),
        )
        .expect("change_window_attributes(隐藏)")
        .check()
        .expect("隐藏光标被 X 拒绝");

        // 恢复：XCB_NONE = 继承父窗口的默认箭头
        conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::default().cursor(Some(x11rb::NONE)),
        )
        .expect("change_window_attributes(恢复)")
        .check()
        .expect("恢复光标被 X 拒绝");

        conn.destroy_window(win).expect("destroy_window");
        conn.flush().expect("flush");
        println!("OK: 隐形 cursor 建/设/还原 全部通过 X 服务器校验");
    }
}
