//! 浮动工具栏组件
//!
//! MVP 阶段：定义工具栏的元数据（按钮位置、顺序）和回调接口。
//! 实际的 GPUI Button 渲染依赖 gpui-component crate 接入，留到后续迭代完善。

// 引入 RGBA 颜色类型，用于表示工具栏中当前选中的颜色状态
use crate::overlay::drawing::RGBA;

/// 工具栏按钮类型
///
/// 枚举所有可出现在浮动工具栏上的工具/动作按钮。
/// 每个变体对应工具栏上的一个按钮，点击后通过回调通知上层处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolButton {
    /// 矩形选区/标注工具
    Rectangle,
    /// 箭头标注工具
    Arrow,
    /// 自由画笔工具
    Freehand,
    /// 文本标注工具
    Text,
    /// 马赛克/打码工具
    Mosaic,
    /// 取色器工具
    ColorPicker,
    /// 撤销上一步操作
    Undo,
    /// 重做被撤销的操作
    Redo,
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
        ToolButton::Arrow,
        ToolButton::Freehand,
        ToolButton::Text,
        ToolButton::Mosaic,
        ToolButton::ColorPicker,
        ToolButton::Undo,
        ToolButton::Redo,
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
            ToolButton::Arrow => "箭头",
            ToolButton::Freehand => "画图",
            ToolButton::Text => "文字",
            ToolButton::Mosaic => "马赛克",
            ToolButton::ColorPicker => "颜色",
            ToolButton::Undo => "撤销",
            ToolButton::Redo => "重做",
            ToolButton::Finish => "完成",
            ToolButton::Cancel => "取消",
        }
    }
}

/// 工具栏状态
///
/// 保存工具栏当前的交互状态，包括选中的工具、当前颜色、线宽等。
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
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: None,
            current_color: RGBA::RED,
            line_width: 2.0,
        }
    }
}