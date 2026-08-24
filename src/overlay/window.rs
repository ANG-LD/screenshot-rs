//! 全屏覆盖窗口：把捕获的帧作为背景 + 半透明 dim + 选区矩形边框。
//!
//! 用户拖拽选区，松开鼠标后选区 bounds 通过 mpsc 发回主线程；
//! 主线程据此裁剪原帧并写入剪贴板。Esc → 取消（发 selection=None 并停靠）。
//!
//! GPUI 应用为进程级常驻单例（`OverlayService`）：专用线程跑
//! `QuitMode::Explicit` 的 `application().run()`，覆盖窗口与 Pin 窗口都在
//! 同一个应用内创建/销毁，截图完成不退出进程。主线程在 channel 上阻塞等结果。
//!
//! 覆盖窗口**常驻复用**：启动时创建一次（同步编译整套 wgpu shader pipeline，
//! 约 0.5s），之后每次截图不再新建窗口——会话结束只把窗口 unmap 停靠
//! （不可见、不挡输入、自动释放焦点），下次截图 resize + map 唤醒 + 换帧，
//! 免去每窗重编译 pipeline 的 ~570ms（见 `park_overlay_window` /
//! `reuse_overlay_window`）。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};

use crate::error::{AppError, AppResult};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, QuitMode, Render, RenderImage, Size,
    WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    TitlebarOptions, WindowOptions, canvas, div, point, prelude::*, px, quad, rgba,
};
use gpui_component::button::Button;
use gpui_component::button::{ButtonVariant, ButtonVariants};
use gpui_component::Disableable;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::Sizable;
use gpui_component::Icon;
use gpui_component::popover::Popover;
use gpui_component::scroll::ScrollableElement;
use gpui_platform::application;
use image::{Frame, ImageBuffer, Rgba};
#[cfg(target_os = "linux")]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use smallvec::SmallVec;

use crate::capture::CapturedFrame;
use crate::overlay::drawing::{DrawCommand, DrawingState, FontWeight, RGBA};
use crate::overlay::palette;
use crate::overlay::selection::{DragState, SelectionState};
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

/// 已提交形状（矩形/椭圆/箭头/自由线）的离屏光栅化缓存。
///
/// 形状光栅化（解析式 AA）是拖拽绘制热路径里最贵的部分：原实现每帧把所有
/// 已提交形状重新光栅化一遍。这里按 `DrawingState.revision` 缓存：仅提交/
/// 撤销/拖动等命令变更时重建，拖拽绘制期间每帧复用，只增量重绘
/// `in_progress` 那一笔。
struct ShapeLayerCache {
    /// 快照时的 `DrawingState.revision`（失效判据）
    revision: u64,
    /// 快照时的 scale_factor（缩放改变则失效）
    scale_factor: f32,
    /// 已转 BGRA 的 RenderImage，可直接 paint_image
    image: Arc<RenderImage>,
    /// 联合包围盒（逻辑像素，含 AA 外扩），即 paint_image 的目标 Bounds
    bounds: ub::Bounds,
}


/// GPUI 视图：覆盖窗口内容
pub struct OverlayView {
    /// 捕获帧的 GPUI 渲染图（已转 BGRA）
    frame_image: Arc<RenderImage>,
    /// 屏幕边界（逻辑像素，与 GPUI 坐标系一致）
    screen_bounds: ub::Bounds,
    /// 覆盖窗口在屏幕上的客户端区原点（逻辑像素）。
    ///
    /// 注意：`window.bounds().origin` 返回的是窗口**外框**位置（含 DWM 隐形边框），
    /// 与客户端区原点相差顶部边框偏移；选区/画布坐标是客户端区坐标，所以
    /// 计算屏幕位置必须用这里存的客户端原点，而不是 `window.bounds().origin`。
    client_origin: ub::Point,
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
    /// Arc 共享：拖动绘制时每帧克隆为 O(1) 指针复制，避免深拷贝增长的 Freehand/Mosaic 数据
    in_progress: Option<std::sync::Arc<DrawCommand>>,

    /// 已提交形状的离屏光栅化缓存（见 `ShapeLayerCache`）
    shape_layer_cache: Option<ShapeLayerCache>,



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

    /// 活动输入文字测量缓存：`(value, font_size, weight, adv_px, th_px)`。
    /// render 每帧重测会做两次 cosmic-text shaping，按值缓存避免鼠标移动等
    /// 无关 notify 触发重复排版。
    text_measure: Option<(String, f32, FontWeight, f32, f32)>,

    /// 原始捕获帧像素（RGBA），用于 OCR 等需要像素数据的操作
    frame_pixels: Vec<u8>,
    /// 捕获帧宽度（物理像素）
    frame_width: u32,
    /// 捕获帧高度（物理像素）
    frame_height: u32,

    /// 全屏原始捕获帧（仅当窗口被系统压缩、显示帧被裁剪时才保留）。
    /// 会话结束时由 commit 原样归还给主线程，避免主线程整帧 clone。
    original_frame: Option<CapturedFrame>,

    /// OCR 工具：选中的识别区域（None 表示尚未框选）
    ocr_rect: Option<ub::Bounds>,
    /// OCR 工具：识别结果文字
    /// OCR 工具：是否正在识别中
    /// OCR 工具：框选拖拽起点（None 表示未在拖拽）
    ocr_drag_start: Option<BoundsPoint>,

    /// Tooltip：工具栏 div 当前是否被鼠标悬停（用于 root.on_mouse_down 判断
    /// 点击是否落在工具栏上）。工具栏按钮宽高随图标+中文标签动态变化，
    /// 预估矩形（compute_toolbar_bounds）不準；改用 on_mouse_move/on_mouse_down
    /// 在工具栏根 div 上的真实事件来挂标志。
    toolbar_hovered: bool,
    /// OCR 结果面板是否被鼠标按下（用于 root.on_mouse_down 判断，避免 prevent_default 阻断按钮 click 事件）

    /// 当前选中的已绘制命令索引（DrawingState.commands 中的实际索引）
    selected_cmd_actual_idx: Option<usize>,

    /// 对选中命令的活跃拖拽操作
    cmd_drag: Option<CmdDragState>,

    /// 鼠标当前是否悬停在某个可选中形状的描边线条上（用于 hover 小手光标）
    hover_shape: bool,

    /// HiDPI 缩放因子（物理像素 / 逻辑像素）。
    ///
    /// screen_bounds 和所有鼠标交互使用逻辑像素（与 GPUI 坐标系一致），
    /// commit 时乘以 scale_factor 转回物理像素供 app.rs 裁剪/栅格化。
    scale_factor: f32,

    /// dim 遮罩不透明度（0=透明, 1=最大 dim）——直接到 1.0，无淡入动画
    dim_opacity: f32,
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
    /// 拖边框移动整框
    Move,
    /// 四角 resize
    ResizeNW,
    ResizeNE,
    ResizeSW,
    ResizeSE,
    /// 四边中点 resize（与矩形选中框一致：8 个手柄）
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

/// Pin 固定所需全部数据：裁剪+应用命令后的帧（物理像素）、屏幕逻辑坐标、物理→逻辑缩放。
#[derive(Debug, Clone)]
pub struct PinPayload {
    /// 已按选区裁剪并应用标注命令的帧（物理像素 RGBA）
    pub frame: CapturedFrame,
    /// pin 窗口内容左上角的屏幕逻辑 x
    pub origin_x: f32,
    /// pin 窗口内容左上角的屏幕逻辑 y
    pub origin_y: f32,
    /// 物理像素 → 逻辑像素 缩放因子
    pub sx: f32,
    pub sy: f32,
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
    /// 非 None 表示用户点了「固定」：主线程应把 payload 交给 `OverlayService::open_pin`
    pub pin: Option<PinPayload>,
    /// 非 None 表示用户点了「滚动截屏」：`selection` 应为 None，主线程据此
    /// 运行滚动截屏（region 是物理像素，主屏相对坐标）
    pub scroll_region_px: Option<ub::Bounds>,
    /// true 表示用户点了「手动滚动」：滚动由用户手动进行，应用只检测拼接
    pub scroll_manual: bool,
    /// 覆盖层归还的原始捕获帧（移动，非复制）：主线程用它做最终裁剪，
    /// 避免主线程先整帧 clone 再 clip_region。取消/滚动截屏等路径也会归还，
    /// 调用方按需使用或直接丢弃。
    pub frame: Option<CapturedFrame>,
}

impl OverlayView {
    fn new(
        frame: CapturedFrame,
        original_frame: Option<CapturedFrame>,
        screen_bounds: ub::Bounds,
        client_origin: ub::Point,
        scale_factor: f32,
        tx: Sender<OverlayResult>,
        cx: &mut Context<Self>,
    ) -> Self {
        let this = Self {
            frame_image: build_render_image_from_pixels(
                frame.width,
                frame.height,
                frame.pixels.clone(),
            ),
            screen_bounds,
            client_origin,
            selection: SelectionState::new(screen_bounds),
            tx,
            focus_handle: cx.focus_handle(),
            mode: OverlayMode::Selecting,
            toolbar: ToolbarState::default(),
            drawing: DrawingState::new(),
            in_progress: None,
            shape_layer_cache: None,
            text_input: None,
            text_input_anchor: BoundsPoint::ZERO,
            text_input_rect: ub::Bounds::new(BoundsPoint::ZERO, BoundsPoint::ZERO),
            text_input_drag: None,
            text_input_finalized: false,
            text_input_cmd_idx: None,
            text_measure: None,
            frame_pixels: frame.pixels,
            frame_width: frame.width,
            frame_height: frame.height,
            original_frame,
            ocr_rect: None,
            ocr_drag_start: None,
            toolbar_hovered: false,
            selected_cmd_actual_idx: None,
            cmd_drag: None,
            hover_shape: false,
            scale_factor,
            dim_opacity: 1.0,
        };

        this
    }

    /// 复用常驻窗口开始一次新会话：换帧 + 重置全部交互状态。
    ///
    /// 窗口复用路径（`reuse_overlay_window`）不销毁窗口，因此这里必须把
    /// 上次会话遗留的一切状态清干净，等效于重新 new 一个 OverlayView。
    /// `focus_handle` 保持不动（窗口/焦点句柄跨会话复用）。
    #[allow(clippy::too_many_arguments)]
    fn start_session(
        &mut self,
        frame: CapturedFrame,
        original_frame: Option<CapturedFrame>,
        screen_bounds: ub::Bounds,
        client_origin: ub::Point,
        scale_factor: f32,
        tx: Sender<OverlayResult>,
        cx: &mut Context<Self>,
    ) {
        // 换帧：clone 一份 RGBA 原地转 BGRA 给 RenderImage（gpui 数据约定
        // BGRA），原 RGBA 移动给 frame_pixels（OCR/提交用）。
        // 注意：不要改成"拷贝+转换合并"的一次遍历——基准实测 debug 构建下
        // 合并版 25.9ms/帧 vs clone+原地转换 12.4ms/帧（debug 下 Vec::clone
        // 走优化的 memcpy，显式 u32 循环无优化反而慢 2 倍）。
        self.frame_image =
            build_render_image_from_pixels(frame.width, frame.height, frame.pixels.clone());
        self.screen_bounds = screen_bounds;
        self.client_origin = client_origin;
        self.selection = SelectionState::new(screen_bounds);
        self.tx = tx;
        self.mode = OverlayMode::Selecting;
        self.toolbar = ToolbarState::default();
        self.drawing = DrawingState::new();
        self.in_progress = None;
        self.shape_layer_cache = None;
        self.text_input = None;
        self.text_input_anchor = BoundsPoint::ZERO;
        self.text_input_rect = ub::Bounds::new(BoundsPoint::ZERO, BoundsPoint::ZERO);
        self.text_input_drag = None;
        self.text_input_finalized = false;
        self.text_input_cmd_idx = None;
        self.text_measure = None;
        self.frame_pixels = frame.pixels;
        self.frame_width = frame.width;
        self.frame_height = frame.height;
        self.original_frame = original_frame;
        self.ocr_rect = None;
        self.ocr_drag_start = None;
        self.toolbar_hovered = false;
        self.selected_cmd_actual_idx = None;
        self.cmd_drag = None;
        self.hover_shape = false;
        self.scale_factor = scale_factor;
        tracing::info!("[overlay] start_session scale_factor={scale_factor} frame={}x{}", self.frame_width, self.frame_height);
        self.dim_opacity = 1.0;
        cx.notify();
    }

    /// 发送结果并停靠窗口（复用：窗口不销毁，缩到不可见/unmap 供下次使用）
    ///
    /// 内部将 selection 和 commands 的坐标从逻辑像素转为物理像素，
    /// 以匹配 `CapturedFrame` 的物理像素坐标系（app.rs 的 clip_region 和
    /// commands.rs 的栅格化都用物理像素）。
    fn commit(&mut self, result: OverlayResult, window: &mut Window) {
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
            result.commands.iter().map(|c| scale_draw_command(c, sx, sy)).collect();

        tracing::info!(
            "commit: selection={:?} commands_count={}",
            selection,
            commands.len()
        );
        for (i, c) in commands.iter().enumerate() {
            match c {
                DrawCommand::Text { anchor, content, font_size, color, weight, max_width, .. } => {
                    tracing::info!(
                        "cmd[{}] Text anchor=({},{}) size={} weight={:?} max_w={:?} color={:?} content={:?}",
                        i, anchor.x, anchor.y, font_size, weight, max_width, color, content
                    );
                }
                _ => tracing::info!("cmd[{}] {:?}", i, c),
            }
        }
        let no_clipboard = result.no_clipboard;
        let pin = result.pin;
        // scroll_region_px 在 Scroll 按钮处已换算成物理像素，这里原样透传，不参与缩放
        let scroll_region_px = result.scroll_region_px;
        let scroll_manual = result.scroll_manual;
        // 归还原始帧（移动，零拷贝）：全屏时 frame_pixels 就是原帧；
        // 窗口被系统压缩、显示帧被裁剪时返回单独保存的 original_frame。
        let frame = match self.original_frame.take() {
            Some(f) => f,
            None => CapturedFrame {
                width: self.frame_width,
                height: self.frame_height,
                pixels: std::mem::take(&mut self.frame_pixels),
            },
        };
        let _ = self.tx.send(OverlayResult {
            selection,
            commands,
            no_clipboard,
            pin,
            scroll_region_px,
            scroll_manual,
            frame: Some(frame),
        });
        // 停靠而非关闭：窗口与 WgpuRenderer（含已编译的 shader pipeline）保持
        // 存活，下次截图直接复用，免去每窗重编译 pipeline 的 ~570ms。
        park_overlay_window(window);
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
        self.in_progress = Some(std::sync::Arc::new(match tool {
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
            | ToolButton::Redo | ToolButton::Bold | ToolButton::Scroll | ToolButton::ScrollManual
            | ToolButton::Finish | ToolButton::Cancel | ToolButton::Pin => return,
        }));
    }

    /// 推进 in_progress 的当前点（鼠标拖动时调用）
    fn update_in_progress(&mut self, p: BoundsPoint) {
        let Some(cmd) = self.in_progress.as_mut() else { return };
        // make_mut：渲染闭包每帧替换后计数=1，原地修改零拷贝
        let cmd = std::sync::Arc::make_mut(cmd);
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
                // 点简化：与上一点距离 < 1.5px 的过近点不记录——
                // 上万点（长线）时点数大幅减少，光栅化/上传加速；
                // 渲染与提交共用同一份简化点 → 一致性不变（1.5px 精度，细线可接受）
                const MIN_D: f32 = 1.5;
                if let Some(last) = points.last() {
                    let dx = dp.x - last.x;
                    let dy = dp.y - last.y;
                    if dx * dx + dy * dy < MIN_D * MIN_D {
                        return;
                    }
                }
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
        // 解 Arc：唯一持有者直接取出，否则克隆内容
        let cmd = std::sync::Arc::try_unwrap(cmd).unwrap_or_else(|a| (*a).clone());
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
        match self.drawing.commands.last().map(|a| &**a) {
            Some(DrawCommand::Rectangle { .. })
            | Some(DrawCommand::Ellipse { .. })
            | Some(DrawCommand::Arrow { .. }) => {
                self.selected_cmd_actual_idx = Some(self.drawing.commands.len() - 1);
            }
            _ => {}
        }
    }

    /// 把修改应用到当前选中的已绘制命令（改宽/改色），并标记重渲染。
    fn apply_style_to_selected(&mut self, update: impl FnOnce(&mut DrawCommand)) {
        if let Some(idx) = self.selected_cmd_actual_idx {
            if let Some(cmd) = self.drawing.get_visible_mut(idx) {
                update(cmd);
                self.drawing.revision += 1;
            }
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
        self.text_measure = None;
        self.text_input_anchor = p;
        // 初始输入框大小（logical pixels），auto_grow(3,8) 会根据内容自动扩展。
        // 新输入从紧凑大小起步，重新编辑时沿用旧宽度。
        // 高度随字号缩放，保证大字号能完整显示。
        // 裁剪到选区范围内，防止靠近边缘时文字框/手柄超出截图区域。
        let limits = self.selection.current().unwrap_or(self.screen_bounds);
        // 初始框刻意做小：64×1行，输字后 auto_grow 按内容扩宽/扩高。
        let w = max_w_override.unwrap_or(64.0);
        // 空框行高由窗口 line_height 决定；输字后 auto_grow 测量里会再按
        // max(窗口行高, 1.4×字号) 补足，避免大字号溢出。
        let line_h = window.line_height().as_f32();
        let init_h = (line_h + 14.0).max(40.0);
        self.text_input_rect = ub::Bounds::new(p, BoundsPoint::new(p.x + w, p.y + init_h))
            .clamp_inside(limits);
        tracing::debug!("open_text_input: anchor=({:.1}, {:.1}) initial={}", p.x, p.y, initial.is_some());

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("")
                .auto_grow(1, 8)
                .soft_wrap(false)
        });
        // 预填旧内容（重新编辑场景）：直接移交所有权，避免 clone。
        // 先重新借用 window，使 move 闭包只捕获该借用而非整个 &mut Window，
        // 闭包用完后原借用自动失效，后续代码可继续使用 window。
        if let Some(text) = initial {
            let window = &mut *window;
            input.update(cx, move |state, cx| {
                state.set_value(text, window, cx);
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
        cx.subscribe_in(&input, window, |this, state, event, _window, cx| match event {
            // submit_on_enter=false → Enter/Shift+Enter 都用于换行（InputState
            // 内部已插入 \n）。PressEnter 不触发 finalize，让用户继续编辑；
            // 完成输入靠 Blur：点输入框外 / 点 Finish / 点其他工具都会 Blur。
            InputEvent::PressEnter { .. } => {
                cx.notify();
            }
            InputEvent::Blur => {
                tracing::debug!("text_input Blur: popup={}", this.toolbar.popup.is_some());
                // 若 popover 正打开，失焦是因为用户点击了样式选项
                // （Bold/字号/颜色），此时不应提交文字——保留输入框
                // 让用户继续编辑。样式按钮 handler 会更新 toolbar 属性，
                // Input 组件下次 render 时自然应用新样式。
                if this.toolbar.popup.is_some() {
                    return;
                }
                // 兜底：其他失焦场景（点输入框外、点工具栏、切工具）
                // 把当前文字落成命令，避免丢失。
                this.finalize_text_input_if_active(cx);
            }
            InputEvent::Change => {
                let value = state.read(cx).value().to_string();
                let r = this.text_input_rect;
                tracing::debug!(
                    "text_input Change: value={:?} box_origin=({:.1},{:.1}) box_size=({:.1},{:.1})",
                    value, r.origin.x, r.origin.y, r.size.x, r.size.y
                );
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
                "finalize text: value={:?} anchor=({:.1},{:.1}) rect_size=({:.1},{:.1}) fs={:.1} sf={:.1}",
                value.to_string(), anchor.x, anchor.y,
                self.text_input_rect.size.x, self.text_input_rect.size.y,
                self.toolbar.current_size, self.scale_factor
            );
            // 测量编辑态首行行盒顶相对 box 顶的偏移（校准 paint_command 的 origin_fy）：
            // range_to_bounds 返回 editor 元素内首行行盒的窗口坐标，box 顶 = anchor.y。
            if let Some(lh) = state.read(cx).range_to_bounds(&(0..1)) {
                tracing::info!(
                    "finalize measure: line1_top={:.2} box_top={:.2} offset={:+.2} lh={:.2}",
                    lh.origin.y, anchor.y, lh.origin.y - anchor.y.into(), lh.size.height
                );
            } else {
                tracing::info!("finalize measure: range_to_bounds None (not laid out)");
            }
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
        self.text_measure = None;
        // 文字提交后退出 Text 工具：否则 active_tool 一直是 Text，后续点击
        // 矩形/椭圆/箭头会被 Text 分支跳过命令命中检测，没法再选中出拖动手柄。
        self.toolbar.active_tool = None;
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
                cx,
            ))
            .child(render_simple_button(
                ToolButton::Scroll,
                false,
                sel.size.x < 20.0 || sel.size.y < 20.0,
                weak.clone(),
                cx,
            ))
            .child(render_simple_button(
                ToolButton::ScrollManual,
                false,
                sel.size.x < 20.0 || sel.size.y < 20.0,
                weak.clone(),
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
                cx,
            ))
            .child(render_simple_button(
                ToolButton::Redo,
                false,
                !can_redo,
                weak.clone(),
                cx,
            ))
            // 3) Pin / Cancel / Finish
            .child(render_simple_button(
                ToolButton::Pin,
                false,
                false,
                weak.clone(),
                cx,
            ))
            .child(render_simple_button(
                ToolButton::Cancel,
                false,
                false,
                weak.clone(),
                cx,
            ))
            .child(render_simple_button(
                ToolButton::Finish,
                false,
                false,
                weak,
                cx,
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
            .bg(gpui::rgba(0x1E1E1EF5))
            .rounded_lg()
            .border_1()
            .border_color(gpui::rgba(0xFFFFFF26))
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
        ToolButton::Scroll => IconName::ChevronDown,
        ToolButton::ScrollManual => IconName::ChevronsUpDown,
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
    let toolbar_w = TOOLBAR_BTN_SIZE * 14.0 + TOOLBAR_GAP * 13.0 + TOOLBAR_PAD * 2.0;
    let toolbar_x = sel.origin.x.min(screen_bounds.origin.x + screen_bounds.size.x - toolbar_w - TOOLBAR_OFFSET_Y);
    (toolbar_x, toolbar_y, toolbar_w, toolbar_h)
}

/// 工具栏按钮中的图标+文字，紧凑间距
///
/// gpui-component 的 Custom 按钮变体忽略 `foreground` 字段（渲染时 text_color
/// 取的是 `colors.color`，即背景色），文字会继承到近透明的白色而看不清。
/// 这里显式把图标/文字设为白色，禁用按钮用半透明白区分状态。
fn icon_label(btn: ToolButton, disabled: bool) -> impl IntoElement {
    let color = if disabled {
        gpui::rgba(0xFFFFFF66)
    } else {
        gpui::rgba(0xFFFFFFFF)
    };
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .text_color(color)
        .child(Icon::new(icon_for(btn)).size(px(12.0)).text_color(color))
        .child(div().text_xs().text_color(color).child(btn.label()))
}

/// 工具栏按钮配色
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolbarBtnStyle {
    /// 普通按钮：深色工具栏上的浅色半透明底 + 亮文字
    Neutral,
    /// 激活/主操作：蓝色强调
    Accent,
    /// 完成：绿色
    Success,
}

/// 构造工具栏按钮的定制样式（背景/文字/hover/active 配色）
fn toolbar_btn_style(cx: &App, kind: ToolbarBtnStyle) -> gpui_component::button::ButtonCustomVariant {
    let base = gpui_component::button::ButtonCustomVariant::new(cx);
    match kind {
        ToolbarBtnStyle::Neutral => base
            .color(gpui::rgba(0xFFFFFF0F).into())
            .foreground(gpui::rgba(0xE4E4EA).into())
            .hover(gpui::rgba(0xFFFFFF20).into())
            .active(gpui::rgba(0xFFFFFF2E).into()),
        ToolbarBtnStyle::Accent => base
            .color(gpui::rgba(0x3B82F6).into())
            .foreground(gpui::rgba(0xFFFFFFFF).into())
            .hover(gpui::rgba(0x4C8FFA).into())
            .active(gpui::rgba(0x3374E8).into()),
        ToolbarBtnStyle::Success => base
            .color(gpui::rgba(0x22C55E).into())
            .foreground(gpui::rgba(0xFFFFFFFF).into())
            .hover(gpui::rgba(0x2ED36B).into())
            .active(gpui::rgba(0x1CAE50).into()),
    }
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
    let trigger_style = toolbar_btn_style(
        cx,
        if is_active { ToolbarBtnStyle::Accent } else { ToolbarBtnStyle::Neutral },
    );
    let trigger = Button::new(("tool", btn as usize))
        .tooltip(btn.label())
        .with_size(gpui_component::Size::Small)
        .compact()
        .custom(trigger_style)
        .selected(is_active)
        .child(icon_label(btn, false))
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
    cx: &mut Context<OverlayView>,
) -> Button {
    // Finish 用绿色"完成"；激活工具用蓝色；其余中性配色
    let style = toolbar_btn_style(
        cx,
        if btn == ToolButton::Finish {
            ToolbarBtnStyle::Success
        } else if active {
            ToolbarBtnStyle::Accent
        } else {
            ToolbarBtnStyle::Neutral
        },
    );
    let weak_for_click = weak.clone();
    let b = Button::new(("action", btn as usize))
        .tooltip(btn.label())
        .with_size(gpui_component::Size::Small)
        .compact()
        .custom(style)
        .disabled(disabled)
        .child(icon_label(btn, disabled))
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
                                pin: None,
                                scroll_region_px: None,
                                scroll_manual: false,
                                frame: None,
                            },
                            window,
                        );
                    }
                    ToolButton::Ocr => {
                        this.finalize_text_input_if_active(cx);
                        if this.toolbar.active_tool == Some(ToolButton::Ocr) {
                            this.toolbar.active_tool = None;
                            this.ocr_rect = None;
                        } else {
                            this.toolbar.active_tool = Some(ToolButton::Ocr);
                            this.ocr_rect = None;
                        }
                        cx.notify();
                    }
                    ToolButton::Pin => {
                        this.finalize_text_input_if_active(cx);
                        let s = this.selection.current().or(Some(this.screen_bounds));
                        let cmds: Vec<DrawCommand> =
                            this.drawing.visible_commands().map(|a| &**a).cloned().collect();

                        let wb = window.bounds();
                        let sx = this.frame_width as f32 / f32::from(wb.size.width).max(1.0);
                        let sy = this.frame_height as f32 / f32::from(wb.size.height).max(1.0);
                        tracing::info!(
                            "[Pin] overlay window: origin=({:.0},{:.0}) size=({:.0},{:.0}) frame={}x{} sx={:.2} sy={:.2}",
                            wb.origin.x, wb.origin.y, wb.size.width, wb.size.height,
                            this.frame_width, this.frame_height, sx, sy
                        );

                        let scaled_cmds: Vec<DrawCommand> =
                            cmds.iter().map(|c| scale_draw_command(c, sx, sy)).collect();

                        // 固定：把裁剪+标注后的帧放进 OverlayResult，由主线程交给
                        // OverlayService::open_pin 在同一个 GPUI 应用里开 pin 窗口。
                        let mut pin: Option<PinPayload> = None;

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

                            let fw = this.frame_width;
                            let fh = this.frame_height;

                            // 直接从帧像素切片裁剪，避免先整帧 clone 再裁
                            if let Ok(mut clipped) = CapturedFrame::clip_pixels(
                                fw,
                                fh,
                                &this.frame_pixels,
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
                                // 屏幕位置 = 覆盖窗口客户端原点 + 画布(客户端)坐标。
                                // 不能用 wb.origin（窗口外框位置，含 DWM 隐形边框）：
                                // 那会把固定窗口整体上移一个顶部边框偏移（几 px）。
                                let pin_x = this.client_origin.x + sel.origin.x;
                                let pin_y = this.client_origin.y + sel.origin.y;
                                tracing::info!(
                                    "[Pin] target position: ({:.0},{:.0}) clipped_frame={}x{}",
                                    pin_x, pin_y, clipped.width, clipped.height
                                );
                                pin = Some(PinPayload {
                                    frame: clipped,
                                    origin_x: pin_x,
                                    origin_y: pin_y,
                                    sx,
                                    sy,
                                });
                            }
                        }

                        this.commit(OverlayResult { selection: s, commands: cmds, no_clipboard: true, pin, scroll_region_px: None, scroll_manual: false, frame: None }, window);
                    }
                    ToolButton::Scroll | ToolButton::ScrollManual => {
                        // 滚动截屏：把选区（物理像素）交给主线程去滚动拼接。
                        // ScrollManual 由用户手动滚动，应用只负责检测拼接。
                        this.finalize_text_input_if_active(cx);
                        let manual = btn == ToolButton::ScrollManual;
                        let Some(s) = this.selection.current() else {
                            return;
                        };
                        let wb = window.bounds();
                        let sx = this.frame_width as f32 / f32::from(wb.size.width).max(1.0);
                        let sy = this.frame_height as f32 / f32::from(wb.size.height).max(1.0);
                        let region = ub::Bounds {
                            origin: ub::Point::new(s.origin.x * sx, s.origin.y * sy),
                            size: ub::Point::new(s.size.x * sx, s.size.y * sy),
                        };
                        tracing::info!(
                            "[Scroll] selection physical: origin=({:.0},{:.0}) size=({:.0},{:.0}) manual={manual}",
                            region.origin.x, region.origin.y, region.size.x, region.size.y
                        );
                        this.commit(
                            OverlayResult {
                                selection: None,
                                commands: vec![],
                                no_clipboard: false,
                                pin: None,
                                scroll_region_px: Some(region),
                                scroll_manual: manual,
                                frame: None,
                            },
                            window,
                        );
                    }
                    ToolButton::Finish => {
                        // 兜底：若 Text 工具还活着没提交，先把它的内容落成命令
                        this.finalize_text_input_if_active(cx);
                        let s = this.selection.current().or(Some(this.screen_bounds));
                        let cmds: Vec<DrawCommand> =
                            this.drawing.visible_commands().map(|a| &**a).cloned().collect();
                        this.commit(OverlayResult { selection: s, commands: cmds, no_clipboard: false, pin: None, scroll_region_px: None, scroll_manual: false, frame: None }, window);
                    }
                    _ => {}
                }
            });
        });
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

    // 第一行：粗细档位（8 档，flex_wrap 自动换行成 4×2）
    let mut top = div().flex().flex_wrap().gap(px(4.0)).items_center();
    for (i, &lw) in LINE_WIDTHS.iter().enumerate() {
        let weak_lw = weak.clone();
        let is_current = (cur_lw - lw).abs() < f32::EPSILON;
        // 0.5 显示 "0.5"，整数档显示 "1"、"2" 等
        let label: gpui::SharedString = format!("{}", lw).into();
        let btn = Button::new(("lw", i))
            .label(label)
            .compact()
            .selected(is_current)
            .on_click(move |_, _, cx| {
                let _ = weak_lw.update(cx, |this, cx| {
                    this.finalize_text_input_if_active(cx);
                    this.toolbar.line_width = lw;
                    // 应用到选中命令（改宽），下次可二次编辑
                    this.apply_style_to_selected(|cmd| match cmd {
                        DrawCommand::Rectangle { line_width, .. }
                        | DrawCommand::Ellipse { line_width, .. }
                        | DrawCommand::Arrow { line_width, .. }
                        | DrawCommand::Freehand { line_width, .. } => *line_width = lw,
                        _ => {}
                    });
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
                        // 应用到选中命令（改色）
                        this.apply_style_to_selected(|cmd| match cmd {
                            DrawCommand::Rectangle { color, .. }
                            | DrawCommand::Ellipse { color, .. }
                            | DrawCommand::Arrow { color, .. }
                            | DrawCommand::Freehand { color, .. } => *color = c,
                            _ => {}
                        });
                        cx.notify();
                    });
                }),
        );
    }
    row
}


/// RGBA → BGRA 通道 swap（GPUI RenderImage 用 BGRA）
/// RGBA → BGRA（RenderImage 数据约定是 BGRA，见 gpui_wgpu swizzle_upload_data）。
///
/// 按 u32 批量位运算，一次处理 4 字节：迭代次数是逐字节 swap 的 1/4，debug
/// 未优化构建下也快得多（release 下约 2-3ms / 1920×1080 帧，debug 下原先
/// chunks_exact_mut(4)+swap 的 207 万次迭代要 ~100ms+，是复用路径的主要开销）。
fn rgba_to_bgra(pixels: &mut [u8]) {
    debug_assert_eq!(pixels.len() % 4, 0);
    if (pixels.as_ptr() as usize).is_multiple_of(4) {
        // 快路径：u32 批量位运算，一次处理 4 字节（迭代数是逐字节 swap 的 1/4）
        let words: &mut [u32] = unsafe {
            std::slice::from_raw_parts_mut(pixels.as_mut_ptr() as *mut u32, pixels.len() / 4)
        };
        for w in words {
            // 输入 RGBA(LE u32): R | G<<8 | B<<16 | A<<24 → 输出 BGRA: B | G<<8 | R<<16 | A<<24
            *w = ((*w & 0x0000_00FF) << 16)
                | (*w & 0x0000_FF00)
                | ((*w & 0x00FF_0000) >> 16)
                | (*w & 0xFF00_0000);
        }
    } else {
        // 慢路径：缓冲区未 4 字节对齐时退回避让（罕见）
        for c in pixels.chunks_exact_mut(4) {
            c.swap(0, 2);
        }
    }
}

/// 检测点击是否落在文字输入框的边框（Move）或 resize 手柄上，返回对应的 DragState
fn hit_test_text_drag(rect: ub::Bounds, p: BoundsPoint) -> Option<TextDragState> {
    // 手柄命中容差：±4px（对应 8px 手柄，与矩形选中框一致）
    const HANDLE_HALF: f32 = 4.0;
    // 边框移动环厚度（px）
    const MOVE_RING: f32 = 6.0;
    // 手柄中心在边框线上（跨线一半在外侧），与矩形选中框的 8 个手柄一致
    let hit = |cx: f32, cy: f32| -> bool {
        p.x >= cx - HANDLE_HALF && p.x <= cx + HANDLE_HALF
            && p.y >= cy - HANDLE_HALF && p.y <= cy + HANDLE_HALF
    };
    let x = rect.origin.x;
    let y = rect.origin.y;
    let w = rect.size.x;
    let h = rect.size.y;
    let mx = x + w / 2.0;
    let my = y + h / 2.0;

    // 优先检测 8 个 resize 手柄（四角 + 四边中点）
    let checks: &[(TextDragMode, f32, f32)] = &[
        (TextDragMode::ResizeNW, x, y),
        (TextDragMode::ResizeN, mx, y),
        (TextDragMode::ResizeNE, x + w, y),
        (TextDragMode::ResizeW, x, my),
        (TextDragMode::ResizeE, x + w, my),
        (TextDragMode::ResizeSW, x, y + h),
        (TextDragMode::ResizeS, mx, y + h),
        (TextDragMode::ResizeSE, x + w, y + h),
    ];
    for &(mode, hx, hy) in checks {
        if hit(hx, hy) {
            return Some(TextDragState {
                mode,
                start_mouse: p,
                start_rect: rect,
            });
        }
    }

    // 四条边框内侧 6px 环为 Move 拖动区（与四边覆盖层一致；手柄优先命中）
    let on_ring = (p.x >= x && p.x <= x + w
        && (p.y >= y && p.y <= y + MOVE_RING || (p.y >= y + h - MOVE_RING && p.y <= y + h)))
        || (p.y >= y && p.y <= y + h
            && (p.x >= x && p.x <= x + MOVE_RING || (p.x >= x + w - MOVE_RING && p.x <= x + w)));
    if on_ring {
        return Some(TextDragState {
            mode: TextDragMode::Move,
            start_mouse: p,
            start_rect: rect,
        });
    }
    None
}

/// 计算 Move 拖动后的新 rect：只钳制 origin、保持尺寸不变（不能用 clamp_inside，
/// 它会在越界时收缩 height，导致框贴底后"往下拖不动"——实为被压扁）。
fn text_move_rect(
    start: ub::Bounds,
    start_mouse: BoundsPoint,
    p: BoundsPoint,
    limits: ub::Bounds,
) -> ub::Bounds {
    let dx = p.x - start_mouse.x;
    let dy = p.y - start_mouse.y;
    let max_x = (limits.origin.x + limits.size.x - start.size.x).max(limits.origin.x);
    let max_y = (limits.origin.y + limits.size.y - start.size.y).max(limits.origin.y);
    ub::Bounds {
        origin: BoundsPoint::new(
            (start.origin.x + dx).clamp(limits.origin.x, max_x),
            (start.origin.y + dy).clamp(limits.origin.y, max_y),
        ),
        size: start.size,
    }
}

/// 计算 ResizeN（顶部中点手柄）拖动后的新 rect：顶边跟随鼠标、高度随之增减，
/// 高度低于 MIN_H 时钳制并让顶边停在对应位置。
fn text_resize_n_rect(start: ub::Bounds, start_mouse: BoundsPoint, p: BoundsPoint) -> ub::Bounds {
    const MIN_H: f32 = 40.0;
    let dy = p.y - start_mouse.y;
    let new_y = start.origin.y + dy;
    let new_h = (start.size.y - dy).max(MIN_H);
    let clamped_y = if new_h == MIN_H {
        start.origin.y + (start.size.y - MIN_H)
    } else {
        new_y
    };
    ub::Bounds {
        origin: BoundsPoint::new(start.origin.x, clamped_y),
        size: BoundsPoint::new(start.size.x, new_h),
    }
}

/// 应用文字框拖动 / resize 增量到 `text_input_rect`
fn apply_text_drag(this: &mut OverlayView, drag: TextDragState, p: BoundsPoint) {
    let dx = p.x - drag.start_mouse.x;
    let dy = p.y - drag.start_mouse.y;
    let start = drag.start_rect;
    // 最小尺寸限制：避免拖成 1px
    const MIN_W: f32 = 80.0;
    const MIN_H: f32 = 40.0;
    let limits = this.selection.current().unwrap_or(this.screen_bounds);
    let new_rect = match drag.mode {
        TextDragMode::Move => text_move_rect(start, drag.start_mouse, p, limits),
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
        TextDragMode::ResizeN => text_resize_n_rect(start, drag.start_mouse, p),
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
    this.text_input_rect = match drag.mode {
        TextDragMode::Move => new_rect,
        _ => new_rect.clamp_inside(limits),
    };
}

/// 点到线段的最近距离
fn point_segment_distance(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-6 {
        return ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
    }
    let t = (((px - x1) * dx + (py - y1) * dy) / len_sq).clamp(0.0, 1.0);
    let proj_x = x1 + t * dx;
    let proj_y = y1 + t * dy;
    ((px - proj_x).powi(2) + (py - proj_y).powi(2)).sqrt()
}

/// 描边命中半径：线宽一半 + 固定容差（下限保证细线也容易点中）
fn stroke_hit_radius(line_width: f32) -> f32 {
    (line_width * 0.5 + 4.0).max(6.0)
}

/// 判断点是否落在形状的「描边线条」上（点击选中 / hover 小手光标共用）
fn hit_test_stroke(cmd: &DrawCommand, p: BoundsPoint) -> bool {
    match cmd {
        DrawCommand::Rectangle { rect, line_width, .. } => {
            let x1 = rect.0.x.min(rect.1.x);
            let y1 = rect.0.y.min(rect.1.y);
            let x2 = rect.0.x.max(rect.1.x);
            let y2 = rect.0.y.max(rect.1.y);
            let r = stroke_hit_radius(*line_width);
            point_segment_distance(p.x, p.y, x1, y1, x2, y1) <= r
                || point_segment_distance(p.x, p.y, x1, y2, x2, y2) <= r
                || point_segment_distance(p.x, p.y, x1, y1, x1, y2) <= r
                || point_segment_distance(p.x, p.y, x2, y1, x2, y2) <= r
        }
        DrawCommand::Ellipse { rect, line_width, .. } => {
            let cx = (rect.0.x + rect.1.x) / 2.0;
            let cy = (rect.0.y + rect.1.y) / 2.0;
            let rx = (rect.0.x - rect.1.x).abs() / 2.0;
            let ry = (rect.0.y - rect.1.y).abs() / 2.0;
            let r = stroke_hit_radius(*line_width);
            let n = 128;
            let mut prev: Option<(f32, f32)> = None;
            for i in 0..=n {
                let theta = 2.0 * std::f32::consts::PI * i as f32 / n as f32;
                let cur = (cx + rx * theta.cos(), cy + ry * theta.sin());
                if let Some((ax, ay)) = prev {
                    if point_segment_distance(p.x, p.y, ax, ay, cur.0, cur.1) <= r {
                        return true;
                    }
                }
                prev = Some(cur);
            }
            false
        }
        DrawCommand::Arrow { from, to, line_width, .. } => {
            let r = stroke_hit_radius(*line_width).max(HANDLE_HIT_HALF * 2.0);
            point_segment_distance(p.x, p.y, from.x, from.y, to.x, to.y) <= r
        }
        _ => false,
    }
}

/// 检测点击是否落在已绘制命令的手柄或描边线条上
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
            // 只有点在描边（线条）上才算命中（选中）；内部空白/外部都不命中。
            if hit_test_stroke(cmd, p) {
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
                if (0.0..=1.0).contains(&t) {
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

/// 鼠标是否悬停在某个可选中形状的描边线条上（矩形/椭圆/箭头）
fn any_shape_stroke_hit(drawing: &DrawingState, p: BoundsPoint) -> bool {
    drawing
        .visible_commands_with_indices()
        .any(|(_, cmd)| hit_test_stroke(cmd, p))
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
    // 命令已原地修改：递增 revision，让形状层缓存在下一帧失效重建
    this.drawing.revision += 1;
}

/// 鼠标按下时开始文字框拖动/缩放：直接设置 `text_input_drag`（记录起点 rect 与
/// 按下位置），不再依赖 root.on_mouse_down 的几何命中检测。拖动/缩放过程由
/// root.on_mouse_move 与文字框自身的 on_mouse_move 共同驱动，松手由 on_mouse_up 结束。
fn begin_text_drag(
    this: &mut OverlayView,
    mode: TextDragMode,
    ev: &MouseDownEvent,
    window: &mut Window,
    cx: &mut Context<OverlayView>,
) {
    let p = to_bounds_point(ev.position);
    this.text_input_drag = Some(TextDragState {
        mode,
        start_mouse: p,
        start_rect: this.text_input_rect,
    });
    window.prevent_default();
    cx.stop_propagation();
}

/// 构造文字输入框的角 resize handle（6×6 方块）
///
/// 鼠标按下时把 `text_input_drag` 置为对应 mode + 记录起点 rect。
/// 鼠标移动在 root.on_mouse_move 里统一处理（用户可以拖到框外）。
/// 在 handle 上直接绑 on_mouse_down 并 stop_propagation，避免依赖 root 的
/// 几何命中检测（此前点手柄/拖动条偶尔不响应）。
fn make_resize_handle(
    id: impl Into<gpui::ElementId>,
    left: f32,
    top: f32,
    mode: TextDragMode,
    cx: &mut Context<OverlayView>,
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
        .size(px(HANDLE_VISUAL_SIZE))
        .bg(gpui::rgba(0xFFFFFFFF))
        .border_1()
        .border_color(gpui::rgba(0x0066CCFF))
        .cursor(cursor)
        .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev, window, cx| {
            begin_text_drag(this, mode, ev, window, cx);
        }))
}

/// 由 RGBA 像素数据构建 GPUI RenderImage（原地转 BGRA 后移交所有权，
/// 避免整帧 clone）。调用方不再需要该像素时直接传入，零拷贝。
fn build_render_image_from_pixels(width: u32, height: u32, mut pixels: Vec<u8>) -> Arc<RenderImage> {
    rgba_to_bgra(&mut pixels);
    let buffer = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, pixels)
        .expect("CapturedFrame 像素长度必须与 width*height*4 一致");
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)))
}

/// 把 GPUI 像素坐标转成 SelectionState 用的 f32 点（utils::bounds::Point）
fn to_bounds_point(p: Point<Pixels>) -> BoundsPoint {
    BoundsPoint::new(f32::from(p.x), f32::from(p.y))
}

/// 把 DrawCommand 中的所有坐标从 canvas 坐标转为帧物理像素坐标。
/// 只读借用输入，重建所有字段，避免调用方先 clone 再移交所有权。
fn scale_draw_command(cmd: &DrawCommand, sx: f32, sy: f32) -> DrawCommand {
    use crate::overlay::drawing::Point as DP;
    let sp = |p: &DP| DP::new(p.x * sx, p.y * sy);
    match cmd {
        DrawCommand::Rectangle { rect, color, line_width } => DrawCommand::Rectangle {
            rect: (sp(&rect.0), sp(&rect.1)),
            color: *color,
            // 线宽随坐标系缩放：物理 buffer 里 = lw×scale，paint 缩回逻辑显示 = lw
            line_width: *line_width * sx,
        },
        DrawCommand::Ellipse { rect, color, line_width } => DrawCommand::Ellipse {
            rect: (sp(&rect.0), sp(&rect.1)),
            color: *color,
            line_width: *line_width * sx,
        },
        DrawCommand::Arrow { from, to, color, line_width } => DrawCommand::Arrow {
            from: sp(from),
            to: sp(to),
            color: *color,
            line_width: *line_width * sx,
        },
        DrawCommand::Freehand { points, color, line_width } => DrawCommand::Freehand {
            points: points.iter().map(sp).collect(),
            color: *color,
            line_width: *line_width * sx,
        },
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
            DrawCommand::Text {
                anchor: sp(anchor),
                content: content.clone(),
                font_size: *font_size,
                color: *color,
                max_width: max_width.map(|w| w * sx),
                weight: *weight,
            }
        }
        DrawCommand::Mosaic { regions, block_size, color } => DrawCommand::Mosaic {
            regions: regions.iter().map(|r| (sp(&r.0), sp(&r.1))).collect(),
            block_size: (*block_size as f32 * sx).max(1.0) as u32,
            color: *color,
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

/// 把矩形/椭圆/箭头/画图这 4 类形状用解析式抗锯齿光栅化到离屏缓冲，再整幅贴到 canvas。
///
/// 原 preview 用 `paint_thick_line` 叠很多小圆角 quad，每个 quad 的 bounds 会被
/// 是否为走离屏解析式 AA 的形状命令（Text/Mosaic 由元素层 / 即时模式绘制）
fn is_shape_command(c: &DrawCommand) -> bool {
    matches!(
        c,
        DrawCommand::Rectangle { .. }
            | DrawCommand::Ellipse { .. }
            | DrawCommand::Arrow { .. }
            | DrawCommand::Freehand { .. }
    )
}

/// 光栅化一组形状命令到 BGRA 图像 + 联合包围盒（逻辑像素，含 AA 外扩）。
///
/// GPUI `paint_quad` pixel_snap 到整数像素，重叠边缘产生串珠/锯齿感，所以这里
/// 复用 commit 路径的 `commands::apply_commands` 逐像素解析式 AA，得到与最终成图
/// 一致的平滑线条。返回 `None` 若无形状。
fn rasterize_shapes(
    shapes: &[&DrawCommand],
    scale_factor: f32,
    window: &Window,
    step: u32,
) -> Option<(Arc<RenderImage>, ub::Bounds)> {
    if shapes.is_empty() {
        return None;
    }

    // 联合包围盒（逻辑像素）+ 最大线宽，外扩 padding 覆盖描边/箭头外扩
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    let mut max_lw = 0.0_f32;
    // 箭头头的底边半宽（head_w = 2*line_width），垂直方向超出箭杆中线，需计入外扩
    let mut max_arrow_head_w = 0.0_f32;
    for cmd in shapes.iter().copied() {
        match cmd {
            DrawCommand::Rectangle { rect, line_width, .. }
            | DrawCommand::Ellipse { rect, line_width, .. } => {
                let (a, b) = rect;
                min_x = min_x.min(a.x.min(b.x));
                min_y = min_y.min(a.y.min(b.y));
                max_x = max_x.max(a.x.max(b.x));
                max_y = max_y.max(a.y.max(b.y));
                max_lw = max_lw.max(*line_width);
            }
            DrawCommand::Arrow { from, to, line_width, .. } => {
                min_x = min_x.min(from.x.min(to.x));
                min_y = min_y.min(from.y.min(to.y));
                max_x = max_x.max(from.x.max(to.x));
                max_y = max_y.max(from.y.max(to.y));
                max_lw = max_lw.max(*line_width);
                max_arrow_head_w = max_arrow_head_w.max((*line_width * 2.0).max(1.0));
            }
            DrawCommand::Freehand { points, line_width, .. } => {
                for p in points {
                    min_x = min_x.min(p.x);
                    min_y = min_y.min(p.y);
                    max_x = max_x.max(p.x);
                    max_y = max_y.max(p.y);
                }
                max_lw = max_lw.max(*line_width);
            }
            _ => {}
        }
    }

    // line_width 是物理像素，转逻辑 px 外扩；箭头还需覆盖头底边（半宽 head_w），
    // 再给 AA 留 1px
    let pad = (max_lw * 0.5 + max_arrow_head_w) / scale_factor + 1.0;
    min_x -= pad;
    min_y -= pad;
    max_x += pad;
    max_y += pad;

    let win = window.bounds();
    let win_w = f32::from(win.size.width);
    let win_h = f32::from(win.size.height);
    let origin_x = min_x.floor().clamp(0.0, win_w);
    let origin_y = min_y.floor().clamp(0.0, win_h);
    let size_w = (max_x - origin_x).ceil().max(1.0).min((win_w - origin_x).max(1.0));
    let size_h = (max_y - origin_y).ceil().max(1.0).min((win_h - origin_y).max(1.0));

    // 1x 分辨率光栅化（与提交成图一致）：不超采样——超采样图缩小显示依赖
    // GPU 线性过滤，1x 屏幕上边界对齐/采样行为不可控（曾出现线条缺像素）。
    // 边缘平滑由 draw_thick_line 的 AA 过渡带宽（aa=1.0）保证。
    let raster_scale = scale_factor;
    let phys_w = (size_w * raster_scale).round() as u32;
    let phys_h = (size_h * raster_scale).round() as u32;
    if phys_w == 0 || phys_h == 0 {
        return None;
    }
    let phys_origin_x = origin_x * raster_scale;
    let phys_origin_y = origin_y * raster_scale;

    // 透明离屏缓冲：形状坐标转物理像素后走与 commit 相同的解析式 AA
    let mut frame = CapturedFrame {
        width: phys_w,
        height: phys_h,
        pixels: vec![0; (phys_w * phys_h * 4) as usize],
    };
    let scaled: Vec<DrawCommand> = shapes
        .iter()
        .copied()
        .map(|c| scale_draw_command(c, raster_scale, raster_scale))
        .collect();
    let _ = crate::overlay::commands::apply_commands_step(
        &mut frame,
        phys_origin_x,
        phys_origin_y,
        &scaled,
        step,
    );

    let img = build_render_image_from_pixels(frame.width, frame.height, frame.pixels);
    Some((
        img,
        ub::Bounds {
            origin: ub::Point::new(origin_x, origin_y),
            size: ub::Point::new(size_w, size_h),
        },
    ))
}

/// 把已光栅化的形状层画到窗口
fn paint_raster(window: &mut Window, image: &Arc<RenderImage>, bounds: ub::Bounds) {
    let _ = window.paint_image(
        Bounds {
            origin: gpui::point(gpui::px(bounds.origin.x), gpui::px(bounds.origin.y)),
            size: Size::new(gpui::px(bounds.size.x), gpui::px(bounds.size.y)),
        },
        Default::default(),
        image.clone(),
        0,
        false,
    );
}

/// 把一个 DrawCommand 渲染到 window 上（Phase 3 preview，Phase 4 也会复用）
fn paint_command(cmd: &DrawCommand, window: &mut Window, cx: &mut App, scale_factor: f32) {
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
        // 箭头由形状层走离屏解析式 AA（与最终成图一致），这里无需处理
        DrawCommand::Arrow { .. } => {}
        DrawCommand::Freehand { ref points, color, line_width } => {
            for w in points.windows(2) {
                paint_thick_line(w[0].x, w[0].y, w[1].x, w[1].y, *line_width, *color, window);
            }
        }
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
                let fs = *font_size / scale_factor;
                // 行盒必须随字号缩放，避免 paint_layer 高度 < asc+descent 时字形被裁剪。
                let line_height = px(fs * 1.5);
                // 编辑态 Input 的实际文字行盒：实测（range_to_bounds）等于输入字号×1.5，即
                // 与 paint 的行盒 line_height 相同。字形相对行盒顶偏移 (lh-asc-desc)/2，两者
                // 行盒一致 → 补偿 (input_lh - line_height)/2 = 0，paint 起点 = 编辑态行盒顶。
                // 用 window.line_height()（24）或 fs×1.4 都会让补偿非零，提交后文字上下漂移。
                let input_lh = line_height;
                let origin_fx = anchor.x + TO_X;
                // 编辑态文字行盒顶相对 box 的偏移：6px 顶部占位 spacer + 2px input_py
                // 内边距 = +8（已用 range_to_bounds 实测，Linux 与 Windows 一致）。
                // 旧实现 Linux 用 +7（当时编辑态 Input 带 1px 边框）；去掉边框后行盒顶
                // 变为 +8，沿用 +7 会让提交后文字上移 1px。
                let origin_fy = anchor.y + TO_Y;
                let origin_x = window.pixel_snap(px(origin_fx));
                // 先对 box 基准点做像素对齐（与 Input 所在 box 的整块栅格化一致），
                // 再叠加行高偏移，避免偏移非整数时 pixel_snap 单独取整导致错位。
                let mut origin_y = window.pixel_snap(px(origin_fy))
                    + px((input_lh - line_height).as_f32() / 2.0);
                tracing::debug!(
                    "render Text paint: anchor=({:.1},{:.1}) origin=({:.1},{:.1}) content={:?} fs={:.1} max_w={:?}",
                    anchor.x, anchor.y, origin_fx, origin_fy, content, *font_size, *max_width
                );

                let mut base_run = window.text_style().to_run(0);
                base_run.font.family = gpui::SharedString::from(crate::overlay::font::TEXT_FONT_FAMILY);
                base_run.color = Hsla::from(rgba(rgba_u32(*color)));
                if *weight == FontWeight::Bold {
                    base_run.font.weight = gpui::FontWeight::BOLD;
                }

                // 只把 max_width 传给 paint 作对齐宽度（TextAlign::Left 下无效）。
                // 不能传 force_width 给 shape_line：GPUI 会把超出宽度的字形按
                // glyph_index*force_width 重排，文本宽于框时中间出现整段空隙（"文字分开"）。
                let force_width = max_width.map(px);

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
                        None,
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

/// 同步执行 OCR：从 frame_pixels 中裁切 rect 区域，放大后交给 PaddleOCR
/// （PP-OCRv6，本地 ONNX 推理）识别。
///
/// `window_w` / `window_h` 是 GPUI 窗口的实际尺寸（逻辑像素），必须从
/// `window.bounds()` 获取。它与 frame 物理尺寸可能有差异（如任务栏挤压），
/// `paint_image` 会基于两者之比缩放图像，像素提取需用相同比率。
/// 在后台线程对选区区域做 OCR：接收选区 RGBA 像素（已裁剪，避免克隆整帧）。
fn run_ocr_sync(
    region_pixels: Vec<u8>,
    region_width: u32,
    region_height: u32,
) -> String {
    let w = region_width;
    let h = region_height;
    if w == 0 || h == 0 {
        return String::new();
    }

    // 从 RGBA 区域像素转 RGB
    let mut rgb: Vec<u8> = Vec::with_capacity((w * h * 3) as usize);
    for row in 0..h {
        let base = row as usize * w as usize * 4;
        for col in 0..w {
            let idx = base + col as usize * 4;
            rgb.push(region_pixels[idx]);     // R
            rgb.push(region_pixels[idx + 1]); // G
            rgb.push(region_pixels[idx + 2]); // B
        }
    }

    // 注意：不再放大。PaddleOCR 检测器内部会把输入 resize 到
    // limit_side_len（480）再推理，放大只会增加内存/耗时、无识别收益
    // （实测放大 2 倍识别结果与耗时均无变化）。
    let up = image::RgbImage::from_raw(w, h, rgb).unwrap_or_else(|| image::RgbImage::new(w, h));

    // 写入预处理后的调试 PNG（用系统临时目录，Linux 的 /tmp 在 Windows/macOS 上不存在）
    let debug_path = std::env::temp_dir().join("screenshot_ocr_debug.png");
    if let Err(e) = up.save(&debug_path) {
        tracing::error!("OCR: 保存调试 PNG 失败: {}", e);
    } else {
        tracing::info!(
            "OCR: 调试 PNG 已保存到 {} ({}x{})",
            debug_path.display(),
            w,
            h,
        );
    }

    // PaddleOCR（PP-OCRv6 medium）识别：首次使用自动下载模型（约 132 MB）
    // 到缓存目录；推理在本地 ONNX Runtime 完成。
    match crate::ocr::paddle::recognize_rgb(up.as_raw(), up.width(), up.height()) {
        Ok(text) => {
            tracing::info!("OCR: 识别结果 ({} bytes): {:?}", text.len(), text);
            text
        }
        Err(e) => {
            tracing::error!("OCR 识别失败: {e}");
            format!("⚠ OCR 失败: {e}")
        }
    }
}

impl Render for OverlayView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // dim 遮罩直接到位（无淡入动画）——动画造成"两次变暗"的视觉，感知上拖慢响应

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
        // Arc 共享：克隆指针(O(1))而非深拷贝已提交命令(Freehand点集/Mosaic区域)
        let visible_cmds: Vec<std::sync::Arc<DrawCommand>> =
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

        // 已提交形状层：命令/缩放未变则复用缓存，仅在提交/撤销/拖动等编辑时重建
        let drawing_revision = self.drawing.revision;
        let cache_stale = match &self.shape_layer_cache {
            None => true,
            Some(c) => c.revision != drawing_revision || c.scale_factor != scale_factor,
        };
        if cache_stale {
            // committed 完整渲染所有形状（含 Freehand）——与 in_progress 同一光栅化
            let committed: Vec<&DrawCommand> = self
                .drawing
                .visible_commands()
                .filter(|c| is_shape_command(c))
                .map(|c| &**c)
                .collect();
            self.shape_layer_cache =
                rasterize_shapes(&committed, scale_factor, window, 1).map(|(image, bounds)| {
                    ShapeLayerCache {
                        revision: drawing_revision,
                        scale_factor,
                        image,
                        bounds,
                    }
                });
        }
        let committed_shape_layer = self
            .shape_layer_cache
            .as_ref()
            .map(|c| (c.image.clone(), c.bounds));

        // 当前笔画：所有形状（含 Freehand）统一走 rasterize_shapes 全量光栅化——
        // 与提交成图同一函数同一参数，像素级一致（无增量/全量两套 bounds 差异）。
        // 性能由 draw_polyline 网格分桶保证（3000 点约 24ms）。
        let in_progress_shape_layer = match &self.in_progress {
            Some(ip) if is_shape_command(ip) => {
                rasterize_shapes(&[&**ip], scale_factor, window, 1)
            }
            _ => None,
        };

        // 已提交的 Input 展示态：canvas 应跳过对应 Text 命令，避免文字重复
        // （已提交文字由元素层 Input 绘制）。
        let skip_canvas_idx: Option<usize> =
            if self.text_input_finalized { self.text_input_cmd_idx } else { None };

        let ocr_rect = self.ocr_rect;
        let ocr_dragging = self.ocr_drag_start.is_some();
        let dim_opacity = self.dim_opacity;
        let hover_shape = self.hover_shape;
        // 文字框 auto_grow 测量提前到 render 开头：canvas 边框与 Input 必须基于
        // 同一份 text_input_rect。若在 Input 渲染时才更新 size，canvas closure 捕获
        // 的是测量前的旧值，导致边框与输入框错位（光标跑到框外、文字被边框盖住）。
        if let Some(ref input) = self.text_input {
            if !self.text_input_finalized {
                let value: String = input.read(cx).value().to_string();
                if !value.is_empty() {
                    let sf = self.scale_factor;
                    let fs = self.toolbar.current_size;
                    let weight = self.toolbar.current_weight;
                    // 命中缓存则跳过两次 cosmic-text shaping；值/字号/字重任一变化才重测。
                    let (adv_px, th_px) = match &self.text_measure {
                        Some((v, f, w, a, t)) if *v == value && *f == fs && *w == weight => {
                            (*a, *t)
                        }
                        _ => {
                            let (_tw_px, th_px, _, _) =
                                crate::overlay::commands::measure_text_px(&value, fs, None, weight);
                            // 宽度必须用真实行宽 advance（光标能到的右边界），不能用字形包围盒：
                            // 包围盒对 CJK 会低估（ink≈0.78×advance），导致长文本下光标贴近右缘时
                            // 编辑器产生负 scroll_offset 把整行文字左移，首字被 overflow_hidden 裁掉。
                            let adv_px = crate::overlay::commands::measure_line_advance_px(
                                &value, fs, weight,
                            );
                            self.text_measure = Some((value.clone(), fs, weight, adv_px, th_px));
                            (adv_px, th_px)
                        }
                    };
                    // Input 实际内边距：1px border + 8px padding，每侧 9px 共 18px；
                    // 再留 10px 右缘（RIGHT_MARGIN），使 content = adv + 10 ≥ adv，不触发左滚。
                    const INSET_X: f32 = 18.0;
                    const RIGHT_MARGIN: f32 = 10.0;
                    const MIN_W: f32 = 100.0;
                    const MIN_H: f32 = 40.0;
                    let new_w = if adv_px > 0.0 {
                        (adv_px / sf + INSET_X + RIGHT_MARGIN).max(MIN_W)
                    } else {
                        MIN_W
                    };
                    // 高度按 Input 实际行盒计算，避免多行文字向下溢出编辑框：
                    // th_px 是字形包围盒高，对多行会低估行距（字形 < 行盒）。
                    // 注意：Input 实际行高是 1.5×字号（.line_height(relative(1.5))，
                    // 实测 range_to_bounds 的 lh = fs×1.5）。auto-grow 必须用 1.5×字号，
                    // 否则每行少算 0.1×字号（fs=24 时 2.4px），行数越多框越矮，文字在
                    // 框内相对下沉、多行越多越明显（单行无累积）。
                    // auto_grow(1,8) → 框高度锁定在 1..8 行，超出 8 行 Input 内部滚动。
                    let rows = value.matches('\n').count() + 1;
                    let effective_rows = rows.clamp(1, 8);
                    let line_h = window.line_height().as_f32().max(fs / sf * 1.5);
                    let new_h =
                        (effective_rows as f32 * line_h + 6.0 + 2.0 + 2.0 + 4.0).max(MIN_H);
                    let old_w = self.text_input_rect.size.x;
                    let old_h = self.text_input_rect.size.y;
                    self.text_input_rect.size = BoundsPoint::new(new_w, new_h);
                    // 高度随行数增长后裁剪回截图框（selection）内，防止文字画出截图区域
                    let limits = self.selection.current().unwrap_or(self.screen_bounds);
                    self.text_input_rect = self.text_input_rect.clamp_inside(limits);
                    if (old_w - new_w).abs() > 0.01 || (old_h - new_h).abs() > 0.01 {
                        tracing::info!(
                            "textbox auto-grow: value={:?} fs={:.1} sf={:.1} th_px={:.1} adv_px={:.1} box=({:.1},{:.1})->({:.1},{:.1})",
                            value, fs, sf, th_px, adv_px,
                            old_w, old_h, new_w, new_h,
                        );
                    }
                }
            }
        }

        // 文字框编辑态：canvas 负责绘制边框与 8 个手柄（与矩形选中框同款），
        // 元素层只留透明命中区，避免 div 渲染被裁剪/遮挡导致手柄缺失。
        let text_editing = self.text_input.is_some() && !self.text_input_finalized;
        let text_input_rect = self.text_input_rect;

        let paint_canvas = canvas(
            move |_, _, _| (in_progress, visible_cmds, committed_shape_layer, in_progress_shape_layer, sel_visible_idx, scale_factor, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity, hover_shape, text_editing, text_input_rect),
            move |_, (in_progress, visible_cmds, committed_shape_layer, in_progress_shape_layer, sel_visible_idx, scale_factor, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity, hover_shape, text_editing, text_input_rect), window, cx| {
                // 悬停在可选中形状的描边上时，整个窗口显示小手光标（window 级光标
                // 优先级高于元素级 cursor；未悬停时不设置，让文字/手柄的 cursor 正常生效）。
                if hover_shape {
                    window.set_window_cursor_style(gpui::CursorStyle::PointingHand);
                }

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
                    // 矩形/椭圆/箭头/画图 → 已提交形状走缓存 + in_progress 增量重绘；
                    // Text（GPUI 文字）/ Mosaic（棋盘模拟）仍走 paint_command。
                    for (i, cmd) in visible_cmds.iter().map(|c| &**c).enumerate() {
                        if skip_canvas_idx == Some(i) {
                            continue;
                        }
                        if is_shape_command(cmd) {
                            continue;
                        }
                        paint_command(cmd, window, cx, scale_factor);
                    }
                    if let Some(ref ip) = in_progress {
                        if !is_shape_command(ip) {
                            paint_command(ip, window, cx, scale_factor);
                        }
                    }
                    // 形状层：已提交形状（缓存）先画，in_progress 那一笔增量叠在其上
                    if let Some((img, b)) = &committed_shape_layer {
                        paint_raster(window, img, *b);
                    }
                    if let Some((img, b)) = &in_progress_shape_layer {
                        paint_raster(window, img, *b);
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
                        if let Some(cmd) = visible_cmds.get(vidx).map(|c| &**c) {
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

                    // 4.5) 文字框编辑态：灰色边框 + 8 个手柄（与矩形选中框同款）。
                    // 直接画在 canvas 层（元素层 div 的 border/手柄可能被裁剪或遮挡），
                    // 保证四边灰色边框、8 个手柄都居中在边框线上且一定可见。
                    if text_editing {
                        let tr = text_input_rect;
                        let tx = px(tr.origin.x);
                        let ty = px(tr.origin.y);
                        let tw = px(tr.size.x.max(1.0));
                        let th = px(tr.size.y.max(1.0));
                        let gray = Hsla::from(rgba(0x999999FF));
                        let clear = gpui::transparent_black();
                        // 四条 1px 灰色边框线
                        window.paint_quad(quad(
                            Bounds { origin: point(tx, ty), size: Size::new(tw, px(1.0)) },
                            px(0.), gray, px(0.), clear, Default::default(),
                        ));
                        window.paint_quad(quad(
                            Bounds { origin: point(tx, ty + th - px(1.0)), size: Size::new(tw, px(1.0)) },
                            px(0.), gray, px(0.), clear, Default::default(),
                        ));
                        window.paint_quad(quad(
                            Bounds { origin: point(tx, ty), size: Size::new(px(1.0), th) },
                            px(0.), gray, px(0.), clear, Default::default(),
                        ));
                        window.paint_quad(quad(
                            Bounds { origin: point(tx + tw - px(1.0), ty), size: Size::new(px(1.0), th) },
                            px(0.), gray, px(0.), clear, Default::default(),
                        ));
                        // 8 个手柄
                        let handle_fill = Hsla::from(rgba(0xFFFFFFFFu32));
                        let handle_border = Hsla::from(rgba(0x0066CCFFu32));
                        let half = px(HANDLE_VISUAL_SIZE / 2.0);
                        let edge = px(HANDLE_VISUAL_SIZE);
                        for hp in tr.handle_positions() {
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
        // 不传 max_width → 不自动换行，仅手动 Enter 换行；宽度在 render 前测量。
        if let Some(ref input) = self.text_input {
            // text_input_rect 已在 render 开头按内容 auto_grow，canvas 与这里读同一份值
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
                                        })
                                        .font_family(gpui::SharedString::from(
                                            crate::overlay::font::TEXT_FONT_FAMILY,
                                        ))
                                        // 行盒随字号缩放，避免大字号时编辑器把第一行顶部裁掉
                                        .line_height(gpui::relative(1.5)),
                                ),
                        ),
                );
            } else {
                let h_size = 6.0_f32;
                // 手柄 8×8（与矩形选中框一致），中心在边框线上（跨线各一半）：
                // 外侧一半靠去掉 overflow_hidden 保持可见
                let hh = HANDLE_VISUAL_SIZE / 2.0;
                let h_neg = -hh;
                let h_mx = lw / 2.0 - hh;
                let h_my = lh / 2.0 - hh;
                let h_rx = lw - hh;
                let h_by = lh - hh;
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
                            // 顶部透明占位（6px）：保证 Input 位置与提交态一致。
                            // 边框线与移动由 text-move-top 覆盖层负责（单实线 + 小手拖动）。
                            div()
                                .w_full()
                                .h(px(h_size)),
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
                                        })
                                        .font_family(gpui::SharedString::from(
                                            crate::overlay::font::TEXT_FONT_FAMILY,
                                        ))
                                        .line_height(gpui::relative(1.5)),
                                ),
                        )
                        // 四条单实线边框（1px，覆盖在 Input 边缘之上），同时也是移动
                        // 抓取区：悬停四条边任意位置显示小手，按住即可拖动整框。
                        // （Input 自身不再画边框，避免双线。）
                        .child(
                            div()
                                .id("text-move-top")
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .w(px(lw))
                                .h(px(h_size))
                                .border_t_1()
                                .border_color(gpui::rgba(0x999999FF))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_mouse_down(MouseButton::Left, cx.listener(|this, ev, window, cx| {
                                    begin_text_drag(this, TextDragMode::Move, ev, window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("text-move-bottom")
                                .absolute()
                                .top(px(lh - h_size))
                                .left(px(0.0))
                                .w(px(lw))
                                .h(px(h_size))
                                .border_b_1()
                                .border_color(gpui::rgba(0x999999FF))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_mouse_down(MouseButton::Left, cx.listener(|this, ev, window, cx| {
                                    begin_text_drag(this, TextDragMode::Move, ev, window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("text-move-left")
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .w(px(h_size))
                                .h(px(lh))
                                .border_l_1()
                                .border_color(gpui::rgba(0x999999FF))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_mouse_down(MouseButton::Left, cx.listener(|this, ev, window, cx| {
                                    begin_text_drag(this, TextDragMode::Move, ev, window, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("text-move-right")
                                .absolute()
                                .top(px(0.0))
                                .left(px(lw - h_size))
                                .w(px(h_size))
                                .h(px(lh))
                                .border_r_1()
                                .border_color(gpui::rgba(0x999999FF))
                                .cursor(gpui::CursorStyle::PointingHand)
                                .on_mouse_down(MouseButton::Left, cx.listener(|this, ev, window, cx| {
                                    begin_text_drag(this, TextDragMode::Move, ev, window, cx);
                                })),
                        )
                        .child(make_resize_handle("text-resize-nw", h_neg, h_neg, TextDragMode::ResizeNW, cx))
                        .child(make_resize_handle("text-resize-n", h_mx, h_neg, TextDragMode::ResizeN, cx))
                        .child(make_resize_handle("text-resize-ne", h_rx, h_neg, TextDragMode::ResizeNE, cx))
                        .child(make_resize_handle("text-resize-w", h_neg, h_my, TextDragMode::ResizeW, cx))
                        .child(make_resize_handle("text-resize-e", h_rx, h_my, TextDragMode::ResizeE, cx))
                        .child(make_resize_handle("text-resize-sw", h_neg, h_by, TextDragMode::ResizeSW, cx))
                        .child(make_resize_handle("text-resize-s", h_mx, h_by, TextDragMode::ResizeS, cx))
                        .child(make_resize_handle("text-resize-se", h_rx, h_by, TextDragMode::ResizeSE, cx))
                        // 文字框自身的 mouse_move/mouse_up 兜底：拖动/缩放过程中鼠标
                        // 始终落在框内（抓取点随框移动），即使事件没冒泡到 root 也能
                        // 继续拖动，保证移动跟手、不卡顿。
                        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                            if let Some(drag) = this.text_input_drag {
                                apply_text_drag(this, drag, to_bounds_point(ev.position));
                                cx.notify();
                            }
                        }))
                        .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| {
                            if this.text_input_drag.is_some() {
                                this.text_input_drag = None;
                            }
                        })),
                );
            }
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
                            // 不 return：继续向下走命令命中检测，让这次点击能
                            // 直接选中矩形/椭圆/箭头或重新编辑文字，而非只关闭输入框。
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
                                    .map(|(i, a)| (i, &**a))
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
                                .map(|(i, a)| (i, &**a))
                                .collect();
                            for (idx, cmd) in visible.iter().rev() {
                                if let Some(mode) = hit_test_cmd_drag(cmd, p) {
                                    this.selected_cmd_actual_idx = Some(*idx);
                                    // 同步工具栏显示该命令的线宽/颜色（便于二次编辑）
                                    this.toolbar.line_width = match cmd {
                                        DrawCommand::Rectangle { line_width, .. }
                                        | DrawCommand::Ellipse { line_width, .. }
                                        | DrawCommand::Arrow { line_width, .. }
                                        | DrawCommand::Freehand { line_width, .. } => *line_width,
                                        _ => this.toolbar.line_width,
                                    };
                                    this.toolbar.current_color = match cmd {
                                        DrawCommand::Rectangle { color, .. }
                                        | DrawCommand::Ellipse { color, .. }
                                        | DrawCommand::Arrow { color, .. }
                                        | DrawCommand::Freehand { color, .. } => *color,
                                        _ => this.toolbar.current_color,
                                    };
                                    this.cmd_drag = Some(CmdDragState {
                                        mode,
                                        start_mouse: p,
                                        cmd_index: *idx,
                                    });
                                    tracing::debug!("mouse_down: HIT cmd idx={}", idx);
                                    // 命中后立即重绘，否则手柄要等下一次 mouse_move 才出现
                                    // （干净点击 down+up 不产生 move，会看起来"点不中"）。
                                    cx.notify();
                                    return;
                                }
                            }
                        }
                        // 未命中任何命令 → 取消选中：只有点线条才选中，
                        // 点其他任何区域（内部空白/外部/选区手柄）都取消选中。
                        if this.selected_cmd_actual_idx.is_some() {
                            this.selected_cmd_actual_idx = None;
                            cx.notify();
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
                } else if this.selection.drag != DragState::Idle {
                    this.selection.mouse_move(p);
                } else {
                    // 纯 Idle：无拖拽/绘制。更新 hover 状态——鼠标悬停在可选中形状的
                    // 描边线条上时显示小手光标；状态变化才重绘。
                    let over = this.mode == OverlayMode::Editing
                        && this.toolbar.active_tool != Some(ToolButton::Text)
                        && any_shape_stroke_hit(&this.drawing, p);
                    if over != this.hover_shape {
                        this.hover_shape = over;
                        cx.notify();
                    }
                    return;
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
                    // OCR 框选结束 → 提取像素、后台异步识别，并立即提交关闭遮罩
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
                                let fw = this.frame_width;
                                let fh = this.frame_height;
                                let wb = window.bounds();
                                let sx = this.frame_width as f32 / f32::from(wb.size.width).max(1.0);
                                let sy = this.frame_height as f32 / f32::from(wb.size.height).max(1.0);
                                // 裁剪选区像素构造 PinPayload（供 OcrPin 窗口左侧显示）
                                let sel_px = ub::Bounds {
                                    origin: ub::Point::new(rect.origin.x * sx, rect.origin.y * sy),
                                    size: ub::Point::new(rect.size.x * sx, rect.size.y * sy),
                                };
                                if let Ok(clipped) = CapturedFrame::clip_pixels(
                                    fw,
                                    fh,
                                    &this.frame_pixels,
                                    sel_px.origin.x as u32,
                                    sel_px.origin.y as u32,
                                    sel_px.size.x as u32,
                                    sel_px.size.y as u32,
                                ) {
                                    let pin_x = this.client_origin.x + rect.origin.x;
                                    let pin_y = this.client_origin.y + rect.origin.y;
                                    // 选区区域像素（RGBA，几百 KB）给后台 OCR 线程——只克隆选区
                                    // 而非整帧（整帧 8MB 一次性拷贝），显著减少内存与拷贝耗时。
                                    let region_pixels = clipped.pixels.clone();
                                    let region_w = clipped.width;
                                    let region_h = clipped.height;
                                    let payload = PinPayload {
                                        frame: clipped,
                                        origin_x: pin_x,
                                        origin_y: pin_y,
                                        sx,
                                        sy,
                                    };
                                    // 立即打开左图右文 OcrPin 窗口（右侧显示"识别中…"）
                                    let _ = ensure_started().send(OverlayCommand::OpenOcrPin(payload));
                                    std::thread::spawn(move || {
                                        let text =
                                            run_ocr_sync(region_pixels, region_w, region_h);
                                        if !text.is_empty() {
                                            if let Err(e) = crate::clipboard::global().write_text(&text) {
                                                tracing::error!("OCR: 结果写入剪贴板失败: {e}");
                                            } else {
                                                tracing::info!(
                                                    "OCR: 结果已复制到剪贴板 ({} bytes)",
                                                    text.len()
                                                );
                                            }
                                        } else {
                                            tracing::info!("OCR: 未识别到文字");
                                        }
                                        let _ = ensure_started().send(OverlayCommand::UpdateOcrPin(text));
                                    });
                                }
                                // 立即提交关闭遮罩（不再单独开 pin；图像由 OcrPin 窗口左侧展示）
                                this.commit(
                                    OverlayResult {
                                        selection: Some(rect),
                                        commands: vec![],
                                        no_clipboard: true,
                                        pin: None,
                                        scroll_region_px: None,
                                        scroll_manual: false,
                                        frame: None,
                                    },
                                    window,
                                );
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
                        // finish_draw 会 push 命令并自动选中，需立即重绘才能显示
                        // 提交后的形状与拖拽手柄（否则停留在上一帧的 in-progress 画面）。
                        cx.notify();
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
                            .map(|a| &**a)
                            .cloned()
                            .collect();
                        this.commit(OverlayResult { selection: sel, commands: cmds, no_clipboard: false, pin: None, scroll_region_px: None, scroll_manual: false, frame: None }, window);
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
                    this.commit(OverlayResult { selection: None, commands: vec![], no_clipboard: false, pin: None, scroll_region_px: None, scroll_manual: false, frame: None }, window);
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
                        .map(|a| &**a)
                        .cloned()
                        .collect();
                    this.commit(OverlayResult { selection: sel, commands: cmds, no_clipboard: false, pin: None, scroll_region_px: None, scroll_manual: false, frame: None }, window);
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
    fn new(frame: CapturedFrame, cx: &mut Context<Self>) -> Self {
        tracing::info!(
            "[Pin] PinWindowView::new: frame={}x{}",
            frame.width, frame.height
        );
        Self {
            image: build_render_image_from_pixels(frame.width, frame.height, frame.pixels),
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
                                    // Windows 上 gpui_windows 未实现 start_window_move
                                    // （gpui::PlatformWindow 默认 no-op），用 Win32 原生
                                    // 标题栏拖拽：ReleaseCapture + WM_NCLBUTTONDOWN(HTCAPTION)。
                                    #[cfg(target_os = "windows")]
                                    {
                                        use raw_window_handle::{
                                            HasWindowHandle, RawWindowHandle,
                                        };
                                        use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
                                        use windows_sys::Win32::UI::WindowsAndMessaging::{
                                            SendMessageW, HTCAPTION, WM_NCLBUTTONDOWN,
                                        };
                                        if let Ok(handle) = window.window_handle() {
                                            if let RawWindowHandle::Win32(win) = handle.as_raw() {
                                                unsafe {
                                                    let hwnd = win.hwnd.get()
                                                        as *mut core::ffi::c_void;
                                                    ReleaseCapture();
                                                    SendMessageW(
                                                        hwnd,
                                                        WM_NCLBUTTONDOWN,
                                                        HTCAPTION as usize,
                                                        0,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    #[cfg(not(target_os = "windows"))]
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
                            [action, above_atom, 0, 1, 0],
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
                            [2, h, v, 1, 0], // 2=Toggle
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

/// 主线程 → GPUI 线程的命令
enum OverlayCommand {
    /// 打开截图覆盖窗口；`reply` 由 OverlayView::commit 发回结果
    Capture {
        frame: CapturedFrame,
        screen_bounds: ub::Bounds,
        reply: Sender<OverlayResult>,
    },
    /// 在同一个 GPUI 应用里打开 Pin 窗口
    OpenPin(PinPayload),
    /// 打开 OCR 模型管理窗口（查看模型状态 / 重新下载 / 进度）
    OpenOcrModels,
    /// 打开/重用 OCR 识别窗口（左图右文，类似微信文字识别）：左侧选区图 + 右侧结果区
    OpenOcrPin(PinPayload),
    /// 更新当前 OCR 窗口的右侧文字（后台识别完成后调用）
    UpdateOcrPin(String),
    /// 打开滚动截屏进度小窗（cancel/progress 由主线程与 GPUI 线程共享原子）
    ShowProgress {
        cancel: Arc<AtomicBool>,
        /// 手动滚动模式下用户点「完成」置 true（自动模式传哑值，不使用）
        done: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
        /// 引擎每轮更新的「内容是否在动」标志：静止时才显示「完成」按钮
        moving: Arc<AtomicBool>,
        /// 引擎每轮更新的「最近一帧底部是否含内容」：点「完成」时据此弹确认
        bottom_has_content: Arc<AtomicBool>,
        /// 确认态标志（手动模式点「完成」后弹「可能没滚到底」确认时置 true）
        confirming: Arc<AtomicBool>,
        /// true = 手动滚动模式（进度窗显示「完成」按钮 + 手动提示文案）
        manual: bool,
        /// 选区物理像素（用于把小窗摆到不遮挡选区的位置）
        region_px: ub::Bounds,
        /// 主屏物理像素尺寸（换算逻辑坐标用）
        screen_px: ub::Bounds,
    },
    /// 关闭滚动截屏进度小窗
    HideProgress,
}

/// 进程级唯一的 GPUI 服务：持有命令 Sender，首次使用时才拉起 GPUI 线程。
///
/// 常驻单应用（`QuitMode::Explicit`）是修复「Windows 截图后进程被
/// gpui_windows 的 `ExitProcess(0)` 杀掉」的关键：截图/固定都在同一个
/// `application().run()` 内创建/销毁窗口，事件循环永不退出。
pub struct OverlayService {
    cmd: Sender<OverlayCommand>,
}

impl OverlayService {
    pub fn new() -> Self {
        Self {
            cmd: ensure_started(),
        }
    }

    /// 打开覆盖窗口并阻塞到用户完成/取消。取消时 selection=None。
    pub fn open_overlay(&self, frame: CapturedFrame, screen_bounds: ub::Bounds) -> OverlayResult {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let _ = self.cmd.send(OverlayCommand::Capture {
            frame,
            screen_bounds,
            reply: reply_tx,
        });
        // 主线程阻塞等结果；取消走显式 OverlayResult（selection=None，见 commit）。
        // 应用退出时 reply Sender drop → recv 返回 Err → 同样视为取消。
        let result = reply_rx.recv().unwrap_or(OverlayResult {
            selection: None,
            commands: vec![],
            no_clipboard: false,
            pin: None,
            scroll_region_px: None,
            scroll_manual: false,
            frame: None,
        });

        result
    }

    /// 在同一个 GPUI 应用里打开 Pin 窗口（fire-and-forget）。
    pub fn open_pin(&self, payload: PinPayload) {
        let _ = self.cmd.send(OverlayCommand::OpenPin(payload));
    }

    /// 打开 OCR 模型管理窗口（fire-and-forget）。
    pub fn open_ocr_models(&self) {
        let _ = self.cmd.send(OverlayCommand::OpenOcrModels);
    }

    /// 打开自动滚动截屏进度小窗（主线程调用，不阻塞）
    pub fn open_scroll_progress(
        &self,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
        region_px: ub::Bounds,
        screen_px: ub::Bounds,
    ) {
        let _ = self.cmd.send(OverlayCommand::ShowProgress {
            cancel,
            done: Arc::new(AtomicBool::new(false)),
            progress,
            moving: Arc::new(AtomicBool::new(false)),
            bottom_has_content: Arc::new(AtomicBool::new(false)),
            confirming: Arc::new(AtomicBool::new(false)),
            manual: false,
            region_px,
            screen_px,
        });
    }

    /// 打开手动滚动截屏进度小窗（主线程调用，不阻塞）
    ///
    /// `done` 由用户点「完成」置 true，主线程据此结束拼接；
    /// `moving` 由引擎每轮更新，进度窗只在静止时显示「完成」按钮；
    /// `bottom_has_content` / `confirming` 见 `ScrollProgress::show_manual` 注释。
    #[allow(clippy::too_many_arguments)]
    pub fn open_manual_scroll_progress(
        &self,
        done: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
        region_px: ub::Bounds,
        screen_px: ub::Bounds,
        moving: Arc<AtomicBool>,
        bottom_has_content: Arc<AtomicBool>,
        confirming: Arc<AtomicBool>,
    ) {
        let _ = self.cmd.send(OverlayCommand::ShowProgress {
            cancel,
            done,
            progress,
            moving,
            bottom_has_content,
            confirming,
            manual: true,
            region_px,
            screen_px,
        });
    }

    /// 关闭滚动截屏进度小窗
    pub fn close_scroll_progress(&self) {
        let _ = self.cmd.send(OverlayCommand::HideProgress);
    }
}

/// 拉起唯一的 GPUI 线程并返回命令通道（OnceLock 保证全局只启动一次）。
fn ensure_started() -> Sender<OverlayCommand> {
    static SERVICE: OnceLock<Sender<OverlayCommand>> = OnceLock::new();
    SERVICE
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("gpui-overlay".to_string())
                .spawn(move || run_overlay_app(rx))
                .expect("failed to spawn gpui overlay thread");
            tx
        })
        .clone()
}

/// 常驻 GPUI 应用线程：跑一个 `QuitMode::Explicit` 的应用，命令循环在
/// 应用内打开/关闭窗口，事件循环永不退出（除非进程退出）。
fn run_overlay_app(rx: Receiver<OverlayCommand>) {
    // 注册 gpui-component-assets 提供默认 Lucide 图标 svg 资源。
    // 不调用时 IconName::XXX 渲染会找不到 svg、按钮看不出图标。
    application()
        .with_assets(gpui_component_assets::Assets)
        // QuitMode::Explicit：窗口关闭不自动退出；只有显式 cx.quit() 才结束
        // 事件循环。这是避免 gpui_windows::WindowsPlatform::run 末尾
        // ExitProcess(0) 杀进程的关键。
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            // gpui-component 必须在第一个窗口前初始化，否则全局主题/状态会 panic
            gpui_component::init(cx);

            // 把内置 Noto Sans CJK SC Regular/Bold 注册进 GPUI text system，
            // 这样预览文字用 family="Noto Sans CJK SC" + weight=BOLD 时能命中
            // Bold face（与提交栅格化一致），而不是退化成系统字体/普通字重。
            if let Err(err) = cx.text_system().add_fonts(vec![
                std::borrow::Cow::Owned(crate::overlay::drawing::FontWeight::Normal.font_bytes().to_vec()),
                std::borrow::Cow::Owned(crate::overlay::drawing::FontWeight::Bold.font_bytes().to_vec()),
            ]) {
                eprintln!("[overlay] register Noto fonts failed: {err}");
            }

            cx.spawn(async move |async_cx: &mut AsyncApp| {
                // 启动时创建常驻覆盖窗口（停靠态：X11 unmap 不可见、不抢焦点、
                // 不挡输入）。窗口创建会同步编译整套 wgpu shader pipeline（约
                // 0.5s）——这正是每次截图「开窗」的成本。窗口常驻后，每次截图
                // 只是放大 + 换帧 + map 唤醒，不再重新编译 pipeline（见
                // reuse_overlay_window），后续截图的窗口开销接近 0。
                let mut overlay: Option<OverlayWindowSlot> = async_cx.update(open_parked_overlay);

                let mut progress: Option<WindowHandle<ProgressView>> = None;
                let mut ocr_pin: Option<WindowHandle<gpui_component::Root>> = None;
                let mut ocr_models: Option<WindowHandle<gpui_component::Root>> = None;
                loop {
                    match rx.try_recv() {
                        Ok(OverlayCommand::Capture { frame, screen_bounds, reply }) => {
                            async_cx.update(|cx| {
                                let t0 = std::time::Instant::now();
                                // 显示尺寸变化（多显示器/分辨率切换，罕见）时重建窗口
                                if let Some(slot) = &overlay {
                                    let target = overlay_target_size(cx, &screen_bounds);
                                    if slot.target != target {
                                        tracing::info!(
                                            "[overlay] 显示尺寸变化（{:?} → {:?}），重建覆盖窗口",
                                            slot.target,
                                            target
                                        );
                                        let _ = slot.window.update(cx, |_, w, _| w.remove_window());
                                        overlay = None;
                                    }
                                }
                                if let Some(slot) = &overlay {
                                    reuse_overlay_window(&slot.window, frame, screen_bounds, reply, cx);
                                    tracing::info!(
                                        "[overlay] window reuse took {:.0}ms",
                                        t0.elapsed().as_millis()
                                    );
                                } else {
                                    overlay = open_parked_overlay(cx);
                                    if let Some(slot) = &overlay {
                                        reuse_overlay_window(&slot.window, frame, screen_bounds, reply, cx);
                                        tracing::info!(
                                            "[overlay] window open took {:.0}ms",
                                            t0.elapsed().as_millis()
                                        );
                                    } else {
                                        // 创建失败（极端情况）：回一个取消结果，避免主线程永久阻塞
                                        let _ = reply.send(OverlayResult {
                                            selection: None,
                                            commands: vec![],
                                            no_clipboard: false,
                                            pin: None,
                                            scroll_region_px: None,
                                            scroll_manual: false,
                                            frame: Some(frame),
                                        });
                                    }
                                }
                            });
                        }
                        Ok(OverlayCommand::OpenPin(payload)) => {
                            let _ = async_cx.update(|cx| open_pin_in_app(payload, cx));
                        }
                        Ok(OverlayCommand::OpenOcrModels) => {
                            // 始终只有一个 OCR 模型窗口：已存在则聚焦到前台，不重复开
                            let alive = if let Some(h) = &ocr_models {
                                h.update(async_cx, |_, window, _| {
                                    window.activate_window();
                                    true
                                })
                                .unwrap_or(false)
                            } else {
                                false
                            };
                            if !alive {
                                match async_cx.update(open_ocr_models_in_app) {
                                    Ok(h) => ocr_models = Some(h),
                                    Err(e) => tracing::error!("[overlay] 打开 OCR 模型窗口失败: {e}"),
                                }
                            }
                        }
                        Ok(OverlayCommand::OpenOcrPin(payload)) => {
                            // 多次 OCR 只保留一个窗口：关掉旧的，开新的（左图右文）
                            if let Some(old) = ocr_pin.take() {
                                let _ = old.update(async_cx, |_, window, _| window.remove_window());
                            }
                            match async_cx.update(|cx| open_ocr_pin_in_app(payload, cx)) {
                                Ok(handle) => ocr_pin = Some(handle),
                                Err(e) => tracing::error!("[overlay] open ocr pin window failed: {e}"),
                            }
                        }
                        Ok(OverlayCommand::UpdateOcrPin(text)) => {
                            if let Some(handle) = &ocr_pin {
                                let _ = handle.update(async_cx, |root, _, cx| {
                                    // Root 包裹后 downcast 访问 OcrPinView
                                    if let Ok(view) = root.view().clone().downcast::<OcrPinView>() {
                                        view.update(cx, |view, cx| {
                                            if text.is_empty() {
                                                view.text = None;
                                                view.text_state = None;
                                            } else {
                                                view.text = Some(text.clone());
                                                // 重建 TextViewState（markdown 解析），支持选中/复制/全选
                                                view.text_state = Some(cx.new(|cx| {
                                                    let md = format!("```text\n{}\n```", text);
                                                    gpui_component::text::TextViewState::markdown(&md, cx)
                                                }));
                                            }
                                            cx.notify();
                                        });
                                    }
                                });
                            }
                        }
                        Ok(OverlayCommand::ShowProgress { cancel, done, progress: progress_arc, moving, bottom_has_content, confirming, manual, region_px, screen_px }) => {
                            // 先关掉可能残留的旧进度窗
                            if let Some(old) = progress.take() {
                                let _ = old.update(async_cx, |_, window, _| window.remove_window());
                            }
                            match async_cx.update(|cx| {
                                open_progress_window(cancel, done, progress_arc, moving, bottom_has_content, confirming, manual, region_px, screen_px, cx)
                            }) {
                                Ok(handle) => progress = Some(handle),
                                Err(e) => eprintln!("[overlay] open progress window failed: {e}"),
                            }
                        }
                        Ok(OverlayCommand::HideProgress) => {
                            if let Some(handle) = progress.take() {
                                let _ = handle.update(async_cx, |_, window, _| window.remove_window());
                            }
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => break,
                    }
                    // 滚动进度窗：每 tick 从原子读最新高度重绘
                    if let Some(handle) = &progress {
                        let _ = handle.update(async_cx, |_, _, cx| cx.notify());
                    }
                    async_cx
                        .background_executor()
                        .timer(std::time::Duration::from_millis(5))
                        .await;
                }
            })
            .detach();
        });
}

/// 常驻覆盖窗口的句柄 + 上次会话的目标窗口尺寸（用于检测显示尺寸变化）
struct OverlayWindowSlot {
    window: WindowHandle<gpui_component::Root>,
    /// 上次会话的目标窗口尺寸（逻辑像素）
    target: (f32, f32),
}

/// 启动时创建常驻覆盖窗口（停靠态）。
///
/// 窗口创建会同步编译整套 wgpu shader pipeline（约 0.5s）——这正是每次截图
/// 「开窗」要付的成本。把窗口常驻：创建一次，之后每次截图只是 unmap→map
/// 放大 + 换帧，不再重新编译 pipeline。创建后立即 unmap 停靠（不可见、不抢
/// 焦点、不挡输入），首个会话由 `reuse_overlay_window` 唤醒。
fn open_parked_overlay(cx: &mut App) -> Option<OverlayWindowSlot> {
    let t0 = std::time::Instant::now();
    // 占位帧：停靠态不显示任何内容；用全透明像素，即使创建后到 unmap 之间
    // 渲染了一帧也完全不可见（不透明黑会被拉伸成全屏黑屏闪现）。
    let placeholder = CapturedFrame { width: 1, height: 1, pixels: vec![0, 0, 0, 0] };
    // 停靠态不会有会话，用一个无人接收的 channel 占位；会话开始时被替换
    let (tx, _rx) = std::sync::mpsc::channel::<OverlayResult>();
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(1.0), px(1.0)),
    });
    let win_w = f32::from(display_bounds.size.width).max(1.0);
    let win_h = f32::from(display_bounds.size.height).max(1.0);
    // X11 -2px 修正（见 open_overlay_in_app 注释）：位置在创建时定死，之后
    // 不能移动，所以必须在这里就按主显示原点修正。
    #[cfg(target_os = "linux")]
    let origin_x = f32::from(display_bounds.origin.x) - 2.0;
    #[cfg(not(target_os = "linux"))]
    let origin_x = f32::from(display_bounds.origin.x);

    // 窗口直接按全屏尺寸创建（不先建 1×1 再放大）：park 只 unmap、reuse 只
    // map，窗口尺寸从头到尾不变，gpui 的 bounds 一直正确，避免复用路径里
    // resize 后 ConfigureNotify 异步到达导致的首帧错位。
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(origin_x), display_bounds.origin.y),
                size: Size::new(px(win_w), px(win_h)),
            })),
            window_background: WindowBackgroundAppearance::Transparent,
            titlebar: None,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            focus: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| {
                OverlayView::new(
                    placeholder,
                    None,
                    ub::Bounds::new(ub::Point::ZERO, ub::Point::new(win_w, win_h)),
                    ub::Point::ZERO,
                    1.0,
                    tx,
                    cx,
                )
            });
            cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
        },
    );
    match result {
        Ok(handle) => {
            // 立即停靠（unmap）：避免全屏透明窗口在屏幕上闪现/挡点击
            #[cfg(target_os = "linux")]
            let _ = handle.update(cx, |_, window, _| park_overlay_window(window));
            tracing::info!(
                "[overlay] 覆盖窗口已创建并停靠（pipeline 编译 {:.0}ms）",
                t0.elapsed().as_millis()
            );
            Some(OverlayWindowSlot {
                window: handle,
                target: (win_w, win_h),
            })
        }
        Err(e) => {
            tracing::warn!("[overlay] 覆盖窗口创建失败（将退化为每次新建）：{e}");
            None
        }
    }
}

/// 覆盖窗口的目标尺寸（逻辑像素）：主显示 bounds。
fn overlay_target_size(cx: &App, screen_bounds: &ub::Bounds) -> (f32, f32) {
    let wb = cx.primary_display().map(|d| d.bounds()).unwrap_or(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(screen_bounds.size.x), px(screen_bounds.size.y)),
    });
    (f32::from(wb.size.width), f32::from(wb.size.height))
}

/// 复用常驻覆盖窗口开始一次截图会话（替代「每次新建窗口」）。
///
/// 窗口与 WgpuRenderer 保持存活，pipeline 已在启动时编译好；这里只做四件事：
/// 1) `start_session` 换帧 + 重置交互状态；
/// 2) 放大到全屏；
/// 3) X11 下 map + 置顶唤醒；
/// 4) 聚焦 + 激活。
///
/// 整段耗时远低于重新建窗（~570ms 的 pipeline 编译没了）。
#[allow(clippy::too_many_arguments)]
fn reuse_overlay_window(
    overlay_window: &WindowHandle<gpui_component::Root>,
    frame: CapturedFrame,
    screen_bounds: ub::Bounds,
    reply: Sender<OverlayResult>,
    cx: &mut App,
) {
    // —— 与 open_overlay_in_app 相同的窗口/裁剪参数计算 ——
    let win_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(screen_bounds.size.x), px(screen_bounds.size.y)),
    });
    // 窗口客户端原点的屏幕位置：取显示区域原点（与 open_overlay_in_app 一致）
    let client_origin =
        ub::Point::new(f32::from(win_bounds.origin.x), f32::from(win_bounds.origin.y));
    let actual_w = f32::from(win_bounds.size.width).max(1.0);
    let actual_h = f32::from(win_bounds.size.height).max(1.0);
    let sx = screen_bounds.size.x / actual_w;
    let sy = screen_bounds.size.y / actual_h;
    let src_x = (f32::from(win_bounds.origin.x) * sx) as u32;
    let src_y = (f32::from(win_bounds.origin.y) * sy) as u32;
    let clip_w = ((actual_w * sx) as u32).min(frame.width.saturating_sub(src_x));
    let clip_h = ((actual_h * sy) as u32).min(frame.height.saturating_sub(src_y));
    let fullscreen = src_x == 0 && src_y == 0 && clip_w == frame.width && clip_h == frame.height;
    let (display, original) = if fullscreen {
        (frame, None)
    } else {
        match frame.clip_region(src_x, src_y, clip_w, clip_h) {
            Ok(clipped) => (clipped, Some(frame)),
            Err(_) => (frame, None),
        }
    };
    let scale = display.width as f32 / actual_w;
    let logical_bounds = ub::Bounds::new(ub::Point::ZERO, ub::Point::new(actual_w, actual_h));

    let _ = overlay_window.update(cx, |root, window, cx| {
        let t_upd = std::time::Instant::now();
        let view = root
            .view()
            .clone()
            .downcast::<OverlayView>()
            .expect("覆盖窗口的根视图应是 OverlayView");
        // 1) 换帧 + 重置状态（此时窗口仍 unmap，不会闪现旧内容）
        view.update(cx, |view, view_cx| {
            view.start_session(
                display,
                original,
                logical_bounds,
                client_origin,
                scale,
                reply,
                view_cx,
            );
        });
        let t_session = std::time::Instant::now();
        // 清除残留的 tooltip 浮层：窗口复用后 TooltipOverlay.content 跨会话
        // 残留（Esc 关窗时鼠标未离开按钮，无 mouse_exited/on_mouse_down 事件
        // 触发隐藏），不清除会在下次会话一开始就显示旧浮层。
        // 注意：不能经 Root::tooltip_overlay 清除——本闭包内 root 实体正被
        // update 借用，再读会 panic；Root::hide_tooltip 内部直接更新
        // TooltipOverlay 实体，不触碰 root 自身。
        root.hide_tooltip(window, cx);
        // 2) 放大到全屏：X11 平台窗口尺寸从头到尾不变（park 只 unmap），无需
        //    resize——gpui 的 bounds 一直正确，map 后即为最终尺寸；非 X11
        //    平台 park 时缩成了 1×1，需要恢复。
        #[cfg(not(target_os = "linux"))]
        window.resize(Size::new(px(actual_w), px(actual_h)));
        // 3) 唤醒：map + 置顶
        #[cfg(target_os = "linux")]
        unpark_overlay_window(window);
        let t_unpark = std::time::Instant::now();
        // 4) 聚焦 + 激活（raise）
        let fh = view.read(cx).focus_handle.clone();
        fh.focus(window, cx);
        window.activate_window();
        let t_activate = std::time::Instant::now();
        tracing::info!(
            "[overlay] reuse 分段: downcast+start_session={:.0}ms unpark={:.0}ms focus+activate={:.0}ms",
            t_session.duration_since(t_upd).as_millis(),
            t_unpark.duration_since(t_session).as_millis(),
            t_activate.duration_since(t_unpark).as_millis()
        );
    });
}

/// 停靠覆盖窗口：X11 下直接 unmap（不可见、不挡输入、X 服务器自动释放键盘
/// 焦点），窗口与渲染器保持存活，下次截图由 `unpark_overlay_window` 唤醒。
/// GPUI 后端对 UnmapNotify 只更新内部 is_mapped 标志、不会销毁窗口，因此
/// 绕过 GPUI 的 unmap 是安全的。
#[cfg(target_os = "linux")]
fn park_overlay_window(window: &mut Window) {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    use x11rb::xcb_ffi::XCBConnection;

    if let (Ok(wh), Ok(dh)) = (window.window_handle(), window.display_handle()) {
        if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
            (wh.as_raw(), dh.as_raw())
        {
            if let Some(conn_ptr) = xcb_dh.connection {
                if let Ok(conn) = unsafe {
                    XCBConnection::from_raw_xcb_connection(conn_ptr.as_ptr().cast(), false)
                } {
                    let _ = conn.unmap_window(xcb_wh.window.into());
                    let _ = conn.flush();
                }
            }
        }
    }
}

/// 非 X11 平台退化为 1×1 缩窗停靠（无 unmap 原语；窗口保持 1×1 时几乎不可见）。
/// Windows 下 1×1 窗口仍持有键盘焦点会吞掉用户后续按键，主动 SetFocus(NULL)
/// 交还焦点（X11 走 unmap，由 X 服务器自动释放，无需此步）。
#[cfg(not(target_os = "linux"))]
fn park_overlay_window(window: &mut Window) {
    window.resize(Size::new(px(1.0), px(1.0)));
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        let _ = unsafe { SetFocus(std::ptr::null_mut()) };
    }
}

/// 唤醒覆盖窗口：X11 下 map + 置顶。调用方应在 map 前完成 resize（避免
/// 闪现 1×1 过渡帧）；焦点由调用方随后通过 gpui focus/activate 设置。
#[cfg(target_os = "linux")]
fn unpark_overlay_window(window: &mut Window) {
    use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, StackMode};
    use x11rb::xcb_ffi::XCBConnection;

    if let (Ok(wh), Ok(dh)) = (window.window_handle(), window.display_handle()) {
        if let (RawWindowHandle::Xcb(xcb_wh), RawDisplayHandle::Xcb(xcb_dh)) =
            (wh.as_raw(), dh.as_raw())
        {
            if let Some(conn_ptr) = xcb_dh.connection {
                if let Ok(conn) = unsafe {
                    XCBConnection::from_raw_xcb_connection(conn_ptr.as_ptr().cast(), false)
                } {
                    let _ = conn.map_window(xcb_wh.window.into());
                    let _ = conn.configure_window(
                        xcb_wh.window.into(),
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    );
                    let _ = conn.flush();
                }
            }
        }
    }
}

/// 非 X11 平台：窗口从未 unmap，无需唤醒
#[cfg(not(target_os = "linux"))]
fn unpark_overlay_window(_window: &mut Window) {}

/// 滚动截屏进度小窗视图（auto/manual 共用；manual 显示「完成」按钮）
struct ProgressView {
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    /// 引擎每轮更新的「内容是否在动」标志：静止时才显示「完成」按钮
    moving: Arc<AtomicBool>,
    /// 引擎每轮更新的「最近一帧底部是否含内容」：点「完成」时据此弹确认
    bottom_has_content: Arc<AtomicBool>,
    /// 确认态标志：点「完成」且底部有内容时置 true，弹「可能没滚到底」确认
    confirming: Arc<AtomicBool>,
    manual: bool,
}

impl Render for ProgressView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let height = self.progress.load(Ordering::Relaxed);
        let confirming_now = self.manual && self.confirming.load(Ordering::Relaxed);
        let text = if confirming_now {
            // 确认态：提示用户可能还没滚到底
            "底部可能还有内容？".to_string()
        } else if self.manual {
            format!("手动滚动截屏中… {height}px")
        } else {
            format!("滚动截屏中… {height}px")
        };
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(px(44.0))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .child(div().text_sm().child(text)),
            )
            .when(confirming_now, {
                let confirming = self.confirming.clone();
                let done = self.done.clone();
                // 确认态：继续滚动 / 确定结束
                move |b| {
                    b.child(
                        Button::new("scroll-continue")
                            .label("继续滚动")
                            .compact()
                            .with_size(gpui_component::Size::Small)
                            .on_click(move |_, _, _| confirming.store(false, Ordering::Relaxed)),
                    )
                    .child(
                        Button::new("scroll-confirm-done")
                            .label("确定结束")
                            .compact()
                            .with_size(gpui_component::Size::Small)
                            .on_click(move |_, _, _| done.store(true, Ordering::Relaxed)),
                    )
                }
            })
            // 手动模式：内容静止时才显示「完成」按钮（滚动动画中途不可点，
            // 避免最后一段还没拼进去就结束）。点「完成」时若最近一帧底部还有
            // 内容，先弹确认——很可能页面还没滚到底，直接结束会拼接缺底。
            .when(
                self.manual && !confirming_now && !self.moving.load(Ordering::Relaxed),
                {
                    let done = self.done.clone();
                    let confirming = self.confirming.clone();
                    let bottom_has_content = self.bottom_has_content.clone();
                    move |b| {
                        b.child(
                            Button::new("scroll-done")
                                .label("完成")
                                .compact()
                                .with_size(gpui_component::Size::Small)
                                .on_click(move |_, _, _| {
                                    if bottom_has_content.load(Ordering::Relaxed) {
                                        confirming.store(true, Ordering::Relaxed);
                                    } else {
                                        done.store(true, Ordering::Relaxed);
                                    }
                                }),
                        )
                    }
                },
            )
            .child({
                let cancel = self.cancel.clone();
                Button::new("scroll-cancel")
                    .label("取消")
                    .compact()
                    .with_size(gpui_component::Size::Small)
                    .on_click(move |_, _, _| cancel.store(true, Ordering::Relaxed))
            })
    }
}

/// 两个逻辑像素矩形是否相交（用于把进度窗摆到不遮挡选区的角落）
fn bounds_intersect(a: ub::Bounds, b: ub::Bounds) -> bool {
    a.origin.x < b.origin.x + b.size.x
        && a.origin.x + a.size.x > b.origin.x
        && a.origin.y < b.origin.y + b.size.y
        && a.origin.y + a.size.y > b.origin.y
}

/// 打开滚动截屏进度小窗，摆到不与选区重叠的屏幕角落
#[allow(clippy::too_many_arguments)]
fn open_progress_window(
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    moving: Arc<AtomicBool>,
    bottom_has_content: Arc<AtomicBool>,
    confirming: Arc<AtomicBool>,
    manual: bool,
    region_px: ub::Bounds,
    screen_px: ub::Bounds,
    cx: &mut App,
) -> AppResult<WindowHandle<ProgressView>> {
    let display = cx
        .primary_display()
        .ok_or_else(|| AppError::Gpui("no primary display".into()))?;
    let dbounds = display.bounds();

    // 物理像素 → 逻辑像素（选区与主屏都是物理像素，GPUI 用逻辑坐标）
    let sx = f32::from(dbounds.size.width) / screen_px.size.x.max(1.0);
    let sy = f32::from(dbounds.size.height) / screen_px.size.y.max(1.0);

    // 手动模式多一个「完成」按钮，窗口加宽
    let win_w = if manual { 360.0 } else { 280.0 };
    const WIN_H: f32 = 44.0;
    let dw = f32::from(dbounds.size.width);
    let dh = f32::from(dbounds.size.height);
    // 兜底角落：右下、左下、右上、左上
    let corners = [
        point(px(dw - win_w), px(dh - WIN_H)),
        point(px(0.0), px(dh - WIN_H)),
        point(px(dw - win_w), px(0.0)),
        point(px(0.0), px(0.0)),
    ];
    let region_logical = ub::Bounds {
        origin: ub::Point::new(region_px.origin.x * sx, region_px.origin.y * sy),
        size: ub::Point::new(region_px.size.x * sx, region_px.size.y * sy),
    };
    // 优先把进度窗放到选区旁边，指针可及、且不污染截图；候选位越界/与选区相交
    // 就跳过，最后回退到角落。手动模式的「完成」按钮在窗内，用户要滚动+点完成，
    // 窗口优先放选区**右侧**（贴近页面右边，用户停下即可点）；自动模式保持
    // 下方→上方→右侧→左侧（滚动方向正下方离指针最近）。
    let clamp_x = |x: f32| x.clamp(0.0, (dw - win_w).max(0.0));
    let clamp_y = |y: f32| y.clamp(0.0, (dh - WIN_H).max(0.0));
    // 进度窗可视内容比请求位置偏出约几像素（GPUI 窗口框偏移），候选位需与选区
    // 留出边距，否则窗口边缘会压进选区底部，每一帧都带一条白条。
    const MARGIN: f32 = 12.0;
    let candidates = if manual {
        [
            // 右侧
            (
                clamp_x(region_logical.origin.x + region_logical.size.x + MARGIN),
                clamp_y(region_logical.origin.y + (region_logical.size.y - WIN_H) / 2.0),
            ),
            // 下方（滚动方向正下方，指针最近）
            (
                clamp_x(region_logical.origin.x + (region_logical.size.x - win_w) / 2.0),
                clamp_y(region_logical.origin.y + region_logical.size.y + MARGIN),
            ),
            // 上方
            (
                clamp_x(region_logical.origin.x + (region_logical.size.x - win_w) / 2.0),
                clamp_y(region_logical.origin.y - WIN_H - MARGIN),
            ),
            // 左侧
            (
                clamp_x(region_logical.origin.x - win_w - MARGIN),
                clamp_y(region_logical.origin.y + (region_logical.size.y - WIN_H) / 2.0),
            ),
        ]
    } else {
        [
            // 下方（滚动方向正下方，指针最近）
            (
                clamp_x(region_logical.origin.x + (region_logical.size.x - win_w) / 2.0),
                clamp_y(region_logical.origin.y + region_logical.size.y + MARGIN),
            ),
            // 上方
            (
                clamp_x(region_logical.origin.x + (region_logical.size.x - win_w) / 2.0),
                clamp_y(region_logical.origin.y - WIN_H - MARGIN),
            ),
            // 右侧
            (
                clamp_x(region_logical.origin.x + region_logical.size.x + MARGIN),
                clamp_y(region_logical.origin.y + (region_logical.size.y - WIN_H) / 2.0),
            ),
            // 左侧
            (
                clamp_x(region_logical.origin.x - win_w - MARGIN),
                clamp_y(region_logical.origin.y + (region_logical.size.y - WIN_H) / 2.0),
            ),
        ]
    };
    let origin = candidates
        .into_iter()
        .find(|(x, y)| {
            !bounds_intersect(
                ub::Bounds {
                    origin: ub::Point::new(*x, *y),
                    size: ub::Point::new(win_w, WIN_H),
                },
                region_logical,
            )
        })
        .map(|(x, y)| point(px(x), px(y)))
        .unwrap_or_else(|| corners[0]);

    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: Size::new(px(win_w), px(WIN_H)),
            })),
            window_background: WindowBackgroundAppearance::Opaque,
            titlebar: None,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            focus: false,
            ..Default::default()
        },
        |_, cx| cx.new(|_| ProgressView { cancel, done, progress, moving, bottom_has_content, confirming, manual }),
    )
    .map_err(|e| AppError::Gpui(format!("打开进度窗失败: {e}")))
}

/// 在常驻应用里打开 Pin 窗口（原 `spawn_pin_window` 的窗口构建逻辑）。
///
/// 不再新建 `application().run()`——Pin 窗口与覆盖窗口共用一个常驻应用，
/// 避免 Windows 上第二个并发 GPUI app 与 `ExitProcess(0)` 冲突。
fn open_pin_in_app(payload: PinPayload, cx: &mut App) {
    let PinPayload { frame: pin_frame, origin_x, origin_y, sx, sy } = payload;

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
        "[Pin] open window: origin=({:.0},{:.0}) img_logical={:.1}x{:.1} img_physical={}x{} win_size={:.1}x{:.1} scale={:.2}",
        origin_x, origin_y,
        img_w, img_h,
        pin_frame.width, pin_frame.height,
        win_w, win_h, scale
    );

    let target_x = origin_x;
    // 窗口上移标题栏高度，使图片内容与原始选区位置对齐。
    // 图片实际渲染在 client y = 边框1px + 标题栏32px = 33 处。
    // Windows 上 GPUI 的 calculate_window_rect 假设边框对称（height_offset/2=4），
    // 但这类窗口实际顶部边框为 0，导致客户端被放高 4px（ClientToScreen 实测）。
    // 因此 Windows 需要补偿：target_y = origin_y - 33 + 4 = origin_y - 29。
    // 图像实际渲染在 client y=33（边框1px + 标题栏32px）。窗口先按
    // target_y = origin_y - 32 请求，创建后由 `schedule_client_top_adjustment`
    // 延迟到 App 借期外动态校正客户端位置（见下），跨平台无需硬编码偏移量。
    let target_y = origin_y - CUSTOM_TITLEBAR_H;

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
            // 动态校正：图像在 client y=33，把客户端顶移到 origin_y - 33，
            // 使图像与选区对齐。跨平台无需硬编码系统栏高度/边框偏移。
            //
            // 不能在这里直接 SetWindowPos：会同步触发 WM_MOVE → gpui 的 on_moved
            // 回调重新进入 App，而 open_window 期间 App 仍被 update 借出，报
            // "RefCell already borrowed"（gpui_windows 的 restart 也有同款注释）。
            // 改为捕获 HWND 后延迟到 App 借期外执行。
            #[cfg(target_os = "windows")]
            schedule_client_top_adjustment(
                cx,
                window_hwnd(window),
                (origin_y - 33.0) as i32,
            );

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
                                            target_y as i32,
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
                                            target_x, target_y
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

                                    let adjusted_y = target_y as i32 - frame_extent_top as i32;
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

            let view = cx.new(|cx| PinWindowView::new(pin_frame, cx));
            let handle = view.read(cx).focus_handle.clone();
            handle.focus(window, cx);
            view
        },
    )
    .expect("open pin window failed");
}

// ---------------------------------------------------------------------------
// OCR 模型管理窗口：查看本地模型状态 / 远程地址 / 重新下载 / 下载进度
// ---------------------------------------------------------------------------

/// OCR 模型管理视图：每次 render 从 `paddle::model_snapshot()` 拉最新状态，
/// 下载期间由打开处的定时器驱动重绘。
pub struct OcrModelsView {
    focus_handle: FocusHandle,
    /// 自身弱引用：按钮回调（只拿 &mut App）用它更新视图并触发重绘
    weak: WeakEntity<Self>,
    /// 最近一次操作失败的提示（档位, 原因）；None=无错误
    activation_error: Option<(String, String)>,
}

impl OcrModelsView {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            weak: cx.entity().downgrade(),
            activation_error: None,
        }
    }
}

impl Render for OcrModelsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use crate::ocr::paddle::{FileStatus, ModelSnapshot};
        let snap: ModelSnapshot = crate::ocr::paddle::model_snapshot();
        let downloading = snap.downloading;
        let downloading_tier = snap.downloading_tier.clone();
        let batch_download = snap.batch_download;
        let current_file = snap.current_file.clone();
        let (done, total) = snap.progress;
        let pct = total
            .filter(|t| *t > 0)
            .map(|t| (done as f64 / t as f64 * 100.0).min(100.0));
        let cache_dir = snap.cache_dir.display().to_string();
        let last_download = snap.last_download.clone();

        // 每个档位一个区块：档位头（radio 切换 + 名称 + 说明 + 重新下载按钮）+ 三文件行
        let tier_blocks = snap.tiers.iter().map(|t| {
            let tier = t.tier.clone();
            let selected = t.selected;
            let note = t.note.clone();
            // 只有「本档整档下载中」才置按钮为下载中；其他档位按钮保持可点
            let busy = downloading
                && batch_download
                && downloading_tier.as_deref() == Some(tier.as_str());
            // 本档三件套是否全部就绪：决定整档按钮显示「重新下载」还是「批量下载」
            let all_ready = t.files.iter().all(|f| matches!(f.status, FileStatus::Ready));
            // file_rows 闭包持有档位名/弱引用/下载状态的独立副本，避免与外层借用冲突
            let file_rows_tier = tier.clone();
            let file_rows_weak = self.weak.clone();
            let file_rows_current_file = current_file.clone();
            let file_rows_downloading_tier = downloading_tier.clone();
            let file_rows_batch = batch_download;
            let file_rows = t.files.iter().map(move |f| {
                // 正在下载的文件行状态置为「下载中…」
                let (mark, mark_color) = if downloading
                    && file_rows_current_file.as_deref() == Some(f.name)
                {
                    ("下载中…", gpui::rgba(0x42A5F5FF))
                } else {
                    match &f.status {
                        FileStatus::Ready => ("✓ 已存在", gpui::rgba(0x4CAF50FF)),
                        FileStatus::Missing => ("未下载", gpui::rgba(0x9E9E9EFF)),
                        FileStatus::Downloading => ("下载中…", gpui::rgba(0x42A5F5FF)),
                        FileStatus::Error(_) => ("失败", gpui::rgba(0xEF5350FF)),
                    }
                };
                let size_text = f
                    .size
                    .map(|s| format!("{:.1} MB", s as f64 / 1048576.0))
                    .unwrap_or_else(|| "-".into());
                let path_text = f
                    .local_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "（本地无此文件）".into());
                let url = f.url.clone();
                let tier_for_btn = file_rows_tier.clone();
                let name_for_btn = f.name.to_string();
                // 单文件按钮状态：只有正在下载的这个按钮变「下载中…」，其他保持原样。
                // 未下载=「下载」；已存在=「重新下载」。
                let file_busy = downloading
                    && !file_rows_batch
                    && file_rows_downloading_tier.as_deref() == Some(file_rows_tier.as_str())
                    && file_rows_current_file.as_deref() == Some(f.name);
                let file_ready = matches!(f.status, FileStatus::Ready);
                let file_label = if file_busy {
                    "下载中…"
                } else if file_ready {
                    "重新下载"
                } else {
                    "下载"
                };
                let file_variant = if file_busy {
                    ButtonVariant::Default
                } else {
                    ButtonVariant::Info
                };
                div()
                    .flex_col()
                    .gap(px(1.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().text_color(mark_color).text_sm().child(gpui::SharedString::from(mark)))
                            .child(div().flex_1().text_sm().child(gpui::SharedString::from(f.name)))
                            .child(div().text_color(gpui::rgba(0x9E9E9EFF)).text_xs().child(gpui::SharedString::from(size_text)))
                            .child(
                                Button::new(format!("dl-file-{file_rows_tier}-{name_for_btn}"))
                                    .label(file_label)
                                    .with_variant(file_variant)
                                    .with_size(gpui_component::Size::XSmall)
                                    .disabled(file_busy)
                                    .on_click({
                                        let weak = file_rows_weak.clone();
                                        move |_, _, app| {
                                            let Some(entity) = weak.upgrade() else { return };
                                            entity.update(app, |this, cx| {
                                                match crate::ocr::paddle::start_download_file(
                                                    &tier_for_btn,
                                                    &name_for_btn,
                                                ) {
                                                    Ok(()) => {
                                                        this.activation_error = None;
                                                        tracing::info!(
                                                            "OCR: 开始下载文件 {name_for_btn}（{tier_for_btn}）"
                                                        );
                                                    }
                                                    Err(e) => {
                                                        tracing::error!(
                                                            "OCR: 启动下载失败: {e}"
                                                        );
                                                        this.activation_error =
                                                            Some((tier_for_btn.clone(), e));
                                                    }
                                                }
                                                cx.notify();
                                            });
                                        }
                                    }))
                            )
                    .child(
                        div()
                            .text_color(gpui::rgba(0x808080FF))
                            .text_xs()
                            .child(gpui::SharedString::from(path_text)),
                    )
                    .child(
                        div()
                            .text_color(gpui::rgba(0x5C6BC0FF))
                            .text_xs()
                            .child(gpui::SharedString::from(url)),
                    )
            });
            div()
                .flex_col()
                .gap(px(6.0))
                .p(px(10.0))
                .rounded_md()
                .border_1()
                .border_color(if selected {
                    gpui::rgba(0x42A5F5FF)
                } else {
                    gpui::rgba(0x2E2E2EFF)
                })
                // 档位头：名称 + 说明在前，激活 / 重新下载按钮都在行尾
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if selected {
                                    gpui::rgba(0x42A5F5FF)
                                } else {
                                    gpui::rgba(0xE6E6E6FF)
                                })
                                .child(gpui::SharedString::from(tier.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(gpui::rgba(0x9E9E9EFF))
                                .text_xs()
                                .child(gpui::SharedString::from(note)),
                        )
                        // 激活 / 已激活（绿色；当前档位灰色静态）
                        .child({
                            let tier = tier.clone();
                            if selected {
                                Button::new(format!("active-{tier}"))
                                    .label("✓ 已激活")
                                    .with_variant(ButtonVariant::Ghost)
                                    .with_size(gpui_component::Size::XSmall)
                                    .disabled(true)
                            } else {
                                Button::new(format!("active-{tier}"))
                                    .label("激活")
                                    .with_variant(ButtonVariant::Success)
                                    .with_size(gpui_component::Size::XSmall)
                                    .on_click({
                                    let weak = self.weak.clone();
                                    let tier = tier.clone();
                                    move |_, _, app| {
                                        let Some(entity) = weak.upgrade() else { return };
                                        entity.update(app, |this, cx| {
                                            match crate::ocr::paddle::set_tier(&tier) {
                                                Ok(()) => {
                                                    this.activation_error = None;
                                                    tracing::info!("OCR: 已激活档位 {tier}");
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        "OCR: 激活 {tier} 失败: {e}"
                                                    );
                                                    this.activation_error =
                                                        Some((tier.clone(), e));
                                                }
                                            }
                                            cx.notify();
                                        });
                                    }
                                })
                            }
                        })
                        // 整档按钮：全部存在=「重新下载」(全部重下)；有缺失=「批量下载」(只补缺失)；
                        // 本档整档下载中才变「下载中…」
                        .child(
                            Button::new(format!("dl-{tier}"))
                                .label(if busy {
                                    "下载中…"
                                } else if all_ready {
                                    "重新下载"
                                } else {
                                    "批量下载"
                                })
                                .with_variant(if busy {
                                    ButtonVariant::Default
                                } else {
                                    ButtonVariant::Info
                                })
                                .with_size(gpui_component::Size::XSmall)
                                .disabled(busy)
                                .on_click({
                                    let weak = self.weak.clone();
                                    let tier = tier.clone();
                                    move |_, _, app| {
                                        let Some(entity) = weak.upgrade() else { return };
                                        entity.update(app, |this, cx| {
                                            match crate::ocr::paddle::start_download(&tier) {
                                                Ok(()) => {
                                                    this.activation_error = None;
                                                    tracing::info!(
                                                        "OCR: 开始重新下载模型（{tier}）"
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        "OCR: 启动下载失败: {e}"
                                                    );
                                                    this.activation_error =
                                                        Some((tier.clone(), e));
                                                }
                                            }
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
                .child(div().flex_col().gap(px(4.0)).children(file_rows))
        });

        div()
            .id("ocr-models")
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgba(0x181818FF))
            .text_color(gpui::rgba(0xE6E6E6FF))
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .flex_1()
                    .overflow_y_scrollbar()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(12.0))
                    // 缓存目录说明
                    .child(
                        div()
                            .text_color(gpui::rgba(0x808080FF))
                            .text_xs()
                            .child(gpui::SharedString::from(format!(
                                "缓存目录：{cache_dir}（模型查找顺序：OCR_MODEL_DIR / 项目 models/PP-OCRv6 / 缓存）"
                            ))),
                    )
                    .children(tier_blocks)
                    // 下载进度 / 最近结果
                    // 激活失败提示（模型文件不齐全时显示）
                    .child(if let Some((t, msg)) = &self.activation_error {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(8.0))
                            .rounded_md()
                            .border_1()
                            .border_color(gpui::rgba(0xEF535088))
                            .bg(gpui::rgba(0xEF535020))
                            .child(
                                div()
                                    .text_color(gpui::rgba(0xEF5350FF))
                                    .text_xs()
                                    .child(gpui::SharedString::from(format!("{t}：{msg}"))),
                            )
                    } else {
                        div()
                    })
                    .child(if downloading {
                        let pct_text = match (done, total) {
                            (d, Some(t)) if t > 0 => format!(
                                "{:.0}%  {:.1}/{:.1} MB",
                                pct.unwrap_or(0.0),
                                d as f64 / 1048576.0,
                                t as f64 / 1048576.0
                            ),
                            (d, _) => format!("{:.1} MB", d as f64 / 1048576.0),
                        };
                        div()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_color(gpui::rgba(0x42A5F5FF))
                                    .text_sm()
                                    .child(gpui::SharedString::from(format!(
                                        "正在下载：{pct_text}"
                                    ))),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(gpui::rgba(0x2A2A2AFF))
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .w(px(480.0 * pct.unwrap_or(0.0) as f32 / 100.0))
                                            .bg(gpui::rgba(0x42A5F5FF)),
                                    ),
                            )
                    } else {
                        div().child(match &last_download {
                            Some(Ok(())) => div()
                                .text_color(gpui::rgba(0x4CAF50FF))
                                .text_sm()
                                .child("✓ 最近一次下载完成"),
                            Some(Err(e)) => div()
                                .text_color(gpui::rgba(0xEF5350FF))
                                .text_sm()
                                .child(gpui::SharedString::from(format!(
                                    "最近一次下载失败：{e}"
                                ))),
                            None => div()
                                .text_color(gpui::rgba(0x808080FF))
                                .text_xs()
                                .child("未执行过下载（切换档位或首次 OCR 时会自动下载缺失模型）"),
                        })
                    }),
            )
    }
}

/// 在常驻应用里打开 OCR 模型管理窗口（屏幕居中，fire-and-forget）。
fn open_ocr_models_in_app(cx: &mut App) -> AppResult<WindowHandle<gpui_component::Root>> {
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or_else(|| {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: Size::new(px(1280.0), px(800.0)),
        }
    });
    let win_w = 760.0_f32;
    let win_h = 660.0_f32;
    let origin = point(
        px(f32::from(display_bounds.origin.x) + (f32::from(display_bounds.size.width) - win_w) / 2.0),
        px(f32::from(display_bounds.origin.y) + (f32::from(display_bounds.size.height) - win_h) / 2.0),
    );
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: Size::new(px(win_w), px(win_h)),
            })),
            window_background: WindowBackgroundAppearance::Opaque,
            // 系统标题栏 + 系统关闭按钮（用户要求）
            titlebar: Some(TitlebarOptions {
                title: Some("OCR 模型".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            kind: WindowKind::Normal,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            focus: true,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(OcrModelsView::new);
            // 下载进行中时每 300ms 重绘一次刷新进度；窗口关闭（实体销毁）后退出。
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(300))
                        .await;
                    let Some(entity) = weak.upgrade() else {
                        break;
                    };
                    entity.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
            let handle = view.read(cx).focus_handle.clone();
            handle.focus(window, cx);
            // 包 Root：与 OcrPinView 一致（TextView 选择控制器依赖 Root）
            cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
        },
    )
    .map_err(|e| AppError::Gpui(format!("打开 OCR 模型窗口失败: {e}")))
}

/// OCR 识别窗口：左侧选区图 + 右侧识别结果（类似微信文字识别）。
/// 多次 OCR 复用同一个窗口（OpenOcrPin 关旧开新）；后台识别完成后
/// UpdateOcrPin 把文字填入右侧。
struct OcrPinView {
    focus_handle: FocusHandle,
    image: Arc<RenderImage>,
    /// 图片逻辑显示宽高（用于保持宽高比，窗口缩放不变形）
    img_w: f32,
    img_h: f32,
    /// None=识别中;Some(text)=显示文字
    text: Option<String>,
    /// 文字视图状态：支持鼠标拖选 + Ctrl+C 复制 + Ctrl+A 全选
    text_state: Option<Entity<gpui_component::text::TextViewState>>,
}

impl OcrPinView {
    fn new(
        frame: CapturedFrame,
        text: Option<String>,
        disp_w: f32,
        disp_h: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        let (w, h, pixels) = (frame.width, frame.height, frame.pixels);
        let img = build_render_image_from_pixels(w, h, pixels);
        Self {
            focus_handle: cx.focus_handle(),
            image: img,
            img_w: disp_w,
            img_h: disp_h,
            text,
            text_state: None,
        }
    }
}

impl Render for OcrPinView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        let img_w = self.img_w;
        let img_h = self.img_h;
        // 右侧结果区
        let right = match &self.text {
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(gpui::rgba(0x9E9E9EFF))
                        .text_sm()
                        .child(gpui::SharedString::from("OCR 识别中…")),
                ),
            Some(text) => {
                // 覆盖代码块配色：黑底白字（默认 muted 灰底），文字顶到标题栏下
                let code_style = gpui::StyleRefinement::default()
                    .bg(gpui::rgba(0x000000FF))
                    .text_color(gpui::rgba(0xFFFFFFFF));
                let tv_style = gpui_component::text::TextViewStyle {
                    code_block: code_style,
                    ..Default::default()
                };
                let text_view = if let Some(state) = &self.text_state {
                    // 有状态句柄：支持拖选/Ctrl+C/Ctrl+A
                    gpui_component::text::TextView::new(state).style(tv_style).selectable(true)
                } else {
                    let md = format!("```text\n{}\n```", text);
                    gpui_component::text::TextView::markdown("ocr-pin-text", md)
                        .style(tv_style)
                        .selectable(true)
                };
                div()
                    .size_full()
                    .bg(gpui::rgba(0x000000FF))
                    .text_color(gpui::rgba(0xFFFFFFFF))
                    .child(
                        div()
                            .p(px(10.0))
                            .overflow_y_scrollbar()
                            .child(text_view),
                    )
            }
        };
        // canvas 占满图片区；绘制时按图片宽高比 letterbox 居中（窗口缩放不变形）
        let paint = canvas(
            move |_, _, _| image.clone(),
            move |bounds, img, window, _cx| {
                // 在可用区域内按 img_w:img_h 比例计算居中子矩形（保持比例）
                let avail_w = f32::from(bounds.size.width);
                let avail_h = f32::from(bounds.size.height);
                let scale = (avail_w / img_w).min(avail_h / img_h).max(0.001);
                let draw_w = img_w * scale;
                let draw_h = img_h * scale;
                let target = Bounds {
                    origin: point(
                        bounds.origin.x + px((avail_w - draw_w) / 2.0),
                        bounds.origin.y + px((avail_h - draw_h) / 2.0),
                    ),
                    size: Size::new(px(draw_w), px(draw_h)),
                };
                let _ = window.paint_image(target, Default::default(), img.clone(), 0, false);
            },
        )
        .size_full();
        div()
            .id("ocr-pin")
            .size_full()
            .flex()
            .bg(gpui::rgba(0x181818FF))
            .text_color(gpui::rgba(0xE6E6E6FF))
            .track_focus(&self.focus_handle)
            // Ctrl+C / Cmd+C 复制选中文字、Ctrl+A / Cmd+A 全选。
            // 用 arboard 长存剪贴板（GPUI write_to_clipboard 在 X11 不可靠），
            // 且不依赖焦点落在 TextView 上（根拦截）。
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                let is_c = ev.keystroke.key == "c" || ev.keystroke.key == "C";
                let is_a = ev.keystroke.key == "a" || ev.keystroke.key == "A";
                let mods = ev.keystroke.modifiers;
                let copy = (mods.control || mods.platform) && is_c;
                let select_all = (mods.control || mods.platform) && is_a;
                if !copy && !select_all {
                    return;
                }
                let Some(state) = &this.text_state else { return };
                if copy {
                    let selected = state.read(cx).selected_text();
                    if !selected.trim().is_empty() {
                        if let Err(e) = crate::clipboard::global().write_text(&selected) {
                            tracing::error!("OCR 复制选中文字失败: {e}");
                        } else {
                            tracing::info!("OCR 已复制选中文字 ({} bytes)", selected.len());
                        }
                    }
                    cx.stop_propagation();
                } else if select_all {
                    state.update(cx, |s, cx| s.select_all(cx));
                    cx.stop_propagation();
                }
            }))
            // 左侧图片区：flex 自适应占满剩余空间，窗口缩放时图片保持比例居中
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .bg(gpui::rgba(0x000000FF))
                    .child(paint),
            )
            // 右侧结果区（固定宽度 360）
            .child(div().w(px(360.0)).h_full().child(right))
    }
}

/// 打开 OCR 识别窗口（左图右文）。窗口大小 = 图片显示宽 + 360，高度按比例。
fn open_ocr_pin_in_app(payload: PinPayload, cx: &mut App) -> AppResult<WindowHandle<gpui_component::Root>> {
    let PinPayload { frame, sx, sy, .. } = payload;
    let img_w = frame.width as f32 / sx;
    let img_h = frame.height as f32 / sy;
    // 限制显示尺寸（选区高度即默认窗口高度，左侧宽度即图片显示宽度）
    let max_w = 900.0_f32;
    let max_h = 700.0_f32;
    let scale = (max_w / img_w).min(max_h / img_h).min(1.0).max(150.0 / img_w);
    let disp_w = img_w * scale;
    let disp_h = img_h * scale;
    const RIGHT_W: f32 = 360.0;
    let win_w = disp_w + RIGHT_W;
    let win_h = disp_h;
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or_else(|| {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: Size::new(px(1280.0), px(800.0)),
        }
    });
    let origin = point(
        px(f32::from(display_bounds.origin.x) + (f32::from(display_bounds.size.width) - win_w) / 2.0),
        px(f32::from(display_bounds.origin.y) + (f32::from(display_bounds.size.height) - win_h) / 2.0),
    );
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin,
                size: Size::new(px(win_w), px(win_h)),
            })),
            window_background: WindowBackgroundAppearance::Opaque,
            titlebar: Some(TitlebarOptions {
                title: Some("OCR 识别".into()),
                appears_transparent: false,
                ..Default::default()
            }),
            kind: WindowKind::Normal,
            is_movable: true,
            is_resizable: true,
            is_minimizable: false,
            focus: true,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| OcrPinView::new(frame, None, disp_w, disp_h, cx));
            let h = view.read(cx).focus_handle.clone();
            h.focus(window, cx);
            // 包 gpui-component Root：TextView 的鼠标选择控制器依赖 Root 的
            // selection scope（window.on_mouse_event + Root::update），
            // 不包 Root 则右侧文字无法拖选。
            cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
        },
    )
    .map_err(|e| AppError::Gpui(format!("打开 OCR 识别窗口失败: {e}")))
}

/// 取窗口的 Win32 HWND（仅 Windows；非 Win32 句柄返回 None）。
#[cfg(target_os = "windows")]
fn window_hwnd(window: &mut Window) -> Option<*mut core::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = window.window_handle() {
        if let RawWindowHandle::Win32(win) = handle.as_raw() {
            return Some(win.hwnd.get() as *mut core::ffi::c_void);
        }
    }
    None
}

/// 把窗口客户端区的屏幕 Y 校正到 `desired_client_top`，延迟到 App 借期外执行（Windows）。
///
/// GPUI 的 `calculate_window_rect` 假设边框对称（height_offset/2 平分上下），
/// 但实际窗口顶部边框可能为 0（全部在底部），导致客户端被放高几像素。
/// 窗口创建后用 `ClientToScreen` 实测客户端原点，再用 `SetWindowPos` 校正——
/// 任何平台/DPI 都自动正确，无需硬编码系统栏高度。
///
/// 不能直接在 `open_window` 回调里调 `adjust_window_client_top`：`SetWindowPos` 会
/// 同步触发 `WM_MOVE`，gpui_windows 的 `on_moved` 回调重新进入 App，而回调执行期间
/// App 仍被 `open_window` 所在的 update 借出，报 "RefCell already borrowed"
/// （gpui_windows 的 `restart` 处 defer 注释是同一问题）。这里捕获 HWND 后交给
/// foreground executor，在借期外、窗口真正落地后再位移。
#[cfg(target_os = "windows")]
fn schedule_client_top_adjustment(
    cx: &mut App,
    hwnd: Option<*mut core::ffi::c_void>,
    desired_client_top: i32,
) {
    let Some(hwnd) = hwnd else { return };
    let hwnd_key = hwnd as usize;
    let desired = desired_client_top;
    cx.spawn(async move |async_cx| {
        // 等约一帧，确保窗口已由系统完成创建与首帧布局
        async_cx
            .background_executor()
            .timer(std::time::Duration::from_millis(16))
            .await;
        adjust_window_client_top(hwnd_key as *mut core::ffi::c_void, desired);
    })
    .detach();
}

/// 用 Win32 API 把窗口客户端区顶校正到 `desired_client_top`。
///
/// 只操作 HWND，不经过 GPUI：调用方（`schedule_client_top_adjustment`）在 App 借期外
/// 执行，避免 `SetWindowPos` 触发 `WM_MOVE` 回调重新进入 App 造成 "RefCell already
/// borrowed"。
#[cfg(target_os = "windows")]
fn adjust_window_client_top(hwnd: *mut core::ffi::c_void, desired_client_top: i32) {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    unsafe {
        let mut pt: POINT = std::mem::zeroed();
        ClientToScreen(hwnd, &mut pt);
        let dy = desired_client_top - pt.y;
        if dy != 0 {
            let mut wr: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut wr);
            tracing::debug!(
                "[adjust] client_top actual={} desired={} dy={}",
                pt.y, desired_client_top, dy
            );
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                wr.left,
                wr.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_move_down_keeps_size_and_clamps_to_selection() {
        // 选区 (100,100)~(600,500)（size 500x400），文字框 (200,150)~(300,198)（size 100x48）
        let limits = ub::Bounds::new(
            BoundsPoint::new(100.0, 100.0),
            BoundsPoint::new(600.0, 500.0),
        );
        let start = ub::Bounds::new(
            BoundsPoint::new(200.0, 150.0),
            BoundsPoint::new(300.0, 198.0),
        );
        // 从拖动条 (250,155) 按住往下拖 100px → dy=100，origin.y=150+100=250
        let moved = text_move_rect(
            start,
            BoundsPoint::new(250.0, 155.0),
            BoundsPoint::new(250.0, 255.0),
            limits,
        );
        assert_eq!(moved.origin.y, 250.0);
        assert_eq!(moved.size.y, 48.0, "尺寸必须保持，不能被压扁");
        // 继续拖到底：max_y = 100+400-48 = 452
        let bottom = text_move_rect(
            start,
            BoundsPoint::new(250.0, 155.0),
            BoundsPoint::new(250.0, 500.0),
            limits,
        );
        assert_eq!(bottom.origin.y, 452.0);
        assert_eq!(bottom.size.y, 48.0);
        // 水平方向同样只钳制 origin、保持宽度
        let right = text_move_rect(
            start,
            BoundsPoint::new(250.0, 155.0),
            BoundsPoint::new(600.0, 155.0),
            limits,
        );
        assert_eq!(right.origin.x, 500.0, "max_x = 100+500-100 = 500");
        assert_eq!(right.size.x, 100.0);
    }

    #[test]
    fn text_resize_n_grows_up_and_clamps_at_min_height() {
        // 文字框 (200,150)~(300,198)（size 100x48），抓顶部中点手柄
        let start = ub::Bounds::new(
            BoundsPoint::new(200.0, 150.0),
            BoundsPoint::new(300.0, 198.0),
        );
        // 往上拖 20px：顶边跟随到 130，高度增至 68
        let up = text_resize_n_rect(
            start,
            BoundsPoint::new(250.0, 150.0),
            BoundsPoint::new(250.0, 130.0),
        );
        assert_eq!(up.origin.y, 130.0, "往上拖：顶边 y=150-20");
        assert_eq!(up.size.y, 68.0, "高度=48+20");
        assert_eq!(up.size.x, 100.0, "宽度不变");
        // 往下拖 6px：顶边下移到 156、高度减至 42（仍 ≥ MIN_H）
        let down = text_resize_n_rect(
            start,
            BoundsPoint::new(250.0, 150.0),
            BoundsPoint::new(250.0, 156.0),
        );
        assert_eq!(down.origin.y, 156.0);
        assert_eq!(down.size.y, 42.0);
        // 拖过头：高度钳制到 MIN_H=40，顶边停在 150 + (48-40) = 158
        let clamp = text_resize_n_rect(
            start,
            BoundsPoint::new(250.0, 150.0),
            BoundsPoint::new(250.0, 400.0),
        );
        assert_eq!(clamp.size.y, 40.0);
        assert_eq!(clamp.origin.y, 158.0, "MIN_H 时顶边 = origin + (size - MIN_H)");
    }

    #[test]
    fn rgba_to_bgra_swaps_channels_correctly() {
        // RGBA(LE u32) = R | G<<8 | B<<16 | A<<24 → BGRA = B | G<<8 | R<<16 | A<<24
        let mut px: Vec<u8> = vec![
            0x11, 0x22, 0x33, 0xFF, // 像素 0: R=0x11 G=0x22 B=0x33 A=0xFF
            0xAA, 0xBB, 0xCC, 0x00, // 像素 1: 半透明/全透明通道也要保留
            0x00, 0x00, 0x00, 0x00, // 像素 2: 全零
            0xFF, 0x80, 0x40, 0x80, // 像素 3: 混合值
        ];
        rgba_to_bgra(&mut px);
        assert_eq!(
            px,
            vec![
                0x33, 0x22, 0x11, 0xFF, // BGRA
                0xCC, 0xBB, 0xAA, 0x00,
                0x00, 0x00, 0x00, 0x00,
                0x40, 0x80, 0xFF, 0x80,
            ]
        );
    }
}
