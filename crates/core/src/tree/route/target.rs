//! 输入目标（[`crate::kinds::InputTarget`]）在元素树里的部分：按下时谁拿焦点、选字，把指针、按键交给有焦点的目标，
//! 焦点变化通知它们，指针形状，Tab 走不走得到。元素树只按接口调用，不认具体的种类：输入框、可选中的文字、字符网格、
//! 画布和自己定义的元素走同一条路。
//!
//! 种类对象留在节点上（不像绘制那样取下来）：上下文只借文字系统和这个元素的几何，调用完按它的要求重画、重新量、
//! 重建无障碍节点（[`Tree::with_kind`]）。
//!
//! 选择根（[`crate::Element::selection_root`]）里的文字由选择根接管选字、⌘C、⌘A（跨好几段文字，见 `tree/selection.rs`），
//! 不交给文字自己。按着 Shift 的移动键扩展选区也在那里，单独一段可选中的文字走同一套。

use super::super::{Edited, NodeId, Tree};
use crate::editor::motion::Motion;
use crate::editor::{EditReport, PointerAction};
use crate::event::{KeyEvent, PointerEventKind};
use crate::geometry::Rect;
use crate::input::{Key, MouseButton};
use crate::interaction::Cursor;
use crate::kinds::{ElementKind, Focusable, InputTarget, TargetContext};
use crate::property::registry::KindMask;

/// 调用完种类对象之后元素树要做的：交出焦点、输入框改了什么
#[derive(Default)]
pub(in crate::tree) struct TargetOutcome {
    pub release_focus: bool,
    pub edit: EditReport,
}

impl Tree {
    /// 元素的几何：左上角的布局坐标、大小、内容区（元素坐标）
    pub(in crate::tree) fn target_geometry(&self, id: NodeId) -> Option<([f32; 2], [f32; 2], Rect)> {
        let node = self.nodes.get(id)?;
        let b = node.bounds;
        let content = match self.layout.try_get(node.layout) {
            Some(l) => Rect {
                x: l.padding.left + l.border.left,
                y: l.padding.top + l.border.top,
                width: l.content_box_width(),
                height: l.content_box_height(),
            },
            None => Rect { x: 0.0, y: 0.0, width: b.width, height: b.height },
        };
        Some(([b.x, b.y], [b.width, b.height], content))
    }

    /// 借着树调用元素的种类对象（输入目标、文字输入客户端）：`f` 返回 `None` 表示种类没有这种能力。
    /// 调用完按上下文里的要求重画、重新量、重建无障碍节点
    pub(in crate::tree) fn with_kind<R>(
        &mut self,
        id: NodeId,
        f: impl FnOnce(&mut dyn ElementKind, &mut TargetContext) -> Option<R>,
    ) -> Option<(R, TargetOutcome)> {
        let (origin, size, content) = self.target_geometry(id)?;
        let focused = self.router.focused() == Some(id);
        // 宿主时刻：插入点闪烁、撤销的合并不跟动画时钟走
        let seconds = self.anim.host();
        let kind = self.nodes.element_kind_mut(id)?;
        let mut cx = TargetContext::new(&mut self.text, seconds, origin, size, content, focused);
        let result = f(&mut **kind, &mut cx);
        let TargetContext { dirty, release_focus, edit, .. } = cx;
        if !dirty.is_empty() {
            self.apply_dirty(id, dirty);
        }
        Some((result?, TargetOutcome { release_focus, edit }))
    }

    /// 元素的输入目标（种类实现了的）
    pub(in crate::tree) fn input_target(&self, id: NodeId) -> Option<&dyn InputTarget> {
        self.nodes.element_kind(id)?.input_target()
    }

    /// 输入目标说的能不能获得焦点；没有输入目标的是 `No`
    pub(in crate::tree) fn target_focusable(&self, id: NodeId) -> Focusable {
        self.input_target(id).map_or(Focusable::No, |t| t.focusable())
    }

    /// 选择根里的文字：选择根接管它的选字、⌘C、⌘A
    pub(in crate::tree) fn in_selection_root(&self, id: NodeId) -> bool {
        self.nodes.element_kind(id).is_some_and(|k| k.kind_mask().contains(KindMask::TEXT)) && self.selection_root_of(id).is_some()
    }

    /// 自己就是文字的焦点目标：能获得焦点的输入目标（输入框、可选中的文字、字符网格……），或者选择根里的文字。
    /// 按键先交给它，指针按下时由它放置插入点、选择；回车、空格不在它上面合成点击
    pub(in crate::tree) fn is_text(&self, id: NodeId) -> bool {
        self.target_focusable(id) != Focusable::No || self.in_selection_root(id)
    }

    /// 有焦点时要接文字输入（输入框）：它自己处理点击（不往外传）、方向键（不滚外面的视口）
    pub(in crate::tree) fn wants_text_input(&self, id: NodeId) -> bool {
        self.input_target(id).is_some_and(|t| t.wants_text_input())
    }

    /// 按在 `id` 上（窗口坐标 `window`；不知道位置时 `None`，比如辅助技术的点击）时谁拿焦点、选字：
    /// 它自己（能获得焦点的输入目标、按在它的子目标上；选择根里的文字）
    pub(in crate::tree) fn press_target(&self, id: NodeId, window: Option<[f32; 2]>) -> Option<NodeId> {
        if self.in_selection_root(id) {
            return Some(id);
        }
        if let Some(target) = self.input_target(id)
            && target.focusable() != Focusable::No
        {
            let hit = match window {
                Some(window) => {
                    let (_, _, content) = self.target_geometry(id)?;
                    target.hit(self.local_point(id, window), content).is_some()
                }
                None => true,
            };
            return hit.then_some(id);
        }
        None
    }

    /// 输入目标在指针下给的形状（输入框、可选中的文字、字符网格是文字形状的指针）
    pub(in crate::tree) fn target_cursor(&self, id: NodeId, window: [f32; 2]) -> Option<Cursor> {
        if let Some(target) = self.input_target(id) {
            let (_, _, content) = self.target_geometry(id)?;
            if let Some(cursor) = target.cursor(self.local_point(id, window), content) {
                return Some(cursor);
            }
        }
        self.in_selection_root(id).then_some(Cursor::Text)
    }

    /// 有焦点的输入目标接指针：按下（`clicks` 是连击数）、拖动、松开，`window` 是窗口坐标。
    /// 选择根里的文字交给选择根。返回接没接
    pub(in crate::tree) fn target_pointer(&mut self, id: NodeId, kind: PointerEventKind, window: [f32; 2], clicks: u32) -> bool {
        if self.in_selection_root(id)
            && let Some(scope) = self.selection_root_of(id)
        {
            let action = match kind {
                PointerEventKind::Down => PointerAction::Press { clicks: clicks.clamp(1, 3), extend: self.router.modifiers().has_shift() },
                PointerEventKind::Move => PointerAction::Drag,
                _ => return true,
            };
            self.selection_root_pointer(scope, id, window, action);
            return true;
        }
        let button = matches!(kind, PointerEventKind::Down | PointerEventKind::Up).then_some(MouseButton::Left);
        let event = self.pointer_event(id, kind, window, button, clicks);
        self.with_kind(id, |kind, cx| Some(kind.input_target_mut()?.on_pointer(&event, cx))).is_some_and(|(handled, _)| handled.is_handled())
    }

    /// 有焦点的输入目标处理按键；选择根里的文字的 ⌘C、⌘A 交给选择根，有选区的文字的 Shift+移动键扩展选区。
    /// 不是输入目标时是 `None`
    pub(in crate::tree) fn target_key(&mut self, id: NodeId, event: &KeyEvent) -> Option<Edited> {
        if event.modifiers.has_shift()
            && let Some(motion) = Motion::of_key(event)
            && self.extend_selection(id, motion)
        {
            return Some(Edited { node: id, handled: true, ..Default::default() });
        }
        // 复制之前：行内元素代替它的文字
        if !self.inlines.is_empty() && event.has_primary_modifier() {
            self.refresh_inline_copy();
        }
        if self.in_selection_root(id)
            && event.has_primary_modifier()
            && let Key::Character(c) = &event.key
            && let Some(copy) = [("c", true), ("a", false)].iter().find(|(k, _)| c.eq_ignore_ascii_case(k)).map(|&(_, copy)| copy)
            && let Some(handled) = self.selection_root_key(id, copy)
        {
            return Some(Edited { node: id, handled, ..Default::default() });
        }
        let (handled, outcome) = self.with_kind(id, |kind, cx| Some(kind.input_target_mut()?.on_key_down(event, cx)))?;
        Some(Edited { node: id, handled: handled.is_handled(), release_focus: outcome.release_focus, edit: outcome.edit })
    }

    /// 通知输入目标得到、失去焦点；输入框失去焦点时改完了的内容（`on_commit`）
    pub(in crate::tree) fn target_focus_changed(&mut self, id: NodeId, focused: bool) -> Option<String> {
        let (_, outcome) = self.with_kind(id, |kind, cx| {
            kind.input_target_mut()?.on_focus(focused, cx);
            Some(())
        })?;
        outcome.edit.commit
    }
}
