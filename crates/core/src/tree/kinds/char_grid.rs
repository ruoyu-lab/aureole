//! 字符网格（[`crate::char_grid`]）是一种元素种类：大小是列数 × 格宽、行数 × 格高（格子按物理像素取整，窗口缩放变了重新量），
//! 插入点每帧推进闪烁，反色时要知道底下是什么颜色。它是输入目标（[`InputTarget`]）：按下拿焦点、拖着选格子、⌘C、⌘A；
//! 别的键交给应用（`on_key_down`）。有 [`crate::Element::on_text_insert`] 时也是文字输入客户端（[`TextInputClient`]）：
//! 有焦点时开输入法（用途是终端），预编辑画在插入点处，提交的文字交给 `on_text_insert`。数据改了重画见 `tree/char_grid.rs`。

use std::ops::Range;

use crate::char_grid::CharGridBlock;
use crate::event::{KeyEvent, PointerEvent, PointerEventKind};
use crate::geometry::Rect;
use crate::input::{ImePurpose, Key};
use crate::interaction::Cursor;
use accesskit::Role;

use crate::kinds::{
    AccessibilityContext, AccessibilityProvider, AvailableSpace, ElementKind, Focusable, Handled, InputTarget, KindHooks, KindInfo,
    MeasureContext, NextFrame, PaintContext, StyleContext, TargetContext, TextInputClient, TickContext,
};
use crate::property::registry::{Dirty, KindMask};

impl ElementKind for CharGridBlock {
    fn name(&self) -> &'static str {
        "char_grid"
    }

    fn kind_mask(&self) -> KindMask {
        KindMask::CHAR_GRID
    }

    /// 插入点闪烁要推进（到换亮灭的时刻出一帧，见 `next_frame`）；格子按物理像素取整，窗口缩放变了要重新量
    fn hooks(&self) -> KindHooks {
        KindHooks::TICK | KindHooks::SCALE_FACTOR
    }

    fn measure(&mut self, cx: &mut MeasureContext, known: [Option<f32>; 2], _: [AvailableSpace; 2]) -> [f32; 2] {
        let [w, h] = CharGridBlock::measure(self, cx.text);
        [known[0].unwrap_or(w), known[1].unwrap_or(h)]
    }

    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        self.selection = cx.selection;
        if self.set_style(cx.style) { Dirty::LAYOUT } else { Dirty::empty() }
    }

    fn refresh_fonts(&mut self) -> Dirty {
        self.invalidate();
        Dirty::LAYOUT
    }

    fn tick(&mut self, cx: &TickContext) -> Dirty {
        if self.blink(cx.seconds) { Dirty::PAINT } else { Dirty::empty() }
    }

    /// 插入点在闪：下一次换亮灭的那一刻要一帧
    fn next_frame(&self, now: std::time::Duration) -> NextFrame {
        self.next_blink(now.as_secs_f64()).map_or(NextFrame::Idle, |at| NextFrame::At(std::time::Duration::from_secs_f64(at)))
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        let (area, ink) = (cx.area, cx.ink);
        // 反色时要知道底下是什么颜色
        let backdrop = cx.tree.backdrop(cx.id);
        let focused = cx.tree.router.focused() == Some(cx.id);
        let selection = [self.selection, cx.tree.paint.colors().rich.inactive];
        let origin = [area.content.x, area.content.y];
        self.inset = [area.content.x - area.bounds.x, area.content.y - area.bounds.y];
        CharGridBlock::paint(self, &mut cx.tree.text, origin, &area.local, ink, backdrop, selection, focused, &mut cx.list.quads);
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

    fn describe(&self, info: &mut KindInfo) {
        let [cols, rows] = self.grid.size();
        if let Some((preedit, _)) = &self.preedit {
            info.fields.push(("preedit", preedit.clone()));
        }
        info.fields.push(("size", format!("{cols}x{rows}")));
        if self.ligated {
            info.fields.push(("ligated", "true".to_owned()));
        }
    }
}

/// 字符网格：按下拿焦点（设了 `focusable` 时 Tab 也走得到）、拖着选格子；⌘C 复制选中的格子，⌘A 全选，别的键交给应用
impl InputTarget for CharGridBlock {
    fn focusable(&self) -> Focusable {
        Focusable::Pointer
    }

    fn on_pointer(&mut self, event: &PointerEvent, cx: &mut TargetContext) -> Handled {
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

    fn on_key_down(&mut self, event: &KeyEvent, cx: &mut TargetContext) -> Handled {
        let Key::Character(c) = &event.key else { return Handled::No };
        if !event.has_primary_modifier() {
            return Handled::No;
        }
        if c.eq_ignore_ascii_case("c") {
            if let Some(text) = self.grid.selected_text() {
                crate::system::clipboard::write_text(text);
            }
            Handled::Yes
        } else if c.eq_ignore_ascii_case("a") {
            self.select_all();
            cx.request_paint();
            Handled::Yes
        } else {
            Handled::No
        }
    }

    fn cursor(&self, _: [f32; 2], _: Rect) -> Option<Cursor> {
        Some(Cursor::Text)
    }

    /// 有 `on_text_insert` 时接文字输入（开输入法）
    fn wants_text_input(&self) -> bool {
        self.text_input
    }
}

/// 字符网格给辅助技术的：终端，值是全部文字（一行一行用换行符连起来）
impl AccessibilityProvider for CharGridBlock {
    fn role(&self) -> Option<Role> {
        Some(Role::Terminal)
    }

    fn write_node(&self, node: &mut accesskit::Node, _: &AccessibilityContext) {
        node.set_value(self.text());
    }
}

/// 终端里用输入法打字：预编辑画在插入点处（不改网格的数据），提交的文字交给 `on_text_insert`（应用写进终端程序）
impl TextInputClient for CharGridBlock {
    fn purpose(&self) -> ImePurpose {
        ImePurpose::Terminal
    }

    fn caret_area(&self) -> Rect {
        let r = self.caret_rect();
        Rect { x: r.x + self.inset[0], y: r.y + self.inset[1], ..r }
    }

    fn set_preedit(&mut self, text: &str, cursor: Option<Range<usize>>, cx: &mut TargetContext) {
        self.preedit = (!text.is_empty()).then(|| (text.to_owned(), cursor.map(|c| c.end)));
        cx.request_paint();
    }

    fn commit(&mut self, text: &str, cx: &mut TargetContext) {
        self.preedit = None;
        if !text.is_empty() {
            cx.edit.inserted = Some(text.to_owned());
        }
        cx.request_paint();
    }

    fn cancel_composition(&mut self, cx: &mut TargetContext) {
        if self.preedit.take().is_some() {
            cx.request_paint();
        }
    }

    /// 听写、辅助技术改的文字：终端不能改已经显示的内容，替换成插入
    fn replace_range(&mut self, _: Range<usize>, text: &str, cx: &mut TargetContext) {
        self.commit(text, cx);
    }
}
