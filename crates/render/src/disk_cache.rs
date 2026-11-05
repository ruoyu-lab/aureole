//! 磁盘上的管线缓存：后端支持时（wgpu 的 `PIPELINE_CACHE`，现在只有 Vulkan），编译好的管线存进应用的缓存目录，
//! 下次启动时交给驱动，省掉从中间代码重新生成机器码的那一步。
//!
//! - 目录由运行时启动时设（[`set_pipeline_cache_dir`]），没设时（测试、离屏渲染、工具）不存盘。
//! - 每个设备一份，[`crate::Gpu`] 拿着：打开设备时按适配器的键（厂商、设备、驱动，`wgpu::util::pipeline_cache_key`）
//!   找文件，交给 wgpu 建缓存；用 [`crate::Renderer::from_gpu`] 建的渲染器编译管线时都用它。
//!   不按 `wgpu::Device` 查全局表：设备比较的只是编号，不同实例上的设备编号会相同，查到别的实例的缓存时 wgpu 会 panic。
//! - 存盘：后台预编译完、渲染器释放（关窗口、退出）时，缓存比上次存的大了才写；先写临时文件再改名，不会只写一半。
//! - 驱动自己大多也有磁盘缓存（Mesa、NVIDIA），这里是不依赖驱动的一层；Metal、DX12、GL 上没有这个功能，什么都不做。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 管线缓存存在哪个目录（应用的缓存目录下，运行时启动时设）；`None` 时不存盘。之后打开的设备才用它
pub fn set_pipeline_cache_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(PoisonError::into_inner) = dir;
}

/// 一个设备的管线缓存和它的文件
pub(crate) struct DiskCache {
    pub cache: wgpu::PipelineCache,
    path: PathBuf,
    /// 上次存的（或者打开时用上的）字节数：没变大时不再写
    saved: Mutex<usize>,
}

impl DiskCache {
    /// 设备打开以后：设了目录、后端支持时读文件、建缓存
    pub(crate) fn open(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Option<Arc<Self>> {
        if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return None;
        }
        let dir = DIR.lock().unwrap_or_else(PoisonError::into_inner).clone()?;
        let key = wgpu::util::pipeline_cache_key(&adapter.get_info())?;
        let path = dir.join(key);
        let data = std::fs::read(&path).ok();
        let descriptor = wgpu::PipelineCacheDescriptor { label: Some("pipelines"), data: data.as_deref(), fallback: true };
        // SAFETY: `data` 只可能是这个文件里的：文件名是适配器的键，内容只由下面的 `save` 写，写的是同一个键的适配器上
        // `PipelineCache::get_data` 的结果（先写临时文件再改名，不会只写了一半）。wgpu 先核对它自己加的头（版本、后端、
        // 适配器的键），驱动再按 Vulkan 的约定核对它的头（厂商、设备、缓存的 UUID），对不上时当作空的缓存（`fallback`）。
        // 别人故意改了这个用户目录里的文件不在这里防得住，wgpu 的文档说明了这一点
        let cache = unsafe { device.create_pipeline_cache(&descriptor) };
        // 用上了多少（文件对不上这个驱动时是空的）：之后比它大了才写
        let saved = cache.get_data().map_or(0, |d| d.len());
        tracing::debug!(target: crate::pipeline::COMPILE_LOG, path = %path.display(), read = data.as_ref().map_or(0, Vec::len), used = saved, "管线缓存");
        Some(Arc::new(Self { cache, path, saved: Mutex::new(saved) }))
    }

    /// 比上次存的大了就写进文件（临时文件再改名）。写不了只记一条警告：下次启动照样从头编译
    pub(crate) fn save(&self) {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(data) = self.cache.get_data() else { return };
        if data.len() <= *saved {
            return;
        }
        match write(&self.path, &data) {
            Ok(()) => {
                *saved = data.len();
                tracing::debug!(target: crate::pipeline::COMPILE_LOG, path = %self.path.display(), bytes = data.len(), "存了管线缓存");
            }
            Err(error) => tracing::warn!(path = %self.path.display(), "存不了管线缓存：{error}"),
        }
    }
}

fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}
