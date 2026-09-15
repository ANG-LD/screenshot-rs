//! Marian 分词器：直接加载 HuggingFace 官方格式的 `tokenizer.json`。
//!
//! 为什么不自己撸 Unigram + Viterbi：`tokenizer.json` 的 normalizer 是
//! SentencePiece 的 `Precompiled` charsmap（二进制 DARTS 前缀树），自研要重写
//! 两百多行规范化查表，还容易和训练侧不一致、把译文里的标点悄悄改掉。
//! `tokenizers` crate 本来就是为这个格式写的，而且**它已经在依赖树里**
//! （`oar-ocr-core → tokenizers`），引进来零额外编译成本。

use std::path::Path;

use tokenizers::Tokenizer;

/// 英文 → 中文翻译用的分词器（源、目标共享同一份词表）。
pub struct MarianTokenizer {
    inner: Tokenizer,
}

impl MarianTokenizer {
    /// 从 `tokenizer.json` 加载（会先修掉一个会让 `tokenizers` **panic** 的坑，
    /// 见 [`patch_null_charsmap`]）。
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read(path)
            .map_err(|e| format!("读取分词器失败 {}: {e}", path.display()))?;
        let patched = patch_null_charsmap(&raw);
        let inner = Tokenizer::from_bytes(&patched)
            .map_err(|e| format!("加载分词器失败 {}: {e}", path.display()))?;
        Ok(Self { inner })
    }

    /// 批量编码为 token id 序列。
    ///
    /// `add_special_tokens = true`：tokenizer.json 里的 post_processor
    /// （TemplateProcessing）会自动在末尾补 `</s>`，即 Marian 的 EOS——解码循环
    /// 靠它判断结束，所以这里不能手动再加一次。
    pub fn encode_batch(&self, texts: &[String]) -> Result<Vec<Vec<i64>>, String> {
        let encodings = self
            .inner
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| format!("分词失败: {e}"))?;
        Ok(encodings
            .into_iter()
            .map(|e| e.get_ids().iter().map(|id| *id as i64).collect())
            .collect())
    }

    /// 单个文本编码（转发批量接口，保证两条路径行为完全一致）。
    pub fn encode(&self, text: &str) -> Result<Vec<i64>, String> {
        self.encode_batch(&[text.to_string()])
            .map(|mut v| v.pop().unwrap_or_default())
    }

    /// 解码 token id → 文本。`skip_special_tokens = true`：丢掉 `</s>`/`<pad>`，
    /// 并按 tokenizer.json 的 decoder（Metaspace）把 `▁` 还原成空格。
    pub fn decode(&self, ids: &[i64]) -> Result<String, String> {
        let ids: Vec<u32> = ids
            .iter()
            .filter_map(|id| u32::try_from(*id).ok())
            .collect();
        self.inner
            .decode(&ids, true)
            .map_err(|e| format!("解码失败: {e}"))
    }
}

/// 修掉 `tokenizer.json` 里会让 `tokenizers` **panic** 的一处：
///
/// Xenova 导出的 Marian 词表里 normalizer 是
/// `{"type":"Precompiled","precompiled_charsmap":null}`，而 tokenizers 在反序列化
/// Precompiled 时写的是 `.expect("Precompiled")` —— 遇到 null 不是返回错误而是
/// **直接 panic**（tokenizers 0.23.1 / normalizers/mod.rs）。
///
/// 这个 charsmap 本是 SentencePiece 的 `nmt_nfkc` 规范化表，导出时被置空
/// （transformers.js 侧不实现它）。这里换成等价的 NFKC：英译中的输入是 OCR 出来的
/// 英文，NFKC 与 nmt_nfkc 在这些字符上的行为一致，又比「完全不规范化」更接近
/// Python 参考实现——否则全角标点这类字符会分不出 token。
///
/// 只改 normalizer 这一处；charsmap 正常（非 null）或本来就不是 Precompiled 的
/// 词表原样透传，JSON 解析失败也原样透传（让下游报出真正的原因）。
fn patch_null_charsmap(raw: &[u8]) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return raw.to_vec();
    };
    let needs_patch = v
        .get("normalizer")
        .map(|n| {
            n.get("type").and_then(|t| t.as_str()) == Some("Precompiled")
                && n.get("precompiled_charsmap").map(|c| c.is_null()).unwrap_or(true)
        })
        .unwrap_or(false);
    if !needs_patch {
        return raw.to_vec();
    }
    tracing::info!("翻译: 词表的 Precompiled charsmap 为空（导出时被置空），改用 NFKC 规范化");
    if let Some(norm) = v.get_mut("normalizer") {
        *norm = serde_json::json!({ "type": "NFKC" });
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| raw.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_null_charsmap_only() {
        // null charsmap → 换成 NFKC（否则 tokenizers 会 panic）
        let null = br#"{"normalizer":{"type":"Precompiled","precompiled_charsmap":null},"x":1}"#;
        let out: serde_json::Value = serde_json::from_slice(&patch_null_charsmap(null)).unwrap();
        assert_eq!(out["normalizer"]["type"], "NFKC");
        // 其余字段必须原样保留
        assert_eq!(out["x"], 1);

        // charsmap 有内容 → 一个字节都不改（不能破坏真实规范化的词表）
        let real = br#"{"normalizer":{"type":"Precompiled","precompiled_charsmap":"QUJD"}}"#;
        assert_eq!(patch_null_charsmap(real), real.to_vec());

        // 非 Precompiled → 不动
        let nfkc = br#"{"normalizer":{"type":"NFKC"}}"#;
        assert_eq!(patch_null_charsmap(nfkc), nfkc.to_vec());

        // 没有 normalizer / 非法 JSON → 原样透传（让下游报真正的原因）
        let none = br#"{"model":{"type":"Unigram"}}"#;
        assert_eq!(patch_null_charsmap(none), none.to_vec());
        assert_eq!(patch_null_charsmap(b"not json"), b"not json".to_vec());
    }

    /// 真词表测试：需要先把模型下载到本地（设置 `SCREENSHOT_RS_TRANSLATE_MODEL_DIR`
    /// 指向含 tokenizer.json 的目录才跑，否则直接跳过）。
    /// 这样 CI 里不必带 6MB 词表，本地又能真实验证 encode/decode 往返。
    fn tokenizer_if_available() -> Option<MarianTokenizer> {
        let dir = std::env::var("SCREENSHOT_RS_TRANSLATE_MODEL_DIR").ok()?;
        let path = std::path::Path::new(&dir).join("tokenizer.json");
        if !path.exists() {
            return None;
        }
        MarianTokenizer::from_file(&path).ok()
    }

    #[test]
    fn encode_decode_roundtrip_with_real_vocab() {
        let Some(tk) = tokenizer_if_available() else {
            eprintln!("跳过：未设置 SCREENSHOT_RS_TRANSLATE_MODEL_DIR");
            return;
        };
        // 编码必须非空、且末尾是 EOS(0)——解码循环靠 EOS 结束
        let ids = tk.encode("Hello world.").unwrap();
        assert!(!ids.is_empty());
        assert_eq!(*ids.last().unwrap(), 0, "末尾应为 </s>（EOS）");
        // 往返：英文文本应能基本还原（Metaspace 的 ▁ → 空格）
        let back = tk.decode(&ids[..ids.len() - 1]).unwrap();
        assert!(back.contains("Hello"), "还原结果异常: {back:?}");
        assert!(back.contains("world"), "还原结果异常: {back:?}");
        // 批量接口与单个接口必须给出相同结果
        let batch = tk.encode_batch(&["Hello world.".to_string()]).unwrap();
        assert_eq!(batch[0], ids);
    }

    #[test]
    fn decode_ignores_special_and_negative_ids() {
        let Some(tk) = tokenizer_if_available() else { return };
        // 负数/超大 id 不能 panic（解码循环里出现异常值时必须安全降级）
        let out = tk.decode(&[-1, 0, i64::MAX]).unwrap();
        assert!(out.trim().is_empty(), "特殊 token 应被跳过，得到 {out:?}");
    }
}
