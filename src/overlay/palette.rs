//! HSV 调色板预设
//!
//! 给工具栏颜色按钮循环切换 current_color 时使用。
//! v0.1 简化：不接 gpui-component 的 ColorPicker Popover，
//! 而是生成 12 个固定 hue 的高饱和度色让用户点一次循环一个。

use crate::overlay::drawing::RGBA;
use crate::utils::color::hsv_to_rgb;

/// 纯白打头 + 基础色系 + 彩虹 12 色 + 暗色变体
///
/// 顺序：白、黑、灰阶（浅/中/深）、12 色彩虹（饱和 0.85、亮度 0.9）、
/// 暗色变体（暗红、棕、暗绿、暗青、暗蓝、暗紫）。
/// 白色放首位（最常用/最易选）；补上黑色、灰色、暗色等重要色系，
/// 否则文字/背景无法选黑字、灰底或深色调。
pub fn default_palette() -> Vec<RGBA> {
    let mut p = Vec::with_capacity(23);
    // 最重要基础色：白、黑
    p.push(RGBA::WHITE);
    p.push(RGBA::new(0x00, 0x00, 0x00, 0xFF)); // 黑
    // 灰色阶（三档，浅→中→深）
    p.push(RGBA::new(0xDC, 0xDC, 0xDC, 0xFF)); // 浅灰
    p.push(RGBA::new(0x9E, 0x9E, 0x9E, 0xFF)); // 中灰
    p.push(RGBA::new(0x5A, 0x5A, 0x5A, 0xFF)); // 深灰
    // 12 色高饱和彩虹
    p.extend(hsv_swatch(12, 0.85, 0.9));
    // 暗色变体：暗红、棕、暗绿、暗青、暗蓝、暗紫
    const DARKS: &[(u8, u8, u8)] = &[
        (0x8B, 0x00, 0x00), // 暗红
        (0x8B, 0x45, 0x13), // 棕/暗橙
        (0x00, 0x64, 0x00), // 暗绿
        (0x00, 0x6E, 0x6E), // 暗青
        (0x00, 0x00, 0x8B), // 暗蓝
        (0x4B, 0x00, 0x82), // 暗紫
    ];
    for &(r, g, b) in DARKS {
        p.push(RGBA::new(r, g, b, 0xFF));
    }
    p
}

/// HSV 环均匀采样：hue 0..360 按 hue_steps 切分，固定 sat/val
pub fn hsv_swatch(hue_steps: u32, sat: f32, val: f32) -> Vec<RGBA> {
    (0..hue_steps)
        .map(|i| {
            let hue = i as f32 * 360.0 / hue_steps as f32;
            let (r, g, b) = hsv_to_rgb(hue, sat, val);
            RGBA::new(r, g, b, 0xFF)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_palette_starts_with_black_gray_and_rainbow() {
        let p = default_palette();
        // 第一个是纯白
        assert_eq!(p[0], RGBA::WHITE);
        // 第二个是纯黑
        assert_eq!(p[1], RGBA::new(0x00, 0x00, 0x00, 0xFF));
        // 第三个应有灰阶成份（浅灰：R=G=B 且接近但不纯白）
        assert_eq!(p[2].r, p[2].g);
        assert_eq!(p[2].g, p[2].b);
        assert!(p[2].r < 0xFF, "gray should not be white");
        // 彩虹红位于白/黑/灰之后（index=5）
        let r = p[5].r;
        let g = p[5].g;
        let b = p[5].b;
        assert!(r > 200, "red should be bright, got r={r}");
        assert!(g < 100, "red should have low green, got g={g}");
        assert!(b < 100, "red should have low blue, got b={b}");
        // 总数 = 1 白 + 1 黑 + 3 灰 + 12 彩虹 + 6 暗色
        assert_eq!(p.len(), 23);
    }

    #[test]
    fn hsv_swatch_covers_full_hue_circle() {
        let p = hsv_swatch(36, 1.0, 1.0);
        assert_eq!(p.len(), 36);
        // hue=120 应该是纯绿
        let (r, g, b) = (p[12].r, p[12].g, p[12].b);
        assert!(g > 200, "hue=120 should be green, got g={g}");
        assert!(r < 50 && b < 50, "green should have low r and b, got r={r} b={b}");
        // hue=240 应该是纯蓝
        let (r, g, b) = (p[24].r, p[24].g, p[24].b);
        assert!(b > 200, "hue=240 should be blue, got b={b}");
        assert!(r < 50 && g < 50, "blue should have low r and g");
    }

    #[test]
    fn all_palette_colors_are_opaque() {
        let p = default_palette();
        for c in &p {
            assert_eq!(c.a, 0xFF, "palette color should be opaque, got a={}", c.a);
        }
    }
}
