//! 渐变：线性、径向、锥形，用作背景（[`crate::Element::background`]）或文字、路径的颜色
//! （[`crate::Element::color`]）。
//!
//! 色标默认在预乘的 OKLab 里插值（和颜色动画一样），互补色之间不会发灰。渐变在元素自己的
//! 坐标里求值，跟着元素的变换和合成层动画一起平移、缩放、旋转。
//!
//! 动画：背景色、文字颜色在渐变之间（或纯色和渐变之间）过渡时，按进度混合两端，见 [`FillMix`]。
//!
//! 广色域、HDR 的颜色（[`WideColor`]）也走渐变的编码：纯色就是只有一个色标的渐变，
//! 色标的值不截断，着色器按目标能显示的范围放进去。

use std::sync::Arc;

use scene::{Angle, Color, ColorSpace, DisplayList, GradientPaint, GradientShape, WideColor};

use crate::geometry::Affine;
use crate::geometry::Rect;

/// 渐变。几何参数都相对元素自己的框，所以同一个渐变可以用在不同大小的元素上；
/// 克隆只是增加引用计数。
///
/// ```
/// # use aureole::prelude::*;
/// let sunset = Gradient::linear(deg(135), [(0.0, Color::hex(0xff5f6d)), (1.0, Color::hex(0xffc371))]);
/// let glow = Gradient::radial([(0.0, Color::hex(0x64d2ff)), (1.0, Color::hex(0x0a84ff).with_alpha(0.0))]).center(0.3, 0.2);
/// let ring = Gradient::conic(deg(0), [(0.0, Color::hex(0xff375f)), (0.5, Color::hex(0x5e5ce6)), (1.0, Color::hex(0xff375f))]);
/// rect().size(240, 140).radius(20).background(sunset);
/// ```
#[derive(Clone, Debug)]
pub struct Gradient(Arc<Def>);

#[derive(Clone, Debug, PartialEq)]
struct Def {
    shape: GradientShape,
    /// 线性渐变的方向、锥形渐变的起始角：度，0 指向正上方，顺时针
    angle: f32,
    /// 径向、锥形渐变的中心：在元素框里的比例
    center: [f32; 2],
    /// 径向渐变的半径：中心到最远角距离的倍数
    radius: f32,
    color_space: ColorSpace,
    /// 位置单调不减；颜色是 `color_space` 里的预乘值
    stops: Vec<(f32, [f32; 4])>,
    /// 有扩展范围的色标（sRGB 色域外、比 SDR 白亮）
    wide: bool,
}

impl PartialEq for Gradient {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl Gradient {
    fn new<C: Into<StopColor>>(shape: GradientShape, angle: f32, stops: impl IntoIterator<Item = (f32, C)>) -> Self {
        let color_space = ColorSpace::default();
        let mut max = f32::NEG_INFINITY;
        let mut wide = false;
        let stops = stops
            .into_iter()
            .map(|(position, color)| {
                // 位置比前面的小时取前面的最大值，保证单调
                max = max.max(position);
                let color = match color.into() {
                    StopColor::Srgb(color) => color_space.encode(color),
                    StopColor::Wide(color) => {
                        wide |= !color.is_in_srgb_gamut();
                        color_space.encode_wide(color)
                    }
                };
                (max, color)
            })
            .collect();
        Self(Arc::new(Def { shape, angle, center: [0.5, 0.5], radius: 1.0, color_space, stops, wide }))
    }

    /// 线性渐变。`angle` 是方向：`deg(0)` 从下到上，`deg(90)` 从左到右，`deg(180)` 从上到下。
    /// 色标是 (位置, 颜色)，位置 0..1 沿渐变线；渐变线的长度让四个角正好落在 0 和 1 上。
    /// 颜色可以是 [`Color`]，也可以是广色域、HDR 的 [`WideColor`]（同一个渐变里写成同一种）
    pub fn linear<C: Into<StopColor>>(angle: Angle, stops: impl IntoIterator<Item = (f32, C)>) -> Self {
        Self::new(GradientShape::Linear, angle.degrees(), stops)
    }

    /// 径向渐变：圆形，默认中心在元素中间，半径到最远的角
    pub fn radial<C: Into<StopColor>>(stops: impl IntoIterator<Item = (f32, C)>) -> Self {
        Self::new(GradientShape::Radial, 0.0, stops)
    }

    /// 锥形渐变：绕中心转一圈，从 `angle`（`deg(0)` 是正上方）开始顺时针
    pub fn conic<C: Into<StopColor>>(angle: Angle, stops: impl IntoIterator<Item = (f32, C)>) -> Self {
        Self::new(GradientShape::Conic, angle.degrees(), stops)
    }

    /// 一种颜色铺满（只有一个色标）：广色域、HDR 的纯色就这样画（见 [`Fill`] 的 `From<WideColor>`）
    pub(crate) fn solid(color: WideColor) -> Self {
        Self::new(GradientShape::Linear, 90.0, [(0.0, color)])
    }

    fn def_mut(&mut self) -> &mut Def {
        Arc::make_mut(&mut self.0)
    }

    /// 径向、锥形渐变的中心，元素框里的比例（0.5, 0.5 是正中）
    pub fn center(mut self, x: f32, y: f32) -> Self {
        self.def_mut().center = [x, y];
        self
    }

    /// 径向渐变的半径：中心到最远角距离的倍数，默认 1
    pub fn radius(mut self, radius: f32) -> Self {
        self.def_mut().radius = radius;
        self
    }

    /// 线性渐变的方向、锥形渐变的起始角
    pub fn angle(mut self, angle: Angle) -> Self {
        self.def_mut().angle = angle.degrees();
        self
    }

    /// 色标之间在哪个色彩空间插值，默认 [`ColorSpace::Oklab`]
    pub fn color_space(mut self, color_space: ColorSpace) -> Self {
        let def = self.def_mut();
        let from = def.color_space;
        for stop in &mut def.stops {
            stop.1 = from.convert(stop.1, color_space);
        }
        def.color_space = color_space;
        self
    }

    /// 有不透明的色标
    pub(crate) fn is_visible(&self) -> bool {
        self.0.stops.iter().any(|(_, c)| c[3] > 0.0)
    }

    /// 位置 `x` 处的颜色（插值空间里的预乘值）。`right`：正好落在硬边（几个色标同一位置）上时取右侧的颜色
    fn sample(&self, x: f32, right: bool) -> [f32; 4] {
        let stops = &self.0.stops;
        let Some(first) = stops.first() else { return [0.0; 4] };
        let last = stops[stops.len() - 1];
        if x < first.0 || (x == first.0 && !right) {
            return first.1;
        }
        if x > last.0 || (x == last.0 && right) {
            return last.1;
        }
        // 第一个位置大于 x（取右侧时）或不小于 x（取左侧时）的色标
        let i = stops.partition_point(|s| if right { s.0 <= x } else { s.0 < x });
        let (a, b) = (stops[i - 1], stops[i]);
        if b.0 - a.0 <= 1e-9 {
            return if right { b.1 } else { a.1 };
        }
        lerp4(a.1, b.1, (x - a.0) / (b.0 - a.0))
    }

    /// 代表色（中点的颜色），给只能用一种颜色的地方
    fn representative(&self) -> Color {
        self.0.color_space.to_color(self.sample(0.5, false))
    }

    /// 相对元素中心的布局坐标 -> 渐变坐标，元素框大小 `size`
    fn local_affine(&self, [w, h]: [f32; 2]) -> Affine {
        let def = &*self.0;
        let center = [(def.center[0] - 0.5) * w, (def.center[1] - 0.5) * h];
        match def.shape {
            GradientShape::Linear => {
                // 渐变线过元素中心，长度让四个角正好落在两端
                let (sin, cos) = def.angle.to_radians().sin_cos();
                let length = ((w * sin).abs() + (h * cos).abs()).max(1e-6);
                Affine { linear: [sin / length, 0.0, -cos / length, 0.0], offset: [0.5, 0.0] }
            }
            GradientShape::Radial => {
                let corner = [w * 0.5 + center[0].abs(), h * 0.5 + center[1].abs()];
                let radius = (corner[0].hypot(corner[1]) * def.radius).max(1e-6);
                Affine { linear: [1.0 / radius, 0.0, 0.0, 1.0 / radius], offset: [-center[0] / radius, -center[1] / radius] }
            }
            GradientShape::Conic => {
                // 逆时针转回起始角，着色器从正上方顺时针量角度
                let (sin, cos) = def.angle.to_radians().sin_cos();
                let linear = [cos, -sin, sin, cos];
                let [x, y] = Affine { linear, offset: [0.0, 0.0] }.apply(center);
                Affine { linear, offset: [-x, -y] }
            }
        }
    }

    /// 登记进显示列表，返回给图元 `gradient` 字段用的下标。`bounds` 是元素的布局位置，
    /// `frame` 是布局坐标 -> 层坐标的累积变换；退化（缩放为 0）时返回 0（纯色）
    pub(crate) fn push(&self, list: &mut DisplayList, bounds: Rect, frame: &Affine) -> u32 {
        let Some(inverse) = frame.inverse() else { return 0 };
        let [cx, cy] = bounds.center();
        let to_element = Affine { linear: Affine::IDENTITY.linear, offset: [-cx, -cy] }.compose(&inverse);
        let to_gradient = self.local_affine([bounds.width, bounds.height]).compose(&to_element);
        list.push_gradient(&GradientPaint {
            shape: self.0.shape,
            linear: to_gradient.linear,
            offset: to_gradient.offset,
            color_space: self.0.color_space,
            stops: &self.0.stops,
            extended: self.0.wide,
        })
    }
}

impl Gradient {
    /// 登记成局部坐标里的渐变（渐变边框）：渐变坐标相对图元中心、不含图元的变换，`size` 是铺满的框的大小
    pub(crate) fn push_local(&self, list: &mut DisplayList, size: [f32; 2]) -> u32 {
        let to_gradient = self.local_affine(size);
        list.push_gradient(&GradientPaint {
            shape: self.0.shape,
            linear: to_gradient.linear,
            offset: to_gradient.offset,
            color_space: self.0.color_space,
            stops: &self.0.stops,
            extended: self.0.wide,
        })
    }
}

/// 渐变的一个色标的颜色：[`Color`] 或 [`WideColor`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StopColor {
    /// 8 位 sRGB 颜色
    Srgb(Color),
    /// 广色域、HDR 的颜色
    Wide(WideColor),
}

impl From<Color> for StopColor {
    fn from(color: Color) -> Self {
        Self::Srgb(color)
    }
}

impl From<WideColor> for StopColor {
    fn from(color: WideColor) -> Self {
        Self::Wide(color)
    }
}

/// 填充：纯色或渐变。[`crate::Element::background`] 和 [`crate::Element::color`] 都接受
/// `Color`、`WideColor`、`Gradient` 或它们的绑定；要在纯色和渐变之间切换时让闭包返回 `Fill`
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Fill {
    /// 纯色
    Color(Color),
    /// 渐变（广色域、HDR 的纯色也是：只有一个色标的渐变）
    Gradient(Gradient),
}

impl From<Color> for Fill {
    fn from(color: Color) -> Self {
        Self::Color(color)
    }
}

/// 广色域、HDR 的纯色：在 sRGB 色域里、不比白亮的就是普通的 [`Color`]；否则是只有一个色标的渐变
/// （色标不截断，着色器按目标能显示的范围放进去），和渐变一样能做动画
impl From<WideColor> for Fill {
    fn from(color: WideColor) -> Self {
        if color.is_in_srgb_gamut() {
            Self::Color(color.to_srgb())
        } else {
            Self::Gradient(Gradient::solid(color))
        }
    }
}

impl From<Gradient> for Fill {
    fn from(gradient: Gradient) -> Self {
        Self::Gradient(gradient)
    }
}

impl Fill {
    pub(crate) fn is_visible(&self) -> bool {
        match self {
            Self::Color(c) => !c.is_transparent(),
            Self::Gradient(g) => g.is_visible(),
        }
    }

    /// 纯色本身，或渐变的代表色
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Color(c) => *c,
            Self::Gradient(g) => g.representative(),
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| lerp(a[i], b[i], t))
}

/// 渐变参与的填充过渡：按进度 `t`（0..1，弹簧可能略超出）在两端之间混合。
///
/// - 纯色和渐变之间：纯色当作和对方几何、色标位置都相同、每个色标都是这个颜色的渐变，
///   所以纯色会"长出"渐变，渐变也会收成纯色
/// - 两个渐变色标数相同：逐个色标插值位置和颜色，色标会滑动
/// - 色标数不同：两边都在全部色标位置（并集）上重新取样再逐个插值。分段线性的渐变在并集上取样
///   是精确的，所以过渡的每一帧都正好是两端逐点的混合；硬边（同一位置的两个色标）保留
/// - 角度、中心、半径按数值插值（0° 到 360° 会转一整圈）；形状不同时在一半处切换；
///   插值空间不同时用目标的
pub(crate) struct FillMix {
    pub from: Fill,
    pub to: Fill,
}

impl FillMix {
    pub fn at(&self, t: f32) -> Fill {
        match (&self.from, &self.to) {
            (Fill::Color(a), Fill::Color(b)) => Fill::Color(ColorSpace::Oklab.to_color(lerp4(
                ColorSpace::Oklab.encode(*a),
                ColorSpace::Oklab.encode(*b),
                t,
            ))),
            (Fill::Color(c), Fill::Gradient(g)) => Fill::Gradient(fade(g, *c, 1.0 - t)),
            (Fill::Gradient(g), Fill::Color(c)) => Fill::Gradient(fade(g, *c, t)),
            (Fill::Gradient(a), Fill::Gradient(b)) => Fill::Gradient(mix_gradients(a, b, t)),
        }
    }
}

/// 渐变的每个色标往纯色 `color` 靠 `t`（`wide` 保持：纯色在 sRGB 色域里）
fn fade(gradient: &Gradient, color: Color, t: f32) -> Gradient {
    let mut def = (*gradient.0).clone();
    let target = def.color_space.encode(color);
    for stop in &mut def.stops {
        stop.1 = clamp_alpha(lerp4(stop.1, target, t));
    }
    Gradient(Arc::new(def))
}

/// 弹簧过冲时 alpha 可能出界
fn clamp_alpha(mut c: [f32; 4]) -> [f32; 4] {
    c[3] = c[3].clamp(0.0, 1.0);
    c
}

fn mix_gradients(a: &Gradient, b: &Gradient, t: f32) -> Gradient {
    let (da, db) = (&*a.0, &*b.0);
    let color_space = db.color_space;
    let a_stops: Vec<(f32, [f32; 4])> = da.stops.iter().map(|&(p, c)| (p, da.color_space.convert(c, color_space))).collect();
    let stops = if a_stops.len() == db.stops.len() {
        a_stops.iter().zip(&db.stops).map(|(x, y)| (lerp(x.0, y.0, t), clamp_alpha(lerp4(x.1, y.1, t)))).collect()
    } else {
        let a = Gradient(Arc::new(Def { stops: a_stops, color_space, ..da.clone() }));
        union_positions(&a, b)
            .into_iter()
            .map(|(p, right)| (p, clamp_alpha(lerp4(a.sample(p, right), b.sample(p, right), t))))
            .collect()
    };
    let mut stops: Vec<(f32, [f32; 4])> = stops;
    // 过冲后位置可能交错，保持单调
    let mut max = f32::NEG_INFINITY;
    for stop in &mut stops {
        max = max.max(stop.0);
        stop.0 = max;
    }
    Gradient(Arc::new(Def {
        shape: if t < 0.5 { da.shape } else { db.shape },
        angle: lerp(da.angle, db.angle, t),
        center: [lerp(da.center[0], db.center[0], t), lerp(da.center[1], db.center[1], t)],
        radius: lerp(da.radius, db.radius, t),
        color_space,
        stops,
        wide: da.wide || db.wide,
    }))
}

/// 两个渐变全部色标位置的并集，升序；任何一边在某个位置有硬边时，这个位置出现两次
/// （先取左侧、再取右侧的颜色）
fn union_positions(a: &Gradient, b: &Gradient) -> Vec<(f32, bool)> {
    let mut positions: Vec<f32> = a.0.stops.iter().chain(&b.0.stops).map(|s| s.0).collect();
    positions.sort_by(f32::total_cmp);
    positions.dedup();
    let hard = |g: &Gradient, p: f32| g.0.stops.iter().filter(|s| s.0 == p).count() >= 2;
    let mut out = Vec::with_capacity(positions.len() + 2);
    for p in positions {
        if hard(a, p) || hard(b, p) {
            out.push((p, false));
            out.push((p, true));
        } else {
            out.push((p, false));
        }
    }
    out
}

/// 速度投影用的取样数
const SAMPLES: usize = 9;
const SAMPLED: usize = SAMPLES * 4 + 4;

/// 填充在函数空间里的取样：沿渐变均匀取样的颜色（预乘 OKLab）+ 几何参数。
/// 纯色用 `like` 的几何。用来估计打断时的速度，不求精确
fn sampled(fill: &Fill, like: &Fill) -> [f32; SAMPLED] {
    let mut out = [0.0; SAMPLED];
    let geometry = match (fill, like) {
        (Fill::Gradient(g), _) | (Fill::Color(_), Fill::Gradient(g)) => {
            let d = &*g.0;
            [d.angle / 360.0, d.center[0], d.center[1], d.radius]
        }
        _ => [0.0; 4],
    };
    for i in 0..SAMPLES {
        let x = i as f32 / (SAMPLES - 1) as f32;
        let color = match fill {
            Fill::Color(c) => ColorSpace::Oklab.encode(*c),
            Fill::Gradient(g) => g.0.color_space.convert(g.sample(x, false), ColorSpace::Oklab),
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&color);
    }
    out[SAMPLES * 4..].copy_from_slice(&geometry);
    out
}

fn difference(to: &Fill, from: &Fill) -> [f32; SAMPLED] {
    let (a, b) = (sampled(to, from), sampled(from, to));
    std::array::from_fn(|i| a[i] - b[i])
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 过渡被打断、改去 `target` 时，新进度的初始速度：把此刻的变化速度投影到新方向上。
/// 来回切换（新方向和原来的平行）时正好连续，弹簧不会突然停下再出发。
///
/// `motion` 是此刻的运动：正在混合时为（混合，进度速度），纯色动画时为预乘 OKLab 的颜色速度
pub(crate) fn retarget_velocity(current: &Fill, target: &Fill, motion: FillVelocity) -> f32 {
    let direction = difference(target, current);
    let length = dot(&direction, &direction);
    if length <= 1e-9 {
        return 0.0;
    }
    let along = match motion {
        FillVelocity::Mix(mix, speed) => dot(&difference(&mix.to, &mix.from), &direction) * speed,
        FillVelocity::Color(velocity) => (0..SAMPLES).map(|i| dot(&velocity, &direction[i * 4..i * 4 + 4])).sum(),
    };
    (along / length).clamp(-100.0, 100.0)
}

pub(crate) enum FillVelocity<'a> {
    Mix(&'a FillMix, f32),
    Color([f32; 4]),
}
