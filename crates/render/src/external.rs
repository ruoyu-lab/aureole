//! 外部纹理：框架以外的代码在自己的回调里往一张 GPU 纹理里画（内嵌的 CEF 视图把每一帧复制进来），
//! 元素按编号引用它（显示列表里是 [`crate::QuadEffect::Image`]），渲染器采样它。
//!
//! 登记表是进程里共用的一份：纹理建在窗口共用的设备（[`Gpu`]）上，任何线程都能换纹理、报告内容变了。
//! 渲染器每帧开始时对一次版本号：换了纹理的重建绑定组，只是内容变了的让有图片的分组缓存失效。
//! 建在别的设备上的纹理（设备丢失、重新创建以后还没换掉的）不用，那个编号先什么都不画。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::Gpu;
use crate::image::Images;

/// 一张外部纹理的登记：编号、纹理、它建在哪个设备上。丢掉时撤销登记，显示列表里还引用它的图元不再画
#[derive(Debug)]
pub struct ExternalTexture {
    id: u32,
}

struct Entry {
    texture: wgpu::Texture,
    /// 建在哪个设备上（[`Gpu::id`]）
    device: u64,
    /// 换过几次纹理
    replaced: u64,
    /// 内容变过几次（含换纹理）
    content: u64,
}

struct Registry {
    entries: Mutex<BTreeMap<u32, Entry>>,
    /// 登记表有任何变化时加一：渲染器对得上就不用锁
    revision: AtomicU64,
    next: AtomicU32,
}

static REGISTRY: Registry = Registry { entries: Mutex::new(BTreeMap::new()), revision: AtomicU64::new(0), next: AtomicU32::new(0) };

fn entries() -> std::sync::MutexGuard<'static, BTreeMap<u32, Entry>> {
    REGISTRY.entries.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 登记表此刻的版本：有纹理换了、内容变了、撤销了都会变。渲染线程拿它判断要不要重画
pub fn external_revision() -> u64 {
    REGISTRY.revision.load(Ordering::Acquire)
}

impl ExternalTexture {
    /// 分配一个编号，还没有纹理（显示列表里引用它时先什么都不画）
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let id = scene::EXTERNAL_TEXTURE_BASE + REGISTRY.next.fetch_add(1, Ordering::Relaxed);
        Self { id }
    }

    /// 显示列表里引用它用的编号（[`crate::QuadEffect::Image`] 的 `texture`）
    pub fn id(&self) -> u32 {
        self.id
    }

    /// 换成 `texture`（第一次给，或者大小变了建了新的）。纹理要建在 `gpu` 上，能当着色器的输入
    /// （`TEXTURE_BINDING`），颜色是预乘的；sRGB 格式的按 sRGB 解码
    pub fn set(&self, gpu: &Gpu, texture: wgpu::Texture) {
        let mut entries = entries();
        let entry = entries.entry(self.id).or_insert_with(|| Entry { texture: texture.clone(), device: gpu.id(), replaced: 0, content: 0 });
        entry.texture = texture;
        entry.device = gpu.id();
        entry.replaced += 1;
        entry.content += 1;
        drop(entries);
        REGISTRY.revision.fetch_add(1, Ordering::AcqRel);
    }

    /// 纹理的内容变了（原地写过）：下一帧重画用到它的地方，缓存了它的分组作废
    pub fn changed(&self) {
        if let Some(entry) = entries().get_mut(&self.id) {
            entry.content += 1;
        }
        REGISTRY.revision.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for ExternalTexture {
    fn drop(&mut self) {
        entries().remove(&self.id);
        REGISTRY.revision.fetch_add(1, Ordering::AcqRel);
    }
}

/// 一个渲染器上次对到的状态
#[derive(Default)]
pub(crate) struct Seen {
    revision: u64,
    /// 编号 -> (换纹理的次数, 内容的次数)
    entries: HashMap<u32, (u64, u64)>,
}

/// 把登记表对到渲染器的图片资源上：换了的纹理重建绑定组，撤销的拿掉。内容变过时返回真（有图片的分组缓存要作废）
pub(crate) fn sync(images: &mut Images, gpu: &Gpu, seen: &mut Seen) -> bool {
    let revision = external_revision();
    if revision == seen.revision {
        return false;
    }
    seen.revision = revision;
    let entries = entries();
    let mut changed = false;
    seen.entries.retain(|id, _| {
        let keep = entries.get(id).is_some_and(|e| e.device == gpu.id());
        if !keep {
            images.remove_external(*id);
            changed = true;
        }
        keep
    });
    for (&id, entry) in entries.iter() {
        if entry.device != gpu.id() {
            continue;
        }
        let state = (entry.replaced, entry.content);
        match seen.entries.get(&id) {
            Some(&old) if old == state => {}
            Some(&(replaced, _)) if replaced == entry.replaced => {
                seen.entries.insert(id, state);
                changed = true;
            }
            _ => {
                images.set_external(&gpu.device, id, &entry.texture);
                seen.entries.insert(id, state);
                changed = true;
            }
        }
    }
    changed
}
