//! 输入框（[`crate::text_field`]、[`crate::text_area`]）是一种元素种类：单行的宽度由样式决定、高度是一行，
//! 多行的按内容区宽度断行、高度是全部内容（样式的最大高度限制它，超出时自己滚动）；有焦点时插入点闪烁、一直要帧；
//! 多行的只画看得见的那几段，画在自己的内容层里（渲染线程按滚动偏移平移这一层）。它是输入目标（[`InputTarget`]）：拿焦点、放插入点和选字、编辑键都在这里接进来；
//! 也是文字输入客户端（[`TextInputClient`]）：输入法、听写、辅助技术改文字都按编辑记进撤销。
//! 元素树只按接口调用（`tree/route/target.rs`、`tree/route/compose.rs`）。

use std::ops::Range;

use crate::compositor::{LayerInfo, LayerValues};
use crate::editor::{EditOutcome, EditorBlock, PointerAction};
use crate::geometry::Affine;
use crate::scroll::own_content_key;
use crate::text::Ink;
use crate::event::{KeyEvent, PointerEvent, PointerEventKind};
use crate::geometry::Rect;
use crate::input::ImePurpose;
use crate::interaction::Cursor;
use accesskit::{Action, ActionData, Role, TextPosition, TextSelection};

use super::text_runs::{cluster_index, cluster_offset, stamp, text_run};
use crate::editor::AccessibleLine;
use crate::kinds::{
    AccessibilityAction, AccessibilityContext, AccessibilityProvider, AvailableSpace, ElementKind, Focusable, Handled, InputTarget, KindInfo,
    MeasureContext, NextFrame, PaintContext, StyleContext, SubTarget, TargetContext, TextInputClient, TickContext,
};
use crate::property::registry::{Dirty, KindMask};

impl ElementKind for EditorBlock {
    fn name(&self) -> &'static str {
        if self.multiline() { "text_area" } else { "text_field" }
    }

    /// 多行的自己带滚动：滚动条的颜色这类滚动视口的属性也适用
    fn kind_mask(&self) -> KindMask {
        if self.multiline() { KindMask::EDITOR | KindMask::SCROLL } else { KindMask::EDITOR }
    }

    fn measure(&mut self, cx: &mut MeasureContext, known: [Option<f32>; 2], available: [AvailableSpace; 2]) -> [f32; 2] {
        if self.multiline() {
            // 按内容区宽度断行，高度是全部内容
            let width = match available[0] {
                AvailableSpace::Definite(w) => Some(w.max(0.0)),
                AvailableSpace::MinContent | AvailableSpace::MaxContent => known[0],
            };
            EditorBlock::measure(self, cx.text, width)
        } else {
            // 宽度由样式决定（不随内容变），高度是一行
            [known[0].unwrap_or(0.0), known[1].unwrap_or(self.line_height())]
        }
    }

    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        let mut dirty = Dirty::empty();
        if self.set_colors(cx.selection, cx.inactive, cx.placeholder) {
            dirty |= Dirty::PAINT;
        }
        if self.update_style(|_, s| *s = cx.style) {
            dirty |= Dirty::LAYOUT;
        }
        dirty
    }

    fn refresh_fonts(&mut self) -> Dirty {
        self.invalidate();
        Dirty::LAYOUT
    }

    /// 有焦点时才调用：插入点闪烁。第一次换亮灭说明停下来半秒了：打字时没排完的长段这时排完，高度可能变
    fn tick(&mut self, cx: &TickContext) -> Dirty {
        if !self.blink(cx.seconds) {
            return Dirty::empty();
        }
        if self.stop_typing() { Dirty::PAINT | Dirty::LAYOUT | Dirty::ACCESSIBILITY } else { Dirty::PAINT }
    }

    /// 有焦点时才问：插入点下一次换亮灭的那一刻要一帧（窗口在那之前睡着，每秒只醒两次左右）
    fn next_frame(&self, now: std::time::Duration) -> NextFrame {
        self.next_blink(now.as_secs_f64()).map_or(NextFrame::Idle, |at| NextFrame::At(std::time::Duration::from_secs_f64(at)))
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        let (area, ink) = (*cx.area, cx.ink);
        let Some(viewport) = cx.tree.editor_viewport(cx.id) else {
            EditorBlock::paint(self, &mut cx.tree.text, area.bounds, area.content, &area.local, ink, None, [0, 0], &mut cx.list.quads);
            return;
        };
        // 多行：只画看得见的那几段，画在自己的内容层里（层里从布局坐标重新开始，透明度由层带着），
        // 渲染线程按滚动偏移平移这一层：主线程卡住时也接着滚；裁剪用元素自己的
        cx.layers.push(LayerInfo {
            key: own_content_key(cx.id),
            parent: area.layer,
            base: area.local,
            base_opacity: area.opacity,
            center: area.bounds.center(),
            values: LayerValues { translate: [0.0; 2], scale: 1.0, rotation: 0.0, opacity: 1.0, offset: [0.0; 2] },
            group: false,
            depth: None,
        });
        let layer = cx.layers.len() as u32;
        let start = cx.list.quads.len();
        let ink = Ink { opacity: 1.0, ..ink };
        // 层号和裁剪在生成、拷贝图元时一起设好，不用再把所有图元走一遍
        let place = [layer, area.inner];
        EditorBlock::paint(self, &mut cx.tree.text, area.bounds, area.content, &Affine::IDENTITY, ink, Some(viewport), place, &mut cx.list.quads);
        cx.placed = Some(start);
    }

    fn input_target(&self) -> Option<&dyn InputTarget> {
        Some(self)
    }

    fn input_target_mut(&mut self) -> Option<&mut dyn InputTarget> {
        Some(self)
    }

    fn text_input_client(&self) -> Option<&dyn TextInputClient> {
        Some(self)
    }

    fn text_input_client_mut(&mut self) -> Option<&mut dyn TextInputClient> {
        Some(self)
    }

    fn accessibility_provider(&self) -> Option<&dyn AccessibilityProvider> {
        Some(self)
    }

    fn accessibility_provider_mut(&mut self) -> Option<&mut dyn AccessibilityProvider> {
        Some(self)
    }

    fn describe(&self, info: &mut KindInfo) {
        info.text = Some(self.text());
        if self.password() {
            info.fields.push(("password", "true".to_owned()));
        }
        let placeholder = self.placeholder_text();
        if !placeholder.is_empty() {
            info.fields.push(("placeholder", placeholder.to_owned()));
        }
    }
}

/// 输入框：按下、Tab 都能拿焦点；按下放插入点、拖着选字；编辑键、打字、剪贴板、撤销
impl InputTarget for EditorBlock {
    fn hit(&self, _: [f32; 2], _: Rect) -> Option<SubTarget> {
        Some(SubTarget::Whole)
    }

    fn focusable(&self) -> Focusable {
        Focusable::Keyboard
    }

    fn on_pointer(&mut self, event: &PointerEvent, cx: &mut TargetContext) -> Handled {
        let action = match event.kind {
            PointerEventKind::Down => PointerAction::Press { clicks: event.clicks.clamp(1, 3), extend: event.modifiers.has_shift() },
            PointerEventKind::Move => PointerAction::Drag,
            _ => return Handled::No,
        };
        // 输入框按布局坐标记文字的原点
        let point = [event.position[0] + cx.origin[0], event.position[1] + cx.origin[1]];
        self.pointer(cx.text, point, action, cx.seconds);
        cx.dirty |= Dirty::PAINT | Dirty::ACCESSIBILITY;
        Handled::Yes
    }

    /// 单行输入框的内容比框长：手指横着拖时滚内容（多行输入框是滚动视口，走视口那一路）
    fn can_scroll_content(&self, delta: [f32; 2], content: Rect) -> bool {
        delta[0].abs() >= delta[1].abs() && self.can_scroll_x(delta[0], content.width)
    }

    fn scroll_content(&mut self, delta: [f32; 2], cx: &mut TargetContext) {
        self.scroll_x_by(delta[0], cx.content_rect().width);
        cx.dirty |= Dirty::PAINT;
    }

    fn on_key_down(&mut self, event: &KeyEvent, cx: &mut TargetContext) -> Handled {
        let outcome = self.key(cx.text, event, cx.seconds);
        self.report(outcome, cx)
    }

    /// 失去焦点时内容和获得焦点时不一样：改完了（`on_commit`）
    fn on_focus(&mut self, focused: bool, cx: &mut TargetContext) {
        // 打字时没排完的长段排完（失去焦点后不再有闪烁的时钟）
        if self.settle(cx.text) {
            cx.dirty |= Dirty::LAYOUT;
        }
        self.set_focused(focused, cx.seconds);
        if !focused {
            cx.edit.commit = self.take_commit();
        }
    }

    fn cursor(&self, _: [f32; 2], _: Rect) -> Option<Cursor> {
        Some(Cursor::Text)
    }

    fn wants_text_input(&self) -> bool {
        true
    }
}

/// 输入框：输入法的预编辑放进文档、加下划线（不写回信号）；提交、听写、辅助技术改值都按编辑记一步撤销。
/// 密码不开输入法（用途是密码，窗口请求系统的安全输入）
impl TextInputClient for EditorBlock {
    fn purpose(&self) -> ImePurpose {
        if self.password() { ImePurpose::Password } else { ImePurpose::Normal }
    }

    fn caret_area(&self) -> Rect {
        EditorBlock::caret_area(self)
    }

    fn surrounding(&self) -> Option<(String, Range<usize>)> {
        EditorBlock::surrounding(self)
    }

    fn set_preedit(&mut self, text: &str, cursor: Option<Range<usize>>, cx: &mut TargetContext) {
        let outcome = self.preedit(cx.text, text, cursor, cx.seconds);
        self.report(outcome, cx);
    }

    fn commit(&mut self, text: &str, cx: &mut TargetContext) {
        let outcome = self.commit_text(cx.text, text, cx.seconds);
        self.report(outcome, cx);
    }

    fn delete_surrounding(&mut self, before: usize, after: usize, cx: &mut TargetContext) {
        let outcome = self.delete_around(cx.text, before, after, cx.seconds);
        self.report(outcome, cx);
    }

    fn cancel_composition(&mut self, cx: &mut TargetContext) {
        let outcome = self.cancel_compose(cx.text, cx.seconds);
        self.report(outcome, cx);
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut TargetContext) {
        let outcome = self.replace(cx.text, range, text, cx.seconds);
        self.report(outcome, cx);
    }
}

/// 输入框给辅助技术的：角色、值、占位文字、选区；排出来的每一行一个文字片段（字长按字素簇，带每个字的位置），
/// 只重发变了的那几行。组字期间预编辑文字不算，插入点报在组字开始的地方。选字、改值、替换选中的文字按编辑记进撤销
impl AccessibilityProvider for EditorBlock {
    fn role(&self) -> Option<Role> {
        Some(match () {
            _ if self.multiline() => Role::MultilineTextInput,
            _ if self.password() => Role::PasswordInput,
            _ => Role::TextInput,
        })
    }

    fn write_node(&self, node: &mut accesskit::Node, cx: &AccessibilityContext) {
        let (text, anchor, focus) = self.accessible();
        node.set_value(text);
        let placeholder = self.placeholder_text();
        if !placeholder.is_empty() {
            node.set_placeholder(placeholder);
        }
        if !cx.is_disabled() {
            node.add_action(Action::SetValue);
            node.add_action(Action::SetTextSelection);
            node.add_action(Action::ReplaceSelectedText);
        }
        let lines = self.accessible_lines();
        if let (Some(anchor), Some(focus)) = (position(&lines, anchor, cx), position(&lines, focus, cx)) {
            node.set_text_selection(TextSelection { anchor, focus });
        }
    }

    fn virtual_children(&self, cx: &mut AccessibilityContext) {
        for (i, line) in self.accessible_lines().iter().enumerate() {
            cx.push_child(i as u32, stamp(&line.text, line.line_break, line.rect), || {
                let positions = self.cluster_positions(line);
                let mut node = text_run(&line.text, line.line_break, positions.as_ref().map(|(x, w)| (x.as_slice(), w.as_slice())));
                let [x, y, w, h] = line.rect;
                node.set_bounds(accesskit::Rect::new(x as f64, y as f64, (x + w) as f64, (y + h) as f64));
                node
            });
        }
    }

    fn action(&mut self, action: &AccessibilityAction, cx: &mut TargetContext) -> Handled {
        match (action.action, action.data) {
            (Action::SetTextSelection, Some(ActionData::SetTextSelection(selection))) => {
                let lines = self.accessible_lines();
                let offset = |p: TextPosition| {
                    let line = lines.get(action.child_key(p.node)? as usize)?;
                    Some(line.start + cluster_offset(&line.text, p.character_index))
                };
                if let (Some(anchor), Some(focus)) = (offset(selection.anchor), offset(selection.focus)) {
                    drop(lines);
                    self.select(anchor, focus);
                    cx.request_paint();
                    cx.request_accessibility();
                }
                Handled::Yes
            }
            // 替换选中的文字（听写、语音控制）、整段改值：按编辑记进撤销，改值事件的来源是辅助技术
            (Action::ReplaceSelectedText, Some(ActionData::Value(text))) => {
                let outcome = self.replace(cx.text, self.selection_range(), text, cx.seconds);
                self.report(outcome, cx)
            }
            (Action::SetValue, Some(ActionData::Value(text))) => {
                let outcome = self.replace(cx.text, 0..usize::MAX, text, cx.seconds);
                self.report(outcome, cx)
            }
            _ => Handled::No,
        }
    }
}

/// 内容里的字节偏移在哪一行的第几个字素簇（自动折行处算到下一行的开头；段尾算到换行符上）
fn position(lines: &[AccessibleLine], offset: usize, cx: &AccessibilityContext) -> Option<TextPosition> {
    let last = lines.len().checked_sub(1)?;
    lines.iter().enumerate().find_map(|(i, line)| {
        let end = line.start + line.text.len();
        let inside = offset < end || (offset == end && (line.line_break || i == last));
        (inside && offset >= line.start)
            .then(|| TextPosition { node: cx.child_id(i as u32), character_index: cluster_index(&line.text, offset - line.start) })
    })
}

impl EditorBlock {
    /// 一次编辑的结果交给元素树：要重画、重新量、重建无障碍节点，内容变了要写回信号，提交、改完、交出焦点
    pub(crate) fn report(&mut self, outcome: EditOutcome, cx: &mut TargetContext) -> Handled {
        if outcome.handled || outcome.changed {
            cx.dirty |= Dirty::PAINT | Dirty::ACCESSIBILITY;
        }
        // 多行输入框的行数变了：重新量高度
        if self.take_resized() {
            cx.dirty |= Dirty::LAYOUT;
        }
        // 文字变了才拼全文（一万字的文档按方向键时不用拼）
        if outcome.changed || outcome.submit {
            cx.edit.text = Some(self.text());
        }
        cx.edit.changed |= outcome.changed;
        cx.edit.submit |= outcome.submit;
        cx.edit.value = Some(self.value);
        if outcome.submit {
            cx.edit.commit = self.take_commit();
        }
        if outcome.blur {
            cx.unfocus();
        }
        outcome.handled.into()
    }
}
