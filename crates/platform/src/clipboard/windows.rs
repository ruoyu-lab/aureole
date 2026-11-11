//! Windows 的剪贴板：一次放几种格式、带格式的文字、位图、文件列表都直接调系统（纯文字照旧走 arboard）。
//! 二进制格式怎么写、怎么读在 [`super::dib`] 里；PNG 用系统的图片编解码（WIC）。
//!
//! - 写：所有格式先准备好（分配内存、编码图片），都成了才清空剪贴板、放进去，哪一步失败都不动用户原来的内容。
//!   文字的换行写成 `\r\n`（Windows 的程序认这个）；图片放三份：PNG（办公软件、绘图软件认它，带透明度）、
//!   `CF_DIBV5`（带透明度）、`CF_DIB`（叠在白底上，给不认透明度的老程序）。
//! - 读：图片优先 PNG（有的程序复制的图片透明度只在它里面）；拷出数据、关上剪贴板以后再解码，不让别的程序等着。
//!   窄字符的文件名按系统的代码页（中文系统上是 GBK）换成 UTF-16。

use std::borrow::Cow;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use windows::Win32::Foundation::{E_FAIL, HGLOBAL};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppRGBA, IWICBitmapFrameEncode, IWICImagingFactory,
    IWICPalette, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoUninitialize, STREAM_SEEK_CUR};
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};

use super::{ClipFormat, ClipboardContent, RgbaImage, dib};
use crate::PlatformError;

/// 系统格式名：带格式文字的交换格式
const RTF_FORMAT_NAME: &str = "Rich Text Format";
/// 程序之间约定的格式名：PNG 文件的字节
const PNG_FORMAT_NAME: &str = "PNG";
/// 一张图最多多少字节（解码出来的 RGBA）：坏数据不让这里分配大块内存
const MAX_IMAGE_BYTES: usize = 1 << 30;

/// 向系统登记过的格式编号（第一次用时登记，之后不变）
fn registered(name: &str, slot: &OnceLock<u32>) -> u32 {
    *slot.get_or_init(|| {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` 以 0 结尾，调用期间活着
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
    })
}

/// 应用自己定的格式的编号：按名字向系统登记（同一个名字在所有程序里是同一个编号）。名字是空的时是 `None`
pub(crate) fn custom_format(name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` 以 0 结尾，调用期间活着
    let format = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
    (format != 0).then_some(format)
}

/// 带格式文字的格式编号（剪贴板和拖放共用）
pub(crate) fn rtf_format() -> u32 {
    static FORMAT: OnceLock<u32> = OnceLock::new();
    registered(RTF_FORMAT_NAME, &FORMAT)
}

/// PNG 的格式编号（剪贴板和拖放共用）
pub(crate) fn png_format() -> u32 {
    static FORMAT: OnceLock<u32> = OnceLock::new();
    registered(PNG_FORMAT_NAME, &FORMAT)
}

/// 开着的剪贴板，离开作用域时关上
struct OpenBoard;

impl OpenBoard {
    /// 别的程序可能正开着（同一时刻只能有一个程序开着）：隔一会儿再试几次
    fn new() -> Result<Self, PlatformError> {
        for _ in 0..20 {
            // SAFETY: 空窗口句柄：剪贴板归这个线程
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Self);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Err(PlatformError::System("剪贴板被别的程序占着".into()))
    }
}

impl Drop for OpenBoard {
    fn drop(&mut self) {
        // SAFETY: 和 new 里成功的 OpenClipboard 配对
        unsafe { CloseClipboard() };
    }
}

fn available(format: u32) -> bool {
    // SAFETY: 只收整数，不用开着剪贴板
    unsafe { IsClipboardFormatAvailable(format) != 0 }
}

/// 全局内存块里的内容拷出来
///
/// # Safety
/// `handle` 是有效的全局内存句柄（或者空）
pub(crate) unsafe fn global_bytes(handle: HANDLE) -> Option<Vec<u8>> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: 调用方保证句柄有效；锁住以后到解锁之前指针有效，有 `GlobalSize` 个字节
    unsafe {
        let pointer = GlobalLock(handle).cast::<u8>();
        if pointer.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(pointer, GlobalSize(handle)).to_vec();
        GlobalUnlock(handle);
        Some(bytes)
    }
}

/// 自己分配的一块全局内存（可移动的），内容是给的字节。交给系统（[`Global::release`]）之前释放时自己还掉
pub(crate) struct Global(HANDLE);

impl Global {
    /// 分配并拷进 `bytes`；分配不了时是 `None`
    pub(crate) fn new(bytes: &[u8]) -> Option<Self> {
        // SAFETY: 新分配的内存块至少 `bytes.len()` 字节，锁住时指针有效，和 `bytes` 不重叠；锁不住时还掉
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
            if handle.is_null() {
                return None;
            }
            let pointer = GlobalLock(handle).cast::<u8>();
            if pointer.is_null() {
                GlobalFree(handle);
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            GlobalUnlock(handle);
            Some(Self(handle))
        }
    }

    /// 句柄（还归这里）
    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    /// 系统收下了：句柄归系统，这里不再还
    pub(crate) fn release(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // SAFETY: 句柄是 `new` 分配的、还没交出去，只还一次
        unsafe { GlobalFree(self.0) };
    }
}

/// 这种格式的内容（要开着剪贴板）
fn read_bytes(_open: &OpenBoard, format: u32) -> Option<Vec<u8>> {
    // SAFETY: 剪贴板开着；返回的句柄归系统，只读不释放，关剪贴板之前有效
    unsafe { global_bytes(GetClipboardData(format)) }
}

/// 现在剪贴板上有哪些格式
pub fn formats() -> Vec<ClipFormat> {
    let mut formats = Vec::new();
    if available(u32::from(CF_HDROP)) {
        formats.push(ClipFormat::Files);
    }
    if available(png_format()) || available(u32::from(CF_DIBV5)) || available(u32::from(CF_DIB)) {
        formats.push(ClipFormat::Image);
    }
    if available(rtf_format()) {
        formats.push(ClipFormat::RichText);
    }
    if available(u32::from(CF_UNICODETEXT)) {
        formats.push(ClipFormat::Text);
    }
    formats
}

/// 剪贴板上的带格式文字
pub fn read_rich_text() -> Result<Option<Vec<u8>>, PlatformError> {
    if !available(rtf_format()) {
        return Ok(None);
    }
    let bytes = read_bytes(&OpenBoard::new()?, rtf_format());
    // 系统的内存块按页取整，内容以 0 结束：后面的不算
    Ok(bytes.map(|mut bytes| {
        if let Some(end) = bytes.iter().position(|b| *b == 0) {
            bytes.truncate(end);
        }
        bytes
    }))
}

/// 剪贴板上的图片：优先 PNG，其次带透明度的位图（系统在两种位图之间自动转换）。关上剪贴板以后再解码
pub fn read_image() -> Result<Option<RgbaImage>, PlatformError> {
    if available(png_format()) {
        let png = read_bytes(&OpenBoard::new()?, png_format());
        if let Some(image) = png.and_then(|bytes| decode_png(&bytes)) {
            return Ok(Some(image));
        }
    }
    let format = if available(u32::from(CF_DIBV5)) {
        CF_DIBV5
    } else if available(u32::from(CF_DIB)) {
        CF_DIB
    } else {
        return Ok(None);
    };
    let bytes = read_bytes(&OpenBoard::new()?, u32::from(format));
    Ok(bytes.and_then(|bytes| dib::decode(&bytes)))
}

/// 剪贴板上应用自己定的格式，原样的字节
pub fn read_custom(name: &str) -> Result<Option<Vec<u8>>, PlatformError> {
    let Some(format) = custom_format(name).filter(|format| available(*format)) else { return Ok(None) };
    Ok(read_bytes(&OpenBoard::new()?, format))
}

/// 剪贴板上的文件
pub fn read_files() -> Result<Vec<PathBuf>, PlatformError> {
    if !available(u32::from(CF_HDROP)) {
        return Ok(Vec::new());
    }
    let bytes = read_bytes(&OpenBoard::new()?, u32::from(CF_HDROP));
    Ok(bytes.map_or_else(Vec::new, |bytes| paths_from_list(&bytes)))
}

/// `CF_HDROP` 的内容换成路径（窄字符的按系统的代码页）
pub(crate) fn paths_from_list(bytes: &[u8]) -> Vec<PathBuf> {
    dib::decode_file_list_with(bytes, ansi_to_wide).iter().map(|path| PathBuf::from(OsString::from_wide(path))).collect()
}

/// 换掉剪贴板的内容，每种格式放一份。所有格式先准备好，都成了才清空剪贴板：失败时用户原来的内容还在
pub fn write(content: &ClipboardContent) -> Result<(), PlatformError> {
    if let Some(image) = &content.image
        && !image.is_valid()
    {
        return Err(PlatformError::System(format!("图片的像素数不对：{}×{}，{} 字节", image.width, image.height, image.rgba.len()).into()));
    }
    let files = content
        .files
        .iter()
        .map(|path| std::path::absolute(path).map(|path| crate::system::windows::shell_path(&path).as_os_str().encode_wide().collect::<Vec<u16>>()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(PlatformError::from)?;
    let mut prepared: Vec<(u32, Global)> = Vec::new();
    let mut add = |format: u32, bytes: &[u8]| -> Result<(), PlatformError> {
        let global = Global::new(bytes).ok_or_else(|| PlatformError::System("内存不够，放不进剪贴板".into()))?;
        prepared.push((format, global));
        Ok(())
    };
    if let Some(text) = &content.text {
        let wide: Vec<u8> = to_crlf(text).encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        add(u32::from(CF_UNICODETEXT), &wide)?;
    }
    if let Some(rich) = &content.rich_text {
        let mut bytes = rich.clone();
        bytes.push(0);
        add(rtf_format(), &bytes)?;
    }
    if let Some(image) = &content.image {
        // 编码不了 PNG（没有图片编解码组件）时只放位图
        if let Some(png) = encode_png(image) {
            add(png_format(), &png)?;
        }
        add(u32::from(CF_DIBV5), &dib::encode(image))?;
        add(u32::from(CF_DIB), &dib::encode_opaque(image))?;
    }
    if !files.is_empty() {
        add(u32::from(CF_HDROP), &dib::encode_file_list(&files))?;
    }
    for (name, bytes) in &content.custom {
        let format = custom_format(name).ok_or_else(|| PlatformError::System(format!("登记不了剪贴板格式「{name}」").into()))?;
        add(format, bytes)?;
    }
    let _open = OpenBoard::new()?;
    // SAFETY: 剪贴板开着
    if unsafe { EmptyClipboard() } == 0 {
        return Err(PlatformError::System("系统没能清空剪贴板".into()));
    }
    for (format, global) in prepared {
        // SAFETY: 剪贴板开着；成功后内存块归系统（交出所有权），失败时还归这里，`global` 释放时还掉
        if unsafe { SetClipboardData(format, global.handle()) }.is_null() {
            return Err(PlatformError::System("系统没能写进剪贴板".into()));
        }
        global.release();
    }
    Ok(())
}

/// 换行写成 `\r\n`（Windows 的程序认这个）：单独的 `\n` 前面补上 `\r`，已经是 `\r\n` 的不动
pub(crate) fn to_crlf(text: &str) -> Cow<'_, str> {
    let bare = text.bytes().enumerate().any(|(i, b)| b == b'\n' && (i == 0 || text.as_bytes()[i - 1] != b'\r'));
    if !bare {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + text.len() / 16 + 1);
    let mut previous = '\0';
    for c in text.chars() {
        if c == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(c);
        previous = c;
    }
    Cow::Owned(out)
}

/// 代码页的字节换成 UTF-16：`MultiByteToWideChar`。代码页不认识、内容为空时是 `None`
pub(crate) fn code_page_to_wide(code_page: u32, bytes: &[u8]) -> Option<Vec<u16>> {
    let length = i32::try_from(bytes.len()).ok().filter(|l| *l > 0)?;
    // SAFETY: 输入的指针和长度对得上；第一次只问要多长，第二次写进刚分配的缓冲区
    unsafe {
        let needed = MultiByteToWideChar(code_page, 0, bytes.as_ptr(), length, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(code_page, 0, bytes.as_ptr(), length, wide.as_mut_ptr(), needed);
        (written > 0).then(|| {
            wide.truncate(written as usize);
            wide
        })
    }
}

/// 系统代码页（"非 Unicode 程序的语言"，中文系统上是 GBK）的字节换成 UTF-16：老程序的文件名、链接用它。
/// 换不了时按单字节展开
pub(crate) fn ansi_to_wide(bytes: &[u8]) -> Vec<u16> {
    code_page_to_wide(CP_ACP, bytes).unwrap_or_else(|| bytes.iter().map(|&b| u16::from(b)).collect())
}

/// 代码页的字节换成文字：`MultiByteToWideChar`
pub fn decode_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    code_page_to_wide(code_page, bytes).map(|wide| String::from_utf16_lossy(&wide))
}

/// 这一段代码要用 COM（图片编解码）：线程上还没有时初始化（单线程套间），用完配对释放；
/// 已经是多线程套间（`RPC_E_CHANGED_MODE`）也能用，什么都不做
struct ComScope(bool);

impl ComScope {
    fn enter() -> Self {
        // SAFETY: 和 Drop 里的 CoUninitialize 配对（只在成功时）
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self(hr.is_ok())
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 和 enter 里成功的 CoInitializeEx 配对
            unsafe { CoUninitialize() };
        }
    }
}

fn imaging_factory() -> windows::core::Result<IWICImagingFactory> {
    // SAFETY: 调用方初始化了 COM；类编号是系统的图片编解码工厂
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// 编码成 PNG（系统的图片编解码，直通 alpha）；不行时记一条、返回 `None`
pub(crate) fn encode_png(image: &RgbaImage) -> Option<Vec<u8>> {
    let _com = ComScope::enter();
    let stride = image.width.checked_mul(4)?;
    // SAFETY: COM 在上面初始化好了
    unsafe { encode_png_with_com(image, stride) }.map_err(|error| tracing::warn!("编码不了 PNG：{error}")).ok()
}

/// # Safety
/// 这个线程上已经初始化了 COM；`stride` 是 `image.width × 4`
unsafe fn encode_png_with_com(image: &RgbaImage, stride: u32) -> windows::core::Result<Vec<u8>> {
    // SAFETY: 调用方初始化了 COM；对象只在这个函数里、这个线程上用；像素缓冲区有 stride × height 个字节
    unsafe {
        let factory = imaging_factory()?;
        let stream = CreateStreamOnHGlobal(HGLOBAL::default(), true)?;
        let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut options: Option<IPropertyBag2> = None;
        encoder.CreateNewFrame(&mut frame, &mut options)?;
        let frame = frame.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        frame.Initialize(options.as_ref())?;
        frame.SetSize(image.width, image.height)?;
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format)?;
        if format != GUID_WICPixelFormat32bppBGRA {
            return Err(E_FAIL.into());
        }
        let bgra: Vec<u8> = image.rgba.as_chunks::<4>().0.iter().flat_map(|[r, g, b, a]| [*b, *g, *r, *a]).collect();
        frame.WritePixels(image.height, stride, &bgra)?;
        frame.Commit()?;
        encoder.Commit()?;
        // 写到了哪：编码好的字节数（内存块按页取整，后面的不算）
        let mut size = 0u64;
        stream.Seek(0, STREAM_SEEK_CUR, Some(&raw mut size))?;
        let global = GetHGlobalFromStream(&stream)?;
        let mut bytes = global_bytes(global.0).ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        bytes.truncate(usize::try_from(size).unwrap_or(usize::MAX));
        Ok(bytes)
    }
}

/// 解码 PNG（或者系统认识的其他图片格式）成直通 alpha 的 RGBA；坏数据、太大的图是 `None`
pub(crate) fn decode_png(bytes: &[u8]) -> Option<RgbaImage> {
    let _com = ComScope::enter();
    // SAFETY: COM 在上面初始化好了；流直接读 `bytes`，`bytes` 活到函数结束、比解码器长；输出缓冲区有 stride × height 个字节
    unsafe {
        let factory = imaging_factory().ok()?;
        let stream = factory.CreateStream().ok()?;
        stream.InitializeFromMemory(bytes).ok()?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter.Initialize(&frame, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None::<&IWICPalette>, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let (mut width, mut height) = (0u32, 0u32);
        converter.GetSize(&mut width, &mut height).ok()?;
        let len = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        if len == 0 || len > MAX_IMAGE_BYTES {
            return None;
        }
        let mut rgba = vec![0u8; len];
        converter.CopyPixels(std::ptr::null(), width * 4, &mut rgba).ok()?;
        Some(RgbaImage { width, height, rgba: Arc::from(rgba) })
    }
}
