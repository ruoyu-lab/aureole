//! 中文、日文按词分：双击选词、Option+方向键、Option+退格在中日文里按词走（可选中的文字和输入框都是）。
//!
//! parley 找词边界用的 ICU4X 数据里没有中日文词典，中日文每个字都算一个词。让 parley 排版时就带上词典
//! （它的 `complex-scripts` 特性）的话，每次排中文都要多查一遍词典，实测排版慢 5%～10%，二进制大 3.1 MB；
//! 这里改成用到时（双击、按词移动和删除）才把插入点附近的一串中日文交给 ICU4X 的中日文词典分词，排版的速度不变，
//! 只带中日文的词典（东南亚文字的词典不带）。词典在默认打开的 `cjk-words` 特性里，关掉时退回按字。
//!
//! 英文等其他文字的按词规则不变：只有插入点旁边（双击处）的字是汉字、平假名、片假名时才走这里。

use std::ops::Range;

/// 插入点前后各看这么多个字：分词只看附近，几万字的一段里按键也很快
const WINDOW: usize = 64;

/// 要查词典才分得出词的字：汉字、平假名、片假名（含长音符、叠字符、半角片假名）
/// 字素簇的边界（字节偏移，含两端）：组合的表情符号、组合附加符号算一个。用的是排版已经编进来的 ICU4X 分割器
pub(crate) fn graphemes(text: &str) -> impl Iterator<Item = usize> + '_ {
    icu_segmenter::GraphemeClusterSegmenter::new().segment_str(text)
}

pub(crate) fn is_ideographic(c: char) -> bool {
    matches!(c as u32,
        0x3005..=0x3007          // 々〆〇
        | 0x3040..=0x30FA        // 平假名、片假名（不含中点 ・）
        | 0x30FC..=0x30FF        // ー ヽ ヾ ヿ
        | 0x31F0..=0x31FF        // 片假名音标扩展
        | 0x3400..=0x4DBF        // 扩展 A
        | 0x4E00..=0x9FFF        // 基本区
        | 0xF900..=0xFAFF        // 兼容汉字
        | 0xFF66..=0xFF9F        // 半角片假名
        | 0x20000..=0x323AF      // 扩展 B 及以后
    )
}

/// 包含 `index` 处这个字的词。这个字不是中日文（或者没带词典）时返回 `None`，交给 parley
pub(crate) fn word_at(text: &str, index: usize) -> Option<Range<usize>> {
    let c = text.get(index..)?.chars().next()?;
    if !is_ideographic(c) {
        return None;
    }
    let bounds = boundaries(text, index)?;
    let end = bounds.iter().copied().find(|&b| b > index)?;
    let start = bounds.iter().copied().rev().find(|&b| b <= index)?;
    Some(start..end)
}

/// 往后按词走：`index` 后面的字是中日文时，到它所在的词的末尾
pub(crate) fn next_word_end(text: &str, index: usize) -> Option<usize> {
    word_at(text, index).map(|word| word.end)
}

/// 往前按词走：`index` 前面的字是中日文时，到它所在的词的开头
pub(crate) fn previous_word_start(text: &str, index: usize) -> Option<usize> {
    let (before, _) = text.get(..index)?.char_indices().next_back()?;
    word_at(text, before).map(|word| word.start)
}

/// `index` 所在的那一串中日文（前后最多各 [`WINDOW`] 个字）里的词边界，全文的字节偏移，含两端
fn boundaries(text: &str, index: usize) -> Option<Vec<usize>> {
    let mut start = index;
    for (i, c) in text[..index].char_indices().rev().take(WINDOW) {
        if !is_ideographic(c) {
            break;
        }
        start = i;
    }
    let mut end = index;
    for (i, c) in text[index..].char_indices().take(WINDOW) {
        if !is_ideographic(c) {
            break;
        }
        end = index + i + c.len_utf8();
    }
    let run = &text[start..end];
    Some(dictionary::segment(run)?.into_iter().map(|b| start + b).collect())
}

#[cfg(feature = "cjk-words")]
mod dictionary {
    use icu_provider::prelude::*;
    use icu_segmenter::WordSegmenter;
    use icu_segmenter::options::WordBreakOptions;
    use icu_segmenter::provider::{
        Baked, SegmenterBreakGraphemeClusterV1, SegmenterBreakWordOverrideV1, SegmenterBreakWordV1, SegmenterDictionaryAutoV2,
        SegmenterDictionaryExtendedV2,
    };

    /// 只给中日文词典的数据源：分词规则和中日文词典用 ICU4X 编译进来的数据，东南亚文字的词典说没有（不链接进来）
    struct ChineseJapanese;

    macro_rules! baked {
        ($($marker:ty),*) => {
            $(impl DataProvider<$marker> for ChineseJapanese {
                fn load(&self, req: DataRequest) -> Result<DataResponse<$marker>, DataError> {
                    Baked.load(req)
                }
            })*
        };
    }

    baked!(SegmenterBreakWordV1, SegmenterBreakWordOverrideV1, SegmenterDictionaryAutoV2, SegmenterBreakGraphemeClusterV1);

    impl DataProvider<SegmenterDictionaryExtendedV2> for ChineseJapanese {
        fn load(&self, req: DataRequest) -> Result<DataResponse<SegmenterDictionaryExtendedV2>, DataError> {
            Err(DataErrorKind::IdentifierNotFound.with_req(SegmenterDictionaryExtendedV2::INFO, req))
        }
    }

    thread_local! {
        /// 第一次用到时创建：数据是编译进来的静态数据，不用读文件、不用解析
        static SEGMENTER: Option<WordSegmenter> =
            WordSegmenter::try_new_dictionary_unstable(&ChineseJapanese, WordBreakOptions::default()).ok();
    }

    /// 词边界（字节偏移，含两端）
    pub(super) fn segment(run: &str) -> Option<Vec<usize>> {
        SEGMENTER.with(|segmenter| Some(segmenter.as_ref()?.as_borrowed().segment_str(run).collect()))
    }
}

#[cfg(not(feature = "cjk-words"))]
mod dictionary {
    /// 没带词典：按字
    pub(super) fn segment(_run: &str) -> Option<Vec<usize>> {
        None
    }
}
