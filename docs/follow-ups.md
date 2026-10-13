# Follow-ups

## 给 gpui-component 上游提 PR，去掉 third_party/vendored 副本

- **状态**：待办。当前 `third_party/gpui-component` 与上游 0.7.3 只差两个文件（详见
  `third_party/README.md`）：`pub mod root` + `set_root_bordered()`，以及 `hide_tooltip()`。
- **影响**：不影响功能；成本是升级该 crate 时要重新取源码、重打这两处。
- **解决路径**：
    1. 上游 PR ①：让 `RootPlugin` 的 `window_border()` 可配置（或提供「无装饰」档）
    2. 上游 PR ②：公开一个清 tooltip 浮层的入口
    3. 上游发版后删除 `third_party/`，`Cargo.toml` 去掉 `[patch.crates-io]` 那条

## `XMODIFIERS` 指向未运行的输入法时收不到按键

- **状态**：已知环境问题（gpui-kit 侧行为）。环境里 `@im=fcitx` 而实际运行 ibus 时，
  应用连不上 XIM，日志 `zed_xim::x11rb: Try connect fcitx` 之后再无键事件，键盘全部失效。
- **影响**：只在环境变量与实际输入法不一致的机器上出现；正常桌面不受影响。
- **解决路径**：观察是否有用户遇到；必要时在上游反馈（让 XIM 连不上时有回退路径）。

## screenshots v0.6.0 future-incompat 警告

- **状态**：已知问题，编译时出现 "the following packages contain code that will be rejected by a future version of Rust: screenshots v0.6.0"
- **影响**：当前可编译运行，但未来 Rust 版本可能拒绝该 crate 的代码
- **解决路径**：
    1. 跟踪 screenshots crate 升级（v0.7+）
    2. 如果新版本不再维护，考虑 fork 或自实现
    3. 在 Cargo.toml 中 pin 具体版本号
- **跟踪任务**：在 Task 6/7 实际使用该 crate 时再处理
