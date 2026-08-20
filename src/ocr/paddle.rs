//! PaddleOCR（PP-OCRv6）本地识别：替换 tesseract CLI。
//!
//! 模型文件（检测 / 识别 / 词典）按需从 GitHub Releases 下载到缓存目录，
//! 首次使用 OCR 时触发下载（与原先 tesseract 的分发模式一致）。推理由
//! `oar-ocr` 驱动（ONNX Runtime，构建时由 `download-binaries` feature
//! 下载 CPU 版运行时）。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};

/// 识别引擎的全局单例：懒初始化，线程安全（一次只允许一个识别任务）。
/// 引擎通过 `Box::leak` 提升为 `'static`（程序生命周期内常驻，不释放）。
static ENGINE: OnceLock<Mutex<Option<&'static OAROCR>>> = OnceLock::new();

/// 模型文件名（oar-ocr v0.7.0 release 的 PP-OCRv6 medium 档，检测 + 识别约 132 MB）
pub const MODEL_DET: &str = "pp-ocrv6_medium_det.onnx";
pub const MODEL_REC: &str = "pp-ocrv6_medium_rec.onnx";
pub const MODEL_DICT: &str = "ppocrv6_dict.txt";

/// 模型下载基址。注意：必须固定 v0.7.0 —— `latest` 指向 v0.9.2，但该
/// release 不带模型资产（404）；v0.7.0 起所有模型资产齐全（已逐一验证 HTTP 200）。
const MODEL_BASE_URL: &str = "https://github.com/GreatV/oar-ocr/releases/download/v0.7.0";

/// 返回模型三件套路径。
///
/// 查找顺序：
/// 1. 用户显式指定目录（`OCR_MODEL_DIR` 环境变量或配置 `ocr.model_dir`）；
/// 2. 项目内 `models/PP-OCRv6`（开发者放置，相对当前工作目录）；
/// 3. 缓存目录：文件存在则用，否则从 GitHub 自动下载。
pub fn ensure_models(cache_dir: &Path) -> Result<[PathBuf; 3], String> {
    for dir in crate::config::ocr_model_dir()
        .into_iter()
        .chain(std::env::current_dir().ok().map(|d| d.join("models").join("PP-OCRv6")))
    {
        let paths = [dir.join(MODEL_DET), dir.join(MODEL_REC), dir.join(MODEL_DICT)];
        if paths.iter().all(|p| p.exists()) {
            tracing::info!("OCR: 使用本地模型目录 {}", dir.display());
            return Ok(paths);
        }
        tracing::warn!(
            "OCR: 模型目录 {} 缺少文件（需要 {}/{}/{}），继续查找",
            dir.display(),
            MODEL_DET,
            MODEL_REC,
            MODEL_DICT
        );
    }
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建 OCR 模型缓存目录失败: {e}"))?;
    let mut out = [cache_dir.join(MODEL_DET), cache_dir.join(MODEL_REC), cache_dir.join(MODEL_DICT)];
    for (path, name) in out.iter_mut().zip([MODEL_DET, MODEL_REC, MODEL_DICT]) {
        if !path.exists() {
            let url = format!("{MODEL_BASE_URL}/{name}");
            tracing::info!("OCR: 下载模型 {name} ← {url}");
            let resp = ureq::get(&url)
                .call()
                .map_err(|e| format!("下载模型 {name} 失败: {e}"))?;
            let mut reader = resp.into_reader();
            let tmp = cache_dir.join(format!("{name}.part"));
            let mut file = std::fs::File::create(&tmp)
                .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
            std::io::copy(&mut reader, &mut file)
                .map_err(|e| format!("写入模型 {name} 失败: {e}"))?;
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
            ..Default::default()
        })
        .build()
        .map_err(|e| format!("初始化 PaddleOCR 引擎失败: {e}"))?;
        *guard = Some(Box::leak(Box::new(ocr)));
    }
    Ok(*guard.as_ref().unwrap())
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
