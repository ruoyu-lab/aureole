//! 基础控件：按钮、输入框（单行、多行、密码）、复选框、单选组、开关、下拉选择、滑块、进度条、数字输入、标签，
//! 日历和日期选择、时间选择、颜色选择、文件选择。
//!
//! - **行为是完整的**：点击、键盘（空格、回车、方向键、翻页键、打字跳转）、焦点和焦点环、禁用、拖动、弹出的列表、
//!   无障碍（角色、状态、数值、操作）都在控件里，用的时候不用自己写事件。
//! - **默认的样子朴素**：只有浅色一套，中性的灰加一个偏蓝的强调色，1 像素细线、小圆角；**没有内置动画**，
//!   状态一变立即换成新的样子。要动画就在样式对象里声明过渡（`.transition(..)`），用不用由应用决定。
//! - **改样子**用普通的链式方法和样式对象（[`aureole_core::Style`]）。`.style(&s)` 改控件本身（最外面的元素），
//!   控件里面的**部位**各有一个方法：`Slider::new(v).track_style(&s).fill_style(&s).thumb_style(&s)`、`Switch::new(on).thumb_style(&s)`、
//!   `Select::new(v).list_style(&s).option_style(&s)`、`Checkbox::new(c).mark_style(&s)`……每个部位按自己的状态换值
//!   （悬停、按下、焦点、禁用，部位相关的勾选、选中、打开），先后规则见 [`aureole_core::Element::style`]。
//! - 控件默认按内容的宽度，不会被竖排容器拉宽；没有内容宽度的（输入框、滑块、进度条）有默认宽度。
//!   按钮、输入框本身就是元素（[`button`]、[`aureole_core::text_field`]、[`aureole_core::text_area`]），有部位的控件是构建器，
//!   上面照样能直接用元素的方法：`checkbox(v).on_click(..)`、`slider(v).width(pct(100))`（[`aureole_core::ElementMethods`]）。
//!   构建器直接放进 `.child(..)`（实现了 [`View`](aureole_core::View)）；要 [`Element`] 时（函数的返回类型、分支）`.build()`。
//! - 有值的控件都有 `.on_change(|值| ..)`（用户每改一次）和 `.on_commit(|值| ..)`（改完了：滑块松手、输入框回车或失去焦点、
//!   数字输入夹进范围之后；复选框、开关、单选组、下拉框每次改都是改完了）。代码里改绑定的信号时都不调用。
//!   处理器也可以收整个 [`aureole_core::ChangeEvent`]（`|e: &ChangeEvent<bool>| ..`），里面还有是怎么改的（指针、键盘、
//!   辅助技术、标签）。基础控件处理完自己的点击、按键都停止传播，外面的可点击元素不会再收到。
//! - **有值的控件都能完全重做界面**：逻辑和界面分开，逻辑在状态类型上（[`CheckboxState`]、[`SwitchState`]、[`RadioState`]、[`SelectState`]、
//!   [`SliderState`]、[`NumberState`]、[`CalendarState`]、[`TimeState`]、[`ColorState`]、[`FileState`]），默认的界面也只用它们。
//!   `.view(|state| ..)` 换掉界面，绑定的信号、`on_change`、`on_commit`、范围、禁用都留着；样式对象只作用于默认的界面。
//!   弹出的控件（日期、时间、颜色选择）换的是弹出的内容。角色、键盘、焦点是界面的事，自己重做时要自己写。
//! - `on_change` 只发给控件自己；表单要一处收到所有控件的改动，在外面的容器上用
//!   [`aureole_core::Element::on_field_change`]：每次用户改了值，控件往外冒泡一次表单级的改值汇总（名字、焦点句柄、新值的描述）。
//! - [`label`] 是标签：点它作用到对应的控件上（获得焦点；复选框、开关切换，下拉框打开，按钮被点），
//!   辅助技术按它念控件的名字。
//! - 新的控件也放在这里：一个控件一个文件，默认的样子加进 `look.rs`，值的事件照同样的约定；有值的控件要有状态类型和 `.view`，默认的界面只用状态类型的公开方法。
//!
//! ```no_run
//! # use aureole::prelude::*;
//! let (name, agree, volume) = (signal(String::new()), signal(false), signal(0.6f32));
//! let name_focus = FocusHandle::new();
//! column().gap(12)
//!     .child(label("名字").target(name_focus))
//!     .child(text_field(name).placeholder("怎么称呼你").focus_handle(name_focus))
//!     .child(checkbox(agree).label("同意条款"))
//!     .child(slider(volume))
//!     .child(button("提交").disabled(move || !agree.get()).on_click(|| println!("提交")));
//! ```

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

/// 给构建器加上元素的方法（[`aureole_core::ElementMethods`]）和 `build`。构建器要有 `after` 字段和 `construct` 方法
macro_rules! element_methods {
    ([$($generics:tt)*] $ty:ty) => {
        impl<$($generics)*> aureole_core::ElementMethods for $ty {
            fn element(mut self, f: impl FnOnce(aureole_core::Element) -> aureole_core::Element + 'static) -> Self {
                self.after.push(Box::new(f));
                self
            }
        }

        impl<$($generics)*> $ty {
            /// 构建成元素。元素的方法在构建器上就能用，一般不用调用它：直接放进 `.child(..)`
            pub fn build(mut self) -> aureole_core::Element {
                let after = std::mem::take(&mut self.after);
                after.into_iter().fold(self.construct(), |el, f| f(el))
            }
        }
    };
}

/// 记下来的元素方法，建好元素时按调用的顺序用上
type ElementCalls = Vec<Box<dyn FnOnce(aureole_core::Element) -> aureole_core::Element>>;

mod button;
mod calendar;
mod checkbox;
mod color_picker;
mod date_picker;
mod file_picker;
mod label;
mod look;
mod number;
mod picker;
mod progress;
mod radio;
mod select;
mod slider;
mod switch;
mod time_picker;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use reactive::{Memo, Signal, memo, signal};

use aureole_core::{Builtin, ChangeEvent, ChangeSource, ClickEvent, Element, FieldHandle, FieldValue, IntoChangeHandler, KeyEvent, Prop, State, States, Style};

pub use button::button;
pub use calendar::{Calendar, CalendarState, CalendarTexts, calendar};
pub use checkbox::{Checkbox, CheckboxState, checkbox};
pub use color_picker::{ColorFormat, ColorPanel, ColorPicker, ColorState, ColorTexts, color_panel_view, color_picker};
pub use date_picker::{DatePicker, date_picker};
pub use file_picker::{FilePicker, FileState, FileTexts, FileValue, file_picker};
pub use label::{Label, label};
pub use number::{NumberField, NumberState, number_field};
pub use picker::PickerValue;
pub use progress::{ProgressBar, progress_bar};
pub use radio::{RadioGroup, RadioState, radio_group};
pub use select::{Select, SelectState, select};
pub use slider::{Slider, SliderState, slider};
pub use switch::{Switch, SwitchState, switch};
pub use time_picker::{TimePicker, TimeState, TimeTexts, time_picker};

/// 基础控件一次导入（也在 `aureole::prelude` 里）
pub mod prelude {
    pub use super::{
        Calendar, CalendarState, CalendarTexts, Checkbox, CheckboxState, ColorFormat, ColorPanel, ColorPicker, ColorState, ColorTexts, DatePicker, FilePicker, FileState, FileTexts, Label, NumberField, NumberState, ProgressBar, RadioGroup, RadioState, Select, SelectState, Slider, SliderState, Switch, SwitchState, TimePicker, TimeState, TimeTexts, button, calendar, checkbox, color_panel_view, color_picker, date_picker, file_picker,
        label, number_field, progress_bar, radio_group, select, slider, switch, time_picker,
    };
}

// ---- 控件共用的小东西 ----

/// 控件处理自己的点击（内置处理器，排在使用的人加的 `on_click` 之后）：没被取消激活（`Builtin::ACTIVATE`）就做 `act`
/// （参数是点击是怎么来的），然后停止传播，外面的可点击元素不再收到这次点击
pub(crate) fn on_activate(el: Element, act: impl Fn(ChangeSource) + 'static) -> Element {
    el.builtin_handlers(|el| {
        el.on_click(move |e: &ClickEvent| {
            if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                act(e.source.into());
            }
            e.stop_propagation();
        })
    })
}

/// 有值的控件共用的改值事件：用户每改一次（`on_change`）、改完了（`on_commit`），都是 [`ChangeEvent`]，带着是怎么改的。
/// 只在用户改动时发，代码里改绑定的信号时不发（控件只在自己处理用户的输入时调用这里）。
///
/// 每改一次还从控件往外冒泡一次表单级的改值汇总（[`FieldHandle`]，外面的容器用 `on_field_change` 收）
pub(crate) struct Changes<T> {
    change: Vec<OnChange<T>>,
    commit: Vec<OnChange<T>>,
    field: Option<(FieldHandle, Describe<T>)>,
}

/// 改值事件的处理器
type OnChange<T> = Rc<dyn Fn(&ChangeEvent<T>)>;

/// 把值说成表单级的改值汇总里的 [`FieldValue`]。放在 `Box` 里：不带捕获的闭包（复选框、开关、滑块、数字输入）不分配
type Describe<T> = Box<dyn Fn(&T) -> FieldValue>;

impl<T> Default for Changes<T> {
    fn default() -> Self {
        Self { change: Vec::new(), commit: Vec::new(), field: None }
    }
}

impl<T: Clone + 'static> Changes<T> {
    pub fn on_change<M>(&mut self, handler: impl IntoChangeHandler<T, M>) {
        self.change.push(handler.into_change_handler());
    }

    pub fn on_commit<M>(&mut self, handler: impl IntoChangeHandler<T, M>) {
        self.commit.push(handler.into_change_handler());
    }

    /// 每次用户改了，往外报告（`describe` 把值说成 [`FieldValue`]）。返回交给控件元素的句柄（`.field_handle(..)`）
    pub fn report(&mut self, describe: impl Fn(&T) -> FieldValue + 'static) -> FieldHandle {
        let handle = FieldHandle::new();
        self.field = Some((handle, Box::new(describe)));
        handle
    }

    /// 用户改了
    pub fn changed(&self, value: T, source: ChangeSource) {
        let event = ChangeEvent { value, source };
        self.change.iter().for_each(|h| h(&event));
        self.notify(&event);
    }

    /// 往外冒泡表单级的改值汇总
    fn notify(&self, event: &ChangeEvent<T>) {
        if let Some((handle, describe)) = &self.field {
            handle.notify(describe(&event.value), event.source);
        }
    }

    /// 改完了
    pub fn committed(&self, value: T, source: ChangeSource) {
        let event = ChangeEvent { value, source };
        self.commit.iter().for_each(|h| h(&event));
    }

    /// 改了，同时也改完了（复选框、开关、单选、下拉框每次改都是改完了）
    pub fn both(&self, value: T, source: ChangeSource) {
        let event = ChangeEvent { value, source };
        self.change.iter().for_each(|h| h(&event));
        self.commit.iter().for_each(|h| h(&event));
        self.notify(&event);
    }
}

/// 控件在捕获阶段截下按键（内置处理器）：`handle` 返回 `true` 表示处理了，停止传播，里面的元素收不到
pub(crate) fn on_key_capture(el: Element, handle: impl Fn(&KeyEvent) -> bool + 'static) -> Element {
    el.builtin_handlers(|el| {
        el.on_key_down_capture(move |e: &KeyEvent| {
            if !e.is_propagation_stopped() && handle(e) {
                e.stop_propagation();
            }
        })
    })
}

/// 控件处理按键（内置处理器，排在使用的人加的 `on_key_down` 之后）：`handle` 返回 `true` 表示处理了，停止传播
pub(crate) fn on_key(el: Element, handle: impl Fn(&KeyEvent) -> bool + 'static) -> Element {
    el.builtin_handlers(|el| {
        el.on_key_down(move |e: &KeyEvent| {
            if !e.is_propagation_stopped() && handle(e) {
                e.stop_propagation();
            }
        })
    })
}

/// 不常变的数据放进信号：状态类型（`CheckboxState`……）里只存这个，才能是 `Copy`，放进闭包不用克隆。
/// 取出来是一份引用计数，回调运行时不占着信号，回调里可以再改别的信号
pub(crate) struct Stored<T: 'static>(Signal<Rc<T>>);

impl<T: 'static> Clone for Stored<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Stored<T> {}

impl<T: 'static> Stored<T> {
    pub fn new(value: T) -> Self {
        Self(signal(Rc::new(value)))
    }

    pub fn get(self) -> Rc<T> {
        self.0.with_untracked(Rc::clone)
    }
}

/// 能按的外壳：一个信号记着悬停、键盘焦点、按下（`Copy`），部位的样式对象读它。全用公开的写法：
/// 外壳上的 `on_hover`、`on_press`、`on_keyboard_focus` 写这个信号，部位的 [`States`] 读它
#[derive(Clone, Copy)]
pub(crate) struct Press {
    bits: Signal<u8>,
}

/// [`Press`] 里的位
const HOVER: u8 = 1;
const FOCUS: u8 = 1 << 1;
const DOWN: u8 = 1 << 2;

impl Press {
    pub fn new() -> Self {
        Self { bits: signal(0) }
    }

    /// 只写不读：元素卸载、作用域释放之后才到的处理器（浮层关上时失去焦点）写一个已释放的信号什么都不做
    fn set(self, bit: u8, on: bool) {
        self.bits.update(|b| if on { *b |= bit } else { *b &= !bit });
    }

    fn reads(self, bit: u8) -> impl Fn() -> bool + 'static {
        move || self.bits.get() & bit != 0
    }

    /// 装到外壳上：记悬停、按下、键盘焦点（和外面再加的 `on_hover`、`on_press` 同时生效），能获得焦点
    pub fn attach(self, el: Element) -> Element {
        self.attach_pointer(el).on_keyboard_focus(move |focused| self.set(FOCUS, focused)).focusable(true)
    }

    /// 只记悬停和按下（单选组的一项：焦点在整组上）
    pub fn attach_pointer(self, el: Element) -> Element {
        el.on_hover(move |inside| self.set(HOVER, inside)).on_press(move |down| self.set(DOWN, down))
    }

    /// 交给样式对象的状态：悬停、焦点、按下（禁用由控件的 [`Flag`] 给）
    pub fn states(self) -> States {
        self.pointer_states().focused(self.reads(FOCUS))
    }

    /// 只有悬停和按下从这里来
    pub fn pointer_states(self) -> States {
        States::new().hovered(self.reads(HOVER)).pressed(self.reads(DOWN))
    }
}

/// 控件的开关量（禁用……）：没给、固定的不建任何响应式节点，绑定的建一个派生值。`Copy`
#[derive(Clone, Copy)]
pub(crate) enum Flag {
    Off,
    On,
    Live(Memo<bool>),
}

impl Flag {
    pub fn new(prop: Option<Prop<bool>>) -> Self {
        match prop {
            None | Some(Prop::Static(false)) => Self::Off,
            Some(Prop::Static(true)) => Self::On,
            Some(Prop::Dynamic(f)) => Self::Live(memo(f)),
        }
    }

    /// 响应式
    #[inline]
    pub fn get(self) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::Live(m) => m.get(),
        }
    }

    /// 交给样式对象
    pub fn state(self, states: States, state: State) -> States {
        match self {
            Self::Off => states.when(state, false),
            Self::On => states.when(state, true),
            Self::Live(m) => states.when(state, m),
        }
    }

    /// 给元素的 `.disabled(..)`；没给时不设
    pub fn apply(self, el: Element) -> Element {
        match self {
            Self::Off => el,
            Self::On => el.disabled(true),
            Self::Live(m) => el.disabled(m),
        }
    }
}

/// 部位：先用默认的样子，再用使用的人给的（按状态合起来，见 [`aureole_core::Element::style`]：
/// `.style(&a).style(&b)` 和 `.style(&(a + &b))` 一样）。使用的人没给时第二次什么都不做
pub(crate) fn part(el: Element, default: &Style, custom: &Style, states: &States) -> Element {
    el.style_with(default, states).style_with(custom, states)
}

/// 属性分成两份（同一份文字既显示出来、又交给辅助技术当名字）：静态值复制，绑定共用一个函数
pub(crate) fn share<T: Clone + 'static>(prop: Prop<T>) -> (Prop<T>, Prop<T>) {
    match prop {
        Prop::Static(v) => (Prop::Static(v.clone()), Prop::Static(v)),
        Prop::Dynamic(f) => {
            let f: Rc<dyn Fn() -> T> = Rc::from(f);
            let g = f.clone();
            (Prop::Dynamic(Box::new(move || f())), Prop::Dynamic(Box::new(move || g())))
        }
    }
}

/// 属性变成读取函数（表单级的改值汇总要念出选项的文字）：静态值复制，绑定直接用
pub(crate) fn reader(prop: Prop<String>) -> Rc<dyn Fn() -> String> {
    match prop {
        Prop::Static(v) => Rc::new(move || v.clone()),
        Prop::Dynamic(f) => Rc::from(f),
    }
}

/// 文字元素，内容是已经转好的属性
pub(crate) fn caption(content: Prop<String>) -> Element {
    aureole_core::text(content)
}

/// 打字跳转：停顿这么久之后重新开始拼
const TYPEAHEAD_RESET: Duration = Duration::from_millis(900);

/// 打字跳转的状态：拼起来的字（停顿 0.9 秒后清空，按按键事件的时间戳，和定时器同一个时钟，测试工具里是虚拟时间）。
/// 连按同一个字时在以它开头的项之间轮流
#[derive(Default)]
pub(crate) struct Typeahead {
    typed: RefCell<String>,
    last: Cell<Duration>,
}

impl Typeahead {
    /// 一次按键：是打字就找到要跳去的项（从 `from` 开始找，到头绕回来）；不是打字、没找到时是 `None`
    pub fn key(&self, e: &KeyEvent, labels: &[String], from: usize) -> Option<usize> {
        if e.modifiers.has_super() || e.modifiers.has_control() || e.modifiers.has_alt() {
            return None;
        }
        let ch = e.text.as_deref().filter(|s| !s.trim().is_empty() && !s.chars().any(char::is_control))?;
        // 停顿太久：重新开始拼
        let now = e.time;
        if now.saturating_sub(self.last.replace(now)) > TYPEAHEAD_RESET {
            self.typed.borrow_mut().clear();
        }
        let mut typed = self.typed.borrow_mut();
        // 连按同一个字：在以它开头的项之间轮流（从下一项找）
        let repeating = !typed.is_empty() && typed.chars().all(|c| ch.chars().all(|d| c == d));
        typed.push_str(ch);
        let (needle, start) = if repeating { (ch.to_lowercase(), from + 1) } else { (typed.to_lowercase(), from) };
        let count = labels.len();
        (0..count).map(|k| (start + k) % count).find(|&i| labels[i].to_lowercase().starts_with(&needle))
    }
}
