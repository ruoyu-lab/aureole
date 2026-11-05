//! 窗口的颜色输出：显示器能不能显示广色域、HDR，窗口用 8 位 sRGB 还是扩展范围的表面。
//!
//! 工作空间一直是线性的 sRGB 原色，sRGB 的内容在两种表面上一样。扩展范围的表面（半精度浮点）
//! 带宽是 8 位的两倍，所以默认（[`ColorOutput::Auto`]）只在画面里有扩展范围的内容（[`aureole_core::WideColor`]、
//! 广色域或 HDR 的图片）、显示器又支持时才换过去，内容没了 [`LINGER`] 秒后换回来。
//! 主线程每隔 [`POLL_INTERVAL`] 秒问一次显示器（窗口挪到别的显示器时马上问；帧时钟因为画面静止停下时，
//! 用着扩展范围的表面的窗口照样隔这么久问一次），决定好以后告诉渲染线程。

/// 窗口表面的颜色输出，见 [`crate::Window::color_output`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorOutput {
    /// 默认：画面里有广色域、HDR 的内容，显示器又支持时用扩展范围的表面，否则用 8 位 sRGB
    #[default]
    Auto,
    /// 一直用 8 位 sRGB：广色域的颜色截到 sRGB，HDR 压到 SDR 白
    Srgb,
    /// 只要表面支持，一直用扩展范围的表面（测性能、对比时用）
    Extended,
}

/// 窗口所在显示器的颜色能力和窗口此刻的输出方式，见 [`crate::WindowHandle::display_color`]、[`display_color`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayColor {
    /// 能显示 Display P3 色域
    pub wide_gamut: bool,
    /// 此刻比 SDR 白还能亮几倍（EDR 的余量，1 表示没有）：随亮度、环境光、画面里有没有 HDR 内容变化
    pub headroom: f32,
    /// 最多能有多少余量（显示器的上限，1 表示不支持 HDR）
    pub potential_headroom: f32,
    /// 窗口此刻用扩展范围的表面输出：比 SDR 白亮的颜色、HDR 图片按 `headroom` 做色调映射；
    /// 用 8 位 sRGB 时按 1（普通显示器）
    pub extended: bool,
}

impl DisplayColor {
    /// 普通显示器、8 位 sRGB 输出（测试工具、离屏截图里就是这样）
    pub const SDR: Self = Self { wide_gamut: false, headroom: 1.0, potential_headroom: 1.0, extended: false };

    pub(crate) fn from_platform(display: platform::DisplayColor, extended: bool) -> Self {
        let platform::DisplayColor { wide_gamut, headroom, potential_headroom } = display;
        Self { wide_gamut, headroom, potential_headroom, extended }
    }
}

impl Default for DisplayColor {
    fn default() -> Self {
        Self::SDR
    }
}

/// 当前窗口（正在构建视图、处理事件或推进帧的窗口）的 [`DisplayColor`]，响应式；
/// 不在窗口里（测试工具）时是 [`DisplayColor::SDR`]
pub fn display_color() -> DisplayColor {
    crate::current_window().map_or(DisplayColor::SDR, |w| w.display_color())
}

/// 多久问一次显示器，秒（亮度、环境光变了余量就变，系统不发通知）
pub(crate) const POLL_INTERVAL: f64 = 0.5;
/// 扩展范围的内容没了以后，再过这么多秒才换回 8 位的表面（滚动、切换时不来回换）
pub(crate) const LINGER: f64 = 2.0;

/// 要不要用扩展范围的表面：表面支持（`supported`），并且按 `mode`：
/// 自动时显示器值得用（广色域或能比 SDR 白亮）且最近有扩展范围的内容（`content`）
pub(crate) fn wants_extended(mode: ColorOutput, supported: bool, display: platform::DisplayColor, content: bool) -> bool {
    supported
        && match mode {
            ColorOutput::Srgb => false,
            ColorOutput::Extended => true,
            ColorOutput::Auto => content && display.is_capable(),
        }
}

/// 一个窗口的输出状态（主线程）：多久问一次显示器、最近有没有扩展范围的内容、上次告诉渲染线程的是什么
pub(crate) struct OutputState {
    pub mode: ColorOutput,
    /// 表面支持扩展范围的格式
    pub supported: bool,
    pub display: platform::DisplayColor,
    /// 上次问显示器的时间（帧时钟，秒）；`None` 表示下一帧马上问
    polled: Option<f64>,
    /// 画面里现在有扩展范围的内容
    content: bool,
    /// 最后一次有扩展范围的内容的时间
    content_at: Option<f64>,
    /// 上次的决定：（扩展范围，余量）
    decided: Option<(bool, f32)>,
}

impl OutputState {
    pub fn new(mode: ColorOutput, supported: bool, display: platform::DisplayColor) -> Self {
        Self { mode, supported, display, polled: None, content: false, content_at: None, decided: None }
    }

    /// 下一帧马上重新问显示器（窗口挪动了，可能到了另一个显示器上）
    pub fn repoll(&mut self) {
        self.polled = None;
    }

    /// 该问显示器了
    pub fn due(&self, now: f64) -> bool {
        self.polled.is_none_or(|t| now - t >= POLL_INTERVAL || now < t)
    }

    /// 离下次该问显示器还有多少秒
    pub fn until_due(&self, now: f64) -> f64 {
        self.polled.map_or(0.0, |t| (t + POLL_INTERVAL - now).clamp(0.0, POLL_INTERVAL))
    }

    /// 告诉渲染线程的是扩展范围的表面
    pub fn is_extended(&self) -> bool {
        self.decided.is_some_and(|d| d.0)
    }

    /// 问到了显示器（问不到时按普通显示器）
    pub fn polled(&mut self, now: f64, display: Option<platform::DisplayColor>) {
        self.polled = Some(now);
        self.display = display.unwrap_or(platform::DisplayColor::SDR);
    }

    /// 推进到时间 `now`。`content`：这一帧重画了时，画面里有没有扩展范围的内容（没重画时为 `None`）。
    /// 决定变了（第一次也算）时返回新的（扩展范围，余量）
    pub fn update(&mut self, now: f64, content: Option<bool>) -> Option<(bool, f32)> {
        if let Some(content) = content {
            self.content = content;
        }
        if self.content {
            self.content_at = Some(now);
        }
        let recent = self.content_at.is_some_and(|t| now - t < LINGER);
        let extended = wants_extended(self.mode, self.supported, self.display, recent);
        // 余量按 1% 取整：亮度慢慢变时不每次都重画
        let headroom = if extended { (self.display.headroom * 100.0).round() / 100.0 } else { 1.0 };
        let decided = (extended, headroom.max(1.0));
        (self.decided != Some(decided)).then(|| {
            self.decided = Some(decided);
            decided
        })
    }
}
