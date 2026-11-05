//! 富文本里的行内元素（[`crate::Span::inline`]）在文字这一边的部分：排版里的占位盒子、盒子的位置、跨过盒子的选区和复制。
//! 元素本身是元素树里的节点（见 `tree/inline.rs`），这里只知道它占多大、要怎么对齐、排在第几段。
//!
//! 全文里每个行内元素占一个**占位字符**（零宽空格），盒子排在它后面。选区的两端因此分得清"元素前"和"元素后"，
//! 占位字符后面可以断行，所以放不下的元素整个挪到下一行，它前面的字留在这一行。占位字符只在排版用的全文里：
//! [`TextBlock::content`]、复制出来的文字里都没有它。

use std::ops::Range;

use parley::{Affinity, BoundingBox, Cursor, InlineBox, InlineBoxKind, Layout, Line, PositionedLayoutItem, RangedBuilder, VerticalAlign};
use scene::Color;

use super::{Rich, SpanId, TextBlock, TextSystem, span_selection};
use crate::editor::motion;
use crate::geometry::Rect;
use crate::tree::NodeId;

/// 行内元素在全文里占的字符：零宽，后面可以断行
pub(crate) const PLACEHOLDER: &str = "\u{200B}";

/// 行内元素在一行里怎么对齐（[`crate::Element::inline_align`]）。元素高出一行时，行高被它撑开
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum InlineAlign {
    /// 元素的基线和文字的基线对齐：有文字的元素（按钮）按它的第一行字，别的（图片）是底边
    #[default]
    Baseline,
    /// 元素的中线和小写字母的中间对齐：图标放在字中间
    Center,
    /// 元素的顶和这一段字的顶（上伸）对齐
    TextTop,
    /// 元素的底和这一段字的底（下伸）对齐
    TextBottom,
    /// 元素的顶和这一行的顶对齐
    Top,
    /// 元素的底和这一行的底对齐
    Bottom,
}

impl InlineAlign {
    fn parley(self) -> VerticalAlign {
        match self {
            Self::Baseline => VerticalAlign::BASELINE,
            Self::Center => VerticalAlign::MIDDLE,
            Self::TextTop => VerticalAlign::TEXT_TOP,
            Self::TextBottom => VerticalAlign::TEXT_BOTTOM,
            Self::Top => VerticalAlign::TOP,
            Self::Bottom => VerticalAlign::BOTTOM,
        }
    }
}

/// 占位盒子要多大、怎么放：来自元素自己的布局结果
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct InlineMetrics {
    /// 外边距盒的大小
    pub size: [f32; 2],
    /// 基线，相对外边距盒的顶；没有基线时是底边
    pub baseline: Option<f32>,
    pub align: InlineAlign,
}

/// 一个行内元素：排在第几段、哪个节点、占位盒子的大小，排版之后的位置
pub(crate) struct InlineSlot {
    pub span: usize,
    pub node: NodeId,
    pub metrics: InlineMetrics,
    /// 外边距盒的左上角，相对文字原点，对齐物理像素
    pub at: [f32; 2],
    /// 它占的那一列：盒子的横向范围、所在那一行的高度，相对文字原点（选区、点在哪）
    pub column: Rect,
    /// 排出来了：截断时被截掉的是 false，不显示、不响应
    pub shown: bool,
    /// 复制时代替它的文字（复制之前由元素树写）
    pub copy: String,
}

impl InlineSlot {
    pub fn new(span: usize, node: NodeId) -> Self {
        Self { span, node, metrics: InlineMetrics::default(), at: [0.0; 2], column: Rect::default(), shown: true, copy: String::new() }
    }
}

/// 富文本里的行内元素
#[derive(Default)]
pub(super) struct Inlines {
    slots: Vec<InlineSlot>,
    /// 没有占位字符的全文（[`TextBlock::content`] 读它）
    plain: String,
    /// 量行内元素时给的宽度：文字内容区的宽度，还不知道时是 `None`（不限）
    available: Option<f32>,
    /// 选区画在行内元素上的颜色（绘制文字时记下）
    selection_fill: Option<Color>,
    /// 按这个窗口缩放读过位置（没读过时是 0）：排版没变时不用再读
    placed_scale: f32,
}

impl Rich {
    /// 行内元素，按在全文里的先后
    pub fn inlines(&self) -> &[InlineSlot] {
        &self.inlines.slots
    }

    /// 第 `span` 段是行内元素时的节点
    pub fn inline_of_span(&self, span: usize) -> Option<NodeId> {
        self.inlines.slots.iter().find(|slot| slot.span == span).map(|slot| slot.node)
    }

    /// 量行内元素时给的宽度
    pub fn inline_available(&self) -> Option<f32> {
        self.inlines.available
    }

    pub fn set_inline_available(&mut self, width: f32) -> bool {
        self.inlines.available.replace(width) != Some(width)
    }

    pub fn set_inline_copy(&mut self, k: usize, copy: String) {
        if let Some(slot) = self.inlines.slots.get_mut(k) {
            slot.copy = copy;
        }
    }

    /// 没有占位字符的全文；没有行内元素时是 `None`（全文就是）
    pub(super) fn plain(&self) -> Option<&str> {
        (!self.inlines.slots.is_empty()).then_some(self.inlines.plain.as_str())
    }

    /// 全文变了：重新去掉占位字符
    pub(super) fn refresh_plain(&mut self, content: &str) {
        if self.inlines.slots.is_empty() {
            return;
        }
        let mut plain = String::with_capacity(content.len());
        let mut at = 0;
        for slot in &self.inlines.slots {
            let range = &self.ranges[slot.span];
            plain.push_str(&content[at..range.start]);
            at = range.end;
        }
        plain.push_str(&content[at..]);
        self.inlines.plain = plain;
    }

    /// 把占位盒子交给排版：只放占位字符在前 `limit` 个字节里的（截断时）
    pub(super) fn push_inline_boxes(&self, builder: &mut RangedBuilder<'_, SpanId>, limit: usize) {
        for (k, slot) in self.inlines.slots.iter().enumerate() {
            let index = self.ranges[slot.span].end;
            if index > limit {
                break;
            }
            builder.push_inline_box(inline_box(k, index, &slot.metrics));
        }
    }

    /// 第 `k` 个行内元素在全文里占的字节范围
    fn inline_range(&self, k: usize) -> Range<usize> {
        self.ranges[self.inlines.slots[k].span].clone()
    }
}

fn inline_box(k: usize, index: usize, metrics: &InlineMetrics) -> InlineBox {
    let [width, height] = metrics.size;
    InlineBox { id: k as u64, kind: InlineBoxKind::InFlow, index, width, height, baseline: metrics.baseline, vertical_align: metrics.align.parley() }
}

impl TextBlock {
    /// 有行内元素的富文本（普通文字、没有行内元素的富文本走到这里只多这一次判断）
    pub(super) fn with_inlines(&self) -> Option<&Rich> {
        self.rich.as_deref().filter(|rich| !rich.inlines.slots.is_empty())
    }

    /// 挂载时：行内元素排在哪几段
    pub fn attach_inlines(&mut self, slots: Vec<InlineSlot>) {
        let Some(rich) = self.rich.as_deref_mut() else { return };
        rich.inlines.slots = slots;
        self.content = rich.restart();
        self.invalidate();
    }

    /// 元素的大小、基线、对齐变了：重新断行（不重新整形）。变了返回 true
    pub fn set_inline_metrics(&mut self, k: usize, metrics: InlineMetrics) -> bool {
        let Some(slot) = self.rich.as_deref_mut().and_then(|rich| rich.inlines.slots.get_mut(k)) else { return false };
        if slot.metrics == metrics {
            return false;
        }
        slot.metrics = metrics;
        if let Some(layout) = &mut self.layout
            && let Some(placed) = layout.inline_boxes_mut().find(|b| b.id == k as u64)
        {
            *placed = inline_box(k, placed.index, &metrics);
        }
        self.broken_at = None;
        self.truncated = None;
        self.aligned = false;
        self.forget_measures();
        true
    }

    /// 按最终宽度排好，记下每个元素的位置（对齐物理像素）。哪些元素显示变了（截断）时返回 true
    pub fn place_inlines(&mut self, text: &mut TextSystem, width: f32) -> bool {
        self.break_at(text, Some(width));
        let scale = text.scale();
        // 排版变了都要重新对齐（见 `layout_at`）：还对齐着、缩放也没变，读过的位置还能用
        if self.aligned && self.with_inlines().is_some_and(|rich| rich.inlines.placed_scale == scale) {
            return false;
        }
        self.layout_at(text, width);
        let layout = match &self.truncated {
            Some(t) => &t.layout,
            None => self.layout.as_ref().expect("刚断过行"),
        };
        let Some(rich) = self.rich.as_deref_mut() else { return false };
        rich.inlines.placed_scale = scale;
        let mut placed = vec![false; rich.inlines.slots.len()];
        let snap = |v: f32| (v * scale).round() / scale;
        for line in layout.lines() {
            let metrics = line.metrics();
            for item in line.items() {
                let PositionedLayoutItem::InlineBox(b) = item else { continue };
                let Some(slot) = rich.inlines.slots.get_mut(b.id as usize) else { continue };
                slot.at = [snap(b.x), snap(b.y)];
                slot.column = Rect { x: b.x, y: metrics.block_min_coord, width: b.width, height: metrics.block_max_coord - metrics.block_min_coord };
                placed[b.id as usize] = true;
            }
        }
        let mut changed = false;
        for (slot, placed) in rich.inlines.slots.iter_mut().zip(placed) {
            changed |= slot.shown != placed;
            slot.shown = placed;
        }
        changed
    }

    /// 排版后面多出来的空行有几行（0 或者 1）：比这一行还宽的元素排在最后时，排版库把它放下之后马上断行，后面什么都没有，
    /// 多出一个空行。文字结尾的换行符后面的空行不算这种
    pub(super) fn inline_blank_lines(&self, layout: &Layout<SpanId>) -> usize {
        if self.with_inlines().is_none() {
            return 0;
        }
        let ends_with_box = |line: Line<'_, SpanId>| line.items().last().is_some_and(|item| matches!(item, PositionedLayoutItem::InlineBox(_)));
        let lines = layout.len();
        let blank = lines >= 2
            && layout.get(lines - 1).is_some_and(|last| last.items().next().is_none())
            && layout.get(lines - 2).is_some_and(ends_with_box);
        usize::from(blank)
    }

    /// 多出来的空行有多高
    pub(super) fn inline_blank_height(&self, layout: &Layout<SpanId>) -> f32 {
        if self.inline_blank_lines(layout) == 0 {
            return 0.0;
        }
        layout.get(layout.len() - 1).map_or(0.0, |last| {
            let metrics = last.metrics();
            metrics.block_max_coord - metrics.block_min_coord
        })
    }

    /// 截断时累加宽度：占位字符（`range`）后面跟着的元素有多宽
    pub(super) fn inline_width_after(&self, range: &Range<usize>) -> f32 {
        let Some(rich) = self.with_inlines() else { return 0.0 };
        rich.inlines.slots.iter().find(|slot| rich.ranges[slot.span] == *range).map_or(0.0, |slot| slot.metrics.size[0])
    }

    /// 点在哪个行内元素的那一列上：它占的字节范围，点在元素开始的那一半（从左到右的文字是左半边）时 `true`
    fn inline_hit(&self, [x, y]: [f32; 2]) -> Option<(Range<usize>, bool)> {
        let rich = self.with_inlines()?;
        let k = rich.inlines.slots.iter().position(|slot| slot.shown && slot.column.contains([x, y]))?;
        let column = rich.inlines.slots[k].column;
        let left = x < column.x + column.width * 0.5;
        let rtl = self.shown().is_some_and(|layout| layout.is_rtl());
        Some((rich.inline_range(k), left != rtl))
    }

    /// 点在行内元素上时，离哪边近就是元素的哪一头
    pub(super) fn inline_offset(&self, point: [f32; 2]) -> Option<usize> {
        self.inline_hit(point).map(|(range, start)| if start { range.start } else { range.end })
    }

    /// 点在行内元素上时，它占的字节范围（双击选中它）
    pub(super) fn inline_range(&self, point: [f32; 2]) -> Option<Range<usize>> {
        self.inline_hit(point).map(|(range, _)| range)
    }

    /// 按在行内元素上（元素自己不要指针时才到这里）：单击放下一端，双击选中它。处理了返回 true
    pub(super) fn press_inline(&mut self, point: [f32; 2], clicks: u32, extend: bool) -> bool {
        let (range, at) = match (clicks, self.inline_hit(point)) {
            (1 | 2, Some((range, start))) => (range.clone(), if start { range.start } else { range.end }),
            _ => return false,
        };
        let Some(layout) = self.shown() else { return false };
        let selection = match clicks {
            1 => span_selection(layout, self.selection.filter(|_| extend).map_or(at, |s| s.anchor().index()), at),
            _ => span_selection(layout, range.start, range.end),
        };
        self.word = (clicks == 2).then_some(range);
        self.selection = Some(selection);
        self.selected_all = false;
        true
    }

    /// 拖到行内元素上：选区的另一头落在元素的一头；双击之后按词扩展时，整个元素算一个词。处理了返回 true
    pub(super) fn drag_inline(&mut self, point: [f32; 2]) -> bool {
        let Some((range, start)) = self.inline_hit(point) else { return false };
        let (Some(layout), Some(selection)) = (self.shown(), self.selection) else { return false };
        self.selection = Some(match &self.word {
            Some(initial) if range.start < initial.start => span_selection(layout, initial.end, range.start),
            Some(initial) => span_selection(layout, initial.start, range.end.max(initial.end)),
            None => span_selection(layout, selection.anchor().index(), if start { range.start } else { range.end }),
        });
        self.selected_all = false;
        true
    }

    // ---- 键盘（见 `tree/selection.rs`） ----
    //
    // 占位字符排在盒子前面，parley 算位置时只认字：元素后面（占位字符后面）算在盒子的左边。元素放不下挪到下一行时，占位字符
    // 留在上一行末尾，同一个偏移还可能在两行上。选区按字节算盖没盖住元素，所以这里按元素那一列重新说这些位置在哪：
    // 元素后面在它的右边（从右到左时左边）；元素前面贴着后面（`Downstream`）时在下一行、它的左边，贴着前面时是上一行的末尾。

    /// 第 `k` 个行内元素不在这段的这一排（纵坐标 `y`）上
    fn inline_off_row(&self, slot: &InlineSlot, y: f32) -> bool {
        y < slot.column.y || y >= slot.column.y + slot.column.height
    }

    /// 紧挨着 `offset`（`after` 时是它前面，否则是它后面）的、显示出来的行内元素：它占的字节范围和那一列
    fn inline_at_offset(&self, offset: usize, after: bool) -> Option<(Range<usize>, &InlineSlot)> {
        let rich = self.with_inlines()?;
        rich.inlines.slots.iter().enumerate().find_map(|(k, slot)| {
            let range = rich.inline_range(k);
            (slot.shown && offset == if after { range.end } else { range.start }).then_some((range, slot))
        })
    }

    /// 选区一端挨着行内元素时的矩形；别的位置是 `None`，按字算
    pub(super) fn inline_caret_box(&self, at: Cursor) -> Option<Rect> {
        let layout = self.shown()?;
        let rtl = layout.is_rtl();
        let edge = |column: Rect, right: bool| Rect { x: if right { column.x + column.width } else { column.x }, y: column.y, width: 1.0, height: column.height };
        if let Some((_, slot)) = self.inline_at_offset(at.index(), true) {
            return Some(edge(slot.column, !rtl));
        }
        let (_, slot) = self.inline_at_offset(at.index(), false)?;
        let r = at.geometry(layout, 0.0);
        let wrapped = self.inline_off_row(slot, ((r.y0 + r.y1) * 0.5) as f32);
        (wrapped && at.affinity() == Affinity::Downstream).then(|| edge(slot.column, rtl))
    }

    /// 这一排（纵坐标 `y`）上的位置 `offset` 是占位字符后面，元素却挪到了别的行：算元素前面，返回它
    pub(super) fn inline_left_behind(&self, offset: usize, y: f32) -> Option<usize> {
        let (range, slot) = self.inline_at_offset(offset, true)?;
        self.inline_off_row(slot, y).then_some(range.start)
    }

    /// 这一排（纵坐标 `y`）上离横坐标 `x` 最近的行内元素的近的一头
    pub(super) fn inline_nearest(&self, x: f32, y: f32) -> Option<usize> {
        let rich = self.with_inlines()?;
        let rtl = self.shown().is_some_and(|layout| layout.is_rtl());
        let distance = |column: Rect| (column.x - x).max(x - (column.x + column.width)).max(0.0);
        let k = (0..rich.inlines.slots.len())
            .filter(|&k| rich.inlines.slots[k].shown && !self.inline_off_row(&rich.inlines.slots[k], y))
            .min_by(|&a, &b| distance(rich.inlines.slots[a].column).total_cmp(&distance(rich.inlines.slots[b].column)))?;
        let (column, range) = (rich.inlines.slots[k].column, rich.inline_range(k));
        Some(if (x < column.x + column.width * 0.5) != rtl { range.start } else { range.end })
    }

    /// 键盘到行首行尾。有行内元素时按它算的这一行来：行首的元素算在这一行里（行首在它前面），
    /// 挪到下一行的元素不算在这一行里（行尾在它前面）
    pub fn line_edge(&self, at: Cursor, end: bool) -> Option<Cursor> {
        let layout = self.shown()?;
        if self.with_inlines().is_none() {
            return Some(if end { motion::line_end(layout, at) } else { motion::line_start(layout, at) });
        }
        let caret = self.caret_box(at)?;
        let y = caret.y + caret.height * 0.5;
        let line = layout.get(motion::line_at_y(layout, y))?;
        let range = line.text_range();
        let side = if end { Affinity::Upstream } else { Affinity::Downstream };
        let edge = if range.is_empty() {
            Cursor::from_byte_index(layout, range.end, side)
        } else {
            let first = Cursor::from_byte_index(layout, range.start, Affinity::Downstream);
            if end { motion::line_end(layout, first) } else { motion::line_start(layout, first) }
        };
        let Some((before, slot)) = self.inline_at_offset(edge.index(), true) else { return Some(edge) };
        let move_back = if end { self.inline_off_row(slot, y) } else { !self.inline_off_row(slot, y) };
        Some(if move_back { Cursor::from_byte_index(layout, before.start, side) } else { edge })
    }

    /// 复制时 `range`（`content` 里的字节范围）里的文字：占位字符去掉，整个选中的元素换成代替它的文字
    pub(super) fn copy_text(&self, content: &str, range: Range<usize>) -> String {
        let Some(rich) = self.with_inlines() else { return content[range].to_owned() };
        let mut out = String::with_capacity(range.len());
        let mut at = range.start;
        for slot in &rich.inlines.slots {
            let placeholder = &rich.ranges[slot.span];
            // 截断时显示出来的文字到省略号为止：省略号占的可能正好是后面那个元素的位置，要真的是占位字符才算
            if placeholder.start >= at && placeholder.end <= range.end && content.get(placeholder.clone()) == Some(PLACEHOLDER) {
                out.push_str(&content[at..placeholder.start]);
                out.push_str(&slot.copy);
                at = placeholder.end;
            }
        }
        out.push_str(&content[at..range.end]);
        out
    }

    /// 选区的矩形去掉行内元素占的那几列：元素上的选区画在元素上面（[`Self::selected_inline_columns`]），
    /// 没选中的元素下面也没有选区
    pub(super) fn around_inlines(&self, rects: Vec<BoundingBox>) -> Vec<BoundingBox> {
        let Some(rich) = self.with_inlines() else { return rects };
        let mut rects = rects;
        for slot in rich.inlines.slots.iter().filter(|slot| slot.shown && slot.column.width > 0.0) {
            let column = slot.column;
            let (x0, x1) = (column.x as f64, (column.x + column.width) as f64);
            let middle = (column.y + column.height * 0.5) as f64;
            let mut next = Vec::with_capacity(rects.len() + 1);
            for rect in rects {
                if rect.y0 > middle || rect.y1 < middle || rect.x1 <= x0 || rect.x0 >= x1 {
                    next.push(rect);
                    continue;
                }
                if rect.x0 < x0 {
                    next.push(BoundingBox::new(rect.x0, rect.y0, x0, rect.y1));
                }
                if rect.x1 > x1 {
                    next.push(BoundingBox::new(x1, rect.y0, rect.x1, rect.y1));
                }
            }
            rects = next;
        }
        rects
    }

    /// 选区整个盖住的行内元素占的那几列（相对文字原点）
    pub fn selected_inline_columns(&self) -> Vec<Rect> {
        let (Some(rich), Some(selection)) = (self.with_inlines(), self.selection) else { return Vec::new() };
        let range = selection.text_range();
        rich.inlines
            .slots
            .iter()
            .filter(|slot| {
                let placeholder = &rich.ranges[slot.span];
                slot.shown && slot.column.width > 0.0 && range.start <= placeholder.start && placeholder.end <= range.end
            })
            .map(|slot| slot.column)
            .collect()
    }

    /// 绘制文字时记下选区的颜色（有没有焦点决定的），子元素画完后画元素上的选区时用
    pub fn set_inline_selection_fill(&mut self, fill: Color) {
        if let Some(rich) = self.rich.as_deref_mut() {
            rich.inlines.selection_fill = Some(fill);
        }
    }

    /// 元素上的选区画成什么颜色
    pub fn inline_selection_fill(&self) -> Option<Color> {
        self.rich.as_deref()?.inlines.selection_fill
    }
}
