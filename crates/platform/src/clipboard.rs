//! 剪贴板和拖放交换的内容：纯文字、带格式的文字（RTF）、位图、文件列表。
//!
//! 接口是后端的 [`crate::ClipboardService`]。纯文字走 arboard；别的格式和"一次放几种"arboard 做不到，
//! macOS（`NSPasteboard`）、Windows（剪贴板 API）直接调系统；Linux 走 arboard 能做的（图片、文件列表，一次一种，没有带格式的文字）。
//!
//! 不调用系统的纯逻辑在 [`rtf`]、[`dib`] 里，在任何平台上都能测试。

use std::path::PathBuf;
use std::sync::Arc;

pub mod dib;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
pub mod rtf;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

/// 剪贴板上内容的格式
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClipFormat {
    /// 纯文字
    Text,
    /// 带格式的文字
    RichText,
    /// 图片
    Image,
    /// 文件列表
    Files,
}

/// 内存里的一张位图：RGBA8，sRGB，直通 alpha，一行接一行紧密排列
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 像素，`width × height × 4` 个字节
    pub rgba: Arc<[u8]>,
}

impl RgbaImage {
    /// 像素数和宽高对得上、不是空图
    pub fn is_valid(&self) -> bool {
        let expected = (self.width as usize).checked_mul(self.height as usize).and_then(|n| n.checked_mul(4));
        self.width > 0 && self.height > 0 && expected == Some(self.rgba.len())
    }
}

/// 一次放进剪贴板（或者从剪贴板读出来）的内容：同一份内容的几种表示。什么都没有的是"清空"
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    /// 纯文字
    pub text: Option<String>,
    /// 带格式的文字（RTF，见 [`rtf`]）
    pub rich_text: Option<Vec<u8>>,
    /// 图片
    pub image: Option<RgbaImage>,
    /// 文件（绝对路径）
    pub files: Vec<PathBuf>,
    /// 应用自己定的格式：格式名和原样的字节（见 [`crate::ClipboardService::read_custom`]）。
    /// macOS 上格式名是剪贴板的类型（用反向域名，比如 `com.example.layers`），Windows 上向系统登记成剪贴板格式
    pub custom: Vec<(String, Vec<u8>)>,
}

impl ClipboardContent {
    /// 什么都没有（放进去就是清空）
    pub fn is_empty(&self) -> bool {
        self.formats().is_empty() && self.custom.is_empty()
    }

    /// 这些内容有哪几种格式：文件、图片、带格式的文字、纯文字
    pub fn formats(&self) -> Vec<ClipFormat> {
        let mut formats = Vec::new();
        if !self.files.is_empty() {
            formats.push(ClipFormat::Files);
        }
        if self.image.is_some() {
            formats.push(ClipFormat::Image);
        }
        if self.rich_text.is_some() {
            formats.push(ClipFormat::RichText);
        }
        if self.text.is_some() {
            formats.push(ClipFormat::Text);
        }
        formats
    }
}
