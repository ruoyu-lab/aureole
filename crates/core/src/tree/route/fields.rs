//! 表单级的改值汇总：控件的值被用户改了，从控件往外冒泡一次 [`FieldChangeEvent`]（见 [`crate::FieldHandle`]）。
//! 约定见 [`super`]，传播见 `dispatch.rs`。
//!
//! 路径沿逻辑父元素（浮层里的控件报告到声明浮层的元素，模态浮层是边界）。路径上没有元素听改值汇总时什么都不做，
//! 不造事件（打字时每个字都会走到这里，不分配）。

use super::super::{NodeId, TreeRef};
use super::dispatch::{Options, dispatch};
use crate::event::{ChangeSource, FieldChangeEvent, FieldValue, PropagationPhase};
use crate::handlers::EventType;

/// `node` 的值被用户改了：从它往外冒泡。`value` 只在有元素听的时候才求值
pub(crate) fn field_change(tree: &TreeRef, node: NodeId, value: impl FnOnce() -> FieldValue, source: ChangeSource) {
    let (path, mut event) = {
        let t = tree.borrow();
        if !t.nodes.contains_key(node) {
            return;
        }
        let path = t.route_path(node);
        if !path.iter().any(|&id| t.handlers(id).is_some_and(|h| h.listens_in(EventType::FieldChange, PropagationPhase::Target))) {
            return;
        }
        let name = t.nodes.name(node).map(str::to_owned);
        let handle = t.router.handle_within(&t.nodes, node);
        (path, FieldChangeEvent::new(name, handle, value(), source))
    };
    dispatch(tree, &path, EventType::FieldChange, &mut event, Options::default(), |_, _, _, _| {});
}

/// 输入框的内容被用户改了（打字、粘贴、撤销、输入法提交、辅助技术）：报告新的内容；密码框不带内容
pub(crate) fn text_changed(tree: &TreeRef, node: NodeId, text: impl FnOnce() -> String, source: ChangeSource) {
    let secret = tree
        .borrow()
        .nodes
        .element_kind(node)
        .and_then(|k| k.text_input_client())
        .is_some_and(|client| client.purpose() == crate::input::ImePurpose::Password);
    field_change(tree, node, || if secret { FieldValue::Secret } else { FieldValue::Text(text()) }, source);
}
