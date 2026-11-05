//! 日期选择：能直接打日期的输入框，右边的按钮（或 ↓）弹出日历。

use std::rc::Rc;

use reactive::{Effect, Signal, signal, untracked};

use super::calendar::{CalendarOptions, CalendarTexts, Pick, calendar_options, calendar_panel};
use super::picker::{PickerValue, Shell, field_with_popup, line_icon};
use super::{Changes, Flag};
use aureole_core::{
    ChangeSource, Date, Dismiss, Element, FieldValue, FocusHandle, IntoChangeHandler, IntoProp, Prop, Style, VectorPath, View, Weekday,
};

/// 把日期写成文字
type Format = Rc<dyn Fn(Date) -> String>;
/// 把文字读成日期
type Parse = Rc<dyn Fn(&str) -> Option<Date>>;

/// 日期选择的构建器；[`date_picker`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let day = signal(Date::new(2026, 11, 5));
/// date_picker(day);
/// // 能清除：绑定 Option；只能选今天以后，选完了做点什么
/// let due = signal(None::<Date>);
/// DatePicker::new(due)
///     .range(today(), today().add_years(1))
///     .placeholder("截止日期")
///     .on_commit(|d: Option<Date>| println!("定在 {d:?}"));
/// // 换写法：显示成 11月5日，自己解析
/// DatePicker::new(day)
///     .format(|d| format!("{}月{}日", d.month(), d.day()))
///     .parse(|t| {
///         let (month, day) = t.trim_end_matches('日').split_once('月')?;
///         Date::checked(2026, month.parse().ok()?, day.parse().ok()?)
///     });
/// ```
///
/// 部位：外框（`.style`）、输入框（[`DatePicker::field_style`]）、右边的按钮（[`DatePicker::button_style`]）、弹出的面板
/// （[`DatePicker::panel_style`]），日历里面的部位和 [`super::Calendar`] 一样。外框的状态：焦点（输入框有焦点）、打开、禁用。
///
/// 输入：回车或失去焦点时读成日期。认 `2026-11-05`、`2026/11/5`、`2026年11月5日`、`20261105`；读不懂、不能选时恢复原来的写法；
/// 绑定 `Option` 时清空就是清除。键盘：↓ 打开日历，焦点进日历（键盘操作见 [`super::Calendar`]），选中或 Esc 后焦点回输入框。
///
/// 值只在这几个时候变：回车、失去焦点、在日历上选。变了时先调用 [`DatePicker::on_change`] 再调用
/// [`DatePicker::on_commit`]；代码里改绑定的信号时都不调用。
pub struct DatePicker<V> {
    value: Signal<V>,
    options: CalendarOptions,
    placeholder: String,
    format: Format,
    parse: Parse,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<V>,
    style: Style,
    input: Style,
    button: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 日历的图标（24 的画布）
fn calendar_icon() -> VectorPath {
    VectorPath::svg("M5 6 H19 V20 H5 Z M5 10.5 H19 M9 3.5 V7.5 M15 3.5 V7.5").view_box(0.0, 0.0, 24.0, 24.0)
}

element_methods!([V: PickerValue<Date>] DatePicker<V>);

impl<V: PickerValue<Date>> DatePicker<V> {
    /// 绑定到 `value` 的日期选择。`value` 可以是 [`Date`]，也可以是 `Option<Date>`（能清除）
    pub fn new(value: Signal<V>) -> Self {
        Self {
            after: Default::default(),
            value,
            options: CalendarOptions::default(),
            placeholder: "年-月-日".into(),
            format: Rc::new(|d| d.to_string()),
            parse: Rc::new(|t| t.parse().ok()),
            disabled: None,
            focus: None,
            changes: Changes::default(),
            style: Style::new(),
            input: Style::new(),
            button: Style::new(),
        }
    }

    calendar_options!();

    /// 没有值时输入框里显示的文字（默认"年-月-日"）
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// 输入框里怎么写日期（默认 `2026-11-05`）。改了写法时通常也要 [`DatePicker::parse`]
    pub fn format(mut self, format: impl Fn(Date) -> String + 'static) -> Self {
        self.format = Rc::new(format);
        self
    }

    /// 怎么读输入框里打的字（默认认几种常见的写法，见 `Date` 的 `parse`）；读不懂时返回 `None`
    pub fn parse(mut self, parse: impl Fn(&str) -> Option<Date> + 'static) -> Self {
        self.parse = Rc::new(parse);
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

    /// 用户改了值（在日历上选、回车、失去焦点），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：和 [`DatePicker::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 改外框的样子：焦点状态是输入框有焦点，打开状态是日历弹出着
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
        self.options.panel = self.options.panel.extend(style);
        self
    }

    fn construct(self) -> Element {
        let DatePicker { value, options, placeholder, format, parse, disabled, focus, mut changes, style, input, button, after: _ } = self;
        let report = changes.report(|v: &V| FieldValue::Text(v.picked().map(|d| d.to_string()).unwrap_or_default()));
        let disabled = Flag::new(disabled);
        let focus = focus.unwrap_or_default();
        let open = signal(false);
        let icon_name = options.texts.open.clone();
        let options = Rc::new(CalendarOptions { panel: aureole_core::Style::new().shadow(aureole_core::defaults::OVERLAY_SHADOW).extend(&options.panel), ..options });
        let shown = {
            let format = format.clone();
            move |v: &V| v.picked().map_or_else(String::new, |d| format(d))
        };
        let draft = signal(value.with_untracked(&shown));
        {
            // 值从外面变了：输入框跟着显示
            let shown = shown.clone();
            Effect::new(move || {
                let text = value.with(&shown);
                if untracked(|| draft.get()) != text {
                    draft.set(text);
                }
            });
        }
        // 用户给了一个值：不能选的不收，变了才写、才通知；输入框换成整理过的写法
        let apply: Rc<dyn Fn(Option<Date>, ChangeSource)> = {
            let (options, shown) = (options.clone(), shown.clone());
            Rc::new(move |date, source| {
                let acceptable = date.is_none_or(|d| options.allowed(d));
                if acceptable && let Some(next) = V::from_picked(date) && value.with_untracked(|current| *current != next) {
                    value.set(next.clone());
                    changes.both(next, source);
                }
                draft.set(value.with_untracked(&shown));
            })
        };
        let commit: Rc<dyn Fn()> = {
            let apply = apply.clone();
            Rc::new(move || {
                let typed = draft.get_untracked();
                let typed = typed.trim();
                if typed.is_empty() {
                    apply(None, ChangeSource::Keyboard);
                } else if let Some(date) = parse(typed) {
                    apply(Some(date), ChangeSource::Keyboard);
                } else {
                    // 读不懂：恢复原来的写法
                    draft.set(value.with_untracked(&shown));
                }
            })
        };
        let popup: Rc<dyn Fn() -> Element> = Rc::new(move || {
            let apply = apply.clone();
            let pick: Pick = Rc::new(move |date, source| {
                apply(date, source);
                open.set(false);
                focus.focus();
            });
            let panel = calendar_panel(options.clone(), move || value.with(V::picked), V::clearable(), pick, true);
            // 自己重做的界面的名字留着
            if options.view.is_some() { panel } else { panel.name("date-picker-panel") }
        });
        field_with_popup(Shell {
            style,
            input,
            button,
            disabled,
            focus,
            open,
            draft,
            placeholder,
            commit,
            icon: line_icon(calendar_icon()),
            icon_name,
            report,
            popup,
            on_dismiss: Rc::new(move |reason| {
                open.set(false);
                // 在外面点了别的东西时不抢焦点
                if reason != Dismiss::Outside {
                    focus.focus();
                }
            }),
        })
    }
}

impl<V: PickerValue<Date>> View for DatePicker<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 日期选择，[`DatePicker::new`] 的简写
pub fn date_picker<V: PickerValue<Date>>(value: Signal<V>) -> DatePicker<V> {
    DatePicker::new(value)
}
