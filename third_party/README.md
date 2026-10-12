# third_party

## gpui-component（vendored 0.7.3）

迁移到 gpui-kit 后，组件库来自 gpui-kit 依赖的 crates.io `gpui-component ^0.7.1`。
本目录是该 crate 的源码副本（0.7.3），由根 `Cargo.toml` 的 `[patch.crates-io]` 接管，
**只为一处补丁**：

- `src/lib.rs`：`mod root;` → `pub mod root;`
- `src/root.rs`：追加公开包装 `pub fn hide_tooltip(root: &gpui_base::Root, cx: &mut App)`

原因：上游把 `WindowState`（内含 `TooltipOverlay`）收成 `pub(crate)`，外部没有清浮层的入口。
而覆盖窗口是复用的 —— Esc 关窗时鼠标若还停在工具栏按钮上，`TooltipOverlay.content` 会跨会话
残留，下次一开遮罩就先闪出上一个会话的浮层。

调用点（`src/overlay/window.rs`）正处于 `Root` 自身的 update 闭包内，此时任何
`window.root::<Root>()` 重读都会 panic（`cannot read Root while it is already being updated`），
所以补丁收调用方**已经持有的** `&Root`、只经 `Root::plugin::<WindowState>()` 取插件实体，
全程不重读 root。

升级该 crate 时：重新取上游源码、重打上面两处，再按下表复核失效情况。

**与旧补丁的等价性**：旧补丁（`951e3de` 的 `crates/ui/src/root.rs`）补丁体是
`self.tooltip_overlay.update(cx, |o, cx| o.hide(cx))`；本目录的包装调的是同一个实体的
同一个方法，唯一区别是实体来源 —— 应用侧拿不到 `WindowState` 的私有字段，只能经
`Root::plugin::<WindowState>()` 取。而 `TooltipOverlay::hide`（gpui-base `tooltip.rs`）
第一件事就是 `self.content = None`（连带清 bounds / show_task / hide_task），
即**必然清掉跨会话残留的浮层内容**。

## 迁移前 6 处本地补丁的去向

迁移前的 vendored 副本是 `951e3de`（monorepo 布局，`crates/ui` 等），带 6 处本地补丁。
逐条处置如下。

| 旧补丁 | 现状 |
| --- | --- |
| `input/element.rs`：`RIGHT_MARGIN` 10 → 2 | **不需要带**。该常量现属 gpui-base（`src/input/base/element.rs`），值回到上游默认 `px(10.)`；本仓框宽已改成「文字宽 + 2×`inset_x`」（`inset_x = text_box_pad_x + Input 自带 8`），代入应用算式后滚动条件化简为 `pad_x < 2`，而 `pad_x ≥ 4`。实测（`examples/text_box_inset_probe.rs`，光标推到末尾）fs=14/24/64 的 `scroll_offset.x` 全为 `0px` |
| `input/state.rs`：新增 `editor_scrollbar` 字段（默认开、可关） | **无法带，但已确认不构成实际回归**。新库把滚动条做进编辑器元素自身（gpui-base `EditorScrollbar`），没有公开开关；`soft_wrap(false)` 下用的是双向 `Scrollbar`。不过 gpui-base 自己的语义是「无溢出就没有可交互轨道」（其测试 `no_overflow_has_no_interactive_track`），而本仓框宽永远贴住文字（横向实测 `scroll_offset.x = 0`）⇒ **横向条不会出现**（正是当年那个抱怨点）；纵向条只在超过 `auto_grow(1, 8)` 的 8 行、且正在滚动时出现、2 秒后淡出（`ScrollbarMode::Scrolling`），属标准行为 |
| `popover.rs`：受控关闭时恢复焦点、`set_open` | **无对应调用点**。本仓没有直接调用 `set_open`（新库也没有该方法），弹层开关由 keyed state 管理。弹层关闭后的焦点行为列入桌面验证 |
| `resizable/mod.rs`：总量非有限/≤0 时的防 NaN | **无需求**。本仓完全不使用 `Resizable`（0 处引用） |
| `root.rs`：`clear_tooltip` | 本仓从未调用（0 处引用） |
| `tooltip.rs`：`TooltipOverlay::hide` 由 `pub(crate)` 改 `pub` | 已由本目录的 `hide_tooltip` 包装等价替代 |

## 复核用的命令

```bash
# 实测文本框首字是否被裁（会开一个绿底小窗约 3.5 秒）
cargo run --example text_box_inset_probe -- 14
cargo run --example text_box_inset_probe -- 24
cargo run --example text_box_inset_probe -- 64

# 对应的守卫测试
cargo test --lib text_box_inset_x_covers_editor_right_margin
```

## 本项目补丁 2：`set_root_bordered` —— 恢复旧版 `Root::bordered(false)`

**问题**：0.7.3 把旧版 `Root::new(..).bordered(false)` 这个 builder 换成了 `RootPlugin`
（`WindowState`）里**无条件**的 `window_border()`。而 gpui 的 `WindowDecorations` 只有
`Server`（mutter 会给 37px 服务端标题栏、全屏客户端被挤扁）和 `Client`（CSD 阴影把内容
四边内缩约 20px）两种，没有「无装饰」；`Window::set_client_inset(px(0.))` 也会被平台按
`Client` 重新设回，实测无效。

**表现**：alt+s 后遮罩里画面被四边内缩约 16~20px（用户报的「黑色画布 + 不是全屏」的
后一半；前一半是 `paint_image` 的 `image_bounds` 填空 —— 见 commit `ee98c55`）。

**补丁**：`src/root.rs` 加了 `pub fn set_root_bordered(bool)`（全局原子标志）+ `decorate()`
里的分支；应用在建窗前的 `gpui::component::init(cx)` 之后调用
`set_root_bordered(false)`。语义与 main 上 5 处 `.bordered(false)` 一致。

**升级时**：重新取上游源码后需重打本处与补丁 1（`hide_tooltip`）。
