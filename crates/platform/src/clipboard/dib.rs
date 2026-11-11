//! Windows 剪贴板和拖放里的两种二进制格式，不调用系统，在任何平台上都能测试：
//! 位图（`CF_DIBV5`、`CF_DIB`）和文件列表（`CF_HDROP` 的 `DROPFILES`）。

use std::sync::Arc;

use super::RgbaImage;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const V5_HEADER: usize = 124;
const INFO_HEADER: usize = 40;
/// 一张图最大多少字节：坏数据不让这里分配大块内存
const MAX_BYTES: usize = 1 << 30;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(bytes: &[u8], at: usize) -> Option<i32> {
    u32_at(bytes, at).map(|v| v as i32)
}

/// 写成 `BITMAPV5HEADER` 加像素：从下往上一行行排，32 位 BGRA，直通 alpha，sRGB
pub fn encode(image: &RgbaImage) -> Vec<u8> {
    let (width, height) = (image.width as usize, image.height as usize);
    let mut out = Vec::with_capacity(V5_HEADER + width * height * 4);
    out.extend_from_slice(&(V5_HEADER as u32).to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // 平面数
    out.extend_from_slice(&32u16.to_le_bytes()); // 每像素位数
    out.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
    out.extend_from_slice(&((width * height * 4) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]); // 分辨率、调色板
    for mask in [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
        out.extend_from_slice(&mask.to_le_bytes());
    }
    out.extend_from_slice(b"BGRs"); // LCS_sRGB，按小端读是 'sRGB'
    out.extend_from_slice(&[0; 36 + 12]); // 色度坐标、伽马：sRGB 不用
    out.extend_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES
    out.extend_from_slice(&[0; 12]); // 配置文件的位置、大小，保留
    debug_assert_eq!(out.len(), V5_HEADER);
    if width > 0 {
        for row in image.rgba.chunks_exact(width * 4).rev() {
            for [r, g, b, a] in row.as_chunks::<4>().0 {
                out.extend_from_slice(&[*b, *g, *r, *a]);
            }
        }
    }
    out
}

/// 写成老的 `BITMAPINFOHEADER` 加像素（`CF_DIB`）：24 位、从下往上、每行补到 4 字节，透明的地方叠在白底上。
/// 不认透明度的老程序读它：透明处是白的，不是黑的；认透明度的读 `CF_DIBV5` 或 PNG
pub fn encode_opaque(image: &RgbaImage) -> Vec<u8> {
    let (width, height) = (image.width as usize, image.height as usize);
    let stride = (width * 3).div_ceil(4) * 4;
    let mut out = Vec::with_capacity(INFO_HEADER + stride * height);
    out.extend_from_slice(&(INFO_HEADER as u32).to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // 平面数
    out.extend_from_slice(&24u16.to_le_bytes()); // 每像素位数
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&((stride * height) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]); // 分辨率、调色板
    debug_assert_eq!(out.len(), INFO_HEADER);
    // 叠在白底上：c·a + 255·(1 − a)
    let over_white = |c: u8, a: u8| ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a)) + 127) / 255) as u8;
    if width > 0 {
        for row in image.rgba.chunks_exact(width * 4).rev() {
            for [r, g, b, a] in row.as_chunks::<4>().0 {
                out.extend_from_slice(&[over_white(*b, *a), over_white(*g, *a), over_white(*r, *a)]);
            }
            out.extend(std::iter::repeat_n(0, stride - width * 3));
        }
    }
    out
}

/// 读 `CF_DIB`、`CF_DIBV5`：头部（`BITMAPINFOHEADER` 和它后来的几个版本）紧跟着像素。认 24 位、32 位；
/// 别的位深（带调色板的、16 位）是 `None`
pub fn decode(dib: &[u8]) -> Option<RgbaImage> {
    let header = u32_at(dib, 0)? as usize;
    if header < INFO_HEADER {
        return None;
    }
    let width = i32_at(dib, 4)?;
    let raw_height = i32_at(dib, 8)?;
    let bits = u16_at(dib, 14)?;
    let compression = u32_at(dib, 16)?;
    let palette = u32_at(dib, 32)? as usize;
    if width <= 0 || raw_height == 0 || !matches!(bits, 24 | 32) || !matches!(compression, BI_RGB | BI_BITFIELDS) {
        return None;
    }
    let top_down = raw_height < 0;
    let (width, height) = (width as usize, raw_height.unsigned_abs() as usize);
    let stride = (width.checked_mul(usize::from(bits))?.checked_add(31)? / 32).checked_mul(4)?;
    if stride.checked_mul(height)? > MAX_BYTES {
        return None;
    }
    // 掩码：V4、V5 的在头部里；老的头部用 BI_BITFIELDS 时紧跟在头部后面（三个，没有 alpha）
    let mut masks = [0x00ff_0000u32, 0x0000_ff00, 0x0000_00ff, 0];
    let mut pixels_at = header;
    if compression == BI_BITFIELDS {
        let (at, count) = if header >= INFO_HEADER + 16 {
            (INFO_HEADER, 4)
        } else if header >= INFO_HEADER + 12 {
            (INFO_HEADER, 3)
        } else {
            pixels_at += 12;
            (INFO_HEADER, 3)
        };
        for (index, mask) in masks.iter_mut().take(count).enumerate() {
            *mask = u32_at(dib, at + index * 4)?;
        }
    }
    pixels_at = pixels_at.checked_add(palette.checked_mul(4)?)?;
    let data = dib.get(pixels_at..)?;
    if data.len() < stride.checked_mul(height)? {
        return None;
    }

    let channel = |value: u32, mask: u32| -> u8 {
        if mask == 0 {
            return 0;
        }
        let shifted = (value & mask) >> mask.trailing_zeros();
        let max = mask >> mask.trailing_zeros();
        if max == 255 { shifted as u8 } else { (u64::from(shifted) * 255 / u64::from(max)) as u8 }
    };
    let mut rgba = vec![0u8; width * height * 4];
    let mut any_alpha = false;
    for y in 0..height {
        let source = &data[(if top_down { y } else { height - 1 - y }) * stride..];
        let target = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for (x, pixel) in target.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (r, g, b, a) = if bits == 24 {
                (source[x * 3 + 2], source[x * 3 + 1], source[x * 3], 255)
            } else {
                let value = u32::from_le_bytes([source[x * 4], source[x * 4 + 1], source[x * 4 + 2], source[x * 4 + 3]]);
                // 老格式的第四个字节没有定义：有掩码才算透明度
                (channel(value, masks[0]), channel(value, masks[1]), channel(value, masks[2]), channel(value, masks[3]))
            };
            any_alpha |= a != 0;
            *pixel = [r, g, b, a];
        }
    }
    // 没有透明度的图（掩码是空的，或者全是 0）当不透明
    if !any_alpha {
        rgba.as_chunks_mut::<4>().0.iter_mut().for_each(|pixel| pixel[3] = 255);
    }
    Some(RgbaImage { width: width as u32, height: height as u32, rgba: Arc::from(rgba) })
}

/// `DROPFILES` 的大小：文件名从这里开始
const DROP_FILES_HEADER: usize = 20;

/// 写成 `CF_HDROP` 的内容：`DROPFILES` 加上一串以 0 结尾的 UTF-16 路径，最后再多一个 0
pub fn encode_file_list(paths: &[Vec<u16>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(DROP_FILES_HEADER as u32).to_le_bytes()); // 文件名的位置
    out.extend_from_slice(&[0; 12]); // 放下的位置、是不是非客户区
    out.extend_from_slice(&1u32.to_le_bytes()); // 宽字符
    for path in paths {
        out.extend(path.iter().flat_map(|unit| unit.to_le_bytes()));
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// 读 `CF_HDROP` 的内容：每个路径一串 UTF-16（窄字符的老格式按单字节展开，见 [`decode_file_list_with`]）
pub fn decode_file_list(bytes: &[u8]) -> Vec<Vec<u16>> {
    decode_file_list_with(bytes, |path| path.iter().map(|&byte| u16::from(byte)).collect())
}

/// 读 `CF_HDROP` 的内容，窄字符的老格式（系统的代码页，中文系统上是 GBK）每个路径交给 `narrow` 换成 UTF-16
pub fn decode_file_list_with(bytes: &[u8], narrow: impl Fn(&[u8]) -> Vec<u16>) -> Vec<Vec<u16>> {
    let Some(at) = u32_at(bytes, 0).map(|at| at as usize) else { return Vec::new() };
    let wide = u32_at(bytes, 16).is_some_and(|wide| wide != 0);
    let Some(list) = bytes.get(at..) else { return Vec::new() };
    if wide {
        let units: Vec<u16> = list.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).collect();
        units.split(|unit| *unit == 0).take_while(|path| !path.is_empty()).map(<[u16]>::to_vec).collect()
    } else {
        // 双字节的代码页里 0 不会出现在字的第二个字节，按 0 切开是对的
        list.split(|byte| *byte == 0).take_while(|path| !path.is_empty()).map(narrow).collect()
    }
}
