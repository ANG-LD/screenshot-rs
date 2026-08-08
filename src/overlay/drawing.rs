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
    /// 椭圆（空心描边，外接矩形与 Rectangle 一致）
    Ellipse {
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
        /// 旋转角度（度），0 = 水平，逆时针
        rotation: f32,
    },
    /// 马赛克画笔：沿鼠标轨迹的多个方块，每个方块内像素被 block_size 像素化
    Mosaic {
        /// 所有画笔方块（每个是 brush_size×brush_size 的矩形）
        regions: Vec<Rect>,
        /// 像素化块大小
        block_size: u32,
        /// 预览叠加颜色
        color: RGBA,
    },
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
            Self::Normal => include_bytes!("../../assets/fonts/NotoSansSC-Regular-subset.otf"),
            Self::Bold   => include_bytes!("../../assets/fonts/NotoSansSC-Bold-subset.otf"),
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
    /// 截断已撤销的尾部命令后追加，保证 push 后 history_index == commands.len()。
    pub fn push(&mut self, cmd: DrawCommand) {
        self.commands.truncate(self.history_index);
        self.commands.push(cmd);
        self.history_index = self.commands.len();
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
    /// LIFO 语义：`history_index` 表示从头开始的可见数量。
    /// undo 使最后一条命令不可见，redo 恢复。
    pub fn is_visible(&self, i: usize) -> bool {
        i < self.history_index
    }

    /// 当前可见的命令迭代器
    pub fn visible_commands(&self) -> impl Iterator<Item = &DrawCommand> {
        self.commands.iter().take(self.history_index)
    }

    /// 遍历可见命令及其实际索引（用于命中测试定位命令）
    pub fn visible_commands_with_indices(&self) -> impl Iterator<Item = (usize, &DrawCommand)> {
        self.commands.iter().enumerate().take(self.history_index)
    }

    /// 按实际索引获取可见命令的可变引用（用于原地修改）
    pub fn get_visible_mut(&mut self, index: usize) -> Option<&mut DrawCommand> {
        if self.is_visible(index) {
            self.commands.get_mut(index)
        } else {
            None
        }
    }

    /// 移除一条可见命令并更新 history_index（用于 Text 命令的重新编辑）
    pub fn remove_visible(&mut self, index: usize) -> Option<DrawCommand> {
        if index >= self.history_index {
            return None;
        }
        let cmd = self.commands.remove(index);
        self.history_index -= 1;
        Some(cmd)
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
