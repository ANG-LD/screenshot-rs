//! 选区状态机测试：覆盖 Resizing 分支（鼠标拖 handle 调整大小）

use screenshot_rs::overlay::selection::{DragState, SelectionState};
use screenshot_rs::utils::bounds::{Bounds, Handle, Point};

fn screen() -> Bounds {
    Bounds::new(Point::new(0.0, 0.0), Point::new(1920.0, 1080.0))
}

#[test]
fn selection_state_drag_top_left_shrinks() {
    let mut s = SelectionState::new(screen());
    // 先建一个 100x100 选区
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(200.0, 200.0));
    s.mouse_up();
    assert_eq!(s.current().unwrap().size, Point::new(100.0, 100.0));

    // 抓 TopLeft 把手往内拖：按下在 (100,100)，拖到 (130,130)，
    // 手柄跟随到 (130,130)（grab_offset = 0），选区应该变成 (130,130)-(200,200) = 70x70
    s.mouse_down(Point::new(100.0, 100.0));
    assert_eq!(s.drag, DragState::Resizing { handle: Handle::TopLeft, grab_offset: Point::new(0.0, 0.0) });
    s.mouse_move(Point::new(130.0, 130.0));
    s.mouse_up();
    let b = s.current().unwrap();
    assert_eq!(b.origin, Point::new(130.0, 130.0));
    assert_eq!(b.size, Point::new(70.0, 70.0));
}

#[test]
fn selection_state_resize_with_grab_offset_does_not_jump() {
    let mut s = SelectionState::new(screen());
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(200.0, 200.0));
    s.mouse_up();

    // 把手中心在 (100, 100)，按下点偏离 5px → grab_offset = (5, 5)
    s.mouse_down(Point::new(105.0, 105.0));
    // 鼠标移到 (115, 115)：手柄跟随到 (115 - 5, 115 - 5) = (110, 110)
    s.mouse_move(Point::new(115.0, 115.0));
    let b = s.current().unwrap();
    assert_eq!(b.origin, Point::new(110.0, 110.0));
    assert_eq!(b.size, Point::new(90.0, 90.0));
}

#[test]
fn selection_state_drag_bottom_right_grows() {
    let mut s = SelectionState::new(screen());
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(200.0, 200.0));
    s.mouse_up();

    // 抓 BottomRight 把手往下右拖：按下在 (200,200)，拖到 (250,260)
    s.mouse_down(Point::new(200.0, 200.0));
    assert_eq!(s.drag, DragState::Resizing { handle: Handle::BottomRight, grab_offset: Point::new(0.0, 0.0) });
    s.mouse_move(Point::new(250.0, 260.0));
    s.mouse_up();
    let b = s.current().unwrap();
    assert_eq!(b.origin, Point::new(100.0, 100.0));
    assert_eq!(b.size, Point::new(150.0, 160.0));
}

#[test]
fn selection_state_resize_clamps_to_screen() {
    let mut s = SelectionState::new(screen()); // 1920x1080
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(200.0, 200.0));
    s.mouse_up();

    // 把 TopLeft 把手拖到屏幕外 (-50, -50)，应该被裁剪回 (0, 0)
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(-50.0, -50.0));
    s.mouse_up();
    let b = s.current().unwrap();
    assert_eq!(b.origin, Point::new(0.0, 0.0));
}

#[test]
fn selection_state_click_inside_without_handle_enters_moving_not_resizing() {
    let mut s = SelectionState::new(screen());
    s.mouse_down(Point::new(100.0, 100.0));
    s.mouse_move(Point::new(200.0, 200.0));
    s.mouse_up();

    // 点选区内部（远离把手），应该进入 Moving
    s.mouse_down(Point::new(150.0, 150.0));
    assert!(matches!(s.drag, DragState::Moving { .. }));
}