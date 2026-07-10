//! 系统剪贴板写入服务
//!
//! 使用 `arboard` crate 跨平台抽象。截图完成时调用 `write_frame` 把 RGBA
//! 数据写入剪贴板，粘贴到任意位置（Slack/编辑器/浏览器）都能看到图像。

use arboard::ImageData;

use crate::capture::CapturedFrame;
use crate::error::AppResult;

/// 跨平台剪贴板服务
pub struct ClipboardService;

impl ClipboardService {
    pub fn new() -> Self {
        Self
    }

    /// 把捕获的帧写入剪贴板
    pub fn write_frame(&self, frame: &CapturedFrame) -> AppResult<()> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(crate::error::AppError::Clipboard)?;
        let img_data = ImageData {
            width: frame.width as usize,
            height: frame.height as usize,
            bytes: frame.pixels.clone().into(), // Cow<[u8]>
        };
        clipboard
            .set_image(img_data)
            .map_err(crate::error::AppError::Clipboard)?;
        Ok(())
    }
}
