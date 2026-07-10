//! 通用几何类型：`Point`（坐标点）和 `Bounds<P>`（矩形区域）。
//!
//! 设计为 GPUI 无关的纯逻辑，方便单元测试。

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
    pub fn clamp_inside(self, limits: Bounds) -> Bounds {
        let mut b = self;
        // 左裁剪
        if b.origin.x < limits.origin.x {
            let dx = limits.origin.x - b.origin.x;
            b.origin.x = limits.origin.x;
            b.size.x = (b.size.x - dx).max(0.0);
        }
        // 上裁剪
        if b.origin.y < limits.origin.y {
            let dy = limits.origin.y - b.origin.y;
            b.origin.y = limits.origin.y;
            b.size.y = (b.size.y - dy).max(0.0);
        }
        // 右裁剪
        let max_x = limits.origin.x + limits.size.x;
        if b.origin.x + b.size.x > max_x {
            b.size.x = (max_x - b.origin.x).max(0.0);
        }
        // 下裁剪
        let max_y = limits.origin.y + limits.size.y;
        if b.origin.y + b.size.y > max_y {
            b.size.y = (max_y - b.origin.y).max(0.0);
        }
        b
    }
}
