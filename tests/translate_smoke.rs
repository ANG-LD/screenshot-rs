//! 真实模型翻译冒烟测试。
//!
//! 需要先把模型准备好（约 110MB）：
//! ```text
//! SCREENSHOT_RS_TRANSLATE_MODEL_DIR=<含 tokenizer.json 与 onnx/ 的目录> cargo test --test translate_smoke -- --nocapture
//! ```
//! 没设置环境变量时直接跳过（CI 里不带模型也能过）。

/// 逐句翻译并打印，肉眼核对质量；同时断言译文确实变成了中文。
#[test]
fn translates_real_sentences() {
    let Ok(dir) = std::env::var("SCREENSHOT_RS_TRANSLATE_MODEL_DIR") else {
        eprintln!("跳过：未设置 SCREENSHOT_RS_TRANSLATE_MODEL_DIR");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if !screenshot_rs::translate::models_ready(&dir) {
        eprintln!("跳过：{} 下模型文件不齐", dir.display());
        return;
    }
    let paths = screenshot_rs::translate::model_paths(&dir);

    let cases = [
        "Hello world.",
        "Open the file and save it.",
        "Screenshot",
        "Settings",
        "The operation completed successfully.",
        "Failed to connect to the server, please try again later.",
    ];
    let started = std::time::Instant::now();
    for src in cases {
        let t0 = std::time::Instant::now();
        let out = screenshot_rs::translate::engine::translate(src, &paths)
            .unwrap_or_else(|e| panic!("翻译 {src:?} 失败: {e}"));
        println!("[{:>5}ms] {src}\n         → {out}", t0.elapsed().as_millis());
        assert!(!out.trim().is_empty(), "{src:?} 译文为空");
        assert!(
            out.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "{src:?} 译文里没有汉字: {out:?}"
        );
    }
    println!("六句合计 {:?}", started.elapsed());

    // 多行：排版必须保住（空行不被吞掉）
    let multi = screenshot_rs::translate::engine::translate(
        "Open the file.\n\nSave it as PNG.",
        &paths,
    )
    .expect("多行翻译");
    println!("--- 多行 ---\n{multi}");
    assert_eq!(multi.split('\n').count(), 3, "空行必须保留: {multi:?}");
}
