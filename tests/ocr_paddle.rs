//! PaddleOCR 端到端集成测试。
//!
//! 需要 PP-OCRv6 medium 模型文件（约 132 MB），通过 `TESSERACT_CACHE_DIR`
//! 指向模型目录（检测/识别/词典放在 `<dir>/paddle/` 下）：
//!
//! ```bash
//! TESSERACT_CACHE_DIR=/path/to/models cargo test --test ocr_paddle -- --ignored --nocapture
//! ```

/// 仓库自带的测试图。原来直接用 `/tmp/ocr_test.png`，那张临时图会被系统清理，
/// 导致这条 ignored 测试以「测试图不存在」莫名失败（与代码无关）。
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ocr_test.png");

#[test]
#[ignore = "需要 PP-OCRv6 medium 模型文件（约 132 MB）"]
fn recognize_chinese_and_english() {
    let path = if std::path::Path::new(FIXTURE).exists() {
        FIXTURE
    } else {
        "/tmp/ocr_test.png"
    };
    let img = image::open(path).expect("测试图不存在");
    let rgb = img.to_rgb8();
    let text = screenshot_rs::ocr::paddle::recognize_rgb(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
    )
    .expect("识别失败");
    println!("OCR result: {text:?}");
    // 中文与英文都应识别出来（PP-OCRv6 中英混排）
    let has_chinese = text.contains("你好") || text.contains("中文");
    let has_english = text.to_lowercase().contains("hello") || text.to_lowercase().contains("world");
    assert!(has_chinese, "中文未识别出: {text:?}");
    assert!(has_english, "英文未识别出: {text:?}");
}
