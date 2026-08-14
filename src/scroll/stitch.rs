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
fn band_has_energy(sig_a: &[u64], s: usize, h: usize) -> bool {
    let n = h - s;
    if n < 2 {
        return false;
    }
    let mut total = 0u64;
    for r in (s + 1)..h {
        total += sig_a[r].abs_diff(sig_a[r - 1]);
    }
    total / n as u64 >= MIN_ENERGY
}

/// 对候选滚动量 s 做逐像素抽样验证：重叠带内取 5 行 × 8 列，比较 RGB。
fn verify_rows(a: &CapturedFrame, b: &CapturedFrame, s: usize, w: usize, vcols: &[usize]) -> bool {
    let h = a.height as usize;
    let n = h - s; // 重叠行数
    if n < 5 {
        return false;
    }
    // 5 等分采样行，覆盖重叠带全高，降低局部误匹配概率
    let rows = [n / 6, n * 2 / 6, n * 3 / 6, n * 4 / 6, n * 5 / 6];
    for &r in &rows {
        let ar = s + r; // a 侧行
        let br = r; // b 侧行
        for &c in vcols {
            let pa = (ar * w + c) * 4;
            let pb = (br * w + c) * 4;
            for ch in 0..3 {
                if a.pixels[pa + ch].abs_diff(b.pixels[pb + ch]) > PIXEL_TOLERANCE {
                    return false;
                }
            }
        }
    }
    true
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
}
