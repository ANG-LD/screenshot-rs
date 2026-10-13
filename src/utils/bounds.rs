//! 通用几何类型：`Point`（坐标点）和 `Bounds<P>`（矩形区域）。
//!
//! 设计为 GPUI 无关的纯逻辑，方便单元测试。

/// 选区四周的 8 个 resize handle
///
/// 索引顺序对应 `SelectionState` 中已有的 `DragState::Resizing::handle: u8`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    TopLeft = 0,
    Top = 1,
    TopRight = 2,
    Left = 3,
    Right = 4,
    BottomLeft = 5,
    Bottom = 6,
    BottomRight = 7,
}

impl Handle {
    /// 全部 8 个 handle 的数组（按 `as u8` 索引顺序）
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Left,
        Handle::Right,
        Handle::BottomLeft,
        Handle::Bottom,
        Handle::BottomRight,
    ];
}

/// 二维坐标点，浮点精度（屏幕坐标）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 矩形区域：原点 + 大小
///
/// 构造时 size 可以为负（用户从右下往左上拖），需要 `normalize` 后再使用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds<P = Point> {
    pub origin: P,
    pub size: P,
}

impl Bounds<Point> {
    /// 从两个对角点构造矩形。`from` 是按下点，`to` 是当前点。
    pub const fn new(from: Point, to: Point) -> Self {
        let size_x = to.x - from.x;
        let size_y = to.y - from.y;
        Self {
            origin: from,
            size: Point::new(size_x, size_y),
        }
    }

    /// 规范化：如果 size 为负则翻转 origin，让 size 永远为正
    pub fn normalize(self) -> Self {
        let mut b = self;
        if b.size.x < 0.0 {
            b.origin.x += b.size.x;
            b.size.x = -b.size.x;
        }
        if b.size.y < 0.0 {
            b.origin.y += b.size.y;
            b.size.y = -b.size.y;
        }
        b
    }

    /// 判断点是否在矩形内（含边界）
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.origin.x
            && p.x <= self.origin.x + self.size.x
            && p.y >= self.origin.y
            && p.y <= self.origin.y + self.size.y
    }

    /// 将当前矩形裁剪到 `limits` 范围内（用于屏幕边界保护）
    ///
    /// 左右上下四个方向均裁剪，origin 和 size 最终都保证在 limits 内。
    pub fn clamp_inside(self, limits: Bounds) -> Bounds {
        let mut b = self;
        let max_x = limits.origin.x + limits.size.x;
        let max_y = limits.origin.y + limits.size.y;
        // 左裁剪
        if b.origin.x < limits.origin.x {
            b.size.x = (b.size.x - (limits.origin.x - b.origin.x)).max(0.0);
            b.origin.x = limits.origin.x;
        }
        // 右裁剪：先裁剪 size，再兜底裁剪 origin（防止 origin 独自超出右边界）
        if b.origin.x + b.size.x > max_x {
            b.size.x = (max_x - b.origin.x).max(0.0);
        }
        if b.origin.x > max_x {
            b.origin.x = max_x;
        }
        // 上裁剪
        if b.origin.y < limits.origin.y {
            b.size.y = (b.size.y - (limits.origin.y - b.origin.y)).max(0.0);
            b.origin.y = limits.origin.y;
        }
        // 下裁剪
        if b.origin.y + b.size.y > max_y {
            b.size.y = (max_y - b.origin.y).max(0.0);
        }
        if b.origin.y > max_y {
            b.origin.y = max_y;
        }
        b
    }

    /// 把矩形收进 `limits`，但**优先保持尺寸**：先把 origin 往左/上挪回来，
    /// 只有当矩形本身比 `limits` 还大时，才收缩 size。
    ///
    /// 与 `clamp_inside` 的区别在"触边怎么办"：`clamp_inside` 一律**先缩 size** ——
    /// 把选区吸回屏幕时这是对的；但文字框用它会被截图框边界压窄，内容溢出视口，
    /// gpui-base 的编辑器随即画出一条横向滚动条（见 `overlay/window.rs` 的文字框
    /// auto-grow）。文字框的诉求相反：宁可挪位置，也要把整段文字完整放下。
    pub fn clamp_keeping_size(self, limits: Bounds) -> Bounds {
        let mut b = self.normalize();
        let max_x = limits.origin.x + limits.size.x;
        let max_y = limits.origin.y + limits.size.y;

        // 1) 右/下越界 → 整体往左/上挪，能挪多少挪多少（挪完尺寸不变）
        let overflow_x = (b.origin.x + b.size.x - max_x).max(0.0);
        let room_x = (b.origin.x - limits.origin.x).max(0.0);
        b.origin.x -= overflow_x.min(room_x);
        let overflow_y = (b.origin.y + b.size.y - max_y).max(0.0);
        let room_y = (b.origin.y - limits.origin.y).max(0.0);
        b.origin.y -= overflow_y.min(room_y);

        // 2) 起点落在左上界外 → 抬回界内并裁掉多出的部分
        if b.origin.x < limits.origin.x {
            b.size.x = (b.size.x - (limits.origin.x - b.origin.x)).max(0.0);
            b.origin.x = limits.origin.x;
        }
        if b.origin.y < limits.origin.y {
            b.size.y = (b.size.y - (limits.origin.y - b.origin.y)).max(0.0);
            b.origin.y = limits.origin.y;
        }

        // 3) 挪到头仍越界（矩形比 limits 还大）→ 收缩到界内
        b.size.x = b.size.x.min((max_x - b.origin.x).max(0.0));
        b.size.y = b.size.y.min((max_y - b.origin.y).max(0.0));
        b
    }

    /// 8 个 resize handle 的中心点（顺时针从左上开始）
    pub fn handle_positions(self) -> [Point; 8] {
        let l = self.origin.x;
        let t = self.origin.y;
        let r = self.origin.x + self.size.x;
        let b = self.origin.y + self.size.y;
        let mx = self.origin.x + self.size.x / 2.0;
        let my = self.origin.y + self.size.y / 2.0;
        [
            Point::new(l, t),     // TopLeft
            Point::new(mx, t),    // Top
            Point::new(r, t),     // TopRight
            Point::new(l, my),    // Left
            Point::new(r, my),    // Right
            Point::new(l, b),     // BottomLeft
            Point::new(mx, b),    // Bottom
            Point::new(r, b),     // BottomRight
        ]
    }

    /// 判断点是否落在某个 handle 的容差范围内（容差为 ±half_size 像素的方框）
    pub fn hit_handle(self, p: Point, half_size: f32) -> Option<Handle> {
        let positions = self.handle_positions();
        for (i, hp) in positions.iter().enumerate() {
            if (p.x - hp.x).abs() <= half_size && (p.y - hp.y).abs() <= half_size {
                return Some(Handle::ALL[i]);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds {
        Bounds::new(Point::new(x, y), Point::new(x + w, y + h))
    }

    fn screen() -> Bounds {
        rect(0.0, 0.0, 1920.0, 1080.0)
    }

    /// 文字长出屏幕右侧：尺寸必须保留，只把框整体左移（这正是文字框"自适应文字长度"）
    #[test]
    fn clamp_keeping_size_shifts_left_and_keeps_width() {
        let out = rect(1800.0, 100.0, 400.0, 44.0).clamp_keeping_size(screen());
        assert_eq!(out.size.x, 400.0, "宽度不能被压窄");
        assert_eq!(out.size.y, 44.0);
        assert_eq!(out.origin.x, 1520.0, "应整体左移到贴着屏幕右缘");
        assert_eq!(out.origin.y, 100.0);
    }

    /// 文字框向下越界：同样只挪位置
    #[test]
    fn clamp_keeping_size_shifts_up_and_keeps_height() {
        let out = rect(100.0, 1050.0, 300.0, 44.0).clamp_keeping_size(screen());
        assert_eq!((out.size.x, out.size.y), (300.0, 44.0));
        assert_eq!(out.origin.y, 1036.0);
    }

    /// 只有比整屏还宽/高时才收缩（极端情况）
    #[test]
    fn clamp_keeping_size_shrinks_only_when_larger_than_limits() {
        let out = rect(0.0, 0.0, 2500.0, 44.0).clamp_keeping_size(screen());
        assert_eq!(out.size.x, 1920.0);
        assert_eq!(out.origin.x, 0.0);
    }

    #[test]
    fn clamp_keeping_size_leaves_inside_rect_untouched() {
        let r = rect(450.0, 400.0, 216.0, 44.0);
        assert_eq!(r.clamp_keeping_size(screen()), r);
    }

    /// 与 `clamp_inside` 的语义对照：旧的"裁回选区"会把 700 宽的文本框压到 550，
    /// 内容随即溢出视口 → 编辑器画出横向滚动条（用户反馈的问题）；新策略保留 700。
    #[test]
    fn clamp_keeping_size_keeps_text_width_where_clamp_inside_would_narrow_it() {
        let selection = rect(400.0, 300.0, 600.0, 400.0);
        let text_box = rect(450.0, 400.0, 700.0, 44.0);
        assert_eq!(text_box.clamp_inside(selection).size.x, 550.0, "旧行为：被选区压窄");
        assert_eq!(
            text_box.clamp_keeping_size(screen()).size.x,
            700.0,
            "新行为：整段文字宽度保留"
        );
    }
}
