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

/// 模型下载/管理的整体状态快照（窗口每次刷新重新计算）
#[derive(Debug, Clone)]
pub struct ModelSnapshot {
    /// 当前档位（config 解析结果）
    pub tier: String,
    /// 档位说明
    pub tier_note: String,
    /// 模型下载基址
    pub base_url: String,
    /// 缓存目录
    pub cache_dir: PathBuf,
    /// 三件套文件状态（检测 / 识别 / 词典）
    pub files: Vec<ModelFileInfo>,
    /// 是否有下载任务进行中
    pub downloading: bool,
    /// 当前文件下载进度（已下载字节, 总字节）
    pub progress: (u64, Option<u64>),
    /// 最近一次下载结果（None=尚无下载动作）
    pub last_download: Option<Result<(), String>>,
}

/// 全局模型管理状态（下载线程与 UI 线程共享）
struct ModelManagerState {
    downloading: bool,
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
            progress: (0, None),
            current_file: String::new(),
            last_download: None,
        })
    })
}

/// 模型档位 → 模型文件名（oar-ocr v0.7.0 release 资产）。
/// small ≈30MB（det 9.4MB + rec 21MB + dict 75KB，CPU 快）；
/// medium ≈132MB（det 62MB + rec 76MB，准确率更高但 CPU 慢）。
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
        "medium" => "medium ≈132MB：准确率更高，但 CPU 上多行场景慢（~14s）".into(),
        _ => "small ≈30MB：CPU 快（代码区 ~0.8s），准确率 ~95%".into(),
    }
}

/// 当前档位（config 解析）
fn model_names() -> [&'static str; 3] {
    model_names_for_tier(&crate::config::ocr_model_tier())
}

/// 模型下载基址。注意：必须固定 v0.7.0 —— `latest` 指向 v0.9.2，但该
/// release 不带模型资产（404）；v0.7.0 起所有模型资产齐全（已逐一验证 HTTP 200）。
const MODEL_BASE_URL: &str = "https://github.com/GreatV/oar-ocr/releases/download/v0.7.0";

/// 收集模型管理快照：按档位扫描本地查找位置，报告每个文件的状态。
pub fn model_snapshot() -> ModelSnapshot {
    let tier = crate::config::ocr_model_tier();
    let names = model_names_for_tier(&tier);
    let cache_dir = crate::config::ocr_cache_dir();
    let state = manager()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let snapshot = ModelSnapshot {
        tier: tier.clone(),
        tier_note: tier_note(&tier),
        base_url: MODEL_BASE_URL.to_string(),
        cache_dir: cache_dir.clone(),
        files: names
            .iter()
            .map(|name| {
                // 查找顺序：显式目录 → 项目 models/PP-OCRv6 → 缓存
                let mut local = None;
                for dir in crate::config::ocr_model_dir()
                    .into_iter()
                    .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
                    .chain(std::iter::once(cache_dir.clone()))
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
            .collect(),
        downloading: state.downloading,
        progress: state.progress,
        last_download: state.last_download.clone(),
    };
    snapshot
}

/// 更新下载进度（下载循环每块调用；UI 线程读取）
fn update_progress(name: &str, downloaded: u64, total: Option<u64>) {
    if let Ok(mut g) = manager().lock() {
        g.current_file = name.to_string();
        g.progress = (downloaded, total);
    }
}

/// 后台强制重新下载指定档位模型到缓存目录（已存在也覆盖，供「重新下载」）。
/// 进度写入全局状态，UI 轮询 `model_snapshot()` 展示。返回后 `last_download`
/// 记录结果。
pub fn start_download(tier: &str) -> Result<(), String> {
    let mut g = manager().lock().map_err(|_| "模型管理锁失效".to_string())?;
    if g.downloading {
        return Err("已有下载任务进行中".into());
    }
    g.downloading = true;
    g.progress = (0, None);
    g.last_download = None;
    drop(g);

    let cache_dir = crate::config::ocr_cache_dir();
    let tier = tier.to_string();
    std::thread::spawn(move || {
        let result = download_tier_force(&tier, &cache_dir);
        if let Ok(mut g) = manager().lock() {
            g.downloading = false;
            g.last_download = Some(result);
        }
    });
    Ok(())
}

/// 强制下载指定档位三件套到缓存目录（带逐块进度上报）。
fn download_tier_force(tier: &str, cache_dir: &Path) -> Result<(), String> {
    let names = model_names_for_tier(tier);
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建 OCR 模型缓存目录失败: {e}"))?;
    for name in names {
        let dest = cache_dir.join(name);
        let url = format!("{MODEL_BASE_URL}/{name}");
        tracing::info!("OCR: 下载模型 {name} ← {url}");
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("下载模型 {name} 失败: {e}"))?;
        let total = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok());
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
            file.write_all(&buf[..n])
                .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
            downloaded += n as u64;
            update_progress(name, downloaded, total);
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| format!("移动模型 {name} 失败: {e}"))?;
        tracing::info!("OCR: 模型 {name} 下载完成（{} 字节）", downloaded);
        update_progress(name, downloaded, total);
    }
    Ok(())
}

/// 返回模型三件套路径。
///
/// 查找顺序：
/// 1. 用户显式指定目录（`OCR_MODEL_DIR` 环境变量或配置 `ocr.model_dir`）；
/// 2. 项目内 `models/PP-OCRv6`（开发者放置，相对当前工作目录）；
/// 3. 缓存目录：文件存在则用，否则从 GitHub 自动下载。
pub fn ensure_models(cache_dir: &Path) -> Result<[PathBuf; 3], String> {
    let [det, rec, dict] = model_names();
    let tier = crate::config::ocr_model_tier();
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
    for (path, name) in out.iter_mut().zip([det, rec, dict]) {
        if !path.exists() {
            // 下载（带进度上报，模型管理窗口可见）
            let url = format!("{MODEL_BASE_URL}/{name}");
            tracing::info!("OCR: 下载模型 {name} ← {url}");
            let resp = ureq::get(&url)
                .call()
                .map_err(|e| format!("下载模型 {name} 失败: {e}"))?;
            let total = resp
                .header("Content-Length")
                .and_then(|s| s.parse::<u64>().ok());
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
                file.write_all(&buf[..n])
                    .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
                downloaded += n as u64;
                update_progress(name, downloaded, total);
            }
            std::fs::rename(&tmp, path)
                .map_err(|e| format!("移动模型 {name} 失败: {e}"))?;
            tracing::info!("OCR: 模型 {name} 下载完成");
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
