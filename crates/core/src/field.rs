//! 表单级的改值汇总：控件的值被用户改了时，从控件往外冒泡一次 [`FieldChangeEvent`]，外面的容器用
//! [`Element::on_field_change`] 收到里面任何控件的改动（哪个控件、新值的描述、怎么改的）。
//!
//! 控件自己的 `on_change`、`on_commit` 照旧只发给控件自己；这里是给表单用的另一条路。
//! 输入框（核心的元素）、基础控件（`aureole-controls`）都已经报告了；自己做的控件用 [`FieldHandle`] 加入：
//! 交给控件的元素（[`Element::field_handle`]），用户改了值时调用 [`FieldHandle::notify`]。

use std::cell::RefCell;
use std::rc::Weak;

use reactive::{Signal, signal};

use crate::element::Element;
use crate::event::{ChangeSource, FieldChangeEvent, FieldValue, IntoHandler};
use crate::handlers::{EventType, Handler};
use crate::tree::{NodeId, Tree};

/// 报告用户改值的句柄：交给控件的元素（[`Element::field_handle`]），用户改了值时调用 [`FieldHandle::notify`]，
/// 从这个元素往外冒泡一次 [`FieldChangeEvent`]。和信号一样是 `Copy` 的。
///
/// 基础控件和输入框已经报告了，自己做的控件用它加入表单级的改值汇总：
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::{ChangeSource, FieldHandle, FieldValue};
/// // 自己做的星级评分：点第几颗星就是几分
/// let stars = signal(0u32);
/// let field = FieldHandle::new();
/// row().field_handle(field).children((1..=5).map(move |n| {
///     rect().size(20, 20).on_click(move || {
///         stars.set(n);
///         field.notify(FieldValue::Number(n as f64), ChangeSource::Pointer);
///     })
/// }));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldHandle {
    /// 挂载时交给的元素树和节点
    target: Signal<Option<Target>>,
}

#[derive(Clone)]
struct Target(Weak<RefCell<Tree>>, NodeId);

impl FieldHandle {
    /// 新句柄，归当前作用域所有
    pub fn new() -> Self {
        Self { target: signal(None) }
    }

    /// 用户把值改成了 `value`：从交给句柄的元素往外冒泡一次 [`FieldChangeEvent`]（立即，在这个调用里）。
    /// 元素还没挂载、已经卸载时什么都不做；代码里改值时不要调用（和 `on_change` 一样只报告用户的改动）
    pub fn notify(&self, value: FieldValue, source: ChangeSource) {
        let Some(Target(tree, id)) = self.target.with_untracked(Clone::clone) else { return };
        if let Some(tree) = tree.upgrade() {
            crate::tree::field_change(&tree, id, move || value, source);
        }
    }

    /// 挂载：记下元素树和节点
    pub(crate) fn bind(&self, tree: &crate::tree::TreeRef, id: NodeId) {
        self.target.set(Some(Target(std::rc::Rc::downgrade(tree), id)));
    }
}

impl Default for FieldHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Element {
    /// 交给报告改值的句柄（见 [`FieldHandle`]）：用户改了值时从这个元素往外冒泡 [`FieldChangeEvent`]
    pub fn field_handle(mut self, handle: FieldHandle) -> Self {
        self.interaction().field_handle = Some(handle);
        self
    }

    /// 里面任何控件（输入框、复选框、开关、单选组、下拉选择、滑块、数字输入，以及用 [`FieldHandle`] 报告的自己做的控件）
    /// 被用户改了值：从控件往外冒泡，事件里有哪个控件（名字、焦点句柄）、新值的描述、怎么改的。控件自己也收得到。
    /// 代码里改绑定的信号时不发；控件自己的 `on_change` 照旧只发给控件自己
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (name, agree, dirty) = (signal(String::new()), signal(false), signal(false));
    /// column()
    ///     .on_field_change(move |e: &FieldChangeEvent| {
    ///         println!("{} 改成了 {}", e.name.as_deref().unwrap_or("?"), e.value);
    ///         dirty.set(true);
    ///     })
    ///     .child(text_field(name).name("name"))
    ///     .child(checkbox(agree).label("同意").name("agree"));
    /// ```
    pub fn on_field_change<M>(self, handler: impl IntoHandler<FieldChangeEvent, M>) -> Self {
        self.listen(EventType::FieldChange, Handler::Field(handler.into_handler()))
    }

    /// 改值汇总的捕获阶段：从外往里，在里面的控件之前收到，见 [`Element::on_field_change`]。
    /// 自己做的复合控件用它把里面的部件的改动截下来，只报告自己的值
    pub fn on_field_change_capture<M>(self, handler: impl IntoHandler<FieldChangeEvent, M>) -> Self {
        self.listen_capture(EventType::FieldChange, Handler::Field(handler.into_handler()))
    }
}
