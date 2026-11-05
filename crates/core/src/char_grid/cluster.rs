//! 字符网格的字素簇：怎么切、占几列。
//!
//! 切分用 [`crate::segmentation::graphemes`]（和排版同一套 ICU4X 规则）。查规则比查一张表慢得多，而终端、代码编辑器里
//! 绝大多数是 ASCII 和普通汉字，所以先过一道 [`simple_width`]：这些字旁边也是这类字时，每个字自己就是一个簇，不用查规则。
//!
//! 列宽（[`cluster_width`]）：`unicode-width` 按整个字符串算的宽度，最多 2。它认得表情的组合序列（ZWJ、肤色、VS16 都是 2）；
//! 变音符号、连接用的零宽字符算 0，不增加宽度；两个地区指示符各 1，合起来正好 2。

use unicode_width::UnicodeWidthStr;

use crate::segmentation::graphemes;

/// 一个簇最多留这么多字节，多出来的组合字符丢掉。最长的标准表情序列（带肤色的情侣、亲吻）35 字节；
/// 不设上限的话，一个程序往同一格里不停写变音符号，这一格的文字就会一直长
pub(super) const MAX_CLUSTER: usize = 64;

/// 这个字旁边也是这类字时，它肯定自己是一个簇：返回它的列宽。别的字（组合用的、表情、别的文字）是 `None`，要查规则。
///
/// 只收常见的几段，收多了这里本身变慢。它们的字素类别都是"其他"（汉字、假名、拉丁字母……）或者谚文的整字音节：
/// 互相之间不连，只和后面的组合字符连，而组合字符不在这张表里（走到它时查规则）。`测试：simple_characters_are_clusters_with_their_width`
/// 逐个核对
#[inline]
pub(super) fn simple_width(ch: char) -> Option<usize> {
    simple_width_of_code(ch as u32)
}

/// [`simple_width`]，收的是码点（格子里存的内容不用先转成字；宽字的第二格和簇的下标都不在表里）
#[inline]
pub(super) fn simple_width_of_code(code: u32) -> Option<usize> {
    if code < 0x300 {
        return matches!(code, 0x20..=0x7e | 0xa0..=0xac | 0xae..=0x2ff).then_some(1);
    }
    // 最常见的基本区汉字先判断
    if (0x4e00..=0x9fff).contains(&code) {
        return Some(2);
    }
    match code {
        // 别的汉字、谚文音节、中日文标点、假名、全角字符
        0x3400..=0x4dbf | 0xac00..=0xd7a3 | 0x3000..=0x3029 | 0x3041..=0x3096 | 0x30a1..=0x30fa | 0xff01..=0xff60 | 0xffe0..=0xffe6 => Some(2),
        // 希腊字母、西里尔字母、标点、箭头、数学符号、制表符和方块
        0x370..=0x482 | 0x48a..=0x52f | 0x2010..=0x2027 | 0x2190..=0x22ff | 0x2500..=0x259f => Some(1),
        _ => None,
    }
}

/// `text` 开头第一个字素簇的字节数
pub(super) fn first_cluster_len(text: &str) -> usize {
    graphemes(text).nth(1).unwrap_or(text.len())
}

/// 簇占几列，0 是不占格（控制字符、单独的零宽字符）
pub(super) fn cluster_width(cluster: &str) -> usize {
    if cluster.chars().next().is_none_or(char::is_control) {
        return 0;
    }
    cluster.width().min(2)
}

/// 去掉超过 [`MAX_CLUSTER`] 的部分（不切开一个字）
pub(super) fn clamped(cluster: &str) -> &str {
    if cluster.len() <= MAX_CLUSTER {
        return cluster;
    }
    let end = (0..=MAX_CLUSTER).rev().find(|&i| cluster.is_char_boundary(i)).unwrap_or(0);
    &cluster[..end]
}
