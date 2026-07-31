//! 全屏覆盖窗口：把捕获的帧作为背景 + 半透明 dim + 选区矩形边框。
//!
//! 用户拖拽选区，松开鼠标后选区 bounds 通过 mpsc 发回主线程；
//! 主线程据此裁剪原帧并写入剪贴板。Esc / 关闭窗口 → 取消。
//!
//! GPUI 主线程由 `run_blocking` 在 std::thread 中拉起；调用方在 channel 上阻塞等待结果。

use std::sync::mpsc::Sender;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Hsla, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, Rems, Render, RenderImage, Size, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowKind, WindowOptions,
    canvas,
    div, point,
    prelude::*, px, quad, rgba,
};
use gpui_component::ActiveTheme;
use gpui_component::button::Button;
use gpui_component::button::ButtonVariants;
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::Sizable;
use gpui_component::Icon;
use gpui_component::popover::Popover;
use gpui_platform::application;
use image::{Frame, ImageBuffer, Rgba};
#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use smallvec::SmallVec;

use crate::capture::CapturedFrame;
use crate::overlay::drawing::{DrawCommand, DrawingState, FontWeight, RGBA};
use crate::overlay::palette;
use crate::overlay::selection::SelectionState;
use crate::overlay::toolbar::{ToolButton, ToolbarPopup, ToolbarState};
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
    /// 屏幕边界（逻辑像素，与 GPUI 坐标系一致）
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

    /// Text 工具：是否正在编辑一段文字
    ///
    /// Text 的交互模式与 Rectangle/Arrow/Freehand 不同：
    /// Rectangle/Arrow/Freehand 是"按下→拖动→松开"的一次性画图；
    /// Text 是"点击→弹输入→输入文字→Enter 提交"。
    /// 因此用独立 state 跟踪文字编辑会话，不复用 `in_progress`。
    ///
    /// 用 gpui_component::input::InputState 而不是手撸 String 拼接：
    /// InputState 自带 IME 合成支持，能正确处理中文输入法（拼音/五笔等）
    /// 的组合过程——手撸 on_key_down 只能捕获单个按键事件，IME 合成期间
    /// 的事件（Process、compositionstart 等）全部丢失。
    text_input: Option<Entity<gpui_component::input::InputState>>,

    /// Text 工具：文字锚点（逻辑像素）—— Text 命令的 anchor
    text_input_anchor: BoundsPoint,

    /// Text 工具：输入框完整 rect（逻辑像素，与 GPUI 坐标系一致）
    text_input_rect: ub::Bounds,

    /// Text 工具：拖动 / resize 模式（拖顶部 bar 移动整框、拖角 resize）
    text_input_drag: Option<TextDragState>,

    /// Text 工具：文字已提交，Input 仅作展示（无拖拽条、手柄、边框）
    text_input_finalized: bool,

    /// 已提交文字对应的 DrawingState.commands 中的索引，用于重新编辑时移除
    text_input_cmd_idx: Option<usize>,

    /// 原始捕获帧像素（RGBA），用于 OCR 等需要像素数据的操作
    frame_pixels: Vec<u8>,
    /// 捕获帧宽度（物理像素）
    frame_width: u32,
    /// 捕获帧高度（物理像素）
    frame_height: u32,

    /// OCR 工具：选中的识别区域（None 表示尚未框选）
    ocr_rect: Option<ub::Bounds>,
    /// OCR 工具：识别结果文字
    ocr_result: Option<String>,
    /// OCR 工具：是否正在识别中
    ocr_loading: bool,
    /// OCR 工具：框选拖拽起点（None 表示未在拖拽）
    ocr_drag_start: Option<BoundsPoint>,

    /// Tooltip：工具栏 div 当前是否被鼠标悬停（用于 root.on_mouse_down 判断
    /// 点击是否落在工具栏上）。工具栏按钮宽高随图标+中文标签动态变化，
    /// 预估矩形（compute_toolbar_bounds）不準；改用 on_mouse_move/on_mouse_down
    /// 在工具栏根 div 上的真实事件来挂标志。
    toolbar_hovered: bool,
    /// OCR 结果面板是否被鼠标按下（用于 root.on_mouse_down 判断，避免 prevent_default 阻断按钮 click 事件）
    ocr_panel_hovered: bool,

    /// 当前选中的已绘制命令索引（DrawingState.commands 中的实际索引）
    selected_cmd_actual_idx: Option<usize>,

    /// 对选中命令的活跃拖拽操作
    cmd_drag: Option<CmdDragState>,

    /// HiDPI 缩放因子（物理像素 / 逻辑像素）。
    ///
    /// screen_bounds 和所有鼠标交互使用逻辑像素（与 GPUI 坐标系一致），
    /// commit 时乘以 scale_factor 转回物理像素供 app.rs 裁剪/栅格化。
    scale_factor: f32,

    /// dim 遮罩透明度（0.0 = 完全透明，1.0 = 最终效果）。
    ///
    /// 窗口打开时从 0 动画过渡到 1，减轻突然出现的撕裂感。
    dim_opacity: f32,
    /// 淡入动画起始时刻
    animation_start: std::time::Instant,
}

/// 文字输入框拖动 / resize 状态
#[derive(Debug, Clone, Copy)]
struct TextDragState {
    mode: TextDragMode,
    /// 鼠标按下时的 root 坐标（logical pixels）
    start_mouse: BoundsPoint,
    /// 按下时输入框的原始 rect（logical pixels）
    start_rect: ub::Bounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDragMode {
    /// 拖顶部 bar 移动整框
    Move,
    /// 四角 resize
    ResizeNW,
    ResizeNE,
    ResizeSW,
    ResizeSE,
    /// 四边中点 resize
    ResizeN,
    ResizeS,
    ResizeW,
    ResizeE,
}

/// 对已绘制命令的拖拽模式
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum CmdDragMode {
    /// 拖拽矩形的某个 resize 手柄
    ResizeRect { handle: crate::utils::bounds::Handle, start_rect: (crate::overlay::drawing::Point, crate::overlay::drawing::Point) },
    /// 拖拽矩形内部 → 整体移动
    MoveRect { start_rect: (crate::overlay::drawing::Point, crate::overlay::drawing::Point) },
    /// 拖拽箭头起点
    MoveArrowFrom { start_from: crate::overlay::drawing::Point, start_to: crate::overlay::drawing::Point },
    /// 拖拽箭头终点
    MoveArrowTo { start_from: crate::overlay::drawing::Point, start_to: crate::overlay::drawing::Point },
    /// 拖拽箭杆 → 整体移动
    MoveArrow { start_from: crate::overlay::drawing::Point, start_to: crate::overlay::drawing::Point },
}

/// 已绘制命令的拖拽状态
#[derive(Debug, Clone, Copy)]
struct CmdDragState {
    mode: CmdDragMode,
    /// 鼠标按下时的坐标（logical pixels）
    start_mouse: BoundsPoint,
    /// 命令在 DrawingState.commands 中的实际索引
    cmd_index: usize,
}

/// 覆盖窗口完成后回传给主线程的结果
///
/// `selection` 为 None 表示用户取消；否则 `selection` 是选区 bounds，
/// `commands` 是 DrawingState 中所有可见（未撤销）的标注命令。
pub struct OverlayResult {
    pub selection: Option<ub::Bounds>,
    pub commands: Vec<DrawCommand>,
    /// Pin 固定时跳过剪贴板复制
    pub no_clipboard: bool,
}

impl OverlayView {
    fn new(
        frame: &CapturedFrame,
        screen_bounds: ub::Bounds,
        scale_factor: f32,
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
            text_input: None,
            text_input_anchor: BoundsPoint::ZERO,
            text_input_rect: ub::Bounds::new(BoundsPoint::ZERO, BoundsPoint::ZERO),
            text_input_drag: None,
            text_input_finalized: false,
            text_input_cmd_idx: None,
            frame_pixels: frame.pixels.clone(),
            frame_width: frame.width,
            frame_height: frame.height,
            ocr_rect: None,
            ocr_result: None,
            ocr_loading: false,
            ocr_drag_start: None,
            toolbar_hovered: false,
            ocr_panel_hovered: false,
            selected_cmd_actual_idx: None,
            cmd_drag: None,
            scale_factor,
            dim_opacity: 0.0,
            animation_start: std::time::Instant::now(),
        }
    }

    /// 发送结果并关闭窗口
    ///
    /// 内部将 selection 和 commands 的坐标从逻辑像素转为物理像素，
    /// 以匹配 `CapturedFrame` 的物理像素坐标系（app.rs 的 clip_region 和
    /// commands.rs 的栅格化都用物理像素）。
    fn commit(&self, result: OverlayResult, window: &mut Window) {
        // 用实际窗口尺寸计算 canvas 坐标 → 帧物理像素的缩放比。
        // paint_image 会把帧图像缩放到 window.bounds() 内显示，因此
        // canvas 坐标要乘上 frame_dim / window_dim 才能正确映射到帧像素。
        // 不能用 run_blocking 里算的 scale_factor（只反映显示缩放），
        // 因为窗口实际大小可能与显示尺寸不一致（任务栏挤压等）。
        let wb = window.bounds();
        let sx = self.frame_width as f32 / f32::from(wb.size.width).max(1.0);
        let sy = self.frame_height as f32 / f32::from(wb.size.height).max(1.0);
        let selection = result.selection.map(|b| ub::Bounds {
            origin: ub::Point::new(b.origin.x * sx, b.origin.y * sy),
            size: ub::Point::new(b.size.x * sx, b.size.y * sy),
        });
        let commands: Vec<DrawCommand> =
            result.commands.into_iter().map(|c| scale_draw_command(c, sx, sy)).collect();

        tracing::info!(
            "commit: selection={:?} commands_count={}",
            selection,
            commands.len()
        );
        for (i, c) in commands.iter().enumerate() {
            match c {
                DrawCommand::Text { anchor, content, font_size, color, weight, max_width } => {
                    tracing::info!(
                        "cmd[{}] Text anchor=({},{}) size={} weight={:?} max_w={:?} color={:?} content={:?}",
                        i, anchor.x, anchor.y, font_size, weight, max_width, color, content
                    );
                }
                _ => tracing::info!("cmd[{}] {:?}", i, c),
            }
        }
        let no_clipboard = result.no_clipboard;
        let _ = self.tx.send(OverlayResult { selection, commands, no_clipboard });
        window.remove_window();
    }

    /// 在 Editing 模式下，按当前 active_tool 启动一个新 DrawCommand
    ///
    /// 仅在 toolbar.active_tool 是绘图工具时被调用（调用方应已检查）。
    /// Text 工具走独立的 `open_text_input` 流程（on_mouse_down 已拦截），
    /// 不在这里创建空 Text 命令——空 content 会被 finish_draw 过滤，等于死代码。
    fn begin_draw(&mut self, p: BoundsPoint) {
        let Some(tool) = self.toolbar.active_tool else { return };
        let color = self.toolbar.current_color;
        let lw = self.toolbar.line_width;
        let dp = crate::overlay::drawing::Point::new(p.x, p.y);
        tracing::info!("begin_draw: tool={:?} p=({},{}) color={:?} lw={}", tool, p.x, p.y, color, lw);
        self.in_progress = Some(match tool {
            ToolButton::Rectangle => DrawCommand::Rectangle {
                rect: (dp, dp),
                color,
                line_width: lw,
            },
            ToolButton::Ellipse => DrawCommand::Ellipse {
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
            ToolButton::Mosaic => {
                // 将工具栏线宽档位 (2/4/6/8) 映射为画笔大小 (8/16/24/32)
                let bs = self.toolbar.line_width * 4.0;
                let half = bs / 2.0;
                let stamp = (
                    crate::overlay::drawing::Point::new(dp.x - half, dp.y - half),
                    crate::overlay::drawing::Point::new(dp.x + half, dp.y + half),
                );
                DrawCommand::Mosaic {
                    regions: vec![stamp],
                    block_size: (self.toolbar.line_width * 2.0).max(4.0) as u32,
                    color: self.toolbar.current_color,
                }
            }
            // Text 走 open_text_input、Ocr 走框选识别（on_mouse_down 已拦截），
            // 其余非绘图工具忽略。
            ToolButton::Text | ToolButton::Ocr | ToolButton::ColorPicker | ToolButton::Undo
            | ToolButton::Redo | ToolButton::Bold | ToolButton::Finish | ToolButton::Cancel
            | ToolButton::Pin => return,
        });
    }

    /// 推进 in_progress 的当前点（鼠标拖动时调用）
    fn update_in_progress(&mut self, p: BoundsPoint) {
        let Some(cmd) = self.in_progress.as_mut() else { return };
        let dp = crate::overlay::drawing::Point::new(p.x, p.y);
        match cmd {
            DrawCommand::Rectangle { rect, .. }
            | DrawCommand::Ellipse { rect, .. } => {
                rect.1 = dp;
            }
            DrawCommand::Arrow { to, .. } => {
                *to = dp;
            }
            DrawCommand::Freehand { points, .. } => {
                points.push(dp);
            }
            DrawCommand::Mosaic { regions, block_size, .. } => {
                // 画笔模式：仅在距上一个 stamp 足够远时才添加新 stamp（避免重叠过多）
                let bs = self.toolbar.line_width * 4.0;
                let spacing = bs * 0.5; // 50% 重叠保证覆盖连续
                let half = bs / 2.0;
                let add = match regions.last() {
                    Some(last) => {
                        let cx = (last.0.x + last.1.x) / 2.0;
                        let cy = (last.0.y + last.1.y) / 2.0;
                        ((dp.x - cx).powi(2) + (dp.y - cy).powi(2)).sqrt() >= spacing
                    }
                    None => true,
                };
                if add {
                    let stamp = (
                        crate::overlay::drawing::Point::new(dp.x - half, dp.y - half),
                        crate::overlay::drawing::Point::new(dp.x + half, dp.y + half),
                    );
                    regions.push(stamp);
                }
                let _ = block_size;
            }
            // Text 暂不支持拖拽改内容
            DrawCommand::Text { .. } => {}
        }
    }

    /// 结束 in_progress：归一化 rect，过滤太小的图形，push 到 DrawingState
    fn finish_draw(&mut self) {
        let Some(cmd) = self.in_progress.take() else { return };
        tracing::info!("finish_draw: cmd={:?}", cmd);
        let valid = match &cmd {
            DrawCommand::Rectangle { rect, .. }
            | DrawCommand::Ellipse { rect, .. } => {
                let w = (rect.0.x - rect.1.x).abs();
                let h = (rect.0.y - rect.1.y).abs();
                w >= 2.0 && h >= 2.0
            }
            DrawCommand::Mosaic { regions, .. } => !regions.is_empty(),
            DrawCommand::Arrow { from, to, .. } => {
                (from.x - to.x).abs() >= 2.0 || (from.y - to.y).abs() >= 2.0
            }
            DrawCommand::Freehand { points, .. } => points.len() >= 2,
            DrawCommand::Text { content, .. } => !content.is_empty(),
        };
        if !valid { return; }
        // 归一化 Rectangle 的 rect 为 (左上, 右下)；Mosaic 每个 stamp 也归一化
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
            DrawCommand::Ellipse { rect, color, line_width } => {
                let a = rect.0;
                let b = rect.1;
                DrawCommand::Ellipse {
                    rect: (
                        crate::overlay::drawing::Point::new(a.x.min(b.x), a.y.min(b.y)),
                        crate::overlay::drawing::Point::new(a.x.max(b.x), a.y.max(b.y)),
                    ),
                    color,
                    line_width,
                }
            }
            DrawCommand::Mosaic { mut regions, block_size, color } => {
                // 归一化每个 stamp 为 (左上, 右下)
                for rect in regions.iter_mut() {
                    let a = rect.0;
                    let b = rect.1;
                    rect.0 = crate::overlay::drawing::Point::new(a.x.min(b.x), a.y.min(b.y));
                    rect.1 = crate::overlay::drawing::Point::new(a.x.max(b.x), a.y.max(b.y));
                }
                DrawCommand::Mosaic { regions, block_size, color }
            }
            other => other,
        };
        self.drawing.push(normalized);
        // 绘制完成后自动选中，方便用户二次编辑（Mosaic 画笔不支持拖拽编辑）
        match &self.drawing.commands.last() {
            Some(DrawCommand::Rectangle { .. })
            | Some(DrawCommand::Ellipse { .. })
            | Some(DrawCommand::Arrow { .. }) => {
                self.selected_cmd_actual_idx = Some(self.drawing.commands.len() - 1);
            }
            _ => {}
        }
    }

    /// undo/redo 后检查选中命令是否仍可见，不可见则清除选中
    fn check_selected_visible(&mut self) {
        if let Some(idx) = self.selected_cmd_actual_idx {
            if !self.drawing.is_visible(idx) {
                self.selected_cmd_actual_idx = None;
                self.cmd_drag = None;
            }
        }
    }

    /// 打开文字输入（Text 工具 + 选区内点击 → 弹一个 inline 输入框）
    ///
    /// 与 Rectangle/Arrow/Freehand 的"按下→拖动→松开"不同，Text 是
    /// "点击→弹输入→输入→Enter 提交"。所以这里不写 `in_progress`，
    /// 而是把 InputState 实体存在 `self.text_input` 里。
    ///
    /// InputState 自带 IME 支持，能正确处理中文输入法（拼音/五笔等）
    /// 的组合过程——手撸 on_key_down 只能捕获单个按键事件，IME 合成期间
    /// 的事件（Process、compositionstart 等）全部丢失。
    fn open_text_input(
        &mut self,
        p: BoundsPoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_text_input_impl(p, None, None, window, cx);
    }

    /// 打开文字输入并预填内容（用于重新编辑已固化的 Text 命令）
    fn open_text_input_with_content(
        &mut self,
        p: BoundsPoint,
        initial_content: String,
        old_max_w: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_text_input_impl(p, Some(initial_content), old_max_w, window, cx);
    }

    fn open_text_input_impl(
        &mut self,
        p: BoundsPoint,
        initial: Option<String>,
        max_w_override: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::input::{InputEvent, InputState};
        self.text_input_finalized = false;
        self.text_input_cmd_idx = None;
        self.text_input_anchor = p;
        // 初始输入框大小（logical pixels），auto_grow(3,8) 会根据内容自动扩展。
        // 新输入从紧凑大小起步，重新编辑时沿用旧宽度。
        // 裁剪到选区范围内，防止靠近边缘时文字框/手柄超出截图区域。
        let limits = self.selection.current().unwrap_or(self.screen_bounds);
        let w = max_w_override.unwrap_or(100.0);
        self.text_input_rect = ub::Bounds::new(p, BoundsPoint::new(p.x + w, p.y + 48.0))
            .clamp_inside(limits);
        tracing::debug!("open_text_input: anchor=({:.1}, {:.1}) initial={}", p.x, p.y, initial.is_some());

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入文字…（换行按 Enter，完成后点击框外）")
                .auto_grow(3, 8)
        });
        // 预填旧内容（重新编辑场景）
        if let Some(text) = initial {
            input.update(cx, |state, cx| {
                state.set_value(text.clone(), window, cx);
            });
        }
        // 立即 focus，让键盘事件路由到这里（IME 组合也走 focus handle）
        input.update(cx, |state, cx| {
            state.focus(window, cx);
        });

        // PopUp 窗口（override_redirect）在 X11 上可能不会被 WM 分配键盘焦点，
        // 手动调用 activate_window() 确保 X 服务器把键盘事件送到本窗口。
        window.activate_window();

        // 订阅 InputState 事件：
        //   PressEnter → 提交（push Text 命令）
        //   Blur       → 用户点击外部也提交（避免半截文字丢失）
        //   Change     → 通知重绘（输入框内容变化→重渲染）
        cx.subscribe_in(&input, window, |_this, _state, event, _window, cx| match event {
            // submit_on_enter=false → Enter/Shift+Enter 都用于换行（InputState
            // 内部已插入 \n）。PressEnter 不触发 finalize，让用户继续编辑；
            // 完成输入靠 Blur：点输入框外 / 点 Finish / 点其他工具都会 Blur。
            InputEvent::PressEnter { .. } => {
                cx.notify();
            }
            InputEvent::Blur => {
                tracing::debug!("text_input Blur: popup={}", _this.toolbar.popup.is_some());
                // 若 popover 正打开，失焦是因为用户点击了样式选项
                // （Bold/字号/颜色），此时不应提交文字——保留输入框
                // 让用户继续编辑。样式按钮 handler 会更新 toolbar 属性，
                // Input 组件下次 render 时自然应用新样式。
                if _this.toolbar.popup.is_some() {
                    return;
                }
                // 兜底：其他失焦场景（点输入框外、点工具栏、切工具）
                // 把当前文字落成命令，避免丢失。
                _this.finalize_text_input_if_active(cx);
            }
            InputEvent::Change => {
                cx.notify();
            }
            InputEvent::Focus => {
                // Focus 由 open_text_input 主动触发，无需额外处理
            }
        })
        .detach();

        self.text_input = Some(input);
        cx.notify();
    }

    /// 与 `finalize_text_input_impl` 同语义，但用于 commit 前的兜底——
    /// 若 `text_input` 已被 None（如 PressEnter 处理后已清），则跳过。
    /// 防止\"文本工具输了字 → 直接点 Finish / 按 Enter\"场景下：commit 收集到
    /// 的 commands 里没有该 Text 命令（因为 PressEnter 是 input 自己的事件，
    /// 但用户不点 input 直接按 Finish 按钮 commit 时 input 还活着没提交）。
    fn finalize_text_input_if_active(&mut self, cx: &mut Context<Self>) {
        if self.text_input_finalized {
            return;
        }
        let Some(state) = self.text_input.clone() else { return };
        self.finalize_text_input_impl(&state, cx);
    }

    fn finalize_text_input_impl(
        &mut self,
        state: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) {
        let value = state.read(cx).value();
        if !value.is_empty() {
            // 用当前 text_input_rect 的 origin 作 anchor、size.x 作 max_width。
            // 用户可能拖动 / resize 过框，那时 anchor 已不是最初点击位置。
            let anchor = self.text_input_rect.origin;
            let max_w = self.text_input_rect.size.x;
            tracing::info!(
                "finalize text: anchor=({:.1},{:.1}) rect_size=({:.1},{:.1}) fs={:.1} sf={:.1}",
                anchor.x, anchor.y, self.text_input_rect.size.x, self.text_input_rect.size.y,
                self.toolbar.current_size, self.scale_factor
            );
            // SharedString 没有 Display impl；用 String::from 走 From<SharedString>
            let content: String = String::from(value);
            self.drawing.push(DrawCommand::Text {
                anchor: crate::overlay::drawing::Point::new(anchor.x, anchor.y),
                content,
                font_size: self.toolbar.current_size,
                color: self.toolbar.current_color,
                max_width: Some(max_w),
                weight: self.toolbar.current_weight,
            });
            // 记录对应 DrawCommand 索引，便于重新编辑时移除。
            self.text_input_cmd_idx = Some(self.drawing.history_index - 1);
        }
        // 保留 Input 组件继续渲染文字，只隐藏 chrome（拖拽条、手柄、边框），
        // 避免因 canvas 渲染路径位置计算差异导致文字跳动。
        self.text_input_finalized = true;
        cx.notify();
    }

    /// 渲染浮动工具栏（在 Editing 模式下挂在选区下方）
    ///
    /// 工具栏一行布局（参考微信截图）：
    /// - 5 个绘图工具按钮（矩形 / 箭头 / 画笔 / 文字 / 马赛克）
    ///   - 第一次点击 → 选中工具
    ///   - 选中后再次点击 active 工具 → 浮出对应 popover
    /// - Undo / Redo
    /// - Finish (primary)
    /// - Cancel
    ///
    /// popover 内容由 active_tool 决定：
    /// - Text → 字号档位 + Bold + 颜色
    /// - Rectangle/Ellipse/Arrow/Freehand/Mosaic → 粗细档位 + 颜色
    ///
    /// 鼠标点击落在工具栏 div 上时由 root.on_mouse_down 默认 swallow（return）
    /// 阻止 selection.mouse_down 把选区打散，让 Button.on_click 正常触发。
    fn render_toolbar(
        &self,
        sel: ub::Bounds,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_tool = self.toolbar.active_tool;
        let can_undo = self.drawing.history_index > 0;
        let can_redo = self.drawing.history_index < self.drawing.commands.len();

        let (toolbar_x, toolbar_y, _toolbar_w, _toolbar_h) =
            compute_toolbar_bounds(sel, self.screen_bounds);

        let weak = cx.weak_entity();
        let row = div()
            .flex()
            .gap(px(TOOLBAR_GAP))
            .items_center()
            // 1) 绘图工具按钮组：6 个工具按钮，每个都是 Popover trigger
            //    (active_tool 在 Popover 内根据当前选中状态确定 popover kind)
            .child(render_tool_button_with_popover(
                ToolButton::Rectangle,
                active_tool == Some(ToolButton::Rectangle),
                weak.clone(),
                self,
                cx,
            ))
            .child(render_tool_button_with_popover(
                ToolButton::Ellipse,
                active_tool == Some(ToolButton::Ellipse),
                weak.clone(),
                self,
                cx,
            ))
            .child(render_tool_button_with_popover(
                ToolButton::Arrow,
                active_tool == Some(ToolButton::Arrow),
                weak.clone(),
                self,
                cx,
            ))
            .child(render_tool_button_with_popover(
                ToolButton::Freehand,
                active_tool == Some(ToolButton::Freehand),
                weak.clone(),
                self,
                cx,
            ))
            .child(render_tool_button_with_popover(
                ToolButton::Text,
                active_tool == Some(ToolButton::Text),
                weak.clone(),
                self,
                cx,
            ))
            .child(render_simple_button(
                ToolButton::Ocr,
                active_tool == Some(ToolButton::Ocr),
                false,
                weak.clone(),
            ))
            .child(render_tool_button_with_popover(
                ToolButton::Mosaic,
                active_tool == Some(ToolButton::Mosaic),
                weak.clone(),
                self,
                cx,
            ))
            // 2) Undo / Redo
            .child(render_simple_button(
                ToolButton::Undo,
                false,
                !can_undo,
                weak.clone(),
            ))
            .child(render_simple_button(
                ToolButton::Redo,
                false,
                !can_redo,
                weak.clone(),
            ))
            // 3) Pin / Cancel / Finish
            .child(render_simple_button(
                ToolButton::Pin,
                false,
                false,
                weak.clone(),
            ))
            .child(render_simple_button(
                ToolButton::Cancel,
                false,
                false,
                weak.clone(),
            ))
            .child(render_simple_button(
                ToolButton::Finish,
                false,
                false,
                weak,
            ));

        // 工具栏根 div
        // 通过 on_mouse_down 设 toolbar_hovered=true，配合 root.on_mouse_down 检查
        // 该标志 → 早 return，吞掉点击，避免 selection.mouse_down 把选区打散。
        // 之前用 compute_toolbar_bounds 的几何矩形判断，但按钮实际宽度（图标+
        // 中文标签）远超 32px 估算，导致 Finish / Cancel 落在估算外、被当成拖选。
        // 子 div 的 on_mouse_down 先于 root 的 listener 跑（冒泡顺序：内→外），
        // 所以 root.on_mouse_down 看到的 toolbar_hovered 是本次按下时刚 set 的值。
        // on_mouse_up 时清回 false，避免下次非工具栏点击误判。
        div()
            .absolute()
            .top(px(toolbar_y))
            .left(px(toolbar_x))
            .bg(gpui::rgba(0x202020FF))
            .rounded_md()
            .p(px(TOOLBAR_PAD))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _window, _cx| {
                this.toolbar_hovered = true;
            }))
            .child(row)
    }
}

/// handle 视觉尺寸：边长（像素）
const HANDLE_VISUAL_SIZE: f32 = 8.0;
/// handle 命中容差的一半（与 selection::HANDLE_HALF_SIZE 保持一致）
const HANDLE_HIT_HALF: f32 = 8.0;

/// 工具栏按钮视觉尺寸（正方形边长，px）
const TOOLBAR_BTN_SIZE: f32 = 32.0;
/// 工具栏按钮之间间距
const TOOLBAR_GAP: f32 = 4.0;
/// 工具栏整体内边距
const TOOLBAR_PAD: f32 = 6.0;
/// 工具栏距离选区上沿的距离（px）
const TOOLBAR_OFFSET_Y: f32 = 8.0;

/// 把 ToolButton 映射到 gpui-component 的 Lucide 图标名
///
/// gpui-component-assets 内置图标有限，找不到对应语义时取最接近的占位：
/// - 画笔/手绘用 Asterisk（无 pen.svg/brush.svg/pencil.svg）
/// - 马赛克用 LayoutDashboard（无 mosaic.svg，网格感接近）
/// - Bold 用 CaseSensitive（无 bold.svg，字母大小写图标兜底）
fn icon_for(btn: ToolButton) -> IconName {
    match btn {
        ToolButton::Rectangle => IconName::Frame,
        ToolButton::Ellipse => IconName::CircleX,
        ToolButton::Arrow => IconName::ArrowUp,
        ToolButton::Freehand => IconName::Asterisk,
        ToolButton::Text => IconName::SquareTerminal,
        ToolButton::Ocr => IconName::Eye,
        ToolButton::Mosaic => IconName::LayoutDashboard,
        ToolButton::ColorPicker => IconName::Palette,
        ToolButton::Undo => IconName::Undo2,
        ToolButton::Redo => IconName::Redo2,
        ToolButton::Finish => IconName::Check,
        ToolButton::Cancel => IconName::Close,
        ToolButton::Pin => IconName::ExternalLink,
        ToolButton::Bold => IconName::CaseSensitive,
    }
}

/// 计算工具栏位置 (x, y, width, height)
///
/// width 仅是估算值，用来在 root.on_mouse_down 中判定点击是否落在工具栏区域。
/// 实际渲染宽度可能因 popover 触发器而略宽，但估算不影响功能正确性——
/// 若估算偏小，root 会让点击穿透到 selection.mouse_down 打散选区，
/// 触发后会通过按钮 on_click 收到 click —— 设计上我们通过 div + flexbox
/// 测量真实 bounds 再吞掉冒泡，目前用估算偏宽避免吞掉事件。
fn compute_toolbar_bounds(
    sel: ub::Bounds,
    screen_bounds: ub::Bounds,
) -> (f32, f32, f32, f32) {
    let screen_h = screen_bounds.origin.y + screen_bounds.size.y;
    let toolbar_h = TOOLBAR_BTN_SIZE + TOOLBAR_PAD * 2.0;
    let toolbar_y_below = sel.origin.y + sel.size.y + TOOLBAR_OFFSET_Y;
    // 选区下方放得下 → 放在选区下方；否则 → 固定屏幕底部
    let toolbar_y = if toolbar_y_below + toolbar_h + TOOLBAR_OFFSET_Y <= screen_h {
        toolbar_y_below
    } else {
        screen_h - toolbar_h - TOOLBAR_OFFSET_Y
    };
    let toolbar_w = TOOLBAR_BTN_SIZE * 12.0 + TOOLBAR_GAP * 11.0 + TOOLBAR_PAD * 2.0;
    let toolbar_x = sel.origin.x.min(screen_bounds.origin.x + screen_bounds.size.x - toolbar_w - TOOLBAR_OFFSET_Y);
    (toolbar_x, toolbar_y, toolbar_w, toolbar_h)
}

/// 工具栏按钮中的图标+文字，紧凑间距
fn icon_label(btn: ToolButton) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .child(Icon::new(icon_for(btn)).size(px(14.0)))
        .child(btn.label())
}

/// 给 5 个绘图工具构造带 Popover 的按钮 Popover
///
/// trigger = Button（带工具图标）。第一次点击 → 选中工具；
/// 再点 active 工具 → 浮出 popover。popover 内容由 active_tool 决定：
/// - Text → 字号档位 + Bold + 颜色
/// - Rectangle/Arrow/Freehand/Mosaic → 粗细档位 + 颜色
fn render_tool_button_with_popover(
    btn: ToolButton,
    is_active: bool,
    weak: gpui::WeakEntity<OverlayView>,
    view: &OverlayView,
    cx: &mut Context<OverlayView>,
) -> Popover {
    // 计算该按钮对应的 popover kind（仅 active 时弹出才需要内容）
    let popup_kind = if btn == ToolButton::Text {
        ToolbarPopup::Text
    } else {
        ToolbarPopup::Stroke
    };
    let is_open = is_active && view.toolbar.popup == Some(popup_kind);

    let weak_for_trigger = weak.clone();
    let trigger = Button::new(("tool", btn as usize))
        .tooltip(btn.label())
        .compact()
        .selected(is_active)
        .child(icon_label(btn))
        .on_click(move |_, _, cx| {
            let _ = weak_for_trigger.update(cx, |this, cx| {
                if this.toolbar.active_tool == Some(btn) {
                    // 已 active：toggle popover
                    this.toolbar.popup = if this.toolbar.popup == Some(popup_kind) {
                        None
                    } else {
                        Some(popup_kind)
                    };
                } else {
                    // 切到新工具：先提交活跃的 Text 输入，避免文字丢失
                    this.finalize_text_input_if_active(cx);
                    this.toolbar.active_tool = Some(btn);
                    this.toolbar.popup = None;
                }
                cx.notify();
            });
        });

    let weak_content = weak.clone();
    Popover::new(("tool-popover", btn as usize))
        .trigger(trigger)
        .open(is_open)
        .on_open_change(cx.listener(move |this, open, _w, cx| {
            if *open {
                this.toolbar.popup = Some(popup_kind);
            } else if this.toolbar.popup == Some(popup_kind) {
                this.toolbar.popup = None;
            }
            cx.notify();
        }))
        .content(move |_state, _window, cx| {
            // content 闭包不能捕获 view 借用（要求 'static）。
            // 每次渲染通过 weak 读当前 OverlayView 状态，确保选中态紧跟最新 toolbar。
            let weak = weak_content.clone();
            let (cur_color, cur_size, cur_weight, cur_lw) = weak
                .read_with(cx, |this, _| {
                    (
                        this.toolbar.current_color,
                        this.toolbar.current_size,
                        this.toolbar.current_weight,
                        this.toolbar.line_width,
                    )
                })
                .unwrap_or((RGBA::new(0, 0, 0, 255), 24.0, FontWeight::Normal, 4.0));
            match popup_kind {
                ToolbarPopup::Text => render_text_popover_content(cur_color, cur_size, cur_weight, weak),
                ToolbarPopup::Stroke => render_stroke_popover_content(cur_color, cur_lw, weak),
            }
        })
}

/// 简单按钮（Undo/Redo/Cancel/Finish，没有 Popover）
///
/// 用 Button.on_click 直接处理 click。Finish 在 active=false 时也走 primary 让"完成"
/// 在视觉上始终高亮（用户期望"完成"是醒目的绿色对勾）。
fn render_simple_button(
    btn: ToolButton,
    active: bool,
    disabled: bool,
    weak: gpui::WeakEntity<OverlayView>,
) -> Button {
    let weak_for_click = weak.clone();
    let mut b = Button::new(("action", btn as usize))
        .tooltip(btn.label())
        .compact()
        .disabled(disabled)
        .child(icon_label(btn))
        .on_click(move |_, window, cx| {
            let _ = weak_for_click.update(cx, |this, cx| {
                this.toolbar.popup = None;
                match btn {
                    ToolButton::Undo => {
                        this.drawing.undo();
                        this.check_selected_visible();
                        cx.notify();
                    }
                    ToolButton::Redo => {
                        this.drawing.redo();
                        this.check_selected_visible();
                        cx.notify();
                    }
                    ToolButton::Cancel => {
                        this.commit(
                            OverlayResult {
                                selection: None,
                                commands: vec![],
                                no_clipboard: false,
                            },
                            window,
                        );
                    }
                    ToolButton::Ocr => {
                        this.finalize_text_input_if_active(cx);
                        if this.toolbar.active_tool == Some(ToolButton::Ocr) {
                            this.toolbar.active_tool = None;
                            this.ocr_rect = None;
                            this.ocr_result = None;
                        } else {
                            this.toolbar.active_tool = Some(ToolButton::Ocr);
                            this.ocr_rect = None;
                            this.ocr_result = None;
                        }
                        cx.notify();
                    }
                    ToolButton::Pin => {
                        this.finalize_text_input_if_active(cx);
                        let s = this.selection.current().or(Some(this.screen_bounds));
                        let cmds: Vec<DrawCommand> =
                            this.drawing.visible_commands().cloned().collect();

                        let wb = window.bounds();
                        let sx = this.frame_width as f32 / f32::from(wb.size.width).max(1.0);
                        let sy = this.frame_height as f32 / f32::from(wb.size.height).max(1.0);
                        tracing::info!(
                            "[Pin] overlay window: origin=({:.0},{:.0}) size=({:.0},{:.0}) frame={}x{} sx={:.2} sy={:.2}",
                            wb.origin.x, wb.origin.y, wb.size.width, wb.size.height,
                            this.frame_width, this.frame_height, sx, sy
                        );

                        let scaled_cmds: Vec<DrawCommand> =
                            cmds.iter().map(|c| scale_draw_command(c.clone(), sx, sy)).collect();

                        if let Some(sel) = s {
                            let sel_px = ub::Bounds {
                                origin: ub::Point::new(sel.origin.x * sx, sel.origin.y * sy),
                                size: ub::Point::new(sel.size.x * sx, sel.size.y * sy),
                            };
                            tracing::info!(
                                "[Pin] selection logical: origin=({:.0},{:.0}) size=({:.0},{:.0})",
                                sel.origin.x, sel.origin.y, sel.size.x, sel.size.y
                            );
                            tracing::info!(
                                "[Pin] selection physical: origin=({:.0},{:.0}) size=({:.0},{:.0})",
                                sel_px.origin.x, sel_px.origin.y, sel_px.size.x, sel_px.size.y
                            );
                            let full_pixels = this.frame_pixels.clone();
                            let fw = this.frame_width;
                            let fh = this.frame_height;

                            let pin_frame = CapturedFrame {
                                width: fw,
                                height: fh,
                                pixels: full_pixels,
                            };
                            if let Ok(mut clipped) = pin_frame.clip_region(
                                sel_px.origin.x as u32,
                                sel_px.origin.y as u32,
                                sel_px.size.x as u32,
                                sel_px.size.y as u32,
                            ) {
                                let _ = crate::overlay::commands::apply_commands(
                                    &mut clipped,
                                    sel_px.origin.x,
                                    sel_px.origin.y,
                                    &scaled_cmds,
                                );
                                let pin_x = f32::from(wb.origin.x) + sel.origin.x;
                                let pin_y = f32::from(wb.origin.y) + sel.origin.y;
                                tracing::info!(
                                    "[Pin] target position: ({:.0},{:.0}) clipped_frame={}x{}",
                                    pin_x, pin_y, clipped.width, clipped.height
                                );
                                spawn_pin_window(clipped, pin_x, pin_y, sx, sy);
                            }
                        }

                        this.commit(OverlayResult { selection: s, commands: cmds, no_clipboard: true }, window);
                    }
                    ToolButton::Finish => {
                        // 兜底：若 Text 工具还活着没提交，先把它的内容落成命令
                        this.finalize_text_input_if_active(cx);
                        let s = this.selection.current().or(Some(this.screen_bounds));
                        let cmds: Vec<DrawCommand> =
                            this.drawing.visible_commands().cloned().collect();
                        this.commit(OverlayResult { selection: s, commands: cmds, no_clipboard: false }, window);
                    }
                    _ => {}
                }
            });
        });
    if active || btn == ToolButton::Finish {
        b = b.primary();
    }
    b
}

/// 渲染文字 popover 内容：字号档位 + Bold + 12 色色板
fn render_text_popover_content(
    cur_color: RGBA,
    cur_size: f32,
    cur_weight: FontWeight,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Div {
    use crate::overlay::toolbar::FONT_SIZES;

    let mut col = div().flex().flex_col().gap(px(6.0)).p(px(6.0)).min_w(px(220.0));

    // 第一行：Bold + 字号档位
    let mut top = div().flex().gap(px(4.0)).items_center();
    let weak_bold = weak.clone();
    let bold_btn = Button::new("font-bold")
        .icon(IconName::CaseSensitive)
        .label("B")
        .tooltip("加粗")
        .compact()
        .selected(cur_weight == FontWeight::Bold)
        .on_click(move |_, _, cx| {
            let _ = weak_bold.update(cx, |this, cx| {
                this.toolbar.current_weight = match this.toolbar.current_weight {
                    FontWeight::Normal => FontWeight::Bold,
                    FontWeight::Bold => FontWeight::Normal,
                };
                cx.notify();
            });
        });
    top = top.child(bold_btn);

    for (i, &size) in FONT_SIZES.iter().enumerate() {
        let weak_s = weak.clone();
        let is_current = (cur_size - size).abs() < f32::EPSILON;
        let label: gpui::SharedString = format!("{}", size as i32).into();
        let btn = Button::new(("font-size", i))
            .label(label)
            .compact()
            .selected(is_current)
            .on_click(move |_, _, cx| {
                let _ = weak_s.update(cx, |this, cx| {
                    this.toolbar.current_size = size;
                    cx.notify();
                });
            });
        top = top.child(btn);
    }
    col = col.child(top);

    // 第二行：颜色色板
    col = col.child(render_color_swatch_row(cur_color, weak));
    col
}

/// 渲染画图类 popover 内容：粗细档位 + 12 色色板
fn render_stroke_popover_content(
    cur_color: RGBA,
    cur_lw: f32,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Div {
    use crate::overlay::toolbar::LINE_WIDTHS;

    let mut col = div().flex().flex_col().gap(px(6.0)).p(px(6.0)).min_w(px(200.0));

    // 第一行：粗细档位
    let mut top = div().flex().gap(px(4.0)).items_center();
    for (i, &lw) in LINE_WIDTHS.iter().enumerate() {
        let weak_lw = weak.clone();
        let is_current = (cur_lw - lw).abs() < f32::EPSILON;
        let label: gpui::SharedString = format!("{}", lw as i32).into();
        let btn = Button::new(("lw", i))
            .label(label)
            .compact()
            .selected(is_current)
            .on_click(move |_, _, cx| {
                let _ = weak_lw.update(cx, |this, cx| {
                    this.finalize_text_input_if_active(cx);
                    this.toolbar.line_width = lw;
                    cx.notify();
                });
            });
        top = top.child(btn);
    }
    col = col.child(top);

    // 第二行：颜色色板
    col = col.child(render_color_swatch_row(cur_color, weak));
    col
}

/// 渲染 12 色色板行（构造时不依赖 OverlayView listener，所有回调用 WeakEntity）
fn render_color_swatch_row(cur_color: RGBA, weak: gpui::WeakEntity<OverlayView>) -> gpui::Div {
    let swatch = palette::default_palette();
    let mut row = div().flex().gap(px(4.0)).items_center().flex_wrap();
    for (i, &c) in swatch.iter().enumerate() {
        let bg = gpui::rgba(rgba_u32(c));
        let weak_c = weak.clone();
        let is_current = c == cur_color;
        let border_color = if is_current {
            gpui::rgba(0xFFFFFFFF)
        } else {
            gpui::rgba(0xFFFFFF33)
        };
        row = row.child(
            div()
                .id(("swatch", i))
                .size(px(22.0))
                .rounded(px(4.0))
                .bg(bg)
                .border_2()
                .border_color(border_color)
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    let _ = weak_c.update(cx, |this, cx| {
                        this.toolbar.current_color = c;
                        cx.notify();
                    });
                }),
        );
    }
    row
}


/// RGBA → BGRA 通道 swap（GPUI RenderImage 用 BGRA）
fn rgba_to_bgra(pixels: &mut [u8]) {
    for c in pixels.chunks_exact_mut(4) {
        c.swap(0, 2);
    }
}

/// 检测点击是否落在文字输入框的拖动条或 resize 手柄上，返回对应的 DragState
fn hit_test_text_drag(rect: ub::Bounds, p: BoundsPoint) -> Option<TextDragState> {
    const H: f32 = 6.0;
    let hit = |x: f32, y: f32| -> bool {
        p.x >= x && p.x <= x + H && p.y >= y && p.y <= y + H
    };
    let x = rect.origin.x;
    let y = rect.origin.y;
    let w = rect.size.x;
    let h = rect.size.y;
    let cx = x + w / 2.0 - H / 2.0;
    let cy = y + h / 2.0 - H / 2.0;

    // 优先检测 8 个 resize 手柄（角 + 边中点）
    let checks: &[(TextDragMode, f32, f32)] = &[
        (TextDragMode::ResizeNW, x, y),
        (TextDragMode::ResizeN,  cx, y),
        (TextDragMode::ResizeNE, x + w - H, y),
        (TextDragMode::ResizeW,  x, cy),
        (TextDragMode::ResizeE,  x + w - H, cy),
        (TextDragMode::ResizeSW, x, y + h - H),
        (TextDragMode::ResizeS,  cx, y + h - H),
        (TextDragMode::ResizeSE, x + w - H, y + h - H),
    ];
    for &(mode, hx, hy) in checks {
        if hit(hx, hy) {
            return Some(TextDragState { mode, start_mouse: p, start_rect: rect });
        }
    }

    // 顶部 6px 整条为 Move 拖动区（排除已被手柄命中的区域，手柄优先检测）
    if p.y >= y && p.y <= y + H && p.x >= x && p.x <= x + w {
        return Some(TextDragState {
            mode: TextDragMode::Move,
            start_mouse: p,
            start_rect: rect,
        });
    }
    None
}

/// 应用文字框拖动 / resize 增量到 `text_input_rect`
fn apply_text_drag(this: &mut OverlayView, drag: TextDragState, p: BoundsPoint) {
    let dx = p.x - drag.start_mouse.x;
    let dy = p.y - drag.start_mouse.y;
    let start = drag.start_rect;
    // 最小尺寸限制：避免拖成 1px
    const MIN_W: f32 = 80.0;
    const MIN_H: f32 = 40.0;
    let new_rect = match drag.mode {
        TextDragMode::Move => ub::Bounds {
            origin: BoundsPoint::new(start.origin.x + dx, start.origin.y + dy),
            size: start.size,
        },
        TextDragMode::ResizeNW => {
            let new_x = start.origin.x + dx;
            let new_y = start.origin.y + dy;
            let new_w = (start.size.x - dx).max(MIN_W);
            let new_h = (start.size.y - dy).max(MIN_H);
            let clamped_x = if new_w == MIN_W { start.origin.x + (start.size.x - MIN_W) } else { new_x };
            let clamped_y = if new_h == MIN_H { start.origin.y + (start.size.y - MIN_H) } else { new_y };
            ub::Bounds {
                origin: BoundsPoint::new(clamped_x, clamped_y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeN => {
            let new_y = start.origin.y + dy;
            let new_h = (start.size.y - dy).max(MIN_H);
            let clamped_y = if new_h == MIN_H { start.origin.y + (start.size.y - MIN_H) } else { new_y };
            ub::Bounds {
                origin: BoundsPoint::new(start.origin.x, clamped_y),
                size: BoundsPoint::new(start.size.x, new_h),
            }
        }
        TextDragMode::ResizeNE => {
            let new_y = start.origin.y + dy;
            let new_w = (start.size.x + dx).max(MIN_W);
            let new_h = (start.size.y - dy).max(MIN_H);
            let clamped_y = if new_h == MIN_H { start.origin.y + (start.size.y - MIN_H) } else { new_y };
            ub::Bounds {
                origin: BoundsPoint::new(start.origin.x, clamped_y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeW => {
            let new_x = start.origin.x + dx;
            let new_w = (start.size.x - dx).max(MIN_W);
            let clamped_x = if new_w == MIN_W { start.origin.x + (start.size.x - MIN_W) } else { new_x };
            ub::Bounds {
                origin: BoundsPoint::new(clamped_x, start.origin.y),
                size: BoundsPoint::new(new_w, start.size.y),
            }
        }
        TextDragMode::ResizeE => {
            let new_w = (start.size.x + dx).max(MIN_W);
            ub::Bounds {
                origin: start.origin,
                size: BoundsPoint::new(new_w, start.size.y),
            }
        }
        TextDragMode::ResizeSW => {
            let new_x = start.origin.x + dx;
            let new_w = (start.size.x - dx).max(MIN_W);
            let new_h = (start.size.y + dy).max(MIN_H);
            let clamped_x = if new_w == MIN_W { start.origin.x + (start.size.x - MIN_W) } else { new_x };
            ub::Bounds {
                origin: BoundsPoint::new(clamped_x, start.origin.y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeS => {
            let new_h = (start.size.y + dy).max(MIN_H);
            ub::Bounds {
                origin: start.origin,
                size: BoundsPoint::new(start.size.x, new_h),
            }
        }
        TextDragMode::ResizeSE => {
            let new_w = (start.size.x + dx).max(MIN_W);
            let new_h = (start.size.y + dy).max(MIN_H);
            ub::Bounds {
                origin: start.origin,
                size: BoundsPoint::new(new_w, new_h),
            }
        }
    };
    this.text_input_rect = new_rect.clamp_inside(this.selection.current().unwrap_or(this.screen_bounds));
}

/// 检测点击是否落在已绘制命令的手柄或主体上
fn hit_test_cmd_drag(cmd: &DrawCommand, p: BoundsPoint) -> Option<CmdDragMode> {
    match cmd {
        DrawCommand::Rectangle { rect, .. }
        | DrawCommand::Ellipse { rect, .. } => {
            let a = rect.0;
            let b = rect.1;
            let bounds = ub::Bounds::new(
                ub::Point::new(a.x.min(b.x), a.y.min(b.y)),
                ub::Point::new(a.x.max(b.x), a.y.max(b.y)),
            );
            if let Some(handle) = bounds.hit_handle(p, HANDLE_HIT_HALF) {
                return Some(CmdDragMode::ResizeRect { handle, start_rect: *rect });
            }
            if bounds.contains(p) {
                return Some(CmdDragMode::MoveRect { start_rect: *rect });
            }
            None
        }
        DrawCommand::Arrow { from, to, .. } => {
            // 检查端点
            let d_from = ((p.x - from.x).powi(2) + (p.y - from.y).powi(2)).sqrt();
            if d_from <= HANDLE_HIT_HALF {
                return Some(CmdDragMode::MoveArrowFrom { start_from: *from, start_to: *to });
            }
            let d_to = ((p.x - to.x).powi(2) + (p.y - to.y).powi(2)).sqrt();
            if d_to <= HANDLE_HIT_HALF {
                return Some(CmdDragMode::MoveArrowTo { start_from: *from, start_to: *to });
            }
            // 检查箭杆：点到线段的距离
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let len_sq = dx * dx + dy * dy;
            if len_sq > 0.0 {
                let t = ((p.x - from.x) * dx + (p.y - from.y) * dy) / len_sq;
                if t >= 0.0 && t <= 1.0 {
                    let proj_x = from.x + t * dx;
                    let proj_y = from.y + t * dy;
                    let dist = ((p.x - proj_x).powi(2) + (p.y - proj_y).powi(2)).sqrt();
                    if dist <= HANDLE_HIT_HALF * 2.0 {
                        return Some(CmdDragMode::MoveArrow { start_from: *from, start_to: *to });
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// 文字输入框内，文字相对于外层 box 原点的偏移量
///
/// 元素层结构：外层 box → 拖拽条(6px) → 内容区（Input 组件）。
/// Input 组件有 input_px=8、input_py=2 的 padding，Editor 元素位于
/// padding 内部，所以文字原点相对于外层 box 约为 (8, 8)。
/// Canvas 渲染时需加相同偏移以对齐。
const TO_X: f32 = 8.0;
const TO_Y: f32 = 8.0;

/// 检测点击是否落在已固化的 Text 命令区域内（用于"点击重新编辑"）
///
/// anchor 是外层 box 原点，文字实际渲染位置偏移 (TO_X, TO_Y)。
fn hit_test_text_cmd(cmd: &DrawCommand, p: BoundsPoint) -> bool {
    match cmd {
        DrawCommand::Text { anchor, content, font_size, .. } => {
            let char_w = font_size * 0.6;
            let line_h = font_size * 1.25;
            let lines: Vec<&str> = content.split('\n').collect();
            let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
            let w = char_w * max_chars.max(1.0);
            let h = line_h * lines.len().max(1) as f32;
            const PAD: f32 = 4.0;
            p.x >= anchor.x + TO_X - PAD && p.x <= anchor.x + TO_X + w + PAD
                && p.y >= anchor.y + TO_Y - PAD && p.y <= anchor.y + TO_Y + h + PAD
        }
        _ => false,
    }
}

/// 应用命令拖拽增量到 DrawingState 中的命令
///
/// 所有拖拽操作都裁剪到选区边界内（若选区存在），防止矩形/箭头超出截图框。
fn apply_cmd_drag(this: &mut OverlayView, drag: CmdDragState, p: BoundsPoint) {
    use crate::overlay::drawing::Point as DP;
    let dx = p.x - drag.start_mouse.x;
    let dy = p.y - drag.start_mouse.y;
    let limits = this.selection.current().unwrap_or(this.screen_bounds);
    let Some(cmd) = this.drawing.get_visible_mut(drag.cmd_index) else {
        this.cmd_drag = None;
        return;
    };
    match drag.mode {
        CmdDragMode::ResizeRect { handle, start_rect } => {
            let bounds = ub::Bounds::new(
                ub::Point::new(start_rect.0.x.min(start_rect.1.x), start_rect.0.y.min(start_rect.1.y)),
                ub::Point::new(start_rect.0.x.max(start_rect.1.x), start_rect.0.y.max(start_rect.1.y)),
            );
            let positions = bounds.handle_positions();
            let hp = positions[handle as usize];
            let new_handle = ub::Point::new(hp.x + dx, hp.y + dy);
            let new_bounds = crate::overlay::selection::apply_resize(bounds, handle, new_handle, limits);
            if let DrawCommand::Rectangle { ref mut rect, .. }
                 | DrawCommand::Ellipse { ref mut rect, .. } = cmd {
                rect.0 = DP::new(new_bounds.origin.x, new_bounds.origin.y);
                rect.1 = DP::new(new_bounds.origin.x + new_bounds.size.x, new_bounds.origin.y + new_bounds.size.y);
            }
        }
        CmdDragMode::MoveRect { start_rect } => {
            let a = start_rect.0;
            let b = start_rect.1;
            let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
            let w = (b.x - a.x).abs();
            let h = (b.y - a.y).abs();
            let new_origin = ub::Point::new(x1 + dx, y1 + dy);
            let new_bounds = ub::Bounds { origin: new_origin, size: ub::Point::new(w, h) }
                .clamp_inside(limits);
            if let DrawCommand::Rectangle { ref mut rect, .. }
                 | DrawCommand::Ellipse { ref mut rect, .. } = cmd {
                rect.0 = DP::new(new_bounds.origin.x, new_bounds.origin.y);
                rect.1 = DP::new(new_bounds.origin.x + new_bounds.size.x, new_bounds.origin.y + new_bounds.size.y);
            }
        }
        CmdDragMode::MoveArrowFrom { start_from, start_to: _ } => {
            let x = (start_from.x + dx).clamp(limits.origin.x, limits.origin.x + limits.size.x);
            let y = (start_from.y + dy).clamp(limits.origin.y, limits.origin.y + limits.size.y);
            if let DrawCommand::Arrow { ref mut from, .. } = cmd {
                *from = DP::new(x, y);
            }
        }
        CmdDragMode::MoveArrowTo { start_to, .. } => {
            let x = (start_to.x + dx).clamp(limits.origin.x, limits.origin.x + limits.size.x);
            let y = (start_to.y + dy).clamp(limits.origin.y, limits.origin.y + limits.size.y);
            if let DrawCommand::Arrow { ref mut to, .. } = cmd {
                *to = DP::new(x, y);
            }
        }
        CmdDragMode::MoveArrow { start_from, start_to } => {
            let new_from = ub::Point::new(start_from.x + dx, start_from.y + dy);
            let new_to = ub::Point::new(start_to.x + dx, start_to.y + dy);
            let min_x = new_from.x.min(new_to.x);
            let max_x = new_from.x.max(new_to.x);
            let min_y = new_from.y.min(new_to.y);
            let max_y = new_from.y.max(new_to.y);
            let clamp_dx = if min_x < limits.origin.x { limits.origin.x - min_x }
                else if max_x > limits.origin.x + limits.size.x { (limits.origin.x + limits.size.x) - max_x }
                else { 0.0 };
            let clamp_dy = if min_y < limits.origin.y { limits.origin.y - min_y }
                else if max_y > limits.origin.y + limits.size.y { (limits.origin.y + limits.size.y) - max_y }
                else { 0.0 };
            if let DrawCommand::Arrow { ref mut from, ref mut to, .. } = cmd {
                *from = DP::new(new_from.x + clamp_dx, new_from.y + clamp_dy);
                *to = DP::new(new_to.x + clamp_dx, new_to.y + clamp_dy);
            }
        }
    }
}

/// 构造文字输入框的角 resize handle（6×6 方块）
///
/// 鼠标按下时把 `text_input_drag` 置为对应 mode + 记录起点 rect。
/// 鼠标移动在 root.on_mouse_move 里统一处理（用户可以拖到框外）。
fn make_resize_handle(
    id: impl Into<gpui::ElementId>,
    left: f32,
    top: f32,
    mode: TextDragMode,
) -> impl IntoElement {
    let cursor = match mode {
        TextDragMode::ResizeNW | TextDragMode::ResizeSE => gpui::CursorStyle::ResizeUpRightDownLeft,
        TextDragMode::ResizeNE | TextDragMode::ResizeSW => gpui::CursorStyle::ResizeUpLeftDownRight,
        TextDragMode::ResizeN | TextDragMode::ResizeS => gpui::CursorStyle::ResizeUpDown,
        TextDragMode::ResizeW | TextDragMode::ResizeE => gpui::CursorStyle::ResizeLeftRight,
        _ => gpui::CursorStyle::Arrow,
    };
    div()
        .id(id)
        .absolute()
        .top(px(top))
        .left(px(left))
        .size(px(6.0))
        .bg(gpui::rgba(0xFFFFFFFF))
        .border_1()
        .border_color(gpui::rgba(0x000000CC))
        .cursor(cursor)
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

/// 把 DrawCommand 中的所有坐标从 canvas 坐标转为帧物理像素坐标
fn scale_draw_command(cmd: DrawCommand, sx: f32, sy: f32) -> DrawCommand {
    use crate::overlay::drawing::Point as DP;
    let sp = |p: DP| DP::new(p.x * sx, p.y * sy);
    match cmd {
        DrawCommand::Rectangle { rect, color, line_width } => DrawCommand::Rectangle {
            rect: (sp(rect.0), sp(rect.1)),
            color,
            line_width,
        },
        DrawCommand::Ellipse { rect, color, line_width } => DrawCommand::Ellipse {
            rect: (sp(rect.0), sp(rect.1)),
            color,
            line_width,
        },
        DrawCommand::Arrow { from, to, color, line_width } => DrawCommand::Arrow {
            from: sp(from),
            to: sp(to),
            color,
            line_width,
        },
        DrawCommand::Freehand { points, color, line_width } => DrawCommand::Freehand {
            points: points.into_iter().map(sp).collect(),
            color,
            line_width,
        },
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
            DrawCommand::Text {
                anchor: sp(anchor),
                content,
                font_size,
                color,
                max_width: max_width.map(|w| w * sx),
                weight,
            }
        }
        DrawCommand::Mosaic { regions, block_size, color } => DrawCommand::Mosaic {
            regions: regions.into_iter().map(|r| (sp(r.0), sp(r.1))).collect(),
            block_size: (block_size as f32 * sx).max(1.0) as u32,
            color,
        },
    }
}

/// RGBA → GPUI rgba u32（0xRRGGBBAA）
fn rgba_u32(c: RGBA) -> u32 {
    (u32::from(c.r) << 24)
        | (u32::from(c.g) << 16)
        | (u32::from(c.b) << 8)
        | u32::from(c.a)
}

/// 画一条指定粗细的实线
///
/// 沿线以高密度采样圆角正方形（corner_radius=lw/2），
/// 重叠的圆角方块形成平滑的抗锯齿厚线条。
/// 采样密度随线宽自适应：越细的线步长越小，确保充分重叠。
fn paint_thick_line(x1: f32, y1: f32, x2: f32, y2: f32, lw: f32, color: RGBA, window: &mut Window) {
    let hsla = Hsla::from(gpui::rgba(rgba_u32(color)));
    // 方块略大于线宽，确保重叠覆盖
    let size = (lw * 1.5).max(2.0);
    let size_half = size / 2.0;
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(x1 - size_half), gpui::px(y1 - size_half)),
                size: Size::new(gpui::px(size), gpui::px(size)),
            },
            gpui::px(size_half),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    // 采样密度：越细的线步长越小，确保充分重叠消除锯齿
    let spacing = if lw < 3.0 { 0.125 } else if lw < 6.0 { 0.2 } else { 0.25 };
    let steps = (len / spacing).ceil() as usize;
    for i in 0..=steps {
        let t = i as f32 * spacing;
        let cx = x1 + ux * t;
        let cy = y1 + uy * t;
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(cx - size_half), gpui::px(cy - size_half)),
                size: Size::new(gpui::px(size), gpui::px(size)),
            },
            gpui::px(size_half),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
    }
}

/// 画一条宽度渐变的线段
///
/// 同 `paint_thick_line`，用重叠圆角正方形实现平滑抗锯齿，
/// 采样密度随最大线宽自适应。
fn paint_tapered_line(
    x1: f32, y1: f32,
    x2: f32, y2: f32,
    start_lw: f32,
    end_lw: f32,
    color: RGBA,
    window: &mut Window,
) {
    let hsla = Hsla::from(gpui::rgba(rgba_u32(color)));
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    let max_lw = start_lw.max(end_lw);
    if len < 0.5 {
        let half = (end_lw / 2.0).max(0.8);
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(x1 - half), gpui::px(y1 - half)),
                size: Size::new(gpui::px(end_lw), gpui::px(end_lw)),
            },
            gpui::px(half),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let start_half = (start_lw / 2.0).max(0.8);
    let end_half = (end_lw / 2.0).max(0.8);
    let spacing = if max_lw < 3.0 { 0.125 } else if max_lw < 6.0 { 0.2 } else { 0.25 };
    let steps = (len / spacing).ceil() as usize;
    for i in 0..=steps {
        let t = i as f32 * spacing;
        let frac = (t / len).min(1.0);
        let cur_half = start_half + (end_half - start_half) * frac;
        let cur_diam = cur_half * 2.0;
        let cur_size = (cur_diam * 1.5).max(2.0);
        let cur_size_half = cur_size / 2.0;
        let cx = x1 + ux * t;
        let cy = y1 + uy * t;
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(cx - cur_size_half), gpui::px(cy - cur_size_half)),
                size: Size::new(gpui::px(cur_size), gpui::px(cur_size)),
            },
            gpui::px(cur_size_half),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
    }
}

/// 画空心矩形边框（4 条粗线）
fn paint_rect_outline(x: f32, y: f32, w: f32, h: f32, lw: f32, color: RGBA, window: &mut Window) {
    paint_thick_line(x, y, x + w, y, lw, color, window);
    paint_thick_line(x, y + h, x + w, y + h, lw, color, window);
    paint_thick_line(x, y, x, y + h, lw, color, window);
    paint_thick_line(x + w, y, x + w, y + h, lw, color, window);
}

/// 画空心椭圆边框（用 64 段折线近似椭圆轮廓）
fn paint_ellipse_outline(x: f32, y: f32, w: f32, h: f32, lw: f32, color: RGBA, window: &mut Window) {
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let rx = w / 2.0;
    let ry = h / 2.0;
    let n = 128;
    let mut prev: Option<(f32, f32)> = None;
    for i in 0..=n {
        let theta = 2.0 * std::f32::consts::PI * i as f32 / n as f32;
        let px = cx + rx * theta.cos();
        let py = cy + ry * theta.sin();
        if let Some((px0, py0)) = prev {
            paint_thick_line(px0, py0, px, py, lw, color, window);
        }
        prev = Some((px, py));
    }
}

/// 把一个 DrawCommand 渲染到 window 上（Phase 3 preview，Phase 4 也会复用）
fn paint_command(cmd: &DrawCommand, window: &mut Window, cx: &mut App, scale_factor: f32, font_family: &gpui::SharedString) {
    match cmd {
        DrawCommand::Rectangle { rect, color, line_width } => {
            let a = rect.0;
            let b = rect.1;
            let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
            let w = (b.x - a.x).abs();
            let h = (b.y - a.y).abs();
            paint_rect_outline(x1, y1, w, h, *line_width, *color, window);
        }
        DrawCommand::Ellipse { rect, color, line_width } => {
            let a = rect.0;
            let b = rect.1;
            let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
            let w = (b.x - a.x).abs();
            let h = (b.y - a.y).abs();
            paint_ellipse_outline(x1, y1, w, h, *line_width, *color, window);
        }
        DrawCommand::Arrow { from, to, color, line_width } => {
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1.0 {
                paint_thick_line(from.x, from.y, to.x, to.y, *line_width, *color, window);
                return;
            }
            let ux = dx / len;
            let uy = dy / len;
            let head_len = (*line_width * 7.0).max(14.0);
            let head_w = (*line_width * 2.0).max(4.0);
            let bx = to.x - ux * head_len;
            let by = to.y - uy * head_len;
            // 主线：从起点窄到箭头底部宽
            let start_lw = (*line_width * 0.3).max(1.0);
            paint_tapered_line(from.x, from.y, bx, by, start_lw, *line_width, *color, window);
            // 箭头 V 字
            let px = -uy;
            let py = ux;
            let p1x = bx + px * head_w;
            let p1y = by + py * head_w;
            let p2x = bx - px * head_w;
            let p2y = by - py * head_w;
            paint_thick_line(to.x, to.y, p1x, p1y, *line_width, *color, window);
            paint_thick_line(to.x, to.y, p2x, p2y, *line_width, *color, window);
        }
        DrawCommand::Freehand { ref points, color, line_width } => {
            for w in points.windows(2) {
                paint_thick_line(w[0].x, w[0].y, w[1].x, w[1].y, *line_width, *color, window);
            }
        }
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
                let fs = *font_size / scale_factor;
                let line_height = Rems(1.25).to_pixels(window.rem_size());
                let origin_x = window.pixel_snap(px(anchor.x + TO_X));
                let mut origin_y = window.pixel_snap(px(anchor.y + TO_Y));

                let mut base_run = window.text_style().to_run(0);
                base_run.font.family = font_family.clone();
                base_run.color = Hsla::from(rgba(rgba_u32(*color)));
                if *weight == FontWeight::Bold {
                    base_run.font.weight = gpui::FontWeight::BOLD;
                }

                let force_width = max_width.map(|mw| px(mw));

                for line_text in content.split('\n') {
                    if line_text.is_empty() {
                        origin_y += line_height;
                        continue;
                    }
                    let mut run = base_run.clone();
                    run.len = line_text.len();

                    let shaped = window.text_system().shape_line(
                        gpui::SharedString::from(line_text),
                        px(fs),
                        &[run],
                        force_width,
                    );

                    let _ = shaped.paint(
                        point(origin_x, origin_y),
                        line_height,
                        gpui::TextAlign::Left,
                        force_width,
                        window,
                        cx,
                    );

                    origin_y += line_height;
                }
            }
        DrawCommand::Mosaic { ref regions, color, block_size } => {
            // 用 block_size 网格模拟马赛克像素化效果
            let bs = (*block_size).max(1) as f32;
            let bright = Hsla::from(rgba((u32::from(color.r) << 24)
                | (u32::from(color.g) << 16)
                | (u32::from(color.b) << 8)
                | 0x60));
            let dim = Hsla::from(rgba((u32::from(color.r) << 24)
                | (u32::from(color.g) << 16)
                | (u32::from(color.b) << 8)
                | 0x28));
            for rect in regions {
                let a = rect.0;
                let b = rect.1;
                let (x1, y1) = (a.x.min(b.x), a.y.min(b.y));
                let w = (b.x - a.x).abs();
                let h = (b.y - a.y).abs();
                if w < 1.0 || h < 1.0 { continue; }
                let cells_x = (w / bs).ceil() as i32;
                let cells_y = (h / bs).ceil() as i32;
                for cy in 0..cells_y {
                    for cx in 0..cells_x {
                        let cell_fill = if (cx + cy) % 2 == 0 { bright } else { dim };
                        let cx1 = x1 + cx as f32 * bs;
                        let cy1 = y1 + cy as f32 * bs;
                        let cw = bs.min(x1 + w - cx1);
                        let ch = bs.min(y1 + h - cy1);
                        if cw < 1.0 || ch < 1.0 { continue; }
                        window.paint_quad(quad(
                            Bounds {
                                origin: point(px(cx1), px(cy1)),
                                size: Size::new(px(cw), px(ch)),
                            },
                            px(0.),
                            cell_fill,
                            px(0.),
                            gpui::transparent_black(),
                            Default::default(),
                        ));
                    }
                }
            }
        }
    }
}

/// 同步执行 OCR：从 frame_pixels 中裁切 rect 区域，保存为 PNG，调用 tesseract CLI。
///
/// `window_w` / `window_h` 是 GPUI 窗口的实际尺寸（逻辑像素），必须从
/// `window.bounds()` 获取。它与 frame 物理尺寸可能有差异（如任务栏挤压），
/// `paint_image` 会基于两者之比缩放图像，像素提取需用相同比率。
fn run_ocr_sync(
    rect: ub::Bounds,
    frame_pixels: &[u8],
    frame_width: u32,
    frame_height: u32,
    window_w: f32,
    window_h: f32,
) -> String {
    use std::process::Command;

    // canvas 坐标 → 物理像素：需要考虑 paint_image 的缩放比
    let x_ratio = frame_width as f32 / window_w.max(1.0);
    let y_ratio = frame_height as f32 / window_h.max(1.0);
    let x = (rect.origin.x * x_ratio).round().max(0.0) as u32;
    let y = (rect.origin.y * y_ratio).round().max(0.0) as u32;
    let w = (rect.size.x * x_ratio).round() as u32;
    let h = (rect.size.y * y_ratio).round() as u32;

    tracing::info!(
        "OCR: logical rect=({:.1},{:.1} {}x{}) win=({:.0}x{:.0}) ratio=({:.3},{:.3}) -> physical ({},{}) {}x{}; frame={}x{}",
        rect.origin.x, rect.origin.y, rect.size.x, rect.size.y,
        window_w, window_h,
        x_ratio, y_ratio,
        x, y, w, h,
        frame_width, frame_height,
    );

    // 边界裁剪
    let w = w.min(frame_width.saturating_sub(x));
    let h = h.min(frame_height.saturating_sub(y));
    if w == 0 || h == 0 {
        return String::new();
    }

    // 从 RGBA 帧中提取区域，转为 RGB
    let mut rgb: Vec<u8> = Vec::with_capacity((w * h * 3) as usize);
    for row in 0..h {
        let base = ((y + row) * frame_width + x) as usize * 4;
        for col in 0..w {
            let idx = base + col as usize * 4;
            rgb.push(frame_pixels[idx]);     // R
            rgb.push(frame_pixels[idx + 1]); // G
            rgb.push(frame_pixels[idx + 2]); // B
        }
    }

    // 写入调试 PNG（固定路径方便检查）
    let debug_path = std::path::PathBuf::from("/tmp/screenshot_ocr_debug.png");
    {
        let img = image::RgbImage::from_raw(w, h, rgb).unwrap_or_else(|| {
            image::RgbImage::new(w, h)
        });
        if let Err(e) = img.save(&debug_path) {
            tracing::error!("OCR: 保存调试 PNG 失败: {}", e);
        } else {
            tracing::info!("OCR: 调试 PNG 已保存到 {}", debug_path.display());
        }
    }

    // 调用 tesseract
    let output = Command::new("tesseract")
        .arg(&debug_path)
        .arg("stdout")
        .arg("-l")
        .arg("chi_sim+eng")
        .arg("--psm")
        .arg("6")
        .output();

    match output {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() {
                tracing::info!("OCR: tesseract stderr: {}", stderr.trim());
            }
            tracing::info!("OCR: 识别结果 ({} bytes): {:?}", text.len(), text);
            text.trim_end().to_string()
        }
        Err(e) => {
            tracing::error!("tesseract OCR 失败: {}", e);
            String::new()
        }
    }
}

impl Render for OverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 淡入动画：窗口打开后 150ms 内 dim 遮罩从透明过渡到最终效果
        {
            let elapsed = self.animation_start.elapsed().as_secs_f32();
            const DURATION: f32 = 0.15;
            if elapsed < DURATION {
                self.dim_opacity = elapsed / DURATION;
                cx.notify();
            } else if self.dim_opacity < 1.0 {
                self.dim_opacity = 1.0;
            }
        }

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
        // canvas 闭包会 move visible_cmds，这里提前克隆一份给后面的元素层文字渲染用
        let sel_visible_idx = self.selected_cmd_actual_idx.and_then(|idx| {
            if self.drawing.is_visible(idx) {
                // LIFO 模型：可见命令是 commands[0..history_index]，idx 即位置
                Some(idx)
            } else {
                None
            }
        });
        let scale_factor = self.scale_factor;
        let font_family = cx.theme().font_family.clone();
        // 已提交的 Input 展示态：canvas 应跳过对应 Text 命令，避免文字重复
        let skip_canvas_idx: Option<usize> = if self.text_input_finalized {
            self.text_input_cmd_idx
        } else {
            None
        };

        let ocr_rect = self.ocr_rect;
        let ocr_dragging = self.ocr_drag_start.is_some();
        let dim_opacity = self.dim_opacity;

        let paint_canvas = canvas(
            move |_, _, _| (in_progress, visible_cmds, sel_visible_idx, scale_factor, font_family, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity),
            move |_, (in_progress, visible_cmds, sel_visible_idx, scale_factor, font_family, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity), window, cx| {
                let win_bounds = window.bounds();

                // 1) 把捕获帧作为全屏背景（始终从 (0,0) 开始，确保 canvas 坐标与 frame_pixels 对齐）
                let _ = window.paint_image(
                    Bounds {
                        origin: point(px(0.), px(0.)),
                        size: win_bounds.size,
                    },
                    Default::default(),
                    frame_image.clone(),
                    0,
                    false,
                );

                // 2) 半透明 dim 遮罩（选区外），alpha 随 dim_opacity 动画过渡
                let dim_alpha = (0xAAu32 as f32 * dim_opacity).round() as u32;
                let dim = Hsla::from(rgba(dim_alpha));

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
                    // 跳过已提交但由元素层展示的 Text 命令，避免文字重复
                    for (i, cmd) in visible_cmds.iter().enumerate() {
                        if skip_canvas_idx == Some(i) {
                            continue;
                        }
                        paint_command(cmd, window, cx, scale_factor, &font_family);
                    }
                    if let Some(ref ip) = in_progress {
                        paint_command(ip, window, cx, scale_factor, &font_family);
                    }

                    // 2.55) OCR 框选矩形（高亮半透明 + 绿色描边）
                    if let Some(ocr) = ocr_rect {
                        if ocr_dragging && ocr.size.x > 0.0 && ocr.size.y > 0.0 {
                            let ocr_fill = Hsla::from(rgba(0x00FF8844));
                            let ocr_border = Hsla::from(rgba(0x00FF88FF));
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(px(ocr.origin.x), px(ocr.origin.y)),
                                    size: Size::new(px(ocr.size.x), px(ocr.size.y)),
                                },
                                px(0.),
                                ocr_fill,
                                px(2.0),
                                ocr_border,
                                Default::default(),
                            ));
                        }
                    }

                    // 2.6) 在选中的已绘制命令上渲染拖拽手柄
                    if let Some(vidx) = sel_visible_idx {
                        if let Some(cmd) = visible_cmds.get(vidx) {
                            let handle_fill = Hsla::from(rgba(0xFFFFFFFF));
                            let handle_border = Hsla::from(rgba(0x0066CCFF));
                            let half = px(HANDLE_VISUAL_SIZE / 2.0);
                            let edge = px(HANDLE_VISUAL_SIZE);
                            match cmd {
                                DrawCommand::Rectangle { rect, .. }
                                | DrawCommand::Ellipse { rect, .. } => {
                                    let a = rect.0;
                                    let b = rect.1;
                                    let bounds = ub::Bounds::new(
                                        ub::Point::new(a.x.min(b.x), a.y.min(b.y)),
                                        ub::Point::new(a.x.max(b.x), a.y.max(b.y)),
                                    );
                                    for hp in bounds.handle_positions() {
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
                                DrawCommand::Arrow { from, to, .. } => {
                                    for pt in &[from, to] {
                                        window.paint_quad(quad(
                                            Bounds {
                                                origin: point(px(pt.x) - half, px(pt.y) - half),
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
                                _ => {}
                            }
                        }
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

        // 渲染活动文字输入（gpui-component Input，自带 IME 支持）
        // text_input_rect 存储逻辑像素（与 GPUI 坐标系一致），直接用 px() 做 CSS 定位。
        // 多行模式（auto_grow）：输入内容超过 max_rows 后内部滚动。
        // 不用 .small() —— single-line 模式下高度被钉死在 h_6=24px，
        // multi_line 自动调 h_auto()，垂直 padding 由 input_py 控制；视觉比 small 更宽松。
        if let Some(ref input) = self.text_input {
            let rect = self.text_input_rect;
            let (lx, ly, lw, lh) = (rect.origin.x, rect.origin.y, rect.size.x, rect.size.y);

            if self.text_input_finalized {
                // 已提交：只渲染文字（无拖拽条、手柄、边框），避免文字跳动。
                // 结构必须与编辑态对齐：用一个 invisible 占位块顶替拖拽条(6px)，
                // 保证 flex_1() 容器在两个模式下的可用高度一致，文字位置不变。
                root = root.child(
                    div()
                        .absolute()
                        .top(px(ly))
                        .left(px(lx))
                        .w(px(lw))
                        .h(px(lh))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .w_full()
                                .h(px(6.0)),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .child(
                                    gpui_component::input::Input::new(input)
                                        .appearance(false)
                                        .bordered(false)
                                        .text_color(gpui::rgba(rgba_u32(self.toolbar.current_color)))
                                        .with_size(gpui_component::Size::Size(gpui::px(
                                            self.toolbar.current_size / 0.875 / self.scale_factor,
                                        )))
                                        .font_weight(match self.toolbar.current_weight {
                                            FontWeight::Bold => gpui::FontWeight::BOLD,
                                            FontWeight::Normal => gpui::FontWeight::NORMAL,
                                        }),
                                ),
                        ),
                );
            } else {
                let h_size = 6.0_f32;
                let hh = h_size / 2.0;
                let h_cx = lw / 2.0 - hh;
                let h_cy = lh / 2.0 - hh;
                let h_r = lw - h_size;
                let h_b = lh - h_size;
                root = root.child(
                    div()
                        .absolute()
                        .top(px(ly))
                        .left(px(lx))
                        .w(px(lw))
                        .h(px(lh))
                        .flex()
                        .flex_col()
                        .child(
                            // 顶部拖动 bar
                            div()
                                .id("text-drag-bar")
                                .w_full()
                                .h(px(h_size))
                                .bg(gpui::rgba(0x4A4A4AEE))
                                .rounded_t_md()
                                .cursor_move(),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .child(
                                    gpui_component::input::Input::new(input)
                                        .appearance(false)
                                        .bordered(true)
                                        .text_color(gpui::rgba(rgba_u32(self.toolbar.current_color)))
                                        .with_size(gpui_component::Size::Size(gpui::px(
                                            self.toolbar.current_size / 0.875 / self.scale_factor,
                                        )))
                                        .font_weight(match self.toolbar.current_weight {
                                            FontWeight::Bold => gpui::FontWeight::BOLD,
                                            FontWeight::Normal => gpui::FontWeight::NORMAL,
                                        }),
                                ),
                        )
                        // 8 个 resize 手柄（相对于 outer div，覆盖全框四边）
                        .child(make_resize_handle("text-resize-nw", 0.0, 0.0, TextDragMode::ResizeNW))
                        .child(make_resize_handle("text-resize-n", h_cx, 0.0, TextDragMode::ResizeN))
                        .child(make_resize_handle("text-resize-ne", h_r, 0.0, TextDragMode::ResizeNE))
                        .child(make_resize_handle("text-resize-w", 0.0, h_cy, TextDragMode::ResizeW))
                        .child(make_resize_handle("text-resize-e", h_r, h_cy, TextDragMode::ResizeE))
                        .child(make_resize_handle("text-resize-sw", 0.0, h_b, TextDragMode::ResizeSW))
                        .child(make_resize_handle("text-resize-s", h_cx, h_b, TextDragMode::ResizeS))
                        .child(make_resize_handle("text-resize-se", h_r, h_b, TextDragMode::ResizeSE)),
                );
            }
        }

        // OCR 结果面板（右侧）
        if let Some(ref text) = self.ocr_result {
            let panel_w = 320.0;
            let panel_x = screen_bounds.origin.x + screen_bounds.size.x - panel_w;
            let panel_y = screen_bounds.origin.y;
            let panel_h = screen_bounds.size.y;
            let weak = cx.weak_entity();
            let text_for_ui = text.clone();
            root = root.child(
                div()
                    .absolute()
                    .top(px(panel_y))
                    .left(px(panel_x))
                    .w(px(panel_w))
                    .h(px(panel_h))
                    .bg(gpui::rgba(0x1A1A1AF0))
                    .rounded_l_lg()
                    .border_1()
                    .border_color(gpui::rgba(0x444444CC))
                    .flex()
                    .flex_col()
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _window, _cx| {
                        this.ocr_panel_hovered = true;
                    }))
                    // 标题栏 + 关闭按钮 + 复制按钮
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .p(px(8.0))
                            .border_b_1()
                            .border_color(gpui::rgba(0x44444488))
                            .child(
                                div()
                                    .text_color(gpui::rgba(0xCCCCCCFF))
                                    .text_sm()
                                    .child(gpui::SharedString::from("OCR 识别结果")),
                            )
                            .child(
                                div().flex().gap(px(4.0))
                                    .child({
                                        let t = text_for_ui.clone();
                                        Button::new("ocr-copy")
                                            .icon(IconName::Copy)
                                            .compact()
                                            .on_click(move |_, _window, cx| {
                                                // 用 GPUI 的 write_to_clipboard（底层 X11
                                                // Clipboard 长存于 client），而不是每次
                                                // arboard::Clipboard::new()——后者 drop 即
                                                // 释放 X11 所有权，粘贴拿到空 → "复制没效果"。
                                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                    t.clone(),
                                                ));
                                            })
                                    })
                                    .child({
                                        let w = weak.clone();
                                        Button::new("ocr-close")
                                            .icon(IconName::Close)
                                            .compact()
                                            .on_click(move |_, _, cx| {
                                                if let Err(e) = w.update(cx, |this, cx| {
                                                    this.ocr_result = None;
                                                    this.ocr_rect = None;
                                                    cx.notify();
                                                }) {
                                                    tracing::error!(
                                                        "[OCR] close: 实体更新失败：{e}"
                                                    );
                                                }
                                            })
                                    }),
                            ),
                    )
                    // 文字内容（可选中的富文本，用 TextView 支持鼠标拖选和 Ctrl+C）
                    .child(
                        div()
                            .flex_1()
                            .p(px(10.0))
                            .child({
                                let md = format!("```text\n{}\n```", text_for_ui);
                                gpui_component::text::TextView::markdown("ocr-text", md)
                                    .selectable(true)
                            }),
                    ),
            );
        } else if self.ocr_loading {
            // 加载指示器
            let panel_w = 200.0;
            let panel_x = screen_bounds.origin.x + screen_bounds.size.x - panel_w - 16.0;
            let panel_y = screen_bounds.origin.y + 16.0;
            root = root.child(
                div()
                    .absolute()
                    .top(px(panel_y))
                    .left(px(panel_x))
                    .w(px(panel_w))
                    .h(px(48.0))
                    .bg(gpui::rgba(0x1A1A1AF0))
                    .rounded_lg()
                    .border_1()
                    .border_color(gpui::rgba(0x444444CC))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, _| {})
                    .child(
                        div()
                            .text_color(gpui::rgba(0xCCCCCCFF))
                            .text_sm()
                            .child(gpui::SharedString::from("OCR 识别中…")),
                    ),
            );
        }

        root
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let p = to_bounds_point(ev.position);
                    // 本次点击前是否已有活跃输入框（用于判断点击后是否应新建）
                    let had_text_input = this.text_input.is_some();
                    tracing::debug!(
                        "mouse_down p=({:.1},{:.1}) mode={:?} tool={:?} th={} ti={}",
                        p.x, p.y, this.mode, this.toolbar.active_tool,
                        this.toolbar_hovered, this.text_input.is_some()
                    );

                    // 工具栏区域内的点击交给 Button 自己处理（按钮 .on_click 由 GPUI 在
                    // mouse_up 时触发），root 不应把它当成 \"拖选区/移动选区\" 信号——
                    // 否则 selection.mouse_down 会把当前选区打散，工具栏跟着消失。
                    // 工具栏根 div 的 on_mouse_down 已先 set toolbar_hovered=true，
                    // 这里据此判断；不再用 compute_toolbar_bounds 的几何估算
                    // （按钮带图标+中文标签，真实宽度远超 32px 估算）。
                    if this.mode == OverlayMode::Editing && this.toolbar_hovered {
                        // 工具栏可能覆盖右侧 handle（宽度 > 选区宽度时），
                        // 此时优先处理选区 handle 的 resize 操作。
                        if let Some(sel) = this.selection.current() {
                            if sel.hit_handle(p, HANDLE_HIT_HALF).is_some() {
                                this.selection.mouse_down(p);
                                return;
                            }
                        }
                        return;
                    }

                    // OCR 结果面板：由面板 div 的 on_mouse_down 设 ocr_panel_hovered=true，
                    // root 据此早返回，避免把按钮点击当成"新建选区/移动选区"。
                    if this.ocr_panel_hovered {
                        return;
                    }
                    // OCR 加载中面板（无按钮，保留几何判断）
                    if this.ocr_loading {
                        let pw = 200.0;
                        let px = this.screen_bounds.origin.x + this.screen_bounds.size.x - pw;
                        let py = this.screen_bounds.origin.y;
                        let ph = 48.0;
                        if p.x >= px && p.x <= px + pw && p.y >= py && p.y <= py + ph {
                            window.prevent_default();
                            return;
                        }
                    }

                    // 文字输入框存在时，优先检测是否点击了拖动条或 resize 手柄
                    if this.text_input.is_some() {
                        // 已提交的展示态：点击内部 → 移除 canvas 命令并恢复编辑；
                        // 点击外部 → 移除展示（canvas 命令保留）
                        if this.text_input_finalized {
                            if this.text_input_rect.contains(p) {
                                // 从 drawing 中移除对应的 canvas Text 命令，
                                // 恢复 Input 编辑态，用户可继续修改文字。
                                tracing::debug!("text_input re-edit focus at ({:.1},{:.1})", p.x, p.y);
                                if let Some(idx) = this.text_input_cmd_idx.take() {
                                    this.drawing.remove_visible(idx);
                                }
                                this.text_input_finalized = false;
                                if let Some(ref input) = this.text_input {
                                    input.update(cx, |state, cx| {
                                        state.focus(window, cx);
                                    });
                                }
                                window.prevent_default();
                                cx.notify();
                                return;
                            }
                            this.text_input = None;
                            this.text_input_finalized = false;
                            this.text_input_cmd_idx = None;
                            cx.notify();
                            return;
                        } else {
                            if let Some(drag) = hit_test_text_drag(this.text_input_rect, p) {
                                this.text_input_drag = Some(drag);
                                return;
                            }
                            // 点在输入框内部（非拖拽条/手柄）→ 显式聚焦 Input 组件
                            // 必须 prevent_default()：根 div 的 track_focus 会在 bubble 阶段
                            // 先于 Input 触发自动聚焦，抢走焦点。阻止此行为后 Input 保持聚焦。
                            if this.text_input_rect.contains(p) {
                                tracing::debug!("text_input focus at ({:.1},{:.1})", p.x, p.y);
                                if let Some(ref input) = this.text_input {
                                    input.update(cx, |state, cx| {
                                        state.focus(window, cx);
                                    });
                                }
                                window.prevent_default();
                                return;
                            }
                            // 点在输入框外 → 先提交活跃 Text 输入，避免文字丢失
                            this.finalize_text_input_if_active(cx);
                        }
                    }

                    // 点击已固化的 Text 命令 → 自动切到 Text 工具并重新编辑
                    // （无需手动先点 Text 工具按钮，对任何当前工具都生效）
                    if this.mode == OverlayMode::Editing && this.text_input.is_none() {
                        if let Some(sel) = this.selection.current() {
                            if sel.contains(p) {
                                let visible: Vec<(usize, &DrawCommand)> = this
                                    .drawing
                                    .visible_commands_with_indices()
                                    .collect();
                                let mut edit = None;
                                for (idx, cmd) in visible.iter().rev() {
                                    if hit_test_text_cmd(cmd, p) {
                                        if let DrawCommand::Text { anchor, content, font_size, max_width, weight, color } = cmd {
                                            edit = Some((*idx, BoundsPoint::new(anchor.x, anchor.y), content.clone(), *font_size, *max_width, *weight, *color));
                                        }
                                        break;
                                    }
                                }
                                if let Some((idx, old_anchor, old_content, old_fs, old_mw, old_wt, old_clr)) = edit {
                                    tracing::debug!("mouse_down: HIT text cmd idx={}", idx);
                                    this.drawing.remove_visible(idx);
                                    this.selected_cmd_actual_idx = None;
                                    this.toolbar.active_tool = Some(ToolButton::Text);
                                    this.toolbar.popup = None;
                                    this.toolbar.current_size = old_fs;
                                    this.toolbar.current_weight = old_wt;
                                    this.toolbar.current_color = old_clr;
                                    this.open_text_input_with_content(old_anchor, old_content, old_mw, window, cx);
                                    return;
                                }
                            }
                        }
                    }

                    // Editing 模式下：优先检测已绘制命令的手柄/主体命中（顶部命令优先）
                    // Text 工具不需要命令拖拽——点在已绘制命令上的点击应打开
                    // 文字输入而非拖拽，所以 Text 激活时跳过命中检测。
                    if this.mode == OverlayMode::Editing {
                        if this.toolbar.active_tool != Some(ToolButton::Text) {
                            let visible: Vec<(usize, &DrawCommand)> = this
                                .drawing
                                .visible_commands_with_indices()
                                .collect();
                            for (idx, cmd) in visible.iter().rev() {
                                if let Some(mode) = hit_test_cmd_drag(cmd, p) {
                                    this.selected_cmd_actual_idx = Some(*idx);
                                    this.cmd_drag = Some(CmdDragState {
                                        mode,
                                        start_mouse: p,
                                        cmd_index: *idx,
                                    });
                                    tracing::debug!("mouse_down: HIT cmd idx={}", idx);
                                    return;
                                }
                            }
                        }
                        // 未命中任何命令 + 无绘图工具 → 取消选中
                        if this.toolbar.active_tool.is_none()
                            || this.toolbar.active_tool == Some(ToolButton::Text)
                        {
                            this.selected_cmd_actual_idx = None;
                        }
                    }

                    // Editing 模式下分发
                    if this.mode == OverlayMode::Editing {
                        if let Some(sel) = this.selection.current() {
                            // 1) handle 命中 — 最高优先，即使 text_input 开着也能 resize 选区
                            if sel.hit_handle(p, HANDLE_HIT_HALF).is_some() {
                                tracing::debug!("mouse_down: HIT handle, start resize/move");
                                this.selection.mouse_down(p);
                                return;
                            }
                            tracing::debug!(
                                "mouse_down: no hit. sel=({:.0},{:.0} {}x{}) p=({:.0},{:.0})",
                                sel.origin.x, sel.origin.y, sel.size.x, sel.size.y, p.x, p.y
                            );
                            // 2) Text 工具 + 选区内点击 → 打开 inline 输入（自带 IME）
                            // 重新编辑已在上方统一处理，这里只管新建空白输入。
                            // 若点击前就有活跃输入框（本次点击把它提交了），
                            // 不应立即再开新框——用户只是想结束编辑。
                            if this.toolbar.active_tool == Some(ToolButton::Text)
                                && sel.contains(p)
                                && this.text_input.is_none()
                                && !had_text_input
                            {
                                this.open_text_input(p, window, cx);
                                return;
                            }
                            // 2.5) OCR 工具 + 选区内点击 → 开始框选识别区域
                            if this.toolbar.active_tool == Some(ToolButton::Ocr)
                                && sel.contains(p)
                            {
                                this.finalize_text_input_if_active(cx);
                                this.ocr_result = None;
                                this.ocr_rect = Some(ub::Bounds::new(p, BoundsPoint::ZERO));
                                this.ocr_drag_start = Some(p);
                                tracing::info!(
                                    "OCR drag start: p=({:.1},{:.1}) sel=({:.1},{:.1} {}x{}) sf={:.2}",
                                    p.x, p.y,
                                    sel.origin.x, sel.origin.y, sel.size.x, sel.size.y,
                                    this.scale_factor,
                                );
                                return;
                            }
                            // 3) active_tool 选了绘图工具 + 点在选区内 → 开始绘图
                            if this.toolbar.active_tool.is_some() && sel.contains(p) {
                                this.finalize_text_input_if_active(cx);
                                this.begin_draw(p);
                                return;
                            }
                        }
                    }
                    // Editing 模式下已有截图框时，禁止点击框外区域（按钮栏、OCR 面板除外，
                    // 它们已在上面被拦截 return）。点击 dim 区域不再打散/重选选区。
                    if this.mode == OverlayMode::Editing && this.selection.current().is_some() {
                        return;
                    }
                    // Selecting 模式或无选区：点击任意位置开始新选区
                    this.finalize_text_input_if_active(cx);
                    this.selection.mouse_down(p);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let mut p = to_bounds_point(ev.position);
                // 将鼠标位置裁剪到屏幕范围内，防止所有拖拽操作超出截图区域。
                // GPUI 在快速拖拽时可能报告窗口外的坐标。
                p.x = p.x.clamp(
                    this.screen_bounds.origin.x,
                    this.screen_bounds.origin.x + this.screen_bounds.size.x,
                );
                p.y = p.y.clamp(
                    this.screen_bounds.origin.y,
                    this.screen_bounds.origin.y + this.screen_bounds.size.y,
                );
                // 绘制中（in_progress）/ 拖拽命令 / OCR 框选时进一步裁剪到选区边界，
                // 防止矩形/箭头/自由画笔超出截图框进入 dim 区域。
                let sel = this.selection.current();
                if this.in_progress.is_some() || this.cmd_drag.is_some() || this.ocr_drag_start.is_some() {
                    if let Some(s) = sel {
                        p.x = p.x.clamp(s.origin.x, s.origin.x + s.size.x);
                        p.y = p.y.clamp(s.origin.y, s.origin.y + s.size.y);
                    }
                }
                // 优先处理文字输入框的拖动 / resize
                if let Some(drag) = this.text_input_drag {
                    apply_text_drag(this, drag, p);
                    cx.notify();
                    return;
                }
                // 处理命令拖拽
                if let Some(drag) = this.cmd_drag {
                    apply_cmd_drag(this, drag, p);
                    cx.notify();
                    return;
                }
                // OCR 框选中：更新 ocr_rect
                if let Some(start) = this.ocr_drag_start {
                    let x1 = start.x.min(p.x);
                    let y1 = start.y.min(p.y);
                    let x2 = start.x.max(p.x);
                    let y2 = start.y.max(p.y);
                    this.ocr_rect = Some(ub::Bounds {
                        origin: BoundsPoint::new(x1, y1),
                        size: BoundsPoint::new(x2 - x1, y2 - y1),
                    });
                    cx.notify();
                    return;
                }
                if this.in_progress.is_some() {
                    this.update_in_progress(p);
                } else {
                    this.selection.mouse_move(p);
                }
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // 工具栏按钮 on_click 在 mouse_up 阶段触发，到这里 toolbar_hovered
                    // 已完成它的使命；清回 false 避免下次非工具栏点击误判。
                    this.toolbar_hovered = false;
                    this.ocr_panel_hovered = false;
                    // 文字框拖动 / resize 结束
                    if this.text_input_drag.is_some() {
                        this.text_input_drag = None;
                        return;
                    }
                    // 命令拖拽结束
                    if this.cmd_drag.is_some() {
                        this.cmd_drag = None;
                        return;
                    }
                    // OCR 框选结束 → 提取像素、启动识别
                    if this.ocr_drag_start.is_some() {
                        tracing::info!(
                            "OCR mouse_up: drag_start=({:.1},{:.1}) ocr_rect={:?}",
                            this.ocr_drag_start.unwrap().x,
                            this.ocr_drag_start.unwrap().y,
                            this.ocr_rect.map(|r| (r.origin.x, r.origin.y, r.size.x, r.size.y)),
                        );
                        this.ocr_drag_start = None;
                        if let Some(rect) = this.ocr_rect {
                            if rect.size.x > 5.0 && rect.size.y > 5.0 {
                                let pixels = this.frame_pixels.clone();
                                let fw = this.frame_width;
                                let fh = this.frame_height;
                                this.ocr_loading = true;
                                cx.notify();
                                let wb = window.bounds();
                                let text = run_ocr_sync(rect, &pixels, fw, fh, f32::from(wb.size.width), f32::from(wb.size.height));
                                tracing::info!("OCR result len={}, content={:?}", text.len(), text);
                                this.ocr_result = if text.is_empty() { None } else { Some(text) };
                                this.ocr_loading = false;
                                cx.notify();
                            } else {
                                tracing::info!("OCR rect too small, cleared");
                                this.ocr_rect = None;
                            }
                        } else {
                            tracing::info!("OCR ocr_rect is None");
                        }
                        return;
                    }
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
                                    cx.notify();
                                    return;
                                }
                            }
                            // 没有有效选区 → 保持 Selecting（等用户继续拖）
                        }
                        OverlayMode::Editing => {
                            // 在 Editing 模式下松开只是结束 resize / moving，
                            // 不 commit；用户必须点"完成"或按 Enter 才确认
                            cx.notify();
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
                        this.commit(OverlayResult { selection: sel, commands: cmds, no_clipboard: false }, window);
                    }
                }),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    // popover 打开时：Esc 只收起 popover，不结束会话（避免误关）
                    if this.toolbar.popup.is_some() {
                        this.toolbar.popup = None;
                        cx.notify();
                        return;
                    }
                    this.commit(OverlayResult { selection: None, commands: vec![], no_clipboard: false }, window);
                } else if ev.keystroke.key == "enter" {
                    // Enter 先尝试提交活跃的 Text 输入（如果 Text 工具正在输入）。
                    // 若 finalize 了 Text 命令，说明这次 Enter 是\"写字时按 Enter 提交\"
                    // 语义，不应同时 commit 整个会话——直接 return 让用户继续编辑。
                    let had_text = this.text_input.is_some();
                    this.finalize_text_input_if_active(cx);
                    if had_text && this.text_input.is_none() {
                        return;
                    }
                    // 否则 Enter 直接确认当前选区；没有选区则全屏
                    let sel = this.selection.current().or(Some(this.screen_bounds));
                    let cmds: Vec<DrawCommand> = this
                        .drawing
                        .visible_commands()
                        .cloned()
                        .collect();
                    this.commit(OverlayResult { selection: sel, commands: cmds, no_clipboard: false }, window);
                } else if ev.keystroke.key == "z" && ev.keystroke.modifiers.control {
                    // Ctrl+Z 撤销 / Ctrl+Shift+Z 重做
                    if ev.keystroke.modifiers.shift {
                        this.drawing.redo();
                    } else {
                        this.drawing.undo();
                    }
                    this.check_selected_visible();
                    cx.notify();
                } else if ev.keystroke.key == "c" && ev.keystroke.modifiers.control {
                    // Ctrl+C：若 OCR 结果面板可见，复制全部识别文字
                    // 与复制按钮一样走 GPUI 长存剪贴板，避免 arboard drop 丢 X11 所有权
                    if let Some(ref text) = this.ocr_result {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    }
                }
            }))
    }
}

/// 标题栏按钮 tooltip 视图
struct TooltipLabel {
    text: gpui::SharedString,
}

impl Render for TooltipLabel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .bg(rgba(0x2d2d2dee))
            .rounded(px(3.0))
            .border_1()
            .border_color(rgba(0x55555588))
            .text_color(rgba(0xeeeeeeff))
            .text_size(px(11.0))
            .child(self.text.clone())
    }
}

/// Pin 窗口视图：显示固定到桌面的标注截图
#[derive(Clone, Copy, PartialEq)]
enum HoveredButton {
    AlwaysOnTop,
    Minimize,
    Maximize,
    Close,
}

struct PinWindowView {
    image: Arc<RenderImage>,
    focus_handle: FocusHandle,
    is_always_on_top: bool,
    hovered_button: Option<HoveredButton>,
}

impl PinWindowView {
    fn new(frame: &CapturedFrame, cx: &mut Context<Self>) -> Self {
        tracing::info!(
            "[Pin] PinWindowView::new: frame={}x{}",
            frame.width, frame.height
        );
        Self {
            image: build_render_image(frame),
            focus_handle: cx.focus_handle(),
            is_always_on_top: false,
            hovered_button: None,
        }
    }
}

impl Render for PinWindowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        let focus_handle = self.focus_handle.clone();
        let is_always_on_top = self.is_always_on_top;
        let hovered_button = self.hovered_button;
        let entity = cx.entity().downgrade();

        let paint_canvas = canvas(
            move |_, _, _| image.clone(),
            move |bounds, image, window, _cx| {
                let _ = window.paint_image(
                    bounds,
                    Default::default(),
                    image.clone(),
                    0,
                    false,
                );
            },
        );

        let entity_for_pin = entity.clone();
        let is_on_top = is_always_on_top;

        div()
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(rgba(0x00000088))
            .border_1()
            .border_color(rgba(0xffffff22))
            .child(
                // 自定义标题栏
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(32.0))
                    .px(px(6.0))
                    .gap(px(6.0))
                    .bg(rgba(0x353535ee))
                    .text_color(rgba(0xffffffff))
                    // 左侧：置顶按钮
                    .child(
                        div()
                            .id("pin-always-on-top")
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(28.0))
                            .h(px(28.0))
                            .rounded(px(4.0))
                            .when(
                                is_on_top
                                    || hovered_button
                                        == Some(
                                            HoveredButton::AlwaysOnTop,
                                        ),
                                |d| {
                                    let alpha = if is_on_top
                                        && hovered_button
                                            == Some(
                                                HoveredButton::AlwaysOnTop,
                                            )
                                    {
                                        0x66
                                    } else if is_on_top {
                                        0x55
                                    } else {
                                        0x44
                                    };
                                    d.bg(rgba(0xffffff00 | alpha))
                                },
                            )
                            .on_hover({
                                let entity = entity_for_pin.clone();
                                move |hovered: &bool,
                                      _window: &mut Window,
                                      app: &mut App| {
                                    let _ = entity.update(
                                        app,
                                        |this, cx| {
                                            if *hovered {
                                                this.hovered_button =
                                                    Some(
                                                        HoveredButton::AlwaysOnTop,
                                                    );
                                            } else if this
                                                .hovered_button
                                                == Some(
                                                    HoveredButton::AlwaysOnTop,
                                                )
                                            {
                                                this.hovered_button =
                                                    None;
                                            }
                                            cx.notify();
                                        },
                                    );
                                }
                            })
                            .tooltip(|_window, app| {
                                app.new(|_cx| TooltipLabel {
                                    text: "固定".into(),
                                })
                                .into()
                            })
                            .child(
                                Icon::new(IconName::ArrowUp)
                                    .size(px(14.0)),
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                {
                                    let entity = entity_for_pin.clone();
                                    move |_ev: &MouseDownEvent,
                                          window: &mut Window,
                                          app: &mut App| {
                                        let new_state = !is_on_top;
                                        #[cfg(target_os = "linux")]
                                        send_wm_state_above(
                                            window, new_state,
                                        );
                                        let _ = entity.update(
                                            app,
                                            |this, cx| {
                                                this.is_always_on_top =
                                                    new_state;
                                                cx.notify();
                                            },
                                        );
                                    }
                                },
                            ),
                    )
                    // 中间：可拖拽空白区域
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .on_mouse_down(
                                MouseButton::Left,
                                move |_ev: &MouseDownEvent,
                                      window: &mut Window,
                                      _app: &mut App| {
                                    window.start_window_move();
                                },
                            ),
                    )
                    // 右侧：最小化、最大化、关闭
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .id("pin-minimize")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .rounded(px(4.0))
                                    .when(
                                        hovered_button
                                            == Some(
                                                HoveredButton::Minimize,
                                            ),
                                        |d| d.bg(rgba(0xffffff44)),
                                    )
                                    .on_hover({
                                        let entity = entity.clone();
                                        move |hovered: &bool,
                                              _window: &mut Window,
                                              app: &mut App| {
                                            let _ = entity.update(
                                                app,
                                                |this, cx| {
                                                    if *hovered {
                                                        this.hovered_button =
                                                            Some(
                                                                HoveredButton::Minimize,
                                                            );
                                                    } else if this
                                                        .hovered_button
                                                        == Some(
                                                            HoveredButton::Minimize,
                                                        )
                                                    {
                                                        this.hovered_button =
                                                            None;
                                                    }
                                                    cx.notify();
                                                },
                                            );
                                        }
                                    })
                                    .tooltip(|_window, app| {
                                        app.new(|_cx| TooltipLabel {
                                            text: "最小化".into(),
                                        })
                                        .into()
                                    })
                                    .child(
                                        Icon::new(IconName::Minimize)
                                            .size(px(14.0)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        move |_ev: &MouseDownEvent,
                                              window: &mut Window,
                                              _app: &mut App| {
                                            #[cfg(target_os = "linux")]
                                            pin_minimize_window(window);
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .id("pin-maximize")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .rounded(px(4.0))
                                    .when(
                                        hovered_button
                                            == Some(
                                                HoveredButton::Maximize,
                                            ),
                                        |d| d.bg(rgba(0xffffff44)),
                                    )
                                    .on_hover({
                                        let entity = entity.clone();
                                        move |hovered: &bool,
                                              _window: &mut Window,
                                              app: &mut App| {
                                            let _ = entity.update(
                                                app,
                                                |this, cx| {
                                                    if *hovered {
                                                        this.hovered_button =
                                                            Some(
                                                                HoveredButton::Maximize,
                                                            );
                                                    } else if this
                                                        .hovered_button
                                                        == Some(
                                                            HoveredButton::Maximize,
                                                        )
                                                    {
                                                        this.hovered_button =
                                                            None;
                                                    }
                                                    cx.notify();
                                                },
                                            );
                                        }
                                    })
                                    .tooltip(|_window, app| {
                                        app.new(|_cx| TooltipLabel {
                                            text: "最大化".into(),
                                        })
                                        .into()
                                    })
                                    .child(
                                        Icon::new(IconName::Maximize)
                                            .size(px(14.0)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        move |_ev: &MouseDownEvent,
                                              window: &mut Window,
                                              _app: &mut App| {
                                            #[cfg(target_os = "linux")]
                                            pin_toggle_maximize(window);
                                        },
                                    ),
                            )
                            .child(
                                div()
                                    .id("pin-close")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .rounded(px(4.0))
                                    .when(
                                        hovered_button
                                            == Some(HoveredButton::Close),
                                        |d| d.bg(rgba(0xe81123cc)),
                                    )
                                    .on_hover({
                                        let entity = entity.clone();
                                        move |hovered: &bool,
                                              _window: &mut Window,
                                              app: &mut App| {
                                            let _ = entity.update(
                                                app,
                                                |this, cx| {
                                                    if *hovered {
                                                        this.hovered_button =
                                                            Some(
                                                                HoveredButton::Close,
                                                            );
                                                    } else if this
                                                        .hovered_button
                                                        == Some(
                                                            HoveredButton::Close,
                                                        )
                                                    {
                                                        this.hovered_button =
                                                            None;
                                                    }
                                                    cx.notify();
                                                },
                                            );
                                        }
                                    })
                                    .tooltip(|_window, app| {
                                        app.new(|_cx| TooltipLabel {
                                            text: "关闭".into(),
                                        })
                                        .into()
                                    })
                                    .child(
                                        Icon::new(IconName::Close)
                                            .size(px(14.0)),
                                    )
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        move |_ev: &MouseDownEvent,
                                              window: &mut Window,
                                              _app: &mut App| {
                                            window.remove_window();
                                        },
                                    ),
                            ),
                    ),
            )
            .child(paint_canvas.flex_1())
            .on_key_down(|ev: &KeyDownEvent, window, _cx| {
                if ev.keystroke.key == "escape" {
                    window.remove_window();
                }
            })
    }
}

/// 通过 EWMH _NET_WM_STATE_ABOVE 切换窗口置顶状态
#[cfg(target_os = "linux")]
fn send_wm_state_above(window: &mut Window, add: bool) {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask, send_event};
    use x11rb::xcb_ffi::XCBConnection;

    if let (Ok(wh), Ok(dh)) = (window.window_handle(), window.display_handle()) {
        if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
            (wh.as_raw(), dh.as_raw())
        {
            if let Some(conn_ptr) = xcb_dh.connection {
                let conn_result = unsafe {
                    XCBConnection::from_raw_xcb_connection(conn_ptr.as_ptr().cast(), false)
                };
                if let Ok(conn) = conn_result {
                    let root = conn.setup().roots[0].root;
                    let net_wm_state = conn
                        .intern_atom(false, b"_NET_WM_STATE")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    let net_wm_state_above = conn
                        .intern_atom(false, b"_NET_WM_STATE_ABOVE")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    if let (Some(state_atom), Some(above_atom)) =
                        (net_wm_state, net_wm_state_above)
                    {
                        let action: u32 = if add { 1 } else { 0 };
                        let event = ClientMessageEvent::new(
                            32,
                            xcb_wh.window.into(),
                            state_atom,
                            [action, above_atom.into(), 0, 1, 0],
                        );
                        let _ = send_event(
                            &conn,
                            false,
                            root,
                            EventMask::SUBSTRUCTURE_REDIRECT
                                | EventMask::SUBSTRUCTURE_NOTIFY,
                            event,
                        );
                        let _ = conn.flush();
                        tracing::info!(
                            "[Pin] always_on_top {} (action={})",
                            if add { "on" } else { "off" },
                            action
                        );
                    }
                }
            }
        }
    }
}

/// 通过 X11 原生协议最小化窗口
#[cfg(target_os = "linux")]
fn pin_minimize_window(window: &mut Window) {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask, send_event};
    use x11rb::xcb_ffi::XCBConnection;

    if let (Ok(wh), Ok(dh)) = (window.window_handle(), window.display_handle()) {
        if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
            (wh.as_raw(), dh.as_raw())
        {
            if let Some(conn_ptr) = xcb_dh.connection {
                let conn_result = unsafe {
                    XCBConnection::from_raw_xcb_connection(conn_ptr.as_ptr().cast(), false)
                };
                if let Ok(conn) = conn_result {
                    const ICONIC_STATE: u32 = 3;
                    let wm_change_state = conn
                        .intern_atom(false, b"WM_CHANGE_STATE")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    if let Some(cs_atom) = wm_change_state {
                        let event = ClientMessageEvent::new(
                            32,
                            xcb_wh.window.into(),
                            cs_atom,
                            [ICONIC_STATE, 0, 0, 0, 0],
                        );
                        let _ = send_event(
                            &conn,
                            false,
                            conn.setup().roots[0].root,
                            EventMask::SUBSTRUCTURE_REDIRECT
                                | EventMask::SUBSTRUCTURE_NOTIFY,
                            event,
                        );
                        let _ = conn.flush();
                    }
                }
            }
        }
    }
}

/// 通过 X11 原生协议最大化/还原窗口
#[cfg(target_os = "linux")]
fn pin_toggle_maximize(window: &mut Window) {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask, send_event};
    use x11rb::xcb_ffi::XCBConnection;

    if let (Ok(wh), Ok(dh)) = (window.window_handle(), window.display_handle()) {
        if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
            (wh.as_raw(), dh.as_raw())
        {
            if let Some(conn_ptr) = xcb_dh.connection {
                let conn_result = unsafe {
                    XCBConnection::from_raw_xcb_connection(conn_ptr.as_ptr().cast(), false)
                };
                if let Ok(conn) = conn_result {
                    let net_wm_state = conn
                        .intern_atom(false, b"_NET_WM_STATE")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    let max_h = conn
                        .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_HORZ")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    let max_v = conn
                        .intern_atom(false, b"_NET_WM_STATE_MAXIMIZED_VERT")
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| r.atom);
                    if let (Some(state), Some(h), Some(v)) =
                        (net_wm_state, max_h, max_v)
                    {
                        let event = ClientMessageEvent::new(
                            32,
                            xcb_wh.window.into(),
                            state,
                            [2, h.into(), v.into(), 1, 0], // 2=Toggle
                        );
                        let _ = send_event(
                            &conn,
                            false,
                            conn.setup().roots[0].root,
                            EventMask::SUBSTRUCTURE_REDIRECT
                                | EventMask::SUBSTRUCTURE_NOTIFY,
                            event,
                        );
                        let _ = conn.flush();
                    }
                }
            }
        }
    }
}


/// 在新线程中启动独立的 GPUI 窗口，展示标注后的截图
fn spawn_pin_window(pin_frame: CapturedFrame, origin_x: f32, origin_y: f32, sx: f32, sy: f32) {
    std::thread::spawn(move || {
        application()
            .with_assets(gpui_component_assets::Assets)
            .run(move |cx: &mut App| {
                gpui_component::init(cx);

                // pin_frame 尺寸是物理像素，转为逻辑像素用于窗口尺寸
                let img_w = pin_frame.width as f32 / sx;
                let img_h = pin_frame.height as f32 / sy;
                let max_w = 1200.0_f32;
                let max_h = 900.0_f32;
                const MIN_IMG_W: f32 = 150.0;
                let scale = (max_w / img_w)
                    .min(max_h / img_h)
                    .min(1.0)
                    .max(MIN_IMG_W / img_w);
                // 自定义标题栏高度（原生标题栏已移除，由 PinWindowView render 绘制）
                const CUSTOM_TITLEBAR_H: f32 = 32.0;
                let win_w = px(img_w * scale);
                let win_h = px(img_h * scale + CUSTOM_TITLEBAR_H);
                // 使用 Normal 窗口：支持 start_window_move / 键盘事件等 WM 交互
                tracing::info!(
                    "[Pin] spawn window: origin=({:.0},{:.0}) img_logical={:.1}x{:.1} img_physical={}x{} win_size={:.1}x{:.1} scale={:.2}",
                    origin_x, origin_y,
                    img_w, img_h,
                    pin_frame.width, pin_frame.height,
                    win_w, win_h, scale
                );

                let target_x = origin_x;
                // 窗口上移自定义标题栏高度，使图片内容与原始选区位置对齐。
                // 图片渲染在标题栏下方 y=32 处，因此窗口原点需设于 origin_y - 32。
                let target_y = origin_y - CUSTOM_TITLEBAR_H;
                let frame_y = origin_y - CUSTOM_TITLEBAR_H;

                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(target_x), px(target_y)),
                            size: Size::new(win_w, win_h),
                        })),
                        titlebar: None,
                        window_background: WindowBackgroundAppearance::Transparent,
                        kind: WindowKind::Normal,
                        is_movable: false,
                        is_resizable: false,
                        is_minimizable: true,
                        window_decorations: Some(WindowDecorations::Client),
                        focus: true,
                        ..Default::default()
                    },
                    move |window, cx| {
                        let actual = window.bounds();
                        tracing::info!(
                            "[Pin] actual window after open: origin=({:.0},{:.0}) size=({:.0},{:.0})",
                            actual.origin.x, actual.origin.y, actual.size.width, actual.size.height
                        );

                        #[cfg(target_os = "linux")]
                        {
                            use x11rb::connection::Connection;
                            use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
                            use x11rb::protocol::xproto::{AtomEnum, ConfigureWindowAux, ConnectionExt, PropMode};
                            use x11rb::x11_utils::Serialize;
                            use x11rb::xcb_ffi::XCBConnection;

                            if let (Ok(wh), Ok(dh)) =
                                (window.window_handle(), window.display_handle())
                            {
                                if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
                                    (wh.as_raw(), dh.as_raw())
                                {
                                    if let Some(conn_ptr) = xcb_dh.connection {
                                        let conn_result = unsafe {
                                            XCBConnection::from_raw_xcb_connection(
                                                conn_ptr.as_ptr().cast(),
                                                false,
                                            )
                                        };
                                        match conn_result {
                                            Ok(conn) => {
                                                // 设置 WM_NORMAL_HINTS 的 PPosition 标志，
                                                // 告知窗口管理器此窗口位置由程序显式指定
                                                let nh_atom = conn
                                                    .intern_atom(false, b"WM_NORMAL_HINTS")
                                                    .ok()
                                                    .and_then(|c| c.reply().ok())
                                                    .map(|r| r.atom);
                                                let sh_atom = conn
                                                    .intern_atom(false, b"WM_SIZE_HINTS")
                                                    .ok()
                                                    .and_then(|c| c.reply().ok())
                                                    .map(|r| r.atom);
                                                if let (Some(nh), Some(sh)) = (nh_atom, sh_atom) {
                                                    let mut size_hints = WmSizeHints::new();
                                                    size_hints.position = Some((
                                                        WmSizeHintsSpecification::ProgramSpecified,
                                                        target_x as i32,
                                                        frame_y as i32,
                                                    ));
                                                    let data = size_hints.serialize();
                                                    let _ = conn.change_property(
                                                        PropMode::REPLACE,
                                                        xcb_wh.window.into(),
                                                        nh,
                                                        sh,
                                                        32,
                                                        (data.len() / 4) as u32,
                                                        &data,
                                                    );
                                                    tracing::info!(
                                                        "[Pin] WM_NORMAL_HINTS set PPosition ({:.0},{:.0})",
                                                        target_x, frame_y
                                                    );
                                                }

                                                // 设置 _MOTIF_WM_HINTS 移除服务端窗口装饰（兜底）
                                                let mh_result =
                                                    conn.intern_atom(
                                                        false,
                                                        b"_MOTIF_WM_HINTS",
                                                    );
                                                if let Ok(mh_cookie) = mh_result {
                                                    if let Ok(mh_reply) =
                                                        mh_cookie.reply()
                                                    {
                                                        let hints: [u32; 5] =
                                                            [2, 0, 0, 0, 0];
                                                        let hint_bytes: [u8; 20] =
                                                            unsafe {
                                                                std::mem::transmute(
                                                                    hints,
                                                                )
                                                            };
                                                        let _ = conn.change_property(
                                                            PropMode::REPLACE,
                                                            xcb_wh.window.into(),
                                                            mh_reply.atom,
                                                            mh_reply.atom,
                                                            32,
                                                            5,
                                                            &hint_bytes,
                                                        );
                                                        tracing::info!(
                                                            "[Pin] _MOTIF_WM_HINTS no-decorations"
                                                        );
                                                    }
                                                }

                                                // 读取 _NET_FRAME_EXTENTS 获取 WM 附加的边框高度，
                                                // 用于修正窗口位置（客户端装饰下应为 0，但部分
                                                // WM 可能仍添加阴影/边框导致内容偏移）
                                                let mut frame_extent_top: u32 = 0;
                                                let net_fe_result = conn
                                                    .intern_atom(false, b"_NET_FRAME_EXTENTS");
                                                if let Ok(net_fe_cookie) = net_fe_result {
                                                    if let Ok(net_fe_reply) = net_fe_cookie.reply() {
                                                        if let Ok(reply) = conn.get_property(
                                                            false,
                                                            xcb_wh.window.into(),
                                                            net_fe_reply.atom,
                                                            AtomEnum::CARDINAL,
                                                            0,
                                                            4,
                                                        ) {
                                                            if let Ok(reply) = reply.reply() {
                                                                if reply.value.len() >= 16 {
                                                                    let left = u32::from_ne_bytes(reply.value[0..4].try_into().unwrap_or_default());
                                                                    let right = u32::from_ne_bytes(reply.value[4..8].try_into().unwrap_or_default());
                                                                    frame_extent_top = u32::from_ne_bytes(reply.value[8..12].try_into().unwrap_or_default());
                                                                    let bottom = u32::from_ne_bytes(reply.value[12..16].try_into().unwrap_or_default());
                                                                    tracing::info!(
                                                                        "[Pin] frame_extents: left={} right={} top={} bottom={}",
                                                                        left, right, frame_extent_top, bottom
                                                                    );
                                                                }
                                                            }
                                                        }
                                                    }
                                                }

                                                let adjusted_y = frame_y as i32 - frame_extent_top as i32;
                                                let values = ConfigureWindowAux::new()
                                                    .x(target_x as i32)
                                                    .y(adjusted_y);
                                                if let Err(e) =
                                                    conn.configure_window(xcb_wh.window.into(), &values)
                                                {
                                                    tracing::warn!(
                                                        "[Pin] configure_window failed: {:?}",
                                                        e
                                                    );
                                                }
                                                let _ = conn.flush();
                                                tracing::info!(
                                                    "[Pin] X11 moved window to ({:.0},{:.0}) frame_extent_top={}",
                                                    target_x, adjusted_y, frame_extent_top
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "[Pin] XCB connection failed: {:?}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        let view = cx.new(|cx| PinWindowView::new(&pin_frame, cx));
                        let handle = view.read(cx).focus_handle.clone();
                        handle.focus(window, cx);
                        view
                    },
                )
                .expect("open pin window failed");

                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
            });
    });
}

/// 在新线程里跑 GPUI 覆盖窗口，阻塞到用户完成/取消。
///
/// 返回值：完整会话结果（选区 + 可见的 DrawCommand 列表）；取消时 selection=None。
pub fn run_blocking(frame: CapturedFrame, screen_bounds: ub::Bounds) -> OverlayResult {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // 注册 gpui-component-assets 提供默认 Lucide 图标 svg 资源。
        // 不调用时 IconName::XXX 渲染会找不到 svg、按钮看不出图标。
        application()
            .with_assets(gpui_component_assets::Assets)
            .run(move |cx: &mut App| {
            // gpui-component 必须在第一个窗口前初始化，否则全局主题/状态会 panic
            gpui_component::init(cx);

            // screen_bounds 来自 CapturedFrame 的物理像素尺寸，但 GPUI 所有
            // 坐标（鼠标事件、paint 定位）都使用逻辑像素。这里用主屏 bounds
            // 与帧尺寸的比率作为 scale_factor，把 screen_bounds 转为逻辑像素，
            // 来保证 clamp_inside 等边界检查生效。
            let win_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or(Bounds {
                origin: point(px(0.), px(0.)),
                size: Size::new(px(screen_bounds.size.x), px(screen_bounds.size.y)),
            });

            cx.open_window(
                WindowOptions {
                    // PopUp 窗口在 X11 上可能绕过 WM 的 strut 约束，
                    // 覆盖到系统面板区域，确保 dim 遮罩对齐屏幕顶部。
                    window_bounds: Some(WindowBounds::Windowed(win_bounds)),
                    window_background: WindowBackgroundAppearance::Transparent,
                    titlebar: None,
                    kind: WindowKind::PopUp,
                    is_movable: false,
                    is_resizable: false,
                    focus: true,
                    ..Default::default()
                },
                move |window, cx| {
                    // 根据窗口实际屏幕位置裁剪截图帧，确保截图与桌面完美对齐。
                    // WM 可能把窗口放在工作区内（不含系统面板），不裁剪会导致
                    // 截图内容与真实桌面错位，产生"重影"。
                    let actual = window.bounds();
                    let phys_w = screen_bounds.size.x;
                    let phys_h = screen_bounds.size.y;
                    let actual_w = f32::from(actual.size.width).max(1.0);
                    let actual_h = f32::from(actual.size.height).max(1.0);
                    let sx = phys_w / actual_w;
                    let sy = phys_h / actual_h;
                    let src_x = (f32::from(actual.origin.x) * sx) as u32;
                    let src_y = (f32::from(actual.origin.y) * sy) as u32;
                    let clip_w = ((actual_w * sx) as u32).min(frame.width.saturating_sub(src_x));
                    let clip_h = ((actual_h * sy) as u32).min(frame.height.saturating_sub(src_y));
                    let clipped = frame.clip_region(src_x, src_y, clip_w, clip_h)
                        .unwrap_or(frame);

                    let scale = clipped.width as f32 / actual_w;
                    let logical_bounds = ub::Bounds::new(
                        ub::Point::ZERO,
                        ub::Point::new(actual_w, actual_h),
                    );

                    let view = cx.new(|cx| OverlayView::new(&clipped, logical_bounds, scale, tx, cx));
                    // 主动把焦点给到 view 自己的 focus_handle，
                    // 这样 track_focus 的 div 能收到键盘事件
                    let handle = view.read(cx).focus_handle.clone();
                    handle.focus(window, cx);
                    // PopUp 窗口（override_redirect）在 X11 上绕过 WM 焦点管理，
                    // 必须手动 activate 才能收到键盘事件。
                    window.activate_window();
                    // 必须用 gpui_component::Root 包一层：
                    // gpui-component 的 Input 在 blur 时会调
                    // `Root::update(window, cx, ...)` 去清 `focused_input`，
                    // 找不到 Root 会 panic "BUG: window first layer should be
                    // a gpui_component::Root." → 整个 GPUI 线程 panic →
                    // 覆盖窗口闪退（用户报告的"切图框消失"）。
                    // toolbar / Button 不需要 Root（它们不调 Root::update），
                    // 但 Input 需要，所以开了 Text 工具 + 点击输入框后任何
                    // blur 路径（按 Enter、点外面）都会触发 panic。
                    // bordered(false): 全屏覆盖窗口不需要 Linux CSD 窗口阴影。
                    // 默认 bordered(true) 会在元素层四周加 12px shadow padding，
                    // 导致元素层坐标和 canvas paint 的窗口坐标系之间产生偏移。
                    cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
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
        no_clipboard: false,
    })
}