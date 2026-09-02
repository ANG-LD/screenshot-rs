//! 滚动截屏拼接：重叠检测（纯函数，可单测）。
//!
//! 连续两次抓取同一屏幕区域 a、b（内容向下滚动）。向下滚动 s 行意味着
//! 内容上移 s 行，因此 b 的顶部 (h-s) 行等于 a 的底部 (h-s) 行（重叠带），
//! b 的底部 s 行是新进入视口的内容。`find_scroll_delta` 负责找出 s。
//!
//! ## 算法（行级匹配 + 匹配行数计数）
//!
//! 对每个候选滚动量 s，统计重叠带内「逐行内容一致」的行数（行匹配 =
//! 8 个采样列的 3×3 局部平均 RGB 差 ≤ 容差），取**匹配行数最多**的 s 为答案。
//!
//! 相比「签名平均差最小」的旧算法，匹配行数对网页场景更鲁棒：
//! - 网页滚动是整帧合成平移，静止帧的重叠带逐行完全一致 → 真实 s 匹配行数
//!   压倒性最高；自相似内容（段落/列表）的假偏移只有少数行巧合匹配 → 被拒。
//! - 动画中间帧（亚像素混合）行级匹配普遍失败 → None（不拼不丢，等静止帧）。
//! - 动态元素（广告轮播/时钟）只影响少数行 → 匹配行数仍最高 → 正确拼接。
//! - 唯一性：最佳 s 的匹配数必须明显优于**远邻**（|Δs|>2）次优——周期性/
//!   自相似内容会在 s±周期 处同样高匹配 → 拒绝（宁缺毋滥，不拼错重复）。
//! - 快速滚动（重叠带 < STRICT_OVERLAP）：额外要求邻居 s±1/±2 不匹配
//!   （小重叠带下 ±1 行误差会放大成明显重复）。

use std::cell::RefCell;

use crate::capture::CapturedFrame;

/// 行匹配采样列数（8 列均匀分布）
const VCOLS: usize = 8;
/// 判定重叠带「有内容」的相邻行平均差阈值（行平均矩阵 24 值差和）
const MIN_ENERGY: u64 = 16;
/// 滚动量上限（超出视为异常，拒绝）
const MAX_SCROLL: usize = 800;
/// 要求重叠带至少保留的行数（太少则无法可靠判定）
const MIN_OVERLAP: usize = 30;
/// 逐行匹配容差（每通道，3×3 局部平均）。
///
/// 网页滚动是整帧合成平移：静止帧的重叠带**逐像素一致**（差 0），所以容差
/// 只需覆盖极小噪声。**不能太大**：3×3 平均会平滑相邻行，宽松容差会让
/// 「相邻行」也近似匹配（自相似/随机纹理下多个偏移假匹配 → 无法判定）。
/// 24 与 frames_differ 的变化阈值一致；动态元素（广告/时钟）只影响少数行，
/// 由匹配行数比例（15% 容错）吸收。
const PIXEL_TOLERANCE: u8 = 24;
/// 重叠带行数低于此值时按「快速滚动」处理：要求更高匹配率 + 邻居无歧义
const STRICT_OVERLAP: usize = 80;
/// 判别真滚动的唯一性/匹配率分级门槛见 `find_scroll_delta` 内部注释：
/// 旧「占重叠带比例」(MIN_MATCH_RATIO/STRICT_MATCH_RATIO) 会把 vxe-table 等
/// **固定区表格**的真实滚动误拒（固定表头/分页栏占比高 → 匹配率被稀释到阈值下）
/// → 引擎测不出 → 拼接失败（只有一页，本 bug 根因）。现改为「唯一性为主、匹配率
/// 为辅」：固定区滚动靠唯一峰（远邻 Δs 差）+ 匹配率分档识别，真伪滚动可兼得。
///
/// 每行的匹配签名：8 列 × 3 通道的 3×3 局部平均（预计算，避免重复 box_avg）。
type RowAvg = [u8; VCOLS * 3];

/// `find_scroll_delta` 的可复用中间缓冲（避免每帧反复 malloc）。
struct StitchScratch {
    vcols: Vec<usize>,
    ma: Vec<RowAvg>,
    mb: Vec<RowAvg>,
    /// 候选列表：(匹配行数, s)，按匹配行数降序
    cands: Vec<(usize, usize)>,
}

impl Default for StitchScratch {
    fn default() -> Self {
        Self {
            vcols: Vec::with_capacity(VCOLS),
            ma: Vec::new(),
            mb: Vec::new(),
            cands: Vec::new(),
        }
    }
}

impl StitchScratch {
    /// 前进检查：a 与 b 同位置逐行匹配率 ≥ 阈值，说明内容基本没滚动（相同帧 /
    /// 几乎未滚），任何候选 s 的匹配都是「内容自相似」的假峰，必须拒绝。
    ///
    /// 返回 `None` 表示内容没动（调用方应视为无滚动）；`Some(())` 表示内容动了。
    fn content_moved(&self, h: usize) -> bool {
        let mut same = 0u32;
        let total = h as u32;
        for r in 0..h {
            if row_matches(&self.ma[r], &self.mb[r]) {
                same += 1;
            }
        }
        // 相同帧：同位置匹配率 ≥ 90% → 无滚动。真实滚动时顶部行错位，匹配率显著
        // 低于 90%。用 90% 而非 100%，容忍动态元素局部变化。
        same * 10 < total * 9
    }

    /// 计算全部候选偏移的匹配行数，**按匹配数降序**填充 `self.cands`，
    /// 返回 (最佳匹配数, 最佳 s)。返回 None 表示没有任何候选（重叠带无纹理）。
    ///
    /// `max_s` 控制最大滚动量：严格检测用 `min(h-MIN_OVERLAP, MAX_SCROLL)`；
    /// 宽松估计允许滚到接近整个帧高（快速滚动无重叠时也能取到最可能偏移）。
    fn score_candidates(&mut self, h: usize, max_s: usize) -> Option<(usize, usize)> {
        self.cands.clear();
        for s in 1..=max_s {
            // band_has_energy 拒绝空白/均匀带（任何偏移都能匹配，无法判定）
            if !band_has_energy(&self.ma, s, h) {
                continue;
            }
            let n = h - s;
            let mut count = 0usize;
            for r in 0..n {
                if row_matches(&self.ma[s + r], &self.mb[r]) {
                    count += 1;
                }
            }
            self.cands.push((count, s));
        }
        self.cands.sort_unstable_by(|x, y| y.0.cmp(&x.0));
        self.cands.first().copied()
    }

    /// 严格重叠检测（保持原有语义，唯一性 + 匹配率分档判真滚动）。
    fn find_scroll_delta(&mut self, a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
        if a.width != b.width || a.height != b.height {
            return None;
        }
        let w = a.width as usize;
        let h = a.height as usize;
        if w == 0 || h == 0 {
            return None;
        }
        fill_vcols(w, &mut self.vcols);
        fill_row_avgs(a, w, h, &self.vcols, &mut self.ma);
        fill_row_avgs(b, w, h, &self.vcols, &mut self.mb);
        if !self.content_moved(h) {
            return None;
        }

        let max_s = h.saturating_sub(MIN_OVERLAP).min(MAX_SCROLL);
        if max_s == 0 {
            return None;
        }
        let Some((best_count, best_s)) = self.score_candidates(h, max_s) else {
            return None;
        };
        // 候选过少：band_has_energy 拒绝了绝大多数 s（重叠带几乎无纹理——低能量
        // 页面/纯色段/大段空白），此时任何 s 都可能「巧合通过」，唯一性检查样本
        // 不足 → 拒绝。否则没滚动的低能量帧会误报 Some(1~3)（假小偏移，无害但
        // 刷屏），真滚动帧也可能被唯一性薄弱放过假偏移（重复拼接风险）。
        if self.cands.len() < 4 {
            return None;
        }

        let n_best = h - best_s;
        // 匹配行数下限（宽松兜底，过滤完全无信号/全空白帧）。
        //
        // **关键修正（vxe-table 等表格）**：滚动量大时，重叠带 b 顶部是**固定表头/
        // 搜索栏**，a 底部是**固定分页栏/底栏**——这两块固定区不与对方内容对齐，
        // 永不匹配，占重叠带比例随 s 增大而恶化（s=250 时 82%，s=450 时 50%，
        // s=540 时 13%）。若仍按「占重叠带比例」门槛（0.66/0.85）判定，真实滚动
        // 会被误拒 → 引擎测不出 → 拼接失败 = 只有一页。**这正是本 bug 的根因。**
        //
        // 新方案：**绝对唯一性为主、匹配率为辅**。
        //  1) 远邻唯一性（absolute margin）：任何候选必须显著优于远邻（|Δs|>2）。
        //     这拒绝**周期性/自相似内容**的假偏移（真实滚动量 s 附近有「周期对齐」
        //     的假峰，匹配数与 real s 接近或更高 → margin 不足）。
        //  2) 通过唯一性后，再按匹配率决定是否接受：
        //     - 高匹配率（真实整帧平移）→ 直接接受；
        //     - 低匹配率（固定区稀释）→ 还要求 second 显著小于 best（排除局部 patch）。
        let margin = (n_best / 25).max(3);
        let match_ratio = (best_count as f32) / (n_best as f32);
        let second = self
            .cands
            .iter()
            .find(|(_, s)| s.abs_diff(best_s) > 2)
            .map(|(c, _)| *c)
            .unwrap_or(0);
        // ① 远邻唯一性（绝对差分）：周期/自相似内容的假偏移在此被拒
        //（periodic 35: second≈count → 差值 0 < margin 15 → None）。
        if best_count.saturating_sub(second) < margin {
            tracing::debug!(
                "fd: uniq-gate reject s={best_s} count={best_count} second={second} margin={margin}"
            );
            return None;
        }
        // ② 匹配率二次判别：高匹配=真实整帧滚动；低匹配=固定区稀释，需唯一峰更强。
        let high_match = match_ratio >= 0.85;
        let second_rel = if best_count == 0 {
            0.0
        } else {
            (second as f32) / (best_count as f32)
        };
        // 低匹配率（固定区滚动 best/n_best 低到 13%）时，second 必须逼近 0（唯一峰
        // 占绝对主导）；局部窄带 patch 的 second 相对大（如 0.75）→ 拒绝。
        let relative_ok = if high_match {
            true
        } else {
            second_rel <= 0.30
        };
        if !relative_ok {
            tracing::debug!(
                "fd: rel-gate reject s={best_s} count={best_count} second={second} n_best={n_best} ratio={match_ratio:.2} second_rel={second_rel:.2} high={high_match}"
            );
            return None;
        }

        // 快速滚动（小重叠带）：邻居 s±1/±2 也必须明显更差——
        // 否则偏移不唯一（渐变/自相似内容），±1 行误差在小重叠带下
        // 会放大成可见重复块。
        if n_best < STRICT_OVERLAP {
            let neighbor = self
                .cands
                .iter()
                .find(|(_, s)| s.abs_diff(best_s) <= 2 && *s != best_s)
                .map(|(c, _)| *c)
                .unwrap_or(0);
            if best_count.saturating_sub(neighbor) < margin {
                return None;
            }
        }

        tracing::debug!(
            "fd: accept s={best_s} count={best_count} n_best={n_best} second={second} margin={margin}"
        );
        Some(best_s)
    }

    /// 宽松最佳偏移估计：内容明显移动时，返回**匹配行数最多**的偏移，即便它不唯一。
    ///
    /// `force` = 跳过 `content_moved` 的「同位置匹配 ≥90% → 判定没动」关卡。
    /// 对 vxe-table / 固定区表格，滚动后新帧与旧帧在**同位置**的行常常仍高度匹配
    /// （固定表头 + 自相似行，平均值被稀释），`content_moved` 会**误判成「没滚动」**
    /// → 返回 None → 手动循环把这段整段丢弃（长图中间缺行 = 「31→39 跳号」bug 根因）。
    /// 当调用方已确认内容**确实变了**（`frames_differ` 为真）时，应传 `force=true`，
    /// 按最匹配偏移继续拼，宁肯有一两行缝也不丢几十行。仍由 `best_count` 下限兜底，
    /// 防止只靠一个闪烁元素的无滚动帧被误拼。
    ///
    /// `force=false` 为保守路径（原语义）。
    fn estimate_scroll_delta(
        &mut self,
        a: &CapturedFrame,
        b: &CapturedFrame,
        force: bool,
    ) -> Option<usize> {
        if a.width != b.width || a.height != b.height {
            return None;
        }
        let w = a.width as usize;
        let h = a.height as usize;
        if w == 0 || h == 0 {
            return None;
        }
        fill_vcols(w, &mut self.vcols);
        fill_row_avgs(a, w, h, &self.vcols, &mut self.ma);
        fill_row_avgs(b, w, h, &self.vcols, &mut self.mb);
        if !force && !self.content_moved(h) {
            return None;
        }

        // 快速滚动可能整个视口都换了内容（无重叠），此时任何偏移都只是随机匹配。
        // 让估计滚动量上探到接近整帧，取最可能的一个；若连一个像样的峰都没有 → None。
        let max_s = h.saturating_sub(MIN_OVERLAP);
        if max_s == 0 {
            return None;
        }
        let Some((best_count, best_s)) = self.score_candidates(h, max_s) else {
            return None;
        };
        let n_best = h - best_s;
        // 最小可信匹配数：重叠带至少要有约 1/12 的行真正对齐，才有「真实重叠」的底气。
        // 低于此值是随机匹配 / 无重叠（滚动超一屏），不是真实偏移，拒绝（宁缺毋滥）。
        if best_count < (n_best / 12).max(8) {
            tracing::debug!(
                "est: reject s={best_s} count={best_count} n_best={n_best}"
            );
            return None;
        }
        tracing::debug!(
            "est: accept s={best_s} count={best_count} n_best={n_best}"
        );
        Some(best_s)
    }
}

thread_local! {
    static SCRATCH: RefCell<StitchScratch> = RefCell::new(StitchScratch::default());
}

/// 返回 b 相对 a 向下滚动的行数 s。
///
/// 若内容没动 / 无法可靠判定（空白、歧义、动画中间帧、匹配不上）→ `None`。
pub fn find_scroll_delta(a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
    SCRATCH.with(|s| s.borrow_mut().find_scroll_delta(a, b))
}

/// 宽松最佳偏移估计（见 [`StitchScratch::estimate_scroll_delta`]）。
///
/// 与严格检测的区别：放弃唯一性门槛，取匹配行数最多的偏移，宁肯有一行缝也不丢段。
/// 用于手动滚动时快速滚动 / 虚拟表格重建行导致的「严格检测测不出 → 丢内容」。
pub fn estimate_scroll_delta(a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
    SCRATCH.with(|s| s.borrow_mut().estimate_scroll_delta(a, b, false))
}

/// 强制版宽松估计：跳过 `content_moved` 的「没动」关卡（见 [`estimate_scroll_delta`]）。
///
/// 仅当调用方已确认两帧内容**确实变了**（如手动循环里 `frames_differ` 为真）时使用，
/// 用于 vxe-table 这类固定区/自相似表格：它们滚动后同位置行仍高度匹配，保守版会误判
/// 「没滚动」而返回 None，导致整段内容被丢弃（长图跳号）。强制版仍受 `best_count`
/// 下限约束，防止无滚动帧被误拼。
pub fn force_estimate_scroll_delta(a: &CapturedFrame, b: &CapturedFrame) -> Option<usize> {
    SCRATCH.with(|s| s.borrow_mut().estimate_scroll_delta(a, b, true))
}

/// 计算在偏移 `s` 下，重叠带（a 的 [s..h] 与 b 的 [0..h-s]）的平均像素差
/// （RGB 三通道 abs 差之和 / 采样数）。采样行 × 采样列做逐像素对比。
fn offset_mean_diff(
    a: &CapturedFrame,
    b: &CapturedFrame,
    s: usize,
    w: usize,
    h: usize,
    row_stride: usize,
    cols: &[usize],
) -> Option<u64> {
    let n = h.saturating_sub(s); // 重叠带行数
    if n < MIN_OVERLAP {
        return None;
    }
    let mut sum = 0u64;
    let mut cnt = 0u64;
    for rr in (0..n).step_by(row_stride) {
        let pa = ((s + rr) * w) * 4; // a 的行 (s+rr)
        let pb = (rr * w) * 4; // b 的行 rr
        for &c in cols {
            let p = pa + c * 4;
            let q = pb + c * 4;
            let d = (a.pixels[p] as i32 - b.pixels[q] as i32).unsigned_abs()
                + (a.pixels[p + 1] as i32 - b.pixels[q + 1] as i32).unsigned_abs()
                + (a.pixels[p + 2] as i32 - b.pixels[q + 2] as i32).unsigned_abs();
            sum += d as u64;
            cnt += 1;
        }
    }
    if cnt == 0 {
        None
    } else {
        Some(sum / cnt)
    }
}

/// 计算某单一偏移 `s` 下的重叠带平均像素差（供 try_append_scrolled 的**相对**判据用）：
/// 固定表头/分页栏对**所有**候选都加一个**常量像素差惩罚**（它们不随滚动移动），
/// 因此绝对阈值不可靠，需比较「wide 扫描的最佳 s」与「粗估 s」的相对大小。
pub fn pixel_diff_at(a: &CapturedFrame, b: &CapturedFrame, s: usize) -> Option<u64> {
    if a.width != b.width || a.height != b.height || a.width == 0 || a.height == 0 {
        return None;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    let row_stride = (h / 16).max(1);
    let cols: Vec<usize> = (0..64).map(|i| w * (i + 1) / 65).collect();
    offset_mean_diff(a, b, s, w, h, row_stride, &cols)
}

/// 整帧**未对齐**的平均每像素差（同一坐标 a vs b）。衡量「内容是否真的变了」：
/// 静止帧（b≈a）接近 0；真实滚动（内容整体移动、行数据不同）偏大。用于区分
/// 「静止帧被误取周期大偏移 s=630 重复拼接」与「真实滚动」。
pub fn mean_unaligned_diff(a: &CapturedFrame, b: &CapturedFrame) -> Option<u64> {
    if a.width != b.width || a.height != b.height || a.width == 0 || a.height == 0 {
        return None;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    let row_stride = (h / 16).max(1);
    let cols: Vec<usize> = (0..64).map(|i| w * (i + 1) / 65).collect();
    offset_mean_diff(a, b, 0, w, h, row_stride, &cols)
}

/// 在合法偏移范围 [MIN_OFFSET ..= min(h-MIN_OVERLAP, MAX_SCROLL)] **全范围**扫描，
/// 返回「**接近最小**重叠带平均像素差、且**最小 s**」的偏移 `(s, mean_diff)`。
///
/// 行级签名匹配的 `estimate`/`find_scroll_delta` 按「匹配行数最多」选偏移，在
/// vxe-table 这类**行周期重复**（行距 ≈ 30px）的表格里会被周期整数倍偏移骗到。
/// 真实滚动是整帧**逐像素一致平移**：真实 s 的重叠带像素差最低。但**白色主导**的
/// 表格里平均差被白底稀释，**多个偏移都显「干净」**（真实小滚动与周期假偏移的差
/// 都被压到很低）——用纯 argmin 会选中周期大偏移（如本页 iter50/55/75 的 352/239/196
/// 重复拼接整段），而真实滚动是小步（小 s）。因此取「接近最小像素差（±TOL）中
/// **最小**的 s」：真实小滚动是最小且最靠前的干净平移，周期假偏移虽近最小但对应
/// 大 s。仅当确实没有更小偏移也接近最小像素差（即最小 s 本身匹配很差）才回落 argmin。
///
/// 注意：固定表头/分页栏给**所有**候选加常量惩罚，故「接近最小」用**绝对容差**
/// （min_diff + TOL）判定即可（常量惩罚抵掉后相对差不受影响）。
pub fn best_pixel_offset(a: &CapturedFrame, b: &CapturedFrame) -> Option<(usize, u64)> {
    if a.width != b.width || a.height != b.height {
        return None;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return None;
    }
    const MIN_OFFSET: usize = 4; // 跳过 0（无滚动）
    const TOL: u64 = 2; // 允许「接近最小」的像素差兜底（真实平移噪声小）
    let lo = MIN_OFFSET;
    let hi = h.saturating_sub(MIN_OVERLAP).min(MAX_SCROLL);
    if hi <= lo {
        return None;
    }
    let row_stride = (h / 16).max(1);
    let cols: Vec<usize> = (0..64).map(|i| w * (i + 1) / 65).collect();
    // 第一遍：找全范围最小像素差。
    let mut min_diff = u64::MAX;
    for cand in lo..=hi {
        if let Some(diff) = offset_mean_diff(a, b, cand, w, h, row_stride, &cols) {
            if diff < min_diff {
                min_diff = diff;
            }
        }
    }
    if min_diff == u64::MAX {
        return None;
    }
    // 第二遍：在「接近最小（≤ min_diff+TOL）」里挑最小的 s（同 s 取最小差）。
    let mut best: Option<(usize, u64)> = None;
    for cand in lo..=hi {
        if let Some(diff) = offset_mean_diff(a, b, cand, w, h, row_stride, &cols) {
            if diff <= min_diff + TOL {
                let take = match best {
                    None => true,
                    Some((bs, bd)) => cand < bs || (cand == bs && diff < bd),
                };
                if take {
                    best = Some((cand, diff));
                }
            }
        }
    }
    best
}

/// 逐像素验证采样列（8 列均匀分布）
fn fill_vcols(w: usize, out: &mut Vec<usize>) {
    out.clear();
    out.extend((0..VCOLS).map(|i| w * (i + 1) / (VCOLS + 1)));
}

/// 预计算每行的匹配签名：8 列 × 3 通道的 3×3 局部平均。
fn fill_row_avgs(
    f: &CapturedFrame,
    w: usize,
    h: usize,
    vcols: &[usize],
    out: &mut Vec<RowAvg>,
) {
    out.clear();
    out.reserve(h);
    let px = &f.pixels;
    for r in 0..h {
        let mut row = [0u8; VCOLS * 3];
        let mut k = 0;
        for &c in vcols {
            for ch in 0..3 {
                row[k] = box_avg(px, r, c, w, h, ch);
                k += 1;
            }
        }
        out.push(row);
    }
}

/// 两行的签名是否一致（所有采样列 × 通道的 3×3 平均差 ≤ 容差）。
#[inline]
fn row_matches(ma: &RowAvg, mb: &RowAvg) -> bool {
    for i in 0..VCOLS * 3 {
        if ma[i].abs_diff(mb[i]) > PIXEL_TOLERANCE {
            return false;
        }
    }
    true
}

/// 重叠带（a 的 s..h 行）必须有内容：相邻行签名差过低说明整带均匀，
/// 此时任何偏移都能「匹配」，无法可靠判定。
///
/// 但「均值过低」对**大部分空白/平滑、夹一条窄纹理带**的帧会误判：空白行把均值
/// 稀释到阈值以下，而那条纹理带其实是能钉住偏移的。因此除了均值判定，还要看
/// 带内**有纹理的行数**——存在可观纹理行（≥1/8 行相邻差达标）就仍可判定。
fn band_has_energy(ma: &[RowAvg], s: usize, h: usize) -> bool {
    let n = h - s;
    if n < 2 {
        return false;
    }
    let mut total = 0u64;
    let mut textured = 0u32;
    for r in (s + 1)..h {
        let d = row_avg_diff(&ma[r], &ma[r - 1]);
        total += d;
        if d >= MIN_ENERGY {
            textured += 1;
        }
    }
    total / n as u64 >= MIN_ENERGY || textured * 8 >= n as u32
}

/// 两行签名的差（24 值差和）。
#[inline]
fn row_avg_diff(a: &RowAvg, b: &RowAvg) -> u64 {
    let mut acc = 0u64;
    for i in 0..VCOLS * 3 {
        acc += a[i].abs_diff(b[i]) as u64;
    }
    acc
}

/// (row, col) 处 3×3 邻域的某通道平均值（越界夹取到图像边缘）。
#[inline]
fn box_avg(px: &[u8], row: usize, col: usize, w: usize, h: usize, ch: usize) -> u8 {
    let mut sum = 0u64;
    let mut n = 0u64;
    let r0 = row.saturating_sub(1);
    let r1 = (row + 1).min(h - 1);
    let c0 = col.saturating_sub(1);
    let c1 = (col + 1).min(w - 1);
    for rr in r0..=r1 {
        for cc in c0..=c1 {
            let p = (rr * w + cc) * 4 + ch;
            sum += px[p] as u64;
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

    /// 像素值 = f(行, 列)：行列都参与散列，行签名唯一性强——
    /// 单色行（值域仅 256）会让大量偏移「假匹配」→ 歧义拒绝，测不出真实滚动量。
    fn px(row: usize, col: usize) -> u8 {
        row_val(row.wrapping_mul(7).wrapping_add(col.wrapping_mul(13)))
    }

    /// 造一帧 h 行、行内每列有区分度的图像（模拟真实网页：行内容丰富）
    fn frame(w: usize, h: usize) -> CapturedFrame {
        let mut pixels = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for c in 0..w {
                let v = px(row, c);
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

    /// 造 b = a 向下滚动 shift 行的帧（顶部重叠带沿用 a 内容，底部是新内容）。
    /// 内容在滚动中保持连续（真实网页语义）。
    fn scrolled(_a: &CapturedFrame, w: usize, h: usize, shift: usize) -> CapturedFrame {
        let mut pixels = vec![0u8; w * h * 4];
        for row in 0..h {
            let src = if row + shift < h { row + shift } else { h + row };
            for c in 0..w {
                let v = px(src, c);
                let p = (row * w + c) * 4;
                pixels[p] = v;
                pixels[p + 1] = v.wrapping_add(37);
                pixels[p + 2] = v.wrapping_mul(3);
                pixels[p + 3] = 255;
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
        let a = frame(200, 600);
        let b = scrolled(&a, 200, 600, 120);
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
    /// 行级匹配对真实偏移 s 仍通过（混合值与 3×3 平均差 ≤ 42 < 50），
    /// 邻居 s±1 同样接近但**常规重叠带不检查邻居**；关键是**不能**返回
    /// 更大假偏移（如把 120 检测成 280 → 拼接重复）。
    #[test]
    fn detects_scroll_delta_with_subpixel_blur() {
        let w = 200usize;
        let h = 600usize;
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            // 行内容 = px(row, c)：每列不同，行签名唯一（避免单色行碰撞歧义）
            for c in 0..w {
                let p = (row * w + c) * 4;
                let v = px(row, c);
                ap[p] = v;
                ap[p + 1] = v;
                ap[p + 2] = v;
                ap[p + 3] = 255;
            }
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            let src: usize = if row + 120 < h { row + 120 } else { h + row };
            for c in 0..w {
                let p = (row * w + c) * 4;
                let v0 = px(src, c);
                let v1 = px(src + 1, c);
                let v = (v0 as u16 + v1 as u16) / 2; // 亚像素模糊：两行各半
                bp[p] = v as u8;
                bp[p + 1] = v as u8;
                bp[p + 2] = v as u8;
                bp[p + 3] = 255;
            }
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        match find_scroll_delta(&a, &b) {
            Some(s) if s == 120 || s == 121 => {}
            other => panic!("expected Some(120|121), got {other:?}"),
        }
    }

    /// 大部分空白、夹一条纹理带的帧（内容在滚动中连续）：纹理带钉住偏移，
    /// 空白行不干扰（它们对所有 s 都匹配，但纹理行只在真实 s 匹配）。
    #[test]
    fn detects_scroll_delta_with_sparse_texture() {
        let w = 1261usize;
        let h = 312usize;
        // a：行 100..220 有纹理，其余空白
        let mut ap = vec![0u8; w * h * 4];
        for row in 100..220 {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = px(row, x);
                ap[p] = v;
                ap[p + 1] = v.wrapping_add(37);
                ap[p + 2] = v.wrapping_mul(3);
                ap[p + 3] = 255;
            }
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        // b = a 向下滚 80 行：纹理带内容移到 20..140（内容连续）
        let mut bp = vec![0u8; w * h * 4];
        for row in 20..140 {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = px(row + 80, x);
                bp[p] = v;
                bp[p + 1] = v.wrapping_add(37);
                bp[p + 2] = v.wrapping_mul(3);
                bp[p + 3] = 255;
            }
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        match find_scroll_delta(&a, &b) {
            Some(s) if (60..=100).contains(&s) => {}
            other => panic!("expected Some(~80), got {other:?}"),
        }
    }

    /// 页面含**固定顶部栏**（不随滚动移动，真实网页常见）：滚动量大时重叠带小、
    /// 固定区占比高 → 匹配率被稀释（本例 250 行滚动 + 60 行固定栏 → 仅 82.9%）。
    /// 匹配率门槛必须容忍固定区（唯一性检查兜底假偏移），否则真滚动被拒 → 丢段。
    #[test]
    fn detects_scroll_delta_with_fixed_header() {
        let w = 400usize;
        let h = 600usize;
        let header = 60usize; // 顶部 60 行固定（10%）
        // a：行 0..60 固定栏（内容与正文不同域，避免与正文混淆），行 60..600 正文
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = if row < header { px(row + 5000, x) } else { px(row, x) };
                ap[p] = v;
                ap[p + 1] = v.wrapping_add(37);
                ap[p + 2] = v.wrapping_mul(3);
                ap[p + 3] = 255;
            }
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        // b：固定栏不变；正文 = a 正文滚 250 行（内容连续）
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = if row < header {
                    px(row + 5000, x)
                } else {
                    px(row + 250, x) // 正文上移 250
                };
                bp[p] = v;
                bp[p + 1] = v.wrapping_add(37);
                bp[p + 2] = v.wrapping_mul(3);
                bp[p + 3] = 255;
            }
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        match find_scroll_delta(&a, &b) {
            // 250 或 251（3×3 平均 ±1 行模糊）
            Some(s) if s == 250 || s == 251 => {}
            other => panic!("expected Some(250|251) with fixed header, got {other:?}"),
        }
    }

    /// 大部分空白、夹一条纹理带的**相同**帧：没滚动时 find_scroll_delta 必须返回
    /// None。若空白自相似让某偏移拿到高匹配数，就会误拼出重复。
    #[test]
    fn sparse_identical_frames_return_none() {
        let w = 1282usize;
        let h = 304usize;
        let mk = |textured: std::ops::Range<usize>| {
            let mut px_vec = vec![0u8; w * h * 4];
            for row in textured {
                for x in 0..w {
                    let p = (row * w + x) * 4;
                    let v = px(row, x);
                    px_vec[p] = v;
                    px_vec[p + 1] = v.wrapping_add(37);
                    px_vec[p + 2] = v.wrapping_mul(3);
                    px_vec[p + 3] = 255;
                }
            }
            CapturedFrame { width: w as u32, height: h as u32, pixels: px_vec }
        };
        let a = mk(100..220);
        let b = a.clone();
        let r = find_scroll_delta(&a, &b);
        eprintln!("sparse identical (band mid) -> delta={r:?}");
        if r.is_some() {
            panic!("sparse identical frames should return None, got {r:?}");
        }
    }

    /// 快速滚动（重叠带 < STRICT_OVERLAP）且内容唯一：s 精确唯一（邻居不匹配），
    /// 严格验证不应误伤——仍返回真实滚动量。
    #[test]
    fn large_delta_precise_when_unique() {
        let w = 200usize;
        let h = 600usize;
        let a = frame(w, h);
        let b = scrolled(&a, w, h, 540);
        assert_eq!(find_scroll_delta(&a, &b), Some(540));
    }

    /// 快速滚动 + 行内容变化平缓（每行差 1，如浅色渐变的空白页）：s=540 与
    /// s=539/541 同样匹配（偏移不唯一）→ 小重叠带严格检查邻居 → 必须拒绝。
    /// 若返回 Some，拼接会把已拼的重叠带重复拼入（长图重复块）。
    #[test]
    fn large_delta_ambiguous_rejected() {
        let w = 200usize;
        let h = 600usize;
        let mk = |pixels: &mut Vec<u8>, w: usize, row: usize, src: usize| {
            let v = (src % 256) as u8; // 相邻行差 1：任何 ±1 偏移都“匹配”
            for c in 0..w {
                let p = (row * w + c) * 4;
                pixels[p] = v;
                pixels[p + 1] = v.wrapping_add(37);
                pixels[p + 2] = v.wrapping_mul(3);
                pixels[p + 3] = 255;
            }
        };
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            mk(&mut ap, w, row, row);
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            let v = if row + 540 < h {
                ((row + 540) % 256) as u8
            } else {
                row_val(h + row)
            };
            for c in 0..w {
                let p = (row * w + c) * 4;
                bp[p] = v;
                bp[p + 1] = v.wrapping_add(37);
                bp[p + 2] = v.wrapping_mul(3);
                bp[p + 3] = 255;
            }
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        let r = find_scroll_delta(&a, &b);
        eprintln!("large delta ambiguous -> delta={r:?}");
        if r.is_some() {
            panic!("ambiguous large delta should be rejected, got {r:?}");
        }
    }

    /// 网页自相似内容（段落/列表重复结构）：只有真实偏移让整行内容对齐，
    /// 假偏移（如把 30 行滚动报成 200）匹配行数显著低 → 拒绝。
    #[test]
    fn self_similar_false_offset_rejected() {
        let w = 300usize;
        let h = 500usize;
        // 每 40 行一个「段落块」：块内行内容相同（自相似），块间用散列值区分
        //（不精确周期——真实网页段落内容各异，只是结构相似）
        let mk = |pixels: &mut Vec<u8>, w: usize, row: usize, block: usize| {
            for c in 0..w {
                let p = (row * w + c) * 4;
                let v = row_val(block.wrapping_mul(31).wrapping_add(c.wrapping_mul(13)));
                pixels[p] = v;
                pixels[p + 1] = v.wrapping_add(37);
                pixels[p + 2] = v.wrapping_mul(3);
                pixels[p + 3] = 255;
            }
        };
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            mk(&mut ap, w, row, row / 40);
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        // b = a 滚 30 行：块内容上移 30（行号映射保持内容连续）
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            let src = if row + 30 < h { row + 30 } else { h + row };
            mk(&mut bp, w, row, src / 40);
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        match find_scroll_delta(&a, &b) {
            // 允许 30 或 31（3×3 平均的 ±1 行模糊）；绝不能报出 200 这类假偏移
            Some(s) if s == 30 || s == 31 => {}
            other => panic!("expected Some(30|31), got {other:?}"),
        }
    }

    /// 周期内容（如重复表格行）：真实 s 与 s±周期 匹配行数同样高 →
    /// 远邻唯一性检查拒绝（宁缺毋滥，避免把重叠带错位拼接成重复）。
    #[test]
    fn periodic_content_rejected_when_ambiguous() {
        let w = 200usize;
        let h = 400usize;
        // 周期 20 的行模式：行内容 = px(row%20, c)（列参与 → 行签名唯一但每 20 行重复）
        let mk = |pixels: &mut Vec<u8>, w: usize, row: usize, src_row: usize| {
            for c in 0..w {
                let p = (row * w + c) * 4;
                let v = px(src_row % 20, c);
                pixels[p] = v;
                pixels[p + 1] = v.wrapping_add(37);
                pixels[p + 2] = v.wrapping_mul(3);
                pixels[p + 3] = 255;
            }
        };
        let mut ap = vec![0u8; w * h * 4];
        for row in 0..h {
            mk(&mut ap, w, row, row);
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        // b = a 滚 35 行：相位移动 35（≡15 mod 20）——35 与 15（相位相同的假偏移）
        // 的行模式逐行一致 → 匹配行数接近 → 唯一性检查拒绝（None）
        let mut bp = vec![0u8; w * h * 4];
        for row in 0..h {
            let src = if row + 35 < h { row + 35 } else { h + row };
            mk(&mut bp, w, row, src);
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        let r = find_scroll_delta(&a, &b);
        eprintln!("periodic 35 -> delta={r:?}");
        // 35 ≡ 15 (mod 20)：15 是「相位相同」的假偏移（内容模式相同但行位置错 20）
        // → 匹配行数与 35 接近 → 应拒绝（None），否则把重叠带错位拼成重复
        assert!(r.is_none(), "periodic scroll should be rejected as ambiguous, got {r:?}");
    }

    /// 诊断：模拟 vxe-table 表格真实滚动场景。
    ///
    /// 用户框选整个表格组件，选区结构（宽 1647、高 661，来自真实 HTML）:
    ///   - 顶部 45px 表头（固定，不随内容滚动）
    ///   - 中间 630px 表体（唯一可滚动内容区，滚动量 s）
    ///   - 底部约 60px 分页栏/底部（固定）
    ///   - 左右固定列（序号/操作，DOM 用 JS 同步，视为内容以相同 s 滚动）
    ///
    /// 验证：真实滚动 s 行时，find_scroll_delta 能否测出 s（而非 None / 假偏移）。
    /// 这是「自动/手动滚动都只有一页」的最可能根因——若这里测不出，就找到了病灶。
    #[test]
    fn vxe_table_scroll_delta_detected() {
        let w = 1647usize; // 表体宽
        let h = 661usize; // 选区高
        let header = 45usize; // 表头固定
        let pager = 60usize; // 分页栏固定
        let body_bottom = h - pager; // 表体区底部
        // 行内容 = 行号散列（模拟不同数据行），列参与避免单色行歧义
        let pxv = |row: usize, c: usize| -> u8 {
            let x = row
                .wrapping_mul(2654435761)
                .wrapping_add(c.wrapping_mul(97));
            ((x >> 16) ^ (x >> 8) ^ x) as u8
        };
        // 构造：header 固定; body(header..body_bottom) 是滚动内容; pager 固定
        let mk = |scroll: Option<usize>| -> Vec<u8> {
            let mut ap = vec![0u8; w * h * 4];
            for row in 0..h {
                for c in 0..w {
                    let p = (row * w + c) * 4;
                    let v = if row < header || row >= body_bottom {
                        // 固定区：内容完全相同（不随滚动变）
                        pxv(row + 100000, c)
                    } else {
                        // 内容区：滚动后上移 scroll 行
                        let src = if let Some(s) = scroll {
                            if row + s < body_bottom {
                                row + s
                            } else {
                                s + row // 底部补位（模拟新行进入）
                            }
                        } else {
                            row
                        };
                        pxv(src, c)
                    };
                    ap[p] = v;
                    ap[p + 1] = v.wrapping_add(37);
                    ap[p + 2] = v.wrapping_mul(3);
                    ap[p + 3] = 255;
                }
            }
            ap
        };
        // a：无滚动（scroll=None 即内容区不偏移）
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: mk(Some(0)) };
        // 分别测试滚动 30 / 120 / 250 / 450 / 540 行的检测（600 物理上无解，见注释）
        for s in [30usize, 120, 250, 450, 540] {
            let b = CapturedFrame { width: w as u32, height: h as u32, pixels: mk(Some(s)) };
            let r = find_scroll_delta(&a, &b);
            eprintln!("vxe s={s} -> delta={r:?}");
            // 允许 ±1 行（3×3 平均模糊）；若返回 None，说明表头+分页栏固定区把
            // 匹配率稀释到 MIN_MATCH_RATIO 之下 → 引擎测不出真实滚动量（病灶）
            match r {
                Some(got) if got.abs_diff(s) <= 1 => {}
                other => panic!(
                    "vxe-table 滚动 s={s} 应测出 {s}±1，实际 {other:?} —— 固定表头/分页栏稀释导致拼接失败",
                ),
            }
        }
    }

    /// 宽松估计：唯一内容滚动 120 → 必须测出 ~120（即便严格检测因歧义拒掉它）。
    #[test]
    fn estimate_detects_scroll_delta() {
        let a = frame(200, 600);
        let b = scrolled(&a, 200, 600, 120);
        match estimate_scroll_delta(&a, &b) {
            Some(s) if s == 120 || s == 121 => {}
            other => panic!("expected Some(120|121), got {other:?}"),
        }
    }

    /// 宽松估计：同一帧（没滚动）→ 必须 None（不能把「没动」当成滚动去拼）。
    #[test]
    fn estimate_identical_frames_return_none() {
        let a = frame(200, 600);
        let b = frame(200, 600);
        assert_eq!(estimate_scroll_delta(&a, &b), None);
    }

    /// 宽松估计：稀疏纹理（空白夹一条纹理带）滚动 80 → 仍能测出（纹理带钉住偏移）。
    /// 严格检测在稀疏自相似下也常测出，这里确认估计的「宽容」底线不放过真实滚动。
    #[test]
    fn estimate_detects_scroll_with_sparse_texture() {
        let w = 1261usize;
        let h = 312usize;
        let mut ap = vec![0u8; w * h * 4];
        for row in 100..220 {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = px(row, x);
                ap[p] = v;
                ap[p + 1] = v.wrapping_add(37);
                ap[p + 2] = v.wrapping_mul(3);
                ap[p + 3] = 255;
            }
        }
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: ap };
        let mut bp = vec![0u8; w * h * 4];
        for row in 20..140 {
            for x in 0..w {
                let p = (row * w + x) * 4;
                let v = px(row + 80, x);
                bp[p] = v;
                bp[p + 1] = v.wrapping_add(37);
                bp[p + 2] = v.wrapping_mul(3);
                bp[p + 3] = 255;
            }
        }
        let b = CapturedFrame { width: w as u32, height: h as u32, pixels: bp };
        match estimate_scroll_delta(&a, &b) {
            Some(s) if (60..=100).contains(&s) => {}
            other => panic!("expected Some(~80), got {other:?}"),
        }
    }

    /// best_pixel_offset：全范围「重叠带平均像素差最小」→ 精确平移 s。
    /// 真实 s 的重叠带是逐像素一致平移（差 0）；周期整数倍等假偏移下像素差更高，
    /// 所以被正确瓦解、钉回真实平移。
    #[test]
    fn best_pixel_offset_snaps_to_exact_translation() {
        let w = 1083usize;
        let h = 326usize;
        let a = frame(w, h);
        // true_s 需保证重叠带 n = h-true_s >= MIN_OVERLAP(30)，否则真实偏移被（正确地）排除
        for true_s in [4usize, 30, 200, 290] {
            assert!(h - true_s >= 30, "true_s={true_s} 重叠带不足");
            let b = scrolled(&a, w, h, true_s);
            match best_pixel_offset(&a, &b) {
                Some((s, diff)) => {
                    assert_eq!(s, true_s, "true_s={true_s} 应被对准到精确平移");
                    assert!(diff <= 1, "true_s={true_s} 的像素差应接近 0，实得 {diff}");
                }
                None => panic!("true_s={true_s} 应返回 Some"),
            }
        }
    }

    /// 全范围最小像素差：即使粗估落在「周期整数倍」假峰上，也能拉回真实小偏移。
    /// 构造 b = a 向下滚动 shift；再给一个**错误的大粗估**（周期倍数），验证
    /// best_pixel_offset 仍返回真实的 shift（像素差最小）而不是那个周期倍数。
    #[test]
    fn best_pixel_offset_ignores_misleading_coarse_guess() {
        let w = 1083usize;
        let h = 326usize;
        let a = frame(w, h);
        let true_s = 30usize; // 真实小滚动
        let b = scrolled(&a, w, h, true_s);
        // 粗估被周期重复误导到 4×true_s=120（模拟 iter52 s=120）
        let (s, _diff) = best_pixel_offset(&a, &b).expect("应返回 Some");
        assert_eq!(s, true_s, "应拉回真实偏移 {true_s}，而不是周期整数倍 120");
    }

    /// 宽松估计 + vxe-table 固定表头/分页栏：真实滚动 s 仍要测出（估计不能把固定区
    /// 稀释当成「无重叠」而拒绝——手动滚动的兜底正是要靠它不丢段）。
    #[test]
    fn estimate_vxe_table_scroll_delta_detected() {
        let w = 1647usize;
        let h = 661usize;
        let header = 45usize;
        let pager = 60usize;
        let body_bottom = h - pager;
        let pxv = |row: usize, c: usize| -> u8 {
            let x = row
                .wrapping_mul(2654435761)
                .wrapping_add(c.wrapping_mul(97));
            ((x >> 16) ^ (x >> 8) ^ x) as u8
        };
        let mk = |scroll: Option<usize>| -> Vec<u8> {
            let mut ap = vec![0u8; w * h * 4];
            for row in 0..h {
                for c in 0..w {
                    let p = (row * w + c) * 4;
                    let v = if row < header || row >= body_bottom {
                        pxv(row + 100000, c)
                    } else {
                        let src = if let Some(s) = scroll {
                            if row + s < body_bottom {
                                row + s
                            } else {
                                s + row
                            }
                        } else {
                            row
                        };
                        pxv(src, c)
                    };
                    ap[p] = v;
                    ap[p + 1] = v.wrapping_add(37);
                    ap[p + 2] = v.wrapping_mul(3);
                    ap[p + 3] = 255;
                }
            }
            ap
        };
        let a = CapturedFrame { width: w as u32, height: h as u32, pixels: mk(Some(0)) };
        for s in [30usize, 120, 250, 450, 540] {
            let b = CapturedFrame { width: w as u32, height: h as u32, pixels: mk(Some(s)) };
            // 重叠带足够大时，全范围最小像素差也应对准 s（固定表头/分页栏只占少数采样）。
            // s 极大（重叠带 < 200，如 540/661）时固定表头占主导，无法像素级对准——
            // 此时 try_append_scrolled 里 diff<=40 关卡会回退到粗估，不误拼，所以这里只
            // 在重叠带足够大时断言。
            if h - s >= 200 {
                match best_pixel_offset(&a, &b) {
                    Some((bbo, _)) if bbo.abs_diff(s) <= 3 => {}
                    other => panic!(
                        "best_pixel_offset vxe-table 滚动 s={s} 应测出 {s} 附近，实际 {other:?}",
                    ),
                }
            }
            let r = estimate_scroll_delta(&a, &b);
            eprintln!("estimate vxe s={s} -> delta={r:?}");
            match r {
                Some(got) if got.abs_diff(s) <= 1 || got.abs_diff(s) <= 3 => {}
                other => panic!(
                    "estimate vxe-table 滚动 s={s} 应测出 {s} 附近，实际 {other:?}",
                ),
            }
        }
    }
}
