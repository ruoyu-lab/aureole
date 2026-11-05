//! 富文本里的链接：悬停（手指形状、下划线、悬停色）、点击、键盘焦点（Tab 在一段文字的链接之间走，
//! 回车、空格打开）、焦点环。
//!
//! 链接不是元素：整段文字是一个元素、一次排版，链接是里面的几段，按排版里的位置判断指针在不在上面。
//! 焦点在链接上时，树的焦点是这段文字，文字记着是第几个链接。无障碍树里的链接节点见 `accessibility.rs`。

use scene::{DisplayList, Quad};

use super::{FocusCallbacks, NodeId, Tree, TreeRef, bind};
use crate::geometry::Affine;
use crate::handlers::{EventType, Handler};
use crate::element::Element;
use crate::property::Prop;
use crate::rich_text::{LinkAction, Span};
use crate::geometry::Rect;
use crate::text::{PLACEHOLDER, Rich, RichSpan, TextBlock};

/// 挂载富文本的各段：不绑定的值直接放进去；绑定的（文字、字号、字重、颜色）各建一个副作用，变了只改这一段
/// （文字、字号、字重重新排版，颜色只重画）。返回行内元素（第几段、元素），由挂载子元素的地方挂载
pub(super) fn mount_spans(tree: &TreeRef, id: NodeId, spans: Vec<Span>) -> Vec<(usize, Element)> {
    /// 静态值拿出来，绑定原样留着
    fn split<T>(prop: Option<Prop<T>>) -> (Option<T>, Option<Prop<T>>) {
        match prop {
            Some(Prop::Static(value)) => (Some(value), None),
            dynamic => (None, dynamic),
        }
    }
    let mut parts = Vec::with_capacity(spans.len());
    let mut bound = Vec::new();
    let mut inlines = Vec::new();
    for (i, span) in spans.into_iter().enumerate() {
        let Span { content, size, weight, color, mut style, link, inline } = span;
        // 行内元素：这一段只有占位字符，文字的样式、链接不起作用
        if let Some(element) = inline {
            inlines.push((i, *element));
            parts.push(RichSpan { text: PLACEHOLDER.to_owned(), style: Default::default(), color: None, link: None });
            continue;
        }
        let (text, content) = split(Some(content));
        let (size, size_bound) = split(size);
        let (weight, weight_bound) = split(weight);
        let (color, color_bound) = split(color);
        style.size = size;
        style.weight = weight;
        parts.push(RichSpan { text: text.unwrap_or_default(), style, color, link });
        bound.push((i, content, size_bound, weight_bound, color_bound));
    }
    {
        let mut t = tree.borrow_mut();
        if let Some(block) = t.nodes.kind_mut::<TextBlock>(id) {
            block.set_rich(Rich::new(parts));
        }
    }
    for (i, content, size, weight, color) in bound {
        bind(tree, content, move |t, v| t.update_span(id, i, |s| s.text = v));
        bind(tree, size, move |t, v| t.update_span(id, i, |s| s.style.size = Some(v)));
        bind(tree, weight, move |t, v| t.update_span(id, i, |s| s.style.weight = Some(v)));
        bind(tree, color, move |t, v| t.update_span(id, i, |s| s.color = Some(v)));
    }
    inlines
}

impl Tree {
    pub(super) fn text_block(&self, id: NodeId) -> Option<&TextBlock> {
        self.nodes.kind(id)
    }

    pub(super) fn text_block_mut(&mut self, id: NodeId) -> Option<&mut TextBlock> {
        self.nodes.kind_mut(id)
    }

    /// 改富文本的第 `i` 段（绑定的值变了）：文字、样式变了重新排版，颜色变了只重画
    pub(super) fn update_span(&mut self, id: NodeId, i: usize, f: impl FnOnce(&mut RichSpan)) {
        let Some(layout) = self.nodes.get(id).map(|node| node.layout) else { return };
        let Some(block) = self.nodes.kind_mut::<TextBlock>(id) else { return };
        let (relayout, repaint) = block.update_span(i, f);
        if relayout {
            self.layout.mark_dirty(layout);
            self.accessibility.touch(id);
        }
        if repaint {
            self.paint.invalidate();
        }
    }

    /// 元素里的文字有几个链接（不是文字时是 0）
    pub(super) fn links(&self, id: NodeId) -> usize {
        self.text_block(id).and_then(TextBlock::rich).map_or(0, |rich| rich.links())
    }

    /// 文字的原点（内容区左上角）相对元素左上角的位置
    pub(super) fn text_inset(&self, id: NodeId) -> [f32; 2] {
        let Some(node) = self.nodes.get(id) else { return [0.0, 0.0] };
        self.layout.try_get(node.layout).map_or([0.0, 0.0], |l| [l.padding.left + l.border.left, l.padding.top + l.border.top])
    }

    /// 窗口坐标换算到文字里（相对文字原点）
    pub(super) fn text_point(&self, id: NodeId, window: [f32; 2]) -> Option<[f32; 2]> {
        let node = self.nodes.get(id)?;
        let local = node.world_invert(window)?;
        let inset = self.text_inset(id);
        Some([local[0] - node.bounds.x - inset[0], local[1] - node.bounds.y - inset[1]])
    }

    /// 改链接的状态（悬停、按着、焦点），变了就重画
    fn set_link_state(&mut self, id: NodeId, f: impl FnOnce(&mut Rich)) {
        if let Some(rich) = self.text_block_mut(id).and_then(TextBlock::rich_mut) {
            let before = (rich.hovered, rich.pressed, rich.focused);
            f(rich);
            if (rich.hovered, rich.pressed, rich.focused) != before {
                self.paint.invalidate();
            }
        }
    }

    /// 指针移动、布局变了之后：找指针下的链接（指针下最内层的元素是有链接的文字时），更新悬停的样子
    pub(super) fn hover_link(&mut self) {
        let found = match (self.router.pointer(), self.router.hovered().last().copied()) {
            (Some(point), Some(id)) if self.links(id) > 0 && !self.disabled(id) => self
                .text_point(id, point)
                .and_then(|p| self.text_block(id)?.link_at(p))
                .map(|k| (id, k)),
            _ => None,
        };
        if found == self.router.link_hover() {
            return;
        }
        if let Some((id, _)) = self.router.link_hover() {
            self.set_link_state(id, |rich| rich.hovered = None);
        }
        if let Some((id, k)) = found {
            self.set_link_state(id, |rich| rich.hovered = Some(k));
        }
        self.router.set_link_hover(found);
    }

    /// 指针在这个元素的链接上（指针是手指形状）
    pub(super) fn over_link(&self, id: NodeId) -> bool {
        self.router.link_hover().is_some_and(|(n, _)| n == id)
    }

    /// 按下：指针在链接上就记下来（松开时还在同一个链接上才算点击），焦点给这段文字、落在这个链接上（不画焦点环）。
    /// 返回链接所在的文字
    pub(super) fn press_link(&mut self) -> Option<NodeId> {
        let Some((id, k)) = self.router.link_hover() else {
            // 按在可选中的富文本的其他字上：焦点还在这段文字上，但不在链接上了
            if let Some(id) = self.router.focused() {
                self.set_link_state(id, |rich| rich.focused = None);
            }
            return None;
        };
        self.router.set_link_press(Some((id, k)));
        self.set_link_state(id, |rich| {
            rich.pressed = Some(k);
            rich.focused = Some(k);
        });
        Some(id)
    }

    /// 松开：还在按下的那个链接上，并且没有拖出选区（可选中的文字），返回要做的事
    pub(super) fn release_link(&mut self) -> Option<LinkAction> {
        let (id, k) = self.router.take_link_press()?;
        self.set_link_state(id, |rich| rich.pressed = None);
        if self.router.link_hover() != Some((id, k)) || self.disabled(id) {
            return None;
        }
        let block = self.text_block(id)?;
        if block.has_selection() {
            return None;
        }
        self.link_action(id, k)
    }

    /// 第 `k` 个链接被点击时做什么
    pub(super) fn link_action(&self, id: NodeId, k: usize) -> Option<LinkAction> {
        let rich = self.text_block(id)?.rich()?;
        let (_, link) = rich.link(k)?;
        LinkAction::new(link, self.handlers_of(id, EventType::LinkClick, Handler::text))
    }

    /// 焦点所在的链接
    pub(super) fn focused_link(&self) -> Option<(NodeId, usize)> {
        let id = self.router.focused()?;
        Some((id, self.text_block(id)?.rich()?.focused?))
    }

    /// 回车、空格：焦点在链接上时打开它
    pub(super) fn focused_link_action(&self) -> Option<LinkAction> {
        let (id, k) = self.focused_link()?;
        if self.disabled(id) {
            return None;
        }
        self.link_action(id, k)
    }

    /// Tab：焦点在有链接的文字上时，先在它的链接之间走（焦点在文字上、不在链接上时往后走到第一个）。
    /// 走出这段文字时返回 `None`，交给普通的焦点切换
    pub(super) fn step_link(&mut self, forward: bool) -> Option<FocusCallbacks> {
        let id = self.router.focused()?;
        let n = self.links(id);
        if n == 0 {
            return None;
        }
        let current = self.text_block(id)?.rich()?.focused;
        let next = match (current, forward) {
            (None, true) => 0,
            (Some(k), true) if k + 1 < n => k + 1,
            (Some(k), false) if k > 0 => k - 1,
            _ => return None,
        };
        self.set_link_state(id, |rich| rich.focused = Some(next));
        self.accessibility.touch(id);
        Some(self.focus_to(Some(id), true))
    }

    /// 焦点刚换到有链接的文字上（Tab、Shift+Tab、辅助技术）：落在第一个（往回走时最后一个）链接上
    pub(super) fn enter_links(&mut self, id: NodeId, forward: bool, link: Option<usize>) {
        let n = self.links(id);
        if n == 0 {
            return;
        }
        let k = link.unwrap_or(if forward { 0 } else { n - 1 }).min(n - 1);
        self.set_link_state(id, |rich| rich.focused = Some(k));
    }

    /// 键盘焦点在第 `k` 个链接上：画焦点环（每行一个，紧贴着字），代替整个元素的焦点环
    pub(super) fn paint_link_ring(&self, id: NodeId, k: usize, layer: u32, clip: u32, local: &Affine, opacity: f32, list: &mut DisplayList) {
        let Some(node) = self.nodes.get(id) else { return };
        let Some(block) = self.text_block(id) else { return };
        let ring = node.input.as_ref().and_then(|i| i.ring).unwrap_or_default();
        if !ring.visible() {
            return;
        }
        let inset = self.text_inset(id);
        let origin = [node.bounds.x + inset[0], node.bounds.y + inset[1]];
        let grow = ring.offset.min(2.0) + ring.width;
        for b in block.link_boxes(k, true) {
            let rect = Rect { x: origin[0] + b.x - grow, y: origin[1] + b.y - grow, width: b.width + grow * 2.0, height: b.height + grow * 2.0 };
            let [cx, cy] = local.apply(rect.center());
            let mut quad = Quad::new(cx - rect.width * 0.5, cy - rect.height * 0.5, rect.width, rect.height)
                .radius(ring.radius.unwrap_or(4.0 + grow))
                .fill(scene::Color::TRANSPARENT)
                .border(ring.width, ring.color)
                .opacity(opacity);
            quad.transform = local.linear;
            quad.layer = layer;
            quad.clip = clip;
            list.quads.push(quad);
        }
    }

    /// 测试工具：命名的文字里第 `k` 个链接第一行的位置（窗口坐标）
    pub(crate) fn link_bounds(&self, name: &str, k: usize) -> Option<Rect> {
        let id = self.nodes.named(name)?;
        let node = &self.nodes[id];
        let b = *self.text_block(id)?.link_boxes(k, false).first()?;
        let inset = self.text_inset(id);
        let [x0, y0] = node.world().apply([node.bounds.x + inset[0] + b.x, node.bounds.y + inset[1] + b.y]);
        let [x1, y1] = node.world().apply([node.bounds.x + inset[0] + b.x + b.width, node.bounds.y + inset[1] + b.y + b.height]);
        Some(Rect { x: x0.min(x1), y: y0.min(y1), width: (x1 - x0).abs(), height: (y1 - y0).abs() })
    }
}
