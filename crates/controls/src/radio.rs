//! 单选组：一组选项里只能选一个，整组一个焦点，方向键切换。

use std::rc::Rc;

use reactive::Signal;

use super::look::look;
use super::{Changes, Flag, Press, Stored, caption, on_activate, on_key, part, reader, share};
use aureole_core::{ChangeSource, Color, Direction, Element, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, KeyEvent, Prop, Role, State, Style, View, column, rect, row};

/// 单选组的构建器；[`radio_group`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// #[derive(Clone, Copy, PartialEq)]
/// enum Size { S, M, L }
/// let size = signal(Size::M);
/// RadioGroup::new(size).option(Size::S, "小").option(Size::M, "中").option(Size::L, "大").direction(Direction::Row);
/// // 改样子：选中的一项文字加粗
/// RadioGroup::new(size).option(Size::S, "小").option(Size::L, "大").item_style(&Style::new().checked(|s| s.font_weight(600)));
/// ```
///
/// 部位：整组（`.style`）、每一项（[`RadioGroup::item_style`]，圆和文字排成的一行）、圆（[`RadioGroup::indicator_style`]）、
/// 中间的圆点（[`RadioGroup::mark_style`]）。默认：16 的圆，白底灰色描边；选中时强调色实底、中间 6 的白色圆点。
/// 状态：每一项有自己的悬停、按下，整组的焦点、禁用，勾选是"这一项是选中的"。
///
/// 键盘：整组是一个焦点（Tab 走到整组），↑ ↓ ← → 切换选项（到头绕回来）。
/// 对辅助技术是单选组，每项是带勾选状态的单选按钮（名字是选项的文字），也能直接"按下"某一项。
pub struct RadioGroup<V: 'static> {
    selected: Signal<V>,
    options: Vec<(V, Prop<String>)>,
    direction: Direction,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<V>,
    view: Option<Box<dyn FnOnce(RadioState<V>) -> Element>>,
    style: Style,
    item: Style,
    indicator: Style,
    mark: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 单选组的逻辑，不带界面：有哪些选项、选的哪个、方向键怎么走、选了之后通知谁。默认的单选组也只用这里的方法。
/// 自己重做界面（[`RadioGroup::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let plan = signal("free");
/// // 分段按钮的样子
/// RadioGroup::new(plan).option("free", "免费版").option("pro", "专业版").view(|state: RadioState<&'static str>| {
///     let segments = (0..state.count()).map(move |i| {
///         button(state.label(i))
///             .background(move || if state.is_selected(i) { Color::hex(0x2563eb) } else { Color::WHITE })
///             .on_click(move |e: &ClickEvent| state.choose(i, e.source.into()))
///     });
///     row().focusable(true).on_key_down(move |e: &KeyEvent| {
///         if state.handle_key(e) {
///             e.stop_propagation();
///         }
///     }).children(segments)
/// });
/// ```
pub struct RadioState<V: 'static> {
    selected: Signal<V>,
    disabled: Flag,
    direction: Direction,
    focus: Option<FocusHandle>,
    shared: Stored<RadioShared<V>>,
}

impl<V: 'static> Clone for RadioState<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for RadioState<V> {}

struct RadioShared<V> {
    values: Rc<Vec<V>>,
    labels: Vec<Rc<dyn Fn() -> String>>,
    changes: Changes<V>,
}

impl<V: Clone + PartialEq + 'static> RadioState<V> {
    /// 有几个选项
    pub fn count(self) -> usize {
        self.shared.get().values.len()
    }

    /// 第 `index` 个选项的值
    pub fn value_at(self, index: usize) -> Option<V> {
        self.shared.get().values.get(index).cloned()
    }

    /// 第 `index` 个选项的文字
    pub fn label(self, index: usize) -> String {
        self.shared.get().labels.get(index).map(|label| label()).unwrap_or_default()
    }

    /// 现在的值，响应式
    pub fn value(self) -> V {
        self.selected.get()
    }

    /// 现在选的是第几个，响应式；当前的值不在选项里时是 `None`
    pub fn selected_index(self) -> Option<usize> {
        let shared = self.shared.get();
        self.selected.with(|current| shared.values.iter().position(|v| v == current))
    }

    /// 第 `index` 个是不是选中的，响应式
    pub fn is_selected(self, index: usize) -> bool {
        let shared = self.shared.get();
        self.selected.with(|current| shared.values.get(index) == Some(current))
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// 选项怎么排（[`RadioGroup::direction`]）
    pub fn direction(self) -> Direction {
        self.direction
    }

    /// [`RadioGroup::focus_handle`] 给的焦点句柄：放到自己的元素上，点标签才能作用到它
    pub fn focus_handle(self) -> Option<FocusHandle> {
        self.focus
    }

    /// 用户选了第 `index` 个。禁用时、值没变时什么都不做
    pub fn choose(self, index: usize, source: ChangeSource) {
        if self.disabled.get() {
            return;
        }
        let shared = self.shared.get();
        let Some(value) = shared.values.get(index) else { return };
        if self.selected.with_untracked(|current| current != value) {
            self.selected.set(value.clone());
            shared.changes.both(value.clone(), source);
        }
    }

    /// 往后（负数往前）走这么多项，到头绕回来。当前的值不在选项里时选第一项
    pub fn move_by(self, delta: isize, source: ChangeSource) {
        let count = self.count() as isize;
        if count == 0 {
            return;
        }
        let shared = self.shared.get();
        let current = self.selected.with_untracked(|s| shared.values.iter().position(|v| v == s)).map_or(-1, |i| i as isize);
        let next = if current < 0 { 0 } else { (current + delta).rem_euclid(count) };
        self.choose(next as usize, source);
    }

    /// 处理一个按键：↓ → 下一项，↑ ← 上一项。处理了返回 `true`（不认识的键返回 `false`，让它接着传）
    pub fn handle_key(self, e: &KeyEvent) -> bool {
        if e.is("ArrowDown") || e.is("ArrowRight") {
            self.move_by(1, ChangeSource::Keyboard);
        } else if e.is("ArrowUp") || e.is("ArrowLeft") {
            self.move_by(-1, ChangeSource::Keyboard);
        } else {
            return false;
        }
        true
    }
}

element_methods!([V: Clone + PartialEq + 'static] RadioGroup<V>);

impl<V: Clone + PartialEq + 'static> RadioGroup<V> {
    /// 绑定到 `selected` 的单选组，用 [`Self::option`] 加选项
    pub fn new(selected: Signal<V>) -> Self {
        Self {
            after: Default::default(),
            selected,
            options: Vec::new(),
            direction: Direction::Column,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            item: Style::new(),
            indicator: Style::new(),
            mark: Style::new(),
        }
    }

    /// 加一个选项：值和显示的文字
    pub fn option<S: Into<String> + 'static>(mut self, value: V, label: impl IntoProp<S>) -> Self {
        self.options.push((value, label.into_prop().map(S::into)));
        self
    }

    /// 选项怎么排：`Direction::Row` 横着排，`Direction::Column` 竖着排（默认）
    pub fn direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// 禁用：整组变淡，不能切换，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制整组的焦点；[`super::Label::target`] 用同一个句柄时点标签就让整组获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户换了选项（点击、方向键、辅助技术），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 换选项完成：每次换都是完成的，和 [`RadioGroup::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、禁用都照常工作，逻辑在 [`RadioState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(RadioState<V>) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改整组的样子（间距、排列）
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改每一项（圆和文字排成的一行）的样子：勾选状态是"这一项是选中的"
    pub fn item_style(mut self, style: &Style) -> Self {
        self.item = self.item.extend(style);
        self
    }

    /// 改圆的样子
    pub fn indicator_style(mut self, style: &Style) -> Self {
        self.indicator = self.indicator.extend(style);
        self
    }

    /// 改中间圆点的样子：默认平时透明度 0、勾选时 1
    pub fn mark_style(mut self, style: &Style) -> Self {
        self.mark = self.mark.extend(style);
        self
    }

    fn construct(self) -> Element {
        let RadioGroup { selected, options, direction, disabled, focus, mut changes, view, style, item, indicator, mark, after: _ } = self;
        let disabled = Flag::new(disabled);
        let values: Rc<Vec<V>> = Rc::new(options.iter().map(|(v, _)| v.clone()).collect());
        // 每一项的文字分一份出来：表单级的改值汇总念出选中项的文字
        let (options, texts): (Vec<_>, Vec<_>) = options
            .into_iter()
            .map(|(v, label)| {
                let (label, text) = share(label);
                ((v, label), reader(text))
            })
            .unzip();
        let field = {
            let (values, texts) = (values.clone(), texts.clone());
            changes.report(move |v: &V| {
                let index = values.iter().position(|o| o == v).unwrap_or(0);
                FieldValue::Choice { index, text: texts.get(index).map(|text| text()).unwrap_or_default() }
            })
        };
        let state = RadioState { selected, disabled, direction, focus, shared: Stored::new(RadioShared { values, labels: texts, changes }) };
        if let Some(view) = view {
            return view(state).field_handle(field);
        }
        let l = look();
        let press = Press::new();
        let group_states = disabled.state(press.states(), State::Disabled);
        let mut group = disabled.apply(press.attach(column()).accessibility_role(Role::RadioGroup).field_handle(field));
        if let Some(handle) = focus {
            group = group.focus_handle(handle);
        }
        let horizontal = direction == Direction::Row;
        group = part(group, if horizontal { &l.radio_row } else { &l.radio_group }, &style, &group_states);
        for (index, (_, label)) in options.into_iter().enumerate() {
            let is = move || state.is_selected(index);
            let own = Press::new();
            let states = disabled.state(own.pointer_states(), State::Disabled).checked(is);
            let dot = part(rect().size(6, 6).radius(3).background(Color::WHITE), &l.mark, &mark, &states);
            let circle = part(row(), &l.radio_circle, &indicator, &states).child(dot);
            let (shown, name) = share(label);
            let row = own.attach_pointer(row()).accessibility_role(Role::RadioButton).accessibility_name(name).accessibility_checked(is);
            group = group.child(on_activate(part(row, &l.radio_item, &item, &states), move |source| state.choose(index, source)).child(circle).child(caption(shown)));
        }
        on_key(group, move |e: &KeyEvent| state.handle_key(e))
    }
}

impl<V: Clone + PartialEq + 'static> View for RadioGroup<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 单选组的简写：`radio_group(size, [(Size::S, "小"), (Size::M, "中")])`
pub fn radio_group<V: Clone + PartialEq + 'static, L: Into<String>>(
    selected: Signal<V>,
    options: impl IntoIterator<Item = (V, L)>,
) -> RadioGroup<V> {
    options.into_iter().fold(RadioGroup::new(selected), |g, (v, l)| g.option(v, l.into()))
}
