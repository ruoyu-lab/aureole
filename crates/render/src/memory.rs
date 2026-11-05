//! 显存记账：渲染器自己创建的纹理和缓冲区按字节数记下来，创建时加、释放时减。
//!
//! 每个 [`crate::Renderer`] 一本账（[`crate::Renderer::gpu_memory`]），[`gpu_memory`] 是本进程所有渲染器的合计。
//! 只算渲染器自己分配的：交换链的纹理由系统分配，驱动内部的开销也不在里面。

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// 渲染器分配的显存，字节
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuMemory {
    /// 字形图集、路径图集、图片图集
    pub atlases: u64,
    /// 单独纹理的大图（和图集放不下的图片）
    pub images: u64,
    /// 分组、模糊用的离屏纹理（池里留着、跨帧复用的）
    pub offscreen: u64,
    /// 分组缓存留着的离屏纹理（内容没变的分组下一帧直接用，见 [`crate::set_cache_budget`]）
    pub cache: u64,
    /// 图元、合成层、裁剪、渐变等缓冲区
    pub buffers: u64,
}

impl GpuMemory {
    /// 合计
    pub fn total(&self) -> u64 {
        self.atlases + self.images + self.offscreen + self.cache + self.buffers
    }
}

/// 一笔分配记在哪一类
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Atlas,
    Image,
    Offscreen,
    Cache,
    Buffer,
}

/// 一个渲染器的账
#[derive(Debug, Default)]
pub(crate) struct Ledger {
    bytes: [AtomicU64; 5],
}

impl Ledger {
    pub fn read(&self) -> GpuMemory {
        let [atlases, images, offscreen, cache, buffers] = self.bytes.each_ref().map(|b| b.load(Ordering::Relaxed));
        GpuMemory { atlases, images, offscreen, cache, buffers }
    }
}

/// 所有还活着的账：读合计时一起加
static LEDGERS: Mutex<Vec<Weak<Ledger>>> = Mutex::new(Vec::new());

/// 新开一本账，登记进合计
pub(crate) fn ledger() -> Arc<Ledger> {
    let ledger = Arc::new(Ledger::default());
    if let Ok(mut all) = LEDGERS.lock() {
        all.retain(|l| l.strong_count() > 0);
        all.push(Arc::downgrade(&ledger));
    }
    ledger
}

/// 本进程所有渲染器分配的显存合计
pub fn gpu_memory() -> GpuMemory {
    let Ok(all) = LEDGERS.lock() else { return GpuMemory::default() };
    all.iter().filter_map(Weak::upgrade).map(|l| l.read()).fold(GpuMemory::default(), |a, b| GpuMemory {
        atlases: a.atlases + b.atlases,
        images: a.images + b.images,
        offscreen: a.offscreen + b.offscreen,
        cache: a.cache + b.cache,
        buffers: a.buffers + b.buffers,
    })
}

/// 记在账上的一笔分配：释放时减掉
#[derive(Debug)]
pub(crate) struct Allocation {
    ledger: Arc<Ledger>,
    kind: Kind,
    bytes: u64,
}

impl Allocation {
    pub fn new(ledger: &Arc<Ledger>, kind: Kind, bytes: u64) -> Self {
        ledger.bytes[kind as usize].fetch_add(bytes, Ordering::Relaxed);
        Self { ledger: ledger.clone(), kind, bytes }
    }

    /// 改记到另一类（离屏纹理被分组缓存留下、又还回纹理池）
    pub fn move_to(&mut self, kind: Kind) {
        self.ledger.bytes[self.kind as usize].fetch_sub(self.bytes, Ordering::Relaxed);
        self.ledger.bytes[kind as usize].fetch_add(self.bytes, Ordering::Relaxed);
        self.kind = kind;
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        self.ledger.bytes[self.kind as usize].fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// 记了账的纹理或缓冲区，用起来和原来一样（解引用）
#[derive(Debug)]
pub(crate) struct Tracked<T> {
    resource: T,
    _allocation: Allocation,
}

impl<T> Deref for Tracked<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.resource
    }
}

/// 建一张纹理并记账：每级 mipmap 的宽 × 高 × 每像素字节数之和
pub(crate) fn texture(device: &wgpu::Device, ledger: &Arc<Ledger>, kind: Kind, desc: &wgpu::TextureDescriptor) -> Tracked<wgpu::Texture> {
    let resource = device.create_texture(desc);
    Tracked { resource, _allocation: Allocation::new(ledger, kind, texture_bytes(desc)) }
}

/// 建一个缓冲区并记账
pub(crate) fn buffer(device: &wgpu::Device, ledger: &Arc<Ledger>, desc: &wgpu::BufferDescriptor) -> Tracked<wgpu::Buffer> {
    let resource = device.create_buffer(desc);
    Tracked { resource, _allocation: Allocation::new(ledger, Kind::Buffer, desc.size) }
}

/// 纹理占多少字节（未压缩的格式）
pub(crate) fn texture_bytes(desc: &wgpu::TextureDescriptor) -> u64 {
    let texel = u64::from(desc.format.block_copy_size(None).unwrap_or(4));
    let size = desc.size;
    (0..desc.mip_level_count.max(1))
        .map(|level| u64::from((size.width >> level).max(1)) * u64::from((size.height >> level).max(1)) * texel)
        .sum::<u64>()
        * u64::from(size.depth_or_array_layers.max(1))
        * u64::from(desc.sample_count.max(1))
}
