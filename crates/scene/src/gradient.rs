//! 渐变在显示列表里的编码。
//!
//! 所有渐变放在一个 `vec4` 数组（存储缓冲区）里，图元的 `gradient` 字段是表头的下标，0 表示纯色。
//! 一个渐变：
//!
//! - 表头 2 个 `vec4`：层坐标 -> 渐变坐标的仿射变换（列主序 2x2、平移），形状、插值空间和"有扩展范围的色标"，色标数
//! - 色标位置，每 4 个一组
//! - 色标颜色：插值空间里预乘过的值
//!
//! 元素的布局、变换和渐变自己的几何参数在 CPU 上合成一个仿射变换，顶点着色器算出每个顶点的
//! 渐变坐标，片元里插值后求颜色。所以渐变跟着元素和合成层一起平移、缩放、旋转，
//! 而图元本身只多一个下标，仍然是同一次 draw call。

use crate::color::{decode_srgb, encode_srgb, linear_to_oklab, oklab_to_linear};
use crate::{Color, DisplayList, WideColor};

/// 渐变的形状。渐变坐标 `p` 是仿射变换后的位置：线性渐变的位置是 `p.x`，
/// 径向是 `|p|`，锥形是 `p` 从正上方顺时针转过的角度（圈数）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GradientShape {
    /// 线性（默认）
    #[default]
    Linear,
    /// 径向
    Radial,
    /// 锥形，绕中心一圈
    Conic,
}

/// 色标之间在哪个色彩空间插值
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ColorSpace {
    /// 感知均匀：亮度和色相匀速变化，互补色之间不发灰（默认）
    #[default]
    Oklab,
    /// 线性光，物理上的混合，中间偏亮
    LinearRgb,
    /// sRGB 编码值，最常见的老做法；互补色之间发灰发暗
    Srgb,
}

impl ColorSpace {
    /// 颜色在这个空间里的预乘值（插值、动画都在这上面做）
    pub fn encode(self, color: Color) -> [f32; 4] {
        let [x, y, z, alpha] = match self {
            Self::Oklab => color.to_oklab(),
            Self::LinearRgb => color.to_linear(),
            Self::Srgb => color.0.map(|c| c as f32 / 255.0),
        };
        [x * alpha, y * alpha, z * alpha, alpha]
    }

    /// 扩展范围的颜色在这个空间里的预乘值，不截断（sRGB 编码值在 0..=1 之外对称延伸）
    pub fn encode_wide(self, color: WideColor) -> [f32; 4] {
        let [x, y, z] = self.encode_linear(color.rgb);
        let alpha = color.alpha;
        [x * alpha, y * alpha, z * alpha, alpha]
    }

    /// 把这个空间里的预乘值换到另一个空间
    pub fn convert(self, value: [f32; 4], to: ColorSpace) -> [f32; 4] {
        if self == to {
            return value;
        }
        let alpha = value[3];
        if alpha <= 1e-6 {
            return [0.0; 4];
        }
        let linear = self.to_linear([value[0] / alpha, value[1] / alpha, value[2] / alpha]);
        let [x, y, z] = to.encode_linear(linear);
        [x * alpha, y * alpha, z * alpha, alpha]
    }

    /// 预乘值 -> 8 位 sRGB 颜色，超出色域的截断
    pub fn to_color(self, value: [f32; 4]) -> Color {
        let alpha = value[3].clamp(0.0, 1.0);
        if alpha <= 1e-6 {
            return Color::TRANSPARENT;
        }
        let [l, a, b, _] = self.convert(value, Self::Oklab);
        Color::from_oklab([l / alpha, a / alpha, b / alpha, alpha])
    }

    /// 预乘值 -> 扩展范围的颜色，不截断
    pub fn to_wide(self, value: [f32; 4]) -> WideColor {
        let alpha = value[3].clamp(0.0, 1.0);
        if alpha <= 1e-6 {
            return WideColor::default();
        }
        WideColor { rgb: self.to_linear([value[0] / alpha, value[1] / alpha, value[2] / alpha]), alpha }
    }

    fn to_linear(self, [x, y, z]: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Oklab => oklab_to_linear([x, y, z]),
            Self::LinearRgb => [x, y, z],
            Self::Srgb => [x, y, z].map(decode_srgb),
        }
    }

    fn encode_linear(self, rgb: [f32; 3]) -> [f32; 3] {
        match self {
            Self::Oklab => linear_to_oklab(rgb),
            Self::LinearRgb => rgb,
            Self::Srgb => rgb.map(encode_srgb),
        }
    }
}

/// 一个要画的渐变：几何已经合成到层坐标里
#[derive(Clone, Copy, Debug)]
pub struct GradientPaint<'a> {
    /// 形状
    pub shape: GradientShape,
    /// 层坐标 -> 渐变坐标：列主序 2x2
    pub linear: [f32; 4],
    /// 层坐标 -> 渐变坐标：平移
    pub offset: [f32; 2],
    /// 插值用的色彩空间
    pub color_space: ColorSpace,
    /// 色标：位置（单调不减）和 `color_space` 里的预乘颜色（[`ColorSpace::encode`]）
    pub stops: &'a [(f32, [f32; 4])],
    /// 有扩展范围的色标（[`WideColor`]：sRGB 色域外、比 SDR 白亮）：着色器按目标能显示的范围放进去
    /// （8 位目标截掉负的分量、比余量亮的按最大分量等比压下来），而不是逐个分量截到 0..=1；
    /// 显示列表记下 [`DisplayList::extended`]
    pub extended: bool,
}

impl DisplayList {
    /// 登记一个渐变，返回给图元 [`crate::Quad::gradient`] 用的下标
    pub fn push_gradient(&mut self, gradient: &GradientPaint) -> u32 {
        // 下标 0 表示纯色，放一个占位
        if self.gradients.is_empty() {
            self.gradients.push([0.0; 4]);
        }
        let index = self.gradients.len() as u32;
        let empty = [(0.0, [0.0; 4])];
        let stops = if gradient.stops.is_empty() { &empty[..] } else { gradient.stops };
        // 形状和插值空间、色标数都是小整数，用浮点存（位模式存整数会被当成非规格化数清零）
        let flags = gradient.shape as u32 + 4 * gradient.color_space as u32 + if gradient.extended { 16 } else { 0 };
        self.extended |= gradient.extended;
        self.gradients.push(gradient.linear);
        self.gradients.push([gradient.offset[0], gradient.offset[1], flags as f32, stops.len() as f32]);
        for chunk in stops.chunks(4) {
            let mut positions = [chunk[chunk.len() - 1].0; 4];
            for (slot, stop) in positions.iter_mut().zip(chunk) {
                *slot = stop.0;
            }
            self.gradients.push(positions);
        }
        self.gradients.extend(stops.iter().map(|stop| stop.1));
        index
    }
}
