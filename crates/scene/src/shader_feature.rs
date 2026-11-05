//! 着色器特性：图元着色时只有少数图元用得到的几段代码（透视的裁剪、扩展范围的颜色、单独的投影和渐变边框）。
//!
//! 渲染器的图元着色器把这几段做成管线常量，只编进用得到它们的管线：多出来的代码即使不执行，也让片元着色器多占寄存器、
//! 整屏变慢。一帧里只有几个图元用到某种特性时，渲染器按绘制区间选管线，只有那几段图元走带这种特性的管线。
//! 哪些图元要哪些特性，由这里按显示列表算出来（[`DisplayList::feature_ranges`]），是纯数据，不认识 GPU。

use std::ops::{BitOr, BitOrAssign, Range};

use crate::{Clip, DisplayList, Quad};

/// 一组着色器特性（位集合）。渲染器按它选管线变体：一种目标格式上最多 2 的 [`ShaderFeatures::COUNT`] 次方种
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ShaderFeatures(u8);

impl ShaderFeatures {
    /// 没有特性：平常的图元
    pub const NONE: Self = Self(0);
    /// 裁剪链上有可能带透视的裁剪（[`Clip::PROJECTIVE`]）：按投影算裁剪
    pub const PROJECTIVE_CLIPS: Self = Self(1);
    /// 扩展范围的内容（广色域、HDR 的渐变，浮点纹理的大图）：放进这一帧的目标能显示的范围
    pub const EXTENDED_COLORS: Self = Self(1 << 1);
    /// 单独的投影、内阴影、渐变边框（[`Quad::is_rich`]）
    pub const RICH_SHAPES: Self = Self(1 << 2);
    /// 全部特性
    pub const ALL: Self = Self((1 << Self::COUNT) - 1);
    /// 特性的种数
    pub const COUNT: u32 = 3;

    /// 位的值（第 0 位是 [`Self::PROJECTIVE_CLIPS`]，依次往上）
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// 从位的值得到；不认识的位丢掉
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & Self::ALL.0)
    }

    /// 没有任何特性
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 包含 `other` 的全部特性
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 两组的并
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 两组的交
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl BitOr for ShaderFeatures {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

impl BitOrAssign for ShaderFeatures {
    fn bitor_assign(&mut self, other: Self) {
        *self = self.union(other);
    }
}

/// 一段连续的图元和它们要的着色器特性
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureRange {
    /// 图元的下标范围
    pub quads: Range<u32>,
    /// 这一段的图元要的特性（不为空）
    pub features: ShaderFeatures,
}

impl DisplayList {
    /// 要用到着色器特性的图元，按区间列出：按图元下标排列、互不重叠，相邻而特性相同的合成一段；
    /// 不要特性的图元不在里面，所以平常的界面返回空的。渲染器按它给每段绘制选管线变体。
    ///
    /// 每个图元要的特性：
    /// - [`ShaderFeatures::PROJECTIVE_CLIPS`]：裁剪链上有带 [`Clip::PROJECTIVE`] 的裁剪；
    /// - [`ShaderFeatures::RICH_SHAPES`]：[`Quad::is_rich`]；
    /// - [`ShaderFeatures::EXTENDED_COLORS`]：列表标了 [`DisplayList::extended`]，图元的填充或边框是有扩展范围色标的渐变，
    ///   或者是单独纹理的大图（可能是浮点纹理）
    pub fn feature_ranges(&self) -> Vec<FeatureRange> {
        let clips = self.projective_clips();
        let mut ranges: Vec<FeatureRange> = Vec::new();
        for (i, quad) in self.quads.iter().enumerate() {
            let features = self.quad_features(quad, &clips);
            if features.is_empty() {
                continue;
            }
            let i = i as u32;
            match ranges.last_mut() {
                Some(last) if last.quads.end == i && last.features == features => last.quads.end = i + 1,
                _ => ranges.push(FeatureRange { quads: i..i + 1, features }),
            }
        }
        ranges
    }

    /// 裁剪链（图元、分组的裁剪下标：[`DisplayList::clips`] 的下标 + 1，0 表示不裁剪）要的特性：
    /// 链上有带 [`Clip::PROJECTIVE`] 的裁剪时是 [`ShaderFeatures::PROJECTIVE_CLIPS`]
    pub fn clip_features(&self, clip: u32) -> ShaderFeatures {
        let mut index = clip;
        // 链上的父裁剪下标比自己小（不是的话是坏数据，到此为止）
        while index != 0 {
            let Some(c) = self.clips.get(index as usize - 1) else { break };
            if c.layer & Clip::PROJECTIVE != 0 {
                return ShaderFeatures::PROJECTIVE_CLIPS;
            }
            if c.parent >= index {
                break;
            }
            index = c.parent;
        }
        ShaderFeatures::NONE
    }

    /// 每个裁剪下标（0 是"不裁剪"）的链上有没有带透视的裁剪。父裁剪的下标比自己小，按顺序一遍算完
    fn projective_clips(&self) -> Vec<bool> {
        let mut table = Vec::with_capacity(self.clips.len() + 1);
        table.push(false);
        for (i, clip) in self.clips.iter().enumerate() {
            let parent = clip.parent as usize <= i && table[clip.parent as usize];
            table.push(clip.layer & Clip::PROJECTIVE != 0 || parent);
        }
        table
    }

    fn quad_features(&self, quad: &Quad, projective_clips: &[bool]) -> ShaderFeatures {
        let mut features = ShaderFeatures::NONE;
        if projective_clips.get(quad.clip as usize).copied().unwrap_or(false) {
            features |= ShaderFeatures::PROJECTIVE_CLIPS;
        }
        if quad.is_rich() {
            features |= ShaderFeatures::RICH_SHAPES;
        }
        if self.extended {
            // 采样纹理的图元（彩色字形、图片）的 `gradient` 是滤镜的颜色矩阵，不是渐变
            let samples_texture = matches!(quad.kind, Quad::COLOR_GLYPH | Quad::IMAGE | Quad::IMAGE_TEXTURE);
            let fill = !samples_texture && self.extended_gradient(quad.gradient);
            if quad.kind == Quad::IMAGE_TEXTURE || fill || self.extended_gradient(quad.border_gradient()) {
                features |= ShaderFeatures::EXTENDED_COLORS;
            }
        }
        features
    }

    /// 渐变（[`DisplayList::push_gradient`] 返回的下标，0 表示没有）有扩展范围的色标
    fn extended_gradient(&self, gradient: u32) -> bool {
        // 表头第二项的第三个分量是标志位，16 是"有扩展范围的色标"（见 `push_gradient`）
        gradient != 0 && self.gradients.get(gradient as usize + 1).is_some_and(|info| info[2] as u32 & 16 != 0)
    }
}
