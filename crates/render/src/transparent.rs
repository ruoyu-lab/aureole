//! 窗口透明时（透出系统的窗口材质）的最后一步：见 `shaders/transparent.wgsl`。
//!
//! 只有透明的窗口用：根先画进一张中间纹理（和要读后面内容时同一条路），再由这条管线换算着写进窗口表面。
//! 管线、绑定布局第一次用到时才建，不透明的窗口一点开销都没有。

use crate::pipeline::{COMPILE_LOG, shader_module};

const SHADER: &str = include_str!("shaders/transparent.wgsl");

/// 换算用的管线（按目标格式）
pub(crate) struct TransparentOutput {
    format: wgpu::TextureFormat,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}

impl TransparentOutput {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let start = std::time::Instant::now();
        let module = shader_module(device, "transparent output", wgpu::ShaderSource::Wgsl(SHADER.into()));
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("transparent output"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("transparent output"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("transparent output"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        tracing::debug!(target: COMPILE_LOG, "窗口透明的管线（{format:?}）：{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
        Self { format, layout, pipeline }
    }

    /// 目标格式
    pub(crate) fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// 管线
    pub(crate) fn pipeline(&self) -> &wgpu::RenderPipeline {
        &self.pipeline
    }

    /// 采样这张中间纹理的绑定
    pub(crate) fn bind(&self, device: &wgpu::Device, source: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("transparent output"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) }],
        })
    }
}
