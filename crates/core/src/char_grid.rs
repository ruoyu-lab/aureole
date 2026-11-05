//! 字符网格：终端、代码编辑器这类等宽字符的大网格。
//!
//! 每格（汉字、emoji 占两格）放一个字素簇：一个字，或者带变音符号的字、组合的 emoji（ZWJ、肤色、国旗、键帽、VS16）；
//! 还有前景色、背景色、粗体、斜体、下划线、删除线、反色；另外有插入点（方块、竖线、下划线，可以闪）和选区。
//! 数据在 [`CharGrid`] 里，应用往里写，元素在下一帧把改过的行重新生成图元：
//!
//! - **一格 20 个字节，放得下一个字就放一个字**：放不下的簇（几个码点）存在这一行的簇表里，格子里放下标。没有簇的行、
//!   没有簇的格子和只放字时一样大、一样快
//! - **不走 parley 排版**：格子的位置是算出来的（列 × 格宽），每个字直接查字形图集。字第一次出现时用 parley 排一次
//!   这一个字，取出字体和字形（顺便得到缺字回退、粗体斜体的字面或合成），之后按字查缓存。簇要真的整形：
//!   第一次见到时整形，结果按簇和粗斜体缓存
//! - **连字是开关**（[`Element::ligated`]，默认关）：开着时同样式连续的格子一起整形（遇到空格分开），
//!   字形仍然放在各自的格子上
//! - **按行缓存图元**：每行改一次换一个新的版本号，元素按版本号缓存这一行的背景、字形、下划线、删除线图元（行内坐标）；
//!   改一行只重新生成这一行，滚动（[`CharGrid::scroll_up`]）只是挪行，行的缓存跟着走
//! - 格子对齐物理像素（格宽、格高、基线都取整到物理像素），字形不用亚像素位置，背景色块之间没有缝
//!
//! ```
//! # use aureole::prelude::*;
//! let grid = CharGrid::new(80, 24);
//! grid.write(0, 0, "$ cargo run", CharCellStyle::default());
//! grid.write(1, 0, "   Compiling", CharCellStyle::default().foreground(Color::hex(0x3d9a5b)).bold(true));
//! grid.write(1, 13, "终端里的汉字占两格", CharCellStyle::default());
//! grid.set_caret(CharGridCaret::at(2, 0));
//! char_grid(&grid).font_size(13).line_height(1.2);
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::rc::Rc;

use reactive::{Signal, detached, signal};
use scene::{Color, Quad};
use unicode_width::UnicodeWidthChar;

use crate::element::Element;
use crate::geometry::{Affine, Rect};
use crate::segmentation::graphemes;
use crate::text::{CellGlyph, CellMetrics, CellShape, GlyphHasher, Ink, TextStyle, TextSystem};

mod cluster;

use cluster::{clamped, cluster_width, first_cluster_len, simple_width, simple_width_of_code};

/// 一格的样式：前景色、背景色（`None` 是元素的文字颜色、透明）、粗体、斜体、下划线、删除线、反色
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CharCellStyle {
    /// 字的颜色；`None` 用元素的文字颜色（继承）
    pub foreground: Option<Color>,
    /// 格子的底色；`None` 是透明（露出元素的背景）
    pub background: Option<Color>,
    /// 粗体（字体有粗体字面就用，没有时加粗画）
    pub bold: bool,
    /// 斜体（字体有斜体字面就用，没有时倾斜画）
    pub italic: bool,
    /// 下划线
    pub underline: bool,
    /// 删除线
    pub strikethrough: bool,
    /// 反色：前景色和底色对调
    pub inverse: bool,
}

impl CharCellStyle {
    /// 字的颜色
    pub fn foreground(mut self, color: Color) -> Self {
        self.foreground = Some(color);
        self
    }

    /// 格子的底色
    pub fn background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// 粗体
    pub fn bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    /// 斜体
    pub fn italic(mut self, italic: bool) -> Self {
        self.italic = italic;
        self
    }

    /// 下划线
    pub fn underline(mut self, underline: bool) -> Self {
        self.underline = underline;
        self
    }

    /// 删除线
    pub fn strikethrough(mut self, strikethrough: bool) -> Self {
        self.strikethrough = strikethrough;
        self
    }

    /// 反色
    pub fn inverse(mut self, inverse: bool) -> Self {
        self.inverse = inverse;
        self
    }

    /// 什么都没设的样式（[`Default::default`] 在常量里用不了）
    const PLAIN: Self = Self { foreground: None, background: None, bold: false, italic: false, underline: false, strikethrough: false, inverse: false };
}

/// 一格的内容（[`CharGrid::cell`] 读到的）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CharCellContent {
    /// 一个字；空格是空的格子
    Char(char),
    /// 几个码点组成的一个字素簇：带变音符号的字、组合的 emoji
    Cluster(Rc<str>),
    /// 宽字（汉字、emoji）占的第二格
    WideTail,
}

/// 写成文字：宽字的第二格是空的
impl std::fmt::Display for CharCellContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Char(ch) => write!(f, "{ch}"),
            Self::Cluster(text) => f.write_str(text),
            Self::WideTail => Ok(()),
        }
    }
}

/// 一格：内容和样式。宽字（汉字、emoji）占两格，第二格的内容是 [`CharCellContent::WideTail`]
#[derive(Clone, Debug, PartialEq)]
pub struct CharCell {
    /// 这一格的内容
    pub content: CharCellContent,
    /// 样式
    pub style: CharCellStyle,
}

impl CharCell {
    /// 空的格子
    pub const BLANK: CharCell = CharCell { content: CharCellContent::Char(' '), style: CharCellStyle::PLAIN };
}

/// 插入点的形状
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaretShape {
    /// 盖住整格的方块，里面的字反色；网格没有焦点时画成空心的框
    #[default]
    Block,
    /// 格子左边的竖线
    Bar,
    /// 格子底下的横线
    Underline,
}

/// 插入点：在第几行第几列、形状、闪不闪、颜色
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharGridCaret {
    /// 行
    pub row: usize,
    /// 列
    pub column: usize,
    /// 形状
    pub shape: CaretShape,
    /// 闪烁（亮 0.5 秒、灭 0.5 秒，移动之后先亮着）
    pub blinking: bool,
    /// 不显示
    pub hidden: bool,
    /// 颜色；`None` 用元素的文字颜色
    pub color: Option<Color>,
}

impl CharGridCaret {
    /// 第 `row` 行第 `col` 列的方块插入点，闪烁
    pub fn at(row: usize, column: usize) -> Self {
        Self { row, column, shape: CaretShape::Block, blinking: true, hidden: false, color: None }
    }

    /// 换形状
    pub fn shape(mut self, shape: CaretShape) -> Self {
        self.shape = shape;
        self
    }

    /// 闪不闪
    pub fn blinking(mut self, blinking: bool) -> Self {
        self.blinking = blinking;
        self
    }

    /// 颜色
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }
}

/// 网格里的位置：第几行、第几列（列是格子之间的边界，0 到列数）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CharGridPosition {
    /// 行
    pub row: usize,
    /// 列
    pub column: usize,
}

impl CharGridPosition {
    /// 第 `row` 行第 `col` 列
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// 统计：一共重新生成过多少行的图元、整形过多少段字（所有显示这个网格的元素加起来）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CharGridStats {
    /// 重新生成过的行数
    pub rows_built: u64,
    /// 整形过的段数：一个簇，或者连字开着时同样式连续的一串格子。缓存里有的不算
    pub shapes_built: u64,
}

/// 一格存的内容，压成 4 个字节：一个字，或者这一行簇表的下标（最高位是 1，字最大才 21 位）。
/// `0` 是宽字占的第二格
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot(u32);

impl Slot {
    const WIDE_TAIL: Self = Self(0);
    const SPACE: Self = Self(' ' as u32);
    const CLUSTER: u32 = 1 << 31;

    fn from_char(ch: char) -> Self {
        Self(ch as u32)
    }

    fn from_cluster(index: usize) -> Self {
        Self(Self::CLUSTER | index as u32)
    }

    /// 放的是一个字（空格算，宽字的第二格和簇不算）
    #[inline]
    fn char(self) -> Option<char> {
        if self == Self::WIDE_TAIL { None } else { char::from_u32(self.0) }
    }

    /// 放的是簇：在这一行簇表里的下标
    #[inline]
    fn cluster(self) -> Option<usize> {
        (self.0 & Self::CLUSTER != 0).then_some((self.0 & !Self::CLUSTER) as usize)
    }
}

/// 存在行里的一格，大小和只放一个字时一样
#[derive(Clone, Copy)]
struct StoredCell {
    slot: Slot,
    style: CharCellStyle,
}

const _: () = assert!(size_of::<StoredCell>() == 20, "格子变大会让大网格的内存跟着涨，改之前想清楚");

impl StoredCell {
    const BLANK: Self = Self { slot: Slot::SPACE, style: CharCellStyle::PLAIN };
}

/// 一行：格子、这一行的簇表、版本号（每改一次换一个新的，元素按它缓存这一行的图元）
struct Row {
    cells: Vec<StoredCell>,
    /// 放不进一个字的簇。被覆盖的簇先留在这里，满了扫一遍（[`Self::add_cluster`]）；清空一行时一起清。
    /// 跟着行走，滚屏不用动。空着时不占堆内存
    clusters: Vec<Rc<str>>,
    version: u64,
}

impl Row {
    fn new(cols: usize, version: u64) -> Self {
        Self { cells: vec![StoredCell::BLANK; cols], clusters: Vec::new(), version }
    }

    /// 整行填成空格（`style` 可以带底色）
    fn clear(&mut self, style: CharCellStyle, version: u64) {
        self.cells.fill(StoredCell { slot: Slot::SPACE, style });
        self.clusters.clear();
        self.version = version;
    }

    /// 第 `col` 格的簇占几格
    #[inline]
    fn span_at(&self, col: usize) -> usize {
        if self.cells.get(col + 1).is_some_and(|c| c.slot == Slot::WIDE_TAIL) { 2 } else { 1 }
    }

    /// 第 `col` 格落在宽字的后一半时，退回前一半
    #[inline]
    fn head_of(&self, col: usize) -> usize {
        if col > 0 && self.cells.get(col).is_some_and(|c| c.slot == Slot::WIDE_TAIL) { col - 1 } else { col }
    }

    /// 把一个簇放进簇表，返回放在格子里的内容。表里最多有列数个还在用的（每个至少占一格），满了扫一遍，至少又能放列数个：
    /// 平摊下来每次放一个簇只多常数的工作
    fn add_cluster(&mut self, text: &str) -> Slot {
        if self.clusters.len() >= self.cells.len() * 2 + 8 {
            let mut kept = Vec::with_capacity(self.cells.len().min(self.clusters.len()));
            for cell in &mut self.cells {
                if let Some(index) = cell.slot.cluster() {
                    kept.push(self.clusters[index].clone());
                    cell.slot = Slot::from_cluster(kept.len() - 1);
                }
            }
            self.clusters = kept;
        }
        self.clusters.push(text.into());
        Slot::from_cluster(self.clusters.len() - 1)
    }

    /// 把字或簇放在第 `col` 格（`col` 在行内），宽字占两格，最后一列放不下时写空格。被拆开的宽字，另一半变成空格。
    /// 返回占了几格
    #[inline]
    fn place(&mut self, col: usize, slot: Slot, width: usize, style: CharCellStyle) -> usize {
        let cells = &mut self.cells[..];
        if col > 0 && cells[col].slot == Slot::WIDE_TAIL {
            cells[col - 1].slot = Slot::SPACE;
        }
        let span = if width == 2 && col + 1 < cells.len() { 2 } else { 1 };
        if let Some(next) = cells.get_mut(col + span)
            && next.slot == Slot::WIDE_TAIL
        {
            next.slot = Slot::SPACE;
        }
        cells[col] = StoredCell { slot: if width == 2 && span == 1 { Slot::SPACE } else { slot }, style };
        if span == 2 {
            cells[col + 1] = StoredCell { slot: Slot::WIDE_TAIL, style };
        }
        span
    }

    /// 第 `col` 列前面的簇是简单的字（或者前面没有格子）：简单的字接不上去，不用查规则
    #[inline]
    fn previous_is_simple(&self, col: usize) -> bool {
        if col == 0 {
            return true;
        }
        let previous = self.cells[col - 1].slot;
        // 汉字占两格，前一格是它的第二格
        let previous = if previous == Slot::WIDE_TAIL && col > 1 { self.cells[col - 2].slot } else { previous };
        simple_width_of_code(previous.0).is_some()
    }

    /// 写一个字，返回写完后的列（`col` 不超过列数）。这个字和前一个簇都简单时直接放，不走切分
    #[inline]
    fn put_char(&mut self, col: usize, ch: char, style: CharCellStyle) -> usize {
        if let Some(width) = simple_width(ch)
            && col < self.cells.len()
            && self.previous_is_simple(col)
        {
            return col + self.place(col, Slot::from_char(ch), width, style);
        }
        let mut buf = [0u8; 4];
        self.write(col, ch.encode_utf8(&mut buf), style)
    }

    /// 从第 `col` 列开始写一串字（不换行，超出行尾的截掉），按字素簇切开，返回写完后的列
    fn write(&mut self, col: usize, text: &str, style: CharCellStyle) -> usize {
        let cols = self.cells.len();
        let bytes = text.as_bytes();
        let (mut col, mut at) = self.join_previous(col, text);
        while at < bytes.len() && col < cols {
            // 最常见的先判断：可打印的 ASCII 后面还是 ASCII，不用解码
            let byte = bytes[at];
            if (0x20..0x7f).contains(&byte) && bytes.get(at + 1).is_none_or(|next| *next < 0x80) {
                col += self.place(col, Slot(byte as u32), 1, style);
                at += 1;
                continue;
            }
            // 接着是一串简单的字（汉字这些）：一个字后面还是简单的字，它自己就是一个簇。每个字只解码、查表一次
            let rest = &text[at..];
            let used = self.write_simple(&mut col, rest, style);
            if used > 0 {
                at += used;
                continue;
            }
            // 查规则：可能是一个组合的簇
            let len = first_cluster_len(rest);
            at += len;
            let cluster = clamped(&rest[..len]);
            let width = cluster_width(cluster);
            if width == 0 {
                continue;
            }
            let mut chars = cluster.chars();
            let slot = match (chars.next(), chars.next()) {
                (Some(only), None) => Slot::from_char(only),
                _ => self.add_cluster(cluster),
            };
            col += self.place(col, slot, width, style);
        }
        col
    }

    /// 写 `text` 开头的一串简单的字，停在写满一行、不简单的字、或者后面跟着不简单的字的那个字上（它可能和后面的连成一个簇）。
    /// 返回用掉的字节数
    #[inline]
    fn write_simple(&mut self, col: &mut usize, text: &str, style: CharCellStyle) -> usize {
        let cols = self.cells.len();
        let mut chars = text.chars();
        let mut used = 0;
        let Some(mut ch) = chars.next() else { return 0 };
        let mut width = simple_width(ch);
        while let Some(w) = width
            && *col < cols
        {
            let next = chars.next();
            let next_width = next.and_then(simple_width);
            if next.is_some() && next_width.is_none() {
                break;
            }
            *col += self.place(*col, Slot::from_char(ch), w, style);
            used += ch.len_utf8();
            let Some(next) = next else { break };
            ch = next;
            width = next_width;
        }
        used
    }

    /// 上一次写入可能停在组合序列中间（逐字符写的终端、分段到来的输出）：`text` 开头能和第 `col` 列前面那个簇连成一个簇的话，
    /// 接上去（保持那一格的样式；变宽时盖住后面一格，最后一列放不下时不接，丢掉这几个字）。返回接着写的列和 `text` 里用掉的字节数
    fn join_previous(&mut self, col: usize, text: &str) -> (usize, usize) {
        let unchanged = (col, 0);
        let Some(first) = text.chars().next().filter(|_| col > 0 && col <= self.cells.len()) else { return unchanged };
        // 两边都是简单的字（绝大多数时候）：连不上，不用查规则
        if simple_width(first).is_some() && self.previous_is_simple(col) {
            return unchanged;
        }
        let head = self.head_of(col - 1);
        let previous = self.cells[head].slot;
        let mut joined = String::with_capacity(cluster::MAX_CLUSTER * 2);
        match (previous.char(), previous.cluster()) {
            (Some(ch), _) => joined.push(ch),
            (_, Some(index)) => joined.push_str(&self.clusters[index]),
            _ => return unchanged,
        }
        let before = joined.len();
        joined.push_str(clamped(text));
        let end = first_cluster_len(&joined);
        if end <= before {
            return unchanged;
        }
        let used = end - before;
        let cluster = clamped(&joined[..end]);
        // 这一格已经满了：多出来的组合字符丢掉
        if cluster.len() <= before {
            return (col, used);
        }
        let (old, new) = (self.span_at(head), cluster_width(cluster).max(1));
        if new > old && head + new > self.cells.len() {
            return (col, used);
        }
        let slot = self.add_cluster(cluster);
        match new.cmp(&old) {
            std::cmp::Ordering::Greater => {
                let style = self.cells[head].style;
                self.place(head, slot, new, style);
            }
            std::cmp::Ordering::Less => {
                self.cells[head].slot = slot;
                self.cells[head + 1].slot = Slot::SPACE;
            }
            std::cmp::Ordering::Equal => self.cells[head].slot = slot,
        }
        (head + new, used)
    }

    /// 读一格（公开的 [`CharCell`]）
    fn cell(&self, col: usize) -> Option<CharCell> {
        let cell = self.cells.get(col)?;
        let content = match (cell.slot.char(), cell.slot.cluster()) {
            (Some(ch), _) => CharCellContent::Char(ch),
            (_, Some(index)) => CharCellContent::Cluster(self.clusters[index].clone()),
            _ => CharCellContent::WideTail,
        };
        Some(CharCell { content, style: cell.style })
    }

    /// `from..to` 列的文字：宽字的第二格跳过，去掉行尾的空格
    fn text(&self, from: usize, to: usize) -> String {
        let len = self.cells.len();
        let mut text = String::new();
        for cell in &self.cells[from.min(len)..to.min(len)] {
            match (cell.slot.char(), cell.slot.cluster()) {
                (Some(ch), _) => text.push(ch),
                (_, Some(index)) => text.push_str(&self.clusters[index]),
                _ => {}
            }
        }
        text.truncate(text.trim_end().len());
        text
    }
}

struct GridData {
    cols: usize,
    rows: Vec<Row>,
    next_version: u64,
    caret: CharGridCaret,
    /// 插入点最近一次动的时间（闪烁从这里算起，动过先亮着）；由元素在下一帧填上
    caret_moved: bool,
    selection: Option<(CharGridPosition, CharGridPosition)>,
    rows_built: u64,
    shapes_built: u64,
    /// 已经通知过元素这一帧要重画（一帧里写很多格只通知一次）
    notified: bool,
    /// 行数、列数变了：要重新布局
    resized: bool,
}

/// 字符网格的数据：行 × 列的格子、插入点、选区。是一个句柄，`clone` 出来的是同一个网格；
/// 用 [`char_grid`] 显示。写的时候记下哪几行变了，下一帧只重新生成这几行的图元。
///
/// 只能在主线程用（和信号一样）。
///
/// **一格放一个字素簇**：带变音符号的字、组合的 emoji（ZWJ、肤色、国旗、键帽、VS16）都在一格或两格里，不拆开。
/// 占几格按 `unicode-width` 对整个簇的算法，最多两格：普通的字一格，汉字、emoji 两格，变音符号不加宽度。写到一半宽字上时，
/// 被拆开的另一半变成空格；最后一列放不下两格的簇时写空格。
///
/// **组合字符接到前一格上**：[`Self::set`]、[`Self::write`] 写的开头如果和前一格的簇连得成一个簇（变音符号、`ZWJ` 和它后面的字、
/// 第二个地区指示符），接上去，样式还是前一格的。所以逐字符写和一次写结果一样。接不上的零宽字符丢掉，控制字符也丢掉
#[derive(Clone)]
pub struct CharGrid {
    data: Rc<RefCell<GridData>>,
    /// 改了就加一：显示它的元素订阅这个信号，下一帧重画
    version: Signal<u64>,
}

impl CharGrid {
    /// `cols` 列、`rows` 行的空网格，插入点在左上角
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut data = GridData {
            cols,
            rows: Vec::with_capacity(rows),
            next_version: 1,
            caret: CharGridCaret::at(0, 0),
            caret_moved: true,
            selection: None,
            rows_built: 0,
            shapes_built: 0,
            notified: false,
            resized: false,
        };
        for _ in 0..rows {
            let version = data.bump();
            data.rows.push(Row::new(cols, version));
        }
        // 网格可以比创建它的作用域活得久（放在应用状态里），信号不归任何作用域
        let version = detached(|| signal(0u64));
        Self { data: Rc::new(RefCell::new(data)), version }
    }

    /// 改了就加一的信号（元素订阅它）
    pub(crate) fn version_signal(&self) -> Signal<u64> {
        self.version
    }

    /// `[列数, 行数]`
    pub fn size(&self) -> [usize; 2] {
        let data = self.data.borrow();
        [data.cols, data.rows.len()]
    }

    /// 改 `f` 里的数据，然后通知显示它的元素（一帧里第一次改时才通知）
    fn update<R>(&self, f: impl FnOnce(&mut GridData) -> R) -> R {
        let (result, notify) = {
            let mut data = self.data.borrow_mut();
            let result = f(&mut data);
            (result, !std::mem::replace(&mut data.notified, true))
        };
        if notify {
            self.version.update(|v| *v += 1);
        }
        result
    }

    /// 第 `row` 行第 `col` 列写一个字，返回写完后的列；宽字占这一格和下一格（最后一列放不下宽字时写空格）。越界的忽略。
    /// 组合字符（变音符号、`ZWJ` 和它后面的字、肤色……）接到前一格的簇上，这时 `style` 不起作用：
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let grid = CharGrid::new(10, 1);
    /// let mut col = 0;
    /// for ch in "e\u{301}x".chars() {
    ///     col = grid.set(0, col, ch, CharCellStyle::default()); // 终端逐字符写
    /// }
    /// assert_eq!(col, 2);
    /// assert_eq!(grid.row_text(0), "e\u{301}x");
    /// ```
    pub fn set(&self, row: usize, col: usize, ch: char, style: CharCellStyle) -> usize {
        self.update(|data| {
            let version = data.bump();
            match data.rows.get_mut(row) {
                Some(r) if col <= r.cells.len() => {
                    r.version = version;
                    r.put_char(col, ch, style)
                }
                _ => col,
            }
        })
    }

    /// 从第 `row` 行第 `col` 列开始写一串字（不换行，超出行尾的截掉），按字素簇切开，返回写完后的列
    pub fn write(&self, row: usize, col: usize, text: &str, style: CharCellStyle) -> usize {
        self.update(|data| {
            let version = data.bump();
            match data.rows.get_mut(row) {
                Some(r) if col <= r.cells.len() && !text.is_empty() => {
                    r.version = version;
                    r.write(col, text, style)
                }
                _ => col,
            }
        })
    }

    /// 把第 `row` 行从 `cols` 范围里的格子填成同一个字（`' '` 就是清掉）
    pub fn set_cells(&self, row: usize, cols: std::ops::Range<usize>, ch: char, style: CharCellStyle) {
        let width = ch.width().unwrap_or(0);
        self.update(|data| {
            let version = data.bump();
            let columns = data.cols;
            let Some(r) = data.rows.get_mut(row).filter(|_| width > 0 && cols.start < columns) else { return };
            let mut col = cols.start;
            while col < cols.end.min(columns) {
                col += r.place(col, Slot::from_char(ch), width, style);
            }
            r.version = version;
        });
    }

    /// 清空一行（空格，样式是 `style`：可以带底色）
    pub fn clear_row(&self, row: usize, style: CharCellStyle) {
        self.update(|data| {
            let version = data.bump();
            if let Some(r) = data.rows.get_mut(row) {
                r.clear(style, version);
            }
        });
    }

    /// 清空整个网格
    pub fn clear(&self) {
        self.update(|data| {
            for row in 0..data.rows.len() {
                let version = data.bump();
                data.rows[row].clear(CharCellStyle::PLAIN, version);
            }
            data.selection = None;
        });
    }

    /// 整体上移 `n` 行（终端滚屏）：顶上的 `n` 行移出去，底下补 `n` 行空行。只挪行，没变的行不用重新生成图元；
    /// 选区跟着上移，移出去的部分去掉
    pub fn scroll_up(&self, n: usize) {
        self.update(|data| {
            let n = n.min(data.rows.len());
            data.rows.rotate_left(n);
            let len = data.rows.len();
            for row in len - n..len {
                let version = data.bump();
                data.rows[row].clear(CharCellStyle::PLAIN, version);
            }
            data.selection = data.selection.and_then(|(a, b)| {
                (b.row >= n).then(|| {
                    let a = if a.row >= n { CharGridPosition::new(a.row - n, a.column) } else { CharGridPosition::new(0, 0) };
                    (a, CharGridPosition::new(b.row - n, b.column))
                })
            });
        });
    }

    /// 改成 `cols` 列、`rows` 行：多出来的格子是空的，少了的截掉；元素下一帧重新布局
    pub fn resize(&self, cols: usize, rows: usize) {
        self.update(|data| {
            if (data.cols, data.rows.len()) == (cols, rows) {
                return;
            }
            data.cols = cols;
            for row in &mut data.rows {
                // 新的最后一列是宽字的前一半：另一半切掉了，变成空格
                if cols > 0 && row.cells.get(cols).is_some_and(|c| c.slot == Slot::WIDE_TAIL) {
                    row.cells[cols - 1].slot = Slot::SPACE;
                }
                row.cells.resize(cols, StoredCell::BLANK);
            }
            data.rows.truncate(rows);
            while data.rows.len() < rows {
                data.rows.push(Row::new(cols, 0));
            }
            for row in 0..rows {
                let version = data.bump();
                data.rows[row].version = version;
            }
            data.resized = true;
        });
    }

    /// 一格的内容；越界时是 `None`
    pub fn cell(&self, row: usize, col: usize) -> Option<CharCell> {
        self.data.borrow().rows.get(row)?.cell(col)
    }

    /// 一行的文字（宽字只算一次，去掉行尾的空格）
    pub fn row_text(&self, row: usize) -> String {
        let data = self.data.borrow();
        data.rows.get(row).map(|r| r.text(0, r.cells.len())).unwrap_or_default()
    }

    /// 插入点
    pub fn caret(&self) -> CharGridCaret {
        self.data.borrow().caret
    }

    /// 放插入点（闪烁的插入点移动之后先亮着）
    pub fn set_caret(&self, caret: CharGridCaret) {
        self.update(|data| {
            if data.caret != caret {
                data.caret = caret;
                data.caret_moved = true;
            }
        });
    }

    /// 选区：从 `start` 到 `end`（按阅读顺序，`end` 不含）。也可以用指针在网格上拖出来，⌘C 复制。
    /// 画和复制时都按整个簇：一端落在宽字中间时扩到整个字
    pub fn selection(&self) -> Option<(CharGridPosition, CharGridPosition)> {
        self.data.borrow().selection
    }

    /// 设置选区（`None` 清掉）
    pub fn set_selection(&self, selection: Option<(CharGridPosition, CharGridPosition)>) {
        self.update(|data| data.selection = selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) }));
    }

    /// 选中的文字：行与行之间换行，每行去掉行尾的空格
    pub fn selected_text(&self) -> Option<String> {
        let data = self.data.borrow();
        let (start, end) = data.whole_clusters_selected().filter(|_| !data.rows.is_empty())?;
        let mut lines = Vec::new();
        for row in start.row..=end.row.min(data.rows.len().saturating_sub(1)) {
            let r = &data.rows[row];
            let from = if row == start.row { start.column } else { 0 };
            let to = if row == end.row { end.column } else { r.cells.len() };
            lines.push(r.text(from, to));
        }
        Some(lines.join("\n"))
    }

    /// 统计：一共重新生成过多少行的图元、整形过多少段字
    pub fn stats(&self) -> CharGridStats {
        let data = self.data.borrow();
        CharGridStats { rows_built: data.rows_built, shapes_built: data.shapes_built }
    }
}

impl GridData {
    fn bump(&mut self) -> u64 {
        self.next_version += 1;
        self.next_version
    }

    /// 非空的选区，两端落在宽字中间时扩到整个簇
    fn whole_clusters_selected(&self) -> Option<(CharGridPosition, CharGridPosition)> {
        let (mut start, mut end) = self.selection.filter(|(a, b)| a != b)?;
        if let Some(row) = self.rows.get(start.row) {
            start.column = row.head_of(start.column);
        }
        if let Some(row) = self.rows.get(end.row)
            && row.head_of(end.column) != end.column
        {
            end.column += 1;
        }
        Some((start, end))
    }
}

/// 字符网格元素：显示一个 [`CharGrid`]。大小是列数 × 格宽、行数 × 格高（格宽是等宽字体的字宽，格高是字号 × 行高，
/// 都对齐到物理像素），字号、行高、字体族、文字颜色可以设在元素上或者继承（字体族默认是系统的等宽字体；
/// 终端一般用 `.line_height(1.2)`）。
///
/// 指针拖动选择格子（按整个字素簇选，双击选一串不是空格的字，三击选整行），⌘C 复制、⌘A 全选；点它获得键盘焦点，
/// 按键用 [`Element::on_key_down`] 接（终端把按键交给自己的程序）。没有焦点时方块插入点画成空心的、选区画成不活跃的颜色。
/// 读屏软件读到的是网格里的全部文字。要字体的连字（编程字体的 `->`）时设 [`Element::ligated`]
pub fn char_grid(grid: &CharGrid) -> Element {
    Element::from_kind(CharGridBlock::new(grid.clone()))
}

/// 一行生成好的图元（行内坐标：左上角是这一行的左上角）
struct RowQuads {
    backgrounds: Vec<Quad>,
    glyphs: Vec<Quad>,
    /// 每个图元的颜色从哪来（和上面两组一一对应）：没设颜色的格子用元素的文字颜色、底下的颜色，
    /// 画的时候才填上，文字颜色在过渡时不用重新生成
    background_inks: Vec<Tint>,
    glyph_inks: Vec<Tint>,
}

/// 图元的颜色：生成时定好的，或者画的时候填的文字颜色、底色
#[derive(Clone, Copy, PartialEq)]
enum Tint {
    Fixed,
    Text,
    Backdrop,
}

/// 一格的字和底的颜色
#[derive(Clone, Copy, PartialEq)]
enum Paint {
    Color(Color),
    Text,
    Backdrop,
}

impl Paint {
    fn resolve(self) -> (Color, Tint) {
        match self {
            Self::Color(c) => (c, Tint::Fixed),
            Self::Text => (Color::TRANSPARENT, Tint::Text),
            Self::Backdrop => (Color::TRANSPARENT, Tint::Backdrop),
        }
    }
}

/// 缓存的图元在这些都没变时才能用
#[derive(Clone, Copy, PartialEq)]
struct CacheKey {
    style: TextStyle,
    scale: f32,
    /// 字形图集扩大过就要重新取字形的位置
    atlas: u64,
    ligated: bool,
}

/// 字符网格元素的内容：数据的句柄、格子的尺寸、字形和每行图元的缓存
pub(crate) struct CharGridBlock {
    pub grid: CharGrid,
    /// 继承来的选区颜色
    pub selection: Color,
    style: TextStyle,
    metrics: Option<(f32, CellMetrics)>,
    key: Option<CacheKey>,
    /// 字（低 24 位）+ 粗斜体（高 8 位）→ 字形（`None` 是哪个字体里都没有）。生成一行时每格查一次，用快的哈希
    glyphs: HashMap<u32, Option<CellGlyph>, BuildHasherDefault<GlyphHasher>>,
    /// 同样式连续的格子一起整形（[`Element::ligated`]）
    pub ligated: bool,
    /// 整形过的字（簇，连字开着时是一串格子）→ 结果，按粗斜体分四张表
    shapes: [HashMap<Box<str>, Rc<CellShape>>; 4],
    /// 上面几张表一共存了多少字形，到 [`SHAPE_CACHE_GLYPHS`] 就全部丢掉
    shape_glyphs: usize,
    /// 这一帧整形过几段（记进网格的统计）
    shapes_built: u64,
    /// 连字开着时拼一串格子的文字、每格文字的起点（字节）和列；留着内存，不用每行都分配
    run_text: String,
    run_cells: Vec<(u32, u32)>,
    /// 行的版本号 → 这一行的图元
    rows: HashMap<u64, RowQuads>,
    /// 插入点闪烁：从什么时候算起、现在亮不亮
    blink_epoch: f64,
    pub caret_on: bool,
    /// 双击、三击之后拖动按词、按行扩展：一开始选中的范围
    unit: Option<(u32, CharGridPosition, CharGridPosition)>,
    anchor: Option<CharGridPosition>,
    /// 接文字输入（有 [`crate::Element::on_text_insert`]）：有焦点时开输入法，预编辑画在插入点处
    pub text_input: bool,
    /// 输入法的预编辑文字和其中的插入点（字节偏移）；没在组字时是 `None`
    pub preedit: Option<(String, Option<usize>)>,
    /// 内容区左上角在元素里的位置（绘制时更新）：插入点区域换算成元素坐标
    pub inset: [f32; 2],
}

/// 插入点亮、灭各这么久
const BLINK: f64 = 0.5;
/// 整形缓存最多存这么多字形（一个字形 16 字节）：连字开着时不停输出没见过的文字，缓存也不会一直涨
const SHAPE_CACHE_GLYPHS: usize = 1 << 16;

impl CharGridBlock {
    pub fn new(grid: CharGrid) -> Self {
        Self {
            grid,
            selection: Color::TRANSPARENT,
            style: TextStyle::default(),
            metrics: None,
            key: None,
            glyphs: HashMap::default(),
            ligated: false,
            shapes: Default::default(),
            shape_glyphs: 0,
            shapes_built: 0,
            run_text: String::new(),
            run_cells: Vec::new(),
            rows: HashMap::new(),
            blink_epoch: 0.0,
            caret_on: true,
            unit: None,
            anchor: None,
            text_input: false,
            preedit: None,
            inset: [0.0, 0.0],
        }
    }

    /// 插入点所在的格子（组字时是预编辑文字占的那几格），相对内容区左上角；还没量过格子时按 0 大小
    pub fn caret_rect(&self) -> Rect {
        let m = self.metrics.map(|(_, m)| [m.width, m.height]).unwrap_or_default();
        let caret = self.grid.caret();
        let data = self.grid.data.borrow();
        let row = data.rows.get(caret.row);
        let column = row.map_or(caret.column, |r| r.head_of(caret.column));
        let span = match &self.preedit {
            Some((text, _)) => preedit_columns(text).max(1),
            None => row.map_or(1, |r| r.span_at(column)),
        };
        Rect { x: column as f32 * m[0], y: caret.row as f32 * m[1], width: span as f32 * m[0], height: m[1] }
    }

    /// 继承来的文字样式变了，返回 true（要重新布局）
    pub fn set_style(&mut self, style: TextStyle) -> bool {
        if self.style == style {
            return false;
        }
        self.style = style;
        self.invalidate();
        true
    }

    /// 丢掉格子尺寸、字形和图元的缓存（加载了新字体、换了样式）
    pub fn invalidate(&mut self) {
        self.metrics = None;
        self.key = None;
        self.glyphs.clear();
        self.shapes = Default::default();
        self.shape_glyphs = 0;
        self.rows.clear();
    }

    fn metrics(&mut self, text: &mut TextSystem) -> CellMetrics {
        let scale = text.scale();
        match self.metrics {
            Some((s, m)) if s == scale => m,
            _ => {
                let m = text.cell_metrics(&self.style);
                self.metrics = Some((scale, m));
                m
            }
        }
    }

    /// 大小：列数 × 格宽、行数 × 格高
    pub fn measure(&mut self, text: &mut TextSystem) -> [f32; 2] {
        let m = self.metrics(text);
        let [cols, rows] = self.grid.size();
        self.grid.data.borrow_mut().resized = false;
        [cols as f32 * m.width, rows as f32 * m.height]
    }

    /// 行列数变了（要重新布局），顺便清掉"已通知"
    pub fn take_resized(&mut self) -> bool {
        std::mem::take(&mut self.grid.data.borrow_mut().resized)
    }

    /// 插入点下一次换亮灭的时刻（统一时钟的秒，`now` 之后）：插入点不显示、不闪时是 `None`
    pub fn next_blink(&self, now: f64) -> Option<f64> {
        let caret = self.grid.data.borrow().caret;
        (!caret.hidden && caret.blinking).then(|| crate::editor::next_toggle(self.blink_epoch, now, BLINK))
    }

    /// 推进插入点闪烁，返回亮灭是否变了
    pub fn blink(&mut self, now: f64) -> bool {
        let mut data = self.grid.data.borrow_mut();
        if std::mem::take(&mut data.caret_moved) {
            self.blink_epoch = now;
        }
        let on = !data.caret.blinking || (((now - self.blink_epoch).max(0.0) / BLINK) as u64).is_multiple_of(2);
        std::mem::replace(&mut self.caret_on, on) != on
    }

    /// 内容区里的一点（相对网格左上角）在哪一行、靠近哪条格子边界
    fn pos_at(&self, [x, y]: [f32; 2]) -> CharGridPosition {
        let Some((_, m)) = self.metrics else { return CharGridPosition::default() };
        let [cols, rows] = self.grid.size();
        let row = ((y / m.height).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let mut col = ((x / m.width).round().max(0.0) as usize).min(cols);
        // 落在宽字中间：去近的那一边，不把一个簇切开
        if let Some(r) = self.grid.data.borrow().rows.get(row)
            && r.head_of(col) != col
        {
            col = if x / m.width < col as f32 { col - 1 } else { col + 1 };
        }
        CharGridPosition::new(row, col)
    }

    /// 双击：一串不是空格的字；三击：整行
    fn unit_at(&self, point: [f32; 2], clicks: u32) -> (CharGridPosition, CharGridPosition) {
        let Some((_, m)) = self.metrics else { return Default::default() };
        let [cols, _] = self.grid.size();
        if cols == 0 || self.grid.data.borrow().rows.is_empty() {
            return Default::default();
        }
        let row = self.pos_at(point).row;
        if clicks >= 3 {
            return (CharGridPosition::new(row, 0), CharGridPosition::new(row, cols));
        }
        let col = ((point[0] / m.width).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let data = self.grid.data.borrow();
        let cells = &data.rows[row].cells;
        let word = |c: &StoredCell| c.slot != Slot::SPACE;
        if !word(&cells[col]) {
            return (CharGridPosition::new(row, col), CharGridPosition::new(row, col + 1));
        }
        let start = (0..=col).rev().take_while(|&i| word(&cells[i])).last().unwrap_or(col);
        let end = (col..cols).take_while(|&i| word(&cells[i])).last().map_or(col + 1, |i| i + 1);
        (CharGridPosition::new(row, start), CharGridPosition::new(row, end))
    }

    /// 指针按下（`point` 相对网格左上角）：单击放下起点（Shift 扩展），双击选词，三击选整行
    pub fn press(&mut self, point: [f32; 2], clicks: u32, extend: bool) {
        let pos = self.pos_at(point);
        let selection = match clicks {
            1 if extend && self.anchor.is_some() => Some((self.anchor.unwrap_or(pos), pos)),
            1 => {
                self.anchor = Some(pos);
                self.unit = None;
                None
            }
            _ => {
                let (a, b) = self.unit_at(point, clicks);
                self.anchor = Some(a);
                self.unit = Some((clicks, a, b));
                Some((a, b))
            }
        };
        self.set_selection(selection);
    }

    /// 拖动：扩展选区（双击、三击之后按词、按行）
    pub fn drag(&mut self, point: [f32; 2]) {
        let Some(anchor) = self.anchor else { return };
        let selection = match self.unit {
            Some((clicks, a, b)) => {
                let (c, d) = self.unit_at(point, clicks);
                (a.min(c), b.max(d))
            }
            None => (anchor, self.pos_at(point)),
        };
        self.set_selection(Some(selection));
    }

    /// 全选
    pub fn select_all(&mut self) {
        let [cols, rows] = self.grid.size();
        self.anchor = Some(CharGridPosition::default());
        self.set_selection(Some((CharGridPosition::default(), CharGridPosition::new(rows.saturating_sub(1), cols))));
    }

    /// 元素自己改选区（指针）：直接写进数据，不再通知自己（调用的地方已经标记了重画）
    fn set_selection(&mut self, selection: Option<(CharGridPosition, CharGridPosition)>) {
        let selection = selection.filter(|(a, b)| a != b).map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        self.grid.data.borrow_mut().selection = selection;
    }

    /// 生成图元。`origin` 是内容区左上角（布局坐标），`world` 是元素的累积变换；`focused` 时选区、插入点画成有焦点的样子
    pub fn paint(
        &mut self,
        text: &mut TextSystem,
        origin: [f32; 2],
        world: &Affine,
        ink: Ink,
        background: Color,
        selection: [Color; 2],
        focused: bool,
        quads: &mut Vec<Quad>,
    ) {
        let m = self.metrics(text);
        let scale = text.scale();
        let key = CacheKey { style: self.style, scale, atlas: text.atlas_generation(), ligated: self.ligated };
        if self.key != Some(key) {
            self.rows.clear();
            self.key = Some(key);
        }
        let data_ref = self.grid.data.clone();
        let mut data = data_ref.borrow_mut();
        data.notified = false;
        let cols = data.cols;

        // 行的图元：版本号没变的直接用，变了的重新生成（缓存换成这一帧用到的行）
        let mut cached = std::mem::take(&mut self.rows);
        let mut order = Vec::with_capacity(data.rows.len());
        let mut built = 0;
        for row in &data.rows {
            if let std::collections::hash_map::Entry::Vacant(slot) = cached.entry(row.version) {
                slot.insert(self.build_row(text, row, &m));
                built += 1;
            }
            order.push(row.version);
        }
        data.rows_built += built;
        data.shapes_built += std::mem::take(&mut self.shapes_built);
        // 图集满了：有的行缺字。扩大图集后会重画，那时图集的代数变了，所有行都重新生成

        // 对齐物理像素：没有缩放旋转时整个网格的左上角取整到物理像素，行内的图元已经是对齐的
        let snap = world.is_translation();
        let base = if snap {
            let [x, y] = world.apply(origin);
            [(x * scale).round() / scale, (y * scale).round() / scale]
        } else {
            origin
        };
        let backdrop = if background.is_transparent() { Color::WHITE } else { background };
        let tint = |quad: &Quad, tint: Tint| match tint {
            Tint::Fixed => *quad,
            Tint::Text => Quad { fill: ink.color, ..*quad },
            Tint::Backdrop => Quad { fill: backdrop, ..*quad },
        };
        let place = |quad: &Quad, dy: f32, opacity: f32| {
            let mut q = *quad;
            q.rect[0] += base[0];
            q.rect[1] += base[1] + dy;
            if !snap {
                let [w, h] = [q.rect[2], q.rect[3]];
                let [cx, cy] = world.apply([q.rect[0] + w * 0.5, q.rect[1] + h * 0.5]);
                q.rect[0] = cx - w * 0.5;
                q.rect[1] = cy - h * 0.5;
                q.transform = world.linear;
            }
            q.opacity *= opacity;
            q
        };
        let opacity = ink.opacity;
        // 先画所有行的底色，再画选区和方块插入点，最后画字（字在选区上面）
        for (i, version) in order.iter().enumerate() {
            let (dy, row) = (i as f32 * m.height, &cached[version]);
            quads.extend(row.backgrounds.iter().zip(&row.background_inks).map(|(q, t)| place(&tint(q, *t), dy, opacity)));
        }
        let rect = |col: usize, span: usize| Quad::new(col as f32 * m.width, 0.0, span as f32 * m.width, m.height);
        if let Some((start, end)) = data.whole_clusters_selected() {
            let fill = if focused { selection[0] } else { selection[1] };
            for row in start.row..=end.row.min(data.rows.len().saturating_sub(1)) {
                let from = if row == start.row { start.column } else { 0 };
                let to = if row == end.row { end.column } else { cols };
                if to > from {
                    quads.push(place(&rect(from, to - from).fill(fill), row as f32 * m.height, opacity));
                }
            }
        }
        let caret = data.caret;
        let show_caret = !caret.hidden && self.caret_on && caret.row < data.rows.len() && caret.column < cols;
        let caret_color = caret.color.unwrap_or(ink.color);
        // 插入点盖住的簇：第一格和占几格
        let under = show_caret.then(|| {
            let row = &data.rows[caret.row];
            let head = row.head_of(caret.column);
            (head, row.span_at(head))
        });
        let px = 1.0 / scale;
        let line = ((2.0 * scale).round().max(1.0)) / scale;
        let dy = caret.row as f32 * m.height;
        if let Some((head, span)) = under.filter(|_| caret.shape == CaretShape::Block && self.preedit.is_none()) {
            let block = rect(head, span);
            let block = if focused { block.fill(caret_color) } else { block.fill(Color::TRANSPARENT).border(px.max(1.0), caret_color) };
            quads.push(place(&block, dy, opacity));
        }
        for (i, version) in order.iter().enumerate() {
            let (dy, row) = (i as f32 * m.height, &cached[version]);
            quads.extend(row.glyphs.iter().zip(&row.glyph_inks).map(|(q, t)| place(&tint(q, *t), dy, opacity)));
        }
        // 输入法的预编辑画在插入点处：盖住底下的格子，加下划线，插入点画在预编辑里的位置
        let preedit = self.preedit.clone().filter(|_| caret.row < data.rows.len());
        let under = if preedit.is_some() { None } else { under };
        // 预编辑盖住的第一格：插入点落在宽字中间时从这个字开始
        let preedit_start = data.rows.get(caret.row).map_or(caret.column, |r| r.head_of(caret.column));
        if let Some((preedit, preedit_caret)) = &preedit {
            let mut col = preedit_start;
            let mut caret_col = None;
            let mut glyphs = Vec::new();
            let mut at = 0;
            for end in graphemes(preedit).skip(1) {
                let cluster = &preedit[at..end];
                if preedit_caret.is_some_and(|c| c <= at) && caret_col.is_none() {
                    caret_col = Some(col);
                }
                at = end;
                let width = cluster_width(cluster);
                if width == 0 || col + width > cols {
                    continue;
                }
                let mut chars = cluster.chars();
                match (chars.next(), chars.next()) {
                    (Some(ch), None) => {
                        if !drawn(ch, col as f32 * m.width, &m, scale, ink.color, &mut glyphs) {
                            self.push_glyph(text, ch, 0, col, width, &m, ink.color, &mut glyphs);
                        }
                    }
                    _ => self.push_cluster(text, cluster, 0, col, width, &m, ink.color, &mut glyphs),
                }
                col += width;
            }
            let span = col.saturating_sub(preedit_start);
            quads.push(place(&rect(preedit_start, span.max(1)).fill(backdrop), dy, opacity));
            quads.extend(glyphs.iter().map(|q| place(q, dy, opacity)));
            quads.push(place(&Quad::new(preedit_start as f32 * m.width, m.height - line, span as f32 * m.width, line).fill(ink.color), dy, opacity));
            if focused && self.caret_on && preedit_caret.is_some() {
                let at = caret_col.unwrap_or(col);
                quads.push(place(&Quad::new(at as f32 * m.width, 0.0, line, m.height).fill(caret_color), dy, opacity));
            }
        }
        if let Some((head, span)) = under {
            let left = head as f32 * m.width;
            match caret.shape {
                // 实心方块里的字反色：这一行方块范围里的字形、线用底色再画一遍。连字可能只有一部分在方块里，按方块切开
                CaretShape::Block if focused => {
                    let right = left + span as f32 * m.width;
                    let row = &cached[&order[caret.row]];
                    for quad in row.glyphs.iter().filter(|q| q.kind != Quad::COLOR_GLYPH) {
                        if let Some(inside) = clipped_x(quad, left, right) {
                            quads.push(place(&inside.fill(backdrop), dy, opacity));
                        }
                    }
                }
                CaretShape::Block => {}
                CaretShape::Bar => quads.push(place(&Quad::new(left, 0.0, line, m.height).fill(caret_color), dy, opacity)),
                CaretShape::Underline => {
                    let bar = Quad::new(left, m.height - line, span as f32 * m.width, line).fill(caret_color);
                    quads.push(place(&bar, dy, opacity));
                }
            }
        }
        drop(data);
        self.rows = cached;
        // 这一帧没用到的行（被覆盖、移出去的）不留
        if self.rows.len() > order.len() {
            self.rows.retain(|v, _| order.contains(v));
        }
    }

    /// 生成一行的图元：同样底色的相邻格子合成一块，每个字一个字形图元（方块、制表线自己画），簇和连字整形，
    /// 下划线、删除线一段一段地画
    fn build_row(&mut self, text: &mut TextSystem, source: &Row, m: &CellMetrics) -> RowQuads {
        let cells = &source.cells[..];
        let mut row = RowQuads {
            backgrounds: Vec::new(),
            glyphs: Vec::with_capacity(cells.len()),
            background_inks: Vec::new(),
            glyph_inks: Vec::with_capacity(cells.len()),
        };
        let colors = |cell: &StoredCell| {
            let f = cell.style.foreground.map_or(Paint::Text, Paint::Color);
            let b = cell.style.background.map(Paint::Color);
            match cell.style.inverse {
                true => (b.unwrap_or(Paint::Backdrop), Some(f)),
                false => (f, b),
            }
        };
        let mut col = 0;
        while col < cells.len() {
            let (_, b) = colors(&cells[col]);
            let start = col;
            col += 1;
            while col < cells.len() && colors(&cells[col]).1 == b {
                col += 1;
            }
            if let Some(b) = b.filter(|b| *b != Paint::Color(Color::TRANSPARENT)) {
                let (color, tint) = b.resolve();
                row.backgrounds.push(Quad::new(start as f32 * m.width, 0.0, (col - start) as f32 * m.width, m.height).fill(color));
                row.background_inks.push(tint);
            }
        }
        if self.ligated {
            let mut col = 0;
            while col < cells.len() {
                let (color, tint) = colors(&cells[col]).0.resolve();
                let before = row.glyphs.len();
                // 同样式连续的几格一起整形（一个簇的不用）
                let end = shaped_run_end(cells, col);
                if end > col + source.span_at(col) {
                    self.push_run(text, source, col, end, m, color, &mut row.glyphs);
                } else {
                    self.push_cell(text, source, col, m, color, &mut row.glyphs);
                }
                for (i, cell) in cells[col..end].iter().enumerate() {
                    push_lines(cell, col + i, m, color, &mut row.glyphs);
                }
                row.glyph_inks.resize(row.glyphs.len().max(before), tint);
                col = end;
            }
        } else {
            for (col, cell) in cells.iter().enumerate() {
                let (color, tint) = colors(cell).0.resolve();
                let before = row.glyphs.len();
                self.push_cell(text, source, col, m, color, &mut row.glyphs);
                push_lines(cell, col, m, color, &mut row.glyphs);
                row.glyph_inks.resize(row.glyphs.len().max(before), tint);
            }
        }
        row
    }

    /// 一格的字形图元：方块、制表线自己画，一个字查字形，簇整形
    #[inline]
    fn push_cell(&mut self, text: &mut TextSystem, row: &Row, col: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
        let cell = &row.cells[col];
        if cell.slot == Slot::SPACE || cell.slot == Slot::WIDE_TAIL {
            return;
        }
        let flags = cell.style.bold as u8 | (cell.style.italic as u8) << 1;
        if let Some(index) = cell.slot.cluster() {
            self.push_cluster(text, &row.clusters[index], flags, col, row.span_at(col), m, color, out);
        } else if let Some(ch) = cell.slot.char()
            && !drawn(ch, col as f32 * m.width, m, text.scale(), color, out)
        {
            self.push_glyph(text, ch, flags, col, row.span_at(col), m, color, out);
        }
    }

    /// 一个字的字形图元（行内坐标）：主字体里的字从格子左边开始，回退来的字（宽度和格子不一样）放在占的格子正中
    fn push_glyph(&mut self, text: &mut TextSystem, ch: char, flags: u8, col: usize, span: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
        let style = &self.style;
        let glyph = self.glyphs.entry(ch as u32 | (flags as u32) << 24).or_insert_with(|| text.cell_glyph(ch, &styled(style, flags)));
        let Some(glyph) = glyph else { return };
        let scale = text.scale();
        let room = span as f32 * m.width;
        let left = if (glyph.advance - m.width).abs() < 0.5 / scale { 0.0 } else { ((room - glyph.advance) * 0.5 * scale).round() / scale };
        let x = (col as f32 * m.width + left) * scale;
        if let Some(quad) = text.cell_quad(&glyph.face, glyph.glyph, self.style.size * scale, [x.round(), (m.baseline * scale).round()], color) {
            out.push(quad);
        }
    }

    /// 一个簇的字形图元：放在占的格子里
    fn push_cluster(&mut self, text: &mut TextSystem, cluster: &str, flags: u8, col: usize, span: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
        let shape = self.shape(text, cluster, flags);
        self.push_shape(text, &shape, &[(0, col as u32)], col + span, m, color, out);
    }

    /// 连字开着时同样式连续的 `start..end` 几格：拼成一段文字，整形（有缓存），字形放到各自的格子上
    fn push_run(&mut self, text: &mut TextSystem, row: &Row, start: usize, end: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
        let mut content = std::mem::take(&mut self.run_text);
        let mut cells = std::mem::take(&mut self.run_cells);
        content.clear();
        cells.clear();
        for col in start..end {
            let slot = row.cells[col].slot;
            if slot == Slot::WIDE_TAIL {
                continue;
            }
            cells.push((content.len() as u32, col as u32));
            match (slot.char(), slot.cluster()) {
                (Some(ch), _) => content.push(ch),
                (_, Some(index)) => content.push_str(&row.clusters[index]),
                _ => {}
            }
        }
        let style = row.cells[start].style;
        let shape = self.shape(text, &content, style.bold as u8 | (style.italic as u8) << 1);
        self.push_shape(text, &shape, &cells, end, m, color, out);
        self.run_text = content;
        self.run_cells = cells;
    }

    /// 整形（有缓存）
    fn shape(&mut self, text: &mut TextSystem, content: &str, flags: u8) -> Rc<CellShape> {
        if let Some(shape) = self.shapes[flags as usize].get(content) {
            return shape.clone();
        }
        if self.shape_glyphs > SHAPE_CACHE_GLYPHS {
            self.shapes = Default::default();
            self.shape_glyphs = 0;
        }
        let shape = Rc::new(text.cell_shape(content, &styled(&self.style, flags)));
        self.shape_glyphs += shape.glyph_count() + 1;
        self.shapes_built += 1;
        self.shapes[flags as usize].insert(content.into(), shape.clone());
        shape
    }

    /// 把整形结果放到格子上。`cells` 是文字里每一格的起点（字节）和列，`end` 是最后一格之后的列。
    /// 每组字形（一个簇或一个连字）放在它占的格子里：和格子一样宽时靠左，不一样宽时居中，比占的格子宽出很多时
    /// 缩小到放得下（字体里没有这个组合的 emoji，分开画成几个），字形的位置都取整到物理像素
    fn push_shape(&self, text: &mut TextSystem, shape: &CellShape, cells: &[(u32, u32)], end: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
        let scale = text.scale();
        let col_of = |byte: u32| cells[cells.partition_point(|c| c.0 <= byte).saturating_sub(1)].1 as usize;
        let end_of = |byte: u32| cells.get(cells.partition_point(|c| c.0 < byte)).map_or(end, |c| c.1 as usize);
        let units = &shape.units[..];
        let mut i = 0;
        while i < units.len() {
            let col = col_of(units[i].text.start);
            // parley 把一格的簇分成了几组（用了几个字体）：合起来放
            let mut next = i + 1;
            let (mut advance, mut last) = (units[i].advance, units[i].text.end);
            while next < units.len() && col_of(units[next].text.start) == col {
                advance += units[next].advance;
                last = last.max(units[next].text.end);
                next += 1;
            }
            let room = (end_of(last).max(col + 1) - col) as f32 * m.width;
            let fit = if advance > room * OVERFLOW { room / advance } else { 1.0 };
            let left = if (advance * fit - room).abs() < 0.5 / scale { 0.0 } else { ((room - advance * fit) * 0.5 * scale).round() / scale };
            let mut pen = col as f32 * m.width + left;
            let size = self.style.size * scale * fit;
            for unit in &units[i..next] {
                for glyph in shape.glyphs(unit) {
                    let position = [((pen + glyph.x * fit) * scale).round(), ((m.baseline + glyph.y) * scale).round()];
                    if let Some(quad) = text.cell_quad(shape.face(unit), glyph.id, size, position, color) {
                        out.push(quad);
                    }
                }
                pen += unit.advance * fit;
            }
            i = next;
        }
    }

    /// 读屏软件读到的文字：每行一段
    pub fn text(&self) -> String {
        let [_, rows] = self.grid.size();
        (0..rows).map(|r| self.grid.row_text(r)).collect::<Vec<_>>().join("\n")
    }
}

/// 一组字形比占的格子宽这么多倍时缩小：一点点的出入（彩色表情比两格略宽）不值得换一个字号
const OVERFLOW: f32 = 1.25;

/// 一格的下划线、删除线
#[inline]
fn push_lines(cell: &StoredCell, col: usize, m: &CellMetrics, color: Color, out: &mut Vec<Quad>) {
    let left = col as f32 * m.width;
    if cell.style.underline {
        out.push(Quad::new(left, m.baseline + m.underline_offset, m.width, m.underline_size).fill(color));
    }
    if cell.style.strikethrough {
        out.push(Quad::new(left, m.baseline - m.strikethrough_offset, m.width, m.strikethrough_size).fill(color));
    }
}

/// 这一格是 `flags` 的粗斜体时用的文字样式
fn styled(style: &TextStyle, flags: u8) -> TextStyle {
    let mut style = *style;
    if flags & 1 != 0 {
        style.weight = 700.0;
    }
    style.italic |= flags & 2 != 0;
    style
}

/// 连字开着时从第 `start` 格开始、一起整形的一串格子到哪一格为止：样式相同、不是空格、不是自己画的字。
/// 这一格本身不整形（空格、自己画的）时是 `start + 1`。遇到空格分开：没有字体的连字跨空格，分开之后同一个词能共用缓存
fn shaped_run_end(cells: &[StoredCell], start: usize) -> usize {
    let shaped = |cell: &StoredCell| cell.slot != Slot::SPACE && !cell.slot.char().is_some_and(is_drawn);
    if !shaped(&cells[start]) {
        return start + 1;
    }
    let style = &cells[start].style;
    let run = cells[start + 1..].iter().take_while(|cell| cell.style == *style && shaped(cell)).count();
    start + 1 + run
}

/// 自己画的字（[`drawn`] 里的那些）
fn is_drawn(ch: char) -> bool {
    matches!(ch as u32, 0x2580..=0x2595) || matches!(ch, '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼')
}

/// 图元落在 `left..right` 里的部分（行内坐标）：只有横向切开，字形的图集坐标按比例跟着切。完全在外面时是 `None`
fn clipped_x(quad: &Quad, left: f32, right: f32) -> Option<Quad> {
    let [x, _, width, _] = quad.rect;
    let (from, to) = (x.max(left), (x + width).min(right));
    if to <= from {
        return None;
    }
    let mut inside = *quad;
    inside.rect[0] = from;
    inside.rect[2] = to - from;
    if quad.kind != Quad::SHAPE {
        let [u0, _, u1, _] = quad.uv;
        inside.uv[0] = u0 + (u1 - u0) * (from - x) / width;
        inside.uv[2] = u0 + (u1 - u0) * (to - x) / width;
    }
    Some(inside)
}

/// 方块（U+2580 ~ U+2595）和细的制表线（─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼）自己画成矩形：字体里的这些字不一定铺满格子，
/// 连起来会有缝。画了返回 true。`x` 是格子左边（行内坐标），矩形对齐物理像素
fn drawn(ch: char, x: f32, m: &CellMetrics, scale: f32, color: Color, out: &mut Vec<Quad>) -> bool {
    let snap = |v: f32| (v * scale).round() / scale;
    let (w, h) = (m.width, m.height);
    let mut rect = |x0: f32, y0: f32, x1: f32, y1: f32, opacity: f32| {
        let (x0, y0, x1, y1) = (snap(x + x0), snap(y0), snap(x + x1), snap(y1));
        if x1 > x0 && y1 > y0 {
            out.push(Quad::new(x0, y0, x1 - x0, y1 - y0).fill(color).opacity(opacity));
        }
    };
    let code = ch as u32;
    match code {
        // ▀ 上半，▁ ~ ▇ 下面 1/8 ~ 7/8，█ 整格，▉ ~ ▏ 左边 7/8 ~ 1/8，▐ 右半
        0x2580 => rect(0.0, 0.0, w, h * 0.5, 1.0),
        0x2581..=0x2588 => rect(0.0, h * (1.0 - (code - 0x2580) as f32 / 8.0), w, h, 1.0),
        0x2589..=0x258f => rect(0.0, 0.0, w * (0x2590 - code) as f32 / 8.0, h, 1.0),
        0x2590 => rect(w * 0.5, 0.0, w, h, 1.0),
        // ░ ▒ ▓：整格，按浓淡减淡
        0x2591..=0x2593 => rect(0.0, 0.0, w, h, (code - 0x2590) as f32 * 0.25),
        // ▔ 上面 1/8，▕ 右边 1/8
        0x2594 => rect(0.0, 0.0, w, h / 8.0, 1.0),
        0x2595 => rect(w * 7.0 / 8.0, 0.0, w, h, 1.0),
        _ => {
            // 细线：从格子正中往左、右、上、下伸出去的几段
            let (left, right, up, down) = match ch {
                '─' => (true, true, false, false),
                '│' => (false, false, true, true),
                '┌' => (false, true, false, true),
                '┐' => (true, false, false, true),
                '└' => (false, true, true, false),
                '┘' => (true, false, true, false),
                '├' => (false, true, true, true),
                '┤' => (true, false, true, true),
                '┬' => (true, true, false, true),
                '┴' => (true, true, true, false),
                '┼' => (true, true, true, true),
                _ => return false,
            };
            let t = snap(1.0).max(1.0 / scale);
            let (cx, cy) = (snap(w * 0.5 - t * 0.5), snap(h * 0.5 - t * 0.5));
            if left || right {
                rect(if left { 0.0 } else { cx }, cy, if right { w } else { cx + t }, cy + t, 1.0);
            }
            if up || down {
                rect(cx, if up { 0.0 } else { cy }, cx + t, if down { h } else { cy + t }, 1.0);
            }
        }
    }
    true
}

/// 预编辑文字占几列（按字素簇算）
fn preedit_columns(text: &str) -> usize {
    let mut at = 0;
    graphemes(text)
        .skip(1)
        .map(|end| {
            let width = cluster_width(&text[at..end]);
            at = end;
            width
        })
        .sum()
}
