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

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{AppError, AppResult};

use gpui::{
    App, AsyncApp, Bounds, Context, Entity, FocusHandle, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, QuitMode, Render, RenderImage, Size,
    WeakEntity, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    TitlebarOptions, WindowOptions, canvas, div, point, prelude::*, px, quad, rgba,
};
use gpui_component::button::Button;
use gpui_component::button::{ButtonVariant, ButtonVariants};
use gpui_component::ActiveTheme;
use gpui_component::Disableable;
use gpui_component::IconName;
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
// 统一设计令牌（颜色/圆角/间距/阴影）+ 复用按钮样式：见 ui_theme 模块文档
use crate::overlay::ui_theme as theme;
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


/// 马赛克**已提交笔迹**的显示层缓存（松手之后到最终提交之前，画布上要一直看得见）。
///
/// 为什么需要它：绘制分两条路 —— 拖动中走 `in_progress`（有实时预览层），松手后这一笔
/// 被移进"已提交命令"列表，而**已提交命令在编辑态原本不显示**（只在最终提交时烤进像素）。
/// 于是松手瞬间预览层失效、笔迹凭空消失，用户会以为"没生效"（"拖动能预览、结束就消失"）。
///
/// **一笔一层**（不是把所有笔迹合成一层）：每笔的颜色/块大小可以不同，合成一层就只能用
/// 其中一个颜色，第二笔换色会把前面所有笔迹一起染色 —— 这正是"第一笔也跟着变色"的成因。
/// 逐笔渲染也与提交路径一致（`apply_commands` 是**每条命令单独应用**）。
struct CommittedMosaicCache {
    /// 快照时的 `DrawingState.revision`（提交/撤销/重做都会 +1）
    revision: u64,
    /// 快照时的画布逻辑尺寸 → 帧物理像素的缩放比
    scale: (f32, f32),
    /// 每笔一层，按命令顺序
    layers: Vec<(Arc<RenderImage>, ub::Bounds)>,
}

/// 马赛克**实时预览**层缓存：当前这一笔的真像素光栅化结果。
///
/// 用户明确要"拖动中就看到真实效果"。预览不能画示意图（那正是"拖动中与成型后
/// 不一致"的来源），所以这里放的是**真像素**：由
/// [`crate::overlay::commands::render_mosaic_stroke_pixels`] 算出，与松手提交走的是
/// 同一份代码，逐像素一致（有测试钉住）。
///
/// 只在"笔迹长出新 stamp"或缩放入参变化时重算；同一帧内多次渲染直接复用。
struct MosaicPreviewCache {
    /// 快照时的 stamp 个数：笔迹只增不改，个数变了才需要重算
    region_count: usize,
    /// 快照时的最后一个 stamp（同个数但换笔时兜底）
    last_region: Option<((f32, f32), (f32, f32))>,
    /// 画布逻辑坐标 → 帧物理像素的缩放比（窗口尺寸变了要重算）
    scale: (f32, f32),
    /// 光栅化结果（BGRA，未覆盖处透明）
    image: Arc<RenderImage>,
    /// paint_image 的目标 Bounds（逻辑像素）
    bounds: ub::Bounds,
}

/// Freehand 增量渲染状态（累积所有已画 Freehand）：
/// 拖动时每帧只画新增段(O(新增))；buffer 覆盖所有 Freehand 的联合 bbox，
/// bbox 超界时重建(拷贝旧像素+画新段)。bbox 计算与 rasterize_shapes 完全
/// 一致(pad=lw/2+1, origin=floor, size=ceil)→ 与提交成图像素级一致。
struct IncrFreehand {
    /// CPU buffer（RGBA，透明底，逻辑像素）
    frame: CapturedFrame,
    /// buffer 逻辑原点
    origin: (f32, f32),
    /// 当前笔画已渲染点数（新笔画开始归零；buffer 保留旧 Freehand 像素）
    rendered: usize,
    /// 当前笔画线宽（变化时全量重建）
    lw: f32,
    /// 渲染结果（图 + 逻辑 bounds）
    image: Option<(Arc<RenderImage>, ub::Bounds)>,
}

/// GPUI 视图：覆盖窗口内容
pub struct OverlayView {
    /// 捕获帧的 GPUI 渲染图（已转 BGRA）
    frame_image: Arc<RenderImage>,
    /// 待释放的图像：GPUI 的 `RenderImage` 在 GPU atlas 里占一块瓦片，换图后不显式
    /// `window.drop_image()` 就永不回收——`RenderImage` 没有 Drop 实现，而全仓此前
    /// 一处 drop_image 都没调，于是拖动/画线时每秒新建几十张整幅图，显存单调增长
    /// 直到关闭窗口（4K 帧一张 ≈33MB 的像素，上传后还占 atlas 瓦片）。
    ///
    /// 这里只挂"已经被替换掉、不再被任何状态引用"的图，在下一帧 render 开头统一释放。
    /// **滞后一帧是必须的**：本帧刚 paint 进场景的图还没上传到 atlas，立刻回收会画不出来
    /// （与 Zed 自己的 remote_video_track_view 同做法：留当前帧，下一帧释放上一帧）。
    pending_image_drops: Vec<Arc<RenderImage>>,
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
    /// 当前这一笔马赛克的实时预览层（见 `MosaicPreviewCache`）
    mosaic_preview: Option<MosaicPreviewCache>,
    /// 已提交马赛克笔迹的显示层（见 `CommittedMosaicCache`）
    mosaic_layer: Option<CommittedMosaicCache>,
    /// Freehand 统一增量层（累积所有已画 Freehand；committed 不渲染 Freehand）
    freehand_incr: Option<IncrFreehand>,



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

    /// 鼠标光标位置（逻辑像素，窗口坐标系）。用来画全屏十字参考线与坐标徽章。
    /// `None` = 本次会话还没收到过鼠标移动（刚唤起覆盖层时）。
    cursor_pos: Option<BoundsPoint>,
    /// 上一次**已经显示过**的物理像素坐标（取整）。只有它变化才 `notify`：
    /// 否则鼠标每移动一个亚像素都要重绘整个覆盖层（画布 + 工具栏 + 形状），白烧 CPU。
    cursor_phys: Option<(i32, i32)>,
    /// 「选区外禁止点击」状态：选区落定后指向选区外（工具栏除外）时为 true。
    /// 画布端据此换禁止光标 + 画 🚫 角标，和 `on_mouse_down` 里真正拦掉点击的行为对齐。
    forbidden_hover: bool,

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
            pending_image_drops: Vec::new(),
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
            mosaic_preview: None,
            mosaic_layer: None,
            freehand_incr: None,
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
            cursor_pos: None,
            cursor_phys: None,
            forbidden_hover: false,
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
        // 用 replace 取得旧图并挂起释放：直接赋值会让旧帧的 atlas 瓦片永久泄漏
        let old_frame_image = std::mem::replace(
            &mut self.frame_image,
            build_render_image_from_pixels(frame.width, frame.height, frame.pixels.clone()),
        );
        self.pending_image_drops.push(old_frame_image);
        self.screen_bounds = screen_bounds;
        self.client_origin = client_origin;
        self.selection = SelectionState::new(screen_bounds);
        self.tx = tx;
        self.mode = OverlayMode::Selecting;
        self.toolbar = ToolbarState::default();
        self.drawing = DrawingState::new();
        // 新会话必须丢掉上一轮的像素层缓存（帧内容与命令都换了），否则可能残留旧图
        self.mosaic_preview = None;
        self.mosaic_layer = None;
        self.in_progress = None;
        // 清缓存前交出旧图：形状层缓存是"全部形状联合 bbox"的整幅光栅，
        // 大的能到整屏，直接置 None 就把这块 atlas 瓦片永久留下了
        if let Some(old) = self.shape_layer_cache.take() {
            self.pending_image_drops.push(old.image);
        }
        // 置空前先把持有的增量图挂起释放，否则这张图就留在了 atlas 里
        if let Some((old, _)) = self.freehand_incr.take().and_then(|st| st.image) {
            self.pending_image_drops.push(old);
        }
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
        self.cursor_pos = None;
        self.cursor_phys = None;
        self.forbidden_hover = false;
        self.scale_factor = scale_factor;
        tracing::info!("[overlay] start_session scale_factor={scale_factor} frame={}x{}", self.frame_width, self.frame_height);
        self.dim_opacity = 1.0;
        self.apply_ui_probe();
        cx.notify();
    }

    /// **仅用于 UI 视觉调试**：`SCREENSHOT_RS_UI_PROBE=<工具>[:popover]` 时，
    /// 会话一开始就造一个默认选区、选中指定工具（可选同时展开它的弹层）。
    ///
    /// 动机：截图工具栏/二级弹层的样式只能在真实覆盖窗口里看到，而手工
    /// 拖拽+点击既慢又容易点空（还会误改用户桌面上的东西）。带上这个环境
    /// 变量后，一次 `alt+s` 就能直接得到「已选中矩形工具 + 弹层展开」的画面，
    /// 供截图脚本逐像素核对。工具名：rect/ellipse/arrow/pen/text/mosaic。
    ///
    /// 正常使用不带该环境变量，此函数立即返回。
    fn apply_ui_probe(&mut self) {
        let Ok(spec) = std::env::var("SCREENSHOT_RS_UI_PROBE") else {
            return;
        };
        // spec 形如 `rect[:popover]`；也可以和辅助窗口探针组合成
        // `rect:popover,pin,progress`（逗号后面是 [`probe_aux_windows`] 的取值），
        // 所以这里按逗号切分后再判断有没有 popover。
        let (head, rest) = match spec.split_once(':') {
            Some((n, p)) => (n.to_string(), p.to_string()),
            None => (spec.clone(), String::new()),
        };
        let (name, want_popup) = (
            head.split(',').next().unwrap_or("").trim(),
            rest.split(',').any(|p| p.trim() == "popover"),
        );
        let tool = match name {
            "rect" => ToolButton::Rectangle,
            "ellipse" => ToolButton::Ellipse,
            "arrow" => ToolButton::Arrow,
            "pen" => ToolButton::Freehand,
            "text" => ToolButton::Text,
            "mosaic" => ToolButton::Mosaic,
            other => {
                tracing::warn!("[UI 探针] 未知工具 {other}，忽略");
                return;
            }
        };
        // 默认选区：屏幕中部 640x420 的框（工具栏/弹层相对它定位）
        let sb = self.screen_bounds;
        let w = sb.size.x.min(640.0);
        let h = sb.size.y.min(420.0);
        let ox = sb.origin.x + (sb.size.x - w) / 2.0;
        let oy = sb.origin.y + (sb.size.y - h) / 2.0;
        self.selection.mouse_down(ub::Point::new(ox, oy));
        self.selection.mouse_move(ub::Point::new(ox + w, oy + h));
        self.selection.mouse_up();
        // 正常流程里 mouse_up 会把模式切到 Editing（工具栏只在 Editing 显示），
        // 这里直接驱动纯逻辑状态机，必须自己补上这一步。
        self.mode = OverlayMode::Editing;
        self.toolbar.active_tool = Some(tool);
        if want_popup {
            self.toolbar.popup = Some(if tool == ToolButton::Text {
                ToolbarPopup::Text
            } else {
                ToolbarPopup::Stroke
            });
        }
        tracing::warn!(
            "[UI 探针] 已注入默认选区 {:?} 工具={tool:?} 弹层={:?}",
            self.selection.current(),
            self.toolbar.popup
        );
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
        // 新 Freehand 开始：rendered 归零（该笔画的增量从 0 起），buffer 保留旧
        if tool == ToolButton::Freehand {
            if let Some(st) = self.freehand_incr.as_mut() {
                st.rendered = 0;
            }
        }
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
                // 将工具栏线宽档位映射为画笔大小：画笔越宽单次覆盖越大，
                // block_size 也随之增大，彻底马赛克遮字（见 mosaic_geom）。
                let (bs, block_size) = mosaic_geom(self.toolbar.line_width);
                let half = bs / 2.0;
                let stamp = (
                    crate::overlay::drawing::Point::new(dp.x - half, dp.y - half),
                    crate::overlay::drawing::Point::new(dp.x + half, dp.y + half),
                );
                DrawCommand::Mosaic {
                    regions: vec![stamp],
                    block_size,
                    color: self.toolbar.current_color,
                }
            }
            // Text 走 open_text_input、Ocr 走框选识别（on_mouse_down 已拦截），
            // 其余非绘图工具忽略。
            ToolButton::Text | ToolButton::Ocr | ToolButton::Translate | ToolButton::ColorPicker
            | ToolButton::Undo
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
                // 画笔模式：沿线段**间隔采样**补 stamp，填满快速拖拽留下的空隙。
                // 旧实现只在「距上一个 stamp >= spacing」时才在终点加一个，快速拖动时
                // 两个事件点距离可能远超 spacing，导致马赛克出现断点 → 要反复擦涂。
                let (bs, _) = mosaic_geom(self.toolbar.line_width);
                // 50% 重叠保证覆盖连续（配合 apply_mosaic 的全局对齐网格，
                // 重叠不会交叉涂抹，块边界保持清晰可辨）。
                let spacing = bs * 0.5;
                let half = bs / 2.0;
                if let Some(last) = regions.last() {
                    let cx = (last.0.x + last.1.x) / 2.0;
                    let cy = (last.0.y + last.1.y) / 2.0;
                    let dx = dp.x - cx;
                    let dy = dp.y - cy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist >= spacing {
                        // 从上一个 stamp 到当前点，按 spacing 等分补 stamp，覆盖整段
                        let steps = (dist / spacing).floor().max(1.0) as usize;
                        for i in 1..=steps {
                            let t = i as f32 / steps as f32;
                            let ix = cx + dx * t;
                            let iy = cy + dy * t;
                            regions.push((
                                crate::overlay::drawing::Point::new(ix - half, iy - half),
                                crate::overlay::drawing::Point::new(ix + half, iy + half),
                            ));
                        }
                    }
                } else {
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
        // Freehand 松手：清空增量层（曲线已进 committed，与矩形等单层显示，
        // 避免多层叠加在重渲染时抖动/消失又出现）
        if matches!(normalized, DrawCommand::Freehand { .. }) {
            // 置空前先把持有的增量图挂起释放，否则这张图就留在了 atlas 里
        if let Some((old, _)) = self.freehand_incr.take().and_then(|st| st.image) {
            self.pending_image_drops.push(old);
        }
        }
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
        // 默认高度提高：行高加成 14→22、下限 40→50，初始框更明显
        let line_h = window.line_height().as_f32();
        let init_h = (line_h + 22.0).max(50.0);
        self.text_input_rect = ub::Bounds::new(p, BoundsPoint::new(p.x + w, p.y + init_h))
            .clamp_inside(limits);
        tracing::debug!("open_text_input: anchor=({:.1}, {:.1}) initial={}", p.x, p.y, initial.is_some());

        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("")
                .auto_grow(1, 8)
                .soft_wrap(false)
                // 关掉编辑器自带滚动条：框体随文字自动扩宽，但 soft_wrap(false)
                // 下编辑器只要有**任何**方向的滚动（换行/光标移动/IME 合成）就会
                // 闪出**横向**滚动条（gpui-component 里 !soft_wrap 时固定用横向条），
                // 在这么小的浮层框里纯属干扰。光标跟随滚动本身不受影响。
                .editor_scrollbar(false)
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
                // 还要带上 toolbar_hovered：点工具栏按钮打开样式弹层时，mousedown
                // 已先把 toolbar_hovered=true，而 popup 要等弹层打开(on_open_change)
                // 才置 Some，存在 blur 在前、popup 未定的竞态——若只判 popup，
                // 「点 Text 按钮开色板改颜色」会误把编辑态提交。
                if this.toolbar.popup.is_some() || this.toolbar_hovered {
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
                background: self.toolbar.current_bg,
                box_size: (self.text_input_rect.size.x, self.text_input_rect.size.y),
                text_inset: crate::overlay::window::TEXT_BOX_INSET,
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
        // 滚动截屏需要一个像样的选区（太小没有可滚动内容，注入滚动也没意义）
        let scroll_disabled = sel.size.x < 20.0 || sel.size.y < 20.0;

        let (toolbar_x, toolbar_y, _toolbar_w, toolbar_h) =
            compute_toolbar_bounds(sel, self.screen_bounds);

        // 弹层展开方向：**永远不要盖住工具栏自己**（用户反馈：文字弹层会挡住
        // 「文字」按钮）。判定顺序：
        //   1) 先看首选方向放不放得下——首选是「不遮截图框」：工具栏在选区上方
        //      时向上展开，否则向下展开；
        //   2) 首选方向放不下就换另一边；
        //   3) 两边都放不下（矮屏幕 + 超长弹层）就选空间大的那边，此时 gpui 的
        //      snap 仍可能平移，但至少不会把弹层挪到工具栏正上方。
        let screen_top = self.screen_bounds.origin.y;
        let screen_bottom = self.screen_bounds.origin.y + self.screen_bounds.size.y;
        let popover_est_h = if active_tool == Some(ToolButton::Text) {
            POPOVER_EST_H_TEXT
        } else {
            POPOVER_EST_H_STROKE
        };
        let toolbar_bottom = toolbar_y + toolbar_h;
        let space_below = screen_bottom - toolbar_bottom - POPOVER_EDGE_MARGIN;
        let space_above = toolbar_y - screen_top - POPOVER_EDGE_MARGIN;
        let prefer_up = toolbar_bottom <= sel.origin.y;
        let popover_open_up = match (prefer_up, space_above, space_below) {
            (true, above, _) if above >= popover_est_h => true,
            (true, _, below) if below >= popover_est_h => false,
            (false, _, below) if below >= popover_est_h => false,
            (false, above, _) if above >= popover_est_h => true,
            // 两边都放不下：选空间更大的一边（尽量少触发 snap 平移）
            _ => space_above > space_below,
        };

        let weak = cx.weak_entity();

        // 按 ToolButton::GROUPS 分组渲染：组内按钮紧凑排列，组间画一条竖直
        // 分隔线，把「绘图工具 → 识别/滚动 → 撤销重做 → 收尾动作」四类分开，
        // 用户扫视时能快速定位（也避免 16 个按钮糊成一片）。
        let mut bar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(theme::m::GROUP_GAP));
        for (gi, group) in ToolButton::GROUPS.iter().enumerate() {
            if gi > 0 {
                bar = bar.child(group_divider());
            }
            let mut row = div().flex().items_center().gap(px(theme::m::BTN_GAP));
            for &btn in group.iter() {
                // 禁用条件集中在这里：撤销/重做看历史栈，滚动截屏要求选区够大
                let disabled = match btn {
                    ToolButton::Undo => !can_undo,
                    ToolButton::Redo => !can_redo,
                    ToolButton::Scroll | ToolButton::ScrollManual => scroll_disabled,
                    _ => false,
                };
                let is_active = active_tool == Some(btn);
                let el = if btn.has_popover() {
                    render_tool_button_with_popover(
                        btn,
                        is_active,
                        popover_open_up,
                        weak.clone(),
                        self,
                        cx,
                    )
                    .into_any_element()
                } else {
                    render_simple_button(btn, is_active, disabled, weak.clone()).into_any_element()
                };
                row = row.child(el);
            }
            bar = bar.child(row);
        }

        // 工具栏根 div
        // 通过 on_mouse_down 设 toolbar_hovered=true，配合 root.on_mouse_down 检查
        // 该标志 → 早 return，吞掉点击，避免 selection.mouse_down 把选区打散。
        // 不用几何估算判断：按钮宽度随标签/图标变化，几何估算容易漏判，
        // 而「内层先于外层」的冒泡顺序保证这里 set 的标志在 root 里已是新值。
        div()
            .absolute()
            .top(px(toolbar_y))
            .left(px(toolbar_x))
            // 限制工具栏最大宽度为「从 toolbar_x 到屏幕右缘」，让按钮在窄截图区内
            // 自动换行，保证「完成/取消」等右侧按钮始终可见。
            .max_w(px(self.screen_bounds.origin.x + self.screen_bounds.size.x - toolbar_x))
            .bg(theme::c::rgb(theme::tokens::PANEL_BG))
            .rounded(px(theme::r::PANEL))
            .border_1()
            .border_color(theme::c::rgb(theme::tokens::PANEL_BORDER))
            .shadow(theme::panel_shadow())
            .p(px(theme::m::PANEL_PAD))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _window, _cx| {
                this.toolbar_hovered = true;
            }))
            .child(bar)
    }
}

/// handle 视觉尺寸：边长（像素）
const HANDLE_VISUAL_SIZE: f32 = 8.0;
/// 角手柄视觉直径（正圆）
const HANDLE_CORNER: f32 = 9.0;
/// 边手柄视觉长边（细胶囊）
const HANDLE_EDGE_LONG: f32 = 14.0;
/// 边手柄视觉短边（细胶囊）
const HANDLE_EDGE_THIN: f32 = 4.0;

/// 单个手柄的视觉尺寸 (宽, 高)，索引顺序与 `Bounds::handle_positions()` 一致：
/// 0 TL / 1 T / 2 TR / 3 L / 4 R / 5 BL / 6 B / 7 BR。
/// 上/下边给横胶囊、左/右边给竖胶囊、四角给圆点（圆点=宽高相等的圆角 quad）。
fn handle_visual_size(i: usize) -> (f32, f32) {
    match i {
        1 | 6 => (HANDLE_EDGE_LONG, HANDLE_EDGE_THIN),
        3 | 4 => (HANDLE_EDGE_THIN, HANDLE_EDGE_LONG),
        _ => (HANDLE_CORNER, HANDLE_CORNER),
    }
}

/// 画 8 个缩放/拖动手柄：**四角圆点 + 四边细胶囊**（白填充 + 主题强调色描边）。
///
/// 取代原来「8 个 8×8 小白方块」：方块压在截图上笨重、直角也和圆角边框不搭。
/// 位置仍取 `bounds.handle_positions()`（顺序 TL/T/TR/L/R/BL/B/BR），中心落在
/// 边框线上；命中容差由 `HANDLE_HIT_HALF` / `HANDLE_HALF_SIZE` 独立判定，不受
/// 视觉尺寸影响（视觉可以更小更精致，命中区保持好点）。
fn paint_handles(window: &mut Window, bounds: ub::Bounds) {
    let fill = Hsla::from(rgba(0xFFFFFFF2));
    let border = Hsla::from(rgba(theme::tokens::ACCENT));
    for (i, hp) in bounds.handle_positions().iter().enumerate() {
        let (w, h) = handle_visual_size(i);
        window.paint_quad(quad(
            Bounds {
                origin: point(px(hp.x - w / 2.0), px(hp.y - h / 2.0)),
                size: Size::new(px(w), px(h)),
            },
            // 半径取短边一半：角上是正圆，边上正好是胶囊
            px(w.min(h) / 2.0),
            fill,
            px(1.0),
            border,
            Default::default(),
        ));
    }
}
/// handle 命中容差的一半（与 selection::HANDLE_HALF_SIZE 保持一致）
const HANDLE_HIT_HALF: f32 = 8.0;

/// 弹层与窗口边缘的安全间距（px）——与 gpui-component 的
/// `snap_to_window_with_margin(px(8.))` 保持一致。
const POPOVER_EDGE_MARGIN: f32 = 8.0;
/// 弹层高度估计（px，宁大勿小）：只用来判断「往哪边展开放得下」。
///
/// gpui-component 的 Popover 用 `snap_to_window_with_margin` 兜底：如果按锚定
/// 方向放不下，它会把弹层**整体平移**回窗口内——平移方向是向上，结果就是弹层
/// 压住工具栏、盖住「文字/矩形」这些按钮本身（用户反馈）。所以展开方向必须
/// 自己先算准，不能让 snap 去挪。
/// 实测：粗细弹层 12 列色板 ≈ 190px（留出余量）
const POPOVER_EST_H_STROKE: f32 = 210.0;
/// 文字弹层更高（标签 + 字号档位 + 两组色板），实测 ≈ 250px（字号档位现在是
/// 固定宽度、**保证一行**，见 `fixed_chip_button`）；取 280 兜住各平台字体行高差异。
/// 估计值偏大只会让弹层偶尔多向上展开，偏小则会被 snap 推回来盖住按钮。
const POPOVER_EST_H_TEXT: f32 = 280.0;
/// 工具栏距离选区上沿的距离（px）
const TOOLBAR_OFFSET_Y: f32 = 8.0;

/// 把 ToolButton 映射到图标
///
/// 优先用**项目自带**的 Lucide 图标（`assets/icons/ui`，经 [`crate::assets::AppAssets`]
/// 以 `app-icons/` 前缀暴露）——截图工具需要的语义（画笔 / 文字 / 扫描 /
/// 马赛克 / 固定 / 加粗 / 滚轮）在 gpui-component 内置集里没有好的对应，
/// 之前只能用 Frame / CircleX / Asterisk / SquareTerminal 之类凑数，语义不清。
/// 其余（撤销 / 重做 / 完成 / 取消）沿用内置图标，两者同源同风格。
fn icon_for(btn: ToolButton) -> Icon {
    use crate::assets::icons as app_icon;
    match btn {
        ToolButton::Rectangle => Icon::empty().path(app_icon::SQUARE),
        ToolButton::Ellipse => Icon::empty().path(app_icon::CIRCLE),
        ToolButton::Arrow => Icon::empty().path(app_icon::MOVE_UP_RIGHT),
        ToolButton::Freehand => Icon::empty().path(app_icon::PENCIL),
        ToolButton::Text => Icon::empty().path(app_icon::TYPE),
        ToolButton::Ocr => Icon::empty().path(app_icon::SCAN_TEXT),
        ToolButton::Translate => Icon::empty().path(app_icon::TRANSLATE),
        ToolButton::Mosaic => Icon::empty().path(app_icon::GRID_2X2),
        ToolButton::ColorPicker => Icon::empty().path(app_icon::PIPETTE),
        ToolButton::Pin => Icon::empty().path(app_icon::PIN),
        ToolButton::Bold => Icon::empty().path(app_icon::BOLD),
        ToolButton::Scroll => Icon::empty().path(app_icon::UNFOLD_VERTICAL),
        ToolButton::ScrollManual => Icon::empty().path(app_icon::MOUSE),
        ToolButton::Undo => Icon::new(IconName::Undo2),
        ToolButton::Redo => Icon::new(IconName::Redo2),
        ToolButton::Finish => Icon::new(IconName::Check),
        ToolButton::Cancel => Icon::new(IconName::Close),
    }
}

/// 工具栏实际渲染宽度估算（px）
///
/// 用途只有一个：光标贴屏幕右缘时把工具栏整体左移，避免「完成」被截断在屏外。
/// 渲染与估算共用 `ToolButton::GROUPS` / `shows_label()` 这一份定义，
/// 任何按钮增删/改标签都自动同步，不会再出现「估算 32px、实际 800px」的漂移。
fn toolbar_width_estimate() -> f32 {
    /// 图标与标签之间的间距（与 btn_content 里的 gap 一致）
    const ICON_LABEL_GAP: f32 = 5.0;
    let mut w = theme::m::PANEL_PAD * 2.0;
    for (gi, group) in ToolButton::GROUPS.iter().enumerate() {
        if gi > 0 {
            // 组间：左右间距 + 1px 分隔线
            w += theme::m::GROUP_GAP * 2.0 + 1.0;
        }
        for (bi, &btn) in group.iter().enumerate() {
            if bi > 0 {
                w += theme::m::BTN_GAP;
            }
            w += if btn.shows_label() {
                // 中文字宽略大于字号（实测 12.5px 字号渲染约 13.2px/字），
                // 低估会让工具栏在屏幕右缘被裁掉十几个像素（「完成」被切），
                // 所以这里按 13.2 估并整体留 4px 余量。
                theme::m::BTN_PAD_X * 2.0
                    + theme::m::ICON
                    + ICON_LABEL_GAP
                    + btn.label().chars().count() as f32 * (theme::m::FONT_LABEL * 1.056)
            } else {
                theme::m::BTN_H
            };
        }
    }
    w + 4.0
}


// ---------------------------------------------------------------------------
// 辅助窗口的共用视觉组件
//
// 模型窗口与系统设置窗口都长在辅助窗口里，样式必须一致——所以卡片、区块头、
// 状态胶囊这三样抽成共用函数，而不是各写一份相近但不相同的 div 树。
// 颜色一律走 ui_theme 的 token，禁止在这里写死色值。
// ---------------------------------------------------------------------------

/// 区块卡片：深色底 + 细描边 + 统一内边距与圆角。
fn window_card() -> gpui::Div {
    div()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .rounded_md()
        .border_1()
        .border_color(theme::c::rgb(theme::tokens::PANEL_BORDER))
        .bg(theme::c::rgb(theme::tokens::POPOVER_BG))
}

/// 窗口标题区：主标题 + 说明，下方紧跟一条分隔线。
fn window_header(title: &str, subtitle: &str) -> impl IntoElement {
    use theme::tokens as t;
    div()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_base()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme::c::rgb(t::TEXT))
                .child(gpui::SharedString::from(title.to_string())),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::c::rgb(t::SECTION_LABEL))
                .child(gpui::SharedString::from(subtitle.to_string())),
        )
        .child(div().h(px(1.0)).w_full().bg(theme::c::rgb(t::DIVIDER)))
}

/// 区块头：左边标题 + 说明，右边放按钮（没有按钮时传空 div）。
fn section_header(title: &str, subtitle: &str, right: impl IntoElement) -> impl IntoElement {
    use theme::tokens as t;
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::c::rgb(t::TEXT))
                        .child(gpui::SharedString::from(title.to_string())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::c::rgb(t::SECTION_LABEL))
                        .child(gpui::SharedString::from(subtitle.to_string())),
                ),
        )
        .child(right)
}

/// 状态胶囊：同色系柔和底 + 彩色文字。整屏都是纯色文字会显得很"生"，
/// 胶囊把"状态"从正文里拎出来。
fn status_chip(text: String, color: u32, soft_bg: u32) -> impl IntoElement {
    div()
        .px(px(7.0))
        .py(px(2.0))
        .rounded_sm()
        .bg(gpui::rgba(soft_bg))
        .text_xs()
        .text_color(gpui::rgba(color))
        .child(gpui::SharedString::from(text))
}

/// 只取文件名，去掉目录前缀。
///
/// 翻译模型的清单里写的是 `onnx/encoder_model_int8.onnx` 这种相对路径（下载时要拼 URL），
/// 但界面要跟 OCR 模型那几行一致——**只显示文件名**，别把路径当内容显示出来。
fn file_basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// 把热键写法拆成键帽序列（"ctrl+shift+a" → ["Ctrl", "Shift", "A"]）。
///
/// 只做展示美化用：大小写与别名都按用户能认出来的写法给。
fn hotkey_caps(spec: &str) -> Vec<String> {
    spec.split('+')
        .map(|p| {
            let p = p.trim();
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "alt" | "option" => "Alt".to_string(),
                "shift" => "Shift".to_string(),
                "super" | "cmd" | "win" | "meta" => "Super".to_string(),
                other => other.to_uppercase(),
            }
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// 「UI 区域」：工具栏本体，外加（二级弹层展开时）弹层所占的那片区域。
///
/// 用途只有一个——判断指针是不是落在 UI 上。工具栏会被 `compute_toolbar_bounds`
/// 摆到选区**之外**（选区太小或贴屏幕边时），所以"是否在选区内"不足以描述 UI。
fn ui_zone(sel: Option<ub::Bounds>, screen_bounds: ub::Bounds, popup_open: bool) -> ub::Bounds {
    /// 二级弹层的最大高度估计：比最高那个（文字样式）再留点余量。
    const POPOVER_MAX_H: f32 = 340.0;
    /// 外扩一点，容忍工具栏阴影/边框与坐标取整。
    const PAD: f32 = 6.0;

    let Some(sel) = sel else {
        return ub::Bounds::new(BoundsPoint::ZERO, BoundsPoint::ZERO);
    };
    let (x, y, w, h) = compute_toolbar_bounds(sel, screen_bounds);
    let mut min = BoundsPoint::new(x - PAD, y - PAD);
    let mut max = BoundsPoint::new(x + w + PAD, y + h + PAD);
    if popup_open {
        // 弹层总是朝"远离选区"的方向展开（见工具栏渲染处的 popover_open_up 判定），
        // 所以只往那一侧扩，不会把选区这一侧的画布误划进 UI 区域。
        if y + h / 2.0 < sel.origin.y + sel.size.y / 2.0 {
            min.y -= POPOVER_MAX_H;
        } else {
            max.y += POPOVER_MAX_H;
        }
    }
    ub::Bounds::new(min, max)
}

fn compute_toolbar_bounds(
    sel: ub::Bounds,
    screen_bounds: ub::Bounds,
) -> (f32, f32, f32, f32) {
    let screen_y0 = screen_bounds.origin.y;
    let screen_h = screen_y0 + screen_bounds.size.y;
    let toolbar_h = theme::m::BTN_H + theme::m::PANEL_PAD * 2.0;
    let toolbar_y_below = sel.origin.y + sel.size.y + TOOLBAR_OFFSET_Y;
    // 优先级：选区下方 → 选区上方 → 屏幕底部。
    // 选区贴屏幕底部时放下方会与选区重叠，选区的鼠标处理截获点击
    // 导致工具栏按钮点不到（用户报"点不到箭头/椭圆等按钮"）。
    let toolbar_y = if toolbar_y_below + toolbar_h + TOOLBAR_OFFSET_Y <= screen_h {
        toolbar_y_below
    } else if sel.origin.y - toolbar_h - TOOLBAR_OFFSET_Y >= screen_y0 {
        sel.origin.y - toolbar_h - TOOLBAR_OFFSET_Y
    } else {
        screen_h - toolbar_h - TOOLBAR_OFFSET_Y
    };
    let toolbar_w = toolbar_width_estimate();
    let toolbar_x = sel.origin.x.min(screen_bounds.origin.x + screen_bounds.size.x - toolbar_w - TOOLBAR_OFFSET_Y);
    (toolbar_x, toolbar_y, toolbar_w, toolbar_h)
}

/// 按钮组之间的竖直分隔线（高度约按钮的 55%，居中）
fn group_divider() -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(theme::m::BTN_H * 0.55))
        .flex_none()
        .rounded_full()
        .bg(theme::c::rgb(theme::tokens::DIVIDER))
}

/// 工具栏按钮内容：图标（+ 可选的 2 字短标签）
///
/// gpui-component 的 Custom 按钮变体**忽略** `foreground` 字段（渲染时
/// text_color 取的是 `colors.color`，即背景色），所以图标/文字颜色必须在这里
/// 显式指定，否则会继承到近乎透明的背景色而看不见。
fn btn_content(btn: ToolButton, disabled: bool, tone: ToolbarBtnStyle) -> impl IntoElement {
    let color = if disabled {
        theme::c::rgb(theme::tokens::TEXT_DISABLED)
    } else {
        match tone {
            // 实心强调底（蓝/绿）上用白字
            ToolbarBtnStyle::Accent | ToolbarBtnStyle::Success => {
                theme::c::rgb(theme::tokens::TEXT_ON_ACCENT)
            }
            ToolbarBtnStyle::Neutral | ToolbarBtnStyle::Danger => {
                theme::c::rgb(theme::tokens::TEXT)
            }
        }
    };
    let mut row = div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(5.0))
        .text_color(color)
        .child(icon_for(btn).size(px(theme::m::ICON)).text_color(color));
    if btn.shows_label() {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(theme::m::FONT_LABEL))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(color)
                .child(btn.label()),
        );
    }
    row
}

/// 工具栏按钮配色
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolbarBtnStyle {
    /// 普通按钮：深色玻璃上的极淡底，靠 hover 提亮提示可点
    Neutral,
    /// 激活工具：蓝色实心强调
    Accent,
    /// 主操作（完成）：绿色实心
    Success,
    /// 危险/次操作（取消）：常态中性，hover 泛红
    Danger,
}

impl ToolbarBtnStyle {
    /// 由按钮语义 + 是否激活推导配色
    fn for_button(btn: ToolButton, is_active: bool) -> Self {
        match btn {
            // 「完成」始终是主操作：绿色实心，视觉上最醒目
            ToolButton::Finish => ToolbarBtnStyle::Success,
            // 「取消」hover 时泛红，降低误点成本
            ToolButton::Cancel => ToolbarBtnStyle::Danger,
            _ if is_active => ToolbarBtnStyle::Accent,
            _ => ToolbarBtnStyle::Neutral,
        }
    }
}

/// 按钮尺寸档：工具栏按钮（30px 高）/ 弹层与浮窗里的 chip（26px 高）
#[derive(Clone, Copy, PartialEq, Eq)]
enum BtnSize {
    Toolbar,
    Chip,
    /// **固定宽度**的档位 chip（字号 / 粗细）：左右内边距收紧，
    /// 让固定宽度里给数字留足空间（见 [`fixed_chip_button`]）
    ChipDense,
    /// 对话框按钮：高度同工具栏，左右内边距更宽（中文按钮不至于挤）
    Dialog,
}

impl BtnSize {
    /// (高度, 圆角, 带标签时的左右内边距)
    fn metrics(self) -> (f32, f32, f32) {
        match self {
            BtnSize::Toolbar => (theme::m::BTN_H, theme::r::BTN, theme::m::BTN_PAD_X),
            BtnSize::Chip => (theme::m::CHIP, theme::r::CHIP, 7.0),
            BtnSize::ChipDense => (theme::m::CHIP, theme::r::CHIP, 4.0),
            BtnSize::Dialog => (theme::m::BTN_H, theme::r::BTN, 15.0),
        }
    }
}

/// 自绘按钮（工具栏 / 弹层 / 浮窗通用）
///
/// **为什么不用 `gpui_component::Button`**：它的 Custom 变体内部把背景色按
/// `color.mix_oklab(transparent, 0.2)` 处理，实测落到屏幕上只剩约 17% 不透明度
/// ——绿色主操作按钮（0x2BB673）渲染出来是 (30,60,53) 的暗绿，蓝色激活态同样
/// 发灰（截图逐像素核对过）。而且该变体**忽略** `foreground`，图标/文字颜色还得
/// 逐个显式指定。自绘 div 能拿到满不透明度的强调色、精确的圆角与内边距，以及
/// 常态/hover/按下三态，外观完全可控。
///
/// 交互语义不减：`on_click`、禁用（不挂回调 + 变灰 + 默认光标）、原生 tooltip
/// （`TooltipLabel`，与 Pin 标题栏同一套）。
// 参数确实多（内容/配色/尺寸/是否带文字/禁用/提示/回调），但它们都是
// "按钮的一格属性"，拆成结构体反而让 9 个调用点更啰嗦；这里显式豁免。
#[allow(clippy::too_many_arguments)]
fn ui_button(
    id: impl Into<gpui::ElementId>,
    content: impl IntoElement,
    tone: ToolbarBtnStyle,
    size: BtnSize,
    labeled: bool,
    disabled: bool,
    tooltip: Option<&'static str>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use theme::tokens as t;
    let (h, radius, pad_x) = size.metrics();
    let (bg, hover, active, fg) = if disabled {
        // 禁用：比常态更暗的底 + 灰字，且完全不响应 hover
        (
            theme::c::rgb(t::BTN_BG_DISABLED),
            theme::c::rgb(t::BTN_BG_DISABLED),
            theme::c::rgb(t::BTN_BG_DISABLED),
            theme::c::rgb(t::TEXT_DISABLED),
        )
    } else {
        match tone {
            ToolbarBtnStyle::Neutral => (
                theme::c::rgb(t::BTN_BG),
                theme::c::rgb(t::BTN_BG_HOVER),
                theme::c::rgb(t::BTN_BG_ACTIVE),
                theme::c::rgb(t::TEXT),
            ),
            ToolbarBtnStyle::Accent => (
                theme::c::rgb(t::ACCENT),
                theme::c::rgb(t::ACCENT_HOVER),
                theme::c::rgb(t::ACCENT_ACTIVE),
                theme::c::rgb(t::TEXT_ON_ACCENT),
            ),
            ToolbarBtnStyle::Success => (
                theme::c::rgb(t::SUCCESS),
                theme::c::rgb(t::SUCCESS_HOVER),
                theme::c::rgb(t::SUCCESS_ACTIVE),
                theme::c::rgb(t::TEXT_ON_ACCENT),
            ),
            // 取消/关闭：常态低调，hover 泛红（危险预警），按下实心红
            ToolbarBtnStyle::Danger => (
                theme::c::rgb(t::BTN_BG),
                theme::c::rgb(t::DANGER_SOFT),
                theme::c::rgb(t::DANGER),
                theme::c::rgb(t::TEXT),
            ),
        }
    };

    let mut el = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        // flex_none：工具栏一行放不下时按钮**不许被压缩**（图标会被压扁），
        // 宁可让工具栏整体换行/溢出，由上层决定布局。
        .flex_none()
        .h(px(h))
        .min_w(px(h))
        .rounded(px(radius))
        .text_color(fg)
        .bg(bg);
    el = if labeled {
        el.px(px(pad_x))
    } else {
        el.w(px(h)).px(px(0.0))
    };
    // 实心强调色按钮加 1px 接触影，和面板分离出层次
    if !disabled && matches!(tone, ToolbarBtnStyle::Accent | ToolbarBtnStyle::Success) {
        el = el.shadow(theme::button_shadow());
    }
    let el = if disabled {
        el.cursor_default()
    } else {
        el.cursor_pointer()
            .hover(move |s| s.bg(hover))
            .active(move |s| s.bg(active))
            .on_click(on_click)
    };
    el.when_some(tooltip, |el, text| {
        el.tooltip(move |_window, cx| {
            cx.new(|_| TooltipLabel { text: text.into() }).into()
        })
    })
    .child(content)
}

/// Popover 的 trigger 要求 `Selectable + IntoElement`，自绘按钮是普通 `Div`，
/// 用这个薄包装满足约束。选中态不参与样式（颜色已按 `ToolbarBtnStyle` 定死），
/// 仅用于满足接口。
struct ToolbarTrigger {
    el: gpui::Stateful<gpui::Div>,
    selected: bool,
}

impl gpui_component::Selectable for ToolbarTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl IntoElement for ToolbarTrigger {
    type Element = gpui::Stateful<gpui::Div>;
    fn into_element(self) -> Self::Element {
        self.el
    }
}

/// 给绘图工具构造带 Popover 的按钮 Popover
///
/// trigger = Button（工具图标 + 2 字短标签）。第一次点击 → 选中工具；
/// 再点 active 工具 → 浮出 popover。popover 内容由 active_tool 决定：
/// - Text → 字号档位 + Bold + 颜色
/// - Rectangle/Arrow/Freehand/Mosaic → 粗细档位 + 颜色
fn render_tool_button_with_popover(
    btn: ToolButton,
    is_active: bool,
    popover_open_up: bool,
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
    let tone = ToolbarBtnStyle::for_button(btn, is_active);
    let trigger = ToolbarTrigger {
        el: ui_button(
            ("tool", btn as usize),
            btn_content(btn, false, tone),
            tone,
            BtnSize::Toolbar,
            btn.shows_label(),
            false,
            Some(btn.tooltip_text()),
            move |_, _, cx| {
                let _ = weak_for_trigger.update(cx, |this, cx| {
                    if this.toolbar.active_tool != Some(btn) {
                        // 切到新工具：先提交活跃的 Text 输入，避免文字丢失，关旧弹层
                        this.finalize_text_input_if_active(cx);
                        this.toolbar.active_tool = Some(btn);
                        this.toolbar.popup = None;
                        cx.notify();
                    }
                    // 已 active：弹层开/关完全由 GPUI popover 状态驱动
                    //（trigger 的 toggle → on_open_change 回调同步 toolbar.popup）。
                    // 此前 on_click 再 toggle 会与 GPUI 双重竞争——
                    // 弹层刚被 GPUI 打开(open_change→popup=Some)又立刻被我们关掉，
                    // 表现为"弹出即消失"。
                });
            },
        ),
        selected: is_active,
    };

    let weak_content = weak.clone();
    Popover::new(("tool-popover", btn as usize))
        // 展开方向随工具栏位置切换：工具栏在选区上方→向上展开，避免
        // 弹层向下盖住截图框；否则向下展开（默认 TopLeft）。
        .anchor(if popover_open_up {
            // 向上展开：弹层底边贴住工具栏上沿（实测缝隙 ≤1px，无需再补偏移）
            gpui::Anchor::BottomLeft
        } else {
            gpui::Anchor::TopLeft
        })
        // 向下展开（默认 TopLeft 锚定）。
        // overlay_closable(false)：弹层打开后不因「鼠标还在按钮上/未移入
        // 弹层」的点击外部判定而立即消失——用户报"弹层弹出后鼠标没移动到
        // 弹层上就消失"。关闭靠：再点按钮 toggle / 切工具 / Esc。
        .overlay_closable(false)
        // appearance(false)：关掉 gpui-component 弹层自带的底色/描边/阴影/
        // padding（它跟随主题，会在深色工具栏旁多包一层浅色壳），弹层外观
        // 全部交给 `popover_panel()` 自绘。
        .appearance(false)
        .trigger(trigger)
        .open(is_open)
        .on_open_change(cx.listener(move |this, open, _w, cx| {
            if *open {
                // 仅当该按钮就是当前 active 工具时才记录弹层打开。
                // 点击「新工具」时 GPUI trigger 在 mouse_down 阶段也会
                // toggle 打开（on_open_change(true)），此时 active_tool
                // 尚未切换；若无条件设 popup=Some(kind)，旧 active 按钮
                // 若同为该 kind 会误判 is_open → 闪现兄弟按钮的弹层。
                if this.toolbar.active_tool == Some(btn) {
                    this.toolbar.popup = Some(popup_kind);
                }
            } else if this.toolbar.popup == Some(popup_kind) {
                this.toolbar.popup = None;
            }
            cx.notify();
        }))
        .content(move |_state, _window, cx| {
            // content 闭包不能捕获 view 借用（要求 'static）。
            // 每次渲染通过 weak 读当前 OverlayView 状态，确保选中态紧跟最新 toolbar。
            let weak = weak_content.clone();
            let (cur_color, cur_size, cur_weight, cur_bg, cur_lw) = weak
                .read_with(cx, |this, _| {
                    (
                        this.toolbar.current_color,
                        this.toolbar.current_size,
                        this.toolbar.current_weight,
                        this.toolbar.current_bg,
                        this.toolbar.line_width,
                    )
                })
                .unwrap_or((RGBA::new(0, 0, 0, 255), 24.0, FontWeight::Normal, RGBA::TRANSPARENT, 4.0));
            match popup_kind {
                ToolbarPopup::Text => render_text_popover_content(
                    popover_panel(),
                    cur_color,
                    cur_size,
                    cur_weight,
                    cur_bg,
                    weak,
                ),
                ToolbarPopup::Stroke => {
                    render_stroke_popover_content(popover_panel(), cur_color, cur_lw, weak)
                }
            }
        })
}

/// 简单按钮（Ocr/Scroll/Pin/Undo/Redo/Cancel/Finish，没有 Popover）
///
/// 用 Button.on_click 直接处理 click。配色由 [`ToolbarBtnStyle::for_button`]
/// 决定：Finish 恒为绿色实心（主操作），Cancel hover 泛红，激活工具蓝色实心。
fn render_simple_button(
    btn: ToolButton,
    active: bool,
    disabled: bool,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Stateful<gpui::Div> {
    let tone = ToolbarBtnStyle::for_button(btn, active);
    let weak_for_click = weak.clone();
    ui_button(
        ("action", btn as usize),
        btn_content(btn, disabled, tone),
        tone,
        BtnSize::Toolbar,
        btn.shows_label(),
        disabled,
        Some(btn.tooltip_text()),
        move |_, window, cx| {
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
                            cx.notify();
                        } else {
                            this.toolbar.active_tool = Some(ToolButton::Ocr);
                            this.ocr_rect = None;
                            // 已经有截图框 → 直接识别框内全部内容，不用再框一次
                            if let Some(sel) = this.selection.current().filter(|s| ocr_rect_usable(*s))
                            {
                                this.start_ocr_or_translate(sel, false, window, cx);
                                return;
                            }
                            cx.notify();
                        }
                    }
                    // 翻译工具：复用 OCR 的框选状态（ocr_rect / ocr_drag_start）与画布上的
                    // 选框渲染，只有"松开之后干什么"不同——省掉一整套重复的拖拽状态。
                    ToolButton::Translate => {
                        this.finalize_text_input_if_active(cx);
                        if this.toolbar.active_tool == Some(ToolButton::Translate) {
                            this.toolbar.active_tool = None;
                            this.ocr_rect = None;
                            cx.notify();
                        } else {
                            this.toolbar.active_tool = Some(ToolButton::Translate);
                            this.ocr_rect = None;
                            // 同 OCR：直接翻译当前截图框内的全部内容，不用二次框选
                            if let Some(sel) = this.selection.current().filter(|s| ocr_rect_usable(*s))
                            {
                                this.start_ocr_or_translate(sel, true, window, cx);
                                return;
                            }
                            cx.notify();
                        }
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
        },
    )
}

/// 马赛克画笔几何 → (画笔边长, 像素化块边长)
///
/// **跟随工具栏「粗细」档位**（[`crate::overlay::toolbar::LINE_WIDTHS`]，1..8，默认 3）：
/// 用户觉得遮盖面积大就调细、要一次盖住整段就调粗 —— 这是"遮盖多大一片"的直接旋钮，
/// 不用改代码。默认档（3）→ 画笔 24px、块 12px。
///
/// 两级尺寸保持 **2:1**（笔刷宽度 = 2 个块），所以调细只是把整片缩小，马赛克的
/// "颗粒感"不变 —— 之前固定 48/24 且带 36px 下限，档位调到底也缩不下去，
/// 用户反馈"马赛克区域太大"却无从调整。
///
/// 两个尺寸的关系是"一笔就能遮挡"的关键：块必须**大到跨过笔画与背景**（否则块平均
/// 就等于原文那一小块的颜色，等于没遮），笔刷要**宽到一笔扫过一片内容**。所以块有
/// 8px 下限（再小块均值就贴近原文局部色），块上限 28px（再粗就糊成一片、看不出是马赛克）。
fn mosaic_geom(lw: f32) -> (f32, u32) {
    let brush = (lw * 8.0).max(12.0).round();
    let block = (brush * 0.5).round().max(8.0);
    (brush, (block as u32).clamp(8, 28))
}

// ── 二级弹层（popover）视觉 ────────────────────────────────────────────────
//
// 弹层用 `.appearance(false)` 关掉 gpui-component 的默认样式（它跟随主题色，
// 浮在深色工具栏旁会突兀），改由下面这套自绘容器统一：深色玻璃底 + 冷白描边
// + 柔和阴影，与工具栏共用同一套令牌。内容按「分区标签 + 档位 chip / 色板网格」
// 组织——文字弹层与画图弹层结构一致，来回切换时不跳动。

/// 色板每行格子数（固定列数 → 网格整齐、弹层宽度可预期）
const SWATCH_COLS: usize = 12;
/// 色板格间距（px）
const SWATCH_GAP: f32 = 8.0;

/// 弹层内容区左右内边距（px）
const POPOVER_PAD: f32 = 10.0;

/// `字号` / `粗细` 档位行的 chip 间距（px）——比色板间距紧，给「一行排完」留余量
const CHIP_ROW_GAP: f32 = 3.0;
/// 字号档位 chip 的固定宽度（px）：11 档 → 11×28 + 10×3 = 338 ≤ 弹层内容宽 352
const FONT_SIZE_CHIP_W: f32 = 28.0;
/// 粗细档位 chip 的固定宽度（px）：内含「按线宽加粗的白线(14) + 间距(5) + 数字」≈26
/// → 8 档：8×40 + 7×3 = 341 ≤ 352
const LW_CHIP_W: f32 = 40.0;

/// 色板「当前色」判定容差（RGB 欧氏距离）
///
/// 默认红 (255,0,0) 与色板红 (230,34,34) 的距离约 49，取 60 既能覆盖这种
/// 同色系偏差，又不至于把相邻色相（相距 100+）误判成同一个。
const SWATCH_MATCH_TOLERANCE: f32 = 60.0;

/// 两个 RGBA 的 RGB 欧氏距离（忽略 alpha；alpha 由「无背景」格子单独表示）
fn color_distance(a: RGBA, b: RGBA) -> f32 {
    let dr = a.r as f32 - b.r as f32;
    let dg = a.g as f32 - b.g as f32;
    let db = a.b as f32 - b.b as f32;
    (dr * dr + dg * dg + db * db).sqrt()
}

/// 色板网格的内容宽度（12 列固定），弹层与网格共用，保证左右对齐
fn swatch_grid_width() -> f32 {
    SWATCH_COLS as f32 * theme::m::SWATCH + (SWATCH_COLS as f32 - 1.0) * SWATCH_GAP
}

/// 弹层容器：深色玻璃面板
///
/// 宽度 = 色板网格宽 + 左右内边距 + 左右 1px 描边。**描边必须算进宽度**：
/// gpui 是 border-box（宽度含内边距与描边），漏掉这 2px 会让网格比内容盒宽
/// 2px —— 右侧色块贴到描边上、与左侧 10px 留白不对称（弹层本来就要与网格左右对齐）。
fn popover_panel() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(9.0))
        .p(px(POPOVER_PAD))
        .w(px(swatch_grid_width() + POPOVER_PAD * 2.0 + 2.0))
        .bg(theme::c::rgb(theme::tokens::POPOVER_BG))
        .rounded(px(theme::r::PANEL))
        .border_1()
        .border_color(theme::c::rgb(theme::tokens::POPOVER_BORDER))
        .shadow(theme::popover_shadow())
}

/// 弹层分区标签（如「字号」「颜色」）
fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .flex_none()
        .text_size(px(theme::m::FONT_SECTION))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme::c::rgb(theme::tokens::SECTION_LABEL))
        .child(text)
}

/// chip 里的纯文本（字号/字重统一，颜色由外层 text_color 决定）
///
/// `whitespace_nowrap`：短标签（"64" / "加粗" / "稍后"）任何情况下都不许折行——
/// 档位 chip 是固定高度，一旦折成两行就会溢出按钮边框（见 [`fixed_chip_button`]）。
fn label_text(text: impl Into<gpui::SharedString>) -> impl IntoElement {
    div()
        .flex_none()
        .whitespace_nowrap()
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .line_height(gpui::relative(1.0))
        .child(text.into())
}

/// 弹层 / 浮窗里的动作按钮（图标 + 可选文字），按 `tone` 取色
///
/// 与工具栏按钮同一套令牌，只是尺寸更小（chip 高度），用于 popover 档位、
/// 滚动进度窗的「完成 / 取消」等。
fn bar_button(
    id: impl Into<gpui::ElementId>,
    content: impl IntoElement,
    tone: ToolbarBtnStyle,
    disabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    ui_button(
        id,
        content,
        tone,
        BtnSize::Chip,
        true,
        disabled,
        None,
        on_click,
    )
}

/// 弹层里的档位 chip（字号 / 线宽 / 加粗）：选中 = 蓝底白字
fn chip_button(
    id: impl Into<gpui::ElementId>,
    content: impl IntoElement,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let tone = if selected {
        ToolbarBtnStyle::Accent
    } else {
        ToolbarBtnStyle::Neutral
    };
    bar_button(id, content, tone, false, on_click)
}

/// **固定宽度**的档位 chip（字号 / 粗细）：宽度与字体无关，保证一行排得下
///
/// 动机（用户反馈）：`字号`/`粗细` 档位行原来用文字自然宽度排，在 Windows 上
/// （Segoe UI 字宽与 Linux 默认字体不同）总宽刚好越过弹层内容宽 352px →
/// `flex_wrap` 把最后一个档位挤到第二行。固定宽度后总宽是常量，字体/DPI 无关：
/// - 字号：11 × 28 + 10 × 3 = 338 ≤ 352
/// - 粗细： 8 × 40 +  7 × 3 = 341 ≤ 352
/// （回归测试见 `popover_chip_rows_fit_one_line`）
fn fixed_chip_button(
    id: impl Into<gpui::ElementId>,
    width: f32,
    content: impl IntoElement,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let tone = if selected {
        ToolbarBtnStyle::Accent
    } else {
        ToolbarBtnStyle::Neutral
    };
    // ChipDense：内边距收紧到 4px，28px 宽的字号 chip 里仍有 20px 放数字
    ui_button(
        id,
        content,
        tone,
        BtnSize::ChipDense,
        true,
        false,
        None,
        on_click,
    )
    .w(px(width))
}

/// 图标 + 文字的按钮内容（颜色由外层按钮的 text_color 统一决定）
fn icon_label_content(icon: Icon, label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(icon.size(px(13.0)))
        .child(label_text(label))
}

/// 色板点击目标：决定单击色块时改的是哪种颜色
#[derive(Clone, Copy, PartialEq, Eq)]
enum SwatchTarget {
    /// 画笔/边框颜色
    StrokeColor,
    /// 文字颜色
    TextColor,
    /// 文字背景色（alpha=0 = 无背景）
    TextBackground,
}

impl SwatchTarget {
    /// 把颜色写回工具栏状态
    fn apply_state(self, toolbar: &mut ToolbarState, color: RGBA) {
        match self {
            SwatchTarget::StrokeColor | SwatchTarget::TextColor => toolbar.current_color = color,
            SwatchTarget::TextBackground => toolbar.current_bg = color,
        }
    }
}

/// 把颜色应用到**已选中**的命令（选中后能二次改色 / 改背景）
fn apply_swatch_to_cmd(target: SwatchTarget, cmd: &mut DrawCommand, color: RGBA) {
    match (target, cmd) {
        (
            SwatchTarget::StrokeColor,
            DrawCommand::Rectangle { color: c, .. }
            | DrawCommand::Ellipse { color: c, .. }
            | DrawCommand::Arrow { color: c, .. }
            | DrawCommand::Freehand { color: c, .. },
        ) => *c = color,
        (SwatchTarget::TextColor, DrawCommand::Text { color: c, .. }) => *c = color,
        (SwatchTarget::TextBackground, DrawCommand::Text { background, .. }) => *background = color,
        _ => {}
    }
}

/// 单个色块：选中 = 蓝色描边 + 对勾（对勾按底色亮度取黑/白，保证可读）
fn swatch_box(
    id: impl Into<gpui::ElementId>,
    color: RGBA,
    selected: bool,
    on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    // 感知亮度（Rec.601）→ 决定对勾用深色还是白色
    let luma = 0.299 * color.r as f32 + 0.587 * color.g as f32 + 0.114 * color.b as f32;
    let check_color = if luma > 140.0 {
        theme::c::rgb(0x1B1E27FF)
    } else {
        theme::c::rgb(theme::tokens::TEXT_ON_ACCENT)
    };
    div()
        .id(id)
        .flex_none()
        .size(px(theme::m::SWATCH))
        .rounded(px(theme::r::CHIP - 1.0))
        .bg(gpui::rgba(rgba_u32(color)))
        .border_2()
        .border_color(if selected {
            theme::c::rgb(theme::tokens::ACCENT_BORDER)
        } else {
            theme::c::rgb(theme::tokens::SWATCH_BORDER)
        })
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(selected, |d| {
            d.child(Icon::new(IconName::Check).size(px(13.0)).text_color(check_color))
        })
        .on_mouse_down(MouseButton::Left, on_click)
}

/// 色板网格：固定 12 列（23 色 → 2 行，弹层更扁）；`with_none_tile` 时末尾追加「无背景」棋盘格
fn swatch_grid(
    cur: RGBA,
    target: SwatchTarget,
    with_none_tile: bool,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Div {
    let swatches = palette::default_palette();
    let mut grid = div().flex().flex_wrap().gap(px(SWATCH_GAP)).w(px(swatch_grid_width()));
    // 当前颜色在色板里的"最近一格"：精确相等判定会漏掉默认色——`ToolbarState`
    // 默认 `current_color = RGBA::RED`(255,0,0)，而调色板里的红是 HSV 采样出来的
    // (230,34,34)，两者不相等 → 弹层打开时**没有任何格子被标记**，用户看不出
    // 当前用的是什么颜色。改成"颜色距离最近且在阈值内"即视为选中。
    // 当前色完全透明时不做「最近色」判定：距离计算忽略 alpha，透明色会命中纯黑，
    // 出现「黑色」与「无背景」同时选中的假象（背景色默认就是透明）。
    let nearest = if with_none_tile && cur.a == 0 {
        None
    } else {
        swatches
            .iter()
            .enumerate()
            .map(|(i, &c)| (i, color_distance(c, cur)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|&(_, d)| d <= SWATCH_MATCH_TOLERANCE)
            .map(|(i, _)| i)
    };
    for (i, &c) in swatches.iter().enumerate() {
        let weak_c = weak.clone();
        let selected = nearest == Some(i);
        grid = grid.child(swatch_box(
            ("swatch", target as usize * 1000 + i),
            c,
            selected,
            move |_, _, cx| {
                let _ = weak_c.update(cx, |this, cx| {
                    target.apply_state(&mut this.toolbar, c);
                    this.apply_style_to_selected(|cmd| apply_swatch_to_cmd(target, cmd, c));
                    cx.notify();
                });
            },
        ));
    }
    if with_none_tile {
        let weak_none = weak;
        // 「无背景」：5×5 棋盘格示意透明 + 选中蓝框；放在末位（“无”读到最后）
        let is_none = cur.a == 0;
        let mut board = div().absolute().inset(px(0.0)).flex_wrap();
        let mut idx = 0_u32;
        for _ in 0..5 {
            for _ in 0..5 {
                let cell = if idx % 2 == 0 {
                    theme::c::rgb(theme::tokens::CHECKER_LIGHT)
                } else {
                    theme::c::rgb(theme::tokens::CHECKER_DARK)
                };
                board = board.child(div().size(px(4.4)).bg(cell));
                idx += 1;
            }
        }
        grid = grid.child(
            div()
                .id("swatch-none")
                .flex_none()
                .relative()
                .size(px(theme::m::SWATCH))
                .rounded(px(theme::r::CHIP - 1.0))
                .border_2()
                .border_color(if is_none {
                    theme::c::rgb(theme::tokens::ACCENT_BORDER)
                } else {
                    theme::c::rgb(theme::tokens::SWATCH_BORDER)
                })
                .overflow_hidden()
                .cursor_pointer()
                .child(board)
                .child(
                    div()
                        .absolute()
                        .inset(px(0.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::c::rgb(0x1B1E27FF))
                        .child("无"),
                )
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    let _ = weak_none.update(cx, |this, cx| {
                        target.apply_state(&mut this.toolbar, RGBA::TRANSPARENT);
                        this.apply_style_to_selected(|cmd| {
                            apply_swatch_to_cmd(target, cmd, RGBA::TRANSPARENT)
                        });
                        cx.notify();
                    });
                }),
        );
    }
    grid
}

/// 渲染文字 popover 内容：字号 + 加粗 + 文字颜色 + 背景色
fn render_text_popover_content(
    panel: gpui::Div,
    cur_color: RGBA,
    cur_size: f32,
    cur_weight: FontWeight,
    cur_bg: RGBA,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Div {
    use crate::overlay::toolbar::FONT_SIZES;

    // 1) 字号档位：**固定宽度 chip、不换行**，一行排完全部 11 档
    //    （用文字自然宽度排会在 Windows 上溢出内容宽 352px → 换行，见
    //    `fixed_chip_button` 的注释）
    let mut size_row = div().flex().items_center().gap(px(CHIP_ROW_GAP));
    for (i, &size) in FONT_SIZES.iter().enumerate() {
        let weak_s = weak.clone();
        let is_current = (cur_size - size).abs() < f32::EPSILON;
        size_row = size_row.child(fixed_chip_button(
            ("font-size", i),
            FONT_SIZE_CHIP_W,
            label_text(format!("{}", size as i32)),
            is_current,
            move |_, _, cx| {
                let _ = weak_s.update(cx, |this, cx| {
                    this.toolbar.current_size = size;
                    cx.notify();
                });
            },
        ));
    }

    // 2) 加粗开关：与「字号」标签同行，右对齐（图标 chip，选中 = 蓝底）
    let weak_bold = weak.clone();
    let bold_chip = chip_button(
        "font-bold",
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(Icon::empty().path(crate::assets::icons::BOLD).size(px(13.0)))
            .child(label_text("加粗")),
        cur_weight == FontWeight::Bold,
        move |_, _, cx| {
            let _ = weak_bold.update(cx, |this, cx| {
                this.toolbar.current_weight = match this.toolbar.current_weight {
                    FontWeight::Normal => FontWeight::Bold,
                    FontWeight::Bold => FontWeight::Normal,
                };
                cx.notify();
            });
        },
    );

    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(section_label("字号"))
        .child(bold_chip);

    panel
        .child(header)
        .child(size_row)
        .child(section_label("文字颜色"))
        .child(swatch_grid(cur_color, SwatchTarget::TextColor, false, weak.clone()))
        .child(section_label("背景色"))
        .child(swatch_grid(cur_bg, SwatchTarget::TextBackground, true, weak))
}

/// 渲染画图类 popover 内容：粗细档位 + 颜色
fn render_stroke_popover_content(
    panel: gpui::Div,
    cur_color: RGBA,
    cur_lw: f32,
    weak: gpui::WeakEntity<OverlayView>,
) -> gpui::Div {
    use crate::overlay::toolbar::LINE_WIDTHS;

    // 粗细档位：同样固定宽度 + 不换行（理由见 `fixed_chip_button`）
    let mut width_row = div().flex().items_center().gap(px(CHIP_ROW_GAP));
    for (i, &lw) in LINE_WIDTHS.iter().enumerate() {
        let weak_lw = weak.clone();
        let is_current = (cur_lw - lw).abs() < f32::EPSILON;
        width_row = width_row.child(fixed_chip_button(
            ("lw", i),
            LW_CHIP_W,
            // 线宽 chip：一条按比例加粗的线段 + 数字，比纯数字更直观
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .child(
                    div()
                        .w(px(14.0))
                        .h(px(lw.clamp(1.0, 4.0)))
                        .rounded_full()
                        .bg(gpui::rgba(0xFFFFFFFF)),
                )
                .child(label_text(format!("{}", lw as i32))),
            is_current,
            move |_, _, cx| {
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
            },
        ));
    }

    panel
        .child(section_label("粗细"))
        .child(width_row)
        .child(section_label("颜色"))
        .child(swatch_grid(cur_color, SwatchTarget::StrokeColor, false, weak))
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

/// 文本框「文字插入点距框左」的水平缩进逻辑像素 = box 左内边距(10) + Input 内边距(约8)。
/// 编辑态文本由 Input 按此缩进绘制；成图(rasterize_text)需用同一值偏移，否则文字会贴到框左。
const TEXT_BOX_INSET: f32 = 18.0;
/// Text 输入框内 Input 自己的水平内边距（gpui-component custom size 的 input_px）。
const TEXT_INPUT_PAD: f32 = 8.0;
/// 文本框左内边距（box div pl）：TEXT_BOX_INSET - TEXT_INPUT_PAD。
const TEXT_BOX_LPAD: f32 = TEXT_BOX_INSET - TEXT_INPUT_PAD;

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

/// 光标旁的浮动小徽章：坐标显示或「禁止点击」提示。
///
/// 位置默认贴光标右下 18px；贴近屏幕右/下边缘时翻到另一侧，避免徽章跑出屏幕。
/// 传 `icon` 时在文字左侧加个小图标（禁止提示用 ⊘）。
fn hud_badge(
    cursor: BoundsPoint,
    label: String,
    window: &Window,
    icon: Option<&str>,
) -> impl IntoElement {
    let win = window.bounds().size;
    // 宽度按最长内容（"1234, 5678    1920 × 1080"）估个上限，仅用于判断是否翻边
    let (bw, bh) = (210.0_f32, 26.0_f32);
    let x = if cursor.x + 18.0 + bw > f32::from(win.width) {
        cursor.x - 18.0 - bw
    } else {
        cursor.x + 18.0
    };
    let y = if cursor.y + 18.0 + bh > f32::from(win.height) {
        cursor.y - 18.0 - bh
    } else {
        cursor.y + 18.0
    };
    let mut badge = div()
        .absolute()
        .left(px(x.max(0.0)))
        .top(px(y.max(0.0)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.0))
        .px(px(8.0))
        .py(px(3.0))
        .rounded(px(5.0))
        .bg(gpui::rgba(theme::tokens::PANEL_BG))
        .border_1()
        .border_color(gpui::rgba(theme::tokens::PANEL_BORDER))
        .text_size(px(12.0))
        .text_color(gpui::rgba(theme::tokens::TEXT));
    if let Some(path) = icon {
        badge = badge.child(
            gpui::svg()
                .path(path)
                .size(px(15.0))
                .text_color(gpui::rgba(0xE5484DFF)),
        );
    }
    badge.child(label)
}

/// 记录鼠标光标位置并换算成对外的物理像素坐标（坐标徽章显示用）。
///
/// 存进视图的是**按物理像素取整后反算回逻辑像素**的位置，这样画出来的十字线
/// 正好落在徽章上那个坐标对应的像素上（不会差半个物理像素）。
/// 只在取整后的物理坐标变化时才 `notify`：高 DPI 下鼠标移动的亚像素抖动不该
/// 触发整个覆盖层重绘。
fn update_cursor_readout(
    this: &mut OverlayView,
    p: BoundsPoint,
    window: &Window,
    cx: &mut Context<OverlayView>,
) {
    let (sx, sy) = frame_scale(window, this.frame_width, this.frame_height);
    let phys = ((p.x * sx).round() as i32, (p.y * sy).round() as i32);
    let changed = this.cursor_phys != Some(phys);
    this.cursor_pos = Some(BoundsPoint::new(phys.0 as f32 / sx, phys.1 as f32 / sy));
    this.cursor_phys = Some(phys);
    if changed {
        cx.notify();
    }
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
    // 命中区**只**负责收鼠标（cursor + on_mouse_down）：观感统一由 canvas 的
    // `paint_handles` 画（圆点 + 胶囊）。元素层再画一遍方块会和 canvas 的圆点
    // 重叠出双重边缘，看着脏——之前两处都在画。
    div()
        .id(id)
        .absolute()
        .top(px(top))
        .left(px(left))
        .size(px(HANDLE_VISUAL_SIZE))
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
    // 用 push **移动** Frame 进 SmallVec：`SmallVec::from_elem(frame, 1)` 内部是
    // `ptr::write(ptr, elem.clone())`（smallvec 实现），会把整帧像素再复制一遍
    // （1080p 8MB，长图/大选区更大），而原值随即被丢弃——纯浪费。
    let mut frames: SmallVec<[Frame; 1]> = SmallVec::new();
    frames.push(Frame::new(buffer));
    Arc::new(RenderImage::new(frames))
}

/// 把 GPUI 像素坐标转成 SelectionState 用的 f32 点（utils::bounds::Point）
fn to_bounds_point(p: Point<Pixels>) -> BoundsPoint {
    BoundsPoint::new(f32::from(p.x), f32::from(p.y))
}

/// 逻辑像素 → 帧物理像素的缩放系数 (sx, sy)。
///
/// 与提交时「裁剪 / rasterize」用的是同一套映射（帧宽 ÷ 覆盖层窗口宽），所以
/// 鼠标逻辑坐标乘上它就是成图里的像素坐标——坐标徽章显示的数字因此可直接用来
/// 对像素做测量。覆盖层窗口即整块屏幕，两者在高 DPI 下并不相等（2x 屏为 2.0）。
fn frame_scale(window: &Window, frame_width: u32, frame_height: u32) -> (f32, f32) {
    let size = window.bounds().size;
    (
        frame_width as f32 / f32::from(size.width).max(1.0),
        frame_height as f32 / f32::from(size.height).max(1.0),
    )
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
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight, background, box_size, text_inset } => {
            DrawCommand::Text {
                anchor: sp(anchor),
                content: content.clone(),
                font_size: *font_size,
                color: *color,
                max_width: max_width.map(|w| w * sx),
                weight: *weight,
                background: *background,
                box_size: (box_size.0 * sx, box_size.1 * sy),
                text_inset: *text_inset * sx,
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

/// 一笔记马的渲染参数：笔迹区域 + 块大小 + 颜色。
type MosaicStroke = (
    Vec<(crate::overlay::drawing::Point, crate::overlay::drawing::Point)>,
    u32,
    crate::overlay::drawing::RGBA,
);

/// 从命令序列里取出所有马赛克笔迹，**每笔一条、各自保留颜色与块大小**。
///
/// 单独抽成函数是为了能直接测：曾经的 bug 是把所有笔迹**合并成一层**、颜色只取最后
/// 一笔的，于是"第一笔红色、第二笔改成黑色"时，第二笔落下会让第一笔也变成黑色
/// （用户报的"第一个模糊也变色了"）。
fn mosaic_strokes_of<'a>(
    cmds: impl Iterator<Item = &'a std::sync::Arc<DrawCommand>>,
) -> Vec<MosaicStroke> {
    cmds.filter_map(|cmd| match &**cmd {
        DrawCommand::Mosaic { regions, block_size, color } => {
            Some((regions.clone(), (*block_size).max(1), *color))
        }
        _ => None,
    })
    .collect()
}

/// 把一笔马赛克渲染成一层真像素（画布逻辑坐标入参，内部换算到帧物理像素）。
///
/// 与预览、提交共用 [`crate::overlay::commands::render_mosaic_stroke_pixels`]，
/// 所以三者是同一张图。
fn mosaic_layer_from_stroke(
    frame: &[u8],
    fw: u32,
    fh: u32,
    regions: &[(crate::overlay::drawing::Point, crate::overlay::drawing::Point)],
    block_size: u32,
    color: crate::overlay::drawing::RGBA,
    sx: f32,
    sy: f32,
) -> Option<(Arc<RenderImage>, ub::Bounds)> {
    use crate::overlay::drawing::Point;
    let scaled: Vec<(Point, Point)> = regions
        .iter()
        .map(|(a, b)| (Point::new(a.x * sx, a.y * sy), Point::new(b.x * sx, b.y * sy)))
        .collect();
    let (crop, cw, ch, cx0, cy0, local) = crate::overlay::commands::mosaic_aligned_crop(
        frame,
        fw,
        fh,
        &scaled,
        block_size,
    )?;
    let (pix, px, py, pw, ph) = crate::overlay::commands::render_mosaic_stroke_pixels(
        &crop,
        cw,
        ch,
        &local,
        block_size,
        color,
    )?;
    // 与预览同样：`ub::Bounds::new` 是「两对角点」，这里必须直接给 size
    let bounds = ub::Bounds {
        origin: ub::Point::new((cx0 + px) as f32 / sx, (cy0 + py) as f32 / sy),
        size: ub::Point::new(pw as f32 / sx, ph as f32 / sy),
    };
    Some((build_render_image_from_pixels(pw, ph, pix), bounds))
}

/// 生成（或复用）**已提交马赛克笔迹**的显示层，**每笔一层**。
///
/// 覆盖"松手后 → 最终提交前"这一段：这一笔已经不在 `in_progress` 里，但必须仍然看得见，
/// 否则用户会以为操作没生效。
///
/// 逐笔（而不是合并）渲染是必须的：每笔的颜色与块大小都可以不同，合并就只能取一个颜色，
/// 第二笔换色会把前面所有笔迹一起染色（"第一笔也跟着变色"）。
///
/// 缓存键是 `DrawingState.revision`（提交/撤销/重做都会 +1），所以撤销一笔马上跟着消失。
fn update_committed_mosaic_layers(
    self_: &mut OverlayView,
    window: &Window,
) -> Vec<(Arc<RenderImage>, ub::Bounds)> {
    // 先把各笔的 (regions, 块大小, 颜色) 取出来（克隆），后面要可变借用 self_
    let strokes = mosaic_strokes_of(self_.drawing.visible_commands());
    if strokes.is_empty() {
        self_.mosaic_layer = None;
        return Vec::new();
    }
    let wb = window.bounds();
    let sx = self_.frame_width as f32 / f32::from(wb.size.width).max(1.0);
    let sy = self_.frame_height as f32 / f32::from(wb.size.height).max(1.0);
    let fresh = self_
        .mosaic_layer
        .as_ref()
        .is_some_and(|c| c.revision == self_.drawing.revision && c.scale == (sx, sy));
    if fresh {
        return self_
            .mosaic_layer
            .as_ref()
            .map(|c| c.layers.clone())
            .unwrap_or_default();
    }
    let mut layers = Vec::with_capacity(strokes.len());
    for (regions, block_size, color) in &strokes {
        if let Some(layer) = mosaic_layer_from_stroke(
            &self_.frame_pixels,
            self_.frame_width,
            self_.frame_height,
            regions,
            *block_size,
            *color,
            sx,
            sy,
        ) {
            layers.push(layer);
        }
    }
    self_.mosaic_layer = Some(CommittedMosaicCache {
        revision: self_.drawing.revision,
        scale: (sx, sy),
        layers: layers.clone(),
    });
    layers
}


/// 生成（或复用）当前这一笔马赛克的**实时真像素预览层**。
///
/// 与提交路径共用 [`crate::overlay::commands::render_mosaic_stroke_pixels`]，
/// 所以拖动中看到的就是松手后的成图（有测试逐像素钉住）。
///
/// 性能：只把笔迹包围盒**外扩一圈并按块对齐**的那块像素喂给核心函数 ——
/// 对齐是必须的，块网格钉在 `bs` 整数倍上，裁剪原点错开会让块相位变化、预览与
/// 提交对不上；好处是避免每帧克隆整帧像素（1080p 8MB）。
fn update_mosaic_preview(
    self_: &mut OverlayView,
    window: &Window,
) -> Option<(Arc<RenderImage>, ub::Bounds)> {
    let Some(ip) = self_.in_progress.clone() else {
        return None;
    };
    let DrawCommand::Mosaic { regions, block_size, color } = &*ip else {
        self_.mosaic_preview = None;
        return None;
    };
    if regions.is_empty() {
        self_.mosaic_preview = None;
        return None;
    }
    let wb = window.bounds();
    let sx = self_.frame_width as f32 / f32::from(wb.size.width).max(1.0);
    let sy = self_.frame_height as f32 / f32::from(wb.size.height).max(1.0);

    // 画布逻辑坐标 → 帧物理像素
    let scaled: Vec<(crate::overlay::drawing::Point, crate::overlay::drawing::Point)> = regions
        .iter()
        .map(|(a, b)| {
            (
                crate::overlay::drawing::Point::new(a.x * sx, a.y * sy),
                crate::overlay::drawing::Point::new(b.x * sx, b.y * sy),
            )
        })
        .collect();

    // 缓存判据：笔迹只增不改 → stamp 个数 + 最后一个 stamp 就够
    let last = scaled.last().map(|(a, b)| ((a.x, a.y), (b.x, b.y)));
    let fresh = self_.mosaic_preview.as_ref().is_some_and(|c| {
        c.region_count == scaled.len() && c.last_region == last && c.scale == (sx, sy)
    });
    if fresh {
        let c = self_.mosaic_preview.as_ref()?;
        return Some((c.image.clone(), c.bounds));
    }

    // 对齐裁剪：只抠出笔迹附近的一块（对齐到块网格，相位不变），避免克隆整帧
    let (crop, cw, ch, cx0, cy0, local) = crate::overlay::commands::mosaic_aligned_crop(
        &self_.frame_pixels,
        self_.frame_width,
        self_.frame_height,
        &scaled,
        *block_size,
    )?;
    let rendered = crate::overlay::commands::render_mosaic_stroke_pixels(
        &crop,
        cw,
        ch,
        &local,
        *block_size,
        *color,
    );
    let (pix, px, py, pw, ph) = rendered?;
    // 目标 Bounds 用**逻辑像素**（画布坐标）：帧像素 / 缩放比。
    //
    // 注意：`ub::Bounds::new(from, to)` 是「两个对角点」构造函数，不是「原点+尺寸」！
    // 这里必须用结构体字面量直接给 size —— 用 `new` 传 (原点, 尺寸) 会把 size 算成
    // `尺寸 - 原点`（负数），`paint_image` 拿到负尺寸就什么都不画：预览层算得再对，
    // 屏幕上也看不到（这个 bug 就是这么来的）。
    let bounds = ub::Bounds {
        origin: ub::Point::new((cx0 + px) as f32 / sx, (cy0 + py) as f32 / sy),
        size: ub::Point::new(pw as f32 / sx, ph as f32 / sy),
    };
    let image = build_render_image_from_pixels(pw, ph, pix);
    self_.mosaic_preview = Some(MosaicPreviewCache {
        region_count: scaled.len(),
        last_region: last,
        scale: (sx, sy),
        image: image.clone(),
        bounds,
    });
    Some((image, bounds))
}

/// 增量画 Freehand 当前笔画到 freehand_incr（累积层）：
/// 每帧只画新增段；buffer 矩形覆盖整笔包围盒（origin/尺寸都取整 + 对齐 32px 网格）
/// 时直接复用，笔尖跑出 buffer 才重建。origin 取整保证绘制偏移的整数性，
/// 因此线与提交路径（rasterize_shapes）光栅化到同一像素。
/// 返回 (图, bounds) 供 paint，bounds 恒等于 buffer 矩形。
fn update_in_progress_incr(
    self_: &mut OverlayView,
    _window: &Window,
) -> Option<(Arc<RenderImage>, ub::Bounds)> {
    let Some(ip) = &self_.in_progress else {
        return self_.freehand_incr.as_ref().and_then(|st| st.image.clone());
    };
    let DrawCommand::Freehand { points, color, line_width } = &**ip else {
        return self_.freehand_incr.as_ref().and_then(|st| st.image.clone());
    };
    if points.len() < 2 {
        return None;
    }
    let lw = *line_width;
    let now = points.len();

    // 当前笔画逻辑 bbox（含 pad，与 rasterize_shapes 相同外扩）
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    let pad = lw / 2.0 + 1.0;
    let cur_min_x = min_x - pad;
    let cur_min_y = min_y - pad;
    let cur_max_x = max_x + pad;
    let cur_max_y = max_y + pad;

    // 复用判据：已有 buffer 的矩形**覆盖**当前笔画包围盒即可，不再要求逐像素相等。
    // 旧判据比对的是"旧 buffer ∪ 当前笔画 bbox"算出的 origin/size，笔尖每往外扩 1px
    // 这个并集就变一次 → 立刻掉回重建分支（vec![0; A] 清零 + 交集拷贝 + A 克隆 +
    // rgba_to_bgra + GPU 上传，约 6×缓冲字节）；而画笔只会落在 buffer 内部，图层位置
    // 完全由 bounds 决定，所以只要 buffer 覆盖住这一笔，画出来就是像素级一致的。
    let reusable = match &self_.freehand_incr {
        Some(st) => {
            st.lw == lw
                && st.origin.0 <= cur_min_x
                && st.origin.1 <= cur_min_y
                && st.origin.0 + st.frame.width as f32 >= cur_max_x
                && st.origin.1 + st.frame.height as f32 >= cur_max_y
        }
        None => false,
    };

    if reusable {
        let st = self_.freehand_incr.as_mut().unwrap();
        // 复用分支的 bounds 只能取自 buffer 自身矩形，不能重算成当前笔画的 bbox：
        // paint_raster 会把图像拉伸到 bounds，留了余量的 buffer 比 bbox 大，
        // 拿 bbox 当 bounds 会让整层线条整体错位/缩放。
        let ox = st.origin.0;
        let oy = st.origin.1;
        let bw = st.frame.width;
        let bh = st.frame.height;
        let bounds = ub::Bounds {
            origin: ub::Point::new(ox, oy),
            size: ub::Point::new(bw as f32, bh as f32),
        };
        if now > st.rendered {
            // 增量：画新段（rendered-1 起重叠点连接），Exact 两端（与全量中间顶点一致）
            let start = st.rendered.saturating_sub(1);
            let mut newpts: Vec<(f32, f32)> = Vec::with_capacity(now - start);
            for p in &points[start..now] {
                newpts.push((p.x - ox, p.y - oy));
            }
            let _ = crate::overlay::commands::draw_polyline_pub(
                &mut st.frame, &newpts, lw, *color, 1,
                crate::overlay::commands::Cap::Exact,
                crate::overlay::commands::Cap::Exact,
            );
            st.rendered = now;
            let img = build_render_image_from_pixels(bw, bh, st.frame.pixels.clone());
            // 替换旧图：拖动时每帧都换一张，不释放就是每帧漏一块瓦片
            if let Some((old, _)) = st.image.replace((img, bounds)) {
                self_.pending_image_drops.push(old);
            }
        }
        st.image.clone()
    } else {
        // 重建：拷贝旧 buffer + 续画当前笔画（已画像素由交集拷贝保留）
        //
        // 矩形 = (当前笔画包围盒 + 50% 余量, clamp 32..256) ∪ 旧 buffer 矩形，再对齐
        // 32px 网格。余量把同一笔画内的重建频率从"每外扩 1px 一次"降到"每几十~几百
        // px 一次"（1200×700 的一笔在旧逻辑下几乎每帧都要重建约 20MB 的缓冲）。
        // 余量**只按当前笔画算**：若像旧代码那样先并上旧矩形再留余量，每重建一次就
        // 再多一份余量，缓冲随重建次数线性膨胀（实测一笔 1200px 会涨到 69MB）。
        const INCR_GRID: f32 = 32.0;
        let span = (cur_max_x - cur_min_x).max(cur_max_y - cur_min_y);
        let slack = (span * 0.5).clamp(INCR_GRID, 256.0);
        let (mut all_min_x, mut all_max_x) = (cur_min_x - slack, cur_max_x + slack);
        let (mut all_min_y, mut all_max_y) = (cur_min_y - slack, cur_max_y + slack);
        // 并上旧 buffer 矩形：它可能比"当前笔画+余量"更大（同一笔此前的像素还在里面），
        // 必须完整包含它——重建仍从 rendered-1 处续画，依赖的正是旧像素被完整保留。
        // 网格对齐只向外扩，新矩形因此恒 ⊇ 旧矩形。
        if let Some(old) = &self_.freehand_incr {
            all_min_x = all_min_x.min(old.origin.0);
            all_min_y = all_min_y.min(old.origin.1);
            all_max_x = all_max_x.max(old.origin.0 + old.frame.width as f32);
            all_max_y = all_max_y.max(old.origin.1 + old.frame.height as f32);
        }
        // origin 向下取整到网格、右下角向上取整：整数 origin + 整数尺寸，
        // bounds 与 buffer 才能逐像素对应
        let ox = (all_min_x / INCR_GRID).floor() * INCR_GRID;
        let oy = (all_min_y / INCR_GRID).floor() * INCR_GRID;
        let bw = (((all_max_x / INCR_GRID).ceil() * INCR_GRID - ox) as u32).max(1);
        let bh = (((all_max_y / INCR_GRID).ceil() * INCR_GRID - oy) as u32).max(1);
        // bounds（逻辑）：必须与 buffer 矩形逐像素一致——paint_raster 按 bounds
        // 拉伸图像，对不上会让整层线条位移/缩放
        let bounds = ub::Bounds {
            origin: ub::Point::new(ox, oy),
            size: ub::Point::new(bw as f32, bh as f32),
        };
        let mut frame = CapturedFrame {
            width: bw,
            height: bh,
            pixels: vec![0; (bw * bh * 4) as usize],
        };
        if let Some(old) = &self_.freehand_incr {
            // 交集拷贝
            let ox0 = old.origin.0.max(ox);
            let oy0 = old.origin.1.max(oy);
            let ox1 = (old.origin.0 + old.frame.width as f32).min(ox + bw as f32);
            let oy1 = (old.origin.1 + old.frame.height as f32).min(oy + bh as f32);
            if ox1 > ox0 && oy1 > oy0 {
                for yy in 0..((oy1 - oy0) as u32) {
                    let sy = (oy0 + yy as f32 - old.origin.1) as usize;
                    let dy = (oy0 + yy as f32 - oy) as usize;
                    let sx = (ox0 - old.origin.0) as usize;
                    let dx = (ox0 - ox) as usize;
                    let len = ((ox1 - ox0) as usize) * 4;
                    let src = sy * old.frame.width as usize * 4 + sx * 4;
                    let dst = dy * bw as usize * 4 + dx * 4;
                    if dst + len <= frame.pixels.len() && src + len <= old.frame.pixels.len() {
                        frame.pixels[dst..dst + len]
                            .copy_from_slice(&old.frame.pixels[src..src + len]);
                    }
                }
            }
        }
        // 画当前笔画全部（重建时：整条一次画，Exact 内部 + 首尾 Full 由首帧处理）
        let start = self_.freehand_incr.as_ref().map(|s| s.rendered.saturating_sub(1)).unwrap_or(0);
        let s0 = if self_.freehand_incr.is_some() { start } else { 0 };
        if now > s0 {
            let mut newpts: Vec<(f32, f32)> = Vec::with_capacity(now - s0);
            for p in &points[s0..now] {
                newpts.push((p.x - ox, p.y - oy));
            }
            let _ = crate::overlay::commands::draw_polyline_pub(
                &mut frame, &newpts, lw, *color, 1,
                crate::overlay::commands::Cap::Exact,
                crate::overlay::commands::Cap::Exact,
            );
        }
        let img = build_render_image_from_pixels(bw, bh, frame.pixels.clone());
        // debug 而非 info：默认 filter 就是 info，留着等于每帧一次 format + stdout 写
        tracing::debug!(
            "freehand_incr: rebuild bbox=({:.0},{:.0} {}x{}) pts={}",
            ox, oy, bw, bh, now
        );
        if let Some((old, _)) = self_.freehand_incr.take().and_then(|st| st.image) {
            self_.pending_image_drops.push(old);
        }
        self_.freehand_incr = Some(IncrFreehand {
            frame,
            origin: (ox, oy),
            rendered: now,
            lw,
            image: Some((img.clone(), bounds)),
        });
        Some((img, bounds))
    }
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
    // 箭头头的底边半宽（head_w = max(2*line_width, 3)），垂直方向超出箭杆中线，需计入外扩
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
                // 与 commands.rs 箭头头公式保持一致（细线保底半宽 3px）
                max_arrow_head_w = max_arrow_head_w.max((*line_width * 2.0).max(3.0));
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
fn paint_command(cmd: &DrawCommand, window: &mut Window, cx: &mut App, scale_factor: f32, draw_glyphs: bool) {
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
        DrawCommand::Text { anchor, content, font_size, color, max_width, weight, background, box_size, text_inset } => {
                let fs = *font_size / scale_factor;
                // 行盒必须随字号缩放，避免 paint_layer 高度 < asc+descent 时字形被裁剪。
                let line_height = px(fs * 1.5);
                // 编辑态 Input 的实际文字行盒：实测（range_to_bounds）等于输入字号×1.5，即
                // 与 paint 的行盒 line_height 相同。字形相对行盒顶偏移 (lh-asc-desc)/2，两者
                // 行盒一致 → 补偿 (input_lh - line_height)/2 = 0，paint 起点 = 编辑态行盒顶。
                // 用 window.line_height()（24）或 fs×1.4 都会让补偿非零，提交后文字上下漂移。
                let input_lh = line_height;
                // 水平起点 = 框左 + text_inset（与编辑态 Input 的 `box pl + input_px` 一致），
                // 保证提交预览与编辑/成图文字横坐标一致。旧 TO_X=8 仅等于 input_px，未含
                // box 左内边距，会让提交预览文字比编辑态偏左。
                let origin_fx = anchor.x + *text_inset;
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

                // 背景：铺「实际编辑框」大小的矩形（与编辑态一致），带小圆角，画在字形之下。
                // 尺寸来自 box_size；为 0（旧命令）时退回文字测量包围盒。
                if background.a > 0 {
                    let bg = Hsla::from(rgba(rgba_u32(*background)));
                    let (bw, bh) = if box_size.0 > 0.0 && box_size.1 > 0.0 {
                        (box_size.0, box_size.1)
                    } else {
                        // 兜底：测量最宽行 advance 与行数
                        let mut bg_max_w = 0.0_f32;
                        let mut bg_lines = 0usize;
                        for lt in content.split('\n') {
                            if lt.is_empty() {
                                bg_lines += 1;
                                continue;
                            }
                            let mut run = base_run.clone();
                            run.len = lt.len();
                            let shaped = window.text_system().shape_line(
                                gpui::SharedString::from(lt),
                                px(fs),
                                &[run],
                                None,
                            );
                            bg_max_w = bg_max_w.max(shaped.width().as_f32());
                            bg_lines += 1;
                        }
                        (bg_max_w.max(fs * 0.5), bg_lines as f32 * fs * 1.5)
                    };
                    // 很小圆角：随字号缩放，但不超过框短边一半
                    let radius = px((bw.min(bh) * 0.08).max(2.0).min(bw.min(bh) * 0.5));
                    window.paint_quad(quad(
                        Bounds {
                            origin: point(px(anchor.x), px(anchor.y)),
                            size: Size::new(px(bw), px(bh)),
                        },
                        radius,
                        bg,
                        px(0.),
                        gpui::transparent_black(),
                        Default::default(),
                    ));
                }

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

                    if !draw_glyphs {
                        origin_y += line_height;
                        continue;
                    }

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
        DrawCommand::Mosaic { .. } => {
            // **空实现**：马赛克已经烤进帧像素（`apply_mosaic`），这里不需要再画任何东西。
            //
            // 这里原来画一层"亮/暗交替的格子"当示意，问题有两层：
            //  1. 它不是马赛克本身，只是"这块被马赛克过"的图标；而真马赛克已经在
            //     下面烤好了，这层示意图等于**盖在成图上**；
            //  2. 已提交的笔迹更不该再画提示。
            // 于是用户看到的是"拖动中=示意图、松手后=真像素"两张不同的图 —— 这就是
            // "鼠标移动后效果和最终成型不一致"的来源。
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

/// 「够不够大才值得识别」：小于这个尺寸的框多半是误点/误拖。
fn ocr_rect_usable(rect: ub::Bounds) -> bool {
    rect.size.x > 5.0 && rect.size.y > 5.0
}

impl OverlayView {
    /// 对给定的**逻辑坐标矩形**跑 OCR / 翻译：裁剪该区域像素 → 立刻开左图右文结果窗
    /// （右侧先显示「识别中…/翻译中…」）→ 后台线程识别/翻译并回填 + 写剪贴板 →
    /// 立即 commit 关闭遮罩。
    ///
    /// 两个入口共用同一份逻辑：
    ///   1. 点工具栏「OCR / 翻译」按钮：直接用**当前整个截图框**——用户要求不用再二次框选；
    ///   2. 在截图框内拖出更小的区域：保留"只想识别某一块"的精细能力。
    fn start_ocr_or_translate(
        &mut self,
        rect: ub::Bounds,
        translate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fw = self.frame_width;
        let fh = self.frame_height;
        let wb = window.bounds();
        let sx = self.frame_width as f32 / f32::from(wb.size.width).max(1.0);
        let sy = self.frame_height as f32 / f32::from(wb.size.height).max(1.0);
        // 裁剪选区像素构造 PinPayload（供结果窗左侧显示原图）
        let sel_px = ub::Bounds {
            origin: ub::Point::new(rect.origin.x * sx, rect.origin.y * sy),
            size: ub::Point::new(rect.size.x * sx, rect.size.y * sy),
        };
        let Ok(clipped) = CapturedFrame::clip_pixels(
            fw,
            fh,
            &self.frame_pixels,
            sel_px.origin.x as u32,
            sel_px.origin.y as u32,
            sel_px.size.x as u32,
            sel_px.size.y as u32,
        ) else {
            tracing::error!("OCR/翻译: 裁剪区域像素失败，放弃");
            return;
        };
        let pin_x = self.client_origin.x + rect.origin.x;
        let pin_y = self.client_origin.y + rect.origin.y;
        // 选区区域像素（RGBA，几百 KB）给后台线程——只克隆选区而非整帧（整帧 8MB
        // 一次性拷贝），显著减少内存与拷贝耗时。
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
        // 立即打开左图右文窗口（右侧显示"识别中…/翻译中…"）
        let _ = ensure_started().send(OverlayCommand::OpenResultPin { payload, translate });
        std::thread::spawn(move || {
            if translate {
                run_translate_and_update(region_pixels, region_w, region_h);
                return;
            }
            let text = run_ocr_sync(region_pixels, region_w, region_h);
            if !text.is_empty() {
                if let Err(e) = crate::clipboard::global().write_text(&text) {
                    tracing::error!("OCR: 结果写入剪贴板失败: {e}");
                } else {
                    tracing::info!("OCR: 结果已复制到剪贴板 ({} bytes)", text.len());
                }
            } else {
                tracing::info!("OCR: 未识别到文字");
            }
            let _ = ensure_started().send(OverlayCommand::UpdateResultPin {
                translate: false,
                text,
            });
        });
        // 立即提交关闭遮罩（不再单独开 pin；图像由结果窗左侧展示）
        self.commit(
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
        let _ = cx;
    }
}

/// 翻译工具的后台任务：确保模型就位 → OCR → 翻译 → 回填窗口 + 复制译文。
///
/// 独立成一个函数是因为 OCR 与翻译**共用同一条框选/裁剪路径**，只在"松开之后"
/// 分岔。首次使用会先下载约 110MB 模型（进度由 `translate::progress()` 暴露），
/// 这期间结果窗一直显示「翻译中…」。
fn run_translate_and_update(region_pixels: Vec<u8>, region_w: u32, region_h: u32) {
    let dir = crate::config::translate_cache_dir();
    if !crate::translate::models_ready(&dir) {
        tracing::info!("翻译: 模型未就绪，先下载到 {}", dir.display());
        if let Err(e) = crate::translate::ensure_models(&dir) {
            tracing::error!("翻译: 模型下载失败: {e}");
            let _ = ensure_started().send(OverlayCommand::UpdateResultPin {
                translate: true,
                text: format!("翻译模型下载失败：{e}"),
            });
            return;
        }
    }
    let text = run_ocr_sync(region_pixels, region_w, region_h);
    if text.trim().is_empty() {
        tracing::info!("翻译: 未识别到文字");
        let _ = ensure_started().send(OverlayCommand::UpdateResultPin {
            translate: true,
            text: "未识别到文字".to_string(),
        });
        return;
    }
    let translated = match crate::translate::translate(&text) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("翻译失败: {e}");
            format!("翻译失败：{e}")
        }
    };
    // 复制的是**译文**（微信的翻译也是给你中文）
    if let Err(e) = crate::clipboard::global().write_text(&translated) {
        tracing::error!("翻译: 译文写入剪贴板失败: {e}");
    }
    let _ = ensure_started().send(OverlayCommand::UpdateResultPin {
        translate: true,
        text: translated,
    });
}

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

    // 写出预处理后的调试 PNG。**必须门控**：这是给排查识别问题时用的，
    // 之前无条件执行——每次 OCR/翻译都要对整幅预处理图做 PNG 编码加写盘
    // （选区常几百 KB~几 MB，4K 全屏 >20MB），白花几十到几百毫秒。
    // 门控变量与 capture/linux.rs、app.rs 里的调试 dump 保持一致。
    let debug_enabled = std::env::var("SCREENSHOT_RS_DEBUG_DUMP")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let debug_path = std::env::temp_dir().join("screenshot_ocr_debug.png");
    if !debug_enabled {
        // 不写盘，直接继续识别
    } else if let Err(e) = up.save(&debug_path) {
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
        // 上一帧的绘制已经上传完成，这时释放它替换掉的图才是安全的
        for image in std::mem::take(&mut self.pending_image_drops) {
            // drop_image 返回 Result（失败只记日志），显式忽略避免 unused_must_use
            let _ = window.drop_image(image);
        }
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
            // committed 渲染所有形状（含已固化的 Freehand）——单层显示
            let committed: Vec<&DrawCommand> = self
                .drawing
                .visible_commands()
                .filter(|c| is_shape_command(c))
                .map(|c| &**c)
                .collect();
            let prev_layer = self.shape_layer_cache.take();
            self.shape_layer_cache =
                rasterize_shapes(&committed, scale_factor, window, 1).map(|(image, bounds)| {
                    ShapeLayerCache {
                        revision: drawing_revision,
                        scale_factor,
                        image,
                        bounds,
                    }
                });
            // 上一版形状层已经没有任何引用了，挂起释放
            if let Some(old) = prev_layer {
                self.pending_image_drops.push(old.image);
            }
        }
        let committed_shape_layer = self
            .shape_layer_cache
            .as_ref()
            .map(|c| (c.image.clone(), c.bounds));

        // 显示层叠加：(freehand_incr 已画 Freehand, 当前工具形状)。
        // Freehand 由增量层显示（committed 不渲染），其他工具形状全量——
        // 切到箭头/矩形等时已画线条不消失
        // 类型别名：显示层 = (已画 Freehand 层, 当前工具层)
        type ShapeLayer = Option<(Arc<RenderImage>, ub::Bounds)>;
        let in_progress_shape_layer: (ShapeLayer, ShapeLayer) =
            match &self.in_progress {
                Some(ip) if matches!(&**ip, DrawCommand::Freehand { .. }) => {
                    let cur = update_in_progress_incr(self, window);
                    (cur, None)
                }
                Some(ip) if is_shape_command(ip) => {
                    let done = self.freehand_incr.as_ref().and_then(|st| st.image.clone());
                    let fresh = rasterize_shapes(&[&**ip], scale_factor, window, 1);
                    // 这张图只在这一帧被 paint，之后没有任何状态引用它——挂起，
                    // 下一帧释放。画箭头/矩形时每次鼠标移动都会新建一张整幅光栅图。
                    if let Some((img, _)) = &fresh {
                        self.pending_image_drops.push(img.clone());
                    }
                    (done, fresh)
                }
                // 松手后 Freehand 已在 committed（单层），不再用 freehand_incr
                _ => (None, None),
            };
        // 已提交马赛克笔迹的显示层：松手后到最终提交前必须一直看得见，
        // 否则"松手即消失"，用户会以为没生效。
        let committed_mosaic_layers: Vec<(Arc<RenderImage>, ub::Bounds)> =
            update_committed_mosaic_layers(self, window);

        // 马赛克实时预览层：拖动中把**真像素**（与提交同一份实现）画在帧图之上，
        // 所以"拖动中"看到的就是松手后的成图。非马赛克工具时为 None。
        let mosaic_preview_layer: ShapeLayer = if matches!(
            &self.in_progress.as_deref(),
            Some(DrawCommand::Mosaic { .. })
        ) {
            update_mosaic_preview(self, window)
        } else {
            self.mosaic_preview = None;
            None
        };

        // 已提交的 Input 展示态：canvas 应跳过对应 Text 命令，避免文字重复
        // （已提交文字由元素层 Input 绘制）。
        let skip_canvas_idx: Option<usize> =
            if self.text_input_finalized { self.text_input_cmd_idx } else { None };

        let ocr_rect = self.ocr_rect;
        let ocr_dragging = self.ocr_drag_start.is_some();
        let dim_opacity = self.dim_opacity;
        let hover_shape = self.hover_shape;
        let forbidden_hover = self.forbidden_hover;
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
                    // 盒子宽度：左右对称留白（文本插入点距框左 = 距框右 = TEXT_BOX_INSET）。
                    // 同时它显著宽于单字内容（输入视口 ≈ adv + 10），避免编辑器左滚、
                    // 出现横向滚动条。
                    // 注意：不能再用大 MIN_W 撑宽。文字/字符少时 adv+2×inset < MIN_W，
                    // 若框强撑到 MIN_W，文字贴左、右侧空出大段背景 → 左右内边距不一致。
                    // 改为「框宽 = 文字宽 + 2×inset」，文字在框内左右居中。
                    const MIN_W: f32 = 100.0;
                    const MIN_H: f32 = 40.0;
                    let new_w = if adv_px > 0.0 {
                        (adv_px / sf + TEXT_BOX_INSET * 2.0).max(TEXT_BOX_INSET * 2.0)
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
        // 编辑态背景：文字正在输入时若选了背景色，把整个编辑框铺上该色（背景=编辑框）。
        let editing_bg = if text_editing {
            self.toolbar.current_bg
        } else {
            RGBA::TRANSPARENT
        };

        let paint_canvas = canvas(
            move |_, _, _| (in_progress, visible_cmds, committed_shape_layer, committed_mosaic_layers, in_progress_shape_layer, mosaic_preview_layer, sel_visible_idx, scale_factor, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity, hover_shape, forbidden_hover, text_editing, text_input_rect, editing_bg),
            move |_, (in_progress, visible_cmds, committed_shape_layer, committed_mosaic_layers, in_progress_shape_layer, mosaic_preview_layer, sel_visible_idx, scale_factor, skip_canvas_idx, ocr_rect, ocr_dragging, dim_opacity, hover_shape, forbidden_hover, text_editing, text_input_rect, editing_bg), window, cx| {
                // 悬停在可选中形状的描边上时，整个窗口显示小手光标（window 级光标
                // 优先级高于元素级 cursor；未悬停时不设置，让文字/手柄的 cursor 正常生效）。
                if forbidden_hover {
                    // 禁止光标：让"点不动"有明确反馈（不是没反应）
                    window.set_window_cursor_style(gpui::CursorStyle::OperationNotAllowed);
                } else if hover_shape {
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
                            // 已提交文字由 Input 元素层绘制（glyphs），此处只在 canvas 补
                            // 铺背景色带，使「选框/高亮背景色」在提交后仍可见。
                            if let DrawCommand::Text { background, .. } = cmd {
                                if background.a > 0 {
                                    paint_command(cmd, window, cx, scale_factor, false);
                                }
                            }
                            continue;
                        }
                        if is_shape_command(cmd) {
                            continue;
                        }
                        paint_command(cmd, window, cx, scale_factor, true);
                    }
                    if let Some(ref ip) = in_progress {
                        if !is_shape_command(ip) {
                            paint_command(ip, window, cx, scale_factor, true);
                        }
                    }
                    // 形状层：已提交形状（缓存）先画，in_progress 那一笔增量叠在其上
                    if let Some((img, b)) = &committed_shape_layer {
                        paint_raster(window, img, *b);
                    }
                    // 已提交的马赛克笔迹（松手后仍然可见；撤销后随之消失）。
                    // **逐笔一层、按命令顺序画**：每笔颜色/块大小可不同，合并会用同一个颜色。
                    for (img, b) in &committed_mosaic_layers {
                        paint_raster(window, img, *b);
                    }
                    // 先画已画 Freehand（freehand_incr），再画当前工具形状
                    if let Some((img, b)) = &in_progress_shape_layer.0 {
                        paint_raster(window, img, *b);
                    }
                    if let Some((img, b)) = &in_progress_shape_layer.1 {
                        paint_raster(window, img, *b);
                    }
                    // 当前这一笔马赛克的真像素预览（与提交同源，逐像素一致）
                    if let Some((img, b)) = &mosaic_preview_layer {
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
                            match cmd {
                                DrawCommand::Rectangle { rect, .. }
                                | DrawCommand::Ellipse { rect, .. } => {
                                    let a = rect.0;
                                    let b = rect.1;
                                    let bounds = ub::Bounds::new(
                                        ub::Point::new(a.x.min(b.x), a.y.min(b.y)),
                                        ub::Point::new(a.x.max(b.x), a.y.max(b.y)),
                                    );
                                    paint_handles(window, bounds);
                                }
                                DrawCommand::Arrow { from, to, .. } => {
                                    // 箭头两端是「端点」而非边框角：一律用圆点样式，
                                    // 与四角手柄观感一致。
                                    for pt in &[from, to] {
                                        window.paint_quad(quad(
                                            Bounds {
                                                origin: point(
                                                    px(pt.x - HANDLE_CORNER / 2.0),
                                                    px(pt.y - HANDLE_CORNER / 2.0),
                                                ),
                                                size: Size::new(px(HANDLE_CORNER), px(HANDLE_CORNER)),
                                            },
                                            px(HANDLE_CORNER / 2.0),
                                            Hsla::from(rgba(0xFFFFFFF2)),
                                            px(1.0),
                                            Hsla::from(rgba(theme::tokens::ACCENT)),
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

                    // 4) Editing 模式下额外画 8 个 handle（圆点 + 胶囊，见 paint_handles）
                    if mode == OverlayMode::Editing {
                        paint_handles(window, sel);
                    }

                    // 4.5) 文字框编辑态：圆角强调色描边 + 8 个手柄（与选区/形状手柄
                    // 同款：四角圆点、四边细胶囊）。
                    // 画在 canvas 层而不是元素层：元素层的 div 边框/手柄会和这里的
                    // 描边、手柄重叠成双层边，且可能被裁剪或遮挡。
                    if text_editing {
                        let tr = text_input_rect;
                        let tx = px(tr.origin.x);
                        // 编辑态背景：背景色 = 实际编辑框（有移动手柄的整个框），带小圆角。
                        if editing_bg.a > 0 {
                            let ebg = Hsla::from(rgba(rgba_u32(editing_bg)));
                            let ed = tr.size.x.min(tr.size.y).max(1.0) * 0.08;
                            window.paint_quad(quad(
                                Bounds {
                                    origin: point(tx, px(tr.origin.y)),
                                    size: Size::new(px(tr.size.x.max(1.0)), px(tr.size.y.max(1.0))),
                                },
                                px(ed.max(2.0)),
                                ebg,
                                px(0.),
                                gpui::transparent_black(),
                                Default::default(),
                            ));
                        }
                        let ty = px(tr.origin.y);
                        let tw = px(tr.size.x.max(1.0));
                        let th = px(tr.size.y.max(1.0));
                        // 边框：单圈圆角描边（原来是四条 1px 灰线，直角、发闷）。
                        // 半径与上面的编辑态背景用同一份，边框和背景不会错位。
                        let radius = px((tr.size.x.min(tr.size.y).max(1.0) * 0.08).max(2.0));
                        window.paint_quad(quad(
                            Bounds { origin: point(tx, ty), size: Size::new(tw, th) },
                            radius,
                            gpui::transparent_black(),
                            px(1.5),
                            Hsla::from(rgba(theme::tokens::ACCENT)),
                            Default::default(),
                        ));
                        // 8 个手柄（圆点 + 胶囊，与选区/形状手柄同款）
                        paint_handles(window, tr);
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

        // 光标美化：短十字参考线 + 坐标徽章 + 「禁止点击」角标（参考微信 / Snipaste）。
        // 放在画布之后、工具栏之前：工具栏弹出时天然盖住它们，互不打架；这些都是
        // 无 id / 无 handler 的纯展示 div，不参与命中测试，不会挡住底下的操作。
        if let Some(c) = self.cursor_pos {
            let (sx, sy) = frame_scale(window, self.frame_width, self.frame_height);
            let (phys_x, phys_y) = self.cursor_phys.unwrap_or((0, 0));
            // ① 十字 + 坐标只在**还没画出选区**时显示（拖框过程中保留，好让用户看尺寸）。
            //    选区一旦落定，光标要去调手柄 / 移动 / 提示禁止点击，再挂个十字和坐标
            //    只会挡住选区边界，所以这时候不显示坐标。
            //    （OCR / 翻译工具在已有选区里再拖一个小框时也要看坐标，所以一并保留。）
            let show_hud = self.selection.current().is_none()
                || self.selection.is_dragging()
                || self.ocr_drag_start.is_some();
            if show_hud {
                // 短十字：四段短臂围着光标，中间留 GAP 露出鼠标本身。之前是横贯/纵贯
                // 整个屏幕的十字，把屏幕切成四块，反而看不清选区边界（用户反馈）。
                const ARM: f32 = 16.0;
                const GAP: f32 = 5.0;
                let shade = gpui::rgba(0x00000066);
                let line = gpui::rgba(0xFFFFFFCC);
                let arms: [(f32, f32, f32, f32); 4] = [
                    (c.x - GAP - ARM, c.y, ARM, 1.0),
                    (c.x + GAP, c.y, ARM, 1.0),
                    (c.x, c.y - GAP - ARM, 1.0, ARM),
                    (c.x, c.y + GAP, 1.0, ARM),
                ];
                root = root.children(arms.iter().flat_map(|(ax, ay, aw, ah)| {
                    [
                        // 深色线偏 1px 当描边、亮色线压在上面：浅底深底都看得清
                        div().absolute().left(px(*ax + 1.0)).top(px(*ay + 1.0)).w(px(*aw)).h(px(*ah)).bg(shade),
                        div().absolute().left(px(*ax)).top(px(*ay)).w(px(*aw)).h(px(*ah)).bg(line),
                    ]
                }));

                // 坐标徽章：物理像素坐标（与成图像素一一对应）；拖框时补上「宽 × 高」
                let mut label = format!("{phys_x}, {phys_y}");
                if self.selection.is_dragging() {
                    if let Some(sel) = self.selection.current() {
                        let w = (sel.size.x * sx).round() as i32;
                        let h = (sel.size.y * sy).round() as i32;
                        label = format!("{label}    {w} × {h}");
                    }
                }
                root = root.child(hud_badge(c, label, window, None));
            } else if self.forbidden_hover {
                // ② 选区已落定 + 指针在选区外：明确的「禁止点击」角标。画布那边同时把
                //    光标换成 not-allowed，这里做双保险（部分 Linux 后端不一定映射该光标）。
                root = root.child(hud_badge(c, "禁止点击".to_string(), window, Some(crate::assets::icons::BAN)));
            }
        }

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
                        // 左内边距（TEXT_BOX_LPAD）：与右侧 input_px 对称，使文本在框内居中
                        .pl(px(TEXT_BOX_LPAD))
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
                        // 左内边距（TEXT_BOX_LPAD）：与右侧 input_px 对称，使文本在框内居中
                        .pl(px(TEXT_BOX_LPAD))
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
                        // 四条边是**移动抓取区**（6px 宽，悬停变小手、按住拖整框）：
                        // 只做命中，不画边框——边框由 canvas 画一圈圆角描边，避免
                        // 元素层的直角线和 canvas 的圆角线重叠成双层边框。
                        .child(
                            div()
                                .id("text-move-top")
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .w(px(lw))
                                .h(px(h_size))
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
                                // 恢复编辑态时要把 Text 工具重新选中，否则工具栏
                                // 按钮的"选中"态丢失（active_tool=None），二次点击
                                // 文字时不会弹出样式二级面板，而是新建一个空输入。
                                this.toolbar.active_tool = Some(ToolButton::Text);
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
                            // 点在输入框外 → 先提交活跃 Text 输入，避免文字丢失。
                            // 但若点击落在弹出样式面板（改色/改字号）或工具栏上，
                            // 不应提交——否则「点色板改颜色」会把文字退出编辑态，
                            // 且点击会穿透到命令命中检测误选到其他命令。
                            if this.toolbar.popup.is_some() || this.toolbar_hovered {
                                return;
                            }
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
                                        if let DrawCommand::Text { anchor, content, font_size, max_width, weight, color, background, box_size, text_inset: _ } = cmd {
                                            edit = Some((*idx, BoundsPoint::new(anchor.x, anchor.y), content.clone(), *font_size, *max_width, *weight, *color, *background, *box_size));
                                        }
                                        break;
                                    }
                                }
                                if let Some((idx, old_anchor, old_content, old_fs, old_mw, old_wt, old_clr, old_bg, _old_box)) = edit {
                                    tracing::debug!("mouse_down: HIT text cmd idx={}", idx);
                                    this.drawing.remove_visible(idx);
                                    this.selected_cmd_actual_idx = None;
                                    this.toolbar.active_tool = Some(ToolButton::Text);
                                    this.toolbar.popup = None;
                                    this.toolbar.current_size = old_fs;
                                    this.toolbar.current_weight = old_wt;
                                    this.toolbar.current_color = old_clr;
                                    this.toolbar.current_bg = old_bg;
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
                            tracing::info!(
                                "mouse_down: deselect. freehand_incr={} committed_cache={}",
                                this.freehand_incr.is_some(),
                                this.shape_layer_cache.is_some()
                            );
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
                            if matches!(
                                this.toolbar.active_tool,
                                Some(ToolButton::Ocr) | Some(ToolButton::Translate)
                            ) && sel.contains(p)
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
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
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
                // 记录光标（此时只裁到屏幕内）：十字参考线要跟手走遍全屏，
                // 所以不能等下面裁到选区之后才记录。
                update_cursor_readout(this, p, window, cx);
                // 选区画出来之后，选区外（工具栏/二级弹层除外）属于「禁止点击」区：
                // 这里只算状态，光标与角标在画布/渲染端体现；真正拦掉点击的是
                // on_mouse_down 里 "Editing 模式下已有截图框时禁止点击框外区域" 那段。
                // 工具栏与二级弹层所在的整片区域不算"选区外"：工具栏经常被摆到选区
                // 之外（选区很小或贴着屏幕边时），只按"是否在选区内"判断会把悬停工具栏
                // 误判成选区外，于是鼠标一进操作栏就冒 🚫（用户反馈"操作栏也需要正常显示"）。
                let ui = ui_zone(
                    this.selection.current(),
                    this.screen_bounds,
                    this.toolbar.popup.is_some(),
                );
                let forbid = this.mode == OverlayMode::Editing
                    && this.selection.current().map(|s| !s.contains(p) && !ui.contains(p)).unwrap_or(false);
                if forbid != this.forbidden_hover {
                    this.forbidden_hover = forbid;
                    cx.notify();
                }
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
                    // OCR/翻译 框选结束 → 交给共用入口（提取像素、开结果窗、后台识别）
                    if this.ocr_drag_start.is_some() {
                        this.ocr_drag_start = None;
                        let translate = this.toolbar.active_tool == Some(ToolButton::Translate);
                        match this.ocr_rect {
                            Some(rect) if ocr_rect_usable(rect) => {
                                this.start_ocr_or_translate(rect, translate, window, cx);
                            }
                            _ => {
                                tracing::info!("OCR/翻译: 框选区域过小或为空，忽略");
                                this.ocr_rect = None;
                            }
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
                    tracing::info!(
                        "ESC in root on_key_down: mode={:?} active_tool={:?} popup={:?}",
                        this.mode, this.toolbar.active_tool, this.toolbar.popup
                    );
                    // 只有「确实在显示的弹层」（弹层类型与 active_tool 匹配）才只收弹层。
                    // 若按钮选中但无弹层（popup 与 active_tool 错位 / 弹层已关但 popup 残留，
                    // 或 active_tool=None），一律放行 → 取消截图窗口，
                    // 避免"选中了工具却没弹层时 Esc 无效关不掉窗口"。
                    let popup_showing = matches!(
                        (this.toolbar.active_tool, this.toolbar.popup),
                        (Some(ToolButton::Text), Some(ToolbarPopup::Text))
                            | (Some(ToolButton::Rectangle), Some(ToolbarPopup::Stroke))
                            | (Some(ToolButton::Ellipse), Some(ToolbarPopup::Stroke))
                            | (Some(ToolButton::Arrow), Some(ToolbarPopup::Stroke))
                            | (Some(ToolButton::Freehand), Some(ToolbarPopup::Stroke))
                            | (Some(ToolButton::Mosaic), Some(ToolbarPopup::Stroke))
                    );
                    if popup_showing {
                        tracing::info!("ESC closes popover only (popup_showing=true)");
                        this.toolbar.popup = None;
                        cx.notify();
                        return;
                    }
                    tracing::info!("ESC canceling overlay window (popup_showing=false)");
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
///
/// Pin 窗口是透明窗口（桌面从窗口后面透出），tooltip 若用半透明底会和桌面
/// 颜色混色、边缘发虚，因此这里用**不透明**深色底 + 柔和阴影，去掉原来那条
/// 53% 透明度的 1px 描边（半透明边框叠半透明底会读成"双层边"）。
struct TooltipLabel {
    text: gpui::SharedString,
}

impl Render for TooltipLabel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .max_w(px(220.0))
            .bg(rgba(0x2A2F3BFF))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgba(0xFFFFFF1F))
            .shadow_sm()
            .text_color(rgba(0xF3F5FAFF))
            .text_size(px(12.0))
            .child(self.text.clone())
    }
}

/// Pin 窗口视图：显示固定到桌面的标注截图
///
/// 视觉规范与工具栏同源（[`theme`] 令牌）：顶部一条 32px 深色标题栏
/// （置顶开关 + 尺寸信息 + 最小化/最大化/关闭），下方图片区**满幅 1:1**
/// 绘制——根容器不再画 1px 描边，否则内容盒被吃掉 2px、图片被缩放出
/// 一点点模糊（见下方 `paint_canvas` 注释）。
struct PinWindowView {
    image: Arc<RenderImage>,
    focus_handle: FocusHandle,
    is_always_on_top: bool,
    /// 放大前的窗口矩形（物理像素：left, top, 宽, 高）；`Some` = 当前处于放大态。
    ///
    /// 只有 Windows 需要自己记状态：那边的「放大」是自绘实现（固定尺寸窗口
    /// 没有 `WS_MAXIMIZEBOX`，Win32 不认 `ShowWindow(SW_MAXIMIZE)`，见
    /// [`toggle_pin_maximize`]）。Linux 交给窗口管理器（EWMH），无需回读。
    /// 放大期间窗口会被临时置顶（否则压不住 topmost 的任务栏），
    /// 还原时按 `is_always_on_top` 恢复 z 序。
    #[cfg(target_os = "windows")]
    maximize_restore: Option<(i32, i32, i32, i32)>,
}

/// Pin 窗口标题栏高度
///
/// 窗口高度 = 图片高 + 这个值；图片从 client y = 该值处开始绘制（根容器不再
/// 画描边）。窗口尺寸计算（[`open_pin_in_app`]）与视图渲染共用此常量，
/// 改一处即可，不会再出现"视图改了高度、窗口尺寸没跟着改"的错位。
const PIN_TITLEBAR_H: f32 = 32.0;

/// Pin 标题栏按钮尺寸（24px 见方：32px 标题栏内留 4px 上下呼吸）
const PIN_TITLE_BTN: f32 = 24.0;

/// Pin 标题栏按钮配色
#[derive(Clone, Copy, PartialEq, Eq)]
enum PinBtnTone {
    /// 普通按钮：无底色，hover 才浮出一层浅底
    Neutral,
    /// 已开启状态（置顶）：蓝色实心 + 白图标，一眼可辨
    On,
    /// 危险操作（关闭）：hover 变红
    Danger,
}

/// Pin 标题栏按钮：图标 + tooltip + hover/按下三态
///
/// 用 `.hover()`/`.active()` 直接表达三态，不再像旧实现那样把
/// `hovered_button` 存进实体再 `cx.notify()` 重绘——状态存在实体里会让
/// 每次鼠标进出都触发整窗重绘（固定窗口可能很大），且四个按钮重复四份
/// 样板代码、容易漏改。
fn pin_title_button(
    id: &'static str,
    icon: Icon,
    tone: PinBtnTone,
    tooltip: &'static str,
    on_mouse_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    use theme::tokens as t;
    let (bg, hover, active, fg) = match tone {
        PinBtnTone::Neutral => (
            None,
            theme::c::rgb(t::BTN_BG_HOVER),
            theme::c::rgb(t::BTN_BG_ACTIVE),
            theme::c::rgb(t::TEXT_MUTED),
        ),
        PinBtnTone::On => (
            Some(theme::c::rgb(t::ACCENT)),
            theme::c::rgb(t::ACCENT_HOVER),
            theme::c::rgb(t::ACCENT_ACTIVE),
            theme::c::rgb(t::TEXT_ON_ACCENT),
        ),
        PinBtnTone::Danger => (
            None,
            theme::c::rgb(t::DANGER),
            theme::c::rgb(t::DANGER_ACTIVE),
            theme::c::rgb(t::TEXT_ON_ACCENT),
        ),
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .size(px(PIN_TITLE_BTN))
        .rounded(px(6.0))
        .cursor_pointer()
        .when_some(bg, |d, c| d.bg(c))
        .hover(move |d| d.bg(hover))
        .active(move |d| d.bg(active))
        .tooltip(move |_window, cx| {
            cx.new(|_| TooltipLabel { text: tooltip.into() }).into()
        })
        .child(icon.size(px(15.0)).text_color(fg))
        .on_mouse_down(MouseButton::Left, on_mouse_down)
}

impl PinWindowView {
    fn new(frame: CapturedFrame, window: &mut Window, cx: &mut Context<Self>) -> Self {
        tracing::info!(
            "[Pin] PinWindowView::new: frame={}x{}",
            frame.width, frame.height
        );
        let image = build_render_image_from_pixels(frame.width, frame.height, frame.pixels);
        // 这张图在整个窗口生命周期里只被 paint、从不被替换，所以没有"上一帧的旧图"
        // 可挂起——泄漏发生在窗口销毁时：Arc 被丢，可 `RenderImage` 没有 Drop，
        // atlas 瓦片会一直留到进程退出（每固定一张图漏一块整图瓦片）。
        // 因此注册系统级关窗回调（Alt+F4 / 任务栏关闭 / WM_CLOSE），在窗口真正
        // 消失前把它从 atlas 摘掉。`drop_image` 只是删除缓存条目（图若再画会重新
        // 上传），幂等且安全。
        // 注意：自绘标题栏的关闭按钮与 Esc 走 `Window::remove_window()`，只在本帧
        // 标记 removed、不触发该回调，那两条路径在 render 里单独释放。
        let image_for_close = image.clone();
        window.on_window_should_close(cx, move |window, _cx| {
            let _ = window.drop_image(image_for_close.clone());
            true
        });
        Self {
            image,
            focus_handle: cx.focus_handle(),
            is_always_on_top: false,
            #[cfg(target_os = "windows")]
            maximize_restore: None,
        }
    }
}

impl Render for PinWindowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::assets::icons as app_icon;

        let image = self.image.clone();
        // 自绘关闭按钮 / Esc 走 `window.remove_window()`（只标记本帧移除，不触发
        // 系统关窗回调），所以这两条路径各自带一份 Arc，在关窗前把 atlas 瓦片摘掉。
        // 视图销毁后没人再持有这张图，不释放就随窗口一起漏在 atlas 里。
        let image_for_btn_close = self.image.clone();
        let image_for_esc_close = self.image.clone();
        let focus_handle = self.focus_handle.clone();
        let is_on_top = self.is_always_on_top;
        let entity = cx.entity().downgrade();
        // 固定出来的图片尺寸（物理像素）：贴在标题栏中间，用户一眼知道
        // 这张固定图多大（旧版标题栏中间是纯空白，只有拖拽功能）。
        let img_size = image.size(0);
        let size_label = theme::format_size(img_size.width.0 as f32, img_size.height.0 as f32);

        // 图片画布：`paint_canvas.flex_1()` 独占标题栏以下的全部空间，且根容器
        // **不画描边**——边框会按 border-box 吃掉内容盒 2px，图片因此被缩放
        // 到 (w-2)×(h-2) 渲染，"1:1 固定"就名不副实了。
        //
        // 绘制时**等比贴合 + 居中**（不是拉伸到 bounds）：正常尺寸下窗口就是按图片
        // 尺寸开的，贴合矩形 == bounds（与拉伸等价、仍是 1:1）；「最大化」后窗口
        // 比例 ≠ 图片比例，拉伸会把图片压扁，必须自己算贴合矩形（见 `contain_scale`）。
        let paint_canvas = canvas(
            move |_, _, _| image.clone(),
            move |bounds, image, window, _cx| {
                let dims = image.size(0);
                let sf = window.scale_factor();
                let img_w = dims.width.0 as f32 / sf;
                let img_h = dims.height.0 as f32 / sf;
                let k = contain_scale(
                    img_w,
                    img_h,
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                );
                let fit_w = px(img_w * k);
                let fit_h = px(img_h * k);
                let fit = Bounds {
                    origin: point(
                        bounds.origin.x + (bounds.size.width - fit_w) / 2.0,
                        bounds.origin.y + (bounds.size.height - fit_h) / 2.0,
                    ),
                    size: Size::new(fit_w, fit_h),
                };
                let _ = window.paint_image(fit, Default::default(), image.clone(), 0, false);
            },
        );

        let entity_for_top = entity.clone();
        let entity_for_max = entity.clone();
        // 放大态下按钮语义变「还原」。只有 Windows 自持该状态（见
        // `PinWindowView::maximize_restore`）；Linux 由窗口管理器持有，无法回读。
        #[cfg(target_os = "windows")]
        let max_tip: &'static str = if self.maximize_restore.is_some() {
            "还原"
        } else {
            "最大化"
        };
        #[cfg(not(target_os = "windows"))]
        let max_tip: &'static str = "最大化";

        div()
            .track_focus(&focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::c::rgb(theme::tokens::PANEL_BG))
            .child(
                // ── 自定义标题栏 ────────────────────────────────────────
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_none()
                    .h(px(PIN_TITLEBAR_H))
                    .px(px(4.0))
                    .gap(px(2.0))
                    .bg(theme::c::rgb(theme::tokens::PANEL_BG))
                    .border_b_1()
                    .border_color(theme::c::rgb(theme::tokens::DIVIDER))
                    // 左侧：置顶开关（开启=蓝色实心）
                    .child(pin_title_button(
                        "pin-always-on-top",
                        Icon::empty().path(if is_on_top {
                            app_icon::PIN
                        } else {
                            app_icon::PIN_OFF
                        }),
                        if is_on_top {
                            PinBtnTone::On
                        } else {
                            PinBtnTone::Neutral
                        },
                        if is_on_top { "取消置顶" } else { "置顶" },
                        move |_ev: &MouseDownEvent, window: &mut Window, app: &mut App| {
                            let new_state = !is_on_top;
                            #[cfg(any(target_os = "linux", target_os = "windows"))]
                            send_wm_state_above(window, new_state);
                            let _ = entity_for_top.update(app, |this, cx| {
                                this.is_always_on_top = new_state;
                                cx.notify();
                            });
                        },
                    ))
                    // 中间：尺寸信息 + 可拖拽空白区
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .pl(px(6.0))
                            .text_size(px(11.0))
                            .text_color(theme::c::rgb(theme::tokens::TEXT_MUTED))
                            .child(size_label)
                            .on_mouse_down(
                                MouseButton::Left,
                                move |_ev: &MouseDownEvent, window: &mut Window, _app: &mut App| {
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
                    // 右侧：最小化 / 最大化 / 关闭
                    // 图标用「-」和「口」的窗口控制字形（与系统/组件标题栏一致），
                    // 不用 Lucide 的 minimize-2 / maximize-2（两个斜向箭头，容易被
                    // 当成「缩到角落」「放大图片」而不是窗口最小化/最大化）。
                    .child(pin_title_button(
                        "pin-minimize",
                        Icon::empty().path(crate::assets::icons::MINUS),
                        PinBtnTone::Neutral,
                        "最小化",
                        move |_ev: &MouseDownEvent, window: &mut Window, _app: &mut App| {
                            #[cfg(any(target_os = "linux", target_os = "windows"))]
                            pin_minimize_window(window);
                        },
                    ))
                    .child(pin_title_button(
                        "pin-maximize",
                        // 「口」= 组件内置矩形工具的同一字形（方形描边）
                        Icon::empty().path(crate::assets::icons::SQUARE),
                        PinBtnTone::Neutral,
                        max_tip,
                        move |_ev: &MouseDownEvent, window: &mut Window, app: &mut App| {
                            // ── Windows：自绘「放大 / 还原」──────────────────────
                            // Pin 窗口是固定尺寸窗口（没有 WS_MAXIMIZEBOX），
                            // `ShowWindow(SW_MAXIMIZE)` 在 Win32 里是空操作——
                            // 旧实现正因此「点了没反应」，改成自己算几何 + SetWindowPos：
                            // 铺满**整屏**（rcMonitor，含任务栏）并临时置顶，见
                            // `toggle_pin_maximize`。
                            //
                            // 必须延后到 App 借期外执行：SetWindowPos 会**同步**派发
                            // WM_SIZE / WM_MOVE，gpui 的 resize/moved 回调要重新借用
                            // App，在这里直接调会报 "RefCell already borrowed"
                            //（与 `schedule_client_top_adjustment` 同一原因）。
                            #[cfg(target_os = "windows")]
                            {
                                let Some(hwnd) = window_hwnd(window) else { return };
                                let weak = entity_for_max.clone();
                                let hwnd_key = hwnd as usize;
                                app.spawn(async move |cx| {
                                    // 等一帧，确保已退出当前鼠标事件回调（App 借期）
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(16))
                                        .await;
                                    // 状态在任务里读（而不是点击时读）：连点两次时以最新
                                    // 状态为准，不会用「放大前」的旧值再放大一次。
                                    // `on_top` = 标题栏「置顶」开关的当前状态，还原时用它
                                    // 恢复 z 序（放大期间窗口是被我们临时置顶的）。
                                    let (restore, on_top) = weak
                                        .read_with(cx, |this, _| {
                                            (this.maximize_restore, this.is_always_on_top)
                                        })
                                        .unwrap_or((None, false));
                                    let state = toggle_pin_maximize(
                                        hwnd_key as *mut core::ffi::c_void,
                                        restore,
                                        on_top,
                                    );
                                    let _ = weak.update(cx, |this, cx| {
                                        this.maximize_restore = state;
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }
                            // Linux：交给窗口管理器（EWMH _NET_WM_STATE_MAXIMIZED_*）
                            #[cfg(target_os = "linux")]
                            pin_toggle_maximize(window);
                        },
                    ))
                    .child(pin_title_button(
                        "pin-close",
                        Icon::new(IconName::Close),
                        PinBtnTone::Danger,
                        "关闭",
                        // 关闭按钮固定在拖拽区之外：鼠标按下即关，不等抬起。
                        move |_ev: &MouseDownEvent, window: &mut Window, _app: &mut App| {
                            let _ = window.drop_image(image_for_btn_close.clone());
                            window.remove_window();
                        },
                    )),
            )
            .child(paint_canvas.flex_1())
            .on_key_down(move |ev: &KeyDownEvent, window, _cx| {
                if ev.keystroke.key == "escape" {
                    let _ = window.drop_image(image_for_esc_close.clone());
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


// ── Windows 版 置顶 / 最小化 / 最大化（Pin 窗口标题栏按钮）。
// GPUI 的 WindowKind::Normal + 自绘标题栏不会把这三个按钮接到 Win32，这里显式调用：
// `window_hwnd` 取窗口令牌；SetWindowPos 置顶、ShowWindow 最小化/最大化。
#[cfg(target_os = "windows")]
fn send_wm_state_above(window: &mut Window, add: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    if let Some(h) = window_hwnd(window) {
        unsafe {
            SetWindowPos(
                h,
                if add { HWND_TOPMOST } else { HWND_NOTOPMOST },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn pin_minimize_window(window: &mut Window) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE};
    if let Some(h) = window_hwnd(window) {
        unsafe {
            ShowWindow(h, SW_MINIMIZE);
        }
    }
}

/// Pin 窗口「放大 / 还原」（Windows 自绘实现）
///
/// **为什么不用 `ShowWindow(SW_MAXIMIZE)`**：Pin 窗口按「截取的实际尺寸」1:1 创建
/// （`is_resizable: false`），gpui_windows 因此不给它加 `WS_THICKFRAME` /
/// `WS_MAXIMIZEBOX`；而 Win32 只对「可缩放」窗口执行最大化 —— 旧实现直接调
/// `ShowWindow`，在 Windows 上就是**空操作**（用户反馈「固定窗口点放大没效果」）。
///
/// 这里自己算几何：
/// - **放大**：记住当前窗口矩形 → `SetWindowPos` 把窗口铺满当前显示器**整屏**
///   （`rcMonitor`，含任务栏区域）并**临时置顶** —— 任务栏本身是 topmost 窗口，
///   普通窗口即使盖在它上面也会被它压在下面，不置顶就「铺不满」；图片由画布
///   等比贴合居中显示，不会被拉伸变形（见 `PinWindowView` 的 `paint_canvas`）；
/// - **还原**：`SetWindowPos` 回记住的矩形，并把 z 序恢复成 `on_top`（标题栏
///   「置顶」开关的当前状态）；放大期间那个开关的图标可能显示为「未置顶」，
///   但窗口实际是置顶的（再点一次「置顶」会切回未置顶，任务栏随之露出）。
///
/// 返回新的状态：`Some(矩形)` = 已放大（矩形供还原），`None` = 已还原。
///
/// 调用方必须**在 App 借期外**调用（见 `pin-maximize` 按钮处的说明）。
#[cfg(target_os = "windows")]
fn toggle_pin_maximize(
    hwnd: *mut core::ffi::c_void,
    restore: Option<(i32, i32, i32, i32)>,
    on_top: bool,
) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST,
        SWP_NOACTIVATE,
    };

    unsafe {
        // ── 还原：回到放大前的位置与尺寸，并恢复原有 z 序 ───────────────────
        if let Some((x, y, w, h)) = restore {
            SetWindowPos(
                hwnd,
                if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST },
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE,
            );
            tracing::info!("[Pin] restore to ({x},{y}) {w}x{h} topmost={on_top}");
            return None;
        }

        // ── 放大：窗口铺满当前显示器整屏（含任务栏）────────────────────────
        let mut wr: RECT = std::mem::zeroed();
        let mut cr: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0 || GetClientRect(hwnd, &mut cr) == 0 {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
            return None;
        }
        // rcMonitor = 整屏（含任务栏区域），rcWork = 工作区（留出任务栏）。
        // 用户要的是「铺满整屏」，所以取 rcMonitor；配合下面的 HWND_TOPMOST 才能真正
        // 盖住任务栏（任务栏是 topmost，非置顶窗口压不住它）。
        let target = mi.rcMonitor;
        // 非客户区：这个自绘标题栏窗口实际是 **左 8 / 上 0 / 右 8 / 下 8**
        //（日志实测 frame=16x8；gpui 的 WM_NCCALCSIZE 处理只吃掉了上边框）。
        // 要让**客户区**（我们绘制的面板）铺满整屏，窗口矩形必须：
        //   尺寸 = 目标 + 非客户区总量；位置 = 目标 − 客户端在窗口内的偏移。
        // 只加尺寸不反向偏移的话，客户端会整体右下偏 8px —— 左边露出 8px 桌面、
        // 图片右边被屏幕裁掉 8px（用户反馈「没铺满」就是这个）。
        let mut pt: POINT = std::mem::zeroed();
        if ClientToScreen(hwnd, &mut pt) == 0 {
            return None;
        }
        let nc_left = pt.x - wr.left;
        let nc_top = pt.y - wr.top;
        let frame_w = (wr.right - wr.left) - cr.right;
        let frame_h = (wr.bottom - wr.top) - cr.bottom;
        let win_w = (target.right - target.left) + frame_w;
        let win_h = (target.bottom - target.top) + frame_h;
        let (x, y) = (target.left - nc_left, target.top - nc_top);

        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            win_w,
            win_h,
            SWP_NOACTIVATE,
        );
        tracing::info!(
            "[Pin] maximize -> window=({x},{y}) {win_w}x{win_h} topmost \
             (client={}x{} at ({},{}) target=({},{}) {}x{}, nc={nc_left},{nc_top} \
             frame={frame_w}x{frame_h})",
            win_w - frame_w,
            win_h - frame_h,
            x + nc_left,
            y + nc_top,
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
        );
        Some((wr.left, wr.top, wr.right - wr.left, wr.bottom - wr.top))
    }
}

/// 等比 contain 的缩放系数：图片按此系数缩放后能**完整放进** `avail_w × avail_h`。
///
/// Pin 窗口「最大化」后窗口比例 ≠ 图片比例（窗口铺满屏幕），画布不能拉伸到
/// bounds（会把图片压扁），而要按这个系数贴合 + 居中（空出来的部分露出面板底色）。
/// 正常尺寸下窗口就是按图片开的，系数恒为 1（贴合矩形 == 画布，仍是 1:1）。
/// 退化输入（0/负尺寸）返回 1，绝不产生 0 或负的绘制尺寸。
fn contain_scale(img_w: f32, img_h: f32, avail_w: f32, avail_h: f32) -> f32 {
    if img_w <= 0.0 || img_h <= 0.0 || avail_w <= 0.0 || avail_h <= 0.0 {
        return 1.0;
    }
    (avail_w / img_w).min(avail_h / img_h)
}

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
    /// 打开模型管理窗口（OCR 档位 + 翻译模型：状态 / 下载 / 进度）
    OpenOcrModels,
    /// 打开系统设置窗口（热键查看与替换 / 版本 / 检查更新）
    OpenSettings,
    /// 打开/重用 OCR 识别窗口（左图右文，类似微信文字识别）：左侧选区图 + 右侧结果区
    OpenResultPin {
        payload: PinPayload,
        /// true=翻译结果窗（标题「翻译」、等待文案「翻译中…」）
        translate: bool,
    },
    /// 更新当前 OCR 窗口的右侧文字（后台识别完成后调用）
    UpdateResultPin {
        translate: bool,
        text: String,
    },
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
    /// 启动时检查到新版本，弹「发现新版本」提示窗（用户确认后调用 update 下载替换）
    PromptUpdate {
        /// 新版本号（如 "0.2.0"），仅用于展示
        new_version: String,
    },
}

/// 进程级唯一的 GPUI 服务：持有命令 Sender，首次使用时才拉起 GPUI 线程。
///
/// 常驻单应用（`QuitMode::Explicit`）是修复「Windows 截图后进程被
/// gpui_windows 的 `ExitProcess(0)` 杀掉」的关键：截图/固定都在同一个
/// `application().run()` 内创建/销毁窗口，事件循环永不退出。
#[derive(Clone)]
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

    /// 打开模型管理窗口（fire-and-forget）。
    pub fn open_ocr_models(&self) {
        let _ = self.cmd.send(OverlayCommand::OpenOcrModels);
    }

    /// 打开系统设置窗口（fire-and-forget）。
    pub fn open_settings(&self) {
        let _ = self.cmd.send(OverlayCommand::OpenSettings);
    }

    /// 启动时检查到新版本：弹「发现新版本」提示窗，让用户确认是否下载更新。
    pub fn prompt_update(&self, new_version: String) {
        let _ = self.cmd.send(OverlayCommand::PromptUpdate {
            new_version,
        });
    }

    /// 打开自动滚动截屏进度小窗（主线程调用，不阻塞）
    ///
    /// `done` 由用户点「完成」置 true：自动模式结束滚动并生成拼接内容到剪贴板；
    /// `cancel` 置 true 则直接取消、不生成。自动模式引擎每轮自动滚动，故「完成」
    /// 按钮始终可点（不按 `moving` 门控）。
    pub fn open_scroll_progress(
        &self,
        cancel: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
        region_px: ub::Bounds,
        screen_px: ub::Bounds,
    ) {
        let _ = self.cmd.send(OverlayCommand::ShowProgress {
            cancel,
            done,
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
    // 资源源：**项目自带图标**（assets/icons/ui，路径前缀 app-icons/）优先，
    // 其余回退 gpui-component 内置的 Lucide 图标集（icons/*.svg，IconName::XXX）。
    // 见 `crate::assets::AppAssets`：不注册资源源时 svg 取不到字节，
    // 按钮只剩文字、图标全空。
    application()
        .with_assets(crate::assets::AppAssets)
        // QuitMode::Explicit：窗口关闭不自动退出；只有显式 cx.quit() 才结束
        // 事件循环。这是避免 gpui_windows::WindowsPlatform::run 末尾
        // ExitProcess(0) 杀进程的关键。
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            // gpui-component 必须在第一个窗口前初始化，否则全局主题/状态会 panic
            gpui_component::init(cx);

            // 全局切到**深色主题**：截图工具栏、二级弹层、Pin 标题栏、滚动进度窗、
            // OCR 面板都是深色玻璃风格，组件默认的浅色主题会让它们内部弹出的
            // 组件（tooltip / Button 变体 / 滚动条 / 文本选区）白得发亮、风格割裂。
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

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
                // 翻译结果窗（与 OCR 结果窗互不干扰，各自只保留一个）
                let mut trans_pin: Option<WindowHandle<gpui_component::Root>> = None;
                let mut ocr_models: Option<WindowHandle<gpui_component::Root>> = None;
                // 系统设置窗口（同一时刻只保留一个）
                let mut settings_window: Option<WindowHandle<gpui_component::Root>> = None;
                let mut update_prompt: Option<WindowHandle<gpui_component::Root>> = None;
                // UI 视觉调试：SCREENSHOT_RS_UI_PROBE=pin|progress|ocr|update 时，
                // 应用一起来就把对应的独立窗口摆出来，方便用截图脚本核对样式
                // （正常使用不带该环境变量，这里是空操作）。
                probe_aux_windows(&mut progress, &mut update_prompt, async_cx);
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
                        Ok(OverlayCommand::OpenSettings) => {
                            // 系统设置窗口同样只留一个：已存在就聚焦，不重复开
                            let alive = if let Some(h) = &settings_window {
                                h.update(async_cx, |_, window, _| {
                                    window.activate_window();
                                    true
                                })
                                .unwrap_or(false)
                            } else {
                                false
                            };
                            if !alive {
                                match async_cx.update(open_settings_in_app) {
                                    Ok(h) => settings_window = Some(h),
                                    Err(e) => tracing::error!("[overlay] 打开系统设置窗口失败: {e}"),
                                }
                            }
                        }
                        Ok(OverlayCommand::PromptUpdate { new_version }) => {
                            // 始终只有一个更新提示窗：已存在则聚焦到前台，不重复开
                            let alive = if let Some(h) = &update_prompt {
                                h.update(async_cx, |_, window, _| {
                                    window.activate_window();
                                    true
                                })
                                .unwrap_or(false)
                            } else {
                                false
                            };
                            if !alive {
                                match async_cx.update(|cx| open_update_prompt_in_app(new_version, cx)) {
                                    Ok(h) => update_prompt = Some(h),
                                    Err(e) => tracing::error!("[overlay] 打开更新提示窗失败: {e}"),
                                }
                            }
                        }
                        Ok(OverlayCommand::OpenResultPin { payload, translate }) => {
                            // 同一类窗口只保留一个：关掉旧的，开新的（左图右文）
                            let slot = if translate { &mut trans_pin } else { &mut ocr_pin };
                            if let Some(old) = slot.take() {
                                let _ = old.update(async_cx, |root, window, cx| {
                                    // 关旧窗时顺手把左图从 atlas 摘掉：`remove_window()` 只是
                                    // 把窗口标记为本帧移除，视图随后被销毁，Arc 掉了但
                                    // `RenderImage` 没有 Drop，瓦片不会自己回收 → 每次
                                    // OCR/翻译都漏一块"选区大小"的 atlas 瓦片。
                                    // 只在类型对得上时释放，拿不到就照旧关窗。
                                    if let Ok(view) = root.view().clone().downcast::<OcrPinView>() {
                                        let img = view.read(cx).image.clone();
                                        let _ = window.drop_image(img);
                                    }
                                    window.remove_window();
                                });
                            }
                            match async_cx.update(|cx| open_result_pin_in_app(payload, translate, cx)) {
                                Ok(handle) => *slot = Some(handle),
                                Err(e) => tracing::error!("[overlay] 打开结果窗失败: {e}"),
                            }
                        }
                        Ok(OverlayCommand::UpdateResultPin { translate, text }) => {
                            let handle = if translate { &trans_pin } else { &ocr_pin };
                            if let Some(handle) = handle {
                                let _ = handle.update(async_cx, |root, _, cx| {
                                    // Root 包裹后 downcast 访问 OcrPinView
                                    if let Ok(view) = root.view().clone().downcast::<OcrPinView>() {
                                        view.update(cx, |view, cx| {
                                            if text.is_empty() {
                                                view.text = None;
                                                view.text_state = None;
                                            } else {
                                                // 先由 `&text` 生成 markdown 字符串，再**移动** text 给
                                                // view.text（原为 clone，避免整份 OCR 文本深拷贝）。
                                                // 渲染方式按内容自适应：
                                                //   - OCR 结果：一律等宽代码块（表格/缩进靠等宽对齐）；
                                                //   - 译文：纯段落走 markdown 散文（中文读着舒服），
                                                //     但只要带版式（行首缩进 / 连续空行）就必须包代码块
                                                //     ——markdown 会把行首空白和多余空行吃掉，那样
                                                //     "混排保留原格式"在界面上根本看不出来。
                                                let keep_layout = translate
                                                    && (text.contains("\n\n")
                                                        || text
                                                            .lines()
                                                            .any(|l| l.starts_with(' ') || l.starts_with('\t')));
                                                let md = if translate && !keep_layout {
                                                    text.clone()
                                                } else {
                                                    format!("```text\n{}\n```", text)
                                                };
                                                view.text = Some(text);
                                                // 重建 TextViewState（markdown 解析），支持选中/复制/全选
                                                view.text_state = Some(cx.new(|cx| {
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
            // Windows：创建即隐藏（show:false），首个会话由 reuse 的
            // activate_window → set_window_placement 显示，避免启动时残留一个
            // 全屏灰色遮罩窗口（之前只有 Linux 在创建后 unmap；Windows 一直没
            // 隐藏，且 Transparent 背景未合成 alpha → 显示成灰色全屏窗，点掉才消失）。
            #[cfg(target_os = "windows")]
            show: false,
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
    // 帧尺寸在下方 move 进 (display, original) 前先取出，供对齐诊断使用
    // （Windows 分支的 schedule_overlay_client_align 会用）
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    let (frame_w, frame_h) = (frame.width, frame.height);
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
        // Windows：把遮罩窗口客户端顶对齐到帧捕获原点（主屏物理 0,0）。
        // 帧图像从物理(0,0)捕获、绘制在 client (0,0)；若客户端被放高数 px
        // （GPUI calculate_window_rect 假设边框对称，部分窗口实际顶部边框为 0，
        // 见 adjust_window_client_top 注释），遮罩会相对实屏整幅上移——即用户
        // 看到的「屏幕偏移 N px」。Pin 窗口已校正过，这里给遮罩窗口同样校正；
        // 下方日志打印实测偏移，dy=0 时幂等不动窗口。
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = window_hwnd(window) {
            schedule_overlay_client_align(
                hwnd as usize,
                (client_origin.y * window.scale_factor()) as i32,
                window.scale_factor(),
                client_origin,
                frame_w,
                frame_h,
            );
        }
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

/// UI 视觉调试用的辅助窗口探针（见 [`OverlayView::apply_ui_probe`]）
///
/// 只认环境变量 `SCREENSHOT_RS_UI_PROBE`，取值：
/// - `progress`：开一个假的滚动进度浮窗（计数器会自增，能看到"进行中"的样子）
/// - `pin`：开一个固定窗，内容是一张合成的测试图（核对标题栏与 1:1 绘制）
/// - `update`：开「发现新版本」提示窗
///
/// 多个值可用 `,` 组合。正常使用不带该变量，函数体不执行任何逻辑。
fn probe_aux_windows(
    progress: &mut Option<WindowHandle<ProgressView>>,
    update_prompt: &mut Option<WindowHandle<gpui_component::Root>>,
    cx: &mut AsyncApp,
) {
    let Ok(spec) = std::env::var("SCREENSHOT_RS_UI_PROBE") else {
        return;
    };
    let wants = |k: &str| spec.split(',').any(|p| p.trim() == k || p.trim().starts_with(&format!("{k}:")));

    if wants("progress") {
        use std::sync::atomic::{AtomicBool, AtomicU32};
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let moving = Arc::new(AtomicBool::new(false));
        let bottom = Arc::new(AtomicBool::new(false));
        // confirming 是 Arc<AtomicBool>（见 ProgressView 字段）
        let confirming = Arc::new(AtomicBool::new(false));
        let height = Arc::new(AtomicU32::new(1284));
        let region = ub::Bounds::new(ub::Point::new(320.0, 240.0), ub::Point::new(1280.0, 900.0));
        let screen = ub::Bounds::new(ub::Point::ZERO, ub::Point::new(1920.0, 1080.0));
        let opened = cx.update(|cx| {
            open_progress_window(
                cancel, done, height, moving, bottom, confirming, false, region, screen, cx,
            )
        });
        match opened {
            Ok(h) => *progress = Some(h),
            Err(e) => tracing::warn!("[UI 探针] 打开进度窗失败: {e}"),
        }
    }

    if wants("update") {
        match cx.update(|cx| open_update_prompt_in_app("9.9.9".to_string(), cx)) {
            Ok(h) => *update_prompt = Some(h),
            Err(e) => tracing::warn!("[UI 探针] 打开更新提示窗失败: {e}"),
        }
    }

    if wants("pin") {
        // 合成一张 360x240 的测试图：四角彩色 + 中间渐变，方便判断是否 1:1
        let (w, h) = (360u32, 240u32);
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let r = (x * 255 / w.max(1)) as u8;
                let g = (y * 255 / h.max(1)) as u8;
                let b = if (x / 12 + y / 12) % 2 == 0 { 0xF0 } else { 0x40 };
                px.extend_from_slice(&[r, g, b, 0xFF]);
            }
        }
        let frame = CapturedFrame {
            width: w,
            height: h,
            pixels: px,
        };
        cx.update(|cx| {
            open_pin_in_app(
                PinPayload { frame, origin_x: 120.0, origin_y: 120.0, sx: 1.0, sy: 1.0 },
                cx,
            );
        });
    }
}

/// 滚动截屏进度浮窗：面板高度
const PROGRESS_PANEL_H: f32 = 46.0;
/// 滚动截屏进度浮窗：面板宽度（图标 + 文案 + 计数 + 两个按钮）
const PROGRESS_PANEL_W: f32 = 344.0;
/// 滚动截屏进度浮窗：窗口内边距（面板四周留白，给阴影用；窗口背景为 Transparent）
const PROGRESS_PAD: f32 = 12.0;

/// 滚动截屏进度小窗视图（auto/manual 共用；manual 显示「完成」按钮）
struct ProgressView {
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    /// 引擎每轮更新的「内容是否在动」标志：静止时才显示「完成」按钮
    moving: Arc<AtomicBool>,
    /// 引擎每轮更新的「最近一帧底部是否含内容」：点「完成」时据此弹确认
    ///
    /// 当前 UI 不再读它（点「完成」直接结束，理由见 `ProgressView::render`），
    /// 但引擎仍在写、命令通道仍要传，保留字段以免动到滚动引擎的公共接口。
    #[allow(dead_code)]
    bottom_has_content: Arc<AtomicBool>,
    /// 确认态标志：点「完成」且底部有内容时置 true，弹「可能没滚到底」确认
    confirming: Arc<AtomicBool>,
    manual: bool,
}

impl Render for ProgressView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let height = self.progress.load(Ordering::Relaxed);
        let confirming_now = self.manual && self.confirming.load(Ordering::Relaxed);
        let label = if confirming_now {
            // 确认态：提示用户可能还没滚到底
            "底部可能还有内容？".to_string()
        } else if self.manual {
            "手动滚动截屏中…".to_string()
        } else {
            "滚动截屏中…".to_string()
        };
        let busy = self.moving.load(Ordering::Relaxed);
        let mono = cx.theme().mono_font_family.clone();

        // 浮窗外观：深色玻璃面板 + 柔和阴影。窗口铺满但四边留 PROGRESS_PAD，
        // 面板在其中（窗口背景 Transparent，圆角外与阴影区自然透出桌面）。
        div()
            .size_full()
            .p(px(PROGRESS_PAD))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .size_full()
                    .bg(theme::c::rgb(theme::tokens::PANEL_BG))
                    .text_color(theme::c::rgb(theme::tokens::TEXT))
                    .rounded(px(theme::r::PANEL))
                    .border_1()
                    .border_color(theme::c::rgb(theme::tokens::PANEL_BORDER))
                    .shadow(theme::panel_shadow())
                    .child(
                        Icon::new(IconName::LoaderCircle)
                            .size(px(15.0))
                            .text_color(theme::c::rgb(theme::tokens::ACCENT)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(13.0))
                            .text_color(theme::c::rgb(theme::tokens::TEXT))
                            .child(label),
                    )
                    // 计数用等宽字体 + 固定宽度右对齐：数字从 "7px" 涨到 "1284px"
                    // 时不会推着右侧按钮左右抖动。
                    .child(
                        div()
                            .flex_none()
                            .w(px(74.0))
                            .text_right()
                            .font_family(mono)
                            .text_size(px(13.0))
                            .text_color(theme::c::rgb(theme::tokens::TEXT_MUTED))
                            .child(format!("{height}px")),
                    )
                    .child(div().flex_1())
                    .when(confirming_now, {
                        let confirming = self.confirming.clone();
                        let done = self.done.clone();
                        // 确认态：继续滚动 / 确定结束
                        move |b| {
                            b.child(bar_button(
                                "scroll-continue",
                                icon_label_content(Icon::new(IconName::ArrowLeft), "继续滚动"),
                                ToolbarBtnStyle::Neutral,
                                false,
                                move |_, _, _| confirming.store(false, Ordering::Relaxed),
                            ))
                            .child(bar_button(
                                "scroll-confirm-done",
                                icon_label_content(Icon::new(IconName::Check), "确定结束"),
                                ToolbarBtnStyle::Success,
                                false,
                                move |_, _, _| done.store(true, Ordering::Relaxed),
                            ))
                        }
                    })
                    // 完成按钮：**始终显示**（不再随 moving 显隐，避免按钮忽隐忽现、行宽变化
                    // 导致「取消」跟着左右抖动）。内容在动时**置灰禁用**（手动滚动动画未落定
                    // 不可点，避免最后一段还没拼进去就结束）；静止时恢复可点。自动模式因引擎
                    // 每轮自动滚动、`moving` 恒为 false，完成按钮始终可点（随时可停止）。点
                    // 「完成」直接结束（不再弹「底部可能还有内容？」确认）：vxe-table 这类内容
                    // 始终填满视口的表格，`bottom_has_content` 几乎永远为 true，若以它作为确认
                    // 条件，用户明明滚到底了点「完成」还会被追问「继续滚动」。
                    .when(!confirming_now, {
                        let done = self.done.clone();
                        move |b| {
                            b.child(
                                bar_button(
                                    "scroll-done",
                                    icon_label_content(Icon::new(IconName::Check), "完成"),
                                    ToolbarBtnStyle::Success,
                                    busy,
                                    move |_, _, _| {
                                        done.store(true, Ordering::Relaxed);
                                    },
                                ),
                            )
                        }
                    })
                    .child({
                        let cancel = self.cancel.clone();
                        bar_button(
                            "scroll-cancel",
                            icon_label_content(Icon::new(IconName::Close), "取消"),
                            ToolbarBtnStyle::Danger,
                            false,
                            move |_, _, _| cancel.store(true, Ordering::Relaxed),
                        )
                    }),
            )
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

    // 自动/手动模式都显示「完成」+「取消」两个按钮，窗口统一加宽。
    // 窗口本身用 Transparent 背景，四边留 PROGRESS_PAD 的空隙给面板阴影，
    // 这样浮窗才有圆角 + 立体感（不透明窗口会把圆角外画成方块底色）。
    let win_w = PROGRESS_PANEL_W + PROGRESS_PAD * 2.0;
    const WIN_H: f32 = PROGRESS_PANEL_H + PROGRESS_PAD * 2.0;
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
            window_background: WindowBackgroundAppearance::Transparent,
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

/// 「发现新版本」提示窗：启动时检查到新版本后弹出，用户点「立即更新」则后台
/// 下载并替换运行中的二进制，点「稍后」关闭。整个更新在后台线程进行，
/// `status`/`error` 共享状态由窗口以 300ms 轮询刷新（见 `open_update_prompt_in_app`）。
struct UpdatePromptView {
    new_version: String,
    /// 0=待确认, 1=更新中, 2=更新完成请重启, -1=失败
    status: Arc<AtomicI32>,
    error: Arc<Mutex<String>>,
}

impl Render for UpdatePromptView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use crate::assets::icons as app_icon;
        use theme::tokens as t;

        let status = self.status.load(Ordering::Relaxed);
        let err: String = self.error.lock().map(|g| g.clone()).unwrap_or_default();
        let (title, body) = match status {
            0 => (
                "发现新版本".to_string(),
                "新版本已发布，是否立即下载并安装？".to_string(),
            ),
            1 => (
                "正在更新…".to_string(),
                "正在下载并安装新版本，请稍候…".to_string(),
            ),
            2 => ("更新完成".to_string(), "已更新，正在重启应用…".to_string()),
            _ => (
                "更新失败".to_string(),
                if err.is_empty() { "请稍后重试".into() } else { err },
            ),
        };
        // 状态语义色：待确认/更新中=强调蓝，完成=绿，失败=红。
        // 标题图标、圆点、版本徽标都用它，整窗只有一处强调色，不花。
        let accent = match status {
            2 => theme::c::rgb(t::SUCCESS),
            -1 => theme::c::rgb(t::DANGER),
            _ => theme::c::rgb(t::ACCENT),
        };
        let status_icon = match status {
            2 => Icon::new(IconName::CircleCheck),
            -1 => Icon::new(IconName::TriangleAlert),
            // 下载/安装进行中：环形箭头（静止图形，不引入动画以免无谓重绘）
            1 => Icon::new(IconName::LoaderCircle),
            _ => Icon::empty().path(app_icon::SPARKLES),
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .px(px(22.0))
            .py(px(18.0))
            .gap(px(10.0))
            .bg(theme::c::rgb(t::PANEL_BG))
            .text_color(theme::c::rgb(t::TEXT))
            // ── 标题行：状态图标 + 标题 + 版本徽标 ──────────────────────
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .child(status_icon.size(px(19.0)).text_color(accent))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::c::rgb(t::TEXT))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .px(px(9.0))
                            .py(px(2.0))
                            .rounded(px(theme::r::CHIP))
                            .bg(accent.opacity(0.16))
                            .text_color(accent)
                            .child(format!("v{}", self.new_version)),
                    ),
            )
            // ── 正文 ────────────────────────────────────────────────────
            .child(
                div()
                    .text_sm()
                    .text_color(theme::c::rgb(t::TEXT_MUTED))
                    .child(body),
            )
            // 弹性占位：把按钮行推到窗底（窗口高度可被 WM 拉伸时也贴底）
            .child(div().flex_1())
            // ── 分隔线 + 按钮行（右对齐）─────────────────────────────────
            .child(
                div()
                    .mt(px(12.0))
                    .pt(px(12.0))
                    .border_t_1()
                    .border_color(theme::c::rgb(t::DIVIDER))
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap(px(8.0))
                    // 主按钮：待确认 → 「立即更新」；失败 → 「重试」；
                    // 更新中 → 同位置的**禁用占位**（换文案不换位置，避免按钮
                    // 突然消失导致「稍后」左右跳动）。
                    .when(status != 2, {
                        let status_state = self.status.clone();
                        let error = self.error.clone();
                        let in_progress = status == 1;
                        let label = if status == -1 { "重试" } else { "立即更新" };
                        move |b| {
                            let content = div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    Icon::empty()
                                        .path(app_icon::DOWNLOAD)
                                        .size(px(15.0)),
                                )
                                .child(label);
                            b.child(ui_button(
                                "update-start",
                                content,
                                ToolbarBtnStyle::Accent,
                                BtnSize::Dialog,
                                true,
                                in_progress,
                                None,
                                move |_, _, _| {
                                    // 后台执行：下载并替换运行中的二进制，完成后更新状态
                                    status_state.store(1, Ordering::Relaxed);
                                    let status = status_state.clone();
                                    let error = error.clone();
                                    std::thread::spawn(move || {
                                        match crate::update::apply_update() {
                                            Ok(_) => {
                                                // 短暂显示「更新完成」，再自动重启到新版本。
                                                status.store(2, Ordering::Relaxed);
                                                std::thread::sleep(
                                                    std::time::Duration::from_millis(700),
                                                );
                                                crate::update::restart_app();
                                            }
                                            Err(e) => {
                                                {
                                                    let mut guard = error
                                                        .lock()
                                                        .unwrap_or_else(|p| p.into_inner());
                                                    *guard = e;
                                                }
                                                status.store(-1, Ordering::Relaxed);
                                            }
                                        }
                                    });
                                },
                            ))
                        }
                    })
                    // 已完成：只留「关闭」
                    .when(status == 2, {
                        move |b| {
                            b.child(ui_button(
                                "update-close",
                                label_text("关闭"),
                                ToolbarBtnStyle::Success,
                                BtnSize::Dialog,
                                true,
                                false,
                                None,
                                move |_, window, _| window.remove_window(),
                            ))
                        }
                    })
                    // 「稍后」始终显示（更新中也可关闭）
                    .child(ui_button(
                        "update-dismiss",
                        label_text("稍后"),
                        ToolbarBtnStyle::Neutral,
                        BtnSize::Dialog,
                        true,
                        false,
                        None,
                        move |_, window, _| window.remove_window(),
                    )),
            )
    }
}

/// 打开「发现新版本」提示窗，窗口居中。每 300ms 轮询重绘以刷新更新进度。
fn open_update_prompt_in_app(
    new_version: String,
    cx: &mut App,
) -> AppResult<WindowHandle<gpui_component::Root>> {
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or_else(|| {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: Size::new(px(1280.0), px(800.0)),
        }
    });
    let win_w = 420.0_f32;
    let win_h = 230.0_f32;
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
                title: Some("更新提示".into()),
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
            let status = Arc::new(AtomicI32::new(0));
            let error = Arc::new(Mutex::new(String::new()));
            let view = cx.new(|_| UpdatePromptView { new_version, status, error });
            // 后台线程改 status/error 后，这里每 300ms 重绘一次刷新；窗口关闭后退出。
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(300))
                        .await;
                    let Some(entity) = weak.upgrade() else { break; };
                    entity.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
            // 包 Root：与 OCR 模型窗一致——提供主题化、不透明的窗口背景，
            // 否则下层窗口颜色会穿透到更新提示窗，观感很差。
            cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
        },
    )
    .map_err(|e| AppError::Gpui(format!("打开更新提示窗失败: {e}")))
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
    // 固定窗口按「截取的实际逻辑尺寸」1:1 显示，不再缩放到 1200×900 内。
    // 用户反馈：大选区（如全屏）固定后图片被整体缩小，宽高与截图不符。
    // 只对超小图保留 150px 下限（保证可读），绝不缩小；1x 屏下 sx=sy=1，
    // img_w/img_h 等于截取物理宽高，窗口与截图一一对应。
    const MIN_IMG_W: f32 = 150.0;
    let scale = (MIN_IMG_W / img_w).max(1.0);
    // 自定义标题栏高度（原生标题栏已移除，由 PinWindowView render 绘制）
    let win_w = px(img_w * scale);
    let win_h = px(img_h * scale + PIN_TITLEBAR_H);
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
    let target_y = origin_y - PIN_TITLEBAR_H;

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
                // 图片从 client y = PIN_TITLEBAR_H 开始绘制（根容器不再画 1px
                // 描边，所以不再是 33），把客户端顶移到 origin_y - 32 即可让
                // 图片与原始选区严格对齐。
                (origin_y - PIN_TITLEBAR_H) as i32,
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

            let view = cx.new(|cx| PinWindowView::new(pin_frame, window, cx));
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
        use theme::tokens as t;
        use gpui::relative;
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
            // 内置档（small）：模型随应用包分发，不提供下载（无单文件/整档下载按钮）
            let bundled = t.bundled;
            // 只有「本档整档下载中」才置按钮为下载中；其他档位按钮保持可点
            let busy = downloading
                && batch_download
                && downloading_tier.as_deref() == Some(tier.as_str());
            // 本档三件套是否全部就绪：决定整档按钮显示「重新下载」还是「批量下载」
            let all_ready = t.files.iter().all(|f| matches!(f.status, FileStatus::Ready));
            // file_rows 闭包持有档位名/弱引用/下载状态的独立副本，避免与外层借用冲突
            let file_rows_tier = tier.clone();
            let file_rows_bundled = bundled;
            let file_rows_weak = self.weak.clone();
            let file_rows_current_file = current_file.clone();
            let file_rows_downloading_tier = downloading_tier.clone();
            let file_rows_batch = batch_download;
            let file_rows = t.files.iter().map(move |f| {
                // 正在下载的文件行状态置为「下载中…」
                let (mark, mark_color) = if downloading
                    && file_rows_current_file.as_deref() == Some(f.name)
                {
                    ("下载中…", theme::c::rgb(t::ACCENT))
                } else {
                    match &f.status {
                        FileStatus::Ready => ("✓ 已存在", theme::c::rgb(t::SUCCESS)),
                        FileStatus::Missing => ("未下载", theme::c::rgb(t::TEXT_MUTED)),
                        FileStatus::Downloading => ("下载中…", theme::c::rgb(t::ACCENT)),
                        FileStatus::Error(_) => ("失败", theme::c::rgb(t::DANGER)),
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
                    .unwrap_or_else(|| "本地无此文件".into());
                let url = f.url.clone();
                let tier_for_btn = file_rows_tier.clone();
                let name_for_btn = f.name.to_string();
                // 单文件按钮状态：只有正在下载的这个按钮变「下载中…」，其他保持原样。
                // 未下载=「下载」；已存在=「重新下载」。内置档（small）无下载按钮。
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
                            .child(div().text_color(theme::c::rgb(t::TEXT_MUTED)).text_xs().child(gpui::SharedString::from(size_text)))
                            .child(if file_rows_bundled {
                                // 内置档（small）：随应用包分发，无单文件下载按钮
                                div()
                                    .text_color(theme::c::rgb(t::SUCCESS))
                                    .text_xs()
                                    .child("内置")
                                    .into_any_element()
                            } else {
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
                                    })
                                    .into_any_element()
                            })                            )
                    .child(
                        div()
                            .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.8))
                            .text_xs()
                            .child(gpui::SharedString::from(path_text)),
                    )
                    .child(
                        div()
                            .text_color(theme::c::rgb(t::TEXT_MUTED))
                            .text_xs()
                            .child(gpui::SharedString::from(url)),
                    )
            });
            div()
                .flex_col()
                .gap(px(6.0))
                .p(px(12.0))
                .rounded_md()
                .border_1()
                .bg(theme::c::rgb(t::POPOVER_BG))
                .border_color(if selected {
                    theme::c::rgb(t::ACCENT)
                } else {
                    theme::c::rgb(t::PANEL_BORDER)
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
                                    theme::c::rgb(t::ACCENT)
                                } else {
                                    theme::c::rgb(t::TEXT)
                                })
                                .child(gpui::SharedString::from(tier.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(theme::c::rgb(t::TEXT_MUTED))
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
                        // 整档按钮：仅非内置档（medium）显示下载；内置档（small）随应用包分发
                        .child(if bundled {
                            div()
                                .text_color(theme::c::rgb(t::SUCCESS))
                                .text_xs()
                                .child("已随应用内置")
                                .into_any_element()
                        } else {
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
                                })
                                .into_any_element()
                        }),
                )
                .child(div().flex_col().gap(px(4.0)).children(file_rows))
        });

        // ---- 翻译模型区块（英译中，opus-mt-en-zh 量化版，约 110MB）----
        // 与上面 OCR 档位区块同一套视觉：卡片 + 标题行（右侧按钮）+ 文件行 + 进度/结果。
        let t_snap = crate::translate::model_snapshot();
        let t_downloading = t_snap.downloading;
        let t_ready = t_snap.ready;
        let t_current_file = t_snap.current_file.clone();
        let (t_done, t_total) = t_snap.progress;
        let t_pct = t_total
            .filter(|t| *t > 0)
            .map(|t| (t_done as f64 / t as f64 * 100.0).min(100.0));
        let t_mb = |b: u64| format!("{:.1} MB", b as f64 / 1048576.0);
        let t_rows = t_snap.files.iter().map(|f| {
            // (状态文字, 颜色)：文字统一用 String，避免各分支类型不一致
            let (mark, color): (String, _) =
                if t_downloading && t_current_file.as_deref() == Some(f.name) {
                    ("下载中…".to_string(), theme::c::rgb(t::ACCENT))
                } else {
                    match f.status {
                        crate::translate::FileStatus::Ready => {
                            ("✓ 已存在".to_string(), theme::c::rgb(t::SUCCESS))
                        }
                        crate::translate::FileStatus::Missing => {
                            ("未下载".to_string(), theme::c::rgb(t::TEXT_MUTED))
                        }
                        // 只报"体积不符"用户没法判断是下坏了还是版本变了，附上实际大小
                        crate::translate::FileStatus::WrongSize => (
                            match f.local_size {
                                Some(n) => format!("体积不符（本地 {}）", t_mb(n)),
                                None => "体积不符".to_string(),
                            },
                            theme::c::rgb(t::DANGER),
                        ),
                    }
                };
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .text_xs()
                        .text_color(theme::c::rgb(t::TEXT))
                        .child(gpui::SharedString::from(file_basename(f.name).to_string())),
                )
                // 体积列与 OCR 行保持一致：只给期望体积。
                // 本地体积只在"体积不符"这个异常状态里补出来，正常情况不占地方。
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::c::rgb(t::TEXT_MUTED))
                        .child(gpui::SharedString::from(t_mb(f.expected))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color)
                        .child(gpui::SharedString::from(mark.clone())),
                )
        });
        // 远程下载链接：与 OCR 档位卡片里的 url 同一性质。
        // 只给"还没就绪"的文件列完整链接——全都下好了就没有可操作的东西，
        // 那时只留上面的仓库地址一行，别把界面塞满五个长 URL。
        let t_links: Vec<gpui::AnyElement> = t_snap
            .files
            .iter()
            .filter(|f| f.status != crate::translate::FileStatus::Ready)
            .map(|f| {
                div()
                    .text_xs()
                    .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.7))
                    .child(gpui::SharedString::from(format!(
                        "远程链接：{}/{}",
                        t_snap.base_url, f.name
                    )))
                    .into_any_element()
            })
            .collect();

        let translate_block = window_card()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme::c::rgb(t::TEXT))
                                    .child("翻译模型（英译中）"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::c::rgb(t::SECTION_LABEL))
                                    .child(gpui::SharedString::from(format!(
                                        "离线推理，不联网；已就位 {}/{} 个文件{}",
                                        t_snap
                                            .files
                                            .iter()
                                            .filter(|f| {
                                                f.status == crate::translate::FileStatus::Ready
                                            })
                                            .count(),
                                        t_snap.files.len(),
                                        if t_ready { "（就绪）" } else { "" },
                                    ))),
                            ),
                    )
                    .child(
                        Button::new("dl-translate")
                            .label(if t_downloading {
                                "下载中…"
                            } else if t_ready {
                                "重新下载"
                            } else {
                                "下载"
                            })
                            .with_variant(if t_downloading {
                                ButtonVariant::Default
                            } else {
                                ButtonVariant::Info
                            })
                            .with_size(gpui_component::Size::XSmall)
                            .disabled(t_downloading)
                            .on_click({
                                let weak = self.weak.clone();
                                move |_, _, app| {
                                    let Some(entity) = weak.upgrade() else { return };
                                    entity.update(app, |this, cx| {
                                        // start_download 返回 false = 已有下载在跑，
                                        // 不重复起线程（两个线程会互相覆盖 .part 文件）
                                        if crate::translate::start_download() {
                                            this.activation_error = None;
                                            tracing::info!("翻译: 开始在后台下载模型");
                                        }
                                        cx.notify();
                                    });
                                }
                            }),
                    ),
            )
            .child(div().h(px(1.0)).w_full().bg(theme::c::rgb(t::DIVIDER)))
            .children(t_rows)
            // 本地保存路径：与 OCR 档位卡片里的 path_text 同一性质——
            // 文件名只给名字，路径单独一行，用户才知道东西存哪了。
            .child(
                div()
                    .text_xs()
                    .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.7))
                    .child(gpui::SharedString::from(if t_ready {
                        format!("本地路径：{}", t_snap.cache_dir.display())
                    } else {
                        // 没下全时把"会存到哪"也说清楚，方便用户自己去看/清理
                        format!(
                            "本地路径：{}（尚未下载完整）",
                            t_snap.cache_dir.display()
                        )
                    })),
            )
            // 远程仓库地址（下载源）：所有文件都在它下面，换镜像也就换这一行
            .child(
                div()
                    .text_xs()
                    .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.7))
                    .child(gpui::SharedString::from(format!(
                        "远程地址：{}",
                        t_snap.base_url
                    ))),
            )
            .children(t_links)
            .child(if t_downloading {
                let pct_text = match (t_done, t_total) {
                    (d, Some(t)) if t > 0 => format!(
                        "{:.0}%  {:.1}/{:.1} MB",
                        t_pct.unwrap_or(0.0),
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
                            .text_color(theme::c::rgb(t::ACCENT))
                            .text_sm()
                            .child(gpui::SharedString::from(format!("正在下载：{pct_text}"))),
                    )
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(6.0))
                            .rounded_full()
                            .bg(theme::c::rgb(t::PROGRESS_TRACK))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(0.0))
                                    .top(px(0.0))
                                    .h_full()
                                    .rounded_full()
                                    .w(relative(
                                        (t_pct.unwrap_or(0.0) as f32 / 100.0).clamp(0.0, 1.0),
                                    ))
                                    .bg(theme::c::rgb(t::ACCENT)),
                            ),
                    )
            } else {
                div().child(match (&t_snap.last_error, t_ready) {
                    (Some(e), _) => status_chip(
                        format!("下载失败：{e}"),
                        t::DANGER,
                        t::DANGER_SOFT,
                    )
                    .into_any_element(),
                    (None, true) => status_chip(
                        "✓ 已就绪，可直接用「翻译」工具".to_string(),
                        t::SUCCESS,
                        0x2BB6732E,
                    )
                    .into_any_element(),
                    (None, false) => div()
                        .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.85))
                        .text_xs()
                        .child(gpui::SharedString::from(format!(
                            "点右侧「下载」获取（约 {:.0} MB），也可在首次使用翻译时自动下载",
                            t_snap.bytes_total as f64 / 1048576.0
                        )))
                        .into_any_element(),
                })
            });

        div()
            .id("ocr-models")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::c::rgb(t::PANEL_BG))
            .text_color(theme::c::rgb(t::TEXT))
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .flex_1()
                    .overflow_y_scrollbar()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(12.0))
                    // 标题区
                    .child(window_header(
                        "模型管理",
                        "OCR 识别与英译中翻译所需的本地模型；全部离线运行，下载一次即可",
                    ))
                    // 缓存目录（脚注性质：平时不用看，出问题时才有用）
                    .child(
                        div()
                            .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.7))
                            .text_xs()
                            .child(gpui::SharedString::from(format!(
                                "缓存目录：{cache_dir}"
                            ))),
                    )
                    .child(section_header(
                        "OCR 识别模型",
                        "档位越高越准、体积也越大；标「内置」的随应用一起分发，无需下载",
                        div(),
                    ))
                    .children(tier_blocks)
                    // 翻译模型：与 OCR 档位并列的第二块
                    .child(translate_block)
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
                            .border_color(theme::c::rgb(t::DANGER).opacity(0.55))
                            .bg(theme::c::rgb(t::DANGER).opacity(0.14))
                            .child(
                                div()
                                    .text_color(theme::c::rgb(t::DANGER))
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
                                    .text_color(theme::c::rgb(t::ACCENT))
                                    .text_sm()
                                    .child(gpui::SharedString::from(format!(
                                        "正在下载：{pct_text}"
                                    ))),
                            )
                            .child(
                                // 进度条：轨道铺满可用宽度，填充用**百分比宽度**
                                // （旧实现写死 480px，而窗口内容宽约 736px，进度
                                // 到 100% 也只填到 65%，看起来"永远下不完"）。
                                div()
                                    .relative()
                                    .w_full()
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(theme::c::rgb(t::PROGRESS_TRACK))
                                    .child(
                                        div()
                                            .absolute()
                                            .left(px(0.0))
                                            .top(px(0.0))
                                            .h_full()
                                            .rounded_full()
                                            .w(relative(
                                                (pct.unwrap_or(0.0) as f32 / 100.0)
                                                    .clamp(0.0, 1.0),
                                            ))
                                            .bg(theme::c::rgb(t::ACCENT)),
                                    ),
                            )
                    } else {
                        div().child(match &last_download {
                            Some(Ok(())) => div()
                                .text_color(theme::c::rgb(t::SUCCESS))
                                .text_sm()
                                .child("✓ 最近一次下载完成"),
                            Some(Err(e)) => div()
                                .text_color(theme::c::rgb(t::DANGER))
                                .text_sm()
                                .child(gpui::SharedString::from(format!(
                                    "最近一次下载失败：{e}"
                                ))),
                            None => div()
                                .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.8))
                                .text_xs()
                                .child("尚未下载模型"),
                        })
                    }),
            )
    }
}


// ---------------------------------------------------------------------------
// 系统设置窗口：热键（查看 / 替换）、版本、检查更新
// ---------------------------------------------------------------------------

/// 系统设置视图。
///
/// 热键替换是真生效的：写入 config.toml（保留注释排版）后通过
/// `hotkey::request_rebind` 请主循环换绑运行中的热键服务，不用重启。
pub struct SettingsView {
    focus_handle: FocusHandle,
    /// 自身弱引用：按钮回调（只拿 &mut App）用它更新视图并触发重绘
    weak: WeakEntity<Self>,
    /// 热键输入框（InputState 自带 IME 支持）
    hotkey_input: Entity<gpui_component::input::InputState>,
    /// 上次保存热键的结果：Ok=提示语，Err=失败原因
    hotkey_status: Option<Result<String, String>>,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = crate::config::hotkey_screenshot();
        let hotkey_input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder(crate::config::DEFAULT_HOTKEY_SCREENSHOT)
                .default_value(current)
        });
        Self {
            focus_handle: cx.focus_handle(),
            weak: cx.entity().downgrade(),
            hotkey_input,
            hotkey_status: None,
        }
    }

    /// 保存热键输入框里的值：校验 → 写配置 → 请主循环换绑。
    fn save_hotkey(&mut self, cx: &mut Context<Self>) {
        let spec = self.hotkey_input.read(cx).value().to_string();
        let spec = spec.trim().to_string();
        if spec.is_empty() {
            self.hotkey_status = Some(Err("热键不能为空".into()));
            cx.notify();
            return;
        }
        // 先校验再落盘：写坏了重启后仍会被回退到默认键，但用户会以为"改了没用"
        if let Err(e) = crate::hotkey::parse_hotkey(&spec) {
            self.hotkey_status = Some(Err(format!("格式不对：{e}")));
            cx.notify();
            return;
        }
        match crate::config::persist_hotkey_screenshot(&spec) {
            Ok(()) => {
                crate::hotkey::request_rebind(spec.clone());
                self.hotkey_status = Some(Ok(format!("已保存，热键已换绑为 {spec}")));
            }
            Err(e) => self.hotkey_status = Some(Err(e)),
        }
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        use theme::tokens as t;
        let current_version = crate::update::CURRENT_VERSION;
        let check = crate::update::check_state();

        let status_row = |res: &Option<Result<String, String>>| match res {
            None => div().into_any_element(),
            Some(Ok(msg)) => div()
                .text_xs()
                .text_color(theme::c::rgb(t::SUCCESS))
                .child(gpui::SharedString::from(msg.clone()))
                .into_any_element(),
            Some(Err(e)) => div()
                .text_xs()
                .text_color(theme::c::rgb(t::DANGER))
                .child(gpui::SharedString::from(format!("失败：{e}")))
                .into_any_element(),
        };

        div()
            .id("settings")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .p(px(14.0))
            .bg(theme::c::rgb(t::PANEL_BG))
            .text_color(theme::c::rgb(t::TEXT))
            .track_focus(&self.focus_handle)
            // ---------- 标题区 ----------
            .child(window_header(
                "设置",
                "截图热键与版本更新；改完立即生效，不用重启",
            ))
            // ---------- 热键 ----------
            .child(
                window_card()
                    .child(section_header(
                        "截图热键",
                        "写法：修饰键 + 键，如 alt+s、ctrl+shift+a（支持 ctrl / alt / shift / super）",
                        div(),
                    ))
                    // 当前生效的键，用键帽形状展示——比一行纯文本好认得多
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .children(hotkey_caps(&crate::config::hotkey_screenshot()).into_iter().enumerate().flat_map(
                                |(i, cap)| {
                                    let mut v: Vec<gpui::AnyElement> = Vec::new();
                                    if i > 0 {
                                        v.push(
                                            div()
                                                .text_xs()
                                                .text_color(theme::c::rgb(t::TEXT_MUTED))
                                                .child("+")
                                                .into_any_element(),
                                        );
                                    }
                                    v.push(
                                        div()
                                            .px(px(8.0))
                                            .py(px(3.0))
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(theme::c::rgb(t::PANEL_BORDER))
                                            .bg(theme::c::rgb(t::BTN_BG))
                                            .text_xs()
                                            .text_color(theme::c::rgb(t::TEXT))
                                            .child(gpui::SharedString::from(cap))
                                            .into_any_element(),
                                    );
                                    v
                                },
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::c::rgb(t::TEXT_MUTED))
                                    .child("← 当前生效"),
                            ),
                    )
                    .child(
                        gpui_component::input::Input::new(&self.hotkey_input).w_full(),
                    )
                    // 写哪个文件、以及"改了到底算不算数"，都在这里说清楚：
                    // 少了这行，用户遇到"改了没生效"只能猜（环境变量优先级更高时就是这样）。
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::c::rgb(t::TEXT_MUTED).opacity(0.7))
                            .child(gpui::SharedString::from(format!(
                                "配置文件：{}",
                                crate::config::config_file_display()
                            ))),
                    )
                    .child(
                        match crate::config::hotkey_env_override() {
                            Some(v) => status_chip(
                                format!("环境变量 SCREENSHOT_RS_HOTKEY={v} 优先级更高，会覆盖这里的设置"),
                                t::DANGER,
                                t::DANGER_SOFT,
                            )
                            .into_any_element(),
                            None => div().into_any_element(),
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                Button::new("save-hotkey")
                                    .label("保存并生效")
                                    .with_variant(ButtonVariant::Info)
                                    .with_size(gpui_component::Size::Small)
                                    .on_click({
                                        let weak = self.weak.clone();
                                        move |_, _, app| {
                                            let Some(entity) = weak.upgrade() else { return };
                                            entity.update(app, |this, cx| this.save_hotkey(cx));
                                        }
                                    }),
                            )
                            .child(
                                Button::new("reset-hotkey")
                                    .label("恢复默认")
                                    .with_size(gpui_component::Size::Small)
                                    .on_click({
                                        let weak = self.weak.clone();
                                        move |_, window, app| {
                                            let Some(entity) = weak.upgrade() else { return };
                                            entity.update(app, |this, cx| {
                                                let def = crate::config::DEFAULT_HOTKEY_SCREENSHOT.to_string();
                                                this.hotkey_input.update(cx, |input, cx| {
                                                    input.set_value(def, window, cx);
                                                });
                                                this.save_hotkey(cx);
                                            });
                                        }
                                    }),
                            )
                            .child(status_row(&self.hotkey_status)),
                    ),
            )
            // ---------- 版本与更新 ----------
            .child(
                window_card()
                    .child(section_header(
                        &format!("当前版本 v{current_version}"),
                        "更新走 GitHub Release；发现新版本会弹确认窗，不会静默安装",
                        div(),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                Button::new("check-update")
                                    .label(if crate::update::is_checking() {
                                        "检查中…"
                                    } else {
                                        "检查更新"
                                    })
                                    .with_variant(ButtonVariant::Info)
                                    .with_size(gpui_component::Size::Small)
                                    .disabled(crate::update::is_checking())
                                    .on_click({
                                        let weak = self.weak.clone();
                                        move |_, _, app| {
                                            let Some(entity) = weak.upgrade() else { return };
                                            entity.update(app, |_this, cx| {
                                                // start_check 返回 false = 已有检查在跑，不重复发请求
                                                crate::update::start_check();
                                                cx.notify();
                                            });
                                        }
                                    }),
                            )
                            .child(match &check {
                                crate::update::CheckState::Idle => div()
                                    .text_xs()
                                    .text_color(theme::c::rgb(t::TEXT_MUTED))
                                    .child("尚未检查")
                                    .into_any_element(),
                                crate::update::CheckState::Checking => div()
                                    .text_xs()
                                    .text_color(theme::c::rgb(t::ACCENT))
                                    .child("正在检查…")
                                    .into_any_element(),
                                crate::update::CheckState::Done(Ok(None)) => status_chip(
                                    format!("已是最新版本（v{current_version}）"),
                                    t::SUCCESS,
                                    0x2BB6732E,
                                )
                                .into_any_element(),
                                crate::update::CheckState::Done(Ok(Some(v))) => div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::c::rgb(t::ACCENT))
                                            .child(gpui::SharedString::from(format!(
                                                "发现新版本 v{v}"
                                            ))),
                                    )
                                    // 复用既有的「更新提示」窗口：那里负责下载与重启，
                                    // 设置窗口不自己再实现一套更新流程。
                                    .child({
                                        let v = v.clone();
                                        Button::new("go-update")
                                            .label("更新")
                                            .with_variant(ButtonVariant::Info)
                                            .with_size(gpui_component::Size::XSmall)
                                            .on_click(move |_, _, _| {
                                                let _ = ensure_started().send(
                                                    OverlayCommand::PromptUpdate {
                                                        new_version: v.clone(),
                                                    },
                                                );
                                            })
                                    })
                                    .into_any_element(),
                                crate::update::CheckState::Done(Err(e)) => status_chip(
                                    format!("检查失败：{e}"),
                                    t::DANGER,
                                    t::DANGER_SOFT,
                                )
                                .into_any_element(),
                            }),
                    ),
            )
    }
}

/// 在常驻应用里打开系统设置窗口（屏幕居中，fire-and-forget）。
fn open_settings_in_app(cx: &mut App) -> AppResult<WindowHandle<gpui_component::Root>> {
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or_else(|| {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: Size::new(px(1280.0), px(800.0)),
        }
    });
    let win_w = 560.0_f32;
    let win_h = 420.0_f32;
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
                title: Some("系统设置".into()),
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
            let view = cx.new(|cx| SettingsView::new(window, cx));
            // 检查更新是后台线程跑的，状态在全局；这里定时重绘把结果刷出来。
            // 空转成本很低（每秒三次重绘一个静态界面），换来不用引入额外通道。
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
            // 包 Root：与其他辅助窗口一致（输入/选择控制器依赖 Root）
            cx.new(|cx| gpui_component::Root::new(view, window, cx).bordered(false))
        },
    )
    .map_err(|e| AppError::Gpui(format!("打开系统设置窗口失败: {e}")))
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
                title: Some("模型管理".into()),
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
    /// true=翻译结果窗（标题「翻译」、等待文案「翻译中…」），false=OCR 结果窗
    translate: bool,
    /// 是否要在等待文案里提"首次会先下载模型"。
    /// 只在**确实要下载**（翻译窗口且模型未就绪）时为 true：模型早就下好了还挂着
    /// 这句提示纯属误导（用户反馈）。
    needs_download: bool,
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
    #[allow(clippy::too_many_arguments)]
    fn new(
        frame: CapturedFrame,
        text: Option<String>,
        disp_w: f32,
        disp_h: f32,
        translate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (w, h, pixels) = (frame.width, frame.height, frame.pixels);
        let img = build_render_image_from_pixels(w, h, pixels);
        let needs_download =
            translate && !crate::translate::models_ready(&crate::translate::model_dir());
        // 左图同样只在构造时建一次、此后只被 paint：窗口销毁时 Arc 掉了，atlas
        // 瓦片却不会回收 → 每次 OCR/翻译漏一块"选区大小"的瓦片。
        // 用户点标题栏 X / Alt+F4 走系统关窗回调，这里顺手摘掉；程序内"关旧开新"
        // 走 `Window::remove_window()`（不触发该回调），在调用点另行释放。
        let img_for_close = img.clone();
        window.on_window_should_close(cx, move |window, _cx| {
            let _ = window.drop_image(img_for_close.clone());
            true
        });
        Self {
            translate,
            needs_download,
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
        use theme::tokens as t;
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
                        .text_color(theme::c::rgb(t::TEXT_MUTED))
                        .text_sm()
                        .child(gpui::SharedString::from(if !self.translate {
                            "OCR 识别中…"
                        } else if self.needs_download {
                            "翻译中…（首次需要先下载约 110MB 模型，请稍候）"
                        } else {
                            "翻译中…"
                        })),
                ),
            Some(text) => {
                // 覆盖代码块配色：黑底白字（默认 muted 灰底），文字顶到标题栏下。
                // 同时覆盖代码块默认内边距（p-3 ≈12px）——上下只留 2px，
                // 否则识别结果与黑底上边缘间隔太大（用户反馈）。
                let code_style = gpui::StyleRefinement::default()
                    .bg(gpui::rgba(0x000000FF))
                    .text_color(gpui::rgba(0xFFFFFFFF))
                    .py(px(2.0));
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
                            // 顶部紧凑（文字贴近标题栏下缘），左右下保留滚动留白
                            .pt(px(2.0))
                            .pr(px(10.0))
                            .pb(px(10.0))
                            .pl(px(8.0))
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
            .bg(theme::c::rgb(t::PANEL_BG))
            .text_color(theme::c::rgb(t::TEXT))
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
            // 右侧结果区（固定宽度 360）：左侧加一条 1px 竖分隔线。
            // 左右两块都是纯黑底，图片又按比例 letterbox（四周本来就有黑边），
            // 没有这条线时分不清"图片到哪结束、文字从哪开始"。
            .child(
                div()
                    .w(px(360.0))
                    .h_full()
                    .border_l_1()
                    .border_color(theme::c::rgb(t::DIVIDER))
                    .child(right),
            )
    }
}

/// 打开 OCR 识别窗口（左图右文）。窗口大小 = 图片显示宽 + 360，高度按比例。
fn open_result_pin_in_app(
    payload: PinPayload,
    translate: bool,
    cx: &mut App,
) -> AppResult<WindowHandle<gpui_component::Root>> {
    let PinPayload { frame, sx, sy, .. } = payload;
    let img_w = frame.width as f32 / sx;
    let img_h = frame.height as f32 / sy;
    // 显示尺寸约束：
    // - MAX_PIN_W：左侧图片显示宽度**下限**。识别区实际宽 >=720 保持自然宽（不强制缩小，
    //   用户要求）；<720 则强制放大到 720。
    // - MIN_PIN_H：窗口/左侧画布最低高度 120，图片按宽高比居中留白（不强制铺满）。
    const MIN_PIN_H: f32 = 120.0;
    const MAX_PIN_W: f32 = 720.0;
    let display_bounds = cx.primary_display().map(|d| d.bounds()).unwrap_or_else(|| {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: Size::new(px(1280.0), px(800.0)),
        }
    });
    const RIGHT_W: f32 = 360.0;
    // 高度按宽高比、且不超 max_h（避免窄高图放大后窗口超出屏幕）
    let max_h = 700.0_f32;
    let width = img_w.max(MAX_PIN_W);
    let scale = (width / img_w).min(max_h / img_h);
    let disp_w = img_w * scale;
    let disp_h = img_h * scale;
    // 窗口高度 = max(图片显示高, 120)，保证左侧至少 120；绘制端已按宽高比居中留白。
    let win_w = disp_w + RIGHT_W;
    let win_h = disp_h.max(MIN_PIN_H);
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
                title: Some(if translate { "翻译" } else { "OCR 识别" }.into()),
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
            let view = cx.new(|cx| {
                OcrPinView::new(frame, None, disp_w, disp_h, translate, window, cx)
            });
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

/// 校正遮罩窗口的客户端顶到 `desired_client_top`（物理px），校正前打印实测几何。
///
/// 用独立线程 sleep 一帧后调 Win32（App 借期外），避免 `SetWindowPos` 触发
/// `WM_MOVE` 重入 App 报 "RefCell already borrowed"（与 `schedule_client_top_adjustment`
/// 同理；那边走 foreground executor，这里遮罩复用路径拿不到 `&mut App`）。
/// `desired_client_top` = 帧捕获原点（主屏物理 0,0）= 显示原点×scale；dy=0 时不动窗口。
#[cfg(target_os = "windows")]
fn schedule_overlay_client_align(
    hwnd: usize,
    desired_client_top: i32,
    scale: f32,
    display_origin: ub::Point,
    frame_w: u32,
    frame_h: u32,
) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(16));
        let hwnd_ptr = hwnd as *mut core::ffi::c_void;
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            ClientToScreen(hwnd_ptr, &mut pt);
            let client_origin = ub::Point::new(pt.x as f32 / scale, pt.y as f32 / scale);
            tracing::info!(
                "[overlay-align] frame={}x{} scale={:.3} display_origin=({:.1},{:.1}) \
                 client_origin=({:.2},{:.2}) dy={:+.1}px",
                frame_w, frame_h, scale,
                display_origin.x, display_origin.y,
                client_origin.x, client_origin.y,
                client_origin.y - display_origin.y
            );
        }
        adjust_window_client_top(hwnd_ptr, desired_client_top);
    });
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

    /// 只显示文件名：翻译模型清单里的 "onnx/xxx.onnx" 在界面上要跟 OCR 那几行一样
    /// 只给文件名，别把目录前缀当内容显示出来。
    #[test]
    fn file_basename_strips_directory_prefix() {
        assert_eq!(
            super::file_basename("onnx/encoder_model_int8.onnx"),
            "encoder_model_int8.onnx"
        );
        assert_eq!(super::file_basename("tokenizer.json"), "tokenizer.json");
        assert_eq!(super::file_basename("a/b/c.onnx"), "c.onnx");
        assert_eq!(super::file_basename(""), "");
    }

    /// 键帽展示：别名归一（control→Ctrl、cmd→Super）、按键序保留、去掉空段。
    #[test]
    fn hotkey_caps_normalizes_aliases() {
        assert_eq!(super::hotkey_caps("alt+s"), vec!["Alt", "S"]);
        assert_eq!(
            super::hotkey_caps("control+SHIFT+a"),
            vec!["Ctrl", "Shift", "A"]
        );
        assert_eq!(super::hotkey_caps("cmd+space"), vec!["Super", "SPACE"]);
        // 空段（"alt++s"）不能变成空键帽
        assert_eq!(super::hotkey_caps("alt++s"), vec!["Alt", "S"]);
        assert!(super::hotkey_caps("").is_empty());
    }
    use super::*;

    /// 「UI 区域」必须把工具栏本体盖住——工具栏被摆到选区外时（选区小/贴屏幕边），
    /// 这是"悬停操作栏不该算选区外"的唯一依据。顺便锁住弹层展开方向：只往远离
    /// 选区的一侧扩，否则会把选区旁的画布也划成 UI 区域，🚫 就再也不出现了。
    #[test]
    fn ui_zone_covers_toolbar_and_expands_away_from_selection() {
        let screen = ub::Bounds::new(BoundsPoint::new(0.0, 0.0), BoundsPoint::new(1920.0, 1080.0));
        // 选区在上半屏，工具栏会落在它下方
        let sel = ub::Bounds::new(BoundsPoint::new(600.0, 200.0), BoundsPoint::new(1200.0, 400.0));
        let (tx, ty, tw, th) = compute_toolbar_bounds(sel, screen);
        let toolbar_center = BoundsPoint::new(tx + tw / 2.0, ty + th / 2.0);

        let closed = ui_zone(Some(sel), screen, false);
        assert!(
            closed.contains(toolbar_center),
            "工具栏中心必须在 UI 区域内，否则悬停它就会冒禁止角标"
        );

        // 弹层未展开时，工具栏下方 200px 属于画布 → 不在 UI 区域内
        let below = BoundsPoint::new(tx + tw / 2.0, ty + th + 200.0);
        assert!(!closed.contains(below), "弹层没开时工具栏下方应是画布");

        // 弹层展开时，弹层朝下（远离选区）→ 同一位置被划进 UI 区域
        let open = ui_zone(Some(sel), screen, true);
        assert!(open.contains(below), "弹层展开时它占的区域也要算 UI");

        // 但选区那一侧（工具栏上方）不能被扩进来
        let above = BoundsPoint::new(tx + tw / 2.0, ty - 200.0);
        assert!(!open.contains(above), "扩边只能朝远离选区的一侧，不能吃掉画布");

        // 选区贴屏幕底边 → compute_toolbar_bounds 放不下"下方"，只能摆到选区上方，
        // 此时扩边方向必须随之翻转（这才是"工具栏在选区上方"的真实成因）。
        let sel_top = ub::Bounds::new(
            BoundsPoint::new(600.0, 900.0),
            BoundsPoint::new(1200.0, 1050.0),
        );
        let (tx2, ty2, tw2, th2) = compute_toolbar_bounds(sel_top, screen);
        assert!(
            ty2 + th2 <= sel_top.origin.y,
            "前提校验：此时工具栏应被摆到选区上方（ty2={ty2} sel_y={}）",
            sel_top.origin.y
        );
        let open_top = ui_zone(Some(sel_top), screen, true);
        assert!(
            open_top.contains(BoundsPoint::new(tx2 + tw2 / 2.0, ty2 - 200.0)),
            "工具栏在选区上方时，弹层向上展开"
        );
        assert!(
            !open_top.contains(BoundsPoint::new(tx2 + tw2 / 2.0, ty2 + th2 + 200.0)),
            "向下不该扩到画布里"
        );
    }

    /// 识别区域的下限：太小的框多半是误点/误拖，直接跑 OCR 只会白等一次推理。
    /// 这个阈值同时把"工具栏按钮直接用整个截图框"和"框内细化"两条路径统一了。
    #[test]
    fn tiny_ocr_rect_is_rejected() {
        let ok = ub::Bounds::new(BoundsPoint::new(10.0, 10.0), BoundsPoint::new(110.0, 60.0));
        assert!(ocr_rect_usable(ok));
        // 恰好等于阈值不算（要求严格大于）
        let edge = ub::Bounds::new(BoundsPoint::new(10.0, 10.0), BoundsPoint::new(15.0, 15.0));
        assert!(!ocr_rect_usable(edge), "5×5 不该触发识别");
        let dot = ub::Bounds::new(BoundsPoint::new(10.0, 10.0), BoundsPoint::new(10.0, 10.0));
        assert!(!ocr_rect_usable(dot), "零面积（误点）不该触发识别");
        let thin = ub::Bounds::new(BoundsPoint::new(10.0, 10.0), BoundsPoint::new(400.0, 13.0));
        assert!(!ocr_rect_usable(thin), "细长条不该触发识别");
    }

    /// 没有选区时 UI 区域是空矩形：此时不该有任何"禁止点击"判定生效。
    #[test]
    fn ui_zone_is_empty_without_selection() {
        let screen = ub::Bounds::new(BoundsPoint::new(0.0, 0.0), BoundsPoint::new(800.0, 600.0));
        let z = ui_zone(None, screen, true);
        assert_eq!(z.size.x, 0.0);
        assert_eq!(z.size.y, 0.0);
        assert!(!z.contains(BoundsPoint::new(400.0, 300.0)));
    }

    /// 二级弹层的「字号 / 粗细」档位行必须**一行排得下**。
    ///
    /// 回归的是用户反馈：档位 chip 原来用文字自然宽度排，Windows 的系统字体比
    /// Linux 默认字体宽，总宽刚好越过弹层内容宽（= 色板网格宽 352px）→ 最后一个
    /// 档位被挤到第二行。现在 chip 是固定宽度，总宽与字体/DPI 无关，这个测试把
    /// 「固定宽度之和 ≤ 内容宽」锁死，后续加档位/改宽度会先在这里炸。
    #[test]
    fn popover_chip_rows_fit_one_line() {
        use crate::overlay::toolbar::{FONT_SIZES, LINE_WIDTHS};
        let content_w = swatch_grid_width();
        let sizes_w = FONT_SIZES.len() as f32 * FONT_SIZE_CHIP_W
            + (FONT_SIZES.len() - 1) as f32 * CHIP_ROW_GAP;
        let widths_w = LINE_WIDTHS.len() as f32 * LW_CHIP_W
            + (LINE_WIDTHS.len() - 1) as f32 * CHIP_ROW_GAP;
        assert!(
            sizes_w <= content_w,
            "字号档位一行放不下：{sizes_w} > {content_w}（调 FONT_SIZE_CHIP_W / CHIP_ROW_GAP）"
        );
        assert!(
            widths_w <= content_w,
            "粗细档位一行放不下：{widths_w} > {content_w}（调 LW_CHIP_W / CHIP_ROW_GAP）"
        );
    }

    /// 固定宽度里必须放得下 chip 内容：字号 chip 要放两位数字（最大 64），
    /// 粗细 chip 要放「14px 线段 + 5px 间距 + 一位数字」。
    #[test]
    fn popover_chip_widths_leave_room_for_content() {
        let pad = BtnSize::ChipDense.metrics().2;
        assert!(
            FONT_SIZE_CHIP_W - pad * 2.0 >= 20.0,
            "字号 chip 内容区过窄：{}",
            FONT_SIZE_CHIP_W - pad * 2.0
        );
        // 线宽 chip 内容：14（线段）+ 5（间距）+ 2 位余量
        assert!(
            LW_CHIP_W - pad * 2.0 >= 26.0,
            "粗细 chip 内容区过窄：{}",
            LW_CHIP_W - pad * 2.0
        );
    }

    /// Pin 窗口「最大化」后图片的贴合：等比 contain —— 不变形、完整可见、
    /// 且至少一个方向铺满（另一个方向居中留边）。
    #[test]
    fn pin_maximize_fit_keeps_aspect_and_fills_canvas() {
        // 画布 = 铺满工作区后的图片区（1920 × 1040，即工作区高减去标题栏）。
        // 3:2 的图：按高度贴合、宽度居中留边
        let k = contain_scale(300.0, 200.0, 1920.0, 1040.0);
        let (w, h) = (300.0 * k, 200.0 * k);
        assert!(w <= 1920.0 + 1e-3 && h <= 1040.0 + 1e-3, "溢出画布: {w}x{h}");
        assert!((h - 1040.0).abs() < 1e-3, "未铺满高度: {h}");
        assert!((w / h - 1.5).abs() < 1e-4, "比例被破坏: {w}x{h}");

        // 宽图（1920×400）：宽度贴合、高度留边。旧实现是把窗口按图片比例缩到
        // 400 高 → 看起来「放大后高度不是整屏」，现在窗口铺满、图片等比贴合
        let k = contain_scale(1920.0, 400.0, 1920.0, 1040.0);
        let (w, h) = (1920.0 * k, 400.0 * k);
        assert!(w <= 1920.0 + 1e-3 && h <= 1040.0 + 1e-3, "溢出画布: {w}x{h}");
        assert!((w - 1920.0).abs() < 1e-3, "未铺满宽度: {w}");
        assert!(h < 1040.0, "宽图不应拉伸到满高（保持比例）: {h}");

        // 正常尺寸（画布 == 图片）恒为 1:1
        assert!((contain_scale(640.0, 480.0, 640.0, 480.0) - 1.0).abs() < 1e-6);

        // 退化输入不得产生 0/负的绘制尺寸
        assert_eq!(contain_scale(0.0, 480.0, 640.0, 480.0), 1.0);
        assert_eq!(contain_scale(640.0, 480.0, 0.0, 480.0), 1.0);
    }

    /// 手柄样式必须落在正确的位置上：上下边才是横胶囊、左右边才是竖胶囊、四角
    /// 必须是圆点。`handle_positions()` 的顺序一旦变化，这里先炸——而不是等用户
    /// 看到「角上横着一个胶囊」。
    #[test]
    fn handle_visual_sizes_match_positions() {
        let b = ub::Bounds::new(
            BoundsPoint::new(10.0, 20.0),
            BoundsPoint::new(110.0, 70.0),
        );
        let (l, t) = (b.origin.x, b.origin.y);
        let (r, bo) = (b.origin.x + b.size.x, b.origin.y + b.size.y);
        let (mid_x, mid_y) = ((l + r) / 2.0, (t + bo) / 2.0);
        for (i, p) in b.handle_positions().iter().enumerate() {
            let (w, h) = handle_visual_size(i);
            let on_top_bottom = ((p.y - t).abs() < 0.01 || (p.y - bo).abs() < 0.01)
                && (p.x - mid_x).abs() < 0.01;
            let on_left_right = ((p.x - l).abs() < 0.01 || (p.x - r).abs() < 0.01)
                && (p.y - mid_y).abs() < 0.01;
            if on_top_bottom {
                assert!(w > h, "上/下边应是横胶囊, idx={i} size=({w},{h})");
            } else if on_left_right {
                assert!(h > w, "左/右边应是竖胶囊, idx={i} size=({w},{h})");
            } else {
                assert_eq!(
                    (w, h),
                    (HANDLE_CORNER, HANDLE_CORNER),
                    "四角应是圆点, idx={i}"
                );
            }
            // 圆点必须真的是圆：半径取短边一半由 paint_handles 保证，这里锁住尺寸
            if w == h {
                assert_eq!(w, HANDLE_CORNER);
            }
        }
    }

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
    /// 合成一张带"正文段落"的测试画面：白底 + 30px 高的一段文字（比一笔的笔刷窄，
    /// 所以"一次画笔"就该盖满）。字内的笔画都比块小，必须被块平均抹掉。
    fn synth_text_screen(fw: u32, fh: u32) -> Vec<u8> {
        let mut px = vec![0u8; (fw * fh) as usize * 4];
        for y in 0..fh {
            for x in 0..fw {
                let i = ((y * fw + x) * 4) as usize;
                px[i] = 250;
                px[i + 1] = 250;
                px[i + 2] = 252;
                px[i + 3] = 255;
            }
        }
        for row in 0..3u32 {
            let ty = 140 + row * 10;
            for seg in 0..14u32 {
                let x0 = 60 + seg * 20;
                for y in ty..ty + 7 {
                    for x in x0..x0 + 12 {
                        if x < fw && y < fh {
                            let i = ((y * fw + x) * 4) as usize;
                            px[i] = 30;
                            px[i + 1] = 30;
                            px[i + 2] = 40;
                        }
                    }
                }
            }
        }
        px
    }

    /// **预览与提交必须逐像素一致** —— 这是本次改动的核心承诺。
    ///
    /// 用户报的"鼠标移动后效果和最终成型不一致"来自两条各自独立的绘制路径
    /// （预览画亮暗棋盘格、提交才做真像素）。现在两边都走
    /// [`crate::overlay::commands::render_mosaic_stroke_pixels`]，这里把这条不变式
    /// 钉死：同一笔的预览像素与提交后帧像素在笔迹范围内必须完全相同。
    ///
    /// 顺带守住第二件事：**一笔就把原文墨色抹掉**（遮挡），且块间保留色差。
    #[test]
    /// **换色只影响新笔，不改前面已画的笔**（用户报"第一个模糊也变色了"）。
    ///
    /// 缺陷成因：已提交笔迹的显示层把所有马赛克命令**合并成一层**渲染，颜色只取最后一
    /// 笔的 —— 于是第二笔用黑色时，第一笔的红色区域也被重新渲染成黑色。
    /// 这里钉住"每笔各自带颜色"这条不变式：按颜色渲染时输入必须逐笔独立。
    #[test]
    fn mosaic_strokes_keep_their_own_color() {
        use crate::overlay::drawing::{Point as DPoint, RGBA};

        let red = RGBA::new(0xFF, 0x00, 0x00, 0xFF);
        let black = RGBA::new(0x00, 0x00, 0x00, 0xFF);
        let stroke = |y: f32| {
            (
                DPoint::new(10.0, y),
                DPoint::new(50.0, y + 20.0),
            )
        };
        let cmds: Vec<std::sync::Arc<DrawCommand>> = vec![
            std::sync::Arc::new(DrawCommand::Mosaic {
                regions: vec![stroke(10.0)],
                block_size: 12,
                color: red,
            }),
            std::sync::Arc::new(DrawCommand::Mosaic {
                regions: vec![stroke(100.0)],
                block_size: 12,
                color: black,
            }),
            // 非马赛克命令不应混进来
            std::sync::Arc::new(DrawCommand::Rectangle {
                rect: (DPoint::new(0.0, 0.0), DPoint::new(10.0, 10.0)),
                color: red,
                line_width: 2.0,
            }),
        ];

        let strokes = mosaic_strokes_of(cmds.iter());
        assert_eq!(strokes.len(), 2, "应当逐笔一条（非马赛克命令要过滤掉）");
        assert_eq!(strokes[0].2, red, "第一笔必须保持自己的颜色");
        assert_eq!(strokes[1].2, black, "第二笔必须保持自己的颜色");
        // 逐笔独立渲染的前提：两笔的笔迹区域不能混在一起
        assert_eq!(strokes[0].0.len(), 1);
        assert_eq!(strokes[1].0.len(), 1);
        assert_ne!(
            strokes[0].0[0].0.y, strokes[1].0[0].0.y,
            "两笔的区域被合并了 —— 合并就会用同一个颜色渲染"
        );
    }

    fn mosaic_preview_matches_commit_pixel_for_pixel() {
        use crate::overlay::commands::{apply_commands, render_mosaic_stroke_pixels};
        use crate::overlay::drawing::Point as DPoint;

        let (fw, fh) = (520u32, 300u32);
        let orig = synth_text_screen(fw, fh);
        let (brush, block_size) = mosaic_geom(3.0);
        let brush = brush as i32;
        let (cx0, cx1, cy) = (60i32, 460i32, 155i32);
        let half = brush as f32 / 2.0;
        let mut regions: Vec<(DPoint, DPoint)> = Vec::new();
        let mut cx = cx0 as f32;
        while cx <= cx1 as f32 {
            regions.push((
                DPoint::new(cx - half, cy as f32 - half),
                DPoint::new(cx + half, cy as f32 + half),
            ));
            cx += brush as f32 * 0.5;
        }
        // 生产色：默认色板是不透明的红（按 0x60 半透明叠加）
        let color = RGBA::new(0xE6, 0x22, 0x22, 0xFF);

        // ① 预览（走共用路径）
        let (preview, px, py, pw, ph) = render_mosaic_stroke_pixels(
            &orig,
            fw,
            fh,
            &regions,
            block_size,
            color,
        )
        .expect("预览应当产出像素");

        // ② 提交（走真实提交路径）
        let mut committed = crate::capture::CapturedFrame {
            width: fw,
            height: fh,
            pixels: orig.clone(),
        };
        apply_commands(
            &mut committed,
            0.0,
            0.0,
            &[DrawCommand::Mosaic {
                regions: regions.clone(),
                block_size,
                color,
            }],
        )
        .unwrap();

        // ③ 逐像素比对：预览**不透明**的每个像素，都必须与提交结果完全相同。
        //    （预览的半透明像素是"笔迹没覆盖到"的地方，叠在帧图上不改变画面。）
        let mut compared = 0usize;
        let mut diff = 0usize;
        let mut first_bad = None;
        for y in 0..ph as i32 {
            for x in 0..pw as i32 {
                let p_off = ((y * pw as i32 + x) * 4) as usize;
                if preview[p_off + 3] == 0 {
                    continue;
                }
                let c_off = ((((py + y) as u32) * fw + (px + x) as u32) * 4) as usize;
                compared += 1;
                if preview[p_off..p_off + 4] != committed.pixels[c_off..c_off + 4] {
                    diff += 1;
                    if first_bad.is_none() {
                        first_bad = Some((
                            px + x,
                            py + y,
                            [preview[p_off], preview[p_off + 1], preview[p_off + 2]],
                            [
                                committed.pixels[c_off],
                                committed.pixels[c_off + 1],
                                committed.pixels[c_off + 2],
                            ],
                        ));
                    }
                }
            }
        }
        assert!(compared > 5000, "比对的像素太少：{compared}");
        assert_eq!(
            diff, 0,
            "预览与提交有 {diff}/{compared} 个像素不同（首个差异 {:?}）—— 拖动中与成型后不是同一张图",
            first_bad
        );

        // ④ 遮挡成立：笔迹带内不应再有原文墨色（30）
        let core_y0 = cy - brush / 2 + 4;
        let core_y1 = cy + brush / 2 - 4;
        let mut ink = 0usize;
        for y in core_y0..core_y1 {
            for x in (cx0 + brush)..(cx1 - brush) {
                let i = ((y as u32 * fw + x as u32) * 4) as usize;
                if committed.pixels[i] == 30 {
                    ink += 1;
                }
            }
        }
        assert_eq!(ink, 0, "一笔之后仍有 {ink} 个原文墨色像素 —— 没盖住");
    }

    /// **一笔马赛克就要把内容盖住**（用户的核心诉求），并且块之间有色差。
    ///
    /// 针对的缺陷：原来每块只取左上角一个像素（等于把内容搬个位置）、块只有 4px
    /// （块里装不下一笔画、块平均等于原文局部色），于是"要反复涂抹才能遮挡"。
    ///
    /// 叠加 `SCREENSHOT_RS_MOSAIC_DUMP=<前缀>` 可导出**原图 / 成图** PNG 供肉眼核对
    /// （盖没盖住最终只能靠眼睛定案，断言只保证必要条件）。
    #[test]
    fn mosaic_stroke_hides_text_in_one_pass() {
        use crate::overlay::commands::apply_commands;
        use crate::overlay::drawing::Point as DPoint;

        let (fw, fh) = (520u32, 300u32);
        let orig = synth_text_screen(fw, fh);

        // 生产参数（默认档：线宽 3）
        let lw = 3.0f32;
        let (brush, block_size) = mosaic_geom(lw);
        let brush = brush as i32;

        // **一笔**：沿正文横刷一次（就是用户"一次画笔"的动作）
        let (cx0, cx1, cy) = (60i32, 460i32, 155i32);
        let (ry0, ry1) = (cy - brush / 2, cy - brush / 2 + brush);
        let mut regions: Vec<(DPoint, DPoint)> = Vec::new();
        let mut cx = cx0 as f32;
        while cx <= cx1 as f32 {
            regions.push((
                DPoint::new(cx - brush as f32 / 2.0, ry0 as f32),
                DPoint::new(cx - brush as f32 / 2.0 + brush as f32, ry1 as f32),
            ));
            cx += (brush as f32 * 0.5).max(1.0);
        }
        assert!(regions.len() < 60, "一笔不该是几百个方块");

        // 默认色板是不透明的红（按 0x60 半透明叠加）；要单看马赛克本身时
        // 用 SCREENSHOT_RS_MOSAIC_NO_TINT=1 换成 TRANSPARENT。
        let preview_color = if std::env::var_os("SCREENSHOT_RS_MOSAIC_NO_TINT").is_some() {
            RGBA::TRANSPARENT
        } else {
            RGBA::new(0xE6, 0x22, 0x22, 0xFF)
        };
        let mut committed = crate::capture::CapturedFrame {
            width: fw,
            height: fh,
            pixels: orig.clone(),
        };
        apply_commands(
            &mut committed,
            0.0,
            0.0,
            &[DrawCommand::Mosaic {
                regions: regions.clone(),
                block_size,
                color: preview_color,
            }],
        )
        .unwrap();

        let at = |x: i32, y: i32| -> u8 {
            committed.pixels[((y as u32 * fw + x as u32) * 4) as usize]
        };
        // 笔迹**核心带**（避开笔刷边缘那一圈，那里是硬的边界）
        let (core_y0, core_y1) = (cy - brush / 2 + 4, cy + brush / 2 - 4);
        let (core_x0, core_x1) = (cx0 + brush, cx1 - brush);

        // ---- ① 一笔之后，核心带内**原文的墨色必须消失** ----
        //
        // 这是"一笔就遮挡"最直接的度量：原文墨色是 30，一笔刷过之后不应再有像素是 30。
        let mut total = 0usize;
        let mut ink_left = 0usize;
        let mut distinct = std::collections::BTreeSet::new();
        for y in core_y0..core_y1 {
            for x in core_x0..core_x1 {
                let v = at(x, y);
                total += 1;
                distinct.insert(v);
                if v == 30 {
                    ink_left += 1;
                }
            }
        }
        assert!(total > 1000, "取样区太小：{total}");
        assert_eq!(
            ink_left, 0,
            "笔迹带内还有 {ink_left} 个像素是原文的墨色（30）—— 一笔没盖住"
        );

        // ---- ② 带内被**量化成块**：取值种类应是"块的数量级"而非像素数 ----
        //
        // 块内整块同色 → 一条 48px 高、几百像素宽的带子里只应有几十种色值。
        // 这里不断言"逐块完全均匀"：笔迹最外一圈的块会被笔迹边界切掉一条，那一圈本来
        // 就是硬的边界（马赛克没有渐隐），块均值天然与整块不同，不是缺陷。
        let blocks_x = ((core_x1 - core_x0) / block_size as i32).max(1);
        let blocks_y = ((core_y1 - core_y0) / block_size as i32).max(1);
        let block_budget = (blocks_x * blocks_y + blocks_x + blocks_y + 8) as usize;
        assert!(
            distinct.len() <= block_budget,
            "带内取值种类 {} 超过块数量级 {}（块 {block_size}px，区域 {blocks_x}×{blocks_y}），没量化成块",
            distinct.len(),
            block_budget
        );
        // 块之间要有色差（"多个不同颜色的小方块"），不是一片死色
        assert!(
            distinct.len() >= 3,
            "带内只有 {} 种色值，色块看不出差异",
            distinct.len()
        );

        // ---- ①b 预览层按**真实偏移**叠加到帧图上，必须与提交成图一致 ----
        //
        // 预览在渲染时是这样用的：抠出一块（对齐裁剪）→ 算 → 用 `bounds` 画回去。
        // 这里把这套算术完整走一遍，验证"叠加回去"的结果与提交成图相同 ——
        // 位置/尺寸算错（偏移、单位、缩放）都会在这里露出来。
        {
            use crate::overlay::commands::{mosaic_aligned_crop, render_mosaic_stroke_pixels};
            let (crop, cw, ch, cx0, cy0, local) =
                mosaic_aligned_crop(&orig, fw, fh, &regions, block_size).expect("裁剪应当成功");
            let (pix, px, py, pw, ph) =
                render_mosaic_stroke_pixels(&crop, cw, ch, &local, block_size, preview_color)
                    .expect("预览渲染应当成功");
            let mut composite = orig.clone();
            for y in 0..ph as i32 {
                for x in 0..pw as i32 {
                    let s_off = ((y * pw as i32 + x) * 4) as usize;
                    if pix[s_off + 3] == 0 {
                        continue;
                    }
                    let (tx, ty) = (cx0 + px + x, cy0 + py + y);
                    if tx < 0 || ty < 0 || tx >= fw as i32 || ty >= fh as i32 {
                        continue;
                    }
                    let d_off = ((ty as u32 * fw + tx as u32) * 4) as usize;
                    let a = pix[s_off + 3] as u32;
                    let inv = 255 - a;
                    for c in 0..3 {
                        composite[d_off + c] = ((pix[s_off + c] as u32 * a
                            + composite[d_off + c] as u32 * inv)
                            / 255) as u8;
                    }
                }
            }
            let diff = (0..composite.len())
                .step_by(4)
                .filter(|&i| composite[i] != committed.pixels[i])
                .count();
            assert_eq!(
                diff, 0,
                "把预览层按 bounds 叠加回帧图后有 {diff} 个像素与提交成图不同 —— 预览位置/尺寸算错了"
            );
            // 顺带导出合成图，供肉眼核对（预览画出来就是这张）
            if let Ok(prefix) = std::env::var("SCREENSHOT_RS_MOSAIC_DUMP") {
                let img = image::RgbaImage::from_raw(fw, fh, composite).expect("构造图片失败");
                let path = format!("{prefix}_composited.png");
                img.save(&path).expect("保存 PNG 失败");
                eprintln!("[mosaic dump] composited → {path}");
            }
        }

        // ---- ③ 导出对照图（可选） ----
        if let Ok(prefix) = std::env::var("SCREENSHOT_RS_MOSAIC_DUMP") {
            for (tag, px) in [("original", &orig), ("committed", &committed.pixels)] {
                let img = image::RgbaImage::from_raw(fw, fh, px.clone()).expect("构造图片失败");
                let path = format!("{prefix}_{tag}.png");
                img.save(&path).expect("保存 PNG 失败");
                eprintln!("[mosaic dump] {tag} → {path}");
            }
            eprintln!(
                "[mosaic dump] 笔刷 {brush}px 块 {block_size}px 色值 {} 种（一笔 {} 个 stamp）",
                distinct.len(),
                regions.len()
            );
        }
    }

}
