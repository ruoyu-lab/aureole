//! 基础控件默认的样子：每个部位一个样式对象，每个线程只建一次，所有控件共用（克隆只加引用计数）。
//! 颜色、白底细线框都来自核心的 [`aureole_core::defaults`]（输入框是核心的元素，它的样子也在那里）；没有动画，状态一变立即换。

use std::rc::Rc;

use aureole_core::defaults::{
    ACCENT, ACCENT_HOVER, ACCENT_PRESSED, ACCENT_SOFT, BORDER, BORDER_HOVER, CONTROL_HEIGHT, CONTROL_RADIUS, DISABLED_OPACITY, FILL, FILL_PRESSED,
    INDICATOR, LINE, MUTED, OVERLAY_SHADOW, PLACEHOLDER, SURFACE, TEXT, TRACK, field_style,
};
use aureole_core::{Align, Color, Direction, Justify, Shadow, Style, max_content, pct};

/// 按钮、输入框、下拉框、数字输入的高度
pub(crate) const HEIGHT: f32 = CONTROL_HEIGHT;
/// 圆角
pub(crate) const RADIUS: f32 = CONTROL_RADIUS;

/// 所有部位的默认样子
pub(crate) struct Look {
    pub button: Style,
    pub label: Style,
    /// 复选框、开关、单选组的外壳：一行，按内容的宽度，禁用时变淡
    pub toggle_root: Style,
    pub checkbox_box: Style,
    pub mark: Style,
    pub radio_group: Style,
    pub radio_row: Style,
    pub radio_item: Style,
    pub radio_circle: Style,
    pub switch_track: Style,
    pub switch_thumb: Style,
    pub select: Style,
    pub select_arrow: Style,
    pub select_list: Style,
    pub select_option: Style,
    pub select_mark: Style,
    pub slider: Style,
    pub slider_track: Style,
    pub slider_vertical: Style,
    pub slider_track_vertical: Style,
    pub slider_fill: Style,
    pub slider_thumb: Style,
    pub progress: Style,
    pub progress_fill: Style,
    pub number: Style,
    pub number_field: Style,
    pub number_button: Style,
    /// 日期、时间选择的外框
    pub picker: Style,
    pub picker_field: Style,
    pub picker_button: Style,
    pub calendar: Style,
    pub calendar_header: Style,
    pub calendar_nav: Style,
    pub calendar_weekday: Style,
    pub calendar_day: Style,
    pub calendar_footer: Style,
    pub calendar_action: Style,
    pub time_column: Style,
    pub time_item: Style,
    pub color_panel: Style,
    pub color_area: Style,
    pub color_hue: Style,
    pub color_alpha: Style,
    pub color_format: Style,
    pub color_slider_track: Style,
    pub color_thumb: Style,
    pub color_swatch: Style,
    pub file: Style,
    pub file_button: Style,
    pub file_label: Style,
    pub file_clear: Style,
}

thread_local! {
    static LOOK: Rc<Look> = Rc::new(Look::new());
}

/// 默认的样子（这个线程第一次用到时建好）
pub(crate) fn look() -> Rc<Look> {
    LOOK.with(Rc::clone)
}

/// 禁用时变淡
fn dim() -> Style {
    Style::new().disabled(|s| s.opacity(DISABLED_OPACITY))
}

impl Look {
    fn new() -> Self {
        // 像按钮、输入框一样的白底细线框：悬停时描边深一点
        let field = field_style();
        // 勾选框、单选圆：白底深一点的描边，勾上时强调色实底
        let indicator = Style::new()
            .size(16, 16)
            .shrink(0)
            .align(Align::Center)
            .justify(Justify::Center)
            .background(SURFACE)
            .border(1, INDICATOR)
            .color(Color::WHITE)
            .hovered(|s| s.border_color(MUTED))
            .pressed(|s| s.background(FILL_PRESSED))
            .checked(|s| {
                s.background(ACCENT)
                    .border_color(ACCENT)
                    .hovered(|s| s.background(ACCENT_HOVER).border_color(ACCENT_HOVER))
                    .pressed(|s| s.background(ACCENT_PRESSED).border_color(ACCENT_PRESSED))
            });
        Self {
            button: Style::new()
                .height(HEIGHT)
                .padding_x(12)
                .gap(6)
                .radius(RADIUS)
                .align(Align::Center)
                .justify(Justify::Center)
                .width(max_content())
                .background(SURFACE)
                .border(1, BORDER)
                .color(TEXT)
                .font_size(14)
                .hovered(|s| s.background(FILL).border_color(BORDER_HOVER))
                .pressed(|s| s.background(FILL_PRESSED).border_color(BORDER_HOVER))
                .disabled(|s| s.opacity(DISABLED_OPACITY)),
            label: Style::new().color(TEXT).font_size(14),
            toggle_root: dim().gap(8).align(Align::Center).width(max_content()).max_width(pct(100)).radius(RADIUS).font_size(14),
            checkbox_box: indicator.clone().radius(3),
            mark: Style::new().opacity(0.0).checked(|s| s.opacity(1.0)),
            radio_group: dim().gap(8).width(max_content()).max_width(pct(100)).align(Align::Start).radius(RADIUS).font_size(14),
            radio_row: dim()
                .direction(Direction::Row)
                .gap(16)
                .width(max_content())
                .max_width(pct(100))
                .align(Align::Center)
                .radius(RADIUS)
                .font_size(14),
            radio_item: Style::new().gap(8).align(Align::Center),
            radio_circle: indicator.radius(8),
            switch_track: Style::new()
                .size(32, 18)
                .shrink(0)
                .padding(2)
                .radius(9)
                .background(TRACK)
                .hovered(|s| s.background(Color::hex(0xc7cdd5)))
                .pressed(|s| s.background(Color::hex(0xbac1ca)))
                .checked(|s| {
                    s.background(ACCENT)
                        .hovered(|s| s.background(ACCENT_HOVER))
                        .pressed(|s| s.background(ACCENT_PRESSED))
                }),
            switch_thumb: Style::new().size(14, 14).radius(7).background(Color::WHITE).checked(|s| s.translation_x(14)),
            select: field
                .clone()
                .height(HEIGHT)
                .padding_x(8)
                .gap(8)
                .align(Align::Center)
                .width(max_content())
                .min_width(120)
                .open(|s| s.border_color(ACCENT)),
            select_arrow: Style::new().size(12, 12).shrink(0).color(MUTED),
            select_list: Style::new()
                .padding(4)
                .radius(6)
                .background(SURFACE)
                .border(1, LINE)
                .shadow(OVERLAY_SHADOW)
                .color(TEXT)
                .font_size(14)
                .max_height(300),
            select_option: Style::new()
                .height(28)
                .padding_x(8)
                .gap(8)
                .radius(RADIUS)
                .align(Align::Center)
                .focused(|s| s.background(ACCENT_SOFT))
                .pressed(|s| s.background(Color::hex(0xd8e4fb)))
                .selected(|s| s.color(ACCENT)),
            select_mark: Style::new().opacity(0.0).selected(|s| s.opacity(1.0)),
            slider: dim().width(200).height(16).radius(8),
            slider_track: Style::new().height(4).radius(2).background(TRACK),
            slider_vertical: dim().width(16).height(200).radius(8),
            slider_track_vertical: Style::new().width(4).radius(2).background(TRACK),
            slider_fill: Style::new().radius(2).background(ACCENT),
            slider_thumb: Style::new()
                .size(16, 16)
                .radius(8)
                .background(SURFACE)
                .border(1, INDICATOR)
                .hovered(|s| s.border_color(ACCENT))
                .pressed(|s| s.border_color(ACCENT).background(FILL)),
            progress: Style::new().width(200).height(6).radius(3).background(TRACK),
            progress_fill: Style::new().radius(3).background(ACCENT),
            // 描边向内画、不占布局：留出一像素，按钮的底色不盖住描边（number 也一样）
            picker: field
                .clone()
                .height(HEIGHT)
                .padding(1)
                .align(Align::Center)
                .width(max_content())
                .focused(|s| s.border_color(ACCENT))
                .open(|s| s.border_color(ACCENT)),
            picker_field: Style::new()
                .width(104)
                .height(HEIGHT - 2.0)
                .padding_x(8)
                .radius(0)
                .background(Color::TRANSPARENT)
                .border(0, Color::TRANSPARENT)
                .focused(|s| s.border_color(Color::TRANSPARENT))
                .hovered(|s| s.border_color(Color::TRANSPARENT))
                .disabled(|s| s.background(Color::TRANSPARENT).opacity(1.0)),
            picker_button: Style::new()
                .size(HEIGHT - 2.0, HEIGHT - 2.0)
                .shrink(0)
                .radius(RADIUS - 1.0)
                .align(Align::Center)
                .justify(Justify::Center)
                .color(MUTED)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED))
                .disabled(|s| s.color(PLACEHOLDER)),
            // 高度按内容：和更高的东西并排放时不被拉高，四边的留白才一样
            calendar: Style::new()
                .padding(8)
                .gap(6)
                .radius(6)
                .width(max_content())
                .height(max_content())
                .background(SURFACE)
                .border(1, LINE)
                .color(TEXT)
                .font_size(14),
            calendar_header: Style::new().gap(2).height(28).align(Align::Center),
            calendar_nav: Style::new()
                .size(26, 26)
                .shrink(0)
                .radius(RADIUS)
                .align(Align::Center)
                .justify(Justify::Center)
                .color(MUTED)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED))
                .disabled(|s| s.opacity(DISABLED_OPACITY)),
            calendar_weekday: Style::new().size(32, 24).shrink(0).align(Align::Center).justify(Justify::Center).color(MUTED).font_size(12),
            calendar_day: Style::new()
                .size(32, 28)
                .shrink(0)
                .radius(RADIUS)
                .align(Align::Center)
                .justify(Justify::Center)
                .border(1, Color::TRANSPARENT)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED))
                .checked(|s| s.border_color(ACCENT))
                .selected(|s| {
                    s.background(ACCENT)
                        .border_color(ACCENT)
                        .color(Color::WHITE)
                        .hovered(|s| s.background(ACCENT_HOVER).border_color(ACCENT_HOVER))
                        .pressed(|s| s.background(ACCENT_PRESSED).border_color(ACCENT_PRESSED))
                })
                .disabled(|s| s.color(PLACEHOLDER).background(Color::TRANSPARENT).border_color(Color::TRANSPARENT)),
            calendar_footer: Style::new().gap(4).justify(Justify::End).align(Align::Center),
            calendar_action: Style::new()
                .height(26)
                .padding_x(8)
                .font_size(13)
                .background(Color::TRANSPARENT)
                .border_color(Color::TRANSPARENT)
                .color(ACCENT)
                .hovered(|s| s.background(ACCENT_SOFT).border_color(Color::TRANSPARENT))
                .pressed(|s| s.background(Color::hex(0xd8e4fb)).border_color(Color::TRANSPARENT)),
            time_column: Style::new().width(62).height(224),
            time_item: Style::new()
                .height(28)
                .shrink(0)
                .radius(RADIUS)
                .align(Align::Center)
                .justify(Justify::Center)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED))
                .selected(|s| {
                    s.background(ACCENT)
                        .color(Color::WHITE)
                        .hovered(|s| s.background(ACCENT_HOVER))
                        .pressed(|s| s.background(ACCENT_PRESSED))
                })
                .disabled(|s| s.color(PLACEHOLDER).background(Color::TRANSPARENT)),
            // 高度按内容：和更高的东西并排放时不被拉高
            color_panel: Style::new().padding(8).gap(8).radius(6).width(264).height(max_content()).background(SURFACE).border(1, LINE).color(TEXT).font_size(14),
            color_area: Style::new().grow(1).min_width(0).height(150).radius(RADIUS),
            color_hue: Style::new().width(pct(100)).height(16).align(Align::Center),
            color_alpha: Style::new().width(16).shrink(0).align(Align::Center),
            color_slider_track: Style::new().radius(8),
            color_format: Style::new().min_width(0).padding_x(6).gap(2).shrink(0),
            color_thumb: Style::new()
                .size(14, 14)
                .radius(7)
                .border(2, Color::WHITE)
                .shadow(Shadow::new(0.0, 0.0, 2.0, Color::rgba(0, 0, 0, 0.5))),
            color_swatch: Style::new()
                .size(20, 20)
                .padding(2)
                .radius(5)
                .border(1, Color::TRANSPARENT)
                .hovered(|s| s.border_color(BORDER_HOVER))
                .selected(|s| s.border_color(ACCENT)),
            file: dim().gap(8).align(Align::Center).width(300).max_width(pct(100)).padding(2).radius(RADIUS).border(1, Color::TRANSPARENT).font_size(14).open(|s| s.border_color(ACCENT)),
            file_button: Style::new().shrink(0),
            file_label: Style::new().color(PLACEHOLDER).selected(|s| s.color(TEXT)),
            file_clear: Style::new()
                .size(22, 22)
                .shrink(0)
                .radius(RADIUS)
                .align(Align::Center)
                .justify(Justify::Center)
                .color(MUTED)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED)),
            number: field
                .height(HEIGHT)
                .padding(1)
                .align(Align::Center)
                .width(max_content())
                .focused(|s| s.border_color(ACCENT)),
            number_field: Style::new()
                .width(56)
                .height(HEIGHT - 2.0)
                .padding_x(4)
                .radius(0)
                .background(Color::TRANSPARENT)
                .border(0, Color::TRANSPARENT)
                .focused(|s| s.border_color(Color::TRANSPARENT))
                .hovered(|s| s.border_color(Color::TRANSPARENT))
                .disabled(|s| s.background(Color::TRANSPARENT).opacity(1.0)),
            number_button: Style::new()
                .size(HEIGHT - 2.0, HEIGHT - 2.0)
                .shrink(0)
                .radius(RADIUS - 1.0)
                .align(Align::Center)
                .justify(Justify::Center)
                .color(TEXT)
                .hovered(|s| s.background(FILL))
                .pressed(|s| s.background(FILL_PRESSED))
                .disabled(|s| s.color(PLACEHOLDER)),
        }
    }
}
