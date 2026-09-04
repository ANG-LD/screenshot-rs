//! 滚动截屏（长截图）引擎
//!
//! 主循环：抓取同一屏幕区域 → 重叠检测拼接 → XTest 注入滚轮 → 循环，
//! 直到内容不再变化 / 用户取消 / 达到高度上限。
//!
//! 坐标语义：选区是主屏相对物理像素（与 `capture_primary` 的 frame 一致），
//! 直接传给 `capture_area`；只有 XTest 指针 warp 需要绝对屏幕坐标（本实现按
//! 主屏 origin=(0,0) 处理，多数 X11 布局成立）。

pub mod stitch;
pub mod xtest;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::capture::{CapturedFrame, ScreenCapture};
use crate::error::{AppError, AppResult};
use crate::utils::bounds::Bounds;

/// 遮罩窗口销毁后等待桌面恢复，避免首帧抓到遮罩残留
const STARTUP_DELAY: Duration = Duration::from_millis(250);
/// 每轮滚动后等待平滑滚动稳定。
///
/// Windows 上浏览器默认平滑滚动（滚轮触发动画），动画未结束就抓帧会得到中间态，
/// 重叠带对不上 → 检测失败 → 丢段（长图中间缺内容）。因此 Windows 要多等滚动
/// 完全落定再抓帧。Linux/XTest 是离散滚轮事件，120ms 足够。
#[cfg(target_os = "windows")]
const SETTLE_DELAY: Duration = Duration::from_millis(300);
#[cfg(not(target_os = "windows"))]
const SETTLE_DELAY: Duration = Duration::from_millis(150);
/// 检测失败后（疑似动画未结束）额外等待再重抓一帧
#[cfg(target_os = "windows")]
const EXTRA_SETTLE: Duration = Duration::from_millis(450);
#[cfg(not(target_os = "windows"))]
const EXTRA_SETTLE: Duration = Duration::from_millis(250);
/// 每轮注入的滚轮 tick 数（默认值；检测不到时自适应减半）。
///
/// Windows 每 tick 滚动量更大/更不稳定，减到 1 格让重叠带更大、检测更可靠，
/// 减少「滚了但检测不到 → 丢段」的发生。
#[cfg(target_os = "windows")]
const TICKS_PER_ITER: u8 = 1;
#[cfg(not(target_os = "windows"))]
const TICKS_PER_ITER: u8 = 5;
/// 最小有效滚动量（低于视为内容没动）
const MIN_SCROLL: usize = 4;
/// 拼接高度上限（防无限滚动 feed 死循环；MAX_ITERS 提供最终兜底）
const MAX_HEIGHT: u32 = 100_000;
/// 最大迭代次数
const MAX_ITERS: usize = 300;
/// 连续无法检测滚动的次数上限（只对「有纹理且静止」累计，动画/步长问题不算）
const MAX_STREAK: usize = 3;
/// 判定「页面到底」前等待新数据渲染的时间（列表页 AJAX 分页加载）。
///
/// 滚动到底后 ERP/后台列表常异步加载下一页，立即判定「到底」会把还能继续的
/// 长列表误停（自动模式只拼一页）。等待窗口内若内容变化（新数据渲染完成）
/// 则复活继续滚；无变化才是真的到底。
const LOADING_WAIT: Duration = Duration::from_millis(2000);
/// 连续低纹理（空白/纯色/平滑段）迭代次数上限——此时无法判定是否在滚动，
/// 先按「还在滚」继续，达到配额仍无变化才放弃。
///
/// 主要靠空白段的「全屏是否还在动」判断提前停（页面到底），这里是兜底：
/// 屏幕若有无关动画导致全屏总在变，才靠它收敛。25×BLANK_TICKS≈7.5 视口高度，
/// 足够滚过稀疏页面的长空白段。
const MAX_BLANK_STREAK: usize = 25;
/// 相邻采样行平均差的阈值：低于此值视为低纹理（空白/纯色/平滑图）
const TEXTURED_ENERGY: f32 = 5.0;/// 低纹理段每轮注入的滚轮 tick 数：大步长快速滚过空白区
const BLANK_TICKS: u8 = 6;
/// 重定位指针时注入的滚轮 tick 数。
///
/// 6 格（~300px/约 6 行在视口 22 行中占 >25%）足以让 frames_differ（1/4 采样行变化）
/// 触发，确认「这个位置能滚」，无需 10 格（10 格 × 80ms 拖慢 relocate 到约
/// 1.2s/候选，到底是 15 个候选 ≈15s 才停——用户等待超时）。6 格即能确认，单候选也更
/// 快，缩短「到底后确认无法再滚」的时间。
const RELOCATE_TICKS: u8 = 6;

/// 重定位（relocate）确认某位置能滚后，主循环回到该位置继续拼接所用的滚轮 tick 数
/// 上限：直接用 `RELOCATE_TICKS`（10 格）——4 格等小步长对 vxe-table 等虚拟滚动表格
/// 不响应，是「自动只拼一页」的主因；大滚动重叠带被固定元素稀释 → 严格检测测不出
/// → 由 `estimate_scroll_delta` 兜底，不怕大步长。
/// 重定位后的稳定等待。
///
/// 必须 ≥ Chrome 平滑滚动动画时长（~300ms）：relocate 判定依赖 find_scroll_delta
/// 测出滚动量——动画未落定的模糊帧匹配率低 → delta=None → 误判「滚不动」。
/// 150ms 实测全失败（09:36 日志 15 候选 delta 全 None），300ms 能测出（09:18）。
const RELOCATE_SETTLE: Duration = Duration::from_millis(300);
/// X11 warp 指针后、注入滚轮前的 hover 稳定等待：Chromium 需要处理 MotionNotify
/// 更新 hover 元素，立即注入会作用在旧元素上 → 合成滚轮间歇性失效
const HOVER_SETTLE: Duration = Duration::from_millis(50);
/// 重开连接后、注入前等焦点稳定：Electron/Chromium 需要时间注册焦点才接受合成滚轮
const RECONNECT_FOCUS_SETTLE: Duration = Duration::from_millis(100);
/// 重连每批注入的 tick 数（慢速注入，避免被 Chromium 当 fling 丢弃；
/// 也要足够大到 frames_differ/delta 能判定移动——2 格对低敏感页面不足）
const RECONNECT_TICKS: u8 = 4;
/// 重连每批注入后的验证等待
const RECONNECT_VERIFY_WAIT: Duration = Duration::from_millis(200);
/// 重连注入总批数（每批后验证内容是否移动，动了立即复活）
const RECONNECT_ROUNDS: usize = 3;
/// 暂停期间内容静止且指针不回来时允许的最大迭代数（50ms×100 ≈ 5s）。
/// 超时视为本次滚动已卡死，干净收尾返回已拼接结果，避免无限空转。
const MAX_PAUSED_STATIC: usize = 100;
/// 暂停期间判定「已到达页面底部」所需的连续空白帧数（50ms×3 ≈ 150ms）。
///
/// 与主循环的 MAX_BLANK_STREAK 一致地要求多帧证据：单帧空白可能是平滑滚动
/// 中间帧 / DWM 合成间隙 / 瞬时捕获失败，立即停止会提前终止尚未到底的长图
/// （拼接不全）；连续多帧「空白且静止」才可信为真的到底。
const PAUSED_BLANK_REQUIRED: usize = 3;

/// 指针偏离注入目标多少像素视为「用户要把鼠标划到别处」（如点进度窗按钮）：
/// 此时暂停注入/重定位，让指针自由移动，回到目标附近再自动恢复
const PAUSE_RADIUS: f64 = 60.0;
/// 暂停期间的轮询间隔
const PAUSE_POLL: Duration = Duration::from_millis(50);
/// 手动滚动模式的轮询间隔。
///
/// 越短越能抓到滚动过程中的中间帧（滚太快时一轮轮询就滚超视口 → 无重叠 → 丢段）。
/// 受抓帧耗时限制（Windows GDI 区域抓帧约 20-40ms），12ms 即可使迭代周期减半。
const MANUAL_POLL: Duration = Duration::from_millis(12);
/// 「完成」按钮灰/亮去抖所需连续同向帧数。光标闪烁等瞬时帧差会让 moving 高频翻转，
/// 太多抖动；连续这么多帧同向才更新 moving（约 12ms×MOVING_DEBOUNCE 的稳定窗口），
/// 既不闪按钮，也能及时反映真正的滚动/静止切换。
const MOVING_DEBOUNCE: u32 = 4;
/// 手动模式判定大 delta 可信所需的帧间最大像素差：超过半屏的滚动量，只有内容
/// 确实变化才可信；未滚动（帧几乎相同）会被 find_scroll_delta 因空白自相似误报
/// 为大偏移，此时 maxdiff ≈ 0，跳过拼接避免重复。
///
/// 启动期假大偏移已由「取首帧前稳定等待」挡掉，这里只需要拦「帧几乎相同」的
/// 假匹配，因此阈值取低（16，与 frames_differ 的 24 同量级），避免误伤真实快速
/// 滚动（稀疏页面 maxdiff 偏小，过高的阈值会把真实滚动量也拒掉 → 拼接缺失）。
const CREDIBLE_DIFF: u8 = 16;
/// 整帧**未对齐**平均每像素差（同坐标 a vs b）低于此值 → 判「几乎没变」（静止/闪烁，
/// b≈a），拒绝拼接。这是区分「静止帧被误取偏移重复拼接」与「真实滚动」的判据说明：
/// `frames_differ`（行匹配比例）对 vxe-table 自相似行失效，改用 `mean_unaligned_diff`。
///
/// 阈值经验值：真实滚动（内容下移，同位置像素换成下方内容）unaligned 明显偏高——
/// 本页面实测 ≥21（iter 69 u=21、iter 78 u=23、iter 136 u=27、其余 26~55）；
/// 而「滚到底后内容静止」被误判出的小偏移（拼接重复）unaligned 只有 ≤5
/// （iter 70 u=5、iter 175 u=4、iter 182 u=2、iter 174 u=1）。故阈值定在 12——
/// 拦下所有近静止假偏移，保留全部真实滚动（≥21），中间留足余量。
const TRULY_STATIC_MIN: u64 = 12;
/// 大偏移必须伴随大幅内容变化。周期性内容（表格行/列表）会给「几乎没动」的帧骗出一
/// 个**整数倍大偏移 s**（> 半视口），而同一位置像素几乎相同（unaligned 很低）——此时
/// 内容不可能滚了半屏，属于假偏移，拼接会把整条重叠带重复拼入（小块周期性重复）。
/// 仅当 unaligned 低于此值才拒绝大 s；真实大滚动内容变化大，unaligned 明显偏高，不受影响。
const LARGE_S_MAX_STATIC_UNALIGNED: u64 = 30;
/// 判定「大偏移」的行数占比：s 超过半屏即视为大偏移（周期性内容容易在此出现假峰）。
const LARGE_S_FRACTION: u64 = 2;
/// 手动模式取首帧前的稳定等待：连续两帧 max_frame_diff ≤ 此值视为屏幕已静止。
/// 遮罩关闭/进度窗出现的过渡期帧差异大，直接取首帧会让 find_scroll_delta 误报
/// 大滚动量（空白自相似）→ 重复拼接。
const STARTUP_STABLE_DIFF: u8 = 16;
/// 稳定等待每轮的间隔
const STARTUP_STABLE_POLL: Duration = Duration::from_millis(50);
/// 稳定等待的最大轮数（50ms×10 ≈ 0.5s；超时用最近一帧兜底，不强求静止）
const STARTUP_STABLE_ATTEMPTS: usize = 10;
/// 手动模式最大迭代次数（50ms × 20k ≈ 16 分钟，纯兜底；正常由用户点「完成」结束）
const MAX_MANUAL_ITERS: usize = 20_000;

/// 滚动期间进度窗口的显示/隐藏回调（由调用方经 OverlayService 注入）
pub trait ScrollProgress: Send + Sync {
    /// 打开自动滚动进度小窗（摆到不与 region 重叠的屏幕角落）。
    /// `done` 由用户点「完成」置 true：自动模式结束滚动并生成拼接内容到剪贴板；
    /// `cancel` 置 true 则直接取消、不生成。（自动模式引擎每轮自动滚动，故「完成」
    /// 按钮始终可点；用户随时可停止并把已拼接部分复制出去。）
    fn show(
        &self,
        region: &Bounds,
        screen_bounds: &Bounds,
        cancel: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
    );
    /// 打开手动滚动进度小窗：多一个「完成」按钮，用户滚完点它结束。
    /// `moving` 由引擎每轮按「相邻帧是否不同」更新，进度窗据此只在内容静止时
    /// 显示「完成」按钮（避免滚动动画中途误点，导致最后一段没拼进去）。
    /// `bottom_has_content` 由引擎每轮更新：最近一帧底部是否还有内容。点「完成」
    /// 时若为 true，进度窗先弹确认（可能还没滚到底），避免提前结束导致拼接缺底。
    /// `confirming` 为确认态标志：用户继续滚动时由引擎自动复位。
    #[allow(clippy::too_many_arguments)]
    fn show_manual(
        &self,
        region: &Bounds,
        screen_bounds: &Bounds,
        cancel: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
        moving: Arc<AtomicBool>,
        bottom_has_content: Arc<AtomicBool>,
        confirming: Arc<AtomicBool>,
    );
    /// 关闭进度小窗
    fn hide(&self);
}

/// 运行滚动截屏并返回拼接好的长图（调用方负责写剪贴板）。
///
/// 取消时返回 None（不生成到剪贴板）；「完成」或正常到底返回 Some(拼接结果)。
pub fn run_scroll_capture(
    region: &Bounds,
    screen_bounds: &Bounds,
    capture: &dyn ScreenCapture,
    progress: &dyn ScrollProgress,
) -> AppResult<Option<CapturedFrame>> {
    let (x, y, w, h) = (
        region.origin.x as i32,
        region.origin.y as i32,
        region.size.x.max(1.0) as u32,
        region.size.y.max(1.0) as u32,
    );
    if w < 8 || h < 8 {
        return Err(AppError::Window("选区太小，无法滚动截屏".into()));
    }

    // 等遮罩窗口销毁完成，桌面恢复原样
    std::thread::sleep(STARTUP_DELAY);

    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let progress_h = Arc::new(AtomicU32::new(h));
    progress.show(region, screen_bounds, cancel.clone(), done.clone(), progress_h.clone());

    // 内部闭包包住主循环：任何 `?` 提前退出，外层都统一 hide 进度窗，
    // 避免 capture_area 失败时进度窗卡在屏幕上。
    let result = (|| -> AppResult<Option<CapturedFrame>> {
        // 注入器：X11 专用；非 Linux 直接报错
        let mut injector = new_injector()?;

        // 指针移到选区中心：滚轮事件投递给该位置下的窗口
        let center_x = x + (w as i32) / 2;
        let center_y = y + (h as i32) / 2;
        // 当前指针落点（物理像素，主屏坐标）；被固定元素挡住时可重定位到其他落点
        let mut warp = (center_x, center_y);
        injector.warp_to(warp.0 as i16, warp.1 as i16);
        tracing::info!(
            "[scroll] region=({x},{y}) {w}x{h} center=({center_x},{center_y}) screen={}",
            injector.describe_pointer()
        );
        tracing::info!("[scroll] focus before: {}", injector.describe_focus());

        let mut a = capture.capture_area(x, y, w, h)?;
        tracing::info!(
            "[scroll] initial frame energy={:.1} size={}x{}",
            avg_adjacent_diff(&a),
            a.width,
            a.height
        );
        let frame_w = a.width;
        // 预留容量：初始一帧 + 一帧续接余量（2x），减少长滚动下 stitched 反复扩容重分配。
        let mut stitched = Vec::with_capacity(a.pixels.len().saturating_mul(2));
        stitched.extend_from_slice(&a.pixels);
        let mut stitched_h = a.height;
        // 全屏基线：空白段用它判断页面是否还在滚动（还在动 → 在滚过空白，继续；
        // 不动 → 已到底，提前停止）。正常滚动路径不读它，只在空白段与诊断时捕获。
        let mut last_full = capture.capture_primary().ok();
        let displays = capture.list_displays();
        tracing::info!(
            "[scroll] displays={:?}",
            displays
                .iter()
                .map(|d| format!("{}x{}@{:.1}", d.width, d.height, d.scale_factor))
                .collect::<Vec<_>>()
        );
        let mut streak = 0usize;
        // 低纹理段（空白/平滑图）的连续迭代计数，不并入 streak
        let mut blank_streak = 0usize;
        // 暂停状态下「内容持续静止」的连续迭代计数：指针离开注入目标后，若内容
        // 也不在动（用户没在手动滚），累计到上限就干净收尾，防止无限空转
        let mut paused_static = 0usize;
        // 暂停状态下「空白且静止」的连续帧计数：连续达到 PAUSED_BLANK_REQUIRED
        // 才判定真的到底并停止，单帧空白不累计退出
        let mut paused_blank = 0usize;
        // 自适应滚动步长：检测不到时减半，避免每轮滚动量超过视口导致无法重叠
        let mut ticks = TICKS_PER_ITER;
        // 每 tick 平均滚动像素：有内容时由 delta/ticks 校准（EMA），空白段用它把
        // 空白高度按 ticks×每tick像素补进长图（不然空白区不增高度、被跳过）。
        let mut px_per_tick = 0.0f64;
        let mut stop_reason = "max_iters";

        for (iter, _) in (0..MAX_ITERS).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                stop_reason = "canceled";
                break;
            }
            if done.load(Ordering::Relaxed) {
                stop_reason = "done";
                break;
            }
            if stitched_h > MAX_HEIGHT {
                stop_reason = "max_height";
                break;
            }

            // 用户把指针移离注入目标（如想去点进度窗的「取消」）时，暂停 warp/注入，
            // 让指针自由移动；回到目标附近自动恢复，或 cancel 结束。否则引擎每轮
            // warp 回选区中心，鼠标永远划不到角落的按钮。
            //
            // 暂停不能无限空转：指针离开后若内容也不再变化（用户没在手动滚），
            // 说明本次滚动已卡死，累计到 MAX_PAUSED_STATIC 就干净收尾返回已拼接结果。
            // 用户在暂停期间手动滚动目标窗口时，帧在变化，会持续刷新基线并继续等待。
            let dist = injector.pointer_distance_from(warp.0 as i16, warp.1 as i16);
            if dist > PAUSE_RADIUS {
                let b = capture
                    .capture_area(x, y, w, h)
                    .ok();
                let same_size = b
                    .as_ref()
                    .is_some_and(|f| f.width == a.width && f.height == a.height);
                // 捕获是否成功（在下方 `if let Some(b) = b` 部分移动 b 之前取好，日志要用）
                let capture_ok = b.is_some();
                let differ = if same_size {
                    frames_differ(&a, b.as_ref().unwrap())
                } else {
                    false
                };
                // 仅当捕获成功且尺寸一致才评估「空白」。捕获失败（b=None）或尺寸
                // 异常（b_energy 兜底 f32::MAX）都不能当作内容空白：否则一次瞬时
                // capture_area 失败就会把整次滚动截屏误判为「已到底」而终止。
                let b_energy = if same_size {
                    b.as_ref().map(avg_adjacent_diff).unwrap_or(f32::MAX)
                } else {
                    f32::MAX
                };
                if differ {
                    // 用户正在手动滚动目标窗口：跟住新基线
                    if let Some(b) = b {
                        a = b;
                    }
                }
                // 暂停中到达底部：需要连续 PAUSED_BLANK_REQUIRED 帧「空白且静止」
                // 才停止。单帧空白可能是平滑滚动中间帧 / DWM 合成间隙 / 瞬时捕获
                // 失败，立即停止会提前终止尚未到底的长图（拼接不全）。
                if !differ && b_energy < TEXTURED_ENERGY {
                    paused_blank += 1;
                    if paused_blank >= PAUSED_BLANK_REQUIRED {
                        tracing::info!(
                            "[scroll] iter={iter} blank_while_paused dist={dist:.0}px energy={b_energy:.1} capture_ok={capture_ok} same_size={same_size}"
                        );
                        stop_reason = "blank_while_paused";
                        break;
                    }
                } else {
                    paused_blank = 0;
                }
                if differ {
                    paused_static = 0;
                } else {
                    paused_static += 1;
                }
                std::thread::sleep(PAUSE_POLL);
                tracing::info!(
                    "[scroll] iter={iter} pointer_moved dist={dist:.0}px pause paused_static={paused_static} paused_blank={paused_blank}"
                );
                if paused_static >= MAX_PAUSED_STATIC {
                    stop_reason = "pointer_left_static";
                    break;
                }
                continue;
            }
            // 刚退出暂停：捕获当前帧作为新基线再开始注入。
            // 暂停期间若 a 被手动滚动刷新过，丢掉的那一段无法找回（引擎未注入滚轮），
            // 但从当前帧续跑保证后续拼接正确，不会把已滚过的内容重复拼进去。
            let resume_from_pause = paused_static > 0;
            if resume_from_pause {
                if let Ok(fresh) = capture.capture_area(x, y, w, h) {
                    if fresh.width == a.width && fresh.height == a.height {
                        a = fresh;
                        streak = 0;
                        tracing::info!("[scroll] iter={iter} resume_from_pause refresh_baseline");
                    }
                }
            }
            paused_static = 0;
            paused_blank = 0;

            // 每轮重新 warp 指针到当前落点，防止指针漂移导致滚轮事件没投递到目标窗口；
            // 同时把 X 输入焦点给目标窗口（Chromium/Electron 忽略投给未聚焦窗口的合成滚轮）
            injector.warp_to(warp.0 as i16, warp.1 as i16);
            // warp 后等 hover 稳定：立即注入滚轮可能作用在旧元素上（间歇性失效）
            std::thread::sleep(HOVER_SETTLE);
            injector.focus_under_pointer();
            injector.scroll_down(ticks);
            std::thread::sleep(SETTLE_DELAY);

            let mut b = capture.capture_area(x, y, w, h)?;
            // 底层 clamp 导致尺寸变化 → 区域失效，停止
            if b.width != a.width || b.height != a.height {
                stop_reason = "size_changed";
                break;
            }

            let mut delta = stitch::find_scroll_delta(&a, &b);
            // 内容在变动但检测失败（平滑滚动动画未结束）→ 多等一次重抓同一内容
            if delta.is_none() && frames_differ(&a, &b) {
                std::thread::sleep(EXTRA_SETTLE);
                if let Ok(b2) = capture.capture_area(x, y, w, h) {
                    if b2.width == a.width && b2.height == a.height {
                        delta = stitch::find_scroll_delta(&a, &b2);
                        b = b2;
                    }
                }
            }

            match delta {
                Some(s) if s >= MIN_SCROLL => {
                    // 拼接 b 底部新进入视口的 s 行。
                    // 空白段刚过时基线可能局部均匀，但 find_scroll_delta 能返回 Some
                    // 说明重叠带已通过 band_has_energy + 匹配行数验证（纯空白基线
                    // 会因能量门返回 None）；若因此丢弃 delta，会把真实滚动量白白丢掉。
                    let after_blank = blank_streak > 0;
                    // 校准每 tick 像素：本轮注入 ticks 格、浏览器滚动了 s 像素。
                    let per = s as f64 / ticks.max(1) as f64;
                    px_per_tick = if px_per_tick <= 0.0 {
                        per
                    } else {
                        px_per_tick * 0.8 + per * 0.2
                    };
                    let append_off = (b.height as usize - s) * frame_w as usize * 4;
                    if append_off < b.pixels.len() {
                        stitched.extend_from_slice(&b.pixels[append_off..]);
                        stitched_h += s as u32;
                        progress_h.store(stitched_h, Ordering::Relaxed);
                    }
                    a = b;
                    streak = 0;
                    blank_streak = 0;
                    ticks = TICKS_PER_ITER;
                    tracing::info!(
                        "[scroll] iter={iter} delta={s} ticks={ticks} stitched_h={stitched_h} after_blank={after_blank}"
                    );
                }
                Some(_) => {
                    // 微幅滚动：刷新基线但不拼接（太少无意义）
                    a = b;
                    streak += 1;
                    blank_streak = 0;
                    tracing::info!("[scroll] iter={iter} delta_too_small streak={streak}");
                    if streak >= MAX_STREAK {
                        stop_reason = "delta_too_small";
                        break;
                    }
                }
                None if frames_differ(&a, &b) => {
                    // 内容动了但严格检测不出：先用宽松估计尽力拼一段（**不丢内容**，
                    // 这是「滚动只拼一页」之外内容缺失的直接原因），再减半步长，让
                    // 下一轮的滚动量更小、重叠带更大、更容易被严格检测测出。
                    if let Some(s) =
                        try_append_scrolled(&a, &b, frame_w, &mut stitched, &mut stitched_h)
                    {
                        progress_h.store(stitched_h, Ordering::Relaxed);
                        if ticks > 1 {
                            ticks = ticks.div_ceil(2);
                        }
                        a = b;
                        streak = 0;
                        blank_streak = 0;
                        tracing::info!(
                            "[scroll] iter={iter} append_estimate s={s} ticks={ticks} stitched_h={stitched_h}"
                        );
                    } else {
                        a = b;
                        blank_streak = 0;
                        if ticks > 1 {
                            ticks = ticks.div_ceil(2);
                            tracing::info!("[scroll] iter={iter} undetectable, halve ticks -> {ticks}");
                        } else {
                            streak += 1;
                            tracing::info!("[scroll] iter={iter} undetectable streak={streak}");
                            if streak >= MAX_STREAK {
                                stop_reason = "undetectable";
                                break;
                            }
                        }
                    }
                }
                None => {
                    let energy = avg_adjacent_diff(&b);
                    let fd = frames_differ(&a, &b);
                    let md = max_frame_diff(&a, &b);
                    // 自相似内容（vxe/表格行）：frames_differ=false、find_scroll_delta=None，
                    // 但内容其实在滚动（maxdiff 高、unaligned 高）。若按「静止」误判，会
                    // 去 relocate/reconnect（又被指针拿走/自相似拖死）→ 自动只拼一页。
                    // 这里先试健壮的 try_append_scrolled（内部用 mean_unaligned_diff 把关
                    // 「真动了」 vs 静止/闪烁），成功即拼接续滚，避免一页。
                    if energy >= TEXTURED_ENERGY && !fd {
                        if let Some(s) =
                            try_append_scrolled(&a, &b, frame_w, &mut stitched, &mut stitched_h)
                        {
                            a = b;
                            streak = 0;
                            blank_streak = 0;
                            ticks = TICKS_PER_ITER;
                            progress_h.store(stitched_h, Ordering::Relaxed);
                            tracing::info!(
                                "[scroll] iter={iter} append_selfsim s={s} stitched_h={stitched_h}"
                            );
                            continue;
                        }
                    }
                    if energy < TEXTURED_ENERGY {
                        // 低纹理（空白/纯色/平滑图）：无法判定是否在滚动，
                        // 按还在滚继续，大步长滚过这一段；刷新基线
                        blank_streak += 1;
                        // 本轮用于滚动出 `b` 的 tick 数（空白高度估算用；稍后会被重置）
                        let used_ticks = ticks;
                        // 空白段滚动后全屏不再变化 → 页面已到底，提前停止；否则是在
                        // 滚过空白，继续大步长滚到下一个内容段。稀疏页面长空白段不再
                        // 被 MAX_BLANK_STREAK 提前误停（拼接不全）。
                        let mut scrolling_blank = false;
                        if let Ok(full) = capture.capture_primary() {
                            let stopped = last_full
                                .as_ref()
                                .is_some_and(|lf| !frames_differ(lf, &full));
                            last_full = Some(full);
                            scrolling_blank = !stopped;
                            if stopped {
                                stop_reason = "blank_page_stopped";
                                tracing::info!(
                                    "[scroll] iter={iter} blank screen_stopped -> page bottom"
                                );
                                break;
                            }
                        }
                        // 仍在滚过空白（页面没过底）：空白区没有可匹配的内容 → 检测不出
                        // 滚动了多少，只能按「本轮 ticks × 每tick像素」估算空白高度补进
                        // 长图（不然空白段被跳过、长图少一段高度）。校准过 px_per_tick
                        // 才补；高度封顶到视口一半，避免一轮追加过多。
                        if scrolling_blank && px_per_tick > 0.0 {
                            let est = (used_ticks as f64 * px_per_tick).round() as usize;
                            let est = est.min(b.height as usize / 2);
                            let append_off = (b.height as usize - est) * frame_w as usize * 4;
                            if est >= MIN_SCROLL && append_off < b.pixels.len() {
                                stitched.extend_from_slice(&b.pixels[append_off..]);
                                stitched_h += est as u32;
                                progress_h.store(stitched_h, Ordering::Relaxed);
                                tracing::info!(
                                    "[scroll] iter={iter} blank_fill est=+{est}px stitched_h={stitched_h}"
                                );
                            }
                        }
                        a = b;
                        ticks = BLANK_TICKS;
                        tracing::info!("[scroll] iter={iter} low_energy={energy:.1} blank_streak={blank_streak}");
                        if blank_streak >= MAX_BLANK_STREAK {
                            stop_reason = "blank";
                            break;
                        }
                    } else if blank_streak > 0 {
                        // 刚从空白段回到有纹理内容：基线还是空白帧，直接判静止会误停。
                        // 重置基线、恢复常规步长，继续滚（过渡宽限，不累计退出）
                        blank_streak = 0;
                        streak = 0;
                        a = b;
                        ticks = TICKS_PER_ITER;
                        tracing::info!("[scroll] iter={iter} transition_back_to_texture energy={energy:.1}");
                    } else if streak == 0 {
                        // 有纹理但静止：先尝试重定位指针（可能被吸顶/固定元素挡住滚轮）；
                        // 仍无效则重开 X 连接再试（连接退化/浏览器拒绝合成事件时恢复滚动）。
                        // 只在首次尝试，避免每次都白费几秒；都失败即按静止累计。
                        tracing::info!(
                            "[scroll] iter={iter} static streak={streak} blank={blank_streak} ticks={ticks}, pointer under: {} | focus: {}",
                            injector.describe_pointer(),
                            injector.describe_focus()
                        );
                        tracing::info!(
                            "[scroll] iter={iter} diagnostNone delta=None energy={energy:.1} frames_differ={fd} maxdiff={md} ticks={ticks}"
                        );
                        let mut revived: Option<CapturedFrame> = None;
                        if let Some(((npx, npy), frame)) =
                            try_relocate(&injector, capture, &a, region, warp)
                        {
                            warp = (npx, npy);
                            revived = Some(frame);
                            tracing::info!("[scroll] iter={iter} relocated warp -> ({npx},{npy})");
                        } else if injector.pointer_distance_from(warp.0 as i16, warp.1 as i16)
                            > PAUSE_RADIUS
                        {
                            // 指针被用户夺走（要去点「完成/取消」）：跳过 reconnect，
                            // 不再把指针强行拉回内容中心；主循环下轮检测到
                            // dist>PAUSE_RADIUS → 进入暂停，用户可点按钮。
                            tracing::info!(
                                "[scroll] iter={iter} reconnect skip (pointer away)"
                            );
                        } else if let Ok(new_inj) = new_injector() {
                            // 重开 X 连接：旧连接可能已退化，或目标窗口开始忽略旧连接的合成事件。
                            // 注入改用「慢速 + 逐批验证」：Electron/Chromium 可能丢弃快速连续注入
                            // 的滚轮，慢速事件 + 焦点稳定后更可能被接受；每批后验证内容是否移动，
                            // 动了立即复活拼接，不必等整批打完才发现失败。
                            injector = new_inj;
                            injector.warp_to(warp.0 as i16, warp.1 as i16);
                            // warp 后等 hover 稳定（同 relocate：立即注入可能作用在旧元素）
                            std::thread::sleep(HOVER_SETTLE);
                            injector.focus_under_pointer();
                            std::thread::sleep(RECONNECT_FOCUS_SETTLE);
                            let mut saw_capture = false;
                            for r in 0..RECONNECT_ROUNDS {
                                injector.scroll_down(RECONNECT_TICKS);
                                std::thread::sleep(RECONNECT_VERIFY_WAIT);
                                if let Ok(frame) = capture.capture_area(x, y, w, h) {
                                    saw_capture = true;
                                    let moved = if frame.width == a.width
                                        && frame.height == a.height
                                    {
                                        stitch::find_scroll_delta(&a, &frame).is_some()
                                            || frames_differ(&a, &frame)
                                    } else {
                                        false
                                    };
                                    tracing::info!(
                                        "[scroll] iter={iter} reconnect round={r} moved={moved}"
                                    );
                                    if moved {
                                        revived = Some(frame);
                                        tracing::info!(
                                            "[scroll] iter={iter} reconnect revived scroll"
                                        );
                                        break;
                                    }
                                }
                            }
                            if revived.is_none() {
                                tracing::info!(
                                    "[scroll] iter={iter} reconnect {}",
                                    if saw_capture {
                                        "still static"
                                    } else {
                                        "capture failed"
                                    }
                                );
                            }
                        } else {
                            tracing::info!("[scroll] iter={iter} reconnect open failed");
                        }

                        match revived {
                            Some(frame) => {
                                // relocate/reconnect 返回的帧可能仍在平滑滚动动画中
                                //（模糊）：模糊帧与基线测不出重叠 → revive 丢段；
                                // 且把它刷成基线后，后续每轮都是假小偏移
                                //（delta_too_small）→ 误停。先等动画落定，重抓
                                // 静止帧再测/拼接/刷新基线。
                                let mut b = frame;
                                std::thread::sleep(EXTRA_SETTLE);
                                if let Ok(f2) = capture.capture_area(x, y, w, h) {
                                    if f2.width == b.width && f2.height == b.height {
                                        b = f2;
                                    }
                                }
                                // 严格+估计兜底：relocate 已用大滚动把内容移过，这里把
                                // 移进来的这段拼上，避免「只拼一页」时丢掉 relocate 滚过
                                // 的那几十行。
                                if let Some(s) =
                                    try_append_scrolled(&a, &b, frame_w, &mut stitched, &mut stitched_h)
                                {
                                    progress_h.store(stitched_h, Ordering::Relaxed);
                                    tracing::info!(
                                        "[scroll] iter={iter} revived_append s={s} stitched_h={stitched_h}"
                                    );
                                }
                                a = b;
                                streak = 0;
                                blank_streak = 0;
                                // 用 relocate 已验证的步长继续（不缩到 RESUME_TICKS：4 格
                                // 对 vxe-table 等虚拟表格不响应，是「自动只拼一页」主因）。
                                // 大滚动重叠带被固定元素稀释 → 严格检测测不出 → 由
                                // estimate 兜底，不怕大步长。
                                ticks = RELOCATE_TICKS;
                                tracing::info!("[scroll] iter={iter} revived stitched_h={stitched_h}");
                            }
                            None => {
                                streak += 1;
                                // 有纹理却没滚：逐步加大注入步长再试——合成滚轮被
                                // 部分虚拟表格/页面忽略小步长，需要更大步长才触发。
                                // 若步长已到顶仍不动，说明要么真到底、要么页面确实
                                // 不响应合成滚轮（此时只能靠下面的加载窗口/停止判定）。
                                if ticks < RELOCATE_TICKS {
                                    ticks = ticks.saturating_mul(2).min(RELOCATE_TICKS);
                                }
                                tracing::info!("[scroll] iter={iter} no_delta energy={energy:.1} streak={streak} ticks={ticks}");
                                if streak >= MAX_STREAK {
                                    // 列表页 AJAX 分页加载：等一个加载窗口，期间
                                    // 内容变化（新数据渲染）则复活继续滚。
                                    if wait_for_new_content(capture, &a, x, y, w, h) {
                                        streak = 0;
                                        tracing::info!("[scroll] iter={iter} new_data_loaded -> resume");
                                        continue;
                                    }
                                    stop_reason = "no_delta";
                                    break;
                                }
                            }
                        }
                    } else {
                        streak += 1;
                        // 有纹理却没滚：加大步长继续试（同上方 revive-None 路径）。
                        // 避免 4 格对虚拟表格不响应而被误判「到底/一页」。
                        if ticks < RELOCATE_TICKS {
                            ticks = ticks.saturating_mul(2).min(RELOCATE_TICKS);
                        }
                        tracing::info!("[scroll] iter={iter} no_delta energy={energy:.1} streak={streak} ticks={ticks}");
                        if streak >= MAX_STREAK {
                            // 同上：加载窗口内出现新内容则继续，否则判定到底
                            if wait_for_new_content(capture, &a, x, y, w, h) {
                                streak = 0;
                                tracing::info!("[scroll] iter={iter} new_data_loaded -> resume");
                                continue;
                            }
                            stop_reason = "no_delta";
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("[scroll] stop_reason={stop_reason} stitched_h={stitched_h}");

        // 取消 → 不生成到剪贴板（None）；「完成」/正常到底 → Some(拼接结果)。
        if stop_reason == "canceled" {
            return Ok(None);
        }
        Ok(Some(CapturedFrame {
            width: frame_w,
            height: stitched_h,
            pixels: stitched,
        }))
    })();

    progress.hide();
    result
}

/// 运行手动滚动截屏并返回拼接好的长图（调用方负责写剪贴板）。
///
/// 与自动模式的区别：不注入滚轮、不移动指针，由用户自己在目标窗口滚动。
/// 应用只负责轮询抓帧、重叠检测拼接。用户滚完点进度窗的「完成」结束。
/// 取消时返回 None（不生成到剪贴板）。
pub fn run_manual_scroll_capture(
    region: &Bounds,
    screen_bounds: &Bounds,
    capture: &dyn ScreenCapture,
    progress: &dyn ScrollProgress,
) -> AppResult<Option<CapturedFrame>> {
    let (x, y, w, h) = (
        region.origin.x as i32,
        region.origin.y as i32,
        region.size.x.max(1.0) as u32,
        region.size.y.max(1.0) as u32,
    );
    if w < 8 || h < 8 {
        return Err(AppError::Window("选区太小，无法手动滚动截屏".into()));
    }

    // 等遮罩窗口销毁完成，桌面恢复原样
    std::thread::sleep(STARTUP_DELAY);

    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let progress_h = Arc::new(AtomicU32::new(h));
    // 内容是否在动（相邻帧不同）：静止时才让进度窗显示「完成」
    let moving = Arc::new(AtomicBool::new(false));
    // 最近一帧底部是否含内容 + 确认态标志（见 trait 注释）
    let bottom_has_content = Arc::new(AtomicBool::new(false));
    let confirming = Arc::new(AtomicBool::new(false));
    progress.show_manual(
        region,
        screen_bounds,
        cancel.clone(),
        done.clone(),
        progress_h.clone(),
        moving.clone(),
        bottom_has_content.clone(),
        confirming.clone(),
    );

    // 内部闭包包住主循环：任何 `?` 提前退出，外层都统一 hide 进度窗。
    let result = (|| -> AppResult<Option<CapturedFrame>> {
        // 首帧等画面稳定后再取：遮罩关闭 / 进度窗出现的过渡期，画面轻微变化会被
        // find_scroll_delta 因「空白自相似」误判成最大滚动量（如 300）→ 重复拼接。
        // 连续两帧几乎相同（max_frame_diff ≤ 阈值）才算稳定；超时则用最近一帧兜底。
        let mut a = capture.capture_area(x, y, w, h)?;
        for _ in 0..STARTUP_STABLE_ATTEMPTS {
            std::thread::sleep(STARTUP_STABLE_POLL);
            let Ok(f) = capture.capture_area(x, y, w, h) else {
                break;
            };
            if f.width != a.width || f.height != a.height {
                a = f;
                break;
            }
            if max_frame_diff(&a, &f) <= STARTUP_STABLE_DIFF {
                a = f;
                break; // 屏幕已静止
            }
            a = f;
        }
        let frame_w = a.width;
        // 预留容量：初始一帧 + 一帧续接余量（2x），减少长滚动下 stitched 反复扩容重分配。
        let mut stitched = Vec::with_capacity(a.pixels.len().saturating_mul(2));
        stitched.extend_from_slice(&a.pixels);
        let mut stitched_h = a.height;
        // anchor：最近一次成功拼接（或刷新）的基线，直接接管首帧所有权；
        // prev：上一帧（做帧间运动检测），首帧之前不存在，用 Option 延迟持有，
        // 省去对首帧的一次整帧 clone（原来 anchor/prev 各持一份）。
        // 性能：anchor/prev 用 Arc<CapturedFrame> 共享同一帧数据，滚动拼接时
        // `anchor = b.clone(); prev = Some(b)` 这类**每帧 5MB 深拷贝**改为
        // Arc 指针 +1（O(1)）。所有调用点仍是 &CapturedFrame（deref 自动解引用）。
        let mut anchor: Arc<CapturedFrame> = Arc::new(a);
        let mut prev: Option<Arc<CapturedFrame>> = None;
        let mut moving_frames = 0usize;
        let mut stop_reason = "max_iters";
        // 内容是否在动的**去抖门控**：`moving` 只驱动「完成」按钮的灰/亮。vxe-table
        // 的光标闪烁/加载动画让 frames_differ 在相邻帧上高频翻转，若逐帧更新 moving，
        // 按钮会灰-亮高频闪烁。这里要求连续 MOVING_DEBOUNCE 帧同向才更新 moving。
        let mut moving_latched = false;
        let mut moving_same = 0u32;

        for iter in 0..MAX_MANUAL_ITERS {
            if cancel.load(Ordering::Relaxed) {
                stop_reason = "canceled";
                break;
            }
            if done.load(Ordering::Relaxed) {
                stop_reason = "done";
                break;
            }
            if stitched_h > MAX_HEIGHT {
                stop_reason = "max_height";
                break;
            }

            std::thread::sleep(MANUAL_POLL);
            let b = capture.capture_area(x, y, w, h)?;
            if b.width != anchor.width || b.height != anchor.height {
                stop_reason = "size_changed";
                break;
            }
            // 告知进度窗内容是否在动（相邻帧不同）：静止时才显示「完成」按钮。
            // 首帧前 prev=None → false，初始单页即可点「完成」。
            let is_moving = prev.as_ref().is_some_and(|p| frames_differ(p, &b));
            // 去抖：连续 MOVING_DEBOUNCE 帧同向才更新 moving，避免光标闪烁令
            // 「完成」按钮灰/亮高频闪（抖动）。
            if is_moving == moving_latched {
                moving_same += 1;
            } else {
                moving_same = 0;
            }
            if moving_same >= MOVING_DEBOUNCE {
                moving_latched = is_moving;
                moving.store(is_moving, Ordering::Relaxed);
            }
            // 用户重新滚动 → 撤掉「可能没滚到底」的确认态，回到正常进度窗
            if is_moving {
                confirming.store(false, Ordering::Relaxed);
            }
            // 最近一帧底部是否还有内容：点「完成」时据此决定是否先弹确认
            bottom_has_content.store(frame_bottom_has_content(&b), Ordering::Relaxed);

            // 优先尝试重叠检测：只要帧的重叠**可靠**（find_scroll_delta 返回可信 s），
            // 就立即拼接——**含动画中间帧**。虚拟表格（vxe-table）平滑滚动是连续动画，
            // 若只等「静止帧」再拼，连续滚动会让引擎攒一个大跳跃（如 iter21 s=187）才
            // 抓一帧，中间行已被虚拟化滚过、没渲染 → 直接拼接就缺内容（长图跳号）。
            //
            // 这里改成**逐帧连续拼接**：滚动过程中每个可靠中间增量帧都拼一小段，更新
            // anchor 后继续，把整段动画逐步衔接，不留缺口。**安全网**：模糊/不可靠的
            // 帧（find_scroll_delta 返回 None）仍落下方「等静止重抓」分支精确处理；且
            // try_append_scrolled 内部有 frames_differ / best_pixel_offset 的相对判据，
            // 不会把静止帧或模糊帧误拼。is_moving（相邻帧在变）仍驱动「完成」按钮，
            // 滚动停止后用户才能点完成。
            if let Some(s) = stitch::find_scroll_delta(&anchor, &b) {
                if s >= MIN_SCROLL {
                    if let Some(s) =
                        try_append_scrolled(&anchor, &b, frame_w, &mut stitched, &mut stitched_h)
                    {
                        progress_h.store(stitched_h, Ordering::Relaxed);
                        tracing::info!(
                            "[scroll-manual] iter={iter} append s={s} stitched_h={stitched_h} maxdiff={}",
                            max_frame_diff(&anchor, &b)
                        );
                        anchor = Arc::new(b);
                        prev = Some(anchor.clone());
                        moving_frames = 0;
                        continue;
                    }
                }
                // delta 可信但重叠不可靠（模糊中间帧）或小滚动（< MIN_SCROLL）：
                // 不拼，落检测失败分支（动画帧 → 等静止重抓；小滚动无害丢弃）。
                tracing::info!(
                    "[scroll-manual] iter={iter} delta={s} skip_anim_or_small maxdiff={} diff_prev={}",
                    max_frame_diff(&anchor, &b),
                    prev.as_ref().is_some_and(|p| frames_differ(p, &b)),
                );
            }

            // 检测失败：区分「向上滚」「还在滚动动画中」「静止在新位置」
            if stitch::find_scroll_delta(&b, &anchor).is_some() {
                // 反向检测命中 → 用户向上滚了：保持 anchor 在最深基线不动，
                // 之后滚回原位/继续向下时只追加超出当前拼接底部的真正新内容，
                // 避免把已拼接的行重复拼进去
                moving_frames = 0;
                tracing::info!("[scroll-manual] iter={iter} scrolled_up keep_baseline");
            } else if prev.as_ref().is_some_and(|p| frames_differ(p, &b)) {
                // 相邻帧仍在变化 → 平滑滚动动画进行中。12ms 轮询几乎总在动画中，
                // 模糊帧测不出重叠 → 若直接丢段，长图中间缺内容。等动画落定后
                // 重抓一帧再测：静止帧与旧 anchor 的重叠带逐像素一致 → 精确拼接。
                // 注意等待要 > Chrome 平滑滚动时长（~300ms）：EXTRA_SETTLE(250ms)
                // 单独不够，大滚动动画更长，重抓仍抓到模糊帧 → 丢段。补 SETTLE_DELAY。
                moving_frames += 1;
                std::thread::sleep(EXTRA_SETTLE);
                std::thread::sleep(SETTLE_DELAY);
                let retried = capture.capture_area(x, y, w, h).ok().filter(|f| {
                    f.width == anchor.width && f.height == anchor.height
                });
                if let Some(b2) = retried {
                    if let Some(s) =
                        try_append_scrolled(&anchor, &b2, frame_w, &mut stitched, &mut stitched_h)
                    {
                        progress_h.store(stitched_h, Ordering::Relaxed);
                        tracing::info!(
                            "[scroll-manual] iter={iter} append_after_settle s={s} stitched_h={stitched_h}"
                        );
                        anchor = Arc::new(b2);
                        prev = Some(anchor.clone());
                        moving_frames = 0;
                        continue;
                    }
                    // 重抓仍测不出（严格+估计都失败，真无重叠）：用重抓帧继续走
                    // 「静止在新位置/到底」分支
                    tracing::info!(
                        "[scroll-manual] iter={iter} retry_still_undetectable maxdiff={}",
                        max_frame_diff(&anchor, &b2)
                    );
                }
                tracing::info!("[scroll-manual] iter={iter} moving moving_frames={moving_frames}");
            } else if frames_differ(&anchor, &b)
                || max_frame_diff(&anchor, &b) > CREDIBLE_DIFF
            {
                // 已静止但严格 delta 测不出（均匀内容 / 滚动过快无重叠 / vxe-table
                // 重建行）：**不要直接丢段**。先尝试宽松估计拼一段（宁可有缝不缺内容，
                // 这是手动滚动「26-32 行缺失」类 bug 的根因）；只有估计也判「无重叠」
                // （滚动超过一屏）才真正放弃这一段并基线跟到新位置。
                let md = max_frame_diff(&anchor, &b);
                let energy = avg_adjacent_diff(&b);
                // vxe-table/虚拟滚动表格：滚动时行是 JS 重建的，中间会渲染「低能量空白
                // 帧」（行还没填充）。此时 energy 很低（<12）但 maxdiff 大（内容已换），
                // 若直接丢段，用户滚过的行会缺失。先延长等待让虚拟行渲染完成，重抓
                // 稳定帧再测；仍测不出才交给下文的宽松估计兜底。
                if energy < TEXTURED_ENERGY {
                    let mut tried = 0;
                    let mut recovered = false;
                    while tried < 3 {
                        std::thread::sleep(EXTRA_SETTLE);
                        if let Ok(f2) = capture.capture_area(x, y, w, h) {
                            if f2.width == anchor.width && f2.height == anchor.height {
                                if let Some(s) = try_append_scrolled(
                                    &anchor,
                                    &f2,
                                    frame_w,
                                    &mut stitched,
                                    &mut stitched_h,
                                ) {
                                    progress_h.store(stitched_h, Ordering::Relaxed);
                                    tracing::info!(
                                        "[scroll-manual] iter={iter} low_energy_recovered s={s} stitched_h={stitched_h}"
                                    );
                                    anchor = Arc::new(f2);
                                    prev = Some(anchor.clone());
                                    moving_frames = 0;
                                    // 关键：从 `while` 里 break 出来 + 置 recovered，
                                    // 让下方跳过 append_estimate。否则 continue 只继续内层
                                    // while，落空后仍会走 append_estimate，用**已前移**的
                                    // anchor 对比**旧的**低能耗帧 b → 误取一个 s → 拼接重叠
                                    // 区 → 序号重复（如 iter32 low_energy_recovered s=178 后
                                    // 又 append_estimate s=104，重复 23/24/25）。
                                    recovered = true;
                                    break;
                                }
                                if avg_adjacent_diff(&f2) >= TEXTURED_ENERGY {
                                    break; // 渲染完成且内容已稳定
                                }
                            }
                        }
                        tried += 1;
                    }
                    if recovered {
                        continue; // 已成功拼接，跳到下一次外循环，避免重复拼接同一段
                    }
                    tracing::info!(
                        "[scroll-manual] iter={iter} low_energy_still_blank energy={energy:.1} maxdiff={md}"
                    );
                }
                // 低能量兜底 + 正常纹理路径都到这里：优先宽松估计，避免丢段
                if let Some(s) =
                    try_append_scrolled(&anchor, &b, frame_w, &mut stitched, &mut stitched_h)
                {
                    progress_h.store(stitched_h, Ordering::Relaxed);
                    tracing::info!(
                        "[scroll-manual] iter={iter} append_estimate s={s} stitched_h={stitched_h} energy={energy:.1} maxdiff={md}"
                    );
                    anchor = Arc::new(b);
                    prev = Some(anchor.clone());
                    moving_frames = 0;
                    continue;
                }
                anchor = Arc::new(b);
                prev = Some(anchor.clone());
                moving_frames = 0;
                tracing::info!(
                    "[scroll-manual] iter={iter} settled_at_new_position gap_undetectable energy={energy:.1} maxdiff={md}",
                );
                continue;
            } else {
                // 与 anchor 基本一致（没滚 / 滚回原位）→ 无事发生。
                // maxdiff>0 说明有轻微变化但低于阈值：自相似内容滚动时可能落在这里，
                // 记录供诊断（纯静止 maxdiff=0 不刷屏）。
                moving_frames = 0;
                let md = max_frame_diff(&anchor, &b);
                if md > 0 {
                    tracing::info!(
                        "[scroll-manual] iter={iter} idle maxdiff={md} diff_prev={}",
                        prev.as_ref().is_some_and(|p| frames_differ(p, &b)),
                    );
                }
            }
            // 该路径下 b 未被接管：同样入 Arc 共享（避免整帧深拷贝给 prev）。
            prev = Some(Arc::new(b));
        }

        tracing::info!("[scroll-manual] stop_reason={stop_reason} stitched_h={stitched_h}");

        // 取消 → 不生成到剪贴板（None）；「完成」/正常结束 → Some(拼接结果)。
        if stop_reason == "canceled" {
            return Ok(None);
        }
        Ok(Some(CapturedFrame {
            width: frame_w,
            height: stitched_h,
            pixels: stitched,
        }))
    })();

    progress.hide();
    result
}

/// 粗判两帧内容是否显著不同（区分「动画还在进行/滚动了」与「内容静止」）。
/// 均匀采样若干行、每行取 3 列，超过一半采样行有像素差异即认为不同。
/// 帧底部 ~8 行是否含内容（非近白像素）。用于「完成」确认：
/// 底部还有内容说明视口底可能不是页面底，提示用户可能还没滚到底。
///
/// 阈值取 200（不是 235）：页面 footer 常是浅灰底/小号浅色版权字（#D0D0D0 附近），
/// 235 会把它们误判为内容 → 滚到底后点「完成」仍弹「底部可能还有内容？」确认，
/// 用户明明到底了还被追问。只有足够暗/密的像素才算「还有正文」。
fn frame_bottom_has_content(f: &CapturedFrame) -> bool {
    let w = f.width as usize;
    let h = f.height as usize;
    if w == 0 || h == 0 {
        return false;
    }
    let rows = 4usize.min(h);
    let mut hits = 0u32;
    for y in (h - rows)..h {
        for x in (0..w).step_by(8) {
            let p = (y * w + x) * 4;
            let (r, g, b) = (f.pixels[p], f.pixels[p + 1], f.pixels[p + 2]);
            if r < 200 || g < 200 || b < 200 {
                hits += 1;
                if hits > 20 {
                    return true;
                }
            }
        }
    }
    false
}

/// 等待加载窗口：轮询区域内是否出现新内容（列表页 AJAX 分页渲染）。
/// 出现（帧间差异显著）→ true，调用方应复活继续滚动；窗口结束仍无变化
/// → false（真到底）。轮询间隔 400ms，总时长 ≈ LOADING_WAIT。
fn wait_for_new_content(
    capture: &dyn ScreenCapture,
    baseline: &CapturedFrame,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> bool {
    let deadline = std::time::Instant::now() + LOADING_WAIT;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        if let Ok(f) = capture.capture_area(x, y, w, h) {
            if f.width != baseline.width || f.height != baseline.height {
                continue;
            }
            if frames_differ(baseline, &f) {
                return true;
            }
        }
    }
    false
}

/// 尝试把 `frame`（滚动后抓到的帧）相对 `anchor` 向下滚动后新进入视口的行拼到 `stitched`。
///
/// 先走严格检测 [`stitch::find_scroll_delta`]（唯一性 + 匹配率验证，最可靠）；严格
/// 检测失败时再走宽松估计 [`stitch::estimate_scroll_delta`]（取匹配行数最多的偏移）。
/// 宽松估计可能有一两行缝，但**不会像严格检测那样整段丢内容**——手动滚动的快速
/// 滑动 / vxe-table 虚拟表格重建行时，严格检测常返回 None，若丢弃则长图中间缺内容。
///
/// 返回实际使用的滚动量 s；若严格+估计都测不出（内容真的无重叠，如滚动超过一屏）
/// → `None`，调用方决定是否丢段。
fn try_append_scrolled(
    anchor: &CapturedFrame,
    frame: &CapturedFrame,
    frame_w: u32,
    stitched: &mut Vec<u8>,
    stitched_h: &mut u32,
) -> Option<usize> {
    // 真实变化判据：整帧**未对齐**平均差（同一坐标 a vs b）。静止/闪烁帧（b≈a，
    // 仅光标闪动）该值接近 0 → 是「被误取周期大偏移重复拼接」（iter14-37 s=630×N，
    // iter82-84 s=585×3）的元凶，拒绝；真实滚动（内容整体移动、行数据不同）该值
    // 偏高 → 放行。用 mean_unaligned_diff 而非 frames_differ：frames_differ（行匹配
    // 比例）对 vxe-table 自相似行失效（真实小滚动也判 false → 丢段），这个均值差对
    // 自相似内容依然能区分「真动了」与「几乎没变」。
    let unaligned = stitch::mean_unaligned_diff(anchor, frame).unwrap_or(u64::MAX);
    // 求 s：find_scroll_delta（行签名，可信）优先，否则 estimate/force 兜底。
    let s = match stitch::find_scroll_delta(anchor, frame) {
        Some(s) if s >= MIN_SCROLL => s,
        _ => stitch::estimate_scroll_delta(anchor, frame)
            .or_else(|| stitch::force_estimate_scroll_delta(anchor, frame))?,
    };
    // 非静止门槛：只要 unaligned ≥ TRULY_STATIC_MIN（内容至少动了那么一点点），就认为
    // 是真实滚动并放行。不按「大 s + 低 unaligned」拒——那是 vxe 周期假偏移，但那些帧的
    // unaligned 是 0~1（几乎没动），已被下方的 TRULY_STATIC_MIN 拒掉；而**稀疏/低纹理**内容
    // 真实滚动时 unaligned 也可能只有 3~8（像素变化少），不能因 unaligned 低就拒，否则
    // 浅色文字页面「自动只拼一页」。只有 unaligned < TRULY_STATIC_MIN 才拒（真静止/纯闪烁）。
    if unaligned < TRULY_STATIC_MIN {
        tracing::info!(
            "[scroll-manual] try_append reject_static s={s} unaligned={unaligned} maxdiff={}",
            max_frame_diff(anchor, frame)
        );
        return None;
    }
    // 用「全范围像素差最小」的精确偏移回退粗估：粗估按「匹配行数最多」选，会被
    // vxe-table 的行周期重复骗到整数倍偏移（如 120=4×30），一下跳/重叠多行（跳号）。
    // 「像素差最小」瓦解周期假峰，把 s 钉回真实平移（真实 s 重叠带像素差最低）。
    // 但固定表头/分页栏对**所有**候选加**常量**像素差惩罚，绝对阈值不可靠 → 用
    // **相对**判据：仅当 wide 扫描的最佳 s 像素差**明显低于**粗估 s（回退/纠偏有效），
    // 且不高于垃圾上限（内容确实无清晰平移）时才采纳；否则退回粗估 s，不丢段。
    let coarse_diff = stitch::pixel_diff_at(anchor, frame, s).unwrap_or(u64::MAX);
    let refined = stitch::best_pixel_offset(anchor, frame);
    let s = match refined {
        Some((exact, best_diff)) if best_diff < coarse_diff && best_diff <= 150 => exact,
        _ => s,
    };
    // 大偏移 + 内容几乎没变 = 周期性假偏移（vxe-table 行整倍数 s≈500）→ 拒绝，
    // 避免把重叠带重复拼入（小块周期性重复的根因）。真实大滚动内容变化大，
    // unaligned 高，不触此关。
    let half = (frame.height as u64 / LARGE_S_FRACTION).max(1);
    if s as u64 > half && unaligned < LARGE_S_MAX_STATIC_UNALIGNED {
        tracing::info!(
            "[scroll-manual] try_append reject_periodic_overshoot s={s} unaligned={unaligned} h={} maxdiff={}",
            frame.height,
            max_frame_diff(anchor, frame)
        );
        return None;
    }
    let append_off = (frame.height as usize - s) * frame_w as usize * 4;
    if s < MIN_SCROLL || append_off >= frame.pixels.len() {
        return None;
    }
    tracing::info!(
        "[scroll-manual] try_append s={s} unaligned={unaligned} coarse_diff={coarse_diff} best={:?} maxdiff={}",
        refined,
        max_frame_diff(anchor, frame)
    );
    stitched.extend_from_slice(&frame.pixels[append_off..]);
    *stitched_h += s as u32;
    Some(s)
}

fn frames_differ(a: &CapturedFrame, b: &CapturedFrame) -> bool {    if a.width != b.width || a.height != b.height {
        return false;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return false;
    }
    let cols = [w / 4, w / 2, w * 3 / 4];
    // 采样行更密（原 h/16）：合成滚动的平滑滚动量常只有几十到几百行，
    // 采样过疏会漏掉——10:39 日志中滚动 300 行仍 differ=false 即因采样过疏。
    // 阈值从「过半」降到「1/4」：合成滚轮（尤其表格容器）滚动量小于半屏时仍
    // 能判定「内容变了」，否则 relocate/reconnect 误判「滚不动」（自动模式失败主因）。
    let stride = (h / 32).max(1);
    let mut changed = 0u32;
    let mut total = 0u32;
    for r in (0..h).step_by(stride) {
        let base = r * w * 4;
        let mut row_changed = false;
        for &c in &cols {
            let p = base + c * 4;
            for ch in 0..3 {
                if a.pixels[p + ch].abs_diff(b.pixels[p + ch]) > 24 {
                    row_changed = true;
                }
            }
        }
        if row_changed {
            changed += 1;
        }
        total += 1;
    }
    // 1/4 阈值：局部动态元素（光标闪烁等）变化行数少，不会误判；真实滚动/加载
    // 变化行足够，能把「内容变了」判出来
    changed * 4 > total
}

/// 两帧在采样网格上的最大单通道像素差（255 表示尺寸不同）。
fn max_frame_diff(a: &CapturedFrame, b: &CapturedFrame) -> u8 {
    if a.width != b.width || a.height != b.height {
        return 255;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return 0;
    }
    let mut m = 0u8;
    for y in (0..h).step_by(8) {
        for x in (0..w).step_by(8) {
            let p = (y * w + x) * 4;
            for ch in 0..3 {
                let d = a.pixels[p + ch].abs_diff(b.pixels[p + ch]);
                if d > m {
                    m = d;
                }
            }
        }
    }
    m
}

/// 相邻采样行 RGB 平均差：越小越均匀。空白/纯色/平滑大图偏低，
/// 文本/强对比内容偏高。用于区分「有纹理静止」和「低纹理无法判定」。
fn avg_adjacent_diff(f: &CapturedFrame) -> f32 {
    let w = f.width as usize;
    let h = f.height as usize;
    if w == 0 || h == 0 {
        return 0.0;
    }
    let cols = [w / 4, w / 2, w * 3 / 4];
    let stride = (h / 16).max(1);
    let mut total: u64 = 0;
    let mut n: u64 = 0;
    let mut prev: [u64; 3] = [0; 3];
    let mut have_prev = false;
    for r in (0..h).step_by(stride) {
        let base = r * w * 4;
        let mut sig = [0u64; 3];
        for (i, &c) in cols.iter().enumerate() {
            let p = base + c * 4;
            sig[i] = f.pixels[p] as u64 + f.pixels[p + 1] as u64 + f.pixels[p + 2] as u64;
        }
        if have_prev {
            for i in 0..3 {
                total += sig[i].abs_diff(prev[i]);
            }
            n += 1;
        }
        prev = sig;
        have_prev = true;
    }
    if n == 0 {
        0.0
    } else {
        total as f32 / n as f32
    }
}

/// 当前指针落点滚动无效（被吸顶/固定元素挡住）时，尝试区域内其他落点，
/// 找到能继续滚动的那一个。返回 (新落点, 捕获帧)；全部无效则 None。
///
/// 网格扫描：宽向 5 列 × 高向 3 行覆盖整个区域，避开最右进度窗污染带。
/// 编辑器文本通常在左侧，因此按「左中 → 中上 → 右列」的优先级排序，让最可能
/// 命中的落点先测。判定「有效」：在该落点注入滚轮后，帧内容发生明显变化
/// （检测到 delta 或与原基线显著不同），说明事件被投递到了真正在滚动的元素上。
fn try_relocate(
    injector: &xtest::XtestInjector,
    capture: &dyn ScreenCapture,
    baseline: &CapturedFrame,
    region: &Bounds,
    current: (i32, i32),
) -> Option<((i32, i32), CapturedFrame)> {
    let (x, y, w, h) = (
        region.origin.x as i32,
        region.origin.y as i32,
        region.size.x.max(1.0) as u32,
        region.size.y.max(1.0) as u32,
    );
    let wi = w as i32;
    let hi = h as i32;
    // 列：避开左右固定列（vxe-table 序号列在最左约 4%、操作列在最右约 8%），
    // 只落中间可滚动内容区；行避开顶部表头，落在表体数据行。
    let cols = [30, 50, 70];
    let rows = [50, 70];
    // 优先级：6 个位置。**行 50%~70%**——vxe-table 顶部表头/搜索栏固定，可滚动数据区
    // 在视口中下部；列 30%~70% 都是内容列（避开左右固定列）。候选少 + 每个候选都可被
    // 用户夺回指针后**立即中止**（见下方 interruptible 检查），「到底后确认无法再滚」的
    // 抢指针时间最短。
    let order = [
        (0, 0), (1, 0), (2, 0),
        (0, 1), (1, 1), (2, 1),
    ];
    for (ci, ri) in order {
        let px = x + wi * cols[ci] / 100;
        let py = y + hi * rows[ri] / 100;
        if (px, py) == current {
            continue;
        }
        injector.warp_to(px as i16, py as i16);
        // X11 warp 后 Chrome 的 hover 元素更新有延迟：立即注入滚轮可能仍作用在
        // 旧元素上（表现为合成滚轮「间歇性失效」——同一位置有时能滚有时不能）。
        // 等 hover 稳定后再滚动，提高 relocate 命中率。
        std::thread::sleep(HOVER_SETTLE);
        // 大滚动探针：6 格滚动量（约 300px/约 6 行，占视口 22 行 >25%）足以让
        // frames_differ（1/4 采样行变化）触发，确认「这个位置能滚」。
        injector.scroll_down(RELOCATE_TICKS);
        std::thread::sleep(RELOCATE_SETTLE);
        // 用户移离候选位（想夺回指针去点「完成/取消」）：**立即停止 relocate**，不再
        // 抢指针。指针留在用户当前位置，主循环下轮检测到 dist>PAUSE_RADIUS → 进入暂停，
        // 用户就能控制指针点按钮。relocate 因此可被打断，不再把指针锁住 ~9 秒。
        if injector.pointer_distance_from(px as i16, py as i16) > PAUSE_RADIUS {
            tracing::info!(
                "[scroll] relocate abort ({px},{py}) pointer took by user"
            );
            return None;
        }
        let Ok(frame) = capture.capture_area(x, y, w, h) else {
            continue;
        };
        if frame.width != baseline.width || frame.height != baseline.height {
            continue;
        }
        let delta = stitch::find_scroll_delta(baseline, &frame);
        let differ = frames_differ(baseline, &frame);
        let md = max_frame_diff(baseline, &frame);
        // 候选可信判定（**只认 differ=true**）：6 格滚动后内容真的有明显变化，
        // 说明滚轮事件投递到了真正在滚动的元素上。`delta`/`maxdiff` 不可靠：
        // 静态帧上 find_scroll_delta 会因行签名巧合（表格行结构/空白段）报假偏移；
        // 动态元素（光标闪烁/时间）会拉高 maxdiff。differ 是唯一可信标准。
        let credible = differ;
        tracing::info!(
            "[scroll] relocate candidate ({px},{py}) at col{}% row{}% delta={delta:?} differ={differ} maxdiff={md} credible={credible}",
            cols[ci], rows[ri],
        );
        if credible {
            // 确认「能滚」：**不再滚回原位**（滚回会让页面真实地来回跳，用户看见
            // 「无限来回滚动」而失去耐心移开鼠标，引擎随之误暂停——自动只有一页的
            // 直接原因）。直接返回**当前滚动后的帧**作为新基线，主循环从该位置继续
            // 向下滚动拼接（页面只向下，不来回）。基线差异（本轮已滚的 10 格内容）
            // 由主循环下一次 find_scroll_delta 量出并拼入（重叠带需覆盖这 10 格）。
            tracing::info!(
                "[scroll] relocate confirmed ({px},{py}) scroll_used"
            );
            return Some(((px, py), frame));
        }
    }
    // 全部候选无效：把指针恢复到调用前的落点。否则指针停在最后一个候选点，
    // 下轮 dist 检查会把引擎自己的移动误判为「用户移开鼠标」而错误暂停。
    injector.warp_to(current.0 as i16, current.1 as i16);
    None
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn new_injector() -> AppResult<xtest::XtestInjector> {
    xtest::XtestInjector::open()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn new_injector() -> AppResult<xtest::XtestInjector> {
    Err(AppError::Window(
        "滚动截屏仅支持 Linux/X11、Windows 或 macOS 会话".into(),
    ))
}

#[cfg(test)]
mod manual_diag_tests {
    use super::*;

    fn mk_frame(w: usize, h: usize, px_val: impl Fn(usize, usize) -> u8) -> CapturedFrame {
        let mut px = vec![0u8; w * h * 4];
        for row in 0..h {
            for x in 0..w {
                let v = px_val(row, x);
                let p = (row * w + x) * 4;
                px[p] = v;
                px[p + 1] = v;
                px[p + 2] = v;
                px[p + 3] = 255;
            }
        }
        CapturedFrame { width: w as u32, height: h as u32, pixels: px }
    }

    /// 行值 + 列值的散列：行签名唯一性强（单色行 + 256 值域会让多个偏移
    /// 假匹配 → find_scroll_delta 歧义拒绝，测不出真实滚动量）。
    fn row_col_val(row: usize, col: usize) -> u8 {
        // 乘常数散列：无小周期（(row*97)%250 在帧高>250 时内容每 250 行重复，
        // 会让真实滚动量与「周期对齐」的假偏移匹配数接近 → 歧义拒绝）。
        let x = row
            .wrapping_mul(2654435761)
            .wrapping_add(col.wrapping_mul(97));
        ((x >> 16) ^ (x >> 8) ^ x) as u8
    }

    /// 真实滚动帧：b = a 向下滚 200 行。frames_differ 必须为 true，
    /// find_scroll_delta 必须能测出 200。
    #[test]
    fn detection_on_scrolled_frame() {
        let w = 1083usize;
        let h = 326usize;
        let a = mk_frame(w, h, row_col_val);
        let b = mk_frame(w, h, |r, c| {
            let src = if r + 200 < h { r + 200 } else { h + r };
            row_col_val(src, c)
        });
        assert!(frames_differ(&a, &b), "纯滚动帧 frames_differ 必须为 true");
        assert!(max_frame_diff(&a, &b) > 24, "maxdiff 应显著");
        assert_eq!(stitch::find_scroll_delta(&a, &b), Some(200));
    }

    /// 只改右 1/4 区域（模拟「变化发生在采样列之外」）。
    #[test]
    fn detection_when_change_localized() {
        let w = 1083usize;
        let h = 326usize;
        let a = mk_frame(w, h, row_col_val);
        // b 只在右 1/4 列带(813..1083)变化，其余与 a 相同
        let mut b = a.clone();
        for row in 0..h {
            for x in (w * 3 / 4)..w {
                let p = (row * w + x) * 4;
                let v = row_col_val(row + 200, x);
                b.pixels[p] = v;
                b.pixels[p + 1] = v;
                b.pixels[p + 2] = v;
            }
        }
        // 变化在右 1/4（含采样列 3w/4=812）→ frames_differ 必须为 true
        assert!(frames_differ(&a, &b), "右 1/4 变化应被 frames_differ 捕获");
        assert!(max_frame_diff(&a, &b) > 24);
    }

    /// 变化只在最左侧窄带（0..w/4-2，采样列 w/4=270 之外）。
    /// 记录行为：3 列采样会漏掉这种窄带变化（frames_differ=false），
    /// 且这种「局部 patch」不是干净滚动，find_scroll_delta 也返回 None。
    #[test]
    fn frames_differ_misses_narrow_left_change() {
        let w = 1083usize;
        let h = 326usize;
        let a = mk_frame(w, h, row_col_val);
        let mut b = a.clone();
        for row in 0..h {
            for x in 0..(w / 4 - 2) {
                let p = (row * w + x) * 4;
                let v = row_col_val(row + 200, x);
                b.pixels[p] = v;
                b.pixels[p + 1] = v;
                b.pixels[p + 2] = v;
            }
        }
        // 变化在最左窄带：3 列采样(270/541/812)抓不到 → frames_differ=false
        assert!(!frames_differ(&a, &b));
        assert!(max_frame_diff(&a, &b) > 24);
        // 局部 patch 非干净滚动 → find_scroll_delta 正确返回 None
        assert_eq!(stitch::find_scroll_delta(&a, &b), None);
    }

    /// 底部含内容 → true（视口底可能是页面中部，需弹「可能没滚到底」确认）。
    #[test]
    fn bottom_content_detected() {
        let w = 200usize;
        let h = 100usize;
        // 全白底 → 无内容
        let blank = mk_frame(w, h, |_, _| 255);
        assert!(!frame_bottom_has_content(&blank));
        // 底部 8 行画一行深色文字（其他行白）→ 有内容
        let mut f = mk_frame(w, h, |_, _| 255);
        for row in (h - 8)..h {
            for x in 0..w {
                let p = (row * w + x) * 4;
                f.pixels[p] = 30;
                f.pixels[p + 1] = 30;
                f.pixels[p + 2] = 30;
            }
        }
        assert!(frame_bottom_has_content(&f));
        // 底部只有极少量杂色像素（< 阈值）→ 不算内容
        let mut f2 = mk_frame(w, h, |_, _| 255);
        for k in 0..5 {
            let p = ((h - 1) * w + k * 30) * 4;
            f2.pixels[p] = 100;
            f2.pixels[p + 1] = 100;
            f2.pixels[p + 2] = 100;
        }
        assert!(!frame_bottom_has_content(&f2));
    }

    /// `try_append_scrolled`：正常滚动（严格检测命中）应把新进入视口的行拼上，
    /// 并返回正确的滚动量。这验证「不丢段」的核心管道。
    #[test]
    fn try_append_strict_path() {
        // 与 detection_on_scrolled_frame 相同的宽高/内容参数，确保严格检测能测出真实 s
        let w = 1083usize;
        let h = 326usize;
        let scroll = 200usize;
        let a = mk_frame(w, h, |row, col| row_col_val(row, col));
        let b = mk_frame(w, h, |row, col| {
            let src = if row + scroll < h { row + scroll } else { h + row };
            row_col_val(src, col)
        });
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        let s = try_append_scrolled(&a, &b, w as u32, &mut stitched, &mut stitched_h);
        assert_eq!(s, Some(scroll), "strict 应测出真实滚动量 {scroll}");
        // 拼接高度 = 视口高 + 新进入的 scroll 行
        assert_eq!(stitched_h as usize, h + scroll);
        assert_eq!(stitched.len(), (h + scroll) * w * 4);
    }

    /// `try_append_scrolled`：内容没动（相同帧）→ 返回 None，不拼接（防重复/假偏移）。
    #[test]
    fn try_append_static_frame_noop() {
        let w = 200usize;
        let h = 200usize;
        let a = mk_frame(w, h, |row, col| row_col_val(row, col));
        let b = a.clone();
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        let s = try_append_scrolled(&a, &b, w as u32, &mut stitched, &mut stitched_h);
        assert_eq!(s, None);
        assert_eq!(stitched_h, a.height);
    }
}
