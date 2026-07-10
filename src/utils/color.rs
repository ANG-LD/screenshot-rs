//! HSV 与 RGB 颜色空间转换。
//!
//! HSV（Hue/Saturation/Value）便于用户通过调色板选择颜色，绘图时转为 RGB 存储。
//! Hue: 0-360°，Saturation: 0.0-1.0，Value: 0.0-1.0
//! RGB: 0-255 整数

/// HSV → RGB（0-255 整数元组）
pub fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> (u8, u8, u8) {
    if saturation <= 0.0 {
        let v = (value * 255.0).round() as u8;
        return (v, v, v);
    }

    let h = ((hue % 360.0) + 360.0) % 360.0; // 归一化到 [0, 360)
    let s = saturation.clamp(0.0, 1.0);
    let v = value.clamp(0.0, 1.0);

    let h_sector = h / 60.0;
    let sector_index = h_sector.floor() as i32;
    let fractional = h_sector - sector_index as f32;

    let p = v * (1.0 - s);
    let q = v * (1.0 - s * fractional);
    let t = v * (1.0 - s * (1.0 - fractional));

    let (r, g, b) = match sector_index {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        5 => (q, p, v),
        _ => (v, p, q),
    };

    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

/// RGB → HSV（Hue: 0-360°, Saturation/Value: 0.0-1.0）
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let v = max;
    let s = if max <= 0.0 { 0.0 } else { delta / max };

    let h = if delta == 0.0 {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / delta) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };

    (h, s, v)
}