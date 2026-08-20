//! PaddleOCR 端到端集成测试。
//!
//! 需要 PP-OCRv6 medium 模型文件（约 132 MB），通过 `TESSERACT_CACHE_DIR`
//! 指向模型目录（检测/识别/词典放在 `<dir>/paddle/` 下）：
//!
//! ```bash
//! TESSERACT_CACHE_DIR=/path/to/models cargo test --test ocr_paddle -- --ignored --nocapture
//! ```

use image::GenericImageView;

#[test]
#[ignore = "需要 PP-OCRv6 medium 模型文件（约 132 MB）"]
fn recognize_chinese_and_english() {
    let img = image::open("/tmp/ocr_test.png").expect("测试图不存在");
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
