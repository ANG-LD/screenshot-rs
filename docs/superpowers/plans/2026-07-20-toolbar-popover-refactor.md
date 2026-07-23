# 工具栏 Popover 重构（用 gpui-component Popover）

## 背景

`src/overlay/window.rs` 当前未提交的重构把工具栏从「Button.on_click」改成「div 包 Button + div.on_click」chip 形式，目的是让 Color/Font 用 trigger + 浮出面板（popover）的形式实现。

编译失败，错误 7 处：

```
error[E0599]: no method named `on_click` found for struct `gpui::Div`
src/overlay/window.rs:522 / 546 / 607 / 647 / 694 / 736 / 757
```

`gpui::Div` 实现了 `InteractiveElement::on_click`（`prelude::*` 已带回），但 gpui-component 的某些 trait 干扰了方法解析。无论根因如何，手撸 div 包 popover 的实现路径已被证明不稳定。

**用户决策**：放弃 div 包 popover 的设计，改用 gpui-component 自带的 `Popover` 组件重做 Color 和 Font 两个 trigger。

## 目标

1. 编译通过。
2. Color trigger：点击弹出 12 色色板，选中后切换 `toolbar.current_color`；点外面自动 dismiss。
3. Font  trigger：点击弹出 Bold 切换 + 5 档字号选择，切换 `toolbar.current_weight` / `toolbar.current_size`；点外面自动 dismiss。
4. 5 个绘图工具（Rectangle/Arrow/Freehand/Text/Mosaic）、Undo/Redo、Finish/Cancel 仍然可用。
5. 一次只允许一个 popover 打开（互斥）。点绘图工具 / Undo/Redo / Finish/Cancel 时自动收起 popover。

## 设计

### Popover 受控 + 互斥

`Popover::open(b)` + `Popover::on_open_change(cb)` 受控模式：

- `open = toolbar.popup == Some(ToolbarPopup::Color)`（Font 同理）
- `on_open_change(&is_open, ..)` 回调中：
  - `is_open == true` → `toolbar.popup = Some(X)`
  - `is_open == false` → 若当前为 X 则 `toolbar.popup = None`（避免互斥情况下错误清掉另一个）

互斥由 `Popover` 自己完成：当用户点击 Color trigger 时，Popover 触发 open→true；on_open_change 把 `toolbar.popup` 设为 `Some(Color)`，下一个渲染帧 Font Popover 的 `open` 为 false，自动 dismiss。

### Trigger 用 Button

`Popover::trigger` 要 `Selectable + IntoElement`，`Button` 已实现 `Selectable`。trigger Button 上 `.on_click` 不再挂——点击由 Popover 拦截，Popover 内部根据 trigger 被点切换 open。`selected` 由 Popover 通过 `Selectable::selected(is_selected || is_open)` 自动设置（trigger 渲染时已传入）。

### 视觉

- **Color trigger Button**：左 icon=Palette，右 label 是「■」（用 unicode 色块字符或一个小色块 div 当 label 不行 → 用 Button 内置 icon 切换为 `Square`、颜色不能在 Button 上变）。简化：trigger 显示 IconName::Palette + label=空字符串，Popover 内部色板显示当前色为选中态。Button.selected(true) 在 popover 打开时高亮。
  
  当前颜色实时反映在 trigger 上是「nice to have」，非必须。MVP 阶段先不做，trigger 始终显示 Palette 图标 + 选中态边框。

- **Font trigger Button**：label = 当前字号数字（如 "48"），无 icon。点击弹 popover。

### 工具栏根布局

```
row = h_flex gap_1 items_center
  [Rect] [Arrow] [Freehand] [Text] [Mosaic]   ← Button + on_click
  [Undo] [Redo]                                ← Button + on_click
  [Color Popover wrap]                         ← Popover(trigger=Button Palette)
  [Font  Popover wrap]                         ← Popover(trigger=Button "48")
  [Cancel]                                     ← Button + on_click
  [Finish]                                     ← Button + on_click (primary)
```

绘图 / Undo / Redo / Finish / Cancel 复用 HEAD 版本的 Button.on_click 写法（已验证能编译过）。ColorPicker / Bold 这两个原来需要特殊渲染的按钮从 `ToolButton::ORDER` 主循环中跳过，由 Popover 替代。

### 文件改动

- `src/overlay/toolbar.rs`
  - 保留 `ToolbarPopup { Color, Font }` 与 `ToolbarState.popup: Option<ToolbarPopup>` 字段（已就位）。
  - 不需要其它改动。

- `src/overlay/window.rs`
  - 顶部 import 增加 `gpui_component::popover::Popover`。
  - `render_toolbar` 主循环：
    - Bold 在 ORDER 里跳过（已被 Font popover 接管）。
    - ColorPicker 单独走 `render_color_popover(view, cx)`，返回 `Popover`（不是 Button）。
    - 其余按钮走 HEAD 写法：`Button::new(...).icon(..).label(..).tooltip(..).compact().on_click(..)`，根据 active/disabled/primary 调整。
    - 插入 Font popover（`render_font_popover`）放在合适位置。
  - 删除 `render_tool_chip` / `render_action_chip` / `render_color_trigger` / `render_font_trigger` / `render_color_popover` / `render_font_popover` 这 6 个手撸 div 函数。
  - 新增 `render_color_popover(view, cx) -> Popover`：
    - trigger = `Button::new("color-trigger").icon(IconName::Palette).label("").tooltip("颜色").compact().selected(toolbar.popup == Some(Color))`
    - `Popover::new("color-popover").trigger(trigger).open(toolbar.popup == Some(Color)).on_open_change(set popup).content(|state, w, cx| { 12 色色板 div })`
    - 色板内部：循环 `palette::default_palette()`，每色 `Button::new(("swatch", i)).on_click(set current_color; popup = None)`；或继续用 div+on_click（已删除该函数体，改用 Button）。
  - 新增 `render_font_popover(view, cx) -> Popover`：trigger = `Button::new("font-trigger").label(format!("{}", current_size)).tooltip("字体")`；content = 一行 [Bold 切换 Button] + 5 个字号 Button。
  - 工具栏 outer div 不变（absolute + bg + rounded + p）。

- 不动 `src/main.rs`（panic hook 留着，与重构无关）。

## 验证步骤

1. `cargo check` 通过、零错误。
2. `cargo run`：
   - 截图 → 选区 → 工具栏出现。
   - 点 Color → 12 色色板浮出 → 点其中一色 → popover 收起，颜色应用。
   - 点 Font → Bold + 5 字号浮出 → 选 32 → 弹窗收起，回头选 Text 工具画字 → 字号 = 32。
   - 点 Color → 再点 Font → Color 自动收起（互斥）。
   - 点 Color → 点工具栏外的 dim 遮罩 → Color popover 自动收起。
   - 点 Color → 点 Rectangle → Color popover 收起，工具切到 Rectangle。
   - Undo/Redo、Cancel、Finish 仍可用。

## 风险

- **Popover 受控渲染 + `appearance`**：默认 `appearance=true`，会自带 bg/border/padding/shadow，可能与我们的暗色主题不搭。如果外观不对，调 `.appearance(false)` 后在 content 里自己加 bg/padding。
- **Popover anchor**：默认 `Anchor::TopLeft`，让 popover 浮在 trigger 右下角。若挡住工具栏下方按钮，可换 `Anchor::BottomLeft`（向上展开）。需运行时观察。
- **Popover 与 overlay 窗口的 z 序**：Popover 内部用 `deferred` 渲染，理论上跟普通浮层一样工作，但 overlay window 是 fullscreen，可能需要验证 popover 是否能浮在 overlay 之上。
- **Popover focus 抢占**：Popover 打开时会 focus trigger 或 content，可能与 Text 工具的 Input focus 冲突。需运行时验证。

## 不在范围内

- 不重写绘图 / Undo / Redo / Finish / Cancel 的按钮实现（保持 HEAD 写法）。
- 不动 main.rs 的 panic hook。
- 不实现长期想要的「trigger 上显示当前颜色色块」高级视觉。
