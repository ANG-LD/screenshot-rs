//! HSV 调色板预设
//!
//! 给工具栏颜色按钮循环切换 current_color 时使用。
//! v0.1 简化：不接 gpui-component 的 ColorPicker Popover，
//! 而是生成 12 个固定 hue 的高饱和度色让用户点一次循环一个。

use crate::overlay::drawing::RGBA;
use crate::utils::color::hsv_to_rgb;

/// 12 色预设（彩虹 12 等分，饱和度 0.85，亮度 0.9）
///
/// 顺序按 hue 0/30/60/.../330 排列：红、橙、黄、黄绿、绿、青、青蓝、蓝、
/// 蓝紫、紫、品红、玫红
pub fn default_palette() -> Vec<RGBA> {
    hsv_swatch(12, 0.85, 0.9)
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
    fn default_palette_returns_12_colors() {
        let p = default_palette();
        assert_eq!(p.len(), 12);
        // 第 0 个应该是红色（hue=0, sat=0.85, val=0.9）
        let r = p[0].r;
        let g = p[0].g;
        let b = p[0].b;
        assert!(r > 200, "red should be bright, got r={r}");
        assert!(g < 100, "red should have low green, got g={g}");
        assert!(b < 100, "red should have low blue, got b={b}");
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
