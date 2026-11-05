//! 进度条：确定的（0..1）和不确定的。

use std::rc::Rc;

use reactive::signal;

use super::look::look;
use super::part;
use aureole_core::{Element, IntoProp, Prop, RangeValue, Rect, Role, States, Style, View, pct, rect};

/// 进度条的构建器；[`progress_bar`]、[`ProgressBar::indeterminate`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let done = signal(0.3f32);
/// progress_bar(done);
/// ProgressBar::new(move || done.get()).fill_style(&Style::new().background(Color::hex(0x1f9a6b))).width(pct(100));
/// ProgressBar::indeterminate();
/// ```
///
/// 部位：轨道（`.style`，就是进度条本身）、填充（[`ProgressBar::fill_style`]）。默认：宽 200、高 6 的灰色轨道，
/// 填充强调色，裁剪在轨道里；确定的进度改变时填充的长度立即变，要平滑地长就在填充上声明宽度的过渡
/// （`.fill_style(&Style::new().transition(Property::Width, Spring::smooth()))`）。
///
/// **不确定的进度**是一个静止的"忙"的样子：填充是轨道中间的一段（从 30% 到 70%），不动。
/// 框架不自己动它；要动起来，就在填充的样式对象里放平移和循环的过渡，挂载后改一下信号让它开始：
///
/// ```no_run
/// # use aureole::prelude::*;
/// let moving = signal(false);
/// let sweep = Style::new()
///     .translation_x(move || if moving.get() { 70.0 } else { -70.0 })
///     .transition(Property::TranslationX, Tween::new(ms(900)).ease_in_out().alternate(true).forever());
/// ProgressBar::indeterminate().fill_style(&sweep).on_mount(move || moving.set(true));
/// ```
///
/// 对辅助技术是进度条：确定的带数值和百分比，不确定的没有数值；名字用 `.accessibility_name(..)` 起。
pub struct ProgressBar {
    value: Option<Prop<f32>>,
    style: Style,
    fill: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

element_methods!([] ProgressBar);

impl ProgressBar {
    /// 确定的进度，0..1（超出的部分截掉）
    pub fn new(value: impl IntoProp<f32>) -> Self {
        Self { value: Some(value.into_prop()), style: Style::new(), fill: Style::new(), after: Default::default() }
    }

    /// 不知道还要多久：静止的"忙"的样子（见上面怎么让它动起来）
    pub fn indeterminate() -> Self {
        Self { value: None, style: Style::new(), fill: Style::new(), after: Default::default() }
    }

    /// 改轨道（进度条本身）的样子
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改填充的样子
    pub fn fill_style(mut self, style: &Style) -> Self {
        self.fill = self.fill.extend(style);
        self
    }

    fn construct(self) -> Element {
        let ProgressBar { value, style, fill, after: _ } = self;
        let l = look();
        let none = States::new();
        let bar = part(rect().accessibility_role(Role::ProgressIndicator), &l.progress, &style, &none).clip(true);
        let filler = part(rect().height(pct(100)), &l.progress_fill, &fill, &none);
        match value {
            Some(value) => {
                let read: Rc<dyn Fn() -> f32> = match value {
                    Prop::Static(v) => Rc::new(move || v.clamp(0.0, 1.0)),
                    Prop::Dynamic(f) => Rc::new(move || f().clamp(0.0, 1.0)),
                };
                let (width, number, words) = (read.clone(), read.clone(), read);
                // 填充的宽度按量到的轨道宽度算成像素：声明了宽度的过渡（`Property::Width`）就能动
                let track = signal(Rect::default());
                bar.report_bounds(track)
                    .accessibility_value(move || RangeValue::new(number() as f64, 0.0, 1.0))
                    .accessibility_value_text(move || format!("{:.0}%", words() * 100.0))
                    .child(filler.width(move || width() * track.get().width))
            }
            None => bar.child(filler.position(pct(30), 0).width(pct(40))),
        }
    }
}

impl View for ProgressBar {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 确定的进度条，0..1，[`ProgressBar::new`] 的简写；不确定的进度条是 [`ProgressBar::indeterminate`]
pub fn progress_bar(value: impl IntoProp<f32>) -> ProgressBar {
    ProgressBar::new(value)
}
