//! 框架自带的样子：浅色、深色两套调色板（[`Palette::LIGHT`]、[`Palette::DARK`]），都在这里。窗口底色、默认的文字和选区颜色、
//! 占位文字、焦点环、滚动条、提示、浮层的表面，以及基础控件（`aureole-controls`）的默认外观都从这里取；输入框默认的样子也在这里
//! （输入框是核心的元素，其余控件的样子在控件的 crate 里，从这里取这些颜色）。自己做的控件要和基础控件一个样子时也用它们。
//!
//! 用哪一套看应用此刻的外观（[`crate::appearance`]：默认跟着系统，`set_appearance_mode` 固定成浅色或深色），
//! [`palette`] 取。两套只换中性色（底色、表面、文字、描边、轨道），强调色一样；没有主题：要别的样子时在元素上设
//! （窗口底色用 `App::background`，文字颜色、选区颜色设在根元素上会继承，焦点环用 `.focus_ring(..)`，控件用样式对象）。
//! 中性的灰、一个偏蓝的强调色（焦点、选区、勾选），1 像素细线、小圆角，不发光、不用重投影。

use std::time::Duration;

use animation::{Curve, Spring, Tween};
use scene::Color;

use crate::shadow::Shadow;
use crate::style::Style;
use crate::system::Appearance;

/// 一套颜色。框架自带浅色、深色两套，[`palette`] 按应用此刻的外观取
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Palette {
    /// 窗口底色
    pub background: Color,
    /// 控件、浮层的表面
    pub surface: Color,
    /// 正文文字，也是输入框插入点的颜色
    pub text: Color,
    /// 次要文字
    pub muted: Color,
    /// 输入框的占位文字
    pub placeholder: Color,
    /// 控件的描边：按钮、输入框、下拉框、步进按钮
    pub border: Color,
    /// 悬停时控件的描边
    pub border_hover: Color,
    /// 浮层、面板的描边和分隔线（比控件的淡）
    pub line: Color,
    /// 勾选框、单选圆这些小东西的描边：比控件的描边显眼一点，小了也看得清
    pub indicator: Color,
    /// 中性的底色：按钮悬停、日历的一天悬停
    pub fill: Color,
    /// 中性的底色按下时
    pub fill_pressed: Color,
    /// 滑块、进度条的轨道，开关关着时的轨道
    pub track: Color,
    /// 开关关着时轨道悬停
    pub track_hover: Color,
    /// 开关关着时轨道按下
    pub track_pressed: Color,
    /// 强调色：焦点、选区、勾选、打开的开关、进度（两套一样）
    pub accent: Color,
    /// 强调色悬停时
    pub accent_hover: Color,
    /// 强调色按下时
    pub accent_pressed: Color,
    /// 强调色的淡底：下拉列表里键盘、指针所在的那一项
    pub accent_soft: Color,
    /// 强调色的淡底按下时
    pub accent_soft_pressed: Color,
    /// 文字选区（半透明的强调色）
    pub selection: Color,
    /// 没有焦点时的文字选区（半透明的灰）
    pub selection_inactive: Color,
    /// 行内代码的底（半透明）
    pub code: Color,
    /// 滚动条（半透明）
    pub scrollbar: Color,
    /// 提示的底
    pub tooltip: Color,
    /// 提示里的文字
    pub on_tooltip: Color,
}

impl Palette {
    /// 浅色：浅灰的窗口底、白色的表面、深灰的字
    pub const LIGHT: Self = Self {
        background: Color::hex(0xf6f7f9),
        surface: Color::WHITE,
        text: Color::hex(0x1f2328),
        muted: Color::hex(0x5c6570),
        placeholder: Color::hex(0x8c949e),
        border: Color::hex(0xc4cad2),
        border_hover: Color::hex(0x98a1ac),
        line: Color::hex(0xd9dde3),
        indicator: Color::hex(0x8f98a3),
        fill: Color::hex(0xf0f2f5),
        fill_pressed: Color::hex(0xe3e7ec),
        track: Color::hex(0xd5dae0),
        track_hover: Color::hex(0xc7cdd5),
        track_pressed: Color::hex(0xbac1ca),
        accent: Color::hex(0x2563eb),
        accent_hover: Color::hex(0x1f55d0),
        accent_pressed: Color::hex(0x1b48b0),
        accent_soft: Color::hex(0xe7effd),
        accent_soft_pressed: Color::hex(0xd8e4fb),
        selection: Color::rgba(37, 99, 235, 0.353),
        selection_inactive: Color::rgba(0, 0, 0, 0.22),
        code: Color::rgba(0, 0, 0, 0.118),
        scrollbar: Color::rgba(0, 0, 0, 0.55),
        tooltip: Color::hex(0x2b3036),
        on_tooltip: Color::WHITE,
    };

    /// 深色：深灰的窗口底、稍亮一点的表面、浅灰的字；强调色和浅色一样
    pub const DARK: Self = Self {
        background: Color::hex(0x1b1d20),
        surface: Color::hex(0x25282c),
        text: Color::hex(0xe3e6ea),
        muted: Color::hex(0x9aa2ac),
        placeholder: Color::hex(0x6f7782),
        border: Color::hex(0x41464d),
        border_hover: Color::hex(0x5b626b),
        line: Color::hex(0x34383e),
        indicator: Color::hex(0x6c747e),
        fill: Color::hex(0x2f3338),
        fill_pressed: Color::hex(0x383d43),
        track: Color::hex(0x42474e),
        track_hover: Color::hex(0x4c5259),
        track_pressed: Color::hex(0x565d65),
        accent: Color::hex(0x2563eb),
        accent_hover: Color::hex(0x1f55d0),
        accent_pressed: Color::hex(0x1b48b0),
        accent_soft: Color::hex(0x1f3358),
        accent_soft_pressed: Color::hex(0x273e69),
        selection: Color::rgba(37, 99, 235, 0.45),
        selection_inactive: Color::rgba(255, 255, 255, 0.18),
        code: Color::rgba(255, 255, 255, 0.12),
        scrollbar: Color::rgba(255, 255, 255, 0.45),
        tooltip: Color::hex(0x3a3f46),
        on_tooltip: Color::WHITE,
    };

    /// 这种外观用的一套
    pub const fn of(appearance: Appearance) -> &'static Self {
        match appearance {
            Appearance::Dark => &Self::DARK,
            _ => &Self::LIGHT,
        }
    }

    /// 链接：平时、悬停、按下
    pub const fn link(&self) -> [Color; 3] {
        [self.accent, self.accent_hover, self.accent_pressed]
    }
}

/// 应用此刻的外观（[`crate::appearance`]）用的一套。在绑定、样式对象的闭包里读时外观变了跟着变：
/// `Style::new().background(|| palette().surface)`
pub fn palette() -> &'static Palette {
    Palette::of(crate::system::appearance())
}

/// 调色板里的一种颜色，跟着应用的外观变：放进样式对象、链式方法时是一个读外观的绑定
fn tone(pick: fn(&Palette) -> Color) -> impl Fn() -> Color + 'static {
    move || pick(palette())
}

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

/// 像按钮、输入框一样的表面加细线框：悬停时描边显眼一点，禁用时变淡、铺上中性的底（输入框、下拉框、数字输入共用）。
/// 颜色跟着应用的外观
pub fn field_style() -> Style {
    Style::new()
        .radius(CONTROL_RADIUS)
        .background(tone(|p| p.surface))
        .border(1, tone(|p| p.border))
        .color(tone(|p| p.text))
        .font_size(14)
        .hovered(|s| s.border_color(tone(|p| p.border_hover)))
        .disabled(|s| s.opacity(DISABLED_OPACITY).background(tone(|p| p.fill)))
}

thread_local! {
    /// 单行、多行输入框默认的样子：每个线程只建一次，所有输入框共用（克隆只加引用计数）
    static TEXT_FIELD: [Style; 2] = {
        let field = field_style();
        [
            field.clone().width(200).height(CONTROL_HEIGHT).padding_x(8).focused(|s| s.border_color(tone(|p| p.accent))),
            field.width(320).min_height(64).padding(8).focused(|s| s.border_color(tone(|p| p.accent))),
        ]
    };
}

/// 输入框默认的样子（`text_field`、`text_area` 构建时用）
pub(crate) fn text_field_look(multiline: bool) -> Style {
    TEXT_FIELD.with(|looks| looks[usize::from(multiline)].clone())
}
