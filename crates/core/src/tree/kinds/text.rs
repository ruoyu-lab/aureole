//! 文字和富文本（[`crate::text()`]、[`crate::rich_text`]）是一种元素种类：按内容区的可用宽度断行，把基线报给布局，
//! 继承文字样式，画行内代码的底、选区和字形。可选中的、有链接的文字是输入目标（[`InputTarget`]）：按下拿焦点、
//! 拖着选字、⌘C 复制、⌘A 全选。链接的悬停和点击、绕排、选择根（跨元素的选择）见 `tree/links.rs`、`tree/flow.rs`、
//! `tree/selection.rs`。

use crate::event::{KeyEvent, PointerEvent, PointerEventKind};
use crate::geometry::Rect;
use crate::input::Key;
use crate::interaction::Cursor;
use accesskit::{Action, Role};

use crate::kinds::{
    AccessibilityContext, AccessibilityProvider, AvailableSpace, ElementKind, Focusable, Handled, InputTarget, KindInfo, MeasureContext,
    PaintContext, StyleContext, SubTarget, TargetContext,
};
use crate::property::registry::{Dirty, KindMask};
use crate::text::TextBlock;

impl ElementKind for TextBlock {
    fn name(&self) -> &'static str {
        "text"
    }

    fn kind_mask(&self) -> KindMask {
        KindMask::TEXT
    }

    /// 按内容区的可用宽度断行：`available` 已经算上了已知宽度、样式宽度、最小最大宽度，并去掉了内边距和边框
    /// （`known` 是这个元素定下的大小，不能直接拿来断行）。网格求轨道尺寸时会用最小内容（断在每个词）和
    /// 最大内容（不断行）来量，换行的弹性布局用确定的宽度来量
    fn measure(&mut self, cx: &mut MeasureContext, _: [Option<f32>; 2], available: [AvailableSpace; 2]) -> [f32; 2] {
        let max_width = match available[0] {
            AvailableSpace::Definite(w) => Some(w.max(0.0)),
            AvailableSpace::MinContent => Some(0.0),
            AvailableSpace::MaxContent => None,
        };
        TextBlock::measure(self, cx.text, max_width)
    }

    /// 文字的基线（相对内容区顶部），基线对齐时用
    fn baselines(&self) -> Option<(f32, f32)> {
        TextBlock::baselines(self)
    }

    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        let mut dirty = Dirty::empty();
        if std::mem::replace(&mut self.selection_color, cx.selection) != cx.selection {
            dirty |= Dirty::PAINT;
        }
        if self.update(|_, s| *s = cx.style) {
            dirty |= Dirty::LAYOUT;
        }
        dirty
    }

    fn refresh_fonts(&mut self) -> Dirty {
        self.invalidate();
        Dirty::LAYOUT
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        let (area, ink, id) = (*cx.area, cx.ink, cx.id);
        let (content, local, opacity) = (area.content, area.local, area.opacity);
        let tree = &mut *cx.tree;
        let list = &mut *cx.list;
        let origin = [content.x, content.y];
        self.layout_at(&mut tree.text, content.width);
        let theme = &tree.paint.colors().rich;
        // 行内代码的底、选区画在字的下面。没有焦点时选区还在，画成不活跃的颜色
        for (rect, size) in self.code_boxes() {
            list.quads.push(crate::editor::code_quad(origin, rect, size, &local, theme.code, opacity));
        }
        let active = tree.router.focused() == Some(id) || (self.selection_root.is_some() && self.selection_root == area.focused_selection_root);
        let fill = if active { self.selection_color } else { theme.inactive };
        self.set_inline_selection_fill(fill);
        for &rect in self.selection_rects() {
            list.quads.push(crate::editor::selection_quad(origin, rect, &local, fill, opacity));
        }
        let inks = self.inks(theme.links);
        self.paint_glyphs(&mut tree.text, content.width, origin, &local, ink, &inks, &mut list.quads);
    }

    fn input_target(&self) -> Option<&dyn InputTarget> {
        Some(self)
    }

    fn input_target_mut(&mut self) -> Option<&mut dyn InputTarget> {
        Some(self)
    }

    fn accessibility_provider(&self) -> Option<&dyn AccessibilityProvider> {
        Some(self)
    }

    fn describe(&self, info: &mut KindInfo) {
        info.summary = Some(preview(self.content()));
        info.text = Some(self.content().to_owned());
        if self.selectable {
            info.fields.push(("selectable", "true".to_owned()));
        }
        if self.max_lines() > 0 {
            info.fields.push(("max_lines", self.max_lines().to_string()));
        }
    }
}

/// 可选中的文字：按下拿焦点（Tab 走不到）、拖着选字、⌘C、⌘A；有链接的文字：Tab 在链接之间走，
/// 不能选中时只有按在链接上才拿焦点
impl InputTarget for TextBlock {
    fn hit(&self, position: [f32; 2], content: Rect) -> Option<SubTarget> {
        let point = [position[0] - content.x, position[1] - content.y];
        match self.link_at(point) {
            Some(k) => Some(SubTarget::Part(k)),
            None => self.selectable.then_some(SubTarget::Whole),
        }
    }

    fn focusable(&self) -> Focusable {
        if self.rich().is_some_and(|rich| rich.links() > 0) {
            Focusable::Keyboard
        } else if self.selectable {
            Focusable::Pointer
        } else {
            Focusable::No
        }
    }

    fn on_pointer(&mut self, event: &PointerEvent, cx: &mut TargetContext) -> Handled {
        if !self.selectable {
            return Handled::No;
        }
        let content = cx.content_rect();
        let point = [event.position[0] - content.x, event.position[1] - content.y];
        match event.kind {
            PointerEventKind::Down => self.press(point, event.clicks.clamp(1, 3), event.modifiers.has_shift()),
            PointerEventKind::Move => self.drag(point),
            _ => return Handled::No,
        }
        cx.dirty |= Dirty::PAINT | Dirty::ACCESSIBILITY;
        Handled::Yes
    }

    /// 能选中的文字：⌘C 复制选中的，⌘A 选中整段；别的键不处理
    fn on_key_down(&mut self, event: &KeyEvent, cx: &mut TargetContext) -> Handled {
        let Key::Character(c) = &event.key else { return Handled::No };
        if !self.selectable || !event.has_primary_modifier() {
            return Handled::No;
        }
        if c.eq_ignore_ascii_case("c") {
            if let Some(selected) = self.selected_text() {
                crate::system::clipboard::write_text(selected);
            }
            Handled::Yes
        } else if c.eq_ignore_ascii_case("a") {
            self.select_all();
            cx.request_paint();
            cx.request_accessibility();
            Handled::Yes
        } else {
            Handled::No
        }
    }

    /// 失去焦点（点了别处）时选区还在，画成不活跃的颜色；不再记着焦点在哪个链接上
    fn on_focus(&mut self, focused: bool, cx: &mut TargetContext) {
        if !focused
            && let Some(rich) = self.rich_mut()
            && (rich.focused.take().is_some() | rich.pressed.take().is_some())
        {
            cx.request_paint();
        }
    }

    fn cursor(&self, _: [f32; 2], _: Rect) -> Option<Cursor> {
        self.selectable.then_some(Cursor::Text)
    }
}

/// 富文本里有链接或者行内元素：给辅助技术的是一个段落，文字分成几段子节点
fn has_parts(rich: &crate::text::Rich) -> bool {
    rich.links() > 0 || !rich.inlines().is_empty()
}

/// 富文本里第 `k` 个链接的虚拟子节点编号（链接之间的文字是 0、1、2……）
pub(crate) const LINK_KEY: u32 = 1 << 31;

/// 文字给辅助技术的：普通文字是一个标签，值是全部文字；有链接的富文本是一个段落，子节点按顺序是链接之间的文字和链接
/// （名字是链接的文字，带网址，能点击、能获得焦点，边界是它从第一行到最后一行占的范围）。点击、聚焦链接由元素树处理
impl AccessibilityProvider for TextBlock {
    fn role(&self) -> Option<Role> {
        Some(if self.rich().is_some_and(has_parts) { Role::Paragraph } else { Role::Label })
    }

    fn write_node(&self, node: &mut accesskit::Node, _: &AccessibilityContext) {
        if !self.rich().is_some_and(has_parts) {
            node.set_value(self.content());
        }
    }

    fn virtual_children(&self, cx: &mut AccessibilityContext) {
        let Some(rich) = self.rich().filter(|rich| has_parts(rich)) else { return };
        let inset = cx.content_rect();
        let disabled = cx.is_disabled();
        let mut pending = String::new();
        let (mut segment, mut k) = (0u32, 0usize);
        let flush = |pending: &mut String, segment: &mut u32, cx: &mut AccessibilityContext| {
            if !pending.is_empty() {
                let text = std::mem::take(pending);
                cx.push_child(*segment, super::text_runs::stamp(&text, false, [0.0; 4]), || {
                    let mut node = accesskit::Node::new(Role::Label);
                    node.set_value(text.as_str());
                    node
                });
                *segment += 1;
            }
        };
        for (i, span) in rich.spans.iter().enumerate() {
            // 行内元素是真的子节点：记下它排在这里，前后的文字分开
            if let Some(node) = rich.inline_of_span(i) {
                flush(&mut pending, &mut segment, cx);
                cx.inline_order.push((node, cx.children.len()));
                continue;
            }
            let Some(link) = &span.link else {
                pending.push_str(&span.text);
                continue;
            };
            flush(&mut pending, &mut segment, cx);
            let boxes = self.link_boxes(k, false);
            let bounds = boxes.first().map(|first| {
                let x0 = boxes.iter().map(|b| b.x).fold(first.x, f32::min) + inset.x;
                let y0 = first.y + inset.y;
                let x1 = boxes.iter().map(|b| b.x + b.width).fold(first.x + first.width, f32::max) + inset.x;
                let y1 = boxes.iter().map(|b| b.y + b.height).fold(first.y + first.height, f32::max) + inset.y;
                [x0, y0, x1, y1]
            });
            let url = link.url.as_deref();
            let stamp = super::text_runs::stamp(&format!("{}\u{0}{}\u{0}{disabled}", span.text, url.unwrap_or_default()), true, bounds.unwrap_or_default());
            cx.push_child(LINK_KEY | k as u32, stamp, || {
                let mut node = accesskit::Node::new(Role::Link);
                node.set_label(span.text.as_str());
                if let Some(url) = url {
                    node.set_url(url);
                }
                if let Some([x0, y0, x1, y1]) = bounds {
                    node.set_bounds(accesskit::Rect::new(x0 as f64, y0 as f64, x1 as f64, y1 as f64));
                }
                if !disabled {
                    node.add_action(Action::Click);
                    node.add_action(Action::Focus);
                }
                node
            });
            k += 1;
        }
        flush(&mut pending, &mut segment, cx);
    }
}

/// 文字内容的预览：最多 32 个字，换行显示成 ⏎
fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(32).map(|c| if c == '\n' { '⏎' } else { c }).collect();
    if text.chars().nth(32).is_some() {
        out.push('…');
    }
    out
}
