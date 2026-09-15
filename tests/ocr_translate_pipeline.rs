//! 端到端流水线：真图 → OCR → 英译中。
//!
//! 单独成一个测试，是因为"OCR 吐出来的一行行文字交给翻译引擎"这一段在别处没法验证：
//! 单元测试只喂干净句子，而 OCR 的真实输出带空格/标点/换行怪癖，最可能在这里出问题。
//!
//! 需要外部资源，缺任一就跳过（与 `translate_smoke.rs` 同一套路）：
//!   SCREENSHOT_RS_TRANSLATE_MODEL_DIR —— 翻译模型目录
//!   SCREENSHOT_RS_PIPELINE_PNG        —— 含英文文字的原图
#[test]
fn ocr_then_translate_real_image() {
    let dir = match std::env::var("SCREENSHOT_RS_TRANSLATE_MODEL_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => {
            eprintln!("跳过：未设置 SCREENSHOT_RS_TRANSLATE_MODEL_DIR");
            return;
        }
    };
    let png = match std::env::var("SCREENSHOT_RS_PIPELINE_PNG") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("跳过：未设置 SCREENSHOT_RS_PIPELINE_PNG");
            return;
        }
    };
    let img = image::open(&png).expect("读取测试图").to_rgb8();
    let (w, h) = (img.width(), img.height());
    let ocr = screenshot_rs::ocr::paddle::recognize_rgb(img.as_raw(), w, h).expect("OCR 失败");
    println!("--- OCR 原文 ---\n{ocr}");
    assert!(!ocr.trim().is_empty(), "OCR 没识别出文字");
    let zh = screenshot_rs::translate::translate(&ocr).expect("翻译失败");
    println!("--- 译文 ---\n{zh}");
    assert!(
        zh.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
        "译文里没有汉字: {zh}"
    );
    // OCR 的排版（空行）必须保留：截图里分段是信息，翻完挤成一坨就没法读了
    let (a, b) = (ocr.lines().count(), zh.lines().count());
    assert_eq!(a, b, "译文行数与原文不一致（{a} vs {b}）");
    let _ = dir;
}
