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
use gpui_component::button::Button;
use gpui_component::button::ButtonVariants;
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_platform::application;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::capture::CapturedFrame;
use crate::overlay::drawing::{DrawCommand, DrawingState, RGBA};
use crate::overlay::palette;
use crate::overlay::selection::SelectionState;
use crate::overlay::toolbar::{ToolButton, ToolbarState};
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
    tx: Sender<OverlayResult>,
    /// 键盘焦点句柄（让 Esc / Enter 能路由到这里）
    focus_handle: FocusHandle,
    /// 窗口交互模式：Selecting 还是 Editing
    mode: OverlayMode,
    /// 工具栏状态：当前选中的工具 / 颜色 / 线宽
    toolbar: ToolbarState,
    /// 标注历史：含 undo / redo
    drawing: DrawingState,
    /// 当前正在画的一笔（mouse_down 到 mouse_up 之间）
    in_progress: Option<DrawCommand>,
}

/// 覆盖窗口完成后回传给主线程的结果
///
/// `selection` 为 None 表示用户取消；否则 `selection` 是选区 bounds，
/// `commands` 是 DrawingState 中所有可见（未撤销）的标注命令。
pub struct OverlayResult {
    pub selection: Option<ub::Bounds>,
    pub commands: Vec<DrawCommand>,
}

impl OverlayView {
    fn new(
        frame: &CapturedFrame,
        screen_bounds: ub::Bounds,
        tx: Sender<OverlayResult>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            frame_image: build_render_image(frame),
            screen_bounds,
            selection: SelectionState::new(screen_bounds),
            tx,
            focus_handle: cx.focus_handle(),
            mode: OverlayMode::Selecting,
            toolbar: ToolbarState::default(),
            drawing: DrawingState::new(),
            in_progress: None,
        }
    }

    /// 发送结果并关闭窗口
    fn commit(&self, result: OverlayResult, window: &mut Window) {
        let _ = self.tx.send(result);
        window.remove_window();
    }

    /// 在 Editing 模式下，按当前 active_tool 启动一个新 DrawCommand
    ///
    /// 仅在 toolbar.active_tool 是绘图工具时被调用（调用方应已检查）。
    /// Text 工具的 content 暂留空字符串，等 Phase 3 文字输入接入；当前
    /// mouse_up 时空 content 会被忽略。
    fn begin_draw(&mut self, p: BoundsPoint) {
        let Some(tool) = self.toolbar.active_tool else { return };
        let color = self.toolbar.current_color;
        let lw = self.toolbar.line_width;
        let dp = crate::overlay::drawing::Point::new(p.x, p.y);
        self.in_progress = Some(match tool {
            ToolButton::Rectangle => DrawCommand::Rectangle {
                rect: (dp, dp),
                color,
                line_width: lw,
            },
            ToolButton::Arrow => DrawCommand::Arrow {
                from: dp,
                to: dp,
                color,
                line_width: lw,
            },
            ToolButton::Freehand => DrawCommand::Freehand {
                points: vec![dp],
                color,
                line_width: lw,
            },
            ToolButton::Text => DrawCommand::Text {
                anchor: dp,
                content: String::new(),
                font_size: self.toolbar.current_size,
                color: self.toolbar.current_color,
                max_width: self.selection.current().map(|sel| sel.size.x),
                weight: self.toolbar.current_weight,
            },
            ToolButton::Mosaic => DrawCommand::Mosaic {
                rect: (dp, dp),
                block_size: 12,
            },
            // 非绘图工具忽略
            ToolButton::ColorPicker | ToolButton::Undo | ToolButton::Redo
            | ToolButton::Bold | ToolButton::Finish | ToolButton::Cancel => return,
        });
    }

    /// 推进 in_progress 的当前点（鼠标拖动时调用）
    fn update_in_progress(&mut self, p: BoundsPoint) {
        let Some(cmd) = self.in_progress.as_mut() else { return };
        let dp = crate::overlay::drawing::Point::new(p.x, p.y);
        match cmd {
            DrawCommand::Rectangle { rect, .. } | DrawCommand::Mosaic { rect, .. } => {
                rect.1 = dp;
            }
            DrawCommand::Arrow { to, .. } => {
                *to = dp;
            }
            DrawCommand::Freehand { points, .. } => {
                points.push(dp);
            }
            // Text 暂不支持拖拽改内容
            DrawCommand::Text { .. } => {}
        }
    }

    /// 结束 in_progress：归一化 rect，过滤太小的图形，push 到 DrawingState
    fn finish_draw(&mut self) {
        let Some(cmd) = self.in_progress.take() else { return };
        let valid = match &cmd {
            DrawCommand::Rectangle { rect, .. } | DrawCommand::Mosaic { rect, .. } => {
                let w = (rect.0.x - rect.1.x).abs();
                let h = (rect.0.y - rect.1.y).abs();
                w >= 2.0 && h >= 2.0
            }
            DrawCommand::Arrow { from, to, .. } => {
                (from.x - to.x).abs() >= 2.0 || (from.y - to.y).abs() >= 2.0
            }
            DrawCommand::Freehand { points, .. } => points.len() >= 2,
            DrawCommand::Text { content, .. } => !content.is_empty(),
        };
        if !valid { return; }
        // 归一化 Rectangle/Mosaic 的 rect 为 (左上, 右下)
        let normalized = match cmd {
            DrawCommand::Rectangle { rect, color, line_width } => {
                let a = rect.0;
                let b = rect.1;
                DrawCommand::Rectangle {
                    rect: (
                        crate::overlay::drawing::Point::new(a.x.min(b.x), a.y.min(b.y)),
                        crate::overlay::drawing::Point::new(a.x.max(b.x), a.y.max(b.y)),
                    ),
                    color,
                    line_width,
                }
            }
            DrawCommand::Mosaic { rect, block_size } => {
                let a = rect.0;
                let b = rect.1;
                DrawCommand::Mosaic {
                    rect: (
                        crate::overlay::drawing::Point::new(a.x.min(b.x), a.y.min(b.y)),
                        crate::overlay::drawing::Point::new(a.x.max(b.x), a.y.max(b.y)),
                    ),
                    block_size,
                }
            }
            other => other,
        };
        self.drawing.push(normalized);
    }

    /// 渲染浮动工具栏（在 Editing 模式下挂在选区下方）
    ///
    /// `sel` 是当前选区 bounds（screen 坐标），工具栏默认在选区下沿 + OFFSET_Y 处，
    /// 放不下时翻到选区上方；左对齐选区左沿。调用方负责只在 Editing 模式下挂载此节点。
    fn render_toolbar(
        &self,
        sel: ub::Bounds,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_tool = self.toolbar.active_tool;
        let can_undo = self.drawing.history_index > 0;
        let can_redo = self.drawing.history_index < self.drawing.commands.len();

        let mut row = div().flex().gap(px(TOOLBAR_GAP)).items_center();

        for &btn in ToolButton::ORDER {
            // Bold 单独走 render_bold_toggle（toggle 样式 + primary 高亮），
            // 不走统一的 render_tool_button 路径。
            if btn == ToolButton::Bold {
                row = row.child(render_bold_toggle(self, cx));
                continue;
            }

            let on_click = cx.listener(move |this, _ev, _window, cx| match btn {
                ToolButton::Rectangle | ToolButton::Arrow | ToolButton::Freehand
                | ToolButton::Text | ToolButton::Mosaic => {
                    this.toolbar.active_tool = if this.toolbar.active_tool == Some(btn) {
                        None
                    } else {
                        Some(btn)
                    };
                    cx.notify();
                }
                ToolButton::ColorPicker => {
                    // 每次点击 → 在 HSV 12 色调色板中循环下一个颜色
                    let swatch = palette::default_palette();
                    let cur = this.toolbar.current_color;
                    let idx = swatch.iter().position(|c| *c == cur).unwrap_or(0);
                    let next = (idx + 1) % swatch.len();
                    this.toolbar.current_color = swatch[next];
                    cx.notify();
                }
                ToolButton::Undo => {
                    this.drawing.undo();
                    cx.notify();
                }
                ToolButton::Redo => {
                    this.drawing.redo();
                    cx.notify();
                }
                ToolButton::Bold => {
                    // Bold 按钮本身由 render_bold_toggle 处理；这里只在
                    // 退路路径（未走独立渲染）时被调用。切换 Normal/Bold。
                    this.toolbar.current_weight = match this.toolbar.current_weight {
                        crate::overlay::drawing::FontWeight::Normal => {
                            crate::overlay::drawing::FontWeight::Bold
                        }
                        crate::overlay::drawing::FontWeight::Bold => {
                            crate::overlay::drawing::FontWeight::Normal
                        }
                    };
                    cx.notify();
                }
                ToolButton::Finish => {
                    let s = this.selection.current().or(Some(this.screen_bounds));
                    let cmds: Vec<DrawCommand> =
                        this.drawing.visible_commands().cloned().collect();
                    this.commit(OverlayResult { selection: s, commands: cmds }, _window);
                }
                ToolButton::Cancel => {
                    this.commit(
                        OverlayResult {
                            selection: None,
                            commands: vec![],
                        },
                        _window,
                    );
                }
            });

            let (active, disabled) = match btn {
                ToolButton::Rectangle | ToolButton::Arrow | ToolButton::Freehand
                | ToolButton::Text | ToolButton::Mosaic => (active_tool == Some(btn), false),
                ToolButton::Undo => (false, !can_undo),
                ToolButton::Redo => (false, !can_redo),
                _ => (false, false),
            };

            row = row.child(render_tool_button(btn, active, disabled, on_click));
        }

        // 字号选择器（Bold 按钮之后、Finish 之前）
        row = row.child(render_size_dropdown(self, cx));

        // 工具栏位置：选区下沿之下，左对齐选区左沿
        // 如果跑出屏幕底部 → 翻到选区上沿之上
        let screen_h = self.screen_bounds.origin.y + self.screen_bounds.size.y;
        let toolbar_h = TOOLBAR_BTN_SIZE + TOOLBAR_PAD * 2.0;
        let toolbar_y_below = sel.origin.y + sel.size.y + TOOLBAR_OFFSET_Y;
        let toolbar_y_above =
            sel.origin.y - TOOLBAR_OFFSET_Y - toolbar_h;
        let toolbar_y = if toolbar_y_below + toolbar_h + TOOLBAR_OFFSET_Y <= screen_h {
            toolbar_y_below
        } else if toolbar_y_above >= TOOLBAR_OFFSET_Y {
            toolbar_y_above
        } else {
            // 两边都放不下时强行贴屏幕底部
            (screen_h - toolbar_h - TOOLBAR_OFFSET_Y).max(TOOLBAR_OFFSET_Y)
        };
        let toolbar_x = sel.origin.x;

        div()
            .absolute()
            .top(px(toolbar_y))
            .left(px(toolbar_x))
            .bg(gpui::rgba(0x202020EE))
            .rounded_md()
            .p(px(TOOLBAR_PAD))
            .child(row)
    }
}

/// handle 视觉尺寸：边长（像素）
const HANDLE_VISUAL_SIZE: f32 = 8.0;
/// handle 命中容差的一半（与 selection::HANDLE_HALF_SIZE 保持一致）
const HANDLE_HIT_HALF: f32 = 8.0;

/// 工具栏按钮视觉尺寸（正方形边长，px）
const TOOLBAR_BTN_SIZE: f32 = 36.0;
/// 工具栏按钮之间间距
const TOOLBAR_GAP: f32 = 4.0;
/// 工具栏整体内边距
const TOOLBAR_PAD: f32 = 6.0;
/// 工具栏距离选区上沿的距离（px）
const TOOLBAR_OFFSET_Y: f32 = 8.0;

/// 把 ToolButton 映射到 gpui-component 的 Lucide 图标名
///
/// 找不到完美匹配时选语义最接近的。Pencil 等图标本组件未自带，
/// 用 Frame / Asterisk / LayoutDashboard 等近似替代。
fn icon_for(btn: ToolButton) -> IconName {
    match btn {
        ToolButton::Rectangle => IconName::Frame,
        ToolButton::Arrow => IconName::ArrowUp,
        ToolButton::Freehand => IconName::Asterisk,
        ToolButton::Text => IconName::SquareTerminal,
        ToolButton::Mosaic => IconName::LayoutDashboard,
        ToolButton::ColorPicker => IconName::Palette,
        ToolButton::Undo => IconName::Undo,
        ToolButton::Redo => IconName::Redo,
        ToolButton::Finish => IconName::Check,
        ToolButton::Cancel => IconName::Close,
        // gpui-component 自带 icons 目录里没有 bold.svg；用 Asterisk 作占位
        // （按钮同时带 "B" 文字标签兜底，看图仍是"加粗"语义）
        ToolButton::Bold => IconName::Asterisk,
    }
}

/// 渲染单个工具栏按钮
///
/// - active=true 时用 ButtonVariants::primary() 高亮
/// - disabled=true 时按钮变灰（不影响点击穿透，但视觉上提示不可用）
///
/// 关键点：gpui-component 的 Button 渲染是 `icon + label` 两段都
/// `when_some` 加进 h_flex；只设 `.icon()` 时只有图标，但不少主题
/// 下 compact 模式只放图标会因内容过窄而看不出 icon（icon 文字色 +
/// 主题 bg 接近时也容易"看着像空白"）。这里同时设 `.icon()` 和
/// `.label()`，让按钮始终有可见文本兜底。
fn render_tool_button(
    btn: ToolButton,
    active: bool,
    disabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let icon = icon_for(btn);
    let label = btn.label();
    let mut b = Button::new(("toolbar", btn as usize))
        .icon(icon)
        .label(label)
        .tooltip(label)
        .compact()
        .on_click(on_click);
    if disabled {
        b = b.disabled(true);
    }
    if active {
        b = b.primary();
    }
    b
}

/// 渲染 Bold 切换按钮
///
/// 点击切换 `toolbar.current_weight` Normal ↔ Bold；当前为 Bold 时用 primary 高亮。
/// 单独走这条路径而不是通用 render_tool_button，是因为：
/// 1) Bold 没有"active 工具"语义（active_tool 标记绘图工具，Bold 是文字属性开关）
/// 2) 需要独立计算 active 状态（current_weight == Bold）
/// 3) 后面紧跟字号下拉（B + 字号构成"文字属性"子组），不需要通用按钮间距
fn render_bold_toggle(
    view: &OverlayView,
    cx: &mut Context<OverlayView>,
) -> Button {
    let on_click = cx.listener(|this, _ev, _window, cx| {
        this.toolbar.current_weight = match this.toolbar.current_weight {
            crate::overlay::drawing::FontWeight::Normal => {
                crate::overlay::drawing::FontWeight::Bold
            }
            crate::overlay::drawing::FontWeight::Bold => {
                crate::overlay::drawing::FontWeight::Normal
            }
        };
        cx.notify();
    });
    let mut b = Button::new("toolbar-bold")
        .icon(IconName::Asterisk)
        .label("B")
        .tooltip("切换粗体")
        .compact()
        .on_click(on_click);
    if view.toolbar.current_weight == crate::overlay::drawing::FontWeight::Bold {
        b = b.primary();
    }
    b
}

/// 渲染字号选择器（v0.2 简化为一行紧凑按钮组）
///
/// gpui-component 的 `Select` 需要一个 `Entity<SelectState<...>>` + 复杂
/// `SearchableListDelegate` 实现，引入成本远高于本工具栏的实际需求；
/// 这里把 FONT_SIZES 每个值渲染成一个小按钮，当前选中状态用 primary 高亮。
/// 这样免去了新建 GPUI Entity / 实现 Delegate 的复杂度，且视觉上和
/// 工具栏其余按钮一致（与 Bold 紧挨成一组）。
fn render_size_dropdown(
    view: &OverlayView,
    cx: &mut Context<OverlayView>,
) -> impl IntoElement {
    use crate::overlay::toolbar::FONT_SIZES;
    let mut group = div().flex().gap(px(2.0)).items_center();
    for &size in FONT_SIZES {
        let label: gpui::SharedString = format!("{}px", size as i32).into();
        let on_click = cx.listener(move |this, _ev, _window, cx| {
            this.toolbar.current_size = size;
            cx.notify();
        });
        let mut b = Button::new(("toolbar-size", size as usize))
            .label(label.clone())
            .tooltip(label)
            .compact()
            .on_click(on_click);
        // 当前选中的字号高亮
        if (view.toolbar.current_size - size).abs() < f32::EPSILON {
            b = b.primary();
        }
        group = group.child(b);
    }
    group
}

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

/// RGBA → GPUI rgba u32（0xRRGGBBAA）
fn rgba_u32(c: RGBA) -> u32 {
    (u32::from(c.r) << 24)
        | (u32::from(c.g) << 16)
        | (u32::from(c.b) << 8)
        | u32::from(c.a)
}

/// 画一条指定粗细的实线（轴对齐 bounding box 实现，preview 用，不抗锯齿）
///
/// 严格意义上不是真正的"线"（是矩形），但对工具栏交互的视觉反馈够用。
fn paint_thick_line(x1: f32, y1: f32, x2: f32, y2: f32, lw: f32, color: RGBA, window: &mut Window) {
    let hsla = Hsla::from(gpui::rgba(rgba_u32(color)));
    let half = (lw / 2.0).max(0.5);
    let min_x = x1.min(x2) - half;
    let min_y = y1.min(y2) - half;
    let w = (x1.max(x2) - x1.min(x2)) + lw;
    let h = (y1.max(y2) - y1.min(y2)) + lw;
    window.paint_quad(gpui::quad(
        Bounds {
            origin: gpui::point(gpui::px(min_x), gpui::px(min_y)),
            size: Size::new(gpui::px(w), gpui::px(h)),
        },
        gpui::px(0.),
        hsla,
        gpui::px(0.),
        gpui::transparent_black(),
        Default::default(),
    ));
}

/// 画空心矩形边框（4 条粗线）
fn paint_rect_outline(x: f32, y: f32, w: f32, h: f32, lw: f32, color: RGBA, window: &mut Window) {
    paint_thick_line(x, y, x + w, y, lw, color, window);
    paint_thick_line(x, y + h, x + w, y + h, lw, color, window);
    paint_thick_line(x, y, x, y + h, lw, color, window);
    paint_thick_line(x + w, y, x + w, y + h, lw, color, window);
}

/// 把一个 DrawCommand 渲染到 window 上（Phase 3 preview，Phase 4 也会复用）
fn paint_command(cmd: &DrawCommand, window: &mut Window) {
    match *cmd {
        DrawCommand::Rectangle { rect, color, line_width } => {
            let a = rect.0;
            let b = rect.1;
            let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
            let w = (b.x - a.x).abs();
            let h = (b.y - a.y).abs();
            paint_rect_outline(x1, y1, w, h, line_width, color, window);
        }
        DrawCommand::Arrow { from, to, color, line_width } => {
            // 主线
            paint_thick_line(from.x, from.y, to.x, to.y, line_width, color, window);
            // 箭头三角：to → 后退 head_len，再分别往左右垂直方向偏 head_w
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1.0 { return; }
            let ux = dx / len;
            let uy = dy / len;
            let head_len = (line_width * 6.0).max(8.0);
            let head_w = (line_width * 3.0).max(4.0);
            let bx = to.x - ux * head_len;
            let by = to.y - uy * head_len;
            let px = -uy;
            let py = ux;
            let p1x = bx + px * head_w;
            let p1y = by + py * head_w;
            let p2x = bx - px * head_w;
            let p2y = by - py * head_w;
            paint_thick_line(to.x, to.y, p1x, p1y, line_width, color, window);
            paint_thick_line(to.x, to.y, p2x, p2y, line_width, color, window);
            paint_thick_line(p1x, p1y, p2x, p2y, line_width, color, window);
        }
        DrawCommand::Freehand { ref points, color, line_width } => {
            for w in points.windows(2) {
                paint_thick_line(w[0].x, w[0].y, w[1].x, w[1].y, line_width, color, window);
            }
        }
        DrawCommand::Text { anchor, ref content, font_size, color, weight, .. } => {
            // Phase 3 简化：画一个文字占位框（按字符数估算宽度）
            // `weight` 仅作元数据保留 — GPUI paint 阶段不支持 weight，
            // 真正按 weight 栅格化在 CPU 阶段（commands.rs::rasterize_text）。
            let _ = weight;
            let char_w = font_size * 0.6;
            let w = char_w * content.chars().count().max(1) as f32;
            let h = font_size;
            paint_rect_outline(anchor.x, anchor.y, w, h, 1.0, color, window);
        }
        DrawCommand::Mosaic { rect, block_size } => {
            // Phase 3 简化：画斜线网格（实际栅格化在 Phase 4 做）
            let a = rect.0;
            let b = rect.1;
            let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
            let w = (b.x - a.x).abs();
            let h = (b.y - a.y).abs();
            let bs = block_size.max(2) as f32;
            let grid_color = RGBA::new(0xFF, 0xFF, 0xFF, 0xC0);
            let mut x = x1;
            while x < x1 + w {
                paint_thick_line(x, y1, x, y1 + h, 1.0, grid_color, window);
                x += bs;
            }
            let mut y = y1;
            while y < y1 + h {
                paint_thick_line(x1, y, x1 + w, y, 1.0, grid_color, window);
                y += bs;
            }
        }
    }
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

        // 收集 in_progress + 可见命令给 canvas paint 闭包用
        let in_progress = self.in_progress.clone();
        let visible_cmds: Vec<DrawCommand> =
            self.drawing.visible_commands().cloned().collect();

        let paint_canvas = canvas(
            move |_, _, _| (in_progress, visible_cmds),
            move |_, (in_progress, visible_cmds), window, _| {
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

                    // 2.5) 可见的 DrawCommand + 当前 in_progress（在 dim 之上、border 之下）
                    for cmd in &visible_cmds {
                        paint_command(cmd, window);
                    }
                    if let Some(ref ip) = in_progress {
                        paint_command(ip, window);
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
        let mut root = div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(paint_canvas.size_full());

        // Editing 模式下挂浮动工具栏；选区是 None 时仍可挂，但 render_toolbar
        // 用的是 selection.current()，工具栏会贴在 None 处 → 等 Editing 时一定存在
        if mode == OverlayMode::Editing {
            if let Some(sel) = self.selection.current() {
                root = root.child(self.render_toolbar(sel, cx));
            }
        }

        root
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, _| {
                    let p = to_bounds_point(ev.position);
                    // Editing 模式下分发：handle 命中 → 交给 SelectionState
                    // (Resizing)；active_tool 已选 + 点在选区内 → 开始绘图；
                    // 其他情况 → 交给 SelectionState (Moving / Creating)
                    if this.mode == OverlayMode::Editing {
                        if let Some(sel) = this.selection.current() {
                            // handle 命中（即使没 active_tool 也优先 resize）
                            if sel.hit_handle(p, HANDLE_HIT_HALF).is_some() {
                                this.selection.mouse_down(p);
                                return;
                            }
                            // active_tool 选了绘图工具 + 点在选区内 → 开始绘图
                            if this.toolbar.active_tool.is_some() && sel.contains(p) {
                                this.begin_draw(p);
                                return;
                            }
                        }
                    }
                    this.selection.mouse_down(p);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let p = to_bounds_point(ev.position);
                if this.in_progress.is_some() {
                    this.update_in_progress(p);
                } else {
                    this.selection.mouse_move(p);
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, _cx| {
                    // 先结束正在画的那一笔
                    if this.in_progress.is_some() {
                        this.finish_draw();
                        return;
                    }
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
                        let sel = this.selection.current();
                        let cmds: Vec<DrawCommand> = this
                            .drawing
                            .visible_commands()
                            .cloned()
                            .collect();
                        this.commit(OverlayResult { selection: sel, commands: cmds }, window);
                    }
                }),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.commit(OverlayResult { selection: None, commands: vec![] }, window);
                } else if ev.keystroke.key == "enter" {
                    // Enter 直接确认当前选区；没有选区则全屏
                    let sel = this.selection.current().or(Some(this.screen_bounds));
                    let cmds: Vec<DrawCommand> = this
                        .drawing
                        .visible_commands()
                        .cloned()
                        .collect();
                    this.commit(OverlayResult { selection: sel, commands: cmds }, window);
                } else if ev.keystroke.key == "z" && ev.keystroke.modifiers.control {
                    // Ctrl+Z 撤销 / Ctrl+Shift+Z 重做
                    if ev.keystroke.modifiers.shift {
                        this.drawing.redo();
                    } else {
                        this.drawing.undo();
                    }
                    cx.notify();
                }
            }))
    }
}

/// 在新线程里跑 GPUI 覆盖窗口，阻塞到用户完成/取消。
///
/// 返回值：完整会话结果（选区 + 可见的 DrawCommand 列表）；取消时 selection=None。
pub fn run_blocking(frame: CapturedFrame, screen_bounds: ub::Bounds) -> OverlayResult {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        application().run(move |cx: &mut App| {
            // gpui-component 必须在第一个窗口前初始化，否则全局主题/状态会 panic
            gpui_component::init(cx);

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
    // 主线程阻塞等结果；GPUI 线程退出时 Sender 被 drop，本端 recv 返回 Err → 视为取消
    rx.recv().unwrap_or(OverlayResult {
        selection: None,
        commands: vec![],
    })
}