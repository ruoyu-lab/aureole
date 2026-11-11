//! 剪贴板：纯文字、带格式的文字、图片、文件列表、应用自己定的格式，一次能放几种。
//!
//! 输入框、可选中文字的复制粘贴走同一个系统剪贴板（只用纯文字；剪贴板里是图片时输入框粘贴不到东西，
//! 想接图片的应用在 `on_key_down` 里读 [`read_image`]）。测试工具里换成进程内的，测试不碰用户的剪贴板。
//!
//! ```no_run
//! # use aureole::prelude::*;
//! use aureole::clipboard::{self, ClipFormat, ClipboardContent};
//!
//! // 一次放几种：能贴图片的地方贴图片，只认文字的地方贴文字
//! # let picture = aureole::RgbaImage::new(1, 1, vec![0, 0, 0, 255]);
//! clipboard::write(ClipboardContent::new().image(picture).text("一张图"));
//!
//! if clipboard::formats().contains(&ClipFormat::Image) {
//!     let picture = clipboard::read_image();
//! #   let _ = picture;
//! }
//! // 读出来的图片直接给 image()
//! let shown = clipboard::read_image().map(|picture| image(picture));
//! # let _ = shown;
//! ```
//!
//! 应用自己的数据（图层、节点、表格的几行……）放成自己定的格式，同时放一份纯文字给别的程序：
//!
//! ```no_run
//! use aureole::clipboard::{self, ClipboardContent};
//!
//! clipboard::write(ClipboardContent::new().custom("com.example.layers", br#"{"layers":[1,2]}"#.to_vec()).text("2 个图层"));
//! if let Some(bytes) = clipboard::read_custom("com.example.layers") {
//! #   let _ = bytes;
//! }
//! ```
//!
//! 各平台能放哪些格式见 [`crate::Capabilities`]（`supports_clipboard_*`）：macOS、Windows 都能，
//! Linux 上一次只放一种（文件、图片、文字里最优先的），没有带格式的文字（只剩纯文字），放不了应用自己定的格式。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aureole_core::{Color, RgbaImage, Span, SpanInfo};
use platform::clipboard::rtf::{self, RtfRun};

pub use platform::ClipFormat;

/// 读不出来时记一条调试日志，当成没有
fn logged<T>(what: &str, result: Result<T, platform::PlatformError>) -> Option<T> {
    result.map_err(|error| tracing::debug!("读不了剪贴板的{what}：{error}")).ok()
}

/// 一次放进剪贴板的内容：同一份内容的几种表示（比如同一张图的图片和文字说明），接收的程序挑自己认的。
/// [`read`] 读出来的也是它
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipboardContent {
    /// 纯文字
    pub text: Option<String>,
    /// 带格式的文字；空的是没有
    pub rich_text: Vec<SpanInfo>,
    /// 图片
    pub image: Option<RgbaImage>,
    /// 文件
    pub files: Vec<PathBuf>,
    /// 应用自己定的格式：格式名和原样的字节，见 [`ClipboardContent::custom`]
    pub custom: Vec<(String, Vec<u8>)>,
}

impl ClipboardContent {
    /// 什么都没有的内容：放进去就是清空剪贴板
    pub fn new() -> Self {
        Self::default()
    }

    /// 纯文字
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// 带格式的文字：一串 [`Span`]（`&str`、`String`、[`span()`](aureole_core::span) 做的都行）。
    /// 没给纯文字时，纯文字就是这些段连起来：只认文字的程序也贴得出来
    pub fn rich_text(mut self, spans: impl IntoIterator<Item = impl Into<Span>>) -> Self {
        self.rich_text = spans.into_iter().map(|span| span.into().info()).collect();
        self
    }

    /// 图片
    pub fn image(mut self, image: impl Into<RgbaImage>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// 文件
    pub fn files(mut self, files: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        self.files = files.into_iter().map(|file| file.as_ref().to_path_buf()).collect();
        self
    }

    /// 应用自己定的格式，可以放几种：格式名用反向域名（`com.example.layers`），字节原样放进去、原样读出来（[`read_custom`]）。
    /// macOS 上格式名就是剪贴板的类型，Windows 上向系统登记成剪贴板格式（名字相同的程序之间也能交换）。
    /// 同一个格式名放两次时后放的算数
    pub fn custom(mut self, format: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        let format = format.into();
        self.custom.retain(|(name, _)| *name != format);
        self.custom.push((format, bytes.into()));
        self
    }
}

fn to_run(info: &SpanInfo) -> RtfRun {
    RtfRun {
        text: info.text.clone(),
        // RTF 只有粗体一档：600 起算粗体
        bold: info.font_weight.is_some_and(|weight| weight >= 600.0),
        italic: info.italic,
        underline: info.underline,
        strikethrough: info.strikethrough,
        monospaced: info.code,
        font_size: info.font_size,
        color: info.color.map(|Color([r, g, b, _])| [r, g, b]),
        font_family: info.font_family.clone(),
        link: info.link.clone(),
    }
}

fn from_run(run: RtfRun) -> SpanInfo {
    SpanInfo {
        text: run.text,
        font_size: run.font_size,
        font_weight: run.bold.then_some(700.0),
        color: run.color.map(|[r, g, b]| Color::rgb(r, g, b)),
        italic: run.italic,
        underline: run.underline,
        strikethrough: run.strikethrough,
        code: run.monospaced,
        font_family: run.font_family,
        letter_spacing: None,
        link: run.link,
    }
}

fn to_platform(image: &RgbaImage) -> platform::RgbaImage {
    platform::RgbaImage { width: image.width, height: image.height, rgba: Arc::clone(&image.pixels) }
}

fn from_platform(image: platform::RgbaImage) -> RgbaImage {
    RgbaImage { width: image.width, height: image.height, pixels: image.rgba }
}

fn spans_text(spans: &[SpanInfo]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// 剪贴板里的文字；没有文字或读不了时是 `None`
pub fn read_text() -> Option<String> {
    crate::system::install();
    aureole_core::clipboard::read_text()
}

/// 把文字放进剪贴板，返回是否成功
pub fn write_text(text: impl Into<String>) -> bool {
    crate::system::install();
    aureole_core::clipboard::write_text(text)
}

/// 现在剪贴板上有哪些格式，按文件、图片、带格式的文字、纯文字排。读不了时是空的。
/// 剪贴板是用户的，别的程序随时会改：查到有也可能下一刻就没了，读的时候还是要处理 `None`
pub fn formats() -> Vec<ClipFormat> {
    logged("格式", platform::with_backend(|backend| backend.clipboard().formats())).unwrap_or_default()
}

/// 剪贴板里带格式的文字，一段一段的；没有时是 `None`。保留得下来的样式：粗体（字重 700）、斜体、下划线、删除线、字号、颜色、
/// 字体、等宽（当成行内代码）、链接。放进富文本：`spans.into_iter().fold(rich_text(), |el, span| el.span(span))`
pub fn read_rich_text() -> Option<Vec<Span>> {
    read_rich_infos().map(|infos| infos.into_iter().map(Span::from).collect())
}

/// RTF 换成一段一段的（拖进来的带格式的文字也用它）
pub(crate) fn spans_from_rtf(bytes: &[u8]) -> Vec<SpanInfo> {
    rtf::parse(bytes).into_iter().map(from_run).collect()
}

fn read_rich_infos() -> Option<Vec<SpanInfo>> {
    let bytes = logged("带格式的文字", platform::with_backend(|backend| backend.clipboard().read_rich_text())).flatten()?;
    Some(rtf::parse(&bytes).into_iter().map(from_run).collect())
}

/// 把带格式的文字放进剪贴板（同时放它的纯文字），返回是否成功。平台不支持带格式的文字时只放纯文字
pub fn write_rich_text(spans: impl IntoIterator<Item = impl Into<Span>>) -> bool {
    write(ClipboardContent::new().rich_text(spans))
}

/// 剪贴板里的图片，能直接给 [`image()`](aureole_core::image)；没有时是 `None`。
/// 访达、资源管理器复制文件时可能带着文件的图标：同时有文件时先看 [`read_files`]
pub fn read_image() -> Option<RgbaImage> {
    logged("图片", platform::with_backend(|backend| backend.clipboard().read_image())).flatten().map(from_platform)
}

/// 把图片放进剪贴板，返回是否成功。像素数和宽高对不上时不动剪贴板，返回 `false`
pub fn write_image(image: &RgbaImage) -> bool {
    write(ClipboardContent::new().image(image.clone()))
}

/// 剪贴板里的文件（绝对路径）；没有时是空的
pub fn read_files() -> Vec<PathBuf> {
    logged("文件", platform::with_backend(|backend| backend.clipboard().read_files())).unwrap_or_default()
}

/// 把文件放进剪贴板（用户能在访达、资源管理器里粘贴），返回是否成功。相对路径按当前目录换成绝对路径；不检查文件存不存在
pub fn write_files(files: impl IntoIterator<Item = impl AsRef<Path>>) -> bool {
    write(ClipboardContent::new().files(files))
}

/// 剪贴板里应用自己定的格式 `format` 的字节（见 [`ClipboardContent::custom`]）；没有这种格式、平台不支持时是 `None`
pub fn read_custom(format: &str) -> Option<Vec<u8>> {
    logged("应用自己定的格式", platform::with_backend(|backend| backend.clipboard().read_custom(format))).flatten()
}

/// 剪贴板此刻的纯文字、带格式的文字、图片、文件。应用自己定的格式不在里面（读的时候要知道名字，用 [`read_custom`]）：
/// 原样放回去时别的程序的私有格式就没了
pub fn read() -> ClipboardContent {
    ClipboardContent {
        text: read_text(),
        rich_text: read_rich_infos().unwrap_or_default(),
        image: read_image(),
        files: read_files(),
        custom: Vec::new(),
    }
}

/// 换掉剪贴板的内容，一次放几种，返回是否成功；什么都没有的内容是清空。
/// 平台一次只能放一种时（Linux）放最优先的一种：文件、图片、文字
pub fn write(content: ClipboardContent) -> bool {
    let content = platform_content(content.text, &content.rich_text, content.image.as_ref(), &content.files, content.custom);
    platform::with_backend(|backend| backend.clipboard().write(&content)).map_err(|error| tracing::warn!("写不进剪贴板：{error}")).is_ok()
}

/// 交给平台的内容：没给纯文字时用带格式文字的纯文字，带格式的文字编成 RTF，相对路径按当前目录换成绝对路径
fn platform_content(text: Option<String>, rich_text: &[SpanInfo], image: Option<&RgbaImage>, files: &[PathBuf], custom: Vec<(String, Vec<u8>)>) -> platform::ClipboardContent {
    let plain = text.or_else(|| (!rich_text.is_empty()).then(|| spans_text(rich_text)));
    let rich = (!rich_text.is_empty()).then(|| rtf::write(&rich_text.iter().map(to_run).collect::<Vec<_>>()));
    let files = files.iter().map(|file| std::path::absolute(file).unwrap_or_else(|_| file.clone())).collect();
    platform::ClipboardContent { text: plain, rich_text: rich, image: image.map(to_platform), files, custom }
}

/// 拖到应用外面的内容换成交给平台的（和剪贴板同一套表示）
pub(crate) fn drag_content(out: &aureole_core::DragOut) -> platform::ClipboardContent {
    platform_content(out.text_content().map(str::to_owned), out.rich_text_content(), out.image_content(), out.paths(), out.custom_content().to_vec())
}
