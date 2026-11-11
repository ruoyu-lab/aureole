//! 场景数据：一帧要画什么。纯数据和数学，不依赖 GPU。
//!
//! 主线程把元素树画成这里的数据（显示列表、层表、图集更新），交给渲染线程；渲染器（`aureole-render`）
//! 只认识这些数据，负责把它们画成像素。
//!
//! - [`Quad`]：一个图元（圆角矩形、字形、路径、图片），[`Color`] 是它用的 sRGB 颜色；
//!   广色域、比 SDR 白亮的颜色是 [`WideColor`]（经渐变的编码画出来）
//! - [`DisplayList`]：一帧的全部内容：图元、裁剪（[`Clip`]）、分组（[`Group`]）、特效（[`QuadEffect`]）、渐变（[`GradientPaint`]）；
//!   分组可以带滤镜（[`ColorMatrix`]）、遮罩（[`GroupMask`]）、混合模式（[`BlendMode`]）
//! - [`LayerTransform`]：合成层的变换和透明度，每帧单独更新，层内图元不用重新上传
//! - [`GlyphAtlas`]、[`AtlasUpdate`]、[`ImageUpdate`]：图集的 CPU 侧装箱，和交给 GPU 侧的更新
//! - [`Viewport`]：渲染目标的大小和缩放；[`max_texture_size`]：设备的纹理边长上限，装箱时不超过它
//! - [`ShaderFeatures`]、[`FeatureRange`]：哪些图元要用到着色器里少见的几段代码（[`DisplayList::feature_ranges`]），
//!   渲染器按区间给它们选管线变体
//! - [`Angle`]：带单位的角度（[`deg`]、[`rad`]、[`turns`]），颜色的色相、旋转、渐变的方向都用它；
//!   [`ms`]、[`secs`] 写时长（`std::time::Duration`）
//!
//! ```
//! use aureole_scene::{Color, DisplayList, Quad};
//!
//! let card = Quad::new(16.0, 16.0, 120.0, 80.0)
//!     .radius(12.0)
//!     .fill(Color::hex(0x0a84ff))
//!     .shadow(0.0, 4.0, 12.0, Color::BLACK.with_alpha(0.3));
//! let list = DisplayList::new(vec![card]);
//! assert_eq!(list.quads.len(), 1);
//! ```

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod angle;
mod atlas;
mod color;
mod display_list;
mod filter;
mod gradient;
mod image;
mod limits;
mod quad;
mod shader_feature;
mod time;
mod viewport;

pub use angle::{Angle, Number, deg, rad, turns};
pub use atlas::{AtlasKind, AtlasUpdate, GlyphAtlas, MAX_ATLAS_SIZE, max_atlas_size};
pub use color::{BT2020_TO_SRGB, Color, P3_TO_SRGB, SRGB_TO_P3, WideColor, decode_srgb, display_peak, encode_srgb, multiply_matrix3, tone_map};
pub use display_list::{DisplayList, EMPTY_BOUNDS, Group, GroupMask, QuadEffect};
pub use filter::{BlendMode, ColorMatrix};
pub use gradient::{ColorSpace, GradientPaint, GradientShape};
pub use image::{EXTERNAL_TEXTURE_BASE, IMAGE_ATLAS_ALIGN, IMAGE_ATLAS_LEVELS, ImageData, ImageEncoding, ImageLevel, ImageUpdate};
pub use limits::{max_texture_size, set_max_texture_size};
pub use quad::{Clip, LayerTransform, Quad};
pub use shader_feature::{FeatureRange, ShaderFeatures};
pub use time::{ms, secs};
pub use viewport::Viewport;
