//! PaddleOCR（PP-OCRv6）本地识别：替换 tesseract CLI。
//!
//! 模型文件（检测 / 识别 / 词典）按需从 GitHub Releases 下载到缓存目录，
//! 首次使用 OCR 时触发下载（与原先 tesseract 的分发模式一致）。推理由
//! `oar-ocr` 驱动（ONNX Runtime，构建时由 `download-binaries` feature
//! 下载 CPU 版运行时）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::processors::LimitType;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};

/// 识别引擎的全局单例：懒初始化，线程安全（一次只允许一个识别任务）。
/// 引擎通过 `Box::leak` 提升为 `'static`（程序生命周期内常驻，不释放）。
static ENGINE: OnceLock<Mutex<Option<&'static OAROCR>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// 模型管理：状态快照 + 进度感知下载（供托盘「OCR 模型管理」窗口展示/重下）
// ---------------------------------------------------------------------------

/// 单个模型文件的本地状态
#[derive(Debug, Clone, PartialEq)]
pub enum FileStatus {
    /// 已找到本地文件（显式目录 / 项目 models / 缓存）
    Ready,
    /// 本地不存在（可下载）
    Missing,
    /// 正在下载
    Downloading,
    /// 下载失败
    Error(String),
}

/// 单个模型文件的展示信息
#[derive(Debug, Clone)]
pub struct ModelFileInfo {
    pub name: &'static str,
    pub url: String,
    /// 找到的本地路径（仅 Ready 时有值）
    pub local_path: Option<PathBuf>,
    /// 本地文件大小（仅 Ready 时有值）
    pub size: Option<u64>,
    pub status: FileStatus,
}

/// 一个档位的状态（small / medium 各一份，供窗口并列展示）
#[derive(Debug, Clone)]
pub struct TierStatus {
    pub tier: String,
    pub note: String,
    /// 是否为当前生效档位（内存切换 > config）
    pub selected: bool,
    /// 三件套文件状态（检测 / 识别 / 词典）
    pub files: Vec<ModelFileInfo>,
}

/// 模型下载/管理的整体状态快照（窗口每次刷新重新计算）
#[derive(Debug, Clone)]
pub struct ModelSnapshot {
    /// 模型下载基址
    pub base_url: String,
    /// 缓存目录
    pub cache_dir: PathBuf,
    /// 两个档位的完整状态
    pub tiers: Vec<TierStatus>,
    /// 是否有下载任务进行中
    pub downloading: bool,
    /// 正在下载的档位（None=无；供 UI 只把对应档位按钮置为「下载中」）
    pub downloading_tier: Option<String>,
    /// true=整档下载中；false=单文件下载中（供 UI 区分按钮「下载中」归属）
    pub batch_download: bool,
    /// 正在下载的文件名（供 UI 把对应文件行状态置为「下载中」）
    pub current_file: Option<String>,
    /// 当前文件下载进度（已下载字节, 总字节）
    pub progress: (u64, Option<u64>),
    /// 最近一次下载结果（None=尚无下载动作）
    pub last_download: Option<Result<(), String>>,
}

/// 全局模型管理状态（下载线程与 UI 线程共享）
struct ModelManagerState {
    downloading: bool,
    /// true=整档下载（批量/重新下载）；false=单文件下载
    batch: bool,
    /// 正在下载的档位（None=无下载任务；供 UI 区分各档位按钮状态）
    current_tier: Option<String>,
    /// 当前文件进度
    progress: (u64, Option<u64>),
    /// 当前下载的文件名
    current_file: String,
    /// 最近一次下载结果
    last_download: Option<Result<(), String>>,
}

static MANAGER: OnceLock<Mutex<ModelManagerState>> = OnceLock::new();

fn manager() -> &'static Mutex<ModelManagerState> {
    MANAGER.get_or_init(|| {
        Mutex::new(ModelManagerState {
            downloading: false,
            batch: false,
            current_tier: None,
            progress: (0, None),
            current_file: String::new(),
            last_download: None,
        })
    })
}

/// 内存档位覆盖（模型管理窗口手动切换时写入，优先级高于 config）。
static TIER_OVERRIDE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 模型下载互斥锁：防止并发下载（set_tier 后台确保 + 首次 OCR 自动下载 +
/// 手动重新下载）竞争同一 `.part` 临时文件导致 rename 失败。
static DOWNLOAD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn download_lock() -> &'static Mutex<()> {
    DOWNLOAD_LOCK.get_or_init(|| Mutex::new(()))
}

/// 检查指定档位三件套是否本地齐全（显式目录 / 项目 models/PP-OCRv6 / 缓存）。
/// 返回 `Ok(())` 齐全；`Err` 携带缺失文件名列表。
pub fn tier_ready(tier: &str) -> Result<(), Vec<String>> {
    let cache_dir = crate::config::ocr_cache_dir();
    let mut missing = Vec::new();
    for name in model_names_for_tier(tier) {
        let mut exists = false;
        for dir in crate::config::ocr_model_dir()
            .into_iter()
            .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
            .chain(std::iter::once(cache_dir.clone()))
        {
            if dir.join(name).exists() {
                exists = true;
                break;
            }
        }
        if !exists {
            missing.push(name.to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// 手动切换档位：先校验模型齐全（缺失不允许激活），再写入内存覆盖并复位引擎。
/// 返回 Err 时未做任何切换。
pub fn set_tier(tier: &str) -> Result<(), String> {
    let t = tier.to_ascii_lowercase();
    let t = if t == "medium" { "medium".to_string() } else { "small".to_string() };
    // 激活前置校验：模型文件必须本地齐全，否则拒绝切换并提示缺失文件
    if let Err(missing) = tier_ready(&t) {
        tracing::warn!("OCR: 拒绝激活 {t}：缺失模型文件 {missing:?}");
        return Err(format!(
            "模型文件不齐全（缺失 {}），请先点击「重新下载」",
            missing.join("、")
        ));
    }
    let cell = TIER_OVERRIDE.get_or_init(|| Mutex::new(None));
    if let Ok(mut g) = cell.lock() {
        *g = Some(t.clone());
    }
    reset_engine();
    tracing::info!("OCR: 手动切换模型档位 → {t}");
    // 持久化到 config.toml：重启后档位保持一致
    if let Err(e) = crate::config::persist_model_tier(&t) {
        tracing::warn!("OCR: 写入 config.toml 档位失败: {e}（本次运行仍生效）");
    }
    Ok(())
}

/// 当前生效档位：内存覆盖 > config（env OCR_MODEL_TIER > ocr.model_tier > small）。
pub fn effective_tier() -> String {
    if let Some(cell) = TIER_OVERRIDE.get() {
        if let Ok(g) = cell.lock() {
            if let Some(t) = g.as_ref() {
                return t.clone();
            }
        }
    }
    crate::config::ocr_model_tier()
}

/// 模型档位 → 模型文件名（oar-ocr v0.7.0 release 资产）。
/// small ≈30MB（det 9.4MB + rec 21MB + dict 75KB，CPU 快）；
/// medium ≈132MB（det 62MB + rec 76MB，准确率更高但 CPU 慢）。
/// 已知模型文件大小（v0.7.0 release 实测字节），用于整批下载进度预估。
/// 实际下载时仍以服务端 Content-Length 为准（缺失则用此预估值）。
fn known_file_size(name: &str) -> Option<u64> {
    match name {
        "pp-ocrv6_small_det.onnx" => Some(9_880_512),
        "pp-ocrv6_small_rec.onnx" => Some(21_159_378),
        "pp-ocrv6_medium_det.onnx" => Some(62_119_454),
        "pp-ocrv6_medium_rec.onnx" => Some(76_629_984),
        "ppocrv6_dict.txt" => Some(74_947),
        _ => None,
    }
}

fn model_names_for_tier(tier: &str) -> [&'static str; 3] {
    match tier {
        "medium" => [
            "pp-ocrv6_medium_det.onnx",
            "pp-ocrv6_medium_rec.onnx",
            "ppocrv6_dict.txt",
        ],
        _ => [
            "pp-ocrv6_small_det.onnx",
            "pp-ocrv6_small_rec.onnx",
            "ppocrv6_dict.txt",
        ],
    }
}

/// 档位说明（供窗口展示）
fn tier_note(tier: &str) -> String {
    match tier {
        "medium" => "≈132MB：准确率更高，但 CPU 上多行场景慢（~14s）".into(),
        _ => "≈30MB：CPU 快（代码区 ~0.8s），准确率 ~95%".into(),
    }
}

/// 当前生效档位（供引擎加载使用）
fn model_names() -> [&'static str; 3] {
    model_names_for_tier(&effective_tier())
}

/// 模型下载基址。注意：必须固定 v0.7.0 —— `latest` 指向 v0.9.2，但该
/// release 不带模型资产（404）；v0.7.0 起所有模型资产齐全（已逐一验证 HTTP 200）。
const MODEL_BASE_URL: &str = "https://github.com/GreatV/oar-ocr/releases/download/v0.7.0";

/// 给定档位三个文件的本地查找结果
fn locate_tier_files(tier: &str, cache_dir: &Path) -> Vec<ModelFileInfo> {
    let names = model_names_for_tier(tier);
    names
        .iter()
        .map(|name| {
            // 查找顺序：显式目录 → 项目 models/PP-OCRv6 → 缓存
            let mut local = None;
            for dir in crate::config::ocr_model_dir()
                .into_iter()
                .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
                .chain(std::iter::once(cache_dir.to_path_buf()))
            {
                let p = dir.join(name);
                if p.exists() {
                    local = Some(p);
                    break;
                }
            }
            let (status, size) = match &local {
                Some(p) => match p.metadata() {
                    Ok(md) => (FileStatus::Ready, Some(md.len())),
                    Err(_) => (FileStatus::Error("无法读取文件信息".into()), None),
                },
                None => (FileStatus::Missing, None),
            };
            ModelFileInfo {
                name,
                url: format!("{MODEL_BASE_URL}/{name}"),
                local_path: local,
                size,
                status,
            }
        })
        .collect()
}

/// 收集模型管理快照：small / medium 两档并列，报告每个文件的状态。
pub fn model_snapshot() -> ModelSnapshot {
    let current = effective_tier();
    let cache_dir = crate::config::ocr_cache_dir();
    let state = manager().lock().unwrap_or_else(|e| e.into_inner());
    ModelSnapshot {
        base_url: MODEL_BASE_URL.to_string(),
        cache_dir: cache_dir.clone(),
        tiers: ["small", "medium"]
            .iter()
            .map(|tier| TierStatus {
                tier: (*tier).to_string(),
                note: tier_note(tier),
                selected: *tier == current,
                files: locate_tier_files(tier, &cache_dir),
            })
            .collect(),
        downloading: state.downloading,
        downloading_tier: state.current_tier.clone(),
        batch_download: state.batch,
        current_file: if state.downloading { Some(state.current_file.clone()) } else { None },
        progress: state.progress,
        last_download: state.last_download.clone(),
    }
}

/// 更新下载进度（下载循环每块调用；UI 线程读取）
fn update_progress(name: &str, downloaded: u64, total: Option<u64>) {
    if let Ok(mut g) = manager().lock() {
        g.current_file = name.to_string();
        g.progress = (downloaded, total);
    }
}

/// 模型文件是否在任意查找目录存在（OCR_MODEL_DIR / 项目 models/PP-OCRv6 / 缓存）。
fn file_located(name: &str, cache_dir: &Path) -> bool {
    crate::config::ocr_model_dir()
        .into_iter()
        .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
        .chain(std::iter::once(cache_dir.to_path_buf()))
        .any(|d| d.join(name).exists())
}

/// HTTP 客户端：30s 读空闲超时——下载中断时（取消/网络卡住）能及时返回，
/// 避免线程永久阻塞在 `read` 上（取消后 engine 最多等 30s 即可接管下载）。
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_read(std::time::Duration::from_secs(30))
            .build()
    })
}

/// 后台下载任务（手动「重新下载」/ 切换档位后台确保）。可被 OCR 引擎
/// 的下载需求取消（engine 优先，见 `cancel_background_download`）。
struct DownloadTask {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// 当前后台下载任务（None=无后台下载）。engine 需要下载时置 cancel 让位。
static DOWNLOAD_TASK: OnceLock<Mutex<Option<DownloadTask>>> = OnceLock::new();

fn download_task_cell() -> &'static Mutex<Option<DownloadTask>> {
    DOWNLOAD_TASK.get_or_init(|| Mutex::new(None))
}

/// 取消进行中的后台下载：置取消标志并清空任务注册。
/// 后台线程在下一块 `read` 返回（30s 空闲超时内）检查标志、删除 .part、
/// 释放下载锁；engine 随后持锁接管下载。不 join（阻塞等待由下载锁完成）。
fn cancel_background_download() {
    if let Ok(mut g) = download_task_cell().lock() {
        if let Some(task) = g.take() {
            task.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::info!("OCR: 取消后台模型下载（engine 接管）");
        }
    }
}

/// 下载单个模型文件到缓存目录（带进度上报与取消检查）。
/// `cancel` 为 None 时不可取消（engine 自身下载）。
fn download_one(
    name: &str,
    cache_dir: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    batch_offset: u64,
    batch_total: Option<u64>,
) -> Result<(), String> {
    let dest = cache_dir.join(name);
    let url = format!("{MODEL_BASE_URL}/{name}");
    tracing::info!("OCR: 下载模型 {name} ← {url}");
    let resp = http_agent()
        .get(&url)
        .call()
        .map_err(|e| format!("下载模型 {name} 失败: {e}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| known_file_size(name));
    let mut reader = resp.into_reader();
    let tmp = cache_dir.join(format!("{name}.part"));
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读取模型 {name} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        if cancel.map(|c| c.load(std::sync::atomic::Ordering::SeqCst)).unwrap_or(false) {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            update_progress(name, batch_offset + downloaded, batch_total);
            return Err(format!("下载 {name} 已取消（engine 接管）"));
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
        downloaded += n as u64;
        update_progress(name, downloaded, total);
    }
    std::fs::rename(&tmp, &dest)
        .map_err(|e| format!("移动模型 {name} 失败: {e}"))?;
    tracing::info!("OCR: 模型 {name} 下载完成（{} 字节）", downloaded);
    update_progress(name, batch_offset + downloaded, batch_total);
    Ok(())
}

/// 后台强制重新下载指定档位模型到缓存目录（已存在也覆盖，供「重新下载」）。
/// 进度写入全局状态，UI 轮询 `model_snapshot()` 展示；可被 engine 下载取消。
pub fn start_download(tier: &str) -> Result<(), String> {
    let mut g = manager().lock().map_err(|_| "模型管理锁失效".to_string())?;
    if g.downloading {
        return Err("已有下载任务进行中".into());
    }
    // 全部存在 → 重新下载（覆盖全部）；有缺失 → 批量下载（只补缺失）
    let cache_dir = crate::config::ocr_cache_dir();
    let all_exist = model_names_for_tier(tier)
        .iter()
        .all(|n| cache_dir.join(n).exists());
    g.downloading = true;
    g.batch = true;
    g.current_tier = Some(tier.to_string());
    g.progress = (0, None);
    g.last_download = None;
    drop(g);

    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Ok(mut cell) = download_task_cell().lock() {
        *cell = Some(DownloadTask { cancel: cancel.clone() });
    }
    let tier = tier.to_string();
    let cancel_for_task = cancel.clone();
    std::thread::spawn(move || {
        let result = download_tier_force(&tier, &cache_dir, Some(&cancel_for_task), all_exist);
        // 任务结束：若仍是自己的注册则清空（engine 取消时已 take，这里不再覆盖）
        if let Ok(mut cell) = download_task_cell().lock() {
            if cell.as_ref().map(|t| std::sync::Arc::ptr_eq(&t.cancel, &cancel_for_task)).unwrap_or(false) {
                *cell = None;
            }
        }
        if let Ok(mut g) = manager().lock() {
            g.downloading = false;
            g.current_tier = None;
            g.last_download = Some(result);
        }
    });
    Ok(())
}

/// 下载指定档位三件套到缓存目录（带逐块进度上报与取消检查）。
/// `force=true`：全部重新下载（覆盖已有文件）；`force=false`：只下载缺失文件。
fn download_tier_force(
    tier: &str,
    cache_dir: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    force: bool,
) -> Result<(), String> {
    let _guard = download_lock().lock().map_err(|_| "下载锁失效".to_string())?;
    let names = model_names_for_tier(tier);
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建 OCR 模型缓存目录失败: {e}"))?;
    // 整批进度：预估需下载文件的总大小，进度条跨文件单调递增到 100%
    // 批量下载(force=false)：只下载三处目录都没有的文件，total 只累加这些
    let need = |name: &str| force || !file_located(name, cache_dir);
    let batch_total: Option<u64> = names
        .iter()
        .copied()
        .try_fold(0u64, |acc, name| {
            if need(name) {
                known_file_size(name).map(|sz| acc + sz)
            } else {
                Some(acc) // 已存在的不计入总量
            }
        });
    let mut batch_offset: u64 = 0;
    for name in names {
        // 已被取消则不再继续下载后续文件
        if cancel.map(|c| c.load(std::sync::atomic::Ordering::SeqCst)).unwrap_or(false) {
            return Err("下载已取消".into());
        }
        // 非强制模式：任意目录已存在的文件跳过，不重复下载到缓存
        if !need(name) {
            tracing::info!("OCR: {name} 已存在，跳过");
            continue;
        }
        download_one(name, cache_dir, cancel, batch_offset, batch_total)?;
        batch_offset += known_file_size(name).unwrap_or(0);
    }
    // 全部完成：进度置满
    if let Some(t) = batch_total {
        update_progress(names.last().unwrap_or(&""), t, Some(t));
    }
    Ok(())
}

/// 后台下载单个模型文件（供文件行「下载」按钮；已存在则跳过）。
/// 与整档下载共用下载锁/进度/取消机制。
pub fn start_download_file(tier: &str, name: &str) -> Result<(), String> {
    let mut g = manager().lock().map_err(|_| "模型管理锁失效".to_string())?;
    if g.downloading {
        return Err("已有下载任务进行中".into());
    }
    g.downloading = true;
    g.batch = false;
    g.current_tier = Some(tier.to_string());
    g.current_file = name.to_string();
    g.progress = (0, None);
    g.last_download = None;
    drop(g);

    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Ok(mut cell) = download_task_cell().lock() {
        *cell = Some(DownloadTask { cancel: cancel.clone() });
    }
    let cache_dir = crate::config::ocr_cache_dir();
    let name = name.to_string();
    let cancel_for_task = cancel.clone();
    std::thread::spawn(move || {
        let result = (|| {
            let _guard = download_lock().lock().map_err(|_| "下载锁失效".to_string())?;
            // 总是下载该文件（已存在则覆盖，即「重新下载」语义）；
            // 单文件 batch_total = 该文件大小
            let batch_total = known_file_size(&name);
            download_one(&name, &cache_dir, Some(&cancel_for_task), 0, batch_total)
        })();
        if let Ok(mut cell) = download_task_cell().lock() {
            if cell.as_ref().map(|t| std::sync::Arc::ptr_eq(&t.cancel, &cancel_for_task)).unwrap_or(false) {
                *cell = None;
            }
        }
        if let Ok(mut g) = manager().lock() {
            g.downloading = false;
            g.current_tier = None;
            g.last_download = Some(result);
        }
    });
    Ok(())
}

/// 返回模型三件套路径。
///
/// 查找顺序：
/// 1. 用户显式指定目录（`OCR_MODEL_DIR` 环境变量或配置 `ocr.model_dir`）；
/// 2. 项目内 `models/PP-OCRv6`（开发者放置，相对当前工作目录）；
/// 3. 缓存目录：文件存在则用，否则从 GitHub 自动下载（engine 优先：
///    先取消后台下载任务，再持锁下载缺失文件）。
pub fn ensure_models(cache_dir: &Path) -> Result<[PathBuf; 3], String> {
    ensure_models_impl(cache_dir, None)
}

/// ensure_models 实现：`cancel` 为 Some 时可被 engine 的下载需求取消。
fn ensure_models_impl(
    cache_dir: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<[PathBuf; 3], String> {
    let [det, rec, dict] = model_names();
    let tier = effective_tier();
    for dir in crate::config::ocr_model_dir()
        .into_iter()
        .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
    {
        let paths = [dir.join(det), dir.join(rec), dir.join(dict)];
        if paths.iter().all(|p| p.exists()) {
            tracing::info!("OCR: 使用本地模型目录 {}（档位 {tier}）", dir.display());
            return Ok(paths);
        }
        tracing::warn!(
            "OCR: 模型目录 {} 缺少文件（需要 {}/{}/{}），继续查找",
            dir.display(),
            det,
            rec,
            dict
        );
    }
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建 OCR 模型缓存目录失败: {e}"))?;
    let mut out = [cache_dir.join(det), cache_dir.join(rec), cache_dir.join(dict)];
    if out.iter().any(|p| !p.exists()) {
        // 优先：取消其他后台下载任务（让其释放下载锁），再持锁接管下载。
        // engine 调用时 cancel=None（不可被取消）；后台确保时 cancel=Some。
        cancel_background_download();
        let _guard = download_lock().lock().map_err(|_| "下载锁失效".to_string())?;
        // 整批进度：只累加三处目录都没有的文件大小
        let all_names = [det, rec, dict];
        let need = |name: &str| !file_located(name, cache_dir);
        let batch_total: Option<u64> = all_names
            .iter()
            .copied()
            .try_fold(0u64, |acc, name| {
                if need(name) {
                    known_file_size(name).map(|sz| acc + sz)
                } else {
                    Some(acc)
                }
            });
        let mut batch_offset: u64 = 0;
        for (path, name) in out.iter_mut().zip([det, rec, dict]) {
            if !path.exists() {
                if cancel.map(|c| c.load(std::sync::atomic::Ordering::SeqCst)).unwrap_or(false) {
                    return Err("下载已取消（engine 接管）".into());
                }
                if file_located(name, cache_dir) {
                    // 别处目录已有，无需下载到缓存
                    batch_offset += known_file_size(name).unwrap_or(0);
                    continue;
                }
                download_one(name, cache_dir, cancel, batch_offset, batch_total)?;
                batch_offset += known_file_size(name).unwrap_or(0);
            } else {
                batch_offset += known_file_size(name).unwrap_or(0);
            }
        }
        if let Some(t) = batch_total {
            update_progress(dict, t, Some(t));
        }
    }
    Ok(out)
}

/// 获取（或懒初始化）识别引擎。模型路径来自缓存目录。
pub fn engine(cache_dir: &Path) -> Result<&'static OAROCR, String> {
    let mtx = ENGINE.get_or_init(|| Mutex::new(None));
    let mut guard = mtx.lock().map_err(|_| "OCR 引擎锁失效".to_string())?;
    if guard.is_none() {
        let [det, rec, dict] = ensure_models(cache_dir)?;
        let ocr = OAROCRBuilder::new(
            det.to_str().ok_or("模型路径非法")?,
            rec.to_str().ok_or("模型路径非法")?,
            dict.to_str().ok_or("模型路径非法")?,
        )
        .text_detection_config(TextDetectionConfig {
            score_threshold: 0.2,
            box_threshold: 0.45,
            unclip_ratio: 1.4,
            max_candidates: 3000,
            // det 最长边 960：低于此值时屏幕小字号文本会被过度缩小导致漏检
            // （实测 480 时 1191px 宽图文本行缩到 6px，small det 漏检 1/3）。
            // 960 保精度：det 推理 ~300ms，配合 small rec 总耗时仍 <800ms。
            limit_side_len: Some(960),
            limit_type: Some(LimitType::Max),
            ..Default::default()
        })
        .ort_session(
            oar_ocr::core::config::OrtSessionConfig::new()
                // 默认只做 Level1 基础图优化；Level3 常量折叠/算子融合对
                // PP-OCRv6 的卷积网络收益明显（实测推理 1.47s → ~0.9s）。
                .with_optimization_level(
                    oar_ocr::core::config::OrtGraphOptimizationLevel::All,
                )
                .with_intra_threads(6),
        )
        // rec 批大小：batch 张量宽度取组内最大行宽，一行超宽行会撑大整组
        // 导致窄行也被 padding 浪费计算（medium 下实测 batch8=17.6s,
        // batch4=15.8s, batch1=13.9s）→ 用 1 最小化 padding。
        .region_batch_size(1)
        .build()
        .map_err(|e| format!("初始化 PaddleOCR 引擎失败: {e}"))?;
        *guard = Some(Box::leak(Box::new(ocr)));
    }
    Ok(*guard.as_ref().unwrap())
}

/// 应用启动时后台预加载 OCR 引擎（不阻塞 UI 线程）。
///
/// 触发时机：`AppState::new()` 末尾。模型不存在时内部会自动下载
/// （首次可能耗时较长，但全程在后台线程）；加载完成后 `ENGINE`
/// 单例就绪，用户首次使用 OCR 时无需再等待模型加载。
/// 失败不 panic：仅记录警告，首次 OCR 时会再次尝试加载。
pub fn preload() {
    std::thread::spawn(|| {
        let t0 = std::time::Instant::now();
        let cache_dir = crate::config::ocr_cache_dir();
        match engine(&cache_dir) {
            Ok(_) => tracing::info!(
                "OCR: 引擎后台预加载完成（耗时 {:?}，内存约 550MB）",
                t0.elapsed()
            ),
            Err(e) => tracing::warn!(
                "OCR: 引擎后台预加载失败: {e}（首次 OCR 时将重试）"
            ),
        }
    });
}

/// 识别一块 RGB 图像（w×h×3 字节），返回按行拼接的文本。
pub fn recognize_rgb(rgb: &[u8], w: u32, h: u32) -> Result<String, String> {
    let img = oar_ocr::utils::create_rgb_image(w, h, rgb.to_vec())
        .ok_or_else(|| format!("RGB 数据长度不符: {w}x{h}"))?;
    let cache_dir = crate::config::ocr_cache_dir();
    let ocr = engine(&cache_dir)?;
    let results = ocr
        .predict(vec![img])
        .map_err(|e| format!("PaddleOCR 识别失败: {e}"))?;
    let mut lines: Vec<String> = Vec::new();
    for region in results.first().map(|r| &r.text_regions).into_iter().flatten() {
        if let Some(text) = region.text.as_ref() {
            let t = text.trim();
            if !t.is_empty() {
                lines.push(t.to_string());
            }
        }
    }
    Ok(lines.join("\n"))
}

/// 将引擎锁复位（测试用：换模型目录后重建）。
pub fn reset_engine() {
    if let Some(mtx) = ENGINE.get() {
        if let Ok(mut guard) = mtx.lock() {
            *guard = None;
        }
    }
}
