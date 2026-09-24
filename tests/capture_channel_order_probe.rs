//! 探针：捕获出来的像素字节序到底是 RGBA 还是 BGRA。
//!
//! 造一块已知颜色的 X 窗口（纯蓝 0x3355CC），用应用的捕获入口读回来对比。
//! 遮罩里整屏 R/B 互换（用户报的"alt+s 出现遮罩层后屏幕颜色变了"）必须先把这一环
//! 钉死：若这里读出来就是对调的，说明换道在捕获端；若这里是正确的 RGBA，
//! 说明问题在显示端（喂给 gpui 的那一步）。
//!
//! 跑法（需要 DISPLAY；不需要启动应用，因此不占单实例锁）：
//!   DISPLAY=:1 cargo test --test capture_channel_order_probe -- --ignored --nocapture
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, WindowClass};

/// 探针窗口填充色（X 像素值按 0xRRGGBB 给）。取蓝：蓝/红在对调时最明显。
const FILL: u32 = 0x0033_55CC;

fn scan_for(t: &screenshot_rs::capture::CapturedFrame, r: u8, g: u8, b: u8) -> Option<(u32, u32)> {
    let mut hit = None;
    for yy in (0..t.height).step_by(2) {
        for xx in (0..t.width).step_by(2) {
            let j = ((yy * t.width + xx) * 4) as usize;
            if t.pixels[j] == r && t.pixels[j + 1] == g && t.pixels[j + 2] == b {
                hit = Some((xx, yy));
            }
        }
    }
    hit
}

#[test]
#[ignore]
fn probe_capture_channel_order() {
    let (conn, screen_num) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id().unwrap();
    // override_redirect：不让窗口管理器插手，位置才是我们要的（否则会被智能摆放挪走）
    let aux = CreateWindowAux::new()
        .background_pixel(FILL)
        .override_redirect(1);
    conn.create_window(0, win, screen.root, 800, 400, 200, 200, 0,
        WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(800));

    let cap = screenshot_rs::capture::platform_capture();
    let frame = cap.capture_primary().unwrap();

    let truth = scan_for(&frame, 0x33, 0x55, 0xCC);
    let swapped = scan_for(&frame, 0xCC, 0x55, 0x33);
    let (x, y) = truth.or(swapped).unwrap_or((900, 500));
    let i = ((y * frame.width + x) * 4) as usize;
    let (r, g, b, a) = (frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2], frame.pixels[i + 3]);
    println!("  [capture] 帧内真值蓝位置: {truth:?}；对调蓝位置: {swapped:?}");
    println!("  [capture] 真值 R=0x33 G=0x55 B=0xCC → 取样({x},{y}) = R=0x{r:02X} G=0x{g:02X} B=0x{b:02X} A=0x{a:02X}  {}",
        if r == 0x33 && b == 0xCC { "✅ 捕获是正确的 RGBA（问题在显示端）" }
        else if r == 0xCC && b == 0x33 { "❌ 捕获端就已被对调（问题在捕获的 BGRA→RGBA）" }
        else { "? 没找到探针窗口（可能没显示出来或被覆盖）" });
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
}

/// 把一块已知蓝色的窗口摆在屏幕 (800,400) 并**保持 25 秒**，供外部截图做对照。
///
/// 跑法（后台跑，然后用 xdotool/ffmpeg 在前台测遮罩里的颜色）：
///   DISPLAY=:1 cargo test --test capture_channel_order_probe -- --ignored --nocapture hold
#[test]
#[ignore]
fn probe_blue_window_hold() {
    let (conn, screen_num) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[screen_num];
    let win = conn.generate_id().unwrap();
    let aux = CreateWindowAux::new()
        .background_pixel(FILL)
        .override_redirect(1);
    conn.create_window(0, win, screen.root, 800, 400, 200, 200, 0,
        WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    println!("  [hold] 蓝块窗口已摆放 (800,400) 200x200，保持 25s");
    std::thread::sleep(Duration::from_secs(25));
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
    println!("  [hold] 已撤走");
}
