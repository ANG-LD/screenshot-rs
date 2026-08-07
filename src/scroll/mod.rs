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
/// 每轮滚动后等待平滑滚动稳定
const SETTLE_DELAY: Duration = Duration::from_millis(120);
/// 检测失败后（疑似动画未结束）额外等待再重抓一帧
const EXTRA_SETTLE: Duration = Duration::from_millis(250);
/// 每轮注入的滚轮 tick 数（默认值；检测不到时自适应减半）
const TICKS_PER_ITER: u8 = 2;
/// 最小有效滚动量（低于视为内容没动）
const MIN_SCROLL: usize = 4;
/// 拼接高度上限（防无限滚动 feed 死循环；MAX_ITERS 提供最终兜底）
const MAX_HEIGHT: u32 = 100_000;
/// 最大迭代次数
const MAX_ITERS: usize = 300;
/// 连续无法检测滚动的次数上限（只对「有纹理且静止」累计，动画/步长问题不算）
const MAX_STREAK: usize = 3;
/// 连续低纹理（空白/纯色/平滑段）迭代次数上限——此时无法判定是否在滚动，
/// 先按「还在滚」继续，达到配额仍无变化才放弃
const MAX_BLANK_STREAK: usize = 8;
/// 相邻采样行平均差的阈值：低于此值视为低纹理（空白/纯色/平滑图）
const TEXTURED_ENERGY: f32 = 12.0;
/// 低纹理段每轮注入的滚轮 tick 数：大步长快速滚过空白区
const BLANK_TICKS: u8 = 6;
/// 重定位指针时注入的滚轮 tick 数：更大步长 + 更长等待，能捕获慢滚动/动量
const RELOCATE_TICKS: u8 = 10;
/// 重定位后的稳定等待
const RELOCATE_SETTLE: Duration = Duration::from_millis(300);
/// 重开连接后、注入前等焦点稳定：Electron/Chromium 需要时间注册焦点才接受合成滚轮
const RECONNECT_FOCUS_SETTLE: Duration = Duration::from_millis(100);
/// 重连每批注入的 tick 数（慢速注入，避免被 Chromium 当 fling 丢弃）
const RECONNECT_TICKS: u8 = 2;
/// 重连每批注入后的验证等待
const RECONNECT_VERIFY_WAIT: Duration = Duration::from_millis(200);
/// 重连注入总批数（每批后验证内容是否移动，动了立即复活）
const RECONNECT_ROUNDS: usize = 3;
/// 暂停期间内容静止且指针不回来时允许的最大迭代数（50ms×100 ≈ 5s）。
/// 超时视为本次滚动已卡死，干净收尾返回已拼接结果，避免无限空转。
const MAX_PAUSED_STATIC: usize = 100;

/// 指针偏离注入目标多少像素视为「用户要把鼠标划到别处」（如点进度窗按钮）：
/// 此时暂停注入/重定位，让指针自由移动，回到目标附近再自动恢复
const PAUSE_RADIUS: f64 = 60.0;
/// 暂停期间的轮询间隔
const PAUSE_POLL: Duration = Duration::from_millis(50);
/// 手动滚动模式的轮询间隔
const MANUAL_POLL: Duration = Duration::from_millis(50);
/// 手动模式最大迭代次数（50ms × 20k ≈ 16 分钟，纯兜底；正常由用户点「完成」结束）
const MAX_MANUAL_ITERS: usize = 20_000;

/// 滚动期间进度窗口的显示/隐藏回调（由调用方经 OverlayService 注入）
pub trait ScrollProgress: Send + Sync {
    /// 打开自动滚动进度小窗（摆到不与 region 重叠的屏幕角落）
    fn show(
        &self,
        region: &Bounds,
        screen_bounds: &Bounds,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
    );
    /// 打开手动滚动进度小窗：多一个「完成」按钮，用户滚完点它结束
    fn show_manual(
        &self,
        region: &Bounds,
        screen_bounds: &Bounds,
        cancel: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
        progress: Arc<AtomicU32>,
    );
    /// 关闭进度小窗
    fn hide(&self);
}

/// 运行滚动截屏并返回拼接好的长图（调用方负责写剪贴板）。
///
/// 取消或提前结束时返回已拼接部分（至少首帧）。
pub fn run_scroll_capture(
    region: &Bounds,
    screen_bounds: &Bounds,
    capture: &dyn ScreenCapture,
    progress: &dyn ScrollProgress,
) -> AppResult<CapturedFrame> {
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
    let progress_h = Arc::new(AtomicU32::new(h));
    progress.show(region, screen_bounds, cancel.clone(), progress_h.clone());

    // 内部闭包包住主循环：任何 `?` 提前退出，外层都统一 hide 进度窗，
    // 避免 capture_area 失败时进度窗卡在屏幕上。
    let result = (|| -> AppResult<CapturedFrame> {
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
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        // 诊断：全屏基线，用于对比滚动发生后全屏变化的位置
        let mut last_full = capture.capture_primary().ok();
        let displays = capture.list_displays();
        tracing::info!(
            "[scroll] displays={:?}",
            displays
                .iter()
                .map(|d| format!("{}x{}@{:.1}", d.width, d.height, d.scale_factor))
                .collect::<Vec<_>>()
        );
        if let Some(fl) = &last_full {
            dump_png(fl, "start_full");
        }
        dump_png(&a, "start_region");
        let mut streak = 0usize;
        // 低纹理段（空白/平滑图）的连续迭代计数，不并入 streak
        let mut blank_streak = 0usize;
        // 暂停状态下「内容持续静止」的连续迭代计数：指针离开注入目标后，若内容
        // 也不在动（用户没在手动滚），累计到上限就干净收尾，防止无限空转
        let mut paused_static = 0usize;
        // 自适应滚动步长：检测不到时减半，避免每轮滚动量超过视口导致无法重叠
        let mut ticks = TICKS_PER_ITER;
        let mut stop_reason = "max_iters";

        for (iter, _) in (0..MAX_ITERS).enumerate() {
            if cancel.load(Ordering::Relaxed) {
                stop_reason = "canceled";
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
                let b_energy = b.as_ref().map(|f| avg_adjacent_diff(f)).unwrap_or(0.0);
                let same_size = b.as_ref().map_or(false, |f| f.width == a.width && f.height == a.height);
                let differ = if same_size {
                    frames_differ(&a, b.as_ref().unwrap())
                } else {
                    false
                };
                if differ {
                    // 用户正在手动滚动目标窗口：跟住新基线
                    if let Some(b) = b {
                        a = b;
                    }
                }
                // 暂停中到达底部：内容空白且静止 → 立即停止，不等 MAX_PAUSED_STATIC
                if !differ && b_energy < TEXTURED_ENERGY {
                    stop_reason = "blank_while_paused";
                    break;
                }
                if differ {
                    paused_static = 0;
                } else {
                    paused_static += 1;
                }
                std::thread::sleep(PAUSE_POLL);
                tracing::info!(
                    "[scroll] iter={iter} pointer_moved dist={dist:.0}px pause paused_static={paused_static}"
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

            // 每轮重新 warp 指针到当前落点，防止指针漂移导致滚轮事件没投递到目标窗口；
            // 同时把 X 输入焦点给目标窗口（Chromium/Electron 忽略投给未聚焦窗口的合成滚轮）
            injector.warp_to(warp.0 as i16, warp.1 as i16);
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
                    // 说明重叠带已通过 band_has_energy + verify_rows 验证（纯空白基线
                    // 会因能量门返回 None）；若因此丢弃 delta，会把真实滚动量白白丢掉。
                    let after_blank = blank_streak > 0;
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
                    // 内容动了但检测不到：刷新基线（接受丢掉这一段），步长过大则减半
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
                None => {
                    let energy = avg_adjacent_diff(&b);
                    if energy < TEXTURED_ENERGY {
                        // 低纹理（空白/纯色/平滑图）：无法判定是否在滚动，
                        // 按还在滚继续，大步长滚过这一段；刷新基线
                        blank_streak += 1;
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
                        // 诊断：全屏变化位置 + 区域捕获与全屏裁剪的一致性，
                        // 定位「屏幕在滚但区域 capture 不变」是陈旧帧还是区域错位。
                        if let Ok(full) = capture.capture_primary() {
                            let full_bbox = last_full.as_ref().map(|lf| diff_bbox(lf, &full));
                            let (fw, fh) = (full.width as i32, full.height as i32);
                            let mismatch = if x >= 0
                                && y >= 0
                                && x + w as i32 <= fw
                                && y + h as i32 <= fh
                            {
                                region_mismatch(
                                    &full,
                                    x as usize,
                                    y as usize,
                                    w as usize,
                                    h as usize,
                                    &b,
                                )
                            } else {
                                u32::MAX
                            };
                            tracing::info!(
                                "[scroll] iter={iter} diag full_bbox={full_bbox:?} region_vs_full_mismatch={mismatch} full={}x{}",
                                full.width,
                                full.height
                            );
                            dump_png(&full, &format!("static_full_iter{iter}"));
                            dump_png(&b, &format!("static_region_iter{iter}"));
                            last_full = Some(full);
                        }
                        let mut revived: Option<CapturedFrame> = None;
                        if let Some(((npx, npy), frame)) =
                            try_relocate(&injector, capture, &a, region, warp)
                        {
                            warp = (npx, npy);
                            revived = Some(frame);
                            tracing::info!("[scroll] iter={iter} relocated warp -> ({npx},{npy})");
                        } else if let Ok(new_inj) = new_injector() {
                            // 重开 X 连接：旧连接可能已退化，或目标窗口开始忽略旧连接的合成事件。
                            // 注入改用「慢速 + 逐批验证」：Electron/Chromium 可能丢弃快速连续注入
                            // 的滚轮，慢速事件 + 焦点稳定后更可能被接受；每批后验证内容是否移动，
                            // 动了立即复活拼接，不必等整批打完才发现失败。
                            injector = new_inj;
                            injector.warp_to(warp.0 as i16, warp.1 as i16);
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
                                    // 诊断：全屏变化位置（定位滚动发生在屏幕哪里）
                                    if let Ok(full) = capture.capture_primary() {
                                        let bbox =
                                            last_full.as_ref().map(|lf| diff_bbox(lf, &full));
                                        tracing::info!(
                                            "[scroll] iter={iter} reconnect round={r} full_bbox={bbox:?}"
                                        );
                                        last_full = Some(full);
                                    }
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
                                b = frame;
                                if let Some(s) = stitch::find_scroll_delta(&a, &b) {
                                    if s >= MIN_SCROLL && blank_streak == 0 {
                                        let append_off =
                                            (b.height as usize - s) * frame_w as usize * 4;
                                        if append_off < b.pixels.len() {
                                            stitched.extend_from_slice(&b.pixels[append_off..]);
                                            stitched_h += s as u32;
                                            progress_h.store(stitched_h, Ordering::Relaxed);
                                        }
                                    }
                                }
                                a = b;
                                streak = 0;
                                blank_streak = 0;
                                ticks = TICKS_PER_ITER;
                                tracing::info!("[scroll] iter={iter} revived stitched_h={stitched_h}");
                            }
                            None => {
                                streak += 1;
                                tracing::info!("[scroll] iter={iter} no_delta energy={energy:.1} streak={streak}");
                                if streak >= MAX_STREAK {
                                    stop_reason = "no_delta";
                                    break;
                                }
                            }
                        }
                    } else {
                        streak += 1;
                        tracing::info!("[scroll] iter={iter} no_delta energy={energy:.1} streak={streak}");
                        if streak >= MAX_STREAK {
                            stop_reason = "no_delta";
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("[scroll] stop_reason={stop_reason} stitched_h={stitched_h}");

        Ok(CapturedFrame {
            width: frame_w,
            height: stitched_h,
            pixels: stitched,
        })
    })();

    progress.hide();
    result
}

/// 运行手动滚动截屏并返回拼接好的长图（调用方负责写剪贴板）。
///
/// 与自动模式的区别：不注入滚轮、不移动指针，由用户自己在目标窗口滚动。
/// 应用只负责轮询抓帧、重叠检测拼接。用户滚完点进度窗的「完成」结束。
pub fn run_manual_scroll_capture(
    region: &Bounds,
    screen_bounds: &Bounds,
    capture: &dyn ScreenCapture,
    progress: &dyn ScrollProgress,
) -> AppResult<CapturedFrame> {
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
    progress.show_manual(region, screen_bounds, cancel.clone(), done.clone(), progress_h.clone());

    // 内部闭包包住主循环：任何 `?` 提前退出，外层都统一 hide 进度窗。
    let result = (|| -> AppResult<CapturedFrame> {
        let a = capture.capture_area(x, y, w, h)?;
        let frame_w = a.width;
        let mut stitched = a.pixels.clone();
        let mut stitched_h = a.height;
        // anchor：最近一次成功拼接（或刷新）的基线；prev：上一帧（做帧间运动检测）
        let mut anchor = a.clone();
        let mut prev = a;
        let mut moving_frames = 0usize;
        let mut stop_reason = "max_iters";

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

            // 优先尝试重叠检测：帧足够锐利（未处于平滑滚动动画中）且重叠可靠 → 直接拼接。
            // 处于动画中间（亚像素偏移导致相邻行混合）的帧会被 verify_rows 拒绝返回 None。
            if let Some(s) = stitch::find_scroll_delta(&anchor, &b) {
                if s >= MIN_SCROLL {
                    let append_off = (b.height as usize - s) * frame_w as usize * 4;
                    if append_off < b.pixels.len() {
                        stitched.extend_from_slice(&b.pixels[append_off..]);
                        stitched_h += s as u32;
                        progress_h.store(stitched_h, Ordering::Relaxed);
                    }
                    tracing::info!(
                        "[scroll-manual] iter={iter} append delta={s} stitched_h={stitched_h}"
                    );
                }
                anchor = b.clone();
                prev = b;
                moving_frames = 0;
                continue;
            }

            // 检测失败：区分「向上滚」「还在滚动动画中」「静止在新位置」
            if stitch::find_scroll_delta(&b, &anchor).is_some() {
                // 反向检测命中 → 用户向上滚了：保持 anchor 在最深基线不动，
                // 之后滚回原位/继续向下时只追加超出当前拼接底部的真正新内容，
                // 避免把已拼接的行重复拼进去
                moving_frames = 0;
                tracing::info!("[scroll-manual] iter={iter} scrolled_up keep_baseline");
            } else if frames_differ(&prev, &b) {
                // 相邻帧仍在变化 → 动画/滚动进行中；不更新 anchor，等它停
                moving_frames += 1;
                tracing::info!("[scroll-manual] iter={iter} moving moving_frames={moving_frames}");
            } else if frames_differ(&anchor, &b) {
                // 已静止但 delta 测不出（均匀内容 / 单次滚动超过 MAX_SCROLL 无重叠）：
                // 放弃这一段，把基线跟到新位置，后续滚动从新基线继续算
                anchor = b.clone();
                moving_frames = 0;
                tracing::info!("[scroll-manual] iter={iter} settled_at_new_position undetectable");
            } else {
                // 与 anchor 基本一致（没滚 / 滚回原位）→ 无事发生
                moving_frames = 0;
            }
            prev = b;
        }

        tracing::info!("[scroll-manual] stop_reason={stop_reason} stitched_h={stitched_h}");

        Ok(CapturedFrame {
            width: frame_w,
            height: stitched_h,
            pixels: stitched,
        })
    })();

    progress.hide();
    result
}

/// 粗判两帧内容是否显著不同（区分「动画还在进行/滚动了」与「内容静止」）。
/// 均匀采样若干行、每行取 3 列，超过一半采样行有像素差异即认为不同。
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
    let stride = (h / 16).max(1);
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
    changed * 2 > total
}

/// 两帧差异像素的包围盒（每 4px 采样，每通道容差 24）。无差异返回 None。
fn diff_bbox(a: &CapturedFrame, b: &CapturedFrame) -> Option<(u32, u32, u32, u32)> {
    if a.width != b.width || a.height != b.height {
        return None;
    }
    let w = a.width as usize;
    let h = a.height as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let (mut min_x, mut min_y) = (usize::MAX, usize::MAX);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    let mut found = false;
    for y in (0..h).step_by(4) {
        for x in (0..w).step_by(4) {
            let p = (y * w + x) * 4;
            let mut diff = false;
            for ch in 0..3 {
                if a.pixels[p + ch].abs_diff(b.pixels[p + ch]) > 24 {
                    diff = true;
                    break;
                }
            }
            if diff {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                found = true;
            }
        }
    }
    if found {
        Some((min_x as u32, min_y as u32, max_x as u32, max_y as u32))
    } else {
        None
    }
}

/// 统计 full 在 (x,y,w,h) 处裁剪出的区域与 region 帧的差异采样点数（每 4px）。
fn region_mismatch(
    full: &CapturedFrame,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    region: &CapturedFrame,
) -> u32 {
    let fw = full.width as usize;
    let mut cnt = 0u32;
    for r in (0..h).step_by(4) {
        for c in (0..w).step_by(4) {
            let src = ((y + r) * fw + x + c) * 4;
            let dst = (r * w + c) * 4;
            for ch in 0..3 {
                if full.pixels[src + ch].abs_diff(region.pixels[dst + ch]) > 24 {
                    cnt += 1;
                    break;
                }
            }
        }
    }
    cnt
}

/// 诊断：把帧保存为 PNG（/tmp/scroll_<tag>.png）
fn dump_png(frame: &CapturedFrame, tag: &str) {
    let path = format!("/tmp/scroll_{tag}.png");
    match image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels.clone()) {
        Some(img) => match img.save(&path) {
            Ok(()) => tracing::info!("[scroll] saved {path}"),
            Err(e) => tracing::warn!("[scroll] save {path} failed: {e}"),
        },
        None => tracing::warn!(
            "[scroll] save {tag}: bad dims {}x{}",
            frame.width,
            frame.height
        ),
    }
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
    // 列：区域宽度的 12% / 30% / 50% / 70% / 88%
    let cols = [12, 30, 50, 70, 88];
    // 行：区域高度的 25% / 50% / 75%
    let rows = [25, 50, 75];
    // 优先级（列, 行）：左中 → 中上 → 右侧。编辑器文本通常在左侧
    let order = [
        (1, 1), (0, 1), (2, 1),
        (1, 0), (1, 2), (0, 0), (0, 2), (2, 0), (2, 2),
        (3, 1), (4, 1), (3, 0), (3, 2), (4, 0), (4, 2),
    ];
    for (ci, ri) in order {
        let px = x + wi * cols[ci] / 100;
        let py = y + hi * rows[ri] / 100;
        if (px, py) == current {
            continue;
        }
        injector.warp_to(px as i16, py as i16);
        injector.scroll_down(RELOCATE_TICKS);
        std::thread::sleep(RELOCATE_SETTLE);
        let Ok(frame) = capture.capture_area(x, y, w, h) else {
            continue;
        };
        if frame.width != baseline.width || frame.height != baseline.height {
            continue;
        }
        let delta = stitch::find_scroll_delta(baseline, &frame);
        let differ = frames_differ(baseline, &frame);
        tracing::info!(
            "[scroll] relocate candidate ({px},{py}) at col{}% row{}% delta={delta:?} differ={differ}",
            cols[ci], rows[ri],
        );
        if delta.is_some() || differ {
            return Some(((px, py), frame));
        }
    }
    // 全部候选无效：把指针恢复到调用前的落点。否则指针停在最后一个候选点，
    // 下轮 dist 检查会把引擎自己的移动误判为「用户移开鼠标」而错误暂停。
    injector.warp_to(current.0 as i16, current.1 as i16);
    None
}

#[cfg(target_os = "linux")]
fn new_injector() -> AppResult<xtest::XtestInjector> {
    xtest::XtestInjector::open()
}

#[cfg(not(target_os = "linux"))]
fn new_injector() -> AppResult<xtest::XtestInjector> {
    Err(AppError::Window(
        "滚动截屏仅支持 Linux/X11 会话".into(),
    ))
}
