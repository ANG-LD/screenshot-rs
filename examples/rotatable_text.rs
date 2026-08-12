//! 可旋转文本输入框 Demo
//!
//! 演示在 GPUI 中实现文本旋转编辑：
//! - 左侧：普通 Input 输入文字
//! - 右侧：Canvas 实时渲染旋转后的文字（离屏 rasterize + paint_image）
//! - 底部：旋转角度滑块 + 快捷按钮
//!
//! 保证旋转中心=文字包围盒中心，360° 与初始位置完全重合。

use gpui::{
    canvas, div, point, prelude::*, px, rgba, App, Bounds, Context, Entity, IntoElement,
    ParentElement, QuitMode, Render, RenderImage, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowKind, WindowOptions,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::Sizable;
use gpui_component::Size as ComponentSize;
use gpui_platform::application;
use screenshot_rs::capture::CapturedFrame;
use screenshot_rs::overlay::commands::rasterize_text;
use screenshot_rs::overlay::drawing::{FontWeight, RGBA};
use smallvec::SmallVec;
use std::sync::Arc;

struct RotatableTextDemo {
    input: Entity<InputState>,
    rotation: f32,
    font_size: f32,
    slider_dragging: bool,
}

fn main() {
    application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(200.), px(200.)),
                        size: Size::new(px(780.), px(580.)),
                    })),
                    window_background: WindowBackgroundAppearance::default(),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("可旋转文本输入框 Demo".into()),
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
                            .placeholder("输入文字...")
                            .auto_grow(3, 8)
                    });
                    input.update(cx, |state, cx| {
                        state.set_value("Hello, GPUI!", window, cx);
                    });

                    let view = cx.new(|_cx| RotatableTextDemo {
                        input: input.clone(),
                        rotation: 45.0,
                        font_size: 32.0,
                        slider_dragging: false,
                    });

                    let view_id = view.entity_id();
                    cx.subscribe(
                        &input,
                        move |_entity: Entity<InputState>, _event: &InputEvent, cx: &mut App| {
                            cx.notify(view_id);
                        },
                    )
                    .detach();

                    window.activate_window();
                    cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
                },
            )
            .unwrap();
        });
}

impl RotatableTextDemo {
    fn angle_to_slider(&self, angle: f32, track_w: f32) -> f32 {
        ((angle + 180.0) / 360.0 * track_w).clamp(0.0, track_w)
    }

    fn slider_to_angle(&self, x: f32, track_w: f32) -> f32 {
        (x / track_w).clamp(0.0, 1.0) * 360.0 - 180.0
    }

    /// 计算旋转后包围盒尺寸。
    /// 对 0°/90°/180°/270°/360° 等精确角度做 sin/cos 值快照，
    /// 避免 f32::sin(2π) ≈ -8.7e-8 导致 ceil 多 1px 的漂移。
    fn rotated_size(w: f32, h: f32, angle_deg: f32) -> (f32, f32) {
        let angle_rad = angle_deg * std::f32::consts::PI / 180.0;
        let (mut sin, mut cos) = angle_rad.sin_cos();
        // 消除浮点噪声：sin/cos 接近 0 或 ±1 时强制归零/归整
        if sin.abs() < 1e-6 {
            sin = 0.0;
        }
        if cos.abs() < 1e-6 {
            cos = 0.0;
        }
        if (sin.abs() - 1.0).abs() < 1e-6 {
            sin = sin.signum();
        }
        if (cos.abs() - 1.0).abs() < 1e-6 {
            cos = cos.signum();
        }
        let rw = (w * cos).abs() + (h * sin).abs();
        let rh = (w * sin).abs() + (h * cos).abs();
        (rw.ceil().max(4.0), rh.ceil().max(4.0))
    }

    /// 扫描 frame 中非零像素，返回包围盒
    fn find_content_bounds(pixels: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let mut min_x = w;
        let mut min_y = h;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        for y in 0..h {
            for x in 0..w {
                let idx = ((y * w + x) * 4) as usize;
                if pixels[idx + 3] > 0 {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        if max_x < min_x || max_y < min_y {
            None
        } else {
            Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
        }
    }

    /// 渲染旋转后的文字，返回 (pixels, width, height, unrotated_w, unrotated_h)
    fn render_rotated_text(&self, cx: &mut Context<Self>) -> Option<(Vec<u8>, u32, u32, f32, f32)> {
        let value = self.input.read(cx).value();
        if value.is_empty() {
            return None;
        }
        let content: String = String::from(value);
        let fs = self.font_size;
        let rot = self.rotation;
        let max_w = Some(400.0);
        let color = RGBA::new(205, 214, 244, 255);

        // 阶段 1：旋转 0° 渲染，测量文字包围盒（像素级精确）
        let probe_w = (max_w.unwrap_or(500.0) + 100.0) as u32;
        let probe_h = 600u32;
        let mut probe_frame = CapturedFrame {
            width: probe_w,
            height: probe_h,
            pixels: vec![0; (probe_w * probe_h * 4) as usize],
        };
        let probe_anchor_x = probe_w as f32 * 0.15;
        let probe_anchor_y = probe_h as f32 * 0.4;
        let _ = rasterize_text(
            &mut probe_frame,
            (probe_anchor_x, probe_anchor_y),
            (probe_anchor_x, probe_anchor_y),
            &content,
            fs,
            color,
            max_w,
            FontWeight::Normal,
            0.0,
        );

        let content_bounds = Self::find_content_bounds(&probe_frame.pixels, probe_w, probe_h)?;
        let tw = content_bounds.2 as f32;
        let th = content_bounds.3 as f32;

        // 文字包围盒中心相对于 anchor 的偏移（像素精确）
        let text_cx = content_bounds.0 as f32 + tw / 2.0;
        let text_cy = content_bounds.1 as f32 + th / 2.0;
        let offset_x = text_cx - probe_anchor_x;
        let offset_y = text_cy - probe_anchor_y;

        // 阶段 2：计算旋转后包围盒，构建精确大小的帧
        let (rw, rh) = Self::rotated_size(tw, th, rot);
        let frame_w = rw as u32;
        let frame_h = rh as u32;
        let mut frame = CapturedFrame {
            width: frame_w,
            height: frame_h,
            pixels: vec![0; (frame_w * frame_h * 4) as usize],
        };

        // 锚点让 pixel 包围盒中心 = 帧中心，这样 rasterize_text 内部的
        // glyph 中心 ≈ 帧中心，360° 旋转后位置精确重合
        let anchor_x = frame_w as f32 / 2.0 - offset_x;
        let anchor_y = frame_h as f32 / 2.0 - offset_y;

        let _ = rasterize_text(
            &mut frame,
            (anchor_x, anchor_y),
            (frame_w as f32 / 2.0, frame_h as f32 / 2.0),
            &content,
            fs,
            color,
            max_w,
            FontWeight::Normal,
            rot,
        );

        // 裁剪到实际内容区域（旋转后可能稍大于旋转前包围盒）
        let cropped = Self::find_content_bounds(&frame.pixels, frame_w, frame_h)
            .map(|(x, y, cw, ch)| Self::crop_frame(&frame.pixels, frame_w, frame_h, x, y, cw, ch))
            .unwrap_or((frame.pixels, frame_w, frame_h));

        Some((cropped.0, cropped.1, cropped.2, tw, th))
    }

    fn crop_frame(
        pixels: &[u8],
        src_w: u32,
        _src_h: u32,
        x: u32,
        y: u32,
        crop_w: u32,
        crop_h: u32,
    ) -> (Vec<u8>, u32, u32) {
        let mut out = vec![0u8; (crop_w * crop_h * 4) as usize];
        for row in 0..crop_h {
            let src_start = ((y + row) * src_w + x) * 4;
            let dst_start = (row * crop_w) * 4;
            let len = (crop_w * 4) as usize;
            out[dst_start as usize..dst_start as usize + len]
                .copy_from_slice(&pixels[src_start as usize..src_start as usize + len]);
        }
        (out, crop_w, crop_h)
    }
}

impl Render for RotatableTextDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rotation = self.rotation;
        let font_size = self.font_size;
        let slider_w = 300.0_f32;

        let thumb_x = self.angle_to_slider(rotation, slider_w);
        let input_entity = self.input.clone();
        let text_len = self.input.read(cx).value().chars().count();
        let rotated_img = self.render_rotated_text(cx);

        // 预览尺寸：用未旋转文字尺寸或旋转后尺寸
        let (preview_w, preview_h) = match &rotated_img {
            Some((_, rw, rh, _, _)) => ((*rw as f32).max(60.0), (*rh as f32).max(40.0)),
            None => (100.0, 40.0),
        };

        div()
            .size_full()
            .bg(rgba(0x1E1E2EFF))
            .text_color(rgba(0xCDD6F4FF))
            .flex()
            .flex_col()
            .pt(px(24.0))
            // 标题
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .px(px(32.0))
                    .mb(px(8.0))
                    .child("可旋转文本输入框 Demo"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgba(0xA6ADC8FF))
                    .px(px(32.0))
                    .mb(px(20.0))
                    .child("旋转中心 = 文字包围盒中心，360° 与初始位置完全重合"),
            )
            // 主内容：左右布局
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(32.0))
                    .w_full()
                    .px(px(32.0))
                    .flex_1()
                    // 左：输入框
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .flex_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgba(0xA6ADC8FF))
                                    .child("输入文本："),
                            )
                            .child(
                                div()
                                    .bg(rgba(0x313244FF))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgba(0x45475AFF))
                                    .p(px(12.0))
                                    .min_h(px(80.0))
                                    .child(
                                        Input::new(&input_entity)
                                            .appearance(false)
                                            .with_size(ComponentSize::Large)
                                            .text_color(rgba(0xCDD6F4FF)),
                                    ),
                            ),
                    )
                    // 右：旋转预览（宽高与文字内容匹配）
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgba(0xA6ADC8FF))
                                    .child(format!("旋转预览 ({}°)：", rotation as i32)),
                            )
                            .child(
                                div()
                                    .bg(rgba(0x181825FF))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgba(0x45475AFF))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    // 预览尺寸动态匹配旋转后的包围盒
                                    .w(px(preview_w + 32.0))
                                    .h(px(preview_h + 32.0))
                                    .min_w(px(100.0))
                                    .min_h(px(60.0))
                                    .child({
                                        let img_data = rotated_img;
                                        canvas(
                                            move |_, _, _| img_data,
                                            move |_bounds, data, window, _cx| {
                                                if let Some((mut pixels, w, h, _, _)) = data {
                                                    // RGBA → BGRA
                                                    for chunk in pixels.chunks_exact_mut(4) {
                                                        chunk.swap(0, 2);
                                                    }
                                                    if let Some(buf) = image::ImageBuffer::<
                                                        image::Rgba<u8>,
                                                        _,
                                                    >::from_raw(w, h, pixels)
                                                    {
                                                        let img = Arc::new(RenderImage::new(
                                                            SmallVec::from_elem(
                                                                image::Frame::new(buf),
                                                                1,
                                                            ),
                                                        ));
                                                        let _ = window.paint_image(
                                                            Bounds {
                                                                origin: point(px(0.), px(0.)),
                                                                size: Size::new(
                                                                    px(w as f32),
                                                                    px(h as f32),
                                                                ),
                                                            },
                                                            Default::default(),
                                                            img,
                                                            0,
                                                            false,
                                                        );
                                                    }
                                                }
                                            },
                                        )
                                        .size_full()
                                    }),
                            ),
                    ),
            )
            // 底部控制栏
            .child(
                div()
                    .w_full()
                    .px(px(32.0))
                    .pt(px(16.0))
                    .pb(px(16.0))
                    .bg(rgba(0x313244FF))
                    .border_t_1()
                    .border_color(rgba(0x45475AFF))
                    .flex()
                    .flex_row()
                    .gap(px(24.0))
                    .items_center()
                    // 角度信息
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .w(px(60.0))
                            .child(format!("{:.0}°", rotation)),
                    )
                    // 滑块
                    .child(
                        div()
                            .w(px(slider_w))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .relative()
                            .cursor_ew_resize()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener({
                                    let thumb_x = thumb_x;
                                    move |this,
                                          event: &gpui::MouseDownEvent,
                                          _window,
                                          cx| {
                                        this.slider_dragging = true;
                                        let pos_x: f32 = event.position.x.into();
                                        let track_start = pos_x - thumb_x;
                                        this.rotation = this
                                            .slider_to_angle(pos_x - track_start, slider_w);
                                        cx.notify();
                                    }
                                }),
                            )
                            .on_mouse_move(cx.listener({
                                move |this, event: &gpui::MouseMoveEvent, _window, cx| {
                                    if this.slider_dragging {
                                        let pos_x: f32 = event.position.x.into();
                                        let track_x = pos_x
                                            - this.angle_to_slider(this.rotation, slider_w);
                                        this.rotation =
                                            this.slider_to_angle(pos_x - track_x, slider_w);
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_mouse_up(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.slider_dragging = false;
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .w(px(slider_w))
                                    .h(px(4.0))
                                    .bg(rgba(0x585B70FF))
                                    .rounded_full()
                                    .absolute()
                                    .left(px(0.)),
                            )
                            .child(
                                div()
                                    .w(px(16.0))
                                    .h(px(16.0))
                                    .bg(rgba(0x89B4FAFF))
                                    .rounded_full()
                                    .absolute()
                                    .left(px(thumb_x - 8.0)),
                            ),
                    )
                    // 快捷角度
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child(make_angle_btn(-90.0, rotation, cx))
                            .child(make_angle_btn(-45.0, rotation, cx))
                            .child(make_angle_btn(0.0, rotation, cx))
                            .child(make_angle_btn(45.0, rotation, cx))
                            .child(make_angle_btn(90.0, rotation, cx)),
                    )
                    // 分隔
                    .child(div().w(px(1.0)).h(px(20.0)).bg(rgba(0x585B70FF)))
                    // 字号
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgba(0xA6ADC8FF))
                            .child(format!("字号: {:.0}px", font_size)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child(make_size_btn(16.0, font_size, cx))
                            .child(make_size_btn(24.0, font_size, cx))
                            .child(make_size_btn(32.0, font_size, cx))
                            .child(make_size_btn(48.0, font_size, cx)),
                    )
                    // 字符数
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgba(0x6C7086FF))
                            .child(format!("{} 字符", text_len)),
                    ),
            )
    }
}

fn make_angle_btn(angle: f32, current: f32, cx: &mut Context<RotatableTextDemo>) -> gpui::Div {
    let active = (angle - current).abs() < 1.0;
    div()
        .text_xs()
        .px(px(8.0))
        .py(px(4.0))
        .bg(if active {
            rgba(0x89B4FAFF)
        } else {
            rgba(0x45475AFF)
        })
        .text_color(if active {
            rgba(0x1E1E2EFF)
        } else {
            rgba(0xCDD6F4FF)
        })
        .rounded_md()
        .cursor_pointer()
        .hover(|d| {
            if !active {
                d.bg(rgba(0x585B70FF))
            } else {
                d
            }
        })
        .child(format!("{}°", angle as i32))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.rotation = angle;
                cx.notify();
            }),
        )
}

fn make_size_btn(size: f32, current: f32, cx: &mut Context<RotatableTextDemo>) -> gpui::Div {
    let active = (size - current).abs() < 0.5;
    div()
        .text_xs()
        .px(px(8.0))
        .py(px(4.0))
        .bg(if active {
            rgba(0x89B4FAFF)
        } else {
            rgba(0x45475AFF)
        })
        .text_color(if active {
            rgba(0x1E1E2EFF)
        } else {
            rgba(0xCDD6F4FF)
        })
        .rounded_md()
        .cursor_pointer()
        .hover(|d| {
            if !active {
                d.bg(rgba(0x585B70FF))
            } else {
                d
            }
        })
        .child(format!("{}px", size as i32))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.font_size = size;
                cx.notify();
            }),
        )
}
