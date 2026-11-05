//! 输入法、听写：交给焦点上的文字输入客户端（[`crate::kinds::TextInputClient`]），不传播。
//! 元素树只按接口调用：输入框、字符网格、自己定义的编辑器走同一条路。
//!
//! 要文字输入的是焦点上的元素：它的输入目标说要（[`crate::kinds::InputTarget::wants_text_input`]）、种类有文字输入客户端，
//! 没被禁用。窗口每帧问一次 [`Tree::ime_request`]，有值时打开系统的输入法，候选框跟着插入点走；用途是密码时不开输入法。
//!
//! **组字的生命周期归这里管**（路由器记着正在组字的元素）：
//!
//! - 非空的预编辑开始（继续）组字，空的预编辑、提交结束组字；
//! - 焦点换走、任何一次指针按下、正在组字的元素（或祖先）被禁用、窗口失去焦点、元素卸载时，调用它的
//!   `cancel_composition`（预编辑的字去掉），并请平台的输入法重置（[`Tree::take_ime_reset`]：窗口关一次再开一次输入法，
//!   丢掉平台那边没确定的字）。所以旧的组字不会留在原来的元素里，之后的提交也不会落到新焦点上；
//! - 平台关了输入法（`Ime::Disabled`）时也放弃组字（不用再重置）；
//! - 组字期间的按键标上 `composing`，内置的编辑不处理（[`crate::kinds::InputTarget::on_key_down`] 不调用）。
//!
//! 平台在调用里同步送回的输入法事件（重置输入法时马上送来"输入法关闭"）由窗口宿主排队，等这次输入处理完再处理。

use super::super::{Edited, NodeId, Tree, TreeRef};
use crate::geometry::Rect;
use crate::input::{Ime, ImePurpose, ImeRequest};
use crate::kinds::{TargetContext, TextInputClient};

impl Tree {
    /// 要文字输入的焦点元素：焦点上的输入目标要文字输入，种类有文字输入客户端，没被禁用
    pub(in crate::tree) fn text_input_target(&self) -> Option<NodeId> {
        let id = self.router.focused()?;
        let kind = self.nodes.element_kind(id)?;
        let wants = kind.input_target().is_some_and(|t| t.wants_text_input()) && kind.text_input_client().is_some();
        (wants && !self.disabled(id)).then_some(id)
    }

    /// 放弃正在组的字：交给正在组字的元素的文字输入客户端；`reset` 时请平台的输入法也重置。
    /// 只改元素自己（预编辑的字不算内容，不写回信号、不调用处理器），借着树就能做
    pub(in crate::tree) fn cancel_composition(&mut self, reset: bool) {
        let Some(id) = self.router.take_composing() else { return };
        if reset {
            self.router.request_ime_reset();
        }
        self.with_client(id, |client, cx| client.cancel_composition(cx));
    }

    /// 窗口要重置平台的输入法（放弃了组字），取走标记
    pub(crate) fn take_ime_reset(&mut self) -> bool {
        self.router.take_ime_reset()
    }

    /// `id` 和子孙被禁用了：正在里面组字的话放弃
    pub(in crate::tree) fn composition_disabled(&mut self, id: NodeId) {
        if self.router.composing().is_some_and(|c| self.nodes.is_within(c, id)) {
            self.cancel_composition(true);
        }
    }

    /// 借着树调用 `id` 的文字输入客户端（种类对象留在节点上，见 [`Tree::with_kind`]）
    pub(in crate::tree) fn with_client(&mut self, id: NodeId, f: impl FnOnce(&mut dyn TextInputClient, &mut TargetContext)) -> Option<Edited> {
        let (_, outcome) = self.with_kind(id, |kind, cx| {
            f(kind.text_input_client_mut()?, cx);
            Some(())
        })?;
        Some(Edited { node: id, handled: true, release_focus: outcome.release_focus, edit: outcome.edit })
    }

    /// 一个输入法事件：预编辑、提交交给焦点上的文字输入客户端；平台关了输入法时放弃组字
    pub(in crate::tree) fn ime_event(&mut self, event: &Ime) -> Option<Edited> {
        if let Ime::Disabled = event {
            self.cancel_composition(false);
            return None;
        }
        let id = self.text_input_target()?;
        // 正在组字的不是它（换焦点时已经放弃过，不该发生）：先放弃那边的
        if self.router.composing().is_some_and(|c| c != id) {
            self.cancel_composition(true);
        }
        match event {
            Ime::Preedit(text, cursor) => {
                let cursor = cursor.map(|(a, b)| a.min(b)..a.max(b));
                let edited = self.with_client(id, |client, cx| client.set_preedit(text, cursor, cx));
                self.router.set_composing((!text.is_empty()).then_some(id));
                edited
            }
            Ime::Commit(text) => {
                let edited = self.with_client(id, |client, cx| client.commit(text, cx));
                self.router.set_composing(None);
                edited
            }
            Ime::Enabled | Ime::Disabled => None,
        }
    }

    /// 窗口要怎么设系统的文字输入：焦点上的元素要文字输入时，是插入点区域（窗口坐标）和用途；不要时为 `None`
    pub(crate) fn ime_request(&self) -> Option<ImeRequest> {
        let id = self.text_input_target()?;
        let node = self.nodes.get(id)?;
        let client = self.nodes.element_kind(id)?.text_input_client()?;
        let area = client.caret_area();
        let (x, y) = (node.bounds.x + area.x, node.bounds.y + area.y);
        let [x0, y0] = node.world().apply([x, y]);
        let [x1, y1] = node.world().apply([x + area.width, y + area.height]);
        let area = Rect { x: x0.min(x1), y: y0.min(y1), width: (x1 - x0).abs(), height: (y1 - y0).abs() };
        Some(ImeRequest { area, purpose: client.purpose() })
    }

    /// 输入法候选框应该避开的区域（窗口逻辑坐标）；没有要文字输入的元素有焦点、或者是密码（不用输入法）时为 `None`
    pub(crate) fn ime_area(&self) -> Option<Rect> {
        self.ime_request().filter(|r| r.purpose != ImePurpose::Password).map(|r| r.area)
    }
}

/// 窗口失去焦点：放弃正在组的字，平台的输入法也重置
pub(crate) fn end_composition(tree: &TreeRef) {
    tree.borrow_mut().cancel_composition(true);
}
