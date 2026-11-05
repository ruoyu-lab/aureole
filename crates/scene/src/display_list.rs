use std::ops::Range;
use std::sync::Arc;

use crate::{BlendMode, Clip, ColorMatrix, Quad};

/// 一帧要画的全部内容：图元，加上裁剪、分组和特效。
///
/// 没有分组和特效时，所有图元一次 draw call 画完。图元按前序遍历排列
/// （父元素在子元素前面），所以一个元素连同子孙的图元总是连续的一段，
/// 分组和特效都用下标引用它们。
#[derive(Clone, Debug, Default)]
pub struct DisplayList {
    /// 全部图元，按前序遍历排列
    pub quads: Vec<Quad>,
    /// 裁剪区域，图元的 `clip` 是这里的下标 + 1
    pub clips: Vec<Clip>,
    /// 每个合成层（下标同 [`crate::LayerTransform`]）自己的图元在层坐标里的外接矩形
    /// `[x0, y0, x1, y1]`，用来算分组在屏幕上的范围。只有分组里的层需要；空的用 [`EMPTY_BOUNDS`]
    pub layer_bounds: Vec<[f32; 4]>,
    /// 分组，按前序排列：先画到离屏纹理，再整体合成上屏
    pub groups: Vec<Group>,
    /// 特效图元，按图元下标排列
    pub quad_effects: Vec<QuadEffect>,
    /// 自定义着色器的源码（WGSL），[`QuadEffect::Shader`] 引用这里的下标
    pub shaders: Vec<Arc<str>>,
    /// 有随时间变化的自定义着色器：画面里有它们时需要每帧重画
    pub animated: bool,
    /// 渐变的编码（见 [`DisplayList::push_gradient`]），图元的 `gradient` 是这里的下标
    pub gradients: Vec<[f32; 4]>,
    /// 有扩展范围的内容（sRGB 色域外、比 SDR 白亮的颜色，浮点的图片）：窗口在支持的显示器上
    /// 换成扩展范围的表面才显示得出来（渲染器的 `surface_format`）。渲染器只在它为真时用编译了
    /// "放进目标能显示的范围"的管线，所以引用了浮点纹理（[`crate::ImageEncoding::Float16`]）时要设上
    /// （[`DisplayList::push_gradient`] 遇到扩展范围的渐变时自己设）
    pub extended: bool,
}

/// 空的外接矩形：和任何矩形取并集都得到那个矩形
pub const EMPTY_BOUNDS: [f32; 4] = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];

/// 一个分组：元素连同子孙先画进离屏纹理，再按分组透明度（和滤镜、遮罩、混合模式）合成。
/// 这样半透明的元素里，重叠的子元素不会互相透出来。
///
/// 有 `key` 的分组画好的纹理跨帧缓存：内容（组内的图元、裁剪、渐变、组内各层相对这个分组的变换）没变时，
/// 只有这个分组的变换、透明度、滤镜在变的帧直接拿缓存的纹理合成，不重画组内内容
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    /// 组内的图元（含嵌套分组的、遮罩的）
    pub quads: Range<u32>,
    /// 组内的合成层；第一个就是这个分组自己的层，它的 `group_opacity` 是合成用的透明度
    pub layers: Range<u32>,
    /// 高斯模糊的标准差，逻辑像素，0 表示不模糊
    pub blur: f32,
    /// 合成上屏时的裁剪（祖先的裁剪区域）
    pub clip: u32,
    /// 稳定的标识（同一个元素每帧相同），用来跨帧缓存画好的纹理；0 表示不缓存
    pub key: u64,
    /// 合成时的颜色矩阵（[`DisplayList::push_filter`] 返回的下标），0 表示没有
    pub filter: u32,
    /// 和后面已经画好的内容怎么混合
    pub blend: BlendMode,
    /// 遮罩：组内最后一段图元画进另一张纹理，合成时乘它的透明度（或明度）
    pub mask: Option<GroupMask>,
    /// 不透明、没有效果也画进离屏纹理（缓存成纹理）
    pub offscreen: bool,
}

/// 分组的遮罩：组内图元的最后一段（`quads`，从 `first_layer` 这一层起的合成层）只画进遮罩纹理，不当内容画
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupMask {
    /// 遮罩的图元，在分组的图元范围的末尾
    pub quads: Range<u32>,
    /// 遮罩的第一个合成层（之前的是内容的）
    pub first_layer: u32,
    /// 用明度（而不是透明度）当遮罩
    pub luminance: bool,
}

/// 特效图元：画到它时要断开 draw call 单独处理
#[derive(Clone, Debug, PartialEq)]
pub enum QuadEffect {
    /// 背景模糊：`quad`（种类 [`Quad::BACKDROP`]）的形状里填充它后面已经画好的内容，
    /// 模糊（标准差，逻辑像素）并调整饱和度（1 不变）
    Backdrop {
        /// 图元下标
        quad: u32,
        /// 模糊的标准差，逻辑像素
        blur: f32,
        /// 饱和度倍数，1 不变
        saturation: f32,
    },
    /// 自定义着色器：`quad`（种类 [`Quad::CUSTOM`]）由 `shaders[shader]` 着色
    Shader {
        /// 图元下标
        quad: u32,
        /// [`DisplayList::shaders`] 的下标
        shader: u32,
    },
    /// 大图：`quad`（种类 [`Quad::IMAGE_TEXTURE`]）采样单独的纹理 `texture`（[`crate::ImageUpdate::Create`] 的编号）
    Image {
        /// 图元下标
        quad: u32,
        /// 纹理编号
        texture: u32,
    },
}

impl QuadEffect {
    /// 特效所在的图元下标
    pub fn quad(&self) -> u32 {
        match *self {
            Self::Backdrop { quad, .. } | Self::Shader { quad, .. } | Self::Image { quad, .. } => quad,
        }
    }
}

impl DisplayList {
    /// 只有图元（没有裁剪、分组、特效）
    pub fn new(quads: Vec<Quad>) -> Self {
        Self { quads, ..Default::default() }
    }

    /// 清空，留着各张表的容量：上一份列表拿回来再写一遍时不用重新分配
    pub fn clear(&mut self) {
        self.quads.clear();
        self.clips.clear();
        self.layer_bounds.clear();
        self.groups.clear();
        self.quad_effects.clear();
        self.shaders.clear();
        self.animated = false;
        self.gradients.clear();
        self.extended = false;
    }

    /// 放入一个颜色矩阵（滤镜），返回下标：分组的 [`Group::filter`]；图片、彩色字形的 `gradient`
    /// （这两种图元不用渐变，这一项给滤镜用）
    pub fn push_filter(&mut self, matrix: &ColorMatrix) -> u32 {
        // 下标 0 表示没有，放一个占位（和渐变共用）
        if self.gradients.is_empty() {
            self.gradients.push([0.0; 4]);
        }
        let index = self.gradients.len() as u32;
        self.gradients.extend(matrix.rows());
        index
    }
}
