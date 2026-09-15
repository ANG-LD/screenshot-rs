//! 浮动工具栏组件
//!
//! MVP 阶段：定义工具栏的元数据（按钮位置、顺序）和回调接口。
//! 实际的 GPUI Button 渲染依赖 gpui-component crate 接入，留到后续迭代完善。

// 引入 RGBA 颜色类型，用于表示工具栏中当前选中的颜色状态
use crate::overlay::drawing::{FontWeight, RGBA};

/// 字号档位（v0.2 工具栏下拉用）
///
/// 单位：物理像素（与 font_size 字段一致，不随 scale_factor 倍乘）
pub const FONT_SIZES: &[f32] = &[14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 32.0, 40.0, 48.0, 56.0, 64.0];

/// 画笔/边框粗细档位（px）
///
/// 用于矩形、箭头、画笔、马赛克的边线粗细选择。
/// 最小 1px（0.5 超细档因锯齿感已去掉），向上为整数档。
pub const LINE_WIDTHS: &[f32] = &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];


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
    /// 英译中翻译工具（框选后 OCR → 翻译，结果落左图右文窗口）
    Translate,
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
    /// 滚动截屏：自动滚动所选视口并拼接成长图
    Scroll,
    /// 手动滚动截屏：由用户自己滚动，应用检测并拼接（对自动注入失效的应用兜底）
    ScrollManual,
    /// 固定截图到桌面（Pin）
    Pin,
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
        ToolButton::Translate,
        ToolButton::Mosaic,
        ToolButton::ColorPicker,
        ToolButton::Undo,
        ToolButton::Redo,
        ToolButton::Bold,
        ToolButton::Scroll,
        ToolButton::ScrollManual,
        ToolButton::Finish,
        ToolButton::Pin,
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
            ToolButton::Translate => "译",
            ToolButton::Mosaic => "马赛克",
            ToolButton::ColorPicker => "颜色",
            ToolButton::Undo => "撤销",
            ToolButton::Redo => "重做",
            ToolButton::Bold => "B",
            ToolButton::Scroll => "滚动截屏",
            ToolButton::ScrollManual => "手动滚动",
            ToolButton::Pin => "固定",
            ToolButton::Finish => "完成",
            ToolButton::Cancel => "取消",
        }
    }

    /// 悬浮提示文本
    ///
    /// 纯图标按钮（见 [`ToolButton::shows_label`]）靠 tooltip 说明用途。
    /// **只写功能名，2–4 个字**：不写括号补充说明、不写快捷键——长提示在
    /// 按钮下方铺开一大条，反而看不清指哪个按钮（用户反馈）。快捷键统一放在
    /// README / 帮助里，不挤进 tooltip。
    pub fn tooltip_text(&self) -> &'static str {
        match self {
            ToolButton::Rectangle => "矩形",
            ToolButton::Ellipse => "椭圆",
            ToolButton::Arrow => "箭头",
            ToolButton::Freehand => "画笔",
            ToolButton::Text => "文字",
            ToolButton::Ocr => "文字识别",
            ToolButton::Translate => "翻译",
            ToolButton::Mosaic => "马赛克",
            ToolButton::ColorPicker => "取色",
            ToolButton::Undo => "撤销",
            ToolButton::Redo => "重做",
            ToolButton::Bold => "加粗",
            ToolButton::Scroll => "滚动截屏",
            ToolButton::ScrollManual => "手动滚动",
            ToolButton::Pin => "固定",
            ToolButton::Finish => "完成",
            ToolButton::Cancel => "取消",
        }
    }

    /// 工具栏上是否显示中文短标签
    ///
    /// 混合风格：**绘图工具**图标语义弱一些，保留 2 字标签帮助识别；
    /// **操作类按钮**（OCR / 滚动 / 撤销重做 / 固定 / 完成 / 取消）用纯图标
    /// + tooltip，工具栏更紧凑、按钮节奏一致。
    pub fn shows_label(&self) -> bool {
        matches!(
            self,
            ToolButton::Rectangle
                | ToolButton::Ellipse
                | ToolButton::Arrow
                | ToolButton::Freehand
                | ToolButton::Text
                | ToolButton::Mosaic
        )
    }

    /// 是否是带二级弹层（Popover）的按钮
    ///
    /// 绘图工具：点一次选中、再点一次浮出「粗细 + 颜色」；
    /// 文字工具：浮出「字号 + 加粗 + 颜色 + 背景」。
    pub fn has_popover(&self) -> bool {
        matches!(
            self,
            ToolButton::Rectangle
                | ToolButton::Ellipse
                | ToolButton::Arrow
                | ToolButton::Freehand
                | ToolButton::Text
                | ToolButton::Mosaic
        )
    }

    /// 工具栏分组（组内相邻渲染，组间画竖直分隔线）
    ///
    /// 顺序即从左到右的渲染顺序；分组表达「工具 → 识别/滚动 → 编辑 → 收尾」
    /// 的语义层次。渲染与宽度估算都读这一份定义，避免两处漂移。
    pub const GROUPS: &'static [&'static [ToolButton]] = &[
        // 1) 绘图工具（带短标签 + 二级弹层）
        &[
            ToolButton::Rectangle,
            ToolButton::Ellipse,
            ToolButton::Arrow,
            ToolButton::Freehand,
            ToolButton::Text,
            ToolButton::Mosaic,
        ],
        // 2) 识别与滚动截屏（纯图标）
        &[
            ToolButton::Ocr,
            ToolButton::Translate,
            ToolButton::Scroll,
            ToolButton::ScrollManual,
        ],
        // 3) 编辑（纯图标）
        &[ToolButton::Undo, ToolButton::Redo],
        // 4) 收尾动作（纯图标：固定 / 取消 / 完成）
        &[ToolButton::Pin, ToolButton::Cancel, ToolButton::Finish],
    ];
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
    /// 文字选框/高亮背景色（alpha=0 表示无背景）。Text 工具二级面板可改。
    pub current_bg: RGBA,
    /// 当前展开的二级面板（None = 收起）
    pub popup: Option<ToolbarPopup>,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: None,
            current_color: RGBA::RED,
            line_width: 3.0,
            current_size: 24.0,
            current_weight: FontWeight::Normal,
            current_bg: RGBA::TRANSPARENT,
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
        assert_eq!(s.line_width, 3.0);
        assert_eq!(s.current_bg, RGBA::TRANSPARENT);
    }

    /// 翻译按钮：纯图标 + 有 tooltip，且必须同时出现在 ORDER 与 GROUPS 里
    /// （漏掉任一处工具栏就不渲染，或渲染顺序与 ORDER 不一致）。
    #[test]
    fn translate_button_is_icon_only_and_registered() {
        let b = ToolButton::Translate;
        assert!(!b.shows_label(), "翻译按钮应为纯图标（与 OCR 一致）");
        assert!(!b.tooltip_text().is_empty(), "翻译按钮必须有 tooltip");
        assert!(b.tooltip_text().contains("翻译"), "tooltip 应说明是翻译：{}", b.tooltip_text());
        assert!(ToolButton::ORDER.contains(&b), "翻译按钮必须在 ORDER 里");
        assert!(
            ToolButton::GROUPS.iter().any(|g| g.contains(&b)),
            "翻译按钮必须在 GROUPS 里"
        );
        // 位置：紧跟 OCR（识别类工具放一起）
        let order: Vec<ToolButton> = ToolButton::ORDER.to_vec();
        let ocr = order.iter().position(|x| *x == ToolButton::Ocr).unwrap();
        assert_eq!(order[ocr + 1], ToolButton::Translate);
    }

    #[test]
    fn line_widths_no_longer_include_half_pixel() {
        assert!(!LINE_WIDTHS.contains(&0.5));
        assert_eq!(LINE_WIDTHS[0], 1.0);
    }

    #[test]
    fn font_sizes_constant_includes_recommended_values() {
        assert!(FONT_SIZES.contains(&16.0));
        assert!(FONT_SIZES.contains(&48.0));
        // 含新增的小/大档位；用范围验证而非精确长度，便于后续调整档位
        assert!(FONT_SIZES.len() >= 8);
        assert!(FONT_SIZES.contains(&14.0));
        assert!(FONT_SIZES.contains(&64.0));
    }

    #[test]
    fn groups_cover_renderable_buttons_without_duplicates() {
        let mut seen: Vec<ToolButton> = Vec::new();
        for group in ToolButton::GROUPS {
            assert!(!group.is_empty(), "分组不应为空");
            for btn in group.iter() {
                assert!(!seen.contains(btn), "{btn:?} 在 GROUPS 中重复出现");
                seen.push(*btn);
            }
        }
        // GROUPS 覆盖所有实际渲染的按钮（ColorPicker/Bold 由弹层内呈现，
        // 不进工具栏）；与历史顺序表 ORDER 的渲染集合保持一致。
        for btn in ToolButton::ORDER {
            if matches!(btn, ToolButton::ColorPicker | ToolButton::Bold) {
                continue;
            }
            assert!(seen.contains(btn), "{btn:?} 未出现在 GROUPS 中");
        }
        assert_eq!(seen.len(), ToolButton::ORDER.len() - 2);
    }

    #[test]
    fn drawing_tools_keep_labels_and_actions_are_icon_only() {
        for btn in [
            ToolButton::Rectangle,
            ToolButton::Ellipse,
            ToolButton::Arrow,
            ToolButton::Freehand,
            ToolButton::Text,
            ToolButton::Mosaic,
        ] {
            assert!(btn.shows_label(), "{btn:?} 应保留短标签");
            assert!(btn.has_popover(), "{btn:?} 应有二级弹层");
        }
        for btn in [
            ToolButton::Ocr,
            ToolButton::Translate,
            ToolButton::Scroll,
            ToolButton::ScrollManual,
            ToolButton::Undo,
            ToolButton::Redo,
            ToolButton::Pin,
            ToolButton::Finish,
            ToolButton::Cancel,
        ] {
            assert!(!btn.shows_label(), "{btn:?} 应为纯图标按钮");
            assert!(!btn.has_popover(), "{btn:?} 不应有二级弹层");
        }
    }

    #[test]
    fn every_button_has_a_tooltip() {
        for btn in ToolButton::ORDER {
            assert!(!btn.tooltip_text().is_empty(), "{btn:?} 缺少 tooltip");
        }
    }

    #[test]
    fn tooltips_stay_short_and_plain() {
        // 提示统一「只写功能名」：不超过 5 个字，且不含括号补充、不含快捷键。
        // 长提示会在按钮下方铺开一大条，用户分不清它指的是哪个按钮。
        for btn in ToolButton::ORDER {
            let tip = btn.tooltip_text();
            assert!(
                tip.chars().count() <= 5,
                "{btn:?} 提示过长（{} 字）: {tip}",
                tip.chars().count()
            );
            for bad in ['（', '(', '：', ':', '，', ','] {
                assert!(
                    !tip.contains(bad),
                    "{btn:?} 提示含说明性标点 {bad:?}: {tip}"
                );
            }
        }
    }
}