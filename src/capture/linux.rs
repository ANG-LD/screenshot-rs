// 占位 - Task 7 填充
// Linux 平台屏幕捕获实现（X11 / Wayland）。
// 计划：
// - 探测运行环境：X11 还是 Wayland
// - 在 X11 下通过 `screenshots` crate 直接抓取
// - 在 Wayland 下借助 XDG Portal（未来扩展点）
// - 处理多显示器与 HiDPI 缩放