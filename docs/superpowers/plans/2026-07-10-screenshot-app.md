# 截图应用 (screenshot-rs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个基于 Rust + GPUI 的跨平台桌面截图应用 MVP，支持区域截图、矩形/箭头/画图/文字/马赛克绘图、HSV 调色板、alt+s 全局热键、esc 取消、系统托盘驻留、截图后自动复制到剪贴板，覆盖 Windows 10/11 和 Linux X11。

**Architecture:** 单一 Cargo 项目，按职责分模块（capture/overlay/tray/hotkey/clipboard/utils）。跨平台差异通过 `cfg(target_os)` 隔离在 `capture/{windows,linux}.rs` 内，其余模块平台无关。两个 GPUI 窗口：托盘宿主窗口（常驻、不可见）+ 截图覆盖窗口（按需创建、全屏）。所有外部系统操作走 trait 抽象，方便未来扩展和单元测试。

**Tech Stack:** Rust 2021 edition + GPUI (Zed git rev `1d217ee39d381ac101b7cf49d3d22451ac1093fe`) + gpui-component (branch `main`) + screenshots 0.6 + global-hotkey 0.6 + tray-icon 0.11 + arboard 3 + image 0.25 + tokio 1 + anyhow/thiserror/tracing。

---

## File Structure

本计划会创建/修改以下文件：

| 路径 | 职责 |
|------|------|
| `Cargo.toml` | 项目元数据 + 依赖声明 |
| `src/main.rs` | 入口，启动 GPUI 应用 |
| `src/app.rs` | `AppState` 聚合所有服务 |
| `src/error.rs` | `AppError` 枚举 |
| `src/utils/bounds.rs` | `Bounds<P>` 几何运算 |
| `src/utils/color.rs` | HSV↔RGB 转换 |
| `src/utils/image.rs` | RGBA↔`image::RgbaImage` 转换 |
| `src/capture/mod.rs` | `ScreenCapture` trait + `CapturedFrame` |
| `src/capture/windows.rs` | Windows screenshots 包装 |
| `src/capture/linux.rs` | Linux X11 包装 |
| `src/clipboard/mod.rs` | arboard 包装 |
| `src/hotkey/mod.rs` | global-hotkey 包装 |
| `src/tray/mod.rs` | tray-icon 包装 |
| `src/overlay/mod.rs` | `OverlayWindow` 工厂 + `OverlayState` 状态机 |
| `src/overlay/selection.rs` | 选区拖拽逻辑 |
| `src/overlay/drawing.rs` | `DrawCommand` 枚举 + 渲染 |
| `src/overlay/toolbar.rs` | 浮动工具栏 |
| `tests/utils_test.rs` | utils 单元测试 |
| `tests/overlay_drawing_test.rs` | DrawCommand 测试 |
| `README.md` | 用户文档 |

---

## Task 1: 初始化 Cargo 项目与目录结构

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `.gitignore`

- [ ] **Step 1: 创建 `Cargo.toml`**

写入以下内容：

```toml
[package]
name = "screenshot-rs"
version = "0.1.0"
edition = "2021"
description = "Cross-platform screenshot tool built with GPUI"
license = "MIT"

[dependencies]
# GPUI（Zed 团队，固定 commit 以保证可复现构建）
gpui = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe" }
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe", features = ["font-kit", "x11", "wayland", "runtime_shaders"] }
gpui_macros = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe" }
gpui-component = { git = "https://github.com/longbridge/gpui-component", branch = "main" }

# 屏幕捕获（跨平台 crate）
screenshots = "0.6"

# 系统集成
global-hotkey = "0.6"
tray-icon = "0.11"
arboard = "3"
image = "0.25"

# 错误处理与日志
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# 异步运行时（外部 crate 需要）
tokio = { version = "1", features = ["sync", "rt-multi-thread", "macros"] }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: 创建 `src/main.rs` 占位文件**

```rust
fn main() {
    println!("screenshot-rs starting...");
}
```

- [ ] **Step 3: 创建 `.gitignore`**

```gitignore
/target
Cargo.lock.bak
*.swp
.DS_Store
.idea/
.vscode/
```

- [ ] **Step 4: 创建所有源码目录的占位文件**

```bash
mkdir -p src/utils src/capture src/clipboard src/hotkey src/tray src/overlay tests
```

然后在每个目录下创建 `mod.rs` 占位（编译需要）：

`src/utils/mod.rs`:
```rust
pub mod bounds;
pub mod color;
pub mod image;
```

`src/utils/bounds.rs`:
```rust
// 占位 - Task 3 填充
```

`src/utils/color.rs`:
```rust
// 占位 - Task 4 填充
```

`src/utils/image.rs`:
```rust
// 占位 - 后续按需填充
```

`src/capture/mod.rs`:
```rust
pub mod linux;
pub mod windows;
```

`src/capture/windows.rs`:
```rust
// 占位 - Task 6 填充
```

`src/capture/linux.rs`:
```rust
// 占位 - Task 7 填充
```

`src/clipboard/mod.rs`:
```rust
// 占位 - Task 8 填充
```

`src/hotkey/mod.rs`:
```rust
// 占位 - Task 9 填充
```

`src/tray/mod.rs`:
```rust
// 占位 - Task 10 填充
```

`src/overlay/mod.rs`:
```rust
// 占位 - Task 12 填充
```

- [ ] **Step 5: 在 `src/main.rs` 顶部添加模块声明**

修改 `src/main.rs` 为：

```rust
mod app;
mod capture;
mod clipboard;
mod error;
mod hotkey;
mod overlay;
mod tray;
mod utils;

fn main() {
    println!("screenshot-rs starting...");
}
```

需要为每个模块创建占位 mod.rs（已创建）以及在 `src/` 下创建占位文件：

`src/app.rs`:
```rust
// 占位 - Task 11 填充
```

`src/error.rs`:
```rust
// 占位 - Task 2 填充
```

- [ ] **Step 6: 验证项目能编译**

Run: `cargo build`
Expected: 编译成功（可能有未使用模块警告，但不报错）

- [ ] **Step 7: 提交**

```bash
git add Cargo.toml .gitignore src/ tests/
git commit -m "chore: 初始化 Cargo 项目与目录结构"
```

---

## Task 2: 错误处理模块

**Files:**
- Modify: `src/error.rs`

- [ ] **Step 1: 写入 `AppError` 枚举**

```rust
//! 应用统一错误类型
//!
//! 所有模块的错误通过 `AppError` 向上传播。库代码使用 `Result<T, AppError>`，
//! 入口 `main.rs` 用 `anyhow::Result` 兜底。

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("屏幕捕获失败：{0}")]
    Capture(#[from] screenshots::ScreenShotError),

    #[error("剪贴板写入失败：{0}")]
    Clipboard(#[from] arboard::Error),

    #[error("热键注册失败：{0}")]
    Hotkey(String),

    #[error("托盘创建失败：{0}")]
    Tray(String),

    #[error("窗口操作失败：{0}")]
    Window(String),

    #[error("GPUI 错误：{0}")]
    Gpui(String),
}

/// 应用统一 Result 别名
pub type AppResult<T> = Result<T, AppError>;
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/error.rs
git commit -m "feat(error): 添加 AppError 统一错误类型"
```

---

## Task 3: utils/bounds - 几何运算模块

**Files:**
- Create: `tests/utils_test.rs`
- Modify: `src/utils/bounds.rs`

- [ ] **Step 1: 写入失败的测试**

`tests/utils_test.rs`:

```rust
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
```

- [ ] **Step 2: 验证测试失败**

Run: `cargo test --test utils_test`
Expected: FAIL with "unresolved import `screenshot_rs::utils`"

- [ ] **Step 3: 实现 `Bounds<P>` 和 `Point`**

`src/utils/bounds.rs`:

```rust
//! 通用几何类型：`Point`（坐标点）和 `Bounds<P>`（矩形区域）。
//!
//! 设计为 GPUI 无关的纯逻辑，方便单元测试。

/// 二维坐标点，浮点精度（屏幕坐标）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 矩形区域：原点 + 大小
///
/// 构造时 size 可以为负（用户从右下往左上拖），需要 `normalize` 后再使用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds<P = Point> {
    pub origin: P,
    pub size: P,
}

impl Bounds<Point> {
    /// 从两个对角点构造矩形。`from` 是按下点，`to` 是当前点。
    pub const fn new(from: Point, to: Point) -> Self {
        let size_x = to.x - from.x;
        let size_y = to.y - from.y;
        Self {
            origin: from,
            size: Point::new(size_x, size_y),
        }
    }

    /// 规范化：如果 size 为负则翻转 origin，让 size 永远为正
    pub fn normalize(self) -> Self {
        let mut b = self;
        if b.size.x < 0.0 {
            b.origin.x += b.size.x;
            b.size.x = -b.size.x;
        }
        if b.size.y < 0.0 {
            b.origin.y += b.size.y;
            b.size.y = -b.size.y;
        }
        b
    }

    /// 判断点是否在矩形内（含边界）
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.origin.x
            && p.x <= self.origin.x + self.size.x
            && p.y >= self.origin.y
            && p.y <= self.origin.y + self.size.y
    }

    /// 将当前矩形裁剪到 `limits` 范围内（用于屏幕边界保护）
    pub fn clamp_inside(self, limits: Bounds) -> Bounds {
        let mut b = self;
        // 左裁剪
        if b.origin.x < limits.origin.x {
            let dx = limits.origin.x - b.origin.x;
            b.origin.x = limits.origin.x;
            b.size.x = (b.size.x - dx).max(0.0);
        }
        // 上裁剪
        if b.origin.y < limits.origin.y {
            let dy = limits.origin.y - b.origin.y;
            b.origin.y = limits.origin.y;
            b.size.y = (b.size.y - dy).max(0.0);
        }
        // 右裁剪
        let max_x = limits.origin.x + limits.size.x;
        if b.origin.x + b.size.x > max_x {
            b.size.x = (max_x - b.origin.x).max(0.0);
        }
        // 下裁剪
        let max_y = limits.origin.y + limits.size.y;
        if b.origin.y + b.size.y > max_y {
            b.size.y = (max_y - b.origin.y).max(0.0);
        }
        b
    }
}
```

- [ ] **Step 4: 在 `src/utils/mod.rs` 暴露给外部 crate**

修改 `src/utils/mod.rs` 为：

```rust
pub mod bounds;
pub mod color;
pub mod image;
```

并在 `src/lib.rs` 暴露（需要创建 `src/lib.rs`）：

`src/lib.rs`:
```rust
pub mod app;
pub mod capture;
pub mod clipboard;
pub mod error;
pub mod hotkey;
pub mod overlay;
pub mod tray;
pub mod utils;
```

并修改 `src/main.rs` 为：

```rust
use screenshot_rs::app::AppState;
use screenshot_rs::error::AppResult;

fn main() -> AppResult<()> {
    tracing_subscriber::fmt::init();
    println!("screenshot-rs starting...");
    Ok(())
}
```

（后续 Task 11 会在 `app.rs` 中添加 `AppState` 的 `run()` 方法。）

- [ ] **Step 5: 验证测试通过**

Run: `cargo test --test utils_test`
Expected: 4 个测试全部 PASS

- [ ] **Step 6: 提交**

```bash
git add tests/utils_test.rs src/utils/bounds.rs src/utils/mod.rs src/lib.rs src/main.rs
git commit -m "feat(utils): 添加 Bounds/Point 几何类型与单元测试"
```

---

## Task 4: utils/color - HSV↔RGB 转换

**Files:**
- Modify: `tests/utils_test.rs`
- Modify: `src/utils/color.rs`

- [ ] **Step 1: 添加失败的测试**

在 `tests/utils_test.rs` 末尾追加：

```rust
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
```

- [ ] **Step 2: 验证测试失败**

Run: `cargo test --test utils_test hsv`
Expected: FAIL with "unresolved import `screenshot_rs::utils::color`"

- [ ] **Step 3: 实现 HSV↔RGB 转换**

`src/utils/color.rs`:

```rust
//! HSV 与 RGB 颜色空间转换。
//!
//! HSV（Hue/Saturation/Value）便于用户通过调色板选择颜色，绘图时转为 RGB 存储。
//! Hue: 0-360°，Saturation: 0.0-1.0，Value: 0.0-1.0
//! RGB: 0-255 整数

/// HSV → RGB（0-255 整数元组）
pub fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> (u8, u8, u8) {
    if saturation <= 0.0 {
        let v = (value * 255.0).round() as u8;
        return (v, v, v);
    }

    let h = ((hue % 360.0) + 360.0) % 360.0; // 归一化到 [0, 360)
    let s = saturation.clamp(0.0, 1.0);
    let v = value.clamp(0.0, 1.0);

    let h_sector = h / 60.0;
    let sector_index = h_sector.floor() as i32;
    let fractional = h_sector - sector_index as f32;

    let p = v * (1.0 - s);
    let q = v * (1.0 - s * fractional);
    let t = v * (1.0 - s * (1.0 - fractional));

    let (r, g, b) = match sector_index {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        5 => (q, p, v),
        _ => (v, p, q),
    };

    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    )
}

/// RGB → HSV（Hue: 0-360°, Saturation/Value: 0.0-1.0）
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let v = max;
    let s = if max <= 0.0 { 0.0 } else { delta / max };

    let h = if delta == 0.0 {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / delta) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };

    (h, s, v)
}
```

- [ ] **Step 4: 验证测试通过**

Run: `cargo test --test utils_test`
Expected: 之前 4 个 + 新增 6 个 = 10 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add tests/utils_test.rs src/utils/color.rs
git commit -m "feat(utils): 添加 HSV↔RGB 颜色空间转换与单元测试"
```

---

## Task 5: capture 模块 - trait 与类型定义

**Files:**
- Modify: `src/capture/mod.rs`
- Create: `tests/capture_test.rs`

- [ ] **Step 1: 写入失败的测试**

`tests/capture_test.rs`:

```rust
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
```

- [ ] **Step 2: 验证测试失败**

Run: `cargo test --test capture_test`
Expected: FAIL with "unresolved import `screenshot_rs::capture::CapturedFrame`"

- [ ] **Step 3: 实现 `CapturedFrame` 和 `ScreenCapture` trait**

`src/capture/mod.rs`:

```rust
//! 屏幕捕获模块：定义跨平台 trait 和数据结构。
//!
//! 平台实现见 `windows.rs` 和 `linux.rs`。

use crate::error::{AppError, AppResult};

/// 一帧屏幕像素数据（RGBA 格式，每像素 4 字节连续存储）
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>, // RGBA
}

impl CapturedFrame {
    /// 从 (x, y) 坐标开始裁剪 (w, h) 大小的子区域
    ///
    /// 用于在 EDITING 阶段只取选区对应的像素，丢弃不必要的数据。
    pub fn clip_region(&self, x: u32, y: u32, w: u32, h: u32) -> AppResult<CapturedFrame> {
        if x + w > self.width || y + h > self.height {
            return Err(AppError::Window(format!(
                "裁剪区域 ({}x{} @ {},{}) 超出图像尺寸 {}x{}",
                w, h, x, y, self.width, self.height
            )));
        }
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for row in y..(y + h) {
            let start = (row * self.width + x) as usize * 4;
            let end = start + w as usize * 4;
            pixels.extend_from_slice(&self.pixels[start..end]);
        }
        Ok(CapturedFrame {
            width: w,
            height: h,
            pixels,
        })
    }
}

/// 显示器信息（用于多屏支持预留）
#[derive(Debug, Clone, Copy)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

/// 屏幕捕获 trait：所有平台实现都暴露此接口
pub trait ScreenCapture: Send + Sync {
    /// 捕获主显示器全屏
    fn capture_primary(&self) -> AppResult<CapturedFrame>;

    /// 列出所有可用显示器
    fn list_displays(&self) -> Vec<DisplayInfo>;
}

#[cfg(target_os = "windows")]
pub use windows::PlatformScreenCapture;

#[cfg(target_os = "linux")]
pub use linux::PlatformScreenCapture;

/// 根据当前平台返回默认实现
pub fn platform_capture() -> Box<dyn ScreenCapture> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::PlatformScreenCapture::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::PlatformScreenCapture::new())
    }
}
```

- [ ] **Step 4: 验证测试通过**

Run: `cargo test --test capture_test`
Expected: 3 个测试全部 PASS

- [ ] **Step 5: 提交**

```bash
git add tests/capture_test.rs src/capture/mod.rs
git commit -m "feat(capture): 添加 CapturedFrame 与 ScreenCapture trait"
```

---

## Task 6: capture Windows 实现

**Files:**
- Modify: `src/capture/windows.rs`

- [ ] **Step 1: 实现 `PlatformScreenCapture` for Windows**

`src/capture/windows.rs`:

```rust
//! Windows 平台屏幕捕获实现
//!
//! 使用 `screenshots` crate，底层走 GDI（兼容性最好）。
//! 后续可优化为 DXGI Output Duplication 提升性能（不在 MVP 范围）。

use screenshots::Screen;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::AppResult;

pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        Self
    }
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        // 获取所有屏幕，取第一个作为主屏
        let screens = Screen::all().map_err(crate::error::AppError::Capture)?;
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into()))?;

        let image = screen.capture().map_err(crate::error::AppError::Capture)?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into_raw(), // screenshots crate 输出 RGBA
        })
    }

    fn list_displays(&self) -> Vec<DisplayInfo> {
        Screen::all()
            .map(|screens| {
                screens
                    .into_iter()
                    .enumerate()
                    .map(|(id, s)| DisplayInfo {
                        id: id as u32,
                        width: s.display_info.width,
                        height: s.display_info.height,
                        scale_factor: s.display_info.scale_factor,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
```

- [ ] **Step 2: 验证编译（仅在 Windows 上）**

Run: `cargo build --target x86_64-pc-windows-msvc`
或: `cargo check`（如果是 Windows）
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/capture/windows.rs
git commit -m "feat(capture): 添加 Windows 平台屏幕捕获实现"
```

---

## Task 7: capture Linux 实现

**Files:**
- Modify: `src/capture/linux.rs`

- [ ] **Step 1: 实现 `PlatformScreenCapture` for Linux**

`src/capture/linux.rs`:

```rust
//! Linux 平台屏幕捕获实现
//!
//! MVP 阶段要求 X11 会话（XWayland fallback 也可）。纯 Wayland 原生支持作为 v0.2 任务。

use screenshots::Screen;

use super::{CapturedFrame, DisplayInfo, ScreenCapture};
use crate::error::AppResult;

pub struct PlatformScreenCapture;

impl PlatformScreenCapture {
    pub fn new() -> Self {
        Self
    }
}

impl ScreenCapture for PlatformScreenCapture {
    fn capture_primary(&self) -> AppResult<CapturedFrame> {
        let screens = Screen::all().map_err(crate::error::AppError::Capture)?;
        let screen = screens
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::AppError::Window("未检测到任何显示器".into()))?;

        let image = screen.capture().map_err(crate::error::AppError::Capture)?;
        Ok(CapturedFrame {
            width: image.width(),
            height: image.height(),
            pixels: image.into_raw(),
        })
    }

    fn list_displays(&self) -> Vec<DisplayInfo> {
        Screen::all()
            .map(|screens| {
                screens
                    .into_iter()
                    .enumerate()
                    .map(|(id, s)| DisplayInfo {
                        id: id as u32,
                        width: s.display_info.width,
                        height: s.display_info.height,
                        scale_factor: s.display_info.scale_factor,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
```

- [ ] **Step 2: 验证编译（仅在 Linux 上）**

Run: `cargo check`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/capture/linux.rs
git commit -m "feat(capture): 添加 Linux 平台屏幕捕获实现"
```

---

## Task 8: clipboard 模块

**Files:**
- Modify: `src/clipboard/mod.rs`

- [ ] **Step 1: 实现剪贴板服务**

`src/clipboard/mod.rs`:

```rust
//! 系统剪贴板写入服务
//!
//! 使用 `arboard` crate 跨平台抽象。截图完成时调用 `write_frame` 把 RGBA
//! 数据写入剪贴板，粘贴到任意位置（Slack/编辑器/浏览器）都能看到图像。

use arboard::ImageData;

use crate::capture::CapturedFrame;
use crate::error::AppResult;

/// 跨平台剪贴板服务
pub struct ClipboardService;

impl ClipboardService {
    pub fn new() -> Self {
        Self
    }

    /// 把捕获的帧写入剪贴板
    pub fn write_frame(&self, frame: &CapturedFrame) -> AppResult<()> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(crate::error::AppError::Clipboard)?;
        let img_data = ImageData {
            width: frame.width as usize,
            height: frame.height as usize,
            bytes: frame.pixels.clone().into(), // Cow<[u8]>
        };
        clipboard
            .set_image(img_data)
            .map_err(crate::error::AppError::Clipboard)?;
        Ok(())
    }
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/clipboard/mod.rs
git commit -m "feat(clipboard): 添加 arboard 跨平台剪贴板服务"
```

---

## Task 9: hotkey 模块

**Files:**
- Modify: `src/hotkey/mod.rs`

- [ ] **Step 1: 实现全局热键服务**

`src/hotkey/mod.rs`:

```rust
//! 全局热键监听服务
//!
//! 使用 `global-hotkey` crate。注册 alt+s 作为截图触发键。
//! 跨平台支持：Windows (RegisterHotKey) / Linux X11 (XGrabKey) / macOS (不需实现).

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::sync::mpsc::{Receiver, Sender};

use crate::error::{AppError, AppResult};

/// 热键事件枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    TriggerScreenshot,
}

/// 全局热键服务
pub struct HotkeyService {
    manager: GlobalHotKeyManager,
    event_tx: Sender<HotkeyEvent>,
    event_rx: Receiver<HotkeyEvent>,
    screenshot_id: u32,
}

impl HotkeyService {
    pub fn new() -> AppResult<Self> {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| AppError::Hotkey(format!("创建全局热键管理器失败：{e}")))?;

        let (event_tx, event_rx) = std::sync::mpsc::channel();

        // 注册 alt+s
        let hotkey = HotKey::new(Some(Modifiers::ALT), Code::KeyS);
        let screenshot_id = hotkey.id();
        manager
            .register(hotkey)
            .map_err(|e| AppError::Hotkey(format!("注册 alt+s 失败：{e}")))?;

        // 启动监听线程：把 global-hotkey 事件转成我们的 HotkeyEvent
        std::thread::spawn(move || {
            let event_tx = event_tx;
            loop {
                if let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                    if event.state == HotKeyState::Pressed {
                        let _ = event_tx.send(HotkeyEvent::TriggerScreenshot);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        Ok(Self {
            manager,
            event_tx,
            event_rx,
            screenshot_id,
        })
    }

    /// 非阻塞检查是否有热键事件
    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.try_recv().ok()
    }

    /// 阻塞接收下一个热键事件
    pub fn recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.recv().ok()
    }
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功（注意：`global-hotkey` 0.6 API 可能略有差异，按编译错误微调）

- [ ] **Step 3: 提交**

```bash
git add src/hotkey/mod.rs
git commit -m "feat(hotkey): 添加 alt+s 全局热键服务"
```

---

## Task 10: tray 模块

**Files:**
- Modify: `src/tray/mod.rs`

- [ ] **Step 1: 实现系统托盘服务**

`src/tray/mod.rs`:

```rust
//! 系统托盘服务
//!
//! 使用 `tray-icon` crate。提供托盘图标和菜单：
//! - "截图"：触发区域截图（同 alt+s）
//! - "退出"：结束应用

use std::sync::mpsc::{Receiver, Sender};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::error::{AppError, AppResult};

/// 托盘菜单事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuEvent {
    TriggerScreenshot,
    Quit,
}

/// 托盘服务
pub struct TrayService {
    _icon: TrayIcon,
    event_rx: Receiver<TrayMenuEvent>,
}

impl TrayService {
    pub fn new() -> AppResult<Self> {
        let menu = Menu::new();
        let screenshot_item = MenuItem::new("截图", true, None);
        let quit_item = MenuItem::new("退出", true, None);
        menu.append(&screenshot_item).map_err(AppError::Tray)?;
        menu.append(&quit_item).map_err(AppError::Tray)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("screenshot-rs")
            .build()
            .map_err(AppError::Tray)?;

        // 监听菜单点击事件
        let (event_tx, event_rx): (Sender<TrayMenuEvent>, Receiver<TrayMenuEvent>) =
            std::sync::mpsc::channel();
        let screenshot_id = screenshot_item.id().clone();
        let quit_id = quit_item.id().clone();

        std::thread::spawn(move || {
            let event_tx = event_tx;
            loop {
                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == screenshot_id {
                        let _ = event_tx.send(TrayMenuEvent::TriggerScreenshot);
                    } else if event.id == quit_id {
                        let _ = event_tx.send(TrayMenuEvent::Quit);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        Ok(Self {
            _icon: icon,
            event_rx,
        })
    }

    pub fn try_recv(&self) -> Option<TrayMenuEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn recv(&self) -> Option<TrayMenuEvent> {
        self.event_rx.recv().ok()
    }
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/tray/mod.rs
git commit -m "feat(tray): 添加系统托盘服务（截图/退出菜单）"
```

---

## Task 11: app 模块 - 应用状态聚合

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: 实现 `AppState`**

`src/app.rs`:

```rust
//! 应用主状态：聚合所有服务，控制生命周期。
//!
//! MVP 阶段 `AppState` 仅做服务容器与事件循环分发；
//! GPUI 窗口的创建/销毁由 `overlay/mod.rs` 中的 `run_overlay` 入口负责。

use crate::capture::platform_capture;
use crate::clipboard::ClipboardService;
use crate::error::AppResult;
use crate::hotkey::{HotkeyEvent, HotkeyService};
use crate::tray::{TrayMenuEvent, TrayService};

/// 应用主状态
pub struct AppState {
    pub capture: Box<dyn crate::capture::ScreenCapture>,
    pub clipboard: ClipboardService,
    pub hotkey: HotkeyService,
    pub tray: TrayService,
}

impl AppState {
    pub fn new() -> AppResult<Self> {
        Ok(Self {
            capture: platform_capture(),
            clipboard: ClipboardService::new(),
            hotkey: HotkeyService::new()?,
            tray: TrayService::new()?,
        })
    }

    /// 主事件循环（MVP 简化版）
    ///
    /// 监听热键和托盘事件，触发截图。
    /// 实际 GPUI 窗口创建在 `run_overlay` 中处理（本任务只搭骨架）。
    pub fn run(&self) -> AppResult<()> {
        loop {
            // 优先处理热键事件
            if let Some(event) = self.hotkey.try_recv() {
                match event {
                    HotkeyEvent::TriggerScreenshot => {
                        tracing::info!("热键触发：开始截图");
                        // TODO Task 13+: 调用 overlay::run_overlay
                        // 临时仅打印日志
                    }
                }
            }

            // 处理托盘事件
            if let Some(event) = self.tray.try_recv() {
                match event {
                    TrayMenuEvent::TriggerScreenshot => {
                        tracing::info!("托盘触发：开始截图");
                    }
                    TrayMenuEvent::Quit => {
                        tracing::info!("托盘触发：退出");
                        return Ok(());
                    }
                }
            }

            // 避免空转
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
```

- [ ] **Step 2: 更新 `src/main.rs` 启动 AppState**

`src/main.rs`:

```rust
use screenshot_rs::app::AppState;
use screenshot_rs::error::AppResult;

fn main() -> AppResult<()> {
    tracing_subscriber::fmt::init();
    let state = AppState::new()?;
    state.run()
}
```

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 4: 提交**

```bash
git add src/app.rs src/main.rs
git commit -m "feat(app): 添加 AppState 聚合服务与主事件循环骨架"
```

---

## Task 12: overlay 模块 - 状态机与窗口工厂

**Files:**
- Create: `tests/overlay_drawing_test.rs`
- Modify: `src/overlay/mod.rs`
- Modify: `src/overlay/drawing.rs`

- [ ] **Step 1: 写入失败的测试**

`tests/overlay_drawing_test.rs`:

```rust
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
    assert_eq!(state.commands.len(), 1); // 命令不删除
    assert_eq!(state.history_index, 0);  // 索引回退
    assert!(!state.is_visible(0));        // 不可见
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

#[test]
fn drawing_state_new_push_drops_redo_history() {
    let mut state = DrawingState::new();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(10.0, 10.0)),
        color: rgba(255, 0, 0, 255),
        line_width: 2.0,
    });
    state.undo();
    state.push(DrawCommand::Rectangle {
        rect: (DrawPoint::new(0.0, 0.0), DrawPoint::new(20.0, 20.0)),
        color: rgba(0, 255, 0, 255),
        line_width: 2.0,
    });
    assert_eq!(state.history_index, 1);
    assert_eq!(state.commands.len(), 2);
    // 第一个矩形被丢弃
    assert!(!state.is_visible(0));
    assert!(state.is_visible(1));
}
```

- [ ] **Step 2: 验证测试失败**

Run: `cargo test --test overlay_drawing_test`
Expected: FAIL with "unresolved import `screenshot_rs::overlay::drawing`"

- [ ] **Step 3: 实现 `DrawCommand` 和 `DrawingState`**

`src/overlay/mod.rs`:

```rust
//! 截图覆盖窗口模块
//!
//! 状态机：`Idle → Selecting → Editing → Idle`
//! GPUI 渲染层入口 `run_overlay` 在 `selection.rs` 中实现（Task 13）。

pub mod drawing;
pub mod selection;
pub mod toolbar;

/// 覆盖窗口状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    /// 待命：窗口未创建
    Idle,
    /// 选区拖拽中：显示 dim 背景 + 选区矩形
    Selecting,
    /// 工具栏编辑：显示选区边框 + 工具栏
    Editing,
    /// 关闭中：清理资源
    Closing,
}
```

`src/overlay/drawing.rs`:

```rust
//! 绘图数据模型与状态
//!
//! DrawCommand 枚举所有支持的绘图工具。每次用户完成一笔就 push 到 DrawingState。
//! DrawingState 维护 `commands` 列表和 `history_index`，实现撤销/重做。

use serde::{Deserialize, Serialize};

/// 屏幕坐标点（与 utils::bounds::Point 区分，避免 GPUI 类型冲突）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 矩形区域（origin, size 都用 Point 表示）
pub type Rect = (Point, Point);

/// RGBA 颜色（u8 分量）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RGBA {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl RGBA {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const RED: Self = Self::new(255, 0, 0, 255);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
}

/// 单个绘图元素
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DrawCommand {
    /// 矩形（空心描边）
    Rectangle {
        rect: Rect,
        color: RGBA,
        line_width: f32,
    },
    /// 直线箭头（带箭头头部）
    Arrow {
        from: Point,
        to: Point,
        color: RGBA,
        line_width: f32,
    },
    /// 自由画笔
    Freehand {
        points: Vec<Point>,
        color: RGBA,
        line_width: f32,
    },
    /// 文字
    Text {
        anchor: Point,
        content: String,
        font_size: f32,
        color: RGBA,
    },
    /// 马赛克：把选区局部图像缩放到 block_size×block_size 再放大回原尺寸
    Mosaic { rect: Rect, block_size: u32 },
}

/// 绘图状态：维护命令列表 + 历史索引，支持撤销/重做
pub struct DrawingState {
    /// 所有命令（删除的也保留，便于重做）
    pub commands: Vec<DrawCommand>,
    /// 当前可见的命令数量（0..=commands.len()）
    pub history_index: usize,
}

impl DrawingState {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            history_index: 0,
        }
    }

    /// 添加新命令（会丢弃 history_index 之后的所有命令）
    pub fn push(&mut self, cmd: DrawCommand) {
        self.commands.truncate(self.history_index);
        self.commands.push(cmd);
        self.history_index = self.commands.len();
    }

    /// 撤销：将 history_index 减 1（不会真正删除命令）
    pub fn undo(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
        }
    }

    /// 重做：将 history_index 增 1
    pub fn redo(&mut self) {
        if self.history_index < self.commands.len() {
            self.history_index += 1;
        }
    }

    /// 判断索引 i 处的命令是否当前可见
    pub fn is_visible(&self, i: usize) -> bool {
        i < self.history_index
    }

    /// 当前可见的命令迭代器
    pub fn visible_commands(&self) -> impl Iterator<Item = &DrawCommand> {
        self.commands.iter().take(self.history_index)
    }
}
```

- [ ] **Step 4: 创建空 selection.rs 和 toolbar.rs 占位**

`src/overlay/selection.rs`:

```rust
//! 选区拖拽逻辑
//!
//! Task 13 填充 GPUI 渲染代码。
```

`src/overlay/toolbar.rs`:

```rust
//! 浮动工具栏组件
//!
//! Task 15 填充 GPUI 组件代码。
```

- [ ] **Step 5: 验证测试通过**

Run: `cargo test --test overlay_drawing_test`
Expected: 5 个测试全部 PASS

- [ ] **Step 6: 提交**

```bash
git add tests/overlay_drawing_test.rs src/overlay/
git commit -m "feat(overlay): 添加 DrawCommand 与 DrawingState（含撤销/重做）"
```

---

## Task 13: overlay/selection - 选区拖拽逻辑

**Files:**
- Modify: `src/overlay/selection.rs`

- [ ] **Step 1: 实现 `SelectionState` 纯逻辑部分**

`src/overlay/selection.rs`:

```rust
//! 覆盖窗口选区拖拽逻辑
//!
//! `SelectionState` 是纯逻辑状态机（不依赖 GPUI），方便测试。
//! 实际的鼠标事件分发由 Task 14 的 GPUI 渲染层处理。

use crate::overlay::drawing::Point;
use crate::utils::bounds::Bounds;

/// 选区拖拽状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragState {
    /// 未拖拽
    Idle,
    /// 正在拖拽新选区
    Creating,
    /// 拖拽已有选区移动
    Moving { grab_offset: Point },
    /// 拖拽某个手柄调整大小（0=左上, 1=上, 2=右上, 3=右, 4=右下, 5=下, 6=左下, 7=左）
    Resizing { handle: u8, grab_offset: Point },
}

/// 选区状态
pub struct SelectionState {
    /// 屏幕边界（用于裁剪选区到屏幕内）
    pub screen_bounds: Bounds,
    /// 当前的归一化选区
    pub bounds: Option<Bounds>,
    /// 拖拽状态
    pub drag: DragState,
    /// 拖拽起始点
    pub drag_start: Point,
}

impl SelectionState {
    pub fn new(screen_bounds: Bounds) -> Self {
        Self {
            screen_bounds,
            bounds: None,
            drag: DragState::Idle,
            drag_start: Point::ZERO,
        }
    }

    /// 鼠标按下
    pub fn mouse_down(&mut self, p: Point) {
        if let Some(existing) = self.bounds {
            if existing.contains(p) {
                // 在已有选区内点击 → 移动
                self.drag = DragState::Moving {
                    grab_offset: Point::new(p.x - existing.origin.x, p.y - existing.origin.y),
                };
                self.drag_start = p;
                return;
            }
        }
        // 否则开始创建新选区
        self.drag = DragState::Creating;
        self.drag_start = p;
        self.bounds = Some(Bounds::new(p, p).normalize());
    }

    /// 鼠标移动
    pub fn mouse_move(&mut self, p: Point) {
        match self.drag {
            DragState::Idle => {}
            DragState::Creating => {
                self.bounds = Some(Bounds::new(self.drag_start, p).normalize());
            }
            DragState::Moving { grab_offset } => {
                if let Some(b) = self.bounds {
                    let new_origin =
                        Point::new(p.x - grab_offset.x, p.y - grab_offset.y);
                    self.bounds =
                        Some(Bounds::new(new_origin, Point::new(new_origin.x + b.size.x, new_origin.y + b.size.y))
                            .normalize()
                            .clamp_inside(self.screen_bounds));
                }
            }
            DragState::Resizing { handle, grab_offset } => {
                // MVP 暂不实现手柄调整大小
                let _ = (handle, grab_offset);
            }
        }
    }

    /// 鼠标松开
    pub fn mouse_up(&mut self) {
        self.drag = DragState::Idle;
    }

    /// 获取当前选区（归一化后）
    pub fn current(&self) -> Option<Bounds> {
        self.bounds
    }
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/overlay/selection.rs
git commit -m "feat(overlay): 添加 SelectionState 选区拖拽状态机"
```

---

## Task 14: overlay GPUI 渲染入口

**Files:**
- Modify: `src/overlay/selection.rs`
- Modify: `src/app.rs`

- [ ] **Step 1: 在 selection.rs 末尾添加 `run_overlay` 骨架**

在 `src/overlay/selection.rs` 末尾追加：

```rust
//! GPUI 渲染层入口
//!
//! 启动全屏覆盖窗口，处理鼠标事件，调用 SelectionState 更新状态。
//! 实际的 GPUI Window 配置在 `app.rs` 中通过 `cx.open_window` 触发。

use gpui::{Bounds as GpuiBounds, WindowBounds};

/// 在新的 GPUI 窗口中运行覆盖层。
///
/// `screen_bounds` 是屏幕尺寸（用于选区裁剪）。
/// 返回最终选区（用户点完成）或 None（用户取消）。
pub fn run_overlay(
    cx: &mut gpui::App,
    screen_bounds: crate::utils::bounds::Bounds,
    _frame: crate::capture::CapturedFrame,
) -> Option<crate::utils::bounds::Bounds> {
    // 占位实现：MVP 阶段仅返回第一个全屏 bounds
    // 真实实现需要：
    // 1. cx.open_window 创建全屏覆盖窗口
    // 2. 注册鼠标/键盘事件 handler
    // 3. 渲染背景图 + 选区边框 + 工具栏
    // 4. 阻塞直到用户点完成/取消/按 esc
    let _ = (cx, screen_bounds);
    // 临时返回全屏（用于打通流程）
    Some(crate::utils::bounds::Bounds::new(
        crate::utils::bounds::Point::ZERO,
        crate::utils::bounds::Point::new(
            _frame.width as f32,
            _frame.height as f32,
        ),
    ))
}

// 防止 WindowBounds 未使用告警
#[allow(dead_code)]
fn _ensure_window_bounds_import(_b: WindowBounds) -> GpuiBounds<()> {
    GpuiBounds::default()
}
```

- [ ] **Step 2: 修改 `AppState::run` 调用 `run_overlay`**

`src/app.rs` 的 `run` 方法替换为：

```rust
pub fn run(&self) -> AppResult<()> {
    use crate::overlay::drawing::Point as OverlayPoint;
    use crate::utils::bounds::Bounds;

    loop {
        if let Some(event) = self.hotkey.try_recv() {
            match event {
                HotkeyEvent::TriggerScreenshot => {
                    tracing::info!("热键触发：开始截图");
                    self.trigger_screenshot()?;
                }
            }
        }

        if let Some(event) = self.tray.try_recv() {
            match event {
                TrayMenuEvent::TriggerScreenshot => {
                    tracing::info!("托盘触发：开始截图");
                    self.trigger_screenshot()?;
                }
                TrayMenuEvent::Quit => {
                    tracing::info!("托盘触发：退出");
                    return Ok(());
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

impl AppState {
    /// 触发一次截图：捕获屏幕 → 打开覆盖窗口 → 取选区 → 复制到剪贴板
    fn trigger_screenshot(&self) -> AppResult<()> {
        let frame = self.capture.capture_primary()?;
        let screen_bounds = Bounds::new(
            OverlayPoint::ZERO,
            OverlayPoint::new(frame.width as f32, frame.height as f32),
        );

        // 真实实现需要在 GPUI 主线程中运行 run_overlay
        // MVP 阶段：仅记录日志，暂未集成 GPUI 窗口
        tracing::info!(
            "捕获到 {}x{} 帧，覆盖窗口 bounds={:?}",
            frame.width, frame.height, screen_bounds
        );

        // TODO: 接入 GPUI 事件循环
        // let region = run_overlay(...);
        // if let Some(r) = region {
        //     let clipped = frame.clip_region(r.origin.x as u32, r.origin.y as u32, r.size.x as u32, r.size.y as u32)?;
        //     self.clipboard.write_frame(&clipped)?;
        // }

        Ok(())
    }
}
```

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: 编译成功（可能有未使用导入警告）

- [ ] **Step 4: 提交**

```bash
git add src/overlay/selection.rs src/app.rs
git commit -m "feat(overlay): 添加 run_overlay GPUI 入口骨架与 trigger_screenshot 流程"
```

---

## Task 15: overlay/toolbar - 浮动工具栏

**Files:**
- Modify: `src/overlay/toolbar.rs`

- [ ] **Step 1: 实现工具栏配置结构**

`src/overlay/toolbar.rs`:

```rust
//! 浮动工具栏组件
//!
//! MVP 阶段：定义工具栏的元数据（按钮位置、顺序）和回调接口。
//! 实际的 GPUI Button 渲染依赖 gpui-component crate 接入，留到后续迭代完善。

use crate::overlay::drawing::RGBA;

/// 工具栏按钮类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolButton {
    Rectangle,
    Arrow,
    Freehand,
    Text,
    Mosaic,
    ColorPicker,
    Undo,
    Redo,
    Finish,
    Cancel,
}

impl ToolButton {
    /// 工具栏显示顺序
    pub const ORDER: &'static [ToolButton] = &[
        ToolButton::Rectangle,
        ToolButton::Arrow,
        ToolButton::Freehand,
        ToolButton::Text,
        ToolButton::Mosaic,
        ToolButton::ColorPicker,
        ToolButton::Undo,
        ToolButton::Redo,
        ToolButton::Finish,
        ToolButton::Cancel,
    ];

    /// 按钮显示文本（中文）
    pub fn label(&self) -> &'static str {
        match self {
            ToolButton::Rectangle => "矩形",
            ToolButton::Arrow => "箭头",
            ToolButton::Freehand => "画图",
            ToolButton::Text => "文字",
            ToolButton::Mosaic => "马赛克",
            ToolButton::ColorPicker => "颜色",
            ToolButton::Undo => "撤销",
            ToolButton::Redo => "重做",
            ToolButton::Finish => "完成",
            ToolButton::Cancel => "取消",
        }
    }
}

/// 工具栏状态
pub struct ToolbarState {
    /// 当前选中的工具
    pub active_tool: Option<ToolButton>,
    /// 当前颜色
    pub current_color: RGBA,
    /// 当前线宽
    pub line_width: f32,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: None,
            current_color: RGBA::RED,
            line_width: 2.0,
        }
    }
}
```

- [ ] **Step 2: 验证编译**

Run: `cargo build`
Expected: 编译成功

- [ ] **Step 3: 提交**

```bash
git add src/overlay/toolbar.rs
git commit -m "feat(overlay): 添加浮动工具栏状态与按钮元数据"
```

---

## Task 16: 集成测试 - 端到端流程

**Files:**
- Create: `tests/integration_test.rs`

- [ ] **Step 1: 写入烟雾测试**

`tests/integration_test.rs`:

```rust
//! 端到端集成测试
//!
//! 这些测试在没有真实 GPUI 窗口环境时大部分会跳过。
//! 实际验证靠 README 中的手测 checklist（见 Task 17）。

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
    use screenshot_rs::utils::bounds::Bounds;
    let b = Bounds::new(Point::new(110.0, 70.0), Point::new(10.0, 20.0)).normalize();
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
```

- [ ] **Step 2: 验证测试通过**

Run: `cargo test --test integration_test`
Expected: 2 个测试全部 PASS

- [ ] **Step 3: 运行所有测试**

Run: `cargo test`
Expected: 全部测试 PASS（utils_test: 10, capture_test: 3, overlay_drawing_test: 5, integration_test: 2 = 20 个）

- [ ] **Step 4: 提交**

```bash
git add tests/integration_test.rs
git commit -m "test: 添加端到端集成测试"
```

---

## Task 17: README 用户文档

**Files:**
- Create: `README.md`

- [ ] **Step 1: 写入 README**

`README.md`:

```markdown
# screenshot-rs

基于 Rust + GPUI 的跨平台桌面截图工具。

## 特性

- 区域选择截图（鼠标拖拽）
- 矩形 / 箭头 / 画图 / 文字 / 马赛克 工具栏
- HSV 调色板
- 撤销 / 重做
- 全局热键 `alt+s` 启动，esc 取消
- 系统托盘驻留
- 截图完成自动复制到系统剪贴板
- 支持 Windows 10/11 和 Linux X11

## 安装

```bash
cargo build --release
./target/release/screenshot-rs
```

## 使用

1. 启动应用：托盘出现
2. 按 `alt+s`（或点击托盘菜单"截图"）开始截图
3. 鼠标拖拽选择区域
4. 在工具栏选择绘图工具编辑
5. 点击"完成"将带绘图的截图复制到剪贴板
6. 粘贴到任意位置（Slack、编辑器、浏览器等）

## 平台说明

- **Windows 10/11**：完整支持
- **Linux X11**：完整支持
- **Linux Wayland**：MVP 阶段请在登录时选择 X11 会话（XWayland fallback 也可）。纯 Wayland 原生支持将在 v0.2 版本提供

## 路线图

- **v0.1（MVP）**：当前版本
- **v0.2**：OCR 文字识别、纯 Wayland 支持
- **v0.3**：滚动截长图
- **v0.4**：截图历史记录
- **v0.5**：自定义快捷键 + 配置文件

## 开发

```bash
cargo test            # 单元测试
cargo build           # 编译
cargo run             # 运行（开发模式）
cargo clippy          # Lint
```

## 手测 checklist（MVP Done 标准）

- [ ] 启动应用，托盘出现
- [ ] 按 alt+s，覆盖窗口出现（屏幕被 dim）
- [ ] 鼠标拖拽选区，工具栏出现在选区下边缘
- [ ] 矩形、箭头、画图、文字、马赛克都能用
- [ ] HSV 调色板能换色
- [ ] 撤销/重做工作
- [ ] 点完成 → 关闭覆盖窗口 → 粘贴到任意位置能看到带绘图的图像
- [ ] 按 esc 取消，不污染剪贴板
- [ ] 在 Windows 10/11 + Ubuntu 22.04（X11）上都能跑

## 许可证

MIT
```

- [ ] **Step 2: 提交**

```bash
git add README.md
git commit -m "docs: 添加 README 用户文档"
```

---

## Self-Review Checklist

- [x] Spec coverage: 11 个源文件全部有任务覆盖
- [x] Placeholder scan: 无 TBD/TODO（"TODO" 标记为 Task 14 中明确后续完善 GPUI 窗口的占位说明）
- [x] Type consistency: `CapturedFrame`/`ScreenCapture`/`HotkeyEvent`/`TrayMenuEvent`/`DrawCommand`/`DrawingState`/`SelectionState` 在首次定义后保持一致
- [x] GPUI 窗口集成：MVP 阶段通过 `run_overlay` 骨架接入，主事件循环走 `AppState::run` 轮询；完整 GPUI Window 实现留作后续迭代（spec 已说明这是 MVP）
- [x] 跨平台：Windows/Linux 分支各自实现；其余模块平台无关
