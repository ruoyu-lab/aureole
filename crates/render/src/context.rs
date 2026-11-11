//! GPU 适配器和设备：按偏好和退路选适配器，按适配器的能力要设备，设备丢失后重新创建一次。
//!
//! 选适配器的先后（[`try_order`]）：主要后端（Metal、DX12、Vulkan）上偏好的那种显卡（省电：驱动窗口所在显示器的那块，
//! 不知道时集成；高性能：独立）→ 另一种 → 主要后端上的软件适配器（WARP、lavapipe）→ 次要后端（GL）上的，同样先硬件再软件（llvmpipe）。
//! 同一块显卡上平台原生的后端在前：Windows 上 DX12 先于 Vulkan（交换链走 DXGI 的翻转模型，改窗口大小只调整缓冲，
//! 能放到系统合成的对象上；Vulkan 每次改大小要重建交换链）。
//! 一个适配器打不开（能力不够、要设备失败）就试下一个。枚举出来的都不行时，
//! 再让 wgpu 按窗口的表面直接找一次（先找硬件，再 `force_fallback_adapter` 找软件）。都失败时返回 [`GpuError`]。
//!
//! GL 排在软件渲染之后：wgpu 的 GL 后端是尽力而为的，同一块显卡的 Vulkan、DX12 驱动不行时 GL 驱动多半也靠不住；
//! 确实只有 GL 驱动可用的老显卡用 `WGPU_BACKEND=gl` 只开 GL 后端。

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// GPU 适配器、设备和队列，所有窗口共用一份
pub struct Gpu {
    /// 选中的适配器
    pub adapter: wgpu::Adapter,
    /// 逻辑设备，用来创建缓冲、纹理和管线
    pub device: wgpu::Device,
    /// 提交命令和写缓冲的队列
    pub queue: wgpu::Queue,
    /// 选中的适配器：名字、后端、种类
    pub info: GpuInfo,
    lost: Arc<AtomicBool>,
    /// 没被错误范围接住的错误数（[`Gpu::error_count`]）
    errors: Arc<AtomicU64>,
    /// 设备丢失后用来重新创建
    instance: wgpu::Instance,
    preference: GpuPreference,
    display: Option<DisplayGpu>,
    /// 进程里第几个设备（重新创建时认出是哪一个丢了）
    id: u64,
    /// 0：最初的设备；1：丢失后重新创建的
    generation: u32,
    /// 这个设备的管线缓存（设了目录、后端支持时），见 `disk_cache.rs`
    pub(crate) disk: Option<Arc<crate::disk_cache::DiskCache>>,
}

/// 选 GPU 的偏好（[`Gpu::request`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GpuPreference {
    /// 省电（默认）：双显卡的机器上用集成显卡，不唤醒独立显卡。界面应用用它足够快
    #[default]
    LowPower,
    /// 高性能：双显卡的机器上用独立显卡（大量 3D、很重的自定义着色器）
    HighPerformance,
    /// 只用软件渲染的适配器（Windows 的 WARP、Linux 的 llvmpipe / lavapipe）：排查驱动问题、
    /// 在没有 GPU 的机器上测试。macOS 没有软件适配器，这时 [`Gpu::request`] 返回 [`GpuError::NoAdapter`]
    Software,
}

/// 驱动窗口所在显示器的那块 GPU：PCI 的厂商号、设备号，和 `wgpu::AdapterInfo` 的 `vendor`、`device` 对得上。
/// 省电偏好下先用它（[`Gpu::request_for_display`]）：显示器接在独立显卡上时（Windows 的台式机常这样），用集成显卡画
/// 并不省电（独立显卡照样醒着在合成、在输出），每帧还要把画面跨显卡拷过去，改窗口大小时尤其跟不上
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayGpu {
    /// PCI 厂商号
    pub vendor: u32,
    /// PCI 设备号
    pub device: u32,
}

impl GpuPreference {
    /// 环境变量 `AUREOLE_GPU`：`low-power`、`high-performance`、`software`。没设或认不出时为 `None`。
    /// 应用（[`Gpu::new`]、`App`）用它覆盖代码里的偏好，方便排查
    pub fn from_env() -> Option<Self> {
        Self::parse(&std::env::var("AUREOLE_GPU").ok()?)
    }

    /// 认 `low-power`、`high-performance`、`software`（不分大小写，`-` 和 `_` 都行）
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "low-power" | "low" => Some(Self::LowPower),
            "high-performance" | "high" => Some(Self::HighPerformance),
            "software" | "fallback" => Some(Self::Software),
            _ => None,
        }
    }
}

/// 适配器的种类
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GpuKind {
    /// 集成显卡
    Integrated,
    /// 独立显卡
    Discrete,
    /// 软件渲染（在 CPU 上跑）
    Software,
    /// 虚拟机里的虚拟 GPU
    Virtual,
    /// 说不清（GL 常常不报种类）
    Other,
}

impl From<wgpu::DeviceType> for GpuKind {
    fn from(kind: wgpu::DeviceType) -> Self {
        match kind {
            wgpu::DeviceType::IntegratedGpu => Self::Integrated,
            wgpu::DeviceType::DiscreteGpu => Self::Discrete,
            wgpu::DeviceType::Cpu => Self::Software,
            wgpu::DeviceType::VirtualGpu => Self::Virtual,
            wgpu::DeviceType::Other => Self::Other,
        }
    }
}

impl fmt::Display for GpuKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integrated => "集成显卡",
            Self::Discrete => "独立显卡",
            Self::Software => "软件渲染",
            Self::Virtual => "虚拟 GPU",
            Self::Other => "种类不明",
        })
    }
}

/// 一个适配器：名字、后端、种类、驱动
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GpuInfo {
    /// 名字，比如 "Apple M4 Pro"、"Microsoft Basic Render Driver"
    pub name: String,
    /// 图形接口：Metal、DX12、Vulkan、GL
    pub backend: wgpu::Backend,
    /// 集成、独立、软件……
    pub kind: GpuKind,
    /// 驱动名和版本（有的后端不报，是空的）
    pub driver: String,
}

impl GpuInfo {
    fn of(info: &wgpu::AdapterInfo) -> Self {
        let driver = [info.driver.as_str(), info.driver_info.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        Self { name: info.name.clone(), backend: info.backend, kind: info.device_type.into(), driver }
    }
}

impl fmt::Display for GpuInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let backend = match self.backend {
            wgpu::Backend::Metal => "Metal",
            wgpu::Backend::Dx12 => "DX12",
            wgpu::Backend::Vulkan => "Vulkan",
            wgpu::Backend::Gl => "GL",
            other => other.to_str(),
        };
        write!(f, "{}（{backend}，{}）", self.name, self.kind)
    }
}

/// 找不到能用的 GPU
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GpuError {
    /// 一个适配器都没有（没装显卡驱动；远程桌面、虚拟机里没有 GPU；要软件渲染而这个系统没有）
    NoAdapter {
        /// 要的是什么
        preference: GpuPreference,
    },
    /// 有适配器，但都用不了：每个适配器和原因
    Unusable(Vec<(GpuInfo, String)>),
    /// 设备丢失后已经重新创建过一次，又丢失了
    LostAgain,
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter { preference: GpuPreference::Software } => {
                write!(f, "找不到软件渲染的 GPU 适配器（这个系统没有，或者图形驱动没有提供）")
            }
            Self::NoAdapter { .. } => write!(f, "找不到可用的 GPU 适配器：请确认装好了显卡驱动（远程桌面、虚拟机里可能没有 GPU）"),
            Self::Unusable(tried) => {
                write!(f, "找到 {} 个 GPU 适配器，但都用不了：", tried.len())?;
                tried.iter().try_for_each(|(info, reason)| write!(f, "\n  · {info}：{reason}"))
            }
            Self::LostAgain => write!(f, "GPU 设备又丢失了（丢失后已经重新创建过一次，不再重试）"),
        }
    }
}

impl std::error::Error for GpuError {}

/// 创建 wgpu 实例，后端由环境变量 `WGPU_BACKEND` 决定（默认按平台选择）
pub fn instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env())
}

/// 同 [`instance`]，带上窗口系统的连接（事件循环的显示句柄）：Linux 上 GL 后端（EGL）在 Wayland 上要它才能在窗口上画。
///
/// Windows 上：
/// - 没设 `WGPU_BACKEND` 时只开 DX12（有自己的软件渲染 WARP 兜底）：加载 Vulkan、GL 的驱动和枚举它们的适配器要一两百毫秒，
///   启动时白白等着。DX12 一个能用的适配器都没有时，调用方用 [`instance_with_all_backends`] 再来一次；
/// - 调试构建也不默认打开图形接口的校验层：装了"图形工具"的机器上 D3D12 的调试层会跟着打开，每次提交、
///   改交换链大小都慢很多，用框架写的应用 `cargo run` 起来就卡。要校验时设环境变量 `WGPU_VALIDATION=1`
pub fn instance_with_display(display: impl wgpu::rwh::HasDisplayHandle + fmt::Debug + Send + Sync + 'static) -> wgpu::Instance {
    window_instance(display, true)
}

/// 同 [`instance_with_display`]，开全部的后端：Windows 上只开 DX12 的实例找不到能用的适配器时的退路
pub fn instance_with_all_backends(display: impl wgpu::rwh::HasDisplayHandle + fmt::Debug + Send + Sync + 'static) -> wgpu::Instance {
    window_instance(display, false)
}

/// [`instance_with_display`] 只开了平台原生的后端（Windows 上没设 `WGPU_BACKEND` 时）：找不到能用的适配器时要换全部的后端再来
pub fn only_native_backend() -> bool {
    cfg!(target_os = "windows") && std::env::var_os("WGPU_BACKEND").is_none()
}

fn window_instance(display: impl wgpu::rwh::HasDisplayHandle + fmt::Debug + Send + Sync + 'static, native_only: bool) -> wgpu::Instance {
    let mut descriptor = wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display));
    if cfg!(target_os = "windows") {
        descriptor.flags = wgpu::InstanceFlags::empty().with_env();
        if native_only && only_native_backend() {
            descriptor.backends = wgpu::Backends::DX12;
        }
    }
    wgpu::Instance::new(descriptor)
}

/// 支持就开启的可选功能：时间戳查询测 GPU 耗时（软件适配器、部分驱动没有，没有就不测）；
/// 管线缓存（只有 Vulkan 有）把编译好的管线存进磁盘，下次启动用（见 `disk_cache.rs`）
const OPTIONAL_FEATURES: wgpu::Features = wgpu::Features::TIMESTAMP_QUERY.union(wgpu::Features::PIPELINE_CACHE);

impl Gpu {
    /// 进程里第几个设备：设备丢失、重新创建以后是新的编号。建在设备上的资源（外部纹理）用它认是不是这个设备的
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 按 `AUREOLE_GPU`（没设时省电）选适配器；找不到能用的 GPU 时 panic。测试、工具里用，应用里用 [`Gpu::request`]
    pub fn new(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Self {
        // 有意 panic：这是给测试、示例、工具的便捷入口，找不到 GPU 时它们本来也做不下去。应用走 `request` 拿到错误
        Self::request(instance, surface, GpuPreference::from_env().unwrap_or_default()).unwrap_or_else(|e| panic!("{e}"))
    }

    /// 按偏好和退路选适配器、要设备（见模块说明）。`surface` 是要画上去的窗口表面，离屏时为 `None`
    pub fn request(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'_>>, preference: GpuPreference) -> Result<Self, GpuError> {
        Self::request_for_display(instance, surface, preference, None)
    }

    /// 同 [`Gpu::request`]，知道窗口所在的显示器由哪块 GPU 驱动（`display`）：省电偏好下先用它
    pub fn request_for_display(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        preference: GpuPreference,
        display: Option<DisplayGpu>,
    ) -> Result<Self, GpuError> {
        Self::open(instance, surface, preference, display, 0)
    }

    fn open(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        preference: GpuPreference,
        display: Option<DisplayGpu>,
        generation: u32,
    ) -> Result<Self, GpuError> {
        let start = Instant::now();
        let adapters: Vec<wgpu::Adapter> = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
            .into_iter()
            .filter(|adapter| surface.is_none_or(|surface| adapter.is_surface_supported(surface)))
            .collect();
        tracing::debug!(target: STARTUP_LOG, "枚举 GPU 适配器（{} 个能用）：{:.1} ms", adapters.len(), start.elapsed().as_secs_f64() * 1000.0);
        let candidates: Vec<Candidate> = adapters.iter().map(|a| Candidate::of(&a.get_info(), display)).collect();
        let (chosen, failed) = choose(&candidates, preference, |i| open_device(&adapters[i]));
        let mut tried: Vec<(GpuInfo, String)> =
            failed.into_iter().map(|(i, reason)| (GpuInfo::of(&adapters[i].get_info()), reason)).collect();
        let mut chosen = chosen.map(|(i, device)| (adapters[i].clone(), device));
        // 枚举出来的都不行：让 wgpu 按表面直接找（有的后端只有带着表面才找得到适配器），先硬件再软件
        if chosen.is_none() {
            let power = match preference {
                GpuPreference::HighPerformance => wgpu::PowerPreference::HighPerformance,
                _ => wgpu::PowerPreference::LowPower,
            };
            for fallback in [false, true] {
                if preference == GpuPreference::Software && !fallback {
                    continue;
                }
                let options = wgpu::RequestAdapterOptions {
                    power_preference: power,
                    force_fallback_adapter: fallback,
                    compatible_surface: surface,
                    ..Default::default()
                };
                let Ok(adapter) = pollster::block_on(instance.request_adapter(&options)) else { continue };
                let info = GpuInfo::of(&adapter.get_info());
                if tried.iter().any(|(t, _)| *t == info) || (preference == GpuPreference::Software && info.kind != GpuKind::Software) {
                    continue;
                }
                match open_device(&adapter) {
                    Ok(device) => {
                        chosen = Some((adapter, device));
                        break;
                    }
                    Err(reason) => tried.push((info, reason)),
                }
            }
        }
        let Some((adapter, (device, queue))) = chosen else {
            return Err(if tried.is_empty() { GpuError::NoAdapter { preference } } else { GpuError::Unusable(tried) });
        };
        let info = GpuInfo::of(&adapter.get_info());
        for (skipped, reason) in &tried {
            tracing::warn!("GPU：跳过 {skipped}：{reason}");
        }
        tracing::info!(gpu = %info, "选中的 GPU");
        let lost = Arc::new(AtomicBool::new(false));
        let (flag, name) = (lost.clone(), info.to_string());
        device.set_device_lost_callback(move |reason, message| {
            flag.store(true, Ordering::Release);
            tracing::error!("GPU 设备丢失（{name}，{reason:?}）：{message}");
        });
        let errors = Arc::new(AtomicU64::new(0));
        let (count, name) = (errors.clone(), info.to_string());
        // 没被错误范围接住的错误：wgpu 默认 panic，界面因为一个缓冲建坏了就整个退出不值得。记下来接着跑：
        // 出错的资源 wgpu 标成无效，用到它的绘制不画，下一帧照常
        device.on_uncaptured_error(Arc::new(move |error| {
            count.fetch_add(1, Ordering::Relaxed);
            log_uncaptured(&name, &error);
        }));
        scene::set_max_texture_size(device.limits().max_texture_dimension_2d);
        let disk = crate::disk_cache::DiskCache::open(&adapter, &device);
        if let Ok(mut active) = ACTIVE.lock() {
            *active = Some(info.clone());
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // 枚举时每个适配器都建了一份设备（DX12）、一份实例（Vulkan）：没用上的放掉要几百毫秒（这台机器上 8 个适配器约 450 毫秒），
        // 不放在启动的路上，交给后台线程
        release_in_background(adapters);
        Ok(Self { adapter, device, queue, info, lost, errors, instance: instance.clone(), preference, display, id, generation, disk })
    }

    /// 设备丢失了（驱动重置、显卡被拔掉、休眠唤醒后驱动出错……）：之后在它上面的操作都没有效果
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// 这个设备上没被错误范围接住的错误数：校验错误（渲染器或自定义代码的缺陷）、显存不足、驱动的内部错误。
    /// 每个都已经用 `tracing::error!` 记下，不会 panic；测试里查它是不是 0，检查器里显示它
    pub fn error_count(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// 设备丢失后重新创建（按原来的偏好重新选适配器）。**整个进程只试一次**：几个窗口同时发现丢失时
    /// 共用同一个新设备；重新创建的设备再丢失时返回 [`GpuError::LostAgain`]。
    /// 调用方要用新设备重建自己的渲染器、重新配置表面、重新上传图集和图片
    pub fn recover(&self, surface: Option<&wgpu::Surface<'_>>) -> Result<Arc<Gpu>, GpuError> {
        if self.generation > 0 {
            return Err(GpuError::LostAgain);
        }
        let mut slot = RECOVERED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, result)) = slot.as_ref()
            && *id == self.id
        {
            return result.clone();
        }
        tracing::info!("GPU：重新创建设备");
        let result = Self::open(&self.instance, surface, self.preference, self.display, self.generation + 1).map(Arc::new);
        *slot = Some((self.id, result.clone()));
        result
    }

    /// 选适配器时的偏好
    pub fn preference(&self) -> GpuPreference {
        self.preference
    }
}

impl Drop for Gpu {
    /// 设备不用了：管线缓存存一次盘
    fn drop(&mut self) {
        if let Some(disk) = &self.disk {
            disk.save();
        }
    }
}

/// 在后台线程上放掉用不上的适配器；开不了线程时就地放
fn release_in_background(adapters: Vec<wgpu::Adapter>) {
    let mut adapters = Some(adapters);
    let spawned = std::thread::Builder::new().name("释放适配器".into()).spawn({
        let adapters = adapters.take();
        move || drop(adapters)
    });
    if spawned.is_err() {
        drop(adapters);
    }
}

/// 记一个没被错误范围接住的 GPU 错误
fn log_uncaptured(gpu: &str, error: &wgpu::Error) {
    match error {
        wgpu::Error::OutOfMemory { source } => tracing::error!("GPU 显存不足（{gpu}）：{source}"),
        wgpu::Error::Validation { description, .. } => tracing::error!("GPU 校验错误（{gpu}），出错的资源不画：{description}"),
        wgpu::Error::Internal { description, .. } => tracing::error!("GPU 内部错误（{gpu}）：{description}"),
    }
}

/// 最近一次选中的适配器（性能测量、检查器里显示）；还没有设备时为 `None`
pub fn active_adapter() -> Option<GpuInfo> {
    ACTIVE.lock().ok()?.clone()
}

static ACTIVE: Mutex<Option<GpuInfo>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
/// 设备丢失后重新创建的结果：(丢失的设备, 新设备或错误)
type Recovered = Option<(u64, Result<Arc<Gpu>, GpuError>)>;
static RECOVERED: Mutex<Recovered> = Mutex::new(None);

/// 在一个适配器上要设备：能力按默认的要，适配器达不到的项按它实际的（软件、老显卡）；
/// 达不到渲染器最低要求时不要。要设备失败时去掉可选功能再试一次。
/// 能力打了折扣的设备先在错误范围里建一次渲染器，建不起来（着色器、绑定超出能力）就算这个适配器不能用
fn open_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    let start = Instant::now();
    let result = request_device(adapter);
    let outcome = if result.is_ok() { "成功" } else { "失败" };
    tracing::debug!(target: STARTUP_LOG, "打开 {}（{outcome}）：{:.1} ms", GpuInfo::of(&adapter.get_info()), start.elapsed().as_secs_f64() * 1000.0);
    result
}

/// 启动时选适配器、要设备的耗时记在这个目标下（debug 级别），和运行时的启动分段计时同一个目标
const STARTUP_LOG: &str = "aureole::startup";

fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
    let limits = wgpu::Limits::default().or_worse_values_from(&adapter.limits());
    let downlevel = adapter.get_downlevel_capabilities();
    if let Some(reason) = unmet_requirements(&limits, downlevel.flags) {
        return Err(reason);
    }
    let optional = adapter.features() & OPTIONAL_FEATURES;
    let mut attempts = vec![optional];
    if !optional.is_empty() {
        attempts.push(wgpu::Features::empty());
    }
    let mut error = String::new();
    for features in attempts {
        let descriptor = wgpu::DeviceDescriptor {
            label: Some("device"),
            required_features: features,
            required_limits: limits.clone(),
            // 界面的每笔分配都很小：Performance 下 Vulkan、DX12 的分配器一开始就向驱动要 128MB、64MB 起的块，
            // 一个小窗口也占两三百 MB（核显上算进进程的内存）
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        };
        match pollster::block_on(adapter.request_device(&descriptor)) {
            Ok((device, queue)) => {
                let full = downlevel.flags.contains(wgpu::DownlevelFlags::compliant()) && limits == wgpu::Limits::default();
                if !full && let Some(reason) = probe(&device) {
                    return Err(reason);
                }
                return Ok((device, queue));
            }
            Err(e) => error = e.to_string(),
        }
    }
    Err(format!("创建设备失败：{error}"))
}

/// 在错误范围里建一次渲染器，编译窗口第一帧、分组和模糊要的管线，出错时返回原因
fn probe(device: &wgpu::Device) -> Option<String> {
    use crate::pipeline::Entry;
    use scene::ShaderFeatures;
    let start = Instant::now();
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut renderer = crate::Renderer::new(device, wgpu::TextureFormat::Bgra8UnormSrgb);
    renderer.compile(
        device,
        &[
            (Entry::Quad, wgpu::TextureFormat::Bgra8UnormSrgb, ShaderFeatures::NONE),
            (Entry::Quad, crate::encoder::OFFSCREEN, ShaderFeatures::NONE),
            (Entry::Blur, crate::encoder::OFFSCREEN, ShaderFeatures::NONE),
        ],
    );
    drop(renderer);
    let error = pollster::block_on(scope.pop()).map(|e| format!("渲染器建不起来：{e}"));
    tracing::debug!(target: STARTUP_LOG, "能力打了折扣的设备：试建一次渲染器 {:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    error
}

/// 渲染器对设备的最低要求（绑定组、存储缓冲、纹理大小、顶点着色器读存储缓冲）；满足时为 `None`
pub(crate) fn unmet_requirements(limits: &wgpu::Limits, flags: wgpu::DownlevelFlags) -> Option<String> {
    let checks = [
        (limits.max_bind_groups >= 4, "绑定组少于 4 个"),
        (limits.max_storage_buffers_per_shader_stage >= 3, "每个着色器阶段的存储缓冲少于 3 个"),
        (limits.max_sampled_textures_per_shader_stage >= 8, "每个着色器阶段的纹理少于 8 张"),
        (limits.max_texture_dimension_2d >= 2048, "纹理边长上限小于 2048"),
        (flags.contains(wgpu::DownlevelFlags::VERTEX_STORAGE), "顶点着色器不能读存储缓冲"),
    ];
    let unmet: Vec<&str> = checks.iter().filter(|(ok, _)| !ok).map(|(_, reason)| *reason).collect();
    (!unmet.is_empty()).then(|| format!("能力不够：{}", unmet.join("、")))
}

/// 排先后时看适配器的后端、种类，是不是平台原生的后端，驱动不驱动窗口所在的显示器
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub backend: wgpu::Backend,
    pub kind: GpuKind,
    /// 这个平台原生的图形接口（Windows 上 Vulkan 不是，DX12 是；别的平台都算是）
    pub native: bool,
    /// 驱动窗口所在的显示器；不知道是哪块时是 `None`
    pub on_display: Option<bool>,
}

impl Candidate {
    fn of(info: &wgpu::AdapterInfo, display: Option<DisplayGpu>) -> Self {
        let native = !(cfg!(target_os = "windows") && info.backend == wgpu::Backend::Vulkan);
        let on_display = display.map(|d| d.vendor == info.vendor && d.device == info.device);
        Self { backend: info.backend, kind: info.device_type.into(), native, on_display }
    }
}

/// 试适配器的先后（`candidates` 的下标）。不考虑的（不是真正的图形后端；只要软件渲染时的硬件适配器）不在里面
pub(crate) fn try_order(candidates: &[Candidate], preference: GpuPreference) -> Vec<usize> {
    let mut order: Vec<(usize, RankKey)> =
        candidates.iter().enumerate().filter_map(|(i, c)| rank(*c, preference).map(|key| (i, key))).collect();
    order.sort_by_key(|&(i, key)| (key, i));
    order.into_iter().map(|(i, _)| i).collect()
}

/// 打不开的适配器：下标和原因，按试的先后
pub(crate) type Failures = Vec<(usize, String)>;

/// 按 [`try_order`] 的先后逐个打开（`open` 收到下标），第一个成功的胜出。返回胜出的下标和结果，以及打不开的
pub(crate) fn choose<D>(
    candidates: &[Candidate],
    preference: GpuPreference,
    mut open: impl FnMut(usize) -> Result<D, String>,
) -> (Option<(usize, D)>, Failures) {
    let mut failed = Vec::new();
    for i in try_order(candidates, preference) {
        match open(i) {
            Ok(device) => return (Some((i, device)), failed),
            Err(reason) => failed.push((i, reason)),
        }
    }
    (None, failed)
}

/// 排序键：(次要后端, 软件, 省电时不驱动窗口所在的显示器, 种类, 不是原生后端)，越小越先
type RankKey = (u8, u8, u8, u8, u8);

/// 一个候选的 [`RankKey`]；不考虑的是 `None`
fn rank(candidate: Candidate, preference: GpuPreference) -> Option<RankKey> {
    let secondary = match candidate.backend {
        wgpu::Backend::Metal | wgpu::Backend::Dx12 | wgpu::Backend::Vulkan => false,
        wgpu::Backend::Gl => true,
        _ => return None,
    };
    let software = candidate.kind == GpuKind::Software;
    if preference == GpuPreference::Software && !software {
        return None;
    }
    let kind = match (candidate.kind, preference) {
        (GpuKind::Discrete, GpuPreference::HighPerformance) | (GpuKind::Integrated, GpuPreference::LowPower) => 0,
        (GpuKind::Integrated | GpuKind::Discrete, _) => 1,
        (GpuKind::Other, _) => 2,
        (GpuKind::Virtual, _) => 3,
        (GpuKind::Software, _) => 4,
    };
    let off_display = preference == GpuPreference::LowPower && candidate.on_display == Some(false);
    Some((u8::from(secondary), u8::from(software), u8::from(off_display), kind, u8::from(!candidate.native)))
}
