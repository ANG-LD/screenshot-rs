//! 运行时配置文件：OCR（PaddleOCR）模型路径与下载缓存。
//!
//! 配置文件位于跨平台配置目录下的 `screenshot-rs/config.toml`，可通过
//! `SCREENSHOT_RS_CONFIG` 环境变量覆盖位置。所有字段可选，缺失即回落默认。
//! 优先级统一为：**环境变量 > 配置文件 > 默认值**。

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;

/// 默认配置模板（带注释），首次运行时写入配置目录，用户可直接编辑。
const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.toml");

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub ocr: OcrConfig,
    pub hotkey: HotkeyConfig,
    pub translate: TranslateConfig,
}

/// 英译中配置。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct TranslateConfig {
    /// 模型下载基址（默认 HF 优先、hf-mirror 兜底）。用于内网自建镜像或离线分发。
    pub base_url: Option<String>,
}

/// 全局热键配置。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// 截图触发热键，形如 `alt+s` / `ctrl+shift+f1`（大小写与空格不敏感）。
    /// 修饰键：ctrl/alt/shift/super；主键：a-z、0-9、f1-f24 及若干命名键。
    /// 不填则用 [`DEFAULT_HOTKEY_SCREENSHOT`]。
    pub screenshot: Option<String>,
}

/// 截图热键默认值：`alt+s`（沿用历史行为）。
pub const DEFAULT_HOTKEY_SCREENSHOT: &str = "alt+s";

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// 下载缓存目录（模型自动下载时存放于此）
    pub cache_dir: Option<PathBuf>,
    /// PaddleOCR 模型目录（显式指定后不自动下载，直接使用目录下的模型文件）
    pub model_dir: Option<PathBuf>,
    /// 模型档位：small（默认，~30MB，CPU 快）或 medium（~132MB，准确率更高但慢）
    pub model_tier: Option<String>,
    /// 推理后端：cpu（默认）/ cuda / directml / openvino。
    /// 非 cpu 需构建时启用对应 feature（ocr-cuda 等）且系统装有对应运行库，
    /// 否则 ORT 会回落 CPU（日志会提示实际生效的 provider）。
    pub execution_provider: Option<String>,
}

static CONFIG: Lazy<Config> = Lazy::new(load_quiet);

/// 配置文件路径：优先 `SCREENSHOT_RS_CONFIG` 环境变量，否则跨平台配置目录。
fn config_path() -> Option<PathBuf> {
    std::env::var_os("SCREENSHOT_RS_CONFIG")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("screenshot-rs").join("config.toml")))
}

/// 当前生效的配置文件路径（设置窗口用来显示"改到哪去了"）。
pub fn config_file_display() -> String {
    config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（找不到配置文件路径）".to_string())
}

/// 静默加载：配置文件不存在 / 读取失败 / 解析失败均回落默认，不 panic。
fn load_quiet() -> Config {
    ensure_config_file();
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// 若配置文件不存在，自动在配置目录生成默认模板，用户可直接编辑。
fn ensure_config_file() {
    if let Some(path) = config_path() {
        ensure_config_file_at(&path);
    }
}

fn ensure_config_file_at(path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("创建配置目录失败 {}: {}", dir.display(), e);
            return false;
        }
    }
    match std::fs::write(path, DEFAULT_CONFIG_TEMPLATE) {
        Ok(_) => {
            tracing::info!("已生成默认配置文件 {}", path.display());
            true
        }
        Err(e) => {
            tracing::warn!("写入默认配置失败 {}: {}", path.display(), e);
            false
        }
    }
}

/// 触发配置加载（返回引用，惰性初始化）。
pub fn config() -> &'static Config {
    &CONFIG
}

/// 下载缓存目录：env > 配置 > `LOCALAPPDATA|系统缓存目录` + `/screenshot-rs`。
pub fn cache_dir() -> PathBuf {
    env_path("OCR_CACHE_DIR")
        .or_else(|| config().ocr.cache_dir.clone())
        .map(expand_home)
        .unwrap_or_else(default_cache_dir)
}

/// PaddleOCR 模型缓存目录：复用下载缓存目录下的 `paddle` 子目录。
pub fn ocr_cache_dir() -> PathBuf {
    cache_dir().join("paddle")
}

/// 用户显式指定的 PaddleOCR 模型目录：env `OCR_MODEL_DIR` > 配置 `ocr.model_dir`。
/// 指定后直接使用该目录下的模型文件（不再自动下载）；未指定则用缓存目录（自动下载）。
pub fn ocr_model_dir() -> Option<PathBuf> {
    env_path("OCR_MODEL_DIR")
        .or_else(|| config().ocr.model_dir.clone())
        .map(expand_home)
}

/// 截图触发热键：env `SCREENSHOT_RS_HOTKEY` > 配置 `hotkey.screenshot` > `alt+s`。
/// 解析由 `hotkey::parse_hotkey` 负责；这里只负责取值，不做校验（解析失败时
/// 热键服务会 warn 并回退默认键，应用照常启动）。
/// 运行期刚保存过的热键覆盖值：`(配置文件路径, 热键)`。
///
/// 为什么需要它：`config()` 是启动时**一次性**加载的 `static`，改完配置写回文件后
/// 内存里那份还是旧值——表现就是"设置里改成 alt+d，重新打开窗口又显示 alt+s"，
/// 用户会以为没保存成功（实际文件已写对，重启后也生效）。这里存一份覆盖值，
/// 让保存后的读取立刻拿到新值。
///
/// 按配置文件路径区分，是为了让测试里 `SCREENSHOT_RS_CONFIG` 指向不同文件时互不串味。
static HOTKEY_OVERRIDE: std::sync::Mutex<Option<(PathBuf, String)>> =
    std::sync::Mutex::new(None);

/// 环境变量 `SCREENSHOT_RS_HOTKEY` 是否正在覆盖配置里的热键。
///
/// 它优先级最高：设了它，设置窗口里怎么改都不会生效——所以要在界面上明确提醒，
/// 否则用户会一直以为是自己没保存成功。
pub fn hotkey_env_override() -> Option<String> {
    std::env::var("SCREENSHOT_RS_HOTKEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

pub fn hotkey_screenshot() -> String {
    // 刚保存的值优先，且只在同一个配置文件下生效
    if let Some(path) = config_path() {
        if let Ok(g) = HOTKEY_OVERRIDE.lock() {
            if let Some((p, spec)) = g.as_ref() {
                if p == &path {
                    return spec.clone();
                }
            }
        }
    }
    std::env::var("SCREENSHOT_RS_HOTKEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| config().hotkey.screenshot.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HOTKEY_SCREENSHOT.to_string())
}

/// 翻译模型缓存目录：复用下载缓存目录下的 `translate` 子目录。
pub fn translate_cache_dir() -> PathBuf {
    cache_dir().join("translate")
}

/// 翻译模型下载基址：env `SCREENSHOT_RS_TRANSLATE_BASE_URL` > 配置
/// `translate.base_url`。未配置返回 None，由 `translate` 模块用「HF + 镜像」兜底。
pub fn translate_base_url() -> Option<String> {
    std::env::var("SCREENSHOT_RS_TRANSLATE_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| config().translate.base_url.clone())
        .filter(|s| !s.trim().is_empty())
}

/// 推理后端：env `OCR_EXECUTION_PROVIDER` > 配置 `ocr.execution_provider` > "auto"。
/// "auto"（默认）= 运行时检测：有 CUDA GPU 用 GPU，否则 CPU。
/// 也可显式指定：cpu / cuda / directml / openvino。
pub fn ocr_execution_provider() -> String {
    std::env::var("OCR_EXECUTION_PROVIDER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config().ocr.execution_provider.clone())
        .unwrap_or_else(|| "auto".to_string())
        .to_ascii_lowercase()
}

/// 模型档位：env `OCR_MODEL_TIER` > 配置 `ocr.model_tier` > 默认 "small"。
/// small ≈30MB（CPU 上快 ~18 倍，代码区实测 0.77s vs medium 13.9s）；
/// medium ≈132MB（准确率更高，但 CPU 上多行场景过慢）。
pub fn ocr_model_tier() -> String {
    std::env::var("OCR_MODEL_TIER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config().ocr.model_tier.clone())
        .unwrap_or_else(|| "small".to_string())
        .to_ascii_lowercase()
}

/// 把 `ocr.model_tier` 持久化写入配置文件（文本级修改，保留注释与排版），
/// 供模型管理窗口切换档位时调用——重启后档位保持一致。
pub fn persist_model_tier(tier: &str) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("找不到配置文件路径".into());
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取配置文件失败: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // 删除已有的 model_tier 行（含模板里的注释行），再重新插入
    lines.retain(|l| !l.trim_start().starts_with("model_tier"));

    // 找插入点：`[ocr]` 段内最后一条非注释配置行的下一行；
    // 若无（段内只有注释/空行），插到 `[ocr]` 行之后。
    let mut in_ocr = false;
    let mut last_key_line: Option<usize> = None;
    let mut ocr_header: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with('[') && t.ends_with(']') {
            in_ocr = t == "[ocr]";
            if in_ocr && ocr_header.is_none() {
                ocr_header = Some(i);
            }
        } else if in_ocr && !t.is_empty() && !t.starts_with('#') {
            last_key_line = Some(i);
        }
    }
    let insert_at = last_key_line
        .map(|i| i + 1)
        .or(ocr_header.map(|i| i + 1))
        .unwrap_or(lines.len());
    lines.insert(insert_at, format!("model_tier = \"{tier}\""));

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("写入配置文件失败: {e}"))?;
    tracing::info!("OCR: 已持久化模型档位 {tier} → {}", path.display());
    Ok(())
}

/// 把 `hotkey.screenshot` 持久化写入配置文件（文本级修改，保留注释与排版），
/// 供「系统设置」窗口改热键时调用。
///
/// 写完**不会自动生效**：还要让运行中的热键服务重新注册（见
/// `hotkey::request_rebind`），否则用户改了要等重启才生效。
pub fn persist_hotkey_screenshot(spec: &str) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("找不到配置文件路径".into());
    };
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("读取配置文件失败: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // 删掉已有的 screenshot 行（模板里那行注释保留，作为写法说明）
    lines.retain(|l| !l.trim_start().starts_with("screenshot"));

    // 插入点：[hotkey] 段内最后一条非空行的下一行——注释也算内容，
    // 新行会落在注释之后，读起来仍是"说明在前、取值在后"。
    let mut in_hotkey = false;
    let mut last_line: Option<usize> = None;
    let mut header: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with('[') && t.ends_with(']') {
            in_hotkey = t == "[hotkey]";
            if in_hotkey && header.is_none() {
                header = Some(i);
            }
        } else if in_hotkey && !t.is_empty() {
            last_line = Some(i);
        }
    }

    match (last_line, header) {
        (Some(i), _) => lines.insert(i + 1, format!("screenshot = \"{spec}\"")),
        (None, Some(i)) => lines.insert(i + 1, format!("screenshot = \"{spec}\"")),
        // 老配置文件没有 [hotkey] 段：必须连段头一起补，否则这行会落到上一个段里，
        // 读取时被当成别的段的键，用户改了等于没改。
        (None, None) => {
            lines.push(String::new());
            lines.push("[hotkey]".to_string());
            lines.push(format!("screenshot = \"{spec}\""));
        }
    }

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("写入配置文件失败: {e}"))?;
    // 写盘成功才更新内存覆盖值：让设置窗口立刻显示新键，不用重启
    if let Ok(mut g) = HOTKEY_OVERRIDE.lock() {
        *g = Some((path.clone(), spec.to_string()));
    }
    tracing::info!("已持久化截图热键 {spec} → {}", path.display());
    Ok(())
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// 默认下载缓存目录：优先 `%LOCALAPPDATA%/screenshot-rs`；其他平台用系统缓存目录
/// （Linux/macOS 的 `~/.cache/screenshot-rs`，避免模型落在临时目录重启即丢）。
fn default_cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("screenshot-rs")
}

/// 展开路径开头的 `~`（或 `~\`）为用户主目录。
fn expand_home(path: PathBuf) -> PathBuf {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return path,
    };
    let s = path.to_string_lossy();
    if s == "~" {
        home
    } else if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        home.join(rest)
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    /// 环境变量是进程级全局，改它的测试必须串行：并行跑时一个测试会把临时目录删掉，
    /// 另一个正写到一半就报 "No such file or directory"，失败原因还跟被测逻辑无关。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    use super::*;

    fn parse_config(contents: &str) -> Option<Config> {
        toml::from_str(contents).ok()
    }

    /// `[hotkey]` 段：值缺失/为空白时回落默认 alt+s（配置模板里默认是注释掉的）。
    /// 回归：`config()` 是启动时一次性加载的 static，改完写回文件不会更新内存那份，
    /// 表现就是"设置里改成 alt+d，重新打开窗口还显示 alt+s"（用户以为没保存成功）。
    #[test]
    fn persisted_hotkey_is_readable_immediately() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("srs-hotkey-now-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[hotkey]\n").unwrap();
        std::env::set_var("SCREENSHOT_RS_CONFIG", &path);
        std::env::remove_var("SCREENSHOT_RS_HOTKEY");

        persist_hotkey_screenshot("alt+d").unwrap();
        assert_eq!(hotkey_screenshot(), "alt+d", "保存后必须立刻读回新值");

        std::env::remove_var("SCREENSHOT_RS_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 热键持久化：写进已有段、重复写是替换不是追加、老配置缺段时补段头。
    ///
    /// 第三点最容易出错：直接往文件末尾追加 `screenshot = ...` 会落进**上一个段**，
    /// 读取时被当成那个段的键——用户改了等于没改。
    #[test]
    fn persist_hotkey_writes_into_section_without_duplicating() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("srs-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 1) 段已存在（含模板注释行）
        let p1 = dir.join("with_section.toml");
        std::fs::write(
            &p1,
            "[hotkey]\n# screenshot = \"alt+s\"\n\n[ocr]\nmodel_tier = \"small\"\n",
        )
        .unwrap();
        std::env::set_var("SCREENSHOT_RS_CONFIG", &p1);
        persist_hotkey_screenshot("ctrl+shift+a").unwrap();
        let t1 = std::fs::read_to_string(&p1).unwrap();
        assert!(t1.contains("screenshot = \"ctrl+shift+a\""), "{t1}");
        assert!(
            t1.contains("[ocr]") && t1.contains("model_tier"),
            "不能破坏其他段: {t1}"
        );

        // 2) 再写一次：替换而不是追加
        persist_hotkey_screenshot("alt+q").unwrap();
        let t2 = std::fs::read_to_string(&p1).unwrap();
        // 只数真正的键行：模板里那行注释 `# screenshot = "alt+s"` 也含同样的子串
        let keys = t2
            .lines()
            .filter(|l| l.trim_start().starts_with("screenshot ="))
            .count();
        assert_eq!(keys, 1, "键被重复写入: {t2}");
        assert!(t2.contains("alt+q"), "{t2}");
        assert!(!t2.contains("ctrl+shift+a"), "{t2}");

        // 3) 老配置没有 [hotkey] 段 → 必须连段头一起补
        let p2 = dir.join("no_section.toml");
        std::fs::write(&p2, "[ocr]\nmodel_tier = \"small\"\n").unwrap();
        std::env::set_var("SCREENSHOT_RS_CONFIG", &p2);
        persist_hotkey_screenshot("super+f1").unwrap();
        let t3 = std::fs::read_to_string(&p2).unwrap();
        assert!(
            t3.contains("[hotkey]") && t3.contains("super+f1"),
            "缺段时要补段头: {t3}"
        );

        std::env::remove_var("SCREENSHOT_RS_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkey_section_defaults_and_reads_value() {
        let none = parse_config("").unwrap();
        assert_eq!(none.hotkey.screenshot, None);
        let blank = parse_config("[hotkey]\nscreenshot = \"  \"\n").unwrap();
        assert!(blank
            .hotkey
            .screenshot
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true));
        let set = parse_config("[hotkey]\nscreenshot = \"ctrl+shift+a\"\n").unwrap();
        assert_eq!(set.hotkey.screenshot.as_deref(), Some("ctrl+shift+a"));
    }

    #[test]
    fn parses_valid_toml() {
        let c = parse_config(
            r#"
            [ocr]
            cache_dir = "/data/ocr-cache"
            model_dir = "/data/paddle-models"
            "#,
        )
        .unwrap();
        assert_eq!(c.ocr.cache_dir, Some("/data/ocr-cache".into()));
        assert_eq!(c.ocr.model_dir, Some("/data/paddle-models".into()));
    }

    #[test]
    fn invalid_or_empty_toml_falls_back() {
        assert!(parse_config("not [valid toml").is_none());
        assert!(parse_config("").unwrap_or_default().ocr.cache_dir.is_none());
        // 无 [ocr] 表也能反序列化为默认
        let c = parse_config("other = 1").unwrap();
        assert!(c.ocr.cache_dir.is_none());
        assert!(c.ocr.model_dir.is_none());
    }

    #[test]
    fn expands_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/x/y".into()), home.join("x/y"));
        assert_eq!(expand_home("~".into()), home);
        assert_eq!(expand_home("/abs/path".into()), PathBuf::from("/abs/path"));
    }

    #[test]
    fn ensure_generates_default_template_once() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("screenshot-rs-cfg-test-{nanos}"));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(ensure_config_file_at(&path));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[ocr]"));
        assert!(content.contains("model_dir"));
        // 已存在则不覆盖
        assert!(!ensure_config_file_at(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
