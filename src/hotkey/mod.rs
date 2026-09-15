//! 全局热键监听服务
//!
//! 使用 `global-hotkey` crate。截图触发键默认 `alt+s`，可在配置文件的
//! `[hotkey] screenshot` 里改（也支持环境变量 `SCREENSHOT_RS_HOTKEY`）。
//! 跨平台支持：Windows (RegisterHotKey) / Linux X11 (XGrabKey) / macOS (不需实现).
//!
//! 设计要点：
//! - `HotkeyService::new()` 在创建时同步注册 alt+s 全局快捷键；
//! - 启动一个独立线程持续轮询 `GlobalHotKeyEvent::receiver()`，
//!   将底层的 `GlobalHotKeyEvent` 转换为更上层的 `HotkeyEvent`，通过自有 mpsc 通道下发；
//! - 调用方（一般是 `app` 模块）通过 `try_recv()` 或 `recv()` 拿到事件，
//!   进而触发截图流程。

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::sync::mpsc::{Receiver, Sender};

use crate::error::{AppError, AppResult};

/// 应用层可识别的热键事件枚举
///
/// 与底层 `global_hotkey::GlobalHotKeyEvent` 解耦，方便业务侧
/// 根据事件触发相应的截图流程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// 用户按下 alt+s，请求触发截图
    TriggerScreenshot,
}

/// 解析用户配置的热键字符串，如 `"alt+s"` / `"ctrl+shift+f1"`。
///
/// 语法：`修饰键 + ... + 主键`，用 `+` 分隔，大小写与空格不敏感。
/// 修饰键：ctrl/control、alt/option、shift、super/cmd/command/win/meta（可组合）。
/// 主键：a-z、0-9、f1-f24，以及若干命名键（space/enter/esc/tab/printscreen/方向键等）。
///
/// 解析失败返回中文原因（带上原始串），调用方据此提示用户并回退默认键——
/// 全局热键注册失败会导致整个服务起不来，所以宁可回退也不能 panic。
pub fn parse_hotkey(spec: &str) -> Result<HotKey, String> {
    let raw = spec.trim();
    if raw.is_empty() {
        return Err("热键为空".to_string());
    }
    let parts: Vec<&str> = raw.split('+').map(|p| p.trim()).collect();
    let (last, rest) = parts
        .split_last()
        .ok_or_else(|| "热键为空".to_string())?;
    if last.is_empty() {
        return Err(format!("热键「{raw}」结尾多了个 +（缺少主键）"));
    }
    let mut modifiers = Modifiers::empty();
    for part in rest {
        if part.is_empty() {
            return Err(format!("热键「{raw}」中间有空的段（多了个 +？）"));
        }
        let tok = part.to_ascii_lowercase();
        modifiers |= match tok.as_str() {
            "ctrl" | "control" => Modifiers::CONTROL,
            "alt" | "option" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "super" | "cmd" | "command" | "win" | "meta" => Modifiers::SUPER,
            _ => {
                return Err(format!(
                    "热键「{raw}」里的修饰键「{part}」不认识（支持 ctrl/alt/shift/super）"
                ))
            }
        };
    }
    // 裸键（如 "s"）做全局热键会抢掉全系统该按键，几乎肯定是误配，直接拒绝
    if modifiers.is_empty() {
        return Err(format!(
            "热键「{raw}」缺少修饰键（全局热键至少要 ctrl/alt/shift/super 之一）"
        ));
    }
    let key = parse_key_code(&last.to_ascii_lowercase()).ok_or_else(|| {
        format!(
            "热键「{raw}」里的主键「{last}」不认识（支持 a-z、0-9、f1-f24，以及 space/enter/esc/tab/printscreen/方向键/标点等）"
        )
    })?;
    Ok(HotKey::new(Some(modifiers), key))
}

/// 解析主键名 → `Code`。只覆盖常用键，命中不了就让调用方提示用户。
fn parse_key_code(tok: &str) -> Option<Code> {
    // 单字母 / 单数字
    let mut chars = tok.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_lowercase() {
            return LETTERS.get(c as usize - 'a' as usize).copied();
        }
        if c.is_ascii_digit() {
            return DIGITS.get(c as usize - '0' as usize).copied();
        }
    }
    // f1-f24
    if let Some(n) = tok.strip_prefix('f').and_then(|d| d.parse::<usize>().ok()) {
        if (1..=24).contains(&n) {
            return FUNC_KEYS.get(n - 1).copied();
        }
    }
    Some(match tok {
        "space" => Code::Space,
        "enter" | "return" => Code::Enter,
        "esc" | "escape" => Code::Escape,
        "tab" => Code::Tab,
        "backspace" => Code::Backspace,
        "delete" | "del" => Code::Delete,
        "insert" => Code::Insert,
        "home" => Code::Home,
        "end" => Code::End,
        "pageup" => Code::PageUp,
        "pagedown" => Code::PageDown,
        "up" => Code::ArrowUp,
        "down" => Code::ArrowDown,
        "left" => Code::ArrowLeft,
        "right" => Code::ArrowRight,
        "printscreen" | "prtsc" => Code::PrintScreen,
        "minus" | "-" => Code::Minus,
        "equal" | "=" => Code::Equal,
        "comma" | "," => Code::Comma,
        "period" | "." => Code::Period,
        "slash" | "/" => Code::Slash,
        "semicolon" | ";" => Code::Semicolon,
        "quote" | "'" => Code::Quote,
        "backquote" | "`" => Code::Backquote,
        "bracketleft" | "[" => Code::BracketLeft,
        "bracketright" | "]" => Code::BracketRight,
        "backslash" | "\\" => Code::Backslash,
        _ => return None,
    })
}

/// a-z → KeyA..KeyZ（按 `Code` 枚举的物理键顺序）
static LETTERS: [Code; 26] = [
    Code::KeyA, Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyE, Code::KeyF, Code::KeyG,
    Code::KeyH, Code::KeyI, Code::KeyJ, Code::KeyK, Code::KeyL, Code::KeyM, Code::KeyN,
    Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR, Code::KeyS, Code::KeyT, Code::KeyU,
    Code::KeyV, Code::KeyW, Code::KeyX, Code::KeyY, Code::KeyZ,
];
/// 0-9 → Digit0..Digit9
static DIGITS: [Code; 10] = [
    Code::Digit0, Code::Digit1, Code::Digit2, Code::Digit3, Code::Digit4, Code::Digit5,
    Code::Digit6, Code::Digit7, Code::Digit8, Code::Digit9,
];
/// f1-f24 → F1..F24
static FUNC_KEYS: [Code; 24] = [
    Code::F1, Code::F2, Code::F3, Code::F4, Code::F5, Code::F6, Code::F7, Code::F8,
    Code::F9, Code::F10, Code::F11, Code::F12, Code::F13, Code::F14, Code::F15, Code::F16,
    Code::F17, Code::F18, Code::F19, Code::F20, Code::F21, Code::F22, Code::F23, Code::F24,
];

/// 全局热键服务
///
/// 负责：
/// 1. 创建 `GlobalHotKeyManager` 并注册 alt+s；
/// 2. 启动后台监听线程，把底层事件转换为 `HotkeyEvent` 后通过 mpsc 发出；
/// 3. 提供 `try_recv()` / `recv()` 让上层主动轮询或阻塞等待事件。
pub struct HotkeyService {
    /// 全局热键管理器（保留字段，未来可用于注销/重新注册）
    #[allow(dead_code)]
    manager: GlobalHotKeyManager,
    /// 监听线程 → 业务侧的事件通道发送端（保留字段，便于将来扩展）
    #[allow(dead_code)]
    event_tx: Sender<HotkeyEvent>,
    /// 业务侧接收热键事件的通道接收端
    event_rx: Receiver<HotkeyEvent>,
    /// 当前注册的截图热键 ID（保留字段，便于将来注销/重新注册）
    #[allow(dead_code)]
    screenshot_id: u32,
}

impl HotkeyService {
    /// 创建并启动全局热键服务
    ///
    /// 流程：
    /// 1. 创建 `GlobalHotKeyManager`；
    /// 2. 创建自有 mpsc 通道；
    /// 3. 注册 alt+s 作为截图触发键；
    /// 4. 启动后台监听线程，把底层 `GlobalHotKeyEvent` 转为 `HotkeyEvent`。
    pub fn new() -> AppResult<Self> {
        // 1. 创建底层全局热键管理器
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| AppError::Hotkey(format!("创建全局热键管理器失败：{e}")))?;
        tracing::info!("全局热键管理器创建成功");

        // 2. 创建自有事件通道（监听线程 → 业务侧）
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        // 3. 构造并注册截图热键（配置项 `[hotkey] screenshot`，默认 alt+s）。
        //    配置写错（键名不认识 / 少了修饰键）不致命：记一条 warn 后回退默认键，
        //    否则用户改错一个字符串整个应用就起不来了。
        let spec = crate::config::hotkey_screenshot();
        let hotkey = match parse_hotkey(&spec) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("热键配置无效（{e}），回退默认 alt+s");
                parse_hotkey(crate::config::DEFAULT_HOTKEY_SCREENSHOT)
                    .expect("默认热键 alt+s 必须可解析")
            }
        };
        let screenshot_id = hotkey.id();
        manager
            .register(hotkey)
            .map_err(|e| AppError::Hotkey(format!("注册全局热键 {spec} 失败：{e}")))?;
        tracing::info!("已注册全局热键：{spec}（hotkey id = {}）", screenshot_id);

        // 4. 启动监听线程
        //    把底层 global-hotkey 事件转成我们自己的 HotkeyEvent
        //    使用 try_recv + sleep 的轮询模式，避免长时间占用 CPU
        //    注意：克隆一份 Sender 给线程，原 Sender 保留在 HotkeyService 中
        //    以便将来扩展（例如动态注册/注销热键时使用）。
        let tx_for_thread = event_tx.clone();
        std::thread::spawn(move || {
            let event_tx = tx_for_thread;
            loop {
                if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                    tracing::debug!(
                        "热键事件：id={:?} state={:?}",
                        event.id,
                        event.state
                    );
                    // 只关心按键按下事件（松开不重复触发）
                    if event.state == HotKeyState::Pressed {
                        // 这里目前只有一个热键（alt+s），因此直接发出截图事件；
                        // 将来若有多个热键，可根据 event.id() 进行匹配分发。
                        let _ = event_tx.send(HotkeyEvent::TriggerScreenshot);
                    }
                }
                // 50ms 轮询间隔：兼顾响应速度与 CPU 占用
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        Ok(Self {
            manager,
            event_tx,
            event_rx,
            screenshot_id,
        })
    }

    /// 非阻塞检查是否有热键事件
    ///
    /// 若通道里有事件则返回 `Some(HotkeyEvent)`；否则返回 `None`。
    /// 适合在主循环里轮询使用。
    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.try_recv().ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_common_forms() {
        // 默认值必须能解析（HotkeyService 回退时 expect 依赖这一点）
        assert!(parse_hotkey(crate::config::DEFAULT_HOTKEY_SCREENSHOT).is_ok());
        assert!(parse_hotkey("alt+s").is_ok());
        assert!(parse_hotkey("ALT + S").is_ok()); // 大小写/空格不敏感
        assert!(parse_hotkey("ctrl+shift+f1").is_ok());
        assert!(parse_hotkey("super+printscreen").is_ok());
        assert!(parse_hotkey("cmd+space").is_ok());
        assert!(parse_hotkey("ctrl+alt+delete").is_ok());
        assert!(parse_hotkey("ctrl+1").is_ok());
        assert!(parse_hotkey("ctrl+-").is_ok());
    }

    #[test]
    fn modifier_and_key_mapping_is_exact() {
        let h = parse_hotkey("ctrl+alt+s").unwrap();
        assert_eq!(h.mods, Modifiers::CONTROL | Modifiers::ALT);
        assert_eq!(h.key, Code::KeyS);
        let d = parse_hotkey("shift+5").unwrap();
        assert_eq!(d.mods, Modifiers::SHIFT);
        assert_eq!(d.key, Code::Digit5);
        let f = parse_hotkey("super+f12").unwrap();
        assert_eq!(f.mods, Modifiers::SUPER);
        assert_eq!(f.key, Code::F12);
    }

    #[test]
    fn rejects_bad_specs_with_reason() {
        // 裸键：会抢掉全系统按键，必须拒绝
        assert!(parse_hotkey("s").unwrap_err().contains("缺少修饰键"));
        // 未知主键 / 未知修饰键 / 结构错误
        assert!(parse_hotkey("ctrl+nope").unwrap_err().contains("主键"));
        assert!(parse_hotkey("hyper+s").unwrap_err().contains("修饰键"));
        assert!(parse_hotkey("ctrl+").unwrap_err().contains("缺少主键"));
        assert!(parse_hotkey("ctrl++s").unwrap_err().contains("空"));
        assert!(parse_hotkey("").unwrap_err().contains("空"));
        assert!(parse_hotkey("f25").is_err()); // 超出 f1-f24
    }
}
