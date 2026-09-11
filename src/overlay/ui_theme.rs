//! 统一 UI 设计令牌（design tokens）+ 复用样式助手。
//!
//! 应用里所有浮动 UI（截图工具栏、二级弹层、Pin 标题栏、滚动进度窗、
//! 更新提示窗、OCR 窗口、tooltip）共用这一份**配色 / 圆角 / 间距 / 字号 /
//! 阴影**，避免各窗口各自硬编码 `rgba(0x...)` 造成风格漂移。
//!
//! 设计基调：**深色玻璃拟态**（dark glass）
//! - 面板：近黑蓝底（`PANEL_BG`）+ 1px 冷白描边 + 柔和外阴影，浮在截图之上
//!   不抢内容、边界清晰；
//! - 按钮：默认透明浅底 → hover 提亮 → 按下更亮（无边框），激活工具用蓝色
//!   强调色实心，主操作（完成）用绿色实心、次操作（取消）用红色文字/危险 hover；
//! - 图标与文字同色（`TEXT` / `MUTED` / `DISABLED`），激活态统一白色。
//!
//! 所有令牌都是 `u32` 的 `0xRRGGBBAA` 字面量，配合 [`c`] 里的 `rgb()` / `rgb_a()`
//! 助手转成 GPUI 颜色，方便阅读与统一调整。

use gpui::{BoxShadow, Hsla, px, rgba};

/// 颜色令牌：`0xRRGGBBAA`
///
/// 命名约定：`_BG` 背景、`_FG` 前景（文字/图标）、`_HOVER`/`_ACTIVE` 交互态、
/// `_BORDER` 描边、`_SOFT` 低饱和强调底。
pub mod tokens {
    // ── 面板 / 弹层 ────────────────────────────────────────────────────────
    /// 工具栏面板底色（深色玻璃，约 95% 不透明）
    pub const PANEL_BG: u32 = 0x1B1E27F5;
    /// 面板描边（冷白 10%）
    pub const PANEL_BORDER: u32 = 0xFFFFFF1F;
    /// 面板顶部内高光：模拟玻璃受光面（1px 内阴影）
    pub const PANEL_INNER_HIGHLIGHT: u32 = 0xFFFFFF14;
    /// 二级弹层（popover）底色：比工具栏略亮一档，形成层次
    pub const POPOVER_BG: u32 = 0x232733FA;
    /// 弹层描边
    pub const POPOVER_BORDER: u32 = 0xFFFFFF24;
    /// 分组分隔线
    pub const DIVIDER: u32 = 0xFFFFFF1C;
    /// 弹层内分区小标题（如「粗细」「颜色」）
    pub const SECTION_LABEL: u32 = 0x8C93A3FF;

    // ── 文本 / 图标 ────────────────────────────────────────────────────────
    /// 主要文字与图标
    pub const TEXT: u32 = 0xF3F5FAFF;
    /// 次要文字（标签、提示）
    pub const TEXT_MUTED: u32 = 0xB6BDCBFF;
    /// 禁用态文字/图标
    pub const TEXT_DISABLED: u32 = 0xFFFFFF45;
    /// 强调色上的文字/图标（蓝/绿实心底）
    pub const TEXT_ON_ACCENT: u32 = 0xFFFFFFFF;

    // ── 中性按钮 ───────────────────────────────────────────────────────────
    /// 中性按钮常态底（几乎透明，靠 hover 提示可点）
    pub const BTN_BG: u32 = 0xFFFFFF0A;
    /// 中性按钮 hover 底
    pub const BTN_BG_HOVER: u32 = 0xFFFFFF21;
    /// 中性按钮按下底
    pub const BTN_BG_ACTIVE: u32 = 0xFFFFFF38;
    /// 禁用按钮底（比常态更暗，明确「不可点」）
    pub const BTN_BG_DISABLED: u32 = 0xFFFFFF06;

    // ── 强调色（激活工具 / 主按钮）──────────────────────────────────────────
    /// 强调蓝
    pub const ACCENT: u32 = 0x3D7DF6FF;
    /// 强调蓝 hover
    pub const ACCENT_HOVER: u32 = 0x528BFF;
    /// 强调蓝按下
    pub const ACCENT_ACTIVE: u32 = 0x2F6BE0;
    /// 低饱和强调底（激活工具旁的次级高亮、选中色板描边底）
    pub const ACCENT_SOFT: u32 = 0x3D7DF62E;
    /// 强调色描边（选中态色板/尺寸档）
    pub const ACCENT_BORDER: u32 = 0x6FA4FFFF;

    // ── 主/次操作 ──────────────────────────────────────────────────────────
    /// 完成按钮绿
    pub const SUCCESS: u32 = 0x2BB673FF;
    /// 完成按钮 hover
    pub const SUCCESS_HOVER: u32 = 0x35C77F;
    /// 完成按钮按下
    pub const SUCCESS_ACTIVE: u32 = 0x239A61;
    /// 危险/取消 hover 底（红）
    pub const DANGER: u32 = 0xE5484DFF;
    /// 危险 hover 底（半透明红，用于取消/关闭按钮）
    pub const DANGER_SOFT: u32 = 0xE5484D2E;
    /// 危险按下底（比 DANGER 更深一档）
    pub const DANGER_ACTIVE: u32 = 0xC93A3F;

    // ── 其它 ───────────────────────────────────────────────────────────────
    /// 进度条轨道底（比按钮常态底略亮，能看出"总长度"）
    pub const PROGRESS_TRACK: u32 = 0xFFFFFF1A;
    /// 色板/尺寸档未选中描边
    pub const SWATCH_BORDER: u32 = 0xFFFFFF33;
    /// 棋盘格（透明）浅格 / 深格
    pub const CHECKER_LIGHT: u32 = 0xE9ECF2FF;
    pub const CHECKER_DARK: u32 = 0xB9BFCCFF;
    /// 深色 tooltip 底（用于自绘 tooltip）
    pub const TOOLTIP_BG: u32 = 0x2A2F3BFA;
    pub const TOOLTIP_BORDER: u32 = 0xFFFFFF26;
}

/// 颜色助手：u32 令牌 → GPUI 颜色
pub mod c {
    use super::*;

    /// `0xRRGGBBAA` → `Hsla`
    #[inline]
    pub fn rgb(token: u32) -> Hsla {
        rgba(token).into()
    }
}

/// 圆角令牌（px）
pub mod r {
    /// 面板/弹层圆角
    pub const PANEL: f32 = 12.0;
    /// 按钮圆角
    pub const BTN: f32 = 8.0;
    /// 小控件（色板、尺寸档、chip）圆角
    pub const CHIP: f32 = 7.0;
}

/// 尺寸 / 间距令牌（px）
pub mod m {
    /// 工具栏按钮高度
    pub const BTN_H: f32 = 30.0;
    /// 纯图标按钮宽度（正方形）
    pub const BTN_ICON_W: f32 = 30.0;
    /// 带标签按钮的左右内边距
    pub const BTN_PAD_X: f32 = 9.0;
    /// 按钮组内按钮间距
    pub const BTN_GAP: f32 = 3.0;
    /// 工具栏面板内边距
    pub const PANEL_PAD: f32 = 6.0;
    /// 分组之间的间距（分隔线两侧）
    pub const GROUP_GAP: f32 = 6.0;
    /// 按钮内图标尺寸
    ///
    /// 15→16：16px 图标在 30px 方按钮里约占 53% 面积，视觉重量更接近系统
    /// 工具栏惯例（15px 显得偏小、按钮"发空"）；16 是 lucide 的整数倍网格，
    /// 描边不会出现半像素发虚。
    pub const ICON: f32 = 16.0;
    /// 工具栏标签字号
    pub const FONT_LABEL: f32 = 12.5;
    /// 弹层分区标签字号
    pub const FONT_SECTION: f32 = 11.0;
    /// 色板边长
    pub const SWATCH: f32 = 22.0;
    /// 尺寸/字号档位 chip 的边长（高）
    pub const CHIP: f32 = 26.0;
}

/// 面板外阴影（浮层立体感）：两层叠加——大而柔的环境影 + 小而锐的接触影
pub fn panel_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.0), px(10.0), rgba(0x00000059).into())
            .blur_radius(px(28.0))
            .spread_radius(px(-6.0)),
        BoxShadow::new(px(0.0), px(2.0), rgba(0x00000047).into()).blur_radius(px(6.0)),
    ]
}

/// 弹层阴影（比工具栏略轻，避免两层阴影糊成一团）
pub fn popover_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.0), px(8.0), rgba(0x0000004D).into()).blur_radius(px(22.0)),
        BoxShadow::new(px(0.0), px(1.0), rgba(0x00000038).into()).blur_radius(px(4.0)),
    ]
}

/// 实心强调按钮（蓝/绿）的接触影：1px 下投影，把按钮从面板上"抬"起来
pub fn button_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.0), px(1.0), rgba(0x00000066).into()).blur_radius(px(3.0))]
}

/// 工具栏距离选区上沿的距离（px）——工具栏定位与视觉间距的唯一来源
pub const TOOLBAR_OFFSET_Y: f32 = 8.0;

/// 尺寸信息 chip（"1920 × 1080"）等浮标使用的圆角矩形背景高度
pub const CHIP_H: f32 = 22.0;

/// 把像素尺寸格式化成 `宽 × 高` 文本（尺寸浮标 / OCR 结果栏共用）
pub fn format_size(w: f32, h: f32) -> String {
    format!("{} × {}", w.round() as i64, h.round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_readable_and_convertible() {
        // 令牌转 Hsla 不 panic，且文本色接近白色
        let text = c::rgb(tokens::TEXT);
        assert!(text.l > 0.9, "TEXT 应为亮色，实际 l={}", text.l);
        let panel = c::rgb(tokens::PANEL_BG);
        assert!(panel.l < 0.2, "PANEL_BG 应为深色，实际 l={}", panel.l);
    }

    #[test]
    fn shadows_have_expected_layer_counts() {
        assert_eq!(panel_shadow().len(), 2);
        assert_eq!(popover_shadow().len(), 2);
    }

    #[test]
    fn format_size_rounds_to_integers() {
        assert_eq!(format_size(1920.4, 1080.6), "1920 × 1081");
    }
}
