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
- OCR 文字识别（PaddleOCR PP-OCRv6 medium，中英混排）
- 滚动截长图（自动拼接）
- 支持 Windows 10/11 和 Linux X11

## 安装

```bash
cargo build --release
./target/release/screenshot-rs
```

## OCR 模型

识别引擎为 **PaddleOCR PP-OCRv6 medium**（ONNX 格式，检测 62 MB + 识别 76 MB），
首次使用 OCR 时自动下载到缓存目录（`~/.cache/screenshot-rs/paddle`）；
也支持本地放置模型（免下载、离线可用）：

```bash
mkdir -p models/PP-OCRv6
# 放入三个文件（可从 https://github.com/GreatV/oar-ocr/releases 或 ModelScope
# https://modelscope.cn/models/RapidAI/RapidOCR 下载）：
#   pp-ocrv6_medium_det.onnx   检测模型
#   pp-ocrv6_medium_rec.onnx   识别模型
#   ppocrv6_dict.txt           词典（18708 字符）
```

模型查找顺序：`OCR_MODEL_DIR` 环境变量 / 配置 `ocr.model_dir` →
项目内 `models/PP-OCRv6` → 缓存目录（不存在则自动下载）。
运行时二进制已静态链接 ONNX Runtime，**发行包无需附带任何运行库**。

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
- **v0.2**：OCR 文字识别（已完成，PaddleOCR PP-OCRv6）、纯 Wayland 支持
- **v0.3**：滚动截长图（已完成）
- **v0.4**：截图历史记录
- **v0.5**：自定义快捷键 + 配置文件

## 开发

```bash
cargo test            # 单元测试（42 个 + 1 个 OCR 端到端，需模型文件）
cargo build           # 编译
cargo run             # 运行（开发模式）
cargo clippy          # Lint
```

OCR 端到端测试（需要本地模型）：
```bash
cargo test --test ocr_paddle -- --ignored --nocapture
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

## 已知警告

`screenshots v0.6.0` crate 在编译时输出 `future-incompat` 警告（Rust 未来版本可能拒绝该 crate 的代码）。当前不影响构建；跟踪细节见 `docs/follow-ups.md`。

## 许可证

MIT
