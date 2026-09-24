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
    background: RGBA,
    box_size: (f32, f32),
    text_inset: f32,
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
    let (physical_glyphs, max_line_w): (Vec<(f32, f32, cosmic_text::CacheKey)>, f32) =
        with_font_system(|font_system| {
            let metrics = Metrics::new(font_size, font_size * 1.5);
            let mut buffer = Buffer::new(font_system, metrics);
            let attrs = Attrs::new()
                .family(Family::Name(TEXT_FONT_FAMILY))
                .weight(weight_attr);
            buffer.set_text(content, &attrs, Shaping::Advanced, None);
            buffer.set_size(max_w, None);
            buffer.shape_until_scroll(font_system, false);

            let mut out = Vec::new();
            let mut max_line_w: f32 = 0.0;
            for run in buffer.layout_runs() {
                let mut run_max_x: f32 = 0.0;
                for glyph in run.glyphs.iter() {
                    // glyph.physical：offset 是 line 起点 (0, line_y)；scale=1
                    let phys = glyph.physical((0.0, run.line_y), 1.0);
                    let px = phys.x as f32;
                    run_max_x = run_max_x.max(px);
                    out.push((px, phys.y as f32, phys.cache_key));
                }
                max_line_w = max_line_w.max(run.line_w.max(run_max_x));
            }
            (out, max_line_w)
        });

    // 阶段 1.5：旋转中心 = 传入的 pivot（文本框中心），与旋转框保持一致
    let (cx, cy, cos, sin) = if rotation != 0.0 {
        let angle = rotation * std::f32::consts::PI / 180.0;
        let (s, c) = angle.sin_cos();
        (pivot.0, pivot.1, c, s)
    } else {
        (0.0, 0.0, 1.0, 0.0)
    };

    // 背景（高亮/选框）：用「实际编辑框尺寸」铺一块带小圆角的整矩形，画在字形之下。
    // 尺寸来自 box_size（逻辑像素，snapshot 时已换算到物理 px），保证与编辑框一致；
    // 四角用小半径圆角柔化（不做尖锐方形）。
    if background.a > 0 {
        let (bw, bh) = if box_size.0 > 0.0 && box_size.1 > 0.0 {
            box_size
        } else {
            // 兜底：无 box_size（旧命令）时退回字形包围盒
            let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
            for (_, gy, _) in &physical_glyphs {
                min_y = min_y.min(*gy);
                max_y = max_y.max(*gy);
            }
            let pad = font_size * 0.22;
            (max_line_w.max(1.0) + pad * 2.0, (max_y - min_y) + pad * 2.0)
        };
        if bw > 0.0 && bh > 0.0 {
            let radius = (font_size * 0.15).min(bw.min(bh) * 0.5);
            // 无旋转：直接锚点（编辑框左上角）；有旋转时背景只按未旋转框铺。
            if rotation == 0.0 {
                fill_rounded_rect_blend(frame, anchor_x, anchor_y, bw, bh, radius, background)?;
            }
        }
    }

    // 阶段 2：rasterize。每个 glyph 单独进入 swash_cache；为避开
    // 同时借用 font_system + swash_cache 两个 RefCell 的问题，
    // swash_cache 闭包内再开一个 with_font_system（不同 thread_local，无重叠）。
    // 性能：直接用 `get_image(...).as_ref()` 借用缓存里的 mask，**不再 `.cloned()`
    // 深拷贝整张 SwashImage（其 `data: Vec<u8>` 是每字形一张 alpha 位图）**——
    // 把 blend 移到闭包内完成。否则每个字形/每帧都要复制一张位图（几十~上百字形）。
    for (gx, gy, cache_key) in physical_glyphs {
        let (px, py) = if rotation != 0.0 {
            let rx = anchor_x + text_inset + gx - cx;
            let ry = anchor_y + gy - cy;
            (cx + rx * cos - ry * sin, cy + rx * sin + ry * cos)
        } else {
            (anchor_x + text_inset + gx, anchor_y + gy)
        };
        with_swash_cache(|swash| {
            with_font_system(|fs| {
                if let Some(mask) = swash.get_image(fs, cache_key).as_ref() {
                    blend_mask_to_frame(frame, mask, px, py, color);
                }
            });
        });
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
            // **整条命令一次写回**，不逐个 stamp 调用。
            //
            // 逐个写回时，重叠的 stamp 会把同一像素的颜色叠加多次（同一处越涂越深），
            // 而拖动预览是一次算完 —— 两边就对不上。整条命令一次算完，预览与提交才
            // 是同一张图（见 render_mosaic_stroke_pixels 的说明）。
            apply_mosaic(frame, region_origin_x, region_origin_y, regions, *block_size, *color)?;
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
                    // 箭头头随线宽缩放，但细线保底尺寸：1px 线宽下箭头头也要
                    // 清晰可见（原 head_len=7/head_w=2 的三角形太小 + 尖端被
                    // AA 削平，看起来"没有箭头"）。底线 10/3 让 1px 箭头的
                    // 头足够醒目，粗线仍按比例放大；且头长不超过线长 70%，
                    // 避免短箭头底边越过起点导致主线反向延伸。
                    let head_len = ((line_width * 7.0).max(10.0)).min(len * 0.7);
                    let head_w = (line_width * 2.0).max(3.0);
                    let bx = t.0 - ux * head_len;
                    let by = t.1 - uy * head_len;
                    // 主线：均匀宽度，直达箭头底部
                    draw_thick_line(frame, f.0, f.1, bx, by, *line_width, *color, Cap::Full, Cap::Full)?;
                    // 实心箭头头：填满三角形，底边完全盖住主线末端，连接无缝
                    let px = -uy;
                    let py = ux;
                    let p1 = (bx + px * head_w, by + py * head_w);
                    let p2 = (bx - px * head_w, by - py * head_w);
                    // 尖端沿箭头方向外扩半个 AA 带宽：尖端像素中心落在三角形内，
                    // 避免尖角被 AA 削成"钝头"（细线下看起来没有箭头尖）。
                    let tx = t.0 + ux * 0.5;
                    let ty = t.1 + uy * 0.5;
                    draw_filled_triangle(frame, tx, ty, p1.0, p1.1, p2.0, p2.1, *color)?;
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
                draw_polyline(frame, &pts, *line_width, *color, step, Cap::Full, Cap::Full)?;
            }
            DrawCommand::Text { anchor, content, font_size, color, max_width, weight, background, box_size, text_inset } => {
                let a = translate(*anchor, region_origin_x, region_origin_y);
                // 应用层暂不支持文字旋转，固定 0 度（pivot 传 anchor，旋转分支不生效）
                rasterize_text(frame, a, a, content, *font_size, *color, *max_width, *weight, 0.0, *background, *box_size, *text_inset)?;
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

/// 光栅化整条折线到帧（增量渲染用：坐标已 translate 到帧局部）。
/// 供 OverlayView 增量画新增段时直接调用，避免 DrawCommand 构造/分发开销。
pub fn draw_polyline_pub(
    frame: &mut CapturedFrame,
    pts: &[(f32, f32)],
    lw: f32,
    color: RGBA,
    step: u32,
    cap_start: Cap,
    cap_end: Cap,
) -> AppResult<()> {
    draw_polyline(frame, pts, lw, color, step, cap_start, cap_end)
}

/// 测试用：光栅化整条折线（暴露 draw_polyline）
pub fn test_draw_polyline(frame: &mut CapturedFrame, pts: &[(f32, f32)], lw: f32) {
    let _ = draw_polyline(
        frame, pts, lw,
        RGBA { r: 255, g: 0, b: 0, a: 255 },
        1, Cap::Full, Cap::Full,
    );
}

pub fn test_draw_polyline_step(frame: &mut CapturedFrame, pts: &[(f32, f32)], lw: f32, step: u32) {
    let _ = draw_polyline(
        frame, pts, lw,
        RGBA { r: 255, g: 0, b: 0, a: 255 },
        step, Cap::Full, Cap::Full,
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
pub enum Cap {
    Full,
    Exact,
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
        let cap_r = |c: Cap| match c {
            Cap::Full => Some(r),
            Cap::Exact => Some(half),
        };
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
    cap_start: Cap,
    cap_end: Cap,
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

    // —— 二维网格分桶：段/顶点注册到其包围盒覆盖的 cell（cell 大小随线宽），
    //    像素查询 O(1) 定位所在 cell，只检查该 cell 内的段/顶点 ——
    //    避免 O(像素 × 顶点数)（大笔画 + 大包围盒时每帧数百毫秒）
    let cs = ((lw * 2.0 + 4.0).max(8.0)) as i32; // cell 边长
    use std::collections::HashMap;
    let mut seg_cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (i, w) in pts.windows(2).enumerate() {
        let x0 = (w[0].0.min(w[1].0) - r).floor() as i32 / cs;
        let x1 = (w[0].0.max(w[1].0) + r).ceil() as i32 / cs;
        let y0 = (w[0].1.min(w[1].1) - r).floor() as i32 / cs;
        let y1 = (w[0].1.max(w[1].1) + r).ceil() as i32 / cs;
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                seg_cells.entry((cx, cy)).or_default().push(i);
            }
        }
    }
    // 顶点圆帽：首尾 Full，中间 half；每个顶点注册到其覆盖的 cell
    // cell → 顶点圆帽列表：(vx, vy, vr)
    type Vcap = (f32, f32, f32);
    let mut vcap_cells: HashMap<(i32, i32), Vec<Vcap>> = HashMap::new();
    for (i, p) in pts.iter().enumerate() {
        let vr = if i == 0 {
            match cap_start { Cap::Full => r, Cap::Exact => half }
        } else if i == n - 1 {
            match cap_end { Cap::Full => r, Cap::Exact => half }
        } else {
            half
        };
        let cx0 = (p.0 - vr).floor() as i32 / cs;
        let cx1 = (p.0 + vr).ceil() as i32 / cs;
        let cy0 = (p.1 - vr).floor() as i32 / cs;
        let cy1 = (p.1 + vr).ceil() as i32 / cs;
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                vcap_cells.entry((cx, cy)).or_default().push((p.0, p.1, vr));
            }
        }
    }

    let scan_y0 = (min_y.floor() as i32).max(0);
    let scan_y1 = (max_y.ceil() as i32).min(h_px - 1);
    let scan_x0 = (min_x.floor() as i32).max(0);
    let scan_x1 = (max_x.ceil() as i32).min(w_px - 1);

    let step = step.max(1);
    let mut scan_y = scan_y0;
    while scan_y <= scan_y1 {
        let py = scan_y as f32 + 0.5;
        let cell_y = scan_y / cs;
        let mut scan_x = scan_x0;
        // 用 while 手写步进：continue 前必须推进 scan_x（见下方）
        while scan_x <= scan_x1 {
            let px = scan_x as f32 + 0.5;
            let cell_x = scan_x / cs;
            let mut d = f32::MAX;
            // 查所在 cell 的段
            if let Some(sids) = seg_cells.get(&(cell_x, cell_y)) {
                for &si in sids {
                    let w = &pts[si..si + 2];
                    let sd = point_to_segment_distance(px, py, w[0].0, w[0].1, w[1].0, w[1].1);
                    d = d.min(sd);
                }
            }
            // 查所在 cell 的顶点圆帽
            if let Some(vs) = vcap_cells.get(&(cell_x, cell_y)) {
                for &(vx, vy, vr) in vs {
                    let vd = ((px - vx).powi(2) + (py - vy).powi(2)).sqrt();
                    if vd <= vr {
                        d = d.min(vd);
                    }
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
    draw_polyline(frame, &pts, lw, color, step, Cap::Full, Cap::Full)?;
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

/// 带圆角的 SourceOver 填充：铺一个上下左右都留 `radius` 圆角的实心矩形。
///
/// 判定方式：像素若落在「内缩矩形」内，或无圆角中央区；
/// 否则看是否落在 4 个角圆心（半径 radius）的圆内。圆角很小（≈0.15×字号）。
fn fill_rounded_rect_blend(
    frame: &mut CapturedFrame,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: RGBA,
) -> AppResult<()> {
    if w <= 0.0 || h <= 0.0 || radius <= 0.0 {
        return fill_rect_blend(frame, x, y, w, h, color);
    }
    let x_start = x.max(0.0).ceil() as i32;
    let y_start = y.max(0.0).ceil() as i32;
    let x_end = (x + w).ceil() as i32;
    let y_end = (y + h).ceil() as i32;
    let r = radius.max(1.0);
    let (wx, wy) = (x as f32, y as f32);
    let x0 = wx + r;
    let x1 = wx + w - r;
    let y0 = wy + r;
    let y1 = wy + h - r;
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    let r2 = r * r;
    for py in y_start..y_end {
        if py < 0 || py >= h_px {
            continue;
        }
        let fpy = py as f32 + 0.5;
        for px in x_start..x_end {
            if px < 0 || px >= w_px {
                continue;
            }
            let fpx = px as f32 + 0.5;
            // SDF：到内缩矩形 [x0,x1]×[y0,y1] 的距离；<=r 即落在圆角矩形内。
            // 边缘带（非角落）dx/dy 有一项为 0，dist 即到内缩边的距离，同样 <=r。
            let dx = if fpx < x0 { x0 - fpx } else if fpx > x1 { fpx - x1 } else { 0.0 };
            let dy = if fpy < y0 { y0 - fpy } else if fpy > y1 { fpy - y1 } else { 0.0 };
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let idx = ((py * w_px + px) as usize) * 4;
            if idx + 3 >= frame.pixels.len() {
                return Err(AppError::Window("fill_rounded_rect_blend 索引越界".into()));
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

/// **块平均像素化**：算出 `[x0,x1)×[y0,y1)` 每块的平均色，写进 `out`（该区域大小）。
///
/// 每个 `bs×bs` 块填**整块的平均色**，而不是取块内某一个点。这不是细节而是成败
/// 关键：原来马赛克每块只采样左上角一个像素，等于把像素搬了个位置 —— 内容一点没
/// 少，所以用户要"反复涂抹才能遮挡"（涂第二遍时采到的还是原文的像素）。取平均才
/// 是真的把一块里的内容**抹成一个颜色**，一笔就能盖住。
///
/// 网格原点强制为 `bs` 的整数倍（绝对网格）：同一像素落在哪个块，只由图片坐标
/// 决定，与"这次请求了多大区域、由哪个 stamp 发起"无关。少了这一条，重叠的方块
/// 会各自用不同相位取样，边界交叉涂抹。
///
/// 块范围裁到图片内；完全在图片外的块取最近的真实像素（不越界、不留空洞）。
///
/// 返回 `(块均色像素, 每块是否"有内容")`，两者都按 `gw×gh` 的块序排列。
/// "有内容"的判定见 [`mosaic_block_has_content`]：**空白背景不打码**，只有文字、
/// 线条这类有内容的块才打码（用户要求："无效区域没有任何内容只有颜色的不做模糊"）。
#[allow(clippy::too_many_arguments)]
pub fn mosaic_block_average_ex(
    src: &[u8],
    sw: u32,
    sh: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bs: i32,
) -> (Vec<u8>, Vec<bool>) {
    let bs = bs.max(1);
    let rw = (x1 - x0).max(0) as usize;
    let rh = (y1 - y0).max(0) as usize;
    let mut out = vec![0u8; rw * rh * 4];
    if rw == 0 || rh == 0 || sw == 0 || sh == 0 {
        return (out, Vec::new());
    }
    let (swi, shi) = (sw as i32, sh as i32);
    let gx0 = x0.div_euclid(bs) * bs;
    let gy0 = y0.div_euclid(bs) * bs;
    let gx1 = (x1 + bs - 1).div_euclid(bs) * bs;
    let gy1 = (y1 + bs - 1).div_euclid(bs) * bs;
    let gw = ((gx1 - gx0) as usize / bs as usize).max(1);
    let gh = ((gy1 - gy0) as usize / bs as usize).max(1);

    let mut small = vec![0u8; gw * gh * 4];
    let mut flat = vec![false; gw * gh];
    for j in 0..gh {
        for i in 0..gw {
            let abx0 = gx0 + (i * bs as usize) as i32;
            let aby0 = gy0 + (j * bs as usize) as i32;
            let bx0 = abx0.clamp(0, swi);
            let by0 = aby0.clamp(0, shi);
            let bx1 = (abx0 + bs).clamp(0, swi);
            let by1 = (aby0 + bs).clamp(0, shi);
            let d = (j * gw + i) * 4;
            if bx1 <= bx0 || by1 <= by0 {
                // 完全在图片外：取最近的真实像素（边缘复制）
                let cx = abx0.clamp(0, swi - 1) as usize;
                let cy = aby0.clamp(0, shi - 1) as usize;
                let s_off = (cy * sw as usize + cx) * 4;
                small[d..d + 4].copy_from_slice(&src[s_off..s_off + 4]);
                flat[j * gw + i] = false;
                continue;
            }
            let n = ((bx1 - bx0) * (by1 - by0)) as u32;
            let mut acc = [0u32; 4];
            for y in by0..by1 {
                let base = y as usize * sw as usize * 4;
                for x in bx0..bx1 {
                    let s_off = base + x as usize * 4;
                    for c in 0..4 {
                        acc[c] += u32::from(src[s_off + c]);
                    }
                }
            }
            for c in 0..4 {
                small[d + c] = ((acc[c] + n / 2) / n) as u8;
            }
            // 第二遍：块内像素与块均值的最大色差 → 判断这块是不是"纯色"
            // （纯色 = 没有文字笔画、线条边这类结构，只有一片颜色）
            let mut max_dev = 0u8;
            for y in by0..by1 {
                let base = y as usize * sw as usize * 4;
                for x in bx0..bx1 {
                    let s_off = base + x as usize * 4;
                    for c in 0..3 {
                        let dev = src[s_off + c].abs_diff(small[d + c]);
                        if dev > max_dev {
                            max_dev = dev;
                        }
                    }
                }
            }
            flat[j * gw + i] = max_dev <= MOSAIC_FLAT_TOL;
        }
    }
    // 整块铺开：每块是纯色方块，块内不插值
    for y in 0..rh {
        let j = (((y0 + y as i32) - gy0).div_euclid(bs)).clamp(0, gh as i32 - 1) as usize;
        for x in 0..rw {
            let i = (((x0 + x as i32) - gx0).div_euclid(bs)).clamp(0, gw as i32 - 1) as usize;
            let s_off = (j * gw + i) * 4;
            let d_off = (y * rw + x) * 4;
            out[d_off..d_off + 4].copy_from_slice(&small[s_off..s_off + 4]);
        }
    }
    // 每块"有没有内容"：块内有色差（文字笔画/线条边）→ 有内容；纯色的块只有和
    // 整笔背景色一致时才算背景（否则是粗线条/大色块内部，属于内容）。
    let bg = mosaic_dominant_color(src, sw, sh, x0, y0, x1, y1);
    let mut content = vec![false; gw * gh];
    for j in 0..gh {
        for i in 0..gw {
            let d = (j * gw + i) * 4;
            let dev = (0..3)
                .map(|c| small[d + c].abs_diff(bg[c]))
                .max()
                .unwrap_or(0);
            content[j * gw + i] = !flat[j * gw + i] || dev > MOSAIC_BG_TOL;
        }
    }
    (out, content)
}

/// 「一键模糊」的阈值（百分比）：选区内 **≥ 此比例**的内容块已被马赛克覆盖，
/// 就认为"这块已经模糊过了"，再次框选时**还原成清晰**；否则把选区内的内容全部模糊。
pub const MOSAIC_RESTORE_RATIO: usize = 80;

/// 选区的内容块网格 → `(gx0, gy0, 每行块数, 每块是否有内容)`。
///
/// 网格原点与 [`mosaic_block_average_ex`]、渲染器完全一致（钉在 `bs` 整数倍上），
/// 否则"数出来的内容块"和"实际会被打码的块"会错位。
#[allow(clippy::too_many_arguments)]
fn mosaic_content_grid(
    src: &[u8],
    sw: u32,
    sh: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bs: i32,
) -> Option<(i32, i32, usize, Vec<bool>)> {
    let bsi = bs.max(1);
    if x1 <= x0 || y1 <= y0 || sw == 0 || sh == 0 {
        return None;
    }
    let (_, content) = mosaic_block_average_ex(src, sw, sh, x0, y0, x1, y1, bsi);
    if content.is_empty() {
        return None;
    }
    let gx0 = x0.div_euclid(bsi) * bsi;
    let gy0 = y0.div_euclid(bsi) * bsi;
    let gw = (((x1 + bsi - 1).div_euclid(bsi) * bsi - gx0).max(0) / bsi).max(1) as usize;
    Some((gx0, gy0, gw, content))
}

/// 列出选区内**内容块**的矩形（帧像素坐标，`bs×bs`，绝对网格）。
///
/// 「一键模糊」用它**逐块**建马赛克笔迹，而不是拿一个大方块盖住整框 —— 后者在还原时
/// 只能整块删掉：框一小片就会把整大片一起还原（"第二次框很小区域，框外一大片也变清晰了"）。
/// 逐块建笔迹后，还原能精确到块。
#[allow(clippy::too_many_arguments)]
pub fn mosaic_content_block_rects(
    src: &[u8],
    sw: u32,
    sh: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bs: u32,
) -> Vec<(i32, i32, i32, i32)> {
    let bsi = bs.max(1) as i32;
    let Some((gx0, gy0, gw, content)) = mosaic_content_grid(src, sw, sh, x0, y0, x1, y1, bsi)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, &has) in content.iter().enumerate() {
        if !has {
            continue;
        }
        let (i, j) = (idx % gw, idx / gw);
        let bx0 = gx0 + i as i32 * bsi;
        let by0 = gy0 + j as i32 * bsi;
        out.push((bx0, by0, bx0 + bsi, by0 + bsi));
    }
    out
}

/// 统计选区内**内容块**的数量，以及其中已被马赛克覆盖的数量 → `(总数, 已覆盖)`。
///
/// "内容块"用的是与打码同一套判定（块内有色差，或纯色但明显不是背景色），所以
/// "能被模糊的块"与"会被算进比例的块"永远是同一批，不会出现判定错位。
///
/// `stamps` 是已有的马赛克笔迹（**帧像素**坐标）；块中心落在任一 stamp 内即视为
/// 已覆盖。
#[allow(clippy::too_many_arguments)]
pub fn mosaic_content_coverage(
    src: &[u8],
    sw: u32,
    sh: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bs: u32,
    stamps: &[(DrawPoint, DrawPoint)],
) -> (usize, usize) {
    let bsi = bs.max(1) as i32;
    if x1 <= x0 || y1 <= y0 || sw == 0 || sh == 0 {
        return (0, 0);
    }
    let grid = mosaic_content_grid(src, sw, sh, x0, y0, x1, y1, bsi);
    let Some((gx0, gy0, gw, content)) = grid else {
        return (0, 0);
    };
    let mut total = 0usize;
    let mut covered = 0usize;
    for (idx, &has) in content.iter().enumerate() {
        if !has {
            continue;
        }
        total += 1;
        let (i, j) = (idx % gw, idx / gw);
        let bx0 = (gx0 + i as i32 * bsi) as f32;
        let by0 = (gy0 + j as i32 * bsi) as f32;
        let (bx1, by1) = (bx0 + bsi as f32, by0 + bsi as f32);
        // 块与笔迹**相交**即算"这块已经模糊过"（而不是要求块中心落在笔迹内）：
        // 用户第二次框选不可能和第一次严丝合缝，差几像素就判成"没模糊过"会让
        // 切换变得不可用 —— 明明看着已经糊了，再框一次却又糊一层。
        if stamps.iter().any(|(a, b)| {
            let (sx0, sx1) = (a.x.min(b.x), a.x.max(b.x));
            let (sy0, sy1) = (a.y.min(b.y), a.y.max(b.y));
            bx1 > sx0 && bx0 < sx1 && by1 > sy0 && by0 < sy1
        }) {
            covered += 1;
        }
    }
    (total, covered)
}

/// 选区内已有 ≥ [`MOSAIC_RESTORE_RATIO`]% 的内容被模糊 → 该还原成清晰。
///
/// 选区内一个内容块都没有（纯背景）时返回 `false` —— 没有东西可还原。
pub fn mosaic_should_restore(total: usize, covered: usize) -> bool {
    total > 0 && covered * 100 >= total * MOSAIC_RESTORE_RATIO
}

/// 笔迹的中心是否落在框内（用于"还原"时把框内的笔迹挖掉）。
///
/// 用中心而不是整体包含：画笔是一串方块的并集，按整体包含判断会让半个方块卡在
/// 框边的笔迹永远删不掉；按中心判断的结果与"视觉上框住了这块"一致。
pub fn mosaic_stamp_center_in_box(
    region: &(DrawPoint, DrawPoint),
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> bool {
    let (a, b) = region;
    let cx = (a.x + b.x) / 2.0;
    let cy = (a.y + b.y) / 2.0;
    cx >= x0.min(x1) && cx <= x0.max(x1) && cy >= y0.min(y1) && cy <= y0.max(y1)
}

/// 被涂抹区域的**背景色**：整片像素里出现最多的颜色（量化到 32 级后取众数）。
///
/// 用途是区分"空白背景"和"有内容的纯色块"：截图里背景通常占据多数像素，所以众数
/// 就是背景（白底页面→白、深色 UI→深色）。粗线条内部虽然也是纯色，但与背景色差得
/// 远，会被判成内容 —— 线条照样打码。
fn mosaic_dominant_color(
    src: &[u8],
    sw: u32,
    sh: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
) -> [u8; 3] {
    let (swi, shi) = (sw as i32, sh as i32);
    let bx0 = x0.clamp(0, swi);
    let by0 = y0.clamp(0, shi);
    let bx1 = x1.clamp(0, swi);
    let by1 = y1.clamp(0, shi);
    if bx1 <= bx0 || by1 <= by0 {
        return [0, 0, 0];
    }
    // 5 bit/通道 → 32768 个桶
    let mut hist = vec![0u32; 32768];
    for y in by0..by1 {
        let base = y as usize * sw as usize * 4;
        for x in bx0..bx1 {
            let o = base + x as usize * 4;
            let idx = ((src[o] as usize >> 3) << 10)
                | ((src[o + 1] as usize >> 3) << 5)
                | (src[o + 2] as usize >> 3);
            hist[idx] += 1;
        }
    }
    let best = hist
        .iter()
        .enumerate()
        .max_by_key(|(_, n)| **n)
        .map(|(i, _)| i)
        .unwrap_or(0);
    // 桶中心（+4 是半个桶宽）
    [
        (((best >> 10) & 31) as u8) << 3 | 4,
        (((best >> 5) & 31) as u8) << 3 | 4,
        ((best & 31) as u8) << 3 | 4,
    ]
}

/// 纯色块与背景色的最大差 ≤ 此值 → 当作背景（不打码）。
///
/// 留了余量：渐变/压缩噪点不会让背景被判成内容；而文字、线条与背景的差值通常在
/// 60 以上，不会被误判成背景。
const MOSAIC_BG_TOL: u8 = 24;

/// 块内像素与块均值的最大差 ≤ 此值 → 这块是"纯色块"（没有笔画/边缘等结构）。
///
/// 取 8：截图像素本身很干净，抗锯齿笔画与背景的差远超此值；而同一片纯色背景里
/// 的细微波动不会超过它。
const MOSAIC_FLAT_TOL: u8 = 8;

/// 预览用的**对齐裁剪**：抠出笔迹包围盒外扩一圈、且原点对齐到块网格的那块像素。
///
/// 为什么要对齐：块网格钉在 `bs` 的整数倍上（绝对坐标决定块相位）。裁剪原地错了，
/// 块相位就跟着错，预览与提交会对不上 —— 而且从代码里看不出来。这里有测试
/// （`mosaic_crop_does_not_change_block_grid`）钉住"在裁剪上算 == 在整帧上算"。
///
/// 为什么要裁剪：避免每帧为预览克隆整帧像素（1080p 8MB）。
///
/// 返回 `(裁剪像素, 裁剪宽, 裁剪高, 裁剪原点, 局部 regions)`；局部 regions 已平移到裁剪坐标系。
pub fn mosaic_aligned_crop(
    src: &[u8],
    sw: u32,
    sh: u32,
    regions: &[(DrawPoint, DrawPoint)],
    bs: u32,
) -> Option<(Vec<u8>, u32, u32, i32, i32, Vec<(DrawPoint, DrawPoint)>)> {
    if regions.is_empty() || sw == 0 || sh == 0 {
        return None;
    }
    let bsi = bs.max(1) as i32;
    let (mut bx0, mut by0, mut bx1, mut by1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for (a, b) in regions {
        bx0 = bx0.min(a.x.min(b.x).floor() as i32);
        by0 = by0.min(a.y.min(b.y).floor() as i32);
        bx1 = bx1.max((a.x.max(b.x)).ceil() as i32);
        by1 = by1.max((a.y.max(b.y)).ceil() as i32);
    }
    let (swi, shi) = (sw as i32, sh as i32);
    let cx0 = ((bx0 - bsi).div_euclid(bsi) * bsi).clamp(0, swi);
    let cy0 = ((by0 - bsi).div_euclid(bsi) * bsi).clamp(0, shi);
    let cx1 = ((((bx1 + bsi) + bsi - 1).div_euclid(bsi) * bsi)).clamp(0, swi);
    let cy1 = ((((by1 + bsi) + bsi - 1).div_euclid(bsi) * bsi)).clamp(0, shi);
    let (cw, ch) = (cx1 - cx0, cy1 - cy0);
    if cw <= 0 || ch <= 0 {
        return None;
    }
    let fwu = sw as usize;
    let mut crop = vec![0u8; (cw * ch * 4) as usize];
    for y in 0..ch as usize {
        let so = ((cy0 as usize + y) * fwu + cx0 as usize) * 4;
        let dst = y * cw as usize * 4;
        crop[dst..dst + cw as usize * 4]
            .copy_from_slice(&src[so..so + cw as usize * 4]);
    }
    let local: Vec<(DrawPoint, DrawPoint)> = regions
        .iter()
        .map(|(a, b)| {
            (
                DrawPoint::new(a.x - cx0 as f32, a.y - cy0 as f32),
                DrawPoint::new(b.x - cx0 as f32, b.y - cy0 as f32),
            )
        })
        .collect();
    Some((crop, cw as u32, ch as u32, cx0, cy0, local))
}

/// 马赛克色的半透明叠加量（色板颜色当滤镜用，见 [`render_mosaic_stroke_pixels`]）
pub const MOSAIC_TINT_ALPHA: u8 = 0x60;

/// **一笔马赛克的核心**：对 `regions` 覆盖到的像素做块平均像素化 + 颜色叠加，
/// 返回联合包围盒内的像素（RGBA，未覆盖处 alpha=0）。
///
/// 这是**预览与提交共用的同一份实现**，不是"两边各写一遍、靠约定保持一致"。
/// 用户看到的拖动效果就是松手后成图的那一次运算，所以不可能出现
/// "拖动中一个样、成型后又一个样"。
///
/// - 入参 `regions` 的单位是**物理像素**（调用方负责换算：提交路径用逻辑→物理的
///   缩放比换算，预览路径用 "画布坐标 × frame_dim / window_dim" 换算）。
/// - 块平均的值只由**绝对坐标**决定（网格钉在 `bs` 的整数倍），所以"先写半个块、
///   再被后一个 stamp 覆盖"与"一次算整块"结果相同；重叠 stamp 不会交叉涂抹。
/// - 未被覆盖的像素 alpha=0：预览层叠在帧图之上时，只有笔迹范围被替换。
pub fn render_mosaic_stroke_pixels(
    src: &[u8],
    sw: u32,
    sh: u32,
    regions: &[(DrawPoint, DrawPoint)],
    bs: u32,
    color: RGBA,
) -> Option<(Vec<u8>, i32, i32, u32, u32)> {
    if regions.is_empty() || sw == 0 || sh == 0 {
        return None;
    }
    let (swi, shi) = (sw as i32, sh as i32);
    // 联合包围盒：向外取整到整数像素，并裁到画面内
    let mut ux0 = i32::MAX;
    let mut uy0 = i32::MAX;
    let mut ux1 = i32::MIN;
    let mut uy1 = i32::MIN;
    for (a, b) in regions {
        ux0 = ux0.min(a.x.min(b.x).floor() as i32);
        uy0 = uy0.min(a.y.min(b.y).floor() as i32);
        ux1 = ux1.max((a.x.max(b.x)).ceil() as i32);
        uy1 = uy1.max((a.y.max(b.y)).ceil() as i32);
    }
    let ux0 = ux0.clamp(0, swi);
    let uy0 = uy0.clamp(0, shi);
    let ux1 = ux1.clamp(0, swi);
    let uy1 = uy1.clamp(0, shi);
    let (cw, ch) = ((ux1 - ux0).max(0) as u32, (uy1 - uy0).max(0) as u32);
    if cw == 0 || ch == 0 {
        return None;
    }
    // ① 覆盖掩码：哪些像素属于这一笔
    let mut inside = vec![false; (cw * ch) as usize];
    for (a, b) in regions {
        let x0 = (a.x.min(b.x).floor() as i32).max(ux0);
        let y0 = (a.y.min(b.y).floor() as i32).max(uy0);
        let x1 = (a.x.max(b.x).ceil() as i32).min(ux1);
        let y1 = (a.y.max(b.y).ceil() as i32).min(uy1);
        for y in y0.max(0)..y1.max(0) {
            if y < uy0 || y >= uy1 {
                continue;
            }
            let row = (y - uy0) as usize * cw as usize;
            for x in x0.max(0)..x1.max(0) {
                if x < ux0 || x >= ux1 {
                    continue;
                }
                inside[row + (x - ux0) as usize] = true;
            }
        }
    }
    // ② 整块平均（与提交路径同一函数、同一网格）+ 每块"有没有内容"
    let bsi = bs.max(1) as i32;
    let (avg, content) =
        mosaic_block_average_ex(src, sw, sh, ux0, uy0, ux1, uy1, bsi);
    if avg.len() != (cw * ch * 4) as usize {
        return None;
    }
    // 块网格原点（与 `mosaic_block_average_ex` 内部一致），用来把像素映射到块
    let gx0 = ux0.div_euclid(bsi) * bsi;
    let gy0 = uy0.div_euclid(bsi) * bsi;
    let gw = (((ux1 + bsi - 1).div_euclid(bsi) * bsi - gx0).max(0) / bsi) as usize;
    // 空背景块不打码：这些像素保持透明（预览）/不被改写（提交）
    // ③ 只把被覆盖的像素交出去，其余保持透明（预览层叠在帧图上，未覆盖处不该有东西）
    let mut out = vec![0u8; avg.len()];
    let tint = if color.a == 0 {
        RGBA::TRANSPARENT
    } else {
        RGBA::new(color.r, color.g, color.b, MOSAIC_TINT_ALPHA)
    };
    for i in 0..(cw * ch) as usize {
        if !inside[i] {
            continue;
        }
        // 跳过"没有内容"的块（空白背景）：用户要求只对文字、线条这类有效区域打码
        if !content.is_empty() && gw > 0 {
            let (x, y) = (i % cw as usize, i / cw as usize);
            let bi = ((ux0 + x as i32).div_euclid(bsi) * bsi - gx0) / bsi;
            let bj = ((uy0 + y as i32).div_euclid(bsi) * bsi - gy0) / bsi;
            if bi < 0 || bj < 0 {
                continue;
            }
            let (bi, bj) = (bi as usize, bj as usize);
            match content.get(bj * gw + bi) {
                Some(true) => {}
                _ => continue,
            }
        }
        let o = i * 4;
        out[o..o + 4].copy_from_slice(&avg[o..o + 4]);
        if tint.a > 0 {
            blend_pixel(&mut out[o..o + 4], tint);
        }
    }
    Some((out, ux0, uy0, cw, ch))
}

/// 把一条马赛克命令**整趟**写进 frame：块平均像素化 + 颜色叠加。
///
/// `regions` 是逻辑像素坐标（命令空间），这里按 `region_origin` 平移成帧局部坐标。
/// 实现直接复用 [`render_mosaic_stroke_pixels`] —— 与拖动预览**同一份代码**，
/// 所以"拖动中"和"最终成型"逐像素一致，不是靠两边各自小心维护。
fn apply_mosaic(
    frame: &mut CapturedFrame,
    region_origin_x: f32,
    region_origin_y: f32,
    regions: &[(DrawPoint, DrawPoint)],
    block_size: u32,
    color: RGBA,
) -> AppResult<()> {
    let shifted: Vec<(DrawPoint, DrawPoint)> = regions
        .iter()
        .map(|(a, b)| {
            let (ax, ay) = translate(*a, region_origin_x, region_origin_y);
            let (bx, by) = translate(*b, region_origin_x, region_origin_y);
            (DrawPoint::new(ax, ay), DrawPoint::new(bx, by))
        })
        .collect();
    let Some((pix, px, py, pw, ph)) = render_mosaic_stroke_pixels(
        &frame.pixels,
        frame.width,
        frame.height,
        &shifted,
        block_size,
        color,
    ) else {
        return Ok(());
    };
    let (w_px, h_px) = (frame.width as i32, frame.height as i32);
    for y in 0..ph as i32 {
        let ty = py + y;
        if ty < 0 || ty >= h_px {
            continue;
        }
        for x in 0..pw as i32 {
            let tx = px + x;
            if tx < 0 || tx >= w_px {
                continue;
            }
            let s_off = ((y * pw as i32 + x) * 4) as usize;
            if pix[s_off + 3] == 0 {
                continue; // 这一笔没覆盖到，保持原像素
            }
            let d_off = (ty as usize * w_px as usize + tx as usize) * 4;
            frame.pixels[d_off..d_off + 4].copy_from_slice(&pix[s_off..s_off + 4]);
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

    /// **一键模糊的判定**：框内"内容块"的统计与 ≥80% 还原阈值。
    ///
    /// 统计口径必须与打码口径一致（同一套内容块判定），否则会出现"看着已经全糊了，
    /// 但再框一次不还原"或者"只糊了一点点就还原了"。
    #[test]
    fn one_click_blur_counts_content_blocks_and_toggles_at_80_percent() {
        let (w, h) = (40u32, 20u32);
        let mut f = empty_frame(w, h);
        // 左半 4 块宽是"文字"，右半是纯背景
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let ink = x < 16 && x % 4 == 0;
                let v = if ink { 20 } else { 240 };
                for c in 0..3 {
                    f.pixels[i + c] = v;
                }
                f.pixels[i + 3] = 0xFF;
            }
        }
        let bs = 8u32;
        let box_ = (0i32, 0i32, 16i32, 16i32); // 只框住左半"文字"

        // ① 没有任何马赛克笔迹 → 内容块数 > 0，覆盖数 = 0 → 不还原（该模糊）
        let (total, covered) =
            mosaic_content_coverage(&f.pixels, w, h, box_.0, box_.1, box_.2, box_.3, bs, &[]);
        assert!(total > 0, "文字区应当能数出内容块");
        assert_eq!(covered, 0);
        assert!(!mosaic_should_restore(total, covered), "还没模糊过，不该还原");

        // ② 笔迹覆盖整框 → 覆盖率 100% → 该还原
        let full_stamp = vec![(
            DrawPoint::new(0.0, 0.0),
            DrawPoint::new(16.0, 16.0),
        )];
        let (total2, covered2) = mosaic_content_coverage(
            &f.pixels, w, h, box_.0, box_.1, box_.2, box_.3, bs, &full_stamp,
        );
        assert_eq!(total2, total);
        assert_eq!(covered2, total2, "整框被笔迹盖住 → 内容块应全部计入已覆盖");
        assert!(mosaic_should_restore(total2, covered2), "全覆盖 → 该还原");

        // ③ 只有零头被覆盖 → 不还原（继续模糊）
        let tiny_stamp = vec![(
            DrawPoint::new(0.0, 0.0),
            DrawPoint::new(8.0, 8.0),
        )];
        let (t3, c3) = mosaic_content_coverage(
            &f.pixels, w, h, box_.0, box_.1, box_.2, box_.3, bs, &tiny_stamp,
        );
        assert!(c3 * 100 < t3 * MOSAIC_RESTORE_RATIO, "零头覆盖不该触发还原");

        // ④ 纯背景框 → 没有内容块 → 不还原（也没得还原）
        let (t4, c4) =
            mosaic_content_coverage(&f.pixels, w, h, 24, 0, 40, 16, bs, &full_stamp);
        assert_eq!((t4, c4), (0, 0), "纯背景框不该数出内容块");
        assert!(!mosaic_should_restore(t4, c4));
    }

    /// **一键模糊是逐内容块建笔迹**：还原才能精确到块。
    ///
    /// 针对的 bug：原来用**一个大方块**盖住整框，还原时只能整块删掉 —— 于是
    /// "第一次框一大片、第二次框一小片"会把第一次的整大片一起还原。
    #[test]
    fn one_click_blur_builds_one_stamp_per_content_block() {
        let (w, h) = (40u32, 20u32);
        let mut f = empty_frame(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let ink = x < 16 && x % 4 == 0;
                let v = if ink { 20 } else { 240 };
                for c in 0..3 {
                    f.pixels[i + c] = v;
                }
                f.pixels[i + 3] = 0xFF;
            }
        }
        let bs = 8u32;
        let rects = mosaic_content_block_rects(&f.pixels, w, h, 0, 0, 16, 16, bs);
        assert!(!rects.is_empty(), "文字区应当有内容块");
        // 每块都是 bs×bs、落在绝对网格上（否则还原会错位）
        for (a, b, c, d) in &rects {
            assert_eq!(c - a, bs as i32);
            assert_eq!(d - b, bs as i32);
            assert_eq!(a.rem_euclid(bs as i32), 0, "块起点必须钉在 bs 整数倍上");
            assert_eq!(b.rem_euclid(bs as i32), 0);
        }
        // 纯背景区没有块 → 不会建笔迹
        assert!(
            mosaic_content_block_rects(&f.pixels, w, h, 24, 0, 40, 16, bs).is_empty(),
            "纯背景不该产出内容块"
        );
        // 与覆盖面统计口径一致：块数 = 内容块总数
        let (total, _) = mosaic_content_coverage(&f.pixels, w, h, 0, 0, 16, 16, bs, &[]);
        assert_eq!(rects.len(), total, "逐块笔迹数必须等于内容块总数（口径要一致）");
    }

    /// **小块还原不牵连大块**：一大片模糊里，只还原被小框框住的那几块。
    #[test]
    fn one_click_blur_small_box_restores_only_its_own_blocks() {
        let (w, h) = (48u32, 16u32);
        let mut f = empty_frame(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let ink = x % 4 == 0;
                let v = if ink { 20 } else { 240 };
                for c in 0..3 {
                    f.pixels[i + c] = v;
                }
                f.pixels[i + 3] = 0xFF;
            }
        }
        let bs = 8u32;
        // 第一次：一大片（整行文字）
        let big = mosaic_content_block_rects(&f.pixels, w, h, 0, 0, 48, 16, bs);
        assert!(big.len() > 6, "大片应当有很多块，实际 {}", big.len());
        // 第二次：只用小框框住最左边一块
        let small_box = (0.0f32, 0.0f32, 8.0f32, 16.0f32);
        let keep: Vec<_> = big
            .iter()
            .map(|(a, b, c, d)| {
                (
                    DrawPoint::new(*a as f32, *b as f32),
                    DrawPoint::new(*c as f32, *d as f32),
                )
            })
            .filter(|r| {
                // 与实现同一判定：笔迹中心落在框内 → 还原时删掉
                mosaic_stamp_center_in_box(r, small_box.0, small_box.1, small_box.2, small_box.3)
            })
            .collect();
        let removed = big.len() - (big.len() - keep.len());
        assert!(removed >= 1, "小框里应当至少有一块被还原");
        assert!(
            removed * 3 < big.len(),
            "小框只该还原极少几块，实际删了 {removed}/{} —— 大片被牵连还原了",
            big.len()
        );
    }

    /// **还原时按笔迹中心挖框**：框内的笔迹被移除，框外的不受影响。
    #[test]
    fn one_click_blur_restore_removes_only_stamps_inside_the_box() {
        let inside = (DrawPoint::new(10.0, 10.0), DrawPoint::new(30.0, 30.0)); // 中心 (20,20)
        let outside = (DrawPoint::new(100.0, 10.0), DrawPoint::new(120.0, 30.0));
        let straddling_edge = (DrawPoint::new(48.0, 10.0), DrawPoint::new(68.0, 30.0)); // 中心 (58,20)
        let bx = (0.0, 0.0, 50.0, 50.0);
        assert!(mosaic_stamp_center_in_box(&inside, bx.0, bx.1, bx.2, bx.3));
        assert!(!mosaic_stamp_center_in_box(&outside, bx.0, bx.1, bx.2, bx.3));
        assert!(!mosaic_stamp_center_in_box(
            &straddling_edge,
            bx.0,
            bx.1,
            bx.2,
            bx.3
        ));
    }

    /// **只对"有内容"的块打码**：同一笔同时扫过"文字区"和"纯空白区"，
    /// 文字区必须被糊掉，空白区必须**一个像素都不变**。
    ///
    /// 用户要求："只对文字、线条等有效区域能模糊，无效区域没有任何内容只有颜色的
    /// 不做模糊"。空白块被平均后仍是同一个颜色，打码没有任何遮挡收益，却会让笔迹
    /// 拖出一整条醒目的色带。
    #[test]
    fn mosaic_skips_empty_background_blocks() {
        let (w, h) = (48u32, 24u32);
        let mut f = empty_frame(w, h);
        // 左半：白底 + 每 4px 一条深色"笔画"；右半：纯白空白
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let ink = x < w / 2 && x % 4 == 0;
                let v = if ink { 20 } else { 240 };
                f.pixels[i] = v;
                f.pixels[i + 1] = v;
                f.pixels[i + 2] = v;
                f.pixels[i + 3] = 0xFF;
            }
        }
        let before = f.pixels.clone();

        // 一笔横扫整幅（两侧都被笔迹覆盖）
        apply_commands(
            &mut f,
            0.0,
            0.0,
            &[DrawCommand::Mosaic {
                regions: vec![(
                    DrawPoint::new(0.0, 0.0),
                    DrawPoint::new(w as f32, h as f32),
                )],
                block_size: 8,
                color: RGBA::TRANSPARENT, // 不叠色，只看有没有马赛克化
            }],
        )
        .unwrap();

        let at = |x: u32, y: u32| -> u8 { f.pixels[((y * w + x) * 4) as usize] };
        // 右半（纯空白）：必须保持原样
        let mut changed_bg = 0usize;
        for y in 0..h {
            for x in (w / 2 + 8)..w {
                let i = ((y * w + x) * 4) as usize;
                if f.pixels[i..i + 4] != before[i..i + 4] {
                    changed_bg += 1;
                }
            }
        }
        assert_eq!(changed_bg, 0, "空白背景被改动了 {changed_bg} 个像素 —— 不该对空白打码");

        // 左半（文字）：必须已经没有原文墨色（20）
        let mut ink_left = 0usize;
        for y in 0..h {
            for x in 0..(w / 2 - 8) {
                if at(x, y) == 20 {
                    ink_left += 1;
                }
            }
        }
        assert_eq!(ink_left, 0, "文字区还有 {ink_left} 个墨色像素 —— 该打码的没打");
    }

    #[test]
    fn block_average_mosaic_hides_content_in_one_pass() {
        // 这个测试原来断言"红点的原值被搬到整块"——那是**点采样**（每块只取左上角
        // 一个像素）的行为，也正是"要反复涂抹才能遮挡"的原因：内容只是被搬了个
        // 位置，一个像素都没少。改成整块取平均后该断言必然失败，所以按新契约重写。
        let (w, h) = (40u32, 40u32);
        let mut f = empty_frame(w, h);
        // 造"文字"：每 4px 一条深色竖线（比块细，块平均必须把它糊掉）
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let dark = x % 4 == 0;
                let v = if dark { 20 } else { 240 };
                f.pixels[i] = v;
                f.pixels[i + 1] = v;
                f.pixels[i + 2] = v;
                f.pixels[i + 3] = 0xFF;
            }
        }
        let before = f.pixels.clone();

        let bs = 10u32;
        let cmd = DrawCommand::Mosaic {
            regions: vec![(
                DrawPoint::new(0.0, 0.0),
                DrawPoint::new(w as f32, h as f32),
            )],
            block_size: bs,
            color: RGBA::TRANSPARENT, // 不染色，单看马赛克本身
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();

        // ① 块内必须同色（整块平均的机械证据）
        for by in (0..h as i32).step_by(bs as usize) {
            for bx in (0..w as i32).step_by(bs as usize) {
                let at = |x: i32, y: i32| {
                    let i = ((y as u32 * w + x as u32) * 4) as usize;
                    [f.pixels[i], f.pixels[i + 1], f.pixels[i + 2]]
                };
                let a = at(bx, by);
                let b = at(bx + bs as i32 - 1, by + bs as i32 - 1);
                assert_eq!(a, b, "块 ({bx},{by}) 内应为一个纯色，got {a:?} vs {b:?}");
            }
        }

        // ② **一笔就必须把原内容抹掉**：4px 竖线在块内被平均成中间灰，
        //    原来的深/浅对比不能再出现在块图里。
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            f.pixels[i]
        };
        let mut extremes = 0;
        for by in 0..h {
            for bx in 0..w {
                if at(bx, by) < 60 || at(bx, by) > 200 {
                    extremes += 1;
                }
            }
        }
        assert_eq!(
            extremes, 0,
            "马赛克后仍有 {extremes} 个像素保持原来的深浅极端值 —— 内容没被抹掉"
        );
        // 而且确实改过像素（不是"看着有笔迹、其实没落到像素上"）
        let changed = (0..f.pixels.len()).step_by(4).filter(|&i| f.pixels[i] != before[i]).count();
        assert!(changed > (w * h) as usize / 2, "绝大多数像素应当被改过，实际 {changed}");

        // ③ 块均值必须**就是整块的平均**（拿块内的原像素直接算出来对照，
        //    而不是我手写一个算术——上面那版手算就写错了，被这条断言抓住）。
        let (bx, by) = (0usize, 0usize);
        let mut sum = 0u32;
        let mut n = 0u32;
        for y in by..by + bs as usize {
            for x in bx..bx + bs as usize {
                sum += u32::from(before[(y * w as usize + x) * 4]);
                n += 1;
            }
        }
        let expect = ((sum + n / 2) / n) as i32;
        let got = at(5, 5) as i32;
        assert_eq!(got, expect, "块均值应当等于整块平均");
        // ④ `RGBA::TRANSPARENT` 必须表示**不染色**：只做马赛克，不能顺手压暗画面。
        //    （回归：把"一律按 0x60 半透明叠加"写成无条件规则时，透明黑也会叠一层，
        //    整幅图被压暗 —— 用户只想马赛克时没法关掉染色。）
        assert_eq!(got, expect, "传 TRANSPARENT 时不应发生任何染色");
    }

    #[test]
    fn mosaic_crop_does_not_change_block_grid() {
        // 预览为了不克隆整帧，只把笔迹包围盒**外扩一圈并按块对齐**的一小块喂给核心。
        // 这一步一旦错位（裁剪原点不按块对齐 / 平移算错），块网格相位就变了 ——
        // 预览与提交会悄悄对不上，而且看代码看不出来。这条测试把它钉死：
        // 在整帧上算，与在"对齐裁剪"上算，结果必须逐像素相同。
        use super::render_mosaic_stroke_pixels;
        use crate::overlay::drawing::Point as P;

        let (fw, fh) = (200u32, 120u32);
        let mut frame = empty_frame(fw, fh);
        // 有结构的图：棋盘 + 几个色块，块均值才有区分度
        for y in 0..fh {
            for x in 0..fw {
                let i = ((y * fw + x) * 4) as usize;
                let v = if (x / 3 + y / 5) % 2 == 0 { 40 } else { 210 };
                frame.pixels[i] = v;
                frame.pixels[i + 1] = (v as u16 * 2 / 3) as u8;
                frame.pixels[i + 2] = 255 - v;
                frame.pixels[i + 3] = 255;
            }
        }
        let bs = 12u32;
        let regions = vec![
            (P::new(37.0, 41.0), P::new(85.0, 89.0)),
            (P::new(70.0, 41.0), P::new(118.0, 89.0)),
            (P::new(103.0, 41.0), P::new(151.0, 89.0)),
            (P::new(136.0, 41.0), P::new(184.0, 89.0)),
        ];
        let color = RGBA::new(0xE6, 0x22, 0x22, 0xFF);

        // ① 整帧直接算
        let (full, fx, fy, fwid, fhei) =
            render_mosaic_stroke_pixels(&frame.pixels, fw, fh, &regions, bs, color)
                .expect("整帧渲染应当成功");

        // ② 按预览的**生产路径**裁剪（mosaic_aligned_crop，不是测试里另抄一遍）
        let (crop, cw, ch, cx0, cy0, local) =
            super::mosaic_aligned_crop(&frame.pixels, fw, fh, &regions, bs)
                .expect("对齐裁剪应当成功");
        let (cropped, px, py, pw, ph) = render_mosaic_stroke_pixels(
            &crop,
            cw,
            ch,
            &local,
            bs,
            color,
        )
        .expect("裁剪渲染应当成功");

        assert_eq!(
            (fx, fy, fwid, fhei),
            (cx0 + px, cy0 + py, pw, ph),
            "裁剪渲染的包围盒换算回整帧坐标后应当一致"
        );
        // ③ 逐像素比对：换算到整帧坐标系后必须完全相同
        let mut diff = 0usize;
        let mut checked = 0usize;
        for y in 0..ph as i32 {
            for x in 0..pw as i32 {
                let fo = ((y * fwid as i32 + x) * 4) as usize;
                let co = ((y * pw as i32 + x) * 4) as usize;
                checked += 1;
                if full[fo..fo + 4] != cropped[co..co + 4] {
                    diff += 1;
                }
            }
        }
        assert!(checked > 1000, "比对像素太少：{checked}");
        assert_eq!(
            diff, 0,
            "裁剪后有 {diff}/{checked} 个像素与整帧渲染不同 —— 裁剪破坏了块网格相位"
        );
    }

    #[test]
    fn arrow_lw1_head_visible_and_large_enough() {
        // 回归：1px 线宽的箭头头必须清晰可见（曾因 head_len=7/head_w=2
        // 太小 + 尖端被 AA 削平，看起来"没有箭头"）。
        let mut f = empty_frame(80, 40);
        let cmd = DrawCommand::Arrow {
            from: DrawPoint::new(4.0, 20.0),
            to: DrawPoint::new(64.0, 20.0),
            color: RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            line_width: 1.0,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();
        // 尖端附近 (63,20) 至少部分覆盖（外扩后尖角不再全透明）
        let idx = (20 * 80 + 63) * 4;
        assert!(f.pixels[idx + 3] > 50, "near-tip alpha = {}", f.pixels[idx + 3]);
        // 三角形内部 (60,20) 实心
        let idx = (20 * 80 + 60) * 4;
        assert!(f.pixels[idx + 3] > 200, "head interior alpha = {}", f.pixels[idx + 3]);
        // 头底边区域 (56,18)/(56,22) 应被三角形覆盖
        let idx = (18 * 80 + 56) * 4;
        assert!(f.pixels[idx + 3] > 150, "head base upper alpha = {}", f.pixels[idx + 3]);
        // 头尺寸：沿 x 的头跨度 >= 8px（head_len 下限 10 减去 AA 边距），
        // 保证 1px 线箭头头醒目
        let mut tip_x = 0;
        for x in (50..67).rev() {
            let idx = (20 * 80 + x) * 4;
            if f.pixels[idx + 3] > 50 { tip_x = x; break; }
        }
        let mut base_x = 0;
        for x in 50..67 {
            let idx = (20 * 80 + x) * 4;
            if f.pixels[idx + 3] > 50 { base_x = x; break; }
        }
        assert!(tip_x - base_x >= 8, "head length too small: {}px", tip_x - base_x);
    }

    #[test]
    fn arrow_short_head_does_not_reverse_past_start() {
        // 短箭头：head_len 上限 = 线长 70%，底边不得越过起点（主线不反向）
        let mut f = empty_frame(40, 24);
        let cmd = DrawCommand::Arrow {
            from: DrawPoint::new(4.0, 12.0),
            to: DrawPoint::new(16.0, 12.0), // 线长 12
            color: RGBA::new(0xFF, 0x00, 0x00, 0xFF),
            line_width: 1.0,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();
        // 起点 (4,12) 附近不应有反向延伸的主线像素：起点左侧 2px 透明
        //（起点 Full 圆帽半径 0.5 + AA 带 1.0 只覆盖到 1.5px，2px 处应无像素）
        let idx = (12 * 40 + 2) * 4;
        assert_eq!(f.pixels[idx + 3], 0, "pixels left of start: alpha = {}", f.pixels[idx + 3]);
        // 起点处主线仍存在
        let idx = (12 * 40 + 4) * 4;
        assert!(f.pixels[idx + 3] > 100, "start alpha = {}", f.pixels[idx + 3]);
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
                &mut f, (0.0, 0.0), (0.0, 0.0), content, 24.0, RGBA::RED, None, FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
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
            &mut f, (0.0, 0.0), (0.0, 0.0), "", 16.0, RGBA::RED, None, FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
        )
        .unwrap();
        assert_eq!(f.pixels, baseline, "空 content 不能改 frame");
    }

    #[test]
    fn rasterize_text_out_of_frame_anchor_does_not_panic() {
        let mut f = empty_frame(20, 20);
        rasterize_text(
            &mut f, (-100.0, -100.0), (-100.0, -100.0), "test", 16.0, RGBA::RED, None, FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
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
            FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
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
            FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
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
            &mut normal, (10.0, 10.0), (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Normal, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
        )
        .unwrap();
        rasterize_text(
            &mut bold, (10.0, 10.0), (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Bold, 0.0, RGBA::TRANSPARENT, (0.0, 0.0), 0.0,
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
