//! 运行时配置文件：OCR（tesseract）插件路径与下载路径。
//!
//! 配置文件位于跨平台配置目录下的 `screenshot-rs/config.toml`，可通过
//! `SCREENSHOT_RS_CONFIG` 环境变量覆盖位置。所有字段可选，缺失即回落默认。
//! 优先级统一为：**环境变量 > 配置文件 > 默认值**。

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;

/// tesseract 下载包默认 URL（占位符）：把 `tesseract-{platform}.zip` 上传到你的
/// GitHub Releases（分别命名为 tesseract-windows.zip / tesseract-linux.zip /
/// tesseract-macos.zip），然后替换为实际的 `https://github.com/<user>/<repo>/...`。
const DEFAULT_DOWNLOAD_URL: &str =
    "https://github.com/YOUR_USER/YOUR_REPO/releases/latest/download/tesseract-{platform}.zip";

/// 默认配置模板（带注释），首次运行时写入配置目录，用户可直接编辑。
const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.toml");

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub ocr: OcrConfig,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// tesseract 可执行文件绝对路径
    pub engine_path: Option<PathBuf>,
    /// tessdata 目录
    pub tessdata_dir: Option<PathBuf>,
    /// 下载 URL
    pub download_url: Option<String>,
    /// 下载缓存目录
    pub cache_dir: Option<PathBuf>,
}

static CONFIG: Lazy<Config> = Lazy::new(load_quiet);

/// 配置文件路径：优先 `SCREENSHOT_RS_CONFIG` 环境变量，否则跨平台配置目录。
fn config_path() -> Option<PathBuf> {
    std::env::var_os("SCREENSHOT_RS_CONFIG")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("screenshot-rs").join("config.toml")))
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

/// tesseract 可执行文件路径：env > 配置 > 自动定位（返回 None 走原有查找链）。
pub fn engine_path() -> Option<PathBuf> {
    env_path("TESSERACT_ENGINE_PATH")
        .or_else(|| config().ocr.engine_path.clone())
        .map(expand_home)
}

/// tessdata 目录：env > 配置 > None（使用 tesseract 自带目录）。
pub fn tessdata_dir() -> Option<PathBuf> {
    env_path("TESSERACT_TESSDATA_DIR")
        .or_else(|| config().ocr.tessdata_dir.clone())
        .map(expand_home)
}

/// 下载 URL：env > 配置 > 默认常量。空字符串视为未设置。
/// 支持 `{platform}` 占位符，按当前平台替换为 windows / macos / linux。
pub fn download_url() -> String {
    let url = resolve_download_url(
        std::env::var("TESSERACT_DOWNLOAD_URL").ok().as_deref(),
        config().ocr.download_url.as_deref(),
        DEFAULT_DOWNLOAD_URL,
    );
    replace_platform_placeholder(&url)
}

/// 替换 URL 中的 `{platform}` 占位符为当前平台名；无占位符则原样返回。
fn replace_platform_placeholder(url: &str) -> String {
    url.replace("{platform}", platform_name())
}

/// 当前平台名（用于下载 URL 占位符）。
fn platform_name() -> &'static str {
    #[cfg(target_os = "windows")]
    { "windows" }
    #[cfg(target_os = "macos")]
    { "macos" }
    #[cfg(target_os = "linux")]
    { "linux" }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    { "unknown" }
}

/// 下载缓存目录：env > 配置 > `LOCALAPPDATA|temp_dir` + `/screenshot-rs`。
pub fn cache_dir() -> PathBuf {
    env_path("TESSERACT_CACHE_DIR")
        .or_else(|| config().ocr.cache_dir.clone())
        .map(expand_home)
        .unwrap_or_else(default_cache_dir)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// 默认下载缓存目录：`%LOCALAPPDATA%/screenshot-rs`（其他平台回退系统临时目录）。
fn default_cache_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("screenshot-rs")
}

/// 合并下载 URL：env > config > default；空字符串视为未设置。
fn resolve_download_url(env: Option<&str>, config: Option<&str>, default: &str) -> String {
    env.filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| config.filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| default.to_string())
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
    use super::*;

    fn parse_config(contents: &str) -> Option<Config> {
        toml::from_str(contents).ok()
    }

    #[test]
    fn parses_valid_toml() {
        let c = parse_config(
            r#"
            [ocr]
            engine_path  = "/usr/bin/tesseract"
            tessdata_dir = "/usr/share/tesseract-ocr/4.00/tessdata"
            download_url = "https://example.com/tesseract.zip"
            cache_dir    = "/data/ocr-cache"
            "#,
        )
        .unwrap();
        assert_eq!(c.ocr.engine_path, Some("/usr/bin/tesseract".into()));
        assert_eq!(
            c.ocr.tessdata_dir,
            Some("/usr/share/tesseract-ocr/4.00/tessdata".into())
        );
        assert_eq!(c.ocr.download_url.as_deref(), Some("https://example.com/tesseract.zip"));
        assert_eq!(c.ocr.cache_dir, Some("/data/ocr-cache".into()));
    }

    #[test]
    fn invalid_or_empty_toml_falls_back() {
        assert!(parse_config("not [valid toml").is_none());
        assert!(parse_config("").unwrap_or_default().ocr.engine_path.is_none());
        // 无 [ocr] 表也能反序列化为默认
        let c = parse_config("other = 1").unwrap();
        assert!(c.ocr.engine_path.is_none());
        assert!(c.ocr.download_url.is_none());
    }

    #[test]
    fn download_url_priority() {
        assert_eq!(
            resolve_download_url(Some("env-url"), Some("cfg-url"), "default"),
            "env-url"
        );
        assert_eq!(
            resolve_download_url(None, Some("cfg-url"), "default"),
            "cfg-url"
        );
        assert_eq!(resolve_download_url(None, None, "default"), "default");
        // 空字符串视为未设置
        assert_eq!(
            resolve_download_url(Some(""), Some("cfg-url"), "default"),
            "cfg-url"
        );
        assert_eq!(
            resolve_download_url(Some("env"), Some(""), "default"),
            "env"
        );
    }

    #[test]
    fn platform_placeholder_replaced() {
        assert_eq!(
            replace_platform_placeholder("https://x/tesseract-{platform}.zip"),
            format!("https://x/tesseract-{}.zip", platform_name())
        );
        // 无占位符原样返回
        assert_eq!(
            replace_platform_placeholder("https://x/tesseract.zip"),
            "https://x/tesseract.zip"
        );
        assert_eq!(replace_platform_placeholder("no placeholder"), "no placeholder");
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
        assert!(content.contains("engine_path"));
        // 已存在则不覆盖
        assert!(!ensure_config_file_at(&path));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
