//! 图片的像素和图片纹理的更新：小图放进带 mip 的图片图集，大图各自一张带完整 mip 链的纹理。
//! 广色域、HDR 的图片是半精度浮点的像素（[`ImageEncoding::Float16`]），总是单独一张纹理。
//!
//! 像素在 CPU 侧（后台线程）已经预乘、生成好各级 mip。和字形图集一样，CPU 侧决定放在哪，
//! GPU 侧（渲染器）按 [`ImageUpdate`] 建纹理、上传。

use std::fmt;
use std::sync::Arc;

/// 图片图集的 mip 级数（0..=4）。图集里的图片按 [`IMAGE_ATLAS_ALIGN`] 对齐、尺寸补齐到它的倍数，
/// 所以每一级 mip 里每张图都正好占整数个纹素，缩小时不会混进旁边的图
pub const IMAGE_ATLAS_LEVELS: u32 = 5;
/// 图片图集里图片的对齐和尺寸补齐粒度，像素：最小一级 mip 正好是 1 个纹素
pub const IMAGE_ATLAS_ALIGN: u32 = 1 << (IMAGE_ATLAS_LEVELS - 1);

/// 外部纹理（框架以外的代码自己画的 GPU 纹理，渲染器的 `ExternalTexture`）的编号从这里起，
/// 和图片系统分配的单独纹理编号（从 1 往上数）分开。显示列表里照样用 [`crate::QuadEffect::Image`] 引用它
pub const EXTERNAL_TEXTURE_BASE: u32 = 1 << 31;

/// 图片像素的存法
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ImageEncoding {
    /// RGBA8，sRGB 编码的预乘颜色（渲染器的 `IMAGE_FORMAT`）：绝大多数图片，小图可以放进图集
    #[default]
    Srgb8,
    /// RGBA 半精度浮点（渲染器的 `IMAGE_FLOAT_FORMAT`），线性的预乘颜色，sRGB 原色、扩展范围：带色彩配置的
    /// 广色域图片（sRGB 色域外的分量小于 0）、HDR 图片（比 SDR 白亮的分量大于 1）。总是单独一张纹理
    Float16 {
        /// 最亮的分量（SDR 白的倍数）：比显示器此刻的余量亮时，着色器按 [`crate::tone_map`] 压缩
        peak: f32,
    },
}

impl ImageEncoding {
    /// 每个像素几个字节
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Srgb8 => 4,
            Self::Float16 { .. } => 8,
        }
    }
}

/// 解码好的图片：各级 mip，预乘颜色，逐行紧密排列（像素的存法见 [`ImageEncoding`]）
pub struct ImageData {
    /// 图片内容的像素大小（第 0 级里左上角这一块；右边和下边可能有补齐用的边缘像素）
    pub size: [u32; 2],
    /// 第 0 级是原图（可能补齐过），之后每级减半
    pub levels: Vec<ImageLevel>,
    /// 像素的存法：8 位 sRGB（默认）或半精度浮点
    pub encoding: ImageEncoding,
}

/// 一级 mip 的像素
pub struct ImageLevel {
    /// 宽，像素
    pub width: u32,
    /// 高，像素
    pub height: u32,
    /// 预乘像素，逐行紧密排列：RGBA8（[`ImageEncoding::Srgb8`]），
    /// 或者 RGBA 各一个半精度浮点的位模式、本机字节序（[`ImageEncoding::Float16`]）
    pub pixels: Vec<u8>,
}

impl ImageData {
    /// 存储大小（第 0 级）
    pub fn storage(&self) -> [u32; 2] {
        self.levels.first().map_or([0, 0], |l| [l.width, l.height])
    }

    /// 各级像素一共占多少字节
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(|l| l.pixels.len()).sum()
    }
}

impl fmt::Debug for ImageData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageData")
            .field("size", &self.size)
            .field("storage", &self.storage())
            .field("levels", &self.levels.len())
            .field("encoding", &self.encoding)
            .finish()
    }
}

/// CPU 侧交给 GPU 侧的图片纹理更新，经 [`crate::AtlasUpdate::Image`] 和字形更新一起按顺序应用
#[derive(Clone, Debug)]
pub enum ImageUpdate {
    /// 重建图片图集并清空，边长 `size`
    ResetAtlas {
        /// 新的边长，像素
        size: u32,
    },
    /// 把一张图（含各级 mip）放到图集的 (x, y)，x、y 是 [`IMAGE_ATLAS_ALIGN`] 的倍数
    Atlas {
        /// 左上角横坐标，像素
        x: u32,
        /// 左上角纵坐标，像素
        y: u32,
        /// 图片
        image: Arc<ImageData>,
    },
    /// 给一张大图建单独的纹理，`id` 由 CPU 侧分配
    Create {
        /// 纹理编号
        id: u32,
        /// 图片
        image: Arc<ImageData>,
    },
    /// 把新的内容（大小、级数和建纹理时一样）写进已有的单独纹理：动图换帧时用，不重建纹理
    Write {
        /// 纹理编号
        id: u32,
        /// 图片
        image: Arc<ImageData>,
    },
    /// 释放单独的纹理
    Release {
        /// 纹理编号
        id: u32,
    },
}
