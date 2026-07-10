//! 端到端集成测试
//!
//! 这些测试在没有真实 GPUI 窗口环境时大部分会跳过。
//! 实际验证靠 README 中的手测 checklist（见 Task 17）。

// 注意：本文件混合使用了 `bounds::Point` 和 `drawing::Point` 两种不同的 `Point` 类型。
// 通过 alias 区分：
// - `BoundsPoint`：`utils::bounds::Point`，用于 Bounds 几何运算
// - `Point`：`overlay::drawing::Point`，用于 DrawCommand 字段
// 这样可以避免类型冲突，同时保持调用代码的简洁。

#[test]
fn full_pipeline_smoke_test() {
    // 1. 创建 CapturedFrame（模拟屏幕捕获）
    let frame = screenshot_rs::capture::CapturedFrame {
        width: 100,
        height: 100,
        pixels: (0..100 * 100 * 4).map(|i| (i % 256) as u8).collect(),
    };

    // 2. 裁剪出 50x50 中心区域
    let clipped = frame.clip_region(25, 25, 50, 50).unwrap();
    assert_eq!(clipped.width, 50);
    assert_eq!(clipped.height, 50);

    // 3. 构造 DrawCommand 列表
    use screenshot_rs::overlay::drawing::{DrawCommand, Point, RGBA};
    let mut state = screenshot_rs::overlay::drawing::DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (Point::new(10.0, 10.0), Point::new(40.0, 40.0)),
        color: RGBA::RED,
        line_width: 2.0,
    });
    assert_eq!(state.commands.len(), 1);

    // 4. 验证 Bounds 几何运算
    //    Bounds<Point> 需要 bounds::Point 类型，因此用别名 `BoundsPoint` 显式区分
    use screenshot_rs::utils::bounds::{Bounds, Point as BoundsPoint};
    let b = Bounds::new(
        BoundsPoint::new(110.0, 70.0),
        BoundsPoint::new(10.0, 20.0),
    )
    .normalize();
    assert_eq!(b.origin.x, 10.0);
    assert_eq!(b.size.x, 100.0);
}

#[test]
fn color_conversion_roundtrip() {
    use screenshot_rs::utils::color::{hsv_to_rgb, rgb_to_hsv};
    let (h, s, v) = rgb_to_hsv(255, 0, 0);
    let (r, g, b) = hsv_to_rgb(h, s, v);
    assert_eq!((r, g, b), (255, 0, 0));
}