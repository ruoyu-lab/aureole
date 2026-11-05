//! 选择根（[`crate::Element::selection_root`]）：里面的文字按树的顺序连成一串，拖动可以从一段选到另一段，
//! Shift+点击扩展、⌘A 选中里面的全部、⌘C 复制连起来的文字（段与段之间换行，并排的用制表符隔开）。
//!
//! 选区记成两端（文字元素、显示出来的文字里的字节偏移）。每段文字照旧存自己那一截的 parley 选区：
//! 第一段从起点到段尾，中间的整段，最后一段从段首到终点，所以画选区、没焦点时的颜色都和单段文字一样。
//! 焦点在按下的那段上；选择根里的文字有焦点时，这个选择根的选区都画成有焦点的颜色。
//!
//! 成员：子孙里的文字（`text`、`rich_text`），不用逐个写 `.selectable(true)`；可点击、能获得焦点的元素
//! （按钮这类）里面的字、输入框、嵌套的另一个选择根不算。成员表按需重算（树的结构变了之后第一次用到时）。
//! 虚拟列表滚出去的行卸载了：不在成员里；选区一端所在的行卸载后，整个选区清空。
//!
//! 键盘（按着 Shift 的移动键）从活动端扩展、收缩选区，单独一段可选中的文字走同一套（只有一段的成员表）。哪个键往哪走、
//! 一段之内怎么走和输入框共用（`editor/motion.rs`）。这里只有段和段之间：左右到段的边上接着下一段，上下按布局坐标找
//! 下一排的行。没有选区时不处理，按键照常往外传。上下移动保持的横坐标记在树上（一个窗口只有一个焦点），活动端
//! 被别的操作挪走之后对不上，自然作废。选区一端挨着行内元素时的位置按元素那一列算（`text/inline.rs`）。

use std::mem;
use std::ops::Range;

use parley::{Affinity, Cursor, Layout};

use super::{NodeId, Tree, layer_key};
use crate::editor::PointerAction;
use crate::editor::motion::{self, Motion};
use crate::geometry::Rect;
use crate::text::{SpanId, TextBlock};

/// 双击、三击之后拖动：按词、按段扩展
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Word,
    Line,
}

/// 选区的一端：哪段文字、字节偏移
type End = (NodeId, usize);

/// 拖到的位置：文字里的一点（相对文字原点），或者在这段文字的上面、下面（段首、段尾）
#[derive(Clone, Copy)]
enum Spot {
    At([f32; 2]),
    Start,
    End,
}

/// 一个选择根
struct SelectionRoot {
    node: NodeId,
    /// 成员：里面的文字，按树的顺序；`version` 是算它时树的结构版本
    members: Vec<NodeId>,
    version: u64,
    anchor: Option<End>,
    focus: Option<End>,
    /// 活动端贴着哪一边（自动折行处同一个偏移可以在上一行末尾，也可以在下一行开头）
    focus_side: Affinity,
    /// 双击、三击时一开始选中的范围（在起点那段里）
    unit: Option<(Unit, Range<usize>)>,
    /// 现在带着选区的成员（`members` 里第几段到第几段）：换选区时清掉不在新选区里的，键盘只重写变了的段。
    /// 成员表重算时下标对不上了，先全部清掉
    painted: Option<(usize, usize)>,
    /// 上次写选区时布局过几次（[`super::layout::LayoutState::passes`]）
    painted_pass: u64,
}

impl SelectionRoot {
    fn new(node: NodeId) -> Self {
        Self { node, members: Vec::new(), version: u64::MAX, anchor: None, focus: None, focus_side: Affinity::Downstream, unit: None, painted: None, painted_pass: 0 }
    }
}

/// 选择根的登记
#[derive(Default)]
pub(super) struct SelectionRoots {
    list: Vec<SelectionRoot>,
    /// 连续上下移动保持的横坐标（布局坐标）和当时活动端在哪：活动端不在那里了就作废
    goal: Option<(End, f32)>,
}

impl SelectionRoots {
    /// 这个元素是不是一个选择根（属性 `selection_root`）
    pub(super) fn set(&mut self, node: NodeId, root: bool) {
        let index = self.list.iter().position(|s| s.node == node);
        match (index, root) {
            (None, true) => self.list.push(SelectionRoot::new(node)),
            (Some(i), false) => {
                self.list.remove(i);
            }
            _ => {}
        }
    }

    /// 这个元素是选择根
    pub(super) fn contains(&self, node: NodeId) -> bool {
        self.list.iter().any(|s| s.node == node)
    }
}

impl Tree {
    /// 挡住选择根的元素：它和子孙里的字不参加跨段选择
    fn blocks_selection(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else { return true };
        node.exiting
            || self.listens(id, crate::handlers::EventType::Click)
            || node.input.as_ref().is_some_and(|i| i.focusable || i.disabled)
            || self.wants_text_input(id)
    }

    /// 文字所在的选择根（下标）：往上找，先遇到选择根就是它，先遇到挡住的元素（按钮）就不在选择根里
    pub(super) fn selection_root_of(&self, id: NodeId) -> Option<usize> {
        if self.selection_roots.list.is_empty() || self.blocks_selection(id) {
            return None;
        }
        let mut current = self.nodes.get(id)?.parent;
        while let Some(ancestor) = current {
            if let Some(index) = self.selection_roots.list.iter().position(|s| s.node == ancestor) {
                return Some(index);
            }
            // 富文本里的行内元素算在它所在的那段文字里，里面的字不单独参加
            if self.blocks_selection(ancestor) || self.inlines.is_host(ancestor) {
                return None;
            }
            current = self.nodes.get(ancestor)?.parent;
        }
        None
    }

    /// 重算成员表（结构变了才算）
    fn refresh_members(&mut self, ri: usize) {
        if self.selection_roots.list[ri].version == self.nodes.structure() {
            return;
        }
        let root = self.selection_roots.list[ri].node;
        let mut members = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            if id != root {
                if self.selection_roots.list.iter().any(|s| s.node == id) || self.blocks_selection(id) {
                    continue;
                }
                if self.nodes.element_kind(id).is_some_and(|k| k.kind_mask().contains(crate::property::registry::KindMask::TEXT)) {
                    members.push(id);
                    continue;
                }
            }
            stack.extend(node.children.iter().rev());
        }
        let entry = &mut self.selection_roots.list[ri];
        let old = mem::replace(&mut entry.members, members);
        entry.version = self.nodes.structure();
        if let Some((lo, hi)) = entry.painted.take() {
            for &id in &old[lo..=hi] {
                if let Some(block) = self.text_block_mut(id) {
                    block.clear_selection();
                    self.accessibility.touch(id);
                }
            }
        }
    }

    /// 选择根里的文字被按下、拖动（`window` 是窗口坐标）
    pub(super) fn selection_root_pointer(&mut self, ri: usize, id: NodeId, window: [f32; 2], action: PointerAction) {
        self.refresh_members(ri);
        match action {
            PointerAction::Press { clicks, extend } => self.selection_root_press(ri, id, window, clicks, extend),
            PointerAction::Drag => self.selection_root_drag(ri, window),
        }
        self.apply_selection_root(ri, true);
    }

    fn selection_root_press(&mut self, ri: usize, id: NodeId, window: [f32; 2], clicks: u32, extend: bool) {
        let Some(point) = self.text_point(id, window) else { return };
        let Some(block) = self.text_block(id) else { return };
        let entry = &self.selection_roots.list[ri];
        let anchored = entry.anchor.is_some_and(|(n, _)| self.nodes.contains_key(n));
        let (anchor, focus, side, unit) = match clicks {
            1 if extend && anchored => {
                let Some(caret) = block.caret_at(point) else { return };
                (entry.anchor.expect("上面检查过"), (id, caret.index()), caret.affinity(), None)
            }
            1 => {
                let Some(caret) = block.caret_at(point) else { return };
                ((id, caret.index()), (id, caret.index()), caret.affinity(), None)
            }
            2 => {
                let Some(word) = block.word_at(point) else { return };
                ((id, word.start), (id, word.end), Affinity::Upstream, Some((Unit::Word, word)))
            }
            _ => {
                let Some(line) = block.line_at(point) else { return };
                ((id, line.start), (id, line.end), Affinity::Upstream, Some((Unit::Line, line)))
            }
        };
        let entry = &mut self.selection_roots.list[ri];
        (entry.anchor, entry.focus, entry.focus_side, entry.unit) = (Some(anchor), Some(focus), side, unit);
    }

    /// 拖到哪段文字的哪里：在某段上就是它；否则找同一高度上最近的一段，再没有就找上下最近的一段
    /// （段落之间的空白、选择根的边上）。在那段文字上面算段首，下面算段尾；同一高度、在左右两边的由 parley 夹到行首行尾
    fn selection_root_target(&self, ri: usize, window: [f32; 2]) -> Option<(NodeId, Spot)> {
        let mut best: Option<(NodeId, (f32, f32), f32)> = None;
        for &id in &self.selection_roots.list[ri].members {
            let Some(node) = self.nodes.get(id) else { continue };
            let Some([x, y]) = node.world_invert(window) else { continue };
            let b = node.bounds;
            let dx = (b.x - x).max(x - (b.x + b.width)).max(0.0);
            let dy = (b.y - y).max(y - (b.y + b.height)).max(0.0);
            // 先比上下（同一高度的段优先），再比左右
            let score = (dy, dx);
            if best.is_none_or(|(_, s, _)| score < s) {
                best = Some((id, score, y - b.y));
            }
        }
        let (id, (dy, _), below) = best?;
        let spot = match dy > 0.0 {
            true if below > 0.0 => Spot::End,
            true => Spot::Start,
            false => Spot::At(self.text_point(id, window)?),
        };
        Some((id, spot))
    }

    fn selection_root_drag(&mut self, ri: usize, window: [f32; 2]) {
        let Some((target, spot)) = self.selection_root_target(ri, window) else { return };
        let Some(block) = self.text_block(target) else { return };
        let len = block.shown_len();
        // 段首、段尾：词和段都是空的一点
        let range_at = |unit: Unit| match spot {
            Spot::Start => Some(0..0),
            Spot::End => Some(len..len),
            Spot::At(point) if unit == Unit::Word => block.word_at(point),
            Spot::At(point) => block.line_at(point),
        };
        let entry = &self.selection_roots.list[ri];
        let Some((anchor_id, anchor_offset)) = entry.anchor else { return };
        let index = |id: NodeId| entry.members.iter().position(|&m| m == id);
        let (Some(a), Some(t)) = (index(anchor_id), index(target)) else { return };
        let (anchor, focus, side) = match &entry.unit {
            // 双击、三击之后：一开始选中的词（段）一直选着，另一头按词（段）对齐
            Some((unit, initial)) => {
                let Some(range) = range_at(*unit) else { return };
                if (t, range.start) < (a, initial.start) {
                    ((anchor_id, initial.end), (target, range.start), Affinity::Downstream)
                } else {
                    let end = if t == a { range.end.max(initial.end) } else { range.end };
                    ((anchor_id, initial.start), (target, end), Affinity::Upstream)
                }
            }
            None => {
                let (offset, side) = match spot {
                    Spot::Start => (0, Affinity::Downstream),
                    Spot::End => (len, Affinity::Upstream),
                    Spot::At(point) => {
                        let Some(caret) = block.caret_at(point) else { return };
                        (caret.index(), caret.affinity())
                    }
                };
                ((anchor_id, anchor_offset), (target, offset), side)
            }
        };
        let entry = &mut self.selection_roots.list[ri];
        // 按词扩展时起点跟着方向换到词的另一头；一开始的词还记着，拖回来时能换回去
        if entry.unit.is_some() {
            entry.anchor = Some(anchor);
        }
        (entry.focus, entry.focus_side) = (Some(focus), side);
    }

    /// ⌘A：选中选择根里的全部文字
    fn selection_root_select_all(&mut self, ri: usize) {
        self.refresh_members(ri);
        let members = &self.selection_roots.list[ri].members;
        let (Some(&first), Some(&last)) = (members.first(), members.last()) else { return };
        let len = self.text_block(last).map_or(0, |b| b.shown_len());
        let entry = &mut self.selection_roots.list[ri];
        (entry.anchor, entry.focus, entry.focus_side, entry.unit) = (Some((first, 0)), Some((last, len)), Affinity::Upstream, None);
        self.apply_selection_root(ri, true);
    }

    /// 选区的两端按顺序：((第几段, 偏移), (第几段, 偏移))；一端的文字已经卸载时是 `None`
    fn selection_root_span(&self, ri: usize) -> Option<((usize, usize), (usize, usize))> {
        let entry = &self.selection_roots.list[ri];
        let index = |(id, offset): End| Some((entry.members.iter().position(|&m| m == id)?, offset));
        let (a, f) = (index(entry.anchor?)?, index(entry.focus?)?);
        Some(if a <= f { (a, f) } else { (f, a) })
    }

    /// 把选区写进每段文字：第一段从起点到段尾，中间整段，最后一段从段首到终点；不在选区里的清掉。
    /// `whole` 时每段都重写。键盘每按一下只重写变了的段：上次和这次都在中间的段还是整段选中，一万段全选之后按一下
    /// 也只碰两头。中间布局过时也都重写：段的内容换了、重新断行时它自己的选区会被清掉
    fn apply_selection_root(&mut self, ri: usize, whole: bool) {
        let key = layer_key(self.selection_roots.list[ri].node);
        let pass = self.layout.passes();
        let whole = whole || mem::replace(&mut self.selection_roots.list[ri].painted_pass, pass) != pass;
        let span = self.selection_root_span(ri);
        if span.is_none() {
            // 一端卸载了（虚拟列表滚出去的行）：整个选区清空
            let entry = &mut self.selection_roots.list[ri];
            (entry.anchor, entry.focus, entry.unit) = (None, None, None);
        }
        let old = self.selection_roots.list[ri].painted;
        let new = span.map(|((lo, _), (hi, _))| (lo, hi));
        if let Some((a, b)) = old {
            let (below, above) = match new {
                Some((lo, hi)) => (a..lo.min(b + 1), (hi + 1).max(a)..b + 1),
                None => (a..b + 1, 0..0),
            };
            for i in below.chain(above) {
                let id = self.selection_roots.list[ri].members[i];
                if let Some(block) = self.text_block_mut(id) {
                    block.clear_selection();
                    self.accessibility.touch(id);
                }
            }
        }
        if let Some(((lo, start), (hi, end))) = span {
            // 上次和这次都在中间的段不用动
            let keep = match old {
                Some((a, b)) if !whole && a + 1 < b => a + 1..b,
                _ => 0..0,
            };
            let middle = (lo + 1..hi.min(keep.start)).chain(keep.end.max(lo + 1)..hi);
            let last = (hi > lo).then_some(hi);
            let ends = (self.selection_roots.list[ri].anchor, self.selection_roots.list[ri].focus);
            for i in std::iter::once(lo).chain(last).chain(middle) {
                let id = self.selection_roots.list[ri].members[i];
                let Some(block) = self.text_block_mut(id) else { continue };
                match ends {
                    // 同一段里保留方向（起点、终点照原样）
                    (Some((_, a)), Some((_, f))) if lo == hi => block.select_range(a, f, false, key),
                    _ => {
                        let len = block.shown_len();
                        let (from, to) = (if i == lo { start } else { 0 }, if i == hi { end } else { len });
                        block.select_range(from, to, from == 0 && to == len, key);
                    }
                }
                self.accessibility.touch(id);
            }
        }
        self.selection_roots.list[ri].painted = new;
        self.paint.invalidate();
    }

    /// ⌘C 要复制的：各段选中的部分按顺序连起来。上下排的段之间换行，并排的（同一行里的几段文字）用制表符隔开
    fn selection_root_text(&self, ri: usize) -> Option<String> {
        let ((lo, _), (hi, _)) = self.selection_root_span(ri)?;
        let mut text = String::new();
        let mut previous: Option<NodeId> = None;
        for &id in &self.selection_roots.list[ri].members[lo..=hi] {
            let Some(piece) = self.text_block(id).and_then(|b| b.selected_text()) else { continue };
            if let Some(prev) = previous {
                let (a, b) = (self.nodes[prev].bounds, self.nodes[id].bounds);
                text.push(if same_row([a.y, a.y + a.height], [b.y, b.y + b.height]) { '\t' } else { '\n' });
            }
            text.push_str(&piece);
            previous = Some(id);
        }
        (!text.is_empty()).then_some(text)
    }

    /// 有焦点的文字在选择根里时处理 ⌘C、⌘A，返回是否处理了；不在选择根里返回 `None`
    pub(super) fn selection_root_key(&mut self, id: NodeId, copy: bool) -> Option<bool> {
        let ri = self.selection_root_of(id)?;
        if copy {
            if let Some(text) = self.selection_root_text(ri) {
                crate::system::clipboard::write_text(text);
            }
        } else {
            self.selection_root_select_all(ri);
        }
        Some(true)
    }

    /// 卸载之后：卸载了的选择根不再登记，两端指向卸载了的文字的选区清掉
    pub(super) fn forget_selection_roots(&mut self) {
        if !self.selection_roots.list.is_empty() {
            self.selection_roots.list.retain(|s| self.nodes.contains_key(s.node));
            self.forget_selection_root_ends();
        }
    }

    /// 卸载之后：选区一端所在的文字没了（虚拟列表滚出去的行）的选择根，整个选区清空，还挂着的段上的选区也去掉
    fn forget_selection_root_ends(&mut self) {
        for ri in 0..self.selection_roots.list.len() {
            let entry = &self.selection_roots.list[ri];
            let gone = |end: Option<End>| end.is_some_and(|(id, _)| !self.nodes.contains_key(id));
            if gone(entry.anchor) || gone(entry.focus) {
                let entry = &mut self.selection_roots.list[ri];
                (entry.anchor, entry.focus) = (None, None);
                self.apply_selection_root(ri, true);
            }
        }
    }

    /// 绘制时：有焦点的文字所在的选择根（选区画成有焦点的颜色）
    pub(super) fn focused_selection_root(&self) -> Option<u64> {
        self.text_block(self.router.focused()?)?.selection_root
    }

    // ---- 键盘 ----

    /// 按着 Shift 的移动键：从活动端扩展、收缩有焦点的文字的选区（选择根里的整个选区、单独一段可选中的文字）。
    /// 点过之后（哪怕还没选中字）就算有选区；没有时返回 `false`，按键照常往外传
    pub(super) fn extend_selection(&mut self, id: NodeId, motion: Motion) -> bool {
        if self.in_selection_root(id)
            && let Some(ri) = self.selection_root_of(id)
        {
            return self.extend_selection_root(ri, motion);
        }
        if !self.text_block(id).is_some_and(|block| block.selectable) || !self.lay_out_text(id) {
            return false;
        }
        let Some(from) = self.text_block(id).and_then(TextBlock::selection_focus) else { return false };
        let (_, to) = self.selection_step(std::slice::from_ref(&id), 0, from, motion);
        if let Some(block) = self.text_block_mut(id) {
            block.set_selection_focus(to);
        }
        self.accessibility.touch(id);
        self.paint.invalidate();
        self.reveal_selection_end(id, to);
        true
    }

    fn extend_selection_root(&mut self, ri: usize, motion: Motion) -> bool {
        self.refresh_members(ri);
        if self.selection_root_span(ri).is_none() {
            // 没有选区，或者一端已经不是成员了（挪进了按钮里）：有的话清掉
            if self.selection_roots.list[ri].anchor.is_some() {
                self.apply_selection_root(ri, true);
            }
            return false;
        }
        let entry = &mut self.selection_roots.list[ri];
        let (Some((node, offset)), side) = (entry.focus, entry.focus_side) else { return false };
        // 走的时候要排版（借着整棵树），成员表先拿下来
        let members = mem::take(&mut entry.members);
        let index = members.iter().position(|&m| m == node);
        let moved = index.and_then(|index| {
            self.lay_out_text(node).then_some(())?;
            let (layout, _) = self.text_block(node)?.shown_parts()?;
            let from = Cursor::from_byte_index(layout, offset, side);
            let (to_index, to) = self.selection_step(&members, index, from, motion);
            Some((members[to_index], to))
        });
        let entry = &mut self.selection_roots.list[ri];
        entry.members = members;
        let Some((to_node, to)) = moved else { return true };
        (entry.focus, entry.focus_side, entry.unit) = (Some((to_node, to.index())), to.affinity(), None);
        self.apply_selection_root(ri, false);
        self.reveal_selection_end(to_node, to);
        true
    }

    /// 从第 `index` 段的 `from` 走一步，落在第几段的哪里（走不动时原地）。上下移动保持横坐标，别的移动之后作废
    fn selection_step(&mut self, members: &[NodeId], index: usize, from: Cursor, motion: Motion) -> (usize, Cursor) {
        let id = members[index];
        let goal = matches!(motion, Motion::Up | Motion::Down).then(|| self.selection_goal(id, from)).flatten();
        let landed = match motion {
            Motion::Left { word } => self.step_across(members, index, from, false, word),
            Motion::Right { word } => self.step_across(members, index, from, true, word),
            Motion::Up => goal.and_then(|x| self.step_line(members, index, from, false, x)),
            Motion::Down => goal.and_then(|x| self.step_line(members, index, from, true, x)),
            Motion::ParagraphUp => self.step_paragraph(members, index, from, false),
            Motion::ParagraphDown => self.step_paragraph(members, index, from, true),
            Motion::LineStart => self.text_block(id).and_then(|block| block.line_edge(from, false)).map(|to| (index, to)),
            Motion::LineEnd => self.text_block(id).and_then(|block| block.line_edge(from, true)).map(|to| (index, to)),
            Motion::RangeStart => self.range_edge(members, false),
            Motion::RangeEnd => self.range_edge(members, true),
        };
        let (to_index, to) = landed.unwrap_or((index, from));
        self.selection_roots.goal = goal.map(|x| ((members[to_index], to.index()), x));
        (to_index, to)
    }

    /// 上下移动要保持的横坐标（布局坐标）：活动端还在上次走到的地方时接着用，否则是它现在的位置
    fn selection_goal(&self, id: NodeId, from: Cursor) -> Option<f32> {
        if let Some((end, x)) = self.selection_roots.goal
            && end == (id, from.index())
        {
            return Some(x);
        }
        let [x, _] = self.text_origin(id)?;
        Some(x + self.text_block(id)?.caret_box(from)?.x)
    }

    /// 文字原点的布局坐标
    fn text_origin(&self, id: NodeId) -> Option<[f32; 2]> {
        let b = self.nodes.get(id)?.bounds;
        let [left, top, _, _] = self.nodes.insets(id);
        Some([b.x + left, b.y + top])
    }

    /// 按最终宽度排好（绘制时也是这样排的；还没画过的段可能还是量大小时的断行）。不是文字时返回 `false`
    fn lay_out_text(&mut self, id: NodeId) -> bool {
        let Some(b) = self.nodes.get(id).map(|node| node.bounds) else { return false };
        let [left, _, right, _] = self.nodes.insets(id);
        let Some(block) = self.nodes.kind_mut::<TextBlock>(id) else { return false };
        block.layout_at(&mut self.text, b.width - left - right);
        true
    }

    /// 键盘走得到的段：有字（只放了行内元素的也算）、有高度（空段、隐藏的段跳过），排好了
    fn walkable(&mut self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(|node| node.bounds.height > 0.0) && self.lay_out_text(id) && self.text_block(id).is_some_and(|block| block.shown_len() > 0)
    }

    /// 一段之内走一步。选区要变（偏移变了）才算：折行处同一个偏移在上一行末尾和下一行开头是两个位置，
    /// 插入点看得见时算一步，这里看不出来。走不动时是 `None`
    fn step_within(&self, id: NodeId, from: Cursor, forward: bool, word: bool) -> Option<Cursor> {
        let (layout, text) = self.text_block(id)?.shown_parts()?;
        let mut at = from;
        for _ in 0..3 {
            let next = if forward { motion::right(layout, text, at, word) } else { motion::left(layout, text, at, word) };
            if next == at {
                return None;
            }
            if next.index() != from.index() {
                return Some(next);
            }
            at = next;
        }
        None
    }

    /// 左右走一步；到了这一段的边上接着下一段（没有字的段跳过）。段和段之间没有东西可选，接过去的这一步要选上
    /// 下一段的第一个字（词），不然按了一下什么都没变
    fn step_across(&mut self, members: &[NodeId], index: usize, from: Cursor, forward: bool, word: bool) -> Option<(usize, Cursor)> {
        if let Some(to) = self.step_within(members[index], from, forward, word) {
            return Some((index, to));
        }
        for j in ahead(index, members.len(), forward) {
            if !self.walkable(members[j]) {
                continue;
            }
            let (layout, _) = self.text_block(members[j])?.shown_parts()?;
            let edge = edge_of(layout, !forward);
            return Some((j, self.step_within(members[j], edge, forward, word).unwrap_or(edge)));
        }
        None
    }

    /// 上下走一行：这段里还有行就是它；否则按树的顺序往后（往前）找第一条在这一行下面（上面）的行，
    /// 再在它那一排（并排的几段）里挑横向离 `x` 最近的；都没有时到范围的头尾
    fn step_line(&mut self, members: &[NodeId], index: usize, from: Cursor, down: bool, x: f32) -> Option<(usize, Cursor)> {
        let id = members[index];
        let caret = self.text_block(id)?.caret_box(from)?;
        let lines = self.lines_of(id)?;
        let line = lines.at_y(lines.origin[1] + caret.y + caret.height * 0.5);
        let within = if down { (line + 1 < lines.count).then_some(line + 1) } else { line.checked_sub(1) };
        if let Some(next) = within {
            return Some((index, self.cursor_on_line(id, next, x)?));
        }
        let row = lines.line(line)?;
        match self.next_row(members, index, row, down, x) {
            Some((j, next)) => Some((j, self.cursor_on_line(members[j], next, x)?)),
            None => self.range_edge(members, down),
        }
    }

    /// 文字第 `line` 行上布局坐标的横坐标 `x` 处的位置
    fn cursor_on_line(&self, id: NodeId, line: usize, x: f32) -> Option<Cursor> {
        let [left, _] = self.text_origin(id)?;
        self.text_block(id)?.caret_on_line(line, x - left)
    }

    /// 文字的行（布局坐标）
    fn lines_of(&self, id: NodeId) -> Option<ShownLines<'_>> {
        let block = self.text_block(id)?;
        let (layout, _) = block.shown_parts()?;
        Some(ShownLines { layout, count: block.shown_line_count(), origin: self.text_origin(id)? })
    }

    /// `row` 这一行的下一排（`down` 时往下）在第几段的第几行
    fn next_row(&mut self, members: &[NodeId], index: usize, row: LineBox, down: bool, x: f32) -> Option<(usize, usize)> {
        let mut found = None;
        // 分栏：下一栏的段都不在这一行下面。后面再也没有下面的行时接着走下一栏，不跳过它
        let mut column = None;
        for j in ahead(index, members.len(), down) {
            if !self.walkable(members[j]) {
                continue;
            }
            let lines = self.lines_of(members[j])?;
            if let Some(line) = lines.past(&row, down) {
                found = Some((j, line));
                break;
            }
            let edge = lines.edge(down);
            if column.is_none() && lines.line(edge).is_some_and(|l| !l.same_row(&row)) {
                column = Some((j, edge));
            }
        }
        let (j, line) = found.or(column)?;
        // 同一排的后面几段（并排的名字和消息）里挑横向最近的
        let target = self.lines_of(members[j])?.line(line)?;
        let mut best = (j, line, target.distance(x));
        for k in ahead(j, members.len(), down) {
            let b = self.nodes.get(members[k])?.bounds;
            if (down && b.y >= target.bottom) || (!down && b.y + b.height <= target.top) {
                break;
            }
            if !self.walkable(members[k]) {
                continue;
            }
            let lines = self.lines_of(members[k])?;
            if let Some(on) = lines.on_row(&target) {
                let distance = lines.line(on)?.distance(x);
                if distance < best.2 {
                    best = (k, on, distance);
                }
            }
        }
        Some((best.0, best.1))
    }

    /// 到段首（段尾）：一段文字里按换行符分的段；已经在那里时到上一段的段首（下一段的段尾），跨过文字的边界
    fn step_paragraph(&mut self, members: &[NodeId], index: usize, from: Cursor, down: bool) -> Option<(usize, Cursor)> {
        let at = from.index();
        let (layout, text) = self.text_block(members[index])?.shown_parts()?;
        let target = if down {
            let end = paragraph_end(text, at);
            if end > at {
                Some(end)
            } else {
                (at < text.len()).then(|| paragraph_end(text, at + 1))
            }
        } else {
            let start = paragraph_start(text, at);
            if start < at {
                Some(start)
            } else {
                (at > 0).then(|| paragraph_start(text, at - 1))
            }
        };
        if let Some(offset) = target {
            return Some((index, paragraph_edge(layout, offset, down)));
        }
        for j in ahead(index, members.len(), down) {
            if self.walkable(members[j]) {
                let (layout, text) = self.text_block(members[j])?.shown_parts()?;
                let offset = if down { paragraph_end(text, 0) } else { paragraph_start(text, text.len()) };
                return Some((j, paragraph_edge(layout, offset, down)));
            }
        }
        None
    }

    /// 范围的开头、末尾（第一段、最后一段有字的文字）
    fn range_edge(&mut self, members: &[NodeId], end: bool) -> Option<(usize, Cursor)> {
        let count = members.len();
        for k in 0..count {
            let j = if end { count - 1 - k } else { k };
            if self.walkable(members[j]) {
                let (layout, _) = self.text_block(members[j])?.shown_parts()?;
                return Some((j, edge_of(layout, end)));
            }
        }
        None
    }

    /// 活动端滚到看得见（立即跳：按住键时一直看得见，和多行输入框的插入点一样）
    fn reveal_selection_end(&mut self, id: NodeId, at: Cursor) {
        let Some([x, y]) = self.text_origin(id) else { return };
        let Some(caret) = self.text_block(id).and_then(|block| block.caret_box(at)) else { return };
        self.scroll_rect_into_view(id, Rect { x: x + caret.x, y: y + caret.y, ..caret }, false);
    }
}

/// 从第 `index` 段往后（`forward`）或往前的下标
fn ahead(index: usize, len: usize, forward: bool) -> impl Iterator<Item = usize> {
    (1..).map_while(move |k| if forward { Some(index + k).filter(|&i| i < len) } else { index.checked_sub(k) })
}

/// 一段文字的开头、末尾
fn edge_of(layout: &Layout<SpanId>, end: bool) -> Cursor {
    if end { Cursor::from_byte_index(layout, usize::MAX, Affinity::Upstream) } else { Cursor::from_byte_index(layout, 0, Affinity::Downstream) }
}

/// 段首、段尾的位置
fn paragraph_edge(layout: &Layout<SpanId>, offset: usize, end: bool) -> Cursor {
    Cursor::from_byte_index(layout, offset, if end { Affinity::Upstream } else { Affinity::Downstream })
}

/// `offset` 所在的这一段（按换行符分）的开头
fn paragraph_start(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1)
}

/// `offset` 所在的这一段的末尾（换行符前面）
fn paragraph_end(text: &str, offset: usize) -> usize {
    text.as_bytes()[offset..].iter().position(|&b| b == b'\n').map_or(text.len(), |i| offset + i)
}

/// 纵向范围 `a`、`b` 算同一排：重叠超过较矮的一半
fn same_row(a: [f32; 2], b: [f32; 2]) -> bool {
    a[1].min(b[1]) - a[0].max(b[0]) > (a[1] - a[0]).min(b[1] - b[0]) * 0.5
}

/// 文字里的一行，布局坐标
#[derive(Clone, Copy)]
struct LineBox {
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

impl LineBox {
    fn same_row(&self, other: &LineBox) -> bool {
        same_row([self.top, self.bottom], [other.top, other.bottom])
    }

    /// 在 `row` 下面（`down`）或者上面，不和它同一排
    fn past(&self, row: &LineBox, down: bool) -> bool {
        let (mine, its) = (self.top + self.bottom, row.top + row.bottom);
        !self.same_row(row) && if down { mine > its } else { mine < its }
    }

    /// 横向离 `x` 多远
    fn distance(&self, x: f32) -> f32 {
        (self.left - x).max(x - self.right).max(0.0)
    }
}

/// 一段文字排好的行。行从上到下，找行用二分（一段上万行时按一下也不用逐行看）
struct ShownLines<'a> {
    layout: &'a Layout<SpanId>,
    /// 显示出来有几行（行内元素后面多出来的空行不算）
    count: usize,
    origin: [f32; 2],
}

impl ShownLines<'_> {
    fn line(&self, i: usize) -> Option<LineBox> {
        let line = self.layout.get(i)?;
        let m = line.metrics();
        let left = self.origin[0] + m.offset + m.inline_min_coord;
        Some(LineBox { top: self.origin[1] + m.block_min_coord, bottom: self.origin[1] + m.block_max_coord, left, right: left + m.advance })
    }

    /// 第一条满足 `f` 的行（`f` 从某一行起一直成立），没有时是行数
    fn first(&self, f: impl Fn(&LineBox) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.line(mid).is_none_or(|l| f(&l)) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// 从 `row` 往下（往上）碰到的第一行
    fn past(&self, row: &LineBox, down: bool) -> Option<usize> {
        if down {
            Some(self.first(|l| l.past(row, true))).filter(|&i| i < self.count)
        } else {
            self.first(|l| !l.past(row, false)).checked_sub(1)
        }
    }

    /// 布局坐标的纵坐标 `y` 在第几行
    fn at_y(&self, y: f32) -> usize {
        self.first(|l| y < l.bottom).min(self.count.saturating_sub(1))
    }

    /// 和 `row` 同一排的行
    fn on_row(&self, row: &LineBox) -> Option<usize> {
        let i = self.first(|l| l.bottom > row.top);
        self.line(i).filter(|l| l.same_row(row)).map(|_| i)
    }

    /// 往下走时先碰到的一行（第一行），往上走时先碰到的一行（最后一行）
    fn edge(&self, down: bool) -> usize {
        if down { 0 } else { self.count.saturating_sub(1) }
    }
}
