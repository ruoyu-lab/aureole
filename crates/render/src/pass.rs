//! 离屏效果的通道。
//!
//! 分组的效果（模糊、滤镜、遮罩、混合模式）各实现一个 [`GroupPass`]，特效图元（背景模糊、自定义着色器、
//! 单独纹理的大图）各实现一个 [`QuadEffectPass`]。规划只问它们要什么输入（[`PassFeatures`]：要不要离屏、范围外扩多少、
//! 要不要遮罩纹理、要不要读后面的内容、合成时用不用合成管线），编码器只在固定的几个时机调用它们：
//! 分组的内容画好以后（[`GroupPass::encode`]）、合成分组时（[`GroupPass::composite`]）、画到特效图元时（[`QuadEffectPass::encode`]）。
//!
//! 新加一种离屏效果：写一个实现，登记进 [`GROUP_PASSES`]（或 [`quad_effect_pass`]），不改编码器的循环。
//! 换显示列表时每个分组用到哪几种效果算一次（[`PassSet`]），每帧不再逐个问；没有分组时不会调用到这里。

use scene::ShaderFeatures;

use crate::encoder::Encoder;
use crate::pool::Target;
use crate::{BlendMode, Group, Quad, QuadEffect};

/// 一种效果要规划器准备什么
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PassFeatures {
    /// 要画进离屏纹理（不能直接画在原处）
    pub offscreen: bool,
    /// 范围向外扩多少逻辑像素（模糊的光晕）
    pub margin: f32,
    /// 要读后面已经画好的内容：在这一步断开渲染通道，拷出后面的内容
    pub backdrop: bool,
    /// 要组内最后一段图元单独画成的遮罩纹理
    pub mask: bool,
    /// 合成时用合成管线（`fs_composite`），不是普通的分组合成
    pub composite: bool,
}

impl PassFeatures {
    /// 什么都不要：这个分组没用这种效果
    pub const NONE: Self = Self { offscreen: false, margin: 0.0, backdrop: false, mask: false, composite: false };

    /// 两种效果合在一起要的
    pub fn union(self, other: Self) -> Self {
        Self {
            offscreen: self.offscreen || other.offscreen,
            margin: self.margin.max(other.margin),
            backdrop: self.backdrop || other.backdrop,
            mask: self.mask || other.mask,
            composite: self.composite || other.composite,
        }
    }
}

/// 合成一个分组时交给效果的东西（内容的纹理已经由编码器绑定好）
pub(crate) struct CompositeInput<'a> {
    /// 遮罩的纹理
    pub mask: Option<&'a Target>,
    /// 拷出来的后面的内容，左上角在屏幕上的位置（物理像素）；要读后面的内容时有
    pub backdrop: Option<[f32; 2]>,
}

/// 分组的一种效果
pub(crate) trait GroupPass: Sync {
    /// 这种效果对这个分组要什么；分组没用这种效果时是 [`PassFeatures::NONE`]
    fn features(&self, group: &Group) -> PassFeatures;

    /// 分组的内容画进离屏纹理以后加工它（模糊），返回加工后的纹理；不加工的原样返回
    fn encode(&self, _encoder: &mut Encoder, _group: &Group, input: Target) -> Target {
        input
    }

    /// 合成这个分组时，把这种效果的参数写进合成图元（见着色器里 `fs_composite` 的说明）
    fn composite(&self, _group: &Group, _input: &CompositeInput, _quad: &mut Quad) {}
}

/// 特效图元的一种：画到它时断开普通图元的绘制，单独处理
pub(crate) trait QuadEffectPass: Sync {
    /// 这种特效要什么（会不会读后面的内容）
    fn features(&self, effect: &QuadEffect) -> PassFeatures;

    /// 画这个特效图元。`quad` 是它的图元，`features` 是它要的着色器特性
    fn encode(&self, encoder: &mut Encoder, effect: &QuadEffect, quad: &Quad, features: ShaderFeatures);
}

/// 内置的分组效果。编码时按这个顺序调用：先模糊，再在合成时套上滤镜、遮罩、混合模式
pub(crate) static GROUP_PASSES: [&dyn GroupPass; 4] = [&Blur, &Filter, &Mask, &Blend];
const _: () = assert!(GROUP_PASSES.len() <= u8::BITS as usize, "PassSet 是 8 位的");

/// 特效图元用哪个通道
pub(crate) fn quad_effect_pass(effect: &QuadEffect) -> &'static dyn QuadEffectPass {
    match effect {
        QuadEffect::Backdrop { .. } => &BackdropBlur,
        QuadEffect::Shader { .. } => &CustomShader,
        QuadEffect::Image { .. } => &LargeImage,
    }
}

/// 一个分组用到的效果：[`GROUP_PASSES`] 的下标的位集合
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PassSet(u8);

impl PassSet {
    /// 分组用到哪几种效果，和它们合在一起要的
    pub fn of(group: &Group) -> (Self, PassFeatures) {
        let mut set = 0;
        let mut features = PassFeatures::NONE;
        for (i, pass) in GROUP_PASSES.iter().enumerate() {
            let wanted = pass.features(group);
            if wanted != PassFeatures::NONE {
                set |= 1 << i;
                features = features.union(wanted);
            }
        }
        (Self(set), features)
    }

    /// 用到的效果，按 [`GROUP_PASSES`] 的顺序
    pub fn iter(self) -> impl Iterator<Item = &'static dyn GroupPass> {
        GROUP_PASSES.iter().enumerate().filter(move |(i, _)| self.0 & (1 << i) != 0).map(|(_, pass)| *pass)
    }
}

/// 模糊：内容画好以后逐级对半缩小，再横竖各模糊一遍；范围按 3σ 外扩，边缘的光晕不被截断
struct Blur;

impl GroupPass for Blur {
    fn features(&self, group: &Group) -> PassFeatures {
        if group.blur > 0.0 { PassFeatures { offscreen: true, margin: 3.0 * group.blur, ..PassFeatures::NONE } } else { PassFeatures::NONE }
    }

    fn encode(&self, encoder: &mut Encoder, group: &Group, input: Target) -> Target {
        let sigma = group.blur * encoder.scale_factor();
        encoder.blur(&input, sigma)
    }
}

/// 滤镜：合成时按颜色矩阵改颜色（`gradient` 是矩阵在渐变表里的下标）
struct Filter;

impl GroupPass for Filter {
    fn features(&self, group: &Group) -> PassFeatures {
        if group.filter != 0 { PassFeatures { offscreen: true, composite: true, ..PassFeatures::NONE } } else { PassFeatures::NONE }
    }

    fn composite(&self, group: &Group, _input: &CompositeInput, quad: &mut Quad) {
        quad.gradient = group.filter;
    }
}

/// 遮罩：组内最后一段图元画进另一张纹理，合成时乘它的透明度（或明度）
struct Mask;

impl GroupPass for Mask {
    fn features(&self, group: &Group) -> PassFeatures {
        match group.mask {
            Some(_) => PassFeatures { offscreen: true, composite: true, mask: true, ..PassFeatures::NONE },
            None => PassFeatures::NONE,
        }
    }

    fn composite(&self, group: &Group, input: &CompositeInput, quad: &mut Quad) {
        let Some(mask) = &group.mask else { return };
        // 颜色的第二个字节是遮罩方式：1 按透明度，2 按明度
        quad.fill.0[1] = if mask.luminance { 2 } else { 1 };
        if let Some(texture) = input.mask {
            [quad.shadow_blur, quad.border_width] = texture.uv_extent();
        }
    }
}

/// 混合模式：拷出合成范围后面已经画好的内容，按混合模式的定义和它混合
struct Blend;

impl GroupPass for Blend {
    fn features(&self, group: &Group) -> PassFeatures {
        if group.blend != BlendMode::Normal {
            PassFeatures { offscreen: true, composite: true, backdrop: true, ..PassFeatures::NONE }
        } else {
            PassFeatures::NONE
        }
    }

    fn composite(&self, group: &Group, input: &CompositeInput, quad: &mut Quad) {
        // 颜色的第一个字节是混合模式
        quad.fill.0[0] = group.blend as u8;
        if let Some(origin) = input.backdrop {
            quad.shadow_offset = origin;
        }
    }
}

/// 背景模糊：拷出形状后面已经画好的内容（外扩 3σ），模糊、调饱和度，填进形状
struct BackdropBlur;

impl QuadEffectPass for BackdropBlur {
    fn features(&self, effect: &QuadEffect) -> PassFeatures {
        let blur = if let QuadEffect::Backdrop { blur, .. } = *effect { blur } else { 0.0 };
        PassFeatures { backdrop: true, margin: 3.0 * blur, ..PassFeatures::NONE }
    }

    fn encode(&self, encoder: &mut Encoder, effect: &QuadEffect, quad: &Quad, features: ShaderFeatures) {
        let QuadEffect::Backdrop { blur, saturation, .. } = *effect else { return };
        let Some(region) = encoder.backdrop_region(quad, blur) else { return };
        let Some(behind) = encoder.copy_behind(region) else { return };
        let sigma = blur * encoder.scale_factor();
        let blurred = if sigma > 0.0 { encoder.blur(&behind, sigma) } else { behind };
        let mut fill = *quad;
        fill.uv = [region.x as f32, region.y as f32, region.w as f32, region.h as f32];
        fill.shadow_offset = blurred.uv_extent();
        fill.border_width = saturation;
        encoder.draw_extra(fill, &blurred.effect, features);
    }
}

/// 自定义着色器：换成它自己的管线画这一个图元
struct CustomShader;

impl QuadEffectPass for CustomShader {
    fn features(&self, _effect: &QuadEffect) -> PassFeatures {
        PassFeatures::NONE
    }

    fn encode(&self, encoder: &mut Encoder, effect: &QuadEffect, _quad: &Quad, features: ShaderFeatures) {
        if let QuadEffect::Shader { quad, shader } = *effect {
            encoder.draw_custom(quad, shader, features);
        }
    }
}

/// 单独纹理的大图：换成它自己的纹理画这一个图元
struct LargeImage;

impl QuadEffectPass for LargeImage {
    fn features(&self, _effect: &QuadEffect) -> PassFeatures {
        PassFeatures::NONE
    }

    fn encode(&self, encoder: &mut Encoder, effect: &QuadEffect, _quad: &Quad, features: ShaderFeatures) {
        if let QuadEffect::Image { quad, texture } = *effect {
            encoder.draw_image(quad, texture, features);
        }
    }
}
