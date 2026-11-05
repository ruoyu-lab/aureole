//! 数字输入：中间能直接输入的数字，两边减号、加号按钮，↑ ↓ 加减，有范围、步长、小数位数。

use std::rc::Rc;

use reactive::{Effect, Signal, signal, untracked};

use super::look::look;
use super::{Changes, Flag, Press, Stored, on_activate, on_key_capture, part};
use aureole_core::{
    ChangeSource, Element, FieldChangeEvent, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, KeyEvent, VectorPath, PropagationPhase, Prop, RangeValue, Role, State, States,
    Stroke, Style, TextAlign, View, path, row, text_field,
};

/// 数字输入的构建器；[`number_field`] 是整数的简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let copies = signal(1.0);
/// NumberField::new(copies).range(1.0, 99.0);
/// let ratio = signal(1.5);
/// NumberField::new(ratio).range(0.5, 3.0).step(0.25).precision(2).on_commit(|v| println!("比例 {v}"));
/// // 改样子：圆一点，两边的按钮浅灰底
/// NumberField::new(copies).style(&Style::new().radius(15)).button_style(&Style::new().background(Color::hex(0xf0f2f5)));
/// ```
///
/// 部位：外框（`.style`）、中间的输入框（[`NumberField::field_style`]）、两边的按钮（[`NumberField::button_style`]）。
/// 默认：和输入框一样的白底灰色细线框（高 30、圆角 4），输入时描边变强调色；两边是 28 见方的按钮，悬停时浅灰底，
/// 到头时那一边禁用（文字变淡）。没有动画。外框的状态：悬停、焦点（中间的输入框有焦点）、禁用；
/// 按钮的状态：悬停、按下、禁用（到头了或整个禁用）。
///
/// 输入：只收数字、小数点和负号；回车或失去焦点时换成数字（夹在范围里，按小数位数取整），不是数字时恢复原来的值。
/// 键盘：输入框里 ↑ ↓ 加减一步，Page Up / Page Down 十步（先按已经打的数字算）。两边的按钮不在 Tab 的顺序里。
///
/// 值只在这几个时候变：回车、失去焦点、点按钮、按 ↑ ↓。变了时先调用 [`NumberField::on_change`] 再调用
/// [`NumberField::on_commit`]（夹进范围之后的值）；代码里改绑定的信号时都不调用。
///
/// 对辅助技术：中间的输入框是数值输入（数值、范围、步长，能增大减小），两边是"减少""增加"按钮。
pub struct NumberField {
    value: Signal<f64>,
    min: f64,
    max: f64,
    step: f64,
    precision: usize,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<f64>,
    view: Option<NumberView>,
    style: Style,
    input: Style,
    button: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 自己重做的界面
type NumberView = Box<dyn FnOnce(NumberState) -> Element>;

/// 数字输入的逻辑，不带界面：值、输入框里打的字、夹进范围、按小数位数取整、加减一步、改了之后通知谁。默认的数字输入也只用这里的方法。
/// 自己重做界面（[`NumberField::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let copies = signal(1.0);
/// // 上下排的按钮
/// NumberField::new(copies).range(1.0, 9.0).view(|state: NumberState| {
///     column()
///         .child(button("▲").disabled(move || !state.can_increase()).on_click(move |e: &ClickEvent| state.step_by(1.0, e.source.into())))
///         .child(text_field(state.draft_signal()).on_submit(move |_| state.commit_text(ChangeSource::Keyboard)))
///         .child(button("▼").disabled(move || !state.can_decrease()).on_click(move |e: &ClickEvent| state.step_by(-1.0, e.source.into())))
/// });
/// ```
#[derive(Clone, Copy)]
pub struct NumberState {
    value: Signal<f64>,
    draft: Signal<String>,
    min: f64,
    max: f64,
    step: f64,
    precision: usize,
    disabled: Flag,
    focus: FocusHandle,
    changes: Stored<Changes<f64>>,
}

impl NumberState {
    /// 现在的值，响应式
    pub fn value(self) -> f64 {
        self.value.get()
    }

    /// 输入框里打的字的信号：绑给输入框。回车、失去焦点时调用 [`NumberState::commit_text`]
    pub fn draft_signal(self) -> Signal<String> {
        self.draft
    }

    /// 最小值
    pub fn min(self) -> f64 {
        self.min
    }

    /// 最大值
    pub fn max(self) -> f64 {
        self.max
    }

    /// 一步是多少
    pub fn step(self) -> f64 {
        self.step
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// 输入框的焦点句柄：放到自己的元素上（[`NumberField::focus_handle`] 给了的话就是它）
    pub fn focus_handle(self) -> FocusHandle {
        self.focus
    }

    /// 还能不能往下减，响应式
    pub fn can_decrease(self) -> bool {
        !self.disabled.get() && self.value.get() > self.min
    }

    /// 还能不能往上加，响应式
    pub fn can_increase(self) -> bool {
        !self.disabled.get() && self.value.get() < self.max
    }

    fn format(self, value: f64) -> String {
        format!("{value:.*}", self.precision)
    }

    /// 用户给了一个值：夹进范围、按小数位数取整，变了才写、才调用 `on_change` 和 `on_commit`。输入框换成整理过的写法。禁用时什么都不做
    pub fn set_value(self, value: f64, source: ChangeSource) {
        if !self.disabled.get() {
            let scale = 10f64.powi(self.precision.min(15) as i32);
            let value = ((value * scale).round() / scale).clamp(self.min, self.max);
            if value != self.value.get_untracked() {
                self.value.set(value);
                self.changes.get().both(value, source);
            }
        }
        self.draft.set(self.format(self.value.get_untracked()));
    }

    /// 已经打了的数字；不是数字时是 `None`
    fn typed(self) -> Option<f64> {
        self.draft.get_untracked().trim().parse::<f64>().ok().filter(|v| v.is_finite())
    }

    /// 读输入框里打的字：不是数字时恢复原来的值
    pub fn commit_text(self, source: ChangeSource) {
        self.set_value(self.typed().unwrap_or_else(|| self.value.get_untracked()), source);
    }

    /// 加减这么多步，先按已经打的数字算
    pub fn step_by(self, steps: f64, source: ChangeSource) {
        self.set_value(self.typed().unwrap_or_else(|| self.value.get_untracked()) + steps * self.step, source);
    }

    /// 处理一个按键：↑ ↓ 加减一步，Page Up / Page Down 十步。处理了返回 `true`
    pub fn handle_key(self, e: &KeyEvent) -> bool {
        let steps = if e.is("ArrowUp") {
            1.0
        } else if e.is("ArrowDown") {
            -1.0
        } else if e.is("PageUp") {
            10.0
        } else if e.is("PageDown") {
            -10.0
        } else {
            return false;
        };
        self.step_by(steps, ChangeSource::Keyboard);
        true
    }
}

fn minus() -> VectorPath {
    VectorPath::svg("M6 12 H18").view_box(0.0, 0.0, 24.0, 24.0)
}

fn plus() -> VectorPath {
    VectorPath::svg("M6 12 H18 M12 6 V18").view_box(0.0, 0.0, 24.0, 24.0)
}

/// 输入框里收的字
fn numeric(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '.' | '-')
}

element_methods!([] NumberField);

impl NumberField {
    /// 绑定到 `value` 的数字输入：整数，每次加减 1，不限范围
    pub fn new(value: Signal<f64>) -> Self {
        Self {
            after: Default::default(),
            value,
            min: f64::MIN,
            max: f64::MAX,
            step: 1.0,
            precision: 0,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            input: Style::new(),
            button: Style::new(),
        }
    }

    /// 取值范围（到头时那边的按钮禁用）
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.min = min;
        self.max = max.max(min);
        self
    }

    /// 按钮、↑ ↓ 每次加减多少（默认 1）
    pub fn step(mut self, step: f64) -> Self {
        self.step = step.abs().max(f64::EPSILON);
        self
    }

    /// 显示几位小数（默认 0）；输入的数字也按它取整
    pub fn precision(mut self, digits: usize) -> Self {
        self.precision = digits;
        self
    }

    /// 禁用：变淡，不能输入和加减，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制中间输入框的焦点；[`super::Label::target`] 用同一个句柄时点标签就让它获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户改了值（回车、失去焦点、按钮、↑ ↓、辅助技术），参数是夹进范围之后的新值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<f64, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：和 [`NumberField::on_change`] 同时调用（值变了的时候）
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<f64, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、禁用都照常工作，逻辑在 [`NumberState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(NumberState) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改外框的样子：焦点状态是中间的输入框有焦点
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改中间输入框的样子（默认宽 56、没有自己的框）
    pub fn field_style(mut self, style: &Style) -> Self {
        self.input = self.input.extend(style);
        self
    }

    /// 改两边按钮的样子
    pub fn button_style(mut self, style: &Style) -> Self {
        self.button = self.button.extend(style);
        self
    }

    fn construct(self) -> Element {
        let NumberField { value, min, max, step, precision, disabled, focus, mut changes, view, style, input, button, after: _ } = self;
        let report = changes.report(|v| FieldValue::Number(*v));
        let disabled = Flag::new(disabled);
        let focus = focus.unwrap_or_default();
        let format = move |v: f64| format!("{v:.precision$}");
        let draft = signal(untracked(|| format(value.get())));
        // 值从外面变了：输入框跟着显示
        Effect::new(move || {
            let shown = format(value.get());
            if untracked(|| draft.get()) != shown {
                draft.set(shown);
            }
        });
        let state = NumberState { value, draft, min, max, step, precision, disabled, focus, changes: Stored::new(changes) };
        if let Some(view) = view {
            return view(state).field_handle(report);
        }
        let l = look();
        let side = |shape: VectorPath, name: &'static str, blocked: Rc<dyn Fn() -> bool>, steps: f64| {
            let press = Press::new();
            let off = blocked.clone();
            let states = press.pointer_states().disabled(move || off());
            let el = on_activate(press.attach_pointer(row()).accessibility_role(Role::Button).accessibility_name(name).name(name).disabled(move || blocked()), move |source| state.step_by(steps, source));
            part(el, &l.number_button, &button, &states).child(path(shape).size(14, 14).stroke(Stroke::new(1.8).round()))
        };
        let can_down: Rc<dyn Fn() -> bool> = Rc::new(move || !state.can_decrease());
        let can_up: Rc<dyn Fn() -> bool> = Rc::new(move || !state.can_increase());
        let field = {
            let el = text_field(draft)
                .accepted_chars(numeric)
                .text_align(TextAlign::Center)
                .tabular_numbers(true)
                .focus_handle(focus)
                .accessibility_role(Role::SpinButton)
                .accessibility_value(move || RangeValue::new(value.get(), min, max).step(step))
                .on_value_request(move |v| state.set_value(v, ChangeSource::Accessibility))
                .on_focus(move |f| {
                    if !f {
                        state.commit_text(ChangeSource::Keyboard);
                    }
                })
                .on_submit(move |_| state.commit_text(ChangeSource::Keyboard));
            part(el, &l.number_field, &input, &States::new())
        };
        let (down, up) = (side(minus(), "减少", can_down, -1.0), side(plus(), "增加", can_up, 1.0));
        let states = disabled.state(States::new().focused(move || focus.is_focused()), State::Disabled);
        // 表单级的改值汇总：只报告数字输入自己的值（夹进范围之后），中间的输入框打的每个字在捕获阶段截下来
        let el = part(disabled.apply(row()), &l.number, &style, &states)
            .clip(true)
            .field_handle(report)
            .on_field_change_capture(|e: &FieldChangeEvent| {
                if e.phase() == PropagationPhase::Capture {
                    e.stop_propagation();
                }
            });
        // ↑ ↓、翻页键在捕获阶段截下来加减：里面的输入框收不到（不当成移动插入点）
        on_key_capture(el, move |e: &KeyEvent| state.handle_key(e)).child(down).child(field).child(up)
    }
}

impl View for NumberField {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 整数的数字输入，每次加减 1，[`NumberField::new`] 的简写
pub fn number_field(value: Signal<f64>) -> NumberField {
    NumberField::new(value)
}
