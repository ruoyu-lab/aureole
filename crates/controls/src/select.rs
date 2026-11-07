//! 下拉选择：点开（或按 ↓ ↑、回车、空格）弹出选项列表，↑ ↓ 移动、回车选中、Esc 关闭，打字跳到以这些字开头的选项。

use std::rc::Rc;

use reactive::{Signal, signal, untracked};

use super::look::look;
use super::{Changes, Flag, Press, Stored, Typeahead, part};
use aureole_core::{
    Builtin, ChangeSource, ClickEvent, Direction, Dismiss, Element, FieldValue, FocusHandle, FocusRing, IntoProp, KeyEvent, Overlay, VectorPath, Placement, Prop, Rect, Role, State, States, Stroke, Style, View, column, path, row, text, IntoChangeHandler,
};

/// 下拉选择的构建器；[`select`] 是简写。按钮按当前文字的宽度（至少 120），弹出的列表至少和按钮一样宽，
/// 比 300 高时在里面滚动。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let city = signal("hz");
/// Select::new(city).option("bj", "北京").option("sh", "上海").option("hz", "杭州").placeholder("选择城市");
/// let fruit = signal(String::from("苹果"));
/// select(fruit, ["苹果", "香蕉", "橙子"].map(|f| (f.to_string(), f)));
/// // 改样子：圆一点的列表，键盘、指针所在的项用灰底
/// let list = Style::new().radius(10).padding(6);
/// let option = Style::new().radius(6).focused(|s| s.background(Color::hex(0xf0f2f5)));
/// Select::new(city).option("bj", "北京").option("hz", "杭州").list_style(&list).option_style(&option);
/// ```
///
/// 部位：按钮（`.style`）、箭头（[`Select::arrow_style`]）、弹出的列表（[`Select::list_style`]）、每个选项（[`Select::option_style`]）、
/// 选中项旁边的勾（[`Select::mark_style`]）。默认：按钮像输入框（白底灰色细线，打开时描边强调色）；列表白底、细线、
/// 一层很淡的投影、内边距 4；选项高 28，键盘或指针所在的项浅蓝底，当前选中的项文字强调色、右边一个勾。没有动画。
/// 状态：按钮有悬停、按下、焦点、禁用和打开；选项有悬停、按下、焦点（键盘或指针所在的那一项）、选中（当前的值）。
///
/// 键盘：焦点在按钮上时 ↓ ↑、回车、空格打开；打开后 ↑ ↓ 移动，Home / End 到两头，回车选中，Esc 关闭（焦点回到按钮）。
/// 打字跳到以打出的字开头的选项（停顿 0.9 秒后重新开始拼；连按同一个字在以它开头的选项之间轮流）；关着时打字直接选中。
///
/// 对辅助技术：按钮是带展开状态的下拉框，值是选中项的文字（名字用 `.accessibility_name(..)` 或 [`super::label`] 起）；
/// 弹出的是列表，每项带选中状态。
pub struct Select<V: 'static> {
    value: Signal<V>,
    options: Vec<(V, String)>,
    placeholder: String,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<V>,
    view: Option<Box<dyn FnOnce(SelectState<V>) -> Element>>,
    style: Style,
    arrow: Style,
    list: Style,
    option: Style,
    mark: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 下拉选择的逻辑，不带界面：有哪些选项、选的哪个、开没开、打字跳到哪一项、选了之后通知谁。默认的下拉选择也只用这里的方法。
/// 自己重做界面（[`Select::view`]）时绑定的信号、`on_change`、`on_commit`、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let city = signal("hz");
/// // 按钮加一排卡片
/// Select::new(city).option("bj", "北京").option("hz", "杭州").view(|state: SelectState<&'static str>| {
///     let cards = (0..state.count()).map(move |i| {
///         button(state.label(i)).on_click(move |e: &ClickEvent| {
///             state.choose(i, e.source.into());
///             state.close(true);
///         })
///     });
///     let list = row().gap(6).children(cards);
///     button(text(move || state.shown_text())).focus_handle(state.focus_handle()).on_click(move || state.toggle_open())
///         .overlay(state.open_signal(), Overlay::new(move || column().child(text("选一个"))).on_dismiss(move |_| state.close(false)))
///         .child(list)
/// });
/// ```
pub struct SelectState<V: 'static> {
    value: Signal<V>,
    open: Signal<bool>,
    disabled: Flag,
    focus: FocusHandle,
    shared: Stored<SelectShared<V>>,
}

impl<V: 'static> Clone for SelectState<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for SelectState<V> {}

struct SelectShared<V> {
    values: Rc<[V]>,
    labels: Rc<[String]>,
    placeholder: String,
    changes: Changes<V>,
    typing: Typeahead,
}

impl<V: Clone + PartialEq + 'static> SelectState<V> {
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
        self.shared.get().labels.get(index).cloned().unwrap_or_default()
    }

    /// 现在的值，响应式
    pub fn value(self) -> V {
        self.value.get()
    }

    /// 现在选的是第几个，响应式；当前的值不在选项里时是 `None`
    pub fn selected_index(self) -> Option<usize> {
        let shared = self.shared.get();
        self.value.with(|current| shared.values.iter().position(|v| v == current))
    }

    /// 按钮上该显示的文字（当前项的文字，不在选项里时是 [`Select::placeholder`]），响应式
    pub fn shown_text(self) -> String {
        let shared = self.shared.get();
        self.selected_index().map_or_else(|| shared.placeholder.clone(), |i| shared.labels[i].clone())
    }

    /// 开没开，响应式
    pub fn is_open(self) -> bool {
        self.open.get()
    }

    /// 开没开的信号：交给 [`Element::overlay`]
    pub fn open_signal(self) -> Signal<bool> {
        self.open
    }

    /// 打开。禁用时什么都不做
    pub fn open(self) {
        if !self.disabled.get() {
            self.open.set(true);
        }
    }

    /// 关上。`return_focus`：焦点回到按钮（在外面点了别的东西时不要抢焦点）
    pub fn close(self, return_focus: bool) {
        self.open.set(false);
        if return_focus {
            self.focus.focus();
        }
    }

    /// 开着就关上，关着就打开
    pub fn toggle_open(self) {
        if self.open.get_untracked() {
            self.close(true);
        } else {
            self.open();
        }
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// 按钮的焦点句柄：放到自己的元素上（[`Select::focus_handle`] 给了的话就是它）
    pub fn focus_handle(self) -> FocusHandle {
        self.focus
    }

    /// 用户选了第 `index` 个，不关上。禁用时、值没变时什么都不做
    pub fn choose(self, index: usize, source: ChangeSource) {
        if self.disabled.get() {
            return;
        }
        let shared = self.shared.get();
        let Some(next) = shared.values.get(index) else { return };
        if self.value.with_untracked(|current| current != next) {
            self.value.set(next.clone());
            shared.changes.both(next.clone(), source);
        }
    }

    /// 打字跳转：这个按键是打字时，返回该跳去第几项（从第 `from` 项开始找，到头绕回来；停顿 0.9 秒后重新开始拼，连按同一个字在以它开头的项之间轮流）
    pub fn type_ahead(self, e: &KeyEvent, from: usize) -> Option<usize> {
        let shared = self.shared.get();
        shared.typing.key(e, &shared.labels, from)
    }
}

/// 箭头和勾（24 的画布）
fn chevron() -> VectorPath {
    VectorPath::svg("M6 9 L12 15 L18 9").view_box(0.0, 0.0, 24.0, 24.0)
}

fn check() -> VectorPath {
    VectorPath::svg("M5 12.5 L10 17 L19 7").view_box(0.0, 0.0, 24.0, 24.0)
}

element_methods!([V: Clone + PartialEq + 'static] Select<V>);

impl<V: Clone + PartialEq + 'static> Select<V> {
    /// 绑定到 `value` 的下拉选择，用 [`Self::option`] 加选项
    pub fn new(value: Signal<V>) -> Self {
        Self {
            after: Default::default(),
            value,
            options: Vec::new(),
            placeholder: "请选择".into(),
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            arrow: Style::new(),
            list: Style::new(),
            option: Style::new(),
            mark: Style::new(),
        }
    }

    /// 加一个选项：值和显示的文字
    pub fn option(mut self, value: V, label: impl Into<String>) -> Self {
        self.options.push((value, label.into()));
        self
    }

    /// 当前的值不在选项里时显示的文字（默认"请选择"）
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// 禁用：变淡，不能打开，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制按钮的焦点；[`super::Label::target`] 用同一个句柄时点标签就打开它
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户选了另一项（点选项、回车、关着时打字、辅助技术），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 选完了：每次选都是完成的，和 [`Select::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭（弹不弹出、怎么排都自己定），绑定的信号、`on_change`、`on_commit`、禁用都照常工作，
    /// 逻辑在 [`SelectState`] 上。样式对象的那些方法这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl FnOnce(SelectState<V>) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改按钮的样子：打开状态是列表弹出着
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改箭头的样子（打开状态是列表弹出着，可以转过来）
    pub fn arrow_style(mut self, style: &Style) -> Self {
        self.arrow = self.arrow.extend(style);
        self
    }

    /// 改弹出的列表的样子
    pub fn list_style(mut self, style: &Style) -> Self {
        self.list = self.list.extend(style);
        self
    }

    /// 改每个选项的样子：焦点状态是键盘或指针所在的那一项，选中状态是当前的值
    pub fn option_style(mut self, style: &Style) -> Self {
        self.option = self.option.extend(style);
        self
    }

    /// 改选中项旁边的勾：选中状态是"这一项是当前的值"，默认平时透明度 0、选中时 1
    pub fn mark_style(mut self, style: &Style) -> Self {
        self.mark = self.mark.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Select { value, options, placeholder, disabled, focus, mut changes, view, style, arrow, list, option, mark, after: _ } = self;
        let labels: Rc<[String]> = options.iter().map(|(_, label)| label.clone()).collect();
        let values: Rc<[V]> = options.into_iter().map(|(v, _)| v).collect();
        let field = {
            let (labels, values) = (labels.clone(), values.clone());
            changes.report(move |v: &V| {
                let index = values.iter().position(|o| o == v).unwrap_or(0);
                FieldValue::Choice { index, text: labels.get(index).cloned().unwrap_or_default() }
            })
        };
        let open = signal(false);
        let trigger = focus.unwrap_or_default();
        let disabled = Flag::new(disabled);
        let state = SelectState { value, open, disabled, focus: trigger, shared: Stored::new(SelectShared { values, labels, placeholder, changes, typing: Typeahead::default() }) };
        if let Some(view) = view {
            return view(state).field_handle(field);
        }
        let l = look();
        let anchor = signal(Rect::default());
        let press = Press::new();
        let popup = {
            let parts = Rc::new(Parts { list: l.select_list.clone().extend(&list), option: l.select_option.clone().extend(&option), mark: l.select_mark.clone().extend(&mark) });
            move || option_list(&parts, state, anchor)
        };
        let typed = move |e: &KeyEvent| {
            let from = untracked(|| state.selected_index()).unwrap_or(0);
            match state.type_ahead(e, from) {
                Some(i) => {
                    state.choose(i, ChangeSource::Keyboard);
                    true
                }
                None => false,
            }
        };

        let base = disabled.state(press.states(), State::Disabled).open(open);
        // 有值时不写颜色，跟着按钮的样式对象走；只有占位的文字用自己的颜色
        let label = row()
            .grow(1)
            .min_width(0)
            .child_when(move || state.selected_index().is_some(), move || text(move || state.shown_text()).max_lines(1))
            .child_when(move || state.selected_index().is_none(), move || text(move || state.shown_text()).max_lines(1).color(|| aureole_core::defaults::palette().placeholder));
        let arrow = part(path(chevron()).stroke(Stroke::new(2.0).round()), &l.select_arrow, &arrow, &base);
        let mut el = press.attach(row()).accessibility_role(Role::ComboBox).field_handle(field);
        el = disabled.apply(el);
        part(el, &l.select, &style, &base)
            .report_bounds(anchor)
            .focus_handle(trigger)
            .accessibility_expanded(open)
            .accessibility_value_text(move || state.shown_text())
            .builtin_handlers(|el| {
                el.on_click(move |e: &ClickEvent| {
                    if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                        open.update(|o| *o = !*o);
                    }
                    e.stop_propagation();
                })
                .on_key_down(move |e: &KeyEvent| {
                    if e.is("ArrowDown") || e.is("ArrowUp") {
                        open.set(true);
                        e.stop_propagation();
                    } else if typed(e) {
                        e.stop_propagation();
                    }
                })
            })
            .child(label)
            .child(arrow)
            .overlay(open, Overlay::new(popup).placement(Placement::bottom().start()).offset(4).on_dismiss(move |reason| state.close(reason != Dismiss::Outside)))
    }
}

impl<V: Clone + PartialEq + 'static> View for Select<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 下拉选择的简写：`select(value, [(值, "文字"), ..])`
pub fn select<V: Clone + PartialEq + 'static, L: Into<String>>(value: Signal<V>, options: impl IntoIterator<Item = (V, L)>) -> Select<V> {
    options.into_iter().fold(Select::new(value), |s, (v, l)| s.option(v, l))
}

/// 打的是一个字（打字跳转用的）：有文字、不是控制字符，没按 Command、Ctrl、Alt
fn typed_char(e: &KeyEvent) -> bool {
    let plain = !(e.modifiers.has_super() || e.modifiers.has_control() || e.modifiers.has_alt());
    plain && e.text.as_deref().is_some_and(|t| !t.trim().is_empty() && !t.chars().any(char::is_control))
}

/// 弹出列表的几个部位（默认的加上使用的人给的，已经合好）
struct Parts {
    list: Style,
    option: Style,
    mark: Style,
}

/// 弹出的选项列表。打开时焦点在当前选中的那项上；指针经过也把焦点移过去（键盘和指针不会各亮一项）
fn option_list<V: Clone + PartialEq + 'static>(parts: &Parts, state: SelectState<V>, anchor: Signal<Rect>) -> Element {
    let labels: Vec<String> = (0..state.count()).map(|i| state.label(i)).collect();
    let selected = state.selected_index();
    let handles: Rc<[FocusHandle]> = labels.iter().map(|_| FocusHandle::new()).collect();
    let active = signal(selected.unwrap_or(0));
    let mut list = column().focus_arrows(Direction::Column).accessibility_role(Role::ListBox);
    for (i, label) in labels.iter().enumerate() {
        let focus = handles[i];
        let chosen = Some(i) == selected;
        let states = States::new().focused(move || focus.is_focused()).selected(chosen);
        let tick = path(check()).size(12, 12).shrink(0).stroke(Stroke::new(2.4).round()).style_with(&parts.mark, &states);
        let item = row()
            .name(label.clone())
            .accessibility_role(Role::ListBoxOption)
            .accessibility_name(label.clone())
            .accessibility_selected(chosen)
            .focus_handle(focus)
            .focus_ring(FocusRing::NONE)
            .on_hover(move |inside| {
                if inside {
                    focus.focus();
                }
            })
            .on_focus(move |f| {
                if f {
                    active.set(i);
                }
            })
            .builtin_handlers(|el| {
                el.on_click(move |e: &ClickEvent| {
                    if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                        state.choose(i, e.source.into());
                        state.close(true);
                    }
                    e.stop_propagation();
                })
            });
        list = list.child(item.style_with(&parts.option, &states).child(text(label.clone()).grow(1).max_lines(1)).child(tick));
    }
    let first = handles.clone();
    let body = list.on_mount(move || {
        if let Some(focus) = first.get(active.get_untracked()) {
            focus.focus();
        }
    });
    column()
        .name("select-list")
        .style(&parts.list)
        .min_width(move || anchor.get().width)
        .builtin_handlers(|el| el.on_key_down(move |e: &KeyEvent| {
            let last = handles.len().saturating_sub(1);
            let target = if e.is("Home") {
                Some(0)
            } else if e.is("End") {
                Some(last)
            } else {
                state.type_ahead(e, active.get_untracked())
            };
            if let Some(focus) = target.and_then(|i| handles.get(i)) {
                focus.focus();
                e.stop_propagation();
            } else if typed_char(e) {
                // 打字跳转没找到也算列表处理过：列表里的按键会冒泡回下拉框，不让它再拿去跳一次
                e.stop_propagation();
            }
        }))
        .child(body)
}
