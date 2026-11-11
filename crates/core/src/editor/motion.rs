//! 位置怎么走：输入框和选择（选择根、单独一段可选中的文字，见 `tree/selection.rs`）共用的规则。
//!
//! 这里有两样：哪个键是往哪走（修饰键按平台惯例，[`Motion::of_key`]），一段排好的文字之内怎么走（交给 parley 的
//! `Cursor` / `Selection`，中日文按词查词典）。走到段的边上之后去哪归调用的人：输入框到文档的下一段
//! （跨过换行符算一步，插入点看得见），选择到下一段文字（要选区变了才算一步，那里没有插入点）。

use parley::{Affinity, Cluster, Cursor, Layout, Selection};

use crate::event::KeyEvent;
use crate::input::{Key, NamedKey};
use crate::segmentation;
use crate::text::SpanId;

/// 按键要往哪走。"范围"在输入框里是整个文档，在选择里是选择根（或者那一段文字）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Motion {
    /// 往左（`word`：到词首）
    Left { word: bool },
    /// 往右（`word`：到词尾）
    Right { word: bool },
    /// 上一行，横坐标保持
    Up,
    /// 下一行，横坐标保持
    Down,
    /// 到段首；已经在段首时到上一段的段首
    ParagraphUp,
    /// 到段尾；已经在段尾时到下一段的段尾
    ParagraphDown,
    LineStart,
    LineEnd,
    RangeStart,
    RangeEnd,
}

impl Motion {
    /// 这个键（连同修饰键）是不是移动键，按平台惯例：按词的修饰键按词走、上下走到段首段尾；
    /// 主修饰键加方向键到行首行尾、范围首尾（macOS），或者 Home、End 到行首行尾、主修饰键加它们到范围首尾（其他平台）。
    /// Shift 不看：扩不扩展选区是调用的人的事
    pub fn of_key(event: &KeyEvent) -> Option<Self> {
        let Key::Named(key) = event.key else { return None };
        if !matches!(key, NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::Home | NamedKey::End) {
            return None;
        }
        let conventions = crate::system::conventions();
        let primary = event.has_primary_modifier();
        let word = event.modifiers.intersects(conventions.word_modifier);
        let edge = primary && conventions.uses_arrows_for_line_edges;
        let range = primary && !conventions.uses_arrows_for_line_edges;
        Some(match key {
            NamedKey::ArrowLeft if edge => Self::LineStart,
            NamedKey::ArrowRight if edge => Self::LineEnd,
            NamedKey::ArrowLeft => Self::Left { word },
            NamedKey::ArrowRight => Self::Right { word },
            NamedKey::ArrowUp if edge => Self::RangeStart,
            NamedKey::ArrowDown if edge => Self::RangeEnd,
            NamedKey::ArrowUp if word => Self::ParagraphUp,
            NamedKey::ArrowDown if word => Self::ParagraphDown,
            NamedKey::ArrowUp => Self::Up,
            NamedKey::ArrowDown => Self::Down,
            NamedKey::Home if range => Self::RangeStart,
            NamedKey::End if range => Self::RangeEnd,
            NamedKey::Home => Self::LineStart,
            _ => Self::LineEnd,
        })
    }
}

/// 往右一个字（`word` 时到词尾，中日文按词典分的词；平台惯例是到下一个词的开头时（Windows），再跳过词后面的空白）；
/// 走不动（到了这一段的右边）时原样返回
pub(crate) fn right(layout: &Layout<SpanId>, text: &str, from: Cursor, word: bool) -> Cursor {
    if !word {
        return from.next_visual(layout);
    }
    let skips = crate::system::conventions().word_forward_skips_spaces && !layout.is_rtl();
    // 后面就是空白（上一次停在了词尾）：跳过空白就是下一个词的开头
    if skips && let Some(start) = after_spaces(text, from.index()) {
        return Cursor::from_byte_index(layout, start, Affinity::Downstream);
    }
    let end = word_end(layout, text, from);
    match after_spaces(text, end.index()).filter(|_| skips) {
        Some(start) => Cursor::from_byte_index(layout, start, Affinity::Downstream),
        None => end,
    }
}

/// `index` 处是空白时，跳过这一串空白到的位置（跨过换行就停在换行后面：下一行的开头）；不是空白时是 `None`。
/// 按词往后走到下一个词的开头（[`crate::Conventions::word_forward_skips_spaces`]）用
pub(crate) fn after_spaces(text: &str, index: usize) -> Option<usize> {
    let mut end = index;
    for c in text.get(index..)?.chars() {
        if !c.is_whitespace() {
            break;
        }
        end += c.len_utf8();
        if c == '\n' {
            break;
        }
    }
    (end > index).then_some(end)
}

/// 往右到词尾（中日文按词典分的词）
fn word_end(layout: &Layout<SpanId>, text: &str, from: Cursor) -> Cursor {
    if !layout.is_rtl()
        && let Some(end) = segmentation::next_word_end(text, from.index())
    {
        return Cursor::from_byte_index(layout, end, Affinity::Upstream);
    }
    to_word_edge(
        layout,
        from,
        |cursor| cursor.next_visual(layout),
        |left, right| {
            let boundary = if left.is_rtl() { left.is_word_boundary() } else { right.is_word_boundary() };
            boundary && !left.is_space_or_nbsp()
        },
    )
}

/// 往左一个字（`word` 时到词首，中日文按词典分的词）；走不动（到了这一段的左边）时原样返回
pub(crate) fn left(layout: &Layout<SpanId>, text: &str, from: Cursor, word: bool) -> Cursor {
    if word
        && !layout.is_rtl()
        && let Some(start) = segmentation::previous_word_start(text, from.index())
    {
        return Cursor::from_byte_index(layout, start, Affinity::Downstream);
    }
    if !word {
        return from.previous_visual(layout);
    }
    to_word_edge(
        layout,
        from,
        |cursor| cursor.previous_visual(layout),
        |left, right| {
            let word_after = right.is_word_boundary() && !right.is_space_or_nbsp();
            if left.is_rtl() { left.is_word_boundary() && (left.is_space_or_nbsp() || word_after) } else { word_after }
        },
    )
}

/// 一个字一个字走到词的边上。判断和 parley 的 `next_visual_word`、`previous_visual_word` 一样，只多一条：转圈了就停。
///
/// 那两个方法只在走不动时停。从右到左的字正好在折行处结束时，按视觉顺序走到这一行的边上会回到这串字里面，
/// 它们就永远出不来了，界面卡死。转圈时只走一个字，和不按词走时一样（走不出去是不按词走本来就有的，见现存限制）。
///
/// 查转圈不用存走过的位置：每隔 1、2、4、8… 步记一个，又碰到记下的那个就是转圈。不分配，走多远都是线性的。
/// `测试：word_moves_end_where_visual_moves_go_round`
fn to_word_edge(
    layout: &Layout<SpanId>,
    from: Cursor,
    step: impl Fn(Cursor) -> Cursor,
    stop: impl Fn(&Cluster<'_, SpanId>, &Cluster<'_, SpanId>) -> bool,
) -> Cursor {
    let (mut now, mut mark) = (from, from);
    let (mut since_mark, mut mark_every) = (0u32, 1u32);
    loop {
        let next = step(now);
        if next == now {
            return now;
        }
        if next == mark {
            return step(from);
        }
        now = next;
        let [Some(left), Some(right)] = now.visual_clusters(layout) else { return now };
        if stop(&left, &right) {
            return now;
        }
        since_mark += 1;
        if since_mark == mark_every {
            (mark, since_mark, mark_every) = (now, 0, mark_every.saturating_mul(2));
        }
    }
}

/// 所在（折行后的）这一行的开头
pub(crate) fn line_start(layout: &Layout<SpanId>, from: Cursor) -> Cursor {
    Selection::from(from).line_start(layout, false).focus()
}

/// 所在这一行的末尾
pub(crate) fn line_end(layout: &Layout<SpanId>, from: Cursor) -> Cursor {
    Selection::from(from).line_end(layout, false).focus()
}

/// 这个位置在第几行（按它的纵向中点找）
pub(crate) fn line_index(layout: &Layout<SpanId>, cursor: Cursor) -> usize {
    let rect = cursor.geometry(layout, 0.0);
    line_at_y(layout, ((rect.y0 + rect.y1) * 0.5) as f32)
}

/// 纵坐标 `y` 在第几行
pub(crate) fn line_at_y(layout: &Layout<SpanId>, y: f32) -> usize {
    let lines = layout.len();
    (0..lines).find(|&i| layout.get(i).is_some_and(|l| y < l.metrics().block_max_coord)).unwrap_or(lines.saturating_sub(1))
}

/// 这个位置的横坐标（相对文字原点）。parley 给的矩形不算每行自己的起点（绕排时行从旁边的元素后面开始），这里加上
pub(crate) fn x_of(layout: &Layout<SpanId>, cursor: Cursor) -> f32 {
    let start = layout.get(line_index(layout, cursor)).map_or(0.0, |line| line.metrics().inline_min_coord);
    cursor.geometry(layout, 0.0).x0 as f32 + start
}

/// 第 `line` 行上横坐标 `x` 处的位置（上下移动的落点）
pub(crate) fn on_line(layout: &Layout<SpanId>, line: usize, x: f32) -> Option<Cursor> {
    let target = layout.get(line)?;
    let metrics = target.metrics();
    Some(Cursor::from_point(layout, x, (metrics.block_min_coord + metrics.block_max_coord) * 0.5))
}
