//! 复选框：方框里一个勾（半选时一条横线），点方框或旁边的文字切换。

use std::rc::Rc;

use reactive::Signal;

use super::look::look;
use super::{Changes, Flag, Press, Stored, caption, on_activate, part, reader, share};
use aureole_core::{ChangeSource, Element, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, VectorPath, Prop, Role, State, Stroke, Style, Toggled, View, path, rect, row};

/// 复选框的构建器；[`checkbox`] 是它的简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let agree = signal(false);
/// checkbox(agree).label("我已阅读并同意");
/// checkbox(agree).accessibility_name("全选");             // 只有方框，给辅助技术起个名字
/// // 改样子：圆角大一点、勾上时绿色
/// let green = Style::new().radius(5).checked(|s| s.background(Color::hex(0x1f9a6b)).border_color(Color::hex(0x1f9a6b)));
/// checkbox(agree).label("完成").indicator_style(&green);
/// ```
///
/// 部位：外壳（`.style`，方框和文字排成的一行）、方框（[`Checkbox::indicator_style`]）、勾和横线（[`Checkbox::mark_style`]）。
/// 默认：16 见方、圆角 3，白底加灰色描边；勾上（或半选）时强调色实底、白色的勾；禁用时变淡。没有动画。
/// 状态：悬停、按下、焦点、禁用（整行的），勾选（勾上或半选；勾的那一部位是"这个记号该显示"）。
///
/// 键盘：Tab 走到，空格或回车切换。对辅助技术是复选框（半选时是"混合"状态），名字是旁边的文字。
pub struct Checkbox {
    checked: Signal<bool>,
    label: Option<Prop<String>>,
    indeterminate: Option<Prop<bool>>,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<bool>,
    view: Option<CheckboxView>,
    style: Style,
    indicator: Style,
    mark: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 自己重做的界面
type CheckboxView = Box<dyn FnOnce(CheckboxState) -> Element>;

/// 复选框的逻辑，不带界面：勾没勾、半选、切换之后通知谁。默认的复选框也只用这里的方法切换。
/// 自己重做界面（[`Checkbox::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let done = signal(false);
/// Checkbox::new(done).label("做完了").on_change(|v: bool| println!("{v}")).view(|state: CheckboxState| {
///     row()
///         .gap(8)
///         .focusable(true)
///         .accessibility_role(Role::CheckBox)
///         .accessibility_checked(move || state.checked())
///         .on_click(move |e: &ClickEvent| state.toggle(e.source.into()))
///         .child(text(move || if state.checked() { "●" } else { "○" }))
///         .child(text(move || state.label().unwrap_or_default()))
/// });
/// ```
#[derive(Clone, Copy)]
pub struct CheckboxState {
    checked: Signal<bool>,
    mixed: Flag,
    disabled: Flag,
    focus: Option<FocusHandle>,
    shared: Stored<CheckboxShared>,
}

struct CheckboxShared {
    changes: Changes<bool>,
    /// 只有自己重做界面时才有：默认的界面直接用属性，不多一层绑定
    label: Option<Rc<dyn Fn() -> String>>,
}

impl CheckboxState {
    /// 勾没勾，响应式
    pub fn checked(self) -> bool {
        self.checked.get()
    }

    /// 是不是半选（[`Checkbox::indeterminate`]），响应式
    pub fn indeterminate(self) -> bool {
        self.mixed.get()
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// [`Checkbox::label`] 给的文字，响应式；没有时是 `None`。只在自己重做的界面里有值
    pub fn label(self) -> Option<String> {
        self.shared.get().label.as_ref().map(|label| label())
    }

    /// [`Checkbox::focus_handle`] 给的焦点句柄：放到自己的元素上，点标签才能作用到它
    pub fn focus_handle(self) -> Option<FocusHandle> {
        self.focus
    }

    /// 用户切换了。禁用时什么都不做
    pub fn toggle(self, source: ChangeSource) {
        self.set(!self.checked.get_untracked(), source);
    }

    /// 用户把它改成 `value`。禁用时、值没变时什么都不做
    pub fn set(self, value: bool, source: ChangeSource) {
        if !self.disabled.get() && self.checked.get_untracked() != value {
            self.checked.set(value);
            self.shared.get().changes.both(value, source);
        }
    }
}

/// 勾和横线的形状（24 的画布）
fn check() -> VectorPath {
    VectorPath::svg("M5 12.5 L10 17 L19 7").view_box(0.0, 0.0, 24.0, 24.0)
}

fn dash() -> VectorPath {
    VectorPath::svg("M6 12 H18").view_box(0.0, 0.0, 24.0, 24.0)
}

/// 方框、单选圆里的记号：位置叠在一起，按"该不该显示"（交给样式对象的勾选状态）显示
fn mark_path(shape: VectorPath) -> Element {
    path(shape).position(0, 0).size(12, 12).stroke(Stroke::new(3.0).round())
}

element_methods!([] Checkbox);

impl Checkbox {
    /// 绑定到 `checked` 的复选框：点击切换
    pub fn new(checked: Signal<bool>) -> Self {
        Self {
            after: Default::default(),
            checked,
            label: None,
            indeterminate: None,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            indicator: Style::new(),
            mark: Style::new(),
        }
    }

    /// 旁边显示的文字，点它也切换；也是读屏软件念的名字
    pub fn label<S: Into<String> + 'static>(mut self, label: impl IntoProp<S>) -> Self {
        self.label = Some(label.into_prop().map(S::into));
        self
    }

    /// 半选（一组里选了一部分）：画一条横线。点击仍然切换 `checked`，半选由应用自己算
    pub fn indeterminate(mut self, indeterminate: impl IntoProp<bool>) -> Self {
        self.indeterminate = Some(indeterminate.into_prop());
        self
    }

    /// 禁用：变淡，不能切换，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制焦点；[`super::Label::target`] 用同一个句柄时点标签就切换它
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户切换了（点击、空格回车、标签、辅助技术），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<bool, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 切换完成：复选框每次切换都是完成的，和 [`Checkbox::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<bool, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、禁用都照常工作，逻辑在 [`CheckboxState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(CheckboxState) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改外壳（方框和文字排成的一行）的样子
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改方框的样子：勾选状态是勾上或半选
    pub fn indicator_style(mut self, style: &Style) -> Self {
        self.indicator = self.indicator.extend(style);
        self
    }

    /// 改勾和横线的样子（颜色是方框的文字颜色，继承下来）：勾选状态是"这个记号该显示"，默认平时透明度 0、勾选时 1
    pub fn mark_style(mut self, style: &Style) -> Self {
        self.mark = self.mark.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Checkbox { checked, label, indeterminate, disabled, focus, mut changes, view, style, indicator, mark, after: _ } = self;
        let field = changes.report(|v| FieldValue::Bool(*v));
        let disabled = Flag::new(disabled);
        let mixed = Flag::new(indeterminate);
        let (label, label_text) = if view.is_some() { (None, label.map(reader)) } else { (label, None) };
        let state = CheckboxState { checked, mixed, disabled, focus, shared: Stored::new(CheckboxShared { changes, label: label_text }) };
        if let Some(view) = view {
            return view(state).field_handle(field);
        }
        let l = look();
        let press = Press::new();
        let base = disabled.state(press.states(), State::Disabled);
        let on = move || checked.get() || mixed.get();

        let tick = part(mark_path(check()), &l.mark, &mark, &base.clone().checked(move || checked.get() && !mixed.get()));
        let mut marks = rect().size(12, 12).child(tick);
        if !matches!(mixed, Flag::Off) {
            marks = marks.child(part(mark_path(dash()), &l.mark, &mark, &base.clone().checked(move || mixed.get())));
        }
        let square = part(row(), &l.checkbox_box, &indicator, &base.clone().checked(on)).child(marks);

        let mut el = press.attach(row()).accessibility_role(Role::CheckBox).field_handle(field);
        el = disabled.apply(el);
        if let Some(handle) = focus {
            el = el.focus_handle(handle);
        }
        el = part(el, &l.toggle_root, &style, &base.clone().checked(on)).child(square);
        if let Some(label) = label {
            let (shown, name) = share(label);
            el = el.accessibility_name(name).child(caption(shown));
        }
        let toggled = move || if mixed.get() { Toggled::Mixed } else { Toggled::from(checked.get()) };
        on_activate(el, move |source| state.toggle(source)).accessibility_checked(toggled)
    }
}

impl View for Checkbox {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 复选框，[`Checkbox::new`] 的简写
pub fn checkbox(checked: Signal<bool>) -> Checkbox {
    Checkbox::new(checked)
}
