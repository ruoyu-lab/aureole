//! 颜色选择：一块调饱和度和亮度的色板、右边的透明度、下面的色相、能选编码类型的输入框、常用颜色。可以直接放进界面（[`ColorPanel`]），
//! 也可以点色块弹出（[`ColorPicker`]）。逻辑在 [`ColorState`] 上，默认的面板只用它搭。

use std::cell::Cell;
use std::rc::Rc;

use reactive::{Effect, Signal, signal, untracked};

use super::look::look;
use super::picker::{Shell, field_with_popup};
use super::select::Select;
use super::slider::Slider;
use super::{Changes, Flag, Press, part};
use aureole_core::{
    Align, ChangeEvent, ChangeSource, ClickEvent, Color, ColorSpace, Direction, Dismiss, DragEvent, Element, FieldChangeEvent, FieldValue, FocusHandle, GesturePhase, Gradient, IntoChangeHandler,
    IntoProp, KeyEvent, PointerEvent, Prop, PropagationPhase, RangeValue, Rect, Role, States, Style, View, canvas, column, deg, defaults::OVERLAY_SHADOW, pct, rect, row, text_field, without_animation,
};

/// 颜色写成文字的编码类型
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorFormat {
    /// 十六进制：`#2563EB`，带透明度时 `#2563EB80`
    Hex,
    /// 红绿蓝，0 到 255：`37, 99, 235`，带透明度时 `37, 99, 235, 0.5`
    Rgb,
    /// 色相（度）、饱和度、亮度：`221, 83%, 53%`
    Hsl,
    /// 色相（度）、饱和度、明度：`221, 84%, 92%`
    Hsv,
}

impl ColorFormat {
    /// 全部的编码类型
    pub const ALL: [Self; 4] = [Self::Hex, Self::Rgb, Self::Hsl, Self::Hsv];

    /// 给人看的名字
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hex => "HEX",
            Self::Rgb => "RGB",
            Self::Hsl => "HSL",
            Self::Hsv => "HSV",
        }
    }

    /// 把颜色写成这种编码
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let blue = Color::hex(0x2563eb);
    /// assert_eq!(ColorFormat::Hex.write(blue), "#2563EB");
    /// assert_eq!(ColorFormat::Rgb.write(blue), "37, 99, 235");
    /// assert_eq!(ColorFormat::Rgb.write(blue.with_alpha(0.5)), "37, 99, 235, 0.5");
    /// assert_eq!(ColorFormat::Hsl.write(blue), "221, 83%, 53%");
    /// ```
    pub fn write(self, color: Color) -> String {
        let alpha = color.0[3];
        // 透明度最多写两位小数，去掉多余的零
        let suffix = if alpha == 255 {
            String::new()
        } else {
            let text = format!("{:.2}", f32::from(alpha) / 255.0);
            format!(", {}", text.trim_end_matches('0').trim_end_matches('.'))
        };
        let percent = |v: f32| (v * 100.0).round() as u32;
        let degrees = |turns: f32| (turns * 360.0).round() as u32 % 360;
        match self {
            Self::Hex => color.to_hex_string().to_uppercase(),
            Self::Rgb => format!("{}, {}, {}{suffix}", color.0[0], color.0[1], color.0[2]),
            Self::Hsl => {
                let (h, s, l) = color.to_hsl();
                format!("{}, {}%, {}%{suffix}", degrees(h.turns()), percent(s), percent(l))
            }
            Self::Hsv => {
                let (h, s, v) = color.to_hsv();
                format!("{}, {}%, {}%{suffix}", degrees(h.turns()), percent(s), percent(v))
            }
        }
    }

    /// 读文字：现在是这种编码时不用写名字。带名字的写法（`#…`、`rgb(…)`、`hsl(…)`、`hsv(…)`）不管现在是哪种都认。读不懂时是 `None`
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// assert_eq!(ColorFormat::Rgb.parse("255, 0, 0"), Some(Color::hex(0xff0000)));
    /// assert_eq!(ColorFormat::Rgb.parse("#ff0000"), Some(Color::hex(0xff0000)));
    /// assert_eq!(ColorFormat::Hex.parse("rgb(255 0 0 / 50%)"), Some(Color::rgba(255, 0, 0, 0.5)));
    /// assert_eq!(ColorFormat::Hsl.parse("0, 100%, 50%"), Some(Color::hex(0xff0000)));
    /// assert_eq!(ColorFormat::Rgb.parse("300, 0, 0"), None);
    /// ```
    pub fn parse(self, text: &str) -> Option<Color> {
        let lower = text.trim().to_ascii_lowercase();
        if lower.starts_with('#') {
            return Color::parse_hex(&lower);
        }
        let (format, body) = match lower.split_once('(') {
            Some((name, rest)) => {
                let format = match name.trim() {
                    "rgb" | "rgba" => Self::Rgb,
                    "hsl" | "hsla" => Self::Hsl,
                    "hsv" | "hsva" => Self::Hsv,
                    _ => return None,
                };
                (format, rest.strip_suffix(')')?)
            }
            None => (self, lower.as_str()),
        };
        if format == Self::Hex {
            return Color::parse_hex(body);
        }
        let parts: Vec<&str> = body.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
        let [first, second, third, rest @ ..] = parts.as_slice() else { return None };
        let alpha = match rest {
            [] => 1.0,
            [alpha] => unit(alpha)?,
            _ => return None,
        };
        let color = match format {
            Self::Rgb => Color::rgb(channel(first)?, channel(second)?, channel(third)?),
            Self::Hsl => Color::hsl(deg(hue_degrees(first)?), percentage(second)?, percentage(third)?),
            _ => Color::hsv(deg(hue_degrees(first)?), percentage(second)?, percentage(third)?),
        };
        Some(color.with_alpha(alpha))
    }
}

/// 读一个 0 到 255 的通道，也可以写百分数
fn channel(text: &str) -> Option<u8> {
    match text.strip_suffix('%') {
        Some(percent) => {
            let value: f32 = percent.parse().ok()?;
            (0.0..=100.0).contains(&value).then(|| (value * 2.55).round() as u8)
        }
        None => text.parse().ok(),
    }
}

/// 读色相，单位是度，可以写 `deg`
fn hue_degrees(text: &str) -> Option<f32> {
    let value: f32 = text.strip_suffix("deg").unwrap_or(text).parse().ok()?;
    value.is_finite().then_some(value)
}

/// 读 0 到 100 的比例，`%` 可以不写，结果是 0..=1
fn percentage(text: &str) -> Option<f32> {
    let value: f32 = text.strip_suffix('%').unwrap_or(text).parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value / 100.0)
}

/// 读透明度：0 到 1，或者百分数
fn unit(text: &str) -> Option<f32> {
    match text.strip_suffix('%') {
        Some(_) => percentage(text),
        None => {
            let value: f32 = text.parse().ok()?;
            (0.0..=1.0).contains(&value).then_some(value)
        }
    }
}

/// 颜色选择上的文字。[`ColorTexts::chinese`] 是默认的，[`ColorTexts::english`] 是英文的，也可以只改其中几项
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorTexts {
    /// 色板的名字（辅助技术念的）
    pub area: String,
    /// 色板念出来的值的模板：`{saturation}`、`{brightness}` 是百分数
    pub area_value: String,
    /// "色相"
    pub hue: String,
    /// "透明度"
    pub alpha: String,
    /// 输入框的名字
    pub value: String,
    /// 选编码类型的下拉框的名字
    pub format: String,
    /// 弹出色板的按钮的名字
    pub open: String,
}

impl ColorTexts {
    /// 中文（默认）
    pub fn chinese() -> Self {
        Self {
            area: "饱和度和亮度".into(),
            area_value: "饱和度 {saturation}%，亮度 {brightness}%".into(),
            hue: "色相".into(),
            alpha: "透明度".into(),
            value: "颜色值".into(),
            format: "颜色编码".into(),
            open: "选择颜色".into(),
        }
    }

    /// 英文
    pub fn english() -> Self {
        Self {
            area: "Saturation and brightness".into(),
            area_value: "Saturation {saturation}%, brightness {brightness}%".into(),
            hue: "Hue".into(),
            alpha: "Opacity".into(),
            value: "Color value".into(),
            format: "Color format".into(),
            open: "Choose color".into(),
        }
    }
}

impl Default for ColorTexts {
    fn default() -> Self {
        Self::chinese()
    }
}

/// 用户动了哪里之后交出来的
type Report = Rc<dyn Fn(Color, ChangeSource)>;
/// 自己重做的界面
type ColorView = Rc<dyn Fn(ColorState) -> Element>;

/// 面板的设置
#[derive(Clone)]
struct PanelOptions {
    alpha: bool,
    swatches: Vec<Color>,
    texts: ColorTexts,
    value_field: bool,
    formats: Vec<ColorFormat>,
    format: ColorFormat,
    format_signal: Option<Signal<ColorFormat>>,
    tile_radius: f32,
    view: Option<ColorView>,
    panel: Style,
    area: Style,
    thumb: Style,
    hue: Style,
    alpha_shell: Style,
    slider_track: Style,
    slider_thumb: Style,
    value: Style,
    format_select: Style,
    preview: Style,
    swatch: Style,
}

impl Default for PanelOptions {
    fn default() -> Self {
        Self {
            alpha: true,
            swatches: Vec::new(),
            texts: ColorTexts::chinese(),
            value_field: true,
            formats: ColorFormat::ALL.to_vec(),
            format: ColorFormat::Hex,
            format_signal: None,
            tile_radius: 3.0,
            view: None,
            panel: Style::new(),
            area: Style::new(),
            thumb: Style::new(),
            hue: Style::new(),
            alpha_shell: Style::new(),
            slider_track: Style::new(),
            slider_thumb: Style::new(),
            value: Style::new(),
            format_select: Style::new(),
            preview: Style::new(),
            swatch: Style::new(),
        }
    }
}

/// 透明的地方画的棋盘格：看得出有多透明
fn checkerboard() -> Element {
    const TILE: f32 = 4.0;
    canvas(|cx| {
        let [width, height] = cx.size();
        cx.fill_rect(0.0, 0.0, width, height, Color::WHITE);
        let dark = Color::hex(0xcdd2d8);
        for y in 0..=(height / TILE) as u32 {
            for x in 0..=(width / TILE) as u32 {
                if (x + y) % 2 == 1 {
                    cx.fill_rect(x as f32 * TILE, y as f32 * TILE, TILE, TILE, dark);
                }
            }
        }
    })
}

/// 一小块颜色：透明的地方露出棋盘格，最上面一圈半透明的细线，白色、浅色的也看得出边
fn color_tile(color: impl IntoProp<Color>, radius: f32) -> Element {
    row()
        .size(pct(100), pct(100))
        .radius(radius)
        .clip(true)
        .child(layer(checkerboard(), 0.0))
        .child(layer(rect().background(color), 0.0))
        .child(layer(rect().border(1, Color::rgba(0, 0, 0, 0.14)), radius))
}

/// 铺满父元素的一层
fn layer(el: Element, radius: f32) -> Element {
    el.position(0, 0).size(pct(100), pct(100)).radius(radius)
}

fn alpha_of(color: Color) -> f32 {
    f32::from(color.0[3]) / 255.0
}

/// 输入框收的字：十六进制、数字、`rgb(` 这样的名字、分隔符
fn value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '#' | '(' | ')' | ',' | '.' | '%' | '/' | ' ' | '-')
}

/// 色相的渐变：红黄绿青蓝品红再回到红，在 sRGB 编码值上插值，和色板上的颜色对得上
fn hue_gradient() -> Gradient {
    Gradient::linear(deg(90.0), (0..=6).map(|i| (i as f32 / 6.0, Color::hsv(deg(i as f32 * 60.0), 1.0, 1.0)))).color_space(ColorSpace::Srgb)
}

/// 颜色面板的逻辑，不带界面：色相、饱和度、亮度、透明度、编码类型，改了之后通知谁。默认的面板只用这里的方法搭，
/// 所以自己重做界面（[`ColorPanel::view`]）时绑定的信号、`on_change`、`on_commit` 都照常工作。
///
/// 色相、饱和度分开记：纯黑、纯灰换成 RGB 就丢了色相，拖到黑色再拖回来颜色还在。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let color = signal(Color::hex(0x2563eb));
/// // 只要一个色相滑块和输入框
/// ColorPanel::new(color).on_commit(|c: Color| println!("{c:?}")).view(|state: ColorState| {
///     let value = signal(state.text());
///     column()
///         .gap(8)
///         .child(Slider::new(state.hue_signal()).range(0.0, 360.0).on_change(move |e: &ChangeEvent<f32>| state.apply(e.source)).on_commit(move |e: &ChangeEvent<f32>| state.commit(e.source)))
///         .child(text_field(value).on_submit(move |_| {
///             state.set_text(&value.get_untracked(), ChangeSource::Keyboard);
///         }))
///         .child(rect().size(40, 40).background(move || state.color()))
/// });
/// ```
#[derive(Clone, Copy)]
pub struct ColorState {
    value: Signal<Color>,
    hue: Signal<f32>,
    saturation: Signal<f32>,
    brightness: Signal<f32>,
    alpha: Signal<f32>,
    format: Signal<ColorFormat>,
    shared: Signal<Rc<ColorShared>>,
}

/// 状态里不变的部分
struct ColorShared {
    changed: Report,
    committed: Report,
    alpha_enabled: bool,
    formats: Vec<ColorFormat>,
}

impl ColorState {
    fn new(options: &PanelOptions, value: Signal<Color>, changed: Report, committed: Report) -> Self {
        let color = value.get_untracked();
        let (hue, saturation, brightness) = color.to_hsv();
        let state = Self {
            value,
            hue: signal(hue.turns() * 360.0),
            saturation: signal(saturation),
            brightness: signal(brightness),
            alpha: signal(alpha_of(color)),
            format: options.format_signal.unwrap_or_else(|| signal(options.format)),
            shared: signal(Rc::new(ColorShared { changed, committed, alpha_enabled: options.alpha, formats: options.formats.clone() })),
        };
        Effect::new(move || {
            let color = value.get();
            state.follow(color);
        });
        state
    }

    fn shared(self) -> Rc<ColorShared> {
        self.shared.with_untracked(Rc::clone)
    }

    fn combined(self) -> Color {
        untracked(|| Color::hsv(deg(self.hue.get()), self.saturation.get(), self.brightness.get()).with_alpha(self.alpha.get()))
    }

    /// 值从外面变了：能保留的（灰色的色相、黑色的饱和度）留着
    fn follow(self, color: Color) {
        if self.combined() == color {
            return;
        }
        let (hue, saturation, brightness) = color.to_hsv();
        if saturation > 0.0 && brightness > 0.0 {
            self.hue.set(hue.turns() * 360.0);
        }
        if brightness > 0.0 {
            self.saturation.set(saturation);
        }
        self.brightness.set(brightness);
        self.alpha.set(alpha_of(color));
    }

    /// 现在的颜色，响应式
    pub fn color(self) -> Color {
        self.value.get()
    }

    /// 色相，0..360 度，响应式
    pub fn hue(self) -> f32 {
        self.hue.get()
    }

    /// 饱和度，0..=1，响应式
    pub fn saturation(self) -> f32 {
        self.saturation.get()
    }

    /// 亮度，0..=1，响应式
    pub fn brightness(self) -> f32 {
        self.brightness.get()
    }

    /// 透明度，0..=1，响应式
    pub fn alpha(self) -> f32 {
        self.alpha.get()
    }

    /// 色相的信号（0..360）：绑给滑块用。滑块改了之后调用 [`ColorState::apply`]
    pub fn hue_signal(self) -> Signal<f32> {
        self.hue
    }

    /// 透明度的信号（0..=1）：绑给滑块用。滑块改了之后调用 [`ColorState::apply`]
    pub fn alpha_signal(self) -> Signal<f32> {
        self.alpha
    }

    /// 能不能调透明度（[`ColorPanel::alpha`]）
    pub fn alpha_enabled(self) -> bool {
        self.shared().alpha_enabled
    }

    /// 把色相、饱和度、亮度、透明度的信号合成颜色，写进绑定的信号。变了才写、才调用 `on_change`
    pub fn apply(self, source: ChangeSource) {
        let color = self.combined();
        if self.value.get_untracked() != color {
            self.value.set(color);
            (self.shared().changed)(color, source);
        }
    }

    /// 改完了：调用 `on_commit`。拖动松手、按键之后调用
    pub fn commit(self, source: ChangeSource) {
        (self.shared().committed)(self.value.get_untracked(), source);
    }

    /// 改色相（度），调用 `on_change`
    pub fn set_hue(self, hue: f32, source: ChangeSource) {
        self.hue.set(hue.rem_euclid(360.0));
        self.apply(source);
    }

    /// 改饱和度和亮度（0..=1，超出的夹回来），调用 `on_change`
    pub fn set_saturation_brightness(self, saturation: f32, brightness: f32, source: ChangeSource) {
        self.saturation.set(saturation.clamp(0.0, 1.0));
        self.brightness.set(brightness.clamp(0.0, 1.0));
        self.apply(source);
    }

    /// 改透明度（0..=1，超出的夹回来），调用 `on_change`。不能调透明度时什么都不做
    pub fn set_alpha(self, alpha: f32, source: ChangeSource) {
        if self.alpha_enabled() {
            self.alpha.set(alpha.clamp(0.0, 1.0));
            self.apply(source);
        }
    }

    /// 选了一个完整的颜色（点常用颜色、打字），`on_change` 和 `on_commit` 都调用。不能调透明度时总是不透明的。变了才调用
    pub fn set_color(self, color: Color, source: ChangeSource) {
        let color = if self.alpha_enabled() { color } else { color.with_alpha(1.0) };
        if self.value.get_untracked() != color {
            self.value.set(color);
            let shared = self.shared();
            (shared.changed)(color, source);
            (shared.committed)(color, source);
        }
    }

    /// 现在选的编码类型，响应式
    pub fn format(self) -> ColorFormat {
        self.format.get()
    }

    /// 编码类型的信号：绑给下拉框用
    pub fn format_signal(self) -> Signal<ColorFormat> {
        self.format
    }

    /// 换编码类型
    pub fn set_format(self, format: ColorFormat) {
        self.format.set(format);
    }

    /// 能选哪些编码类型（[`ColorPanel::formats`]）
    pub fn formats(self) -> Vec<ColorFormat> {
        self.shared().formats.clone()
    }

    /// 现在的颜色写成现在的编码，响应式
    pub fn text(self) -> String {
        self.format.get().write(self.value.get())
    }

    /// 读用户打的字（见 [`ColorFormat::parse`]），读懂了就是 [`ColorState::set_color`]。读不懂时返回 `false`，颜色不变。
    /// 和现在显示的一样时什么都不做，返回 `true`：写成文字时取了整（HSL、HSV），没改过的字再读回来可能差一点，不能因此改了颜色
    pub fn set_text(self, text: &str, source: ChangeSource) -> bool {
        let format = self.format.get_untracked();
        if text.trim() == format.write(self.value.get_untracked()) {
            return true;
        }
        match format.parse(text) {
            Some(color) => {
                self.set_color(color, source);
                true
            }
            None => false,
        }
    }
}

/// 一个滑块，轨道透明，下面垫着 `below`
fn track_slider(options: &PanelOptions, slider: Slider, name: String, below: Element, shell: Element) -> Element {
    let l = look();
    let clear = Style::new().background(Color::TRANSPARENT);
    let slider = slider.track_style(&clear).fill_style(&clear).thumb_style(&options.slider_thumb).build().accessibility_name(name.clone()).name(name).size(pct(100), pct(100));
    let below = part(layer(below, 0.0), &l.color_slider_track, &options.slider_track, &States::new());
    shell.child(below).child(slider)
}

/// 面板的内容：自己重做的界面，或者默认的
fn color_panel(options: &Rc<PanelOptions>, state: ColorState, focus_now: bool) -> Element {
    match &options.view {
        Some(view) => view(state),
        None => default_panel(options, state, focus_now),
    }
}

/// 默认的面板：只用 [`ColorState`] 的公开方法
fn default_panel(options: &PanelOptions, state: ColorState, focus_now: bool) -> Element {
    let l = look();
    let area_focus = FocusHandle::new();

    // 色板：横着是饱和度，竖着是亮度
    let bounds = signal(Rect::default());
    let pointer = signal([0.0f32, 0.0]);
    let press = Press::new();
    let states = press.states();
    let move_to = move |[x, y]: [f32; 2]| {
        let size = bounds.get_untracked();
        state.set_saturation_brightness(x / size.width.max(1.0), 1.0 - y / size.height.max(1.0), ChangeSource::Pointer);
    };
    let nudge = move |saturation: f32, brightness: f32| {
        let (s, b) = untracked(|| (state.saturation(), state.brightness()));
        state.set_saturation_brightness(s + saturation, b + brightness, ChangeSource::Keyboard);
        state.commit(ChangeSource::Keyboard);
    };
    let white = layer(rect().background(Gradient::linear(deg(90.0), [(0.0, Color::WHITE), (1.0, Color::WHITE.with_alpha(0.0))]).color_space(ColorSpace::Srgb)), 4.0);
    let black = layer(rect().background(Gradient::linear(deg(180.0), [(0.0, Color::BLACK.with_alpha(0.0)), (1.0, Color::BLACK)]).color_space(ColorSpace::Srgb)), 4.0);
    let knob = signal(Rect::default());
    let thumb = part(rect(), &l.color_thumb, &options.thumb, &states)
        .position(0, 0)
        .report_bounds(knob)
        .translation_x(move || state.saturation() * bounds.get().width - knob.get().width / 2.0)
        .translation_y(move || (1.0 - state.brightness()) * bounds.get().height - knob.get().height / 2.0);
    let started = Rc::new(Cell::new(Color::TRANSPARENT));
    let area_value = options.texts.area_value.clone();
    let area = part(press.attach(column()), &l.color_area, &options.area, &states)
        .name("color-area")
        .focus_handle(area_focus)
        .on_mount(move || {
            if focus_now {
                area_focus.focus();
            }
        })
        .accessibility_role(Role::Slider)
        .accessibility_name(options.texts.area.clone())
        .accessibility_value(move || RangeValue::new(f64::from(state.saturation() * 100.0).round(), 0.0, 100.0).step(1.0))
        .accessibility_value_text(move || {
            let percent = |v: f32| ((v * 100.0).round() as u32).to_string();
            area_value.replace("{saturation}", &percent(state.saturation())).replace("{brightness}", &percent(state.brightness()))
        })
        .on_value_request(move |v| {
            let brightness = untracked(|| state.brightness());
            state.set_saturation_brightness(v as f32 / 100.0, brightness, ChangeSource::Accessibility);
            state.commit(ChangeSource::Accessibility);
        })
        .background(move || Color::hsv(deg(state.hue()), 1.0, 1.0))
        .report_bounds(bounds)
        .on_pointer_move(move |e: &PointerEvent| pointer.set(e.position))
        .on_pointer_down(move |e: &PointerEvent| pointer.set(e.position))
        .on_press(move |down| {
            if down {
                started.set(untracked(|| state.color()));
                move_to(pointer.get_untracked());
            } else if untracked(|| state.color()) != started.get() {
                state.commit(ChangeSource::Pointer);
            }
        })
        .on_drag(move |e: &DragEvent| {
            if matches!(e.phase, GesturePhase::Start | GesturePhase::Move) {
                without_animation(|| move_to(e.position));
            }
        })
        .builtin_handlers(|el| {
            el.on_key_down(move |e: &KeyEvent| {
                const STEP: f32 = 0.01;
                const FAR: f32 = 0.1;
                if e.is("ArrowRight") {
                    nudge(STEP, 0.0);
                } else if e.is("ArrowLeft") {
                    nudge(-STEP, 0.0);
                } else if e.is("ArrowUp") {
                    nudge(0.0, STEP);
                } else if e.is("ArrowDown") {
                    nudge(0.0, -STEP);
                } else if e.is("PageUp") {
                    nudge(0.0, FAR);
                } else if e.is("PageDown") {
                    nudge(0.0, -FAR);
                } else if e.is("Home") {
                    nudge(-1.0, 0.0);
                } else if e.is("End") {
                    nudge(1.0, 0.0);
                } else {
                    return;
                }
                e.stop_propagation();
            })
        })
        .child(white)
        .child(black)
        .child(thumb);

    // 色板的右边是透明度，竖着放，上面是不透明的
    let mut top = row().gap(8).child(area);
    if state.alpha_enabled() {
        let fade = move || {
            let solid = Color::hsv(deg(state.hue()), state.saturation(), state.brightness());
            Gradient::linear(deg(0.0), [(0.0, solid.with_alpha(0.0)), (1.0, solid)]).color_space(ColorSpace::Srgb)
        };
        let below = row().clip(true).child(layer(checkerboard(), 0.0)).child(layer(rect().background(fade), 0.0));
        let slider = Slider::new(state.alpha_signal())
            .vertical(true)
            .range(0.0, 1.0)
            .step(0.01)
            .on_change(move |e: &ChangeEvent<f32>| state.apply(e.source))
            .on_commit(move |e: &ChangeEvent<f32>| state.commit(e.source));
        let shell = part(column(), &l.color_alpha, &options.alpha_shell, &States::new());
        top = top.child(track_slider(options, slider, options.texts.alpha.clone(), below, shell));
    }

    // 色相
    let hue = {
        let slider = Slider::new(state.hue_signal())
            .range(0.0, 360.0)
            .step(1.0)
            .on_change(move |e: &ChangeEvent<f32>| state.apply(e.source))
            .on_commit(move |e: &ChangeEvent<f32>| state.commit(e.source));
        let shell = part(row(), &l.color_hue, &options.hue, &States::new());
        track_slider(options, slider, options.texts.hue.clone(), rect().background(hue_gradient()), shell)
    };
    let mut body = part(column(), &l.color_panel, &options.panel, &States::new()).child(top).child(hue);

    // 预览、编码类型、输入框
    if options.value_field {
        let draft = signal(untracked(|| state.text()));
        Effect::new(move || {
            let text = state.text();
            if untracked(|| draft.get()) != text {
                draft.set(text);
            }
        });
        let apply = Rc::new(move || {
            if !state.set_text(&draft.get_untracked(), ChangeSource::Keyboard) || draft.get_untracked().trim() != untracked(|| state.text()) {
                // 读不懂，或者写法不整齐：换成整理过的写法
                draft.set(untracked(|| state.text()));
            }
        });
        let (blur, submit) = (apply.clone(), apply);
        let tile_radius = options.tile_radius;
        let preview = part(row().size(30, 30).shrink(0), &Style::new(), &options.preview, &States::new()).child(color_tile(move || state.color(), tile_radius));
        let field = text_field(draft)
            .accepted_chars(value_char)
            .tabular_numbers(true)
            .accessibility_name(options.texts.value.clone())
            .name("color-value")
            // 不按输入框默认的宽度：面板多宽都对齐
            .width(0)
            .grow(1)
            .min_width(0)
            .on_focus(move |focused| {
                if !focused {
                    blur();
                }
            })
            .on_submit(move |_| submit());
        let field = part(field, &Style::new(), &options.value, &States::new());
        let mut line = row().gap(6).align(Align::Center).child(preview);
        let formats = state.formats();
        if formats.len() > 1 {
            let select = formats.iter().fold(Select::new(state.format_signal()), |select, format| select.option(*format, format.label()));
            let select = select.style(&l.color_format).style(&options.format_select).build().accessibility_name(options.texts.format.clone()).name("color-format");
            line = line.child(select);
        }
        body = body.child(line.child(field));
    }

    // 常用颜色
    if !options.swatches.is_empty() {
        let tile_radius = options.tile_radius;
        let swatches = options.swatches.iter().fold(row().wrap(true).gap(4).focus_arrows(Direction::Row).width(pct(100)), |line, &swatch| {
            let press = Press::new();
            let states = press.states().selected(move || state.color() == swatch);
            let el = press
                .attach(row())
                .accessibility_role(Role::Button)
                .accessibility_name(ColorFormat::Hex.write(swatch))
                .name(format!("swatch-{}", ColorFormat::Hex.write(swatch)))
                .builtin_handlers(|el| {
                    el.on_click(move |e: &ClickEvent| {
                        state.set_color(swatch, e.source.into());
                        e.stop_propagation();
                    })
                })
                .child(color_tile(swatch, tile_radius));
            line.child(part(el, &l.color_swatch, &options.swatch, &states))
        });
        body = body.child(swatches);
    }
    body
}

/// 共有的设置方法
macro_rules! panel_options {
    () => {
        /// 能不能调透明度（默认能，色板的右边有一个竖着的滑块）。不能时值总是不透明的
        pub fn alpha(mut self, alpha: bool) -> Self {
            self.options.alpha = alpha;
            self
        }

        /// 常用颜色：点一下就选
        pub fn swatches(mut self, colors: impl IntoIterator<Item = Color>) -> Self {
            self.options.swatches = colors.into_iter().collect();
            self
        }

        /// 换掉文字（默认中文）
        pub fn texts(mut self, texts: ColorTexts) -> Self {
            self.options.texts = texts;
            self
        }

        /// 一开始用哪种编码（默认十六进制）。用户在下拉框里能换，要记住用户的选择用 [`Self::format_signal`]
        pub fn format(mut self, format: ColorFormat) -> Self {
            self.options.format = format;
            self
        }

        /// 编码类型绑定到这个信号：记住用户的选择，几个控件共用一种编码
        pub fn format_signal(mut self, format: Signal<ColorFormat>) -> Self {
            self.options.format_signal = Some(format);
            self
        }

        /// 能选哪些编码（默认全部）。只有一种时不显示下拉框
        pub fn formats(mut self, formats: impl IntoIterator<Item = ColorFormat>) -> Self {
            self.options.formats = formats.into_iter().collect();
            if let Some(first) = self.options.formats.first().filter(|_| !self.options.formats.contains(&self.options.format)) {
                self.options.format = *first;
            }
            self
        }

        /// 显示不显示下面的输入框、预览和编码类型（默认显示）
        pub fn value_field(mut self, show: bool) -> Self {
            self.options.value_field = show;
            self
        }

        /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit` 都照常工作，逻辑都在 [`ColorState`] 上。
        /// 样式对象的那些方法这时不起作用
        pub fn view<C: View>(mut self, view: impl Fn(ColorState) -> C + 'static) -> Self {
            self.options.view = Some(Rc::new(move |state| view(state).into_element()));
            self
        }

        /// 色块的圆角：常用颜色、预览（默认 3）。色块里面是几层叠起来的，圆角要一起改，所以单独一个设置
        pub fn tile_radius(mut self, radius: f32) -> Self {
            self.options.tile_radius = radius;
            self
        }

        /// 改色相滑块的外壳（默认占满面板的宽、高 16）
        pub fn hue_style(mut self, style: &Style) -> Self {
            self.options.hue = self.options.hue.extend(style);
            self
        }

        /// 改透明度滑块的外壳（默认宽 16，和色板一样高）
        pub fn alpha_style(mut self, style: &Style) -> Self {
            self.options.alpha_shell = self.options.alpha_shell.extend(style);
            self
        }

        /// 改色相、透明度滑块下面那条渐变（默认圆角 8）
        pub fn slider_track_style(mut self, style: &Style) -> Self {
            self.options.slider_track = self.options.slider_track.extend(style);
            self
        }

        /// 改色相、透明度滑块的圆钮
        pub fn slider_thumb_style(mut self, style: &Style) -> Self {
            self.options.slider_thumb = self.options.slider_thumb.extend(style);
            self
        }

        /// 改色板的样子（默认占满剩下的宽、高 150）
        pub fn area_style(mut self, style: &Style) -> Self {
            self.options.area = self.options.area.extend(style);
            self
        }

        /// 改色板上圆钮的样子
        pub fn thumb_style(mut self, style: &Style) -> Self {
            self.options.thumb = self.options.thumb.extend(style);
            self
        }

        /// 改面板里输入框的样子
        pub fn value_field_style(mut self, style: &Style) -> Self {
            self.options.value = self.options.value.extend(style);
            self
        }

        /// 改选编码类型的下拉框的样子
        pub fn format_style(mut self, style: &Style) -> Self {
            self.options.format_select = self.options.format_select.extend(style);
            self
        }

        /// 改输入框旁边的预览的样子（默认 30 见方）
        pub fn preview_style(mut self, style: &Style) -> Self {
            self.options.preview = self.options.preview.extend(style);
            self
        }

        /// 改常用颜色每一块的样子（默认 20 见方）：选中状态是当前的值
        pub fn swatch_style(mut self, style: &Style) -> Self {
            self.options.swatch = self.options.swatch.extend(style);
            self
        }
    };
}

/// 颜色面板的构建器；[`color_panel_view`] 是简写。一直显示着，要弹出的用 [`ColorPicker`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let color = signal(Color::hex(0x2563eb));
/// ColorPanel::new(color).swatches([Color::hex(0xef4444), Color::hex(0x22c55e)]).on_change(|c: Color| println!("{c:?}"));
/// // 不要透明度，只用 RGB
/// ColorPanel::new(color).alpha(false).formats([ColorFormat::Rgb]);
/// ```
///
/// 部位：整个面板（`.style`）、色板（[`ColorPanel::area_style`]）、色板上的圆钮（[`ColorPanel::thumb_style`]）、色相滑块的外壳
/// （[`ColorPanel::hue_style`]）、透明度滑块的外壳（[`ColorPanel::alpha_style`]）、这两个滑块的渐变和圆钮
/// （[`ColorPanel::slider_track_style`]、[`ColorPanel::slider_thumb_style`]）、输入框（[`ColorPanel::value_field_style`]）、编码类型
/// （[`ColorPanel::format_style`]）、预览（[`ColorPanel::preview_style`]）、常用颜色（[`ColorPanel::swatch_style`]），色块的圆角
/// （[`ColorPanel::tile_radius`]）。默认没有动画。样式对象换不了的用 [`ColorPanel::view`] 完全重做界面，逻辑和事件留着，见 [`ColorState`]。
///
/// 色板：按下就跳到按下的位置，拖动跟手；键盘 ← → 改饱和度，↑ ↓ 改亮度，Page Up / Page Down 亮度十步，Home / End 饱和度到两头。
/// 对辅助技术是滑块，念饱和度和亮度。输入框回车或失去焦点时读，见 [`ColorFormat::parse`]，读不懂时恢复。
///
/// 值用 8 位的 sRGB。拖到纯黑、纯灰时色相留着，再拖回来颜色还在。
pub struct ColorPanel {
    value: Signal<Color>,
    options: PanelOptions,
    changes: Changes<Color>,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

element_methods!([] ColorPanel);

impl ColorPanel {
    /// 绑定到 `value` 的颜色面板
    pub fn new(value: Signal<Color>) -> Self {
        Self { value, options: PanelOptions::default(), changes: Changes::default(), after: Default::default() }
    }

    panel_options!();

    /// 改整个面板的样子（默认宽 264，里面的东西跟着这个宽度）
    pub fn style(mut self, style: &Style) -> Self {
        self.options.panel = self.options.panel.extend(style);
        self
    }

    /// 用户改了值（拖动中每一次移动、按键、输入、点常用颜色），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<Color, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：拖动后松开时（值变了才调用），按键每走一步之后，输入回车或失去焦点时，点常用颜色时
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<Color, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    fn construct(self) -> Element {
        let ColorPanel { value, options, mut changes, after: _ } = self;
        let field = changes.report(|c: &Color| FieldValue::Text(ColorFormat::Hex.write(*c)));
        let changes = Rc::new(changes);
        let (for_change, for_commit) = (changes.clone(), changes);
        let options = Rc::new(options);
        let state = ColorState::new(&options, value, Rc::new(move |color, source| for_change.changed(color, source)), Rc::new(move |color, source| for_commit.committed(color, source)));
        color_panel(&options, state, false).field_handle(field).on_field_change_capture(|e: &FieldChangeEvent| {
            if e.phase() == PropagationPhase::Capture {
                e.stop_propagation();
            }
        })
    }
}

impl View for ColorPanel {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 颜色面板，[`ColorPanel::new`] 的简写
pub fn color_panel_view(value: Signal<Color>) -> ColorPanel {
    ColorPanel::new(value)
}

/// 颜色选择的构建器；[`color_picker`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let color = signal(Color::hex(0x2563eb));
/// color_picker(color);
/// ColorPicker::new(color)
///     .format(ColorFormat::Rgb)
///     .swatches([Color::hex(0xef4444), Color::hex(0xf59e0b), Color::hex(0x22c55e), Color::hex(0x2563eb)])
///     .on_commit(|c: Color| println!("定在 {c:?}"));
/// ```
///
/// 部位：外框（`.style`）、输入框（[`ColorPicker::field_style`]）、右边的色块（[`ColorPicker::button_style`]）、弹出的面板
/// （[`ColorPicker::panel_style`]），面板里面的部位和 [`ColorPanel`] 一样。外框的状态：焦点、打开、禁用。
///
/// 输入框按现在的编码类型显示，回车或失去焦点时读（见 [`ColorFormat::parse`]），读不懂时恢复。编码类型在弹出的面板里换。
/// 键盘：↓ 打开面板，焦点进色板；Esc 恢复打开时的颜色并关上，在外面点一下留下选的颜色。
/// 打开面板时拖动、按键值马上变（调用 [`ColorPicker::on_change`]），关上时算改完（值变了才调用 [`ColorPicker::on_commit`]）；
/// 外框的输入框打的字回车、失去焦点时两个都调用。代码里改绑定的信号时都不调用。
pub struct ColorPicker {
    value: Signal<Color>,
    options: PanelOptions,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<Color>,
    style: Style,
    input: Style,
    button: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

element_methods!([] ColorPicker);

impl ColorPicker {
    /// 绑定到 `value` 的颜色选择
    pub fn new(value: Signal<Color>) -> Self {
        Self { value, options: PanelOptions::default(), disabled: None, focus: None, changes: Changes::default(), style: Style::new(), input: Style::new(), button: Style::new(), after: Default::default() }
    }

    panel_options!();

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

    /// 用户改了值，参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<Color, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：面板关上时（值变了才调用），外框的输入框回车、失去焦点时
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<Color, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 改外框的样子：焦点状态是输入框有焦点，打开状态是面板弹出着
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改外框里输入框的样子（默认宽 136）
    pub fn field_style(mut self, style: &Style) -> Self {
        self.input = self.input.extend(style);
        self
    }

    /// 改弹出按钮的样子
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
        let ColorPicker { value, options, disabled, focus, mut changes, style, input, button, after: _ } = self;
        let report = changes.report(|c: &Color| FieldValue::Text(ColorFormat::Hex.write(*c)));
        let disabled = Flag::new(disabled);
        let focus = focus.unwrap_or_default();
        let open = signal(false);
        let changes = Rc::new(changes);
        let icon_name = options.texts.open.clone();
        let tile_radius = options.tile_radius;
        let options = Rc::new(PanelOptions { panel: Style::new().shadow(OVERLAY_SHADOW).extend(&options.panel), ..options });
        // 状态放在外面：编码类型、色相在几次打开之间留着，外框的输入框也用它
        let state = {
            let (for_change, for_commit) = (changes.clone(), changes.clone());
            ColorState::new(
                &options,
                value,
                Rc::new(move |color, source| for_change.changed(color, source)),
                // 面板开着时不算改完，关上时才算
                Rc::new(move |color, source| {
                    if !open.get_untracked() {
                        for_commit.committed(color, source);
                    }
                }),
            )
        };
        let draft = signal(untracked(|| state.text()));
        Effect::new(move || {
            let text = state.text();
            if untracked(|| draft.get()) != text {
                draft.set(text);
            }
        });
        let commit: Rc<dyn Fn()> = Rc::new(move || {
            if !state.set_text(&draft.get_untracked(), ChangeSource::Keyboard) || draft.get_untracked().trim() != untracked(|| state.text()) {
                draft.set(untracked(|| state.text()));
            }
        });
        // 打开时的值：Esc 恢复，关上时比较有没有变
        let opened_with = signal(None::<Color>);
        let popup: Rc<dyn Fn() -> Element> = Rc::new(move || {
            opened_with.set(Some(value.get_untracked()));
            let panel = color_panel(&options, state, true);
            if options.view.is_some() { panel } else { panel.name("color-picker-panel") }
        });
        let finish = move |restore: bool| {
            if let Some(before) = opened_with.get_untracked() {
                if restore {
                    if value.get_untracked() != before {
                        value.set(before);
                        changes.changed(before, ChangeSource::Keyboard);
                    }
                } else if value.get_untracked() != before {
                    changes.committed(value.get_untracked(), ChangeSource::Pointer);
                }
            }
            opened_with.set(None);
            open.set(false);
        };
        let swatch = row().size(16, 16).child(color_tile(move || value.get(), tile_radius));
        field_with_popup(Shell {
            style,
            input: Style::new().width(136).extend(&input),
            button,
            disabled,
            focus,
            open,
            draft,
            placeholder: "#RRGGBB".into(),
            commit,
            icon: swatch,
            icon_name,
            report,
            popup,
            on_dismiss: Rc::new(move |reason| {
                finish(reason == Dismiss::Escape);
                if reason != Dismiss::Outside {
                    focus.focus();
                }
            }),
        })
    }
}

impl View for ColorPicker {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 颜色选择，[`ColorPicker::new`] 的简写
pub fn color_picker(value: Signal<Color>) -> ColorPicker {
    ColorPicker::new(value)
}
