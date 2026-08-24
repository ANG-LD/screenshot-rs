//! CPU 栅格化 DrawCommand 到 CapturedFrame 像素。
//!
//! 入口：`apply_commands(frame, region_origin_x, region_origin_y, commands)`
//!
//! 选区在 GPUI 窗口里用的是**屏幕坐标**；栅格化目标是一个被裁剪到选区大小
//! 的 CapturedFrame（其 (0, 0) = 选区左上角）。所以每个命令的坐标都要先
//! 减去 `region_origin`，再写到 frame 像素。
//!
//! 实现要点（v0.1 简化版）：
//! - Rectangle / Arrow / Freehand：用轴对齐 bounding box 实现"粗线"（非真正
//!   抗锯齿，但对 MVP 视觉可接受）
//! - Mosaic：把 frame 对应区域 downscale 到 (w/block_size, h/block_size) 再
//!   nearest-neighbor upscale 回原尺寸
//! - Text（v0.2）：cosmic-text Buffer + SwashCache 拿到每个 glyph 的 alpha
//!   mask，按 SourceOver 合成到 frame；越界裁剪不报错

use crate::capture::CapturedFrame;
use crate::error::{AppError, AppResult};
use crate::overlay::drawing::{DrawCommand, FontWeight, Point as DrawPoint, RGBA};
use cosmic_text::{Attrs, Buffer, Family, Metrics, Shaping, Weight};
use crate::overlay::font::{with_font_system, with_swash_cache, TEXT_FONT_FAMILY};
use image::imageops::FilterType;

/// 把 Text 命令栅格化到 frame（v0.2 真实现）
///
/// - `anchor` 是 **frame 局部坐标**（已 -region_origin，由调用方 translate）
/// - `font_size` 是 **物理像素**（不随 scale_factor 倍乘，与 overlay 预览一致）
/// - 越界部分（frame 外）裁掉，不报错
///
/// cosmic-text 流程：
/// 1. `Buffer::new` + `set_text` + `set_size(Some(max_width))` 控制折行
/// 2. `shape_until_scroll` → `layout_runs`
/// 3. 每 glyph `LayoutGlyph::physical` 拿 cache_key → `SwashCache::get_image` 拿 alpha mask
/// 4. `blend_mask_to_frame` 写入像素 (SourceOver + mask alpha)
pub fn rasterize_text(
    frame: &mut CapturedFrame,
    anchor: (f32, f32),
    pivot: (f32, f32),
    content: &str,
    font_size: f32,
    color: RGBA,
    max_width: Option<f32>,
    weight: FontWeight,
    rotation: f32,
) -> AppResult<()> {
    if content.is_empty() || font_size <= 0.0 {
        return Ok(());
    }
    let (anchor_x, anchor_y) = anchor;
    let weight_attr = if weight == FontWeight::Normal {
        Weight::NORMAL
    } else {
        Weight::BOLD
    };
    // cosmic-text: Some(0.0) 会 panic，0 宽度当 None 走（不折行）
    let max_w = max_width.filter(|&w| w > 0.0);

    // 阶段 1：layout。只借用 font_system，产出每个 glyph 的 (gx, gy, cache_key)。
    // 用物理坐标（以 anchor 为原点）；line_y 由 cosmic-text 通过 (0, run.line_y) 传给 physical。
    // 行距必须与编辑态 Input 一致（1.5×字号，实测 range_to_bounds 的 lh = fs×1.5）：
    // 若用 1.4×字号，多行文字最终成图的行距比预览小 0.1×字号×行数，行数越多
    // 文字整体越往上收、与预览偏差越大（单行无差异）。
    let physical_glyphs: Vec<(f32, f32, cosmic_text::CacheKey)> = with_font_system(|font_system| {
        let metrics = Metrics::new(font_size, font_size * 1.5);
        let mut buffer = Buffer::new(font_system, metrics);
        let attrs = Attrs::new()
            .family(Family::Name(TEXT_FONT_FAMILY))
            .weight(weight_attr);
        buffer.set_text(content, &attrs, Shaping::Advanced, None);
        buffer.set_size(max_w, None);
        buffer.shape_until_scroll(font_system, false);

        let mut out = Vec::new();
        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                // glyph.physical：offset 是 line 起点 (0, line_y)；scale=1
                let phys = glyph.physical((0.0, run.line_y), 1.0);
                out.push((phys.x as f32, phys.y as f32, phys.cache_key));
            }
        }
        out
    });

    // 阶段 1.5：旋转中心 = 传入的 pivot（文本框中心），与旋转框保持一致
    let (cx, cy, cos, sin) = if rotation != 0.0 {
        let angle = rotation * std::f32::consts::PI / 180.0;
        let (s, c) = angle.sin_cos();
        (pivot.0, pivot.1, c, s)
    } else {
        (0.0, 0.0, 1.0, 0.0)
    };

    // 阶段 2：rasterize。每个 glyph 单独进入 swash_cache；为避开
    // 同时借用 font_system + swash_cache 两个 RefCell 的问题，
    // swash_cache 闭包内再开一个 with_font_system（不同 thread_local，无重叠）。
    for (gx, gy, cache_key) in physical_glyphs {
        let mask_opt = with_swash_cache(|swash| {
            with_font_system(|fs| swash.get_image(fs, cache_key).as_ref().cloned())
        });
        let Some(mask) = mask_opt else {
            continue;
        };
        let (px, py) = if rotation != 0.0 {
            let rx = anchor_x + gx - cx;
            let ry = anchor_y + gy - cy;
            (cx + rx * cos - ry * sin, cy + rx * sin + ry * cos)
        } else {
            (anchor_x + gx, anchor_y + gy)
        };
        blend_mask_to_frame(frame, &mask, px, py, color);
    }
    Ok(())
}

/// 把一张灰度 alpha mask 写到 frame 的指定位置
///
/// mask 数据排布：单字节每像素 (0..=255 alpha)，width × height
/// SwashImage.placement.{left,top} 是相对锚点 (baseline) 的 bearing 偏移：
///   - `placement.left`：glyph 左缘到 baseline 原点的水平 bearing（正向 = 右）
///   - `placement.top`：glyph 顶到 baseline 的**向上**距离（cosmic-text 渲染时取负）
///
/// 参考 cosmic-text::swash::with_pixels 的写法：x 直接加，y 取负再加
fn blend_mask_to_frame(
    frame: &mut CapturedFrame,
    mask: &cosmic_text::SwashImage,
    target_x: f32,
    target_y: f32,
    color: RGBA,
) {
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    // placement.top 是"距离 baseline 向上多少"，所以减
    let start_x = (target_x + mask.placement.left as f32) as i32;
    let start_y = (target_y - mask.placement.top as f32) as i32;
    let mask_w = mask.placement.width as i32;
    let mask_h = mask.placement.height as i32;
    if mask_w <= 0 || mask_h <= 0 {
        return;
    }
    for sy in 0..mask_h {
        let py = start_y + sy;
        if py < 0 || py >= h_px {
            continue;
        }
        for sx in 0..mask_w {
            let px = start_x + sx;
            if px < 0 || px >= w_px {
                continue;
            }
            let m_idx = (sy * mask_w + sx) as usize;
            if m_idx >= mask.data.len() {
                continue;
            }
            let mask_a = mask.data[m_idx] as u32;
            if mask_a == 0 {
                continue;
            }
            let f_idx = ((py * w_px + px) as usize) * 4;
            if f_idx + 3 >= frame.pixels.len() {
                continue;
            }
            blend_pixel_with_text_mask(&mut frame.pixels[f_idx..f_idx + 4], color, mask_a);
        }
    }
}

/// 文字专用 SourceOver：复合 mask alpha 与 color.a
///   eff_a = (color.a / 255) * (mask / 255)
///   rgb_out = text_rgb * eff_a + dst_rgb * (1 - eff_a)
fn blend_pixel_with_text_mask(dst: &mut [u8], text_color: RGBA, mask_a: u32) {
    let eff_a = (text_color.a as u32 * mask_a) / 255;
    let inv = 255 - eff_a;
    for i in 0..3 {
        let s = [text_color.r, text_color.g, text_color.b][i] as u32;
        let d = dst[i] as u32;
        dst[i] = ((s * eff_a + d * inv) / 255) as u8;
    }
    dst[3] = eff_a.max(dst[3] as u32) as u8;
}

/// 测量文字在物理像素下的（未旋转）字形度量。
///
/// 返回 `(width, height, cx_off, cy_off)`，其中 `cx_off/cy_off`
/// 是字形原点包围盒中心到 anchor (0,0) 的偏移量。
pub fn measure_text_px(
    content: &str,
    font_size: f32,
    max_width: Option<f32>,
    weight: FontWeight,
) -> (f32, f32, f32, f32) {
    if content.is_empty() || font_size <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let weight_attr = if weight == FontWeight::Normal {
        Weight::NORMAL
    } else {
        Weight::BOLD
    };
    let max_w = max_width.filter(|&w| w > 0.0);
    with_font_system(|font_system| {
        let metrics = Metrics::new(font_size, font_size * 1.4);
        let mut buffer = Buffer::new(font_system, metrics);
        let attrs = Attrs::new()
            .family(Family::Name(TEXT_FONT_FAMILY))
            .weight(weight_attr);
        buffer.set_text(content, &attrs, Shaping::Advanced, None);
        buffer.set_size(max_w, None);
        buffer.shape_until_scroll(font_system, false);

        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let phys = glyph.physical((0.0, run.line_y), 1.0);
                min_x = min_x.min(phys.x as f32);
                min_y = min_y.min(phys.y as f32);
                max_x = max_x.max(phys.x as f32);
                max_y = max_y.max(phys.y as f32);
            }
        }
        if min_x > max_x {
            return (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
        }
        // 字形原点包围盒
        let tw = (max_x - min_x).ceil();
        let th = (max_y - min_y).ceil();
        let cx = (min_x + max_x) / 2.0;
        let cy = (min_y + max_y) / 2.0;
        // 加单字形余量：字形光栅可能超出原点，旋转后尤其需要
        (tw + font_size, th + font_size * 0.5, cx, cy)
    })
}

/// 测量文字在物理像素下的**行宽 advance**（所有 layout run 的最大 line_w）。
///
/// 与 `measure_text_px` 的字形包围盒宽度不同，这里返回的是 cosmic-text 排版
/// 的实际行进宽度（每个字形 advance 之和），等于光标能到达的右边界。编辑框
/// 自动扩增宽度必须 >= 此值，否则光标贴近右缘时编辑器会产生负 scroll_offset
/// 把整行文字左移（见 gpui-component input/element.rs layout_cursor）。
pub fn measure_line_advance_px(
    content: &str,
    font_size: f32,
    weight: FontWeight,
) -> f32 {
    if content.is_empty() || font_size <= 0.0 {
        return 0.0;
    }
    let weight_attr = if weight == FontWeight::Normal {
        Weight::NORMAL
    } else {
        Weight::BOLD
    };
    with_font_system(|font_system| {
        let metrics = Metrics::new(font_size, font_size * 1.4);
        let mut buffer = Buffer::new(font_system, metrics);
        let attrs = Attrs::new()
            .family(Family::Name(TEXT_FONT_FAMILY))
            .weight(weight_attr);
        buffer.set_text(content, &attrs, Shaping::Advanced, None);
        buffer.set_size(None, None);
        buffer.shape_until_scroll(font_system, false);

        buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max)
    })
}

/// 把 commands 列表应用到 frame 的指定子区域
///
/// - `frame` 是被裁剪到选区大小的 CapturedFrame
/// - `region_origin_x/y` 是选区左上角在**屏幕坐标**中的位置（用于把命令坐标
///   从屏幕坐标系平移到 frame 局部坐标系）
pub fn apply_commands(
    frame: &mut CapturedFrame,
    region_origin_x: f32,
    region_origin_y: f32,
    commands: &[DrawCommand],
) -> AppResult<()> {
    apply_commands_step(frame, region_origin_x, region_origin_y, commands, 1)
}

/// apply_commands 的采样步长版本：`step`>1 时折线降采样光栅化
/// （预览用，加快拖动绘制；提交成图用 step=1 精确）。
pub fn apply_commands_step(
    frame: &mut CapturedFrame,
    region_origin_x: f32,
    region_origin_y: f32,
    commands: &[DrawCommand],
    step: u32,
) -> AppResult<()> {
    // 第一步：马赛克命令 — 只作用于原始截图像素，
    // 保证矩形/箭头/文字等标注叠加在马赛克之上。
    for cmd in commands {
        if let DrawCommand::Mosaic { regions, block_size, color } = cmd {
            for rect in regions {
                let a = translate(rect.0, region_origin_x, region_origin_y);
                let b = translate(rect.1, region_origin_x, region_origin_y);
                let (x1, y1, x2, y2) = normalize_rect(a, b);
                apply_mosaic(frame, x1, y1, x2, y2, *block_size, *color)?;
            }
        }
    }
    // 第二步：所有标注命令 — 绘制在马赛克之上
    for cmd in commands {
        match cmd {
            DrawCommand::Mosaic { .. } => {} // 已在第一步处理
            DrawCommand::Rectangle { rect, color, line_width } => {
                let a = translate(rect.0, region_origin_x, region_origin_y);
                let b = translate(rect.1, region_origin_x, region_origin_y);
                let (x1, y1, x2, y2) = normalize_rect(a, b);
                draw_rect_outline(frame, x1, y1, x2, y2, *line_width, *color)?;
            }
            DrawCommand::Ellipse { rect, color, line_width } => {
                let a = translate(rect.0, region_origin_x, region_origin_y);
                let b = translate(rect.1, region_origin_x, region_origin_y);
                let (x1, y1, x2, y2) = normalize_rect(a, b);
                draw_ellipse_outline(frame, x1, y1, x2, y2, *line_width, *color, step)?;
            }
            DrawCommand::Arrow { from, to, color, line_width } => {
                let f = translate(*from, region_origin_x, region_origin_y);
                let t = translate(*to, region_origin_x, region_origin_y);
                let dx = t.0 - f.0;
                let dy = t.1 - f.1;
                let len = (dx * dx + dy * dy).sqrt();
                if len >= 1.0 {
                    let ux = dx / len;
                    let uy = dy / len;
                    // 箭头头随线宽缩放：细线（0.5/1）用短小箭头，粗线用大箭头
                    let head_len = (line_width * 7.0).max(4.0);
                    let head_w = (line_width * 2.0).max(1.0);
                    let bx = t.0 - ux * head_len;
                    let by = t.1 - uy * head_len;
                    // 主线：均匀宽度，直达箭头底部
                    draw_thick_line(frame, f.0, f.1, bx, by, *line_width, *color, Cap::Full, Cap::Full)?;
                    // 实心箭头头：填满三角形，底边完全盖住主线末端，连接无缝
                    let px = -uy;
                    let py = ux;
                    let p1 = (bx + px * head_w, by + py * head_w);
                    let p2 = (bx - px * head_w, by - py * head_w);
                    draw_filled_triangle(frame, t.0, t.1, p1.0, p1.1, p2.0, p2.1, *color)?;
                } else {
                    // 极短线：至少画一个点
                    draw_thick_line(frame, f.0, f.1, t.0, t.1, *line_width, *color, Cap::Full, Cap::Full)?;
                }
            }
            DrawCommand::Freehand { points, color, line_width } => {
                // 整条折线一次光栅化：连接处连续无缝隙（逐段绘制会因 AA 带错位缺像素）
                let pts: Vec<(f32, f32)> = points
                    .iter()
                    .map(|p| translate(*p, region_origin_x, region_origin_y))
                    .collect();
                draw_polyline(frame, &pts, *line_width, *color, step)?;
            }
            DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
                let a = translate(*anchor, region_origin_x, region_origin_y);
                // 应用层暂不支持文字旋转，固定 0 度（pivot 传 anchor，旋转分支不生效）
                rasterize_text(frame, a, a, content, *font_size, *color, *max_width, *weight, 0.0)?;
            }
        }
    }
    Ok(())
}

/// 把屏幕坐标的命令点平移到 frame 局部坐标
fn translate(p: DrawPoint, ox: f32, oy: f32) -> (f32, f32) {
    (p.x - ox, p.y - oy)
}

/// 测试用：光栅化一条粗线（暴露 draw_thick_line 供连续性验证）
pub fn test_draw_line(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    lw: f32,
) {
    let _ = draw_thick_line(
        frame, x1, y1, x2, y2, lw,
        RGBA { r: 255, g: 0, b: 0, a: 255 },
        Cap::Full, Cap::Full,
    );
}

/// 测试用：光栅化整条折线（暴露 draw_polyline）
pub fn test_draw_polyline(frame: &mut CapturedFrame, pts: &[(f32, f32)], lw: f32) {
    let _ = draw_polyline(
        frame, pts, lw,
        RGBA { r: 255, g: 0, b: 0, a: 255 },
        1,
    );
}

pub fn test_draw_polyline_step(frame: &mut CapturedFrame, pts: &[(f32, f32)], lw: f32, step: u32) {
    let _ = draw_polyline(
        frame, pts, lw,
        RGBA { r: 255, g: 0, b: 0, a: 255 },
        step,
    );
}

/// 给两个对角点，返回 (x1, y1, x2, y2) 其中 x1<=x2, y1<=y2
fn normalize_rect(a: (f32, f32), b: (f32, f32)) -> (f32, f32, f32, f32) {
    (
        a.0.min(b.0),
        a.1.min(b.1),
        a.0.max(b.0),
        a.1.max(b.1),
    )
}

/// 像素到线段的垂距
fn point_to_segment_distance(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 0.001 {
        return ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
    }
    let t = ((px - x1) * dx + (py - y1) * dy) / len_sq;
    let t = t.clamp(0.0, 1.0);
    let proj_x = x1 + t * dx;
    let proj_y = y1 + t * dy;
    ((px - proj_x).powi(2) + (py - proj_y).powi(2)).sqrt()
}

/// smoothstep: 在 edge0..edge1 之间平滑过渡
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 画一条指定粗细的实线（距离反走样）
///
/// 对线段附近每个像素计算到线段的垂距 d：
/// - d ≤ half-0.5 → 完全不透明（线内部）
/// - half-0.5 < d ≤ half+0.5 → smoothstep 过渡（1px 反走样边缘）
/// - d > half+0.5 → 跳过
///
/// 相比之前的重叠软圆方案，内部像素只写一次，不再因多次叠加变糊。
/// 线段端点圆帽：Full=全圆帽（半径含 AA 外扩，线端圆头）；
/// Exact=精确圆帽（半径=线半宽，覆盖折线连接缺口、不超出线宽——不产生珠串鼓包）。
#[derive(Clone, Copy, PartialEq)]
enum Cap {
    Full,

}

#[allow(clippy::too_many_arguments)]
fn draw_thick_line(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    lw: f32,
    color: RGBA,
    cap_a: Cap,
    cap_b: Cap,
) -> AppResult<()> {
    // 不设最小下限：允许 0.5 线宽比 1 更细（半宽 0.25 vs 0.5）
    let half = lw / 2.0;
    // AA 过渡带宽 1px：比 0.5 更柔和（替代超采样的平滑效果），
    // 且不依赖 GPU 缩小采样（1x 光栅化，避免线条缺像素）
    let aa = 1.0_f32;
    let r = half + aa;

    let dx = x2 - x1;
    let dy = y2 - y1;
    let len_sq = dx * dx + dy * dy;

    // 退化到点
    if len_sq < 0.01 {
        return fill_round_dot(frame, x1, y1, half, color);
    }
    let len = len_sq.sqrt();

    let w_px = frame.width as i32;
    let h_px = frame.height as i32;

    let min_y = ((y1.min(y2) - r).floor() as i32).max(0);
    let max_y = ((y1.max(y2) + r).ceil() as i32).min(h_px - 1);

    for scan_y in min_y..=max_y {
        let py = scan_y as f32 + 0.5;

        // — 计算当前扫描行 x 范围 —
        let mut x_min = f32::MAX;
        let mut x_max = f32::MIN;

        // 端点圆帽：Full=含AA外扩圆头；Exact=精确线宽圆帽（折线连接补缺口，
        // 不超出线宽故无珠串）；None=平头（闭合曲线段间）
        let cap_r = |_c: Cap| Some(r);
        // 端点 A 的圆帽
        let dya = py - y1;
        if let Some(cr) = cap_r(cap_a) {
            if dya.abs() < cr {
                let c = (cr * cr - dya * dya).sqrt();
                x_min = x_min.min(x1 - c);
                x_max = x_max.max(x1 + c);
            }
        }
        // 端点 B 的圆帽
        let dyb = py - y2;
        if let Some(cr) = cap_r(cap_b) {
            if dyb.abs() < cr {
                let c = (cr * cr - dyb * dyb).sqrt();
                x_min = x_min.min(x2 - c);
                x_max = x_max.max(x2 + c);
            }
        }

        // 线段主体（两条平行边界线与扫描行的交点）
        if dy.abs() > 0.001 {
            let ux = dx / len;
            let uy = dy / len;
            // 上边界 L+: A + t*D + r*P,  其中 P = (-uy, ux)
            let t_plus = (py - y1 - r * ux) / dy;
            if (0.0..=1.0).contains(&t_plus) {
                let xx = x1 + t_plus * dx - r * uy;
                x_min = x_min.min(xx);
                x_max = x_max.max(xx);
            }
            // 下边界 L-: A + t*D - r*P
            let t_minus = (py - y1 + r * ux) / dy;
            if (0.0..=1.0).contains(&t_minus) {
                let xx = x1 + t_minus * dx + r * uy;
                x_min = x_min.min(xx);
                x_max = x_max.max(xx);
            }
        } else {
            // 水平线：整行都在主体内
            if (py - y1).abs() <= r {
                x_min = x_min.min(x1.min(x2));
                x_max = x_max.max(x1.max(x2));
            }
        }

        if x_min > x_max {
            continue;
        }

        let px0 = (x_min.floor() as i32).max(0);
        let px1 = (x_max.ceil() as i32).min(w_px - 1);

        for scan_x in px0..=px1 {
            let px = scan_x as f32 + 0.5;
            let d = point_to_segment_distance(px, py, x1, y1, x2, y2);

            let coverage = if d <= half {
                1.0
            } else if d >= r {
                continue;
            } else {
                1.0 - smoothstep(half, r, d)
            };

            let alpha = ((color.a as f32) * coverage).round() as u32;
            if alpha == 0 {
                continue;
            }
            let soft = RGBA { r: color.r, g: color.g, b: color.b, a: alpha.min(255) as u8 };
            let idx = ((scan_y * w_px + scan_x) as usize) * 4;
            blend_pixel(&mut frame.pixels[idx..idx + 4], soft);
        }
    }
    Ok(())
}

/// 三角形一条边的预计算数据：内法线平面 + 单位方向 + 边长。
///
/// 每个三角形构造一次，像素循环里只做点积，不再重复开方。
struct TriangleEdge {
    /// 单位内法线 (nx, ny)，边内侧满足 `nx*x + ny*y - c >= 0`
    nx: f32,
    ny: f32,
    c: f32,
    /// 单位方向 (ux, uy) 与起点投影 `a0 = 起点·u`，用于算沿边偏移
    ux: f32,
    uy: f32,
    a0: f32,
    /// 边长度
    len: f32,
}

impl TriangleEdge {
    /// 从 v1 指向 v2 的边；要求三角形顶点逆时针环绕，内法线才指向内部
    fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        let dx = x2 - x1;
        let dy = y2 - y1;
        let len = (dx * dx + dy * dy).sqrt();
        // 逆时针三角形，内部在每条边的左侧：内法线 = (-dy, dx) / len
        let inv = 1.0 / len;
        let nx = -dy * inv;
        let ny = dx * inv;
        Self {
            nx,
            ny,
            c: nx * x1 + ny * y1,
            ux: dx * inv,
            uy: dy * inv,
            a0: (x1 * dx + y1 * dy) * inv,
            len,
        }
    }

    /// 点到边所在直线的有符号垂距（内为正）
    #[inline]
    fn side(&self, px: f32, py: f32) -> f32 {
        self.nx * px + self.ny * py - self.c
    }

    /// 点到边**线段**的距离平方，切向越界自动收敛到端点
    #[inline]
    fn seg_dist_sq(&self, px: f32, py: f32, side: f32) -> f32 {
        let along = (px * self.ux + py * self.uy) - self.a0;
        let beyond = (-along).max(along - self.len).max(0.0);
        side * side + beyond * beyond
    }
}

/// 画实心三角形（带 1px 反走样边缘），用于实心箭头头
///
/// 对每个像素取三条边**线段**距离的最小值作带符号边界距离（内正外负），
/// 顶点附近自动收敛到端点，尖角不产生额外模糊。3 次开方在三角形预计算时
/// 摊销掉（TriangleEdge::new）；像素循环只做点积，仅 |d| <= aa 的边界带
/// 才额外开方，主体区域直接用内外判定。
fn draw_filled_triangle(
    frame: &mut CapturedFrame,
    x1: f32, y1: f32,
    x2: f32, y2: f32,
    x3: f32, y3: f32,
    color: RGBA,
) -> AppResult<()> {
    // 归一化环绕为逆时针，保证 TriangleEdge 的内法线指向内部
    let (x2, y2, x3, y3) = if (x2 - x1) * (y3 - y1) - (y2 - y1) * (x3 - x1) < 0.0 {
        (x3, y3, x2, y2)
    } else {
        (x2, y2, x3, y3)
    };
    let edges = [
        TriangleEdge::new(x1, y1, x2, y2),
        TriangleEdge::new(x2, y2, x3, y3),
        TriangleEdge::new(x3, y3, x1, y1),
    ];

    let aa = 0.5_f32;
    let aa_sq = aa * aa;
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;

    let min_x = ((x1.min(x2).min(x3) - aa).floor() as i32).max(0);
    let max_x = ((x1.max(x2).max(x3) + aa).ceil() as i32).min(w_px - 1);
    let min_y = ((y1.min(y2).min(y3) - aa).floor() as i32).max(0);
    let max_y = ((y1.max(y2).max(y3) + aa).ceil() as i32).min(h_px - 1);

    for scan_y in min_y..=max_y {
        let py = scan_y as f32 + 0.5;
        for scan_x in min_x..=max_x {
            let px = scan_x as f32 + 0.5;

            let mut inside = true;
            let mut min_sq = f32::MAX;
            for e in &edges {
                let side = e.side(px, py);
                inside &= side >= 0.0;
                min_sq = min_sq.min(e.seg_dist_sq(px, py, side));
            }

            // 距边界超过 aa：内部全不透明，外部跳过
            let coverage = if min_sq > aa_sq {
                if !inside {
                    continue;
                }
                1.0
            } else {
                // 边界带内才开方求真实距离做反走样
                let d = min_sq.sqrt();
                let signed = if inside { d } else { -d };
                smoothstep(-aa, aa, signed)
            };

            let alpha = ((color.a as f32) * coverage).round() as u32;
            if alpha == 0 {
                continue;
            }
            let soft = RGBA { r: color.r, g: color.g, b: color.b, a: alpha.min(255) as u8 };
            let idx = ((scan_y * w_px + scan_x) as usize) * 4;
            blend_pixel(&mut frame.pixels[idx..idx + 4], soft);
        }
    }
    Ok(())
}

/// 画一个反走样圆点（退化用）
/// 整条折线一次性光栅化（Freehand / 椭圆轮廓）。
///
/// 逐段独立光栅化在段连接处会因 AA 带错位出现空隙/收窄（椭圆越大越明显）；
/// 本函数对每个像素计算**到整条折线**的距离（所有段 + 顶点），连接处天然连续。
/// 中间顶点圆帽半径 = 线半宽（补凹角外侧缺口，不鼓包）；首尾顶点 Full（圆头）。
#[allow(clippy::too_many_arguments)]
fn draw_polyline(
    frame: &mut CapturedFrame,
    pts: &[(f32, f32)],
    lw: f32,
    color: RGBA,
    step: u32,
) -> AppResult<()> {
    let n = pts.len();
    if n < 2 {
        return Ok(());
    }
    let half = lw / 2.0;
    let aa = 1.0_f32;
    let r = half + aa;

    // 包围盒（外扩 r 覆盖 AA 与圆帽）
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        min_x = min_x.min(p.0);
        min_y = min_y.min(p.1);
        max_x = max_x.max(p.0);
        max_y = max_y.max(p.1);
    }
    min_x -= r;
    min_y -= r;
    max_x += r;
    max_y += r;

    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    let scan_y0 = (min_y.floor() as i32).max(0);
    let scan_y1 = (max_y.ceil() as i32).min(h_px - 1);
    let scan_x0 = (min_x.floor() as i32).max(0);
    let scan_x1 = (max_x.ceil() as i32).min(w_px - 1);

    // 预计算每段的 y 范围（含 AA 外扩）并按 y_min 排序——
    // 扫描行二分定位 y 范围覆盖的段，避免 O(像素×总段数)
    let mut seg_boxes: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(n - 1);
    for w in pts.windows(2) {
        let x0 = w[0].0.min(w[1].0) - r;
        let x1 = w[0].0.max(w[1].0) + r;
        let y0 = w[0].1.min(w[1].1) - r;
        let y1 = w[0].1.max(w[1].1) + r;
        seg_boxes.push((x0, y0, x1, y1));
    }
    seg_boxes.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    // 顶点圆帽：首尾 Full，中间 half；按 y_min(p.y - vr) 排序做行范围过滤
    let mut vcaps: Vec<(f32, f32, f32)> = pts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let vr = if i == 0 || i == n - 1 { r } else { half };
            (p.0, p.1, vr)
        })
        .collect();
    vcaps.sort_by(|a, b| (a.1 - a.2).partial_cmp(&(b.1 - b.2)).unwrap_or(std::cmp::Ordering::Equal));

    let step = step.max(1);
    let mut scan_y = scan_y0;
    while scan_y <= scan_y1 {
        let py = scan_y as f32 + 0.5;
        let mut scan_x = scan_x0;
        // 用 while 手写步进：continue 前必须推进 scan_x（见下方）
        while scan_x <= scan_x1 {
            let px = scan_x as f32 + 0.5;
            // 到整条折线的距离 = min(横穿该行的段, 覆盖该行的顶点圆帽)
            let mut d = f32::MAX;
            // 只检查 y_min <= py 的段（二分），反向遍历时 y_max < py 提前结束
            let seg_lo = seg_boxes.partition_point(|b| b.1 <= py);
            for (i, w) in pts.windows(2).take(seg_lo).enumerate().rev() {
                let (bx0, _by0, bx1, by1) = seg_boxes[i];
                if by1 < py {
                    break;
                }
                if px < bx0 || px > bx1 {
                    continue;
                }
                let sd = point_to_segment_distance(px, py, w[0].0, w[0].1, w[1].0, w[1].1);
                d = d.min(sd);
            }
            // 顶点圆帽：只检查 y_min(p.y-vr) <= py 的顶点（二分定位），
            // 再跳过 y_max(p.y+vr) < py 的——Freehand 数千点时避免每像素全遍历
            let lo = vcaps
                .partition_point(|v| v.1 - v.2 <= py);
            for (vx, vy, vr) in vcaps[..lo].iter().rev() {
                if *vy + *vr < py {
                    break; // y_max < py（按 y_min 排序，反向遍历提前结束）
                }
                if px < vx - vr || px > vx + vr {
                    continue;
                }
                let vd = ((px - vx).powi(2) + (py - vy).powi(2)).sqrt();
                if vd <= *vr {
                    d = d.min(vd);
                }
            }
            if d > r {
                scan_x += step as i32;
                continue;
            }
            let coverage = if d <= half {
                1.0
            } else {
                1.0 - smoothstep(half, r, d)
            };
            let alpha = ((color.a as f32) * coverage).round() as u32;
            if alpha == 0 {
                scan_x += step as i32;
                continue;
            }
            let soft = RGBA { r: color.r, g: color.g, b: color.b, a: alpha.min(255) as u8 };
            let idx = ((scan_y * w_px + scan_x) as usize) * 4;
            blend_pixel(&mut frame.pixels[idx..idx + 4], soft);
            scan_x += step as i32;
        }
        scan_y += step as i32;
    }
    Ok(())
}

fn fill_round_dot(
    frame: &mut CapturedFrame,
    cx: f32,
    cy: f32,
    half: f32,
    color: RGBA,
) -> AppResult<()> {
    if half <= 0.5 {
        return fill_rect_blend(frame, cx - 0.5, cy - 0.5, 1.0, 1.0, color);
    }
    let aa = 1.0_f32;
    let r = half + aa;
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    let x0 = ((cx - r).floor() as i32).max(0);
    let x1 = ((cx + r).ceil() as i32).min(w_px - 1);
    let y0 = ((cy - r).floor() as i32).max(0);
    let y1 = ((cy + r).ceil() as i32).min(h_px - 1);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let dx = (px as f32 + 0.5) - cx;
            let dy = (py as f32 + 0.5) - cy;
            let d = (dx * dx + dy * dy).sqrt();
            let coverage = if d <= half {
                1.0
            } else if d >= r {
                continue;
            } else {
                1.0 - smoothstep(half, r, d)
            };
            let alpha = ((color.a as f32) * coverage).round() as u32;
            if alpha == 0 {
                continue;
            }
            let soft = RGBA { r: color.r, g: color.g, b: color.b, a: alpha.min(255) as u8 };
            let idx = ((py * w_px + px) as usize) * 4;
            blend_pixel(&mut frame.pixels[idx..idx + 4], soft);
        }
    }
    Ok(())
}

/// 画空心椭圆边框（用 128 段折线近似椭圆轮廓）
#[allow(clippy::too_many_arguments)]
fn draw_ellipse_outline(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    lw: f32,
    color: RGBA,
    step: u32,
) -> AppResult<()> {
    let cx = (x1 + x2) / 2.0;
    let cy = (y1 + y2) / 2.0;
    let rx = (x2 - x1) / 2.0;
    let ry = (y2 - y1) / 2.0;
    let n = 128;
    // 整条椭圆折线一次光栅化（polyline）：连接处连续，越大越平滑
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let theta = 2.0 * std::f32::consts::PI * i as f32 / n as f32;
        pts.push((cx + rx * theta.cos(), cy + ry * theta.sin()));
    }
    draw_polyline(frame, &pts, lw, color, step)?;
    Ok(())
}

/// 画空心矩形边框（4 条粗线）
fn draw_rect_outline(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    lw: f32,
    color: RGBA,
) -> AppResult<()> {
    let w = x2 - x1;
    let h = y2 - y1;
    draw_thick_line(frame, x1, y1, x2, y1, lw, color, Cap::Full, Cap::Full)?;
    draw_thick_line(frame, x1, y2, x2, y2, lw, color, Cap::Full, Cap::Full)?;
    draw_thick_line(frame, x1, y1, x1, y2, lw, color, Cap::Full, Cap::Full)?;
    draw_thick_line(frame, x2, y1, x2, y2, lw, color, Cap::Full, Cap::Full)?;
    // 让编译器闭嘴（h 没用上）
    let _ = (w, h);
    Ok(())
}

/// 在 frame 上以 alpha 混合方式画一个填充矩形
///
/// 颜色按标准 "SourceOver" 规则与 frame 现有像素合成；
/// 越界部分被裁剪（不返回错误）。
fn fill_rect_blend(
    frame: &mut CapturedFrame,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: RGBA,
) -> AppResult<()> {
    if w <= 0.0 || h <= 0.0 {
        return Ok(());
    }
    let x_start = x.max(0.0).ceil() as i32;
    let y_start = y.max(0.0).ceil() as i32;
    let x_end = (x + w).ceil() as i32;
    let y_end = (y + h).ceil() as i32;
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    for py in y_start..y_end {
        if py < 0 || py >= h_px {
            continue;
        }
        for px in x_start..x_end {
            if px < 0 || px >= w_px {
                continue;
            }
            let idx = ((py * w_px + px) as usize) * 4;
            if idx + 3 >= frame.pixels.len() {
                return Err(AppError::Window("fill_rect_blend 索引越界".into()));
            }
            blend_pixel(&mut frame.pixels[idx..idx + 4], color);
        }
    }
    Ok(())
}

/// SourceOver alpha 合成：dst = src + dst * (1 - src.a)
fn blend_pixel(dst: &mut [u8], src: RGBA) {
    let sa = src.a as u32;
    let inv = 255 - sa;
    for i in 0..3 {
        let s = [src.r, src.g, src.b][i] as u32;
        let d = dst[i] as u32;
        dst[i] = ((s * sa + d * inv) / 255) as u8;
    }
    dst[3] = src.a.max(dst[3]);
}

/// 对 frame 中 (x1, y1) - (x2, y2) 区域做马赛克
///
/// 1. 把原区域 resize 到 (w/block_size, h/block_size)（nearest-neighbor）
/// 2. 再 resize 回原尺寸
/// 3. 叠加颜色（低 alpha 模拟马赛克预览的调色效果）
/// 4. 写回 frame 对应区域
fn apply_mosaic(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    block_size: u32,
    color: RGBA,
) -> AppResult<()> {
    let bs = block_size.max(1) as i32;
    let rx = x1 as i32;
    let ry = y1 as i32;
    let rw = (x2 - x1) as i32;
    let rh = (y2 - y1) as i32;
    if rw <= 0 || rh <= 0 {
        return Ok(());
    }
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    let small_w = (rw / bs).max(1);
    let small_h = (rh / bs).max(1);

    // 1) 提取原区域像素到 small buffer
    let mut small = vec![0u8; (small_w * small_h * 4) as usize];
    for sy in 0..small_h {
        for sx in 0..small_w {
            let src_x = (rx + sx * bs).clamp(0, w_px - 1);
            let src_y = (ry + sy * bs).clamp(0, h_px - 1);
            let src_idx = ((src_y * w_px + src_x) as usize) * 4;
            let dst_idx = ((sy * small_w + sx) as usize) * 4;
            small[dst_idx..dst_idx + 4].copy_from_slice(&frame.pixels[src_idx..src_idx + 4]);
        }
    }

    // 2) nearest 放大回原尺寸实现马赛克
    let img_small = image::RgbaImage::from_raw(small_w as u32, small_h as u32, small)
        .ok_or_else(|| AppError::Window("mosaic 创建 ImageBuffer 失败".into()))?;
    let img_big = image::imageops::resize(&img_small, rw as u32, rh as u32, FilterType::Nearest);

    // 3) 写回 frame（像素化 + 颜色叠加）
    let tint = RGBA::new(color.r, color.g, color.b, 0x48);
    for dy in 0..rh {
        let py = ry + dy;
        if py < 0 || py >= h_px {
            continue;
        }
        for dx in 0..rw {
            let px = rx + dx;
            if px < 0 || px >= w_px {
                continue;
            }
            let src_idx = ((dy * rw + dx) as usize) * 4;
            let dst_idx = ((py * w_px + px) as usize) * 4;
            frame.pixels[dst_idx..dst_idx + 4]
                .copy_from_slice(&img_big.as_raw()[src_idx..src_idx + 4]);
            blend_pixel(&mut frame.pixels[dst_idx..dst_idx + 4], tint);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_frame(w: u32, h: u32) -> CapturedFrame {
        CapturedFrame {
            width: w,
            height: h,
            pixels: vec![0; (w * h * 4) as usize],
        }
    }

    #[test]
    fn rectangle_outline_paints_4_edges() {
        let mut f = empty_frame(20, 20);
        let rect = (
            DrawPoint::new(2.0, 2.0),
            DrawPoint::new(10.0, 8.0),
        );
        let cmd = DrawCommand::Rectangle {
            rect,
            color: RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            line_width: 1.0,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();
        // 顶部 y=2 应该是红色
        let idx = (2 * 20 + 5) * 4;
        assert!(f.pixels[idx] > 200, "top edge r = {}", f.pixels[idx]);
        // 内部点 (5, 5) 应该是 0（没填充）
        let idx = (5 * 20 + 5) * 4;
        assert_eq!(f.pixels[idx], 0, "interior should be 0");
    }

    #[test]
    fn freehand_connects_consecutive_points() {
        let mut f = empty_frame(20, 20);
        let cmd = DrawCommand::Freehand {
            points: vec![
                DrawPoint::new(0.0, 0.0),
                DrawPoint::new(10.0, 10.0),
                DrawPoint::new(20.0, 0.0),
            ],
            color: RGBA::new(0x00, 0xFF, 0x00, 0xFF),
            line_width: 1.0,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();
        // 起点 (0, 0) 应该是绿色
        let idx_00 = 0;
        assert!(
            f.pixels[idx_00 + 1] > 200,
            "g at (0,0) = {}",
            f.pixels[idx_00 + 1]
        );
        // 中点 (10, 10) 应该是绿色（45° 斜线上，像素中心距线段 0.707px，
        // 半宽 0.5px 的反走样边缘，偏透明是几何正确的）
        let idx_mid = (10 * 20 + 10) * 4;
        assert!(
            f.pixels[idx_mid + 1] > 100,
            "g at (10,10) = {}",
            f.pixels[idx_mid + 1]
        );
    }

    #[test]
    fn translate_offsets_screen_to_local() {
        let mut f = empty_frame(20, 20);
        let rect = (
            DrawPoint::new(102.0, 52.0),  // 屏幕坐标
            DrawPoint::new(108.0, 58.0),
        );
        let cmd = DrawCommand::Rectangle {
            rect,
            color: RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            line_width: 1.0,
        };
        // region origin (100, 50)
        apply_commands(&mut f, 100.0, 50.0, &[cmd]).unwrap();
        // 局部 (2, 2) 应该是红色
        let idx = (2 * 20 + 2) * 4;
        assert!(f.pixels[idx] > 200, "r = {}", f.pixels[idx]);
    }

    #[test]
    fn mosaic_blurs_region() {
        let mut f = empty_frame(20, 20);
        // 在 (10, 10) 标一个红点，其余黑
        let i = (10 * 20 + 10) * 4;
        f.pixels[i] = 0xFF;
        f.pixels[i + 3] = 0xFF;

        let cmd = DrawCommand::Mosaic {
            regions: vec![(DrawPoint::new(0.0, 0.0), DrawPoint::new(20.0, 20.0))],
            block_size: 10,
            color: RGBA::new(0x80, 0x80, 0x80, 0x80),
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();

        // mosaic with block_size=10 → small buffer 2x2
        // small[1,1] samples src (10, 10) → red；其余采样 (0,0)/(10,0)/(0,10) 都是 0
        // nearest upscale 到 20x20 → (10..20, 10..20) 区域全红
        // apply_mosaic 叠加灰色 tint（alpha=0x48），红区变暗红，黑区变暗灰
        let idx = (15 * 20 + 15) * 4;
        assert!(
            f.pixels[idx] > 180,
            "mosaic should propagate red to (15,15), got r={}",
            f.pixels[idx]
        );
        // (0..10, 0..10) 区域原来全黑，叠加灰色 tint 后略灰
        let idx = (5 * 20 + 5) * 4;
        assert!(
            f.pixels[idx] < 60,
            "r at (5,5) should be dark (gray tint over black), got {}",
            f.pixels[idx]
        );
    }

    #[test]
    fn arrow_renders_solid_filled_head() {
        let mut f = empty_frame(64, 32);
        let cmd = DrawCommand::Arrow {
            from: DrawPoint::new(2.0, 16.0),
            to: DrawPoint::new(48.0, 16.0),
            color: RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            line_width: 2.0,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();
        // 箭杆中点应实心红
        let idx = (16 * 64 + 10) * 4;
        assert!(f.pixels[idx] > 200, "shaft r = {}", f.pixels[idx]);
        // 箭头内部（三角形中轴线附近）应被填实，而非空心 V
        let idx = (16 * 64 + 38) * 4;
        assert!(f.pixels[idx] > 200, "head interior r = {}", f.pixels[idx]);
        // 箭头内部靠上一条边也应填实
        let idx = (18 * 64 + 36) * 4;
        assert!(f.pixels[idx] > 100, "head upper r = {}", f.pixels[idx]);
        // 超过尖端 (48,16) 的像素应完全透明，头不能溢出
        let idx = (16 * 64 + 50) * 4;
        assert_eq!(f.pixels[idx + 3], 0, "beyond tip alpha = {}", f.pixels[idx + 3]);
    }

    #[test]
    fn filled_triangle_winding_invariant() {
        let mut ccw = empty_frame(20, 20);
        draw_filled_triangle(&mut ccw, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, RGBA::RED).unwrap();
        let mut cw = empty_frame(20, 20);
        // 顺时针顶点序：应被内部归一化为逆时针后填充一致
        draw_filled_triangle(&mut cw, 0.0, 0.0, 0.0, 10.0, 10.0, 0.0, RGBA::RED).unwrap();
        for (y, x) in [(3, 3), (5, 2), (2, 5)] {
            let idx = (y * 20 + x) * 4;
            assert!(ccw.pixels[idx] > 200, "ccw inside ({x},{y}) r = {}", ccw.pixels[idx]);
            assert!(cw.pixels[idx] > 200, "cw inside ({x},{y}) r = {}", cw.pixels[idx]);
        }
        // 外部 (3,12)：两者都透明
        let idx = (12 * 20 + 3) * 4;
        assert_eq!(ccw.pixels[idx + 3], 0, "ccw outside alpha = {}", ccw.pixels[idx + 3]);
        assert_eq!(cw.pixels[idx + 3], 0, "cw outside alpha = {}", cw.pixels[idx + 3]);
    }

    // ====== T5: rasterize_text 真实现测试 ======

    #[test]
    fn debug_measure_ink_bbox_single_line() {
        // 临时诊断：对比不同 line_height 下 cosmic-text 的首行基线位置
        for lh in [33.6_f32, 36.0_f32] {
            with_font_system(|font_system| {
                let metrics = Metrics::new(24.0, lh);
                let mut buffer = Buffer::new(font_system, metrics);
                let attrs = Attrs::new().family(Family::Name(TEXT_FONT_FAMILY)).weight(Weight::NORMAL);
                buffer.set_text("文字蚊子", &attrs, Shaping::Advanced, None);
                buffer.set_size(None, None);
                buffer.shape_until_scroll(font_system, false);
                for run in buffer.layout_runs() {
                    let first = run.glyphs.iter().next().unwrap();
                    let phys = first.physical((0.0, run.line_y), 1.0);
                    println!(
                        "DEBUG_COSMIC lh={:.1} line_y={:.1} baseline_phys.y={:.1}",
                        lh, run.line_y, phys.y
                    );
                    break;
                }
            });
        }
        // 临时诊断：单行 vs 两行时 rasterize_text 的 ink 包围盒相对 anchor(0,0) 的位置
        for (label, content) in [("single", "文字蚊子"), ("double", "文字蚊子\n第二行字")] {
            let mut f = empty_frame(400, 200);
            rasterize_text(
                &mut f, (0.0, 0.0), (0.0, 0.0), content, 24.0, RGBA::RED, None, FontWeight::Normal, 0.0,
            )
            .unwrap();
            let mut min_x = u32::MAX;
            let mut min_y = u32::MAX;
            let mut max_x = 0;
            let mut max_y = 0;
            for row in 0..f.height {
                for col in 0..f.width {
                    if f.pixels[(row * f.width + col) as usize * 4] != 0 {
                        min_x = min_x.min(col);
                        min_y = min_y.min(row);
                        max_x = max_x.max(col);
                        max_y = max_y.max(row);
                    }
                }
            }
            println!(
                "DEBUG_RASTER {} ink_bbox=({},{})-({},{}) size={}x{}",
                label, min_x, min_y, max_x, max_y, max_x - min_x + 1, max_y - min_y + 1
            );
        }
    }

    #[test]
    fn rasterize_text_empty_content_noop() {
        let mut f = empty_frame(50, 30);
        let baseline = f.pixels.clone();
        rasterize_text(
            &mut f, (0.0, 0.0), (0.0, 0.0), "", 16.0, RGBA::RED, None, FontWeight::Normal, 0.0,
        )
        .unwrap();
        assert_eq!(f.pixels, baseline, "空 content 不能改 frame");
    }

    #[test]
    fn rasterize_text_out_of_frame_anchor_does_not_panic() {
        let mut f = empty_frame(20, 20);
        rasterize_text(
            &mut f, (-100.0, -100.0), (-100.0, -100.0), "test", 16.0, RGBA::RED, None, FontWeight::Normal, 0.0,
        )
        .unwrap();
        assert_eq!(f.width, 20);
        assert_eq!(f.height, 20);
    }

    #[test]
    fn rasterize_text_basic_writes_some_pixels() {
        let mut f = empty_frame(200, 60);
        rasterize_text(
            &mut f,
            (10.0, 10.0),
            (10.0, 10.0),
            "Hi 你好",
            32.0,
            RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            None,
            FontWeight::Normal, 0.0,
        )
        .unwrap();
        let non_zero = f.pixels.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 10, "应至少写 10 个非 0 像素: actual={}", non_zero);
        let red_count = (0..f.pixels.len() / 4)
            .filter(|&i| f.pixels[i * 4] > 100)
            .count();
        assert!(red_count > 0, "应至少有一个明显的红色像素");
    }

    #[test]
    fn rasterize_text_multi_line_when_max_width_small() {
        let mut f = empty_frame(80, 200);
        let content: String = "你好世界ABCDEFGHIJ".repeat(6);
        rasterize_text(
            &mut f,
            (0.0, 0.0),
            (0.0, 0.0),
            &content,
            24.0,
            RGBA::RED,
            Some(50.0),
            FontWeight::Normal, 0.0,
        )
        .unwrap();
        // 60 字 / 50px 约每行 5-6 字 → 应至少跑出 3 行
        let bottom_written = (100..200).any(|row| {
            (0..80).any(|col| f.pixels[(row * 80 + col) * 4] != 0)
        });
        assert!(bottom_written, "max_width=50 应迫使文字折多行，下半部分应有像素");
    }

    #[test]
    fn rasterize_text_bold_changes_at_least_one_pixel() {
        let mut normal = empty_frame(120, 60);
        let mut bold = empty_frame(120, 60);
        rasterize_text(
            &mut normal, (10.0, 10.0), (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Normal, 0.0,
        )
        .unwrap();
        rasterize_text(
            &mut bold, (10.0, 10.0), (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Bold, 0.0,
        )
        .unwrap();
        let diff = normal
            .pixels
            .iter()
            .zip(bold.pixels.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(diff > 0, "Normal 和 Bold 应至少 1 像素不同: diff={}", diff);
    }
}
