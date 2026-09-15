//! Marian 翻译引擎：ORT 编码器 + 解码器两个会话，贪心解码。
//!
//! ## 为什么不做 KV cache
//!
//! 带 KV cache 的 `decoder_model_merged` 版必须喂 `use_cache_branch` 这个
//! **bool** 输入，而 ort 2.0.0-rc.13 没有为 `bool` 实现张量元素类型，绕不过去。
//! 而翻译是**按行**做的（一行几个词到十几个词），解码步数少，每步全量重算的
//! O(n²) 在这个规模下就是几百毫秒，代价远小于为绕一个 bool 张量去碰 ORT 底层
//! API。真到了长段落瓶颈再换 merged 版 + 手工构造 bool tensor。
//!
//! ## 输入/输出名不写死
//!
//! 名字按**实际会话**发现（精确名优先，找不到再按后缀匹配），找不到时把会话里
//! 真实存在的名字全列出来报错。换个导出（`encoder_model`/`decoder_model` 变体
//! 命名不完全一致）时，报错信息直接能看出差在哪，而不是一句 "invalid input name"。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use ort::session::Session;
use ort::session::SessionInputValue;
use ort::value::Tensor;

use super::tokenizer::MarianTokenizer;
use super::ModelPaths;

/// 从 `config.json` 读出来的、解码循环真正需要的几个参数。
#[derive(Debug, Clone, Copy)]
struct ModelConfig {
    /// 解码起始 token（Marian 是 `<pad>`，本模型 = 65000）
    decoder_start_id: i64,
    /// 句末 token（`</s>` = 0），贪心解到它就停
    eos_id: i64,
    /// `<pad>`：generation_config 的 `bad_words_ids` 禁止它作为输出
    pad_id: i64,
    /// 位置编码上限（512）
    max_length: usize,
    /// 束宽（模型 config 里 num_beams=4）
    num_beams: usize,
    /// 长度惩罚指数（模型没写则用 HF 默认 1.0 = 按长度归一化）
    length_penalty: f32,
}

impl ModelConfig {
    fn from_json(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取模型配置失败 {}: {e}", path.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("解析模型配置失败: {e}"))?;
        let num = |key: &str, default: i64| -> i64 {
            v.get(key).and_then(|x| x.as_i64()).unwrap_or(default)
        };
        Ok(Self {
            decoder_start_id: num("decoder_start_token_id", 65000),
            eos_id: num("eos_token_id", 0),
            pad_id: num("pad_token_id", 65000),
            max_length: num("max_length", 512).clamp(1, 512) as usize,
            // 束宽**不读模型配置**：模型里写的是 num_beams=4，但实测在这个导出图上
            // beam 是负优化（opus-mt-en-zh int8+quantized，本机 CPU）：
            //   k=1（贪心）：单句 0.10~0.38s，六句 1.6s；「Screenshot」→ 截图，
            //                「Hello world.」→ 你好，世界。
            //   k=4（beam）：单句 ~3.5s（≈13 倍），六句 21s；因为模型在裸词/短句上
            //                几乎不吐 EOS，4 条束一路跑到步数上限，且长度归一化后反而
            //                挑中更碎的那条（「Hello world.」→ 你好，世界好，你好吗？）。
            // 所以固定贪心；beam 实现（beam_search）保留，换模型或调惩罚时可以直接对比。
            num_beams: NUM_BEAMS,
            length_penalty: v
                .get("length_penalty")
                .and_then(|x| x.as_f64())
                .map(|x| x as f32)
                .filter(|x| *x > 0.0)
                .unwrap_or(1.0),
        })
    }
}

/// 编码器输出：形状 [1, S, D] 的隐藏状态 + 序列长度。
struct Encoded {
    data: Vec<f32>,
    /// [1, S, D]
    shape: [usize; 3],
}

/// 翻译引擎：两个 ORT 会话 + 分词器 + 模型参数。
pub struct Engine {
    encoder: Session,
    decoder: Session,
    tk: MarianTokenizer,
    cfg: ModelConfig,

    // 实际使用的输入/输出名（加载时发现）
    enc_input_ids: String,
    enc_attn_mask: Option<String>,
    enc_hidden_out: String,
    dec_input_ids: String,
    dec_attn_mask: Option<String>,
    dec_enc_hidden: String,
    dec_logits: String,

    /// 词表大小（从 logits 最后一维取，用于切出最后一步的分布）
    vocab: usize,

    // ↓ 解码每步复用的缓冲。放在 self 上而不是每步新建：词表 65000 项，
    //   一屏六行、每行一二十步，每步 260KB 的分配/清零会变成可观的开销。
    /// 施加过重复惩罚的 logit 整行（长度 = vocab）
    logits: Vec<f32>,
    /// 「最近 REP_WINDOW 个 token 里出现过」的标记（长度 = vocab）
    repeated: Vec<bool>,
    /// 「prev 之后曾出现过」的 token（相邻二元组约束用，长度 ≤ 已生成长度）
    followers: Vec<i64>,
    /// 本步挑出的 top-k 候选 (对数概率, token)，由 `step_top` 返回其切片
    top: Vec<(f32, i64)>,
}

impl Engine {
    /// 加载模型（首次约 0.5~1s：两个会话 + 6MB 词表）。
    pub fn load(paths: &ModelPaths) -> Result<Self, String> {
        let cfg = ModelConfig::from_json(&paths.config)?;
        let tk = MarianTokenizer::from_file(&paths.tokenizer)?;

        // CPU 线程数：与 OCR 保持一致用 6（再高收益递减，还会和 OCR 抢核）
        let encoder = build_session(&paths.encoder, 6)?;
        let decoder = build_session(&paths.decoder, 6)?;

        let enc_input_ids = pick(&encoder, &["input_ids"], "输入")?;
        let enc_attn_mask = pick_opt(&encoder, &["attention_mask"]);
        let enc_hidden_out = pick_output(&encoder, &["last_hidden_state", "hidden_states"])?;
        let dec_input_ids = pick(&decoder, &["input_ids"], "输入")?;
        let dec_attn_mask = pick_opt(&decoder, &["encoder_attention_mask", "attention_mask"]);
        let dec_enc_hidden = pick(&decoder, &["encoder_hidden_states"], "输入")?;
        let dec_logits = pick_output(&decoder, &["logits"])?;

        tracing::info!(
            "翻译: 引擎就绪（编码器 {enc_hidden_out} / 解码器 {dec_logits}，\
             起始 token {} / EOS {} / pad {}）",
            cfg.decoder_start_id,
            cfg.eos_id,
            cfg.pad_id
        );
        Ok(Self {
            encoder,
            decoder,
            tk,
            cfg,
            enc_input_ids,
            enc_attn_mask,
            enc_hidden_out,
            dec_input_ids,
            dec_attn_mask,
            dec_enc_hidden,
            dec_logits,
            vocab: 0,
            logits: Vec::new(),
            repeated: Vec::new(),
            followers: Vec::new(),
            top: Vec::new(),
        })
    }

    /// 翻译一行（不做分行；空行由调用方处理）。
    pub fn translate_line(&mut self, line: &str) -> Result<String, String> {
        let ids = self.tk.encode(line)?;
        // 位置编码上限 512，给解码留出空间；超长行截断（截图里的单行不会这么长）
        let ids: Vec<i64> = ids.into_iter().take(self.cfg.max_length - 64).collect();
        if ids.is_empty() {
            return Ok(String::new());
        }
        let enc = self.encode(&ids)?;
        let out = self.beam_search(enc)?;
        Ok(polish_zh(&self.tk.decode(&out)?))
    }

    /// 编码：`input_ids` + 全 1 注意力掩码 → `[1, S, D]` 隐藏状态。
    fn encode(&mut self, ids: &[i64]) -> Result<Encoded, String> {
        let n = ids.len();
        let mask = vec![1i64; n];
        let ids_t = tensor_i64([1, n], ids.to_vec())?;
        let mask_t = tensor_i64([1, n], mask)?;
        let mut feeds: Vec<(String, SessionInputValue)> =
            vec![(self.enc_input_ids.clone(), (&ids_t).into())];
        if let Some(name) = &self.enc_attn_mask {
            feeds.push((name.clone(), (&mask_t).into()));
        }
        let outputs = self
            .encoder
            .run(feeds)
            .map_err(|e| format!("编码失败: {e}"))?;
        let value = outputs
            .get(self.enc_hidden_out.as_str())
            .ok_or_else(|| format!("编码器没有输出 {}", self.enc_hidden_out))?;
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("读取编码结果失败: {e}"))?;
        // 拷成 owned：编码结果只有几十 KB（S×512×4 字节），而解码每一步都要用到它，
        // 不像 KV cache 那样每步几十 MB，所以这里 clone 是最省事且不亏的选择。
        let dims: Vec<usize> = shape.iter().map(|d| (*d).max(0) as usize).collect();
        if dims.len() != 3 {
            return Err(format!("编码器输出形状异常: {dims:?}"));
        }
        Ok(Encoded {
            data: data.to_vec(),
            shape: [dims[0].max(1), dims[1], dims[2]],
        })
    }

    /// Beam search 解码：每步把 k 条候选束拼成一个 batch 前向一次，取全局最优的 k 条。
    ///
    /// 为什么必须做：贪心在**超短输入**上会退化自锁——「Settings」译成
    /// 「设置设置环境设置」、「Screenshot」译成「截截图」。beam 能在第 2~3 步就挑出
    /// 那条以 EOS 收尾的路径。模型自己的 generation_config 就写了 `num_beams=4`，
    /// 这里是照它的意图实现。
    ///
    /// 代价：每步一次 batch=k 的前向（叠加自回归的全量重算，整体约为贪心的 k 倍），
    /// 单句从 ~0.15s 到 ~0.5s；截图翻译是一次性操作，这个代价可以接受。
    fn beam_search(&mut self, enc: Encoded) -> Result<Vec<i64>, String> {
        let Encoded { data, shape } = enc;
        let s = shape[1];
        let d = shape[2];
        let k = self.cfg.num_beams.max(1);
        // 编码结果只准备一份（batch=1）：下面每条束各跑一次前向。
        // 不做 batch=k 的合并前向是有原因的——这个导出图里 cross-attention 的
        // encoder mask 广播对 batch>1 会报 "Attempting to broadcast ... 4 by 16"，
        // 而 batch=1 的形状是已验证可用的。算力上两者等价（每步都是 k 次同样长度的
        // 前向），只是多几次 kernel 启动开销。
        // `data` 是**移动**进来的：`Tensor::from_array` 本来就要拿走所有权，
        // 原来这里 clone 一份（S×512×4 字节）纯属白拷；调用点只有一个
        // （translate_line），编码结果用完即弃。
        let hidden = Tensor::from_array(([1usize, s, d], data))
            .map_err(|e| format!("构造编码结果张量失败: {e}"))?;
        let mask = Tensor::from_array(([1usize, s], vec![1i64; s]))
            .map_err(|e| format!("构造编码掩码张量失败: {e}"))?;

        /// 一条候选：已生成的 token + 累计对数概率
        struct Beam {
            ids: Vec<i64>,
            score: f32,
        }
        let mut active: Vec<Beam> = vec![Beam { ids: Vec::new(), score: 0.0 }];
        let mut finished: Vec<Beam> = Vec::new();
        let max_new = self
            .cfg
            .max_length
            .min(s.saturating_mul(4).max(32))
            .min(256);

        for _ in 0..max_new {
            // 收集所有 (累计分数, 来自哪条束, token)
            let mut cands: Vec<(f32, usize, i64)> = Vec::with_capacity(active.len() * k);
            for (bi, beam) in active.iter().enumerate() {
                // 惩罚、log_softmax、取前 k 全在 step_top 里就地完成（见其注释：
                // 其中 k==1 走的是零分配的单遍 argmax 快路径）。
                let top = self.step_top(&hidden, &mask, &beam.ids)?;
                for &(lp, tok) in top {
                    cands.push((beam.score + lp, bi, tok));
                }
            }
            cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

            let mut next_active: Vec<Beam> = Vec::with_capacity(k);
            for (score, bi, tok) in cands.into_iter().take(k) {
                let mut ids = active[bi].ids.clone();
                if tok == self.cfg.eos_id {
                    // 以 EOS 收尾：记入候选，不再继续扩展
                    finished.push(Beam { ids, score });
                } else {
                    ids.push(tok);
                    next_active.push(Beam { ids, score });
                }
            }
            if next_active.is_empty() {
                break;
            }
            active = next_active;
        }
        // 还没结束的活跃束也算候选（可能一直没吐 EOS）
        finished.extend(active);

        // 长度归一化后取最优：HF 的 length_penalty 默认 1.0，即分数除以长度。
        // 不做归一化会系统性偏袒长句（也正是不做 beam 时"重复到天荒地老"的成因）。
        let norm = |b: &Beam| b.score / (b.ids.len().max(1) as f32).powf(self.cfg.length_penalty);
        finished.sort_by(|a, b| norm(b).partial_cmp(&norm(a)).unwrap_or(std::cmp::Ordering::Equal));
        Ok(finished.remove(0).ids)
    }

    /// 跑一次解码器前向，就地把**最后一个位置**的分布处理成 top-k 候选：
    /// 返回按对数概率降序的 `(log 概率, token)`，最多 k 条（`NUM_BEAMS = 1` 时只有 1 条）。
    ///
    /// 为什么不再返回整行 logits：词表 65000 项，原来的路径是
    /// `step_logits` 把整行 `to_vec()`（260KB）→ 调用方 `log_softmax` 再分配一份
    /// → 建 `Vec<(f32,i64)>`（约 1MB，每项 16 字节含 padding）与 `repeated: Vec<bool>`
    /// （65KB）→ 最后对 65000 个元素**全量排序**，而这些只为了取 1 个最大值。
    /// 一屏六行、每行一二十步，这些固定开销是每步都要付的（本机冒烟测试六句：
    /// 1.65~1.75s → 1.48s）。现在：借用的 ORT 输出行上直接算惩罚（写进复用缓冲）
    /// → 一遍扫描求 log_sum_exp（不再落一份 log_softmax 的 Vec）→ k==1 用单遍 argmax。
    ///
    /// 数值路径与原实现**逐位等价**，因此是纯性能改写：
    /// - 惩罚后的 logit 逐项与原来相同（同样的三条约束、同样的应用顺序）；
    /// - 极值/求和仍按**索引升序**折叠，浮点结果与原 `log_softmax` 一致；
    /// - 平票取**最小索引**，与原「过滤非有限值 → 稳定排序取首个最大」一致。
    ///
    /// 故意在原始 logit 上做惩罚而不是在 log-softmax 之后：
    /// 见 `REP_PENALTY` 的注释（在概率上做会把「几乎确定的 token」也重罚）。
    fn step_top(
        &mut self,
        hidden: &Tensor<f32>,
        mask: &Tensor<i64>,
        generated: &[i64],
    ) -> Result<&[(f32, i64)], String> {
        let n = 1 + generated.len();
        let mut ids: Vec<i64> = Vec::with_capacity(n);
        ids.push(self.cfg.decoder_start_id);
        ids.extend_from_slice(generated);
        let ids_t =
            Tensor::from_array(([1usize, n], ids)).map_err(|e| format!("构造解码输入失败: {e}"))?;
        let mut feeds: Vec<(String, SessionInputValue)> = vec![
            (self.dec_input_ids.clone(), (&ids_t).into()),
            (self.dec_enc_hidden.clone(), hidden.into()),
        ];
        if let Some(name) = &self.dec_attn_mask {
            feeds.push((name.clone(), mask.into()));
        }
        let outputs = self
            .decoder
            .run(feeds)
            .map_err(|e| format!("解码失败: {e}"))?;
        let value = outputs
            .get(self.dec_logits.as_str())
            .ok_or_else(|| format!("解码器没有输出 {}", self.dec_logits))?;
        let (shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("读取解码结果失败: {e}"))?;
        let dims: Vec<usize> = shape.iter().map(|x| (*x).max(0) as usize).collect();
        if dims.len() != 3 {
            return Err(format!("解码器输出形状异常: {dims:?}"));
        }
        let vocab = dims[2];
        self.vocab = vocab;
        // [1, seq, vocab] → 最后一步那一行（**借用** ORT 的输出缓冲，不拷出来）
        let base = (dims[1].saturating_sub(1)) * vocab;
        let row = &data[base..base + vocab];

        let pad_id = self.cfg.pad_id;
        let prev = generated.last().copied().unwrap_or(self.cfg.decoder_start_id);

        // 三条约束的判定材料，全在**原始 logit** 上做（见 REP_PENALTY 的注释说明为什么）：
        //   ① pad 封禁 —— 照模型 generation_config 的 bad_words_ids=[[65000]]；
        //   ② 相邻二元组不重复 —— 治「截截图」这类退化；
        //   ③ 近期出现过的 token 惩罚 logit/1.15 —— 治长距离重复。
        //
        // ①/② 的判定与原实现完全相同，只是换了算法：原实现给**每个词表项**都扫一遍
        // 最近 REP_WINDOW 个 token（65000×20 次比较）、再扫一遍 `generated.windows(2)`
        // （65000×长度 次比较）。这里反过来只遍历那 ≤20 个 token / 那几个二元组，
        // 把它们标进复用缓冲 —— 判定结果一模一样，比较次数降到 O(20)。
        let repeated = &mut self.repeated;
        repeated.clear();
        repeated.resize(vocab, false);
        for &t in generated.iter().rev().take(REP_WINDOW) {
            // 越界/负数 token 原实现也不可能命中（词表索引 0..vocab），直接忽略
            if let Some(slot) = repeated.get_mut(t as usize) {
                *slot = true;
            }
        }
        let followers = &mut self.followers;
        followers.clear();
        for w in generated.windows(2) {
            if w[0] == prev {
                followers.push(w[1]);
            }
        }

        // 施加惩罚，得到本步真正参与比较的 logit 整行（写进复用缓冲，不新建 Vec）
        let logits = &mut self.logits;
        logits.clear();
        logits.reserve(vocab);
        for (i, &v) in row.iter().enumerate() {
            let tok = i as i64;
            let banned = tok == pad_id || prev == tok || followers.contains(&tok);
            logits.push(if banned {
                f32::NEG_INFINITY
            } else if repeated[i] {
                v / REP_PENALTY
            } else {
                v
            });
        }

        // 一遍扫描求 log_sum_exp（不落 Vec）：log_softmax 的极值与求和顺序与原
        // 实现相同（索引升序、同一个 f32::max 折叠），所以 (v - max) - log_sum
        // 与原实现逐位一致。
        let k = self.cfg.num_beams.max(1);
        let top = &mut self.top;
        top.clear();
        if k == 1 {
            let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let sum: f32 = logits.iter().map(|v| (v - max).exp()).sum();
            let log_sum = sum.ln();
            // 快路径：单遍严格大于的 argmax。
            // 必须用**严格大于**：原实现是「过滤掉非有限 logit → 稳定排序」，分数
            // 相同时稳定排序保持原索引顺序，也就是取**最小索引**；严格大于只在严格
            // 更大时才替换，二者等价。比较的是 lp（对数概率）而不是原始 logit ——
            // 减法会把低于 ulp 的差异抹平，原实现比的就是抹平后的值。
            let mut best: Option<(f32, i64)> = None;
            for (i, &v) in logits.iter().enumerate() {
                if !v.is_finite() {
                    continue; // 与原 scored 的过滤条件一致（NaN/±inf 不参与）
                }
                let lp = (v - max) - log_sum;
                if best.map(|(b, _)| lp > b).unwrap_or(true) {
                    best = Some((lp, i as i64));
                }
            }
            // 全行都是非有限值时（理论上只在模型吐出全 NaN 时发生）没有候选，
            // 与原实现「scored 为空 → 不推候选」一致。
            top.extend(best);
        } else {
            // k>1 保留原来的「全排序取前 k」路径（含整行 log_softmax 分配）：
            // 只在换模型/调惩罚做对比时才会走到（NUM_BEAMS 的注释记录了为什么固定
            // 贪心），所以不为它做优化，也就没必要重写。
            let logp = log_softmax(logits);
            let mut scored: Vec<(f32, i64)> = Vec::with_capacity(logp.len());
            for (i, lp) in logp.iter().enumerate() {
                if !logits[i].is_finite() {
                    continue;
                }
                scored.push((*lp, i as i64));
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            top.extend(scored.into_iter().take(k));
        }
        Ok(top.as_slice())
    }
}

/// log-softmax（数值稳定版）：先减最大值再取指数，避免 exp 溢出。
///
/// 现在只有 `NUM_BEAMS > 1` 的慢路径还在用它：贪心路径（k==1）把「求
/// log_sum_exp」和「取 argmax」合成一遍扫描，不再为整行 log_softmax 落一个
/// 65000 项的 Vec（见 `step_top` 的注释）。
fn log_softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = logits.iter().map(|v| (v - max).exp()).sum();
    let log_sum = sum.ln();
    logits.iter().map(|v| (v - max) - log_sum).collect()
}

/// 会话构造：图优化用默认（Level3，ORT 默认全开），CPU 线程数与 OCR 对齐。
fn build_session(path: &Path, threads: usize) -> Result<Session, String> {
    Session::builder()
        .map_err(|e| format!("创建 ORT 会话失败: {e}"))?
        .with_intra_threads(threads)
        .map_err(|e| format!("设置推理线程数失败: {e}"))?
        .commit_from_file(path)
        .map_err(|e| format!("加载模型失败 {}: {e}", path.display()))
}

fn tensor_i64<const N: usize>(
    shape: [usize; N],
    data: Vec<i64>,
) -> Result<Tensor<i64>, String> {
    Tensor::from_array((shape, data)).map_err(|e| format!("构造 int64 张量失败: {e}"))
}

/// 会话里所有输入名（报错时列给用户看）。
fn input_names(session: &Session) -> Vec<String> {
    session.inputs().iter().map(|i| i.name().to_string()).collect()
}

fn output_names(session: &Session) -> Vec<String> {
    session.outputs().iter().map(|o| o.name().to_string()).collect()
}

/// 按候选名挑一个输入：先精确匹配，再退化为「后缀匹配」。
fn pick(session: &Session, candidates: &[&str], kind: &str) -> Result<String, String> {
    pick_opt(session, candidates).ok_or_else(|| {
        format!(
            "模型缺少{kind} {candidates:?}；实际输入：{:?}",
            input_names(session)
        )
    })
}

fn pick_opt(session: &Session, candidates: &[&str]) -> Option<String> {
    let names = input_names(session);
    for c in candidates {
        if let Some(n) = names.iter().find(|n| n.as_str() == *c) {
            return Some(n.clone());
        }
    }
    for c in candidates {
        if let Some(n) = names.iter().find(|n| n.ends_with(*c)) {
            return Some(n.clone());
        }
    }
    None
}

/// 输出名的发现（与输入同理，但走后缀匹配优先——导出常给输出加前缀）。
fn pick_output(session: &Session, candidates: &[&str]) -> Result<String, String> {
    let names = output_names(session);
    for c in candidates {
        if let Some(n) = names.iter().find(|n| n.as_str() == *c) {
            return Ok(n.clone());
        }
    }
    for c in candidates {
        if let Some(n) = names.iter().find(|n| n.ends_with(*c)) {
            return Ok(n.clone());
        }
    }
    Err(format!(
        "模型缺少输出 {candidates:?}；实际输出：{names:?}"
    ))
}

/// 解码束宽。见 `ModelConfig::from_json` 里的实测记录：这个模型上贪心全面优于 beam。
const NUM_BEAMS: usize = 1;

/// 重复惩罚的回顾窗口：只看最近 20 个已生成 token。
const REP_WINDOW: usize = 20;
/// 重复惩罚系数。1.15 是 HF 生成配置里的常见温和值：够打断「设置设置设置…」，
/// 又不会把正常用词压掉。
///
/// **必须作用在原始 logit 上（logit / 1.15），不能作用在对数概率上。** 曾经写成
/// `logp - ln(1.15)`，看似等价其实是灾难：对"几乎确定的 token"（log 概率 ≈ -0.01）
/// 等于施加 0.14 的巨大惩罚，被惩罚的重复词掉下来之后依然压过 EOS，模型于是永远不吐
/// EOS、越译越长（「Screenshot」→ 截图截屏截抓图、「Hello world.」→ 全世界都好，你们好）。
const REP_PENALTY: f32 = 1.15;

/// 全局引擎（首次调用时加载；加载失败不缓存，下次可重试）。
static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();

fn with_engine<T>(
    paths: &ModelPaths,
    f: impl FnOnce(&mut Engine) -> Result<T, String>,
) -> Result<T, String> {
    let cell = ENGINE.get_or_init(|| Mutex::new(None));
    let mut guard = cell
        .lock()
        .map_err(|e| format!("翻译引擎状态异常: {e}"))?;
    if guard.is_none() {
        *guard = Some(Engine::load(paths)?);
    }
    f(guard.as_mut().expect("刚初始化过"))
}

/// 释放引擎（模型管理里「重新下载」后调用，让下次翻译重新加载）。
pub fn reset() {
    if let Some(cell) = ENGINE.get() {
        if let Ok(mut g) = cell.lock() {
            *g = None;
        }
    }
}

/// 译文的中文排版整理（Marian 的输出是「字级 + 半角标点 + 空格」风格）：
///
/// - 汉字后的半角 `,;!?:` 换全角（只在**前一个字符是汉字**时换，避免动到英文、
///   数字千分位里的标点）；
/// - 去掉「汉字/中文标点」之间的空格——模型给每个 token 带一个前置空格，
///   直出会变成「操作成功完成 。」这种带空格的怪样子。
///
/// 只做这两件确定的事，别的（改词、补标点）一律不做，避免把模型的意思改掉。
fn polish_zh(s: &str) -> String {
    let is_cjk = |c: char| ('\u{4e00}'..='\u{9fff}').contains(&c);
    let is_zh_punct = |c: char| "，。、；：！？（）《》“”‘’—-…".contains(c);
    // 第一遍：半角标点 → 全角（依据「前一个字符是汉字」判断）
    let mut mid = String::with_capacity(s.len());
    for c in s.chars() {
        let prev_cjk = mid.chars().last().map(is_cjk).unwrap_or(false);
        let mapped = match c {
            ',' if prev_cjk => '，',
            ';' if prev_cjk => '；',
            '!' if prev_cjk => '！',
            '?' if prev_cjk => '？',
            ':' if prev_cjk => '：',
            _ => c,
        };
        mid.push(mapped);
    }
    // 第二遍：去掉汉字/中文标点之间的空格
    let chars: Vec<char> = mid.chars().collect();
    let mut out = String::with_capacity(mid.len());
    for (i, c) in chars.iter().enumerate() {
        if *c == ' ' {
            let prev = out.chars().last();
            let next = chars.get(i + 1).copied();
            let tight = |c: Option<char>| c.map(|c| is_cjk(c) || is_zh_punct(c)).unwrap_or(false);
            // 前一个是汉字/中文标点，后一个也是 → 这个空格是模型加出来的，丢掉
            if tight(prev) && tight(next) {
                continue;
            }
            // 「完成 。」这类：汉字 + 空格 + 中文标点，空格也丢掉
            if tight(prev) && next.map(|c| "。，、；：！？".contains(c)).unwrap_or(false) {
                continue;
            }
        }
        out.push(*c);
    }
    out
}

/// 翻译多行文本：按行翻译并保留空行，从而保住 OCR 出来的排版（缩进、段落）。
/// 是否是中日韩统一表意文字（判"这一行已经是中文"用，够用即可）。
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// 一行里是否已经有中文。
fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// 中英混排行：按"中文 / 非中文"切成连续片段，中文片段原样保留，非中文片段里的
/// 英文单词交给模型翻译，所有分隔空白也随之保留——这样整行的版式不会变。
fn translate_runs(engine: &mut Engine, core: &str) -> Result<String, String> {
    let mut out = String::with_capacity(core.len());
    let mut run = String::new();
    let mut run_is_cjk = false;
    for ch in core.chars() {
        let cjk = is_cjk(ch);
        if !run.is_empty() && cjk != run_is_cjk {
            out.push_str(&translate_run(engine, &run, run_is_cjk)?);
            run.clear();
        }
        run_is_cjk = cjk;
        run.push(ch);
    }
    if !run.is_empty() {
        out.push_str(&translate_run(engine, &run, run_is_cjk)?);
    }
    Ok(out)
}

/// 单个连续片段：中文片段、以及不含字母的片段（纯标点/数字/空格）原样返回；
/// 含字母的非中文片段翻译，并把它前后的空白原样贴回。
fn translate_run(engine: &mut Engine, run: &str, run_is_cjk: bool) -> Result<String, String> {
    if run_is_cjk || !run.chars().any(|c| c.is_ascii_alphabetic()) {
        return Ok(run.to_string());
    }
    let lead_len = run.len() - run.trim_start().len();
    let (lead, rest) = run.split_at(lead_len);
    let trail_len = rest.len() - rest.trim_end().len();
    let (body, trail) = rest.split_at(rest.len() - trail_len);
    let translated = engine.translate_line(body)?;
    if translated.trim().is_empty() {
        return Ok(run.to_string());
    }
    Ok(format!("{lead}{}{trail}", translated.trim()))
}

pub fn translate(text: &str, paths: &ModelPaths) -> Result<String, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    // 全空就不必加载模型（首次加载要几百毫秒）
    if lines.iter().all(|l| l.trim().is_empty()) {
        return Ok(text.to_string());
    }
    with_engine(paths, |engine| {
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        for line in &lines {
            if line.trim().is_empty() {
                out.push(String::new());
                continue;
            }
            // 行首缩进 / 行尾空白原样保留：截图里缩进是排版信息，模型会把它们吃掉
            let lead_len = line.len() - line.trim_start().len();
            let (lead, rest) = line.split_at(lead_len);
            let trail_len = rest.len() - rest.trim_end().len();
            let (core, trail) = rest.split_at(rest.len() - trail_len);

            if has_cjk(core) {
                // 已经含中文（中英混排、整行中文）：**不整行过模型**——那样中文会被
                // 当成英文源再加工，轻则语序乱掉，重则整行变成重复的乱码。
                // 只把其中的英文片段挑出来翻译，中文片段与所有空白原样保留。
                out.push(format!("{lead}{}{trail}", translate_runs(engine, core)?));
                continue;
            }
            let translated = engine.translate_line(core)?;
            // 纯符号/纯数字行译不出东西：保留原文，避免整行凭空消失
            out.push(if translated.trim().is_empty() {
                (*line).to_string()
            } else {
                format!("{lead}{}{trail}", translated.trim())
            });
        }
        Ok(out.join("\n"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真模型测试：需要 `SCREENSHOT_RS_TRANSLATE_MODEL_DIR` 指向模型目录。
    fn paths_if_available() -> Option<ModelPaths> {
        let dir = std::env::var("SCREENSHOT_RS_TRANSLATE_MODEL_DIR").ok()?;
        let dir = std::path::PathBuf::from(dir);
        super::super::models_ready(&dir).then(|| super::super::model_paths(&dir))
    }

    /// 中英混排：中文与版式（缩进、空行、行数）必须原样保留，只有英文片段被翻译。
    ///
    /// 这是用户明确要求的行为——混排内容整行过模型会把中文也当英文源再加工，
    /// 轻则语序乱掉、重则整行变成重复乱码。
    #[test]
    fn mixed_lines_keep_chinese_and_layout() {
        let Some(paths) = paths_if_available() else {
            eprintln!("跳过：未设置 SCREENSHOT_RS_TRANSLATE_MODEL_DIR");
            return;
        };
        let src = "Settings\n\n设置 Settings 项\n  缩进 Open the file\n全部中文行，不动它。";
        let out = translate(src, &paths).expect("翻译失败");
        println!("--- 混排原文 ---\n{src}\n--- 混排译文 ---\n{out}");
        let (si, so): (Vec<&str>, Vec<&str>) = (src.lines().collect(), out.lines().collect());
        assert_eq!(si.len(), so.len(), "行数必须一致（版式保留）");
        assert_eq!(so[1], "", "空行必须保留");
        // 第 0 行原文是纯英文 → 必须被译成中文（不再是原文）
        assert!(has_cjk(so[0]), "英文行应被译成中文: {:?}", so[0]);
        assert_ne!(so[0], si[0], "英文行原样没译");
        assert!(so[3].starts_with("  "), "行首缩进必须保留: {:?}", so[3]);
        assert!(so[3].contains('缩') || has_cjk(so[3]), "中文片段保留: {:?}", so[3]);
        // 混排行里的中文原样在
        assert!(so[2].contains("设置") && so[2].contains('项'), "混排行中文被改动: {:?}", so[2]);
        // 末行是纯中文 → 必须逐字不变
        assert_eq!(so[4], si[4], "纯中文行不该被改写");
    }

    #[test]
    fn translates_english_to_chinese() {
        let Some(paths) = paths_if_available() else {
            eprintln!("跳过：未设置 SCREENSHOT_RS_TRANSLATE_MODEL_DIR");
            return;
        };
        let mut engine = Engine::load(&paths).expect("加载引擎");
        let out = engine.translate_line("Hello world.").expect("翻译");
        assert!(!out.trim().is_empty(), "译文为空");
        // 译文必须含中日韩统一表意文字（这才是真的翻成了中文，而不是原样返回）
        assert!(
            out.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "译文里没有汉字: {out:?}"
        );
        assert!(!out.contains("Hello"), "没有真正翻译: {out:?}");
    }

    #[test]
    fn multi_line_keeps_blank_lines() {
        let Some(paths) = paths_if_available() else { return };
        let mut engine = Engine::load(&paths).expect("加载引擎");
        let out = super::translate("Open the file.\n\nSave it.", &paths).expect("翻译");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3, "行数应保持 3（含空行）: {out:?}");
        assert_eq!(lines[1], "", "空行必须保留");
        let _ = &mut engine;
    }

    #[test]
    fn polish_zh_tightens_chinese_punctuation() {
        // 模型直出的两处毛病：半角逗号 + 标点前多一个空格
        assert_eq!(polish_zh("操作成功完成 。"), "操作成功完成。");
        assert_eq!(
            polish_zh("连接到服务器失败, 请稍后再试一次 。"),
            "连接到服务器失败，请稍后再试一次。"
        );
        // 汉字之间不该有空格
        assert_eq!(polish_zh("保存 文件"), "保存文件");
        // 但英文/数字之间的空格必须保留（不能把英文句子的词连起来）
        assert_eq!(polish_zh("Open the file"), "Open the file");
        assert_eq!(polish_zh("版本 1.2.3 发布"), "版本 1.2.3 发布");
        // 纯英文里的半角标点不能被改成全角（避免误伤代码/路径）
        assert_eq!(polish_zh("a, b, c"), "a, b, c");
    }

    #[test]
    fn blank_input_short_circuits() {
        // 全空输入不该触发模型加载
        let paths = ModelPaths {
            encoder: "不存在的文件".into(),
            decoder: "不存在的文件".into(),
            tokenizer: "不存在的文件".into(),
            config: "不存在的文件".into(),
        };
        assert_eq!(super::translate("", &paths).unwrap(), "");
        assert_eq!(super::translate("  \n \t\n", &paths).unwrap(), "  \n \t\n");
    }
}
