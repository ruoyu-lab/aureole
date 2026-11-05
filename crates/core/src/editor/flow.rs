//! 很长的一段（上万字不换行）的分块排版。
//!
//! parley 没有增量整形，一段只能整个重排，一段一万字时打一个字要约 2 ms。这里把长段的文字切成一块块（[`Block`]），
//! 每块一份 parley 排版，打一个字只重新整形附近的几块（一般一块；靠近块边，或者字很窄、一行比一块还长时多几块），和段长无关。选择和比较见决策记录 0062。
//!
//! 结果和整段排版逐行、逐字形相同，靠这三条（`测试：chunked_paragraphs_match_whole_ones`）：
//!
//! - **带余量整形**：一块的排版比它自己的文字前后多带一些（[`SLACK`]、一行的长度）。字距、连字、脚本继承、字体回退、
//!   断行和分词规则都只看附近的字，所以切口的影响到不了余量里面；切口只放在简单的字前面（[`can_cut`]），
//!   切不开组合的表情、附加符号和连字，脚本也从这里重新算起。
//! - **一行归行首所在的块**：一行整个排在一份排版里，行内的累加和整段排版是同样的次序，插入点、点击、选区都直接问这份排版。
//!   排版里自己的行前后还有几行别的（前面的余量排成一行，后面接着断出来的），不画也不问，只是让行首行尾的
//!   "前一个字""后一个字"有东西可看。断行从上一块算出的行首接着断，parley 是贪心算法，只看这一行从哪里开始。
//! - **改动后往后波及到对上为止**：一块重新断行后，下一行的行首和上次一样时后面的块都不动；不一样时下一块照新的行首重新断行
//!   （不用重新整形，比整形快一个数量级）。中文每行字数一样，打一个字一直波及到段尾，也只是把后面的块断一遍。
//!   行数变了时后面的块也要重新断行（行的纵坐标按全段第几行算，和整段排版的取整一样）。
//!
//! 波及到段尾的那一遍和段长成正比，所以打字时只排到看得见的地方（[`Near`]），后面的等停下来（插入点第一次换亮灭）
//! 或者要做别的事（移动插入点、点击、辅助技术来读）时才排完。这期间看得见的行、插入点都是准的，只有总高是估的。
//!
//! 只是显示、没人改的长段不分块（第一次编辑时才切）：整段排版一次，首帧和以前一样。
//! 判断不了的（有从右到左的字、段里有强制换行符、找不到能切的地方）也不分块，走整段排版。

use std::ops::Range;

use parley::{Alignment, Affinity, AlignmentOptions, BoundingBox, BreakReason, Cluster, Cursor, Layout, LineMetrics, Selection as Span};
use scene::Quad;

use crate::geometry::Affine;
use crate::segmentation;
use crate::text::{GlyphQuads, Ink, SpanId, TextStyle, TextSystem};

/// 切口前后要多带的字节数：切口的影响（字距、连字、上下文替换、断行和分词规则）只到这么远。
/// 约 20 个汉字或 64 个英文字母；字体的上下文规则实际只看几个字形
const SLACK: usize = 64;

/// 什么长度的段分块、一块多大
#[derive(Clone, Copy, Debug)]
pub(super) struct Tuning {
    /// 段长（字节）到这么多才分块：比这短的段整段排版更省（打一个字在 300 µs 以内，一帧有 8 ms）
    pub enter: usize,
    /// 分块的段短到这么多退回整段排版（比 `enter` 小，免得在边上来回换）
    pub leave: usize,
    /// 一块自己的文字大约多少个字（一行很长时更多）。打一个字的耗时和它成正比；块越小，互相重叠的余量占比越大，
    /// 第一次编辑时整形全部块越慢
    pub core: usize,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { enter: 4096, leave: 2048, core: 300 }
    }
}

/// 只排到这里（打字时）：行顶过了 `bottom`（相对段顶）、块首过了 `offset`（段内偏移，插入点）的块先不排
#[derive(Clone, Copy, Debug)]
pub(super) struct Near {
    pub bottom: f32,
    pub offset: usize,
}

/// 有这些字的段不分块：从右到左的字影响整段的方向和行内的重排，强制换行符（段里一般没有）要另外断行
pub(super) fn needs_whole(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF              // 希伯来文、阿拉伯文、叙利亚文等
        | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFC // 希伯来文、阿拉伯文的表现形式
        | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF // 其他从右到左的文字
        | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069 // 方向控制符
        | 0x000B | 0x000C | 0x000D | 0x0085 | 0x2028 | 0x2029 // 强制换行
    )
}

/// 有自己脚本的简单的字（不是附加符号、表情、标点、数字）：不会和前面的字连成一个簇，脚本也不用从前面继承
fn plain(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c as u32,
            0x00C0..=0x00D6 | 0x00D8..=0x00F6 | 0x00F8..=0x024F // 带符号的拉丁字母
            | 0x0391..=0x03A9 | 0x03B1..=0x03C9                 // 希腊字母
            | 0x0410..=0x044F                                   // 西里尔字母
            | 0x3041..=0x3096 | 0x30A1..=0x30FA                 // 平假名、片假名（不含浊音符、长音符）
            | 0x3400..=0x4DBF | 0x4E00..=0x9FFF                 // 汉字
            | 0xAC00..=0xD7A3                                   // 谚文音节
        )
}

/// `c` 前面能不能切：`c` 是简单的字，前面是空格、标点或者中日韩的字，不会和它连起来。
/// 连接符、变体选择符、前置符号不算；字母后面也不切，可能正好是连字的中间
fn can_cut(prev: char, c: char) -> bool {
    plain(c)
        && ((prev.is_ascii() && !prev.is_ascii_alphanumeric())
            || matches!(prev as u32, 0x3000..=0x303F | 0x3041..=0x3096 | 0x30A1..=0x30FA | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7A3 | 0xFF00..=0xFFEF))
}

fn floor(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `from` 处或之后第一个能切的地方（不含段首）
fn cut_after(text: &str, from: usize) -> Option<usize> {
    let at = floor(text, from.max(1));
    let at = if at == 0 { text.chars().next()?.len_utf8() } else { at };
    let mut prev = text[..at].chars().next_back()?;
    for (i, c) in text[at..].char_indices() {
        if can_cut(prev, c) {
            return Some(at + i);
        }
        prev = c;
    }
    None
}

/// `from` 处或之前最后一个能切的地方，没有时是段首
fn cut_before(text: &str, from: usize) -> usize {
    let mut chars = text[..floor(text, from)].char_indices().rev().peekable();
    while let Some((i, c)) = chars.next() {
        if chars.peek().is_some_and(|&(_, prev)| can_cut(prev, c)) {
            return i;
        }
    }
    0
}

/// 这个位置还能不能当块的起点
fn is_cut(text: &str, at: usize) -> bool {
    let (Some(prev), Some(c)) = (text[..at].chars().next_back(), text[at..].chars().next()) else { return false };
    can_cut(prev, c)
}

/// 从 `from` 往后数 `chars` 个字的地方
fn after_chars(text: &str, from: usize, chars: usize) -> usize {
    text[from..].char_indices().nth(chars).map_or(text.len(), |(i, _)| from + i)
}

/// 行表里的一行
#[derive(Clone, Copy, Debug, PartialEq)]
struct FlowLine {
    /// 行首在段里的字节偏移
    start: u32,
    /// 行顶、行底（相对段顶），算法和 parley 一样
    top: f32,
    bottom: f32,
}

/// 一块的 parley 排版和它自己这几行的字形图元（段顶为 0）
#[derive(Default)]
struct Window {
    layout: Layout<SpanId>,
    glyphs: GlyphQuads,
}

struct Block {
    /// 块首在段里的字节偏移；块尾是下一块的块首
    core: usize,
    /// 块里（或者离块首、自己的行看到的地方很近）的文字改了，还没重新整形
    stale: bool,
    /// 按现在的 `first`、`starts` 和宽度断好了行
    broken: bool,
    /// 排版在块首之前多带了几个字节、几个字
    lead: u32,
    lead_chars: u32,
    /// 排版在块尾之后多带了几个字节
    tail: u32,
    /// 块尾之后还有几个字节和段里的文字一样（后面的字改了时变小，排版不用马上重排）
    tail_valid: u32,
    /// 排版里自己的行前面有几行（余量排成的一行，或者没有）
    head: u32,
    /// 自己的第一行是全段的第几行
    first: u32,
    /// 自己每一行的行首，相对块首。一行归行首所在的块，一块可以没有自己的行（前一块的一行盖过了它）
    starts: Vec<u32>,
    /// 下一行的行首（不归自己了），相对块首；到段尾时是段长
    next: u32,
    /// 自己最后一行断行时看到了哪里（让它超出宽度的那个字的末尾），相对块首：这之后的字变了，自己的行不变
    reach: u32,
    window: Box<Window>,
}

impl Block {
    fn new(core: usize) -> Self {
        Self {
            core,
            stale: true,
            broken: false,
            lead: 0,
            lead_chars: 0,
            tail: 0,
            tail_valid: 0,
            head: 0,
            first: 0,
            starts: Vec::new(),
            next: 0,
            reach: 0,
            window: Box::default(),
        }
    }

    /// 排版的第 0 个字节在段里的位置。过期的块（打字时还没排到）的余量是旧的，可能比块首还大，这时算段首：
    /// 这样的块只有行表还在用，说不准的位置等排完才问
    fn base(&self) -> usize {
        debug_assert!(self.stale || self.core >= self.lead as usize, "没过期的块带的余量都还在段里");
        self.core.saturating_sub(self.lead as usize)
    }
}

/// 放着某一行的那份排版：整段排版的段是段自己的，分块的段是这一行所属的块
#[derive(Clone, Copy)]
pub(super) struct Piece<'a> {
    pub layout: &'a Layout<SpanId>,
    /// 排版的第 0 个字节在段里的位置
    pub base: usize,
}

/// 一段的分块排版
pub(super) struct Flow {
    blocks: Vec<Block>,
    /// 全段的行（各块的行连起来）：按行号、纵坐标找行用
    lines: Vec<FlowLine>,
    wrap: f32,
    alignment: Alignment,
    /// 每行的行高，基线在行顶下面多少、行底在基线下面多少（都一样：行高按字号的倍数算时只看整段的样式，和这一行用了哪些字体无关）
    line_height: f32,
    over: f32,
    under: f32,
    /// 排版在块尾之后带多少字节（一行多一点）
    tail_guess: usize,
    tuning: Tuning,
    /// 上次画了哪几块
    painted: Range<usize>,
    /// 有改动，下次 `refresh` 要处理
    dirty: bool,
    /// 文字改过：行表里的行首要挪
    moved: bool,
}

impl Flow {
    /// 把一段切成块排好。这一段不值得或者不能分块时返回 `None`，走整段排版
    pub fn build(text: &mut TextSystem, content: &str, style: &TextStyle, wrap: f32, tuning: Tuning, shaped: &mut usize) -> Option<Box<Self>> {
        if content.len() < tuning.enter || content.chars().any(needs_whole) {
            return None;
        }
        // 一行很长（很宽的输入框）时块也要大：至少放得下两行
        let tuning = Tuning { core: tuning.core.max((2.0 * wrap / style.size.max(1.0)) as usize), ..tuning };
        let mut blocks = vec![Block::new(0)];
        let mut at = after_chars(content, 0, tuning.core);
        while at < content.len() {
            let Some(cut) = cut_after(content, at) else { break };
            blocks.push(Block::new(cut));
            at = after_chars(content, cut, tuning.core);
        }
        // 最后一块太短时和倒数第二块并起来
        if blocks.len() > 1 && content.len() - blocks[blocks.len() - 1].core < tuning.core / 2 {
            blocks.pop();
        }
        if blocks.len() < 2 {
            return None;
        }
        let mut flow = Box::new(Self {
            blocks,
            lines: Vec::new(),
            wrap,
            alignment: style.alignment(),
            line_height: 0.0,
            over: 0.0,
            under: 0.0,
            tail_guess: 0,
            tuning,
            painted: 0..0,
            dirty: true,
            moved: false,
        });
        flow.refresh(text, content, style, wrap, None, shaped);
        Some(flow)
    }

    /// 把 `range`（改之前的段内偏移）换成了 `inserted` 个字节。文字已经换好了，这里只记下哪些块受影响
    pub fn edit(&mut self, range: Range<usize>, inserted: usize) {
        let (start, end) = (range.start, range.end);
        let delta = inserted as isize - (end - start) as isize;
        let new_end = start + inserted;
        // 删掉的范围跨过的块并进第一块
        let first = self.block_of(start);
        let last = if end > start { self.block_of(end - 1).max(first) } else { first };
        self.blocks.drain(first + 1..=last);
        // 后面的块：离块首很近的改动影响块首的字（字距、断行规则）；改到排版带的余量时也重新整形，
        // 不然余量里已经不是段里的字，删得多时块首还会比余量小
        for block in &mut self.blocks[first + 1..] {
            block.core = block.core.wrapping_add_signed(delta);
            block.stale |= block.core < new_end + SLACK.max(block.lead as usize);
        }
        self.blocks[first].stale = true;
        // 前面的块：改动碰到自己最后一行看到的地方才重新整形；只落在余量里的记下来，以后这一块的行走到附近时才重排
        for i in (0..first).rev() {
            let core_end = self.blocks[i + 1].core;
            let block = &mut self.blocks[i];
            if start < (block.core + block.reach as usize + SLACK).max(core_end + SLACK) {
                block.stale = true;
            } else if start - core_end < block.tail_valid as usize {
                block.tail_valid = (start - core_end) as u32;
            }
        }
        self.dirty = true;
        self.moved = true;
    }

    /// 还有没排好的块（打字时只排到看得见的地方，见 [`Near`]）
    pub fn is_settled(&self) -> bool {
        !self.dirty
    }

    /// 重新整形改过的块、从改过的地方往后重新断行。不再分块时返回 `false`。
    /// `near` 时只排到看得见的地方：后面的块还是上次的行，行表里也是，总高是估的，下次不带 `near` 时才排完
    pub fn refresh(&mut self, text: &mut TextSystem, content: &str, style: &TextStyle, wrap: f32, near: Option<Near>, shaped: &mut usize) -> bool {
        if content.len() < self.tuning.leave {
            return false;
        }
        if self.wrap != wrap || self.tail_guess == 0 {
            self.wrap = wrap;
            // 一行的字节数：一个汉字三个字节占一个字号宽，英文一个字节占约半个字号宽，都差不多是每字号宽三个字节以内
            self.tail_guess = (wrap / style.size.max(1.0) * 3.3) as usize + SLACK;
            self.blocks.iter_mut().for_each(|b| b.broken = false);
            self.dirty = true;
        }
        if !std::mem::take(&mut self.dirty) {
            return true;
        }
        self.normalize(content);
        let (mut at, mut first) = (0usize, 0usize);
        let mut changed = None;
        for j in 0..self.blocks.len() {
            let (core, end) = (self.blocks[j].core, self.core_end(j, content.len()));
            let block = &mut self.blocks[j];
            // 前一块的一行盖过了这一块：这一块没有自己的行
            if at >= end {
                block.starts.clear();
                block.first = first as u32;
                block.broken = false;
                continue;
            }
            let start = (at - core) as u32;
            let unchanged = !block.stale && block.broken && block.first == first as u32 && block.starts.first() == Some(&start);
            if !unchanged {
                // 这一块已经在看得见的范围后面了：先不排，后面的也不排
                if near.is_some_and(|n| first as f32 * self.line_height > n.bottom && core > n.offset) {
                    self.dirty = true;
                    break;
                }
                changed.get_or_insert((j, first));
                self.lay_out(j, start, first, text, content, style, shaped);
            }
            let block = &self.blocks[j];
            at = core + block.next as usize;
            first += block.starts.len();
        }
        // 行表从第一个变了的块重新连起来（后面的块就算没变，起点也挪了）
        let moved = std::mem::take(&mut self.moved).then_some((0, 0));
        let Some((j, line)) = changed.or(moved) else { return true };
        self.lines.truncate(line);
        let (height, over, under) = (self.line_height, self.over, self.under);
        for block in &self.blocks[j..] {
            for &start in &block.starts {
                // 和 parley 算行顶、行底的次序一样：行顶按累加的高度取整，行底按基线加下面的部分取整
                let top = (self.lines.len() as f64 * height as f64).round() as f32;
                let bottom = (top + over + under).round();
                self.lines.push(FlowLine { start: block.core as u32 + start, top, bottom });
            }
        }
        true
    }

    /// 改过的块重新分：块首不能切了的并进前一块，太小的并进后一块，太大的切开。
    /// 切开的两半都不会太小（能切的地方太靠后就不切），块首也都能切，所以切开的不会再并回去，这个循环走得完：
    /// 每次并掉一个原来的块，两次并之间切口只往后走
    fn normalize(&mut self, content: &str) {
        let mut j = 0;
        while j < self.blocks.len() {
            if !self.blocks[j].stale {
                j += 1;
                continue;
            }
            let core = self.blocks[j].core;
            if j > 0 && !is_cut(content, core) {
                self.blocks[j - 1].stale = true;
                self.blocks.remove(j);
                j -= 1;
                continue;
            }
            let end = self.core_end(j, content.len());
            let too_small = self.tuning.core / 4;
            if end - core < too_small && self.blocks.len() > 1 {
                if j + 1 < self.blocks.len() {
                    self.blocks.remove(j + 1);
                    continue;
                }
                self.blocks[j - 1].stale = true;
                self.blocks.remove(j);
                j -= 1;
                continue;
            }
            if end - core > self.tuning.core * 2
                && content[core..end].chars().count() > self.tuning.core * 2
                && let Some(cut) = cut_after(content, after_chars(content, core, self.tuning.core))
                && cut < end
                && end - cut >= too_small
            {
                self.blocks.insert(j + 1, Block::new(cut));
            }
            j += 1;
        }
    }

    fn core_end(&self, j: usize, len: usize) -> usize {
        self.blocks.get(j + 1).map_or(len, |b| b.core)
    }

    /// 排第 `j` 块：改过时重新整形，然后从 `start`（相对块首）起断行。块尾之后带的文字不够自己的最后一行看时带多一点重来
    fn lay_out(&mut self, j: usize, start: u32, first: usize, text: &mut TextSystem, content: &str, style: &TextStyle, shaped: &mut usize) {
        let mut need = 0;
        loop {
            if self.blocks[j].stale {
                self.shape(j, text, content, style, need, shaped);
            }
            self.break_lines(j, start, first, content);
            match self.measure_reach(j, content) {
                Ok(reach) => {
                    self.blocks[j].reach = reach;
                    break;
                }
                Err(at) => {
                    need = at + SLACK * 2 + self.tail_guess;
                    self.blocks[j].stale = true;
                }
            }
        }
    }

    /// 整形第 `j` 块，排版在块尾之后至少带到段里的 `need`
    fn shape(&mut self, j: usize, text: &mut TextSystem, content: &str, style: &TextStyle, need: usize, shaped: &mut usize) {
        let core = self.blocks[j].core;
        let end = self.core_end(j, content.len());
        let start = if core == 0 { 0 } else { cut_before(content, core.saturating_sub(SLACK)) };
        let wanted = (end + self.tail_guess).max(need);
        let stop = if wanted >= content.len() { content.len() } else { cut_after(content, wanted).unwrap_or(content.len()) };
        let block = &mut self.blocks[j];
        block.lead = (core - start) as u32;
        block.lead_chars = content[start..core].chars().count() as u32;
        block.tail = (stop - end) as u32;
        block.tail_valid = block.tail;
        block.stale = false;
        block.broken = false;
        text.build_into(&mut block.window.layout, &content[start..stop], style);
        *shaped += 1;
        if self.line_height == 0.0 {
            let layout = &mut self.blocks[j].window.layout;
            layout.break_all_lines(None);
            if let Some(line) = layout.get(0) {
                let m = line.metrics();
                self.line_height = m.line_height;
                self.over = m.baseline - m.block_min_coord;
                self.under = m.line_height - self.over;
            }
        }
    }

    /// 让 parley 从 `start`（相对块首）起断行。块首到这里的字（和前面的余量）排成一行放在第一行上面（行的纵坐标要有序，
    /// parley 才找得到行），后面的一直断到排版的末尾，读出归自己的行
    fn break_lines(&mut self, j: usize, start: u32, first: usize, content: &str) {
        let (wrap, alignment, height) = (self.wrap, self.alignment, self.line_height as f64);
        let end = self.core_end(j, content.len());
        let block = &mut self.blocks[j];
        let head_chars = block.lead_chars as usize + content[block.core..block.core + start as usize].chars().count();
        let y = first as f64 * height;
        let mut breaker = block.window.layout.break_lines();
        if head_chars > 0 {
            breaker.state_mut().set_line_y(y - height);
            breaker.break_next_with_length(head_chars as u32);
        }
        breaker.state_mut().set_line_y(y);
        breaker.break_remaining(wrap);
        let window = &mut *block.window;
        window.layout.align(alignment, AlignmentOptions::default());
        window.glyphs.clear();
        block.head = (head_chars > 0) as u32;
        block.first = first as u32;
        block.broken = true;
        block.starts.clear();
        let lead = block.lead as usize;
        let end_local = end - block.core + lead;
        let (starts, mut next) = (&mut block.starts, content.len() - block.core);
        for line in window.layout.lines().skip(block.head as usize) {
            let at = line.text_range().start;
            if at >= end_local {
                next = at - lead;
                break;
            }
            starts.push((at - lead) as u32);
        }
        block.next = next as u32;
    }

    /// 自己的最后一行断行时看到了哪里（相对块首）。块尾之后带的文字够它看（再加上余量）时是 `Ok`；
    /// 不够时是 `Err`（看到了段里的哪里）
    fn measure_reach(&self, j: usize, content: &str) -> Result<u32, usize> {
        let block = &self.blocks[j];
        let layout = &block.window.layout;
        let end = self.core_end(j, content.len());
        let reaches_end = end + block.tail as usize == content.len() && block.tail_valid == block.tail;
        let Some(line) = block.starts.len().checked_sub(1).and_then(|last| layout.get(block.head as usize + last)) else { return Ok(0) };
        let base = block.base();
        let range = line.text_range();
        let mut x = line.metrics().advance;
        let mut cluster = if line.break_reason() == BreakReason::None { None } else { Cluster::from_byte_index(layout, range.end) };
        let mut reach = range.end;
        let mut overflowed = false;
        while let Some(c) = cluster {
            let r = c.text_range();
            // 再往后的文字改过了，排版里的已经不是段里的：不够看
            if base + r.end > end + block.tail_valid as usize {
                break;
            }
            x += c.advance();
            reach = r.end;
            if x > self.wrap && !content[base + r.start..base + r.end].chars().all(|c| matches!(c, ' ' | '\t' | '\u{3000}')) {
                overflowed = true;
                break;
            }
            cluster = c.next_logical();
        }
        let at = base + reach;
        if overflowed && at + SLACK <= end + block.tail_valid as usize {
            Ok((at - block.core) as u32)
        } else if reaches_end {
            // 排版一直带到了段尾：不用切，段尾之前看到哪里都准
            Ok(((if overflowed { at } else { content.len() }) - block.core) as u32)
        } else {
            Err(at)
        }
    }

    pub fn height(&self) -> f32 {
        (self.lines.len() as f64 * self.line_height as f64) as f32
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn block_of(&self, offset: usize) -> usize {
        self.blocks.partition_point(|b| b.core <= offset).max(1) - 1
    }

    /// 放着全段第 `k` 行的排版
    pub fn piece_of_line(&self, k: usize) -> Piece<'_> {
        let block = &self.blocks[self.block_of(self.lines[k.min(self.lines.len() - 1)].start as usize)];
        Piece { layout: &block.window.layout, base: block.base() }
    }

    /// 放着这个位置（段内偏移）的排版
    pub fn piece_at(&self, local: usize, affinity: Affinity) -> Piece<'_> {
        self.piece_of_line(self.line_of(local, affinity))
    }

    /// 这个位置在第几行：自动折行处贴着前面的字时算上一行（落在行首那个簇的中间时也是，parley 会把它挪到簇首）
    pub fn line_of(&self, local: usize, affinity: Affinity) -> usize {
        let k = self.lines.partition_point(|l| l.start as usize <= local).max(1) - 1;
        if affinity != Affinity::Upstream || k == 0 {
            return k;
        }
        let start = self.lines[k].start as usize;
        let piece = self.piece_of_line(k);
        let at_start = local == start || Cluster::from_byte_index(piece.layout, local - piece.base).is_some_and(|c| c.text_range().start + piece.base == start);
        if at_start { k - 1 } else { k }
    }

    /// 纵坐标（相对段顶）落在第几行：和 parley 同样的查找，行之间差一个像素的重叠、缝隙也判到同一行
    pub fn line_at(&self, y: f32) -> usize {
        if y < 0.0 {
            return 0;
        }
        let found = self.lines.binary_search_by(|line| {
            if y < line.top {
                std::cmp::Ordering::Greater
            } else if y >= line.bottom {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        match found {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
        .min(self.lines.len() - 1)
    }

    /// 全段第 `k` 行的纵向中点：在排版里问这一行时用它，不会落到相邻的行上
    pub fn line_center(&self, k: usize) -> f32 {
        let line = self.lines[k.min(self.lines.len() - 1)];
        (line.top + line.bottom) * 0.5
    }

    /// 这个纵坐标（相对段顶）在第几行：第一个行底在它下面的行（和整段排版找插入点所在的行一样）
    pub fn line_below(&self, y: f32) -> usize {
        self.lines.partition_point(|l| l.bottom <= y).min(self.lines.len() - 1)
    }

    // ---- 跨块的按词走：parley 的同名方法走到排版的头尾就停了，这里逐簇问，每一步都问放着它的那一块 ----

    fn grapheme_at(&self, at: usize, len: usize) -> Option<Grapheme> {
        if at >= len {
            return None;
        }
        let piece = self.piece_at(at, Affinity::Downstream);
        let cluster = Cluster::from_byte_index(piece.layout, at - piece.base)?;
        let range = cluster.text_range();
        Some(Grapheme { start: range.start + piece.base, end: range.end + piece.base, boundary: cluster.is_word_boundary() })
    }

    fn next_boundary(&self, mut from: Grapheme, len: usize) -> Option<Grapheme> {
        loop {
            let next = self.grapheme_at(from.end, len)?;
            if next.boundary {
                return Some(next);
            }
            from = next;
        }
    }

    fn previous_boundary(&self, mut from: Grapheme, len: usize) -> Option<Grapheme> {
        loop {
            let previous = self.grapheme_at(from.start.checked_sub(1)?, len)?;
            if previous.boundary {
                return Some(previous);
            }
            from = previous;
        }
    }

    /// `Cursor::next_logical_word`：往后到下一个词的开头，跳过空白
    pub fn next_logical_word(&self, content: &str, at: usize) -> usize {
        let len = content.len();
        let Some(mut now) = self.grapheme_at(at, len).or_else(|| at.checked_sub(1).and_then(|i| self.grapheme_at(i, len))) else { return at };
        loop {
            let Some(next) = self.next_boundary(now, len) else { return len };
            now = next;
            if !is_space(content, now.start) {
                return now.start;
            }
        }
    }

    /// `Cursor::previous_logical_word`：往前到词的开头，跳过空白
    pub fn previous_logical_word(&self, content: &str, at: usize) -> usize {
        let len = content.len();
        let Some(mut now) = self.grapheme_at(at, len).or_else(|| at.checked_sub(1).and_then(|i| self.grapheme_at(i, len))) else { return at };
        while let Some(previous) = self.previous_boundary(now, len) {
            now = previous;
            if !is_space(content, now.start) {
                break;
            }
        }
        now.start
    }

    /// 和 `motion::right` 一样，位置是段内偏移和贴哪边（按词走可能走出 `piece` 这份排版）
    pub fn right(&self, content: &str, piece: Piece<'_>, from: Cursor, word: bool) -> (usize, Affinity) {
        let at = piece.base + from.index();
        if !word {
            let next = from.next_visual(piece.layout);
            return (piece.base + next.index(), next.affinity());
        }
        if let Some(end) = segmentation::next_word_end(content, at) {
            return self.snapped(end, Affinity::Upstream);
        }
        // `Cursor::next_visual_word`：分块的段里都是从左到右的字
        self.visual_word(at, from.affinity(), |cursor, layout| cursor.next_visual(layout), |left, right| right.is_word_boundary() && !left.is_space_or_nbsp())
    }

    /// 和 `motion::left` 一样，位置是段内偏移和贴哪边
    pub fn left(&self, content: &str, piece: Piece<'_>, from: Cursor, word: bool) -> (usize, Affinity) {
        let at = piece.base + from.index();
        if !word {
            let next = from.previous_visual(piece.layout);
            return (piece.base + next.index(), next.affinity());
        }
        if let Some(start) = segmentation::previous_word_start(content, at) {
            return self.snapped(start, Affinity::Downstream);
        }
        // `Cursor::previous_visual_word`
        self.visual_word(at, from.affinity(), |cursor, layout| cursor.previous_visual(layout), |_, right| right.is_word_boundary() && !right.is_space_or_nbsp())
    }

    /// 这个位置挪到字素簇的开头（`Cursor::from_byte_index`）
    fn snapped(&self, at: usize, affinity: Affinity) -> (usize, Affinity) {
        let piece = self.piece_at(at, affinity);
        let cursor = Cursor::from_byte_index(piece.layout, at - piece.base, affinity);
        (piece.base + cursor.index(), cursor.affinity())
    }

    fn visual_word(
        &self,
        at: usize,
        affinity: Affinity,
        step: impl Fn(Cursor, &Layout<SpanId>) -> Cursor,
        stop: impl Fn(&Cluster<'_, SpanId>, &Cluster<'_, SpanId>) -> bool,
    ) -> (usize, Affinity) {
        let mut now = (at, affinity);
        loop {
            let piece = self.piece_at(now.0, now.1);
            let cursor = Cursor::from_byte_index(piece.layout, now.0 - piece.base, now.1);
            let next = step(cursor, piece.layout);
            if next == cursor {
                return now;
            }
            now = (next.index() + piece.base, next.affinity());
            let piece = self.piece_at(now.0, now.1);
            let cursor = Cursor::from_byte_index(piece.layout, now.0 - piece.base, now.1);
            let [Some(left), Some(right)] = cursor.visual_clusters(piece.layout) else { return now };
            if stop(&left, &right) {
                return now;
            }
        }
    }

    /// 这一点（相对段顶）所在的词：`Selection::word_from_point`，中日文交给词典
    pub fn word_at(&self, content: &str, x: f32, y: f32) -> Range<usize> {
        let k = self.line_at(y);
        let piece = self.piece_of_line(k);
        let len = content.len();
        let Some((cluster, _)) = Cluster::from_point(piece.layout, x, self.line_center(k)) else { return len..len };
        let range = cluster.text_range();
        let now = Grapheme { start: range.start + piece.base, end: range.end + piece.base, boundary: cluster.is_word_boundary() };
        if let Some(word) = segmentation::word_at(content, now.start) {
            return word;
        }
        let now = if now.boundary { now } else { self.previous_boundary(now, len).unwrap_or(now) };
        let end = self.next_boundary(now, len).map_or(len, |next| next.start);
        now.start..end
    }

    // ---- 绘制 ----

    /// 生成和 `view`（相对段顶）相交的块的字形图元：只有自己的行，别的和整段排版的一样
    pub fn paint_glyphs(
        &mut self,
        text: &mut TextSystem,
        view: Option<[f32; 2]>,
        origin: [f32; 2],
        shift: f32,
        world: &Affine,
        ink: Ink,
        place: [u32; 2],
        quads: &mut Vec<Quad>,
    ) {
        let lines = match view {
            Some([y0, y1]) => {
                let first = self.lines.partition_point(|l| l.bottom < y0);
                first..self.lines.partition_point(|l| l.top <= y1).max(first)
            }
            None => 0..self.lines.len(),
        };
        let blocks = if lines.is_empty() {
            0..0
        } else {
            self.block_of(self.lines[lines.start].start as usize)..self.block_of(self.lines[lines.end - 1].start as usize) + 1
        };
        let painted = std::mem::replace(&mut self.painted, blocks.clone());
        for j in painted.start.min(self.blocks.len())..painted.end.min(self.blocks.len()) {
            if !blocks.contains(&j) {
                self.blocks[j].window.glyphs.release();
            }
        }
        for block in &mut self.blocks[blocks] {
            if !block.starts.is_empty() {
                let (head, count) = (block.head as usize, block.starts.len());
                let window = &mut *block.window;
                text.push_glyphs_cached(&mut window.glyphs, &window.layout, head..head + count, origin, world, ink, &[], shift, place, quads);
            }
        }
    }

    /// 不再画了：还掉所有块的字形图元
    pub fn release_glyphs(&mut self) {
        let painted = std::mem::replace(&mut self.painted, 0..0);
        for j in painted.start.min(self.blocks.len())..painted.end.min(self.blocks.len()) {
            self.blocks[j].window.glyphs.release();
        }
    }

    /// `a..b`（段内偏移，`a < b`）的矩形（相对段顶），和 parley 的 `Selection::geometry_with` 一样：头尾两行逐簇算，
    /// 中间的行直接用整行。只算和 `view`（相对段顶）相交的行
    pub fn range_rects(&self, a: usize, b: usize, view: Option<[f32; 2]>, f: &mut impl FnMut(BoundingBox)) {
        let (first, last) = (self.line_of(a, Affinity::Downstream), self.line_of(b, Affinity::Upstream));
        for k in first..=last {
            let line = self.lines[k];
            if view.is_some_and(|[y0, y1]| line.bottom < y0 || line.top > y1) {
                continue;
            }
            let piece = self.piece_of_line(k);
            if k == first || k == last {
                let from = if k == first { a } else { line.start as usize };
                let to = if k == last { b } else { self.lines[k + 1].start as usize };
                let span = Span::new(
                    Cursor::from_byte_index(piece.layout, from - piece.base, Affinity::Downstream),
                    Cursor::from_byte_index(piece.layout, to - piece.base, Affinity::Upstream),
                );
                span.geometry_with(piece.layout, |rect, _| f(rect));
            } else {
                self.with_line(k, |m, _| {
                    let x = m.offset as f64 + m.inline_min_coord as f64;
                    let y0 = f32::min(m.block_min_coord, m.content_block_min_coord) as f64;
                    let y1 = f32::max(m.block_max_coord, m.content_block_max_coord) as f64;
                    f(BoundingBox::new(x, y0, x + m.advance as f64, y1));
                });
            }
        }
    }

    /// 全段第 `k` 行的度量和文字范围（段内偏移）
    pub fn with_line<R>(&self, k: usize, f: impl FnOnce(&LineMetrics, Range<usize>) -> R) -> Option<R> {
        let (block, line) = self.layout_line(k)?;
        let (range, base) = (line.text_range(), block.base());
        Some(f(line.metrics(), range.start + base..range.end + base))
    }

    /// 全段第 `k` 行和它在排版里的那一行（按行首在块里的位置找：打字时后面的块还没排，行号还是上次的）
    fn layout_line(&self, k: usize) -> Option<(&Block, parley::Line<'_, SpanId>)> {
        let start = self.lines[k].start as usize;
        let block = &self.blocks[self.block_of(start)];
        let index = block.starts.partition_point(|&s| block.core + (s as usize) < start);
        Some((block, block.window.layout.get(index + block.head as usize)?))
    }

}

/// 一个字素簇的范围和它前面是不是词边界
#[derive(Clone, Copy)]
struct Grapheme {
    start: usize,
    end: usize,
    boundary: bool,
}

fn is_space(content: &str, at: usize) -> bool {
    content[at..].chars().next().is_some_and(char::is_whitespace)
}
