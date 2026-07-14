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
//! - Text：v0.1 暂画一个半透明色块作为占位（不接字体，避免引入 TTF 子集依赖）；
//!   v0.2 接入 Noto Sans CJK SC 子集后再做真正的文字光栅化

use crate::capture::CapturedFrame;
use crate::error::{AppError, AppResult};
use crate::overlay::drawing::{DrawCommand, Point as DrawPoint, RGBA};
use image::imageops::FilterType;

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
    for cmd in commands {
        match cmd {
            DrawCommand::Rectangle { rect, color, line_width } => {
                let a = translate(rect.0, region_origin_x, region_origin_y);
                let b = translate(rect.1, region_origin_x, region_origin_y);
                let (x1, y1, x2, y2) = normalize_rect(a, b);
                draw_rect_outline(frame, x1, y1, x2, y2, *line_width, *color)?;
            }
            DrawCommand::Arrow { from, to, color, line_width } => {
                let f = translate(*from, region_origin_x, region_origin_y);
                let t = translate(*to, region_origin_x, region_origin_y);
                draw_thick_line(frame, f.0, f.1, t.0, t.1, *line_width, *color)?;
                // 箭头三角
                let dx = t.0 - f.0;
                let dy = t.1 - f.1;
                let len = (dx * dx + dy * dy).sqrt();
                if len >= 1.0 {
                    let ux = dx / len;
                    let uy = dy / len;
                    let head_len = (line_width * 6.0).max(8.0);
                    let head_w = (line_width * 3.0).max(4.0);
                    let bx = t.0 - ux * head_len;
                    let by = t.1 - uy * head_len;
                    let px = -uy;
                    let py = ux;
                    let p1 = (bx + px * head_w, by + py * head_w);
                    let p2 = (bx - px * head_w, by - py * head_w);
                    draw_thick_line(frame, t.0, t.1, p1.0, p1.1, *line_width, *color)?;
                    draw_thick_line(frame, t.0, t.1, p2.0, p2.1, *line_width, *color)?;
                    draw_thick_line(frame, p1.0, p1.1, p2.0, p2.1, *line_width, *color)?;
                }
            }
            DrawCommand::Freehand { points, color, line_width } => {
                for w in points.windows(2) {
                    let p1 = translate(w[0], region_origin_x, region_origin_y);
                    let p2 = translate(w[1], region_origin_x, region_origin_y);
                    draw_thick_line(frame, p1.0, p1.1, p2.0, p2.1, *line_width, *color)?;
                }
            }
            DrawCommand::Text { anchor, content: _, font_size, color } => {
                // v0.1 简化：用半透明色块占位（v0.2 接 ab_glyph + Noto CJK）
                let a = translate(*anchor, region_origin_x, region_origin_y);
                let char_w = font_size * 0.6;
                // 按字体平均字符宽度估算；content 长度未知，暂用 4 字符宽度避免越界
                let w = char_w * 4.0;
                let h = *font_size;
                let placeholder = RGBA::new(color.r, color.g, color.b, (color.a / 2).max(0x40));
                fill_rect_blend(frame, a.0, a.1, w, h, placeholder)?;
            }
            DrawCommand::Mosaic { rect, block_size } => {
                let a = translate(rect.0, region_origin_x, region_origin_y);
                let b = translate(rect.1, region_origin_x, region_origin_y);
                let (x1, y1, x2, y2) = normalize_rect(a, b);
                apply_mosaic(frame, x1, y1, x2, y2, *block_size)?;
            }
        }
    }
    Ok(())
}

/// 把屏幕坐标的命令点平移到 frame 局部坐标
fn translate(p: DrawPoint, ox: f32, oy: f32) -> (f32, f32) {
    (p.x - ox, p.y - oy)
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

/// 画一条指定粗细的实线（轴对齐 bounding box）
fn draw_thick_line(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    lw: f32,
    color: RGBA,
) -> AppResult<()> {
    let half = (lw / 2.0).max(0.5);
    let min_x = x1.min(x2) - half;
    let min_y = y1.min(y2) - half;
    let max_x = x1.max(x2) + half;
    let max_y = y1.max(y2) + half;
    fill_rect_blend(frame, min_x, min_y, max_x - min_x, max_y - min_y, color)
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
    draw_thick_line(frame, x1, y1, x2, y1, lw, color)?;
    draw_thick_line(frame, x1, y2, x2, y2, lw, color)?;
    draw_thick_line(frame, x1, y1, x1, y2, lw, color)?;
    draw_thick_line(frame, x2, y1, x2, y2, lw, color)?;
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
        dst[i] = ((s * 255 + d * inv) / 255) as u8;
    }
    // alpha 通道：直接覆盖（标量源模式足够好）
    dst[3] = src.a.max(dst[3]);
}

/// 对 frame 中 (x1, y1) - (x2, y2) 区域做马赛克
///
/// 1. 把原区域 resize 到 (w/block_size, h/block_size)（nearest-neighbor）
/// 2. 再 resize 回原尺寸
/// 3. 写回 frame 对应区域
fn apply_mosaic(
    frame: &mut CapturedFrame,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    block_size: u32,
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
            // 取原区域 (sx*bs, sy*bs) 处的代表像素
            let src_x = (rx + sx * bs).clamp(0, w_px - 1);
            let src_y = (ry + sy * bs).clamp(0, h_px - 1);
            let src_idx = ((src_y * w_px + src_x) as usize) * 4;
            let dst_idx = ((sy * small_w + sx) as usize) * 4;
            small[dst_idx..dst_idx + 4].copy_from_slice(&frame.pixels[src_idx..src_idx + 4]);
        }
    }

    // 2) 用 imageops 把 small buffer 视为 RGBA8 图像，先压缩再放大
    //    用两次 nearest 滤镜实现 mosaic
    let mid_w = small_w;
    let mid_h = small_h;
    // small buffer → image::ImageBuffer
    let img_small = image::RgbaImage::from_raw(mid_w as u32, mid_h as u32, small)
        .ok_or_else(|| AppError::Window("mosaic 创建 ImageBuffer 失败".into()))?;
    // nearest 放大回原尺寸
    let img_big = image::imageops::resize(&img_small, rw as u32, rh as u32, FilterType::Nearest);

    // 3) 写回 frame
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
        // 中点 (10, 10) 应该是绿色
        let idx_mid = (10 * 20 + 10) * 4;
        assert!(
            f.pixels[idx_mid + 1] > 200,
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
            rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(20.0, 20.0)),
            block_size: 10,
        };
        apply_commands(&mut f, 0.0, 0.0, &[cmd]).unwrap();

        // mosaic with block_size=10 → small buffer 2x2
        // small[1,1] samples src (10, 10) → red；其余采样 (0,0)/(10,0)/(0,10) 都是 0
        // nearest upscale 到 20x20 → (10..20, 10..20) 区域全红
        let idx = (15 * 20 + 15) * 4;
        assert!(
            f.pixels[idx] > 200,
            "mosaic should propagate red to (15,15), got r={}",
            f.pixels[idx]
        );
        // (0..10, 0..10) 区域应该还是 0
        let idx = (5 * 20 + 5) * 4;
        assert_eq!(f.pixels[idx], 0, "r at (5,5) should be 0, got {}", f.pixels[idx]);
    }
}
