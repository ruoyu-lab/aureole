//! 滤镜的颜色矩阵、混合模式。
//!
//! 颜色矩阵在 sRGB 编码、未预乘的颜色上算（和调色的习惯一致：对比度绕中灰、反相后中灰不变），
//! 着色器里先把线性预乘的颜色换过去，算完再换回来；合成照旧在线性空间。

use crate::Color;

/// 4x5 的颜色矩阵：每一行是输出的 r、g、b、a 对输入 `(r, g, b, a, 1)` 的系数，
/// 输入输出都是 sRGB 编码、未预乘的 0..1 的值。多个滤镜按顺序相乘成一个矩阵，中间不截断
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorMatrix(pub [[f32; 5]; 4]);

/// 亮度系数（Rec. 709）
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

impl Default for ColorMatrix {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl ColorMatrix {
    /// 不改变颜色
    pub const IDENTITY: Self = Self([[1.0, 0.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0, 0.0]]);

    /// 只作用于 rgb 的 3x3 矩阵加偏移
    fn rgb(m: [[f32; 3]; 3], offset: f32) -> Self {
        let row = |r: [f32; 3]| [r[0], r[1], r[2], 0.0, offset];
        Self([row(m[0]), row(m[1]), row(m[2]), [0.0, 0.0, 0.0, 1.0, 0.0]])
    }

    /// 亮度：rgb 乘以 `amount`（1 不变，0 全黑）
    pub fn brightness(amount: f32) -> Self {
        let b = amount.max(0.0);
        Self::rgb([[b, 0.0, 0.0], [0.0, b, 0.0], [0.0, 0.0, b]], 0.0)
    }

    /// 对比度：绕中灰（0.5）拉开或收拢（1 不变，0 全是中灰）
    pub fn contrast(amount: f32) -> Self {
        let c = amount.max(0.0);
        Self::rgb([[c, 0.0, 0.0], [0.0, c, 0.0], [0.0, 0.0, c]], 0.5 * (1.0 - c))
    }

    /// 饱和度（1 不变，0 灰度，大于 1 更鲜艳），保持亮度
    pub fn saturation(amount: f32) -> Self {
        let s = amount.max(0.0);
        let [r, g, b] = LUMA;
        Self::rgb(
            [
                [r + (1.0 - r) * s, g - g * s, b - b * s],
                [r - r * s, g + (1.0 - g) * s, b - b * s],
                [r - r * s, g - g * s, b + (1.0 - b) * s],
            ],
            0.0,
        )
    }

    /// 灰度（0 不变，1 完全灰度）
    pub fn grayscale(amount: f32) -> Self {
        Self::saturation(1.0 - amount.clamp(0.0, 1.0))
    }

    /// 褐色调（0 不变，1 完全褐色）
    pub fn sepia(amount: f32) -> Self {
        let k = 1.0 - amount.clamp(0.0, 1.0);
        Self::rgb(
            [
                [0.393 + 0.607 * k, 0.769 - 0.769 * k, 0.189 - 0.189 * k],
                [0.349 - 0.349 * k, 0.686 + 0.314 * k, 0.168 - 0.168 * k],
                [0.272 - 0.272 * k, 0.534 - 0.534 * k, 0.131 + 0.869 * k],
            ],
            0.0,
        )
    }

    /// 色相旋转，弧度，保持亮度
    pub fn hue_rotation(radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        let [r, g, b] = LUMA;
        Self::rgb(
            [
                [r + cos * (1.0 - r) - sin * r, g - cos * g - sin * g, b - cos * b + sin * (1.0 - b)],
                [r - cos * r + sin * 0.143, g + cos * (1.0 - g) + sin * 0.140, b - cos * b - sin * 0.283],
                [r - cos * r - sin * (1.0 - r), g - cos * g + sin * g, b + cos * (1.0 - b) + sin * b],
            ],
            0.0,
        )
    }

    /// 反相（0 不变，1 完全反相，0.5 全是中灰）
    pub fn inversion(amount: f32) -> Self {
        let i = amount.clamp(0.0, 1.0);
        let k = 1.0 - 2.0 * i;
        Self::rgb([[k, 0.0, 0.0], [0.0, k, 0.0], [0.0, 0.0, k]], i)
    }

    /// 不透明度：alpha 乘以 `amount`（0..1）
    pub fn opacity(amount: f32) -> Self {
        let mut m = Self::IDENTITY;
        m.0[3][3] = amount.clamp(0.0, 1.0);
        m
    }

    /// 改色：rgb 都换成 `color` 的，透明度乘上它的透明度（只留下形状）
    pub fn tint(color: Color) -> Self {
        let [r, g, b, a] = color.0.map(|v| v as f32 / 255.0);
        Self([[0.0, 0.0, 0.0, 0.0, r], [0.0, 0.0, 0.0, 0.0, g], [0.0, 0.0, 0.0, 0.0, b], [0.0, 0.0, 0.0, a, 0.0]])
    }

    /// 先做 `self`，再做 `next`
    pub fn then(&self, next: &Self) -> Self {
        let (a, b) = (&self.0, &next.0);
        let mut out = [[0.0; 5]; 4];
        for (i, row) in out.iter_mut().enumerate() {
            for (j, value) in row.iter_mut().enumerate() {
                let mut sum = if j == 4 { b[i][4] } else { 0.0 };
                for (k, a) in a.iter().enumerate() {
                    sum += b[i][k] * a[j];
                }
                *value = sum;
            }
        }
        Self(out)
    }

    /// 两个矩阵逐项按 `t` 插值（滤镜的结构不同时的过渡）
    pub fn lerp(&self, other: &Self, t: f32) -> Self {
        Self(std::array::from_fn(|i| std::array::from_fn(|j| self.0[i][j] + (other.0[i][j] - self.0[i][j]) * t)))
    }

    /// 是不是单位矩阵（不改变颜色）
    pub fn is_identity(&self) -> bool {
        self.0.iter().flatten().zip(Self::IDENTITY.0.iter().flatten()).all(|(a, b)| (a - b).abs() < 1e-6)
    }

    /// 作用于一个 sRGB 颜色（和着色器里的算法相同，结果截到 0..1）
    pub fn apply(&self, color: Color) -> Color {
        let input = color.0.map(|v| v as f32 / 255.0);
        let out: [f32; 4] = std::array::from_fn(|i| {
            let row = &self.0[i];
            (row[0] * input[0] + row[1] * input[1] + row[2] * input[2] + row[3] * input[3] + row[4]).clamp(0.0, 1.0)
        });
        Color(out.map(|v| (v * 255.0).round() as u8))
    }

    /// 着色器用的布局：四行系数（r、g、b、a 的输入）和一行偏移，各 4 个数
    pub(crate) fn rows(&self) -> [[f32; 4]; 5] {
        let m = &self.0;
        let row = |i: usize| [m[i][0], m[i][1], m[i][2], m[i][3]];
        [row(0), row(1), row(2), row(3), [m[0][4], m[1][4], m[2][4], m[3][4]]]
    }
}

/// 元素和它后面已经画好的内容怎么混合（混合模式）。在线性、预乘的颜色上算，
/// `Normal` 是普通的叠放。除了 `Normal`，画到这个元素时要拷出后面的内容，断开一次渲染通道
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u32)]
#[non_exhaustive]
pub enum BlendMode {
    /// 普通叠放
    #[default]
    Normal = 0,
    /// 正片叠底：相乘，只会变暗
    Multiply = 1,
    /// 滤色：反相相乘再反相，只会变亮
    Screen = 2,
    /// 叠加：按后面的颜色，暗处正片叠底、亮处滤色
    Overlay = 3,
    /// 变暗：逐通道取较暗的
    Darken = 4,
    /// 变亮：逐通道取较亮的
    Lighten = 5,
    /// 颜色减淡
    ColorDodge = 6,
    /// 颜色加深
    ColorBurn = 7,
    /// 强光：按自己的颜色，暗处正片叠底、亮处滤色
    HardLight = 8,
    /// 柔光
    SoftLight = 9,
    /// 差值：相减取绝对值
    Difference = 10,
    /// 排除：和差值相似，对比更低
    Exclusion = 11,
    /// 色相：自己的色相，后面的饱和度和亮度
    Hue = 12,
    /// 饱和度：自己的饱和度，后面的色相和亮度
    Saturation = 13,
    /// 颜色：自己的色相和饱和度，后面的亮度
    Color = 14,
    /// 亮度：自己的亮度，后面的色相和饱和度
    Luminosity = 15,
}

impl BlendMode {
    /// 全部模式，按编号
    pub const ALL: [Self; 16] = [
        Self::Normal,
        Self::Multiply,
        Self::Screen,
        Self::Overlay,
        Self::Darken,
        Self::Lighten,
        Self::ColorDodge,
        Self::ColorBurn,
        Self::HardLight,
        Self::SoftLight,
        Self::Difference,
        Self::Exclusion,
        Self::Hue,
        Self::Saturation,
        Self::Color,
        Self::Luminosity,
    ];

    /// 按定义混合一个像素（线性、未预乘的 rgb）：`backdrop` 是后面的颜色，`source` 是元素的颜色。
    /// 着色器里是同样的算法；这里给测试和需要在 CPU 上算的地方用
    pub fn mix(self, backdrop: [f32; 3], source: [f32; 3]) -> [f32; 3] {
        let (b, s) = (backdrop, source);
        let lum = |c: [f32; 3]| c[0] * LUMA[0] + c[1] * LUMA[1] + c[2] * LUMA[2];
        let clip = |c: [f32; 3]| {
            let l = lum(c);
            let n = c[0].min(c[1]).min(c[2]);
            let x = c[0].max(c[1]).max(c[2]);
            let mut c = c;
            if n < 0.0 {
                c = c.map(|v| l + (v - l) * l / (l - n).max(1e-6));
            }
            if x > 1.0 {
                c = c.map(|v| l + (v - l) * (1.0 - l) / (x - l).max(1e-6));
            }
            c
        };
        let set_lum = |c: [f32; 3], l: f32| {
            let d = l - lum(c);
            clip(c.map(|v| v + d))
        };
        let sat = |c: [f32; 3]| c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2]);
        let set_sat = |c: [f32; 3], s: f32| {
            let (n, x) = (c[0].min(c[1]).min(c[2]), c[0].max(c[1]).max(c[2]));
            if x - n <= 1e-6 { [0.0; 3] } else { c.map(|v| (v - n) * s / (x - n)) }
        };
        let channel = |f: fn(f32, f32) -> f32| [f(b[0], s[0]), f(b[1], s[1]), f(b[2], s[2])];
        match self {
            Self::Normal => s,
            Self::Multiply => channel(|b, s| b * s),
            Self::Screen => channel(screen),
            Self::Overlay => channel(|b, s| hard_light(s, b)),
            Self::Darken => channel(f32::min),
            Self::Lighten => channel(f32::max),
            Self::ColorDodge => channel(|b, s| {
                if b <= 0.0 {
                    0.0
                } else if s >= 1.0 {
                    1.0
                } else {
                    (b / (1.0 - s)).min(1.0)
                }
            }),
            Self::ColorBurn => channel(|b, s| {
                if b >= 1.0 {
                    1.0
                } else if s <= 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - b) / s).min(1.0)
                }
            }),
            Self::HardLight => channel(hard_light),
            Self::SoftLight => channel(|b, s| {
                if s <= 0.5 {
                    b - (1.0 - 2.0 * s) * b * (1.0 - b)
                } else {
                    let d = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
                    b + (2.0 * s - 1.0) * (d - b)
                }
            }),
            Self::Difference => channel(|b, s| (b - s).abs()),
            Self::Exclusion => channel(|b, s| b + s - 2.0 * b * s),
            Self::Hue => set_lum(set_sat(s, sat(b)), lum(b)),
            Self::Saturation => set_lum(set_sat(b, sat(s)), lum(b)),
            Self::Color => set_lum(s, lum(b)),
            Self::Luminosity => set_lum(b, lum(s)),
        }
    }
}

fn screen(b: f32, s: f32) -> f32 {
    b + s - b * s
}

fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 { b * 2.0 * s } else { screen(b, 2.0 * s - 1.0) }
}
