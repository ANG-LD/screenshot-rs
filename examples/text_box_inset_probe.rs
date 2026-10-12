//! 实测「光标到末尾时整行左移、首字被裁」这条历史 bug，在 gpui-kit（gpui-base 0.7.3）
//! 下是否复现。
//!
//! 背景：`RIGHT_MARGIN` 过去是本项目给 gpui-component 打的补丁（10 → 2）。迁移后该常量
//! 归 gpui-base 且回到上游默认的 10，而应用侧框宽改成「文字宽 + 2×inset_x」的对称留白，
//! 推算（见 `commands.rs` 的 `TEXT_BOX_MIN_PAD_X`）认为不再触发滚动。**推算必须实测**。
//!
//! 复现 overlay/window.rs 的精确几何：box.w = adv + 2×inset_x，box 内左侧再让出
//! `text_box_pad_x`，Textarea 自己还带 8px；把光标推到末尾后读 `scroll_offset` 与首字 x。
//! 期望：scroll_x ≈ 0，首字 x ≈ box_left + inset_x。若 scroll_x 明显为负（首字左移），
//! 说明这条 bug 在新库复现，需要调 `TEXT_BOX_MIN_PAD_X` 或 vendored gpui-base。
//!
//! 用法：`cargo run --example text_box_inset_probe -- 14`（参数是字号，默认 14）。
//! 会在屏幕上开一个半透明小窗约 3.5 秒后自动退出。

use std::sync::{Arc, Mutex};

use gpui::component::input::{Textarea, TextareaState};
use gpui::component::Sizable;
use gpui::platform::application;
use gpui::prelude::*;
use gpui::*;
use screenshot_rs::overlay::commands::{
    measure_line_advance_px, text_box_inset_x, text_box_pad_x,
};
use screenshot_rs::overlay::drawing::FontWeight;
use screenshot_rs::overlay::font::TEXT_FONT_FAMILY;

/// 探针文本框左边缘的窗口坐标（便于把测得 x 换算成相对缩进）。
const BOX_LEFT: f32 = 50.0;
const BOX_TOP: f32 = 100.0;

/// 持有 spawn 出的 Task，防止被 drop 时取消（GPUI Task 被 drop 会 abort 内部 future）
static PROBE_TASK: Mutex<Option<Task<()>>> = Mutex::new(None);

struct Probe {
    input: Entity<TextareaState>,
    fs: f32,
    box_w: f32,
    pad_x: f32,
    inset_x: f32,
}

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            div()
                .absolute()
                .top(px(BOX_TOP))
                .left(px(BOX_LEFT))
                .w(px(self.box_w))
                .h(px(self.fs * 1.5 + 24.0))
                .bg(gpui::rgba(0x008000FF)) // 绿色背景便于定位 box 区域
                .flex()
                .flex_col()
                .child(div().w_full().h(px(6.0)))
                .child(
                    // 与 overlay/window.rs 一致：这一层同时定宽、左侧让出 box 内边距
                    div()
                        .relative()
                        .flex_1()
                        .pl(px(self.pad_x))
                        .child(
                            Textarea::new(&self.input)
                                .appearance(false)
                                .bordered(false)
                                .text_color(gpui::rgba(0x000000FF))
                                .with_size(gpui::component::Size::Size(px(self.fs / 0.875)))
                                .font_weight(match FontWeight::Normal {
                                    FontWeight::Bold => gpui::FontWeight::BOLD,
                                    FontWeight::Normal => gpui::FontWeight::NORMAL,
                                })
                                .font_family(SharedString::from(TEXT_FONT_FAMILY))
                                .line_height(gpui::relative(1.5)),
                        ),
                ),
        )
    }
}

fn main() {
    let fs: f32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(14.0);
    // 16 个 CJK 字符：足够长，能压到编辑器的右边界
    let text = "文字蚊子文字蚊子文字蚊子文字蚊子";
    let weight = FontWeight::Normal;

    // 与 overlay/window.rs 的 textbox auto-grow 同一套算式（探针窗口 scale_factor=1）
    let sf = 1.0f32;
    let adv_px = measure_line_advance_px(text, fs, weight);
    let pad_x = text_box_pad_x(fs);
    let inset_x = text_box_inset_x(fs);
    let box_w = adv_px / sf + inset_x * 2.0;
    println!(
        "INSET_PROBE fs={fs} adv_px={adv_px:.2} pad_x={pad_x:.2} inset_x={inset_x:.2} box_w={box_w:.2}"
    );

    application()
        .with_assets(gpui::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui::component::init(cx);
            let _ = cx.text_system().add_fonts(vec![
                std::borrow::Cow::Owned(FontWeight::Normal.font_bytes().to_vec()),
                std::borrow::Cow::Owned(FontWeight::Bold.font_bytes().to_vec()),
            ]);

            let view_handle: Arc<Mutex<Option<Entity<Probe>>>> = Arc::new(Mutex::new(None));
            let handle = view_handle.clone();

            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: Size::new(px(1400.0), px(400.0)),
                        })),
                        window_background: WindowBackgroundAppearance::Transparent,
                        titlebar: None,
                        kind: WindowKind::PopUp,
                        is_movable: false,
                        is_resizable: false,
                        focus: true,
                        ..Default::default()
                    },
                    move |window, cx| {
                        let input = cx.new(|cx| {
                            TextareaState::new(window, cx)
                                .placeholder("")
                                .auto_grow(1, 8)
                                .soft_wrap(false)
                        });
                        {
                            let window = &mut *window;
                            input.update(cx, move |state, cx| {
                                state.set_value(SharedString::from(text), window, cx);
                                // 把光标推到末尾：auto-grow 的测量只在末尾光标下才暴露
                                // 「光标 + RIGHT_MARGIN 越界 → 整行左移」这条路径。
                                state.focus(window, cx);
                                state.select_all(window, cx);
                            });
                        }
                        let view = cx.new(|_cx| Probe {
                            input,
                            fs,
                            box_w,
                            pad_x,
                            inset_x,
                        });
                        handle.lock().unwrap().replace(view.clone());
                        cx.new(|cx| gpui::component::Root::new(view, window, cx))
                    },
                )
                .unwrap();

            *PROBE_TASK.lock().unwrap() = Some(cx.spawn(async move |async_cx: &mut AsyncApp| {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(3500))
                    .await;
                let _ = window.update(async_cx, |_window, _win, cx| {
                    let Some(view) = view_handle.lock().unwrap().as_ref().cloned() else {
                        println!("INSET_PROBE 无视图句柄");
                        cx.quit();
                        return;
                    };
                    let state = view.read(cx).input.read(cx);
                    let scroll = state.scroll_offset();
                    let first = state.range_to_bounds(&(0..1)).map(|b| b.origin.x);
                    let cursor = state.cursor_position();
                    let expected = px(BOX_LEFT) + px(view.read(cx).inset_x);
                    let first_rel = first.map(|x| (x - px(BOX_LEFT)).as_f32());
                    println!("INSET_PROBE cursor={cursor:?} scroll_x={:.2}", scroll.x);
                    match first {
                        Some(x) => println!(
                            "INSET_PROBE first_glyph_x={:.2} 相对缩进={:.2}（期望 inset_x={:.2}, pad+8）verdict={}",
                            x.as_f32(),
                            first_rel.unwrap_or(f32::NAN),
                            view.read(cx).inset_x,
                            if scroll.x.as_f32() <= -0.5 {
                                "SCROLLED:整行左移，历史 bug 复现"
                            } else if (x - expected).as_f32().abs() < 0.5 {
                                "OK:无左滚，缩进等于 inset_x"
                            } else {
                                "MISMATCH:未左滚但缩进与 inset_x 不符"
                            }
                        ),
                        None => println!("INSET_PROBE first_glyph_x=None（未完成布局）"),
                    }
                    cx.quit();
                });
            }));
        });
}
