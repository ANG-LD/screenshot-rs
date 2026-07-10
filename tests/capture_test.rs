//! 屏幕捕获模块测试
//!
//! 注：实际屏幕捕获依赖运行环境（需有真实显示器），CI 上跳过。
//! 这里只测试纯类型/数据结构的逻辑。

use screenshot_rs::capture::CapturedFrame;

#[test]
fn captured_frame_pixel_count_matches_dimensions() {
    let frame = CapturedFrame {
        width: 100,
        height: 50,
        pixels: vec![0; 100 * 50 * 4],
    };
    assert_eq!(frame.pixels.len(), (frame.width * frame.height * 4) as usize);
}

#[test]
fn captured_frame_can_be_clipped_to_subregion() {
    let frame = CapturedFrame {
        width: 100,
        height: 100,
        pixels: (0..100 * 100 * 4).map(|i| (i % 256) as u8).collect(),
    };
    // 取中心 10x10 区域
    let clipped = frame.clip_region(45, 45, 10, 10).unwrap();
    assert_eq!(clipped.width, 10);
    assert_eq!(clipped.height, 10);
    assert_eq!(clipped.pixels.len(), 10 * 10 * 4);
}

#[test]
fn captured_frame_clip_rejects_out_of_bounds() {
    let frame = CapturedFrame {
        width: 50,
        height: 50,
        pixels: vec![0; 50 * 50 * 4],
    };
    assert!(frame.clip_region(40, 40, 20, 20).is_err());
}
