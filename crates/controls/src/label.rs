//! 标签：一段文字，点它作用到对应的控件上；辅助技术按它念控件的名字。

use super::look::look;
use super::{on_activate, part};
use aureole_core::{Element, FocusHandle, IntoProp, Prop, Role, States, Style, View, text};

/// 标签的构建器；[`label`] 是它的简写。
///
/// 标签和控件用同一个焦点句柄（[`FocusHandle`]）连起来：控件 `.focus_handle(h)`，标签 `.target(h)`。
/// 标签放在哪里都行（表格的另一列、上面一行、离得很远），不用包着控件：
///
/// - 点标签时作用到控件上：输入框、滑块、数字输入获得焦点；复选框、开关切换，单选组获得焦点，
///   下拉框打开，按钮被点了。控件禁用时什么都不做；
/// - 对辅助技术，控件"由这个标签命名"（控件自己没有名字时，标签的文字就是它的名字）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let (email, remember) = (signal(String::new()), signal(false));
/// let (email_focus, remember_focus) = (FocusHandle::new(), FocusHandle::new());
/// grid().columns([px(80), fr(1)]).gap(8)
///     .child(label("邮箱").target(email_focus))
///     .child(text_field(email).focus_handle(email_focus))
///     .child(label("记住我").target(remember_focus))
///     .child(checkbox(remember).focus_handle(remember_focus));
/// ```
///
/// 默认：正文颜色、14 号字。复选框、开关、单选项的 `.label(..)` 是它们自己带的文字（点它也切换），不用另外加标签
pub struct Label {
    content: Prop<String>,
    target: Option<FocusHandle>,
    style: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

element_methods!([] Label);

impl Label {
    /// 显示 `content` 的标签
    pub fn new<S: Into<String> + 'static>(content: impl IntoProp<S>) -> Self {
        Self { content: content.into_prop().map(S::into), target: None, style: Style::new(), after: Default::default() }
    }

    /// 对应的控件：控件用同一个句柄（`.focus_handle(handle)`）
    pub fn target(mut self, handle: FocusHandle) -> Self {
        self.target = Some(handle);
        self
    }

    /// 改标签的样子
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Label { content, target, style, after: _ } = self;
        let el = part(text(content).accessibility_role(Role::Label), &look().label, &style, &States::new());
        match target {
            Some(handle) => on_activate(el.accessibility_label_target(handle), move |_| handle.activate()),
            None => el,
        }
    }
}

impl View for Label {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 标签，[`Label::new`] 的简写：`label("用户名").target(handle)`
pub fn label<S: Into<String> + 'static>(content: impl IntoProp<S>) -> Label {
    Label::new(content)
}
