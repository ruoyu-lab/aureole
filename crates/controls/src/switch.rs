//! 开关：胶囊形的轨道里一个圆钮，点开关或旁边的文字切换。

use std::rc::Rc;

use reactive::Signal;

use super::look::look;
use super::{Changes, Flag, Press, Stored, caption, on_activate, part, reader, share};
use aureole_core::{ChangeSource, Element, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, Prop, Role, State, Style, View, rect, row};

/// 开关的构建器；[`switch`] 是它的简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let wifi = signal(true);
/// switch(wifi).label("无线局域网");
/// // 改样子：方一点的轨道，打开时绿色；圆钮滑过去
/// let track = Style::new().radius(4).checked(|s| s.background(Color::hex(0x1f9a6b)));
/// let thumb = Style::new().radius(3).transition(Property::TranslationX, Spring::smooth());
/// switch(wifi).label("自动同步").track_style(&track).thumb_style(&thumb);
/// ```
///
/// 部位：外壳（`.style`，开关和文字排成的一行）、轨道（[`Switch::track_style`]）、圆钮（[`Switch::thumb_style`]）。
/// 默认：32 × 18 的灰色轨道，14 的白色圆钮离边 2；打开时轨道是强调色、圆钮在右边（勾选状态下 `translation_x(14)`）；
/// 禁用时变淡。没有动画：圆钮直接到位，要滑过去就在圆钮的样式对象里声明平移的过渡。
/// 状态：悬停、按下、焦点、禁用（整行的），勾选（打开着）。
///
/// 键盘：Tab 走到，空格或回车切换。对辅助技术是开关，名字是旁边的文字；只有开关时用 `.accessibility_name(..)` 起名字。
pub struct Switch {
    on: Signal<bool>,
    label: Option<Prop<String>>,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<bool>,
    view: Option<SwitchView>,
    style: Style,
    track: Style,
    thumb: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 自己重做的界面
type SwitchView = Box<dyn FnOnce(SwitchState) -> Element>;

/// 开关的逻辑，不带界面：开没开、切换之后通知谁。默认的开关也只用这里的方法切换。
/// 自己重做界面（[`Switch::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let wifi = signal(true);
/// Switch::new(wifi).view(|state: SwitchState| {
///     button(text(move || if state.is_on() { "开" } else { "关" })).on_click(move |e: &ClickEvent| state.toggle(e.source.into()))
/// });
/// ```
#[derive(Clone, Copy)]
pub struct SwitchState {
    on: Signal<bool>,
    disabled: Flag,
    focus: Option<FocusHandle>,
    shared: Stored<SwitchShared>,
}

struct SwitchShared {
    changes: Changes<bool>,
    /// 只有自己重做界面时才有：默认的界面直接用属性，不多一层绑定
    label: Option<Rc<dyn Fn() -> String>>,
}

impl SwitchState {
    /// 开没开，响应式
    pub fn is_on(self) -> bool {
        self.on.get()
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// [`Switch::label`] 给的文字，响应式；没有时是 `None`。只在自己重做的界面里有值
    pub fn label(self) -> Option<String> {
        self.shared.get().label.as_ref().map(|label| label())
    }

    /// [`Switch::focus_handle`] 给的焦点句柄：放到自己的元素上，点标签才能作用到它
    pub fn focus_handle(self) -> Option<FocusHandle> {
        self.focus
    }

    /// 用户切换了。禁用时什么都不做
    pub fn toggle(self, source: ChangeSource) {
        self.set(!self.on.get_untracked(), source);
    }

    /// 用户把它改成 `value`。禁用时、值没变时什么都不做
    pub fn set(self, value: bool, source: ChangeSource) {
        if !self.disabled.get() && self.on.get_untracked() != value {
            self.on.set(value);
            self.shared.get().changes.both(value, source);
        }
    }
}

element_methods!([] Switch);

impl Switch {
    /// 绑定到 `on` 的开关：点击切换
    pub fn new(on: Signal<bool>) -> Self {
        Self {
            after: Default::default(),
            on,
            label: None,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            track: Style::new(),
            thumb: Style::new(),
        }
    }

    /// 旁边显示的文字，点它也切换；也是读屏软件念的名字
    pub fn label<S: Into<String> + 'static>(mut self, label: impl IntoProp<S>) -> Self {
        self.label = Some(label.into_prop().map(S::into));
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

    /// 切换完成：开关每次切换都是完成的，和 [`Switch::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<bool, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、禁用都照常工作，逻辑在 [`SwitchState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(SwitchState) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改外壳（开关和文字排成的一行）的样子
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改轨道的样子
    pub fn track_style(mut self, style: &Style) -> Self {
        self.track = self.track.extend(style);
        self
    }

    /// 改圆钮的样子：打开时的位置是勾选状态下的平移
    pub fn thumb_style(mut self, style: &Style) -> Self {
        self.thumb = self.thumb.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Switch { on, label, disabled, focus, mut changes, view, style, track, thumb, after: _ } = self;
        let field = changes.report(|v| FieldValue::Bool(*v));
        let disabled = Flag::new(disabled);
        let (label, label_text) = if view.is_some() { (None, label.map(reader)) } else { (label, None) };
        let state = SwitchState { on, disabled, focus, shared: Stored::new(SwitchShared { changes, label: label_text }) };
        if let Some(view) = view {
            return view(state).field_handle(field);
        }
        let l = look();
        let press = Press::new();
        let base = disabled.state(press.states(), State::Disabled);
        let states = base.clone().checked(on);
        let knob = part(rect(), &l.switch_thumb, &thumb, &states);
        let rail = part(row(), &l.switch_track, &track, &states).child(knob);

        let mut el = press.attach(row()).accessibility_role(Role::Switch).field_handle(field);
        el = disabled.apply(el);
        if let Some(handle) = focus {
            el = el.focus_handle(handle);
        }
        el = part(el, &l.toggle_root, &style, &states).child(rail);
        if let Some(label) = label {
            let (shown, name) = share(label);
            el = el.accessibility_name(name).child(caption(shown));
        }
        on_activate(el, move |source| state.toggle(source)).accessibility_checked(on)
    }
}

impl View for Switch {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 开关，[`Switch::new`] 的简写
pub fn switch(on: Signal<bool>) -> Switch {
    Switch::new(on)
}
