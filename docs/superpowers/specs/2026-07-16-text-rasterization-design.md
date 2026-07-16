# 文字标注 CPU 栅格化 (screenshot-rs v0.2) 设计文档

| 项目 | 值 |
|------|---|
| 日期 | 2026-07-16 |
| 类型 | 既有功能增量（修复 + 升级） |
| 关联模块 | `src/overlay/{drawing,commands,window,font,toolbar}.rs` + `Cargo.toml` + `assets/fonts/` |
| 关联提交 | 6fe125a feat(overlay): 工具栏 + 5 工具实时预览 + CPU 栅格化 + HSV 调色板 |
| 上游文档 | docs/superpowers/specs/2026-07-10-screenshot-app-design.md |

---

## 1. 背景与现状

`DrawCommand::Text` 在 overlay 渲染层（`render_text_command`，src/overlay/window.rs）由 GPUI div + text 子元素做**实时预览**，用户能看到文字、输入中文、IME 合成。但提交后：

```rust
// src/overlay/commands.rs:72-81 (v0.1 占位)
DrawCommand::Text { anchor, content: _, font_size, color } => {
    let a = translate(*anchor, region_origin_x, region_origin_y);
    let char_w = font_size * 0.6;
    let w = char_w * 4.0;
    let h = *font_size;
    let placeholder = RGBA::new(color.r, color.g, color.b, (color.a / 2).max(0x40));
    fill_rect_blend(frame, a.0, a.1, w, h, placeholder)?;
}
```

CPU 栅格化分支只画了一个**半透明色块**，最终截图中没有真文字。

注释明确写到 "v0.2 接 ab_glyph + Noto CJK"。

真实机测试结果（2026-07-16）：用户输入中文 → overlay 上显示正常 → Finish 后剪贴板上的截图里只有色块（用户报告"文字没保存在截图中"）。

---

## 2. 目标与非目标

### v0.2 目标

- 真正实现文字 CPU 光栅化：把每个字符的 alpha mask blend 到 `CapturedFrame` 像素
- 字段扩充：`DrawCommand::Text` 新增 `max_width: Option<f32>`、`weight: FontWeight`（背景色框见非目标）
- 工具栏增强：B（粗体切换）+ 字号下拉（16/20/24/32/48 物理像素）
- 完整覆盖区域内常见截图标注场景：中文 + 拉丁 + 单行 + 多行 + Normal/Bold
- 不破坏既有：Rectangle / Arrow / Freehand / Mosaic 命令、所有 overlay 预览、HiDPI 坐标、toolbar 拦截逻辑

### v0.2 不做

- 文字背景色框（用户上轮明确要求不要）
- 文本对齐 / justify / 垂直对齐
- 字符宽度缓存（每条 Text 命令独立处理）
- 多线程优化（`FontSystem` 单线程复用已通过 thread_local 拿到）
- 系统字体查询 / 用户自定义字体路径

---

## 3. 设计选择

### 字体来源：内嵌完整 Noto Sans SC OTF

- Regular + Bold 两份 OTF，~10 MB 合计
- 通过 `include_bytes!()` 编译期嵌入；用户不需要安装任何东西
- 许可：SIL OFL 1.1，可商用；分别从 github.com/notofonts/noto-cjk 下载

**为什么不子集化**：构建需 Python + fonttools；v0.2 范围外优体积（v0.3 评估）

### 字体光栅化库：在依赖图里已存在的 cosmic-text

项目依赖图里已经存在 `cosmic-text v0.19.0`（gpui 间接拉入），本次把它从 transitive 升到**直接**依赖（`Cargo.toml` 新增 1 行），二进制净增 0。

**为什么不引 ab_glyph**：再 +300KB，layout 自写 ~150 行；本次目标是让文字能保存到截图，重 layout 大头不划算

**为什么不引 cosmic-text 之外的库**：fontdue 不做 layout；glyph_brush 绑 GPU 路径；自己解 OTF 需要 3000+ 行

### Layout：cosmic-text 白嫖 UAX #14 折行

通过 `Buffer::set_size(&mut font_system, Some(max_width), None)` 让 layout_runs 自动按物理像素宽度拆 run。我们只关心输出 glyph 物理坐标。

### 性能：thread_local + Lazy 字节缓存

- `static REGULAR_BYTES/BOLD_BYTES: Lazy<Vec<u8>>` 全局缓存 OTF 字节（一次性 `to_vec`）
- `thread_local! FONT_SYSTEM/SWASH_CACHE: RefCell<Option<...>>` 线程内 lazy init 一次
- 冷启动 ~30-50ms（解析 OTF + 初始化 swash）；热路径每个 glyph ~50µs
- 100 字中文 Tool 调用 ~5ms 完成

---

## 4. 架构总览

### 改动文件

```
src/
├── overlay/
│   ├── font.rs          ← 新增（~120 行）
│   ├── drawing.rs       ← 字段扩充（DrawCommand::Text）+ 新增 FontWeight
│   ├── commands.rs      ← rasterize_text 重写（替代占位色块）
│   ├── toolbar.rs       ← ToolbarState 加 current_size / current_weight
│   └── window.rs        ← toolbar 加 B + 字号下拉；finalize 透传字段
└── Cargo.toml           ← cosmic-text 升直接依赖

assets/fonts/             ← 新增：
                             - NotoSansSC-Regular.otf  (~5 MB)
                             - NotoSansSC-Bold.otf     (~5 MB)
```

### 数据流

```
overlay 用户输入文字
   │
   │  InputState::PressEnter / Blur  →  OverlayView::finalize_text_input
   │     │
   │     │  push DrawCommand::Text { content, font_size = toolbar.current_size,
   │     │                           color = toolbar.current_color,
   │     │                           max_width = Some(selection.size.x),
   │     │                           weight = toolbar.current_weight }
   │
   ↓
GPUI render（预览）:
   render_text_command → div + text 子元素（用 GPUI 字体框）
   ※ overlay 预览只关心 anchor / content；font_size / weight 在 GPUI 层有等价表达
   │
   ↓
用户点 Finish → OverlayResult { commands }
   │
   ↓
app.rs::trigger_screenshot → apply_commands(&mut clipped, ...)
   │
   │  for DrawCommand::Text → rasterize_text
   │     │
   │     │  cosmic-text Buffer.set_text + set_size(Some(max))
   │     │  Buffer.shape_until_scroll
   │     │  for run in layout_runs() { for glyph in run.glyphs {
   │     │      mask = SwashCache.get_image(font_system, glyph.cache_key)
   │     │      blend_mask_to_frame(frame, mask, gx, gy, color)
   │     │  }}
   │
   ↓
clipboard.write_frame → 剪贴板最终带真文字
```

### 模块依赖图

```
window.rs ─┬─→ toolbar.rs ─→ drawing.rs (FontWeight, DrawCommand::Text)
           │                       ↑
           └─→ font.rs (FontWeight, helpers)  ┘
                        ↓
commands.rs ─→ font.rs (with_font_system, with_swash_cache)
           └─→ drawing.rs (DrawCommand)
```

---

## 5. 各模块改动详设

### 5.1 `Cargo.toml`

```toml
# 新增直接依赖（早已在依赖图中，净增 ~0 二进制）
cosmic-text = "0.19"
```

### 5.2 `src/overlay/drawing.rs`

**新增枚举**：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight { Normal, Bold }

impl FontWeight {
    pub fn font_bytes(self) -> &'static [u8] {
        match self {
            Self::Normal => include_bytes!("../../assets/fonts/NotoSansSC-Regular.otf"),
            Self::Bold   => include_bytes!("../../assets/fonts/NotoSansSC-Bold.otf"),
        }
    }
}
```

**`DrawCommand::Text` 字段扩充**：

```rust
Text {
    anchor: Point,
    content: String,
    font_size: f32,            // 物理像素（与 v0.1 含义一致）
    color: RGBA,
    max_width: Option<f32>,    // None = 单行；Some = 按此物理像素宽度折行
    weight: FontWeight,
},
```

注：背景色框经讨论明确不做，因此字段不引入 `background`；后续若要加，仅加一个 `Option<RGBA>` 字段即可，对外协议向前兼容。

### 5.3 `src/overlay/font.rs` (新增)

```rust
use std::cell::RefCell;
use cosmic_text::{FontSystem, SwashCache};
use crate::overlay::drawing::FontWeight;

static REGULAR_BYTES: once_cell::sync::Lazy<Vec<u8>> =
    once_cell::sync::Lazy::new(|| FontWeight::Normal.font_bytes().to_vec());
static BOLD_BYTES: once_cell::sync::Lazy<Vec<u8>> =
    once_cell::sync::Lazy::new(|| FontWeight::Bold.font_bytes().to_vec());

thread_local! {
    static FONT_SYSTEM: RefCell<Option<FontSystem>> = const { RefCell::new(None) };
    static SWASH_CACHE: RefCell<Option<SwashCache>> = const { RefCell::new(None) };
}

pub fn with_font_system<R>(f: impl FnOnce(&mut FontSystem) -> R) -> R {
    FONT_SYSTEM.with(|cell| {
        let mut b = cell.borrow_mut();
        if b.is_none() {
            let mut fs = FontSystem::new();
            fs.db_mut().load_font_data(REGULAR_BYTES.clone());
            fs.db_mut().load_font_data(BOLD_BYTES.clone());
            *b = Some(fs);
        }
        f(b.as_mut().unwrap())
    })
}

pub fn with_swash_cache<R>(f: impl FnOnce(&mut SwashCache) -> R) -> R {
    SWASH_CACHE.with(|cell| {
        let mut b = cell.borrow_mut();
        if b.is_none() {
            *b = Some(SwashCache::new());
        }
        f(b.as_mut().unwrap())
    })
}
```

### 5.4 `src/overlay/commands.rs`

**新增**：`rasterize_text`、`blend_mask_to_frame`、`blend_pixel_with_text_mask`

**`apply_commands` 现有 Text 分支**改为调 rasterize_text：

```rust
DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
    let a = translate(*anchor, region_origin_x, region_origin_y);
    rasterize_text(frame, a, content, font_size, *color, *max_width, *weight)?;
}
```

**`rasterize_text`** 主体逻辑（cosmic-text 0.19 API）：

```rust
pub fn rasterize_text(
    frame: &mut CapturedFrame,
    anchor: (f32, f32),
    content: &str,
    font_size: f32,
    color: RGBA,
    max_width: Option<f32>,
    weight: FontWeight,
) -> AppResult<()> {
    if content.is_empty() || font_size <= 0.0 {
        return Ok(());
    }
    with_font_system(|font_system| {
        let mut buffer = Buffer::new(font_system);
        let attrs = Attrs::new()
            .family(Family::Name("Noto Sans SC"))
            .style(if weight == FontWeight::Normal { Style::Normal } else { Style::Bold });
        buffer.set_text(font_system, content, attrs);
        buffer.set_size(font_system, max_width, None);
        buffer.shape_until_scroll(font_system, false);

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let gx = anchor.0 + glyph.x + run.line_offset;
                let gy = anchor.1 + run.line_y + glyph.y;
                let mask = with_swash_cache(|swash| {
                    swash.get_image(font_system, glyph.cache_key)
                }).ok_or_else(|| AppError::Window("glyph mask 缺失".into()))?;
                blend_mask_to_frame(frame, &mask, gx, gy, color);
            }
        }
    });
    Ok(())
}
```

### 5.5 `src/overlay/toolbar.rs`

**ToolbarState 字段加 2 项**：

```rust
pub struct ToolbarState {
    pub active_tool: Option<ToolButton>,
    pub current_color: RGBA,
    pub current_size: f32,            // 新增
    pub current_weight: FontWeight,   // 新增
}

pub const FONT_SIZES: &[f32] = &[16.0, 20.0, 24.0, 32.0, 48.0];
```

### 5.6 `src/overlay/window.rs`

**`finalize_text_input`** 用 toolbar 字段：

```rust
fn finalize_text_input(&mut self, state: &Entity<InputState>, cx: &mut Context<Self>) {
    let value = state.read(cx).value();
    if value.is_empty() {
        self.text_input = None;
        cx.notify();
        return;
    }
    let anchor = self.text_input_anchor;
    let content: String = String::from(value);
    self.drawing.push(DrawCommand::Text {
        anchor: DrawPoint::new(anchor.x, anchor.y),
        content,
        font_size: self.toolbar.current_size,
        color: self.toolbar.current_color,
        max_width: self.selection.current().map(|sel| sel.size.x),
        weight: self.toolbar.current_weight,
    });
    self.text_input = None;
    cx.notify();
}
```

**`render_toolbar`** 增加 B 按钮 + 字号下拉，夹在 Undo/Redo 之后、Finish 之前

**`render_text_command`** 用 `toolbar.current_size` + `toolbar.current_weight` 同步预览（GPUI 层把 weight 映射给 GPUI font weight）

---

## 6. 错误处理

| 场景 | 处理 |
|------|------|
| 字体文件编译期缺失 | `include_bytes!()` build error，明确提示文件路径 |
| `cosmic-text` 解析 OTF 失败 | 永不会发生（字节是有效 OTF） |
| `SwashCache::get_image` 返回 None | `rasterize_text` 返回 `AppError::Window` |
| anchor 落在 frame 外 | mask 全部越界 → `blend_mask_to_frame` 内 clamp + skip，不报错 |
| content 为空 | `rasterize_text` 早退 Ok |
| font_size ≤ 0 | `rasterize_text` 早退 Ok |
| `gpui-component::dropdown` API 不存在 | fallback 手撸 `Button + Popover`，对外行为不变 |

---

## 7. 测试覆盖

`src/overlay/commands.rs::tests` 新增 5 个：

| 测试名 | 断言点 |
|--------|--------|
| `rasterize_text_basic_writes_pixels` | 1 个 fixture 文本 + frame → 至少 N 个非零像素 |
| `rasterize_text_empty_content_noop` | content="" → frame 完全不变 + 不 panic |
| `rasterize_text_out_of_frame_clipping` | anchor 超出 frame 边界 → 不 panic + frame 不被破坏 |
| `rasterize_text_multi_line_wraps_when_max_width_small` | max_width=50 + 长内容 → layout_runs 数 > 1 |
| `rasterize_text_bold_differs_from_regular` | 同样字串 + Normal/Bold → 全帧至少 1 像素不同（间接验证） |

`src/overlay/font.rs::tests` 新增 1 个：

| 测试名 | 断言点 |
|--------|--------|
| `font_system_loads_both_weights_after_with_call` | `with_font_system(|fs| { fs.db().len() >= 2 })` |

`src/overlay/toolbar.rs::tests`（如有）新增：默认 `current_size = 18.0`、`current_weight = Normal`。

---

## 8. 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| 工具栏宽度膨胀超出 1440 物理像素 | HiDPI 屏上 Undo/Bold/字号下拉被裁 | 把工具栏贴在屏幕中线 + 测试多种屏幕尺寸 + 必要时缩字号 |
| `gpui-component::dropdown` API 在 v0.19 签名不一致 | 编译失败 | fallback 手撸 `Button + Popover` |
| cosmic-text 0.19 `SwashCache::get_image` 返回值含 `unwrap` 失败 | 编译失败 | 文档已经实测得 `Result` 或 `Option`，try-catch 即可 |
| 用户输入超长字符串导致栅格化耗时长 | 提交时 UI 卡顿几百 ms | 不在 v0.2 限制；cosmic-text 是按字符增量 layout，自然分块 |
| Noto OTF 文件授权争议 | 法务 | 已确认 SIL OFL 1.1 可商用；写入 README + LICENSE NOTICE |
| cosmic-text 升级到 0.20 破坏 `Buffer::set_size` 签名 | 未来阻塞升级 | 固定到 `0.19` 不主动升；用 `=` 锁版本 |

---

## 9. 验收标准（DoD）

- ✅ `cargo check` 0 警告
- ✅ `cargo test --lib` 11 个原有测试 + 6 个新增 = 17 个全过
- ✅ 真机测试：选 Text 工具 → 选区内点 → 输入中文（含 IME 拼音合成）→ Enter → Finish → 剪贴板 → 粘贴到 IM / 图片查看器，能看到中文文字
- ✅ 真机测试：切到 Bold + 字号 32 → 截图中的文字明显变粗变大
- ✅ 真机测试：输入超长字符串 → 自动按选区宽度折多行（如果 max_width 起作用）
- ✅ 真机测试：undo/redo 文字命令后，状态机无混乱

---

## 10. 不写入此设计的项

- ab_glyph 引评估（在 v0.3）
- 字体子集化（v0.3 评估）
- 颜色 alpha 选择器（v0.3）
- 多显示器不同 scale 的字号校准（v0.4）
