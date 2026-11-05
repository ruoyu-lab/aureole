//! 滤镜、混合模式、遮罩、缓存成纹理：元素上的链式方法和它们的值类型。
//!
//! 这些属性（`filter`、`blend`、`mask`、`group_cache`）定义在属性表（`tree/property_table.rs`）里，这里是值的类型。
//! 效果作用于元素连同子孙：先画进离屏纹理（一个分组），合成时再做滤镜、遮罩、混合；
//! 内容没变时渲染线程直接用上次画好的纹理。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use animation::Curve;
use scene::{Angle, ColorMatrix};

pub use scene::BlendMode;

use crate::property::Property;
use crate::element::{Element, View, path, rect};
use crate::gradient::Gradient;
use crate::image::{ImageFit, ImageSource, image};
use crate::property::{IntoProp, Prop};
use crate::property::registry::{PropType, Slot};
use crate::tree::{NodeId, TreeRef, mount};
use crate::vector::VectorPath;

/// 滤镜：亮度、对比度、灰度、饱和度、色相旋转、反相、褐色调、不透明度、模糊，按顺序组合成一串。
///
/// 每一种既能开头（`Filter::grayscale(1.0)`），也能接在后面（`.contrast(1.2)`，要导入 [`FilterChain`]，
/// prelude 里有）。颜色的操作在 sRGB 编码的颜色上按顺序算（对比度绕中灰、反相后中灰不变），合起来是一个颜色矩阵；
/// 模糊总是先做，和 [`Element::blur`] 叠加（两个高斯模糊叠起来的标准差是平方和开方）。
///
/// ```
/// # use aureole::prelude::*;
/// let hovered = signal(false);
/// image("cover.png")
///     .filter(move || if hovered.get() { Filter::NONE } else { Filter::grayscale(1.0).contrast(1.1) })
///     .transition(Property::Filter, Tween::new(ms(200)))
///     .on_hover(move |inside| hovered.set(inside));
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    /// 没有操作时为 `None`。共享、不可变：属性值（[`crate::PropertyValue`]）里放它不变大，复制只加引用计数
    ops: Option<Arc<[Op]>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    Saturation(f32),
    HueRotation(f32),
    Inversion(f32),
    Sepia(f32),
    Opacity(f32),
    Blur(f32),
    /// 结构不同的两个滤镜过渡到一半：直接混合两个颜色矩阵
    Matrix(ColorMatrix),
}

impl Op {
    /// 同一种操作（参数可以不同）
    fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// 不改变画面时的参数
    fn identity(&self) -> Self {
        match *self {
            Self::Brightness(_) => Self::Brightness(1.0),
            Self::Contrast(_) => Self::Contrast(1.0),
            Self::Grayscale(_) => Self::Grayscale(0.0),
            Self::Saturation(_) => Self::Saturation(1.0),
            Self::HueRotation(_) => Self::HueRotation(0.0),
            Self::Inversion(_) => Self::Inversion(0.0),
            Self::Sepia(_) => Self::Sepia(0.0),
            Self::Opacity(_) => Self::Opacity(1.0),
            Self::Blur(_) => Self::Blur(0.0),
            Self::Matrix(_) => Self::Matrix(ColorMatrix::IDENTITY),
        }
    }

    /// 同一种操作按 `t` 插值
    fn lerp(&self, other: &Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        match (*self, *other) {
            (Self::Brightness(a), Self::Brightness(b)) => Self::Brightness(mix(a, b)),
            (Self::Contrast(a), Self::Contrast(b)) => Self::Contrast(mix(a, b)),
            (Self::Grayscale(a), Self::Grayscale(b)) => Self::Grayscale(mix(a, b)),
            (Self::Saturation(a), Self::Saturation(b)) => Self::Saturation(mix(a, b)),
            (Self::HueRotation(a), Self::HueRotation(b)) => Self::HueRotation(mix(a, b)),
            (Self::Inversion(a), Self::Inversion(b)) => Self::Inversion(mix(a, b)),
            (Self::Sepia(a), Self::Sepia(b)) => Self::Sepia(mix(a, b)),
            (Self::Opacity(a), Self::Opacity(b)) => Self::Opacity(mix(a, b)),
            (Self::Blur(a), Self::Blur(b)) => Self::Blur(mix(a, b)),
            (Self::Matrix(a), Self::Matrix(b)) => Self::Matrix(a.lerp(&b, t)),
            _ => *other,
        }
    }

    fn matrix(&self) -> Option<ColorMatrix> {
        Some(match *self {
            Self::Brightness(v) => ColorMatrix::brightness(v),
            Self::Contrast(v) => ColorMatrix::contrast(v),
            Self::Grayscale(v) => ColorMatrix::grayscale(v),
            Self::Saturation(v) => ColorMatrix::saturation(v),
            Self::HueRotation(v) => ColorMatrix::hue_rotation(v),
            Self::Inversion(v) => ColorMatrix::inversion(v),
            Self::Sepia(v) => ColorMatrix::sepia(v),
            Self::Opacity(v) => ColorMatrix::opacity(v),
            Self::Matrix(m) => m,
            Self::Blur(_) => return None,
        })
    }
}

impl Filter {
    /// 没有滤镜
    pub const NONE: Self = Self { ops: None };

    fn from_op(op: Op) -> Self {
        Self::from_ops(vec![op])
    }

    fn from_ops(ops: Vec<Op>) -> Self {
        Self { ops: (!ops.is_empty()).then(|| ops.into()) }
    }

    fn ops(&self) -> &[Op] {
        self.ops.as_deref().unwrap_or_default()
    }

    /// 亮度：1 不变，0 全黑，大于 1 更亮
    pub fn brightness(amount: f32) -> Self {
        Self::from_op(Op::Brightness(amount))
    }

    /// 对比度：1 不变，0 全是中灰，大于 1 对比更强
    pub fn contrast(amount: f32) -> Self {
        Self::from_op(Op::Contrast(amount))
    }

    /// 灰度：0 不变，1 完全去色
    pub fn grayscale(amount: f32) -> Self {
        Self::from_op(Op::Grayscale(amount))
    }

    /// 饱和度：1 不变，0 灰度，大于 1 更鲜艳
    pub fn saturation(amount: f32) -> Self {
        Self::from_op(Op::Saturation(amount))
    }

    /// 色相旋转
    pub fn hue_rotation(angle: Angle) -> Self {
        Self::from_op(Op::HueRotation(angle.radians()))
    }

    /// 反相：0 不变，1 完全反相
    pub fn inversion(amount: f32) -> Self {
        Self::from_op(Op::Inversion(amount))
    }

    /// 褐色调：0 不变，1 完全是旧照片的褐色
    pub fn sepia(amount: f32) -> Self {
        Self::from_op(Op::Sepia(amount))
    }

    /// 不透明度：1 不变，0 完全透明（和 [`Element::opacity`] 一样作用于整个元素，但能和其他滤镜一起过渡）
    pub fn opacity(amount: f32) -> Self {
        Self::from_op(Op::Opacity(amount))
    }

    /// 高斯模糊，标准差（逻辑像素）。和 [`Element::blur`] 相同；在颜色的操作之前做
    pub fn blur(sigma: f32) -> Self {
        Self::from_op(Op::Blur(sigma))
    }

    /// 没有任何操作
    pub fn is_none(&self) -> bool {
        self.ops.is_none()
    }

    fn push(self, op: Op) -> Self {
        let mut ops = self.ops().to_vec();
        ops.push(op);
        Self::from_ops(ops)
    }

    /// 颜色操作合成的矩阵；不改变颜色时为 `None`
    pub(crate) fn color_matrix(&self) -> Option<ColorMatrix> {
        let m = self.ops().iter().filter_map(Op::matrix).fold(ColorMatrix::IDENTITY, |m, op| m.then(&op));
        (!m.is_identity()).then_some(m)
    }

    /// 模糊的标准差（几个模糊叠起来按平方和开方）
    pub(crate) fn blur_sigma(&self) -> f32 {
        let sum: f32 = self.ops().iter().map(|op| if let Op::Blur(s) = op { s.max(0.0).powi(2) } else { 0.0 }).sum();
        sum.sqrt()
    }

    /// 从 `from` 过渡到 `to`，进度 `t`。两边的操作种类和顺序一样时逐个插值参数；
    /// 一边是另一边的开头时，短的那边补上不改变画面的操作；都不是时混合两个颜色矩阵（模糊单独插值）
    pub(crate) fn mix(from: &Self, to: &Self, t: f32) -> Self {
        if t >= 1.0 {
            return to.clone();
        }
        let (a, b) = (from.ops(), to.ops());
        let prefix = a.iter().zip(b).all(|(x, y)| x.same_kind(y));
        if prefix {
            let n = a.len().max(b.len());
            let ops = (0..n)
                .map(|i| {
                    let x = a.get(i).copied().unwrap_or_else(|| b[i].identity());
                    let y = b.get(i).copied().unwrap_or_else(|| a[i].identity());
                    x.lerp(&y, t)
                })
                .collect();
            return Self::from_ops(ops);
        }
        let (ma, mb) = (from.color_matrix().unwrap_or_default(), to.color_matrix().unwrap_or_default());
        let blur = from.blur_sigma() + (to.blur_sigma() - from.blur_sigma()) * t;
        Self::from_ops(vec![Op::Blur(blur), Op::Matrix(ma.lerp(&mb, t))])
    }
}

/// 在滤镜后面接着加一个操作：`Filter::grayscale(1.0).contrast(1.2)`
pub trait FilterChain {
    /// 接着调亮度
    fn brightness(self, amount: f32) -> Filter;
    /// 接着调对比度
    fn contrast(self, amount: f32) -> Filter;
    /// 接着去色
    fn grayscale(self, amount: f32) -> Filter;
    /// 接着调饱和度
    fn saturation(self, amount: f32) -> Filter;
    /// 接着旋转色相
    fn hue_rotation(self, angle: Angle) -> Filter;
    /// 接着反相
    fn inversion(self, amount: f32) -> Filter;
    /// 接着加褐色调
    fn sepia(self, amount: f32) -> Filter;
    /// 接着调不透明度
    fn opacity(self, amount: f32) -> Filter;
    /// 再模糊（和其他模糊叠加，总是在颜色的操作之前做）
    fn blur(self, sigma: f32) -> Filter;
}

impl FilterChain for Filter {
    fn brightness(self, amount: f32) -> Filter {
        self.push(Op::Brightness(amount))
    }

    fn contrast(self, amount: f32) -> Filter {
        self.push(Op::Contrast(amount))
    }

    fn grayscale(self, amount: f32) -> Filter {
        self.push(Op::Grayscale(amount))
    }

    fn saturation(self, amount: f32) -> Filter {
        self.push(Op::Saturation(amount))
    }

    fn hue_rotation(self, angle: Angle) -> Filter {
        self.push(Op::HueRotation(angle.radians()))
    }

    fn inversion(self, amount: f32) -> Filter {
        self.push(Op::Inversion(amount))
    }

    fn sepia(self, amount: f32) -> Filter {
        self.push(Op::Sepia(amount))
    }

    fn opacity(self, amount: f32) -> Filter {
        self.push(Op::Opacity(amount))
    }

    fn blur(self, sigma: f32) -> Filter {
        self.push(Op::Blur(sigma))
    }
}

impl IntoProp<Filter> for Filter {
    fn into_prop(self) -> Prop<Filter> {
        Prop::Static(self)
    }
}

impl IntoProp<BlendMode> for BlendMode {
    fn into_prop(self) -> Prop<BlendMode> {
        Prop::Static(self)
    }
}

/// 滤镜之间的过渡：动画是 0 -> 1 的进度，值是两端的混合（[`Filter::mix`]）
#[derive(Clone, Debug)]
pub(crate) struct FilterMix {
    pub from: Filter,
    pub to: Filter,
}

impl FilterMix {
    pub fn at(&self, t: f32) -> Filter {
        Filter::mix(&self.from, &self.to, t)
    }
}

/// 遮罩：元素连同子孙只在遮罩不透明的地方显示（按遮罩的透明度，或者 [`Mask::luminance`] 时按明度）。
///
/// 遮罩铺满元素（内边距框）：渐变、图片拉伸到元素的大小，路径按画布（[`VectorPath::view_box`]）缩放。
/// 遮罩本身也是一个元素，能绑定信号、做动画（[`Mask::element`] 可以放任何元素）。
///
/// ```
/// # use aureole::prelude::*;
/// // 滚动列表的上下两端淡出
/// let fade = Gradient::linear(deg(180), [(0.0, Color::TRANSPARENT), (0.08, Color::WHITE), (0.92, Color::WHITE), (1.0, Color::TRANSPARENT)]);
/// scroll_y(column().children((0..50).map(|i| text(format!("第 {i} 行"))))).height(240).mask(Mask::gradient(fade));
/// ```
#[derive(Clone)]
pub struct Mask {
    /// 遮罩的元素：挂载时取走一次（同一个遮罩交给两个元素时，后挂载的那个没有遮罩）
    element: Rc<RefCell<Option<Element>>>,
    luminance: bool,
}

impl Mask {
    /// 渐变的透明度当遮罩（可以绑定；[`Mask::transition`] 让换渐变时过渡）
    pub fn gradient(gradient: impl IntoProp<Gradient>) -> Self {
        Self::fill(rect().background(gradient))
    }

    /// 图片的透明度当遮罩，拉伸到元素的大小。图片还没加载好时元素不显示
    pub fn image<V: Into<ImageSource> + 'static>(source: impl IntoProp<V>) -> Self {
        Self::fill(image(source).fit(ImageFit::Stretch))
    }

    /// 路径的形状当遮罩（填充，或者 [`Mask::element`] 里自己设描边）
    pub fn path(shape: impl IntoProp<VectorPath>) -> Self {
        Self::fill(path(shape).color(scene::Color::WHITE))
    }

    /// 任意元素当遮罩：铺满元素的一个容器里放着 `view`（它可以有自己的大小、位置、动画）
    pub fn element(view: impl View) -> Self {
        Self::fill(rect().child(view))
    }

    /// 没有遮罩（属性的默认值）
    pub(crate) fn none() -> Self {
        Self { element: Rc::new(RefCell::new(None)), luminance: false }
    }

    fn fill(element: Element) -> Self {
        Self { element: Rc::new(RefCell::new(Some(element.inset(0)))), luminance: false }
    }

    /// 用明度（而不是透明度）当遮罩：白色显示、黑色隐藏，适合黑白的遮罩图
    pub fn luminance(mut self, luminance: bool) -> Self {
        self.luminance = luminance;
        self
    }

    /// 遮罩的背景（渐变遮罩的渐变）变化时按曲线过渡
    pub fn transition(self, curve: impl Into<Curve>) -> Self {
        let element = self.element.borrow_mut().take().map(|e| e.transition(Property::Background, curve));
        *self.element.borrow_mut() = element;
        self
    }
}

/// 同一个遮罩（克隆出来的）才算一样
impl PartialEq for Mask {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.element, &other.element) && self.luminance == other.luminance
    }
}

impl std::fmt::Debug for Mask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mask").field("luminance", &self.luminance).finish_non_exhaustive()
    }
}

/// 挂载时绑定遮罩：遮罩元素挂在这个元素下面。绑定的遮罩换了时卸载旧的遮罩元素、挂上新的
/// （遮罩元素的绑定跟着这个副作用：换的时候一起释放）
pub(crate) fn bind_mask(tree: &TreeRef, id: NodeId, slot: Slot) {
    let attach = move |tree: &TreeRef, mask: Mask| {
        let element = mask.element.borrow_mut().take();
        let old = tree.borrow().mask_of(id);
        if let Some(old) = old {
            tree.borrow_mut().detach_mask(id, old);
        }
        if let Some(element) = element {
            let node = mount(tree, element);
            tree.borrow_mut().attach_mask(id, node, mask.luminance);
        }
    };
    match slot {
        Slot::Static(stored) => {
            if let Some(mask) = <Mask as PropType>::unstore(stored) {
                attach(tree, mask);
            }
        }
        slot => {
            let weak = Rc::downgrade(tree);
            reactive::Effect::new(move || {
                let Some(mask) = slot.get::<Mask>() else { return };
                if let Some(tree) = weak.upgrade() {
                    attach(&tree, mask);
                }
            });
        }
    }
}

/// 节点上的效果（绘制属性里按需分配的一项）
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PaintEffects {
    pub filter: Filter,
    pub blend: BlendMode,
    /// 遮罩元素（挂在这个元素下面，但不在子元素里：不参与排列、命中测试、无障碍）
    pub mask: Option<NodeId>,
    pub luminance: bool,
    /// 不透明、没有效果也画进离屏纹理，缓存起来
    pub cache: bool,
}

impl PaintEffects {
    /// 一定要离屏：混合模式、遮罩、缓存成纹理
    pub fn forces_group(&self) -> bool {
        self.blend != BlendMode::Normal || self.mask.is_some() || self.cache
    }

    /// 加上滤镜里的模糊之后的模糊标准差
    pub fn blur(&self, sigma: f32) -> f32 {
        let extra = self.filter.blur_sigma();
        if extra == 0.0 { sigma } else { (sigma * sigma + extra * extra).sqrt() }
    }
}
