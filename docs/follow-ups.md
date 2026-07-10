# Follow-ups

## screenshots v0.6.0 future-incompat 警告

- **状态**：已知问题，编译时出现 "the following packages contain code that will be rejected by a future version of Rust: screenshots v0.6.0"
- **影响**：当前可编译运行，但未来 Rust 版本可能拒绝该 crate 的代码
- **解决路径**：
    1. 跟踪 screenshots crate 升级（v0.7+）
    2. 如果新版本不再维护，考虑 fork 或自实现
    3. 在 Cargo.toml 中 pin 具体版本号
- **跟踪任务**：在 Task 6/7 实际使用该 crate 时再处理
