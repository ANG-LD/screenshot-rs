//! 覆盖窗口选区拖拽逻辑
//!
//! `SelectionState` 是纯逻辑状态机（不依赖 GPUI），方便测试。
//! 实际的鼠标事件分发由 Task 14 的 GPUI 渲染层处理。

use crate::utils::bounds::{Bounds, Handle, Point};

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
    /// 拖拽某个手柄调整大小
    ///
    /// `grab_offset` = 鼠标按下点相对手柄中心的偏移；mouse_move 时
    /// 手柄跟随 `mouse - grab_offset`，从而手柄不会"跳"到鼠标位置。
    Resizing {
        handle: Handle,
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
    /// - 如果当前已有选区且点击点在某个 handle 容差范围内 → 进入 `Resizing`，记录 grab_offset
    /// - 否则如果点击点在选区内 → 进入 `Moving` 状态，记录 grab_offset
    /// - 否则 → 进入 `Creating` 状态，并把 bounds 初始化为零大小的矩形（按下点=当前点）
    pub fn mouse_down(&mut self, p: Point) {
        if let Some(existing) = self.bounds {
            // 8 个 handle 优先于"在选区内"判定，否则在 handle 上点击会触发 Moving
            if let Some(handle) = existing.hit_handle(p, HANDLE_HALF_SIZE) {
                let positions = existing.handle_positions();
                let hp = positions[handle as usize];
                self.drag = DragState::Resizing {
                    handle,
                    grab_offset: Point::new(p.x - hp.x, p.y - hp.y),
                };
                self.drag_start = p;
                return;
            }
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
    /// - `Resizing`：用 grab_offset 计算新 handle 位置，按 handle 类型更新 bounds，裁剪到屏幕内
    pub fn mouse_move(&mut self, p: Point) {
        match self.drag {
            DragState::Idle => {}
            DragState::Creating => {
                // 从按下点 drag_start 到当前点 p 构造矩形并归一化，裁剪到屏幕内
                self.bounds = Some(
                    Bounds::new(self.drag_start, p)
                        .normalize()
                        .clamp_inside(self.screen_bounds),
                );
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
                if let Some(b) = self.bounds {
                    // 手柄当前位置 + 偏移 = 鼠标；反过来新手柄位置 = 鼠标 - 偏移
                    let new_handle = Point::new(p.x - grab_offset.x, p.y - grab_offset.y);
                    self.bounds = Some(apply_resize(b, handle, new_handle, self.screen_bounds));
                }
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

/// resize handle 命中容差的一半（正方形 ±HANDLE_HALF_SIZE 像素）
///
/// 8 像素容差在 HiDPI（2x）下视觉上是 4 逻辑像素，仍然容易点中又不会误触。
const HANDLE_HALF_SIZE: f32 = 8.0;

/// 给定旧 bounds、被拖动的 handle 和 handle 的新位置，返回新的 bounds
///
/// 实现要点：每个 handle 拖动时"对侧"保持固定。例如 TopLeft 拖动 → 右下角固定；
/// Bottom 拖动 → 上边固定。完成后用 `clamp_inside` 限制在屏幕内，
/// 用 `normalize` 保证 size 为正。
pub(crate) fn apply_resize(
    bounds: Bounds,
    handle: Handle,
    new_handle_pos: Point,
    screen_bounds: Bounds,
) -> Bounds {
    let mut new = bounds;
    let right = bounds.origin.x + bounds.size.x;
    let bottom = bounds.origin.y + bounds.size.y;

    match handle {
        Handle::TopLeft => {
            new.origin = new_handle_pos;
            new.size = Point::new(right - new.origin.x, bottom - new.origin.y);
        }
        Handle::Top => {
            new.origin.y = new_handle_pos.y;
            new.size.y = bottom - new.origin.y;
        }
        Handle::TopRight => {
            new.origin.y = new_handle_pos.y;
            new.size = Point::new(new_handle_pos.x - bounds.origin.x, bottom - new.origin.y);
        }
        Handle::Left => {
            new.origin.x = new_handle_pos.x;
            new.size.x = right - new.origin.x;
        }
        Handle::Right => {
            new.size.x = new_handle_pos.x - bounds.origin.x;
        }
        Handle::BottomLeft => {
            new.origin.x = new_handle_pos.x;
            new.size = Point::new(right - new.origin.x, new_handle_pos.y - bounds.origin.y);
        }
        Handle::Bottom => {
            new.size.y = new_handle_pos.y - bounds.origin.y;
        }
        Handle::BottomRight => {
            new.size = Point::new(
                new_handle_pos.x - bounds.origin.x,
                new_handle_pos.y - bounds.origin.y,
            );
        }
    }
    new.normalize().clamp_inside(screen_bounds)
}
