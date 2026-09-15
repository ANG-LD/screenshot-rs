//! 英文 → 中文离线翻译。
//!
//! 引擎用 Helsinki-NLP `opus-mt-en-zh`（Marian，编码器 6 层 + 解码器 6 层，
//! d_model=512）的 ONNX int8 版，**直接跑在项目已有的 ONNX Runtime 上**：
//! 不引入新的原生依赖（`trad` / `ct2rs` 那条路要用 cmake 现场编译整个
//! CTranslate2 C++ 运行时，而且模型只能从 HuggingFace 拉）。
//!
//! 模型规模：编码器 50MB + 带 KV cache 的合并解码器 57MB + 词表 6MB ≈ 108MB，
//! 首次使用时按需下载到缓存目录，与 OCR 模型共用同一套「缓存目录 + .part
//! 临时文件 + 原子 rename」的下载约定。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

pub mod engine;
pub mod tokenizer;

/// 主下载源（HuggingFace 官方）。
const BASE_HF: &str = "https://huggingface.co/Xenova/opus-mt-en-zh/resolve/main";

/// 备用镜像：实测本机直连 huggingface.co 8 秒超时不可达，hf-mirror.com 0.17s 返回
/// 200。HF 优先、镜像兜底（第一个文件连不上就整轮切到镜像，不再每个文件白等一次）。
const BASE_MIRROR: &str = "https://hf-mirror.com/Xenova/opus-mt-en-zh/resolve/main";

/// 需要下载的模型文件（相对路径, 期望字节数）。
///
/// 两个量化文件的取舍（实测体积）：
/// - `encoder_model_int8.onnx` 50MB：编码器没有大词嵌入，int8 够用。
/// - 解码器选 `decoder_model_quantized.onnx`（ORT 动态量化，57MB）而**不是**
///   `decoder_model_int8.onnx`（184MB）：后者只量化了 MatMul 权重，65k×512 的
///   词嵌入表还是 fp32（133MB），而 ORT 动态量化会把嵌入一起量化掉。
/// - 也不用带 KV cache 的 `decoder_model_merged_int8.onnx`（57MB)：merged 版必须
///   喂 `use_cache_branch` 这个 **bool** 输入，而 ort 2.0.0-rc.13 没有为 bool 实现
///   张量元素类型，绕不过去；翻译按行做、序列很短，全量重算的代价远小于这个坑。
///
/// 期望字节数用于校验——下载被截断时（连接中断但没报错）能识别出来并要求重下。
const MODEL_FILES: [(&str, u64); 5] = [
    ("config.json", 1503),
    ("generation_config.json", 293),
    ("tokenizer.json", 6380952),
    ("onnx/encoder_model_int8.onnx", 52726553),
    ("onnx/decoder_model_quantized.onnx", 59842102),
];

/// 已知的单个文件体积（进度条在没有 Content-Length 时兜底显示）。
fn known_size(rel: &str) -> Option<u64> {
    MODEL_FILES.iter().find(|(n, _)| *n == rel).map(|(_, s)| *s)
}

/// 已确认可用的下载源序号（0=HF, 1=镜像），NONE 表示还没试过。
/// 避免每个文件都先撞一次超时：一旦 HF 失败就整轮走镜像。
const BASE_UNKNOWN: usize = usize::MAX;
static PREFERRED_BASE: AtomicUsize = AtomicUsize::new(BASE_UNKNOWN);

/// 下载进度（UI 轮询展示）。
#[derive(Debug, Clone, Default)]
pub struct TranslateProgress {
    /// 正在下载的文件（相对路径）
    pub file: String,
    /// 已下载字节数
    pub downloaded: u64,
    /// 总字节数（服务端没给 Content-Length 时用已知体积兜底，仍未知则 None）
    pub total: Option<u64>,
    /// 整体是否已完成（全部文件就位）
    pub done: bool,
}

fn progress_cell() -> &'static Mutex<TranslateProgress> {
    static P: OnceLock<Mutex<TranslateProgress>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(TranslateProgress::default()))
}

// ---------------------------------------------------------------------------
// 模型管理窗口用的状态快照 + 非阻塞下载
// 形状刻意与 `ocr::paddle::{model_snapshot, start_download}` 保持一致，
// 这样模型管理窗口里两栏（OCR / 翻译）能用同一套渲染逻辑。
// ---------------------------------------------------------------------------

/// 单个模型文件在磁盘上的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// 存在且体积与预期一致
    Ready,
    /// 不存在
    Missing,
    /// 存在但体积不符（上次下载被截断，或上游换了量化版本）
    WrongSize,
}

/// 单个文件的快照。
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    /// 仓库内相对路径，同时用作界面显示名
    pub name: &'static str,
    /// 期望字节数
    pub expected: u64,
    /// 本地实际字节数（不存在为 None）
    pub local_size: Option<u64>,
    pub status: FileStatus,
    /// 本地绝对路径
    pub local_path: std::path::PathBuf,
}

/// 翻译模型的整体快照（模型管理窗口每次重绘拉一份）。
#[derive(Debug, Clone)]
pub struct ModelSnapshot {
    pub cache_dir: std::path::PathBuf,
    pub files: Vec<FileSnapshot>,
    /// 全部文件就位
    pub ready: bool,
    /// 是否有下载在跑
    pub downloading: bool,
    /// 正在下载的文件（相对路径）
    pub current_file: Option<String>,
    /// (已下载字节, 总量)
    pub progress: (u64, Option<u64>),
    /// 上一次下载失败的原因
    pub last_error: Option<String>,
    /// 当前使用的下载源（界面显示"从哪下"）
    pub base_url: String,
    /// 总占用（就位文件的字节和）与总需求
    pub bytes_on_disk: u64,
    pub bytes_total: u64,
}

/// 是否有下载线程在跑。
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

fn last_error_cell() -> &'static Mutex<Option<String>> {
    static E: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(None))
}

/// 是否有翻译模型下载在进行中。
pub fn is_downloading() -> bool {
    DOWNLOADING.load(Ordering::SeqCst)
}

/// 收集模型快照（UI 线程用，非阻塞）。
pub fn model_snapshot() -> ModelSnapshot {
    let dir = model_dir();
    let mut files = Vec::with_capacity(MODEL_FILES.len());
    let (mut on_disk, mut total) = (0u64, 0u64);
    for (rel, expected) in MODEL_FILES {
        let path = dir.join(rel);
        let local_size = std::fs::metadata(&path).ok().map(|md| md.len());
        let status = match local_size {
            Some(n) if n == expected => FileStatus::Ready,
            Some(_) => FileStatus::WrongSize,
            None => FileStatus::Missing,
        };
        if status == FileStatus::Ready {
            on_disk += expected;
        }
        total += expected;
        files.push(FileSnapshot {
            name: rel,
            expected,
            local_size,
            status,
            local_path: path,
        });
    }
    let ready = files.iter().all(|f| f.status == FileStatus::Ready);
    let p = progress();
    let last_error = last_error_cell().lock().ok().and_then(|g| g.clone());
    ModelSnapshot {
        cache_dir: dir,
        files,
        ready,
        downloading: is_downloading(),
        current_file: (!p.file.is_empty()).then(|| p.file.clone()),
        progress: (p.downloaded, p.total),
        last_error,
        base_url: base_urls().into_iter().next().unwrap_or_default(),
        bytes_on_disk: on_disk,
        bytes_total: total,
    }
}

/// 启动后台下载（非阻塞），供模型管理窗口的「下载」按钮调用。
///
/// 返回 `false` 表示已经有一个下载在跑（不重复起线程——两个线程写同一批
/// `.part` 文件会互相覆盖）。
pub fn start_download() -> bool {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return false;
    }
    if let Ok(mut g) = last_error_cell().lock() {
        *g = None;
    }
    // 进度清零：否则界面会先显示上一次下载的残留数字，看着像"已经在下了"
    if let Ok(mut g) = progress_cell().lock() {
        *g = TranslateProgress::default();
    }
    std::thread::spawn(move || {
        let dir = model_dir();
        tracing::info!("翻译: 开始下载模型 → {}", dir.display());
        let result = ensure_models(&dir);
        if let Err(e) = &result {
            tracing::error!("翻译: 模型下载失败: {e}");
            if let Ok(mut g) = last_error_cell().lock() {
                *g = Some(e.clone());
            }
        }
        if let Ok(mut g) = progress_cell().lock() {
            g.done = result.is_ok();
        }
        DOWNLOADING.store(false, Ordering::SeqCst);
        tracing::info!("翻译: 下载线程结束（成功={}）", result.is_ok());
    });
    true
}

/// 读取当前下载进度（UI 线程用，非阻塞）。
pub fn progress() -> TranslateProgress {
    progress_cell().lock().map(|g| g.clone()).unwrap_or_default()
}

/// 下载源列表：环境变量 `SCREENSHOT_RS_TRANSLATE_BASE_URL` > 配置
/// `translate.base_url` > [HuggingFace, hf-mirror]。
pub fn base_urls() -> Vec<String> {
    if let Some(custom) = crate::config::translate_base_url() {
        return vec![custom.trim_end_matches('/').to_string()];
    }
    // 上一轮下载已经确认 HF 连不上（撞过连接超时）就把镜像放前面，
    // 省掉后续每个文件一次 5s 的白等。两个源都保留，互为兜底。
    match PREFERRED_BASE.load(Ordering::Relaxed) {
        1 => vec![BASE_MIRROR.to_string(), BASE_HF.to_string()],
        _ => vec![BASE_HF.to_string(), BASE_MIRROR.to_string()],
    }
}

/// HTTP 客户端：读空闲 30s（下载卡住能及时失败），**连接超时 5s**——
/// 直连 huggingface.co 在国内是「连不上但也不立刻报错」，默认 30s 连接超时会让
/// 回退镜像变得无法忍受。
fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(30))
            .build()
    })
}

/// 翻译模型文件的本地路径集合。
#[derive(Debug, Clone)]
pub struct ModelPaths {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
}

/// 由缓存目录推出各文件路径（不检查是否存在）。
pub fn model_paths(dir: &Path) -> ModelPaths {
    ModelPaths {
        encoder: dir.join("onnx/encoder_model_int8.onnx"),
        decoder: dir.join("onnx/decoder_model_quantized.onnx"),
        tokenizer: dir.join("tokenizer.json"),
        config: dir.join("config.json"),
    }
}

/// 五个文件是否都已就位且体积正确。
pub fn models_ready(dir: &Path) -> bool {
    MODEL_FILES.iter().all(|(rel, size)| {
        std::fs::metadata(dir.join(rel))
            .map(|md| md.len() == *size)
            .unwrap_or(false)
    })
}

/// 确保模型就位：缺哪个下哪个（体积不符视为损坏，重下）。
///
/// `dir` 一般是 `config::translate_cache_dir()`；测试里可以指到临时目录。
pub fn ensure_models(dir: &Path) -> Result<ModelPaths, String> {
    if models_ready(dir) {
        return Ok(model_paths(dir));
    }
    let bases = base_urls();
    let mut last_err = String::new();
    for (rel, size) in MODEL_FILES {
        let dest = dir.join(rel);
        if let Ok(md) = std::fs::metadata(&dest) {
            if md.len() == size {
                continue;
            }
            tracing::warn!(
                "翻译: 模型 {rel} 体积不符（{} != {size}），重新下载",
                md.len()
            );
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建模型目录 {} 失败: {e}", parent.display()))?;
        }
        let mut ok = false;
        for (idx, base) in bases.iter().enumerate() {
            match download_one(base, rel, &dest, size) {
                Ok(()) => {
                    if idx > 0 {
                        PREFERRED_BASE.store(idx, Ordering::Relaxed);
                    } else {
                        PREFERRED_BASE.store(0, Ordering::Relaxed);
                    }
                    ok = true;
                    break;
                }
                Err(e) => {
                    tracing::warn!("翻译: 从 {base} 下载 {rel} 失败: {e}");
                    last_err = e;
                }
            }
        }
        if !ok {
            return Err(format!("翻译模型 {rel} 下载失败：{last_err}"));
        }
    }
    if let Ok(mut g) = progress_cell().lock() {
        g.done = true;
        g.downloaded = g.total.unwrap_or(g.downloaded);
    }
    tracing::info!("翻译: 模型已就绪（{}）", dir.display());
    Ok(model_paths(dir))
}

/// 从单个源下载一个文件：先写 `.part` 再原子 rename，避免半截文件被当成可用模型。
fn download_one(base: &str, rel: &str, dest: &Path, expected: u64) -> Result<(), String> {
    let url = format!("{base}/{rel}");
    tracing::info!("翻译: 下载模型 {rel} ← {url}");
    let resp = http_agent()
        .get(&url)
        .call()
        .map_err(|e| format!("{e}"))?;
    let header_len = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());
    let total = header_len.or_else(|| known_size(rel));
    let mut reader = resp.into_reader();
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("创建 {tmp:?} 失败: {e}"))?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| format!("写入失败: {e}"))?;
        downloaded += n as u64;
        if let Ok(mut g) = progress_cell().lock() {
            g.file = rel.to_string();
            g.downloaded = downloaded;
            g.total = total;
        }
    }
    // 体积校验：截断的文件（网络中断）不能让引擎拿到。
    // 服务端给了 Content-Length 就以它为准——上游哪天换了量化版本，体积变了也能正常下；
    // 没给才退回内置的期望体积（此时 expected 是唯一的判据）。
    let ok = match header_len {
        Some(len) => downloaded == len,
        None => downloaded == expected,
    };
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "体积不符（下载 {downloaded} 字节，期望 {}）",
            header_len.map(|l| l.to_string()).unwrap_or_else(|| expected.to_string())
        ));
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("重命名 {tmp:?} 失败: {e}"))?;
    tracing::info!("翻译: {rel} 下载完成（{downloaded} 字节）");
    Ok(())
}

/// 翻译一段英文（可含多行）。
///
/// 多行输入按行翻译：OCR 出来的排版（缩进、空行）能被保留下来，也不会因为
/// 把整段塞进一个序列而顶到 512 token 上限。
pub fn translate(text: &str) -> Result<String, String> {
    let dir = model_dir();
    if !models_ready(&dir) {
        return Err("翻译模型尚未下载完成".to_string());
    }
    engine::translate(text, &model_paths(&dir))
}

/// 翻译模型所在目录。
///
/// 默认是配置里的缓存目录；`SCREENSHOT_RS_TRANSLATE_MODEL_DIR` 可覆盖——这是**开发/测试
/// 钩子**（`tests/ocr_translate_pipeline.rs` 靠它指向临时目录，免得为了跑一次测试就把
/// 110MB 模型塞进用户缓存）。正常运行时该变量不存在，行为不变。
pub fn model_dir() -> std::path::PathBuf {
    match std::env::var("SCREENSHOT_RS_TRANSLATE_MODEL_DIR") {
        Ok(d) if !d.trim().is_empty() => std::path::PathBuf::from(d),
        _ => crate::config::translate_cache_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照必须自洽：模型管理窗口直接拿这些数字画界面（文件行数、总占用、
    /// "就绪/未就绪"），一旦口径不一致，界面会显示自相矛盾的状态。
    #[test]
    fn model_snapshot_is_self_consistent() {
        let snap = model_snapshot();
        assert_eq!(snap.files.len(), MODEL_FILES.len());
        assert_eq!(
            snap.bytes_total,
            MODEL_FILES.iter().map(|(_, n)| *n).sum::<u64>(),
            "总需求应等于各文件期望值之和"
        );
        assert_eq!(
            snap.ready,
            snap.files.iter().all(|f| f.status == FileStatus::Ready),
            "ready 必须等价于全部文件就绪"
        );
        for f in &snap.files {
            match f.local_size {
                Some(n) if n == f.expected => assert_eq!(f.status, FileStatus::Ready),
                Some(_) => assert_eq!(f.status, FileStatus::WrongSize),
                None => assert_eq!(f.status, FileStatus::Missing),
            }
            assert!(f.local_path.ends_with(f.name), "{:?}", f.local_path);
            assert!(f.expected > 0);
        }
    }

    #[test]
    fn base_url_list_prefers_hf_and_includes_mirror() {
        // 不触碰全局配置（可能被环境变量影响），直接验证常量本身的形态
        assert!(BASE_HF.starts_with("https://huggingface.co/"));
        assert!(BASE_MIRROR.starts_with("https://hf-mirror.com/"));
        // 两者镜像自同一仓库，只有主机名不同——换源时路径拼接才一致
        assert_eq!(
            BASE_HF.splitn(4, '/').nth(3),
            BASE_MIRROR.splitn(4, '/').nth(3)
        );
    }

    #[test]
    fn known_sizes_match_file_table() {
        assert_eq!(known_size("tokenizer.json"), Some(6380952));
        assert_eq!(known_size("onnx/encoder_model_int8.onnx"), Some(52726553));
        assert_eq!(known_size("onnx/decoder_model_quantized.onnx"), Some(59842102));
        assert_eq!(known_size("没这个文件"), None);
        // 体积表不能有 0（0 会让体积校验形同虚设）
        assert!(MODEL_FILES.iter().all(|(_, s)| *s > 0));
    }

    #[test]
    fn models_ready_requires_correct_sizes() {
        let dir = std::env::temp_dir().join(format!("screenshot-rs-tr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!models_ready(&dir)); // 目录都不存在
        // 造一个体积不符的文件：必须仍判定为未就绪
        let p = model_paths(&dir);
        std::fs::create_dir_all(p.tokenizer.parent().unwrap()).unwrap();
        std::fs::write(&p.tokenizer, b"x").unwrap();
        assert!(!models_ready(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
