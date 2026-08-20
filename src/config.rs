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
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    /// 下载缓存目录（模型自动下载时存放于此）
    pub cache_dir: Option<PathBuf>,
    /// PaddleOCR 模型目录（显式指定后不自动下载，直接使用目录下的模型文件）
    pub model_dir: Option<PathBuf>,
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
    use super::*;

    fn parse_config(contents: &str) -> Option<Config> {
        toml::from_str(contents).ok()
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
