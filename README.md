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
- OCR 文字识别（PaddleOCR PP-OCRv6，中英混排；框选后立即关闭遮罩、
  后台识别并把文字复制到剪贴板）
- 滚动截长图（自动拼接）
- 支持 Windows 10/11 和 Linux X11

## 安装

```bash
cargo build --release
./target/release/screenshot-rs
```

## OCR 模型

识别引擎为 **PaddleOCR PP-OCRv6**（ONNX 格式），支持两个档位：

| 档位 | 模型大小 | CPU 速度（14 行代码区）| 准确率 |
|---|---|---|---|
| **small（默认）** | ~30 MB | ~0.8s | ~95% |
| medium | ~132 MB | ~14s | ~99% |

切换档位：`config.toml` 的 `[ocr] model_tier = "small"`（或 `"medium"`），
也可用环境变量 `OCR_MODEL_TIER`。首次使用 OCR 时自动下载模型到缓存目录
（`~/.cache/screenshot-rs/paddle`）；也支持本地放置模型（免下载、离线可用）：

```bash
mkdir -p models/PP-OCRv6
# 放入三个文件（small 档示例；可从 https://github.com/GreatV/oar-ocr/releases 下载）：
#   pp-ocrv6_small_det.onnx   检测模型（9.4 MB）
#   pp-ocrv6_small_rec.onnx   识别模型（21 MB）
#   ppocrv6_dict.txt          词典（18708 字符）
```

模型查找顺序：`OCR_MODEL_DIR` 环境变量 / 配置 `ocr.model_dir` →
项目内 `models/PP-OCRv6` → 缓存目录（不存在则自动下载）。
运行时二进制已静态链接 ONNX Runtime，**发行包无需附带任何运行库**。

### 推理后端：默认自动检测（有 GPU 用 GPU，没有用 CPU）

**`cargo build --release` 默认构建即带平台加速能力**（按目标平台自动启用
ONNX Runtime 的对应加速执行器并静态链接，无需任何参数）：

| 平台 | 默认构建内置 | 覆盖 NVIDIA | 覆盖其他 GPU |
|---|---|---|---|
| Linux x86_64 | CUDA（NVIDIA） | `--features ocr-cuda` | 无（AMD 用 CPU）|
| Windows x86_64 | CUDA + DirectML（NVIDIA / AMD / Intel）| `--features ocr-cuda` | `--features ocr-directml` |
| macOS（Apple 芯片）| CoreML（GPU/神经引擎）| `--features ocr-coreml` | — |
| 其他平台（aarch64 Linux 等）| 纯 CPU | — | — |

**运行时自动检测**：一个安装包同时含 CPU + 加速能力，启动时枚举客户机真实
硬件设备自动选择（日志见下）——

- 有对应加速硬件 → 加速推理（日志：`运行时检测到 CUDA GPU 设备 → 使用 GPU 推理`）
- 没有 → **CPU 推理**（日志：`未检测到可用加速设备 → 使用 CPU 推理`）

无需任何配置。也可手动指定（`config.toml` 或环境变量
`OCR_EXECUTION_PROVIDER`）：`auto`（默认）/ `cpu` / `cuda` / `coreml` / `directml` / `openvino`。

> 注：Linux 的 CUDA 加速依赖 ORT 的 CUDA provider 动态库，发行包已随应用
> 分发（见 `packager.toml` resources）；AppImage/deb 安装后即可在有 NVIDIA
> 显卡的机器上直接使用 GPU 推理，无显卡则自动回退 CPU。

GPU 收益：det+rec 推理从 CPU 的数百毫秒~秒级降到几十毫秒（小图），
代码区 small 档约 0.8s → 预计 <0.2s；medium 多行场景收益更大。

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
