//! 通用工具模块测试

use screenshot_rs::utils::bounds::Bounds;
use screenshot_rs::utils::bounds::Point;

#[test]
fn bounds_new_stores_origin_and_size() {
    let b = Bounds::new(Point::new(10.0, 20.0), Point::new(110.0, 70.0));
    assert_eq!(b.origin.x, 10.0);
    assert_eq!(b.origin.y, 20.0);
    assert_eq!(b.size.x, 100.0);
    assert_eq!(b.size.y, 50.0);
}

#[test]
fn bounds_normalize_handles_negative_size() {
    // 用户从右下角拖到左上角，width/height 会为负
    let b = Bounds::new(Point::new(110.0, 70.0), Point::new(10.0, 20.0)).normalize();
    assert_eq!(b.origin.x, 10.0);
    assert_eq!(b.origin.y, 20.0);
    assert_eq!(b.size.x, 100.0);
    assert_eq!(b.size.y, 50.0);
}

#[test]
fn bounds_contains_point() {
    let b = Bounds::new(Point::new(0.0, 0.0), Point::new(100.0, 100.0));
    assert!(b.contains(Point::new(50.0, 50.0)));
    assert!(!b.contains(Point::new(150.0, 50.0)));
    assert!(!b.contains(Point::new(-1.0, 0.0)));
}

#[test]
fn bounds_clamp_inside_limits() {
    let b = Bounds::new(Point::new(-50.0, -50.0), Point::new(200.0, 200.0))
        .clamp_inside(Bounds::new(Point::new(0.0, 0.0), Point::new(100.0, 100.0)));
    assert_eq!(b.origin.x, 0.0);
    assert_eq!(b.origin.y, 0.0);
    assert_eq!(b.size.x, 100.0);
    assert_eq!(b.size.y, 100.0);
}
