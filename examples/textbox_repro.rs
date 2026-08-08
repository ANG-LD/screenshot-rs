//! 复现「编辑框首次自动扩增时文字左移」的最小示例。
//!
//! 结构完全照搬 src/overlay/window.rs render() 里的文字输入框：
//!   div.absolute().overflow_hidden().flex_col()
//!     → 6px 拖动条
//!     → div.relative().flex_1() → Input(bordered)
//! 每次 render 前先 measure_text_px 自动扩增框大小。
//!
//! 运行：DISPLAY=:99 cargo run --example textbox_repro
//! 键盘输入即打字，用于观察扩增瞬间首个字符是否左移被裁。

use gpui::{
    canvas, div, point, prelude::*, px, rgba, App, Bounds, Context, Entity, IntoElement,
    ParentElement, Render, Size, Window, WindowBackgroundAppearance, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::Sizable;
use gpui_component::Size as ComponentSize;
use gpui_platform::application;
use screenshot_rs::overlay::commands::{measure_line_advance_px, measure_text_px};
use screenshot_rs::overlay::drawing::FontWeight;
use screenshot_rs::overlay::font::TEXT_FONT_FAMILY;

const TO_X: f32 = 8.0;
const TO_Y: f32 = 8.0;
const SF: f32 = 1.0;
const FS: f32 = 24.0;

struct TextBoxRepro {
    input: Entity<InputState>,
    lx: f32,
    ly: f32,
    lw: f32,
    lh: f32,
}

impl TextBoxRepro {
    fn auto_size(&mut self, cx: &mut Context<Self>) {
        let value: String = self.input.read(cx).value().to_string();
        if !value.is_empty() {
            let (_, th, _, _) = measure_text_px(&value, FS, None, FontWeight::Normal);
            let adv = measure_line_advance_px(&value, FS, FontWeight::Normal);
            const INSET_X: f32 = 18.0;
            const RIGHT_MARGIN: f32 = 10.0;
            const MIN_W: f32 = 100.0;
            const MIN_H: f32 = 40.0;
            let new_w = if adv > 0.0 {
                (adv / SF + INSET_X + RIGHT_MARGIN).max(MIN_W)
            } else {
                MIN_W
            };
            let new_h = if th > 0.0 {
                (th / SF + TO_Y * 2.0).max(MIN_H)
            } else {
                MIN_H
            };
            self.lw = new_w;
            self.lh = new_h;
        }
    }
}

impl Render for TextBoxRepro {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.auto_size(cx);
        let (lx, ly, lw, lh) = (self.lx, self.ly, self.lw, self.lh);
        let input = self.input.clone();
        // 红色参考文本：完全复刻 paint_command 的渲染（anchor+8, line_height 1.5*fs）。
        // 放在 box 下方 60px（ly+60+8），避免被白色 Input 文字遮住，便于测量。
        let value: String = self.input.read(cx).value().to_string();
        let ref_paint = canvas(
            move |_, _, _| (value, lx, ly),
            move |_, (value, lx, ly), window, cx| {
                if value.is_empty() {
                    return;
                }
                let mut run = window.text_style().to_run(value.len());
                run.font.family = gpui::SharedString::from(TEXT_FONT_FAMILY);
                run.color = gpui::Hsla::from(rgba(0xFF0000FF));
                let shaped = window.text_system().shape_line(
                    gpui::SharedString::from(value),
                    px(FS),
                    &[run],
                    None,
                );
                let lh = gpui::px(FS * 1.5);
                let input_lh = window.line_height();
                let base = window.pixel_snap(px(ly + 60.0 + 8.0));
                let origin_y = base + gpui::px((input_lh - lh).as_f32() / 2.0);
                use std::sync::atomic::{AtomicBool as AB, Ordering as Ord};
                static DBG1: AB = AB::new(false);
                if !DBG1.swap(true, Ord::SeqCst) {
                    eprintln!(
                        "DBG scale={:.3} line_height={:.1} base_snap={:.1} shift={:.1} origin_y={:.1} lh={:.1}",
                        window.scale_factor(),
                        input_lh.as_f32(),
                        base.as_f32(),
                        (input_lh - lh).as_f32() / 2.0,
                        origin_y.as_f32(),
                        lh.as_f32(),
                    );
                }
                use std::sync::atomic::{AtomicBool, Ordering};
                static DBG_PRINTED: AtomicBool = AtomicBool::new(false);
                if !DBG_PRINTED.swap(true, Ordering::SeqCst) {
                    eprintln!(
                        "DBG2 fs={} asc={:.2} desc={:.2} w={:.1}",
                        FS,
                        shaped.ascent.as_f32(),
                        shaped.descent.as_f32(),
                        shaped.width().as_f32(),
                    );
                }
                let _ = shaped.paint(
                    point(px(lx + 8.0), origin_y),
                    lh,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            },
        );
        div()
            .size_full()
            .bg(rgba(0x333333FF))
            .child(ref_paint)
            .child(
                div()
                    .absolute()
                    .top(px(ly))
                    .left(px(lx))
                    .w(px(lw))
                    .h(px(lh))
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("text-drag-bar")
                            .w_full()
                            .h(px(6.0))
                            .bg(rgba(0x00FFFFFF))
                            .rounded_t_md()
                            .cursor_move(),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .child(
                                Input::new(&input)
                                    .appearance(false)
                                    .bordered(false)
                                    .text_color(rgba(0xFFFFFFFF))
                                    .border_1()
                                    .border_color(gpui::rgba(0x00FF00FF))
                                    .with_size(ComponentSize::Size(px(FS / 0.875 / SF)))
                                    .font_weight(gpui::FontWeight::NORMAL)
                                    .font_family(gpui::SharedString::from(TEXT_FONT_FAMILY))
                                    .line_height(gpui::relative(1.5)),
                            ),
                    ),
            )
    }
}

fn main() {
    application()
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(100.), px(100.)),
                        size: Size::new(px(600.), px(400.)),
                    })),
                    window_background: WindowBackgroundAppearance::default(),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("textbox_repro".into()),
                        ..Default::default()
                    }),
                    kind: WindowKind::Normal,
                    is_movable: true,
                    is_resizable: true,
                    focus: true,
                    window_decorations: Some(WindowDecorations::Server),
                    ..Default::default()
                },
                move |window, cx| {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder("输入文字…")
                            .auto_grow(3, 8)
                            .soft_wrap(false)
                    });
                    input.update(cx, |state, cx| {
                        state.focus(window, cx);
                    });

                    let view = cx.new(|_cx| TextBoxRepro {
                        input: input.clone(),
                        lx: 80.0,
                        ly: 80.0,
                        lw: 100.0,
                        lh: 48.0,
                    });
                    let view_id = view.entity_id();
                    cx.subscribe(
                        &input,
                        move |_e: Entity<InputState>, _ev: &InputEvent, cx: &mut App| {
                            cx.notify(view_id);
                        },
                    )
                    .detach();
                    window.activate_window();
                    cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
                },
            );
        });
}
