//! 全屏覆盖窗口：把捕获的帧作为背景 + 半透明 dim + 选区矩形边框。
//!
//! 用户拖拽选区，松开鼠标后选区 bounds 通过 mpsc 发回主线程；
//! 主线程据此裁剪原帧并写入剪贴板。Esc / 关闭窗口 → 取消。
//!
//! GPUI 主线程由 `run_blocking` 在 std::thread 中拉起；调用方在 channel 上阻塞等待结果。

use std::sync::mpsc::Sender;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Render, RenderImage, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, canvas, div, point, prelude::*, px, quad, rgba,
};
use gpui_platform::application;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::capture::CapturedFrame;
use crate::overlay::selection::SelectionState;
use crate::utils::bounds::{self as ub, Point as BoundsPoint};

/// 覆盖窗口交互状态机
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayMode {
    /// 还没选 / 正在拖一个新选区
    Selecting,
    /// 已选完，可调大小 / 标注 / 完成
    Editing,
}

/// GPUI 视图：覆盖窗口内容
pub struct OverlayView {
    /// 捕获帧的 GPUI 渲染图（已转 BGRA）
    frame_image: Arc<RenderImage>,
    /// 屏幕原始 f32 边界
    screen_bounds: ub::Bounds,
    /// 选区状态机
    selection: SelectionState,
    /// 选区结果回调
    tx: Sender<Option<ub::Bounds>>,
    /// 键盘焦点句柄（让 Esc / Enter 能路由到这里）
    focus_handle: FocusHandle,
    /// 窗口交互模式：Selecting 还是 Editing
    mode: OverlayMode,
}

impl OverlayView {
    fn new(
        frame: &CapturedFrame,
        screen_bounds: ub::Bounds,
        tx: Sender<Option<ub::Bounds>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            frame_image: build_render_image(frame),
            screen_bounds,
            selection: SelectionState::new(screen_bounds),
            tx,
            focus_handle: cx.focus_handle(),
            mode: OverlayMode::Selecting,
        }
    }

    /// 发送结果并关闭窗口
    fn commit(&self, result: Option<ub::Bounds>, window: &mut Window) {
        let _ = self.tx.send(result);
        window.remove_window();
    }
}

/// handle 视觉尺寸：边长（像素）
const HANDLE_VISUAL_SIZE: f32 = 8.0;

/// RGBA → BGRA 通道 swap（GPUI RenderImage 用 BGRA）
fn rgba_to_bgra(pixels: &mut [u8]) {
    for c in pixels.chunks_exact_mut(4) {
        c.swap(0, 2);
    }
}

fn build_render_image(frame: &CapturedFrame) -> Arc<RenderImage> {
    let mut pixels = frame.pixels.clone();
    rgba_to_bgra(&mut pixels);
    let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(frame.width, frame.height, pixels)
        .expect("CapturedFrame 像素长度必须与 width*height*4 一致");
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)))
}

/// 把 GPUI 像素坐标转成 SelectionState 用的 f32 点（utils::bounds::Point）
fn to_bounds_point(p: Point<Pixels>) -> BoundsPoint {
    BoundsPoint::new(f32::from(p.x), f32::from(p.y))
}

impl Render for OverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame_image = self.frame_image.clone();
        let selection_bounds = self.selection.current();
        let screen_bounds = self.screen_bounds;
        let mode = self.mode;

        // 把屏幕 f32 边界转成 GPUI Pixels（用于 dim 矩形）
        let screen_x = px(screen_bounds.origin.x);
        let screen_y = px(screen_bounds.origin.y);
        let screen_w = px(screen_bounds.size.x);
        let screen_h = px(screen_bounds.size.y);

        let paint_canvas = canvas(
            move |_, _, _| {},
            move |_, _, window, _| {
                let win_bounds = window.bounds();

                // 1) 把捕获帧作为全屏背景
                let _ = window.paint_image(
                    win_bounds,
                    Default::default(),
                    frame_image.clone(),
                    0,
                    false,
                );

                // 2) 半透明 dim 遮罩（选区外）
                let dim = Hsla::from(rgba(0x000000AA));

                if let Some(sel) = selection_bounds {
                    let sel_x = px(sel.origin.x);
                    let sel_y = px(sel.origin.y);
                    let sel_w = px(sel.size.x.max(1.0));
                    let sel_h = px(sel.size.y.max(1.0));

                    // 上方 dim
                    if sel.origin.y > screen_bounds.origin.y {
                        let h = sel.origin.y - screen_bounds.origin.y;
                        window.paint_quad(quad(
                            Bounds {
                                origin: point(screen_x, screen_y),
                                size: Size::new(screen_w, px(h)),
                            },
                            px(0.),
                            dim,
                            px(0.),
                            gpui::transparent_black(),
                            Default::default(),
                        ));
                    }
                    // 下方 dim
                    let bottom_y = sel.origin.y + sel.size.y;
                    if bottom_y < screen_bounds.origin.y + screen_bounds.size.y {
                        let h = screen_bounds.origin.y + screen_bounds.size.y - bottom_y;
                        window.paint_quad(quad(
                            Bounds {
                                origin: point(screen_x, px(bottom_y)),
                                size: Size::new(screen_w, px(h)),
                            },
                            px(0.),
                            dim,
                            px(0.),
                            gpui::transparent_black(),
                            Default::default(),
                        ));
                    }
                    // 左 dim
                    if sel.origin.x > screen_bounds.origin.x {
                        let w = sel.origin.x - screen_bounds.origin.x;
                        window.paint_quad(quad(
                            Bounds {
                                origin: point(screen_x, sel_y),
                                size: Size::new(px(w), sel_h),
                            },
                            px(0.),
                            dim,
                            px(0.),
                            gpui::transparent_black(),
                            Default::default(),
                        ));
                    }
                    // 右 dim
                    let right_x = sel.origin.x + sel.size.x;
                    if right_x < screen_bounds.origin.x + screen_bounds.size.x {
                        let w = screen_bounds.origin.x + screen_bounds.size.x - right_x;
                        window.paint_quad(quad(
                            Bounds {
                                origin: point(px(right_x), sel_y),
                                size: Size::new(px(w), sel_h),
                            },
                            px(0.),
                            dim,
                            px(0.),
                            gpui::transparent_black(),
                            Default::default(),
                        ));
                    }

                    // 3) 选区边框（4 条 1px 蓝绿色 quad）
                    let border = Hsla::from(rgba(0x00E5FFCC));
                    let bw = px(1.0);
                    // 上
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(sel_x, sel_y),
                            size: Size::new(sel_w, bw),
                        },
                        px(0.),
                        gpui::transparent_black(),
                        bw,
                        border,
                        Default::default(),
                    ));
                    // 下
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(sel_x, px(sel.origin.y + sel.size.y - 1.0)),
                            size: Size::new(sel_w, bw),
                        },
                        px(0.),
                        gpui::transparent_black(),
                        bw,
                        border,
                        Default::default(),
                    ));
                    // 左
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(sel_x, sel_y),
                            size: Size::new(bw, sel_h),
                        },
                        px(0.),
                        gpui::transparent_black(),
                        bw,
                        border,
                        Default::default(),
                    ));
                    // 右
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(px(sel.origin.x + sel.size.x - 1.0), sel_y),
                            size: Size::new(bw, sel_h),
                        },
                        px(0.),
                        gpui::transparent_black(),
                        bw,
                        border,
                        Default::default(),
                    ));

                    // 4) Editing 模式下额外画 8 个 handle（小白方 + 蓝边）
                    if mode == OverlayMode::Editing {
                        let handle_fill = Hsla::from(rgba(0xFFFFFFFFu32));
                        let handle_border = Hsla::from(rgba(0x0066CCFFu32));
                        let half = px(HANDLE_VISUAL_SIZE / 2.0);
                        let edge = px(HANDLE_VISUAL_SIZE);
                        for hp in sel.handle_positions() {
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(px(hp.x) - half, px(hp.y) - half),
                                    size: Size::new(edge, edge),
                                },
                                px(0.),
                                handle_fill,
                                px(1.0),
                                handle_border,
                                Default::default(),
                            ));
                        }
                    }
                } else {
                    // 没选区：整屏 dim（提示用户拖拽）
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(screen_x, screen_y),
                            size: Size::new(screen_w, screen_h),
                        },
                        px(0.),
                        dim,
                        px(0.),
                        gpui::transparent_black(),
                        Default::default(),
                    ));
                }
            },
        );

        // Canvas 自身不能挂鼠标 handler；外面包一层 div 来接收事件。
        // track_focus 让 Esc/Enter 能路由到 on_key_down 监听器。
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(paint_canvas.size_full())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    this.selection.mouse_down(to_bounds_point(ev.position));
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                this.selection.mouse_move(to_bounds_point(ev.position));
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, _cx| {
                    this.selection.mouse_up();
                    // 任意大小选区都接受（clip_region 自然处理 < 1 像素情况）
                    match this.mode {
                        OverlayMode::Selecting => {
                            // 第一次选完：如果选区 > 0 进入 Editing 状态
                            //（Editing 模式才能看到 handle、调整大小、调用工具栏）
                            if let Some(b) = this.selection.current() {
                                if b.size.x > 1.0 && b.size.y > 1.0 {
                                    this.mode = OverlayMode::Editing;
                                    return;
                                }
                            }
                            // 没有有效选区 → 保持 Selecting（等用户继续拖）
                        }
                        OverlayMode::Editing => {
                            // 在 Editing 模式下松开只是结束 resize / moving，
                            // 不 commit；用户必须点"完成"或按 Enter 才确认
                        }
                    }
                    // Selecting 模式下若松手无有效选区则 commit 当前 bounds（兼容老路径）
                    if this.mode == OverlayMode::Selecting {
                        let result = this.selection.current();
                        this.commit(result, window);
                    }
                }),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, _cx| {
                if ev.keystroke.key == "escape" {
                    this.commit(None, window);
                } else if ev.keystroke.key == "enter" {
                    // Enter 直接确认当前选区；没有选区则全屏
                    let result = this.selection.current().or(Some(this.screen_bounds));
                    this.commit(result, window);
                }
            }))
    }
}

/// 在新线程里跑 GPUI 覆盖窗口，阻塞到用户完成/取消。
///
/// 返回值：
/// - `Some(bounds)`：用户确认（拖拽后松开 / 按 Enter）
/// - `None`：用户取消（按 Esc / 关闭窗口 / 选区过小）
pub fn run_blocking(frame: CapturedFrame, screen_bounds: ub::Bounds) -> Option<ub::Bounds> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        application().run(move |cx: &mut App| {
            // 用主屏尺寸作为窗口尺寸
            let win_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(screen_bounds.size.x), px(screen_bounds.size.y)),
            });

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Fullscreen(win_bounds)),
                    window_background: WindowBackgroundAppearance::Transparent,
                    titlebar: None,
                    kind: WindowKind::Normal,
                    is_movable: false,
                    is_resizable: false,
                    focus: true,
                    ..Default::default()
                },
                move |window, cx| {
                    let view = cx.new(|cx| OverlayView::new(&frame, screen_bounds, tx, cx));
                    // 主动把焦点给到 view 自己的 focus_handle，
                    // 这样 track_focus 的 div 能收到键盘事件
                    let handle = view.read(cx).focus_handle.clone();
                    handle.focus(window, cx);
                    view
                },
            )
            .expect("open_window 失败");

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
    });
    // 主线程阻塞等结果；GPUI 线程退出时 Sender 被 drop，本端 recv 返回 Err 当作取消
    rx.recv().ok().flatten()
}