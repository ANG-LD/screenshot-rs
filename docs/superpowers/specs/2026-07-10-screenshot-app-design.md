# 截图应用 (screenshot-rs) 设计文档

| 项目 | 值 |
|------|---|
| 日期 | 2026-07-10 |
| 类型 | 全新项目设计 |
| 目标平台 | Windows 10/11 + Linux (X11/Wayland) |
| UI 框架 | GPUI (Zed) + gpui-component |
| MVP 范围 | 区域截图 + 基础绘图 + 全局热键 + 托盘 + 剪贴板 |

---

## 1. 项目目标

基于 Rust + GPUI 构建一个跨平台桌面截图应用，提供与 QQ/微信截图工具相近的体验：

- **MVP 必含**：区域选择截图、矩形/箭头/画图/文字/马赛克、HSV 调色板、撤销/重做、alt+s 热键、esc 取消、系统托盘驻留、截图后自动复制到剪贴板
- **MVP 不含**（后续迭代）：OCR 文字识别、滚动截长图、多显示器独立选区、截图历史记录、自定义快捷键、配置文件

---

## 2. 整体架构

```
┌──────────────────────────────────────────────────┐
│                  应用主进程 (main)                 │
│                                                    │
│   ┌────────────┐   ┌────────────┐   ┌──────────┐ │
│   │ 热键监听    │   │ 托盘服务    │   │ 截图引擎 │ │
│   │(global-    │   │(tray-icon) │   │(screenshots│
│   │ hotkey)    │   │            │   │  crate)  │ │
│   └────────────┘   └────────────┘   └──────────┘ │
│         │                  │              │        │
│         └──────────────────┼──────────────┘        │
│                            ▼                       │
│              ┌────────────────────────┐            │
│              │   GPUI 事件循环         │           │
│              │  - 主窗口（托盘管理）    │           │
│              │  - 覆盖层窗口（截图）    │           │
│              └────────────────────────┘            │
└──────────────────────────────────────────────────┘
```

**两个 GPUI 窗口**：

1. **托盘宿主窗口**（不可见）：常驻，负责维持应用生命周期、注册全局热键、显示托盘图标
2. **截图覆盖窗口**（按需创建）：按下 alt+s 时全屏创建，捕获鼠标事件、绘制选区、浮动工具栏。截图结束后销毁

这种"两个窗口"的设计解耦了常驻服务与临时交互，避免互相阻塞。

---

## 3. 截图核心流程（状态机）

```
   ┌──────────┐  alt+s / 托盘点击  ┌──────────────┐
   │   IDLE   │ ───────────────► │  SELECTING   │
   │ (待命)   │                   │  (选区拖拽中) │
   └──────────┘                   └──────────────┘
        ▲                              │
        │                              │ 释放鼠标
        │  esc / 关闭按钮              ▼
        │                         ┌──────────────┐
        │                         │  EDITING     │
        │                         │ (工具栏编辑)  │
        │                         └──────────────┘
        │                              │
        │  完成按钮                     │ 取消按钮
        └──────────────────────────────┘
                  esc
```

**各阶段行为**：

- **IDLE**：应用处于托盘驻留状态，无可见窗口。监听全局热键和托盘事件。
- **SELECTING**：
  - 覆盖窗口全屏（覆盖整个主显示器）
  - 背景：捕获到的全屏图像 + 50% 黑色遮罩
  - 鼠标按下→拖动→松开确定矩形
  - 实时显示选区尺寸提示（如 `640 × 480`），位置跟随光标
  - 按 esc 取消，回到 IDLE
- **EDITING**：
  - 隐藏遮罩，只在选区周围显示边框 + 8 个调整手柄
  - 浮动工具栏钉在选区下边缘
  - 用户可绘制矩形/箭头/自由画/文字/马赛克
  - 可撤销/重做
  - 点"完成"→ 渲染最终图像到 RGBA 缓冲区 → 写入剪贴板 → 关闭覆盖窗口 → 回到 IDLE
  - 点"取消"或按 esc → 关闭覆盖窗口，不污染剪贴板 → 回到 IDLE

**状态机实现位置**：`src/overlay/mod.rs` 中的 `OverlayState` 枚举，`OverlayWindow` 在每次事件循环 tick 中根据当前状态分发行为。

---

## 4. 工具栏与绘图层

### 4.1 工具栏布局

工具栏钉在选区下边缘（如果选区贴近屏幕底部则自动翻转到上方），水平排列：

```
┌────────────────────────────────────────────────────────┐
│  [▭ 矩形] [↗ 箭头] [✎ 画图] [T 文字] [▦ 马赛克]        │
│  [● 颜色] [↶ 撤销] [↷ 重做] [✓ 完成] [✕ 取消]         │
└────────────────────────────────────────────────────────┘
        ▲ 选中状态高亮    ▲ HSV 调色板弹窗
```

按钮使用 `gpui-component` 的 `Button` 组件，样式与图标通过 `gpui-component-assets` 提供。

### 4.2 绘图数据模型

```rust
/// 单个绘图元素，由工具栏按下「完成」前的所有笔画构成
enum DrawCommand {
    Rectangle { rect: Bounds, color: Rgba, line_width: f32 },
    Arrow     { from: Point, to: Point, color: Rgba, line_width: f32 },
    Freehand  { points: Vec<Point>, color: Rgba, line_width: f32 },
    Text      { anchor: Point, content: String, font_size: f32, color: Rgba },
    Mosaic    { rect: Bounds, block_size: u32 },  // 马赛克用原图像素块平均色
}
```

### 4.3 渲染层

覆盖窗口是个 GPUI `Canvas`，按下顺序绘制：

1. 背景：捕获到的全屏图像（仅 SELECTING 时加 50% dim）
2. 选区矩形边框（1px 高对比色描边 + 半透明阴影）
3. 所有 `DrawCommand`（按时间顺序）
4. 工具栏（用 `gpui-component` 的 Button 组件）

**撤销/重做**：`DrawCommand` 用 `Vec` 存储，配合 `history_index` 索引。撤销时 `history_index -= 1`；新增命令时丢弃 `history_index` 之后的所有命令。

**马赛克实现**：用 `image::imageops` 把选区局部图像缩放到 `block_size × block_size`（默认 12px），再放大回原尺寸（最近邻插值），产生像素化效果。不依赖 GPU shader，CPU 即可，足够实时。

**HSV 调色板**：点击颜色按钮弹出一个小型浮层，包含：
- 色相滑块（0-360°）
- 饱和度/明度方形选择区
- 当前颜色预览
- 6 个常用预设色快捷按钮（红/橙/黄/绿/蓝/紫）

实现用纯 GPUI 绘制（HSV→RGB 转换函数在 `utils/color.rs`），不引入额外依赖。

---

## 5. 跨平台抽象层

为了让 Windows 和 Linux 都能跑，同时保持代码整洁，引入一个平台抽象层：

```rust
// src/platform/mod.rs
pub mod screenshot;
pub mod clipboard;
pub mod hotkey;
pub mod tray;

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod linux;
```

### 5.1 屏幕捕获 trait

```rust
// src/platform/screenshot.rs
pub trait ScreenCapture: Send + Sync {
    /// 捕获整个主显示器，返回 RGBA 字节 + 尺寸
    fn capture_primary(&self) -> Result<CapturedFrame>;
    /// 枚举所有显示器（用于多屏，未来扩展）
    fn list_displays(&self) -> Vec<DisplayInfo>;
}

pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,  // RGBA, 每像素 4 字节
}
```

**实现选择**：
- **Windows**：`screenshots` crate 包装（底层 DXGI Output Duplication + GDI fallback）
- **Linux X11**：`screenshots` crate 包装（XShm + XRender）
- **Linux Wayland**：`screenshots` crate 对 Wayland 支持有限。MVP 阶段要求用户在登录时选择 X11 会话（XWayland fallback）；纯 Wayland 会话支持作为 v0.2 任务，通过 `zbus` + `org.gnome.Mutter.ScreenCast` 或 KDE Plasma 的 `org.kde.KWin.ScreenShot2` DBus 接口实现

### 5.2 其他 trait

```rust
// src/platform/clipboard.rs
pub trait Clipboard: Send + Sync {
    fn write_image(&self, frame: &CapturedFrame) -> Result<()>;
}

// src/platform/hotkey.rs
pub trait HotkeyManager: Send + Sync {
    fn register(&mut self, id: HotkeyId, combo: KeyCombo) -> Result<()>;
    fn unregister(&mut self, id: HotkeyId) -> Result<()>;
    // 通过 channel 发送触发事件
}

// src/platform/tray.rs
pub trait TrayService: Send + Sync {
    fn create(&mut self, menu: TrayMenu) -> Result<TrayHandle>;
    fn on_menu_event(&self) -> Receiver<TrayMenuEvent>;
}
```

所有平台实现统一走 `screenshots` crate（捕获）和 `arboard`/`tray-icon`/`global-hotkey`（其他模块），抽象层的存在主要是为了：
1. 提供统一的错误类型（`AppError`）
2. 隔离 `screenshots`/`arboard` 等 crate 升级带来的 API 变更
3. 单元测试时可以注入 mock 实现

---

## 6. Cargo 依赖

```toml
[dependencies]
# GPUI（Zed 团队，指定 commit）
gpui = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe" }
gpui_platform = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe", features = ["font-kit", "x11", "wayland", "runtime_shaders"] }
gpui_macros  = { git = "https://github.com/zed-industries/zed", rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe" }
gpui-component = { git = "https://github.com/longbridge/gpui-component", branch = "main" }

# 屏幕捕获（跨平台 crate）
screenshots = "0.6"

# 系统集成
global-hotkey = "0.6"   # 全局热键
tray-icon     = "0.11"  # 系统托盘
arboard       = "3"     # 系统剪贴板
image         = "0.25"  # 图像处理（马赛克等）

# 错误处理与日志
anyhow        = "1"
thiserror     = "1"
tracing       = "0.1"
tracing-subscriber = "0.3"

# 异步运行时（GPUI 自带，但跨平台系统集成 crate 需要）
tokio         = { version = "1", features = ["sync", "rt-multi-thread", "macros"] }

# 序列化（用于未来配置/历史记录）
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

**依赖选型说明**：

| Crate | 选择理由 | 替代方案 |
|-------|---------|---------|
| `screenshots` | 已支持 Windows GDI/DXGI + Linux X11 + macOS；社区维护 | 自实现（开发周期长） |
| `global-hotkey` | GPUI 不直接处理全局热键；该 crate 是事实标准 | `device_query`（仅输入查询，不支持注册） |
| `tray-icon` | 跨平台托盘，纯 Rust 实现 | `ksni`（Linux only） |
| `arboard` | 跨平台剪贴板抽象，对图像支持好 | `x11rb` + `clipboard-win`（分平台） |
| `gpui-component` | 提供现成 Button/Slider 等组件，节省 UI 开发时间 | 自实现（GPUI 原生 API 偏底层） |

---

## 7. 项目目录结构

```
screenshot-rs/
├── Cargo.toml
├── README.md
├── docs/
│   └── superpowers/
│       └── specs/
│           └── 2026-07-10-screenshot-app-design.md   # 本文档
├── src/
│   ├── main.rs                    # 入口，启动 GPUI 应用
│   ├── app.rs                     # AppState：托盘、热键注册、应用生命周期
│   ├── error.rs                   # AppError 枚举
│   │
│   ├── capture/                   # 截图模块
│   │   ├── mod.rs                 # CapturedFrame、ScreenCapture trait
│   │   ├── windows.rs             # Windows 实现（screenshots crate 包装）
│   │   └── linux.rs               # Linux 实现（X11/Wayland 检测）
│   │
│   ├── overlay/                   # 截图覆盖窗口（GPUI）
│   │   ├── mod.rs                 # OverlayWindow 工厂函数 + OverlayState
│   │   ├── selection.rs           # 选区拖拽逻辑
│   │   ├── toolbar.rs             # 浮动工具栏
│   │   └── drawing.rs             # DrawCommand + 渲染
│   │
│   ├── tray/                      # 系统托盘
│   │   └── mod.rs                 # 托盘菜单（退出、截图）
│   │
│   ├── hotkey/                    # 全局热键
│   │   └── mod.rs                 # alt+s 注册
│   │
│   ├── clipboard/                 # 剪贴板写入
│   │   └── mod.rs                 # arboard 包装
│   │
│   └── utils/                     # 工具
│       ├── image.rs               # RGBA ↔ image::RgbaImage 转换
│       ├── bounds.rs              # Bounds<Point> 几何辅助
│       └── color.rs               # HSV ↔ RGB 转换
│
└── tests/                         # 集成测试
    └── integration_test.rs
```

**单文件代码量预估**：

| 文件 | 预估行数 | 说明 |
|------|---------|------|
| `main.rs` | 50 | 入口 |
| `app.rs` | 200 | AppState + 生命周期 |
| `capture/mod.rs` | 80 | trait + 类型 |
| `capture/windows.rs` | 100 | screenshots 包装 |
| `capture/linux.rs` | 150 | X11/Wayland 区分 |
| `overlay/mod.rs` | 300 | 状态机 + 窗口工厂 |
| `overlay/selection.rs` | 200 | 鼠标事件 + 矩形计算 |
| `overlay/toolbar.rs` | 250 | 工具栏组件 |
| `overlay/drawing.rs` | 500 | DrawCommand + 渲染 |
| `tray/mod.rs` | 150 | 托盘服务 |
| `hotkey/mod.rs` | 100 | 热键注册 |
| `clipboard/mod.rs` | 80 | 剪贴板写入 |
| `utils/*` | 200（合计） | 工具函数 |

---

## 8. 错误处理与测试

### 8.1 错误处理

```rust
// src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("屏幕捕获失败：{0}")]
    Capture(#[from] screenshots::ScreenShotError),

    #[error("剪贴板写入失败：{0}")]
    Clipboard(#[from] arboard::Error),

    #[error("热键注册失败：{0}")]
    Hotkey(String),

    #[error("托盘创建失败：{0}")]
    Tray(#[from] tray_icon::Error),

    #[error("窗口操作失败：{0}")]
    Window(String),

    #[error("GPUI 错误：{0}")]
    Gpui(String),
}
```

**关键原则**：截图失败（权限不足、显示器断开）不能让应用崩溃，必须通过托盘菜单或 toast 通知用户。所有 `Result` 在 `main.rs` 用 `?` 向上传播，由 GPUI 的事件循环兜底记录日志。

### 8.2 测试策略

| 层级 | 方法 | 工具 |
|------|------|------|
| 单元 | `utils/bounds.rs` 几何运算、颜色转换 | `cargo test` |
| 单元 | `overlay/drawing.rs` DrawCommand 序列化/命令合并 | `cargo test` |
| 集成 | 启动应用 → 模拟 alt+s → 验证剪贴板内容 | 手测脚本 `scripts/smoke_test.sh` |
| 手测 | 完整流程：热键→选区→绘图→完成→粘贴到 Slack | 每个 PR 必做 |

GPUI 渲染层难以自动测试（依赖真实窗口），MVP 阶段用手测验证。自动化测试聚焦在 `utils` 和 `overlay/drawing` 的纯逻辑部分。

### 8.3 MVP 验收标准

1. ✅ 启动应用，托盘出现
2. ✅ 按 alt+s，覆盖窗口出现（屏幕被 dim）
3. ✅ 鼠标拖拽选区，工具栏出现在选区下边缘
4. ✅ 矩形、箭头、画图、文字、马赛克都能用
5. ✅ HSV 调色板能换色
6. ✅ 撤销/重做工作
7. ✅ 点完成 → 关闭覆盖窗口 → 粘贴到任意位置能看到带绘图的图像
8. ✅ 按 esc 取消，不污染剪贴板
9. ✅ 在 Windows 10/11 + Ubuntu 22.04（X11）上都能跑

---

## 9. 已知风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| Wayland 下 `screenshots` crate 支持有限 | 纯 Wayland 会话用户无法使用 | MVP 文档明确要求登录时选择 X11 会话（XWayland fallback）；纯 Wayland 原生支持作为 v0.2 任务 |
| GPUI 是 git 依赖，API 变化频繁 | 升级 Zed 后编译失败 | 固定 `rev = "1d217ee39d381ac101b7cf49d3d22451ac1093fe"`；用 `Cargo.lock` 锁定 |
| `gpui-component` 是 git 依赖，main 分支可能不稳定 | 工具栏组件行为异常 | 按用户需求使用 `branch = "main"`；CI 必须能跑通，每次 PR 验证编译。如发现 main 分支 API 频繁变更导致维护成本高，后续切换为固定 `rev` |
| GPUI 在 Windows 上需要 WebView2 | Windows 7/8 用户无法使用 | 仅支持 Windows 10+，README 说明 |
| `global-hotkey` 在某些 Linux 桌面（GNOME Wayland）下受限 | 热点失效 | 提供托盘菜单作为备用入口 |

---

## 10. MVP 后续迭代（不在本文档范围）

- **v0.2**：OCR 文字识别（Windows: Windows.Media.Ocr；Linux: Tesseract via subprocess）
- **v0.3**：滚动截图（Chrome DevTools Protocol 或浏览器扩展 + DBus）
- **v0.4**：截图历史记录 + 简单 UI（点击托盘菜单查看最近 20 张）
- **v0.5**：自定义快捷键 + 配置文件（TOML 格式）

---

## 附录 A：peashot 项目参考要点

[peashot](https://github.com/nik-rev/peashot) 是纯 Rust 屏幕捕获库（不依赖 GPUI），只做截图不做编辑。本项目可借鉴的部分：

- ✅ `screenshots` crate 的集成方式（peashot 早期版本用过类似方案）
- ✅ 跨平台屏幕捕获的 trait 抽象思路
- ❌ 编辑/UI 部分 peashot 不涉及，需完全自研
- ❌ peashot 没有 OCR/滚动截图，不在本设计参考范围

## 附录 B：关键 Cargo 命令

```bash
# 编译
cargo build

# 运行
cargo run --release

# 检查
cargo clippy --all-targets

# 测试
cargo test

# 格式化
cargo fmt
```
