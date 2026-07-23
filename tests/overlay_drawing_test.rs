//! 覆盖窗口绘图层测试

use screenshot_rs::overlay::drawing::{DrawCommand, DrawingState, Point as DrawPoint, RGBA};

fn rgba(r: u8, g: u8, b: u8, a: u8) -> RGBA {
    RGBA { r, g, b, a }
}

#[test]
fn drawing_state_starts_empty() {
    let state = DrawingState::new();
    assert_eq!(state.commands.len(), 0);
    assert_eq!(state.history_index, 0);
}

#[test]
fn drawing_state_push_increments_history() {
    let mut state = DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(10.0, 10.0)),
        color: rgba(255, 0, 0, 255),
        line_width: 2.0,
    });
    assert_eq!(state.commands.len(), 1);
    assert_eq!(state.history_index, 1);
}

#[test]
fn drawing_state_undo_does_not_delete_just_moves_index() {
    let mut state = DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(10.0, 10.0)),
        color: rgba(255, 0, 0, 255),
        line_width: 2.0,
    });
    state.undo();
    assert_eq!(state.commands.len(), 1);
    assert_eq!(state.history_index, 0);
    assert!(!state.is_visible(0));
}

#[test]
fn drawing_state_redo_restores() {
    let mut state = DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(10.0, 10.0)),
        color: rgba(255, 0, 0, 255),
        line_width: 2.0,
    });
    state.undo();
    state.redo();
    assert_eq!(state.history_index, 1);
    assert!(state.is_visible(0));
}

/// LIFO 语义：undo 隐藏最后一条命令，push 后截断已撤销的尾部。
#[test]
fn drawing_state_new_push_drops_redo_history() {
    let mut state = DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(10.0, 10.0)),
        color: rgba(255, 0, 0, 255),
        line_width: 2.0,
    });
    // Undo：隐藏第一条
    state.undo();
    assert_eq!(state.history_index, 0);
    // Push 新命令：截断已撤销的旧命令
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(20.0, 20.0)),
        color: rgba(0, 255, 0, 255),
        line_width: 2.0,
    });
    assert_eq!(state.history_index, 1);
    assert_eq!(state.commands.len(), 1); // 旧命令已被截断
    assert!(state.is_visible(0)); // 新命令可见
}
