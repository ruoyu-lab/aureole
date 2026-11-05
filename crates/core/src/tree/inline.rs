//! 富文本里的行内元素（[`crate::Span::inline`]）在元素树这一边的部分。
//!
//! 行内元素是文字节点的子节点，所以样式继承、绘制、命中测试、Tab、无障碍都按元素树照常走。不一样的只有布局：
//! 文字在布局树里是叶子（要量），不能有子节点，行内元素自成一棵（没有父节点），大小量好之后放进文字的排版，
//! 位置再从排版里读出来：
//!
//! 1. **先量元素**（[`Tree::measure_inlines`]，布局之前）：标了脏的元素单独布局一遍，大小、基线、对齐写进文字的占位盒子，
//!    变了就让文字重新断行。里层先量（行内元素里面还可以有带行内元素的文字）；
//! 2. **再对宽度**（[`Tree::settle_inlines`]，布局之后，和绕排在同一个循环里）：文字的宽度定下来了，换这个宽度再量一遍元素
//!    （百分比、里面的文字换行按它算），大小变了就再布局一次；
//! 3. **读位置**（[`Tree::finish_inlines`]）：文字按最终宽度排好，记下每个元素的位置和有没有被截掉，截掉的从子元素里拿掉
//!    （不显示、不响应，但还在挂着，宽度够了再放回来）；
//! 4. **定位**（`tree/place.rs`）：按读到的位置放元素和它的子树。
//!
//! 没有行内元素的应用只多几次"表是空的"判断。

use smallvec::SmallVec;

use slotmap::SecondaryMap;

use super::{FocusCallbacks, NodeId, Tree, TreeRef};
use crate::geometry::Affine;
use crate::kinds::ElementKind;
use crate::text::{InlineAlign, InlineMetrics, InlineSlot, TextBlock};

/// 带行内元素的文字的登记
#[derive(Default)]
pub(super) struct Inlines {
    /// 带行内元素的文字，外层在前（量的时候倒着走）
    hosts: Vec<NodeId>,
    /// 排好 `hosts` 时树的结构版本
    sorted: Option<u64>,
    /// 设了 [`crate::Element::inline_align`] 的元素
    aligns: SecondaryMap<NodeId, InlineAlign>,
}

impl Inlines {
    /// 一个带行内元素的文字都没有
    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// 这个元素是带行内元素的文字
    pub(super) fn is_host(&self, id: NodeId) -> bool {
        self.hosts.contains(&id)
    }

    pub(super) fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        self.hosts.retain(|&id| keep(id));
    }

    /// 元素卸载了
    pub(super) fn forget(&mut self, id: NodeId) {
        self.aligns.remove(id);
    }
}

/// 挂载时登记：哪几段是行内元素
pub(super) fn attach(tree: &TreeRef, id: NodeId, children: &[(usize, NodeId)]) {
    let mut t = tree.borrow_mut();
    let Some(block) = t.nodes.kind_mut::<TextBlock>(id) else { return };
    block.attach_inlines(children.iter().map(|&(span, node)| InlineSlot::new(span, node)).collect());
    t.inlines.hosts.push(id);
    t.inlines.sorted = None;
}

/// 文字卸载时：被截掉的行内元素不在子元素里，要另外卸载
pub(super) fn hidden_children(kind: Option<&dyn ElementKind>) -> SmallVec<[NodeId; 4]> {
    let rich = kind.and_then(|kind| kind.downcast_ref::<TextBlock>()).and_then(TextBlock::rich);
    rich.map_or_else(SmallVec::new, |rich| rich.inlines().iter().filter(|slot| !slot.shown).map(|slot| slot.node).collect())
}

/// Tab 在又有链接又有能拿焦点的行内元素的文字里停留的地方
#[derive(Clone, Copy, PartialEq)]
enum Stop {
    Link(usize),
    Element(NodeId),
}

impl Tree {
    /// 带行内元素的文字，外层在前（结构变了才重新排）
    pub(super) fn inline_hosts(&mut self) -> Vec<NodeId> {
        if self.inlines.sorted != Some(self.nodes.structure()) {
            let depth = |id: NodeId| std::iter::successors(self.nodes.get(id).and_then(|n| n.parent), |&p| self.nodes.get(p).and_then(|n| n.parent)).count();
            let mut hosts = std::mem::take(&mut self.inlines.hosts);
            hosts.sort_by_cached_key(|&id| depth(id));
            self.inlines.hosts = hosts;
            self.inlines.sorted = Some(self.nodes.structure());
        }
        self.inlines.hosts.clone()
    }

    fn rich_of(&self, host: NodeId) -> Option<&crate::text::Rich> {
        self.text_block(host)?.rich()
    }

    /// 布局之前：量标了脏的行内元素，里层先量
    pub(super) fn measure_inlines(&mut self) {
        if self.inlines.is_empty() {
            return;
        }
        for host in self.inline_hosts().into_iter().rev() {
            self.measure_host(host, false);
        }
    }

    /// 量文字里的行内元素（`force` 时全部，否则标了脏的），写进占位盒子。有变的时候文字要重新量，返回 true
    fn measure_host(&mut self, host: NodeId, force: bool) -> bool {
        let Some(rich) = self.rich_of(host) else { return false };
        let width = rich.inline_available();
        let children: SmallVec<[NodeId; 4]> = rich.inlines().iter().map(|slot| slot.node).collect();
        let mut changed = false;
        for (k, child) in children.into_iter().enumerate() {
            let Some(layout) = self.nodes.get(child).map(|node| node.layout) else { continue };
            if !force && !self.layout.is_stale(layout) {
                continue;
            }
            self.layout.compute_detached(layout, width, &mut self.text, &mut self.nodes);
            let result = self.layout.get(layout);
            let (margin, size) = (result.margin, result.size);
            let metrics = InlineMetrics {
                size: [size.width + margin.left + margin.right, size.height + margin.top + margin.bottom],
                baseline: self.inline_baseline(child).map(|b| b + margin.top),
                align: self.inline_align_of(child),
            };
            if let Some(block) = self.text_block_mut(host) {
                changed |= block.set_inline_metrics(k, metrics);
            }
        }
        if changed {
            let layout = self.nodes[host].layout;
            self.layout.mark_dirty(layout);
            self.accessibility.touch(host);
        }
        changed
    }

    /// 元素的基线（相对它的顶）：文字这样带基线的元素用它自己的，容器用第一个子元素的；都没有时是 `None`（底边）
    fn inline_baseline(&self, id: NodeId) -> Option<f32> {
        let mut current = id;
        let mut y = 0.0;
        loop {
            let node = self.nodes.get(current)?;
            let layout = self.layout.try_get(node.layout)?;
            if current != id {
                y += layout.location.y;
            }
            if let Some((first, _)) = self.nodes.element_kind(current).and_then(|kind| kind.baselines()) {
                return Some(y + layout.padding.top + layout.border.top + first);
            }
            current = *node.children.first()?;
        }
    }

    /// 布局之后：文字的宽度定下来了，换这个宽度量行内元素。大小变了要再布局一次，返回 true。
    /// 外层的文字换了宽度时，里层的文字宽度也可能变，所以再走一遍（元素自成一棵，不用等下一次布局）
    pub(super) fn settle_inlines(&mut self) -> bool {
        if self.inlines.is_empty() {
            return false;
        }
        let hosts = self.inline_hosts();
        let mut changed = false;
        for _ in 0..3 {
            let mut resized = false;
            for &host in hosts.iter().rev() {
                let Some(width) = self.nodes.get(host).and_then(|node| self.layout.try_get(node.layout)).map(|layout| layout.content_box_width()) else {
                    continue;
                };
                let Some(rich) = self.text_block_mut(host).and_then(TextBlock::rich_mut) else { continue };
                let new_width = rich.set_inline_available(width);
                resized |= new_width;
                changed |= self.measure_host(host, new_width);
            }
            if !resized {
                break;
            }
        }
        changed
    }

    /// 布局定了：文字按最终宽度排好，记下元素的位置；被截掉的从子元素里拿掉，放得下了放回来
    pub(super) fn finish_inlines(&mut self) {
        if self.inlines.is_empty() {
            return;
        }
        for host in self.inline_hosts() {
            let Some(width) = self.nodes.get(host).and_then(|node| self.layout.try_get(node.layout)).map(|layout| layout.content_box_width()) else {
                continue;
            };
            let Some(block) = self.nodes.kind_mut::<TextBlock>(host) else { continue };
            if block.place_inlines(&mut self.text, width) {
                self.update_shown_inlines(host);
            }
        }
    }

    /// 被截掉的行内元素变了：重新展开子元素（放回来的元素藏着时样式没跟着继承，这里也会再传一遍）；
    /// 焦点在被截掉的元素上时交出去
    fn update_shown_inlines(&mut self, host: NodeId) {
        self.rebuild_children(host);
        let hidden: SmallVec<[NodeId; 4]> =
            self.rich_of(host).map_or_else(SmallVec::new, |rich| rich.inlines().iter().filter(|slot| !slot.shown).map(|slot| slot.node).collect());
        if let Some(focused) = self.router.focused()
            && hidden.iter().any(|&child| self.nodes.is_within(focused, child))
        {
            let callbacks = self.focus_to(None, false);
            self.router.defer_callbacks(callbacks);
        }
    }

    /// 展开文字的子元素时（`rebuild_children`）：行内元素不进布局树，截掉的不算子元素
    pub(super) fn filter_inline_children(&self, id: NodeId, children: &mut Vec<NodeId>, layouts: &mut Vec<taffy::NodeId>) {
        if !self.inlines.is_host(id) {
            return;
        }
        let Some(rich) = self.rich_of(id) else { return };
        for slot in rich.inlines() {
            if !slot.shown {
                children.retain(|&child| child != slot.node);
            }
            if let Some(node) = self.nodes.get(slot.node) {
                layouts.retain(|&layout| layout != node.layout);
            }
        }
    }

    /// 显示着的行内元素和它们的外边距盒左上角（布局坐标）
    pub(super) fn inline_origins(&self, host: NodeId) -> SmallVec<[(NodeId, [f32; 2]); 4]> {
        let (Some(node), Some(rich)) = (self.nodes.get(host), self.rich_of(host)) else { return SmallVec::new() };
        let [left, top, ..] = self.nodes.insets(host);
        let origin = [node.bounds.x + left, node.bounds.y + top];
        rich.inlines().iter().filter(|slot| slot.shown).map(|slot| (slot.node, [origin[0] + slot.at[0], origin[1] + slot.at[1]])).collect()
    }

    /// `id` 是行内元素时：外边距盒左上角相对文字元素的位置（布局算出来的那种"相对父元素的位置"）
    pub(super) fn inline_position(&self, id: NodeId) -> Option<[f32; 2]> {
        if self.inlines.is_empty() {
            return None;
        }
        let host = self.nodes.get(id)?.parent?;
        let slot = self.rich_of(host)?.inlines().iter().find(|slot| slot.node == id)?;
        let [left, top, ..] = self.nodes.insets(host);
        Some([left + slot.at[0], top + slot.at[1]])
    }

    /// `id` 自己是行内元素、或者在行内元素里面时：布局树到那个行内元素为止，还要加上这么多才是窗口里的布局位置
    pub(super) fn inline_origin_in_layout(&self, id: NodeId) -> Option<[f32; 2]> {
        if self.inlines.is_empty() {
            return None;
        }
        let mut current = id;
        loop {
            if let Some([x, y]) = self.inline_position(current) {
                let host = self.nodes.get(current)?.parent?;
                let rect = self.layout_rect(host)?;
                return Some([rect.x + x, rect.y + y]);
            }
            current = self.nodes.get(current)?.parent?;
        }
    }

    // ---- 对齐（属性 `inline_align`）----

    pub(super) fn set_inline_align(&mut self, id: NodeId, align: InlineAlign) {
        self.inlines.aligns.insert(id, align);
        // 元素（或者包着它的那一层）下次量的时候读到新的对齐
        if let Some(layout) = self.nodes.get(id).map(|node| node.layout) {
            self.layout.mark_dirty(layout);
        }
    }

    pub(super) fn inline_align(&self, id: NodeId) -> Option<InlineAlign> {
        self.inlines.aligns.get(id).copied()
    }

    /// 怎么对齐：设在元素上的；用闭包换内容时元素外面包着一层，看里面那一个
    fn inline_align_of(&self, id: NodeId) -> InlineAlign {
        let own = self.inline_align(id);
        let inner = || match self.nodes.get(id)?.children.as_slice() {
            [only] => self.inline_align(*only),
            _ => None,
        };
        own.or_else(inner).unwrap_or_default()
    }

    // ---- 复制 ----

    /// 复制之前：写下代替行内元素的文字
    pub(super) fn refresh_inline_copy(&mut self) {
        for host in self.inlines.hosts.clone() {
            let Some(rich) = self.rich_of(host) else { continue };
            let copies: SmallVec<[String; 4]> = rich.inlines().iter().map(|slot| self.inline_copy_text(slot.node)).collect();
            let Some(rich) = self.text_block_mut(host).and_then(TextBlock::rich_mut) else { continue };
            for (k, copy) in copies.into_iter().enumerate() {
                rich.set_inline_copy(k, copy);
            }
        }
    }

    // ---- 选区 ----

    /// 选区盖住了这段文字里的行内元素：元素画完后在上面画选区
    pub(super) fn has_selected_inlines(&self, id: NodeId) -> bool {
        !self.inlines.is_empty() && self.text_block(id).is_some_and(|block| !block.selected_inline_columns().is_empty())
    }

    /// 选中的行内元素上面的选区：和字下面的选区同一个颜色，一整列（这一行的高度）。
    /// 子元素画完以后才调用，不透明的图片、按钮上也看得出选中了
    pub(super) fn paint_inline_selection(&self, id: NodeId, layer: u32, clip: u32, local: &Affine, opacity: f32, list: &mut scene::DisplayList) {
        if self.inlines.is_empty() {
            return;
        }
        let (Some(block), Some(node)) = (self.text_block(id), self.nodes.get(id)) else { return };
        let Some(fill) = block.inline_selection_fill() else { return };
        let [left, top, ..] = self.nodes.insets(id);
        let origin = [node.bounds.x + left, node.bounds.y + top];
        for column in block.selected_inline_columns() {
            let rect = parley::BoundingBox::new(column.x as f64, column.y as f64, (column.x + column.width) as f64, (column.y + column.height) as f64);
            let mut quad = crate::editor::selection_quad(origin, rect, local, fill, opacity);
            quad.layer = layer;
            quad.clip = clip;
            list.quads.push(quad);
        }
    }

    // ---- Tab ----

    /// 焦点所在的、或者最近的带行内元素的文字
    fn inline_host_of(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            if self.inlines.is_host(id) {
                return Some(id);
            }
            id = self.nodes.get(id)?.parent?;
        }
    }

    /// 文字里 Tab 停留的地方，按在文字里的先后。只有又有链接又有能拿焦点的行内元素时才要排（`Some`）：
    /// 只有链接时是原来的走法，只有元素时按树的顺序就对了
    fn inline_stops(&self, host: NodeId) -> Option<Vec<Stop>> {
        let rich = self.rich_of(host)?;
        if rich.links() == 0 {
            return None;
        }
        let mut stops = Vec::new();
        let mut links = 0;
        let mut any = false;
        for (i, span) in rich.spans.iter().enumerate() {
            if span.link.is_some() {
                stops.push(Stop::Link(links));
                links += 1;
            } else if let Some(node) = rich.inline_of_span(i)
                && rich.inlines().iter().any(|slot| slot.node == node && slot.shown)
            {
                for element in self.focus_order(Some(node)) {
                    stops.push(Stop::Element(element));
                    any = true;
                }
            }
        }
        any.then_some(stops)
    }

    /// 焦点落到文字里的一个停留的地方
    fn focus_stop(&mut self, host: NodeId, stop: Stop, forward: bool) -> FocusCallbacks {
        match stop {
            Stop::Link(k) => {
                let callbacks = self.focus_to(Some(host), true);
                self.enter_links(host, forward, Some(k));
                self.accessibility.touch(host);
                self.scroll_into_view(host, true);
                callbacks
            }
            Stop::Element(element) => {
                let callbacks = self.focus_to(Some(element), true);
                self.enter_links(element, forward, None);
                self.scroll_into_view(element, true);
                callbacks
            }
        }
    }

    /// Tab：焦点在又有链接又有能拿焦点的行内元素的文字里时，按它们在文字里的先后走；走出这段文字时走到树的顺序里
    /// 下一个不在它里面的元素。别的情况返回 `None`，走原来的路
    pub(super) fn step_inline(&mut self, forward: bool) -> Option<FocusCallbacks> {
        if self.inlines.is_empty() {
            return None;
        }
        let focused = self.router.focused()?;
        let host = self.inline_host_of(focused)?;
        let stops = self.inline_stops(host)?;
        let current = if focused == host {
            self.rich_of(host)?.focused.map(Stop::Link)
        } else {
            Some(Stop::Element(focused))
        };
        let position = current.and_then(|stop| stops.iter().position(|&s| s == stop));
        let next = match (position, forward) {
            (Some(i), true) => stops.get(i + 1),
            (Some(i), false) => i.checked_sub(1).and_then(|i| stops.get(i)),
            (None, true) => stops.first(),
            (None, false) => None,
        };
        if let Some(&stop) = next {
            return Some(self.focus_stop(host, stop, forward));
        }
        let trap = self.trap();
        let order = self.focus_order(trap);
        let n = order.len();
        let start = order.iter().position(|&id| id == host)?;
        let outside = (1..n).map(|k| order[if forward { (start + k) % n } else { (start + n - k) % n }]).find(|&id| !self.nodes.is_within(id, host));
        match outside {
            Some(target) => Some(self.tab_to(target, forward)),
            // 只有这段文字能拿焦点：绕回来
            None => {
                let &stop = if forward { stops.first() } else { stops.last() }?;
                Some(self.focus_stop(host, stop, forward))
            }
        }
    }

    /// 用 Tab 从外面走进这样的文字：落在第一个（往回走时最后一个）停留的地方
    pub(super) fn enter_inline_stops(&mut self, target: NodeId, forward: bool) -> Option<FocusCallbacks> {
        if self.inlines.is_empty() {
            return None;
        }
        let host = self.inline_host_of(target)?;
        let stops = self.inline_stops(host)?;
        let &stop = if forward { stops.first() } else { stops.last() }?;
        Some(self.focus_stop(host, stop, forward))
    }
}
