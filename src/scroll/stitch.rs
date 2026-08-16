//! 滚动截屏拼接：重叠检测（纯函数，可单测）。
//!
//! 连续两次抓取同一屏幕区域 a、b（内容向下滚动）。向下滚动 s 行意味着
//! 内容上移 s 行，因此 b 的顶部 (h-s) 行等于 a 的底部 (h-s) 行（重叠带），
//! b 的底部 s 行是新进入视口的内容。`find_scroll_delta` 负责找出 s。

use std::cell::RefCell;

use crate::capture::CapturedFrame;

/// 单行签名采样列数
const SAMPLE_COLS: usize = 16;
/// 采样时跳过的右侧列带比例（进度窗兜底污染时用）
const SKIP_RIGHT_RATIO: usize = 8;
/// 判定重叠带“有内容”的相邻行签名差阈值（签名是采样列 RGB 之和）
const MIN_ENERGY: u64 = 8;
/// 滚动量上限（超出视为异常，拒绝）
const MAX_SCROLL: usize = 800;
/// 要求重叠带至少保留的行数（太少则无法可靠判定）
const MIN_OVERLAP: usize = 30;
/// 逐像素验证的容差（每通道）
const PIXEL_TOLERANCE: u8 = 50;

/// `find_scroll_delta` 的可复用中间缓冲：行签名 / 采样列 / 逐像素验证列。
///
/// 滚动循环每轮会调用 `find_scroll_delta` 1~3 次，复用缓冲区可避免反复
/// malloc/free（每帧约 2×h 个 u64 签名 + 采样列）。
struct StitchScratch {
    cols: Vec<usize>,
    vcols: Vec<usize>,
    sig_a: Vec<u64>,
    sig_b: Vec<u64>,
}

impl Default for StitchScratch {
    fn default() -> Self {
        Self {
            cols: Vec::with_capacity(SAMPLE_COLS),
            vcols: Vec::with_capacity(8),
            sig_a: Vec::new(),
            sig_b: Vec::new(),
        }
    }
}

impl StitchScratch {
    fn find_scroll_delta(&mut self, a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
        if a.width != b.width || a.height != b.height {
            return None;
        }
        let w = a.width as usize;
        let h = a.height as usize;
        if w == 0 || h == 0 {
            return None;
        }

        fill_sample_cols(w, &mut self.cols);
        fill_row_signatures(a, w, h, &self.cols, &mut self.sig_a);
        fill_row_signatures(b, w, h, &self.cols, &mut self.sig_b);
        fill_vcols(w, &mut self.vcols);

        let max_s = h.saturating_sub(MIN_OVERLAP).min(MAX_SCROLL);
        if max_s == 0 {
            return None;
        }

        // 对每个候选滚动量 s 打分：重叠带逐行签名差之和，**除以比较行数**
        // 归一化为每行平均差，消除大 s（少比较行 → 总分天然低）的偏袒。
        // 只保留平均差最小的 3 个候选（无需全量排序，也无需分配候选向量）。
        let mut top: [(u64, usize); 3] = [(u64::MAX, 0); 3];
        for s in 1..=max_s {
            // 比较 a[s..h] 与 b[0..h-s]
            let mut score = 0u64;
            let n = h - s;
            for r in 0..n {
                score += self.sig_a[s + r].abs_diff(self.sig_b[r]);
            }
            let avg = score / n as u64;
            // 插入排序维护 top-3（平均分升序）
            if avg < top[2].0 {
                let mut i = 2;
                while i > 0 && avg < top[i - 1].0 {
                    top[i] = top[i - 1];
                    i -= 1;
                }
                top[i] = (avg, s);
            }
        }
        // 分数升序，取前 3 个候选逐个做像素验证
        for (avg, s) in top {
            if avg == u64::MAX {
                break;
            }
            if band_has_energy(&self.sig_a, s, h) && verify_rows(a, b, s, w, &self.vcols) {
                return Some(s);
            }
        }
        None
    }
}

thread_local! {
    static SCRATCH: RefCell<StitchScratch> = RefCell::new(StitchScratch::default());
}

/// 返回 b 相对 a 向下滚动的行数 s。
///
/// 若内容没动 / 无法可靠判定（空白、歧义、匹配不上）→ `None`。
pub fn find_scroll_delta(a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
    SCRATCH.with(|s| s.borrow_mut().find_scroll_delta(a, b))
}

/// 均匀采样列索引，跳过最右侧 1/8 列带（进度窗兜底污染）
fn fill_sample_cols(w: usize, out: &mut Vec<usize>) {
    let usable_w = w.saturating_sub(w / SKIP_RIGHT_RATIO).max(1);
    let k = SAMPLE_COLS.min(usable_w);
    out.clear();
    out.extend((0..k).map(|i| usable_w * i / k.max(1)));
}

/// 逐像素验证采样列（8 列均匀分布）
fn fill_vcols(w: usize, out: &mut Vec<usize>) {
    out.clear();
    out.extend((0..8).map(|i| w * (i + 1) / 9));
}

/// 每行签名 = 采样列 RGB 之和（丢弃 alpha，通常恒为 255）。写入复用缓冲。
fn fill_row_signatures(f: &CapturedFrame, w: usize, h: usize, cols: &[usize], sig: &mut Vec<u64>) {
    sig.clear();
    sig.reserve(h);
    let px = &f.pixels;
    for r in 0..h {
        let mut acc = 0u64;
        for &c in cols {
            let p = (r * w + c) * 4;
            acc += px[p] as u64 + px[p + 1] as u64 + px[p + 2] as u64;
        }
        sig.push(acc);
    }
}

/// 重叠带（a 的 s..h 行）必须有内容：相邻行签名差的均值过低说明整带均匀，
/// 此时任何偏移都能“匹配”，无法可靠判定。
///
/// 但「均值过低」对**大部分空白/平滑、夹一条窄纹理带**的帧会误判：空白行把均值
/// 稀释到阈值以下，而那条纹理带其实是能钉住偏移的（空白行任意偏移都匹配，只有
/// 纹理带能区分真实滚动量）。因此除了均值判定，还要看带内**有纹理的行数**——
/// 存在可观纹理行（≥1/8 行相邻差达标）就仍可判定，否则才整带均匀拒绝。
fn band_has_energy(sig_a: &[u64], s: usize, h: usize) -> bool {
    let n = h - s;
    if n < 2 {
        return false;
    }
    let mut total = 0u64;
    let mut textured = 0u32;
    for r in (s + 1)..h {
        let d = sig_a[r].abs_diff(sig_a[r - 1]);
        total += d;
        if d >= MIN_ENERGY {
            textured += 1;
        }
    }
    total / n as u64 >= MIN_ENERGY || textured * 8 >= n as u32
}

/// 对候选滚动量 s 做抽样验证：重叠带内取 16 行 × 8 列，比较局部 3×3 平均 RGB。
///
/// 用局部平均而非逐像素：平滑滚动中间帧有亚像素偏移，抗锯齿让同一行像素在
/// 两次抓帧间偏差过大，逐像素比较会使**真实**滚动量验证失败，find_scroll_delta
/// 就退回自相似的更大假偏移 → 拼接段与上一段重叠（下一页头重复上一页尾）。
/// 3×3 平均对 ±1px 的亚像素偏移不敏感，真实滚动量（评分最低、最优先返回）能
/// 通过验证。
///
/// 采样行从 5 加到 16：平均比较比逐像素宽松，采样点少时自相似内容会在个别点
/// 巧合匹配（静态帧误判为滚动 → 重复拼接）；多采样行让这种巧合在整个重叠带
/// 同时成立的概率趋近于零，静态帧仍返回 None。
fn verify_rows(a: &CapturedFrame, b: &CapturedFrame, s: usize, w: usize, vcols: &[usize]) -> bool {
    let h = a.height as usize;
    let n = h - s; // 重叠行数
    if n < 8 {
        return false;
    }
    // 15 等分采样行（n/16..15n/16），避开重叠带两端：顶部 r=0 的 3×3 会混入
    // a 更早的行，底部 r=n-1 的 3×3 会混入 b 新增内容，都会误判为不匹配
    let rows = [
        n * 1 / 16,
        n * 2 / 16,
        n * 3 / 16,
        n * 4 / 16,
        n * 5 / 16,
        n * 6 / 16,
        n * 7 / 16,
        n * 8 / 16,
        n * 9 / 16,
        n * 10 / 16,
        n * 11 / 16,
        n * 12 / 16,
        n * 13 / 16,
        n * 14 / 16,
        n * 15 / 16,
    ];
    let mut passed = 0u32;
    let mut total = 0u32;
    for &r in &rows {
        let ar = s + r; // a 侧行
        let br = r; // b 侧行
        let mut row_ok = true;
        for &c in vcols {
            for ch in 0..3 {
                let da = box_avg(a, ar, c, w, h, ch);
                let db = box_avg(b, br, c, w, h, ch);
                if da.abs_diff(db) > PIXEL_TOLERANCE {
                    row_ok = false;
                }
            }
        }
        if row_ok {
            passed += 1;
        }
        total += 1;
    }
    // 真实滚动整体对齐，个别采样行会因边缘/噪声/亚像素小幅超标而失败；自相似假
    // 匹配在大多数采样行都不对齐。要求 ≥80% 采样行通过即可，既容忍噪声又排除假匹配。
    passed * 5 >= total * 4
}

/// (row, col) 处 3×3 邻域的某通道平均值（越界夹取到图像边缘）。
fn box_avg(f: &CapturedFrame, row: usize, col: usize, w: usize, h: usize, ch: usize) -> u8 {
    let mut sum = 0u64;
    let mut n = 0u64;
    let r0 = row.saturating_sub(1);
    let r1 = (row + 1).min(h - 1);
    let c0 = col.saturating_sub(1);
    let c1 = (col + 1).min(w - 1);
    for rr in r0..=r1 {
        for cc in c0..=c1 {
            let p = (rr * w + cc) * 4 + ch;
            sum += f.pixels[p] as u64;
            n += 1;
        }
    }
    (sum / n) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行号 → 非周期像素值（用乘法定数散列，避免模 256 造成的周期性误匹配）
    fn row_val(row: usize) -> u8 {
        let x = row.wrapping_mul(2654435761);
        ((x >> 16) ^ (x >> 8) ^ x) as u8
    }

    /// 造一帧 h 行、每行有区分度的图像（水平无变化，垂直有变化）
    fn frame(w: usize, h: usize) -> CapturedFrame {
        let mut pixels = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let v = row_val(row);
            for _ in 0..w {
                pixels.push(v);
                pixels.push(v.wrapping_add(37));
                pixels.push(v.wrapping_mul(3));
                pixels.push(255);
            }
        }
        CapturedFrame {
            width: w as u32,
            height: h as u32,
            pixels,
        }
    }

    #[test]
    fn detects_scroll_delta() {
        // b = a 向下滚动 120 行：b 顶部内容 = a 的 [120..]，b 底部是“新内容”
        let a = frame(200, 600);
        let mut b = frame(200, 600);
        let w = 200;
        for row in 0..600 {
            // 顶部 480 行沿用 a 的 [120..]；底部 120 行是新内容（用更大行号保证不同）
            let src = if row + 120 < 600 { row + 120 } else { 600 + row };
            let v = row_val(src);
            for c in 0..w {
                let p = (row * w + c) * 4;
                b.pixels[p] = v;
                b.pixels[p + 1] = v.wrapping_add(37);
                b.pixels[p + 2] = v.wrapping_mul(3);
                b.pixels[p + 3] = 255;
            }
        }
        assert_eq!(find_scroll_delta(&a, &b), Some(120));
    }

    #[test]
    fn identical_frames_return_none() {
        let a = frame(200, 600);
        let b = frame(200, 600);
        assert_eq!(find_scroll_delta(&a, &b), None);
    }

    #[test]
    fn uniform_band_rejected() {
        // 全白内容：任何偏移都能匹配 → 应拒绝
        let mut a = frame(200, 600);
        for px in a.pixels.iter_mut() {
            *px = 255;
        }
        let mut b = frame(200, 600);
        for px in b.pixels.iter_mut() {
            *px = 255;
        }
        assert_eq!(find_scroll_delta(&a, &b), None);
    }

    #[test]
    fn mismatched_dims_return_none() {
        let a = frame(200, 600);
        let b = frame(200, 300);
        assert_eq!(find_scroll_delta(&a, &b), None);
    }

    /// 平滑滚动中间帧有亚像素偏移：b 每行是 a 相邻两行的 50/50 混合（抗锯齿）。
    /// 逐像素验证会被这种模糊击穿（相邻行值差异大时混合值与任一行偏差超容差）
    /// → 真实偏移检测失败 → 退回自相似假偏移 → 拼接重复。修复后（3×3 局部平均
    /// 验证）必须仍返回真实偏移 120。
    ///
    /// 三通道都用行号哈希 v，且不含 +37/×3 这类会被 u8 回绕破坏「混合-逐值」一致
    /// 性的变化；保证 3×3 平均对真实偏移的偏差恒 < 50：平均差 = |v(行+3)-v(行)|/6
    /// ≤ 255/6 ≈ 42。
    #[test]
    fn detects_scroll_delta_with_subpixel_blur() {
        let w = 200usize;
        let h = 600usize;
        let mk = |pixels: &mut Vec<u8>, w: usize, row: usize, v: u8| {
            for c in 0..w {
                let p = (row * w + c) * 4;
                pixels[p] = v;
                pixels[p + 1] = v;
                pixels[p + 2] = v;
                pixels[p + 3] = 255;
            }
        };
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            mk(&mut ap, w, row, row_val(row));
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            // 顶部 480 行 = a[120..] 的相邻行混合；底部 120 行新内容（更大行号）
            let src: usize = if row + 120 < h { row + 120 } else { h + row };
            let v0 = row_val(src);
            let v1 = row_val(src + 1);
            let v = (v0 as u16 + v1 as u16) / 2; // 亚像素模糊：两行各半
            mk(&mut bp, w, row, v as u8);
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        // 亚像素模糊使真实偏移 120 与 121 的评分几乎相同（±1 行歧义），二者都合理；
        // 关键是**不能**返回更大假偏移（如把 30 检测成 280 → 拼接重复）。
        match find_scroll_delta(&a, &b) {
            Some(s) if s == 120 || s == 121 => {}
            other => panic!("expected Some(120|121), got {other:?}"),
        }
    }

    /// 大部分空白、中间夹一条纹理带的帧：空白行稀释了重叠带平均能量，但纹理带
    /// 能钉住偏移。band_has_energy 修复后必须仍能测出真实滚动量。
    #[test]
    fn detects_scroll_delta_with_sparse_texture() {
        let w = 1261usize;
        let h = 312usize;
        let mk = |textured: std::ops::Range<usize>| {
            let mut px = vec![0u8; w * h * 4];
            for row in textured {
                let v = row_val(row);
                for x in 0..w {
                    let p = (row * w + x) * 4;
                    px[p] = v;
                    px[p + 1] = v.wrapping_add(37);
                    px[p + 2] = v.wrapping_mul(3);
                    px[p + 3] = 255;
                }
            }
            CapturedFrame { width: w as u32, height: h as u32, pixels: px }
        };
        // a：行 100..220 有纹理，其余空白
        let a = mk(100..220);
        // b = a 向下滚 80 行：内容上移，纹理带移到 20..140（顶部 0..20 空白，底部 140..312 空白）
        let b = mk(20..140);
        match find_scroll_delta(&a, &b) {
            Some(s) if (60..=100).contains(&s) => {}
            other => panic!("expected Some(~80), got {other:?}"),
        }
    }

    /// 大部分空白、夹一条纹理带的**相同**帧：没滚动时 find_scroll_delta 必须返回
    /// None。若空白自相似让最大偏移(如 274)拿到低分并过验证，就会误拼出重复。
    #[test]
    fn sparse_identical_frames_return_none() {
        let w = 1282usize;
        let h = 304usize;
        let mk = |textured: std::ops::Range<usize>| {
            let mut px = vec![0u8; w * h * 4];
            for row in textured {
                let v = row_val(row);
                for x in 0..w {
                    let p = (row * w + x) * 4;
                    px[p] = v;
                    px[p + 1] = v.wrapping_add(37);
                    px[p + 2] = v.wrapping_mul(3);
                    px[p + 3] = 255;
                }
            }
            CapturedFrame { width: w as u32, height: h as u32, pixels: px }
        };
        // 纹理带在中间(100..220)，底部 274..304 空白
        let a = mk(100..220);
        let b = a.clone();
        let r = find_scroll_delta(&a, &b);
        eprintln!("sparse identical (band mid) -> delta={r:?}");
        if r.is_some() {
            panic!("sparse identical frames should return None, got {r:?}");
        }
    }

}
