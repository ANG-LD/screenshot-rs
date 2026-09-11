//! 应用图标资源（AssetSource）
//!
//! GPUI 的 `Svg`/`Icon` 元素按**资源路径**取字节，路径由应用注册的
//! `AssetSource` 解析。gpui-component 自带一套 Lucide 图标（`icons/*.svg`），
//! 但缺少截图工具语义需要的几个（画笔 / 文字 / 扫描 / 马赛克 / 固定 / 加粗 /
//! 滚轮 / 竖直展开 / 吸管 …），本项目因此自带一套同风格（Lucide 24×24、
//! `stroke="currentColor"`、`stroke-width=2`、圆头圆角）图标放在
//! `assets/icons/ui/`。
//!
//! [`AppAssets`] 把它们**编译期内嵌**进二进制（`include_bytes!`，无文件系统
//! 依赖、发行包不需要额外资源），并按前缀 `app-icons/` 暴露；其余路径原样
//! 回退给 [`gpui_component_assets::Assets`]。这样两种图标可以混用：
//!
//! ```ignore
//! Icon::new(IconName::Check)                 // 组件内置：完成
//! Icon::empty().path(ui_icons::PENCIL)       // 项目自带：画笔
//! ```
//!
//! 新增图标：把 SVG 放进 `assets/icons/ui/`，在本文件 [`icon_table!`] 里登记一行，
//! 并在 [`icons`] 里加一个常量即可（`AssetSource` 的 `list()` 会同步看到）。

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// 项目自带图标的资源路径前缀
pub const APP_ICON_PREFIX: &str = "app-icons/";

/// 项目自带图标路径常量（`Icon::empty().path(...)` 用）
pub mod icons {
    /// 矩形工具
    pub const SQUARE: &str = "app-icons/square.svg";
    /// 椭圆工具
    pub const CIRCLE: &str = "app-icons/circle.svg";
    /// 箭头工具
    pub const MOVE_UP_RIGHT: &str = "app-icons/move-up-right.svg";
    /// 自由画笔
    pub const PENCIL: &str = "app-icons/pencil.svg";
    /// 文字工具
    pub const TYPE: &str = "app-icons/type.svg";
    /// OCR 文字识别
    pub const SCAN_TEXT: &str = "app-icons/scan-text.svg";
    /// 马赛克
    pub const GRID_2X2: &str = "app-icons/grid-2x2.svg";
    /// 取色器
    pub const PIPETTE: &str = "app-icons/pipette.svg";
    /// 固定到桌面（Pin）
    pub const PIN: &str = "app-icons/pin.svg";
    /// 取消置顶
    pub const PIN_OFF: &str = "app-icons/pin-off.svg";
    /// 文字加粗
    pub const BOLD: &str = "app-icons/bold.svg";
    /// 自动滚动截屏（竖直展开）
    pub const UNFOLD_VERTICAL: &str = "app-icons/unfold-vertical.svg";
    /// 手动滚动截屏（鼠标滚轮）
    pub const MOUSE: &str = "app-icons/mouse.svg";
    /// 下载
    pub const DOWNLOAD: &str = "app-icons/download.svg";
    /// 删除
    pub const TRASH_2: &str = "app-icons/trash-2.svg";
    /// 新版本/亮点
    pub const SPARKLES: &str = "app-icons/sparkles.svg";
    /// 性能/加速档位
    pub const GAUGE: &str = "app-icons/gauge.svg";
    /// 已加速（闪电）
    pub const ZAP: &str = "app-icons/zap.svg";
    /// 保存图片
    pub const IMAGE_DOWN: &str = "app-icons/image-down.svg";
}

/// 登记表：`资源路径常量 => 磁盘文件（相对本文件）`
macro_rules! icon_table {
    ($($const_path:path => $file:literal),* $(,)?) => {
        /// 按资源路径取项目自带图标字节
        fn load_app_icon(path: &str) -> Option<Cow<'static, [u8]>> {
            $(
                if path == $const_path {
                    return Some(Cow::Borrowed(include_bytes!(concat!("../assets/icons/ui/", $file))));
                }
            )*
            None
        }

        /// 项目自带图标的全部资源路径（`list()` 用）
        fn app_icon_paths() -> Vec<&'static str> {
            vec![$($const_path),*]
        }
    };
}

icon_table! {
    icons::SQUARE => "square.svg",
    icons::CIRCLE => "circle.svg",
    icons::MOVE_UP_RIGHT => "move-up-right.svg",
    icons::PENCIL => "pencil.svg",
    icons::TYPE => "type.svg",
    icons::SCAN_TEXT => "scan-text.svg",
    icons::GRID_2X2 => "grid-2x2.svg",
    icons::PIPETTE => "pipette.svg",
    icons::PIN => "pin.svg",
    icons::PIN_OFF => "pin-off.svg",
    icons::BOLD => "bold.svg",
    icons::UNFOLD_VERTICAL => "unfold-vertical.svg",
    icons::MOUSE => "mouse.svg",
    icons::DOWNLOAD => "download.svg",
    icons::TRASH_2 => "trash-2.svg",
    icons::SPARKLES => "sparkles.svg",
    icons::GAUGE => "gauge.svg",
    icons::ZAP => "zap.svg",
    icons::IMAGE_DOWN => "image-down.svg",
}

/// 应用资源源：项目自带图标 + gpui-component 内置图标
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = load_app_icon(path) {
            return Ok(Some(bytes));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out: Vec<SharedString> = app_icon_paths()
            .into_iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect();
        out.extend(gpui_component_assets::Assets.list(path)?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个登记的项目图标都能取到非空 SVG 字节，且是合法 SVG 头
    #[test]
    fn every_registered_icon_loads_and_is_svg() {
        for path in app_icon_paths() {
            let bytes = load_app_icon(path).unwrap_or_else(|| panic!("图标缺失: {path}"));
            let text = std::str::from_utf8(&bytes).expect("图标应为 UTF-8 文本");
            assert!(text.contains("<svg"), "{path} 不是 SVG");
            assert!(text.contains("currentColor"), "{path} 缺少 currentColor（无法跟随主题色）");
        }
    }

    /// 内置图标集仍然可达（回退路径没被破坏）
    #[test]
    fn falls_back_to_component_icons() {
        let loaded = AppAssets.load("icons/check.svg").expect("回退 load 不应报错");
        assert!(loaded.is_some(), "gpui-component 内置 icons/check.svg 应可用");
    }

    /// list() 同时包含项目图标与内置图标
    #[test]
    fn list_merges_app_and_component_icons() {
        let all = AppAssets.list("").expect("list 不应报错");
        assert!(all.iter().any(|p| p.starts_with(APP_ICON_PREFIX)));
        assert!(all.iter().any(|p| p.starts_with("icons/")));
    }
}
