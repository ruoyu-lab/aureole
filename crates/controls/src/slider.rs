//! 滑块：拖圆钮、点轨道、方向键，有范围和步长，能横着也能竖着。

use std::cell::Cell;
use std::rc::Rc;

use reactive::{Signal, signal};

use super::look::look;
use super::{Changes, Flag, Press, Stored, part};
use aureole_core::{
    PointerKind,
    Align, ChangeSource, DragEvent, Element, FieldValue, IntoChangeHandler, FocusHandle, GesturePhase, IntoProp, KeyEvent, PointerEvent, Prop, RangeValue, Rect, State, Style, View, pct,
    column, rect, row, without_animation,
};

/// 滑块的构建器；[`slider`] 是 0..1 的简写。宽度默认 200，不随父元素拉伸（可以接着 `.width(..)`、
/// `.grow(1)`，按实际宽度换算位置）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let volume = signal(0.5f32);
/// slider(volume);                                                     // 0..1，连续
/// let size = signal(14.0f32);
/// Slider::new(size).range(10.0, 32.0).step(1.0).on_commit(|v| println!("定在 {v}"));
/// Slider::new(volume).vertical(true);                                 // 竖着放：上面是大的
/// // 改样子：粗一点的轨道、方形的圆钮
/// Slider::new(size)
///     .track_style(&Style::new().height(8).radius(4))
///     .fill_style(&Style::new().radius(4).background(Color::hex(0x1f9a6b)))
///     .thumb_style(&Style::new().size(12, 20).radius(3));
/// ```
///
/// 部位：外壳（`.style`）、轨道（[`Slider::track_style`]）、已填的部分（[`Slider::fill_style`]）、圆钮（[`Slider::thumb_style`]）。
/// 圆钮、已填部分的位置由滑块按值算出来，样式对象里的平移不用在它们上面。
/// 默认：灰色的轨道 4 高，已填的部分强调色；16 的白色圆钮加灰色描边，悬停、拖动时描边变强调色；禁用时变淡。没有动画。
/// 状态（各部位一样，都是整个滑块的）：悬停、按下（拖动中）、焦点、禁用。
///
/// 指针：按下就跳到按下的位置，拖动跟手。键盘：← ↓ 减一步，→ ↑ 加一步，Page Up / Page Down 十步，Home / End 到两头。
/// 没有步长时一步是范围的 1%。对辅助技术是滑块：数值、范围、步长（0..1 的念成百分比），能直接设置值、增大减小一步。
pub struct Slider {
    value: Signal<f32>,
    min: f32,
    max: f32,
    step: Option<f32>,
    vertical: bool,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<f32>,
    view: Option<SliderView>,
    style: Style,
    track: Style,
    fill: Style,
    thumb: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 自己重做的界面
type SliderView = Box<dyn FnOnce(SliderState) -> Element>;

/// 滑块的逻辑，不带界面：值、范围、步长、坐标换算成值、方向键、改了之后通知谁。默认的滑块也只用这里的方法。
/// 自己重做界面（[`Slider::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let level = signal(3.0f32);
/// // 五档按钮
/// Slider::new(level).range(1.0, 5.0).step(1.0).view(|state: SliderState| {
///     let dots = (1..=5).map(move |n| {
///         rect()
///             .size(20, 20)
///             .radius(10)
///             .background(move || if state.value() >= n as f32 { Color::hex(0x2563eb) } else { Color::hex(0xd5dae0) })
///             .on_click(move |e: &ClickEvent| state.step_to(n as f32, e.source.into()))
///     });
///     row().gap(6).focusable(true).on_key_down(move |e: &KeyEvent| {
///         if state.handle_key(e) {
///             e.stop_propagation();
///         }
///     }).children(dots)
/// });
/// ```
#[derive(Clone, Copy)]
pub struct SliderState {
    value: Signal<f32>,
    min: f32,
    max: f32,
    step: Option<f32>,
    vertical: bool,
    disabled: Flag,
    focus: Option<FocusHandle>,
    changes: Stored<Changes<f32>>,
}

impl SliderState {
    /// 现在的值，响应式
    pub fn value(self) -> f32 {
        self.value.get()
    }

    /// 最小值
    pub fn min(self) -> f32 {
        self.min
    }

    /// 最大值
    pub fn max(self) -> f32 {
        self.max
    }

    /// 步长；连续的是 `None`
    pub fn step(self) -> Option<f32> {
        self.step
    }

    /// 按键走一步是多少：步长，连续的是范围的 1%
    pub fn unit(self) -> f32 {
        self.step.unwrap_or((self.max - self.min).max(f32::EPSILON) / 100.0)
    }

    /// 值在范围里的比例（0..=1），响应式
    pub fn ratio(self) -> f32 {
        ((self.value.get() - self.min) / (self.max - self.min).max(f32::EPSILON)).clamp(0.0, 1.0)
    }

    /// 是不是竖着放（[`Slider::vertical`]）
    pub fn is_vertical(self) -> bool {
        self.vertical
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// [`Slider::focus_handle`] 给的焦点句柄：放到自己的元素上，点标签才能作用到它
    pub fn focus_handle(self) -> Option<FocusHandle> {
        self.focus
    }

    /// 用户把值改成 `value`（夹进范围、对齐步长），变了才写、才调用 `on_change`，返回有没有变。拖动中每一次移动用它，松手时调用 [`SliderState::commit`]
    pub fn set_value(self, value: f32, source: ChangeSource) -> bool {
        if self.disabled.get() {
            return false;
        }
        let value = value.clamp(self.min, self.max);
        let value = match self.step {
            Some(step) => (self.min + ((value - self.min) / step).round() * step).clamp(self.min, self.max),
            None => value,
        };
        if value == self.value.get_untracked() {
            return false;
        }
        self.value.set(value);
        self.changes.get().changed(value, source);
        true
    }

    /// 按比例（0..=1）改值，见 [`SliderState::set_value`]
    pub fn set_ratio(self, ratio: f32, source: ChangeSource) -> bool {
        self.set_value(self.min + ratio.clamp(0.0, 1.0) * (self.max - self.min).max(f32::EPSILON), source)
    }

    /// 改完了：调用 `on_commit`
    pub fn commit(self, source: ChangeSource) {
        self.changes.get().committed(self.value.get_untracked(), source);
    }

    /// 一步到位（点一下、按键）：改值，变了就算改完了
    pub fn step_to(self, value: f32, source: ChangeSource) {
        if self.set_value(value, source) {
            self.commit(source);
        }
    }

    /// 往上（负数往下）走这么多步，见 [`SliderState::step_to`]
    pub fn nudge(self, steps: f32, source: ChangeSource) {
        self.step_to(self.value.get_untracked() + steps * self.unit(), source);
    }

    /// 处理一个按键：← ↓ 减一步，→ ↑ 加一步，Page Up / Page Down 十步，Home / End 到两头。处理了返回 `true`
    pub fn handle_key(self, e: &KeyEvent) -> bool {
        if e.is("ArrowRight") || e.is("ArrowUp") {
            self.nudge(1.0, ChangeSource::Keyboard);
        } else if e.is("ArrowLeft") || e.is("ArrowDown") {
            self.nudge(-1.0, ChangeSource::Keyboard);
        } else if e.is("PageUp") {
            self.nudge(10.0, ChangeSource::Keyboard);
        } else if e.is("PageDown") {
            self.nudge(-10.0, ChangeSource::Keyboard);
        } else if e.is("Home") {
            self.step_to(self.min, ChangeSource::Keyboard);
        } else if e.is("End") {
            self.step_to(self.max, ChangeSource::Keyboard);
        } else {
            return false;
        }
        true
    }
}

element_methods!([] Slider);

impl Slider {
    /// 绑定到 `value` 的滑块，范围 0..1、连续
    pub fn new(value: Signal<f32>) -> Self {
        Self {
            after: Default::default(),
            value,
            min: 0.0,
            max: 1.0,
            step: None,
            vertical: false,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            track: Style::new(),
            fill: Style::new(),
            thumb: Style::new(),
        }
    }

    /// 取值范围（默认 0..1）
    pub fn range(mut self, min: f32, max: f32) -> Self {
        self.min = min;
        self.max = max.max(min);
        self
    }

    /// 步长：值总是 `min + n * step`
    pub fn step(mut self, step: f32) -> Self {
        self.step = (step > 0.0).then_some(step);
        self
    }

    /// 竖着放（默认横着）：上面是大的，默认宽 16、高 200。方向键还是 ↑ → 加、↓ ← 减
    pub fn vertical(mut self, vertical: bool) -> Self {
        self.vertical = vertical;
        self
    }

    /// 禁用：变淡，不响应指针和键盘，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制焦点；[`super::Label::target`] 用同一个句柄时点标签就让它获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户改了值（拖动中每一次移动、点轨道、按键、辅助技术），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<f32, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：拖动、点按之后松开指针时（值变了才调用），按键每走一步之后，辅助技术设置之后
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<f32, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、禁用都照常工作，逻辑在 [`SliderState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(SliderState) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改外壳的样子（大小、焦点环的圆角）
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改轨道的样子
    pub fn track_style(mut self, style: &Style) -> Self {
        self.track = self.track.extend(style);
        self
    }

    /// 改已填部分的样子
    pub fn fill_style(mut self, style: &Style) -> Self {
        self.fill = self.fill.extend(style);
        self
    }

    /// 改圆钮的样子
    pub fn thumb_style(mut self, style: &Style) -> Self {
        self.thumb = self.thumb.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Slider { value, min, max, step, vertical, disabled, focus, mut changes, view, style, track, fill, thumb, after: _ } = self;
        // 按 f32 最短的十进制写法换成 f64（0.51 报告成 0.51，不是 0.50999999）
        let field = changes.report(|v| FieldValue::Number(v.to_string().parse().unwrap_or(f64::from(*v))));
        let disabled = Flag::new(disabled);
        let state = SliderState { value, min, max, step, vertical, disabled, focus, changes: Stored::new(changes) };
        if let Some(view) = view {
            return view(state).field_handle(field);
        }
        // 用坐标的哪一维：横着是 x，竖着是 y
        let axis = usize::from(vertical);
        let length = move |r: Rect| if vertical { r.height } else { r.width };
        let l = look();
        let ratio = move || state.ratio();
        let bounds = signal(Rect::default());
        let knob = signal(Rect::default());
        let pointer = signal(0.0f32);
        let press = Press::new();
        let states = disabled.state(press.states(), State::Disabled);

        // 元素里的坐标换成值（指针改的）：圆钮中心能走的范围是两头各让出半个圆钮。竖着时上面是大的
        let from_x = move |x: f32| {
            let w = length(knob.get_untracked());
            let travel = (length(bounds.get_untracked()) - w).max(1.0);
            let along = ((x - w / 2.0) / travel).clamp(0.0, 1.0);
            state.set_ratio(if vertical { 1.0 - along } else { along }, ChangeSource::Pointer);
        };
        let unit = state.unit();
        // 圆钮的起点（左边或上边）在外壳里的位置
        let thumb_at = move || {
            let along = if vertical { 1.0 - ratio() } else { ratio() };
            along * (length(bounds.get()) - length(knob.get()))
        };

        let fill_el = part(rect().width(pct(100)).height(pct(100)), &l.slider_fill, &fill, &states);
        // 和轨道一样长，平移到圆钮的中心（只改平移，拖动时不重新布局，只重画）。竖着时从下面长上来
        let fill_part = if vertical {
            fill_el.translation_y(move || thumb_at() + length(knob.get()) / 2.0)
        } else {
            fill_el.translation_x(move || thumb_at() + length(knob.get()) / 2.0 - length(bounds.get()))
        };
        let track_look = if vertical { &l.slider_track_vertical } else { &l.slider_track };
        let track_part = part(row().grow(1), track_look, &track, &states).clip(true).child(fill_part);
        let thumb_el = part(rect(), &l.slider_thumb, &thumb, &states).report_bounds(knob);
        let thumb_part = if vertical { thumb_el.translation_y(thumb_at) } else { thumb_el.translation_x(thumb_at) };
        // 圆钮放在铺满外壳的一层里居中，不占位置
        let thumb_shell = if vertical { column() } else { row() };
        let thumb_layer = thumb_shell.position(0, 0).size(pct(100), pct(100)).align(Align::Center).child(thumb_part);

        let started = Rc::new(Cell::new(0.0f32));
        // 这次按下是手指：还不知道它是要改值还是要滚动外面的列表，等点按完成、横着拖起来才改
        let finger = Rc::new(Cell::new(false));
        let shell = if vertical { column() } else { row() };
        let mut el = press.attach(shell).field_handle(field);
        el = disabled.apply(el);
        if let Some(handle) = focus {
            el = el.focus_handle(handle);
        }
        let slider_look = if vertical { &l.slider_vertical } else { &l.slider };
        let mut el = part(el, slider_look, &style, &states)
            .align(Align::Center)
            .report_bounds(bounds)
            .accessibility_value(move || RangeValue::new(value.get() as f64, min as f64, max as f64).step(unit as f64))
            .on_value_request(move |v| state.step_to(v as f32, ChangeSource::Accessibility))
            .on_pointer_move(move |e: &PointerEvent| pointer.set(e.position[axis]))
            .on_pointer_down({
                let finger = finger.clone();
                move |e: &PointerEvent| {
                    pointer.set(e.position[axis]);
                    finger.set(e.device == PointerKind::Touch);
                }
            })
            // 手指点按完成：跳到抬起的位置（之后的按下反馈算改完）
            .on_pointer_up({
                let finger = finger.clone();
                move |e: &PointerEvent| {
                    if finger.get() {
                        from_x(e.position[axis]);
                    }
                }
            })
            // 鼠标按下就跳到按下的位置；之后拖动跟手；松开时值变了就算改完
            .on_press({
                let finger = finger.clone();
                move |down| {
                    if down {
                        started.set(value.get_untracked());
                        if !finger.get() {
                            from_x(pointer.get_untracked());
                        }
                    } else if value.get_untracked() != started.get() {
                        state.commit(ChangeSource::Pointer);
                    }
                }
            })
            .on_drag(move |e: &DragEvent| {
                // 手指沿另一个方向拖：让给外面的列表
                if e.phase == GesturePhase::Start && finger.get() && e.translation[1 - axis].abs() > e.translation[axis].abs() {
                    e.decline_gesture();
                    return;
                }
                if matches!(e.phase, GesturePhase::Start | GesturePhase::Move) {
                    without_animation(|| from_x(e.position[axis]));
                }
            })
            .builtin_handlers(|el| el.on_key_down(move |e: &KeyEvent| {
                if state.handle_key(e) {
                    e.stop_propagation();
                }
            }))
            .child(track_part)
            .child(thumb_layer);
        if min == 0.0 && max == 1.0 {
            el = el.accessibility_value_text(move || format!("{:.0}%", value.get() * 100.0));
        }
        el
    }
}

impl View for Slider {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 0..1 的连续滑块，[`Slider::new`] 的简写
pub fn slider(value: Signal<f32>) -> Slider {
    Slider::new(value)
}
