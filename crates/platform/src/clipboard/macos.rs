//! macOS 的剪贴板：`NSPasteboard`。纯文字以外的格式、一次放几种都走这里。
//! 从 `NSPasteboard` 读内容的几个函数拖放也用（拖放的数据也在一块剪贴板上）。

use std::path::PathBuf;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AllocAnyThread, msg_send};
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageFileType, NSBitmapImageRep, NSColorSpace, NSDeviceRGBColorSpace, NSGraphicsContext, NSPasteboard, NSPasteboardItem,
    NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeString,
    NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSPoint, NSRect, NSSize, NSString, NSURL, ns_string};

use super::{ClipFormat, ClipboardContent, RgbaImage};
use crate::PlatformError;

/// 剪贴板上有没有这种类型
pub(crate) fn has_type(pasteboard: &NSPasteboard, kind: &NSPasteboardType) -> bool {
    pasteboard.availableTypeFromArray(&NSArray::from_slice(&[kind])).is_some()
}

fn jpeg_type() -> &'static NSPasteboardType {
    ns_string!("public.jpeg")
}

/// 剪贴板上有图片（文件的图标也算：访达复制文件时带着图标）
pub(crate) fn has_image(pasteboard: &NSPasteboard) -> bool {
    // SAFETY: 这几个类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (png, tiff) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    has_type(pasteboard, png) || has_type(pasteboard, tiff) || has_type(pasteboard, jpeg_type())
}

/// 这块剪贴板上的文件
pub(crate) fn files_on(pasteboard: &NSPasteboard) -> Vec<PathBuf> {
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let kind = unsafe { NSPasteboardTypeFileURL };
    let Some(items) = pasteboard.pasteboardItems() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| item.stringForType(kind))
        .filter_map(|url| NSURL::URLWithString(&url))
        // 文件引用形式（`file:///.file/id=…`）先换成路径形式
        .filter_map(|url| url.filePathURL().unwrap_or(url).to_file_path())
        .collect()
}

/// 这块剪贴板上的图片，换成 sRGB 的 RGBA。优先用 PNG：无损，透明度不会预乘过
pub(crate) fn image_on(pasteboard: &NSPasteboard) -> Option<RgbaImage> {
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (png, tiff) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    let data = pasteboard.dataForType(png).or_else(|| pasteboard.dataForType(tiff)).or_else(|| pasteboard.dataForType(jpeg_type()))?;
    let rep = NSBitmapImageRep::imageRepWithData(&data)?;
    rgba_of(&rep)
}

/// 已经是 sRGB、8 位、直通透明度的 RGBA（框架自己写的 PNG 就是）：不用转换，一个字节不差
fn straight_srgb(rep: &NSBitmapImageRep) -> Option<RgbaImage> {
    let format = rep.bitmapFormat();
    let plain = format == NSBitmapFormat::AlphaNonpremultiplied;
    if !plain || rep.bitsPerPixel() != 32 || rep.samplesPerPixel() != 4 || rep.isPlanar() || rep.colorSpace() != NSColorSpace::sRGBColorSpace() {
        return None;
    }
    let (width, height) = (rep.pixelsWide() as usize, rep.pixelsHigh() as usize);
    let stride = rep.bytesPerRow() as usize;
    let data = rep.bitmapData();
    if width == 0 || height == 0 || data.is_null() || stride < width * 4 {
        return None;
    }
    // SAFETY: `bitmapData` 指向 `bytesPerRow × 高` 个字节（不是平面格式），`rep` 活着，没有别人在写
    let source = unsafe { std::slice::from_raw_parts(data, stride * height) };
    let rgba: Vec<u8> = source.chunks_exact(stride).flat_map(|row| &row[..width * 4]).copied().collect();
    Some(RgbaImage { width: width as u32, height: height as u32, rgba: Arc::from(rgba) })
}

/// 画进一张 sRGB、8 位的位图再读出来：系统按图片自己的色彩配置转换，别的位深、平面格式也一起换好
fn rgba_of(rep: &NSBitmapImageRep) -> Option<RgbaImage> {
    if let Some(image) = straight_srgb(rep) {
        return Some(image);
    }
    let (width, height) = (rep.pixelsWide(), rep.pixelsHigh());
    if width <= 0 || height <= 0 {
        return None;
    }
    // 绘图上下文只接受预乘的位图
    let target = new_bitmap(width, height, NSBitmapFormat::empty())?;
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&target)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let bounds = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width as f64, height as f64));
    let drawn = rep.drawInRect(bounds);
    NSGraphicsContext::restoreGraphicsState_class();
    if !drawn {
        return None;
    }
    let (width, height) = (width as usize, height as usize);
    let stride = target.bytesPerRow() as usize;
    let data = target.bitmapData();
    if data.is_null() || stride < width * 4 {
        return None;
    }
    // SAFETY: 位图是上面建的 8 位 4 通道，`bitmapData` 指向 `bytesPerRow × 高` 个字节，`target` 活着，没有别人在写
    let source = unsafe { std::slice::from_raw_parts(data, stride * height) };
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in source.chunks_exact(stride) {
        for [r, g, b, a] in row[..width * 4].as_chunks::<4>().0 {
            let alpha = u32::from(*a);
            let straight = |c: u8| (u32::from(c) * 255 + alpha / 2).checked_div(alpha).map_or(0, |v| v.min(255) as u8);
            rgba.extend_from_slice(&[straight(*r), straight(*g), straight(*b), *a]);
        }
    }
    Some(RgbaImage { width: width as u32, height: height as u32, rgba: Arc::from(rgba) })
}

/// 新建一张 8 位 RGBA 的位图，色彩空间标成 sRGB
fn new_bitmap(width: isize, height: isize, format: NSBitmapFormat) -> Option<Retained<NSBitmapImageRep>> {
    // SAFETY: 平面指针是空：位图自己分配像素；每像素 32 位，每行字节数和宽度一致；色彩空间名是 AppKit 导出的常量
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            format,
            width * 4,
            32,
        )
    }?;
    rep.bitmapImageRepByRetaggingWithColorSpace(&NSColorSpace::sRGBColorSpace())
}

/// 同一张图的 PNG、TIFF 数据
fn encode_image(image: &RgbaImage) -> Result<(Retained<NSData>, Retained<NSData>), PlatformError> {
    if !image.is_valid() {
        return Err(PlatformError::System(format!("图片的像素数不对：{}×{}，{} 字节", image.width, image.height, image.rgba.len()).into()));
    }
    let failed = || PlatformError::System("系统没能把图片编码出来".into());
    let rep = new_bitmap(image.width as isize, image.height as isize, NSBitmapFormat::AlphaNonpremultiplied).ok_or_else(failed)?;
    let stride = rep.bytesPerRow() as usize;
    let data = rep.bitmapData();
    let width = image.width as usize;
    if data.is_null() || stride < width * 4 {
        return Err(failed());
    }
    for (y, row) in image.rgba.chunks_exact(width * 4).enumerate() {
        // SAFETY: 位图是上面建的，有 `bytesPerRow × 高` 个字节，`y` 小于高，一行写 `width × 4 ≤ bytesPerRow` 个字节；`row` 不和它重叠
        unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), data.add(y * stride), row.len()) };
    }
    // SAFETY: 空字典：没有要设的属性
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }.ok_or_else(failed)?;
    let tiff = rep.TIFFRepresentation().ok_or_else(failed)?;
    Ok((png, tiff))
}

fn file_url(path: &std::path::Path) -> Result<Retained<NSString>, PlatformError> {
    let absolute = std::path::absolute(path).map_err(PlatformError::from)?;
    NSURL::from_file_path(&absolute)
        .and_then(|url| url.absoluteString())
        .ok_or_else(|| PlatformError::System(format!("路径换不成网址：{}", absolute.display()).into()))
}

fn general() -> Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

/// 现在剪贴板上有哪些格式
pub fn formats() -> Vec<ClipFormat> {
    let pasteboard = general();
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (files, rich, string) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeRTF, NSPasteboardTypeString) };
    let mut formats = Vec::new();
    if has_type(&pasteboard, files) {
        formats.push(ClipFormat::Files);
    }
    if has_image(&pasteboard) {
        formats.push(ClipFormat::Image);
    }
    if has_type(&pasteboard, rich) {
        formats.push(ClipFormat::RichText);
    }
    if has_type(&pasteboard, string) {
        formats.push(ClipFormat::Text);
    }
    formats
}

/// 剪贴板上的带格式文字（RTF）
pub fn read_rich_text() -> Option<Vec<u8>> {
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    general().dataForType(unsafe { NSPasteboardTypeRTF }).map(|data| data.to_vec())
}

/// 剪贴板上的图片
pub fn read_image() -> Option<RgbaImage> {
    image_on(&general())
}

/// 剪贴板上的文件
pub fn read_files() -> Vec<PathBuf> {
    files_on(&general())
}

/// 换掉剪贴板的内容。文字、带格式的文字、图片和第一个文件放在同一项上，别的文件各占一项（`NSPasteboard` 的文件列表是一项一个网址）
pub fn write(content: &ClipboardContent) -> Result<(), PlatformError> {
    // 先把会失败的都做完，失败时不动用户原来的内容
    let image = content.image.as_ref().map(encode_image).transpose()?;
    let files = content.files.iter().map(|path| file_url(path)).collect::<Result<Vec<_>, _>>()?;
    let pasteboard = general();
    pasteboard.clearContents();
    if content.formats().is_empty() {
        return Ok(());
    }
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (string, rich, png, tiff, file) =
        unsafe { (NSPasteboardTypeString, NSPasteboardTypeRTF, NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeFileURL) };
    let first = NSPasteboardItem::new();
    if let Some(text) = &content.text {
        first.setString_forType(&NSString::from_str(text), string);
    }
    if let Some(data) = &content.rich_text {
        first.setData_forType(&NSData::with_bytes(data), rich);
    }
    if let Some((png_data, tiff_data)) = &image {
        first.setData_forType(png_data, png);
        first.setData_forType(tiff_data, tiff);
    }
    let mut files = files.into_iter();
    if let Some(url) = files.next() {
        first.setString_forType(&url, file);
    }
    let mut items = vec![ProtocolObject::<dyn NSPasteboardWriting>::from_retained(first)];
    for url in files {
        let item = NSPasteboardItem::new();
        item.setString_forType(&url, file);
        items.push(ProtocolObject::from_retained(item));
    }
    if !pasteboard.writeObjects(&NSArray::from_retained_slice(&items)) {
        return Err(PlatformError::System("系统没能写进剪贴板".into()));
    }
    // 内容是系统来要的时候才交出去：每一项读一遍，都交出去了才算写好。不然写完马上退出的进程只留下第一项（或者什么都不留）
    for item in pasteboard.pasteboardItems().iter().flatten() {
        for kind in item.types() {
            let _ = item.dataForType(&kind);
        }
    }
    Ok(())
}

unsafe extern "C" {
    /// CoreFoundation：Windows 代码页 → `CFStringEncoding`；不认识时是 `kCFStringEncodingInvalidId`
    fn CFStringConvertWindowsCodepageToEncoding(code_page: u32) -> u32;
    /// CoreFoundation：`CFStringEncoding` → `NSStringEncoding`
    fn CFStringConvertEncodingToNSStringEncoding(encoding: u32) -> usize;
}

/// 代码页的字节换成文字（`\'hh` 里的中文大多是 GBK、Big5）：用系统的转换表
pub fn decode_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    const INVALID: u32 = 0xffff_ffff;
    // SAFETY: 两个都是纯函数，只收整数
    let encoding = unsafe {
        let encoding = match code_page {
            // 不是 Windows 的代码页：kCFStringEncodingMacRoman
            10000 => 0,
            page => CFStringConvertWindowsCodepageToEncoding(page),
        };
        if encoding == INVALID {
            return None;
        }
        CFStringConvertEncodingToNSStringEncoding(encoding)
    };
    let data = NSData::with_bytes(bytes);
    // SAFETY: `data` 活着；`initWithData:encoding:` 转不了时返回空，不会读越界
    let string: Option<Retained<NSString>> = unsafe { msg_send![NSString::alloc(), initWithData: &*data, encoding: encoding] };
    string.map(|string| string.to_string())
}
