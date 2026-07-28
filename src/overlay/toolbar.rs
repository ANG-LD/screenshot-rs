//! 浮动工具栏组件
//!
//! MVP 阶段：定义工具栏的元数据（按钮位置、顺序）和回调接口。
//! 实际的 GPUI Button 渲染依赖 gpui-component crate 接入，留到后续迭代完善。

// 引入 RGBA 颜色类型，用于表示工具栏中当前选中的颜色状态
use crate::overlay::drawing::{FontWeight, RGBA};

/// 字号档位（v0.2 工具栏下拉用）
///
/// 单位：物理像素（与 font_size 字段一致，不随 scale_factor 倍乘）
pub const FONT_SIZES: &[f32] = &[16.0, 20.0, 24.0, 32.0, 48.0];

/// 画笔/边框粗细档位（px）
///
/// 用于矩形、箭头、画笔、马赛克的边线粗细选择。
pub const LINE_WIDTHS: &[f32] = &[2.0, 4.0, 6.0, 8.0];


/// 工具栏按钮类型
///
/// 枚举所有可出现在浮动工具栏上的工具/动作按钮。
/// 每个变体对应工具栏上的一个按钮，点击后通过回调通知上层处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolButton {
    /// 矩形选区/标注工具
    Rectangle,
    /// 椭圆标注工具
    Ellipse,
    /// 箭头标注工具
    Arrow,
    /// 自由画笔工具
    Freehand,
    /// 文本标注工具
    Text,
    /// OCR 文字识别工具
    Ocr,
    /// 马赛克/打码工具
    Mosaic,
    /// 取色器工具
    ColorPicker,
    /// 撤销上一步操作
    Undo,
    /// 重做被撤销的操作
    Redo,
    /// 切换文字粗体（v0.2 新增）
    Bold,
    /// 确认并保存截图标注
    Finish,
    /// 取消当前编辑会话
    Cancel,
}

impl ToolButton {
    /// 工具栏显示顺序
    ///
    /// 按此数组的顺序渲染工具栏按钮，从左到右依次出现。
    /// 使用 `&'static [ToolButton]` 保证顺序表在程序生命周期内有效，
    /// 避免每次访问时分配内存。
    pub const ORDER: &'static [ToolButton] = &[
        ToolButton::Rectangle,
        ToolButton::Ellipse,
        ToolButton::Arrow,
        ToolButton::Freehand,
        ToolButton::Text,
        ToolButton::Ocr,
        ToolButton::Mosaic,
        ToolButton::ColorPicker,
        ToolButton::Undo,
        ToolButton::Redo,
        ToolButton::Bold,
        ToolButton::Finish,
        ToolButton::Cancel,
    ];

    /// 按钮显示文本（中文）
    ///
    /// 返回该按钮在 UI 上显示的中文标签。
    /// MVP 阶段先使用静态字符串，后续可改为 i18n 资源加载。
    pub fn label(&self) -> &'static str {
        match self {
            ToolButton::Rectangle => "矩形",
            ToolButton::Ellipse => "椭圆",
            ToolButton::Arrow => "箭头",
            ToolButton::Freehand => "画图",
            ToolButton::Text => "文字",
            ToolButton::Ocr => "OCR",
            ToolButton::Mosaic => "马赛克",
            ToolButton::ColorPicker => "颜色",
            ToolButton::Undo => "撤销",
            ToolButton::Redo => "重做",
            ToolButton::Bold => "B",
            ToolButton::Finish => "完成",
            ToolButton::Cancel => "取消",
        }
    }
}

/// 二级面板内容类型（点 active 绘图工具按钮二次时浮出的 popover 内容）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarPopup {
    /// 画图类 popover：粗细档位 + 颜色
    Stroke,
    /// 文字 popover：字号档位 + Bold 切换 + 颜色
    Text,
}

/// 工具栏状态
///
/// 保存工具栏当前的交互状态，包括选中的工具、当前颜色、字号、二级面板等。
/// 此结构由上层（如 OverlayWindow）持有，工具栏组件通过引用读取与更新。
pub struct ToolbarState {
    /// 当前选中的工具
    ///
    /// `None` 表示当前没有选中任何工具（例如刚进入截图模式、或者刚完成操作）。
    /// 渲染时可通过此字段高亮对应按钮。
    pub active_tool: Option<ToolButton>,
    /// 当前颜色
    ///
    /// 工具栏显示的当前画笔/边框颜色，绘制新标注时默认使用此颜色。
    pub current_color: RGBA,
    /// 当前线宽
    ///
    /// 绘制矩形边框、箭头、画图笔触的线宽（单位：像素）。
    pub line_width: f32,
    /// 当前字号（v0.2 新增）
    ///
    /// 绘制文本标注时的字号（单位：物理像素，不随 scale_factor 倍乘）。
    pub current_size: f32,
    /// 当前字重（v0.2 新增）
    ///
    /// 绘制文本标注时的粗细，Normal/Bold 切换由 Bold 按钮触发。
    pub current_weight: FontWeight,
    /// 当前展开的二级面板（None = 收起）
    pub popup: Option<ToolbarPopup>,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: None,
            current_color: RGBA::RED,
            line_width: 4.0,
            current_size: 24.0,
            current_weight: FontWeight::Normal,
            popup: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_default_state_has_expected_size_and_weight() {
        let s = ToolbarState::default();
        assert_eq!(s.current_size, 24.0);
        assert_eq!(s.current_weight, FontWeight::Normal);
        assert_eq!(s.line_width, 4.0);
    }

    #[test]
    fn font_sizes_constant_includes_recommended_values() {
        assert!(FONT_SIZES.contains(&16.0));
        assert!(FONT_SIZES.contains(&48.0));
        assert_eq!(FONT_SIZES.len(), 5);
    }
}