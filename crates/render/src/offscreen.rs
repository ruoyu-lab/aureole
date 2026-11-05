use std::time::Duration;

use crate::{AtlasUpdate, Color, DisplayList, EXTENDED_FORMAT, Gpu, LayerTransform, RenderStats, Renderer, Viewport};

/// 平时的目标：8 位 sRGB
const SRGB: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// 离屏渲染一帧，返回 RGBA8（sRGB）像素，逐行紧密排列。用于截图和图像测试。
/// `layers` 是合成层的变换（空表示只有根层），`atlas` 是字形图集的完整内容
/// （[`crate::GlyphAtlas::snapshot`]），没有文字时传空；`time` 给自定义着色器。
/// 读回失败（设备丢失）时记一条错误，返回同样长度的全零（全透明）像素
pub fn render_to_image(
    gpu: &Gpu,
    list: &DisplayList,
    layers: &[LayerTransform],
    atlas: &[AtlasUpdate],
    viewport: Viewport,
    clear: Color,
    time: Duration,
) -> Vec<u8> {
    let mut offscreen = Offscreen::with_format(gpu, viewport, SRGB, 1.0);
    offscreen.apply_atlas(gpu, atlas);
    offscreen.set_display_list(gpu, list);
    offscreen.render(gpu, layers, clear, time)
}

/// 离屏渲染一帧到扩展范围的目标（[`EXTENDED_FORMAT`]，和窗口在广色域、HDR 显示器上用的表面一样），
/// 返回线性的预乘 RGBA（sRGB 原色，分量可以小于 0、大于 1），逐行紧密排列。`headroom` 当作显示器此刻的余量
/// （SDR 白的倍数）：比它亮的颜色、HDR 图片按它压缩。其余参数同 [`render_to_image`]
pub fn render_to_extended_image(
    gpu: &Gpu,
    list: &DisplayList,
    layers: &[LayerTransform],
    atlas: &[AtlasUpdate],
    viewport: Viewport,
    clear: Color,
    time: Duration,
    headroom: f32,
) -> Vec<f32> {
    let mut offscreen = Offscreen::with_format(gpu, viewport, EXTENDED_FORMAT, headroom);
    offscreen.apply_atlas(gpu, atlas);
    offscreen.set_display_list(gpu, list);
    let bytes = offscreen.render(gpu, layers, clear, time);
    bytes.as_chunks::<2>().0.iter().map(|&b| half::f16::from_bits(u16::from_ne_bytes(b)).to_f32()).collect()
}

/// 离屏连续画多帧：渲染器和目标纹理跨帧保留，所以分组的纹理缓存照常工作。
/// 测试、截图、基准用；每帧读回像素，[`Offscreen::stats`] 是这一帧的绘制统计
pub struct Offscreen {
    renderer: Renderer,
    texture: wgpu::Texture,
    viewport: Viewport,
    format: wgpu::TextureFormat,
}

impl Offscreen {
    /// 一张 `viewport` 大小的 8 位 sRGB 目标纹理
    pub fn new(gpu: &Gpu, viewport: Viewport) -> Self {
        Self::with_format(gpu, viewport, SRGB, 1.0)
    }

    fn with_format(gpu: &Gpu, viewport: Viewport, format: wgpu::TextureFormat, headroom: f32) -> Self {
        let mut renderer = Renderer::from_gpu(gpu, format);
        renderer.set_headroom(headroom);
        Self { renderer, texture: target(gpu, viewport, format), viewport, format }
    }

    /// 里面的渲染器（调缓存预算等）
    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// 应用图集的更新
    pub fn apply_atlas(&mut self, gpu: &Gpu, atlas: &[AtlasUpdate]) {
        self.renderer.apply_atlas(&gpu.device, &gpu.queue, atlas);
    }

    /// 换显示列表
    pub fn set_display_list(&mut self, gpu: &Gpu, list: &DisplayList) {
        self.renderer.set_display_list(&gpu.device, &gpu.queue, list);
    }

    /// 换原地修补过的显示列表：只上传改过的几段图元（见 [`Renderer::patch_display_list`]）
    pub fn patch_display_list(&mut self, gpu: &Gpu, list: &DisplayList, quads: &[std::ops::Range<u32>]) {
        self.renderer.patch_display_list(&gpu.device, &gpu.queue, list, quads);
    }

    /// 改变目标大小和缩放系数（渲染器和它的缓存保留）
    pub fn resize(&mut self, gpu: &Gpu, viewport: Viewport) {
        self.texture = target(gpu, viewport, self.format);
        self.viewport = viewport;
    }

    /// 画一帧并读回像素，逐行紧密排列（8 位 sRGB 的目标是 RGBA8）。
    /// 读回失败（设备丢失、等不到 GPU）时记一条错误，返回同样长度的全零（全透明）像素
    pub fn render(&mut self, gpu: &Gpu, layers: &[LayerTransform], clear: Color, time: Duration) -> Vec<u8> {
        self.renderer.render(gpu, &self.texture, self.viewport, layers, time, clear, None);
        read_pixels(gpu, &self.texture, self.viewport).unwrap_or_else(|(error, len)| {
            tracing::error!("离屏渲染读回像素失败，返回全透明的像素：{error}");
            vec![0; len]
        })
    }

    /// 上一帧的绘制统计
    pub fn stats(&self) -> RenderStats {
        self.renderer.stats()
    }
}

fn target(gpu: &Gpu, viewport: Viewport, format: wgpu::TextureFormat) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: viewport.width, height: viewport.height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// 读回目标纹理的像素。失败时给出原因和本该有的字节数
fn read_pixels(gpu: &Gpu, texture: &wgpu::Texture, viewport: Viewport) -> Result<Vec<u8>, (String, usize)> {
    let Viewport { width, height, .. } = viewport;
    let pixel = texture.format().block_copy_size(None).unwrap_or(4);
    let row = width * pixel;
    let len = (row * height) as usize;
    let padded_row = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("offscreen readback"),
        size: (padded_row * height) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded_row), rows_per_image: Some(height) },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    // 映射的结果从回调里带出来，不在回调里 panic（回调在 poll 里调用）
    let (sender, mapped) = std::sync::mpsc::channel();
    readback.map_async(wgpu::MapMode::Read, .., move |result| {
        let _ = sender.send(result);
    });
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| (format!("等待 GPU 失败：{e}"), len))?;
    // 等完 GPU 映射就做好了，但回调不一定已经调用：几条线程共用一个设备时，回调由正在轮询的那条线程调用，
    // 可能稍后才轮到。所以只在回调已经报了错时算失败；没有结果就照样去读，读不到才算失败
    if let Ok(Err(e)) = mapped.try_recv() {
        return Err((format!("回读映射失败：{e}"), len));
    }
    let data = readback.get_mapped_range(..).map_err(|e| (format!("读取回读缓冲失败：{e}"), len))?;
    let mut pixels = Vec::with_capacity(len);
    for padded in data.chunks(padded_row as usize) {
        pixels.extend_from_slice(&padded[..row as usize]);
    }
    Ok(pixels)
}
