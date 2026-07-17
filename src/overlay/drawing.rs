//! 绘图数据模型与状态
//!
//! DrawCommand 枚举所有支持的绘图工具。每次用户完成一笔就 push 到 DrawingState。
//! DrawingState 维护 `commands` 列表和 `history_index`，实现撤销/重做。

use serde::{Deserialize, Serialize};

/// 屏幕坐标点（与 utils::bounds::Point 区分，避免 GPUI 类型冲突）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 矩形区域（origin, size 都用 Point 表示）
pub type Rect = (Point, Point);

/// RGBA 颜色（u8 分量）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RGBA {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RGBA {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const RED: Self = Self::new(255, 0, 0, 255);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
}

/// 单个绘图元素
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DrawCommand {
    /// 矩形（空心描边）
    Rectangle {
        rect: Rect,
        color: RGBA,
        line_width: f32,
    },
    /// 直线箭头（带箭头头部）
    Arrow {
        from: Point,
        to: Point,
        color: RGBA,
        line_width: f32,
    },
    /// 自由画笔
    Freehand {
        points: Vec<Point>,
        color: RGBA,
        line_width: f32,
    },
    /// 文字
    Text {
        anchor: Point,
        content: String,
        font_size: f32,
        color: RGBA,
        max_width: Option<f32>,
        weight: FontWeight,
    },
    /// 马赛克：把选区局部图像缩放到 block_size×block_size 再放大回原尺寸
    Mosaic { rect: Rect, block_size: u32 },
}

/// 文字粗细 (v0.2 新增)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FontWeight {
    Normal,
    Bold,
}

impl FontWeight {
    /// 选对应字体的 OTF 字节
    pub fn font_bytes(self) -> &'static [u8] {
        match self {
            Self::Normal => include_bytes!("../../assets/fonts/NotoSansSC-Regular.otf"),
            Self::Bold   => include_bytes!("../../assets/fonts/NotoSansSC-Bold.otf"),
        }
    }
}

/// 绘图状态：维护命令列表 + 历史索引，支持撤销/重做
pub struct DrawingState {
    /// 所有命令（删除的也保留，便于重做）
    pub commands: Vec<DrawCommand>,
    /// 当前可见的命令数量（0..=commands.len()）
    pub history_index: usize,
}

impl DrawingState {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            history_index: 0,
        }
    }

    /// 添加新命令
    ///
    /// 总是追加到 commands 末尾并将 history_index 增 1。
    /// 在 redo 区非空时 push，旧 undo 命令仍会保留下来作为审计日志。
    pub fn push(&mut self, cmd: DrawCommand) {
        self.commands.push(cmd);
        self.history_index += 1;
    }

    /// 撤销：将 history_index 减 1（不会真正删除命令）
    pub fn undo(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
        }
    }

    /// 重做：将 history_index 增 1
    pub fn redo(&mut self) {
        if self.history_index < self.commands.len() {
            self.history_index += 1;
        }
    }

    /// 判断索引 i 处的命令是否当前可见
    ///
    /// "可见"定义为：处于 commands 末尾、且在 `history_index` 范围内的项。
    /// 即索引 `i` 必须满足 `i >= commands.len() - history_index`。
    /// 这样可以同时支持：
    /// - 简单的 push/undo/redo（只有一个项目时）
    /// - undo 后再 push（新命令可见，旧命令保留在 commands 但不可见）
    pub fn is_visible(&self, i: usize) -> bool {
        let inactive_prefix = self.commands.len().saturating_sub(self.history_index);
        i >= inactive_prefix
    }

    /// 当前可见的命令迭代器
    pub fn visible_commands(&self) -> impl Iterator<Item = &DrawCommand> {
        let inactive_prefix = self.commands.len().saturating_sub(self.history_index);
        self.commands.iter().skip(inactive_prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_weight_font_bytes_returns_nonempty_for_both_variants() {
        let regular = FontWeight::Normal.font_bytes();
        let bold = FontWeight::Bold.font_bytes();
        assert!(!regular.is_empty(), "Regular OTF 不能为空");
        assert!(!bold.is_empty(), "Bold OTF 不能为空");
        assert_ne!(regular.as_ptr(), bold.as_ptr(), "Regular/Bold 必须指向不同字节");
    }
}
