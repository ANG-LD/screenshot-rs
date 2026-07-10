//! 通用工具模块集合
//!
//! 提供跨模块复用的基础工具：
//! - `bounds`：几何运算（矩形、相交、归一化等）
//! - `color`：颜色空间转换（HSV / RGB / 像素读写）
//! - `image`：图像处理辅助（编码、缩放、合成）

pub mod bounds;
pub mod color;
pub mod image;