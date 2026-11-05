//! 框架自带的样子：几种固定的浅色，都在这里。窗口底色、默认的文字和选区颜色、占位文字、焦点环、滚动条、
//! 提示、浮层的表面，以及基础控件（`aureole-controls`）的默认外观都从这里取；输入框默认的样子也在这里
//! （输入框是核心的元素，其余控件的样子在控件的 crate 里，从这里取这些颜色）。自己做的控件要和基础控件一个样子时也用它们。
//!
//! 只有浅色一套，不随系统外观变，也没有主题：要别的样子时在元素上设（窗口底色用 `App::background`，
//! 文字颜色、选区颜色设在根元素上会继承，焦点环用 `.focus_ring(..)`，控件用样式对象）。
//! 中性的灰、一个偏蓝的强调色（焦点、选区、勾选），1 像素细线、小圆角，不发光、不用重投影。

use std::time::Duration;

use animation::{Curve, Spring, Tween};
use scene::Color;

use crate::shadow::Shadow;
use crate::style::Style;

/// 窗口底色
pub const BACKGROUND: Color = Color::hex(0xf6f7f9);
/// 控件、浮层的表面
pub const SURFACE: Color = Color::WHITE;
/// 正文文字，也是输入框插入点的颜色
pub const TEXT: Color = Color::hex(0x1f2328);
/// 次要文字
pub const MUTED: Color = Color::hex(0x5c6570);
/// 输入框的占位文字
pub const PLACEHOLDER: Color = Color::hex(0x8c949e);
/// 控件的描边：按钮、输入框、下拉框、步进按钮
pub const BORDER: Color = Color::hex(0xc4cad2);
/// 悬停时控件的描边
pub const BORDER_HOVER: Color = Color::hex(0x98a1ac);
/// 浮层、面板的描边和分隔线（比控件的淡）
pub const LINE: Color = Color::hex(0xd9dde3);
/// 勾选框、单选圆这些小东西的描边：比控件的描边深一点，小了也看得清
pub const INDICATOR: Color = Color::hex(0x8f98a3);
/// 中性的底色：按钮悬停、按下，开关关着时的轨道，滑块、进度条的轨道
pub const FILL: Color = Color::hex(0xf0f2f5);
/// 中性的底色按下时
pub const FILL_PRESSED: Color = Color::hex(0xe3e7ec);
/// 滑块、进度条的轨道，开关关着时的轨道
pub const TRACK: Color = Color::hex(0xd5dae0);
/// 强调色：焦点、选区、勾选、打开的开关、进度
pub const ACCENT: Color = Color::hex(0x2563eb);
/// 强调色悬停时
pub const ACCENT_HOVER: Color = Color::hex(0x1f55d0);
/// 强调色按下时
pub const ACCENT_PRESSED: Color = Color::hex(0x1b48b0);
/// 强调色的浅底：下拉列表里键盘、指针所在的那一项
pub const ACCENT_SOFT: Color = Color::hex(0xe7effd);
/// 文字选区（半透明的强调色，叠在白底上是浅蓝）
pub const SELECTION: Color = Color::rgba(37, 99, 235, 0.353);
/// 没有焦点时的文字选区（半透明的灰）
pub const SELECTION_INACTIVE: Color = Color::rgba(0, 0, 0, 0.22);
/// 行内代码的底（半透明的灰）
pub const CODE: Color = Color::rgba(0, 0, 0, 0.118);
/// 滚动条
pub const SCROLLBAR: Color = Color::rgba(0, 0, 0, 0.55);
/// 链接：平时、悬停、按下
pub const LINK: [Color; 3] = [ACCENT, ACCENT_HOVER, ACCENT_PRESSED];
/// 提示：深灰底、白字
pub const TOOLTIP: Color = Color::hex(0x2b3036);
/// 提示里的文字
pub const ON_TOOLTIP: Color = Color::WHITE;
/// 禁用时整体的透明度
pub const DISABLED_OPACITY: f32 = 0.5;

/// 按钮、输入框、下拉框、数字输入的高度
pub const CONTROL_HEIGHT: f32 = 30.0;
/// 控件的圆角
pub const CONTROL_RADIUS: f32 = 4.0;

/// 浮层（下拉列表、拖起来的元素）的投影：向下 4、模糊 12，很淡
pub const OVERLAY_SHADOW: Shadow = Shadow::new(0.0, 4.0, 12.0, Color::rgba(16, 24, 32, 0.18));

/// 框架里本来就是动画的东西（拖动排序的行让位、松手后落位、共享元素在原地淡入）用的曲线。
/// 系统打开"减少动态效果"时滑动立即到位
pub(crate) fn slide() -> Curve {
    if crate::system::is_motion_reduced() { Curve::Tween(Tween::new(Duration::ZERO)) } else { Spring::new(Duration::from_millis(280), 1.0).into() }
}

/// 淡入淡出
pub const FADE: Tween = Tween::new(Duration::from_millis(150)).ease_out();

/// 像按钮、输入框一样的白底细线框：悬停时描边深一点，禁用时变淡、铺上中性的底（输入框、下拉框、数字输入共用）
pub fn field_style() -> Style {
    Style::new()
        .radius(CONTROL_RADIUS)
        .background(SURFACE)
        .border(1, BORDER)
        .color(TEXT)
        .font_size(14)
        .hovered(|s| s.border_color(BORDER_HOVER))
        .disabled(|s| s.opacity(DISABLED_OPACITY).background(FILL))
}

thread_local! {
    /// 单行、多行输入框默认的样子：每个线程只建一次，所有输入框共用（克隆只加引用计数）
    static TEXT_FIELD: [Style; 2] = {
        let field = field_style();
        [
            field.clone().width(200).height(CONTROL_HEIGHT).padding_x(8).focused(|s| s.border_color(ACCENT)),
            field.width(320).min_height(64).padding(8).focused(|s| s.border_color(ACCENT)),
        ]
    };
}

/// 输入框默认的样子（`text_field`、`text_area` 构建时用）
pub(crate) fn text_field_look(multiline: bool) -> Style {
    TEXT_FIELD.with(|looks| looks[usize::from(multiline)].clone())
}
