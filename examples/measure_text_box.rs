//! 测量文字编辑框首行行盒顶相对 box 顶的偏移（校准 paint_command 的 origin_fy）。
//!
//! 复现 overlay/window.rs 中 text input 的精确结构：
//! box(top=100, h=47.6) → 顶部 spacer(6px) → flex_1 内容区 → Input(fs=24, line_height 1.5)。
//! 布局完成后用 range_to_bounds(0..1) 读首行行盒顶，与 box 顶做差，即为偏移。

use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputState};
use gpui_component::Sizable;
use gpui_platform::application;
use screenshot_rs::overlay::drawing::FontWeight;
use screenshot_rs::overlay::font::TEXT_FONT_FAMILY;

/// 持有 spawn 出的 Task，防止被 drop 时取消（GPUI Task 被 drop 会 abort 内部 future）
static MEASURE_TASK: Mutex<Option<Task<()>>> = Mutex::new(None);

struct BoxMeasure {
    input: Entity<InputState>,
}

impl Render for BoxMeasure {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top(px(100.0))
                    .left(px(50.0))
                    .w(px(124.0))
                    .h(px(47.6))
                    .bg(gpui::rgba(0x008000FF)) // 绿色背景便于定位 box 区域
                    .flex()
                    .flex_col()
                    .child(div().w_full().h(px(6.0)))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .child(
                                Input::new(&self.input)
                                    .appearance(false)
                                    .bordered(false)
                                    .text_color(gpui::rgba(0x000000FF))
                                    .with_size(gpui_component::Size::Size(px(24.0 / 0.875)))
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
    application()
        .with_assets(gpui_component_assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            let _ = cx.text_system().add_fonts(vec![
                std::borrow::Cow::Owned(FontWeight::Normal.font_bytes().to_vec()),
                std::borrow::Cow::Owned(FontWeight::Bold.font_bytes().to_vec()),
            ]);

            let view_handle: Arc<Mutex<Option<Entity<BoxMeasure>>>> =
                Arc::new(Mutex::new(None));
            let handle = view_handle.clone();

            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: Size::new(px(400.0), px(300.0)),
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
                            InputState::new(window, cx)
                                .placeholder("")
                                .auto_grow(1, 8)
                                .soft_wrap(false)
                        });
                        {
                            let window = &mut *window;
                            input.update(cx, move |state, cx| {
                                state.set_value(SharedString::from("文字蚊子"), window, cx);
                            });
                        }
                        let view = cx.new(|cx| BoxMeasure { input });
                        handle.lock().unwrap().replace(view.clone());
                        cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
                    },
                )
                .unwrap();

            *MEASURE_TASK.lock().unwrap() = Some(cx.spawn(async move |async_cx: &mut AsyncApp| {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(4000))
                    .await;
                let _ = window.update(async_cx, |_window, _win, cx| {
                    let Some(view) = view_handle.lock().unwrap().as_ref().cloned() else {
                        println!("BOX_MEASURE no view handle");
                        cx.quit();
                        return;
                    };
                    let state = view.read(cx).input.read(cx);
                    let lh = state.range_to_bounds(&(0..1));
                    match lh {
                        Some(b) => {
                            println!(
                                "BOX_MEASURE line1_top={:.2} box_top=100.0 offset={:+.2} line_h={:.2}",
                                b.origin.y,
                                b.origin.y - px(100.0),
                                b.size.height
                            );
                        }
                        None => println!("BOX_MEASURE range_to_bounds = None (not laid out)"),
                    }
                    cx.quit();
                });
            }));
        });
}
