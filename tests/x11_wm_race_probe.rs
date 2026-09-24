//! X11/WM 竞态探针（`#[ignore]`，需要真机 X11 + mutter，默认不跑）。
//!
//! 用途：钉住「覆盖窗口为什么必须校验停靠」这一 WM 行为——
//! mutter 处理「创建即 map」时会先跑一段 show 流程；客户端若在这之前 unmap，
//! 它事后会把 show 跑完，窗口**留在 mapped**。而窗口一帧都没绘制过时内容未定义
//! ＝全黑全屏，用户看到的就是"程序启动成功后出现一个黑屏"，且该全屏窗还会吃掉
//! 整个工作区的点击。`src/overlay/window.rs` 的 `unmap_overlay_and_verify`
//! 就是为这个竞态写的补发兜底。
//!
//! 跑法：`DISPLAY=:1 cargo test --test x11_wm_race_probe -- --ignored --nocapture --test-threads=1`
//!
//! 全部用 1×1 隐形窗口复刻时序，不影响屏幕观感。实测结论（GNOME/mutter）：
//! - map → 立刻 unmap                    → VIEWABLE（竞态输，窗口留在 mapped）
//! - map → 立刻 unmap → 隔 50ms 补发一次 → UNMAPPED（补发有效）
//! - map → 隔 400ms 再 unmap             → UNMAPPED（mutter 的 show 早已跑完）
//! - 24 位视觉同样会输、加不加 `_NET_WM_STATE` REMOVE FULLSCREEN 都不影响结论
//!   （即：既不是 ARGB 视觉的问题，也不是我们那条全屏消息引入的）

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass,
    send_event,
};
use x11rb::rust_connection::RustConnection;

fn map_state(conn: &RustConnection, win: u32) -> String {
    let attrs = conn.get_window_attributes(win).unwrap().reply().unwrap();
    format!("{:?}", attrs.map_state)
}

/// 找一个 32 位（ARGB）visual：覆盖窗口就是用它建的。
fn find_argb_visual(conn: &RustConnection, screen_num: usize) -> Option<(u8, u32)> {
    use x11rb::protocol::xproto::VisualClass;
    let screen = &conn.setup().roots[screen_num];
    for depth in screen.allowed_depths.iter() {
        if depth.depth != 32 {
            continue;
        }
        for v in depth.visuals.iter() {
            if v.class == VisualClass::TRUE_COLOR {
                return Some((32, v.visual_id));
            }
        }
    }
    None
}

fn send_state(conn: &RustConnection, root: u32, win: u32, name: &[u8], add: bool) {
    let state_atom = conn
        .intern_atom(false, b"_NET_WM_STATE")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let target = conn.intern_atom(false, name).unwrap().reply().unwrap().atom;
    let action: u32 = if add { 1 } else { 0 };
    let event = ClientMessageEvent::new(32, win, state_atom, [action, target, 0, 1, 0]);
    send_event(
        conn,
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )
    .unwrap();
    conn.flush().unwrap();
}

/// 建一个 1×1 隐形窗口（ARGB visual + 客户端装饰＝MOTIF decorations=0 + 不设背景色，
/// 与覆盖窗口一致），返回窗口 id。
fn make_probe_window(conn: &RustConnection, use_argb: bool) -> u32 {
    let screen_num = 0usize;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let (depth, visual) = if use_argb {
        find_argb_visual(conn, screen_num).unwrap_or((screen.root_depth, screen.root_visual))
    } else {
        (screen.root_depth, screen.root_visual)
    };
    let win = conn.generate_id().unwrap();
    let aux = if depth == 32 {
        let colormap = conn.generate_id().unwrap();
        conn.create_colormap(
            x11rb::protocol::xproto::ColormapAlloc::NONE,
            colormap,
            root,
            visual,
        )
        .unwrap();
        CreateWindowAux::new()
            .colormap(colormap)
            .border_pixel(0)
            .event_mask(EventMask::STRUCTURE_NOTIFY)
    } else {
        CreateWindowAux::new()
            .border_pixel(0)
            .event_mask(EventMask::STRUCTURE_NOTIFY)
    };
    conn.create_window(
        depth,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &aux,
    )
    .unwrap();
    let motif = conn
        .intern_atom(false, b"_MOTIF_WM_HINTS")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let hints: [u32; 5] = [1 << 1, 0, 0, 0, 0];
    conn.change_property(
        PropMode::REPLACE,
        win,
        motif,
        motif,
        32,
        5,
        &hints
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    win
}

/// 场景 C/D：复刻「覆盖窗口启动时序」——创建即 map，紧接着 unmap。
/// `with_remove` 控制是否补发 REMOVE FULLSCREEN（＝我们 park 里那条消息）。
fn probe_startup_sequence(conn: &RustConnection, with_remove: bool) -> String {
    let root = conn.setup().roots[0].root;
    let win = make_probe_window(conn, true);
    conn.map_window(win).unwrap();
    conn.unmap_window(win).unwrap();
    if with_remove {
        send_state(conn, root, win, b"_NET_WM_STATE_FULLSCREEN", false);
    }
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    let state = map_state(conn, win);
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
    state
}

#[test]
#[ignore]
fn probe_c_startup_without_remove() {
    let (conn, _) = RustConnection::connect(None).expect("连不上 X");
    println!(
        "[C] 时序：map→unmap（不发 REMOVE）: {}",
        probe_startup_sequence(&conn, false)
    );
}

#[test]
#[ignore]
fn probe_d_startup_with_remove() {
    let (conn, _) = RustConnection::connect(None).expect("连不上 X");
    println!(
        "[D] 时序：map→unmap→REMOVE FULLSCREEN: {}",
        probe_startup_sequence(&conn, true)
    );
}

/// 通用时序探针：map →（延时）→ unmap →（可选）再 unmap → 回读状态。
fn probe_seq(
    conn: &RustConnection,
    map_to_unmap_ms: u64,
    second_unmap: bool,
    use_argb: bool,
) -> String {
    let win = make_probe_window(conn, use_argb);
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(map_to_unmap_ms));
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    if second_unmap {
        std::thread::sleep(Duration::from_millis(300));
        conn.unmap_window(win).unwrap();
        conn.flush().unwrap();
    }
    std::thread::sleep(Duration::from_millis(700));
    let state = map_state(conn, win);
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
    state
}

#[test]
#[ignore]
fn probe_e_delayed_unmap() {
    let (conn, _) = RustConnection::connect(None).expect("连不上 X");
    println!(
        "[E] ARGB 视觉、map 后 400ms 再 unmap: {}",
        probe_seq(&conn, 400, false, true)
    );
}

#[test]
#[ignore]
fn probe_f_double_unmap() {
    let (conn, _) = RustConnection::connect(None).expect("连不上 X");
    println!(
        "[F] ARGB 视觉、map→立刻 unmap→300ms 后再 unmap: {}",
        probe_seq(&conn, 5, true, true)
    );
}

#[test]
#[ignore]
fn probe_g_24bit_immediate() {
    let (conn, _) = RustConnection::connect(None).expect("连不上 X");
    println!(
        "[G] 24 位视觉、map→立刻 unmap: {}",
        probe_seq(&conn, 5, false, false)
    );
}

/// 测「立刻 unmap 失败后，隔多久补发才稳」（决定兜底重试的最小间隔）。
fn probe_retry_delay(conn: &RustConnection, retry_ms: u64) -> String {
    let win = make_probe_window(conn, true);
    conn.map_window(win).unwrap();
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(retry_ms));
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(800));
    let state = map_state(conn, win);
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
    state
}

#[test]
#[ignore]
fn probe_h_retry_50ms() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    println!("[H] 补发间隔 50ms: {}", probe_retry_delay(&conn, 50));
}

#[test]
#[ignore]
fn probe_i_retry_100ms() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    println!("[I] 补发间隔 100ms: {}", probe_retry_delay(&conn, 100));
}

#[test]
#[ignore]
fn probe_j_retry_200ms() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    println!("[J] 补发间隔 200ms: {}", probe_retry_delay(&conn, 200));
}

/// 顺手确认：`_NET_WM_STATE` REMOVE FULLSCREEN 不会把已 unmap 的窗口弄回 mapped
/// （曾怀疑是黑屏主因，已排除）。
#[test]
#[ignore]
fn probe_k_remove_fullscreen_keeps_unmapped() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    let root = conn.setup().roots[0].root;
    let win = make_probe_window(&conn, true);
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let after_map = map_state(&conn, win);
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let after_unmap = map_state(&conn, win);
    send_state(&conn, root, win, b"_NET_WM_STATE_FULLSCREEN", false);
    std::thread::sleep(Duration::from_millis(500));
    let after_remove = map_state(&conn, win);
    println!(
        "[K] map 后={after_map} / unmap 后={after_unmap} / 发 REMOVE FULLSCREEN 后={after_remove}"
    );
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
}

/// 读窗口在 root 坐标系里的绝对几何。
fn abs_geom(conn: &RustConnection, win: u32) -> String {
    let root = conn.setup().roots[0].root;
    let g = conn.get_geometry(win).unwrap().reply().unwrap();
    let t = conn.translate_coordinates(win, root, 0, 0).unwrap().reply().unwrap();
    format!("{}x{}+{}+{}", g.width, g.height, t.dst_x, t.dst_y)
}

/// 完全透明的整屏探针窗口：ARGB visual + background_pixel=0（X 服务器填充
/// 0x00000000＝全透明，合成后不可见）+ WM_HINTS.input=False（不抢输入焦点）。
fn make_invisible_overlay_probe(conn: &RustConnection) -> u32 {
    let screen_num = 0usize;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let (depth, visual) = find_argb_visual(conn, screen_num).unwrap();
    let win = conn.generate_id().unwrap();
    let colormap = conn.generate_id().unwrap();
    conn.create_colormap(x11rb::protocol::xproto::ColormapAlloc::NONE, colormap, root, visual)
        .unwrap();
    conn.create_window(
        depth,
        win,
        root,
        0,
        0,
        screen.width_in_pixels,
        screen.height_in_pixels,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .colormap(colormap)
            .background_pixel(0)
            .border_pixel(0)
            .event_mask(EventMask::STRUCTURE_NOTIFY),
    )
    .unwrap();
    // WM_HINTS: flags=InputHint, input=False → mutter 不给它焦点
    let wm_hints = conn.intern_atom(false, b"WM_HINTS").unwrap().reply().unwrap().atom;
    let hints: [u32; 9] = [1, 0, 0, 0, 0, 0, 0, 0, 0];
    conn.change_property(
        PropMode::REPLACE,
        win,
        wm_hints,
        wm_hints,
        32,
        9,
        &hints.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<u8>>(),
    )
    .unwrap();
    let motif = conn.intern_atom(false, b"_MOTIF_WM_HINTS").unwrap().reply().unwrap().atom;
    let mh: [u32; 5] = [1 << 1, 0, 0, 0, 0];
    conn.change_property(PropMode::REPLACE, win, motif, motif, 32, 5,
        &mh.iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<u8>>()).unwrap();
    conn.flush().unwrap();
    win
}

/// 采样几何变化时间线，直到稳定在「整屏」或超时。
fn sample_timeline(conn: &RustConnection, win: u32, budget_ms: u64, label: &str) {
    let start = std::time::Instant::now();
    let mut last = String::new();
    let mut first_screen_at: Option<u128> = None;
    while start.elapsed().as_millis() < budget_ms as u128 {
        let g = abs_geom(conn, win);
        if g != last {
            let ms = start.elapsed().as_millis();
            if last.is_empty() {
                println!("  [{label}] {ms}ms 初始几何: {g}");
            } else {
                println!("  [{label}] {ms}ms → {g}");
            }
            last = g.clone();
        }
        if first_screen_at.is_none() && g.starts_with("1920x1080+0+0") {
            first_screen_at = Some(start.elapsed().as_millis());
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    match first_screen_at {
        Some(ms) => println!("  [{label}] 到达整屏 1920x1080+0+0: {ms}ms"),
        None => println!("  [{label}] {budget_ms}ms 内始终没到整屏"),
    }
}

/// 变体 A：当前实现时序——map → 立刻 ADD FULLSCREEN 客户端消息。
#[test]
#[ignore]
fn probe_clamp_timeline_current_order() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    let root = conn.setup().roots[0].root;
    let win = make_invisible_overlay_probe(&conn);
    let t0 = std::time::Instant::now();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    println!("  [A] map 到 unmap 前几何: {} ({}ms)", abs_geom(&conn, win), t0.elapsed().as_millis());
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // 唤醒：map → 置顶 → ADD FULLSCREEN
    conn.map_window(win).unwrap();
    conn.configure_window(win, &x11rb::protocol::xproto::ConfigureWindowAux::new()
        .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE)).unwrap();
    send_state(&conn, root, win, b"_NET_WM_STATE_FULLSCREEN", true);
    sample_timeline(&conn, win, 700, "A 当前时序");
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
}

/// 变体 B：先写 _NET_WM_STATE 属性（含 FULLSCREEN）再 map，看 mutter 是否
/// 一上来就按整屏安置（＝没有工作区夹持），以及会不会提前把窗口显示出来。
#[test]
#[ignore]
fn probe_clamp_timeline_prestate() {
    let (conn, _) = RustConnection::connect(None).unwrap();
    let win = make_invisible_overlay_probe(&conn);
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    conn.unmap_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(250));

    // 停靠状态下先写属性
    let state_atom = conn.intern_atom(false, b"_NET_WM_STATE").unwrap().reply().unwrap().atom;
    let fs = conn.intern_atom(false, b"_NET_WM_STATE_FULLSCREEN").unwrap().reply().unwrap().atom;
    let skip_tb = conn.intern_atom(false, b"_NET_WM_STATE_SKIP_TASKBAR").unwrap().reply().unwrap().atom;
    let skip_pg = conn.intern_atom(false, b"_NET_WM_STATE_SKIP_PAGER").unwrap().reply().unwrap().atom;
    conn.change_property(PropMode::REPLACE, win, state_atom,
        x11rb::protocol::xproto::AtomEnum::ATOM, 32, 3,
        &[fs, skip_tb, skip_pg].iter().flat_map(|v| v.to_ne_bytes()).collect::<Vec<u8>>()).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    println!("  [B] 写属性后（应仍 UNMAPPED）: {} / geom={}", map_state(&conn, win), abs_geom(&conn, win));

    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    sample_timeline(&conn, win, 700, "B 预置属性");
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
}
