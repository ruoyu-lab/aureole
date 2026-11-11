//! 帧渲染器：把显示列表画到目标纹理上。
//!
//! 一帧分两段：先规划（`plan.rs`，纯函数）决定哪些分组离屏、哪些用缓存、每个目标画哪几段、每段用哪种管线变体，
//! 再由编码器（`encoder.rs`）照着计划写命令。这里管的是两段之间的东西：GPU 上的资源（管线、绑定、图集、缓冲区、
//! 纹理池、图片纹理），换显示列表时的上传和整理，以及每帧的提交和统计。
//!
//! 没有分组和特效时，整帧就是一次实例化 draw call。分组（分组透明度、模糊、滤镜、遮罩、混合模式）先由内向外
//! 画进离屏纹理，再作为一个图元合成；内容没变的分组直接用上一次画好的纹理（见 `cache.rs`）。
//! 背景模糊、混合模式要读取已经画好的内容，在那里断开渲染通道，拷出背后的区域再接着画。
//! 离屏纹理来自纹理池，跨帧复用。
//! 小图片在图片图集里，和其他图元同一次 draw call；大图片各有纹理，画到它时换一次绑定。

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytemuck::Zeroable;

use crate::cache::{Cache, DEFAULT_CACHE_BUDGET};
use crate::encoder::{BlurParams, Encoder, Frame, Globals, MAX_BLUR_PASSES, UNIFORM_STRIDE};
use crate::image::Images;
use crate::memory::{self, GpuMemory, Kind, Ledger, Tracked};
use crate::pipeline::{self, Pipelines, QUAD_SHADER, Sources};
use crate::plan::{FrameInfo, ListInfo, RenderPlan, Variants};
use crate::pool::{Pool, PoolLayouts, Target};
use crate::{AtlasKind, AtlasUpdate, Clip, Color, DisplayList, Gpu, GpuTimer, LayerTransform, Quad, Viewport};

/// 一帧的绘制统计（[`Renderer::stats`]）：测试和性能分析用
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderStats {
    /// 渲染通道数（含模糊的通道）
    pub passes: u32,
    /// 绘制调用数
    pub draws: u32,
    /// 画进离屏纹理的分组（重画了内容或遮罩的）
    pub groups_drawn: u32,
    /// 直接用缓存的纹理合成的分组
    pub cache_hits: u32,
    /// 其中分组在动、按此刻的变换近似贴上去的（停下以后重画一次）
    pub cache_approximations: u32,
    /// 这一帧被赶出缓存的分组（超出预算、很久没用）
    pub cache_evictions: u32,
    /// 缓存里的分组数
    pub cache_entries: u32,
    /// 缓存的纹理占用的字节数（和 [`Renderer::gpu_memory`] 的 `cache` 一样）
    pub cache_bytes: u64,
}

/// 所有渲染器的分组缓存加在一起的统计（[`cache_stats`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// 累计：直接用缓存合成的次数
    pub hits: u64,
    /// 累计：画进离屏纹理的次数（没有缓存、内容变了、停下后重画）
    pub draws: u64,
    /// 此刻缓存里的分组数
    pub entries: u64,
    /// 此刻缓存的纹理占用的字节数
    pub bytes: u64,
}

static TOTAL_HITS: AtomicU64 = AtomicU64::new(0);
static TOTAL_DRAWN: AtomicU64 = AtomicU64::new(0);
static TOTAL_ENTRIES: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
static BUDGET: AtomicU64 = AtomicU64::new(DEFAULT_CACHE_BUDGET);

/// 分组缓存和离屏纹理池里的纹理空闲（没有画、没有合成）这么久就释放，归还显存。
/// 按最近用到的时刻算，不按帧数：帧率不同、画面静止不出帧时都一样
pub const IDLE_RELEASE: Duration = Duration::from_secs(2);

/// 所有窗口的分组缓存的统计
pub fn cache_stats() -> CacheStats {
    CacheStats {
        hits: TOTAL_HITS.load(Ordering::Relaxed),
        draws: TOTAL_DRAWN.load(Ordering::Relaxed),
        entries: TOTAL_ENTRIES.load(Ordering::Relaxed),
        bytes: TOTAL_BYTES.load(Ordering::Relaxed),
    }
}

/// 每个渲染器（窗口）的分组缓存最多占多少字节的纹理，默认 64 MB；0 表示不缓存
pub fn set_cache_budget(bytes: u64) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

/// 渲染器在 GPU 上的资源：管线、绑定、图集、缓冲区、纹理池、图片纹理。编码器借用它们写命令
pub(crate) struct Resources {
    pub pipelines: Pipelines,
    layout: wgpu::BindGroupLayout,
    effect_layout: wgpu::BindGroupLayout,
    /// 带滤镜、遮罩、混合模式的分组合成：内容和遮罩两张纹理
    composite_layout: wgpu::BindGroupLayout,
    placeholder: wgpu::TextureView,
    pub bind_group: wgpu::BindGroup,
    pub no_effect: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    mask_atlas: Tracked<wgpu::Texture>,
    color_atlas: Tracked<wgpu::Texture>,
    path_atlas: Tracked<wgpu::Texture>,
    globals: Tracked<wgpu::Buffer>,
    globals_capacity: usize,
    layers: Tracked<wgpu::Buffer>,
    layer_capacity: usize,
    clips: Tracked<wgpu::Buffer>,
    clip_capacity: usize,
    /// 渐变的编码，单位是 vec4
    gradients: Tracked<wgpu::Buffer>,
    gradient_capacity: usize,
    pub instances: Tracked<wgpu::Buffer>,
    capacity: usize,
    pub extras: Tracked<wgpu::Buffer>,
    extras_capacity: usize,
    blur_layout: wgpu::BindGroupLayout,
    blur_params: Tracked<wgpu::Buffer>,
    /// 显存的账：分配的纹理和缓冲区
    ledger: Arc<Ledger>,
    pub pool: Pool,
    /// 图片图集和大图纹理（第 2 组绑定）
    pub images: Images,
    /// 窗口透明时最后一步的管线：第一次用到时建
    transparent: Option<crate::transparent::TransparentOutput>,
}

impl Resources {
    /// 窗口透明时最后一步的管线（换了目标格式时重建）
    pub fn transparent_output(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat) -> &crate::transparent::TransparentOutput {
        self.transparent.take_if(|output| output.format() != format);
        self.transparent.get_or_insert_with(|| crate::transparent::TransparentOutput::new(device, format))
    }

    fn new(device: &wgpu::Device, disk: Option<std::sync::Arc<crate::disk_cache::DiskCache>>) -> Self {
        let module = pipeline::shader_module(device, "quad", wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()));
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage_entry = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let uniform_entry = |binding, visibility, size: usize| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(size as u64),
            },
            count: None,
        };
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let vertex_fragment = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[
                uniform_entry(0, vertex_fragment, size_of::<Globals>()),
                texture_entry(1),
                texture_entry(2),
                sampler_entry(3),
                storage_entry(4, vertex_fragment),
                storage_entry(5, wgpu::ShaderStages::FRAGMENT),
                texture_entry(6),
                storage_entry(7, vertex_fragment),
            ],
        });
        let effect_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad effect"),
            entries: &[texture_entry(0)],
        });
        let ledger = memory::ledger();
        let images = Images::new(device, &ledger);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&layout), Some(&effect_layout), Some(images.layout())],
            immediate_size: 0,
        });
        let composite_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite"),
            entries: &[texture_entry(0), texture_entry(1)],
        });
        let composite_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite"),
            bind_group_layouts: &[Some(&layout), Some(&composite_layout), Some(images.layout()), Some(&effect_layout)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear clamp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let blur_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur"),
            entries: &[texture_entry(0), sampler_entry(1), uniform_entry(2, wgpu::ShaderStages::FRAGMENT, size_of::<BlurParams>())],
        });
        let sources = Sources {
            module,
            blur_module: pipeline::blur_module(device),
            quad_layout: pipeline_layout,
            composite_layout: composite_pipeline_layout,
            blur_layout: device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("blur"),
                bind_group_layouts: &[Some(&blur_layout)],
                immediate_size: 0,
            }),
            disk,
        };
        let blur_params = memory::buffer(
            device,
            &ledger,
            &wgpu::BufferDescriptor {
                label: Some("blur params"),
                size: (MAX_BLUR_PASSES * UNIFORM_STRIDE) as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            },
        );

        // 收到图集的 Reset 之前先用 1x1 的占位纹理
        let mask_atlas = create_texture(device, &ledger, "mask atlas", wgpu::TextureFormat::R8Unorm, 1);
        let color_atlas = create_texture(device, &ledger, "color atlas", wgpu::TextureFormat::Rgba8UnormSrgb, 1);
        let path_atlas = create_texture(device, &ledger, "path atlas", wgpu::TextureFormat::R8Unorm, 1);
        let placeholder = create_texture(device, &ledger, "no effect", wgpu::TextureFormat::Rgba8Unorm, 1).create_view(&Default::default());
        let no_effect = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("no effect"),
            layout: &effect_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&placeholder) }],
        });
        let globals_capacity = 4;
        let globals = create_buffer(device, &ledger, "globals", globals_capacity * UNIFORM_STRIDE, wgpu::BufferUsages::UNIFORM);
        let layer_capacity = 64;
        let layers = create_buffer(device, &ledger, "layers", layer_capacity * size_of::<LayerTransform>(), wgpu::BufferUsages::STORAGE);
        let clip_capacity = 16;
        let clips = create_buffer(device, &ledger, "clips", clip_capacity * size_of::<Clip>(), wgpu::BufferUsages::STORAGE);
        let gradient_capacity = 16;
        let gradients = create_buffer(device, &ledger, "gradients", gradient_capacity * 16, wgpu::BufferUsages::STORAGE);
        let bind_group = create_bind_group(
            device,
            &layout,
            &globals,
            [&mask_atlas, &color_atlas, &path_atlas],
            &sampler,
            [&layers, &clips, &gradients],
        );
        let pipelines = Pipelines::new(sources);
        let capacity = 1024;
        let extras_capacity = 16;
        Self {
            pipelines,
            layout,
            effect_layout,
            composite_layout,
            placeholder,
            bind_group,
            no_effect,
            sampler,
            mask_atlas,
            color_atlas,
            path_atlas,
            globals,
            globals_capacity,
            layers,
            layer_capacity,
            clips,
            clip_capacity,
            gradients,
            gradient_capacity,
            instances: create_buffer(device, &ledger, "quad instances", capacity * size_of::<Quad>(), wgpu::BufferUsages::VERTEX),
            capacity,
            extras: create_buffer(device, &ledger, "extra quads", extras_capacity * size_of::<Quad>(), wgpu::BufferUsages::VERTEX),
            extras_capacity,
            blur_layout,
            blur_params,
            pool: Pool::new(ledger.clone()),
            images,
            transparent: None,
            ledger,
        }
    }

    fn rebind(&mut self, device: &wgpu::Device) {
        self.bind_group = create_bind_group(
            device,
            &self.layout,
            &self.globals,
            [&self.mask_atlas, &self.color_atlas, &self.path_atlas],
            &self.sampler,
            [&self.layers, &self.clips, &self.gradients],
        );
    }

    fn upload_quads(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, quads: &[Quad]) {
        if quads.len() > self.capacity {
            self.capacity = quads.len().next_power_of_two();
            self.instances =
                create_buffer(device, &self.ledger, "quad instances", self.capacity * size_of::<Quad>(), wgpu::BufferUsages::VERTEX);
        }
        if !quads.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(quads));
        }
    }

    /// 每帧数据的缓冲不够大时扩容（`layers` 含合成分组时加的层）
    fn reserve(&mut self, device: &wgpu::Device, targets: usize, extras: usize, layers: usize) {
        if layers > self.layer_capacity {
            self.layer_capacity = layers.next_power_of_two();
            self.layers = create_buffer(device, &self.ledger, "layers", self.layer_capacity * size_of::<LayerTransform>(), wgpu::BufferUsages::STORAGE);
            self.rebind(device);
        }
        if targets > self.globals_capacity {
            self.globals_capacity = targets.next_power_of_two();
            self.globals = create_buffer(device, &self.ledger, "globals", self.globals_capacity * UNIFORM_STRIDE, wgpu::BufferUsages::UNIFORM);
            self.rebind(device);
        }
        if extras > self.extras_capacity {
            self.extras_capacity = extras.next_power_of_two();
            self.extras = create_buffer(device, &self.ledger, "extra quads", self.extras_capacity * size_of::<Quad>(), wgpu::BufferUsages::VERTEX);
        }
    }

    /// 从纹理池借一块至少 `width` x `height` 的纹理
    pub fn acquire(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat, width: u32, height: u32) -> Target {
        let layouts = PoolLayouts {
            effect: &self.effect_layout,
            blur: &self.blur_layout,
            sampler: &self.sampler,
            blur_params: &self.blur_params,
        };
        self.pool.acquire(device, &layouts, format, width, height)
    }

    /// 合成带滤镜、遮罩、混合模式的分组用的绑定：内容和遮罩（没有遮罩时用占位）
    pub fn composite_bind(&self, device: &wgpu::Device, content: &Target, mask: Option<&Target>) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite"),
            layout: &self.composite_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&content.view) },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(mask.map_or(&self.placeholder, |m| &m.view)),
                },
            ],
        })
    }
}

/// 原地修补的图元超过这么多段时合成一段上传（每次上传有固定的开销）
const PATCH_UPLOADS: usize = 16;

/// 把 [`DisplayList`] 画到目标纹理上
pub struct Renderer {
    res: Resources,
    list: ListInfo,
    /// 分组画好的纹理，跨帧复用
    cache: Cache,
    /// 只给这个渲染器的缓存预算（测试用）；`None` 用全局的（[`set_cache_budget`]）
    budget: Option<u64>,
    /// 上一帧有分组按近似的变换用了缓存：停下以后要再画一帧
    refine: bool,
    stats: RenderStats,
    /// 报给全局统计的缓存大小（换算成增减）
    reported: (u64, u64),
    /// 扩展范围的目标上显示器此刻的余量，见 [`Renderer::set_headroom`]
    headroom: f32,
    /// 管线变体怎么选（测试里换成整页选，和按区间选对照）
    variants: Variants,
    /// 最终渲染目标（窗口表面）的格式
    format: wgpu::TextureFormat,
    /// 下一帧画完以后在后台先编译常用的管线，见 [`Renderer::set_precompile`]
    precompile: bool,
    /// 窗口透明，见 [`Renderer::set_transparent_output`]
    transparent: bool,
    /// 外部纹理的登记表上次对到哪（`external.rs`）
    external: crate::external::Seen,
}

impl Renderer {
    /// `format` 是最终渲染目标（窗口表面）的格式。只编译着色器模块、建绑定和缓冲区，一条管线都不编译：
    /// 第一帧用到哪条编哪条
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self::build(device, format, None)
    }

    /// 和 [`Renderer::new`] 一样，编译管线时用这个设备的管线缓存（设了目录、后端支持时，见
    /// [`crate::set_pipeline_cache_dir`]）。窗口的渲染器用它：下次启动不用从头编译
    pub fn from_gpu(gpu: &crate::Gpu, format: wgpu::TextureFormat) -> Self {
        Self::build(&gpu.device, format, gpu.disk.clone())
    }

    fn build(device: &wgpu::Device, format: wgpu::TextureFormat, disk: Option<std::sync::Arc<crate::disk_cache::DiskCache>>) -> Self {
        Self {
            res: Resources::new(device, disk),
            list: ListInfo::default(),
            cache: Cache::default(),
            budget: None,
            refine: false,
            stats: RenderStats::default(),
            reported: (0, 0),
            headroom: 1.0,
            variants: Variants::default(),
            format,
            precompile: false,
            transparent: false,
            external: crate::external::Seen::default(),
        }
    }

    /// 窗口透明（透出系统的窗口材质）：根先画进一张中间纹理，最后换算成系统合成要的样子写进目标。
    /// 系统把 8 位表面的像素当作"sRGB 编码的值，按编码后的值预乘"，渲染器画出来的是线性的值预乘，直接交出去半透明的地方会亮很多。
    /// 不透明的窗口不开：没有中间纹理、没有多的通道。见 `shaders/transparent.wgsl`
    pub fn set_transparent_output(&mut self, transparent: bool) {
        self.transparent = transparent;
    }

    /// 打开时，下一帧画完以后在后台线程上把常用的管线变体（投影和内阴影、分组的内容和合成、模糊）先编译好，
    /// 第一次出现它们的那一帧不用当场编译（软件渲染、要从着色器源码现编的驱动上一条要几十到几百毫秒）。
    /// 窗口的渲染器打开它；离屏渲染、测试里用不着
    pub fn set_precompile(&mut self, precompile: bool) {
        self.precompile = precompile;
    }

    /// 当场把这几条管线编译好（试一个能力打了折扣的设备能不能用时）
    pub(crate) fn compile(&mut self, device: &wgpu::Device, keys: &[pipeline::Key]) {
        for &(entry, format, features) in keys {
            self.res.pipelines.get(device, entry, format, features);
        }
    }

    /// 这个渲染器此刻分配的显存（纹理和缓冲区，不含交换链）
    pub fn gpu_memory(&self) -> GpuMemory {
        self.res.ledger.read()
    }

    /// 上一帧的绘制统计
    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// 只给这个渲染器的分组缓存预算（字节，0 表示不缓存）；`None` 用全局的（[`set_cache_budget`]）
    pub fn set_cache_budget(&mut self, bytes: Option<u64>) {
        self.budget = bytes;
    }

    /// 上一帧有分组在动、按近似的变换用了缓存的纹理：画面停下以后应该再画一帧（按此刻的变换重画那些分组，回到逐像素清晰）
    pub fn needs_refine(&self) -> bool {
        self.refine
    }

    /// 分组缓存全部作废
    pub fn clear_cache(&mut self) {
        self.cache.clear(&mut self.res.pool);
    }

    /// 显示器此刻比 SDR 白还能亮几倍（EDR 的余量，1 表示没有）。只对扩展范围的目标（[`crate::EXTENDED_FORMAT`]）
    /// 有用：比它亮的颜色、HDR 图片在着色器里压到它以内；8 位的目标总是按 1 算
    pub fn set_headroom(&mut self, headroom: f32) {
        self.headroom = if headroom.is_finite() { headroom.max(1.0) } else { 1.0 };
    }

    /// 按顺序应用字形图集的更新：重建纹理或上传新字形
    pub fn apply_atlas(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, updates: &[AtlasUpdate]) {
        let mut rebind = false;
        for update in updates {
            match update {
                &AtlasUpdate::Reset { kind, size } => {
                    let res = &mut self.res;
                    match kind {
                        AtlasKind::Mask => res.mask_atlas = create_texture(device, &res.ledger, "mask atlas", wgpu::TextureFormat::R8Unorm, size),
                        AtlasKind::Color => {
                            res.color_atlas = create_texture(device, &res.ledger, "color atlas", wgpu::TextureFormat::Rgba8UnormSrgb, size)
                        }
                        AtlasKind::Path => res.path_atlas = create_texture(device, &res.ledger, "path atlas", wgpu::TextureFormat::R8Unorm, size),
                    }
                    rebind = true;
                    // 图集重建：缓存的分组里可能有旧位置上的字形、路径
                    self.clear_cache();
                }
                AtlasUpdate::Image(update) => {
                    self.res.images.apply(device, queue, update);
                    // 图片的内容（动图换帧）、位置可能变了：有图片的分组不能再用缓存
                    self.cache.state.images += 1;
                }
                AtlasUpdate::Upload { kind, x, y, width, height, pixels } => {
                    // 路径图集里预留的一块会原地重写（路径变形、画布重画）：有路径的分组不能再用缓存
                    if *kind == AtlasKind::Path {
                        self.cache.state.images += 1;
                    }
                    let texture = match kind {
                        AtlasKind::Mask => &self.res.mask_atlas,
                        AtlasKind::Color => &self.res.color_atlas,
                        AtlasKind::Path => &self.res.path_atlas,
                    };
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: *x, y: *y, z: 0 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        pixels,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(width * kind.bytes_per_pixel()),
                            rows_per_image: Some(*height),
                        },
                        wgpu::Extent3d { width: *width, height: *height, depth_or_array_layers: 1 },
                    );
                }
            }
        }
        if rebind {
            self.res.rebind(device);
        }
    }

    /// 只有普通图元（没有裁剪、分组、特效）时的快捷方式
    pub fn set_quads(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, quads: &[Quad]) {
        self.res.upload_quads(device, queue, quads);
        self.list = ListInfo::plain(quads.len() as u32);
    }

    /// 新的显示列表：上传图元、裁剪区域和渐变，整理出每帧规划要用的部分（分组的父子关系、内容签名、着色器特性的区间）
    pub fn set_display_list(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, list: &DisplayList) {
        let res = &mut self.res;
        res.upload_quads(device, queue, &list.quads);

        // 裁剪下标 0 表示"不裁剪"，放一个占位
        let clips: Vec<Clip> = std::iter::once(Clip::zeroed()).chain(list.clips.iter().copied()).collect();
        if clips.len() > res.clip_capacity {
            res.clip_capacity = clips.len().next_power_of_two();
            res.clips = create_buffer(device, &res.ledger, "clips", res.clip_capacity * size_of::<Clip>(), wgpu::BufferUsages::STORAGE);
            res.rebind(device);
        }
        queue.write_buffer(&res.clips, 0, bytemuck::cast_slice(&clips));
        // 没有渐变时不用上传：没有图元引用缓冲区里的内容
        if !list.gradients.is_empty() {
            if list.gradients.len() > res.gradient_capacity {
                res.gradient_capacity = list.gradients.len().next_power_of_two();
                res.gradients = create_buffer(device, &res.ledger, "gradients", res.gradient_capacity * 16, wgpu::BufferUsages::STORAGE);
                res.rebind(device);
            }
            queue.write_buffer(&res.gradients, 0, bytemuck::cast_slice(&list.gradients));
        }

        self.list = ListInfo::new(list);
        // 新的显示列表里没有的分组不用再缓存
        if self.cache.state.len() > 0 {
            let keys: HashSet<u64> = self.list.signatures.iter().flatten().map(|s| s.key).collect();
            self.cache.retain(&keys, &mut self.res.pool);
        }
    }

    /// 原地修补过的显示列表：结构、图元个数、裁剪、分组、渐变都和上一份一样，只有 `quads` 这几段图元变了（颜色、圆角、
    /// 边框、一层外投影）。只上传这几段；分组的内容签名、每层图元的范围跟着改过的图元重算。
    /// 和上一份对不上（图元个数不一样）时按整份新的处理
    pub fn patch_display_list(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, list: &DisplayList, quads: &[Range<u32>]) {
        let fits = quads.iter().all(|r| r.start <= r.end && r.end as usize <= list.quads.len());
        if list.quads.len() as u32 != self.list.count || !fits {
            self.set_display_list(device, queue, list);
            return;
        }
        let (Some(first), Some(last)) = (quads.first(), quads.last()) else { return };
        let size = size_of::<Quad>() as u64;
        // 段很多时（一次改了很多元素）合成一次上传：每次 write_buffer 都有固定的开销
        let merged = first.start..last.end;
        let spans: &[Range<u32>] = if quads.len() > PATCH_UPLOADS { std::slice::from_ref(&merged) } else { quads };
        for r in spans {
            let bytes: &[u8] = bytemuck::cast_slice(&list.quads[r.start as usize..r.end as usize]);
            queue.write_buffer(&self.res.instances, u64::from(r.start) * size, bytes);
        }
        if !self.list.groups.is_empty() {
            self.list.layer_bounds.clone_from(&list.layer_bounds);
            // 改过的图元在要缓存的分组里：内容签名重算（内容变了，缓存的纹理作废）
            let touched = |g: &scene::Group| quads.iter().any(|r| r.start < g.quads.end && g.quads.start < r.end);
            if self.list.signatures.iter().zip(&self.list.groups).any(|(s, g)| s.is_some() && touched(g)) {
                self.list.signatures = crate::cache::signatures(list);
            }
        }
    }

    /// 空闲满 [`IDLE_RELEASE`] 的分组缓存和离屏纹理归还显存。每帧结束时自动做一次；渲染器不出帧（画面静止）时，
    /// 调用方在 [`Renderer::next_release`] 给的时候调用它
    pub fn release_idle(&mut self) {
        self.release_idle_at(Instant::now());
    }

    /// 再过多久有空闲满 [`IDLE_RELEASE`]、可以归还的分组缓存或离屏纹理（已经到了是 0）；什么都没留着时是 `None`。
    /// 渲染器不出帧时，调用方等到这个时候调用 [`Renderer::release_idle`]
    pub fn next_release(&self) -> Option<Duration> {
        let at = [self.cache.state.next_release(), self.res.pool.next_release()].into_iter().flatten().min()?;
        Some(at.saturating_duration_since(Instant::now()))
    }

    fn release_idle_at(&mut self, now: Instant) {
        self.cache.release_idle(now, &mut self.res.pool);
        self.res.pool.end_frame(now);
        self.report(self.cache.state.len() as u64, self.cache.state.bytes());
    }

    /// 画一帧并提交。`layers` 是合成层此刻的变换（空表示只有根层），`time` 给自定义着色器
    pub fn render(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::Texture,
        viewport: Viewport,
        layers: &[LayerTransform],
        time: Duration,
        clear: Color,
        timer: Option<&mut GpuTimer>,
    ) -> wgpu::SubmissionIndex {
        self.render_at(gpu, target, viewport, layers, time.as_secs_f64() as f32, clear, timer, Instant::now())
    }

    /// 同 [`Renderer::render`]，`now` 是这一帧的时刻（离屏资源按它算空闲了多久）
    fn render_at(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::Texture,
        viewport: Viewport,
        layers: &[LayerTransform],
        time: f32,
        clear: Color,
        mut timer: Option<&mut GpuTimer>,
        now: Instant,
    ) -> wgpu::SubmissionIndex {
        let device = &gpu.device;
        // 外部纹理换了、内容变了：绑定组跟上，有图片的分组不能再用缓存
        if crate::external::sync(&mut self.res.images, gpu, &mut self.external) {
            self.cache.state.images += 1;
        }
        let layers = if layers.is_empty() { vec![LayerTransform::IDENTITY] } else { layers.to_vec() };
        let budget = self.budget.unwrap_or_else(|| BUDGET.load(Ordering::Relaxed));
        self.cache.state.begin_frame();

        // 1. 规划：哪些分组离屏、用不用缓存，每个目标画哪几段、每段用哪种管线变体
        let frame = FrameInfo {
            layers: &layers,
            width: viewport.width,
            height: viewport.height,
            scale_factor: viewport.scale_factor,
            budget,
            copyable: target.usage().contains(wgpu::TextureUsages::COPY_SRC),
            // 浮点的目标是线性的，系统合成时按线性算，不用换算
            transparent: self.transparent && target.format().is_srgb(),
            max_texture: crate::max_texture_size(),
            variants: self.variants,
        };
        let plan = crate::plan::plan(&self.list, &frame, &self.cache.state);
        self.refine = plan.approximate > 0;

        // 2. 编码：照着计划写命令
        let mut commands = device.create_command_encoder(&Default::default());
        let timestamps = timer.as_deref_mut().and_then(|timer| timer.begin_frame(&mut commands));
        self.res.reserve(device, plan.targets, plan.extras, layers.len() + plan.composites);
        let extended = target.format() == crate::EXTENDED_FORMAT;
        let frame = Frame {
            stats: RenderStats::default(),
            timestamps,
            first_pass: true,
            globals: Vec::new(),
            extras: Vec::new(),
            blur: Vec::new(),
            blur_passes: 0,
            layers,
            scale_factor: viewport.scale_factor,
            time,
            // 扩展范围的目标（半精度浮点）才能显示负的分量和比白亮的颜色
            extended,
            headroom: if extended { self.headroom } else { 1.0 },
        };
        let mut encoder = Encoder::new(device, commands, &mut self.res, &self.list, frame);
        let results = encoder.groups(&plan, &mut self.cache, budget, now);
        encoder.root(&plan, target, [viewport.width, viewport.height], clear, &results);
        drop(results);
        let (commands, frame) = encoder.finish();

        // 3. 写入这一帧攒下的数据，提交
        let res = &self.res;
        gpu.queue.write_buffer(&res.layers, 0, bytemuck::cast_slice(&frame.layers));
        gpu.queue.write_buffer(&res.globals, 0, &frame.globals);
        if !frame.extras.is_empty() {
            gpu.queue.write_buffer(&res.extras, 0, bytemuck::cast_slice(&frame.extras));
        }
        if !frame.blur.is_empty() {
            gpu.queue.write_buffer(&res.blur_params, 0, &frame.blur);
        }
        let index = gpu.queue.submit([commands.finish()]);
        if let Some(timer) = timer {
            timer.after_submit();
        }
        // 空闲满 IDLE_RELEASE 的缓存和离屏纹理归还显存（按时间，不按帧数：帧率不同、断断续续出帧时都一样）
        self.cache.release_idle(now, &mut self.res.pool);
        self.res.pool.end_frame(now);
        self.finish_stats(frame.stats, &plan);
        // 第一帧交出去了：常用的管线放到后台先编
        if std::mem::take(&mut self.precompile) {
            self.res.pipelines.precompile(device, pipeline::common(self.format));
        }
        index
    }

    /// 这一帧的统计，并把缓存的变化报给全局统计
    fn finish_stats(&mut self, mut stats: RenderStats, plan: &RenderPlan) {
        stats.cache_hits = plan.hits;
        stats.cache_approximations = plan.approximate;
        stats.cache_evictions = self.cache.state.evictions;
        stats.cache_entries = self.cache.state.len() as u32;
        stats.cache_bytes = self.cache.state.bytes();
        self.stats = stats;
        TOTAL_HITS.fetch_add(stats.cache_hits as u64, Ordering::Relaxed);
        TOTAL_DRAWN.fetch_add(stats.groups_drawn as u64, Ordering::Relaxed);
        self.report(stats.cache_entries as u64, stats.cache_bytes);
    }

    fn report(&mut self, entries: u64, bytes: u64) {
        let (old_entries, old_bytes) = std::mem::replace(&mut self.reported, (entries, bytes));
        TOTAL_ENTRIES.fetch_add(entries.wrapping_sub(old_entries), Ordering::Relaxed);
        TOTAL_BYTES.fetch_add(bytes.wrapping_sub(old_bytes), Ordering::Relaxed);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // 这个渲染器的缓存不再算进全局统计
        self.report(0, 0);
    }
}

fn create_buffer(device: &wgpu::Device, ledger: &Arc<Ledger>, label: &str, size: usize, usage: wgpu::BufferUsages) -> Tracked<wgpu::Buffer> {
    memory::buffer(
        device,
        ledger,
        &wgpu::BufferDescriptor { label: Some(label), size: size as u64, usage: usage | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false },
    )
}

fn create_texture(device: &wgpu::Device, ledger: &Arc<Ledger>, label: &str, format: wgpu::TextureFormat, size: u32) -> Tracked<wgpu::Texture> {
    memory::texture(
        device,
        ledger,
        Kind::Atlas,
        &wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
    )
}

/// `atlases`：单色字形、彩色字形、路径；`storage`：合成层、裁剪、渐变
fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    atlases: [&wgpu::Texture; 3],
    sampler: &wgpu::Sampler,
    [layers, clips, gradients]: [&wgpu::Buffer; 3],
) -> wgpu::BindGroup {
    let [mask_view, color_view, path_view] = atlases.map(|atlas| atlas.create_view(&Default::default()));
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: globals,
                    offset: 0,
                    size: wgpu::BufferSize::new(size_of::<Globals>() as u64),
                }),
            },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&mask_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&color_view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 4, resource: layers.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: clips.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&path_view) },
            wgpu::BindGroupEntry { binding: 7, resource: gradients.as_entire_binding() },
        ],
    })
}
