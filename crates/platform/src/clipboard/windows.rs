//! Windows 的剪贴板：一次放几种格式、带格式的文字、位图、文件列表都直接调系统（纯文字照旧走 arboard）。
//! 二进制格式怎么写、怎么读在 [`super::dib`] 里。

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::Globalization::MultiByteToWideChar;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};

use super::{ClipFormat, ClipboardContent, RgbaImage, dib};
use crate::PlatformError;

/// 系统格式名：带格式文字的交换格式
const RTF_FORMAT_NAME: &str = "Rich Text Format";

/// 带格式文字的格式编号（第一次用时向系统登记，之后不变）
fn rtf_format() -> u32 {
    static FORMAT: OnceLock<u32> = OnceLock::new();
    *FORMAT.get_or_init(|| {
        let name: Vec<u16> = RTF_FORMAT_NAME.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `name` 以 0 结尾，调用期间活着
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    })
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

/// 这种格式的内容（要开着剪贴板）
fn read_bytes(_open: &OpenBoard, format: u32) -> Option<Vec<u8>> {
    // SAFETY: 剪贴板开着；返回的句柄归系统，只读不释放，关剪贴板之前有效
    unsafe { global_bytes(GetClipboardData(format)) }
}

/// 放进一种格式：系统收下内存块，收不下时自己释放
fn write_bytes(_open: &OpenBoard, format: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    let failed = || PlatformError::System("系统没能写进剪贴板".into());
    // SAFETY: 新分配的内存块至少 `bytes.len()` 字节，锁住时指针有效，和 `bytes` 不重叠；
    // SetClipboardData 成功后内存块归系统，失败时还归这里，释放一次
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if handle.is_null() {
            return Err(failed());
        }
        let pointer = GlobalLock(handle).cast::<u8>();
        if pointer.is_null() {
            GlobalFree(handle);
            return Err(failed());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        GlobalUnlock(handle);
        if SetClipboardData(format, handle).is_null() {
            GlobalFree(handle);
            return Err(failed());
        }
    }
    Ok(())
}

/// 现在剪贴板上有哪些格式
pub fn formats() -> Vec<ClipFormat> {
    let mut formats = Vec::new();
    if available(u32::from(CF_HDROP)) {
        formats.push(ClipFormat::Files);
    }
    if available(u32::from(CF_DIBV5)) || available(u32::from(CF_DIB)) {
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
    let open = OpenBoard::new()?;
    // 系统的内存块按页取整，内容以 0 结束：后面的不算
    Ok(read_bytes(&open, rtf_format()).map(|mut bytes| {
        if let Some(end) = bytes.iter().position(|b| *b == 0) {
            bytes.truncate(end);
        }
        bytes
    }))
}

/// 剪贴板上的图片。优先带透明度的版本；系统在两种格式之间自动转换
pub fn read_image() -> Result<Option<RgbaImage>, PlatformError> {
    let format = if available(u32::from(CF_DIBV5)) {
        CF_DIBV5
    } else if available(u32::from(CF_DIB)) {
        CF_DIB
    } else {
        return Ok(None);
    };
    let open = OpenBoard::new()?;
    Ok(read_bytes(&open, u32::from(format)).and_then(|bytes| dib::decode(&bytes)))
}

/// 剪贴板上的文件
pub fn read_files() -> Result<Vec<PathBuf>, PlatformError> {
    if !available(u32::from(CF_HDROP)) {
        return Ok(Vec::new());
    }
    let open = OpenBoard::new()?;
    Ok(read_bytes(&open, u32::from(CF_HDROP)).map_or_else(Vec::new, |bytes| paths_from_list(&bytes)))
}

/// `CF_HDROP` 的内容换成路径
pub(crate) fn paths_from_list(bytes: &[u8]) -> Vec<PathBuf> {
    dib::decode_file_list(bytes).iter().map(|path| PathBuf::from(OsString::from_wide(path))).collect()
}

/// 换掉剪贴板的内容，每种格式放一份
pub fn write(content: &ClipboardContent) -> Result<(), PlatformError> {
    // 先把会失败的都做完，失败时不动用户原来的内容
    if let Some(image) = &content.image
        && !image.is_valid()
    {
        return Err(PlatformError::System(format!("图片的像素数不对：{}×{}，{} 字节", image.width, image.height, image.rgba.len()).into()));
    }
    let files = content
        .files
        .iter()
        .map(|path| std::path::absolute(path).map(|path| path.as_os_str().encode_wide().collect::<Vec<u16>>()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(PlatformError::from)?;
    let open = OpenBoard::new()?;
    // SAFETY: 剪贴板开着
    if unsafe { EmptyClipboard() } == 0 {
        return Err(PlatformError::System("系统没能清空剪贴板".into()));
    }
    if let Some(text) = &content.text {
        let wide: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        write_bytes(&open, u32::from(CF_UNICODETEXT), &wide)?;
    }
    if let Some(rich) = &content.rich_text {
        let mut bytes = rich.clone();
        bytes.push(0);
        write_bytes(&open, rtf_format(), &bytes)?;
    }
    if let Some(image) = &content.image {
        write_bytes(&open, u32::from(CF_DIBV5), &dib::encode(image))?;
    }
    if !files.is_empty() {
        write_bytes(&open, u32::from(CF_HDROP), &dib::encode_file_list(&files))?;
    }
    Ok(())
}

/// 代码页的字节换成文字：`MultiByteToWideChar`
pub fn decode_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    let length = i32::try_from(bytes.len()).ok()?;
    // SAFETY: 输入的指针和长度对得上；第一次只问要多长，第二次写进刚分配的缓冲区
    unsafe {
        let needed = MultiByteToWideChar(code_page, 0, bytes.as_ptr(), length, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut wide = vec![0u16; needed as usize];
        let written = MultiByteToWideChar(code_page, 0, bytes.as_ptr(), length, wide.as_mut_ptr(), needed);
        (written > 0).then(|| String::from_utf16_lossy(&wide[..written as usize]))
    }
}
