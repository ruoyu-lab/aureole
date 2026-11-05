use bytemuck::{Pod, Zeroable};

use crate::{Angle, turns};

/// sRGB 颜色，直通 alpha（未预乘），每通道 8 位。广色域（Display P3、OKLCh）和比 SDR 白亮的颜色用 [`WideColor`]
/// （[`Color::p3`]、[`Color::oklch`]、[`Color::hdr`] 返回它）。
///
/// 数值的约定：**按 8 位值构造用 0..=255 的整数**（[`Color::rgb`]、[`Color::hex`]，和设计稿、取色器里的写法一样）；
/// **透明度和别的分量一律是 0..=1 的比例**（[`Color::rgba`] 的透明度、[`Color::with_alpha`]、[`Color::hsv`] 的饱和度和明度）。
/// 半透明的白色写 `Color::rgba(255, 255, 255, 0.5)` 或 `Color::WHITE.with_alpha(0.5)`。
///
/// CPU 端保持紧凑；着色器里再转到线性空间并预乘，混合在线性空间进行。
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Color(pub [u8; 4]);

impl Color {
    /// 全透明
    pub const TRANSPARENT: Self = Self([0, 0, 0, 0]);
    /// 白色
    pub const WHITE: Self = Self([255, 255, 255, 255]);
    /// 黑色
    pub const BLACK: Self = Self([0, 0, 0, 255]);

    /// 不透明颜色，各通道 0..=255
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self([r, g, b, 255])
    }

    /// 带透明度的颜色：各通道 0..=255，透明度 `a` 是比例，在 0..=1（未预乘，超出范围截断）
    pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self([r, g, b, unorm8(a)])
    }

    /// `0xRRGGBB`
    pub const fn hex(rgb: u32) -> Self {
        Self([(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255])
    }

    /// 色相 `h`（[`Angle`]，`turns(0.5)` 是青色，超过一圈自动取模）、饱和度 `s`、明度 `v`（都在 0..=1）
    pub fn hsv(h: Angle, s: f32, v: f32) -> Self {
        let h = h.turns().rem_euclid(1.0) * 6.0;
        let i = h.floor();
        let f = h - i;
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        let (r, g, b) = match i as u32 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        Self([unorm8(r), unorm8(g), unorm8(b), 255])
    }

    /// 换成色相（[`Angle`]，`0..1` 圈）、饱和度（0..=1）、明度（0..=1），不管透明度。灰色的色相、黑色的饱和度算 0
    pub fn to_hsv(self) -> (Angle, f32, f32) {
        let [r, g, b, _] = self.0.map(|c| c as f32 / 255.0);
        let max = r.max(g).max(b);
        let chroma = max - r.min(g).min(b);
        let saturation = if max > 0.0 { chroma / max } else { 0.0 };
        let hue = if chroma == 0.0 {
            0.0
        } else if max == r {
            ((g - b) / chroma).rem_euclid(6.0)
        } else if max == g {
            (b - r) / chroma + 2.0
        } else {
            (r - g) / chroma + 4.0
        };
        (turns(hue / 6.0), saturation, max)
    }

    /// 色相 `h`、饱和度 `s`（0..=1）、亮度 `l`（0..=1）。和 [`Color::hsv`] 的差别：亮度 1 总是白色
    pub fn hsl(h: Angle, s: f32, l: f32) -> Self {
        // 换成 HSV 算：两种写法只差饱和度和明度的定义
        let value = l + s * l.min(1.0 - l);
        let saturation = if value > 0.0 { 2.0 * (1.0 - l / value) } else { 0.0 };
        Self::hsv(h, saturation, value)
    }

    /// 换成色相、饱和度（0..=1）、亮度（0..=1），不管透明度。灰色的色相、饱和度算 0
    pub fn to_hsl(self) -> (Angle, f32, f32) {
        let (hue, saturation, value) = self.to_hsv();
        let lightness = value * (1.0 - saturation / 2.0);
        let divisor = lightness.min(1.0 - lightness);
        (hue, if divisor > 0.0 { (value - lightness) / divisor } else { 0.0 }, lightness)
    }

    /// 读十六进制的写法：`#RGB`、`#RGBA`、`#RRGGBB`、`#RRGGBBAA`，`#` 可以不写，大小写都行。读不懂时是 `None`
    pub fn parse_hex(text: &str) -> Option<Self> {
        let digits = text.trim();
        let digits = digits.strip_prefix('#').unwrap_or(digits);
        if !digits.is_ascii() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let pair = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        let single = |i: usize| u8::from_str_radix(&digits[i..i + 1], 16).ok().map(|n| n * 17);
        match digits.len() {
            3 => Some(Self([single(0)?, single(1)?, single(2)?, 255])),
            4 => Some(Self([single(0)?, single(1)?, single(2)?, single(3)?])),
            6 => Some(Self([pair(0)?, pair(2)?, pair(4)?, 255])),
            8 => Some(Self([pair(0)?, pair(2)?, pair(4)?, pair(6)?])),
            _ => None,
        }
    }

    /// 写成十六进制：不透明时 `#rrggbb`，否则 `#rrggbbaa`
    pub fn to_hex_string(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
    }

    /// 替换透明度，`a` 是比例，在 0..=1，超出范围截断
    pub fn with_alpha(mut self, a: f32) -> Self {
        self.0[3] = unorm8(a);
        self
    }

    /// alpha 为 0
    pub fn is_transparent(self) -> bool {
        self.0[3] == 0
    }

    /// 线性空间 RGBA（未预乘），用于清屏色等需要线性值的地方
    pub fn to_linear(self) -> [f32; 4] {
        let [r, g, b, a] = self.0;
        [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), a as f32 / 255.0]
    }

    /// OKLab（L、a、b、alpha，未预乘）。OKLab 是感知均匀的色彩空间，
    /// 在里面插值，颜色过渡的亮度和色相变化看起来是匀速的
    pub fn to_oklab(self) -> [f32; 4] {
        let [r, g, b, alpha] = self.to_linear();
        let [l, a, b] = linear_to_oklab([r, g, b]);
        [l, a, b, alpha]
    }

    /// 从 OKLab 转回，超出 sRGB 色域的分量截断
    pub fn from_oklab([l, a, b, alpha]: [f32; 4]) -> Self {
        let [r, g, b] = oklab_to_linear([l, a, b]);
        Self([linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(b), unorm8(alpha)])
    }
}

const fn unorm8(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn linear_to_srgb(c: f32) -> u8 {
    unorm8(encode_srgb(c.clamp(0.0, 1.0)))
}

fn srgb_to_linear(c: u8) -> f32 {
    decode_srgb(c as f32 / 255.0)
}

/// sRGB 编码值 -> 线性光。0..=1 之外按原点对称延伸（扩展范围：负数是 sRGB 色域外的分量，大于 1 比 SDR 白亮），
/// 和系统的扩展 sRGB 一样
pub fn decode_srgb(c: f32) -> f32 {
    let a = c.abs();
    let l = if a <= 0.04045 { a / 12.92 } else { ((a + 0.055) / 1.055).powf(2.4) };
    l.copysign(c)
}

/// 线性光 -> sRGB 编码值，0..=1 之外同样对称延伸，见 [`decode_srgb`]
pub fn encode_srgb(c: f32) -> f32 {
    let a = c.abs();
    let e = if a <= 0.003_130_8 { a * 12.92 } else { 1.055 * a.powf(1.0 / 2.4) - 0.055 };
    e.copysign(c)
}

/// 3x3 矩阵（行主序）乘列向量
pub fn multiply_matrix3(m: &[[f32; 3]; 3], [x, y, z]: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * x + row[1] * y + row[2] * z)
}

/// 线性 Display P3 -> 线性 sRGB（同一个 D65 白点，原色不同）。P3 里比 sRGB 饱和的颜色得到负的分量
pub const P3_TO_SRGB: [[f32; 3]; 3] = [
    [1.224_940_2, -0.224_940_18, 0.0],
    [-0.042_056_955, 1.042_057, 0.0],
    [-0.019_637_555, -0.078_636_05, 1.098_273_6],
];

/// 线性 sRGB -> 线性 Display P3，[`P3_TO_SRGB`] 的逆
pub const SRGB_TO_P3: [[f32; 3]; 3] = [
    [0.822_461_97, 0.177_538_02, 0.0],
    [0.033_194_2, 0.966_805_8, 0.0],
    [0.017_082_632, 0.072_397_44, 0.910_519_9],
];

/// 线性 BT.2020（HDR 视频和图片常用的原色）-> 线性 sRGB
pub const BT2020_TO_SRGB: [[f32; 3]; 3] = [
    [1.660_491, -0.587_641_1, -0.072_849_9],
    [-0.124_550_47, 1.132_899_9, -0.008_349_42],
    [-0.018_150_763, -0.100_578_9, 1.118_729_7],
];

/// 线性 sRGB -> OKLab（L、a、b）。分量超出 0..=1（扩展范围）也成立
pub(crate) fn linear_to_oklab([r, g, b]: [f32; 3]) -> [f32; 3] {
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// OKLab -> 线性 sRGB，不截断
pub(crate) fn oklab_to_linear([l, a, b]: [f32; 3]) -> [f32; 3] {
    let l_ = (l + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m_ = (l - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s_ = (l - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    [
        4.076_741_7 * l_ - 3.307_711_6 * m_ + 0.230_969_94 * s_,
        -1.268_438 * l_ + 2.609_757_4 * m_ - 0.341_319_38 * s_,
        -0.004_196_086_3 * l_ - 0.703_418_6 * m_ + 1.707_614_7 * s_,
    ]
}

/// 颜色有多亮：换到 Display P3 的原色上最大的分量。P3 色域里的颜色（包括比 sRGB 饱和的 P3 红、绿）不超过 1，
/// 超过 1 才是比 SDR 白亮的 HDR 颜色。扩展范围的输出按它和显示器的余量比较（广色域显示器都是 P3 的），
/// 不按 sRGB 原色上的分量（P3 的红在 sRGB 原色上是 1.22，并不比白亮）
pub fn display_peak(rgb: [f32; 3]) -> f32 {
    let [r, g, b] = multiply_matrix3(&SRGB_TO_P3, rgb);
    r.max(g).max(b)
}

/// 色调映射的膝点：显示器余量的这么多以下原样不动
const TONE_KNEE: f32 = 0.75;

/// HDR 内容的色调映射：把最亮为 `peak` 的内容放进显示器此刻的余量 `headroom`（都是 SDR 白的倍数，
/// 普通显示器的余量是 1）。`value` 是一个像素有多亮（[`display_peak`]），返回它映射后的值，
/// 三个分量按同一比例缩放（色相不变）。
///
/// `peak` 不超过 `headroom` 时原样不动；否则膝点（`headroom` 的 0.75）以下不变，以上用有理曲线
/// `k + x / (1 + c x)`（`x` 是超出膝点的部分）平滑地压缩：膝点处斜率连续，`peak` 正好落在 `headroom` 上，
/// 余量变大时曲线连续地退回原样。着色器里的同名函数和这里一致
pub fn tone_map(value: f32, peak: f32, headroom: f32) -> f32 {
    let headroom = headroom.max(1e-3);
    if peak <= headroom {
        return value;
    }
    let knee = TONE_KNEE * headroom;
    if value <= knee {
        return value;
    }
    let c = 1.0 / (headroom - knee) - 1.0 / (peak - knee);
    let x = value - knee;
    (knee + x / (1.0 + c * x)).min(headroom)
}

/// 扩展范围的颜色：线性光，sRGB 的原色和白点，分量不限于 0..=1；alpha 未预乘。
///
/// - 分量小于 0：sRGB 色域外的颜色。Display P3 里比 sRGB 更饱和的红、绿，换到 sRGB 的原色上就有负的分量
/// - 分量大于 1：比 SDR 白（界面里的白色）更亮，只有 HDR（EDR）显示器能显示
///
/// 用 [`Color::p3`]、[`Color::p3_hex`]、[`Color::oklch`]、[`Color::hdr`]、[`Color::brightness`] 构造；
/// 背景、文字和路径的颜色、渐变的色标都接受它。在广色域、HDR 显示器上原样输出（窗口换成扩展范围的表面）；
/// 在普通显示器和 8 位 sRGB 的截图里，色域外的分量截断，比显示器能显示的更亮的颜色按最大的分量
/// 等比压下来（色相不变）。
///
/// ```
/// use aureole_scene::{Color, WideColor};
/// let teal = Color::p3(0.0, 0.72, 0.62);      // Display P3 里的青绿，比 sRGB 能显示的更饱和
/// assert!(!teal.is_in_srgb_gamut());
/// let glow = Color::hex(0xffd27a).brightness(3.0); // 三倍于 SDR 白的暖光
/// assert!(glow.is_hdr());
/// assert_eq!(WideColor::from(Color::hex(0x336699)).to_srgb(), Color::hex(0x336699));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WideColor {
    /// 线性 RGB（sRGB 的原色），不限于 0..=1
    pub rgb: [f32; 3],
    /// 透明度 0..=1
    pub alpha: f32,
}

impl WideColor {
    /// 线性 sRGB 原色上的分量（不截断）
    pub const fn linear(r: f32, g: f32, b: f32) -> Self {
        Self { rgb: [r, g, b], alpha: 1.0 }
    }

    /// Display P3 的编码值（和 sRGB 一样的传递曲线），各分量 0..=1
    pub fn p3(r: f32, g: f32, b: f32) -> Self {
        Self { rgb: multiply_matrix3(&P3_TO_SRGB, [r, g, b].map(decode_srgb)), alpha: 1.0 }
    }

    /// Display P3 的 `0xRRGGBB`（取色器在 Display P3 下读到的值）
    pub fn p3_hex(rgb: u32) -> Self {
        let c = |shift: u32| ((rgb >> shift) & 0xff) as f32 / 255.0;
        Self::p3(c(16), c(8), c(0))
    }

    /// OKLCh：明度 `l`（0..=1）、彩度 `c`（0 是灰，常用到 0.3 左右）、色相 `h`（[`Angle`]，比如 `deg(250)`）。
    /// 明度和色相感知均匀，换色相时亮度看起来不变；彩度大的颜色可能在 sRGB 色域外（在广色域显示器上才显示得出来）
    pub fn oklch(l: f32, c: f32, h: Angle) -> Self {
        // 先回到度再换弧度：和以前收度的时候算出来的一模一样
        let (sin, cos) = h.degrees().to_radians().sin_cos();
        Self { rgb: oklab_to_linear([l, c * cos, c * sin]), alpha: 1.0 }
    }

    /// 亮度乘以 `times`：大于 1 就是比 SDR 白更亮的 HDR 颜色
    pub fn brightness(mut self, times: f32) -> Self {
        self.rgb = self.rgb.map(|c| c * times.max(0.0));
        self
    }

    /// 替换透明度，0..=1
    pub fn with_alpha(mut self, a: f32) -> Self {
        self.alpha = a.clamp(0.0, 1.0);
        self
    }

    /// 在 sRGB 色域里、不比 SDR 白亮（8 位 sRGB 能准确表示）
    pub fn is_in_srgb_gamut(self) -> bool {
        const EPS: f32 = 1e-4;
        self.rgb.iter().all(|&c| (-EPS..=1.0 + EPS).contains(&c))
    }

    /// 有多亮（[`display_peak`]，按 Display P3 的原色）：比 1 大多少就是比 SDR 白亮多少倍
    pub fn peak(self) -> f32 {
        display_peak(self.rgb)
    }

    /// 比 SDR 白亮（HDR 颜色）。P3 色域里的颜色都不算
    pub fn is_hdr(self) -> bool {
        self.peak() > 1.0 + 1e-4
    }

    /// 8 位 sRGB 颜色，和 8 位 sRGB 的目标（普通显示器、截图）上画出来的一样：负的分量截掉，
    /// 最大的分量超过 1 时三个分量等比压到 1（色相不变）
    pub fn to_srgb(self) -> Color {
        let peak = self.rgb.map(|c| c.max(0.0)).into_iter().fold(0.0, f32::max);
        let scale = if peak > 1.0 { 1.0 / peak } else { 1.0 };
        let [r, g, b] = self.rgb.map(|c| linear_to_srgb(c.max(0.0) * scale));
        Color([r, g, b, unorm8(self.alpha)])
    }

    /// Display P3 的编码值（和 sRGB 一样的传递曲线，0..=1 之外对称延伸）
    pub fn to_p3(self) -> [f32; 3] {
        multiply_matrix3(&SRGB_TO_P3, self.rgb).map(encode_srgb)
    }

    /// OKLab（L、a、b、alpha，未预乘），不截断
    pub fn to_oklab(self) -> [f32; 4] {
        let [l, a, b] = linear_to_oklab(self.rgb);
        [l, a, b, self.alpha]
    }
}

impl From<Color> for WideColor {
    fn from(color: Color) -> Self {
        let [r, g, b, a] = color.to_linear();
        Self { rgb: [r, g, b], alpha: a }
    }
}

impl Color {
    /// Display P3 的颜色（编码值，各分量 0..=1）：比 sRGB 更饱和的红、绿、青只有它写得出来，
    /// 在广色域显示器上显示，普通显示器上截断到 sRGB。见 [`WideColor`]
    pub fn p3(r: f32, g: f32, b: f32) -> WideColor {
        WideColor::p3(r, g, b)
    }

    /// Display P3 的 `0xRRGGBB`，见 [`Color::p3`]
    pub fn p3_hex(rgb: u32) -> WideColor {
        WideColor::p3_hex(rgb)
    }

    /// OKLCh 写的颜色：明度 0..=1、彩度、色相（[`Angle`]），见 [`WideColor::oklch`]
    pub fn oklch(l: f32, c: f32, h: Angle) -> WideColor {
        WideColor::oklch(l, c, h)
    }

    /// HDR 颜色：`r`、`g`、`b` 是 sRGB 编码值（0..=1），亮度是 SDR 白的 `brightness` 倍。
    /// 显示器的余量不够时按最大的分量等比压到余量（色相不变），普通显示器上就是同色相最亮的 SDR 颜色
    pub fn hdr(r: f32, g: f32, b: f32, brightness: f32) -> WideColor {
        WideColor { rgb: [r, g, b].map(decode_srgb), alpha: 1.0 }.brightness(brightness)
    }

    /// 同样的颜色，亮度乘以 `times`（大于 1 就是 HDR 颜色），见 [`Color::hdr`]
    pub fn brightness(self, times: f32) -> WideColor {
        WideColor::from(self).brightness(times)
    }
}
