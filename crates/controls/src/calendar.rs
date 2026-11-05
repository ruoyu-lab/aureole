//! 日历：一个月一页，点一天、方向键、翻页键。可以直接放进界面（[`Calendar`]），也是日期选择（[`super::DatePicker`]）弹出的内容。

use std::rc::Rc;

use reactive::{Effect, Memo, Signal, memo, signal, untracked};

use super::look::look;
use super::picker::PickerValue;
use super::button::button;
use super::{Changes, Press, on_activate, part};
use aureole_core::{
    Builtin, ChangeSource, ClickEvent, Date, Element, FieldValue, FocusHandle, IntoChangeHandler, KeyEvent, Live, Role, State, States, Stroke, Style, VectorPath, View, Weekday, column, path, row,
    text, today,
};

/// 日历上的文字。[`CalendarTexts::chinese`] 是默认的，[`CalendarTexts::english`] 是英文的，也可以只改其中几项：
///
/// ```no_run
/// # use aureole::prelude::*;
/// let day = signal(Date::new(2026, 11, 5));
/// Calendar::new(day).texts(CalendarTexts { today: "回到今天".into(), ..CalendarTexts::chinese() });
/// ```
///
/// 模板里能写 `{year}`、`{month}`（数字）、`{month_name}`、`{day}`、`{weekday}`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarTexts {
    /// 表头的星期，从星期一开始
    pub weekdays: [String; 7],
    /// 星期的全称（辅助技术念的），从星期一开始
    pub weekday_names: [String; 7],
    /// 月份的名字，从一月开始
    pub month_names: [String; 12],
    /// 这一页的标题的模板
    pub title: String,
    /// 一天的全称的模板（辅助技术念的）
    pub full_date: String,
    /// "上一个月"
    pub previous_month: String,
    /// "下一个月"
    pub next_month: String,
    /// "上一年"
    pub previous_year: String,
    /// "下一年"
    pub next_year: String,
    /// "今天"
    pub today: String,
    /// "清除"
    pub clear: String,
    /// 日期选择右边按钮的名字（辅助技术念的）
    pub open: String,
}

fn strings<const N: usize>(items: [&str; N]) -> [String; N] {
    items.map(str::to_owned)
}

impl CalendarTexts {
    /// 中文（默认）
    pub fn chinese() -> Self {
        Self {
            weekdays: strings(["一", "二", "三", "四", "五", "六", "日"]),
            weekday_names: strings(["星期一", "星期二", "星期三", "星期四", "星期五", "星期六", "星期日"]),
            month_names: strings(["一月", "二月", "三月", "四月", "五月", "六月", "七月", "八月", "九月", "十月", "十一月", "十二月"]),
            title: "{year} 年 {month} 月".into(),
            full_date: "{year} 年 {month} 月 {day} 日 {weekday}".into(),
            previous_month: "上一个月".into(),
            next_month: "下一个月".into(),
            previous_year: "上一年".into(),
            next_year: "下一年".into(),
            today: "今天".into(),
            clear: "清除".into(),
            open: "打开日历".into(),
        }
    }

    /// 英文
    pub fn english() -> Self {
        Self {
            weekdays: strings(["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"]),
            weekday_names: strings(["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]),
            month_names: strings(["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"]),
            title: "{month_name} {year}".into(),
            full_date: "{weekday}, {month_name} {day}, {year}".into(),
            previous_month: "Previous month".into(),
            next_month: "Next month".into(),
            previous_year: "Previous year".into(),
            next_year: "Next year".into(),
            today: "Today".into(),
            clear: "Clear".into(),
            open: "Open calendar".into(),
        }
    }

    /// 按模板写出一天
    fn fill(&self, template: &str, date: Date) -> String {
        template
            .replace("{year}", &date.year().to_string())
            .replace("{month_name}", &self.month_names[usize::from(date.month() - 1)])
            .replace("{month}", &date.month().to_string())
            .replace("{day}", &date.day().to_string())
            .replace("{weekday}", &self.weekday_names[usize::from(date.weekday().from_monday())])
    }
}

impl Default for CalendarTexts {
    fn default() -> Self {
        Self::chinese()
    }
}

/// 能不能选的判断
type DateTest = Rc<dyn Fn(Date) -> bool>;
/// 一天里面放什么
type DayContent = Rc<dyn Fn(Memo<Date>) -> Element>;
/// 自己重做的界面
type CalendarView = Rc<dyn Fn(CalendarState) -> Element>;

/// 日历和日期选择共用的设置：范围、禁用的日期、文字、各部位的样子
#[derive(Clone)]
pub(crate) struct CalendarOptions {
    pub min: Option<Date>,
    pub max: Option<Date>,
    pub blocked: Option<DateTest>,
    pub first_weekday: Weekday,
    pub texts: CalendarTexts,
    pub today_button: bool,
    pub panel: Style,
    pub header: Style,
    pub nav: Style,
    pub weekday: Style,
    pub day: Style,
    pub action: Style,
    pub footer: Style,
    pub day_content: Option<DayContent>,
    pub view: Option<CalendarView>,
}

impl Default for CalendarOptions {
    fn default() -> Self {
        Self {
            min: None,
            max: None,
            blocked: None,
            first_weekday: Weekday::Monday,
            texts: CalendarTexts::chinese(),
            today_button: true,
            panel: Style::new(),
            header: Style::new(),
            nav: Style::new(),
            weekday: Style::new(),
            day: Style::new(),
            action: Style::new(),
            footer: Style::new(),
            day_content: None,
            view: None,
        }
    }
}

impl CalendarOptions {
    /// 这一天能不能选
    pub fn allowed(&self, date: Date) -> bool {
        self.in_range(date) && !self.blocked.as_ref().is_some_and(|blocked| blocked(date))
    }

    fn in_range(&self, date: Date) -> bool {
        self.min.is_none_or(|min| date >= min) && self.max.is_none_or(|max| date <= max)
    }

    /// 夹进范围
    pub fn clamp(&self, date: Date) -> Date {
        let date = self.max.map_or(date, |max| date.min(max));
        self.min.map_or(date, |min| date.max(min))
    }

    /// 从 `target` 开始找一天能选的：先往 `direction` 那边走，走到头再往回（一年之内）。整年都不能选时是 `None`
    fn nearest(&self, target: Date, direction: i64) -> Option<Date> {
        let start = self.clamp(target);
        for direction in [direction, -direction] {
            let mut date = start;
            for _ in 0..=366 {
                if self.allowed(date) {
                    return Some(date);
                }
                date = date.add_days(direction);
                if !self.in_range(date) {
                    break;
                }
            }
        }
        None
    }
}

/// 上一个、下一个的箭头（24 的画布）
fn chevron(path_data: &str) -> VectorPath {
    VectorPath::svg(path_data).view_box(0.0, 0.0, 24.0, 24.0)
}

const PREVIOUS: &str = "M15 6 L9 12 L15 18";
const NEXT: &str = "M9 6 L15 12 L9 18";
const PREVIOUS_FAR: &str = "M12 6 L6 12 L12 18 M18 6 L12 12 L18 18";
const NEXT_FAR: &str = "M6 6 L12 12 L6 18 M12 6 L18 12 L12 18";

/// 日历的逻辑，不带界面：这一页显示哪个月、光标在哪天、哪些日子能选、选了之后通知谁。默认的日历也只用这里的方法搭，
/// 所以自己重做界面（[`Calendar::view`]）时能做到默认的日历做到的一切，范围、禁用的日期、`on_change`、`on_commit`、
/// 绑定的信号都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let day = signal(Date::new(2026, 11, 5));
/// // 一行七天的周视图：键盘、范围、事件都是日历的，样子全是自己的
/// Calendar::new(day).on_commit(|d: Date| println!("{d}")).view(|state: CalendarState| {
///     let week = (0..7).map(move |i| {
///         let date = move || state.cursor().add_days(i);
///         button(text(move || date().day().to_string()))
///             .background(move || if state.selected() == Some(date()) { Color::hex(0x2563eb) } else { Color::WHITE })
///             .on_click(move |e: &ClickEvent| state.pick(date(), e.source.into()))
///     });
///     row().gap(4).children(week).focusable(true).on_key_down(move |e: &KeyEvent| {
///         if state.handle_key(e) {
///             e.stop_propagation();
///         }
///     })
/// });
/// ```
#[derive(Clone, Copy)]
pub struct CalendarState {
    cursor: Signal<Date>,
    month: Memo<Date>,
    page_start: Memo<Date>,
    shared: Signal<Rc<Shared>>,
}

/// 状态里不变的部分
struct Shared {
    options: Rc<CalendarOptions>,
    selected: Rc<dyn Fn() -> Option<Date>>,
    pick: Pick,
    clearable: bool,
}

/// 用户选了（`None` 是清除）
pub(crate) type Pick = Rc<dyn Fn(Option<Date>, ChangeSource)>;

impl CalendarState {
    /// 一页有多少格：六行七列
    pub const CELLS: usize = 42;

    fn new(options: Rc<CalendarOptions>, selected: Rc<dyn Fn() -> Option<Date>>, clearable: bool, pick: Pick) -> Self {
        let start = untracked(|| selected()).unwrap_or_else(today);
        // 光标：方向键走到的一天，也决定显示哪个月。整年都不能选时停在范围里
        let cursor = signal(options.nearest(start, 1).unwrap_or_else(|| options.clamp(start)));
        {
            let (selected, options) = (selected.clone(), options.clone());
            Effect::new(move || {
                if let Some(date) = selected() {
                    cursor.set_if_changed(options.clamp(date));
                }
            });
        }
        let month: Memo<Date> = memo(move || cursor.get().first_of_month());
        let first_weekday = options.first_weekday;
        let page_start: Memo<Date> = memo(move || {
            let first = month.get();
            first.add_days(-i64::from(first.weekday().days_since(first_weekday)))
        });
        Self { cursor, month, page_start, shared: signal(Rc::new(Shared { options, selected, pick, clearable })) }
    }

    fn shared(self) -> Rc<Shared> {
        self.shared.with_untracked(Rc::clone)
    }

    /// 这一页显示的月份（那个月的第一天），响应式
    pub fn month(self) -> Date {
        self.month.get()
    }

    /// 这一页的第 `index` 格（`0..CELLS`）是哪天，响应式。第一格是这个月第一天所在那周的第一天
    pub fn day(self, index: usize) -> Date {
        self.page_start.get().add_days(index as i64)
    }

    /// 光标在哪天，响应式。光标决定显示哪个月
    pub fn cursor(self) -> Date {
        self.cursor.get()
    }

    /// 光标在这一页的第几格，响应式。焦点要跟着光标时用它
    pub fn cursor_index(self) -> usize {
        usize::try_from(self.cursor.get().days_from_epoch() - self.page_start.get().days_from_epoch()).unwrap_or(0)
    }

    /// 选中的是哪天，响应式；没有选时是 `None`
    pub fn selected(self) -> Option<Date> {
        (self.shared().selected)()
    }

    /// 这一天是不是在这一页显示的月份里，响应式
    pub fn in_month(self, date: Date) -> bool {
        let first = self.month.get();
        date.year() == first.year() && date.month() == first.month()
    }

    /// 这一天能不能选：在范围里，也没有被 [`Calendar::disabled_dates`] 禁用
    pub fn is_allowed(self, date: Date) -> bool {
        self.shared().options.allowed(date)
    }

    /// 能不能清除：绑定的是 `Option` 时能
    pub fn clearable(self) -> bool {
        self.shared().clearable
    }

    /// 这一页的标题（[`CalendarTexts::title`]），响应式
    pub fn title(self) -> String {
        let shared = self.shared();
        shared.options.texts.fill(&shared.options.texts.title, self.month.get())
    }

    /// 一天的全称（[`CalendarTexts::full_date`]），给辅助技术念
    pub fn full_date(self, date: Date) -> String {
        let shared = self.shared();
        shared.options.texts.fill(&shared.options.texts.full_date, date)
    }

    /// 日历上的文字
    pub fn texts(self) -> CalendarTexts {
        self.shared().options.texts.clone()
    }

    /// 一周七天，从 [`Calendar::first_weekday`] 开始
    pub fn weekdays(self) -> [Weekday; 7] {
        let first = usize::from(self.shared().options.first_weekday.from_monday());
        std::array::from_fn(|i| Weekday::ALL[(first + i) % 7])
    }

    /// 用户选了这一天：不能选的不收。值变了才调用 `on_change`、`on_commit`（`source` 是怎么选的）
    pub fn pick(self, date: Date, source: ChangeSource) {
        let shared = self.shared();
        if shared.options.allowed(date) {
            self.cursor.set(date);
            (shared.pick)(Some(date), source);
        }
    }

    /// 用户选了今天。今天不能选时什么都不做
    pub fn pick_today(self, source: ChangeSource) {
        self.pick(today(), source);
    }

    /// 用户清除了。绑定的不是 `Option` 时什么都不做
    pub fn clear(self, source: ChangeSource) {
        let shared = self.shared();
        if shared.clearable {
            (shared.pick)(None, source);
        }
    }

    /// 把光标移到 `target`（不改值）。不能选时按 `direction`（`1` 往后，`-1` 往前）找最近能选的一天，整年都不能选时不动，返回 `false`
    pub fn move_cursor(self, target: Date, direction: i64) -> bool {
        match self.shared().options.nearest(target, direction) {
            Some(date) => {
                self.cursor.set(date);
                true
            }
            None => false,
        }
    }

    /// 翻页：往后（负数往前）这么多个月。翻到范围外时不动，返回 `false`
    pub fn shift_months(self, months: i32) -> bool {
        if !self.can_shift_months(months) {
            return false;
        }
        let current = self.cursor.get_untracked();
        self.move_cursor(current.add_months(months), if months < 0 { -1 } else { 1 })
    }

    /// 翻这么多个月之后还有没有能显示的日子（翻页按钮到头时禁用用它），响应式
    pub fn can_shift_months(self, months: i32) -> bool {
        let options = self.shared().options.clone();
        let target = self.cursor.get().add_months(months).first_of_month();
        if months < 0 { options.min.is_none_or(|min| target >= min.first_of_month()) } else { options.max.is_none_or(|max| target <= max.first_of_month()) }
    }

    /// 处理一个按键：← → 一天，↑ ↓ 一周，Home / End 这一周的头尾，Page Up / Page Down 一个月，加 Shift 一年。
    /// 只移动光标，不改值、不管焦点；处理了返回 `true`（不认识的键返回 `false`，让它接着传）
    pub fn handle_key(self, e: &KeyEvent) -> bool {
        let current = self.cursor.get_untracked();
        let first_weekday = self.shared().options.first_weekday;
        let shift = e.modifiers.has_shift();
        let (target, direction) = if e.is("ArrowLeft") {
            (current.add_days(-1), -1)
        } else if e.is("ArrowRight") {
            (current.add_days(1), 1)
        } else if e.is("ArrowUp") {
            (current.add_days(-7), -1)
        } else if e.is("ArrowDown") {
            (current.add_days(7), 1)
        } else if e.is("PageUp") {
            (if shift { current.add_years(-1) } else { current.add_months(-1) }, -1)
        } else if e.is("PageDown") {
            (if shift { current.add_years(1) } else { current.add_months(1) }, 1)
        } else if e.is("Home") {
            (current.add_days(-i64::from(current.weekday().days_since(first_weekday))), -1)
        } else if e.is("End") {
            (current.add_days(6 - i64::from(current.weekday().days_since(first_weekday))), 1)
        } else {
            return false;
        };
        self.move_cursor(target, direction);
        true
    }
}

/// 日历的内容：自己重做的界面，或者默认的
pub(crate) fn calendar_panel(options: Rc<CalendarOptions>, selected: impl Fn() -> Option<Date> + 'static, clearable: bool, pick: Pick, focus_now: bool) -> Element {
    let state = CalendarState::new(options.clone(), Rc::new(selected), clearable, pick);
    match &options.view {
        Some(view) => view(state),
        None => default_calendar(state, &options, focus_now),
    }
}

/// 默认的日历：只用 [`CalendarState`] 的公开方法。`focus_now`：一放上去就把焦点给光标所在的一天（弹出时）
fn default_calendar(state: CalendarState, options: &CalendarOptions, focus_now: bool) -> Element {
    let l = look();
    let handles: Rc<[FocusHandle]> = (0..CalendarState::CELLS).map(|_| FocusHandle::new()).collect();

    let header = {
        let nav = |shape: &str, name: &str, months: i32| {
            let press = Press::new();
            let blocked = move || !state.can_shift_months(months);
            let states = press.states().disabled(blocked);
            let el = on_activate(press.attach(row()).accessibility_role(Role::Button).accessibility_name(name.to_owned()).name(name.to_owned()).disabled(blocked), move |_| {
                state.shift_months(months);
            });
            part(el, &l.calendar_nav, &options.nav, &states).child(path(chevron(shape)).size(14, 14).stroke(Stroke::new(1.8).round()))
        };
        let texts = &options.texts;
        let title = text(move || state.title()).grow(1).text_align(aureole_core::TextAlign::Center).accessibility_live(Live::Polite).name("calendar-title");
        part(row(), &l.calendar_header, &options.header, &States::new())
            .child(nav(PREVIOUS_FAR, &texts.previous_year, -12))
            .child(nav(PREVIOUS, &texts.previous_month, -1))
            .child(title)
            .child(nav(NEXT, &texts.next_month, 1))
            .child(nav(NEXT_FAR, &texts.next_year, 12))
    };

    let weekdays = state.weekdays().into_iter().fold(row().accessibility_role(Role::Row), |line, weekday| {
        let name = options.texts.weekday_names[usize::from(weekday.from_monday())].clone();
        let short = options.texts.weekdays[usize::from(weekday.from_monday())].clone();
        line.child(part(row(), &l.calendar_weekday, &options.weekday, &States::new()).accessibility_role(Role::ColumnHeader).accessibility_name(name).child(text(short)))
    });

    let cell = |i: usize| {
        let date = move || state.day(i);
        let usable = move || state.in_month(date()) && state.is_allowed(date());
        let press = Press::new();
        let chosen = move || state.selected() == Some(date());
        let shown_today = today();
        let states = press.states().selected(chosen).checked(move || date() == shown_today).when(State::Disabled, move || !usable());
        let handle = handles[i];
        let el = press
            .attach(row())
            .name(format!("day-{i}"))
            .focus_handle(handle)
            // 整页只有一个 Tab 停靠点：光标所在的那一天
            .focusable(move || state.cursor() == date())
            .accessibility_role(Role::GridCell)
            .accessibility_name(move || state.full_date(date()))
            .accessibility_selected(chosen)
            .disabled(move || !usable())
            .builtin_handlers(|el| {
                el.on_click(move |e: &ClickEvent| {
                    if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                        state.pick(untracked(date), e.source.into());
                    }
                    e.stop_propagation();
                })
            });
        let content = match &options.day_content {
            Some(make) => make(memo(date)),
            None => text(move || date().day().to_string()).tabular_numbers(true),
        };
        part(el, &l.calendar_day, &options.day, &states).child(content)
    };
    let grid = (0..CalendarState::CELLS / 7).fold(column().accessibility_role(Role::Grid), |grid, line| {
        let days = (0..7).fold(row().accessibility_role(Role::Row), |days, column| days.child(cell(line * 7 + column)));
        grid.child(days)
    });
    let focus_cursor = {
        let handles = handles.clone();
        move || {
            if let Some(handle) = handles.get(untracked(move || state.cursor_index())) {
                handle.focus();
            }
        }
    };
    let on_key = focus_cursor.clone();
    let grid = grid.builtin_handlers(|el| {
        el.on_key_down(move |e: &KeyEvent| {
            if state.handle_key(e) {
                // 光标先改，那个格子才能获得焦点
                on_key();
                e.stop_propagation();
            }
        })
    });
    let grid = if focus_now { grid.on_mount(focus_cursor) } else { grid };

    let mut footer = part(row(), &l.calendar_footer, &options.footer, &States::new());
    let mut has_footer = false;
    if options.today_button {
        footer = footer.child(
            part(button(options.texts.today.clone()).name(options.texts.today.clone()), &l.calendar_action, &options.action, &States::new())
                .disabled(!options.allowed(today()))
                .on_click(move |e: &ClickEvent| state.pick_today(e.source.into())),
        );
        has_footer = true;
    }
    if state.clearable() {
        footer = footer.child(
            part(button(options.texts.clear.clone()).name(options.texts.clear.clone()), &l.calendar_action, &options.action, &States::new())
                .on_click(move |e: &ClickEvent| state.clear(e.source.into())),
        );
        has_footer = true;
    }

    let panel = part(column(), &l.calendar, &options.panel, &States::new()).child(header).child(weekdays).child(grid);
    if has_footer { panel.child(footer) } else { panel }
}

/// 日历的构建器；[`calendar`] 是简写。一直显示着的一个月，要弹出的用 [`super::DatePicker`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let day = signal(Date::new(2026, 11, 5));
/// calendar(day);
/// // 能清除：绑定 Option；只能选今年，周末不能选，从星期日开始
/// let due = signal(None::<Date>);
/// Calendar::new(due)
///     .range(Date::new(2026, 1, 1), Date::new(2026, 12, 31))
///     .disabled_dates(|d| matches!(d.weekday(), Weekday::Saturday | Weekday::Sunday))
///     .first_weekday(Weekday::Sunday)
///     .on_change(|d: Option<Date>| println!("选了 {d:?}"));
/// ```
///
/// 部位：整个面板（`.style`）、标题一行（[`Calendar::header_style`]）、翻页按钮（[`Calendar::nav_style`]）、表头的星期
/// （[`Calendar::weekday_style`]）、每一天（[`Calendar::day_style`]）、下面的"今天""清除"（[`Calendar::action_style`]）和那一行
/// （[`Calendar::footer_style`]）。一天里面放什么可以自己决定（[`Calendar::day_content`]）。
/// 默认：白底细线框；一天 32 × 28，悬停浅灰底，选中的强调色实底白字，今天强调色细线框，别的月的、不能选的文字变淡。没有动画。
/// 一天的状态：悬停、按下、焦点、选中（当前的值）、勾选（今天）、禁用（别的月的、不能选的）。
///
/// 键盘：整页一个 Tab 停靠点。← → 一天，↑ ↓ 一周，Home / End 到这一周的头尾，Page Up / Page Down 一个月，加 Shift 一年，
/// 回车、空格选中。走到别的月时翻页，不能选的日子跳过。对辅助技术是表格，每一天念全称和选中状态，翻页时念新的标题。
///
/// 样式对象换不了的（翻页的图标、六行七列的结构）用 [`Calendar::view`] 完全重做界面：逻辑和事件留着，见 [`CalendarState`]。
pub struct Calendar<V> {
    value: Signal<V>,
    options: CalendarOptions,
    changes: Changes<V>,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// [`Calendar`] 和 [`super::DatePicker`] 共有的设置方法
macro_rules! calendar_options {
    () => {
        /// 能选的范围（两头都能选）。翻页按钮到头时禁用
        pub fn range(mut self, min: Date, max: Date) -> Self {
            self.options.min = Some(min);
            self.options.max = Some(max.max(min));
            self
        }

        /// 这些日子不能选（范围里的节假日、周末）
        pub fn disabled_dates(mut self, blocked: impl Fn(Date) -> bool + 'static) -> Self {
            self.options.blocked = Some(Rc::new(blocked));
            self
        }

        /// 一周从星期几开始（默认星期一）
        pub fn first_weekday(mut self, weekday: Weekday) -> Self {
            self.options.first_weekday = weekday;
            self
        }

        /// 换掉日历上的文字（默认中文）
        pub fn texts(mut self, texts: CalendarTexts) -> Self {
            self.options.texts = texts;
            self
        }

        /// 显示不显示"今天"按钮（默认显示）
        pub fn today_button(mut self, show: bool) -> Self {
            self.options.today_button = show;
            self
        }

        /// 改标题一行的样子
        pub fn header_style(mut self, style: &Style) -> Self {
            self.options.header = self.options.header.extend(style);
            self
        }

        /// 改翻页按钮的样子
        pub fn nav_style(mut self, style: &Style) -> Self {
            self.options.nav = self.options.nav.extend(style);
            self
        }

        /// 改表头星期的样子
        pub fn weekday_style(mut self, style: &Style) -> Self {
            self.options.weekday = self.options.weekday.extend(style);
            self
        }

        /// 改每一天的样子：选中状态是当前的值，勾选状态是今天，禁用状态是别的月的和不能选的。
        /// 换深色时这里也要写文字颜色：默认的样子在选中时有自己的颜色，平时就不从面板继承了
        pub fn day_style(mut self, style: &Style) -> Self {
            self.options.day = self.options.day.extend(style);
            self
        }

        /// 改"今天""清除"按钮的样子
        pub fn action_style(mut self, style: &Style) -> Self {
            self.options.action = self.options.action.extend(style);
            self
        }

        /// 改"今天""清除"那一行的样子
        pub fn footer_style(mut self, style: &Style) -> Self {
            self.options.footer = self.options.footer.extend(style);
            self
        }

        /// 完全重做界面：不用默认的样子，自己搭，范围、禁用的日期、绑定的信号、`on_change`、`on_commit` 都照常工作，
        /// 逻辑都在 [`crate::CalendarState`] 上。样式对象的那些方法这时不起作用
        pub fn view<C: aureole_core::View>(mut self, view: impl Fn(crate::CalendarState) -> C + 'static) -> Self {
            self.options.view = Some(Rc::new(move |state| view(state).into_element()));
            self
        }

        /// 自己决定一天里面放什么（默认是几号）：标记有事的日子、农历、价格。翻页时格子不重建，
        /// 所以参数是"这一格现在是哪天"的派生值，在绑定里读它
        pub fn day_content<C: aureole_core::View>(mut self, content: impl Fn(reactive::Memo<Date>) -> C + 'static) -> Self {
            self.options.day_content = Some(Rc::new(move |date| content(date).into_element()));
            self
        }
    };
}

pub(crate) use calendar_options;

element_methods!([V: PickerValue<Date>] Calendar<V>);

impl<V: PickerValue<Date>> Calendar<V> {
    /// 绑定到 `value` 的日历。`value` 可以是 [`Date`]，也可以是 `Option<Date>`（有"清除"按钮）
    pub fn new(value: Signal<V>) -> Self {
        Self { value, options: CalendarOptions::default(), changes: Changes::default(), after: Default::default() }
    }

    calendar_options!();

    /// 改整个面板的样子
    pub fn style(mut self, style: &Style) -> Self {
        self.options.panel = self.options.panel.extend(style);
        self
    }

    /// 用户选了（点一天、回车、"今天""清除"），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 选完了：每次选都是完成的，和 [`Calendar::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    fn construct(self) -> Element {
        let Calendar { value, options, mut changes, after: _ } = self;
        let field = changes.report(|v: &V| FieldValue::Text(v.picked().map(|d| d.to_string()).unwrap_or_default()));
        let pick: Pick = Rc::new(move |date, source| {
            let Some(next) = V::from_picked(date) else { return };
            if value.with_untracked(|current| *current != next) {
                value.set(next.clone());
                changes.both(next, source);
            }
        });
        calendar_panel(Rc::new(options), move || value.with(V::picked), V::clearable(), pick, false).field_handle(field)
    }
}

impl<V: PickerValue<Date>> View for Calendar<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 日历，[`Calendar::new`] 的简写
pub fn calendar<V: PickerValue<Date>>(value: Signal<V>) -> Calendar<V> {
    Calendar::new(value)
}
