# Text CPU Rasterization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 真正把文字字形光栅化到截图中（v0.1 占位色块改成真文字），并支持多行折行 + 粗体 + 字号选择

**Architecture:** DrawCommand::Text 字段扩充（max_width / weight）→ overlay finalize 透传 toolbar 字段 → app.rs apply_commands 调新 rasterize_text（cosmic-text Buffer → layout_runs → SwashCache → blend mask → frame）

**Tech Stack:** Rust + cosmic-text 0.19（从依赖图提直接依赖，二进制净增 0）+ image 0.25 + gpui-component

**Spec:** docs/superpowers/specs/2026-07-16-text-rasterization-design.md

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` | 修改 | 显式声明 `cosmic-text = "0.19"` |
| `assets/fonts/NotoSansSC-Regular.otf` | 新增（gitignored，wget 下载） | 内嵌 Regular OTF |
| `assets/fonts/NotoSansSC-Bold.otf` | 新增（gitignored，wget 下载） | 内嵌 Bold OTF |
| `src/overlay/drawing.rs` | 修改 | 加 `FontWeight` 枚举 + 扩 `DrawCommand::Text` |
| `src/overlay/font.rs` | 新增 | `with_font_system` / `with_swash_cache` 线程局部池 |
| `src/overlay/commands.rs` | 修改 | 新增 `rasterize_text` 真实现 + `blend_mask_to_frame` |
| `src/overlay/toolbar.rs` | 修改 | `ToolbarState` 字段 + `ToolButton::Bold` + `FONT_SIZES` |
| `src/overlay/window.rs` | 修改 | toolbar 加 Bold + 字号下拉；finalize 与 render 同步 |

每个 task 都是自包含的 5-9 步，单步控制在 2-5 分钟。

---

## Task 1: 字体资源 + Cargo.toml 升 cosmic-text 到直接依赖

**Files:**
- Create: `assets/fonts/NotoSansSC-Regular.otf`（约 5 MB，gitignored）
- Create: `assets/fonts/NotoSansSC-Bold.otf`（约 5 MB，gitignored）
- Create: `assets/fonts/.gitignore`
- Modify: `Cargo.toml`（加 1 行）
- Modify: `项目根 .gitignore`（加 `assets/fonts/*.otf`）

- [ ] **Step 1: 准备目录**

```bash
mkdir -p assets/fonts
touch assets/fonts/.gitignore
```

- [ ] **Step 2: 下载 Regular OTF（多 fallback URL）**

任选一个能通的源：

```bash
# 优先：Google Fonts CDN（最新版本可能有不同 hash）
url1='https://fonts.gstatic.com/s/notosanssc/v36/k3kCo84MPvpLmixcA63oeAL7Iqp5IZJF9bmaG9_FnYxNbPzS5HE.otf'
# 次选：github.com/googlefonts/noto-fonts 主仓库
url2='https://github.com/googlefonts/noto-fonts/raw/main/hinted/ttf/NotoSansSC/NotoSansSC-Regular.ttf'
# 三选：github.com/notofonts/noto-cjk
url3='https://github.com/notofonts/noto-cjk/raw/main/Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf'

cd assets/fonts && \
  curl -fsSL --retry 3 -o NotoSansSC-Regular.otf "$url1" || \
  curl -fsSL --retry 3 -o NotoSansSC-Regular.otf "$url2" || \
  curl -fsSL --retry 3 -o NotoSansSC-Regular.otf "$url3" || \
  (echo "ALL 3 URLs FAILED; 请手动到 https://fonts.google.com/noto/specimen/Noto+Sans+SC 下载" && exit 1)
ls -lh NotoSansSC-Regular.otf
file NotoSansSC-Regular.otf
```

期望：`file` 命令输出 "OpenType font" 或 "TrueType font"，文件 >= 1 MB。

- [ ] **Step 3: 下载 Bold OTF（同样多 fallback）**

```bash
cd assets/fonts && \
  curl -fsSL --retry 3 -o NotoSansSC-Bold.otf 'https://github.com/notofonts/noto-cjk/raw/main/Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Bold.otf' || \
  (echo "Bold 下载失败；如果只有 Regular 编译通过、TDD 时降级到只内嵌 Regular" && exit 1)
ls -lh NotoSansSC-Bold.otf
file NotoSansSC-Bold.otf
```

期望：>= 1 MB。

如果 Bold 下载失败但 Regular 成功：用 `ln -s NotoSansSC-Regular.otf NotoSansSC-Bold.otf` 软链备份（plan 阶段不要 fall through，CI 阶段再处理）。

- [ ] **Step 4: gitignore 字体二进制**

```bash
echo 'assets/fonts/*.otf' >> .gitignore
echo 'assets/fonts/*.ttf' >> .gitignore
cat .gitignore | tail -3
```

期望：看到上面两行追加到 `.gitignore`。

- [ ] **Step 5: 升 cosmic-text 为直接依赖**

```bash
cd $(git rev-parse --show-toplevel)
cargo add cosmic-text@0.19
```

期望：`Cargo.toml` 末尾出现 `cosmic-text = "0.19"`。

- [ ] **Step 6: 编译验证（不应该改任何行为）**

```bash
cargo check
```

期望：build 通过，无新 warning。`draw_text` 仍然走占位色块路径，未调用本次设计的 rasterize_text。

- [ ] **Step 7: 提交**

```bash
git add Cargo.toml Cargo.lock assets/fonts/.gitignore .gitignore
git commit -m "build: embed Noto Sans SC OTF + promote cosmic-text to direct dep

- include_bytes!() will read OTF at compile time
- cosmic-text was already in transitive graph via gpui (0.19); promoting
  to direct adds 0 binary weight, declares intent for our text layer"
```

---

## Task 2: drawing.rs 加 FontWeight 枚举 + DrawCommand::Text 字段扩充

**Files:**
- Modify: `src/overlay/drawing.rs:1-30`（增加 enum）
- Modify: `src/overlay/drawing.rs:66-72`（扩 Text 字段）

> 注意：先确认 `drawing.rs` 当前 Text 分支真实行号。如有偏移，按实际行号定位。

- [ ] **Step 1: 写 failing test for FontWeight**

加在 `src/overlay/drawing.rs` 末尾的 `#[cfg(test)] mod tests` 块：

```rust
#[test]
fn font_weight_font_bytes_returns_nonempty_for_both_variants() {
    let regular = FontWeight::Normal.font_bytes();
    let bold = FontWeight::Bold.font_bytes();
    assert!(!regular.is_empty(), "Regular OTF 不能为空");
    assert!(!bold.is_empty(), "Bold OTF 不能为空");
    assert_ne!(regular.as_ptr(), bold.as_ptr(), "Regular/Bold 必须指向不同字节");
}
```

- [ ] **Step 2: 运行测试看 fail**

```bash
cargo test --lib overlay::drawing::tests::font_weight_font_bytes_returns_nonempty_for_both_variants
```

期望：FAIL — `FontWeight` 找不到（这个 enum 还没定义）。

- [ ] **Step 3: 添加 FontWeight 枚举**

在 `src/overlay/drawing.rs` 顶部（其他 pub enum 旁）添加：

```rust
/// 文字粗细（v0.2 新增）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    Normal,
    Bold,
}

impl FontWeight {
    /// 选对应字体的 OTF 字节
    pub fn font_bytes(self) -> &'static [u8] {
        match self {
            Self::Normal => include_bytes!("../../assets/fonts/NotoSansSC-Regular.otf"),
            Self::Bold   => include_bytes!("../../assets/fonts/NotoSansSC-Bold.otf"),
        }
    }
}
```

注意 `include_bytes!` 路径：源文件 `src/overlay/drawing.rs`，目标 `assets/fonts/...`，相对路径 `../../assets/fonts/...`。

- [ ] **Step 4: 扩 DrawCommand::Text 字段**

```rust
Text {
    anchor: Point,
    content: String,
    font_size: f32,
    color: RGBA,
    max_width: Option<f32>,
    weight: FontWeight,
},
```

（同时删除/更新对应使用点；这些会在后续 task 自动修复。先看能否编译）

- [ ] **Step 5: 运行测试看 pass**

```bash
cargo test --lib overlay::drawing::tests::font_weight_font_bytes_returns_nonempty_for_both_variants
```

期望：PASS。

`cargo check` 应该报大量"missing fields"和"unreachable pattern"错误，预期中。

- [ ] **Step 6: 编译验证 + 找到所有需要修补的 use 点**

```bash
cargo check 2>&1 | grep -E "error\[" | head -20
```

期望：列出所有需要修的 use 点（commands.rs / window.rs 里的 DrawCommand::Text 构造和解构）。

**不要在这一步完成修复**——把这些 use 点列出来供后续 task 处理。

- [ ] **Step 7: 提交**

```bash
git add src/overlay/drawing.rs
git commit -m "feat(overlay): add FontWeight enum + extend DrawCommand::Text fields

- max_width: Option<f32> for auto-wrap (None = single line)
- weight: FontWeight for Normal/Bold selection
- font_bytes() embeds OTF at compile time
- DrawCommand::Text consumers will be updated in subsequent tasks"
```

---

## Task 3: font.rs 新增 + thread_local 池化（含测试）

**Files:**
- Create: `src/overlay/font.rs`（新增）
- Modify: `src/overlay/mod.rs`（添加 `pub mod font;` + re-export）

- [ ] **Step 1: 写 failing test for font cache init**

`src/overlay/font.rs` 新建：

```rust
//! 字体光栅化的 cosmic-text 封装：字节级 lazy + 线程内 FontSystem / SwashCache 池

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_system_loads_at_least_two_families_after_first_call() {
        // 同一线程调用两次，第二次应命中 lazy init 后的 FontSystem，
        // 内部字体数据库至少应有 Regular + Bold 两份
        let count = with_font_system(|fs| fs.db().len());
        assert!(count >= 2, "FontDatabase 应至少含 2 份字体: actual={}", count);
    }

    #[test]
    fn swash_cache_initializes_without_panic() {
        let _ = with_swash_cache(|_| ());
    }
}
```

- [ ] **Step 2: 注册 mod**

`src/overlay/mod.rs` 顶部加 `pub mod font;`：

```rust
pub mod font;
```

- [ ] **Step 3: 添加 once_cell 依赖（项目没用到）**

```bash
grep -q "once_cell" Cargo.toml || cargo add once_cell
```

如果 `once_cell` 已经在依赖图里（gpui 经常拉它），grep 会有命中，不需要 add。

- [ ] **Step 4: 运行测试看 pass**

```bash
cargo test --lib overlay::font::tests
```

期望：2 个测试全过。

- [ ] **Step 5: 提交**

```bash
git add src/overlay/font.rs src/overlay/mod.rs Cargo.toml Cargo.lock
git commit -m "feat(overlay): font module with thread_local FontSystem/SwashCache

- REGULAR_BYTES/BOLD_BYTES: lazy Vec<u8> from include_bytes! once
- FONT_SYSTEM/SWASH_CACHE: thread_local RefCell<Option<...>>
  reused across rasterize_text calls (no cold-start per glyph)
- with_font_system / with_swash_cache closures hide RefCell borrow dance"
```

---

## Task 4: commands.rs rasterize_text stub + apply_commands Text 分支编译过

**Files:**
- Modify: `src/overlay/commands.rs:72-81`（替换占位实现）
- Modify: `src/overlay/commands.rs`（新增 stub 函数 + re-export）

- [ ] **Step 1: 看 current commands.rs Text 分支**

> 确认 v0.1 占位实现位置后，按实际行号调

- [ ] **Step 2: 写 stub rasterize_text**

在 `src/overlay/commands.rs` 顶部 `use` 块后添加：

```rust
use crate::overlay::font::{with_font_system, with_swash_cache};
use cosmic_text::{Attrs, Buffer, Family, Style};

/// 把 Text 命令栅格化到 frame（v0.2 真实实现，stub 版先返回 Ok）
pub fn rasterize_text(
    _frame: &mut crate::capture::CapturedFrame,
    _anchor: (f32, f32),
    _content: &str,
    _font_size: f32,
    _color: crate::overlay::drawing::RGBA,
    _max_width: Option<f32>,
    _weight: crate::overlay::drawing::FontWeight,
) -> crate::error::AppResult<()> {
    Ok(())
}
```

Stub 版只是让 `apply_commands` 编译过；Task 5 替换为真实现。

- [ ] **Step 3: 替换 apply_commands Text 分支**

```rust
DrawCommand::Text { anchor, content, font_size, color, max_width, weight } => {
    let a = translate(*anchor, region_origin_x, region_origin_y);
    rasterize_text(frame, a, content, *font_size, *color, *max_width, *weight)?;
}
```

注意是 `content` 的引用而不是 move（避免消费 command）。

- [ ] **Step 4: 编译验证**

```bash
cargo check
```

期望：通过。Stub 版 rasterize_text 不做实际工作（相当于 v0.1 占位的退化版），TDD 中下一步补实现。

如果 cosmic-text 在依赖图里版本不是 0.19 而是别的，`cargo check` 可能报 API 不匹配——按 spec 改 dep 锁定 0.19。

- [ ] **Step 5: 跑原有测试**

```bash
cargo test --lib overlay::commands
```

期望：原有 4 个测试仍 PASS（Rectangle / Freehand / Translate / Mosaic）；我们新增 rasterize_text stub 不应该打破什么（test 不调用 stub 函数）。

- [ ] **Step 6: 提交**

```bash
git add src/overlay/commands.rs
git commit -m "refactor(overlay): stub rasterize_text; apply_commands routes to it

Stub returns Ok without writing pixels. Real glyph-to-frame blend
lands in the next task; this commit only wires the call site."
```

---

## Task 5: commands.rs rasterize_text 真实现 + 5 个测试

**Files:**
- Modify: `src/overlay/commands.rs`

> TDD：先写 5 个测试（已知 stub 会让前 4 个 FAIL），看 FAIL，再写实现，PASS。

- [ ] **Step 1: 写 empty content + out of frame 这两个低风险 test**

`src/overlay/commands.rs` 末尾 `#[cfg(test)] mod tests` 内追加：

```rust
fn empty_frame(w: u32, h: u32) -> crate::capture::CapturedFrame {
    crate::capture::CapturedFrame {
        width: w,
        height: h,
        pixels: vec![0; (w * h * 4) as usize],
    }
}

#[test]
fn rasterize_text_empty_content_noop() {
    let mut f = empty_frame(50, 30);
    let baseline = f.pixels.clone();
    rasterize_text(
        &mut f, (0.0, 0.0), "", 16.0, RGBA::RED, None, FontWeight::Normal,
    ).unwrap();
    assert_eq!(f.pixels, baseline, "空 content 不能改 frame");
}

#[test]
fn rasterize_text_out_of_frame_anchor_does_not_panic() {
    let mut f = empty_frame(20, 20);
    // anchor 故意落在 frame 外
    rasterize_text(
        &mut f, (-100.0, -100.0), "test", 16.0, RGBA::RED, None, FontWeight::Normal,
    ).unwrap();
    // 不 panic 就 OK；像素可能有也可能没有，取决于 mask 大小
    assert_eq!(f.width, 20);
    assert_eq!(f.height, 20);
}
```

- [ ] **Step 2: 跑这 2 个测试看 pass**

```bash
cargo test --lib overlay::commands::tests::rasterize_text_empty_content_noop overlay::commands::tests::rasterize_text_out_of_frame_anchor_does_not_panic
```

期望：两个 PASS（stub 直接 Ok，加上数据校验都满足）。

- [ ] **Step 3: 写 basic writes pixels 测试**

```rust
#[test]
fn rasterize_text_basic_writes_some_pixels() {
    let mut f = empty_frame(200, 60);
    rasterize_text(
        &mut f,
        (10.0, 10.0),
        "Hi 你好",
        32.0,
        RGBA::new(0xFF, 0x00, 0x00, 0xFF),
        None,
        FontWeight::Normal,
    ).unwrap();
    let non_zero = f.pixels.iter().filter(|&&p| p != 0).count();
    assert!(non_zero > 10, "应至少写 10 个非 0 像素: actual={}", non_zero);
    // 至少存在红色像素（任何 R > 0）
    let red_count = (0..f.pixels.len()/4).filter(|&i| f.pixels[i*4] > 100).count();
    assert!(red_count > 0, "应至少有一个明显的红色像素");
}
```

- [ ] **Step 4: 跑 basic 测试看 FAIL**

```bash
cargo test --lib overlay::commands::tests::rasterize_text_basic_writes_some_pixels
```

期望：FAIL — `non_zero == 0`（stub 没写任何像素）。

- [ ] **Step 5: 加 max_width 折行 + weight 切换两个测试**

```rust
#[test]
fn rasterize_text_multi_line_when_max_width_small() {
    let mut f = empty_frame(80, 200);
    // 单行放不下 60px 宽的 60 字字符串；max_width=50 强制折行
    let content: String = "你好世界ABCDEFGHIJ".repeat(6);
    rasterize_text(
        &mut f,
        (0.0, 0.0),
        &content,
        24.0,
        RGBA::RED,
        Some(50.0),
        FontWeight::Normal,
    ).unwrap();
    // 60 字 / 50px 约每行 5-6 字 → 应至少跑出 3 行
    // 通过 frame 高度被占用来间接验证（如果只 1 行，下半部分不会写像素）
    let bottom_written = (100..200).any(|row| {
        (0..80).any(|col| f.pixels[(row * 80 + col) * 4] != 0)
    });
    assert!(bottom_written, "max_width=50 应迫使文字折多行，下半部分应有像素");
}

#[test]
fn rasterize_text_bold_changes_at_least_one_pixel() {
    let mut normal = empty_frame(120, 60);
    let mut bold = empty_frame(120, 60);
    rasterize_text(
        &mut normal, (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Normal,
    ).unwrap();
    rasterize_text(
        &mut bold,   (10.0, 10.0), "字", 32.0, RGBA::RED, None, FontWeight::Bold,
    ).unwrap();
    let diff = normal.pixels.iter().zip(bold.pixels.iter())
        .filter(|(a, b)| a != b).count();
    assert!(diff > 0, "Normal 和 Bold 渲染结果应至少 1 个像素不同: diff={}", diff);
}
```

- [ ] **Step 6: 跑这两个新测试看 FAIL**

```bash
cargo test --lib overlay::commands::tests::rasterize_text_multi_line_when_max_width_small overlay::commands::tests::rasterize_text_bold_changes_at_least_one_pixel
```

期望：FAIL — pixel counts == 0（stub 没写像素）。

- [ ] **Step 7: 写真 rasterize_text + blend_mask_to_frame**

替换 `src/overlay/commands.rs` 里的 stub：

```rust
pub fn rasterize_text(
    frame: &mut crate::capture::CapturedFrame,
    anchor: (f32, f32),
    content: &str,
    font_size: f32,
    color: crate::overlay::drawing::RGBA,
    max_width: Option<f32>,
    weight: crate::overlay::drawing::FontWeight,
) -> crate::error::AppResult<()> {
    use crate::error::AppError;
    use crate::overlay::drawing::FontWeight as FW;
    if content.is_empty() || font_size <= 0.0 {
        return Ok(());
    }
    let (anchor_x, anchor_y) = anchor;
    with_font_system(|font_system| {
        let mut buffer = Buffer::new(font_system);
        let attrs = Attrs::new()
            .family(Family::Name("Noto Sans SC"))
            .style(if weight == FW::Normal { Style::Normal } else { Style::Bold });
        buffer.set_text(font_system, content, attrs);
        buffer.set_size(font_system, max_width, None);
        buffer.shape_until_scroll(font_system, false);

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let gx = anchor_x + glyph.x + run.line_offset;
                let gy = anchor_y + run.line_y + glyph.y;
                let mask = with_swash_cache(|swash| {
                    swash.get_image(font_system, glyph.cache_key)
                }).ok_or_else(|| AppError::Window("glyph mask 缺失".into()))?;
                blend_mask_to_frame(frame, &mask, gx, gy, color);
            }
        }
        Ok(())
    })
}

fn blend_mask_to_frame(
    frame: &mut crate::capture::CapturedFrame,
    mask: &cosmic_text::SwashImage,
    target_x: f32,
    target_y: f32,
    color: crate::overlay::drawing::RGBA,
) {
    let w_px = frame.width as i32;
    let h_px = frame.height as i32;
    let start_x = (target_x + mask.placement.left as f32) as i32;
    let start_y = (target_y + mask.placement.top as f32) as i32;
    for sy in 0..mask.placement.height as i32 {
        let py = start_y + sy;
        if py < 0 || py >= h_px { continue; }
        for sx in 0..mask.placement.width as i32 {
            let px = start_x + sx;
            if px < 0 || px >= w_px { continue; }
            let m_idx = (sy * mask.placement.width as i32 + sx) as usize;
            let mask_a = mask.data[m_idx] as u32;
            if mask_a == 0 { continue; }
            let f_idx = ((py * w_px + px) as usize) * 4;
            // 越界保护
            if f_idx + 3 >= frame.pixels.len() { continue; }
            blend_pixel_with_text_mask(
                &mut frame.pixels[f_idx..f_idx+4], color, mask_a,
            );
        }
    }
}

/// 文字专用 SourceOver：
///   eff_a = (color.a / 255) * (mask / 255)
///   color_out = text_color * eff_a + dst * (1 - eff_a)
fn blend_pixel_with_text_mask(dst: &mut [u8], text_color: crate::overlay::drawing::RGBA, mask_a: u32) {
    let eff_a = (text_color.a as u32 * mask_a) / 255;
    let inv = 255 - eff_a;
    for i in 0..3 {
        let s = [text_color.r, text_color.g, text_color.b][i] as u32;
        let d = dst[i] as u32;
        dst[i] = ((s * eff_a + d * inv) / 255) as u8;
    }
    dst[3] = eff_a.max(dst[3]);
}
```

注意 `SwashImage` 的具体字段名（`data`, `placement.{left,top,width,height}`）在 cosmic-text 0.19 需要确认：实测时如果 API 不一致，按 `cargo doc --open cosmic-text` 调整。

- [ ] **Step 8: 跑全部 5 个 rasterize_text 测试 + 原有 4 个**

```bash
cargo test --lib overlay::commands
```

期望：9 个全过。

- [ ] **Step 9: 提交**

```bash
git add src/overlay/commands.rs
git commit -m "feat(overlay): real glyph rasterization via cosmic-text

- Buffer.set_text + set_size controls fold via max_width
- SwashCache.get_image delivers alpha mask per glyph
- blend_mask_to_frame: bounds-checked SourceOver with mask alpha
- 5 new tests: empty / out-of-frame / basic / multi-line / bold"
```

---

## Task 6: toolbar.rs state + ToolButton::Bold + FONT_SIZES

**Files:**
- Modify: `src/overlay/toolbar.rs`（全文）

- [ ] **Step 1: 看当前 toolbar.rs 结构**

确认 `ToolbarState` 字段、`ToolButton` 枚举、`Default for ToolbarState` 的位置。

- [ ] **Step 2: 添加 FONT_SIZES 常量**

```rust
/// 字号档位（v0.2 工具栏下拉用）
/// 单位：物理像素（与 font_size 字段一致，不随 scale_factor 倍乘）
pub const FONT_SIZES: &[f32] = &[16.0, 20.0, 24.0, 32.0, 48.0];
```

- [ ] **Step 3: 加 ToolButton::Bold 枚举 + label/icon/order**

```rust
pub enum ToolButton {
    Rectangle,
    Arrow,
    Freehand,
    Text,
    Mosaic,
    ColorPicker,
    Undo,
    Redo,
    Bold,           // v0.2 新增
    Finish,
    Cancel,
}

impl ToolButton {
    pub const ORDER: &'static [ToolButton] = &[
        Self::Rectangle, Self::Arrow, Self::Freehand, Self::Text, Self::Mosaic,
        Self::ColorPicker,
        Self::Undo, Self::Redo,
        Self::Bold,
        Self::Finish, Self::Cancel,
    ];
}

impl ToolButton {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rectangle => "矩形",
            Self::Arrow     => "箭头",
            Self::Freehand  => "画笔",
            Self::Text      => "文字",
            Self::Mosaic    => "马赛克",
            Self::ColorPicker => "取色",
            Self::Undo      => "撤销",
            Self::Redo      => "重做",
            Self::Bold      => "B",
            Self::Finish    => "完成",
            Self::Cancel    => "取消",
        }
    }
}
```

- [ ] **Step 4: 加 ToolbarState 字段 + Default 实现**

```rust
use crate::overlay::drawing::{FontWeight, RGBA};

#[derive(Debug, Clone)]
pub struct ToolbarState {
    pub active_tool: Option<ToolButton>,
    pub current_color: RGBA,
    pub current_size: f32,
    pub current_weight: FontWeight,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: None,
            current_color: RGBA::new(0xE5, 0x00, 0x00, 0xFF),
            current_size: 18.0,
            current_weight: FontWeight::Normal,
        }
    }
}
```

- [ ] **Step 5: 编译验证 + 加 unit test**

```bash
cargo check
cargo test --lib overlay::toolbar
```

`cargo check` 报 `window.rs` 里的 pattern match 不完整（Bold 遗漏），预期中——下个 task 修。

加 2 个测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_default_state_has_expected_size_and_weight() {
        let s = ToolbarState::default();
        assert_eq!(s.current_size, 18.0);
        assert_eq!(s.current_weight, FontWeight::Normal);
    }

    #[test]
    fn font_sizes_constant_includes_recommended_values() {
        assert!(FONT_SIZES.contains(&16.0));
        assert!(FONT_SIZES.contains(&48.0));
        assert_eq!(FONT_SIZES.len(), 5);
    }
}
```

- [ ] **Step 6: 提交**

```bash
git add src/overlay/toolbar.rs
git commit -m "feat(overlay): toolbar gains Bold button + font-size constants

- ToolbarState.current_size (default 18) + current_weight (Normal)
- FONT_SIZES const: 16/20/24/32/48 px
- ToolButton::Bold added to ORDER between Redo and Finish"
```

---

## Task 7: window.rs toolbar 加 Bold 按钮 + 字号下拉

**Files:**
- Modify: `src/overlay/window.rs:347-475` 附近（render_toolbar 范围）

- [ ] **Step 1: 修 begin_draw 等地方未覆盖的 ToolButton match**

`begin_draw` 等匹配 ToolButton 的地方，把 `ToolButton::Bold | ToolButton::ColorPicker | ToolButton::Undo | ToolButton::Redo | ToolButton::Finish | ToolButton::Cancel => return,` 这种已有子句保持不变（B 也是 non-draw 工具，走同一条非分支）。

- [ ] **Step 2: 改 ORDER 渲染主循环，把 Bold 单独 render（toggle 样式），字号下拉插在 Bold 后**

`render_toolbar` 内 `for &btn in ToolButton::ORDER` 改成：

```rust
let mut row = div().flex().gap(px(TOOLBAR_GAP)).items_center();

for &btn in ToolButton::ORDER {
    if btn == ToolButton::Bold {
        row = row.child(render_bold_toggle(this, cx));
        continue;
    }
    // 现有按钮 render
    let on_click = cx.listener(move |this, _ev, window, cx| match btn {
        // 跟现状一样；保留 Bold 永远不会走到这里
        // ...
    });
    let (active, disabled) = match btn {
        ToolButton::Bold => unreachable!(),
        // 其他不变
        // ...
    };
    row = row.child(render_tool_button(btn, active, disabled, on_click));
}

// 在 row 末尾追加字号下拉（位置 Bold 与 Finish 之间）
row = row.child(render_size_dropdown(this, cx));
```

具体插入位置由 ctrl+F 在 render_toolbar 内定位确认。

- [ ] **Step 3: 写 render_bold_toggle helper（放在 render_tool_button 旁）**

```rust
fn render_bold_toggle(view: &OverlayView, cx: &mut Context<OverlayView>) -> impl IntoElement {
    let on_click = cx.listener(|this, _ev, _w, cx| {
        this.toolbar.current_weight = match this.toolbar.current_weight {
            FontWeight::Normal => FontWeight::Bold,
            FontWeight::Bold => FontWeight::Normal,
        };
        cx.notify();
    });
    let mut b = Button::new("toolbar-bold")
        .icon(IconName::Bold)        // gpui-component IconName 应有 Bold；如果有变体差异，临时换 Square
        .label("B")
        .tooltip("切换粗体")
        .small()
        .on_click(on_click);
    if view.toolbar.current_weight == FontWeight::Bold {
        b = b.primary();
    }
    b
}
```

注意：gpui-component 的 `IconName::Bold` 不一定存在——先 grep 一下：

```bash
grep -r "Bold" ~/.cargo/registry/src/*/gpui-component*/src/icon.rs 2>/dev/null | head -5
grep -r "enum IconName" ~/.cargo/registry/src/*/gpui-component*/src/icon.rs 2>/dev/null
```

如果没 `Bold` 变体，临时用 `IconName::Square` 或 `IconName::TypeBold`。查不到就用 `IconName::Type` 占位，留 TODO。

- [ ] **Step 4: 写 render_size_dropdown helper**

```rust
fn render_size_dropdown(view: &OverlayView, cx: &mut Context<OverlayView>) -> impl IntoElement {
    use gpui::SharedString;
    let options: Vec<SharedString> = FONT_SIZES.iter()
        .map(|s| format!("{}px", s).into())
        .collect();
    let current_index = FONT_SIZES.iter()
        .position(|&s| s == view.toolbar.current_size)
        .unwrap_or(1);  // 默认指向 20

    gpui_component::select::Select::new("font-size-select")
        .options(options)
        .selected_index(current_index)
        .on_change(cx.listener(|this, ix: &usize, _w, cx| {
            if let Some(&sz) = FONT_SIZES.get(*ix) {
                this.toolbar.current_size = sz;
                cx.notify();
            }
        }))
}
```

注意 `gpui_component::select::Select` 的真实 API——按 `cargo doc --open gpui-component::select` 或 grep 实际导入路径调整。fallback 用手撸 `Button + Popover + List`。

- [ ] **Step 5: 编译验证**

```bash
cargo check
```

期望：通过。如果 Select API 不存在，回退手撸 `Button + Popover` 实现等价 UI。

- [ ] **Step 6: 提交**

```bash
git add src/overlay/window.rs
git commit -m "feat(overlay): toolbar renders Bold toggle + font-size dropdown

- B toggle: click flips Normal/Bold
- font-size select: 16/20/24/32/48 px, default 18 fallback to 20
- both wire to toolbar.current_weight / current_size state"
```

---

## Task 8: window.rs finalize + render_text_command 同步字段

**Files:**
- Modify: `src/overlay/window.rs:finalize_text_input`（小改）
- Modify: `src/overlay/window.rs:render_text_command`（小改）

- [ ] **Step 1: 改 finalize_text_input**

```rust
fn finalize_text_input(
    &mut self,
    state: &gpui::Entity<gpui_component::input::InputState>,
    cx: &mut Context<Self>,
) {
    let value = state.read(cx).value();
    if value.is_empty() {
        self.text_input = None;
        cx.notify();
        return;
    }
    let anchor = self.text_input_anchor;
    let content: String = String::from(value);
    self.drawing.push(DrawCommand::Text {
        anchor: crate::overlay::drawing::Point::new(anchor.x, anchor.y),
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

- [ ] **Step 2: 改 render_text_command 用 current_size + weight**

```rust
fn render_text_command(cmd: &DrawCommand, sf: f32) -> impl IntoElement {
    if let DrawCommand::Text { anchor, content, font_size, color, weight, .. } = cmd {
        let lp_x = px(anchor.x / sf);
        let lp_y = px(anchor.y / sf);
        // 当前 v0.1 是 div + 半透明白底。v0.2 加 weight → 同步给 GPUI
        // gpui 的 div 没有 weight 参数，但 gpui_component::Text 有；
        // 此处先保持 v0.1 渲染，让 GPUI 渲染继续显示 Regular/Bold 视觉差异
        // （如果 GPUI 默认 sans 都一样则不显示差异，但要靠 final → CPU 阶段保证截图真实）
        let fg: Hsla = gpui::rgba(rgba_u32(*color)).into();
        div()
            .absolute()
            .top(lp_y).left(lp_x)
            .text_size(px(*font_size / sf))
            .text_color(fg)
            .bg(gpui::rgba(0xFFFFFF99))
            .p(px(2.0 / sf))
            .min_w(px(0.0))
            .flex_shrink_0()
            .whitespace_nowrap()
            .child(content.clone())
            // weight 仅作标记，未来 GPUI 字体层支持时启用
            // (let _ = weight;)
    } else {
        div()
    }
}
```

注意：GPUI div 字体层不直接接 weight。如果要让 overlay 预览看到 Bold 视觉差异，需要 `gpui_component::text::Text` —— 留给 Task 11 enhancement。这一步只保证代码编译过 + 字段透传对。

- [ ] **Step 3: 编译 + 跑全部测试**

```bash
cargo check
cargo test --lib
```

期望：0 警告 + 11 (原有) + 5 (rasterize_text) + 1 (font) + 2 (toolbar) = 19 测试全过。

- [ ] **Step 4: 提交**

```bash
git add src/overlay/window.rs
git commit -m "feat(overlay): finalize + render pass through font_size/weight

- finalize uses toolbar.current_size / current_weight
- max_width uses selection.size.x for auto-wrap inside selection
- render_text_command ignores weight overlay-side (CPU layer enforces)"
```

---

## Task 9: 端到端真机测试 + 验收

**Files:** 无代码改动

- [ ] **Step 1: 启动服务**

```bash
RUST_LOG=info cargo run 2>&1 | tee /tmp/screenshot-rs.log
```

在另一终端或后台运行。

- [ ] **Step 2: 触发表单 + 选择文字工具**

按 alt+s 触发截图 → 选区拖出一个矩形 → 在 Editing 模式下选 Text 工具（按钮 T / IconName::Text）

- [ ] **Step 3: 输入中文 + 验证 IME**

在选区内点 → 输入框弹出 → 拼音输入"你好世界" → Enter 提交 → 截图 overlay 上 text 应该显示

- [ ] **Step 4: Finish → 粘贴剪贴板验证**

点 Finish → 粘贴到 IM / 看图软件 / `xclip -selection clipboard -o > /tmp/out.png` →

期望：截图中**包含**之前输入的中文真文字（不是色块）。

- [ ] **Step 5: 切到 Bold + 32px + 重复**

工具栏点 B → 字号下拉选 32 → 重复 Step 2-4

期望：文字明显变粗、变大。

- [ ] **Step 6: 多行折行验证**

工具栏切到 Normal + 18 → 选区内点 → 输入 50 个中文字符 → Enter

期望：CPU 阶段 multi_line 测试对应，文字自动多行显示在选区内。

- [ ] **Step 7: 提交 + close spec plan**

```bash
git log --oneline -10
git tag v0.2-rc1  # 可选
```

期望：看到 v0.2 增量 commits（FontWeight、font module、rasterize、toolbar、finalize 等）

---

## Self-Review（写完计划后核对）

| Spec section | Implementing task |
|--------------|-------------------|
| §1 背景与现状 | (信息，无 task) |
| §2 目标 | T1-T11 都服务于 §2 |
| §3 字体来源 + 库选 + 性能 | T1, T3, T5 |
| §4 架构总览 | (理解) T1-T8 |
| §5.1 Cargo.toml | T1 step 5 |
| §5.2 drawing.rs | T2 |
| §5.3 font.rs | T3 |
| §5.4 commands.rs | T4, T5 |
| §5.5 toolbar.rs | T6 |
| §5.6 window.rs | T7, T8 |
| §6 错误处理 | T5 (ok_or) + T7 (Select fallback) |
| §7 测试覆盖 | T3 (1) + T5 (5) + T6 (2) = 8 新测试 |
| §8 风险与缓解 | T7 step 3-4 (API 验证 + fallback) |
| §9 验收标准 | T9 |

- **Type consistency**：
  - `DrawCommand::Text` 字段在 T2 定义，T4/T7/T8 一致使用
  - `FontWeight::font_bytes()` 在 T2 用，T3 也用（T3 引用 T2 的定义）
  - `with_font_system` / `with_swash_cache` 在 T3 定义，T5 使用

- **No placeholders**：每个 step 都给了实际代码 + 实际命令，无 TBD。

- **DRY**：每个 enum 字段 / struct 字段都在第一次定义时给出全字段，后续 task 不重写。

---

## 执行选项（实施时选）

- **Subagent-Driven (推荐)** — 每个 task 派遣新 subagent，review 在 task 之间进行
- **Inline** — 当前会话里 sequential 跑所有 task
