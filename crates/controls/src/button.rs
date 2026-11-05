//! 按钮：里面可以放任何东西（文字、图标、几样排在一起），点击、回车、空格触发。

use super::look::look;
use super::on_activate;
use aureole_core::{Element, Role, View, row};

/// 按钮：里面放 `content`（文字，或者任何元素；后面还能 `.child(..)` 接着加，横着排、间距 6）。
/// 返回的就是元素，接着用元素的链式方法：`.on_click(..)`、`.disabled(..)`、`.style(&s)`、`.width(..)`、
/// `.tooltip(..)`、`.focus_handle(..)`……
///
/// ```no_run
/// # use aureole::prelude::*;
/// let saving = signal(false);
/// button("保存").disabled(saving).on_click(move || saving.set(true));
/// // 里面放任何东西：图标加文字
/// let down = VectorPath::svg("M12 4 V16 M6 10 L12 16 L18 10").view_box(0.0, 0.0, 24.0, 24.0);
/// button(path(down).size(14, 14).stroke(Stroke::new(2.0))).child("下载");
/// // 改样子：强调色实底
/// let primary = Style::new()
///     .background(Color::hex(0x2563eb))
///     .border_color(Color::hex(0x2563eb))
///     .color(Color::WHITE)
///     .hovered(|s| s.background(Color::hex(0x1f55d0)))
///     .pressed(|s| s.background(Color::hex(0x1b48b0)));
/// button("提交").style(&primary);
/// button("登录").width(pct(100));   // 占满一行
/// ```
///
/// 默认：高 30、左右 12、圆角 4，白底加 1 像素灰色描边，悬停、按下时底色变灰；禁用时变淡。按内容的宽度。
/// 这些都是先用上的样式对象，状态有悬停、按下、焦点（键盘焦点，同时画默认的焦点环）、禁用（元素的 `.disabled(..)`）。
///
/// 键盘：Tab 走到，回车或空格触发 `on_click`。禁用时不响应点击，Tab 跳过。[`super::Label::target`] 用同一个焦点句柄时点标签就是点按钮。
/// 对辅助技术是按钮，名字默认是里面的文字；只有图标的按钮用 `.accessibility_name(..)` 起名字，或者加提示。
pub fn button(content: impl View) -> Element {
    // 没有部位：悬停、焦点、按下、禁用由样式对象让元素自己记（禁用是元素的 `.disabled(..)`，后面再设也算）
    let el = row().focusable(true).accessibility_role(Role::Button).style(&look().button);
    // 按钮的点击交给使用的人加的 on_click，之后停止传播（外面的可点击元素不再收到）
    on_activate(el.child(content), |_| {})
}
