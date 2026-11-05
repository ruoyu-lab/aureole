//! 时间选择：能直接打时间的输入框，右边的按钮（或 ↓）弹出时、分（可选秒、上下午）几列。

use std::rc::Rc;

use reactive::{Effect, Signal, signal, untracked};

use super::button::button;
use super::look::look;
use super::picker::{PickerValue, Shell, field_with_popup, line_icon};
use super::{Changes, Flag, Press, Stored, part};
use aureole_core::{
    Builtin, ChangeSource, ClickEvent, Direction, Dismiss, Element, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, KeyEvent, Prop, Role, ScrollHandle, State, States, Style, Time, VectorPath, View, column,
    defaults::OVERLAY_SHADOW, local_date_time, row, scroll_y, text,
};

/// 时间选择上的文字。[`TimeTexts::chinese`] 是默认的，[`TimeTexts::english`] 是英文的，也可以只改其中几项
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeTexts {
    /// "上午"
    pub am: String,
    /// "下午"
    pub pm: String,
    /// 上午下午写在时间前面（中文）还是后面（英文）
    pub period_first: bool,
    /// "时"（那一列的名字，辅助技术念的）
    pub hour: String,
    /// "分"
    pub minute: String,
    /// "秒"
    pub second: String,
    /// "上午下午"那一列的名字
    pub period: String,
    /// "现在"
    pub now: String,
    /// "清除"
    pub clear: String,
    /// "确定"
    pub done: String,
    /// 右边按钮的名字
    pub open: String,
}

impl TimeTexts {
    /// 中文（默认）
    pub fn chinese() -> Self {
        Self {
            am: "上午".into(),
            pm: "下午".into(),
            period_first: true,
            hour: "时".into(),
            minute: "分".into(),
            second: "秒".into(),
            period: "上午下午".into(),
            now: "现在".into(),
            clear: "清除".into(),
            done: "确定".into(),
            open: "选择时间".into(),
        }
    }

    /// 英文
    pub fn english() -> Self {
        Self {
            am: "AM".into(),
            pm: "PM".into(),
            period_first: false,
            hour: "Hour".into(),
            minute: "Minute".into(),
            second: "Second".into(),
            period: "AM or PM".into(),
            now: "Now".into(),
            clear: "Clear".into(),
            done: "Done".into(),
            open: "Choose time".into(),
        }
    }
}

impl Default for TimeTexts {
    fn default() -> Self {
        Self::chinese()
    }
}

/// 把时间写成文字
type Format = Rc<dyn Fn(Time) -> String>;
/// 把文字读成时间
type Parse = Rc<dyn Fn(&str) -> Option<Time>>;

/// 时间选择的构建器；[`time_picker`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let at = signal(Time::new(14, 30));
/// time_picker(at);
/// // 五分钟一档，带秒；只能选工作时间；能清除：绑定 Option
/// let start = signal(None::<Time>);
/// TimePicker::new(start)
///     .minute_step(5)
///     .range(Time::new(9, 0), Time::new(18, 0))
///     .on_commit(|t: Option<Time>| println!("定在 {t:?}"));
/// TimePicker::new(at).seconds(true);
/// // 十二小时制
/// TimePicker::new(at).twelve_hour(true).texts(TimeTexts::english());
/// ```
///
/// 部位：外框（`.style`）、输入框（[`TimePicker::field_style`]）、右边的按钮（[`TimePicker::button_style`]）、弹出的面板
/// （[`TimePicker::panel_style`]）、每一列（[`TimePicker::column_style`]）、列里的每一项（[`TimePicker::item_style`]）。
/// 下面的按钮（[`TimePicker::action_style`]）和那一行（[`TimePicker::footer_style`]）。
/// 一项的状态：悬停、按下、焦点、选中（当前的值）、禁用（范围外的）。外框的状态：焦点、打开、禁用。
///
/// 输入：回车或失去焦点时读成时间。认 `14:30`、`14:30:05`、`9:5`、`1430`、`2:30 PM`、`下午2:30`；读不懂时恢复原来的写法，
/// 范围外的夹进范围。键盘：↓ 打开面板，焦点进当前的那一项；面板里 ↑ ↓ 在一列里走（滚进视口），← → 换一列，
/// 回车、空格选中这一项，Esc 恢复打开时的值并关上，"确定"或在外面点一下留下选的值。
///
/// 点一项时值马上变（调用 [`TimePicker::on_change`]），关上面板时算改完（值变了才调用 [`TimePicker::on_commit`]）；
/// 输入框打的字回车、失去焦点时两个都调用。代码里改绑定的信号时都不调用。对辅助技术：输入框是文本框，每一列是列表，每一项带选中状态。
pub struct TimePicker<V> {
    value: Signal<V>,
    min: Option<Time>,
    max: Option<Time>,
    minute_step: u8,
    second_step: u8,
    seconds: bool,
    twelve_hour: bool,
    texts: TimeTexts,
    now_button: bool,
    placeholder: Option<String>,
    format: Option<Format>,
    parse: Option<Parse>,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<V>,
    style: Style,
    input: Style,
    button: Style,
    panel: Style,
    column: Style,
    item: Style,
    action: Style,
    footer: Style,
    view: Option<TimeView>,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 自己重做的弹出内容
type TimeView = Rc<dyn Fn(TimeState) -> Element>;

/// 钟的图标（24 的画布）
fn clock_icon() -> VectorPath {
    VectorPath::svg("M12 3.5 A8.5 8.5 0 1 1 11.99 3.5 M12 7.5 V12 L15 14").view_box(0.0, 0.0, 24.0, 24.0)
}

/// 十二小时制的钟点：1..=12
fn twelve_hour_of(time: Time) -> u8 {
    if time.hour().is_multiple_of(12) { 12 } else { time.hour() % 12 }
}

/// 默认的写法
fn write_time(time: Time, seconds: bool, twelve_hour: bool, texts: &TimeTexts) -> String {
    let second = if seconds { format!(":{:02}", time.second()) } else { String::new() };
    if !twelve_hour {
        return format!("{:02}:{:02}{second}", time.hour(), time.minute());
    }
    let period = if time.hour() < 12 { &texts.am } else { &texts.pm };
    let hour = twelve_hour_of(time);
    let clock = format!("{hour}:{:02}{second}", time.minute());
    if texts.period_first { format!("{period} {clock}") } else { format!("{clock} {period}") }
}

/// 默认的读法：应用换了上午下午的文字时先换回标准写法
fn read_time(text: &str, texts: &TimeTexts) -> Option<Time> {
    let text = text.replace(&texts.am, "am").replace(&texts.pm, "pm");
    text.parse().ok()
}

/// 一列里的一项：显示的文字、点了之后时间怎么变、这一项是不是当前的、换了之后是不是在范围里
struct Item {
    label: String,
    apply: Rc<dyn Fn(Time) -> Time>,
    matches: Rc<dyn Fn(Time) -> bool>,
}

fn item(label: String, apply: impl Fn(Time) -> Time + 'static, matches: impl Fn(Time) -> bool + 'static) -> Item {
    Item { label, apply: Rc::new(apply), matches: Rc::new(matches) }
}

fn set(time: Time, hour: u8, minute: u8, second: u8) -> Time {
    Time::checked(hour, minute, second).unwrap_or(time)
}

/// 时间面板的逻辑，不带界面：现在的时间、能不能选、选了之后通知谁、关上还是恢复。默认的面板也只用这里的方法。
/// 自己重做弹出的内容（[`TimePicker::view`]）时外框的输入框、弹出、`on_change`、`on_commit`、Esc 恢复都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let at = signal(Time::new(9, 0));
/// // 只有几个常用的时间
/// TimePicker::new(at).view(|state: TimeState| {
///     let choices = [Time::new(9, 0), Time::new(12, 0), Time::new(18, 0)].map(|time| {
///         button(time.to_string()).disabled(!state.is_allowed(time)).on_click(move |e: &ClickEvent| {
///             state.pick(time, e.source.into());
///             state.done();
///         })
///     });
///     row().gap(6).children(choices)
/// });
/// ```
#[derive(Clone, Copy)]
pub struct TimeState {
    fallback: Time,
    shared: Stored<TimeShared>,
}

struct TimeShared {
    read: Rc<dyn Fn() -> Option<Time>>,
    write: Rc<dyn Fn(Option<Time>, ChangeSource) -> bool>,
    finish: Rc<dyn Fn(bool)>,
    focus: FocusHandle,
    min: Option<Time>,
    max: Option<Time>,
    minute_step: u8,
    second_step: u8,
    seconds: bool,
    twelve_hour: bool,
    clearable: bool,
    texts: Rc<TimeTexts>,
}

impl TimeState {
    /// 绑定的时间，响应式；没有值时是 `None`
    pub fn value(self) -> Option<Time> {
        (self.shared.get().read)()
    }

    /// 面板上该显示的时间，响应式：没有值时是打开面板时的时间
    pub fn current(self) -> Time {
        self.value().unwrap_or(self.fallback)
    }

    /// 这个时间能不能选（[`TimePicker::range`]）
    pub fn is_allowed(self, time: Time) -> bool {
        let shared = self.shared.get();
        shared.min.is_none_or(|min| time >= min) && shared.max.is_none_or(|max| time <= max)
    }

    /// 用户选了这个时间（夹进范围）。值变了才调用 `on_change`，不关面板；关上时算改完
    pub fn pick(self, time: Time, source: ChangeSource) {
        (self.shared.get().write)(Some(time), source);
    }

    /// 用户选了现在。不带秒时秒是 0
    pub fn pick_now(self, source: ChangeSource) {
        let now = local_date_time().1;
        self.pick(if self.shows_seconds() { now } else { now.with_second(0) }, source);
    }

    /// 用户清除了。绑定的不是 `Option` 时什么都不做
    pub fn clear(self, source: ChangeSource) {
        (self.shared.get().write)(None, source);
    }

    /// 能不能清除：绑定的是 `Option` 时能
    pub fn clearable(self) -> bool {
        self.shared.get().clearable
    }

    /// 选好了：留下选的时间，关上，值变了算改完，焦点回输入框
    pub fn done(self) {
        let shared = self.shared.get();
        (shared.finish)(false);
        shared.focus.focus();
    }

    /// 不要了：恢复打开时的时间，关上，焦点回输入框
    pub fn cancel(self) {
        let shared = self.shared.get();
        (shared.finish)(true);
        shared.focus.focus();
    }

    /// 分钟几分钟一档（[`TimePicker::minute_step`]）
    pub fn minute_step(self) -> u8 {
        self.shared.get().minute_step
    }

    /// 秒几秒一档（[`TimePicker::second_step`]）
    pub fn second_step(self) -> u8 {
        self.shared.get().second_step
    }

    /// 精确到秒吗（[`TimePicker::seconds`]）
    pub fn shows_seconds(self) -> bool {
        self.shared.get().seconds
    }

    /// 十二小时制吗（[`TimePicker::twelve_hour`]）
    pub fn is_twelve_hour(self) -> bool {
        self.shared.get().twelve_hour
    }

    /// 时间选择上的文字
    pub fn texts(self) -> TimeTexts {
        (*self.shared.get().texts).clone()
    }
}

/// 一列的焦点：每一项的句柄、读当前是第几项。← → 换列时焦点去当前的那项
type ColumnFocus = (Rc<[FocusHandle]>, Rc<dyn Fn() -> usize>);

/// 默认的面板里用到的部位
struct PanelParts {
    panel: Style,
    column: Style,
    item: Style,
    action: Style,
    footer: Style,
    now_button: bool,
}

/// 一列
fn time_column(state: TimeState, parts: &PanelParts, name: String, items: Vec<Item>) -> (Element, ColumnFocus) {
    let l = look();
    let handles: Rc<[FocusHandle]> = items.iter().map(|_| FocusHandle::new()).collect();
    let tests: Vec<Rc<dyn Fn(Time) -> bool>> = items.iter().map(|i| i.matches.clone()).collect();
    let selected: Rc<dyn Fn() -> usize> = Rc::new(move || {
        let time = state.current();
        tests.iter().position(|test| test(time)).unwrap_or(0)
    });
    // 左右留一点：焦点环不被滚动的范围裁掉
    let mut list = column().padding_x(3).focus_arrows(Direction::Column).accessibility_role(Role::ListBox).accessibility_name(name.clone());
    for (i, entry) in items.into_iter().enumerate() {
        let press = Press::new();
        let handle = handles[i];
        let Item { label, apply, matches } = entry;
        let chosen = move || matches(state.current());
        let out_of_range = {
            let apply = apply.clone();
            move || !state.is_allowed(apply(state.current()))
        };
        let off = out_of_range.clone();
        let states = press.states().selected(chosen.clone()).when(State::Disabled, off);
        let el = press
            .attach(row())
            .name(format!("{name}-{label}"))
            .accessibility_role(Role::ListBoxOption)
            .accessibility_name(label.clone())
            .accessibility_selected(chosen)
            .focus_handle(handle)
            .disabled(out_of_range)
            .builtin_handlers(|el| {
                el.on_click(move |e: &ClickEvent| {
                    if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                        state.pick(apply(untracked(|| state.current())), e.source.into());
                    }
                    e.stop_propagation();
                })
            });
        list = list.child(part(el, &l.time_item, &parts.item, &states).child(text(label).tabular_numbers(true)));
    }
    // 打开时每一列都露出当前的那项（放在从上往下第四行左右，前后的都看得到）
    let scroll = ScrollHandle::new();
    let at = selected.clone();
    let scroller = part(scroll_y(list).scroll_handle(scroll), &l.time_column, &parts.column, &States::new()).on_mount(move || scroll.jump_to_index(at().saturating_sub(3)));
    (scroller, (handles, selected))
}

/// 默认的面板：只用 [`TimeState`] 的公开方法
fn default_time_panel(state: TimeState, parts: &PanelParts) -> Element {
    let l = look();
    let texts = state.texts();
    let mut columns = Vec::new();
    if state.is_twelve_hour() {
        let hours = [12u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11].map(|hour| {
            item(hour.to_string(), move |t| set(t, hour % 12 + if t.hour() >= 12 { 12 } else { 0 }, t.minute(), t.second()), move |t| twelve_hour_of(t) == hour)
        });
        columns.push(time_column(state, parts, texts.hour.clone(), hours.into_iter().collect()));
    } else {
        let hours = (0..24u8).map(|hour| item(format!("{hour:02}"), move |t| set(t, hour, t.minute(), t.second()), move |t| t.hour() == hour)).collect();
        columns.push(time_column(state, parts, texts.hour.clone(), hours));
    }
    let minutes = (0..60u8).step_by(usize::from(state.minute_step())).map(|minute| item(format!("{minute:02}"), move |t| set(t, t.hour(), minute, t.second()), move |t| t.minute() == minute)).collect();
    columns.push(time_column(state, parts, texts.minute.clone(), minutes));
    if state.shows_seconds() {
        let list = (0..60u8).step_by(usize::from(state.second_step())).map(|second| item(format!("{second:02}"), move |t| set(t, t.hour(), t.minute(), second), move |t| t.second() == second)).collect();
        columns.push(time_column(state, parts, texts.second.clone(), list));
    }
    if state.is_twelve_hour() {
        let periods = vec![
            item(texts.am.clone(), |t| set(t, t.hour() % 12, t.minute(), t.second()), |t| t.hour() < 12),
            item(texts.pm.clone(), |t| set(t, t.hour() % 12 + 12, t.minute(), t.second()), |t| t.hour() >= 12),
        ];
        columns.push(time_column(state, parts, texts.period.clone(), periods));
    }
    let columns_info: Rc<Vec<ColumnFocus>> = Rc::new(columns.iter().map(|(_, focus)| focus.clone()).collect());
    let which: Signal<usize> = signal(0);
    let to_column = {
        let columns_info = columns_info.clone();
        move |index: usize| {
            let (handles, selected) = &columns_info[index];
            which.set(index);
            handles[selected()].focus();
        }
    };
    let mut line = row().gap(2);
    for (element, _) in columns {
        line = line.child(element);
    }
    let focus_first = to_column.clone();
    let count = columns_info.len();
    let line = line.on_mount(move || focus_first(0)).builtin_handlers(|el| {
        el.on_key_down(move |e: &KeyEvent| {
            let at = which.get_untracked();
            if e.is("ArrowLeft") && at > 0 {
                to_column(at - 1);
            } else if e.is("ArrowRight") && at + 1 < count {
                to_column(at + 1);
            } else {
                return;
            }
            e.stop_propagation();
        })
    });
    let mut footer = part(row(), &l.calendar_footer, &parts.footer, &States::new());
    if parts.now_button {
        footer = footer.child(part(button(texts.now.clone()).name(texts.now.clone()), &l.calendar_action, &parts.action, &States::new()).on_click(move |e: &ClickEvent| state.pick_now(e.source.into())));
    }
    if state.clearable() {
        footer = footer.child(part(button(texts.clear.clone()).name(texts.clear.clone()), &l.calendar_action, &parts.action, &States::new()).on_click(move |e: &ClickEvent| state.clear(e.source.into())));
    }
    footer = footer.child(part(button(texts.done.clone()).name(texts.done.clone()), &l.calendar_action, &parts.action, &States::new()).on_click(move || state.done()));
    part(column(), &l.calendar, &parts.panel, &States::new()).name("time-picker-panel").child(line).child(footer)
}

element_methods!([V: PickerValue<Time>] TimePicker<V>);

impl<V: PickerValue<Time>> TimePicker<V> {
    /// 绑定到 `value` 的时间选择，精确到分。`value` 可以是 [`Time`]，也可以是 `Option<Time>`（能清除）
    pub fn new(value: Signal<V>) -> Self {
        Self {
            after: Default::default(),
            value,
            min: None,
            max: None,
            minute_step: 1,
            second_step: 1,
            seconds: false,
            twelve_hour: false,
            texts: TimeTexts::chinese(),
            now_button: true,
            placeholder: None,
            format: None,
            parse: None,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            style: Style::new(),
            input: Style::new(),
            button: Style::new(),
            panel: Style::new(),
            column: Style::new(),
            item: Style::new(),
            action: Style::new(),
            footer: Style::new(),
            view: None,
        }
    }

    /// 能选的范围（两头都能选）。面板里范围外的项禁用，打的字夹进范围
    pub fn range(mut self, min: Time, max: Time) -> Self {
        self.min = Some(min);
        self.max = Some(max.max(min));
        self
    }

    /// 面板里的分钟几分钟一档（默认 1）。只影响面板，打字还能打别的分钟
    pub fn minute_step(mut self, step: u8) -> Self {
        self.minute_step = step.clamp(1, 59);
        self
    }

    /// 面板里的秒几秒一档（默认 1），和 [`TimePicker::seconds`] 一起用
    pub fn second_step(mut self, step: u8) -> Self {
        self.second_step = step.clamp(1, 59);
        self
    }

    /// 精确到秒（默认到分）
    pub fn seconds(mut self, seconds: bool) -> Self {
        self.seconds = seconds;
        self
    }

    /// 十二小时制：多一列上午下午，输入框写成 `下午 2:30`
    pub fn twelve_hour(mut self, twelve_hour: bool) -> Self {
        self.twelve_hour = twelve_hour;
        self
    }

    /// 换掉文字（默认中文）
    pub fn texts(mut self, texts: TimeTexts) -> Self {
        self.texts = texts;
        self
    }

    /// 显示不显示"现在"按钮（默认显示）
    pub fn now_button(mut self, show: bool) -> Self {
        self.now_button = show;
        self
    }

    /// 没有值时输入框里显示的文字（默认"时:分"）
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// 输入框里怎么写时间。改了写法时通常也要 [`TimePicker::parse`]
    pub fn format(mut self, format: impl Fn(Time) -> String + 'static) -> Self {
        self.format = Some(Rc::new(format));
        self
    }

    /// 怎么读输入框里打的字；读不懂时返回 `None`
    pub fn parse(mut self, parse: impl Fn(&str) -> Option<Time> + 'static) -> Self {
        self.parse = Some(Rc::new(parse));
        self
    }

    /// 禁用：变淡，不能输入和打开，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制输入框的焦点；[`super::Label::target`] 用同一个句柄时点标签就让它获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户改了值（点面板里的一项、回车、失去焦点），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：面板关上时（值变了才调用），输入框回车、失去焦点时
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做弹出的内容：外框的输入框、弹出、`on_change`、`on_commit`、Esc 恢复都还是时间选择的，逻辑在 [`TimeState`] 上。
    /// 面板的样式对象这时不起作用，角色、键盘、焦点也要自己写
    pub fn view<C: View>(mut self, view: impl Fn(TimeState) -> C + 'static) -> Self {
        self.view = Some(Rc::new(move |state| view(state).into_element()));
        self
    }

    /// 改外框的样子：焦点状态是输入框有焦点，打开状态是面板弹出着
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改输入框的样子（默认宽 104、没有自己的框）
    pub fn field_style(mut self, style: &Style) -> Self {
        self.input = self.input.extend(style);
        self
    }

    /// 改右边按钮的样子
    pub fn button_style(mut self, style: &Style) -> Self {
        self.button = self.button.extend(style);
        self
    }

    /// 改弹出的面板的样子
    pub fn panel_style(mut self, style: &Style) -> Self {
        self.panel = self.panel.extend(style);
        self
    }

    /// 改每一列的样子（默认宽 62、高 224，在里面滚动）
    pub fn column_style(mut self, style: &Style) -> Self {
        self.column = self.column.extend(style);
        self
    }

    /// 改列里每一项的样子：选中状态是当前的值，禁用状态是范围外的
    pub fn item_style(mut self, style: &Style) -> Self {
        self.item = self.item.extend(style);
        self
    }

    /// 改"现在""清除""确定"按钮的样子
    pub fn action_style(mut self, style: &Style) -> Self {
        self.action = self.action.extend(style);
        self
    }

    /// 改那些按钮那一行的样子
    pub fn footer_style(mut self, style: &Style) -> Self {
        self.footer = self.footer.extend(style);
        self
    }

    fn construct(self) -> Element {
        let TimePicker { value, min, max, minute_step, second_step, seconds, twelve_hour, texts, now_button, placeholder, format, parse, disabled, focus, mut changes, style, input, button: button_style, panel, column: column_style, item: item_style, action, footer: footer_style, view, after: _ } = self;
        let report = changes.report(|v: &V| FieldValue::Text(v.picked().map(|t| t.to_string()).unwrap_or_default()));
        let disabled = Flag::new(disabled);
        let focus = focus.unwrap_or_default();
        let open = signal(false);
        let texts = Rc::new(texts);
        let format: Format = format.unwrap_or_else(|| {
            let texts = texts.clone();
            Rc::new(move |t| write_time(t, seconds, twelve_hour, &texts))
        });
        let parse: Parse = parse.unwrap_or_else(|| {
            let texts = texts.clone();
            Rc::new(move |t| read_time(t, &texts))
        });
        let clamp = move |t: Time| {
            let t = max.map_or(t, |max| t.min(max));
            min.map_or(t, |min| t.max(min))
        };
        let shown = {
            let format = format.clone();
            move |v: &V| v.picked().map_or_else(String::new, |t| format(t))
        };
        let draft = signal(value.with_untracked(&shown));
        {
            let shown = shown.clone();
            Effect::new(move || {
                let text = value.with(&shown);
                if untracked(|| draft.get()) != text {
                    draft.set(text);
                }
            });
        }
        let changes = Rc::new(changes);
        // 写进信号：变了才写、才通知 on_change
        let write: Rc<dyn Fn(Option<Time>, ChangeSource) -> bool> = {
            let changes = changes.clone();
            Rc::new(move |time, source| {
                let Some(next) = V::from_picked(time.map(clamp)) else { return false };
                if value.with_untracked(|current| *current == next) {
                    return false;
                }
                value.set(next.clone());
                changes.changed(next, source);
                true
            })
        };
        let commit: Rc<dyn Fn()> = {
            let (write, changes, shown) = (write.clone(), changes.clone(), shown.clone());
            Rc::new(move || {
                let typed = draft.get_untracked();
                let typed = typed.trim();
                let wanted = if typed.is_empty() { Some(None) } else { parse(typed).map(Some) };
                if let Some(time) = wanted && write(time, ChangeSource::Keyboard) {
                    changes.committed(value.get_untracked(), ChangeSource::Keyboard);
                }
                draft.set(value.with_untracked(&shown));
            })
        };
        // 打开时的值：Esc 恢复，关上时比较有没有变
        let opened_with = signal(None::<V>);
        let finish = {
            let (changes, write) = (changes.clone(), write.clone());
            Rc::new(move |restore: bool| {
                if let Some(before) = opened_with.get_untracked() {
                    if restore {
                        write(before.picked(), ChangeSource::Keyboard);
                    } else if value.with_untracked(|current| *current != before) {
                        changes.committed(value.get_untracked(), ChangeSource::Pointer);
                    }
                }
                opened_with.set(None);
                open.set(false);
            })
        };
        let popup: Rc<dyn Fn() -> Element> = {
            let (write, finish, texts) = (write.clone(), finish.clone(), texts.clone());
            let parts = PanelParts { panel: Style::new().shadow(OVERLAY_SHADOW).extend(&panel), column: column_style, item: item_style, action, footer: footer_style, now_button };
            Rc::new(move || {
                opened_with.set(Some(value.get_untracked()));
                let shared = TimeShared {
                    read: Rc::new(move || value.with(V::picked)),
                    write: write.clone(),
                    finish: finish.clone(),
                    focus,
                    min,
                    max,
                    minute_step,
                    second_step,
                    seconds,
                    twelve_hour,
                    clearable: V::clearable(),
                    texts: texts.clone(),
                };
                // 没有值时从现在开始选
                let state = TimeState { fallback: clamp(local_date_time().1).with_second(0), shared: Stored::new(shared) };
                match &view {
                    Some(view) => view(state),
                    None => default_time_panel(state, &parts),
                }
            })
        };
        let icon_name = texts.open.clone();
        field_with_popup(Shell {
            style,
            input,
            button: button_style,
            disabled,
            focus,
            open,
            draft,
            placeholder: placeholder.unwrap_or_else(|| "时:分".into()),
            commit,
            icon: line_icon(clock_icon()),
            icon_name,
            report,
            popup,
            on_dismiss: Rc::new(move |reason| {
                finish(reason == Dismiss::Escape);
                // 在外面点了别的东西时不抢焦点
                if reason != Dismiss::Outside {
                    focus.focus();
                }
            }),
        })
    }
}

impl<V: PickerValue<Time>> View for TimePicker<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 时间选择，[`TimePicker::new`] 的简写
pub fn time_picker<V: PickerValue<Time>>(value: Signal<V>) -> TimePicker<V> {
    TimePicker::new(value)
}
