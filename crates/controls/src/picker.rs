//! 日期选择、时间选择共用的：能为空的值、"输入框加一个按钮加一个弹出层"的外壳。

use std::rc::Rc;

use reactive::Signal;

use super::look::look;
use super::{Flag, Press, on_activate, on_key_capture, part};
use aureole_core::{
    Date, Dismiss, Element, FieldChangeEvent, FieldHandle, FocusHandle, KeyEvent, Overlay, Placement, PropagationPhase, Role, State, States, Stroke, Style, Time, VectorPath, path, row,
    text_field,
};

/// 日期选择、时间选择绑定的值：可以是值本身（总是有值，不能清除），也可以是 `Option`（能清除）
pub trait PickerValue<T>: Clone + PartialEq + 'static {
    /// 选中的是什么；没有时是 `None`
    fn picked(&self) -> Option<T>;
    /// 选了 `value` 之后绑定的信号里放什么；`value` 是 `None`（清除）而这种值不能为空时是 `None`
    fn from_picked(value: Option<T>) -> Option<Self>;
    /// 能不能清除
    fn clearable() -> bool {
        Self::from_picked(None).is_some()
    }
}

macro_rules! picker_value {
    ($($ty:ty),*) => {$(
        impl PickerValue<$ty> for $ty {
            fn picked(&self) -> Option<$ty> {
                Some(*self)
            }
            fn from_picked(value: Option<$ty>) -> Option<Self> {
                value
            }
        }
        impl PickerValue<$ty> for Option<$ty> {
            fn picked(&self) -> Option<$ty> {
                *self
            }
            fn from_picked(value: Option<$ty>) -> Option<Self> {
                Some(value)
            }
        }
    )*};
}

picker_value!(Date, Time);

/// 按钮里的图标：线条画的（24 的画布）
pub(crate) fn line_icon(shape: VectorPath) -> Element {
    path(shape).size(14, 14).stroke(Stroke::new(1.6).round())
}

/// 外壳要的东西
pub(crate) struct Shell {
    pub style: Style,
    pub input: Style,
    pub button: Style,
    pub disabled: Flag,
    pub focus: FocusHandle,
    pub open: Signal<bool>,
    pub draft: Signal<String>,
    pub placeholder: String,
    /// 输入框打完了（回车、失去焦点）
    pub commit: Rc<dyn Fn()>,
    /// 按钮里放的东西
    pub icon: Element,
    /// 按钮的名字（辅助技术念的）
    pub icon_name: String,
    /// 汇报改动的句柄
    pub report: FieldHandle,
    /// 弹出的内容
    pub popup: Rc<dyn Fn() -> Element>,
    /// 关上时（Esc、点外面）
    pub on_dismiss: Rc<dyn Fn(Dismiss)>,
}

/// 外框里一个输入框、右边一个按钮，下面弹出内容。↓ 打开（焦点进弹出层），Esc 关上（焦点回输入框）
pub(crate) fn field_with_popup(shell: Shell) -> Element {
    let Shell { style, input, button, disabled, focus, open, draft, placeholder, commit, icon, icon_name, report, popup, on_dismiss } = shell;
    let l = look();
    let field = {
        let (blur, submit) = (commit.clone(), commit);
        let el = text_field(draft)
            .placeholder(placeholder)
            .tabular_numbers(true)
            .focus_handle(focus)
            .on_focus(move |focused| {
                if !focused {
                    blur();
                }
            })
            .on_submit(move |_| submit());
        let el = if let Flag::Off = disabled { el } else { disabled.apply(el) };
        part(el, &l.picker_field, &input, &States::new())
    };
    let trigger = {
        let press = Press::new();
        let states = disabled.state(press.pointer_states(), State::Disabled);
        let el = on_activate(disabled.apply(press.attach_pointer(row())).accessibility_role(Role::Button).accessibility_name(icon_name.clone()).name(icon_name), move |_| open.update(|o| *o = !*o));
        part(el, &l.picker_button, &button, &states).child(icon)
    };
    let states = disabled.state(States::new().focused(move || focus.is_focused()), State::Disabled).open(open);
    let el = part(disabled.apply(row()), &l.picker, &style, &states)
        .field_handle(report)
        // 只汇报控件自己的值：中间的输入框打的每个字、弹出层里的控件在捕获阶段截下来
        .on_field_change_capture(|e: &FieldChangeEvent| {
            if e.phase() == PropagationPhase::Capture {
                e.stop_propagation();
            }
        });
    // 弹出层里的按键也沿逻辑上的父元素经过这里：开着时不管
    on_key_capture(el, move |e: &KeyEvent| {
        if e.is("ArrowDown") && !open.get_untracked() && !disabled.get() {
            open.set(true);
            return true;
        }
        false
    })
    .child(field)
    .child(trigger)
    .overlay(
        open,
        Overlay::new(move || popup()).placement(Placement::bottom().start()).offset(4).on_dismiss(move |reason| {
            on_dismiss(reason);
        }),
    )
}
