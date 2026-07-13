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
