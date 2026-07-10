//! 覆盖窗口选区拖拽逻辑
//!
//! `SelectionState` 是纯逻辑状态机（不依赖 GPUI），方便测试。
//! 实际的鼠标事件分发由 Task 14 的 GPUI 渲染层处理。

use crate::utils::bounds::{Bounds, Point};

/// 选区拖拽状态
///
/// 用枚举而不是多个 bool 字段（如 `is_creating`、`is_moving`）的原因：
/// 同一时刻只能处于一种拖拽状态，枚举保证了互斥性，编译期就能阻止非法组合。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragState {
    /// 未拖拽：等待用户按下鼠标
    Idle,
    /// 正在拖拽新选区：按下点到当前点构成的矩形就是选区
    Creating,
    /// 拖拽已有选区移动：记录按下点相对选区原点的偏移，避免选区"跳"到鼠标位置
    Moving {
        /// 鼠标按下点相对选区原点的偏移（grab point）
        grab_offset: Point,
    },
    /// 拖拽某个手柄调整大小（0=左上, 1=上, 2=右上, 3=右, 4=右下, 5=下, 6=左下, 7=左）
    ///
    /// MVP 暂不实现，仅保留数据结构以便后续扩展。
    Resizing {
        /// 手柄索引（0..8）
        handle: u8,
        /// 鼠标按下点相对手柄位置的偏移
        grab_offset: Point,
    },
}

/// 选区状态机
///
/// 维护屏幕边界、当前选区以及拖拽状态。所有公开方法都是纯逻辑（不读 GPUI 句柄），
/// 可以直接在单元测试中构造状态并驱动鼠标事件。
pub struct SelectionState {
    /// 屏幕边界（用于裁剪选区到屏幕内）
    pub screen_bounds: Bounds,
    /// 当前的归一化选区（origin + size 都已规范化，size 为正）
    pub bounds: Option<Bounds>,
    /// 拖拽状态
    pub drag: DragState,
    /// 拖拽起始点：Creating 时是按下的点，Moving 时也记录以便扩展
    pub drag_start: Point,
}

impl SelectionState {
    /// 构造一个新的选区状态，初始无选区、处于 Idle
    pub fn new(screen_bounds: Bounds) -> Self {
        Self {
            screen_bounds,
            bounds: None,
            drag: DragState::Idle,
            drag_start: Point::ZERO,
        }
    }

    /// 鼠标按下
    ///
    /// 行为：
    /// - 如果当前已有选区且点击点在选区内 → 进入 `Moving` 状态，记录 grab_offset
    /// - 否则 → 进入 `Creating` 状态，并把 bounds 初始化为零大小的矩形（按下点=当前点）
    pub fn mouse_down(&mut self, p: Point) {
        if let Some(existing) = self.bounds {
            if existing.contains(p) {
                // 在已有选区内点击 → 移动
                // grab_offset 用于在 mouse_move 时让选区跟随鼠标而不"跳"到鼠标位置
                self.drag = DragState::Moving {
                    grab_offset: Point::new(p.x - existing.origin.x, p.y - existing.origin.y),
                };
                self.drag_start = p;
                return;
            }
        }
        // 否则开始创建新选区
        self.drag = DragState::Creating;
        self.drag_start = p;
        // 用一个零大小的矩形占位（按下点 == 当前点），后续 mouse_move 会扩展它
        self.bounds = Some(Bounds::new(p, p).normalize());
    }

    /// 鼠标移动
    ///
    /// 根据当前拖拽状态更新 bounds：
    /// - `Idle`：忽略
    /// - `Creating`：用 drag_start 和当前点构造矩形
    /// - `Moving`：用 grab_offset 计算新原点，并裁剪到屏幕边界内
    /// - `Resizing`：MVP 暂不实现（保留占位）
    pub fn mouse_move(&mut self, p: Point) {
        match self.drag {
            DragState::Idle => {}
            DragState::Creating => {
                // 从按下点 drag_start 到当前点 p 构造矩形并归一化
                self.bounds = Some(Bounds::new(self.drag_start, p).normalize());
            }
            DragState::Moving { grab_offset } => {
                if let Some(b) = self.bounds {
                    // 用 grab_offset 计算新原点，让选区跟随鼠标
                    let new_origin =
                        Point::new(p.x - grab_offset.x, p.y - grab_offset.y);
                    // 重新用 origin + size 构造（size 保持不变，仅移动 origin）
                    let moved = Bounds::new(
                        new_origin,
                        Point::new(
                            new_origin.x + b.size.x,
                            new_origin.y + b.size.y,
                        ),
                    )
                    .normalize()
                    // 裁剪到屏幕边界，避免选区被拖出屏幕外
                    .clamp_inside(self.screen_bounds);
                    self.bounds = Some(moved);
                }
            }
            DragState::Resizing { handle, grab_offset } => {
                // MVP 暂不实现手柄调整大小
                let _ = (handle, grab_offset);
            }
        }
    }

    /// 鼠标松开
    ///
    /// 重置为 Idle 状态。注意：bounds 保留下来，以便后续点击进入 Moving 或重新创建。
    pub fn mouse_up(&mut self) {
        self.drag = DragState::Idle;
    }

    /// 获取当前选区（已归一化）
    ///
    /// 返回 Option 是因为可能尚未创建任何选区。
    pub fn current(&self) -> Option<Bounds> {
        self.bounds
    }
}

// ============================================================================
// GPUI 渲染层入口（Task 14）
// ============================================================================
//
// 设计要点：
// 1. `SelectionState` 是纯逻辑状态机，与 GPUI 解耦，方便单元测试。
// 2. `run_overlay` 是渲染层的入口，负责：
//    a) 在 GPUI 主线程中创建全屏覆盖窗口
//    b) 把鼠标/键盘事件转发给 SelectionState
//    c) 渲染背景图 + 选区矩形 + 工具栏
//    d) 阻塞至用户点完成/取消/按 Esc
// 3. MVP 阶段：仅返回"全屏占位 bounds"，让 trigger_screenshot 流程能跑通。
//    真实 GPUI 集成留到后续任务（任务 15 工具栏 + 后续 GPUI 绑定任务）。
// ---------------------------------------------------------------------------

/// 在新的 GPUI 窗口中运行覆盖层。
///
/// 参数：
/// - `cx`：GPUI 应用上下文（用于创建窗口）。
/// - `screen_bounds`：屏幕尺寸（用于将选区裁剪到屏幕内）。
/// - `_frame`：已捕获的全屏像素（用于渲染背景图）。
///
/// 返回：
/// - `Some(bounds)`：用户确认（点完成 / 按 Enter），`bounds` 是最终选区（屏幕坐标）。
/// - `None`：用户取消（按 Esc / 点取消 / 关闭窗口）。
///
/// MVP 实现：仅返回全屏占位 bounds（让 trigger_screenshot 流程能跑通）。
pub fn run_overlay(
    cx: &mut gpui::App,
    screen_bounds: crate::utils::bounds::Bounds,
    _frame: crate::capture::CapturedFrame,
) -> Option<crate::utils::bounds::Bounds> {
    // 占位实现说明：
    // 真实集成时需要：
    //   1. cx.open_window(...) 创建一个全屏、置顶、透明背景的窗口
    //   2. 注册鼠标 down/move/up、键盘 Esc/Enter 事件 handler
    //   3. handler 内调用 SelectionState::mouse_down / mouse_move / mouse_up
    //   4. frame 内渲染：背景图 + dim 遮罩 + 选区边框 + 工具栏
    //   5. 用 channel/oneshot 把"用户完成"事件传回 caller，阻塞当前 run()
    // GPUI 0.x 在 Linux 上对 Wayland/X11 的支持仍在演进，先用占位实现。

    // 显式消费参数，避免 dead_code 告警
    let _ = (cx, screen_bounds, _frame.width, _frame.height);

    // 临时返回全屏 bounds（用帧尺寸构造），打通 trigger_screenshot 流程
    Some(crate::utils::bounds::Bounds::new(
        crate::utils::bounds::Point::ZERO,
        crate::utils::bounds::Point::new(_frame.width as f32, _frame.height as f32),
    ))
}

// 防止 WindowBounds / GpuiBounds 类型导入未被使用时告警
// （后续 GPUI 集成任务会真正用到这两个类型，留个最小引用即可）
#[allow(dead_code)]
fn _ensure_window_bounds_import(_b: gpui::WindowBounds) -> gpui::Bounds<gpui::Pixels> {
    gpui::Bounds::default()
}
