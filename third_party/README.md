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

## 迁移前 6 处本地补丁的去向

迁移前的 vendored 副本是 `951e3de`（monorepo 布局，`crates/ui` 等），带 6 处本地补丁。
逐条处置如下。

| 旧补丁 | 现状 |
| --- | --- |
| `input/element.rs`：`RIGHT_MARGIN` 10 → 2 | **不需要带**。该常量现属 gpui-base（`src/input/base/element.rs`），值回到上游默认 `px(10.)`；本仓框宽已改成「文字宽 + 2×`inset_x`」（`inset_x = text_box_pad_x + Input 自带 8`），代入应用算式后滚动条件化简为 `pad_x < 2`，而 `pad_x ≥ 4`。实测（`examples/text_box_inset_probe.rs`，光标推到末尾）fs=14/24/64 的 `scroll_offset.x` 全为 `0px` |
| `input/state.rs`：新增 `editor_scrollbar` 字段（默认开、可关） | **无法带**。新库把滚动条做进编辑器元素自身（gpui-base `EditorScrollbar`），没有公开开关。当初动机是浮层小框里闪**横向**滚动条，而横向已不溢出（同上实测）；只有超过 `auto_grow(1, 8)` 的 8 行时可能冒**纵向**条，待桌面确认 |
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
