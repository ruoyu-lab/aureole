
//! 键盘和输入法：按键分发、松开交给按下时的那条链、修饰键、输入法事件。约定见 [`super`]，传播见 `dispatch.rs`。
//!
//! 按下的顺序：标了优先的快捷键 → 捕获（从外往里到焦点的父元素；浮层宿主在这一段处理 Esc，宿主不在路径上时在捕获的处理器都走完以后）→
//! 目标（焦点上用户的处理器，然后是输入目标的 `on_key_down`）→ 冒泡（每个元素的处理器之后是它的 `focus_arrows`）→
//! 没被停止时：快捷键 → 内置行为（Tab、回车和空格激活、方向键和翻页键滚动）。
//! 目标是焦点；没有焦点时是最上面的模态浮层的内容，没有模态浮层时是第一个根元素。

use reactive::batch;

use super::super::interaction::{Held, KeyScroll};
use super::super::{Edited, NodeId, Tree, TreeRef};
use super::dispatch::{Options, dispatch, dispatch_capture};
use super::pointer::cancel_drag_drop;
use super::run_focus_callbacks;
use crate::event::{ClickSource, Builtin, ChangeEvent, ChangeSource, ClickEvent, KeyEvent, PropagationPhase};
use crate::handlers::{EventType, Handler};
use crate::input::{Ime, Key, KeyInput, Modifiers, MouseButton, NamedKey};
use crate::interaction::find_shortcut;
use crate::overlay::Dismiss;
use crate::layout::Direction;

impl Tree {
    /// 按键的目标：焦点；没有焦点时是最上面的模态浮层的内容，没有模态浮层时是第一个根元素
    pub(in crate::tree) fn key_target(&self) -> Option<NodeId> {
        self.router.focused().or_else(|| self.trap_content()).or_else(|| self.nodes.roots().first().copied())
    }
}

/// 按键按下：分发，然后是快捷键和内置行为。返回是否处理了（有处理器停止了传播、快捷键、内置行为）
pub(crate) fn key_down(tree: &TreeRef, input: &KeyInput) -> bool {
    let mut event = KeyEvent::new(
        input.key.clone(),
        input.base.clone().unwrap_or_else(|| input.key.clone()),
        input.physical.clone(),
        input.text.as_deref().map(str::to_owned),
        input.modifiers,
        input.repeated,
    );
    let (target, dragging) = {
        let mut t = tree.borrow_mut();
        t.router.set_keyboard(true);
        t.router.set_modifiers(input.modifiers);
        // 焦点上的元素正在组字：这个键是给输入法的
        event.composing = t.router.composing().is_some();
        t.hold(input, &event);
        (t.key_target(), t.drag_drop.dragging())
    };
    let escape = event.key == Key::Named(NamedKey::Escape) && input.modifiers.is_empty();
    // 拖放中按 Esc：取消，元素飞回去
    if dragging && event.key == Key::Named(NamedKey::Escape) {
        if !input.repeated {
            cancel_drag_drop(tree);
        }
        return true;
    }
    // 标了优先的快捷键抢在所有元素之前
    if let Some(shortcut) = find_shortcut(&event, true) {
        batch(|| shortcut());
        return true;
    }
    let path = target.map(|id| tree.borrow().route_path(id)).unwrap_or_default();
    // Esc 关浮层：由浮层宿主在捕获阶段处理。能关的浮层不在路径上（焦点在浮层外面）时，它盖在整个窗口上：
    // 捕获的处理器走完（它们能取消）就处理，目标上其余的处理器和输入目标收不到这个键
    let host = if escape { tree.borrow().dismissable_host() } else { None };
    let mut options = Options::default();
    if let Some(host) = host
        && !path.contains(&host)
    {
        dispatch_capture(tree, &path, EventType::KeyDown, &mut event);
        if !event.is_propagation_stopped() && !event.is_builtin_prevented(Builtin::DISMISS) && dismiss_by_escape(tree, host) {
            return true;
        }
        options.captured = true;
    }
    let focused = tree.borrow().router.focused();
    dispatch(tree, &path, EventType::KeyDown, &mut event, options, |tree, id, phase, e: &KeyEvent| match phase {
        PropagationPhase::Capture => {
            if host == Some(id) && !e.is_builtin_prevented(Builtin::DISMISS) && dismiss_by_escape(tree, id) {
                e.stop_propagation();
            }
        }
        PropagationPhase::Target | PropagationPhase::Bubble => {
            // 目标上：有焦点的输入框（或选中了文字的可选中文字、字符网格）编辑，处理了就不往外传
            if phase == PropagationPhase::Target && Some(id) == focused && edits(tree, id, e) {
                e.stop_propagation();
                return;
            }
            if !e.is_propagation_stopped() && focus_arrows(tree, id, e) {
                e.stop_propagation();
            }
        }
    });
    if event.is_propagation_stopped() {
        return true;
    }
    if let Some(shortcut) = find_shortcut(&event, false) {
        batch(|| shortcut());
        return true;
    }
    builtin(tree, &event)
}

/// Esc 关 `host` 这个浮层（交给它的 on_dismiss）；它已经不能关了返回 `false`
fn dismiss_by_escape(tree: &TreeRef, host: NodeId) -> bool {
    let dismiss = tree.borrow().dismissable_host_handler(host);
    match dismiss {
        Some(dismiss) => {
            batch(|| dismiss(Dismiss::Escape));
            true
        }
        None => false,
    }
}

/// 目标上的内置行为：有焦点的输入目标（输入框、可选中的文字、字符网格、自己定义的）处理按键
/// （[`crate::kinds::InputTarget::on_key_down`]）；处理器取消了 `EDIT`（方向键还有 `ARROWS`）、正在组字时不处理。返回是否处理了
fn edits(tree: &TreeRef, id: NodeId, e: &KeyEvent) -> bool {
    if e.is_builtin_prevented(Builtin::EDIT) || e.composing || !tree.borrow().is_text(id) || tree.borrow().disabled(id) {
        return false;
    }
    let arrow = matches!(e.key, Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::ArrowLeft | NamedKey::ArrowRight));
    if arrow && e.is_builtin_prevented(Builtin::ARROWS) {
        return false;
    }
    let edited = tree.borrow_mut().target_key(id, e);
    finish_edit(tree, edited, ChangeSource::Keyboard)
}

/// 元素自己的 `focus_arrows`：方向键在子孙之间移动焦点（带修饰键时、取消了 `ARROWS` 时不动）。返回是否移动了
fn focus_arrows(tree: &TreeRef, id: NodeId, e: &KeyEvent) -> bool {
    if e.is_builtin_prevented(Builtin::ARROWS) || !e.modifiers.is_empty() {
        return false;
    }
    let arrows = {
        let t = tree.borrow();
        if !t.routable(id) {
            return false;
        }
        t.node_input(id).and_then(|i| i.arrows)
    };
    let forward = match (arrows, &e.key) {
        (Some(Direction::Column), Key::Named(NamedKey::ArrowDown)) | (Some(Direction::Row), Key::Named(NamedKey::ArrowRight)) => true,
        (Some(Direction::Column), Key::Named(NamedKey::ArrowUp)) | (Some(Direction::Row), Key::Named(NamedKey::ArrowLeft)) => false,
        _ => return false,
    };
    let moved = tree.borrow_mut().move_focus_within(id, forward);
    match moved {
        Some(callbacks) => {
            run_focus_callbacks(callbacks);
            true
        }
        None => false,
    }
}

/// 按键松开：交给按下时的那条链（按下时冒泡的起点往外），没有元素处理时交给窗口的 `on_key_up`。
/// 没有按下过的键（按下交给了输入法）不交出去。返回是否处理了
pub(crate) fn key_up(tree: &TreeRef, input: &KeyInput) -> bool {
    let base = input.base.clone().unwrap_or_else(|| input.key.clone());
    let held = {
        let mut t = tree.borrow_mut();
        t.router.set_modifiers(input.modifiers);
        let index = t.router.held().iter().position(|h| h.same(input.physical.as_ref(), &base));
        index.map(|i| t.router.held_mut().remove(i))
    };
    match held {
        Some(held) => release(tree, held, input.modifiers),
        None => false,
    }
}

/// 窗口失去焦点（收不到松开了）：按着的键全部松开
pub(crate) fn release_keys(tree: &TreeRef) {
    let (held, modifiers) = {
        let mut t = tree.borrow_mut();
        (std::mem::take(t.router.held_mut()), t.router.modifiers())
    };
    for h in held {
        release(tree, h, modifiers);
    }
}

/// 输入法开始组字：之后的松开系统交给输入法，按着的键（修饰键除外）现在就松开
fn release_for_ime(tree: &TreeRef) {
    let (held, modifiers) = {
        let mut t = tree.borrow_mut();
        let modifier = |k: &Key| matches!(k, Key::Named(NamedKey::Shift | NamedKey::Control | NamedKey::Alt | NamedKey::Super | NamedKey::Meta));
        if !t.router.held().iter().any(|h| !modifier(&h.base)) {
            return;
        }
        let (keep, release): (Vec<Held>, Vec<Held>) = std::mem::take(t.router.held_mut()).into_iter().partition(|h| modifier(&h.base));
        *t.router.held_mut() = keep;
        (release, t.router.modifiers())
    };
    for h in held {
        release(tree, h, modifiers);
    }
}

fn release(tree: &TreeRef, held: Held, modifiers: Modifiers) -> bool {
    crate::interaction::set_held(&held.base, false);
    let mut event = KeyEvent::new(held.key, held.base, held.physical, None, modifiers, false);
    // 交给按下时的那条链（按下时的焦点往外，沿逻辑父元素），跳过卸载了、在退场、在正在关的浮层里的元素
    let path = held.start.map(|id| tree.borrow().route_path(id)).unwrap_or_default();
    dispatch(tree, &path, EventType::KeyUp, &mut event, Options::default(), |_, _, _, _| {});
    if event.is_propagation_stopped() {
        return true;
    }
    let listeners = crate::interaction::key_up_listeners();
    if listeners.is_empty() {
        return false;
    }
    batch(|| listeners.iter().for_each(|l| l(&event)));
    true
}

/// 内置行为（没被停止、没被取消时）：Tab / Shift+Tab 切换焦点（`TAB`）；回车、空格激活有焦点的可点击元素（`ACTIVATE`）；
/// 方向键、翻页键、Home / End、空格滚动焦点（没有焦点时是指针下）所在的滚动视口（`SCROLL`）
fn builtin(tree: &TreeRef, event: &KeyEvent) -> bool {
    let Key::Named(key) = event.key else { return false };
    let mods = event.modifiers;
    let plain = mods.is_empty();
    let prevented = |b: Builtin| event.is_builtin_prevented(b);
    match key {
        NamedKey::Tab if (mods - Modifiers::SHIFT).is_empty() && !prevented(Builtin::TAB) => {
            let callbacks = tree.borrow_mut().move_focus(!mods.has_shift());
            run_focus_callbacks(callbacks);
            true
        }
        NamedKey::Enter | NamedKey::Space if plain && !prevented(Builtin::ACTIVATE) && activate(tree) => true,
        _ if prevented(Builtin::SCROLL) => false,
        _ => {
            let action = match (key, mods.has_shift()) {
                (NamedKey::ArrowUp, _) => (1, KeyScroll::Line(-1.0)),
                (NamedKey::ArrowDown, _) => (1, KeyScroll::Line(1.0)),
                (NamedKey::ArrowLeft, _) => (0, KeyScroll::Line(-1.0)),
                (NamedKey::ArrowRight, _) => (0, KeyScroll::Line(1.0)),
                (NamedKey::PageUp, _) | (NamedKey::Space, true) => (1, KeyScroll::Page(-1.0)),
                (NamedKey::PageDown, _) | (NamedKey::Space, false) => (1, KeyScroll::Page(1.0)),
                (NamedKey::Home, _) => (1, KeyScroll::Start),
                (NamedKey::End, _) => (1, KeyScroll::End),
                _ => return false,
            };
            if !(mods - Modifiers::SHIFT).is_empty() {
                return false;
            }
            let mut t = tree.borrow_mut();
            // 输入框里的方向键不滚动外面的视口
            if t.router.focused().is_some_and(|id| t.wants_text_input(id)) {
                return false;
            }
            let chain: Vec<NodeId> = match t.router.focused() {
                Some(id) => t.ancestry(id).into_iter().rev().collect(),
                None => t.router.hovered().iter().rev().copied().collect(),
            };
            t.key_scroll(&chain, action.0, action.1)
        }
    }
}

/// 回车、空格：有焦点、能点（有 `on_click`）、没被禁用的元素（不是输入框）上合成一次点击（来源是键盘），往外冒泡；
/// 焦点在富文本的链接上时打开链接
fn activate(tree: &TreeRef) -> bool {
    let link = tree.borrow().focused_link_action();
    if let Some(link) = link {
        batch(|| link.run());
        return true;
    }
    let click = {
        let t = tree.borrow();
        let Some(id) = t.router.focused() else { return false };
        if t.is_text(id) || t.disabled(id) || !t.handlers(id).is_some_and(|h| h.listens_in(EventType::Click, PropagationPhase::Target)) {
            return false;
        }
        (t.route_path(id), t.click_event(id, ClickSource::Keyboard, MouseButton::Left, 1))
    };
    let (path, mut event): (_, ClickEvent) = click;
    dispatch(tree, &path, EventType::Click, &mut event, Options { skip_disabled: true, captured: false }, |_, _, _, _| {});
    true
}

/// 修饰键变了（指针事件、Shift+点击扩展选区要用）
pub(crate) fn set_modifiers(tree: &TreeRef, modifiers: Modifiers) {
    tree.borrow_mut().set_modifiers(modifiers);
}

/// 输入法事件，交给焦点上的文字输入客户端（不传播）
pub(crate) fn ime(tree: &TreeRef, event: &Ime) {
    // 开始组字：之后的松开系统交给输入法，按着的键现在就松开
    if matches!(event, Ime::Preedit(text, _) if !text.is_empty()) {
        release_for_ime(tree);
    }
    let edited = tree.borrow_mut().ime_event(event);
    finish_edit(tree, edited, ChangeSource::Keyboard);
}

/// 编辑之后（已释放树的借用）：写回信号、提交、失去焦点。`source` 是改值事件里的"怎么改的"
/// （键盘：打字、删除、粘贴、撤销、输入法提交；辅助技术：改值、替换文字）
pub(in crate::tree) fn finish_edit(tree: &TreeRef, edited: Option<Edited>, source: ChangeSource) -> bool {
    let Some(Edited { node, handled, release_focus, edit }) = edited else { return false };
    let crate::editor::EditReport { changed, submit, text, value, commit, inserted } = edit;
    let (submit, change, commit_handlers, insert) = {
        let t = tree.borrow();
        let submit = if submit { t.handlers_of(node, EventType::Submit, Handler::text) } else { Vec::new() };
        let change = if changed { t.handlers_of(node, EventType::Change, Handler::text_change) } else { Vec::new() };
        let commit = if commit.is_some() { t.handlers_of(node, EventType::Commit, Handler::text_change) } else { Vec::new() };
        let insert = if inserted.is_some() { t.handlers_of(node, EventType::TextInsert, Handler::text) } else { Vec::new() };
        (submit, change, commit, insert)
    };
    let callbacks = if release_focus { tree.borrow_mut().focus_to(None, false) } else { Vec::new() };
    // 没有处理器还要用内容时，内容直接交给信号（一万字的输入框里每打一个字少复制一遍全文）；
    // 表单级的改值汇总有元素听时再从信号里取
    let mut text = text;
    let handed = changed && value.is_some() && change.is_empty() && submit.is_empty();
    batch(|| {
        if changed
            && let Some(value) = value
            && let Some(text) = if handed { text.take() } else { text.clone() }
        {
            value.set_if_changed(text);
        }
        if changed
            && !change.is_empty()
            && let Some(text) = &text
        {
            let event = ChangeEvent { value: text.clone(), source };
            change.iter().for_each(|h| h(&event));
        }
        if let Some(text) = &text {
            submit.iter().for_each(|h| h(text.clone()));
        }
        if let Some(text) = commit {
            let event = ChangeEvent { value: text, source };
            commit_handlers.iter().for_each(|h| h(&event));
        }
        if let Some(text) = inserted {
            insert.iter().for_each(|h| h(text.clone()));
        }
    });
    // 表单级的改值汇总：从输入框往外冒泡
    if changed {
        match (&text, value) {
            (Some(text), _) => super::fields::text_changed(tree, node, || text.clone(), source),
            (None, Some(value)) if handed => super::fields::text_changed(tree, node, || value.get_untracked(), source),
            _ => {}
        }
    }
    run_focus_callbacks(callbacks);
    handled
}
