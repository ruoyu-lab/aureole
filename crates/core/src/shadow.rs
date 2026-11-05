//! 几层投影和内阴影：[`Shadows`] 是元素上的一组投影，`ShadowMix` 是它们之间的过渡。
//!
//! 绘制：最常见的"一层不带扩展的外投影"直接画在元素的形状图元里（和以前一样，不多一个图元）；
//! 其余每层一个图元（[`Quad::SHADOW`]、[`Quad::INSET_SHADOW`]）：外面的投影在形状下面，
//! 内阴影在背景上面、子元素下面。都在同一次 draw call 里，不多渲染通道。

use std::sync::Arc;

use scene::{Color, DisplayList, Quad};

use crate::gradient::Gradient;

/// 投影：偏移、模糊半径（高斯模糊标准差的 2 倍）、扩展和颜色。默认画在形状外面；
/// [`Shadow::inner`] 是内阴影，画在形状里面（边框以内）。几层叠在一起用 [`crate::Element::shadow`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    /// 水平偏移，逻辑像素，正值向右
    pub x: f32,
    /// 竖直偏移，逻辑像素，正值向下
    pub y: f32,
    /// 模糊半径，逻辑像素
    pub blur: f32,
    /// 扩展，逻辑像素：投影的形状先往外扩这么多（负值往里缩）再模糊；内阴影往里多铺这么宽
    pub spread: f32,
    /// 颜色
    pub color: Color,
    /// 内阴影：画在形状里面，像形状的边缘高出一块、在里面投下的影子
    pub inner: bool,
}

impl Shadow {
    /// 没有投影
    pub const NONE: Self = Self { x: 0.0, y: 0.0, blur: 0.0, spread: 0.0, color: Color::TRANSPARENT, inner: false };

    /// 偏移 (x, y)、模糊半径，逻辑像素
    pub const fn new(x: f32, y: f32, blur: f32, color: Color) -> Self {
        Self { x, y, blur, spread: 0.0, color, inner: false }
    }

    /// 内阴影：偏移 (x, y)、模糊半径，逻辑像素。偏移向下时阴影在上边，像按下去的凹槽
    pub const fn inner(x: f32, y: f32, blur: f32, color: Color) -> Self {
        Self { x, y, blur, spread: 0.0, color, inner: true }
    }

    /// 设置扩展，逻辑像素（见同名的字段）
    pub const fn spread(self, spread: f32) -> Self {
        Self { spread, ..self }
    }

    /// 颜色不透明才画
    #[inline]
    pub(crate) fn is_visible(&self) -> bool {
        !self.color.is_transparent()
    }
}

/// 几层投影（[`crate::Element::shadow`]），第一层画在最上面。外面的投影和内阴影可以混在一起，
/// 比如一层贴近的接触投影加一层大范围的柔和投影，或者一层外投影加一层顶部的内阴影。克隆便宜
///
/// ```
/// # use aureole::prelude::*;
/// let ink = Color::hex(0x0b1e24);
/// let raised = Shadows::new([Shadow::new(0.0, 1.0, 2.0, ink.with_alpha(0.12)), Shadow::new(0.0, 8.0, 24.0, ink.with_alpha(0.08))]);
/// let pressed = Shadows::from(Shadow::inner(0.0, 2.0, 6.0, ink.with_alpha(0.18)));
/// assert_eq!(raised.as_slice().len(), 2);
/// assert!(pressed.as_slice()[0].inner);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Shadows(Arc<[Shadow]>);

impl Default for Shadows {
    /// 没有投影
    fn default() -> Self {
        Self::new([])
    }
}

impl Shadows {
    /// 按顺序的几层，第一层画在最上面
    pub fn new(shadows: impl IntoIterator<Item = Shadow>) -> Self {
        Self(shadows.into_iter().collect())
    }

    /// 全部的层，第一层在最上面
    pub fn as_slice(&self) -> &[Shadow] {
        &self.0
    }

    /// 只有一层不带扩展的外投影：直接画在形状图元里
    #[inline]
    pub(crate) fn plain(&self) -> Option<&Shadow> {
        match &*self.0 {
            [s] if !s.inner && s.spread == 0.0 => Some(s),
            _ => None,
        }
    }
}

impl From<Shadow> for Shadows {
    fn from(shadow: Shadow) -> Self {
        Self::new([shadow])
    }
}

impl<const N: usize> From<[Shadow; N]> for Shadows {
    fn from(shadows: [Shadow; N]) -> Self {
        Self::new(shadows)
    }
}

impl From<Vec<Shadow>> for Shadows {
    fn from(shadows: Vec<Shadow>) -> Self {
        Self::new(shadows)
    }
}

impl From<&[Shadow]> for Shadows {
    fn from(shadows: &[Shadow]) -> Self {
        Self::new(shadows.iter().copied())
    }
}

/// 一层投影的动画分量：偏移、模糊、扩展，预乘的 OKLab 颜色
pub(crate) fn components(s: &Shadow) -> [f32; 8] {
    let [l, a, b, alpha] = s.color.to_oklab();
    [s.x, s.y, s.blur, s.spread, l * alpha, a * alpha, b * alpha, alpha]
}

/// 分量 -> 投影（`inner` 由调用方决定）。不内联：别让颜色动画的分量转换变大
#[inline(never)]
pub(crate) fn from_components(c: &[f32], inner: bool) -> Shadow {
    let alpha = c[7].clamp(0.0, 1.0);
    let color = if alpha <= 1e-6 { Color::TRANSPARENT } else { Color::from_oklab([c[4] / alpha, c[5] / alpha, c[6] / alpha, alpha]) };
    Shadow { x: c[0], y: c[1], blur: c[2].max(0.0), spread: c[3], color, inner }
}

/// 几层投影之间的过渡：按进度 `t`（0..1，弹簧可能略超出）混合两端。
///
/// 同一位置、同一种（都是外投影或都是内阴影）的两层逐个分量插值（偏移、模糊、扩展、OKLab 颜色）；
/// 种类不同或者一边没有时交叉淡化：旧的一层原地淡出，新的一层原地淡入。所以按下按钮时外投影收起、
/// 内阴影出现可以一起过渡
pub(crate) struct ShadowMix {
    pub from: Shadows,
    pub to: Shadows,
}

impl ShadowMix {
    pub fn at(&self, t: f32) -> Shadows {
        let (a, b) = (self.from.as_slice(), self.to.as_slice());
        let lerp = |x: [f32; 8], y: [f32; 8]| -> [f32; 8] { std::array::from_fn(|i| x[i] + (y[i] - x[i]) * t) };
        let faded = |s: &Shadow| {
            let mut c = components(s);
            c[4..].fill(0.0);
            c
        };
        let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
        for i in 0..a.len().max(b.len()) {
            match (a.get(i), b.get(i)) {
                (Some(x), Some(y)) if x.inner == y.inner => out.push(from_components(&lerp(components(x), components(y)), y.inner)),
                (x, y) => {
                    if let Some(x) = x {
                        out.push(from_components(&lerp(components(x), faded(x)), x.inner));
                    }
                    if let Some(y) = y {
                        out.push(from_components(&lerp(faded(y), components(y)), y.inner));
                    }
                }
            }
        }
        Shadows::new(out)
    }

    /// 被打断、改去 `target` 时新进度的初始速度：把此刻的变化速度投影到新方向上（来回切换时正好连续）。
    /// `progress`、`speed` 是此刻的进度和进度的速度
    pub fn retarget(&self, target: &Shadows, progress: f32, speed: f32) -> f32 {
        let (from, to) = (self.from.as_slice(), self.to.as_slice());
        let len = from.len().max(to.len()).max(target.as_slice().len());
        let (a, b) = (vector(from, to, len), vector(to, from, len));
        let current: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + (y - x) * progress).collect();
        let goal = vector(target.as_slice(), to, len);
        let direction: Vec<f32> = goal.iter().zip(&current).map(|(g, c)| g - c).collect();
        let motion: Vec<f32> = a.iter().zip(&b).map(|(x, y)| (y - x) * speed).collect();
        project(&motion, &direction)
    }
}

/// 从按分量的动画（一层外投影）被打断、改去 `target`：此刻是 `current`，分量速度 `velocity`
pub(crate) fn retarget_components(current: &Shadow, target: &Shadows, velocity: &[f32]) -> f32 {
    let here = [*current];
    let len = target.as_slice().len().max(1);
    let start = vector(&here, target.as_slice(), len);
    let goal = vector(target.as_slice(), &here, len);
    let direction: Vec<f32> = goal.iter().zip(&start).map(|(g, c)| g - c).collect();
    let mut motion = vec![0.0; direction.len()];
    for (m, v) in motion.iter_mut().zip(velocity.iter().take(8)) {
        *m = *v;
    }
    project(&motion, &direction)
}

fn project(motion: &[f32], direction: &[f32]) -> f32 {
    let length: f32 = direction.iter().map(|d| d * d).sum();
    if length <= 1e-9 {
        return 0.0;
    }
    let along: f32 = motion.iter().zip(direction).map(|(m, d)| m * d).sum();
    (along / length).clamp(-100.0, 100.0)
}

/// 函数空间里的取样：每个位置两格（外投影、内阴影），每格 8 个分量。
/// 没有的格子用 `like` 里同一格的几何、透明的颜色（和 [`ShadowMix::at`] 的交叉淡化一致）
fn vector(list: &[Shadow], like: &[Shadow], len: usize) -> Vec<f32> {
    let mut v = vec![0.0; len * 16];
    for i in 0..len {
        for (k, inner) in [false, true].into_iter().enumerate() {
            let slot = &mut v[(i * 2 + k) * 8..(i * 2 + k + 1) * 8];
            match list.get(i).filter(|s| s.inner == inner) {
                Some(s) => slot.copy_from_slice(&components(s)),
                None => {
                    if let Some(s) = like.get(i).filter(|s| s.inner == inner) {
                        slot[..4].copy_from_slice(&components(s)[..4]);
                    }
                }
            }
        }
    }
    v
}

/// 元素的形状图元带着几层投影、内阴影或渐变边框：外面的投影按从下到上的顺序画在形状下面（第一层不带扩展的
/// 外投影画在形状图元里），内阴影画在形状（背景和边框）上面。`size` 是元素框的大小
#[cold]
#[inline(never)]
pub(crate) fn push_layered(list: &mut DisplayList, mut shape: Quad, shadows: &Shadows, border: Option<&Gradient>, size: [f32; 2]) {
    if let Some(first) = shadows.as_slice().first()
        && !first.inner
        && first.spread == 0.0
        && first.is_visible()
    {
        shape = shape.shadow(first.x, first.y, first.blur, first.color);
    }
    push_outer(list, &shape, shadows);
    let width = shape.border_width;
    if let (true, Some(gradient)) = (width > 0.0, border) {
        shape = shape.with_border_gradient(gradient.push_local(list, size));
    }
    let carries = !shape.shadow_color.is_transparent();
    if !shape.fill.is_transparent() || shape.gradient != 0 || (width > 0.0 && !shape.border_color.is_transparent()) || carries {
        list.quads.push(shape);
    }
    push_inner(list, &shape, shadows, width);
}

/// 外面的投影，按从下到上的顺序（最后一层先画）。`shape` 里已经画了第一层（不带扩展的外投影）时跳过它
#[cold]
#[inline(never)]
pub(crate) fn push_outer(list: &mut DisplayList, shape: &Quad, shadows: &Shadows) {
    let skip = usize::from(!shape.shadow_color.is_transparent());
    for s in shadows.as_slice().iter().skip(skip).rev().filter(|s| !s.inner && s.is_visible()) {
        list.quads.push(outer_quad(shape, s));
    }
}

/// 内阴影，按从下到上的顺序，画在边框（宽 `border`）以内
#[cold]
#[inline(never)]
pub(crate) fn push_inner(list: &mut DisplayList, shape: &Quad, shadows: &Shadows, border: f32) {
    for s in shadows.as_slice().iter().rev().filter(|s| s.inner && s.is_visible()) {
        if let Some(q) = inner_quad(shape, s, border.max(0.0)) {
            list.quads.push(q);
        }
    }
}

/// 单独的一层外投影：形状同 `shape`
pub(crate) fn outer_quad(shape: &Quad, s: &Shadow) -> Quad {
    Quad {
        kind: Quad::SHADOW,
        fill: Color::TRANSPARENT,
        border_color: Color::TRANSPARENT,
        border_width: s.spread,
        shadow_offset: [s.x, s.y],
        shadow_blur: s.blur.max(0.0),
        shadow_color: s.color,
        uv: [0.0; 4],
        gradient: 0,
        ..*shape
    }
}

/// 一层内阴影：在形状的边框以内（`border` 是边框宽度）
pub(crate) fn inner_quad(shape: &Quad, s: &Shadow, border: f32) -> Option<Quad> {
    let [x, y, w, h] = shape.rect;
    let (w, h) = (w - 2.0 * border, h - 2.0 * border);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some(Quad {
        kind: Quad::INSET_SHADOW,
        rect: [x + border, y + border, w, h],
        radii: shape.radii.map(|r| (r - border).max(0.0)),
        fill: Color::TRANSPARENT,
        border_color: Color::TRANSPARENT,
        border_width: s.spread,
        shadow_offset: [s.x, s.y],
        shadow_blur: s.blur.max(0.0),
        shadow_color: s.color,
        uv: [0.0; 4],
        gradient: 0,
        ..*shape
    })
}
