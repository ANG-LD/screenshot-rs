use gpui::prelude::*;
use gpui::*;
use gpui_animation::prelude::*;
use std::time::Duration;

struct MyView {
    is_rotated: bool,
}

impl MyView {
    fn new() -> Self {
        Self { is_rotated: false }
    }

    fn toggle_rotation(&mut self) {
        self.is_rotated = !self.is_rotated;
    }
}

impl Render for MyView {
    fn render(&mut self, _window: &mut WindowContext) -> impl IntoElement {
        // 创建一个容器 div，并绑定旋转动画
        let input_container = div()
            .id("input-container")
            .with_transition("input-container")
            .transition_when(
                self.is_rotated,
                Duration::from_millis(300),
                gpui_animation::transition::general::Linear,
                |style| style.rotate(45.0),
            )
            .child(
                gpui::input::Input::new()
                    .placeholder("请输入文本...")
                    .width(px(300.0))
                    .height(px(40.0))
                    .padding(px(8.0))
                    .border_1()
                    .border_color(gpui::hsla(0.0, 0.0, 0.5, 1.0))
                    .rounded(px(4.0))
                    .bg(gpui::white()),
            );

        // 创建按钮
        let button = gpui::button::Button::new("toggle-btn")
            .label("旋转输入框")
            .on_click(|_, window, _| {
                // 这里需要通过某种方式获取 MyView 的实例并调用 toggle_rotation
                // 由于 GPUI 的架构，我们需要使用 cx 来更新状态
                // 这里仅作示意，实际使用时需要结合 AppContext 或 WindowContext
                // 具体实现方式取决于 GPUI 的版本和 API
                // 下面是一个简化的处理方式
                window.dispatch_action(MyAction::ToggleRotation);
            });

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap(px(20.0))
            .child(input_container)
            .child(button)
            .bg(gpui::gray(100))
    }
}

// 定义自定义 Action
enum MyAction {
    ToggleRotation,
}

impl Action for MyAction {
    // 实现 Action trait 所需的方法
}

// 处理 Action 的响应
impl MyView {
    fn on_action(&mut self, action: &MyAction, _window: &mut WindowContext) {
        match action {
            MyAction::ToggleRotation => {
                self.toggle_rotation();
            }
        }
    }
}

fn main() {
    // 初始化 GPUI 应用
    App::new().run(|app: &mut AppContext| {
        // 创建窗口
        app.new_window(|window| {
            // 初始化视图
            let view = MyView::new();
            // 返回视图作为窗口内容
            view
        });
    });
}