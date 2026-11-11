//! 编辑器的文档：按换行分段，每段单独用 parley 排版。
//!
//! 改一处只重新整形那一段：一万字的文档里打一个字，也只排插入点所在的那一段；画的时候只画看得见的段。
//! 很长的一段（上万字不换行）再切成块，只重新整形一块（见 [`super::flow`]），这里的查询都先问"放着这个位置的是哪份排版"（[`Piece`]）。
//! 位置用全文（各段之间各有一个换行符）里的字节偏移表示，撤销记录、和信号同步都直接用它。
//! 段内的插入点移动、按行移动是和跨段落选择共用的规则（`motion.rs`），跨段的部分在这里处理。

use std::ops::Range;

use parley::{Affinity, AlignmentOptions, BoundingBox, Cluster, Cursor, Layout, Selection as Span};
use scene::Quad;

use super::flow::{Flow, Near, Piece, Tuning, needs_whole};
use super::motion;
use crate::geometry::Affine;
use crate::text::{GlyphQuads, Ink, SpanId, TextStyle, TextSystem, word_range};
use crate::segmentation;

/// 排出来的一行（自动折出来的也算一行），无障碍按行给文字片段时用
pub(crate) struct Line {
    /// 全文里的字节范围（含预编辑文字，不含段尾的换行符）
    pub range: Range<usize>,
    /// 是一段的最后一行（后面是换行符或者文档末尾）
    pub paragraph_end: bool,
    /// 在文档坐标里的矩形
    pub rect: BoundingBox,
}

/// 文档里的一个位置：全文的字节偏移，加上贴着哪一边
/// （自动折行处，同一个偏移既可以在上一行末尾，也可以在下一行开头）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Caret {
    pub offset: usize,
    pub affinity: Affinity,
}

impl Caret {
    pub fn at(offset: usize) -> Self {
        Self { offset, affinity: Affinity::Downstream }
    }

    pub fn upstream(offset: usize) -> Self {
        Self { offset, affinity: Affinity::Upstream }
    }
}

/// 双击、三击之后按住拖动时，选区按什么单位扩展
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unit {
    Word,
    Paragraph,
}

/// 删除的单位：字（退格）、词（Option）、到行首（Command）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    Char,
    Word,
    Line,
}

#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub anchor: Caret,
    pub focus: Caret,
    /// 连续上下移动时保持的横坐标：经过短行之后回到长行，插入点还在原来的列
    pub h_pos: Option<f32>,
    /// 双击、三击最初选中的范围和单位：拖动时它一直被选着，另一头按单位扩展
    pub unit: Option<(Unit, Range<usize>)>,
}

impl Selection {
    pub fn caret(caret: Caret) -> Self {
        Self { anchor: caret, focus: caret, h_pos: None, unit: None }
    }

    pub fn span(anchor: usize, focus: usize) -> Self {
        Self { anchor: Caret::at(anchor), focus: Caret::upstream(focus), h_pos: None, unit: None }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.offset.min(self.focus.offset)..self.anchor.offset.max(self.focus.offset)
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor.offset == self.focus.offset
    }
}

struct Paragraph {
    text: String,
    layout: Layout<SpanId>,
    /// 内容或样式变了，还没重新整形
    stale: bool,
    /// 按哪个宽度断的行；`None` 表示整形之后还没断过
    broken: Option<Option<f32>>,
    /// 段顶在文档里的位置，取整到像素
    top: f32,
    height: f32,
    /// 上次生成的字形图元（按段顶为 0 生成）：重新整形、断行时清掉，段只是上下挪了时照样用
    glyphs: GlyphQuads,
    /// 很长的一段：分块排版（这时 `layout`、`glyphs` 不用）
    flow: Option<Box<Flow>>,
    /// 用户改过：很长的一段第一次改的时候才分块，没人改的段（只是显示）和以前一样整段排版一次
    edited: bool,
}

impl Paragraph {
    fn new(text: String) -> Self {
        Self {
            text,
            layout: Layout::default(),
            stale: true,
            broken: None,
            top: 0.0,
            height: 0.0,
            glyphs: GlyphQuads::default(),
            flow: None,
            edited: false,
        }
    }

    /// 放着这个位置（段内偏移）的排版
    fn piece(&self, local: usize, affinity: Affinity) -> Piece<'_> {
        match &self.flow {
            Some(flow) => flow.piece_at(local, affinity),
            None => Piece { layout: &self.layout, base: 0 },
        }
    }

    /// 放着这个纵坐标（相对段顶）所在的行的排版，和在这份排版里问这一行用的纵坐标
    fn piece_at_y(&self, y: f32) -> (Piece<'_>, f32) {
        match &self.flow {
            Some(flow) => {
                let line = flow.line_at(y);
                (flow.piece_of_line(line), flow.line_center(line))
            }
            None => (Piece { layout: &self.layout, base: 0 }, y),
        }
    }

    /// 排出来几行
    fn line_count(&self) -> usize {
        match &self.flow {
            Some(flow) => flow.line_count(),
            None => self.layout.len(),
        }
    }
}

pub(crate) struct Document {
    paras: Vec<Paragraph>,
    /// 每段第一个字节在全文里的偏移
    starts: Vec<usize>,
    style: TextStyle,
    /// 断行宽度；单行输入框是 `None`（不断行）
    wrap: Option<f32>,
    /// 可能要整形、断行的段（别的段都排好了）；它后面的段的段顶可能要整体挪。`None` 表示都排好了
    pending: Option<Range<usize>>,
    height: f32,
    /// 输入法预编辑文字在全文里的范围：预编辑文字放在段落里一起排版，但不算内容
    pub compose: Option<Range<usize>>,
    pub selection: Selection,
    /// 累计整形过多少段（测试和基准用：确认只重排了改动的那一段）
    pub shaped: usize,
    /// 上次画了哪几段：滚出去的段还掉字形图元的内存
    painted: Range<usize>,
    /// 多长的段分块
    tuning: Tuning,
    /// 有长段只排到了看得见的地方（[`Self::refresh_near`]）
    unsettled: bool,
    /// 换内容以来分块排版过长段
    any_flow: bool,
}

impl Document {
    pub fn new(text: &str, style: TextStyle) -> Self {
        let mut doc = Self {
            paras: Vec::new(),
            starts: Vec::new(),
            style,
            wrap: None,
            pending: None,
            height: 0.0,
            compose: None,
            selection: Selection::caret(Caret::at(0)),
            shaped: 0,
            painted: 0..0,
            tuning: Tuning::default(),
            unsettled: false,
            any_flow: false,
        };
        doc.set_text(text);
        doc
    }

    /// 换掉全部内容，插入点放到末尾
    pub fn set_text(&mut self, text: &str) {
        self.paras = text.split('\n').map(|s| Paragraph::new(s.to_owned())).collect();
        self.restart(0);
        self.compose = None;
        self.pending = Some(0..self.paras.len());
        self.selection = Selection::caret(self.end());
        self.any_flow = false;
    }

    pub fn style(&self) -> &TextStyle {
        &self.style
    }

    /// 样式变了：每段都要重新整形
    pub fn set_style(&mut self, style: TextStyle) {
        if self.style != style {
            self.style = style;
            self.invalidate();
        }
    }

    /// 丢掉所有排版（样式变了、加载了新字体）
    pub fn invalidate(&mut self) {
        self.paras.iter_mut().for_each(|p| {
            p.stale = true;
            p.flow = None;
        });
        self.pending = Some(0..self.paras.len());
    }

    /// 断行宽度变了：每段重新断行（不用重新整形）
    pub fn set_wrap(&mut self, wrap: Option<f32>) {
        if self.wrap != wrap {
            self.wrap = wrap;
            self.pending = Some(0..self.paras.len());
        }
    }

    /// 第 `range` 段要重新整形、断行（编辑改了它们）：和还没处理的合在一起。
    /// 拆、合过段（`split`）时之前记下的段号可能挪了，一直到末尾都算上（很少见：两次编辑之间一般都排过）
    fn touch(&mut self, range: Range<usize>, split: bool) {
        self.pending = Some(match self.pending.take() {
            Some(old) if split => old.start.min(range.start)..usize::MAX,
            Some(old) => old.start.min(range.start)..old.end.max(range.end),
            None => range,
        });
    }

    /// 整形改过的段、按当前宽度断行、重新累加它们的段顶，后面的段整体挪（不用逐段再看）。没有改动时什么都不做
    pub fn refresh(&mut self, text: &mut TextSystem) {
        self.refresh_to(text, None);
    }

    /// 打字时用：和 [`Self::refresh`] 一样，只是很长的一段只排到看得见的地方（文档坐标 `bottom`）和插入点，
    /// 改动波及到更后面时先不管：这一段后面的行还是上次的，文档的总高是估的。之后用 [`Self::refresh`] 排完
    pub fn refresh_near(&mut self, text: &mut TextSystem, bottom: f32) {
        self.refresh_to(text, Some(bottom));
    }

    /// 排完了（没有因为 [`Self::refresh_near`] 留下没排的长段）
    pub fn is_settled(&self) -> bool {
        !self.unsettled
    }

    /// 分块排版过长段（换内容之前一直算：这只决定打字时少画一点，不用逐段看）
    pub fn has_flows(&self) -> bool {
        self.any_flow
    }

    fn refresh_to(&mut self, text: &mut TextSystem, bottom: Option<f32>) {
        if std::mem::take(&mut self.unsettled) {
            self.touch(0..self.paras.len(), false);
        }
        let Some(range) = self.pending.take() else { return };
        let len = self.paras.len();
        let (first, end) = (range.start.min(len), range.end.min(len));
        let mut top = match first {
            0 => 0.0,
            i => self.paras[i - 1].top + self.paras[i - 1].height,
        };
        let alignment = self.style.alignment();
        let (wrap, style, tuning, focus) = (self.wrap, self.style, self.tuning, self.selection.focus.offset);
        for (i, p) in self.paras[first..end].iter_mut().enumerate() {
            // 很长的一段改过之后分块排版；短的（绝大多数）只多这一次判断
            if p.flow.is_some() || (p.edited && p.text.len() >= tuning.enter) {
                let near = bottom.map(|bottom| Near { bottom: bottom - top, offset: focus.saturating_sub(self.starts[first + i]) });
                if refresh_flow(p, text, &style, wrap, tuning, near, &mut self.shaped) {
                    self.any_flow = true;
                    self.unsettled |= p.flow.as_ref().is_some_and(|flow| !flow.is_settled());
                    p.top = top;
                    top += p.height;
                    continue;
                }
            }
            if p.stale {
                text.build_into(&mut p.layout, &p.text, &self.style);
                p.stale = false;
                p.broken = None;
                self.shaped += 1;
            }
            if p.broken != Some(self.wrap) {
                p.layout.break_all_lines(self.wrap);
                p.layout.align(alignment, AlignmentOptions::default());
                p.broken = Some(self.wrap);
                p.height = p.layout.height().round();
                p.glyphs.clear();
            }
            p.top = top;
            top += p.height;
        }
        // 后面的段没变：段顶整体挪（段顶、段高都是整数，挪出来的和逐段累加的一样）
        if let Some(next) = self.paras.get(end)
            && next.top != top
        {
            let shift = top - next.top;
            self.paras[end..].iter_mut().for_each(|p| p.top += shift);
        }
        let last = &self.paras[len - 1];
        self.height = last.top + last.height;
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    /// 最宽一行的宽度（含行尾空格）。逐段找（单行输入框横向滚动时用，只有一段）
    pub fn width(&self) -> f32 {
        // 分块的段只在多行输入框里，用不到它
        self.paras.iter().map(|p| p.layout.full_width()).fold(0.0, f32::max)
    }

    /// 一行大约多高（翻页按它算行数）
    pub fn line_height(&self) -> f32 {
        (self.style.size * self.style.line_height).round().max(1.0)
    }

    /// 全文（含预编辑文字）的字节数
    pub fn len(&self) -> usize {
        let last = self.paras.len() - 1;
        self.starts[last] + self.paras[last].text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paras.len() == 1 && self.paras[0].text.is_empty()
    }

    pub fn end(&self) -> Caret {
        Caret::upstream(self.len())
    }

    /// 内容（不含预编辑文字）
    pub fn text(&self) -> String {
        let mut text = self.slice(0..self.len());
        if let Some(compose) = &self.compose {
            text.replace_range(compose.clone(), "");
        }
        text
    }

    /// 内容是否就是 `value`（不分配内存，信号写回来时用）
    pub fn equals(&self, value: &str) -> bool {
        if self.compose.is_some() || value.len() != self.len() {
            return false;
        }
        // 长度一样，每段的字节和段尾的换行符都对得上，就是同一段文字
        let bytes = value.as_bytes();
        self.paras.iter().zip(&self.starts).all(|(p, &start)| {
            let end = start + p.text.len();
            &bytes[start..end] == p.text.as_bytes() && bytes.get(end).is_none_or(|&b| b == b'\n')
        })
    }

    /// 全文里的一段（含预编辑文字）
    pub fn slice(&self, range: Range<usize>) -> String {
        let (pa, la) = self.locate(range.start);
        let (pb, lb) = self.locate(range.end);
        if pa == pb {
            return self.paras[pa].text[la..lb].to_owned();
        }
        let mut text = String::with_capacity(range.len());
        text.push_str(&self.paras[pa].text[la..]);
        for p in &self.paras[pa + 1..pb] {
            text.push('\n');
            text.push_str(&p.text);
        }
        text.push('\n');
        text.push_str(&self.paras[pb].text[..lb]);
        text
    }

    /// 偏移在哪一段、段内第几个字节
    pub fn locate(&self, offset: usize) -> (usize, usize) {
        let p = self.starts.partition_point(|&s| s <= offset).max(1) - 1;
        (p, (offset - self.starts[p]).min(self.paras[p].text.len()))
    }

    /// 从第 `from` 段起重新累加每段的起始偏移
    fn restart(&mut self, from: usize) {
        self.starts.truncate(from);
        let mut start = match from {
            0 => 0,
            _ => self.starts[from - 1] + self.paras[from - 1].text.len() + 1,
        };
        for p in &self.paras[from..] {
            self.starts.push(start);
            start += p.text.len() + 1;
        }
    }

    /// 把全文里的 `range` 换成 `insert`：只动涉及的段，插入的换行符拆出新段，后面的段只挪起点。
    /// 返回插入的文字之后的偏移
    pub fn replace(&mut self, range: Range<usize>, insert: &str) -> usize {
        let len = self.len();
        let range = range.start.min(len)..range.end.clamp(range.start.min(len), len);
        let (pa, la) = self.locate(range.start);
        let (pb, lb) = self.locate(range.end);
        let shift = insert.len().wrapping_sub(range.len());
        // 后面的段（第 pb 段之后）只是起点挪了
        self.starts[pb + 1..].iter_mut().for_each(|s| *s = s.wrapping_add(shift));
        // 最常见的：在一段里打字、删字，不拆不合
        if pa == pb && !insert.contains('\n') {
            let p = &mut self.paras[pa];
            p.text.replace_range(la..lb, insert);
            p.stale = true;
            p.edited = true;
            if let Some(flow) = &mut p.flow {
                // 打进了判断不了的字（从右到左的字）：这一段退回整段排版
                if insert.chars().any(needs_whole) {
                    p.flow = None;
                } else {
                    flow.edit(la..lb, insert.len());
                }
            }
            self.touch(pa..pa + 1, false);
            return range.start + insert.len();
        }
        let tail = self.paras[pb].text[lb..].to_owned();
        let mut pieces = insert.split('\n');
        let first = pieces.next().unwrap_or_default();
        let mut added: Vec<Paragraph> = pieces.map(|s| Paragraph::new(s.to_owned())).collect();
        let head = &mut self.paras[pa];
        head.text.truncate(la);
        head.text.push_str(first);
        head.stale = true;
        head.flow = None;
        head.edited = false;
        match added.last_mut() {
            Some(last) => last.text.push_str(&tail),
            None => head.text.push_str(&tail),
        }
        let count = added.len();
        self.paras.splice(pa + 1..=pb, added);
        // 新拆出来的段的起点
        let mut start = self.starts[pa] + self.paras[pa].text.len() + 1;
        let starts: Vec<usize> = self.paras[pa + 1..=pa + count]
            .iter()
            .map(|p| {
                let s = start;
                start += p.text.len() + 1;
                s
            })
            .collect();
        self.starts.splice(pa + 1..=pb, starts);
        self.touch(pa..pa + count + 1, true);
        range.start + insert.len()
    }

    // ---- 位置和几何（都要先 `refresh`） ----

    /// 在第几段、放着它的排版、在这份排版里的位置
    fn cursor(&self, caret: Caret) -> (usize, Piece<'_>, Cursor) {
        let (p, local) = self.locate(caret.offset);
        let piece = self.paras[p].piece(local, caret.affinity);
        let cursor = Cursor::from_byte_index(piece.layout, local - piece.base, caret.affinity);
        (p, piece, cursor)
    }

    fn caret_in(&self, p: usize, piece: Piece<'_>, cursor: Cursor) -> Caret {
        Caret { offset: self.starts[p] + piece.base + cursor.index(), affinity: cursor.affinity() }
    }

    /// 插入点的矩形（文档坐标）
    pub fn caret_rect(&self, caret: Caret, width: f32) -> BoundingBox {
        let (p, piece, cursor) = self.cursor(caret);
        let mut rect = cursor.geometry(piece.layout, width);
        let top = self.paras[p].top as f64;
        rect.y0 += top;
        rect.y1 += top;
        rect
    }

    /// 纵坐标落在哪一段（超出上下边时是第一段、最后一段）
    fn paragraph_at(&self, y: f32) -> usize {
        self.paras.partition_point(|p| p.top + p.height <= y).min(self.paras.len() - 1)
    }

    /// 文档坐标下离这个点最近的位置
    pub fn hit(&self, [x, y]: [f32; 2]) -> Caret {
        let p = self.paragraph_at(y);
        let para = &self.paras[p];
        let (piece, y) = para.piece_at_y(y - para.top);
        self.caret_in(p, piece, Cursor::from_point(piece.layout, x, y))
    }

    /// 这个点所在的词（双击）：中日文按词典分词
    pub fn word_at(&self, [x, y]: [f32; 2]) -> Range<usize> {
        let p = self.paragraph_at(y);
        let para = &self.paras[p];
        let range = match &para.flow {
            Some(flow) => flow.word_at(&para.text, x, y - para.top),
            None => word_range(&para.layout, &para.text, x, y - para.top),
        };
        self.starts[p] + range.start..self.starts[p] + range.end
    }

    /// 这个点所在的整段，连同段尾的换行符（三击）
    pub fn paragraph_range_at(&self, [_, y]: [f32; 2]) -> Range<usize> {
        let p = self.paragraph_at(y);
        let newline = usize::from(p + 1 < self.paras.len());
        self.starts[p]..self.starts[p] + self.paras[p].text.len() + newline
    }

    /// 插入点的横坐标（上下移动时保持它）
    pub fn x_of(&self, caret: Caret) -> f32 {
        self.caret_rect(caret, 0.0).x0 as f32
    }

    /// 往右一个字（`word` 时到词尾，中日文按词典分的词）；在段尾时到下一段开头
    pub fn right(&self, from: Caret, word: bool) -> Caret {
        let (p, piece, cursor) = self.cursor(from);
        let para = &self.paras[p];
        let next = match &para.flow {
            // 分块的段：按词走可能走出这份排版
            Some(flow) => flow.right(&para.text, piece, cursor, word),
            None => in_paragraph(motion::right(piece.layout, &para.text, cursor, word)),
        };
        if next == (piece.base + cursor.index(), cursor.affinity()) && p + 1 < self.paras.len() {
            return Caret::at(self.starts[p + 1]);
        }
        Caret { offset: self.starts[p] + next.0, affinity: next.1 }
    }

    /// 往左一个字（`word` 时到词首，中日文按词典分的词）；在段首时到上一段末尾
    pub fn left(&self, from: Caret, word: bool) -> Caret {
        let (p, piece, cursor) = self.cursor(from);
        let para = &self.paras[p];
        let next = match &para.flow {
            Some(flow) => flow.left(&para.text, piece, cursor, word),
            None => in_paragraph(motion::left(piece.layout, &para.text, cursor, word)),
        };
        if next == (piece.base + cursor.index(), cursor.affinity()) && p > 0 {
            return Caret::upstream(self.starts[p - 1] + self.paras[p - 1].text.len());
        }
        Caret { offset: self.starts[p] + next.0, affinity: next.1 }
    }

    /// 所在（折行后的）这一行的开头
    pub fn line_start(&self, from: Caret) -> Caret {
        let (p, piece, cursor) = self.cursor(from);
        self.caret_in(p, piece, motion::line_start(piece.layout, cursor))
    }

    /// 所在这一行的末尾
    pub fn line_end(&self, from: Caret) -> Caret {
        let (p, piece, cursor) = self.cursor(from);
        self.caret_in(p, piece, motion::line_end(piece.layout, cursor))
    }

    pub fn paragraph_start(&self, from: Caret) -> Caret {
        Caret::at(self.starts[self.locate(from.offset).0])
    }

    pub fn paragraph_end(&self, from: Caret) -> Caret {
        let p = self.locate(from.offset).0;
        Caret::upstream(self.starts[p] + self.paras[p].text.len())
    }

    /// 上下移动 `lines` 行（跨段），横坐标保持在 `h_pos`。越过第一行到文档开头，越过最后一行到文档末尾
    pub fn vertical(&self, from: Caret, lines: isize, h_pos: f32) -> Caret {
        let (mut p, line) = {
            let (p, piece, cursor) = self.cursor(from);
            let line = match &self.paras[p].flow {
                Some(flow) => {
                    let rect = cursor.geometry(piece.layout, 0.0);
                    flow.line_below(((rect.y0 + rect.y1) * 0.5) as f32)
                }
                None => motion::line_index(piece.layout, cursor),
            };
            (p, line)
        };
        let mut line = line as isize + lines;
        while line < 0 {
            if p == 0 {
                return Caret::at(0);
            }
            p -= 1;
            line += self.paras[p].line_count().max(1) as isize;
        }
        while line >= self.paras[p].line_count().max(1) as isize {
            if p + 1 == self.paras.len() {
                return self.end();
            }
            line -= self.paras[p].line_count().max(1) as isize;
            p += 1;
        }
        let para = &self.paras[p];
        if let Some(flow) = &para.flow {
            let piece = flow.piece_of_line(line as usize);
            return self.caret_in(p, piece, Cursor::from_point(piece.layout, h_pos, flow.line_center(line as usize)));
        }
        match motion::on_line(&para.layout, line as usize, h_pos) {
            Some(cursor) => self.caret_in(p, Piece { layout: &para.layout, base: 0 }, cursor),
            None => Caret::at(self.starts[p]),
        }
    }

    /// 排出来的所有行（要先 `refresh`），按顺序
    pub fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.paras.len());
        for (p, para) in self.paras.iter().enumerate() {
            let (start, top) = (self.starts[p], para.top as f64);
            if let Some(flow) = &para.flow {
                let count = flow.line_count();
                for i in 0..count {
                    let line = flow.with_line(i, |m, r| {
                        let x0 = (m.offset + m.inline_min_coord) as f64;
                        let y0 = m.block_min_coord.min(m.content_block_min_coord) as f64 + top;
                        let y1 = m.block_max_coord.max(m.content_block_max_coord) as f64 + top;
                        let end = if i + 1 == count { para.text.len() } else { r.end };
                        let rect = BoundingBox::new(x0, y0, x0 + m.advance as f64, y1);
                        Line { range: start + r.start..start + end, paragraph_end: i + 1 == count, rect }
                    });
                    lines.extend(line);
                }
                continue;
            }
            let count = para.layout.len();
            if count == 0 {
                let rect = BoundingBox::new(0.0, top, 0.0, top + para.height.max(self.line_height()) as f64);
                lines.push(Line { range: start..start, paragraph_end: true, rect });
                continue;
            }
            for i in 0..count {
                let Some(line) = para.layout.get(i) else { continue };
                let (r, m) = (line.text_range(), line.metrics());
                let x0 = (m.offset + m.inline_min_coord) as f64;
                let y0 = m.block_min_coord.min(m.content_block_min_coord) as f64 + top;
                let y1 = m.block_max_coord.max(m.content_block_max_coord) as f64 + top;
                let end = if i + 1 == count { para.text.len() } else { r.end };
                let rect = BoundingBox::new(x0, y0, x0 + m.advance as f64, y1);
                lines.push(Line { range: start + r.start..start + end, paragraph_end: i + 1 == count, rect });
            }
        }
        lines
    }

    /// 一行里的文字（`range` 在一段之内，含预编辑文字）
    pub fn line_text(&self, range: Range<usize>) -> &str {
        let (p, a) = self.locate(range.start);
        let text = &self.paras[p].text;
        &text[a..(a + range.len()).min(text.len())]
    }

    /// 一行（`range` 是 [`Self::lines`] 给的一行）里 `offsets`（从小到大，都是字的起点）处插入点的横坐标，和行尾的横坐标
    /// （文档坐标）。沿着这一行从左到右走一遍；有从右到左的字、偏移不在字的起点上时返回 `None`（交给 [`Self::x_at`] 逐个找）
    pub fn cluster_xs(&self, range: Range<usize>, offsets: &[usize]) -> Option<(Vec<f32>, f32)> {
        let (p, local) = self.locate(range.start);
        let piece = self.paras[p].piece(local, Affinity::Downstream);
        let start = self.starts[p] + piece.base;
        let line = Cluster::from_byte_index(piece.layout, local - piece.base)?.line();
        if line.runs().any(|run| run.is_rtl()) {
            return None;
        }
        let (mut xs, mut wanted) = (Vec::with_capacity(offsets.len()), offsets.iter().peekable());
        let (mut base, mut end) = (line.metrics().offset, line.metrics().offset);
        for run in line.runs() {
            let mut x = base;
            for cluster in run.visual_clusters() {
                let at = start + cluster.text_range().start;
                if let Some(&&next) = wanted.peek() {
                    if next < at {
                        return None;
                    }
                    if next == at {
                        xs.push(x);
                        wanted.next();
                    }
                }
                x += cluster.advance();
            }
            end = x;
            base += run.advance();
        }
        (wanted.next().is_none()).then_some((xs, end))
    }

    /// 全文 `offset` 处插入点的横坐标（文档坐标）；`upstream` 时贴着前面的字（行尾）
    pub fn x_at(&self, offset: usize, upstream: bool) -> f32 {
        let affinity = if upstream { Affinity::Upstream } else { Affinity::Downstream };
        self.caret_rect(Caret { offset, affinity }, 0.0).x0 as f32
    }

    /// 选区为空时，退格（`forward` 为假）或向前删除要删掉的范围
    pub fn delete_range(&self, at: Caret, forward: bool, reach: Reach) -> Range<usize> {
        let (p, piece, cursor) = self.cursor(at);
        let (para, layout, base, start) = (&self.paras[p], piece.layout, piece.base, self.starts[p]);
        let text = &para.text;
        let local = base + cursor.index();
        // 段首退格、段尾向前删除：删掉段之间的换行符，和相邻的段合并。
        // 看真的偏移：插入点在段里第一个字素簇中间（辅助技术、输入法改过文字）时 `local` 也是 0，那里没有换行符可删
        if !forward && local == 0 && at.offset == start {
            return at.offset.saturating_sub(1)..at.offset;
        }
        if forward && local == text.len() && at.offset == start + text.len() {
            return at.offset..(at.offset + 1).min(self.len());
        }
        let other = match (forward, reach) {
            (false, Reach::Char) => match cursor.logical_clusters(layout)[0].as_ref() {
                // emoji 整个删掉；其他按字符删（组合附加符号可以一个一个删）
                Some(cluster) if cluster.is_emoji() => base + cluster.text_range().start,
                _ => text[..local].char_indices().next_back().map_or(0, |(i, _)| i),
            },
            (true, Reach::Char) => match cursor.logical_clusters(layout)[1].as_ref() {
                Some(cluster) => base + cluster.text_range().end,
                None => local,
            },
            (false, Reach::Word) => segmentation::previous_word_start(text, local).unwrap_or_else(|| match &para.flow {
                Some(flow) => flow.previous_logical_word(text, local),
                None => cursor.previous_logical_word(layout).index(),
            }),
            // 平台惯例是按词往后到下一个词的开头时（Windows 的 Ctrl+Delete），连词后面的空白一起删
            (true, Reach::Word) => {
                let skips = crate::system::conventions().word_forward_skips_spaces;
                match motion::after_spaces(text, local).filter(|_| skips) {
                    Some(start) => start,
                    None => {
                        let end = segmentation::next_word_end(text, local).unwrap_or_else(|| match &para.flow {
                            Some(flow) => flow.next_logical_word(text, local),
                            None => cursor.next_logical_word(layout).index(),
                        });
                        motion::after_spaces(text, end).filter(|_| skips).unwrap_or(end)
                    }
                }
            }
            (false, Reach::Line) => base + motion::line_start(layout, cursor).index(),
            (true, Reach::Line) => base + motion::line_end(layout, cursor).index(),
        };
        let (a, b) = (local.min(other), local.max(other));
        start + a..start + b
    }

    // ---- 绘制 ----

    /// 生成和纵向范围 `view`（文档坐标；`None` 表示全部）相交的段的字形图元，`origin` 是文档的原点。
    /// 每段的图元按段顶为 0 生成、留着：排版没变的段直接拷贝，只是上下挪了的段拷贝时整体挪到新的段顶。
    /// 有缩放、旋转时（单行输入框跟着元素转）挪不了，段顶算进原点，挪了就重新生成。
    /// 上次画过、这次不画的段（滚出去了）还掉图元的内存。`place` 是图元的层号和裁剪（见 [`TextSystem::push_glyphs_cached`]）
    pub fn paint_glyphs(
        &mut self,
        text: &mut TextSystem,
        view: Option<[f32; 2]>,
        origin: [f32; 2],
        world: &Affine,
        ink: Ink,
        place: [u32; 2],
        quads: &mut Vec<Quad>,
    ) {
        let (first, last) = match view {
            Some([y0, y1]) => {
                (self.paras.partition_point(|p| p.top + p.height < y0), self.paras.partition_point(|p| p.top <= y1))
            }
            None => (0, self.paras.len()),
        };
        let last = last.max(first);
        let painted = std::mem::replace(&mut self.painted, first..last);
        for i in painted.start.min(self.paras.len())..painted.end.min(self.paras.len()) {
            if !(first..last).contains(&i) {
                let p = &mut self.paras[i];
                p.glyphs.release();
                if let Some(flow) = &mut p.flow {
                    flow.release_glyphs();
                }
            }
        }
        let (snap, scale) = (world.is_translation(), text.scale());
        for p in &mut self.paras[first..last] {
            // 段顶取整到物理像素（缩放不是整数时），挪过去的字形还对齐物理像素
            let (at, shift) = if snap { (origin, (p.top * scale).round() / scale) } else { ([origin[0], origin[1] + p.top], 0.0) };
            if let Some(flow) = &mut p.flow {
                let local = view.map(|[y0, y1]| [y0 - p.top, y1 - p.top]);
                flow.paint_glyphs(text, local, at, shift, world, ink, place, quads);
                continue;
            }
            text.push_glyphs_cached(&mut p.glyphs, &p.layout, 0..usize::MAX, at, world, ink, &[], shift, place, quads);
        }
    }

    /// 一段文字范围的矩形（文档坐标），只算和 `view` 相交的段。跨过段尾换行符的地方在行尾补一小段，
    /// 选中空行时也看得见
    pub fn range_rects(&self, range: Range<usize>, view: Option<[f32; 2]>, mut f: impl FnMut(BoundingBox)) {
        if range.is_empty() {
            return;
        }
        let newline = (self.style.size * 0.3) as f64;
        let (pa, la) = self.locate(range.start);
        let (pb, lb) = self.locate(range.end);
        for p in pa..=pb {
            let para = &self.paras[p];
            if view.is_some_and(|[y0, y1]| para.top + para.height < y0 || para.top > y1) {
                continue;
            }
            let (layout, top) = (&para.layout, para.top as f64);
            let a = if p == pa { la } else { 0 };
            let b = if p == pb { lb } else { para.text.len() };
            if let Some(flow) = &para.flow {
                let local = view.map(|[y0, y1]| [y0 - para.top, y1 - para.top]);
                if a < b {
                    flow.range_rects(a, b, local, &mut |r| f(BoundingBox { y0: r.y0 + top, y1: r.y1 + top, ..r }));
                }
                if p < pb {
                    let last = flow.with_line(flow.line_count() - 1, |m, _| {
                        let x = (m.offset + m.inline_min_coord + m.advance) as f64;
                        let y0 = m.block_min_coord.min(m.content_block_min_coord) as f64 + top;
                        let y1 = m.block_max_coord.max(m.content_block_max_coord) as f64 + top;
                        BoundingBox::new(x, y0, x + newline, y1)
                    });
                    last.into_iter().for_each(&mut f);
                }
                continue;
            }
            if a < b {
                let span = Span::new(
                    Cursor::from_byte_index(layout, a, Affinity::Downstream),
                    Cursor::from_byte_index(layout, b, Affinity::Upstream),
                );
                span.geometry_with(layout, |r, _| f(BoundingBox { y0: r.y0 + top, y1: r.y1 + top, ..r }));
            }
            if p < pb
                && let Some(line) = layout.get(layout.len().saturating_sub(1))
            {
                let m = line.metrics();
                let x = (m.offset + m.inline_min_coord + m.advance) as f64;
                let y0 = m.block_min_coord.min(m.content_block_min_coord) as f64 + top;
                let y1 = m.block_max_coord.max(m.content_block_max_coord) as f64 + top;
                f(BoundingBox::new(x, y0, x + newline, y1));
            }
        }
    }
}

/// 整段排版的段里的位置：段内偏移和贴哪边
fn in_paragraph(cursor: Cursor) -> (usize, Affinity) {
    (cursor.index(), cursor.affinity())
}

/// 很长的一段分块排好；不用分块或者不再分块时返回 `false`，走整段排版
fn refresh_flow(p: &mut Paragraph, text: &mut TextSystem, style: &TextStyle, wrap: Option<f32>, tuning: Tuning, near: Option<Near>, shaped: &mut usize) -> bool {
    let Some(wrap) = wrap else {
        p.stale |= p.flow.take().is_some();
        return false;
    };
    match &mut p.flow {
        Some(flow) => {
            if !flow.refresh(text, &p.text, style, wrap, near, shaped) {
                p.flow = None;
                p.stale = true;
                return false;
            }
        }
        None => {
            // 整段排好了、宽度也没变的不动（比如判断不了的长段，每次改了才看一遍）
            if !p.stale && p.broken == Some(Some(wrap)) {
                return false;
            }
            let Some(flow) = Flow::build(text, &p.text, style, wrap, tuning, shaped) else { return false };
            p.flow = Some(flow);
            // 整段的排版用不上了
            p.layout = Layout::default();
            p.glyphs.release();
        }
    }
    p.stale = false;
    p.broken = Some(Some(wrap));
    p.height = p.flow.as_ref().map_or(0.0, |flow| flow.height().round());
    true
}
