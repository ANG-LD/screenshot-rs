//! 滚动截屏拼接：重叠检测（纯函数，可单测）。
//!
//! 连续两次抓取同一屏幕区域 a、b（内容向下滚动）。向下滚动 s 行意味着
//! 内容上移 s 行，因此 b 的顶部 (h-s) 行等于 a 的底部 (h-s) 行（重叠带），
//! b 的底部 s 行是新进入视口的内容。`find_scroll_delta` 负责找出 s。
//!
//! ## 两层算法
//!
//! **粗层（行级匹配 + 匹配行数计数）**：对每个候选滚动量 s，统计重叠带内
//! 「逐行内容一致」的行数（行匹配 = 8 个采样列的 3×3 局部平均 RGB 差 ≤ 容差），
//! 取**匹配行数最多**的 s。抗噪声/动画中间帧好，但水平分辨率只有 8 列。
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
//!
//! **精层（整行哈希 + 固定栏，见 [`plan_append`]）**：粗层的 8 列采样会漏掉
//! 序号列/图标列这类窄特征，于是周期性内容出现「周期整数倍」假峰（多滚 → 重复
//! 拼接）或真实滚动被判歧义（只拼一页）。精层改用**整行哈希**，并按真实网页的
//! 「固定栏 + 滚动内容区」模型判定：
//! - **固定栏** = a、b 在同一位置逐行完全相同的头部/尾部长；只有**有纹理**的
//!   尾部才算固定底栏（纯色/空白段在任意滚动下都相等，误当底栏会把追加窗口上移
//!   → 拼进已拼过的内容）。固定底栏在追加时排除，长图尾部残留的那份还要裁掉，
//!   否则页脚会在长图里被复制多次。
//! - **打分**只在滚动内容区、只统计**有信息行**（与相邻行不同的行），命中率最高者
//!   胜；候选含 s=0、平票取最小 s —— 内容没动时 s=0 满命中，静止帧与周期假偏移
//!   都无法胜出（宁缺毋滥，不拼重复）。

use std::cell::RefCell;

use super::MIN_SCROLL;
use crate::capture::CapturedFrame;

/// 行匹配采样列数（8 列均匀分布）
const VCOLS: usize = 8;
/// 判定重叠带「有内容」的相邻行平均差阈值（行平均矩阵 24 值差和）
const MIN_ENERGY: u64 = 16;
/// 滚动量上限（超出视为异常，拒绝）
const MAX_SCROLL: usize = 800;
/// 要求重叠带至少保留的行数（太少则无法可靠判定）
pub const MIN_OVERLAP: usize = 30;
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
    /// 精层用的整行哈希（a 帧 / b 帧）
    ha: Vec<u64>,
    hb: Vec<u64>,
    /// `score_candidates` 每偏移的匹配行数（counts[s]），跨调用复用避免反复分配
    counts: Vec<usize>,
    /// 相邻行差 d[r] 及其后缀和 / 后缀纹理计数：把「每偏移现算 band_has_energy」
    /// 这类 O(h) 重复计算压成 O(1)（同一对相邻行被 1000 多个偏移重复 diff）
    energy_d: Vec<u64>,
    energy_sum: Vec<u64>,
    energy_tex: Vec<u32>,
    /// 偏移 s 是否通过能量门（与 band_has_energy 逐位等价）
    energy_ok: Vec<bool>,
}

impl Default for StitchScratch {
    fn default() -> Self {
        Self {
            vcols: Vec::with_capacity(VCOLS),
            ma: Vec::new(),
            mb: Vec::new(),
            cands: Vec::new(),
            ha: Vec::new(),
            hb: Vec::new(),
            counts: Vec::new(),
            energy_d: Vec::new(),
            energy_sum: Vec::new(),
            energy_tex: Vec::new(),
            energy_ok: Vec::new(),
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
        if max_s == 0 {
            return None;
        }
        // ── 能量门（band_has_energy）预计算 ────────────────────────────────────
        // 原实现在每个偏移 s 上现算一次：内部要 diff 相邻行 (s+1..h)，于是同一对
        // 相邻行被上千个 s 反复 diff（总代价 O(h·max_s)，1080p 约百万次 row_avg_diff）。
        // 相邻行差只依赖行号 → 先算一遍 d[r]，再做后缀和 / 后缀纹理计数，每个 s 的
        // 判定降为 O(1)。**判定式逐字复刻**（含 `total / (h-s)` 用的是 h-s、而不是
        // 求和项数 h-1-s 这个细节），结果与原来完全一致。
        self.energy_d.clear();
        self.energy_d.resize(h, 0);
        for r in 1..h {
            self.energy_d[r] = row_avg_diff(&self.ma[r], &self.ma[r - 1]);
        }
        self.energy_sum.clear();
        self.energy_sum.resize(h + 1, 0);
        self.energy_tex.clear();
        self.energy_tex.resize(h + 1, 0);
        for r in (1..h).rev() {
            self.energy_sum[r] = self.energy_sum[r + 1] + self.energy_d[r];
            self.energy_tex[r] = self.energy_tex[r + 1] + u32::from(self.energy_d[r] >= MIN_ENERGY);
        }
        self.energy_ok.clear();
        self.energy_ok.resize(max_s + 1, false);
        for s in 1..=max_s {
            let n = h - s;
            if n < 2 {
                continue;
            }
            // 后缀起点 s+1：total = Σ_{r=s+1..h-1} d[r]，textured 同理
            self.energy_ok[s] = self.energy_sum[s + 1] / n as u64 >= MIN_ENERGY
                || self.energy_tex[s + 1] * 8 >= n as u32;
        }

        // ── 行匹配计数 ───────────────────────────────────────────────────────
        // 循环次序调换：外层走 mb 的行（每行只读一次，留在寄存器里）、内层走 s，
        // 于是 `ma[s + r]` 随 s **顺序前进**——原来外层 s、内层 r 时它是斜对角跳读，
        // 1080p 实测 117 万次比较全卡在 L2/L3 上，单次调用 ~18ms。
        // 比较次数与结果不变：counts[s] = #{r : r < h-s 且行签名匹配}。
        self.counts.clear();
        self.counts.resize(max_s + 1, 0);
        for r in 0..h {
            let mb_r = &self.mb[r];
            let end = (h - r).min(max_s + 1);
            if end < 2 {
                continue;
            }
            // 用切片迭代代替下标：一轮里 3 次下标访问的边界检查全部消失
            let rows = &self.ma[r + 1..r + end];
            let oks = &self.energy_ok[1..end];
            let cnts = &mut self.counts[1..end];
            for ((ma_row, ok), cnt) in rows.iter().zip(oks).zip(cnts.iter_mut()) {
                // 能量门在计数前先过（与原实现一样直接跳过无信息偏移）
                if *ok && row_matches(ma_row, mb_r) {
                    *cnt += 1;
                }
            }
        }
        // push 顺序保持「s 升序」不变：排序是 unstable 的，并列时谁胜出取决于输入
        // 顺序，改顺序会让并列偏移的选择漂移（拼接行为跟着变）。
        for s in 1..=max_s {
            if !self.energy_ok[s] {
                continue;
            }
            self.cands.push((self.counts[s], s));
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
    // 栈上数组：这个函数在滚动循环里每次迭代都被调用，别再为 64 个采样列做堆分配
    let cols: [usize; 64] = std::array::from_fn(|i| w * (i + 1) / 65);
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
    let cols: [usize; 64] = std::array::from_fn(|i| w * (i + 1) / 65);
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
    let cols: [usize; 64] = std::array::from_fn(|i| w * (i + 1) / 65);
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

// ───────────────────────── 精层：整行哈希校验 ─────────────────────────
//
// 粗层（上面的 8 列 × 3 通道 3×3 平均签名）抗噪好，但**水平分辨率只有 8 列**：
// 表格的序号列、进度条、图标列等窄特征可能整列落在采样列之间 → 多行签名相同
// → 要么周期假峰（多滚 → 重复拼接），要么真实滚动被判「没动/歧义」（只拼一页）。
//
// 精层的模型更贴近真实网页：一帧 = **固定栏**（页头/页脚/分页栏/悬浮条，每帧
// 同一位置重复出现、不随内容滚动）+ **滚动内容区**。于是：
//   * 固定栏 = a、b 在同一位置逐行完全相同的头部/尾部长（`fixed_bands`）；
//   * 只有滚动内容区携带「滚了多少」的信息，判定与追加都应**只取内容区**——
//     这同时解决三件事：固定底栏不再被当作新内容重复拼入、首帧残留的底栏从
//     长图中间裁掉、固定栏不参与打分（否则大片固定栏会淹掉真实偏移）。
// 打分用**整行哈希**：静止帧的真实滚动是逐字节平移，真实偏移处内容行全中
// （1000‰），假偏移处全错（0‰）。候选里**含 s=0**，平票取最小 s —— 内容没动
// 时 s=0 天然满命中，假偏移（周期倍数）无法胜出，静止帧自动被否，不会拼重复。

/// 整行哈希的采样步长（每 N 个像素取一个；2 已足够判别，耗时减半）
const HASH_STEP: usize = 2;
/// 固定底栏最短长度：1~3 行的相同只是巧合，不按固定栏处理
const BAND_MIN: usize = 4;
/// 固定栏最长占比（h / 此值）：超过说明整帧大半没动（静止帧），不是「固定栏」
const BAND_MAX_DIV: usize = 2;
/// 精层判「精确对齐可信」的最低命中率（千分比：命中有信息行 / 有信息行总数）
pub const EXACT_PERMILLE_MIN: usize = 500;
/// 判「高置信」：有信息行几乎全中（动态元素只影响少数行）。
///
/// 静止帧的真实滚动是逐字节平移，命中率 1000‰；动画中间帧（亚像素混合）会明显
/// 掉下来。手写滚动时**静止帧很多**（滚轮一格一段动画，动画结束后画面静止），
/// 所以先用高置信结果拼（接缝逐像素对齐），高置信长时间不出现才退回宽判据。
const EXACT_PERMILLE_CONFIDENT: usize = 900;
/// 精层最小可信有信息行数（绝对下限，防小重叠带巧合）
const EXACT_INFO_MIN: usize = 8;

/// 每行的哈希（RGB 逐像素采样，跳过 alpha）。
///
/// 用 FNV-1a 逐字节混合：整行内容参与，窄特征（序号列/图标）不会被漏掉。
/// 行宽也混入哈希尾，避免不同宽度帧截断后同哈希（调用方已校验尺寸，纯保险）。
fn fill_row_hashes(f: &CapturedFrame, w: usize, h: usize, out: &mut Vec<u64>) {
    out.clear();
    out.reserve(h);
    let px = &f.pixels;
    let row_bytes = w * 4;
    // 采样仍是「每 HASH_STEP 个像素取一个」，但做了两件让 CPU 跑满的事：
    //
    // 1. **拆开 FNV 的串行乘法依赖链**：FNV 每步 `hash = (hash ^ w) * PRIME`，上一步的
    //    乘法结果喂下一步，单累加器时整个循环被乘法延迟（~5 周期）卡住——1080p 实测
    //    5.5ms/帧。这里用 4 条独立累加链，乱序执行能并行，末尾再混合（§mix4）。
    // 2. **按 4×HASH_STEP 字节的块遍历**：一次拿到 4 个采样点的定长切片，省掉逐点
    //    下标边界检查（编译器能直接消掉）。步长恰好等于 HASH_STEP 个像素的宽度，
    //    所以采样点与逐点写法**完全一致**。
    //
    // 哈希**数值**因此变了，但「两行是否相同」的判定语义不变：同样的采样点、同样
    // 只由 RGB 决定（掩掉 alpha）。1080p 实测 5.5ms → ~0.9ms/帧。
    const W: usize = 4; // 一次处理 4 个采样点
    let chunk = 4 * HASH_STEP; // 单个采样点的字节宽度（= 一个像素 4 字节 × 步长）
    let block = W * chunk;
    #[inline(always)]
    fn word(part: &[u8], off: usize) -> u64 {
        // 只取 RGB（低 3 字节，掩掉 alpha），与逐通道写法等价
        (u32::from_ne_bytes(part[off..off + 4].try_into().unwrap()) & 0x00FF_FFFF) as u64
    }
    #[inline(always)]
    fn mix(h: u64, w: u64) -> u64 {
        (h ^ w).wrapping_mul(0x100_0000_01b3)
    }
    for r in 0..h {
        let row = &px[r * row_bytes..r * row_bytes + row_bytes];
        let mut h0 = 0xcbf2_9ce4_8422_2325u64;
        let mut h1 = 0x9e37_79b9_7f4a_7c15u64;
        let mut h2 = 0xff51_afd7_ed55_8ccdu64;
        let mut h3 = 0xc4ce_b9fe_1a85_ec53u64;
        let mut it = row.chunks_exact(block);
        for part in &mut it {
            h0 = mix(h0, word(part, 0));
            h1 = mix(h1, word(part, chunk));
            h2 = mix(h2, word(part, chunk * 2));
            h3 = mix(h3, word(part, chunk * 3));
        }
        // 行尾不足一整块的部分：用 `chunks`（不是 chunks_exact）拿到最后那段零头，
        // 只要还剩**至少一个像素**就按原语义哈希该像素的 RGB（掩掉 alpha）；
        // 连一个像素都不够则与原来一样不采样。采样集合与「每 HASH_STEP 个像素取一个」
        // 逐点一致（w 为奇数时也不会漏掉/重复末像素）。
        for part in it.remainder().chunks(chunk) {
            if part.len() >= 4 {
                h0 = mix(h0, word(part, 0));
            }
        }
        // 混合 4 条链：不引入新的「不同行 → 相同哈希」风险，只是把并行结果合起来
        let hash = (h0 ^ h1.rotate_left(17) ^ h2.rotate_left(31) ^ h3.rotate_left(47))
            .wrapping_mul(0x100_0000_01b3)
            ^ (w as u64);
        out.push(hash);
    }
}

/// 该行是否「有信息」：与相邻行（前或后）哈希不同。
///
/// 纯色/空白行**在任何偏移下都相等**，把它们算进命中率会给「小偏移」白送一大截
/// 分数（重叠带更大、白送的行更多）→ 精层会选到过小的偏移 → 长图缺行。
/// 只统计有信息行后：真实偏移处有信息行全中，假偏移处全错，判别力远强于
/// 「命中行数最多」。
#[inline]
fn row_informative(hashes: &[u64], i: usize) -> bool {
    let prev_diff = i > 0 && hashes[i] != hashes[i - 1];
    let next_diff = i + 1 < hashes.len() && hashes[i] != hashes[i + 1];
    prev_diff || next_diff
}

/// 固定栏：a、b 在**同一位置逐行完全相同**的头部/尾部长（`(头部, 尾部)`）。
///
/// 这些行不随内容滚动（页头、页脚、分页栏、悬浮条、播放控制条），是「固定栏」；
/// 中间剩下的就是滚动内容区。上限各 h/2（超过说明整帧基本没动）。
pub fn fixed_bands(a: &CapturedFrame, b: &CapturedFrame) -> (usize, usize) {
    if a.width != b.width || a.height != b.height {
        return (0, 0);
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let mut ha = Vec::new();
    let mut hb = Vec::new();
    fill_row_hashes(a, w, h, &mut ha);
    fill_row_hashes(b, w, h, &mut hb);
    bands_from_hashes(&ha, &hb, h)
}

/// 从整行哈希流求固定栏长度（见 [`fixed_bands`]）。哈希是 64 位 FNV，碰撞概率可忽略。
fn bands_from_hashes(ha: &[u64], hb: &[u64], h: usize) -> (usize, usize) {
    let limit = h / BAND_MAX_DIV;
    let mut ft = 0;
    while ft < limit && ha[ft] == hb[ft] {
        ft += 1;
    }
    let mut fb = 0;
    while fb < limit && ha[h - 1 - fb] == hb[h - 1 - fb] {
        fb += 1;
    }
    (ft, fb)
}

/// 该行段是否有纹理（相邻行存在差异）：真实 UI 栏（页脚/分页栏/按钮条）内部一定有
/// 文字、边框、按钮等差异行；**纯色/空白段**没有。
///
/// 这个区分很关键：纯色/空白段在 a、b 里同一位置本来就相等（与滚动无关），若当成
/// 「固定底栏」排除，追加窗口会整体上移 k 行 → 拼进去的是**已经拼过的内容**（重复），
/// 而真正的底栏必须排除（否则每帧复制一条进长图）。所以只把**有纹理**的段当固定栏。
fn run_textured(hashes: &[u64], from: usize, to: usize) -> bool {
    let len = to.saturating_sub(from);
    if len < 2 {
        return false;
    }
    let mut changes = 0usize;
    for i in (from + 1)..to {
        if hashes[i] != hashes[i - 1] {
            changes += 1;
        }
    }
    // 只要段内存在两处相邻行差异就算有纹理：真实 UI 栏必然有边线/文字/按钮，
    // 而纯色/空白段的相邻行**完全相同**（0 处差异）。段本身已由「同一位置逐行相等」
    // 筛过，所以这里不需要比例门槛。
    changes >= 2
}

/// 固定底栏行数：帧底「与上一帧同一位置逐行相同」的连续行数。
///
/// 这里刻意**宁可多算**（不剔除纯色/空白行），因为两个方向的代价完全不对等：
/// - **多算**（底栏上方的空白内容也算成底栏）：只有首帧尾部那段底栏会被多裁一点，
///   每次追加仍正好补上滚动量，接缝严格连续——少掉的是纯色/空白行，肉眼无差别。
/// - **少算**（真正的底栏没算全）：追加窗口会越过内容末尾往回退，每次拼接都重复几行
///   **可见内容**——就是用户反馈的「长图最后一页和倒数第二页部分内容重复」。
///
/// 曾经这里额外剔除了「段首纯色块」（为了对付底栏上方的空白）。问题在于真实页脚常常
/// 是「整条纯色 + 顶部细线」，纯色块就是底栏自己的一部分，减掉它就把底栏判矮了——
/// 于是又回到上面那条灾难路径。回归测试见
/// `scroll::manual_diag_tests::synth_solid_leading_footer_does_not_duplicate_content`。
pub fn fixed_bottom_band(a: &CapturedFrame, b: &CapturedFrame) -> usize {
    if a.width != b.width || a.height != b.height {
        return 0;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return 0;
    }
    let mut ha = Vec::new();
    let mut hb = Vec::new();
    fill_row_hashes(a, w, h, &mut ha);
    fill_row_hashes(b, w, h, &mut hb);
    band_from_identical_tail(&hb, &ha, h)
}

/// 供**判定阶段**使用的底栏估计（此时还没测出滚动量，也还没有长图可对照）。
///
/// 与 [`fixed_bottom_band`] 不同，这里会剔掉尾部相同段**段首的纯色/空白块**：判定阶段
/// 需要尽量多的「有信息行」，纯色块不含信息，还会把内容区压小 → 大滚动量被 `max_s` 拒掉。
/// 只用于算命中率的分母，不决定追加窗口位置（那个必须用「宁可多算」的
/// [`fixed_bottom_band`]，否则会重复拼接可见内容）。
fn band_for_metric(ha: &[u64], hb: &[u64], h: usize) -> usize {
    let (_, fb) = bands_from_hashes(ha, hb, h);
    if fb < BAND_MIN {
        return 0;
    }
    let from = h - fb;
    let mut uniform = 1;
    while uniform < fb && hb[from + uniform] == hb[from] {
        uniform += 1;
    }
    let uniform = if uniform >= 2 { uniform } else { 0 };
    let band = fb - uniform;
    if band < BAND_MIN || !run_textured(hb, h - band, h) {
        return 0;
    }
    band
}

/// 帧底与上一帧同一位置逐行相同的连续行数（见 [`fixed_bottom_band`] 的语义）。
fn band_from_identical_tail(hb: &[u64], ha: &[u64], h: usize) -> usize {
    let limit = h / BAND_MAX_DIV;
    let mut band = 0usize;
    while band < limit && hb[h - 1 - band] == ha[h - 1 - band] {
        band += 1;
    }
    if band < BAND_MIN {
        0
    } else {
        band
    }
}


/// 反向（向上滚）判定：当前帧 `cur` 的内容相对上一次基线 `prev` 是**向上**移动的。
///
/// 只认精层逐字节对齐（命中率高）的结果。为什么不能用粗层：粗层签名对「向下滚」
/// 的一对帧也会给出 1~30 的噪声偏移（周期性内容 / 模糊帧），一旦把它当成「向上滚」，
/// 引擎每轮都会走进「保持基线、不拼接」的分支——用户明明在向下滚，静止帧却全被
/// 白白错过，长图最后只剩第一页。
pub fn scroll_up_delta(cur: &CapturedFrame, prev: &CapturedFrame) -> Option<usize> {
    let plan = plan_append(cur, prev)?;
    if plan.s >= MIN_SCROLL && plan.via_exact && plan.permille >= EXACT_PERMILLE_MIN {
        Some(plan.s)
    } else {
        None
    }
}

/// 精层决策结果（见 [`plan_append`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendPlan {
    /// 滚动量（行）：本帧新进入视口的内容行数
    pub s: usize,
    /// 固定底栏行数：帧末尾这么多行不参与滚动，追加时须排除
    pub band: usize,
    /// 固定顶栏行数（日志/诊断用）
    pub top: usize,
    /// 精层有信息行数（日志/诊断用）
    pub exact: usize,
    /// 精层有信息行命中率（千分比）
    pub permille: usize,
    /// 是否由精层确认（false = 由粗层采样签名给出）
    pub via_exact: bool,
    /// 高置信：精层有信息行几乎全中（接缝可保证逐像素对齐）
    pub confident: bool,
}

impl StitchScratch {
    /// 精层：在滚动内容区里找**有信息行命中率**最高的偏移，返回 (s, 命中率‰, 有信息行数)。
    ///
    /// 候选从 0 开始（0 = 内容没动）；打分只在内容区 `[ft, ft + content_h - s)` 内进行，
    /// 命中率更高者胜、平票保留更小的 s（周期内容取最小倍数不跳过内容；内容没动时
    /// s=0 满命中，天然否决周期假偏移）。
    fn exact_aligned_offset(
        &mut self,
        ft: usize,
        content_h: usize,
        max_s: usize,
    ) -> Option<(usize, usize, usize)> {
        let mut best: Option<(usize, usize, usize)> = None;
        for s in 0..=max_s {
            let n = content_h.saturating_sub(s);
            if n < EXACT_INFO_MIN {
                break;
            }
            let mut info = 0usize;
            let mut hit = 0usize;
            for i in ft..ft + n {
                if !(row_informative(&self.hb, i) || row_informative(&self.ha, s + i)) {
                    continue;
                }
                info += 1;
                if self.hb[i] == self.ha[s + i] {
                    hit += 1;
                }
            }
            if info < EXACT_INFO_MIN {
                continue;
            }
            let permille = hit * 1000 / info;
            if best.is_none_or(|(_, bp, _)| permille > bp) {
                best = Some((s, permille, info));
            }
        }
        best
    }

    /// 追加行（内容区末尾 s 行，即 `[ft + content_h - s, ft + content_h)`）里有多少行是
    /// anchor **已经显示过的内容**（整行哈希命中 anchor 的内容区）。
    ///
    /// 真实滚动追加的是「从未出现过」的新内容 → 该值应接近 0；周期性内容的假大偏移
    /// 会把 anchor 里已有的行再拼一遍（重复拼接）→ 该值很高。
    fn duplicated_rows(&self, s: usize, ft: usize, content_h: usize) -> usize {
        if content_h == 0 || s == 0 {
            return 0;
        }
        let start = ft + content_h - s.min(content_h);
        let end = ft + content_h;
        let mut dup = 0usize;
        for i in start..end.min(self.hb.len()) {
            let hash = self.hb[i];
            if self.ha[ft..end.min(self.ha.len())].contains(&hash) {
                dup += 1;
            }
        }
        dup
    }
}

/// 决定这一帧该怎么拼：**精层优先**（整行哈希确认 + 固定栏识别），精层不够可信时
/// 回落粗层（采样签名，抗噪但分辨率低）。
///
/// 返回 `None` = 这一帧不可可靠拼接，调用方应保留基线等下一帧（宁缺毋滥）：
///   * 内容没动（含「只有固定栏在动」的静止帧）——精层 s=0 胜出即判否；
///   * 内容区太小（整帧几乎都是固定栏）；
///   * 精层与粗层都测不出可靠偏移；
///   * 粗层给出的偏移疑似重复拼接（追加行大段是 anchor 已有内容）。
pub fn plan_append(anchor: &CapturedFrame, frame: &CapturedFrame) -> Option<AppendPlan> {
    if anchor.width != frame.width || anchor.height != frame.height {
        return None;
    }
    let w = anchor.width as usize;
    let h = anchor.height as usize;
    if w == 0 || h == 0 {
        return None;
    }
    SCRATCH.with(|sc| {
        let mut sc = sc.borrow_mut();
        // 整行哈希只算一遍：固定栏、纹理判定、打分都用它（省掉重复的全帧扫描）。
        fill_row_hashes(anchor, w, h, &mut sc.ha);
        fill_row_hashes(frame, w, h, &mut sc.hb);
        // 固定栏：帧首/帧尾在同一位置逐行相同 → 不随内容滚动。**底栏必须排除**，
        // 否则每拼一帧就复制一条底栏进长图（判矮还会让窗口往回退、重复可见内容）。
        let (raw_ft, _) = bands_from_hashes(&sc.ha, &sc.hb, h);
        // 顶栏同理：顶栏里与内容区相邻的纯色/空白块不含信息，还会把内容区压小
        // （大滚动量被 max_s 拒掉 = 「只拼一页」），因此从**判定起点**里去掉；
        // `plan.top` 仍报告识别到的顶栏高度。
        let mut ft = raw_ft;
        while ft > 0 && sc.hb[ft - 1] == sc.hb[ft] {
            ft -= 1;
        }
        // 判定阶段先用 `band_for_metric`（尾部相同段 − 纯色块）估底栏：这里只需要内容区
        // 里有足够多「有信息行」，纯色块会把内容区压小、把大滚动量拒掉。真正决定追加
        // 窗口位置的是返回值里的 `band`，用的是「宁可多算」的 `band_from_identical_tail`
        //（见 `fixed_bottom_band`：判矮会重复拼入可见内容，判高只是少几行纯色）。
        let band = band_for_metric(&sc.ha, &sc.hb, h);
        let content_h = h.saturating_sub(ft).saturating_sub(band);
        if content_h < MIN_SCROLL + EXACT_INFO_MIN {
            // 内容区太小：整帧几乎没动（静止帧 ft+fb≈h）或全被固定栏占满 → 无法判定
            return None;
        }
        let max_s = content_h.saturating_sub(EXACT_INFO_MIN).min(MAX_SCROLL);
        if max_s < MIN_SCROLL {
            return None;
        }
        let exact = sc.exact_aligned_offset(ft, content_h, max_s);
        let (s, info, permille, via_exact) = match exact {
            // 精层满命中但指向「没动」（s=0 胜出）／内容区无信息行 → 内容确实没滚，
            // 直接否（静止帧不拼重复）；仅当粗层给出可信偏移时才用粗层结果。
            Some((0, _, _)) | None => match sc.find_scroll_delta(anchor, frame) {
                Some(s_c) if s_c >= MIN_SCROLL => (s_c, 0, 0, false),
                _ => return None,
            },
            Some((s_e, permille, info)) if permille >= EXACT_PERMILLE_MIN => {
                (s_e, info, permille, true)
            }
            Some((s_e, permille, info)) => {
                // 精层命中率不高：动画中间帧（亚像素混合 → 逐字节不等）居多，交粗层。
                match sc.find_scroll_delta(anchor, frame) {
                    // 粗层与精层指向同一偏移 → 互相印证，采信精层
                    Some(s_c) if s_c.abs_diff(s_e) <= 2 => (s_e, info, permille, true),
                    Some(s_c) => (s_c, info, permille, false),
                    // 粗层测不出但精层有可观命中（≥1/3 有信息行）→ 救回真实滚动。
                    // 这正是「真实滚动被粗层严格门拒掉 → 只拼一页」的场景。
                    None if permille >= EXACT_PERMILLE_MIN * 2 / 3 => (s_e, info, permille, true),
                    None => return None,
                }
            }
        };
        if s < MIN_SCROLL || s > content_h {
            return None;
        }
        // 重复拼接否决：只用于**粗层**给出的偏移（精层已被逐字节确认）。
        // 周期假峰会给出「多滚一截」的偏移，其追加行大段是 anchor 已有内容。
        if !via_exact {
            let dup = sc.duplicated_rows(s, ft, content_h);
            if s >= EXACT_INFO_MIN && dup * 2 > s {
                tracing::debug!("plan: reject_dup s={s} dup={dup} new={s} band={band}");
                return None;
            }
        }
        // 追加窗口用「宁可多算」的底栏判定（见 fixed_bottom_band），理由见那里的注释。
        let band = band_from_identical_tail(&sc.hb, &sc.ha, h);
        Some(AppendPlan {
            s,
            band,
            top: raw_ft,
            exact: info,
            permille,
            via_exact,
            confident: via_exact && permille >= EXACT_PERMILLE_CONFIDENT,
        })
    })
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
///
/// 注意：**生产路径**（`score_candidates`）已把这套判定改写为「相邻行差后缀和 +
/// 后缀纹理计数」的 O(1) 形式（同一对相邻行原先会被上千个偏移重复 diff）。
/// 这里保留逐偏移现算的朴素版本，只作为等价性测试的参照实现。
#[cfg(test)]
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

    /// 性能改造的**等价性护栏**：`score_candidates` 现在用「后缀和预计算能量门 +
    /// 交换循环次序 + 切片迭代」重写（1080p 实测 18ms → 5.8ms），这里按**原始写法**
    /// （外层 s、内层 r、每个 s 现算 band_has_energy）重算候选表逐项比对：
    /// best 偏移、候选顺序都必须完全一致——否则并列偏移的胜出项会漂移、拼接行为跟着变。
    #[test]
    fn score_candidates_equivalent_to_naive_reference() {
        fn naive(
            ma: &[RowAvg],
            mb: &[RowAvg],
            h: usize,
            max_s: usize,
        ) -> (Option<(usize, usize)>, Vec<(usize, usize)>) {
            let mut cands: Vec<(usize, usize)> = Vec::new();
            for s in 1..=max_s {
                if !band_has_energy(ma, s, h) {
                    continue;
                }
                let n = h - s;
                let mut count = 0usize;
                for r in 0..n {
                    if row_matches(&ma[s + r], &mb[r]) {
                        count += 1;
                    }
                }
                cands.push((count, s));
            }
            cands.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            (cands.first().copied(), cands)
        }

        const W: usize = 160;
        const H: usize = 240;
        // 纯色帧：能量门应当大面积拒绝（覆盖 n<2 / 无纹理分支）
        let mut blank = frame(W, H);
        for r in 0..H {
            for c in 0..W {
                let p = (r * W + c) * 4;
                blank.pixels[p] = 240;
                blank.pixels[p + 1] = 240;
                blank.pixels[p + 2] = 240;
            }
        }
        // 周期帧：制造大量并列候选（排序顺序敏感的场景）
        let mut pa = frame(W, H);
        let mut pb = frame(W, H);
        for f in [&mut pa, &mut pb] {
            for r in 0..H {
                for c in 0..W {
                    let p = (r * W + c) * 4;
                    let v = ((r % 12) * 20 + (c % 7) * 3) as u8;
                    f.pixels[p] = v;
                    f.pixels[p + 1] = v.wrapping_add(50);
                    f.pixels[p + 2] = v.wrapping_mul(5);
                }
            }
        }
        let textured = frame(W, H);
        let cases: Vec<(&str, CapturedFrame, CapturedFrame)> = vec![
            ("textured", textured.clone(), scrolled(&textured, W, H, 70)),
            ("blank", blank.clone(), blank.clone()),
            ("periodic", pa, pb),
        ];

        for (label, a, b) in cases {
            let h = a.height as usize;
            let max_s = h.saturating_sub(MIN_OVERLAP);
            let mut sc = StitchScratch::default();
            fill_vcols(W, &mut sc.vcols);
            fill_row_avgs(&a, W, h, &sc.vcols, &mut sc.ma);
            fill_row_avgs(&b, W, h, &sc.vcols, &mut sc.mb);
            let got = sc.score_candidates(h, max_s);
            let (want_best, want_cands) = naive(&sc.ma, &sc.mb, h, max_s);
            assert_eq!(got, want_best, "{label}: best 偏移不一致");
            assert_eq!(sc.cands, want_cands, "{label}: 候选表逐项不一致");
        }
    }

    /// 整行哈希的**语义契约**（性能重写后必须继续成立）：
    /// 只由「每 HASH_STEP 个像素取一个」的采样点 **RGB** 决定——
    ///  - 采样点 RGB 相同（alpha 不同、或未采样像素不同）→ 哈希必须相同；
    ///  - 采样点 RGB 不同 → 哈希必须不同。
    /// 这层契约是固定栏/滚动量判定的基础（哈希值本身可以随实现变，相等关系不能变）。
    #[test]
    fn row_hash_semantics_only_sampled_rgb() {
        for &w in &[8usize, 9, 62, 63, 64, 65, 160] {
            let h = 4usize;
            let mk = || CapturedFrame {
                width: w as u32,
                height: h as u32,
                pixels: vec![0u8; w * h * 4],
            };
            let mut base = mk();
            for r in 0..h {
                for c in 0..w {
                    let p = (r * w + c) * 4;
                    base.pixels[p] = (r as u8).wrapping_mul(31).wrapping_add(c as u8);
                    base.pixels[p + 1] = 90;
                    base.pixels[p + 2] = 200;
                    base.pixels[p + 3] = 255;
                }
            }
            let mut ha = Vec::new();
            let mut hb = Vec::new();
            fill_row_hashes(&base, w, h, &mut ha);

            // alpha 变了：不参与哈希
            let mut alpha = base.clone();
            for p in (3..alpha.pixels.len()).step_by(4) {
                alpha.pixels[p] = 7;
            }
            fill_row_hashes(&alpha, w, h, &mut hb);
            assert_eq!(ha, hb, "w={w}: alpha 不应影响行哈希");

            // 未采样像素（采样点为 0,2,4...，改第 1、3、5... 个像素）：不参与哈希
            let mut unsampled = base.clone();
            for r in 0..h {
                let mut c = 1;
                while c < w {
                    let p = (r * w + c) * 4;
                    unsampled.pixels[p] = unsampled.pixels[p].wrapping_add(77);
                    c += 2;
                }
            }
            fill_row_hashes(&unsampled, w, h, &mut hb);
            assert_eq!(ha, hb, "w={w}: 未采样像素不应影响行哈希");

            // 采样像素 RGB 变了：必须影响哈希
            let mut sampled = base.clone();
            sampled.pixels[0] = sampled.pixels[0].wrapping_add(13);
            fill_row_hashes(&sampled, w, h, &mut hb);
            assert_ne!(ha, hb, "w={w}: 采样像素 RGB 必须影响行哈希");
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
