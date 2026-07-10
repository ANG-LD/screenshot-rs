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

use screenshot_rs::utils::color::{hsv_to_rgb, rgb_to_hsv};

#[test]
fn hsv_red_is_pure_red() {
    let (r, g, b) = hsv_to_rgb(0.0, 1.0, 1.0);
    assert_eq!(r, 255);
    assert_eq!(g, 0);
    assert_eq!(b, 0);
}

#[test]
fn hsv_green_is_pure_green() {
    let (r, g, b) = hsv_to_rgb(120.0, 1.0, 1.0);
    assert_eq!(r, 0);
    assert_eq!(g, 255);
    assert_eq!(b, 0);
}

#[test]
fn hsv_blue_is_pure_blue() {
    let (r, g, b) = hsv_to_rgb(240.0, 1.0, 1.0);
    assert_eq!(r, 0);
    assert_eq!(g, 0);
    assert_eq!(b, 255);
}

#[test]
fn hsv_white_is_pure_white() {
    let (r, g, b) = hsv_to_rgb(0.0, 0.0, 1.0);
    assert_eq!(r, 255);
    assert_eq!(g, 255);
    assert_eq!(b, 255);
}

#[test]
fn hsv_black_is_pure_black() {
    let (r, g, b) = hsv_to_rgb(0.0, 0.0, 0.0);
    assert_eq!(r, 0);
    assert_eq!(g, 0);
    assert_eq!(b, 0);
}

#[test]
fn rgb_to_hsv_roundtrip_red() {
    let (h, s, v) = rgb_to_hsv(255, 0, 0);
    assert!((h - 0.0).abs() < 0.01);
    assert!((s - 1.0).abs() < 0.01);
    assert!((v - 1.0).abs() < 0.01);
}
