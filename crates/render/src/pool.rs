//! 离屏纹理池：分组、模糊每帧都要临时纹理，按块分配、跨帧复用，不在每帧创建。
//! 按最近用过的时刻回收：空闲满 [`IDLE_RELEASE`] 的纹理释放（一帧结束时，或者渲染器空闲时由调用方检查）。

use std::sync::Arc;
use std::time::Instant;

use crate::IDLE_RELEASE;
use crate::memory::{Allocation, Kind, Ledger, texture_bytes};

/// 纹理按这个大小向上取整分配，尺寸变化（比如动画）时也能复用
const BLOCK: u32 = 128;

/// 借出的一块离屏纹理，只用左上角的 `used` 区域
#[derive(Clone)]
pub(crate) struct Target {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    /// 纹理实际大小
    pub size: [u32; 2],
    /// 用到的区域，像素
    pub used: [u32; 2],
    /// 内容的精确大小（逐级缩小后可能不是整数），`used` 是它向上取整
    pub extent: [f32; 2],
    /// 用到的区域左上角在屏幕上的位置，物理像素
    pub origin: [f32; 2],
    /// 给图元着色器采样这张纹理（第 1 组绑定）
    pub effect: wgpu::BindGroup,
    /// 给模糊着色器采样这张纹理
    pub blur: wgpu::BindGroup,
}

impl Target {
    /// 内容占整张纹理的比例
    pub fn uv_extent(&self) -> [f32; 2] {
        [self.extent[0] / self.size[0] as f32, self.extent[1] / self.size[1] as f32]
    }
}

struct Entry {
    target: Target,
    in_use: bool,
    /// 这一帧借出过，或者刚从缓存还回来：一帧结束（或检查空闲）时记成此刻用过
    touched: bool,
    /// 最近用过的时刻：空闲满 [`IDLE_RELEASE`] 的释放
    last_used: Instant,
    /// 被分组缓存留着：不借出、不释放，直到缓存不要它了
    pinned: bool,
    /// 记在显存的账上，释放纹理时减掉（借出去的 `Target` 是同一张纹理的句柄，不另记）。被缓存留着时记在缓存那一类
    allocation: Allocation,
}

pub(crate) struct Pool {
    entries: Vec<Entry>,
    ledger: Arc<Ledger>,
}

/// 创建纹理时需要的绑定布局
pub(crate) struct PoolLayouts<'a> {
    pub effect: &'a wgpu::BindGroupLayout,
    pub blur: &'a wgpu::BindGroupLayout,
    pub sampler: &'a wgpu::Sampler,
    pub blur_params: &'a wgpu::Buffer,
}

impl Pool {
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self { entries: Vec::new(), ledger }
    }

    /// 借一块至少 `width` x `height` 的纹理
    pub fn acquire(
        &mut self,
        device: &wgpu::Device,
        layouts: &PoolLayouts,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Target {
        // 不超过设备的纹理边长上限：更大的分组（比上限还大的窗口）超出的部分裁掉，而不是建纹理失败
        let max = crate::max_texture_size();
        let (width, height) = (width.clamp(1, max), height.clamp(1, max));
        // 够大、又不至于大太多的空闲纹理里挑最小的
        let fits = |e: &&mut Entry| {
            let [w, h] = e.target.size;
            !e.in_use && !e.pinned && e.target.format == format && w >= width && h >= height && (w * h) <= 4 * (width * height).max(BLOCK * BLOCK)
        };
        let entry = match self.entries.iter_mut().filter(fits).min_by_key(|e| e.target.size[0] * e.target.size[1]) {
            Some(entry) => entry,
            None => {
                let size = [width.next_multiple_of(BLOCK).min(max), height.next_multiple_of(BLOCK).min(max)];
                let (target, bytes) = create(device, layouts, format, size);
                let allocation = Allocation::new(&self.ledger, Kind::Offscreen, bytes);
                self.entries.push(Entry { target, in_use: false, touched: false, last_used: Instant::now(), pinned: false, allocation });
                // 不会失败：上一行刚放进去一个
                self.entries.last_mut().expect("刚放进去")
            }
        };
        entry.in_use = true;
        entry.touched = true;
        let mut target = entry.target.clone();
        target.used = [width, height];
        target.extent = [width as f32, height as f32];
        target
    }

    /// 一帧结束、或者空闲时检查（`now` 是此刻）：借出的都还回来，用过的记下此刻；
    /// 空闲满 [`IDLE_RELEASE`] 的释放，归还显存（缓存留着的除外）
    pub fn end_frame(&mut self, now: Instant) {
        for entry in &mut self.entries {
            if std::mem::take(&mut entry.touched) {
                entry.last_used = now;
            }
            entry.in_use = false;
        }
        self.entries.retain(|entry| entry.pinned || now.saturating_duration_since(entry.last_used) < IDLE_RELEASE);
    }

    /// 池里留着的纹理（缓存钉住的不算）最早什么时候空闲满 [`IDLE_RELEASE`]；没有时是 `None`
    pub fn next_release(&self) -> Option<Instant> {
        self.entries.iter().filter(|entry| !entry.pinned).map(|entry| entry.last_used + IDLE_RELEASE).min()
    }

    /// 缓存留下这张纹理（这一帧借出的）：以后不再借给别人，帧结束也不收回；显存记到缓存那一类
    pub fn pin(&mut self, texture: &wgpu::Texture) {
        if let Some(entry) = self.entries.iter_mut().find(|e| &e.target.texture == texture && !e.pinned) {
            entry.pinned = true;
            entry.allocation.move_to(Kind::Cache);
        }
    }

    /// 缓存因为空闲不要这张纹理了：直接释放，归还显存
    pub fn discard(&mut self, texture: &wgpu::Texture) {
        self.entries.retain(|e| &e.target.texture != texture || e.in_use);
    }

    /// 缓存不要这张纹理了：还给纹理池，可以再借出去
    pub fn unpin(&mut self, texture: &wgpu::Texture) {
        if let Some(entry) = self.entries.iter_mut().find(|e| &e.target.texture == texture && e.pinned) {
            entry.pinned = false;
            entry.touched = true;
            entry.allocation.move_to(Kind::Offscreen);
        }
    }
}

/// 建一块离屏纹理，返回它和它占的字节数
fn create(device: &wgpu::Device, layouts: &PoolLayouts, format: wgpu::TextureFormat, size: [u32; 2]) -> (Target, u64) {
    let descriptor = wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    };
    let texture = device.create_texture(&descriptor);
    let view = texture.create_view(&Default::default());
    let effect = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("offscreen effect"),
        layout: layouts.effect,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
    });
    let blur = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("offscreen blur source"),
        layout: layouts.blur,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(layouts.sampler) },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: layouts.blur_params,
                    offset: 0,
                    size: wgpu::BufferSize::new(size_of::<crate::encoder::BlurParams>() as u64),
                }),
            },
        ],
    });
    let target = Target { texture, view, format, size, used: size, extent: [size[0] as f32, size[1] as f32], origin: [0.0; 2], effect, blur };
    (target, texture_bytes(&descriptor))
}
