//! 属性：属性注册表（`registry`：编号、类型化的键、元数据，属性本身定义在 `tree/property_table.rs`），
//! 能做动画的属性和它们的值（[`Property`]、[`PropertyValue`]，拆成分量、按颜色空间混合），
//! 属性方法收的值（[`Prop`]：静态值或者绑定，[`IntoProp`] 把各种写法换成它）。
//!
//! 属性注册表公开的部分也在这里：每个属性只在一处定义（`tree/property_table.rs`），这里是它的键（`property::BACKGROUND` 是
//! [`PropKey<Fill>`](PropKey)）、编号（[`PropId`]）、元数据（[`PropMetadata`]）、属性集合、任意属性的值和按名字的解析。
//! 按名字找属性：`property::PropId::from_name("gap")`

pub(crate) mod registry;

use animation::MAX_COMPONENTS;
use reactive::{Memo, Signal};
use scene::{Angle, Color, WideColor};

use crate::visual_effects::{Filter, FilterMix};
use crate::gradient::{Fill, FillMix, FillVelocity, Gradient};
use crate::shadow::{ShadowMix, Shadows};
use crate::layout::{Align, Direction, Justify};
use crate::shadow::Shadow;
use crate::text::{InlineAlign, TextAlign};
use crate::vector::{VectorPath, Stroke};

/// 能做动画的属性由属性表生成（`tree/property_table.rs`：有 `anim` 的那些），和链式方法同名（蛇形对驼峰）
pub use crate::tree::property_table::Property;
pub use registry::{
    AnimationKind, AnimationSpec, AnyValue, Dirty, FromPropValue, KindMask, ParseError, PropId, PropKey, PropMetadata, PropSet, PropType, Storage, ValueSource,
};
pub use crate::tree::property_table::keys::*;
pub use crate::tree::take_misapplied;
#[doc(hidden)]
pub use registry::{AnyClone, Eval, Slot, Stored};

impl PropId {
    fn anim_kind(self) -> Option<AnimationKind> {
        self.metadata().animation.map(|a| a.kind)
    }

    /// 这个属性接受的值的种类是否和 `value` 一致（退场值、关键帧）
    pub(crate) fn accepts(self, value: &PropertyValue) -> bool {
        match self.anim_kind() {
            Some(AnimationKind::Fill) => matches!(value, PropertyValue::Color(_) | PropertyValue::Gradient(_)),
            Some(AnimationKind::Filter) => matches!(value, PropertyValue::Filter(_)),
            Some(AnimationKind::Shadow) => matches!(value, PropertyValue::Shadow(_) | PropertyValue::Shadows(_)),
            Some(AnimationKind::Number) => matches!(value, PropertyValue::Float(_)),
            // 路径变形、布局位置的滑动没有值可给；不能动画的属性也不接受
            Some(AnimationKind::Path | AnimationKind::Offset) | None => false,
        }
    }

    /// 绕 x、y 轴的旋转和透视：系统打开"减少动态效果"时不做动画，直接到位
    pub(crate) fn is_depth(self) -> bool {
        self.metadata().animation.is_some_and(|a| a.three_dimensional)
    }

    /// 值可能不能拆成分量（渐变、几层投影、内阴影）：这时按进度混合两端（[`Mix`]）
    pub(crate) fn mixes(self) -> bool {
        matches!(self.anim_kind(), Some(AnimationKind::Fill | AnimationKind::Shadow))
    }

    /// 视为停稳的误差，按属性的单位给（写在属性表里）
    pub(crate) fn tolerance(self) -> f32 {
        self.metadata().animation.map_or(1e-2, |a| a.tolerance)
    }
}

impl Property {
    /// 这个属性接受的值的种类是否和 `value` 一致
    pub(crate) fn accepts(self, value: &PropertyValue) -> bool {
        self.id().accepts(value)
    }
}

/// 可动画属性的值：数值、颜色、渐变、投影或滤镜。用于 [`crate::Element::exit`]
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    /// 数值
    Float(f32),
    /// 纯色
    Color(Color),
    /// 渐变
    Gradient(Gradient),
    /// 一层投影
    Shadow(Shadow),
    /// 几层投影
    Shadows(Shadows),
    /// 滤镜
    Filter(Filter),
}

impl From<f32> for PropertyValue {
    fn from(v: f32) -> Self {
        Self::Float(v)
    }
}

impl From<i32> for PropertyValue {
    fn from(v: i32) -> Self {
        Self::Float(v as f32)
    }
}

/// 角度存成弧度：`.exit(Property::Rotation, deg(90))`
impl From<Angle> for PropertyValue {
    fn from(angle: Angle) -> Self {
        Self::Float(angle.radians())
    }
}

impl From<Color> for PropertyValue {
    fn from(c: Color) -> Self {
        Self::Color(c)
    }
}

impl From<Gradient> for PropertyValue {
    fn from(g: Gradient) -> Self {
        Self::Gradient(g)
    }
}

impl From<Fill> for PropertyValue {
    fn from(fill: Fill) -> Self {
        match fill {
            Fill::Color(c) => Self::Color(c),
            Fill::Gradient(g) => Self::Gradient(g),
        }
    }
}

impl From<Shadow> for PropertyValue {
    fn from(s: Shadow) -> Self {
        Self::Shadow(s)
    }
}

/// 只有一层时是 [`PropertyValue::Shadow`]
impl From<Shadows> for PropertyValue {
    fn from(s: Shadows) -> Self {
        match s.as_slice() {
            [one] => Self::Shadow(*one),
            _ => Self::Shadows(s),
        }
    }
}

impl From<Filter> for PropertyValue {
    fn from(f: Filter) -> Self {
        Self::Filter(f)
    }
}

impl PropertyValue {
    /// 背景、文字颜色的值
    pub(crate) fn into_fill(self) -> Option<Fill> {
        match self {
            Self::Color(c) => Some(Fill::Color(c)),
            Self::Gradient(g) => Some(Fill::Gradient(g)),
            _ => None,
        }
    }

    /// 投影的值
    pub(crate) fn into_shadows(self) -> Option<Shadows> {
        match self {
            Self::Shadow(s) => Some(Shadows::from(s)),
            Self::Shadows(s) => Some(s),
            _ => None,
        }
    }

    /// 能拆成分量做动画：数值、纯色、一层外投影。渐变、几层投影、内阴影、滤镜按进度混合（[`Mix`]）
    pub(crate) fn is_simple(&self) -> bool {
        match self {
            Self::Gradient(_) | Self::Shadows(_) | Self::Filter(_) => false,
            Self::Shadow(s) => !s.inner,
            Self::Float(_) | Self::Color(_) => true,
        }
    }

    /// 拆成动画分量，返回分量数。颜色用预乘的 OKLab：感知均匀，
    /// 从透明淡入时也不会出现发黑的过渡。渐变不拆分量，按进度混合（见 [`crate::gradient::FillMix`]）
    pub(crate) fn to_components(&self, out: &mut [f32; MAX_COMPONENTS]) -> usize {
        match *self {
            Self::Float(v) => {
                out[0] = v;
                1
            }
            Self::Color(color) => {
                out[..4].copy_from_slice(&premultiplied_oklab(color));
                4
            }
            Self::Gradient(_) | Self::Shadows(_) => unreachable!("渐变、几层投影按进度混合，不拆分量"),
            Self::Filter(_) => unreachable!("滤镜按进度混合，不拆分量"),
            Self::Shadow(s) => {
                out[..8].copy_from_slice(&crate::shadow::components(&s));
                8
            }
        }
    }

    pub(crate) fn from_components(property: PropId, c: &[f32]) -> Self {
        match property.anim_kind() {
            Some(AnimationKind::Fill) => Self::Color(color_from(c)),
            Some(AnimationKind::Shadow) => Self::Shadow(crate::shadow::from_components(c, false)),
            _ => Self::Float(c[0]),
        }
    }
}

/// 不能拆成分量的过渡：渐变（[`FillMix`]）、几层投影和内阴影（[`ShadowMix`]）、滤镜（[`FilterMix`]）。
/// 动画的是 0 -> 1 的进度，值是两端的混合
pub(crate) enum Mix {
    Fill(FillMix),
    Shadows(ShadowMix),
    Filter(FilterMix),
}

impl Mix {
    /// `from`、`to` 是同一个属性的值
    pub fn new(from: PropertyValue, to: PropertyValue) -> Self {
        if let (PropertyValue::Filter(from), PropertyValue::Filter(to)) = (&from, &to) {
            return Self::Filter(FilterMix { from: from.clone(), to: to.clone() });
        }
        if matches!(from, PropertyValue::Shadow(_) | PropertyValue::Shadows(_)) {
            let from = from.into_shadows().expect("投影");
            return Self::Shadows(ShadowMix { from, to: to.into_shadows().expect("投影只能过渡到投影") });
        }
        let from = from.into_fill().expect("背景、边框、文字颜色只接受纯色和渐变");
        Self::Fill(FillMix { from, to: to.into_fill().expect("背景、边框、文字颜色只接受纯色和渐变") })
    }

    #[inline(never)]
    pub fn at(&self, t: f32) -> PropertyValue {
        match self {
            Self::Fill(mix) => PropertyValue::from(mix.at(t)),
            Self::Shadows(mix) => PropertyValue::from(mix.at(t)),
            Self::Filter(mix) => PropertyValue::Filter(mix.at(t)),
        }
    }

    /// 终点
    pub fn target(&self) -> PropertyValue {
        match self {
            Self::Fill(mix) => PropertyValue::from(mix.to.clone()),
            Self::Shadows(mix) => PropertyValue::from(mix.to.clone()),
            Self::Filter(mix) => PropertyValue::Filter(mix.to.clone()),
        }
    }

    /// 被打断、改去 `target` 时新进度的初始速度。`current` 是此刻的值，`progress`、`speed` 是此刻的进度和进度的速度
    pub fn retarget(&self, current: &PropertyValue, target: &PropertyValue, progress: f32, speed: f32) -> f32 {
        match (self, current.clone().into_fill(), target.clone().into_fill(), target.clone().into_shadows()) {
            (Self::Fill(mix), Some(current), Some(target), _) => crate::gradient::retarget_velocity(&current, &target, FillVelocity::Mix(mix, speed)),
            (Self::Shadows(mix), _, _, Some(target)) => mix.retarget(&target, progress, speed),
            _ => 0.0,
        }
    }

    /// 从按分量的动画（纯色、一层外投影）被打断、改去 `target`：此刻是 `current`，分量速度 `velocity`
    pub fn retarget_components(current: &PropertyValue, target: &PropertyValue, velocity: &[f32]) -> f32 {
        match (current, target) {
            (PropertyValue::Shadow(current), target) => match target.clone().into_shadows() {
                Some(target) => crate::shadow::retarget_components(current, &target, velocity),
                None => 0.0,
            },
            (current, target) => match (current.clone().into_fill(), target.clone().into_fill()) {
                (Some(current), Some(target)) => {
                    let v = [velocity[0], velocity[1], velocity[2], velocity[3]];
                    crate::gradient::retarget_velocity(&current, &target, FillVelocity::Color(v))
                }
                _ => 0.0,
            },
        }
    }
}

fn premultiplied_oklab(color: Color) -> [f32; 4] {
    let [l, a, b, alpha] = color.to_oklab();
    [l * alpha, a * alpha, b * alpha, alpha]
}

fn color_from(c: &[f32]) -> Color {
    let alpha = c[3].clamp(0.0, 1.0);
    if alpha <= 1e-6 {
        return Color::TRANSPARENT;
    }
    Color::from_oklab([c[0] / alpha, c[1] / alpha, c[2] / alpha, alpha])
}

// ---- 属性方法收的值 ----

/// 属性值：静态值，或者绑定（读取信号的闭包）
pub enum Prop<T> {
    /// 固定的值，没有响应式开销
    Static(T),
    /// 绑定：在副作用里求值，读到的信号变了就重新求值
    Dynamic(Box<dyn Fn() -> T>),
}

impl<T: 'static> Prop<T> {
    /// 换成另一种值（例如 `Color` -> `Fill`），绑定仍是绑定
    pub fn map<U>(self, f: impl Fn(T) -> U + 'static) -> Prop<U> {
        match self {
            Self::Static(v) => Prop::Static(f(v)),
            Self::Dynamic(g) => Prop::Dynamic(Box::new(move || f(g()))),
        }
    }
}

/// 能传给链式方法的属性：静态值、闭包、[`Signal`]、[`Memo`]。
///
/// `.opacity(0.5)` 是静态值，没有任何响应式开销；
/// `.opacity(move || if hovered.get() { 1.0 } else { 0.6 })` 是绑定，
/// 只在它读到的信号变化时重新求值，并且只更新这一个属性。
pub trait IntoProp<T> {
    /// 转成属性值
    fn into_prop(self) -> Prop<T>;

    /// 转成建造者里的值，顺便换类型（`Color` -> `Fill`）。闭包、信号直接装一次箱（不先装成 [`Prop`] 再包一层）
    #[doc(hidden)]
    fn into_slot<U: PropType>(self, map: fn(T) -> U) -> Slot
    where
        Self: Sized,
        T: 'static,
    {
        match self.into_prop() {
            Prop::Static(v) => Slot::Static(map(v).store()),
            Prop::Dynamic(f) => Slot::Bound(Box::new(move || map(f()))),
        }
    }
}

impl<T: 'static, F: Fn() -> T + 'static> IntoProp<T> for F {
    fn into_prop(self) -> Prop<T> {
        Prop::Dynamic(Box::new(self))
    }

    fn into_slot<U: PropType>(self, map: fn(T) -> U) -> Slot {
        Slot::Bound(Box::new(move || map(self())))
    }
}

impl<T: Clone + 'static> IntoProp<T> for Signal<T> {
    fn into_prop(self) -> Prop<T> {
        Prop::Dynamic(Box::new(move || self.get()))
    }

    fn into_slot<U: PropType>(self, map: fn(T) -> U) -> Slot {
        Slot::Bound(Box::new(move || map(self.get())))
    }
}

impl<T: Clone + PartialEq + 'static> IntoProp<T> for Memo<T> {
    fn into_prop(self) -> Prop<T> {
        Prop::Dynamic(Box::new(move || self.get()))
    }

    fn into_slot<U: PropType>(self, map: fn(T) -> U) -> Slot {
        Slot::Bound(Box::new(move || map(self.get())))
    }
}

impl Slot {
    /// 换回 [`Prop`]（富文本把文字的内容变成第一段）
    pub(crate) fn into_prop<T: PropType>(self) -> Option<Prop<T>> {
        match self {
            Slot::Static(s) => T::unstore(s).map(Prop::Static),
            Slot::Bound(f) => Some(Prop::Dynamic(Box::new(move || registry::eval::<T>(&*f).expect("属性的类型")))),
            Slot::Shared(f) => Some(Prop::Dynamic(Box::new(move || registry::eval::<T>(&*f).expect("属性的类型")))),
        }
    }
}

/// 已经转好的属性原样传过去（组件把收到的属性转交给里面的元素）
impl<T> IntoProp<T> for Prop<T> {
    fn into_prop(self) -> Prop<T> {
        self
    }
}

macro_rules! static_props {
    ($($t:ty),* $(,)?) => {
        $(
            impl IntoProp<$t> for $t {
                fn into_prop(self) -> Prop<$t> {
                    Prop::Static(self)
                }
            }
        )*
    };
}

// `f64`、`u32` 只有转成长度的方法收（`V: Into<Length>`）；只收 `f32` 的属性不收它们，字面量才推断得出来
static_props!(f32, f64, u32, bool, usize, String, Angle, Color, WideColor, Shadow, Shadows, Align, Justify, Direction, [f32; 4], VectorPath, Stroke, Fill, Gradient, TextAlign, InlineAlign);
static_props!(
    animation::Curve,
    crate::interaction::FocusRing,
    crate::element::Shader,
    crate::drag_drop::DragPreview,
    crate::editor::SubmitKey,
    crate::visual_effects::Mask,
    accesskit::Role,
    accesskit::Live,
);

/// 几层投影：`.shadow([接触投影, 柔和投影])`
impl<const N: usize> IntoProp<[Shadow; N]> for [Shadow; N] {
    fn into_prop(self) -> Prop<[Shadow; N]> {
        Prop::Static(self)
    }
}

/// 几层投影：`.shadow(vec![..])`
impl IntoProp<Vec<Shadow>> for Vec<Shadow> {
    fn into_prop(self) -> Prop<Vec<Shadow>> {
        Prop::Static(self)
    }
}

/// 任何 `&str` 都行（字面量、`name.as_str()`、函数参数），复制一份：`text(name)`
impl IntoProp<String> for &str {
    fn into_prop(self) -> Prop<String> {
        Prop::Static(self.to_owned())
    }
}

/// `text(&title)`：复制一份，原来的 `String` 还能接着用
impl IntoProp<String> for &String {
    fn into_prop(self) -> Prop<String> {
        Prop::Static(self.clone())
    }
}

/// `Cow<str>`（比如 `String::from_utf8_lossy` 的结果）
impl IntoProp<String> for std::borrow::Cow<'_, str> {
    fn into_prop(self) -> Prop<String> {
        Prop::Static(self.into_owned())
    }
}

/// 整数字面量也能当长度用：`.padding(24)`
impl IntoProp<f32> for i32 {
    fn into_prop(self) -> Prop<f32> {
        Prop::Static(self as f32)
    }
}
