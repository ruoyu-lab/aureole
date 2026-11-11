//! 图片纹理：小图放进带 mip 的图片图集（和其他图元同一次 draw call），
//! 大图各自一张带完整 mip 链的纹理（画它时断开一次 draw call，换第 2 组绑定）。
//! 广色域、HDR 的图片是半精度浮点的纹理（[`ImageEncoding::Float16`]），总是单独一张，
//! 绑定组里带着它的峰值亮度，着色器按显示器的余量做色调映射。
//!
//! 像素在 CPU 侧（后台线程）已经预乘、生成好各级 mip，这里只负责建纹理和上传。
//! 和字形图集一样，CPU 侧决定放在哪，GPU 侧按 [`ImageUpdate`] 维护纹理。
//! 像素和更新这些数据（[`crate::ImageData`]、[`ImageUpdate`]）在 scene 里，这里只有纹理。

use std::collections::HashMap;
use std::sync::Arc;

use crate::memory::{self, Kind, Ledger, Tracked};
use crate::{IMAGE_ATLAS_LEVELS, ImageEncoding, ImageLevel, ImageUpdate};

/// 纹理格式：sRGB 编码、预乘 alpha。采样时硬件先转成线性再过滤，过滤在线性预乘空间里进行，
/// 半透明边缘不会出现黑边或白边
pub const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// 广色域、HDR 图片的纹理格式：半精度浮点，线性的预乘颜色（sRGB 原色，分量可以小于 0、大于 1）
pub const IMAGE_FLOAT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// 一种像素存法对应的纹理格式：[`ImageEncoding::Srgb8`] 是 [`IMAGE_FORMAT`]，[`ImageEncoding::Float16`] 是 [`IMAGE_FLOAT_FORMAT`]
fn texture_format(encoding: ImageEncoding) -> wgpu::TextureFormat {
    match encoding {
        ImageEncoding::Srgb8 => IMAGE_FORMAT,
        ImageEncoding::Float16 { .. } => IMAGE_FLOAT_FORMAT,
    }
}

/// GPU 侧的图片纹理（第 2 组绑定：图集、单独的纹理、带 mip 的采样器、这张纹理的参数）
pub(crate) struct Images {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    atlas: Tracked<wgpu::Texture>,
    placeholder: Tracked<wgpu::Texture>,
    /// 8 位纹理的参数（峰值 0：不做色调映射）：图集和 8 位的单独纹理共用
    plain: Tracked<wgpu::Buffer>,
    /// 图集 + 占位纹理：平时绑定的
    default: wgpu::BindGroup,
    /// 单独的纹理，各自一个绑定组（图集 + 这张纹理 + 它的参数）
    textures: HashMap<u32, Single>,
    /// 外部纹理（`external.rs`），各自一个绑定组；纹理归登记它的代码，不记显存账
    externals: HashMap<u32, (wgpu::Texture, wgpu::BindGroup)>,
    ledger: Arc<Ledger>,
}

/// 一张单独的纹理（显存按格式记账：半精度浮点的每像素 8 字节）
struct Single {
    texture: Tracked<wgpu::Texture>,
    /// 浮点纹理的参数（峰值亮度）；8 位纹理用共用的 `plain`
    params: Option<Tracked<wgpu::Buffer>>,
    group: wgpu::BindGroup,
}

/// 着色器里的 `ImageInfo`：峰值亮度，0 表示 8 位纹理（不做色调映射）
fn params_bytes(encoding: ImageEncoding) -> [u8; 16] {
    let peak = match encoding {
        ImageEncoding::Srgb8 => 0.0,
        // 浮点纹理至少按 1 算（> 0 表示要放进目标能显示的范围）
        ImageEncoding::Float16 { peak } => peak.max(1.0),
    };
    bytemuck::cast([peak, 0.0, 0.0, 0.0])
}

impl Images {
    pub fn new(device: &wgpu::Device, ledger: &Arc<Ledger>) -> Self {
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("images"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
            ],
        });
        // 三线性过滤：缩小的照片在 mip 之间平滑过渡，不闪烁、不出摩尔纹
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image trilinear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        // 收到 ResetAtlas 之前用 1x1 的占位
        let atlas = create(device, ledger, Kind::Atlas, "image atlas", [1, 1], 1, IMAGE_FORMAT);
        let placeholder = create(device, ledger, Kind::Image, "image placeholder", [1, 1], 1, IMAGE_FORMAT);
        // 新建的缓冲区是 0：峰值 0，8 位纹理
        let plain = params_buffer(device, ledger);
        let default = bind_group(device, &layout, &sampler, &atlas, &placeholder, &plain);
        Self { layout, sampler, atlas, placeholder, plain, default, textures: HashMap::new(), externals: HashMap::new(), ledger: ledger.clone() }
    }

    pub fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    /// 平时绑定的第 2 组：图集 + 占位纹理
    pub fn default_group(&self) -> &wgpu::BindGroup {
        &self.default
    }

    /// 单独纹理的绑定组；还没建（或已释放）时为 `None`
    pub fn texture_group(&self, id: u32) -> Option<&wgpu::BindGroup> {
        match self.textures.get(&id) {
            Some(single) => Some(&single.group),
            None => self.externals.get(&id).map(|(_, group)| group),
        }
    }

    /// 外部纹理 `id` 换成 `texture`：建它的绑定组
    pub fn set_external(&mut self, device: &wgpu::Device, id: u32, texture: &wgpu::Texture) {
        let group = bind_group(device, &self.layout, &self.sampler, &self.atlas, texture, &self.plain);
        self.externals.insert(id, (texture.clone(), group));
    }

    /// 外部纹理 `id` 撤销了
    pub fn remove_external(&mut self, id: u32) {
        self.externals.remove(&id);
    }

    pub fn apply(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, update: &ImageUpdate) {
        match update {
            &ImageUpdate::ResetAtlas { size } => {
                self.atlas = create(device, &self.ledger, Kind::Atlas, "image atlas", [size, size], IMAGE_ATLAS_LEVELS, IMAGE_FORMAT);
                self.default = bind_group(device, &self.layout, &self.sampler, &self.atlas, &self.placeholder, &self.plain);
                // 单独纹理的绑定组里也有图集，跟着重建
                for single in self.textures.values_mut() {
                    let params = single.params.as_ref().unwrap_or(&self.plain);
                    single.group = bind_group(device, &self.layout, &self.sampler, &self.atlas, &single.texture, params);
                }
                for (texture, group) in self.externals.values_mut() {
                    *group = bind_group(device, &self.layout, &self.sampler, &self.atlas, texture, &self.plain);
                }
            }
            ImageUpdate::Atlas { x, y, image } => {
                // 图集只放 8 位的图（浮点的图总是单独一张纹理）
                if image.encoding != ImageEncoding::Srgb8 {
                    return;
                }
                for (level, data) in image.levels.iter().enumerate().take(IMAGE_ATLAS_LEVELS as usize) {
                    upload(queue, &self.atlas, level as u32, [x >> level, y >> level], data, 4);
                }
            }
            ImageUpdate::Create { id, image } => {
                let encoding = image.encoding;
                let texture = create(device, &self.ledger, Kind::Image, "image", image.storage(), image.levels.len() as u32, texture_format(encoding));
                for (level, data) in image.levels.iter().enumerate() {
                    upload(queue, &texture, level as u32, [0, 0], data, encoding.bytes_per_pixel());
                }
                let params = (encoding != ImageEncoding::Srgb8).then(|| {
                    let params = params_buffer(device, &self.ledger);
                    queue.write_buffer(&params, 0, &params_bytes(encoding));
                    params
                });
                let group = bind_group(device, &self.layout, &self.sampler, &self.atlas, &texture, params.as_ref().unwrap_or(&self.plain));
                self.textures.insert(*id, Single { texture, params, group });
            }
            ImageUpdate::Write { id, image } => {
                if let Some(single) = self.textures.get(id) {
                    let texture = &single.texture;
                    // 格式不同（不应该发生）时不写
                    if texture.format() != texture_format(image.encoding) {
                        return;
                    }
                    if let Some(params) = &single.params {
                        queue.write_buffer(params, 0, &params_bytes(image.encoding));
                    }
                    for (level, data) in image.levels.iter().enumerate().take(texture.mip_level_count() as usize) {
                        upload(queue, texture, level as u32, [0, 0], data, image.encoding.bytes_per_pixel());
                    }
                }
            }
            ImageUpdate::Release { id } => {
                self.textures.remove(id);
            }
        }
    }
}

fn create(
    device: &wgpu::Device,
    ledger: &Arc<Ledger>,
    kind: Kind,
    label: &str,
    [width, height]: [u32; 2],
    levels: u32,
    format: wgpu::TextureFormat,
) -> Tracked<wgpu::Texture> {
    let descriptor = wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: levels.max(1),
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    };
    memory::texture(device, ledger, kind, &descriptor)
}

/// 一张纹理的参数（16 字节，记在缓冲区的账上），内容是 0，用 `queue.write_buffer` 写
fn params_buffer(device: &wgpu::Device, ledger: &Arc<Ledger>) -> Tracked<wgpu::Buffer> {
    let descriptor = wgpu::BufferDescriptor {
        label: Some("image params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    };
    memory::buffer(device, ledger, &descriptor)
}

fn upload(queue: &wgpu::Queue, texture: &wgpu::Texture, level: u32, [x, y]: [u32; 2], data: &ImageLevel, bytes_per_pixel: u32) {
    if data.width == 0 || data.height == 0 {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: level, origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
        &data.pixels,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(data.width * bytes_per_pixel), rows_per_image: Some(data.height) },
        wgpu::Extent3d { width: data.width, height: data.height, depth_or_array_layers: 1 },
    );
}

fn bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    atlas: &wgpu::Texture,
    texture: &wgpu::Texture,
    params: &wgpu::Buffer,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("images"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas.create_view(&Default::default())) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&texture.create_view(&Default::default())) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 3, resource: params.as_entire_binding() },
        ],
    })
}
