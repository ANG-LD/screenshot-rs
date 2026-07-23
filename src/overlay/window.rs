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
    MouseMoveEvent, Pixels, Point, Render, RenderImage, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions, canvas, div, point, prelude::*, px, quad, rgba,
};
use gpui_component::button::Button;
use gpui_component::button::ButtonVariants;
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::popover::Popover;
use gpui_platform::application;
use image::{Frame, ImageBuffer, Rgba};
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

    /// Text 工具：文字锚点（屏幕坐标，物理像素）—— Text 命令的 anchor
    text_input_anchor: BoundsPoint,

    /// Text 工具：输入框完整 rect（屏幕物理像素）
    /// 拖动 / resize 时改这个，渲染时除以 scale_factor 得到 logical px
    text_input_rect: ub::Bounds,

    /// Text 工具：拖动 / resize 模式（拖顶部 bar 移动整框、拖角 resize）
    text_input_drag: Option<TextDragState>,

    /// Tooltip：工具栏 div 当前是否被鼠标悬停（用于 root.on_mouse_down 判断
    /// 点击是否落在工具栏上）。工具栏按钮宽高随图标+中文标签动态变化，
    /// 预估矩形（compute_toolbar_bounds）不準；改用 on_mouse_move/on_mouse_down
    /// 在工具栏根 div 上的真实事件来挂标志。
    toolbar_hovered: bool,

    /// 当前选中的已绘制命令索引（DrawingState.commands 中的实际索引）
    selected_cmd_actual_idx: Option<usize>,

    /// 对选中命令的活跃拖拽操作
    cmd_drag: Option<CmdDragState>,
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
    /// 拖右下角 resize
    ResizeSE,
    /// 拖左下角 resize
    ResizeSW,
    /// 拖右上角 resize
    ResizeNE,
    /// 拖左上角 resize
    ResizeNW,
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
            text_input: None,
            text_input_anchor: BoundsPoint::ZERO,
            text_input_rect: ub::Bounds::new(BoundsPoint::ZERO, BoundsPoint::ZERO),
            text_input_drag: None,
            toolbar_hovered: false,
            selected_cmd_actual_idx: None,
            cmd_drag: None,
        }
    }

    /// 发送结果并关闭窗口
    fn commit(&self, result: OverlayResult, window: &mut Window) {
        tracing::info!(
            "commit: selection={:?} commands_count={}",
            result.selection,
            result.commands.len()
        );
        for (i, c) in result.commands.iter().enumerate() {
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
        let _ = self.tx.send(result);
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
            ToolButton::Mosaic => DrawCommand::Mosaic {
                rect: (dp, dp),
                block_size: 12,
            },
            // Text 走 open_text_input（on_mouse_down 已拦截），
            // 其余非绘图工具忽略。
            ToolButton::Text | ToolButton::ColorPicker | ToolButton::Undo | ToolButton::Redo
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
        tracing::info!("finish_draw: cmd={:?}", cmd);
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
        // 绘制完成后自动选中，方便用户二次编辑
        match &self.drawing.commands.last() {
            Some(DrawCommand::Rectangle { .. })
            | Some(DrawCommand::Arrow { .. })
            | Some(DrawCommand::Mosaic { .. }) => {
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
        use gpui_component::input::{InputEvent, InputState};
        self.text_input_anchor = p;
        // 初始输入框大小（logical pixels），宽 300px 高 120px，auto_grow(3,8) 的 3 行
        self.text_input_rect = ub::Bounds::new(p, BoundsPoint::new(p.x + 300.0, p.y + 120.0));
        tracing::info!("open_text_input: anchor=({:.1}, {:.1})", p.x, p.y);

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("输入文字…（换行按 Enter，完成后点击框外）")
                // 用 auto_grow 而非 multi_line：
                //   - multi_line(true).rows(3) 在 element.rs 里只设 min_height=line_height
                //     （一行高），rows 在 PlainText 模式下没传到 element 层
                //   - auto_grow(min_rows, max_rows) 走 is_auto_grow 分支，
                //     `min_size.height = rows * line_height` —— 真正撑出 3 行
                //   - 输入超过 max_rows 后内部滚动，不再撑高
                .auto_grow(3, 8)
                // 默认 submit_on_enter=false：Enter/Shift+Enter 都换行，
                // 完成靠点输入框外（Blur 自动 finalize）
        });
        // 立即 focus，让键盘事件路由到这里（IME 组合也走 focus handle）
        input.update(cx, |state, cx| {
            state.focus(window, cx);
        });

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
                // 不在 Blur 时 finalize：用户可能正在点工具栏色板/加粗等，
                // Blur 先于 swatch on_mouse_down 触发，finalize 会用到旧颜色。
                // 改为在开始新动作时显式 finalize（on_mouse_down 中 begin_draw /
                // open_text_input 前，以及切工具时）。
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
        }
        self.text_input = None;
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
    /// - Rectangle/Arrow/Freehand/Mosaic → 粗细档位 + 颜色
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
            // 1) 绘图工具按钮组：5 个工具按钮，每个都是 Popover trigger
            //    (active_tool 在 Popover 内根据当前选中状态确定 popover kind)
            .child(render_tool_button_with_popover(
                ToolButton::Rectangle,
                active_tool == Some(ToolButton::Rectangle),
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
            // 3) Cancel / Finish
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
            .bg(gpui::rgba(0x202020EE))
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
        ToolButton::Arrow => IconName::ArrowUp,
        ToolButton::Freehand => IconName::Asterisk,
        ToolButton::Text => IconName::SquareTerminal,
        ToolButton::Mosaic => IconName::LayoutDashboard,
        ToolButton::ColorPicker => IconName::Palette,
        ToolButton::Undo => IconName::Undo2,
        ToolButton::Redo => IconName::Redo2,
        ToolButton::Finish => IconName::Check,
        ToolButton::Cancel => IconName::Close,
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
    let toolbar_y_above = sel.origin.y - TOOLBAR_OFFSET_Y - toolbar_h;
    let toolbar_y = if toolbar_y_below + toolbar_h + TOOLBAR_OFFSET_Y <= screen_h {
        toolbar_y_below
    } else if toolbar_y_above >= TOOLBAR_OFFSET_Y {
        toolbar_y_above
    } else {
        (screen_h - toolbar_h - TOOLBAR_OFFSET_Y).max(TOOLBAR_OFFSET_Y)
    };
    let toolbar_x = sel.origin.x;
    // 主行 9 项（5 绘图 + Undo + Redo + Cancel + Finish）按 32 + 间距 4
    let toolbar_w = TOOLBAR_BTN_SIZE * 9.0 + TOOLBAR_GAP * 8.0 + TOOLBAR_PAD * 2.0;
    (toolbar_x, toolbar_y, toolbar_w, toolbar_h)
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
        .icon(icon_for(btn))
        .label(btn.label())
        .tooltip(btn.label())
        .compact()
        .selected(is_active)
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
        .icon(icon_for(btn))
        .label(btn.label())
        .tooltip(btn.label())
        .compact()
        .disabled(disabled)
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
                            },
                            window,
                        );
                    }
                    ToolButton::Finish => {
                        // 兜底：若 Text 工具还活着没提交，先把它的内容落成命令
                        this.finalize_text_input_if_active(cx);
                        let s = this.selection.current().or(Some(this.screen_bounds));
                        let cmds: Vec<DrawCommand> =
                            this.drawing.visible_commands().cloned().collect();
                        this.commit(OverlayResult { selection: s, commands: cmds }, window);
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
    const HANDLE: f32 = 6.0;
    // 拖动条：顶部 6px 横条
    if p.y >= rect.origin.y && p.y <= rect.origin.y + HANDLE
        && p.x >= rect.origin.x && p.x <= rect.origin.x + rect.size.x
    {
        return Some(TextDragState {
            mode: TextDragMode::Move,
            start_mouse: p,
            start_rect: rect,
        });
    }
    // resize 手柄：四个角 6×6 区域（在拖动条下方）
    let inner_y = rect.origin.y + HANDLE;
    let inner_h = rect.size.y - HANDLE;
    let inner_x = rect.origin.x;
    let inner_w = rect.size.x;
    // NW
    if p.x >= inner_x && p.x <= inner_x + HANDLE
        && p.y >= inner_y && p.y <= inner_y + HANDLE
    {
        return Some(TextDragState { mode: TextDragMode::ResizeNW, start_mouse: p, start_rect: rect });
    }
    // NE
    if p.x >= inner_x + inner_w - HANDLE && p.x <= inner_x + inner_w
        && p.y >= inner_y && p.y <= inner_y + HANDLE
    {
        return Some(TextDragState { mode: TextDragMode::ResizeNE, start_mouse: p, start_rect: rect });
    }
    // SW
    if p.x >= inner_x && p.x <= inner_x + HANDLE
        && p.y >= inner_y + inner_h - HANDLE && p.y <= inner_y + inner_h
    {
        return Some(TextDragState { mode: TextDragMode::ResizeSW, start_mouse: p, start_rect: rect });
    }
    // SE
    if p.x >= inner_x + inner_w - HANDLE && p.x <= inner_x + inner_w
        && p.y >= inner_y + inner_h - HANDLE && p.y <= inner_y + inner_h
    {
        return Some(TextDragState { mode: TextDragMode::ResizeSE, start_mouse: p, start_rect: rect });
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
            let clamped_x = if new_w == MIN_W {
                start.origin.x + (start.size.x - MIN_W)
            } else {
                new_x
            };
            let clamped_y = if new_h == MIN_H {
                start.origin.y + (start.size.y - MIN_H)
            } else {
                new_y
            };
            ub::Bounds {
                origin: BoundsPoint::new(clamped_x, clamped_y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeNE => {
            let new_y = start.origin.y + dy;
            let new_w = (start.size.x + dx).max(MIN_W);
            let new_h = (start.size.y - dy).max(MIN_H);
            let clamped_y = if new_h == MIN_H {
                start.origin.y + (start.size.y - MIN_H)
            } else {
                new_y
            };
            ub::Bounds {
                origin: BoundsPoint::new(start.origin.x, clamped_y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeSW => {
            let new_x = start.origin.x + dx;
            let new_w = (start.size.x - dx).max(MIN_W);
            let new_h = (start.size.y + dy).max(MIN_H);
            let clamped_x = if new_w == MIN_W {
                start.origin.x + (start.size.x - MIN_W)
            } else {
                new_x
            };
            ub::Bounds {
                origin: BoundsPoint::new(clamped_x, start.origin.y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
        TextDragMode::ResizeSE => {
            let new_w = (start.size.x + dx).max(MIN_W);
            let new_h = (start.size.y + dy).max(MIN_H);
            ub::Bounds {
                origin: BoundsPoint::new(start.origin.x, start.origin.y),
                size: BoundsPoint::new(new_w, new_h),
            }
        }
    };
    this.text_input_rect = new_rect.clamp_inside(this.screen_bounds);
}

/// 检测点击是否落在已绘制命令的手柄或主体上
fn hit_test_cmd_drag(cmd: &DrawCommand, p: BoundsPoint) -> Option<CmdDragMode> {
    match cmd {
        DrawCommand::Rectangle { rect, .. } => {
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

/// 应用命令拖拽增量到 DrawingState 中的命令
fn apply_cmd_drag(this: &mut OverlayView, drag: CmdDragState, p: BoundsPoint) {
    use crate::overlay::drawing::Point as DP;
    let dx = p.x - drag.start_mouse.x;
    let dy = p.y - drag.start_mouse.y;
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
            let new_bounds = crate::overlay::selection::apply_resize(bounds, handle, new_handle, this.screen_bounds);
            if let DrawCommand::Rectangle { ref mut rect, .. } = cmd {
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
                .clamp_inside(this.screen_bounds);
            if let DrawCommand::Rectangle { ref mut rect, .. } = cmd {
                rect.0 = DP::new(new_bounds.origin.x, new_bounds.origin.y);
                rect.1 = DP::new(new_bounds.origin.x + new_bounds.size.x, new_bounds.origin.y + new_bounds.size.y);
            }
        }
        CmdDragMode::MoveArrowFrom { start_from, start_to: _ } => {
            let sb = this.screen_bounds;
            let x = (start_from.x + dx).clamp(sb.origin.x, sb.origin.x + sb.size.x);
            let y = (start_from.y + dy).clamp(sb.origin.y, sb.origin.y + sb.size.y);
            if let DrawCommand::Arrow { ref mut from, .. } = cmd {
                *from = DP::new(x, y);
            }
        }
        CmdDragMode::MoveArrowTo { start_to, .. } => {
            let sb = this.screen_bounds;
            let x = (start_to.x + dx).clamp(sb.origin.x, sb.origin.x + sb.size.x);
            let y = (start_to.y + dy).clamp(sb.origin.y, sb.origin.y + sb.size.y);
            if let DrawCommand::Arrow { ref mut to, .. } = cmd {
                *to = DP::new(x, y);
            }
        }
        CmdDragMode::MoveArrow { start_from, start_to } => {
            let new_from = ub::Point::new(start_from.x + dx, start_from.y + dy);
            let new_to = ub::Point::new(start_to.x + dx, start_to.y + dy);
            let limits = this.screen_bounds;
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

/// RGBA → GPUI rgba u32（0xRRGGBBAA）
fn rgba_u32(c: RGBA) -> u32 {
    (u32::from(c.r) << 24)
        | (u32::from(c.g) << 16)
        | (u32::from(c.b) << 8)
        | u32::from(c.a)
}

/// 画一条指定粗细的实线（preview 用，不抗锯齿）
///
/// 沿线段以 1 像素步长采样点，每点画一个 lw × lw 的 quad 叠加 ——
/// 视觉上是真线段，对角线不会变成 bbox 大方块。旧实现 fill 整个 bbox，
/// 对水平/垂直线 OK，对对角线（Arrow 主线、Freehand 折线相邻两点连线）
/// 会画成大方块，用户反馈"画出来是个框"。
fn paint_thick_line(x1: f32, y1: f32, x2: f32, y2: f32, lw: f32, color: RGBA, window: &mut Window) {
    let hsla = Hsla::from(gpui::rgba(rgba_u32(color)));
    let half = (lw / 2.0).max(0.5);
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        // 极短线段：一个 quad 兜底
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(x1 - half), gpui::px(y1 - half)),
                size: Size::new(gpui::px(lw), gpui::px(lw)),
            },
            gpui::px(0.),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
        return;
    }
    let steps = len.ceil() as usize;
    let ux = dx / len;
    let uy = dy / len;
    for i in 0..=steps {
        let t = i as f32;
        let cx = x1 + ux * t;
        let cy = y1 + uy * t;
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(cx - half), gpui::px(cy - half)),
                size: Size::new(gpui::px(lw), gpui::px(lw)),
            },
            gpui::px(0.),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
    }
}

/// 画一条宽度渐变的线段（preview 用，不抗锯齿）
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
    if len < 0.5 {
        let half = (end_lw / 2.0).max(0.5);
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(x1 - half), gpui::px(y1 - half)),
                size: Size::new(gpui::px(end_lw), gpui::px(end_lw)),
            },
            gpui::px(0.),
            hsla,
            gpui::px(0.),
            gpui::transparent_black(),
            Default::default(),
        ));
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let start_half = (start_lw / 2.0).max(0.5);
    let end_half = (end_lw / 2.0).max(0.5);
    let steps = len.ceil() as usize;
    for i in 0..=steps {
        let t = i as f32;
        let frac = (t / len).min(1.0);
        let half = start_half + (end_half - start_half) * frac;
        let lw = half * 2.0;
        let cx = x1 + ux * t;
        let cy = y1 + uy * t;
        window.paint_quad(gpui::quad(
            Bounds {
                origin: gpui::point(gpui::px(cx - half), gpui::px(cy - half)),
                size: Size::new(gpui::px(lw), gpui::px(lw)),
            },
            gpui::px(0.),
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
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1.0 {
                paint_thick_line(from.x, from.y, to.x, to.y, line_width, color, window);
                return;
            }
            let ux = dx / len;
            let uy = dy / len;
            let head_len = (line_width * 7.0).max(14.0);
            let head_w = (line_width * 2.0).max(4.0);
            let bx = to.x - ux * head_len;
            let by = to.y - uy * head_len;
            // 主线：从起点窄到箭头底部宽
            let start_lw = (line_width * 0.3).max(1.0);
            paint_tapered_line(from.x, from.y, bx, by, start_lw, line_width, color, window);
            // 箭头 V 字
            let px = -uy;
            let py = ux;
            let p1x = bx + px * head_w;
            let p1y = by + py * head_w;
            let p2x = bx - px * head_w;
            let p2y = by - py * head_w;
            paint_thick_line(to.x, to.y, p1x, p1y, line_width, color, window);
            paint_thick_line(to.x, to.y, p2x, p2y, line_width, color, window);
        }
        DrawCommand::Freehand { ref points, color, line_width } => {
            for w in points.windows(2) {
                paint_thick_line(w[0].x, w[0].y, w[1].x, w[1].y, line_width, color, window);
            }
        }
        DrawCommand::Text { anchor, ref content, font_size, color, weight, .. } => {
            // Phase 3 简化：画一个文字占位框（按字符数 × 行数估算尺寸）
            // `weight` 仅作元数据保留 — GPUI paint 阶段不支持 weight，
            // 真正按 weight 栅格化在 CPU 阶段（commands.rs::rasterize_text）。
            // 多行文本（content 内含 \n）：宽度按最长行计，高度按行数 × font_size
            let _ = weight;
            let char_w = font_size * 0.6;
            let line_h = font_size * 1.25;
            let lines: Vec<&str> = content.split('\n').collect();
            let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
            let w = char_w * max_chars.max(1.0);
            let h = line_h * lines.len().max(1) as f32;
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
        let sel_visible_idx = self.selected_cmd_actual_idx.and_then(|idx| {
            if self.drawing.is_visible(idx) {
                // LIFO 模型：可见命令是 commands[0..history_index]，idx 即位置
                Some(idx)
            } else {
                None
            }
        });

        let paint_canvas = canvas(
            move |_, _, _| (in_progress, visible_cmds, sel_visible_idx),
            move |_, (in_progress, visible_cmds, sel_visible_idx), window, _| {
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

                    // 2.6) 在选中的已绘制命令上渲染拖拽手柄
                    if let Some(vidx) = sel_visible_idx {
                        if let Some(cmd) = visible_cmds.get(vidx) {
                            let handle_fill = Hsla::from(rgba(0xFFFFFFFF));
                            let handle_border = Hsla::from(rgba(0x0066CCFF));
                            let half = px(HANDLE_VISUAL_SIZE / 2.0);
                            let edge = px(HANDLE_VISUAL_SIZE);
                            match cmd {
                                DrawCommand::Rectangle { rect, .. } => {
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
        // anchor 是屏幕物理像素，div 用 logical px，所以除以 scale_factor。
        // 多行模式（multi_line + rows=3）：Shift+Enter 插入换行；Enter 提交。
        // 不用 .small() —— single-line 模式下高度被钉死在 h_6=24px，
        // multi_line 自动调 h_auto()，垂直 padding 由 input_py 控制；视觉比 small 更宽松。
        if let Some(ref input) = self.text_input {
            let rect = self.text_input_rect;
            // text_input_rect 存储 logical pixels；分离出 f32 做坐标计算，px() 做 CSS
            let (lx, ly, lw, lh) = (rect.origin.x, rect.origin.y, rect.size.x, rect.size.y);
            let handle_size = 6.0_f32;
            root = root.child(
                div()
                    .absolute()
                    .top(px(ly))
                    .left(px(lx))
                    .w(px(lw))
                    .h(px(lh))
                    .flex()
                    .flex_col()
                    // 顶部拖动 bar（6px 高，灰色）；拖动检测在 root mouse_down 中统一处理
                    .child(
                        div()
                            .id("text-drag-bar")
                            .w_full()
                            .h(px(6.0))
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
                                    .text_color(gpui::rgba(rgba_u32(self.toolbar.current_color))),
                            )
                            .child(make_resize_handle(
                                "text-resize-nw",
                                0.0, 0.0,
                                TextDragMode::ResizeNW,
                            ))
                            .child(make_resize_handle(
                                "text-resize-ne",
                                lw - handle_size, 0.0,
                                TextDragMode::ResizeNE,
                            ))
                            .child(make_resize_handle(
                                "text-resize-sw",
                                0.0, lh - handle_size - 6.0,
                                TextDragMode::ResizeSW,
                            ))
                            .child(make_resize_handle(
                                "text-resize-se",
                                lw - handle_size, lh - handle_size - 6.0,
                                TextDragMode::ResizeSE,
                            )),
                    ),
            );
        }

        root
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    let p = to_bounds_point(ev.position);
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

                    // 文字输入框存在时，优先检测是否点击了拖动条或 resize 手柄
                    if this.text_input.is_some() {
                        if let Some(drag) = hit_test_text_drag(this.text_input_rect, p) {
                            this.text_input_drag = Some(drag);
                            return;
                        }
                        // 点在输入框内部（非拖拽条/手柄）→ 让 Input 组件处理聚焦和光标
                        if this.text_input_rect.contains(p) {
                            return;
                        }
                        // 点在输入框外 → 先提交活跃 Text 输入，避免文字丢失
                        this.finalize_text_input_if_active(cx);
                    }

                    // Editing 模式下：优先检测已绘制命令的手柄/主体命中（顶部命令优先）
                    if this.mode == OverlayMode::Editing {
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
                            if this.toolbar.active_tool == Some(ToolButton::Text)
                                && sel.contains(p)
                                && this.text_input.is_none()
                            {
                                this.open_text_input(p, window, cx);
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
                    // 没有选区或点击在选区外 → 开始新选区前先提交活跃 Text 输入
                    this.finalize_text_input_if_active(cx);
                    this.selection.mouse_down(p);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let p = to_bounds_point(ev.position);
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
                        this.commit(OverlayResult { selection: sel, commands: cmds }, window);
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
                    this.commit(OverlayResult { selection: None, commands: vec![] }, window);
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
                    this.commit(OverlayResult { selection: sel, commands: cmds }, window);
                } else if ev.keystroke.key == "z" && ev.keystroke.modifiers.control {
                    // Ctrl+Z 撤销 / Ctrl+Shift+Z 重做
                    if ev.keystroke.modifiers.shift {
                        this.drawing.redo();
                    } else {
                        this.drawing.undo();
                    }
                    this.check_selected_visible();
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
        // 注册 gpui-component-assets 提供默认 Lucide 图标 svg 资源。
        // 不调用时 IconName::XXX 渲染会找不到 svg、按钮看不出图标。
        application()
            .with_assets(gpui_component_assets::Assets)
            .run(move |cx: &mut App| {
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
                    // 必须用 gpui_component::Root 包一层：
                    // gpui-component 的 Input 在 blur 时会调
                    // `Root::update(window, cx, ...)` 去清 `focused_input`，
                    // 找不到 Root 会 panic "BUG: window first layer should be
                    // a gpui_component::Root." → 整个 GPUI 线程 panic →
                    // 覆盖窗口闪退（用户报告的"切图框消失"）。
                    // toolbar / Button 不需要 Root（它们不调 Root::update），
                    // 但 Input 需要，所以开了 Text 工具 + 点击输入框后任何
                    // blur 路径（按 Enter、点外面）都会触发 panic。
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
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