//! GPU 渲染器：SDF 实例化图元。
//!
//! 圆角矩形、边框、阴影、渐变全部在片元着色器里计算，
//! 没有特效时所有图元一次 draw call 画完。裁剪在着色器里逐像素计算；
//! 分组透明度、模糊、滤镜、遮罩、混合模式、背景模糊用离屏纹理，内容没变的分组跨帧缓存画好的纹理；
//! 自定义着色器单独编译管线。
//!
//! - [`Quad`]：一个图元（圆角矩形、字形、路径、图片），[`Color`] 是它用的 sRGB 颜色；
//!   广色域、比 SDR 白亮的颜色是 [`WideColor`]（经渐变的编码画出来）
//! - [`DisplayList`]：一帧的全部内容：图元、裁剪、分组、特效、渐变；分组可以带滤镜（[`ColorMatrix`]）、
//!   遮罩（[`GroupMask`]）、混合模式（[`BlendMode`]）
//! - [`LayerTransform`]：合成层的变换和透明度，每帧单独更新，层内图元不用重新上传
//! - [`GlyphAtlas`]、[`AtlasUpdate`]、[`ImageUpdate`]：CPU 侧装箱，GPU 侧按更新维护纹理
//! - [`Renderer`]：把显示列表画到纹理上；[`Gpu`] 是共用的设备（按 [`GpuPreference`] 和退路选适配器，见 [`GpuError`]），[`render_to_image`] 离屏画一帧，
//!   [`Offscreen`] 离屏连续画多帧（分组缓存照常工作）
//! - [`GpuMemory`]：渲染器分配的显存（创建时记账、释放时减掉），[`gpu_memory`] 是本进程的合计
//! - [`surface_format`]：窗口表面用 8 位 sRGB 还是扩展范围（广色域、HDR）的格式；[`tone_map`] 是 HDR 的色调映射
//!
//! 这个 crate 只认识图元，不知道元素和布局：`aureole` 在主线程生成显示列表和图集更新，
//! 交给渲染线程上的 [`Renderer`] 绘制。底层用 wgpu。图元、显示列表、颜色、图集更新这些纯数据在
//! `aureole-scene` 里（不依赖 GPU），这里照原来的路径转出。
//!
//! ```no_run
//! use aureole_render::{Color, DisplayList, Gpu, Quad, Viewport, instance, render_to_image};
//!
//! let gpu = Gpu::new(&instance(), None);
//! let card = Quad::new(16.0, 16.0, 120.0, 80.0)
//!     .radius(12.0)
//!     .fill(Color::hex(0x0a84ff))
//!     .shadow(0.0, 4.0, 12.0, Color::BLACK.with_alpha(0.3));
//! let list = DisplayList::new(vec![card]);
//! // 逻辑像素 152x112，2 倍缩放
//! let viewport = Viewport { width: 304, height: 224, scale_factor: 2.0 };
//! let pixels = render_to_image(&gpu, &list, &[], &[], viewport, Color::WHITE, std::time::Duration::ZERO);
//! assert_eq!(pixels.len(), 304 * 224 * 4);
//! ```

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod cache;
mod context;
mod disk_cache;
mod encoder;
mod external;
mod image;
mod memory;
mod offscreen;
mod output;
mod pass;
mod pipeline;
mod plan;
mod pool;
mod presentation;
mod renderer;
mod timer;
mod transparent;

// 要画什么的数据在 scene 里，照原来的路径转出：`renderer::Quad`、`renderer::Color` 等照旧能用
pub use scene::{
    AtlasKind, AtlasUpdate, BT2020_TO_SRGB, BlendMode, Clip, Color, ColorMatrix, ColorSpace, DisplayList, EMPTY_BOUNDS, GlyphAtlas,
    GradientPaint, GradientShape, Group, GroupMask, IMAGE_ATLAS_ALIGN, IMAGE_ATLAS_LEVELS, ImageData, ImageEncoding, ImageLevel, ImageUpdate,
    LayerTransform, MAX_ATLAS_SIZE, P3_TO_SRGB, Quad, QuadEffect, SRGB_TO_P3, Viewport, WideColor, decode_srgb, display_peak, encode_srgb,
    max_atlas_size, max_texture_size, multiply_matrix3, tone_map,
};

pub use cache::DEFAULT_CACHE_BUDGET;
pub use context::{
    DisplayGpu, Gpu, GpuError, GpuInfo, GpuKind, GpuPreference, active_adapter, instance, instance_with_all_backends, instance_with_display, only_native_backend,
};
pub use disk_cache::set_pipeline_cache_dir;
pub use external::{ExternalTexture, external_revision};
pub use image::{IMAGE_FLOAT_FORMAT, IMAGE_FORMAT};
pub use memory::{GpuMemory, gpu_memory};
pub use offscreen::{Offscreen, render_to_extended_image, render_to_image};
pub use presentation::{flush_surface_transaction, keep_surface_contents_unscaled, set_surface_present_in_transaction};
pub use output::{EXTENDED_FORMAT, SurfaceOutput, supports_extended, surface_format, transparent_alpha_mode};
pub use renderer::{CacheStats, IDLE_RELEASE, RenderStats, Renderer, cache_stats, set_cache_budget};
pub use timer::GpuTimer;
