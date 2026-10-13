# third_party

## gpui-component（vendored 0.7.3）

迁移到 gpui-kit 后，组件库来自 gpui-kit 依赖的 crates.io `gpui-component ^0.7.1`。
本目录是该 crate 的源码副本（0.7.3），由根 `Cargo.toml` 的 `[patch.crates-io]` 接管。

**与上游 0.7.3 的差异恰好只有两个文件**（补丁处均带 `[本项目补丁]` 注释），可自行核对：

```bash
R=$(ls -d ~/.cargo/registry/src/index.crates.io-*/gpui-component-0.7.3 | head -1)
diff -rq "$R/src" third_party/gpui-component/src   # 只应列出 lib.rs 与 root.rs
```

| 位置 | 改动 | 为什么不能用应用侧代码替代 |
| --- | --- | --- |
| `src/lib.rs` | `mod root;` → `pub mod root;` | 只为能调用下面两个函数（上游 root 模块是私有的） |
| `src/root.rs` | 追加 `pub fn set_root_bordered(bool)` | 0.7.3 把旧版 `Root::new(..).bordered(false)` 换成了 `RootPlugin` 里**无条件**的 `window_border()`，而 `WindowDecorations` 只有 Server / Client 两档（没有「无装饰」），`set_client_inset(0)` 也会被平台按 Client 设回 —— 见下节「本项目补丁 2」 |
| `src/root.rs` | 追加 `pub fn hide_tooltip(root, cx)` | 上游把 `WindowState`（内含 `TooltipOverlay`）收成 `pub(crate)`，外部没有清浮层的入口 |

**这两处 vendoring 什么时候能去掉**：给上游提 PR（① 让 root 的 `window_border` 可配置；
② 公开一个 tooltip 清理入口），等新版发布后即可删掉本目录、回到纯 crates.io 依赖。
在那之前删掉会**直接复现两个已修 bug**（窗口内容四边内缩、跨会话残留浮层）。

### 补丁 1：`hide_tooltip`

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
| `input/state.rs`：新增 `editor_scrollbar` 字段（默认开、可关） | **无法带，改由应用侧处置**。新库把滚动条做进编辑器元素自身（gpui-base `EditorScrollbar`），没有公开开关。应用侧现在做两件事：① 启动时把滚动条主题改成 `ScrollbarMode::Hover` + `ScrollbarMotion::idle(0)`（`src/overlay/window.rs`）——无溢出本就不画轨道，`idle=0` 又关掉了「刚滚动过就显示」那条分支（打字把光标滚进视野也算滚动），于是只在**悬停/拖动**时才可能出现；② 编辑态改成 `soft_wrap(true)`，框宽按编辑器**真实可写宽度**算（`editor_text_width()`），行数由 `measure_wrapped_line_count()` 用与成图**同一套折行参数**给出 ⇒ 正常输入不产生横向溢出。详见下节「文本框几何」 |
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

# 文本框几何（宽度 / 折行行数）的三组守卫测试
cargo test --lib text_box_width_fits_one_line_in_editor
cargo test --lib editor_text_width_subtracts_editor_insets
cargo test --lib wrapped_line_count
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

## xim 那套 crate 钉在 git rev

根 `Cargo.toml` 的 `[patch.crates-io]` 还把 `zed-xim` / `xim-ctext` / `xim-parser` 钉在
`16f35a2c881b815a2b6cdfd6687988e84f8447d8`（= 0.1.4 原先用的那份）。

原因：crates.io 上 gpui-pre 0.7.3 拉的 `xim-ctext 0.3.0` 是**旧实现** —— `decode!` 宏在输出
缓冲满时不推进输入游标，`compound_text_to_utf8` 也不按 ESC 分块处理字符集切换；中文输入法
提交候选（按空格选词）送来的 `COMPOUND_TEXT` 于是一律解成 `Err(UnsupportedEncoding)`，而
`zed-xim` 里是 `xim_ctext::compound_text_to_utf8(&commited).expect("Encoding Error")`
⇒ 直接 panic，覆盖窗口（截图框）随之消失。用户报的「文字输入时按空格截图框没了」就是这个。
该 git rev 已修（分块 + 推进游标），实测打字/空格正常。

**何时能去掉**：等 `xim-ctext` 发布含该修复的版本（或 gpui-kit 换用它）后即可解除 pin。

**已知边界**：`XMODIFIERS` 指向一个**没有在运行**的输入法（例如环境里写着 `@im=fcitx`
但实际跑的是 ibus）时，gpui 的 XIM 客户端连不上，应用**收不到任何按键**（日志里是
`zed_xim::x11rb: Try connect <im>` 之后就没有键事件）。这是 gpui-kit 侧的行为，不在本仓修，
排查输入问题时先看这条。

## 文本框几何（宽度、折行与滚动条）

编辑态文本框（`TextareaState`）**能用的宽度比框宽小得多**，这是几个已修 bug 的共同根因：

```text
框宽                                   ← window.rs 的 textbox auto-grow
  − box 左内边距 text_box_pad_x(fs)     （随字号的对称留白）
  − 编辑器自身左右内边距 2 × 8          （gpui-component Size::Size(_).input_px() = 8）
  − gpui-base 右安全边距 10             （input/base/element.rs 的 RIGHT_MARGIN）
  = editor_text_width()                ← 编辑器里文字真正可用的宽度
```

两条规则（都在 `src/overlay/commands.rs`，都有守卫测试）：

1. **框宽**：`text_box_width_for_line(adv, fs)` = 文字宽 + 上面三项 + 4px 余量，保证**一行
   放得下**；再取 `min(截图区右边界 − 框左)`，上限是截图区（用户要求框不越出截图区）。
   少算这 26px+ 的表现就是「打两个汉字被折成一行一个字、第二个字掉出框外」。
2. **行数**：`measure_wrapped_line_count(value, fs, editor_text_width(new_w, fs) × sf, weight)`
   —— 显式 `\n` 与自动折行统一计数，且与成图 `rasterize_text` 用**完全相同**的 layout 参数
   （同字体/字重、行距 1.5×、`Buffer::set_size(Some(max_width))`）。按框宽算会**少算行数**，
   少算的那一行就掉出框外。固化时 `DrawCommand::Text.max_width` 同样取 `editor_text_width()`：
   成图从 `text_inset` 起画，用框宽当折行宽会让文字向右溢出，并与预览的折行位置不一致。

高度 = 行数 × 行盒(1.5×字号) + 上下内边距，上限 8 行、且不越过截图区下边界；位置兜底用
`clamp_keeping_size`（只挪位置、不缩尺寸 —— `clamp_inside` 会先缩 size，把文字重新挤出视口，
也就重新触发那条横向滚动条）。
