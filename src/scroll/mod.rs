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
/// 稳定等待每轮的间隔
const STARTUP_STABLE_POLL: Duration = Duration::from_millis(50);
/// 稳定等待的最大轮数（50ms×10 ≈ 0.5s；超时用最近一帧兜底，不强求静止）
const STARTUP_STABLE_ATTEMPTS: usize = 20;
/// 首帧「真静止」需要连续多少帧逐字节相同（见 `run_manual_scroll_capture` 首帧逻辑）
const STARTUP_STABLE_REPEATS: usize = 2;
/// 手动模式最大迭代次数（50ms × 20k ≈ 16 分钟，纯兜底；正常由用户点「完成」结束）
const MAX_MANUAL_ITERS: usize = 20_000;
/// 手动模式「高置信优先」的等待时长：这段时间内只接受高置信（静止帧、逐字节对齐）
/// 的拼接结果；超时才放开为宽判据。
///
/// 用**时长**而不是轮数：一轮的耗时取决于抓帧速度（轮询 12ms + 抓帧 20~40ms），
/// 用轮数会让策略随机器性能漂移。700ms 覆盖一次 Chrome 平滑滚动动画（~250ms）
/// 加下一格的静止段：动画结束后的静止帧必然是高置信的，接缝能逐像素对齐；
/// 用户若一直快速滚动（700ms 内都没有静止帧），才放开兜底——宁可接缝差 1~2 行，
/// 也不能整段丢内容。
const EXACT_WAIT: Duration = Duration::from_millis(700);

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

            // 优先用精层判定滚动量（整行哈希 + 固定栏识别）：比 8 列采样签名更准，
            // 同时给出固定底栏行数（追加时排除，避免每帧把页脚复制进长图）。
            //
            // 帧签名（整行哈希 / 8 列行签名）这一轮**只算一次**：下面所有判定
            // （粗层 `find_scroll_delta`、兜底估计、固定底栏）都借用同一份，且
            // `try_append_scrolled` 直接收下这里已算出的 plan，不再重算一遍。
            let mut sigs = stitch::FrameSigs::new(&a, &b);
            let mut plan = sigs.as_ref().and_then(|s| s.pair().plan_append());
            // 内容在变动但检测失败（平滑滚动动画未结束）→ 多等一次重抓同一内容
            if plan.is_none() && frames_differ(&a, &b) {
                std::thread::sleep(EXTRA_SETTLE);
                if let Ok(b2) = capture.capture_area(x, y, w, h) {
                    if b2.width == a.width && b2.height == a.height {
                        b = b2;
                        // 换了帧 → 旧签名作废，重建一组（依旧只算一次）
                        sigs = stitch::FrameSigs::new(&a, &b);
                        plan = sigs.as_ref().and_then(|s| s.pair().plan_append());
                    }
                }
            }

            match plan {
                Some(p) if p.s >= MIN_SCROLL => {
                    // 拼接 b 底部新进入视口的 p.s 行（固定底栏由 p.band 排除）。
                    // 空白段刚过时基线可能局部均匀，但精层/粗层能给出偏移就说明重叠带
                    // 已通过校验（纯空白基线会因能量门返回 None）；丢弃它会白白丢掉真实滚动量。
                    let after_blank = blank_streak > 0;
                    // 校准每 tick 像素：本轮注入 ticks 格、浏览器滚动了 p.s 像素。
                    let per = p.s as f64 / ticks.max(1) as f64;
                    px_per_tick = if px_per_tick <= 0.0 {
                        per
                    } else {
                        px_per_tick * 0.8 + per * 0.2
                    };
                    if append_new_rows(
                        &mut stitched,
                        &mut stitched_h,
                        &a,
                        &b,
                        frame_w,
                        p.s,
                        p.band,
                    )
                    .is_some()
                    {
                        progress_h.store(stitched_h, Ordering::Relaxed);
                    }
                    a = b;
                    streak = 0;
                    blank_streak = 0;
                    ticks = TICKS_PER_ITER;
                    tracing::info!(
                        "[scroll] iter={iter} delta={} band={} exact={}‰{} via_exact={} ticks={ticks} stitched_h={stitched_h} after_blank={after_blank}",
                        p.s,
                        p.band,
                        p.permille,
                        p.exact,
                        p.via_exact
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
                    // 复用本轮已算好的签名与 plan（`plan` 此处必为 None，正是上面那次判定）。
                    let appended = sigs.as_ref().and_then(|sg| {
                        try_append_scrolled(
                            &sg.pair(),
                            plan,
                            frame_w,
                            &mut stitched,
                            &mut stitched_h,
                            false,
                        )
                    });
                    if let Some(s) = appended {
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
                        let appended = sigs.as_ref().and_then(|sg| {
                            try_append_scrolled(
                                &sg.pair(),
                                plan,
                                frame_w,
                                &mut stitched,
                                &mut stitched_h,
                                false,
                            )
                        });
                        if let Some(s) = appended {
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
                                if let Some(s) = try_append_pair(
                                    &a,
                                    &b,
                                    frame_w,
                                    &mut stitched,
                                    &mut stitched_h,
                                    false,
                                ) {
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
        // 首帧必须是**真静止**帧（连续 `STARTUP_STABLE_REPEATS` 帧逐字节相同）。
        //
        // 为什么不能用「变化小于阈值」判静止：平滑滚动的缓出尾段每帧只差几个像素
        // （frac=0.99 时每通道差 ≤1），阈值判据会把这种**亚像素混合帧**当首帧收下。
        // 混合帧按任何整数行偏移都对不齐（整行哈希永远不相等）→ 整场都测不出滚动量，
        // 最后只能退回粗估拼接 → 接缝错位（用户反馈的「第一页尾行和第二页首行对不上」）。
        // 真正静止的画面是逐字节相同的，等它出现即可；超时才用最近一帧兜底。
        let mut a = capture.capture_area(x, y, w, h)?;
        let mut stable = 0usize;
        for _ in 0..STARTUP_STABLE_ATTEMPTS {
            std::thread::sleep(STARTUP_STABLE_POLL);
            let Ok(f) = capture.capture_area(x, y, w, h) else {
                break;
            };
            if f.width != a.width || f.height != a.height {
                a = f;
                stable = 0;
                continue;
            }
            if max_frame_diff(&a, &f) == 0 {
                stable += 1;
                a = f;
                if stable >= STARTUP_STABLE_REPEATS {
                    break; // 画面真的静止了
                }
            } else {
                stable = 0;
                a = f;
            }
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
        // 「高置信优先」：截止时刻之前只接受高置信结果；每次成功拼接后重新计时
        let mut strict_until = std::time::Instant::now() + EXACT_WAIT;

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

            // 拼接尝试（**精层优先**，每轮都问一次）：精层用整行哈希确认「内容确实
            // 滚了、滚了多少」并识别固定底栏，因此比原先「先要求粗层 find_scroll_delta
            // 成功才拼」更少漏拼（这是「只拼到第一页」的根因），也更少重复（周期假峰被
            // 逐字节对齐 + 重复否决拦下）。判定不了（静止 / 动画模糊 / 疑似重复）→ None，
            // 落到下面的分支继续等状态明确，宁缺毋滥。
            //
            // 帧签名（整行哈希 / 行签名）本轮只建一次：plan_append、粗层、向上滚判定
            // 共用同一对 FrameSig，不再各自把整帧重扫一遍。
            let tolerant = std::time::Instant::now() >= strict_until;
            let sigs = stitch::FrameSigs::new(&anchor, &b);
            let pair = sigs.as_ref().map(|sg| sg.pair());
            let plan = pair.as_ref().and_then(|p| p.plan_append());
            if let Some(s) = pair.as_ref().and_then(|p| {
                try_append_scrolled(
                    p,
                    plan,
                    frame_w,
                    &mut stitched,
                    &mut stitched_h,
                    !tolerant,
                )
            }) {
                progress_h.store(stitched_h, Ordering::Relaxed);
                tracing::info!(
                    "[scroll-manual] iter={iter} append s={s} stitched_h={stitched_h} maxdiff={}",
                    max_frame_diff(&anchor, &b)
                );
                anchor = Arc::new(b);
                prev = Some(anchor.clone());
                moving_frames = 0;
                strict_until = std::time::Instant::now() + EXACT_WAIT;
                continue;
            }

            // 检测失败：区分「向上滚」「还在滚动动画中」「静止在新位置」
            // 向上滚判定的帧对是 (b, anchor)，正是上面那对的**反向**：复用同一批签名，
            // 这里一次像素都不用重扫。
            if sigs.as_ref().is_some_and(|sg| sg.reversed().scroll_up_delta().is_some()) {
                // 反向检测命中 → 用户向上滚了：保持 anchor 在最深基线不动，
                // 之后滚回原位/继续向下时只追加超出当前拼接底部的真正新内容，
                // 避免把已拼接的行重复拼进去
                moving_frames = 0;
                tracing::info!("[scroll-manual] iter={iter} scrolled_up keep_baseline");
            } else if prev.as_ref().is_some_and(|p| frames_differ(p, &b)) {
                // 相邻帧仍在变化 → 平滑滚动动画进行中。
                //
                // **这里绝不能 sleep**：原来等 EXTRA_SETTLE(250ms)+SETTLE_DELAY 再重抓，
                // 而平滑滚动一格动画约 250ms、动画结束后的静止段只有 ~150ms——一睡就正好
                // 跳过下一格唯一的静止帧，而静止帧是**唯一能逐字节对齐**的帧（模糊帧按任何
                // 整数偏移都对不齐）。结果是整场都拿不到精确滚动量，最后只能用粗估拼，
                // 接缝就错位。改成 12ms 快速轮询：动画一结束，下一轮循环顶部的精层拼接
                // 立刻就能用静止帧拼上（对得上的帧自己会来）。
                moving_frames += 1;
                tracing::debug!(
                    "[scroll-manual] iter={iter} animating moving_frames={moving_frames} maxdiff={}",
                    max_frame_diff(&anchor, &b)
                );
                continue;
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
                    // 恢复成功的那一帧暂存在这里，循环结束后才写回 anchor。原因：本轮帧
                    // 签名借用着 anchor 指向的帧，在借用区间内给 anchor 赋值会被借用检查器
                    // 拒绝（旧代码没有帧签名对象，可以就地赋值）。
                    let mut recovered_frame: Option<CapturedFrame> = None;
                    while tried < 3 {
                        std::thread::sleep(EXTRA_SETTLE);
                        if let Ok(f2) = capture.capture_area(x, y, w, h) {
                            if f2.width == anchor.width && f2.height == anchor.height {
                                if let Some(s) = try_append_pair(
                                    &anchor,
                                    &f2,
                                    frame_w,
                                    &mut stitched,
                                    &mut stitched_h,
                                    !tolerant,
                                ) {
                                    progress_h.store(stitched_h, Ordering::Relaxed);
                                    tracing::info!(
                                        "[scroll-manual] iter={iter} low_energy_recovered s={s} stitched_h={stitched_h}"
                                    );
                                    // 关键：从 `while` 里 break 出来 + 置 recovered_frame，
                                    // 让下方跳过 append_estimate。否则 continue 只继续内层
                                    // while，落空后仍会走 append_estimate，用**已前移**的
                                    // anchor 对比**旧的**低能耗帧 b → 误取一个 s → 拼接重叠
                                    // 区 → 序号重复（如 iter32 low_energy_recovered s=178 后
                                    // 又 append_estimate s=104，重复 23/24/25）。
                                    recovered_frame = Some(f2);
                                    break;
                                }
                                if avg_adjacent_diff(&f2) >= TEXTURED_ENERGY {
                                    break; // 渲染完成且内容已稳定
                                }
                            }
                        }
                        tried += 1;
                    }
                    if let Some(f2) = recovered_frame {
                        anchor = Arc::new(f2);
                        prev = Some(anchor.clone());
                        moving_frames = 0;
                        continue; // 已成功拼接，跳到下一次外循环，避免重复拼接同一段
                    }
                    tracing::info!(
                        "[scroll-manual] iter={iter} low_energy_still_blank energy={energy:.1} maxdiff={md}"
                    );
                }
                // 低能量兜底 + 正常纹理路径都到这里：优先宽松估计，避免丢段。
                // anchor 与 b 都没变 → 直接复用本轮顶部的帧对与 plan（一个像素都不重扫）。
                let appended = pair.as_ref().and_then(|p| {
                    try_append_scrolled(
                        p,
                        plan,
                        frame_w,
                        &mut stitched,
                        &mut stitched_h,
                        !tolerant,
                    )
                });
                if let Some(s) = appended {
                    progress_h.store(stitched_h, Ordering::Relaxed);
                    tracing::info!(
                        "[scroll-manual] iter={iter} append_estimate s={s} stitched_h={stitched_h} energy={energy:.1} maxdiff={md}"
                    );
                    anchor = Arc::new(b);
                    prev = Some(anchor.clone());
                    moving_frames = 0;
                    continue;
                }
                if !tolerant {
                    // 严格模式：这只是「暂缓」（当前帧还没有高置信对齐），**绝不能**
                    // 把基线跟到新位置——那等于把用户滚过的这一段直接丢掉，长图中间
                    // 出现空档。保持 anchor，下一轮继续用静止帧试。
                    tracing::debug!(
                        "[scroll-manual] iter={iter} defer_keep_anchor energy={energy:.1} maxdiff={md}"
                    );
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

/// 长图尾部是否正好是 anchor 的固定底栏。
///
/// 首帧是**整帧**拼入的，长图尾部因此残留着当时的固定底栏（页脚/分页栏）；
/// 之后每拼一帧又只追加内容行，底栏就永久留在长图中间。检测到这种情形时先把
/// 尾部这段底栏裁掉，长图回到「只存滚动内容」的不变式。
fn stitched_tail_is_band(
    stitched: &[u8],
    frame_w: u32,
    band: usize,
    anchor: &CapturedFrame,
) -> bool {
    let w = frame_w as usize;
    if band == 0 || w == 0 {
        return false;
    }
    let need = band * w * 4;
    if stitched.len() < need || anchor.pixels.len() < need {
        return false;
    }
    let tail = &stitched[stitched.len() - need..];
    let band_px = &anchor.pixels[anchor.pixels.len() - need..];
    // 逐像素比 RGB（alpha 在截图里恒为 255，不参与）
    tail.chunks_exact(4)
        .zip(band_px.chunks_exact(4))
        .all(|(x, y)| x[0] == y[0] && x[1] == y[1] && x[2] == y[2])
}

/// 把「本帧新进入视口的 s 行内容」追加到长图，并统一处理**固定底栏**。
///
/// 两处根因（用户反馈「拼接内容有重复 / 只有第一页」）都在这里收敛：
/// 1. 固定底栏（页脚/分页栏等每帧同一位置重复出现的元素）不随内容滚动。按滚动量
///    整段追加会把底栏一遍遍复制进长图；首帧整帧拼入的那份还会卡在长图中间。
///    因此长图只保留滚动内容：先裁掉尾部残留的底栏，再追加帧里**底栏之上**的 s 行。
/// 2. 帧高减去底栏后剩余的可用内容区不足以容纳一次可靠重叠时，调用方已退回
///    band=0（见 `stitch::plan_append`），这里只做范围校验，绝不越界追加。
///
/// 返回实际追加的行数（= s）；无法追加返回 None（调用方保留基线等下一帧）。
fn append_new_rows(
    stitched: &mut Vec<u8>,
    stitched_h: &mut u32,
    anchor: &CapturedFrame,
    frame: &CapturedFrame,
    frame_w: u32,
    s: usize,
    band: usize,
) -> Option<usize> {
    let w = frame_w as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || s < MIN_SCROLL {
        return None;
    }
    let band = band.min(h.saturating_sub(stitch::MIN_OVERLAP + MIN_SCROLL));
    if band > 0 && stitched_tail_is_band(stitched, frame_w, band, anchor) {
        let cut = band * w * 4;
        if stitched.len() >= cut {
            stitched.truncate(stitched.len() - cut);
            *stitched_h = stitched_h.saturating_sub(band as u32);
            tracing::info!("[scroll] band_trim cut={band} stitched_h={stitched_h}");
        }
    }
    let end = (h - band) * w * 4;
    let start = end.checked_sub(s * w * 4)?;
    if end > frame.pixels.len() || start >= end {
        return None;
    }
    stitched.extend_from_slice(&frame.pixels[start..end]);
    *stitched_h += s as u32;
    Some(s)
}


/// 尝试把 `frame`（滚动后抓到的帧）相对 `anchor` 向下滚动后新进入视口的行拼到 `stitched`。
///
/// 先走严格检测（唯一性 + 匹配率验证，最可靠）；严格检测失败时再走宽松估计
/// （取匹配行数最多的偏移）。宽松估计可能有一两行缝，但**不会像严格检测那样整段
/// 丢内容**——手动滚动的快速滑动 / vxe-table 虚拟表格重建行时，严格检测常返回 None，
/// 若丢弃则长图中间缺内容。
///
/// `pair` 是本轮帧对（内含 a、b 两帧与它们的签名），签名**只算这一次**；`plan` 是调用方
/// 已经算过的 [`stitch::AppendPlan`]（`main`/手动循环里 `plan_append` 通常在判定前就
/// 算过一次了，直接传进来即可，否则这里会把同一对帧重算一遍——那是纯浪费）。
///
/// 返回实际使用的滚动量 s；若严格+估计都测不出（内容真的无重叠，如滚动超过一屏）
/// → `None`，调用方决定是否丢段。
fn try_append_scrolled(
    pair: &stitch::FramePair<'_, '_>,
    plan: Option<stitch::AppendPlan>,
    frame_w: u32,
    stitched: &mut Vec<u8>,
    stitched_h: &mut u32,
    require_exact: bool,
) -> Option<usize> {
    let anchor = pair.a();
    let frame = pair.b();
    // **精层（整行哈希逐字节对齐）先判**，再谈像素均值启发式。
    //
    // 顺序很重要：`mean_unaligned_diff` 是**全帧采样**均值，对**周期性内容**会被稀释——
    // 表格行周期与滚动量成整数倍时，只有窄窄的「序号列」变了，均值差可能只有 3~5，
    // 低于 TRULY_STATIC_MIN(12) 被误判成「没动」→ 整段不拼（长图只到第一页）。
    // 而精层在有信息行上能拿到 1000‰ 命中，能正确量出真实滚动量。所以精层第一优先，
    // 像素均值那道门只用来守下面的粗层/估计路径（它确实需要，见下）。
    if let Some(p) = plan.as_ref() {
        // 精层确认的对齐（多数有信息行命中）：可信，直接拼
        let exact_ok = p.via_exact && p.permille >= stitch::EXACT_PERMILLE_MIN;
        if p.confident || (!require_exact && exact_ok) {
            if let Some(n) = append_new_rows(
                stitched,
                stitched_h,
                anchor,
                frame,
                frame_w,
                p.s,
                p.band,
            ) {
                tracing::info!(
                    "[scroll-manual] try_append_exact s={} band={} exact={} permille={} via_exact={} stitched_h={stitched_h}",
                    p.s,
                    p.band,
                    p.exact,
                    p.permille,
                    p.via_exact
                );
                return Some(n);
            }
        }
    }
    // 严格模式（默认）：没有高置信结果就**暂缓**——保持 anchor，下一轮继续等静止帧
    // （静止帧能给出逐字节对齐的滚动量）。绝不在这里退化成粗估拼接：动画中间帧的
    // 近似滚动量差 1~2 行，拼上去就是可见的接缝错位。
    if require_exact {
        if let Some(p) = plan.as_ref() {
            tracing::debug!(
                "[scroll] try_append defer_unconfident s={} permille={} via_exact={}",
                p.s,
                p.permille,
                p.via_exact
            );
        }
        return None;
    }
    // 真实变化判据（只守下面的粗层/估计路径）：整帧**未对齐**平均差（同一坐标 a vs b）。
    // 静止/闪烁帧（b≈a，仅光标闪动）该值接近 0 → 是「被误取周期大偏移重复拼接」
    // （iter14-37 s=630×N，iter82-84 s=585×3）的元凶，拒绝；真实滚动（内容整体移动、
    // 行数据不同）该值偏高 → 放行。用 mean_unaligned_diff 而非 frames_differ：
    // frames_differ（行匹配比例）对 vxe-table 自相似行失效（真实小滚动也判 false → 丢段）。
    let unaligned = stitch::mean_unaligned_diff(anchor, frame).unwrap_or(u64::MAX);
    // 求 s：find_scroll_delta（行签名，可信）优先，否则 estimate/force 兜底。
    // 三项都读同一份帧签名（`pair`），行签名只算一次。
    let s = match pair.find_scroll_delta() {
        Some(s) if s >= MIN_SCROLL => s,
        _ => pair
            .estimate_scroll_delta(false)
            .or_else(|| pair.estimate_scroll_delta(true))?,
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
    // 精层给出偏移但未达高置信（如模糊帧命中率 500~900‰）：宽判据下可用
    if let Some(p) = plan.as_ref() {
        if let Some(n) = append_new_rows(
            stitched,
            stitched_h,
            anchor,
            frame,
            frame_w,
            p.s,
            p.band,
        ) {
            tracing::info!(
                "[scroll-manual] try_append_exact s={} band={} exact={} permille={} via_exact={} stitched_h={stitched_h}",
                p.s,
                p.band,
                p.exact,
                p.permille,
                p.via_exact
            );
            return Some(n);
        }
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
    if s < MIN_SCROLL || s >= frame.height as usize {
        return None;
    }
    tracing::info!(
        "[scroll-manual] try_append s={s} unaligned={unaligned} coarse_diff={coarse_diff} best={:?} maxdiff={}",
        refined,
        max_frame_diff(anchor, frame)
    );
    // 追加出口统一走 append_new_rows：固定底栏排除 + 长图尾部残留底栏裁剪。
    // 底栏同样读 `pair` 的整行哈希（与精层共用，不再重扫一遍帧）。
    let band = pair.fixed_bottom_band();
    append_new_rows(stitched, stitched_h, anchor, frame, frame_w, s, band)
}

/// 「自带签名」的拼接尝试：自己建帧签名、算一次 plan，再交给 [`try_append_scrolled`]。
///
/// 只在调用方手上**没有**现成帧对时用（如刚重抓的一帧）；主循环里已经有本轮帧对，
/// 就直接把 pair/plan 传给 [`try_append_scrolled`]，别在这里重建。
fn try_append_pair(
    anchor: &CapturedFrame,
    frame: &CapturedFrame,
    frame_w: u32,
    stitched: &mut Vec<u8>,
    stitched_h: &mut u32,
    require_exact: bool,
) -> Option<usize> {
    let sigs = stitch::FrameSigs::new(anchor, frame)?;
    let pair = sigs.pair();
    let plan = pair.plan_append();
    try_append_scrolled(&pair, plan, frame_w, stitched, stitched_h, require_exact)
}

fn frames_differ(a: &CapturedFrame, b: &CapturedFrame) -> bool {
    if a.width != b.width || a.height != b.height {
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
        let s = try_append_pair(&a, &b, w as u32, &mut stitched, &mut stitched_h, false);
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
        let s = try_append_pair(&a, &b, w as u32, &mut stitched, &mut stitched_h, false);
        assert_eq!(s, None);
        assert_eq!(stitched_h, a.height);
    }
    // ─────────────────── 合成「长网页 + 滚动会话」端到端测试 ───────────────────
    //
    // 复现用户反馈的两个症状：拼接只剩第一页 / 拼接内容重复。页面模型贴近真实网页：
    //   * 「序号列」窄带（x < 14）每行唯一，但落在 8 列采样签名**之外**
    //     （采样列从 w/9 开始）——粗层因此看到「多行签名完全相同」；
    //   * 正文区可选**周期重复**（pitch 行一循环），模拟表格/列表这种自相似内容；
    //   * 可选**固定底栏**（页脚/分页栏），每帧同一位置重复出现，不随内容滚动。

    /// 合成长网页（w 像素宽、h 行）。
    struct SynthPage {
        w: usize,
        px: Vec<u8>,
    }

    impl SynthPage {
        /// `pitch` = 正文行重复周期（0 表示每行都不同即无周期）。
        fn new(w: usize, h: usize, pitch: usize) -> Self {
            let mut px = vec![255u8; w * h * 4];
            for r in 0..h {
                for c in 0..w {
                    // 底：白底 + 深色「文字」块
                    let mut v = 245u8;
                    if c >= 14 {
                        let rr = if pitch == 0 { r } else { r % pitch };
                        // 文字块：按 (rr, 列带) 决定，形成一行行的深色小块
                        if row_col_val(rr.wrapping_mul(3), c / 9) % 5 < 2 {
                            v = 40 + row_col_val(rr, c) % 60;
                        }
                    } else {
                        // 序号列：每行唯一（粗层采样列看不到它）
                        v = 20 + row_col_val(r.wrapping_mul(11), c) % 200;
                    }
                    let p = (r * w + c) * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            Self { w, px }
        }

        /// 视口帧：固定顶栏 ft 行 + 滚动内容 + 固定底栏 fb 行（固定栏每帧都一样）。
        fn frame(&self, scroll: usize, vh: usize, ft: usize, fb: usize) -> CapturedFrame {
            let w = self.w;
            let mut px = vec![0u8; w * vh * 4];
            let content = vh - ft - fb;
            for i in 0..ft {
                // 固定顶栏：固定花纹
                for c in 0..w {
                    let v = 200 + row_col_val(9_999, c / 7) % 40;
                    let p = (i * w + c) * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            for i in 0..content {
                let src = (scroll + i) * w * 4;
                let dst = (ft + i) * w * 4;
                px[dst..dst + w * 4].copy_from_slice(&self.px[src..src + w * 4]);
            }
            for i in 0..fb {
                // 固定底栏：深色条 + 顶边线 + 按钮块（与页面内容无关，逐行有差异，
                // 也就是「有纹理」——纯色条会被按内容处理，见 stitch::run_textured）
                let dst = (ft + content + i) * w * 4;
                for c in 0..w {
                    let v = if i == 0 {
                        130
                    } else if i > 6 && i < 30 && (c / 40) % 3 == 1 {
                        205
                    } else {
                        45
                    };
                    let p = dst + c * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            CapturedFrame {
                width: w as u32,
                height: vh as u32,
                pixels: px,
            }
        }
    }

    /// 逐行比较长图与页面：返回 (首行不匹配的位置, 行数)
    fn first_mismatch(stitched: &[u8], frame_w: usize, page: &SynthPage, scroll_base: usize) -> Option<usize> {
        let rows = stitched.len() / (frame_w * 4);
        for r in 0..rows {
            let got = &stitched[r * frame_w * 4..(r + 1) * frame_w * 4];
            let want = &page.px[(scroll_base + r) * frame_w * 4..(scroll_base + r + 1) * frame_w * 4];
            if got != want {
                return Some(r);
            }
        }
        None
    }

    /// 端到端：连续滚动拼接后，长图内容必须**逐行等于**页面对应行
    /// （既不缺行 = 只拼一页/跳号，也不重复 = 重复拼接）。
    #[test]
    fn synth_session_no_band_stitches_exactly() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 0usize);
        let page = SynthPage::new(w, 4000, 0);
        let deltas = [250usize, 180, 320, 90, 240, 137];
        let mut scroll = 0usize;
        let mut anchor = page.frame(0, vh, ft, fb);
        let mut stitched = anchor.pixels.clone();
        let mut stitched_h = anchor.height;
        for d in deltas {
            scroll += d;
            let f = page.frame(scroll, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d, "滚动量必须精确等于真实值");
            assert_eq!(plan.band, 0);
            let n = append_new_rows(&mut stitched, &mut stitched_h, &anchor, &f, w as u32, plan.s, plan.band);
            assert_eq!(n, Some(d));
            anchor = f;
        }
        assert_eq!(
            stitched_h as usize,
            vh + deltas.iter().sum::<usize>(),
            "长图高度 = 视口 + 累计滚动量"
        );
        assert_eq!(
            first_mismatch(&stitched, w, &page, 0),
            None,
            "长图内容必须与页面逐行一致（无缺行、无重复）"
        );
    }

    /// 周期性内容（表格/列表自相似行）：粗层（8 列采样）会被周期整数倍骗到，
    /// 精层（整行哈希，含序号列）必须钉住真实滚动量。
    #[test]
    fn synth_periodic_rows_use_true_delta() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 0usize);
        let page = SynthPage::new(w, 6000, 30); // 正文每 30 行一循环
        let anchor = page.frame(0, vh, ft, fb);
        for d in [17usize, 43, 137, 301, 59] {
            let f = page.frame(d, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("周期内容也必须测出滚动量");
            assert_eq!(plan.s, d, "周期内容被周期整数倍骗到 → 会重复拼接");
            assert!(plan.via_exact, "应由精层确认");
        }
    }

    /// 固定底栏（页脚/分页栏）不能随每帧追加重复拼入长图，且首帧整帧拼入后
    /// 残留在长图中间的底栏要被裁掉——长图只保留滚动内容。
    #[test]
    fn synth_fixed_bottom_band_stitched_once() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 60usize);
        let page = SynthPage::new(w, 4000, 0);
        let mut scroll = 0usize;
        let mut anchor = page.frame(0, vh, ft, fb);
        let mut stitched = anchor.pixels.clone(); // 引擎行为：首帧整帧拼入
        let mut stitched_h = anchor.height;
        // 单次滚动量受「内容区 - 最小重叠」限制（固定底栏占掉 60 行）
        let deltas = [250usize, 180, 260, 90];
        for d in deltas {
            scroll += d;
            let f = page.frame(scroll, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d);
            assert_eq!(plan.band, fb, "固定底栏行数必须被识别");
            assert!(append_new_rows(
                &mut stitched,
                &mut stitched_h,
                &anchor,
                &f,
                w as u32,
                plan.s,
                plan.band
            )
            .is_some());
            anchor = f;
        }
        // 长图 = 首帧内容区 + 累计滚动量（底栏不留中间、不重复）
        assert_eq!(stitched_h as usize, vh - fb + deltas.iter().sum::<usize>());
        assert_eq!(
            first_mismatch(&stitched, w, &page, 0),
            None,
            "长图内容必须与页面逐行一致（固定底栏不得重复拼入）"
        );
    }

    /// 静止帧（内容没动）不得拼入：`plan_append` 之外由 `TRULY_STATIC_MIN` 把关，
    /// 这里确认 `try_append_scrolled` 整体行为（宁缺毋滥，不拼重复内容）。
    #[test]
    fn synth_static_periodic_frame_not_appended() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 0usize);
        let page = SynthPage::new(w, 4000, 30);
        let a = page.frame(0, vh, ft, fb);
        let b = a.clone();
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        assert_eq!(
            try_append_pair(&a, &b, w as u32, &mut stitched, &mut stitched_h, false),
            None,
            "静止帧不得拼接"
        );
        assert_eq!(stitched_h, a.height);
    }

    /// 帧底部是**纯色/空白**段时，不能当成「固定底栏」排除：空白段在任意滚动下
    /// 同一位置都相等，误判为固定栏会让追加窗口整体上移 → 拼进已拼过的内容（重复）。
    #[test]
    fn synth_uniform_bottom_not_treated_as_band() {
        let w = 600usize;
        let (vh, ft) = (400usize, 0usize);
        let mut page = SynthPage::new(w, 4000, 0);
        // 页面末段 220 行是纯白（模拟内容结束后的空白），并不随滚动「固定」
        for r in (600..820).rev() {
            for c in 0..w {
                let p = (r * w + c) * 4;
                page.px[p] = 255;
                page.px[p + 1] = 255;
                page.px[p + 2] = 255;
            }
        }
        let mut scroll = 0usize;
        let mut anchor = page.frame(0, vh, ft, 0);
        let mut stitched = anchor.pixels.clone();
        let mut stitched_h = anchor.height;
        for d in [250usize, 180, 220] {
            scroll += d;
            let f = page.frame(scroll, vh, ft, 0);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d);
            // 底栏判定的取舍：**宁可多算**（底部纯白段可能被算进底栏）。多算只会让长图
            // 少掉几行纯色/空白（首帧尾部那段底栏多裁一点，肉眼无差别）；少算才是灾难
            // ——追加窗口会越过内容末尾伸进底栏，每次拼接都把底栏像素拼进长图，
            // 表现为「最后一页和倒数第二页部分内容重复」。
            assert_eq!(
                append_new_rows(
                    &mut stitched,
                    &mut stitched_h,
                    &anchor,
                    &f,
                    w as u32,
                    plan.s,
                    plan.band,
                ),
                Some(d),
                "每次追加必须正好补上滚动量"
            );
            anchor = f;
        }
        // 高度只会因「多算底栏」而略短：首帧尾部那段底栏（最多 h/2）被裁掉一次，
        // 可见内容不受影响（下面按内容逐行核对）。
        let ideal = vh + 250 + 180 + 220;
        assert!(
            (stitched_h as usize) <= ideal && stitched_h as usize + vh / 2 >= ideal,
            "长图高度 {} 偏离理想值 {ideal} 过多",
            stitched_h
        );
        // 跳过固定顶栏（它对的是固定花纹，不是页面行），只看滚动内容部分
        assert_eq!(
            first_mismatch(&stitched[ft * w * 4..], w, &page, 0),
            None,
            "含纯白段的页面也必须逐行对齐（不得重复/缺行）"
        );
    }

    /// 固定顶栏 + 固定底栏同时存在（真实网页：导航栏 + 页脚/分页栏）：
    /// 长图 = 顶栏 + 滚动内容，底栏既不重复拼入也不留在中间。
    #[test]
    fn synth_top_and_bottom_bands_session() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let page = SynthPage::new(w, 4000, 0);
        let mut scroll = 0usize;
        let mut anchor = page.frame(0, vh, ft, fb);
        let mut stitched = anchor.pixels.clone();
        let mut stitched_h = anchor.height;
        let deltas = [220usize, 150, 260, 110];
        for d in deltas {
            scroll += d;
            let f = page.frame(scroll, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d);
            assert_eq!(plan.band, fb, "固定底栏必须识别");
            assert_eq!(plan.top, ft, "固定顶栏必须识别");
            assert_eq!(plan.permille, 1000, "静止帧的真实偏移应有信息行全中");
            assert!(append_new_rows(
                &mut stitched,
                &mut stitched_h,
                &anchor,
                &f,
                w as u32,
                plan.s,
                plan.band
            )
            .is_some());
            anchor = f;
        }
        // 顶栏保留在顶部；其后是滚动内容；底栏不留痕
        assert_eq!(stitched_h as usize, vh - fb + deltas.iter().sum::<usize>());
        for r in ft..(stitched_h as usize) {
            let got = &stitched[r * w * 4..(r + 1) * w * 4];
            let want = &page.px[(r - ft) * w * 4..(r - ft + 1) * w * 4];
            assert_eq!(got, want, "第 {r} 行应为页面第 {} 行", r - ft);
        }
    }

    /// 稀疏页面（大段留白 + 少量正文块）：留白行在任何偏移下都相等，若把它们算进
    /// 命中率会给「小偏移」白送分数 → 选到过小偏移 → 长图缺行。精层只统计有信息行，
    /// 必须钉住真实滚动量。
    #[test]
    fn synth_sparse_page_uses_true_delta() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 0usize);
        let mut page = SynthPage::new(w, 6000, 0);
        // 每 200 行只保留 60 行正文，其余全白
        for r in 0..6000 {
            if r % 200 >= 60 {
                for c in 0..w {
                    let p = (r * w + c) * 4;
                    page.px[p] = 255;
                    page.px[p + 1] = 255;
                    page.px[p + 2] = 255;
                }
            }
        }
        let anchor = page.frame(0, vh, ft, fb);
        // 滚动量 < 正文块间距，保证重叠带里仍有正文（有信息行）可判定
        for d in [73usize, 137, 61] {
            let f = page.frame(d, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("稀疏页面也必须测出滚动量");
            assert_eq!(plan.s, d, "留白把命中率稀释 → 选到过小偏移（缺行）");
            assert!(plan.via_exact, "应由精层按有信息行命中率确认");
        }
        // 重叠带里没有正文（留白占满）时无法判定 → 宁缺毋滥返回 None，
        // 由上层兜底估计处理，绝不给出「过小偏移」这种会缺行的结果。
        let f = page.frame(261, vh, ft, fb);
        if let Some(plan) = stitch::plan_append(&anchor, &f) {
            assert_eq!(plan.s, 261);
        }
    }

    // ─────────────── 端到端：假屏幕 + 假进度窗，真跑手动滚动引擎 ───────────────
    //
    // 上面几个测试只验证 `plan_append`/`append_new_rows` 这两个纯函数；这里再跑一遍
    // **真实引擎循环**（`run_manual_scroll_capture`），覆盖锚点推进、兜底分支、
    // 固定栏裁剪等集成行为：屏幕按时间推进模拟用户持续滚动，进度窗到点自动「完成」。

    use crate::capture::DisplayInfo;

    /// 假屏幕：按时间推进的滚动页面（每 px_per_sec 像素/秒）。
    struct FakeScrollingScreen {
        page: std::sync::Arc<SynthPage>,
        vh: usize,
        ft: usize,
        fb: usize,
        page_rows: usize,
        px_per_sec: f64,
        t0: std::time::Instant,
    }

    impl ScreenCapture for FakeScrollingScreen {
        fn capture_primary(&self) -> AppResult<CapturedFrame> {
            let w = self.page.w as u32;
            self.capture_area(0, 0, w, self.vh as u32)
        }

        fn capture_area(&self, _x: i32, _y: i32, _w: u32, _h: u32) -> AppResult<CapturedFrame> {
            let t = self.t0.elapsed().as_secs_f64();
            let max_scroll = self.page_rows.saturating_sub(self.vh);
            let scroll = ((t * self.px_per_sec) as usize).min(max_scroll);
            Ok(self.page.frame(scroll, self.vh, self.ft, self.fb))
        }

        fn list_displays(&self) -> Vec<DisplayInfo> {
            Vec::new()
        }
    }

    /// 假进度窗：到点自动置 `done`（等价用户点「完成」），让引擎干净收尾。
    struct FakeProgress {
        done_after: Duration,
    }

    impl ScrollProgress for FakeProgress {
        fn show(
            &self,
            _region: &Bounds,
            _screen_bounds: &Bounds,
            _cancel: Arc<AtomicBool>,
            _done: Arc<AtomicBool>,
            _progress: Arc<AtomicU32>,
        ) {
        }

        #[allow(clippy::too_many_arguments)]
        fn show_manual(
            &self,
            _region: &Bounds,
            _screen_bounds: &Bounds,
            _cancel: Arc<AtomicBool>,
            done: Arc<AtomicBool>,
            _progress: Arc<AtomicU32>,
            _moving: Arc<AtomicBool>,
            _bottom_has_content: Arc<AtomicBool>,
            _confirming: Arc<AtomicBool>,
        ) {
            let d = self.done_after;
            std::thread::spawn(move || {
                std::thread::sleep(d);
                done.store(true, Ordering::Relaxed);
            });
        }

        fn hide(&self) {}
    }

    /// 长图内容必须是页面的**连续切片**：返回匹配行数，遇到不连续直接 panic。
    /// （连续 = 既没有重复行、也没有跳过行——正是用户反馈的两个症状。）
    fn assert_contiguous_slice(
        stitched: &[u8],
        frame_w: usize,
        page: &SynthPage,
        content_top: usize,
        page_rows: usize,
    ) -> usize {
        let row_bytes = frame_w * 4;
        let rows = stitched.len() / row_bytes;
        let first = &stitched[content_top * row_bytes..(content_top + 1) * row_bytes];
        let mut start = None;
        for o in 0..page_rows {
            if &page.px[o * row_bytes..(o + 1) * row_bytes] == first {
                start = Some(o);
                break;
            }
        }
        let start = start.expect("长图首行必须能在页面里找到");
        let n = rows - content_top;
        for i in 0..n {
            let o = start + i;
            assert!(o < page_rows, "长图第 {i} 行超出页面范围（多了内容）");
            let got = &stitched[(content_top + i) * row_bytes..(content_top + i + 1) * row_bytes];
            let want = &page.px[o * row_bytes..(o + 1) * row_bytes];
            assert_eq!(got, want, "长图第 {i} 行与页面第 {o} 行不一致（重复或跳行）");
        }
        n
    }

    /// 端到端：手动滚动引擎必须拼出「页面的连续长片」，且远多于一屏
    /// （只拼一页 = 长度接近一屏；重复拼接 = 连续切片校验失败）。
    #[test]
    fn engine_manual_session_stitches_contiguous_long_image() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let page_rows = 4000usize;
        let page = std::sync::Arc::new(SynthPage::new(w, page_rows, 0));
        let screen = FakeScrollingScreen {
            page: page.clone(),
            vh,
            ft,
            fb,
            page_rows,
            px_per_sec: 900.0,
            t0: std::time::Instant::now(),
        };
        let progress = FakeProgress {
            done_after: Duration::from_millis(2800),
        };
        let region = Bounds {
            origin: crate::utils::bounds::Point::new(0.0, 0.0),
            size: crate::utils::bounds::Point::new(w as f32, vh as f32),
        };
        let out = run_manual_scroll_capture(&region, &region, &screen, &progress)
            .expect("引擎不应报错")
            .expect("点「完成」应返回拼接结果");
        assert_eq!(out.width as usize, w);
        let n = assert_contiguous_slice(&out.pixels, w, &page, ft, page_rows);
        // 1.2s × 900px/s ≈ 1080px ≈ 2.7 屏；要求至少 2 屏，足以证明不是「只有第一页」
        assert!(
            n >= 2 * (vh - fb),
            "只拼了 {n} 行（约 {:.1} 屏），疑似「只拼第一页」",
            n as f64 / (vh - fb) as f64
        );
    }

    // ───────────── 复现用户反馈：接缝错位（1~2 行）与底栏重复 ─────────────

    /// 固定底栏**上方还有一段纯色/空白**：底栏判定不能把那段空白也算进底栏，
    /// 否则追加窗口整体上移 → 拼进已拼过的内容（重复 + 接缝错位）。
    #[test]
    fn synth_band_does_not_swallow_blank_above() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 0usize, 60usize);
        let page = SynthPage::new(w, 4000, 0);
        let mut page = page;
        // 页面 1200 行之后全白（内容结束），于是帧里底栏上方会出现一段空白
        for r in 1200..4000 {
            for c in 0..w {
                let p = (r * w + c) * 4;
                page.px[p] = 255;
                page.px[p + 1] = 255;
                page.px[p + 2] = 255;
            }
        }
        let mut scroll = 900usize;
        let mut anchor = page.frame(scroll, vh, ft, fb);
        let mut stitched = anchor.pixels.clone();
        let mut stitched_h = anchor.height;
        for d in [120usize, 90, 70] {
            scroll += d;
            let f = page.frame(scroll, vh, ft, fb);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d);
            // 底栏上方的空白可能被一起算进底栏（见 `stitch::fixed_bottom_band` 的取舍）：
            // 只多裁几行纯色/空白，可见内容不多不少（下面按内容逐行核对）。
            assert_eq!(
                append_new_rows(
                    &mut stitched,
                    &mut stitched_h,
                    &anchor,
                    &f,
                    w as u32,
                    plan.s,
                    plan.band,
                ),
                Some(d),
                "每次追加必须正好补上滚动量"
            );
            anchor = f;
        }
        // 关键：底栏上方的空白被算成底栏后**不得**把可见内容拼重复/拼错位
        assert_eq!(
            first_mismatch(&stitched, w, &page, 900),
            None,
            "底栏上方有空白时仍须逐行对齐（不得重复可见内容）"
        );
        let ideal = vh - fb + 120 + 90 + 70;
        assert!(
            (stitched_h as usize) <= ideal && stitched_h as usize + vh / 2 >= ideal,
            "长图高度 {} 偏离理想值 {ideal} 过多",
            stitched_h
        );
    }

    /// 回归：**底栏判定偏矮不得让长图重复可见内容**。
    ///
    /// 用户反馈「长图最后一页和倒数第二页部分内容重复」。底栏判定曾用
    /// 「尾部相同段 − 段首纯色块」：真实页脚常常是**整条纯色 + 顶部一条细线**
    /// （也就是底栏自己开头就有多行完全相同的纯色行），减法会把这几行当成「内容空白」
    /// 减掉 → 底栏判矮 → 追加窗口越过内容末尾往回退几行 → 每次拼接都重复几行**可见内容**。
    ///
    /// 判据：长图的滚动内容必须与页面逐行对齐（重复即错位），且不得出现底栏独有的
    /// 整行 130 灰（页面自身没有这种行）。
    #[test]
    fn synth_solid_leading_footer_does_not_duplicate_content() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let content = vh - ft - fb;
        let page = SynthPage::new(w, 4000, 0);
        let row_bytes = w * 4;
        // 底栏：前 4 行纯色 45、第 5 行 130 灰细线、其余 45/205 交替（有纹理）
        let build = |scroll: usize| -> CapturedFrame {
            let mut px = vec![0u8; w * vh * 4];
            for i in 0..ft {
                for c in 0..w {
                    let v = 200 + row_col_val(9_999, c / 7) % 40;
                    let p = (i * w + c) * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            for i in 0..content {
                let src = (scroll + i) * row_bytes;
                let dst = (ft + i) * row_bytes;
                px[dst..dst + row_bytes].copy_from_slice(&page.px[src..src + row_bytes]);
            }
            for i in 0..fb {
                let v: u8 = if i < 4 {
                    45
                } else if i == 4 {
                    130
                } else if (i / 3) % 2 == 0 {
                    45
                } else {
                    205
                };
                let dst = (ft + content + i) * row_bytes;
                for c in 0..w {
                    let p = dst + c * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            CapturedFrame {
                width: w as u32,
                height: vh as u32,
                pixels: px,
            }
        };
        let mut bar_row = Vec::with_capacity(row_bytes);
        for _ in 0..w {
            bar_row.extend_from_slice(&[130, 130, 130, 255]);
        }
        let count_bar_rows = |buf: &[u8]| {
            (0..buf.len() / row_bytes)
                .filter(|&r| buf[r * row_bytes..(r + 1) * row_bytes] == bar_row[..])
                .count()
        };
        assert_eq!(count_bar_rows(&page.px), 0, "合成页面本身不含底栏那条线");

        let mut scroll = 1000usize;
        let mut anchor = build(scroll);
        let mut stitched = anchor.pixels.clone();
        let mut stitched_h = anchor.height;
        for d in [180usize, 160, 200] {
            scroll += d;
            let f = build(scroll);
            let plan = stitch::plan_append(&anchor, &f).expect("应能测出滚动量");
            assert_eq!(plan.s, d);
            assert!(
                plan.band >= fb,
                "底栏判定 {} 小于真实底栏 {fb} → 窗口会往回退，拼重复内容",
                plan.band
            );
            assert_eq!(
                append_new_rows(
                    &mut stitched,
                    &mut stitched_h,
                    &anchor,
                    &f,
                    w as u32,
                    plan.s,
                    plan.band,
                ),
                Some(d)
            );
            anchor = f;
        }
        assert_eq!(
            count_bar_rows(&stitched),
            0,
            "长图里出现整行 130 灰 = 底栏像素被拼进来了"
        );
        // 跳过固定顶栏（固定花纹，不是页面行），滚动内容必须逐行对齐——重复即错位
        assert_eq!(
            first_mismatch(&stitched[ft * row_bytes..], w, &page, 1000),
            None,
            "底栏以纯色行打头时，长图不得重复/错位可见内容"
        );
    }

    /// 假屏幕（带平滑滚动动画）：一个滚轮格 = 180px、250ms 缓出动画 + 150ms 静止，
    /// 与 Chrome 平滑滚动一致。动画中间帧是**亚像素混合**帧（逐字节匹配率不到 100%），
    /// 只有静止帧能给出逐字节精确的滚动量。
    struct FakeAnimatedScreen {
        page: std::sync::Arc<SynthPage>,
        vh: usize,
        ft: usize,
        fb: usize,
        page_rows: usize,
        notch_px: f64,
        anim_ms: f64,
        hold_ms: f64,
        t0: std::time::Instant,
    }

    impl FakeAnimatedScreen {
        /// 当前（可能是小数的）滚动位置
        fn scroll_f(&self) -> f64 {
            let t = self.t0.elapsed().as_secs_f64() * 1000.0;
            let cycle = self.anim_ms + self.hold_ms;
            let notches = (t / cycle).floor();
            let phase = t - notches * cycle;
            let done = notches * self.notch_px;
            let max_scroll = self.page_rows.saturating_sub(self.vh) as f64;
            if phase <= self.anim_ms {
                // 缓出：progress = 1-(1-x)^3
                let x = phase / self.anim_ms;
                let e = 1.0 - (1.0 - x).powi(3);
                (done + self.notch_px * e).min(max_scroll)
            } else {
                (done + self.notch_px).min(max_scroll)
            }
        }
    }

    impl ScreenCapture for FakeAnimatedScreen {
        fn capture_primary(&self) -> AppResult<CapturedFrame> {
            self.capture_area(0, 0, self.page.w as u32, self.vh as u32)
        }

        fn capture_area(&self, _x: i32, _y: i32, _w: u32, _h: u32) -> AppResult<CapturedFrame> {
            // 亚像素混合：`scroll_f()` 允许小数滚动量，按小数部分把相邻两行线性插值，
            // 模拟平滑滚动的中间帧（任何整数偏移都对不齐）。
            let sf = self.scroll_f();
            let base = sf.floor() as usize;
            let frac = sf - base as f64;
            let w = self.page.w;
            let content = self.vh - self.ft - self.fb;
            let mut px = vec![0u8; w * self.vh * 4];
            // 固定顶栏/底栏：与整数帧完全一致
            let whole = self.page.frame(0, self.vh, self.ft, self.fb);
            px[..self.ft * w * 4].copy_from_slice(&whole.pixels[..self.ft * w * 4]);
            px[(self.ft + content) * w * 4..]
                .copy_from_slice(&whole.pixels[(self.ft + content) * w * 4..]);
            for i in 0..content {
                let r0 = base + i;
                let r1 = (r0 + 1).min(self.page_rows.saturating_sub(1));
                let dst = (self.ft + i) * w * 4;
                for c in 0..w {
                    let a = self.page.px[(r0 * w + c) * 4] as f64;
                    let b = self.page.px[(r1 * w + c) * 4] as f64;
                    let v = (a * (1.0 - frac) + b * frac).round() as u8;
                    let p = dst + c * 4;
                    px[p] = v;
                    px[p + 1] = v;
                    px[p + 2] = v;
                    px[p + 3] = 255;
                }
            }
            Ok(CapturedFrame {
                width: w as u32,
                height: self.vh as u32,
                pixels: px,
            })
        }

        fn list_displays(&self) -> Vec<DisplayInfo> {
            Vec::new()
        }
    }

    /// 端到端（带平滑滚动动画）：动画中间帧只能给出近似滚动量（差 1~2 行），
    /// 直接拼会在接缝处错位；引擎必须**优先用静止帧**的精确对齐，长图仍逐行连续。
    #[test]
    fn engine_manual_session_seam_is_exact_under_animation() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let page_rows = 6000usize;
        let page = std::sync::Arc::new(SynthPage::new(w, page_rows, 30));
        let screen = FakeAnimatedScreen {
            page: page.clone(),
            vh,
            ft,
            fb,
            page_rows,
            notch_px: 180.0,
            anim_ms: 250.0,
            hold_ms: 150.0,
            t0: std::time::Instant::now(),
        };
        let progress = FakeProgress {
            done_after: Duration::from_millis(2500),
        };
        let region = Bounds {
            origin: crate::utils::bounds::Point::new(0.0, 0.0),
            size: crate::utils::bounds::Point::new(w as f32, vh as f32),
        };
        let out = run_manual_scroll_capture(&region, &region, &screen, &progress)
            .expect("引擎不应报错")
            .expect("点「完成」应返回拼接结果");
        let n = assert_contiguous_slice(&out.pixels, w, &page, ft, page_rows);
        assert!(
            n >= 2 * (vh - fb),
            "只拼了 {n} 行，疑似「只拼第一页」"
        );
    }

    /// 周期性内容（表格行周期 30，滚动量 180 = 6 个周期）：整帧像素均值差被稀释到
    /// 只有 3~5（只有窄窄的序号列变了），**不能**因此判成「没动」而整段不拼——
    /// 精层按有信息行命中率能拿到 1000‰，必须优先采信，拼出真实滚动量。
    #[test]
    fn synth_periodic_gate_does_not_block_append() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let page = SynthPage::new(w, 4000, 30);
        let a = page.frame(0, vh, ft, fb);
        let b = page.frame(180, vh, ft, fb);
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        let s = try_append_pair(&a, &b, w as u32, &mut stitched, &mut stitched_h, true);
        assert_eq!(s, Some(180), "周期内容的真实滚动量必须拼上（否则只到第一页）");
        assert_eq!(stitched_h as usize, vh - fb + 180);
        for r in ft..stitched_h as usize {
            let got = &stitched[r * w * 4..(r + 1) * w * 4];
            let want = &page.px[(r - ft) * w * 4..(r - ft + 1) * w * 4];
            assert_eq!(got, want, "第 {r} 行应为页面第 {} 行", r - ft);
        }
    }

    /// 端到端：一直滚到页面**最底部**再点「完成」（用户实际用法），
    /// 最后一个视口不能与倒数第二个视口重复。
    #[test]
    fn engine_manual_session_to_page_bottom_is_contiguous() {
        let w = 600usize;
        let (vh, ft, fb) = (400usize, 40usize, 60usize);
        let page_rows = 1600usize; // max_scroll = 1200
        let page = std::sync::Arc::new(SynthPage::new(w, page_rows, 30));
        let screen = FakeAnimatedScreen {
            page: page.clone(),
            vh,
            ft,
            fb,
            page_rows,
            notch_px: 173.0, // 最后一格会被页面底夹住 → 变小（部分滚动）
            anim_ms: 250.0,
            hold_ms: 150.0,
            t0: std::time::Instant::now(),
        };
        let progress = FakeProgress {
            done_after: Duration::from_millis(6000),
        };
        let region = Bounds {
            origin: crate::utils::bounds::Point::new(0.0, 0.0),
            size: crate::utils::bounds::Point::new(w as f32, vh as f32),
        };
        let out = run_manual_scroll_capture(&region, &region, &screen, &progress)
            .expect("引擎不应报错")
            .expect("点「完成」应返回拼接结果");
        // 长图内容 = 顶栏之后的连续切片；末尾必须正好到页面最后一行（不能重复/缺尾）
        let n = assert_contiguous_slice(&out.pixels, w, &page, ft, page_rows);
        // 页面滚到底时最后一行可见内容 = page_rows - ft - fb - 1（底栏占掉最后 fb 行）。
        // 长图末尾必须正好落在这一行：多一行 = 重复，少一行 = 缺尾。
        let rows = out.pixels.len() / (w * 4);
        let want_row = page_rows - ft - fb - 1;
        let got = &out.pixels[(rows - 1) * w * 4..rows * w * 4];
        assert_eq!(
            got,
            &page.px[want_row * w * 4..(want_row + 1) * w * 4],
            "长图末尾没有落在页面最后一行（重复或缺尾），共 {n} 行内容"
        );
    }
}
