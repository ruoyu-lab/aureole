//! Windows 上的快路径：CEF 交来的 D3D11 共享纹理（NT 句柄）在框架的 D3D12 设备上直接打开（不复制），
//! 在自己的一条 GPU 队列上一次复制进登记的纹理（两边的格式只差 sRGB）。
//!
//! - **打开过的留着**（[`Opened`]）：CEF 的几张纹理轮着用，每帧交来的句柄是新复制出来的，底下还是那几张。
//!   每帧重新打开（`OpenSharedHandle`）的话 CPU 上要一毫秒多，系统每次还要把它重新映射进这个进程；
//!   留着打开过的，用 `CompareObjectHandles` 认出是哪一张。
//! - **复制在自己的队列上**（[`Copier`]）：读共享纹理的命令要等 Chromium 把这一帧写完才执行，放在 wgpu 的队列上的话
//!   框架合成窗口的命令也跟着等（任务管理器里这个进程的 GPU 占用也被算得很高）。自己的队列和 wgpu 的队列用 fence
//!   互相等：复制之前等 wgpu 队列里之前的命令（可能还在采样目标纹理）做完，wgpu 之后的命令等复制做完。
//!   目标纹理是自己建的 D3D12 资源，平时一直在"着色器读"的状态，wgpu 按这个状态记着它，复制前后自己转换。
//! - 回调不在 CPU 上等复制做完：CEF 的几张共享纹理是轮着用的，交回去的那张过两帧才会再往里画，复制早就做完了
//!   （复制本身要等内核把这一帧画完，一两毫秒；在 CPU 上等的话 CEF 的界面线程跟着等，帧开始的信号会被丢掉）。
//!   一张纹理又交来时上一次从它的复制还没做完，说明不是这样轮着用的，之后每帧都等（见 [`Copier::copy`]）。

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cef::AcceleratedPaintInfo;
use cef::sys::cef_color_type_t;
use windows::Win32::Foundation::{CloseHandle, CompareObjectHandles, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_COMMAND_QUEUE_FLAG_NONE, D3D12_FENCE_FLAG_NONE, D3D12_HEAP_FLAG_NONE,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
    D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_BOX, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATES, D3D12_RESOURCE_TRANSITION_BARRIER, D3D12_TEXTURE_COPY_LOCATION,
    D3D12_TEXTURE_COPY_LOCATION_0, D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12CommandAllocator, ID3D12CommandList,
    ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};
use windows::core::Interface;

use aureole_render::Gpu;

type Dx12 = wgpu::wgc::api::Dx12;

/// 最多留几张打开过的纹理（CEF 轮着用的一般三四张；改大小以后换一批，旧的按最久没用的先放掉）
const KEEP: usize = 8;

/// 目标纹理平时的状态：wgpu 的"着色器读"（`TextureUses::RESOURCE`）对应的 D3D12 状态
const SAMPLED: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0);

/// 打开过的共享纹理
#[derive(Default)]
pub(super) struct Opened {
    entries: Vec<Entry>,
    /// 建在哪个设备上（[`Gpu::id`]）：换了设备全部作废
    device: u64,
    /// 第几帧，用来找最久没用的
    clock: u64,
}

struct Entry {
    /// 复制出来的句柄，认同一张纹理用（留着它，系统里这张纹理的对象就还在，句柄的值不会被别的纹理重用）
    handle: Handle,
    resource: ID3D12Resource,
    used: u64,
    /// 最近一次从它复制的编号（[`Copier`] 的 fence 值），还没复制过是 0
    copied: u64,
}

/// 打开的一帧：共享纹理的资源和它的格式
pub(super) struct Shared {
    pub resource: ID3D12Resource,
    pub format: wgpu::TextureFormat,
    pub size: [u32; 2],
    /// 在 [`Opened`] 里是第几张（复制完用 [`Opened::copied`] 记下）
    pub index: usize,
    /// 上一次从这张复制的编号
    pub last_copy: u64,
}

/// 自己的一个句柄：丢掉时关上
struct Handle(HANDLE);

// SAFETY: 内核对象的句柄在进程里任何线程上都能用、能关
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: 句柄是这个进程自己的（复制出来的、建的），只关一次
        let _ = unsafe { CloseHandle(self.0) };
    }
}

impl Opened {
    /// `info` 里的共享纹理在 `gpu` 上的资源：打开过的直接用，没打开过的打开、留着。
    /// 不是 D3D12 后端、格式不认识、大小对不上、打不开（CEF 和框架不在同一块 GPU 上）时返回原因
    ///
    /// # Safety
    ///
    /// 只在 CEF 的离屏回调里调用：回调期间 `info` 里的句柄有效（句柄归 CEF，这里不关）
    pub(super) unsafe fn open(&mut self, gpu: &Gpu, info: &AcceleratedPaintInfo) -> Result<Shared, String> {
        let handle = HANDLE(info.shared_texture_handle);
        if handle.is_invalid() {
            return Err("没有共享纹理的句柄".into());
        }
        let (format, dxgi) = match *info.format.as_ref() {
            cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 => (wgpu::TextureFormat::Bgra8Unorm, DXGI_FORMAT_B8G8R8A8_UNORM),
            cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888 => (wgpu::TextureFormat::Rgba8Unorm, DXGI_FORMAT_R8G8B8A8_UNORM),
            other => return Err(format!("不认识的像素格式 {other:?}")),
        };
        let size = &info.extra.coded_size;
        let size = [size.width.max(0) as u32, size.height.max(0) as u32];
        let max = gpu.device.limits().max_texture_dimension_2d;
        if size[0] == 0 || size[1] == 0 || size[0] > max || size[1] > max {
            return Err(format!("大小不对：{}×{}", size[0], size[1]));
        }
        if self.device != gpu.id() {
            self.entries.clear();
            self.device = gpu.id();
        }
        self.clock += 1;
        // SAFETY: 两个句柄都有效（CEF 的在回调期间，自己的一直留着）；只比较是不是同一个对象
        if let Some((index, entry)) = self.entries.iter_mut().enumerate().find(|(_, e)| unsafe { CompareObjectHandles(e.handle.0, handle) }.as_bool())
            && matches(&entry.resource, size, dxgi)
        {
            entry.used = self.clock;
            return Ok(Shared { resource: entry.resource.clone(), format, size, index, last_copy: entry.copied });
        }
        // 大小变了：之前的那一批不会再用到
        self.entries.retain(|e| matches(&e.resource, size, dxgi));
        // SAFETY: 只在 D3D12 后端上取出底层设备；用它打开的资源只在同一个设备上用
        let hal = unsafe { gpu.device.as_hal::<Dx12>() }.ok_or("GPU 设备不是 D3D12 后端")?;
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: 句柄在回调期间有效；打开以后资源自己持有引用，和句柄无关
        unsafe { hal.raw_device().OpenSharedHandle(handle, &mut resource) }.map_err(|e| format!("打不开共享纹理（{e}）"))?;
        drop(hal);
        let resource = resource.ok_or("共享纹理打开以后是空的")?;
        if !matches(&resource, size, dxgi) {
            // SAFETY: 只读资源的描述
            let desc = unsafe { resource.GetDesc() };
            return Err(format!("共享纹理和这一帧对不上：{}×{} {:?}", desc.Width, desc.Height, desc.Format));
        }
        let mut copy = HANDLE::default();
        // SAFETY: 在本进程里复制本进程的句柄，结果写进 `copy`
        unsafe {
            let process = GetCurrentProcess();
            DuplicateHandle(process, handle, process, &mut copy, 0, false, DUPLICATE_SAME_ACCESS)
        }
        .map_err(|e| format!("复制不了共享纹理的句柄（{e}）"))?;
        if self.entries.len() >= KEEP
            && let Some(oldest) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.used).map(|(i, _)| i)
        {
            self.entries.swap_remove(oldest);
        }
        self.entries.push(Entry { handle: Handle(copy), resource: resource.clone(), used: self.clock, copied: 0 });
        tracing::debug!("打开了 CEF 的一张共享纹理（{}×{}，现在留着 {} 张）", size[0], size[1], self.entries.len());
        Ok(Shared { resource, format, size, index: self.entries.len() - 1, last_copy: 0 })
    }

    /// 从第 `index` 张复制了（编号 `value`）
    pub(super) fn copied(&mut self, index: usize, value: u64) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.copied = value;
        }
    }

    /// 网页关掉时：放掉打开过的纹理
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 资源是不是这一帧的大小、格式的 2D 纹理
fn matches(resource: &ID3D12Resource, size: [u32; 2], format: DXGI_FORMAT) -> bool {
    // SAFETY: 只读资源的描述
    let desc = unsafe { resource.GetDesc() };
    desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && desc.Width == u64::from(size[0])
        && desc.Height == size[1]
        && desc.DepthOrArraySize == 1
        && desc.MipLevels == 1
        && desc.SampleDesc.Count == 1
        && desc.Format == format
}

/// 在自己的队列上把共享纹理复制进目标纹理
pub(super) struct Copier {
    /// 建在哪个设备上（[`Gpu::id`]）
    device: u64,
    raw: ID3D12Device,
    queue: ID3D12CommandQueue,
    /// 轮着用的命令分配器、列表，和用它们的那次复制的编号：复制做完以前不能重用
    ring: [(ID3D12CommandAllocator, ID3D12GraphicsCommandList, u64); RING],
    /// 复制做完时发：wgpu 的队列采样之前（在 GPU 上）等它
    copied: ID3D12Fence,
    copied_value: u64,
    /// 每帧在 CPU 上等复制做完：内核的共享纹理不是轮着用的时候（见 [`Copier::copy`]）
    strict: bool,
    /// wgpu 的队列发：在它之前提交的命令（可能在采样目标纹理）都做完了
    idle: ID3D12Fence,
    idle_value: u64,
    event: Handle,
    /// 目标纹理：自己建的资源，和包成的 wgpu 纹理
    target: Option<(ID3D12Resource, wgpu::Texture)>,
    /// 开着的弹出层（下拉框这类）
    popup: Option<Popup>,
    /// 换掉了、可能还排在复制队列里的资源，和要等到的复制的编号
    retired: Vec<(ID3D12Resource, u64)>,
}

/// 弹出层：自己的纹理（平时在 COPY_SOURCE 状态）、大小、格式，叠在目标纹理的哪里（物理像素）
struct Popup {
    resource: ID3D12Resource,
    size: [u32; 2],
    format: wgpu::TextureFormat,
    at: [u32; 2],
}

// SAFETY: 这些 D3D12 对象都是自由线程的（引用计数、方法在任何线程上都能调）；
// 同一时刻只有拿着离屏绘制的锁的一个线程在录命令
unsafe impl Send for Copier {}

/// 一帧复制的结果
pub(super) struct Copied {
    /// 目标纹理（登记给渲染器的那张）
    pub texture: wgpu::Texture,
    /// 目标纹理是这一次新建的
    pub fresh: bool,
    /// 在 CPU 上等了多久（轮着用的命令还没做完、或者每帧都等的时候）
    pub waited: Duration,
    /// 这次复制的编号
    pub value: u64,
}

/// 轮着用几套命令分配器、列表
const RING: usize = 3;

impl Copier {
    /// 在 `gpu` 上建复制用的队列、命令、fence。不是 D3D12 后端时返回原因
    pub(super) fn new(gpu: &Gpu) -> Result<Self, String> {
        // SAFETY: 只在 D3D12 后端上取出底层设备，用它建的对象只在这个设备上用
        let hal = unsafe { gpu.device.as_hal::<Dx12>() }.ok_or("GPU 设备不是 D3D12 后端")?;
        let raw = hal.raw_device().clone();
        drop(hal);
        let failed = |what: &str, e: windows::core::Error| format!("建不了{what}（{e}）");
        // SAFETY: 都是普通的创建调用，参数是合法的描述；建好的对象归这个结构
        unsafe {
            let desc = D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, Priority: 0, Flags: D3D12_COMMAND_QUEUE_FLAG_NONE, NodeMask: 0 };
            let queue: ID3D12CommandQueue = raw.CreateCommandQueue(&desc).map_err(|e| failed("复制用的队列", e))?;
            let commands = || -> Result<_, String> {
                let allocator: ID3D12CommandAllocator = raw.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| failed("命令分配器", e))?;
                let list: ID3D12GraphicsCommandList =
                    raw.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).map_err(|e| failed("命令列表", e))?;
                list.Close().map_err(|e| failed("命令列表", e))?;
                Ok((allocator, list, 0))
            };
            let ring = [commands()?, commands()?, commands()?];
            let copied: ID3D12Fence = raw.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| failed("fence", e))?;
            let idle: ID3D12Fence = raw.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| failed("fence", e))?;
            let event = CreateEventW(None, false, false, None).map_err(|e| failed("事件", e))?;
            Ok(Self {
                device: gpu.id(),
                raw,
                queue,
                ring,
                copied,
                copied_value: 0,
                strict: false,
                idle,
                idle_value: 0,
                event: Handle(event),
                target: None,
                popup: None,
                retired: Vec::new(),
            })
        }
    }

    /// 建在哪个设备上
    pub(super) fn device(&self) -> u64 {
        self.device
    }

    /// 把 `source` 复制进目标纹理（大小、格式变了时先建一张新的）。
    ///
    /// 一般不在 CPU 上等复制做完：内核的几张共享纹理是轮着用的（三张时 0、1、2、0……），交回去的那张要过两帧
    /// 才会再往里画，复制早做完了（复制本身要等内核把这一帧画完，一两毫秒）。这张纹理又交来时上一次从它的复制
    /// 还没做完，说明不是这样轮着用的：之后每帧都等复制做完再返回（最多 `limit`）
    pub(super) fn copy(&mut self, gpu: &Gpu, source: &Shared, limit: Duration) -> Result<Copied, String> {
        let begin = Instant::now();
        self.check_rotation(source);
        let format = source.format.add_srgb_suffix();
        let fresh = !self.target.as_ref().is_some_and(|(_, t)| t.width() == source.size[0] && t.height() == source.size[1] && t.format() == format);
        if fresh {
            // 旧的目标纹理可能还在被复制：等它做完再放掉（复制排在后面，放掉以后才执行就出错了）
            let old = self.target.take();
            self.retire(old.map(|(resource, _)| resource));
            self.target = Some(self.create_target(gpu, source.size, format)?);
        }
        let (target, texture) = self.target.clone().expect("刚建好");
        let popup = self.popup.as_ref().map(|p| (p.resource.clone(), p.at, p.size));
        let value = self.submit(gpu, limit, |list| {
            // SAFETY: 目标纹理在 `SAMPLED` 状态（每次复制完都转回来），共享纹理允许同时访问、从 COMMON 状态隐式地当复制的来源；
            // 弹出层的纹理平时在 COPY_SOURCE 状态
            unsafe {
                transition(list, &target, SAMPLED, D3D12_RESOURCE_STATE_COPY_DEST);
                list.CopyResource(&target, &source.resource);
                // 弹出层（下拉框）开着：叠回去，它不在页面这一帧里
                if let Some((popup, at, size)) = &popup {
                    copy_region(list, &target, *at, popup, *size, source.size);
                }
                transition(list, &target, D3D12_RESOURCE_STATE_COPY_DEST, SAMPLED);
            }
        })?;
        Ok(Copied { texture, fresh, waited: begin.elapsed(), value })
    }

    /// 页面里的弹出层（下拉框这类）的一帧：复制进弹出层自己的纹理（留着，之后页面每一帧都叠上去），
    /// 再叠到目标纹理的 `at`（物理像素）上
    pub(super) fn copy_popup(&mut self, gpu: &Gpu, source: &Shared, at: [u32; 2], limit: Duration) -> Result<Copied, String> {
        let begin = Instant::now();
        self.check_rotation(source);
        let Some((target, texture)) = self.target.clone() else { return Err("页面还没有一帧".into()) };
        let format = source.format.add_srgb_suffix();
        let fits = self.popup.as_ref().is_some_and(|p| p.size == source.size && p.format == format);
        if !fits {
            let old = self.popup.take();
            self.retire(old.map(|p| p.resource));
            let resource = self.create_resource(source.size, format, D3D12_RESOURCE_STATE_COPY_SOURCE)?;
            self.popup = Some(Popup { resource, size: source.size, format, at });
        }
        let popup = self.popup.as_mut().expect("刚建好");
        popup.at = at;
        let (resource, size) = (popup.resource.clone(), popup.size);
        let bounds = [texture.width(), texture.height()];
        let value = self.submit(gpu, limit, |list| {
            // SAFETY: 弹出层的纹理平时在 COPY_SOURCE 状态，目标纹理在 `SAMPLED` 状态，复制完都转回来
            unsafe {
                transition(list, &resource, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
                list.CopyResource(&resource, &source.resource);
                transition(list, &resource, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
                transition(list, &target, SAMPLED, D3D12_RESOURCE_STATE_COPY_DEST);
                copy_region(list, &target, at, &resource, size, bounds);
                transition(list, &target, D3D12_RESOURCE_STATE_COPY_DEST, SAMPLED);
            }
        })?;
        Ok(Copied { texture, fresh: false, waited: begin.elapsed(), value })
    }

    /// 弹出层关了：之后的页面帧不再叠它
    pub(super) fn hide_popup(&mut self) {
        let old = self.popup.take();
        self.retire(old.map(|p| p.resource));
    }

    /// 交来的共享纹理上一次的复制还没做完：内核不是轮着用纹理的，之后每帧都在 CPU 上等（见 [`Copier::copy`]）
    fn check_rotation(&mut self, source: &Shared) {
        // SAFETY: 只读 fence 的值
        let done = unsafe { self.copied.GetCompletedValue() };
        if !self.strict && source.last_copy > done {
            self.strict = true;
            tracing::debug!("CEF 的共享纹理交回来时上一次的复制还没做完，之后每帧等复制做完");
        }
    }

    /// 不再用的资源：可能还排在复制队列里，最近一次复制做完以后才放掉
    fn retire(&mut self, resource: Option<ID3D12Resource>) {
        // SAFETY: 只读 fence 的值
        let done = unsafe { self.copied.GetCompletedValue() };
        self.retired.retain(|(_, value)| *value > done);
        if let Some(resource) = resource {
            self.retired.push((resource, self.copied_value));
        }
    }

    /// 在复制队列上录一批命令、提交：之前先在 GPU 上等 wgpu 队列里之前的命令（渲染线程合成窗口时可能在采样目标纹理）
    /// 做完，之后 wgpu 的命令（下一次合成）在 GPU 上等这批做完。返回这批的编号
    fn submit(&mut self, gpu: &Gpu, limit: Duration, record: impl FnOnce(&ID3D12GraphicsCommandList)) -> Result<u64, String> {
        // 这一套命令上次用的那次复制做完了才能重用
        let slot = (self.copied_value as usize + 1) % RING;
        if !self.wait(self.ring[slot].2, limit) {
            return Err("前面的复制还没做完".into());
        }
        let hal_queue = || {
            // SAFETY: 只在 D3D12 后端上取出底层队列，只用它登记要等、要发的 fence
            unsafe { gpu.queue.as_hal::<Dx12>() }.ok_or_else(|| "GPU 队列不是 D3D12 后端".to_owned())
        };
        // wgpu 的队列里之前提交的命令做完时发 `idle`：登记在下一次提交上，自己马上提交一次空的
        self.idle_value += 1;
        hal_queue()?.add_signal_fence(self.idle.clone(), self.idle_value);
        gpu.queue.submit([]);
        self.copied_value += 1;
        let (allocator, list, used) = &mut self.ring[slot];
        // SAFETY: 这一套命令上次的复制做完了（上面等过），分配器、列表可以重用；资源都在这个设备上
        unsafe {
            let fail = |e: windows::core::Error| format!("录不了复制的命令（{e}）");
            allocator.Reset().map_err(fail)?;
            list.Reset(&*allocator, None).map_err(fail)?;
            record(list);
            list.Close().map_err(fail)?;
            self.queue.Wait(&self.idle, self.idle_value).map_err(fail)?;
            self.queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().map_err(fail)?)]);
            self.queue.Signal(&self.copied, self.copied_value).map_err(fail)?;
        }
        *used = self.copied_value;
        hal_queue()?.add_wait_fence(self.copied.clone(), self.copied_value);
        if self.strict && !self.wait(self.copied_value, limit) {
            tracing::warn!("等 CEF 视图的一帧复制完没等到（{} 毫秒）", limit.as_millis());
        }
        Ok(self.copied_value)
    }

    /// 在 CPU 上等编号 `value` 的复制做完，最多 `limit`。等到了（或者早就做完了）是 `true`
    fn wait(&self, value: u64, limit: Duration) -> bool {
        // SAFETY: 只读 fence 的值、等自己的事件
        unsafe {
            self.copied.GetCompletedValue() >= value
                || (self.copied.SetEventOnCompletion(value, self.event.0).is_ok()
                    && WaitForSingleObject(self.event.0, limit.as_millis().min(u128::from(u32::MAX)) as u32) == WAIT_OBJECT_0)
        }
    }

    /// 建目标纹理：自己的 D3D12 资源（`SAMPLED` 状态），包成 wgpu 的纹理（wgpu 按 `RESOURCE` 记着它）
    fn create_target(&self, gpu: &Gpu, size: [u32; 2], format: wgpu::TextureFormat) -> Result<(ID3D12Resource, wgpu::Texture), String> {
        let resource = self.create_resource(size, format, SAMPLED)?;
        let extent = wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 };
        // SAFETY: `resource` 是这个设备上刚建的 2D 纹理，格式、层数、级数、大小和描述一致
        let hal_texture = unsafe { <Dx12 as wgpu::hal::Api>::Device::texture_from_raw(resource.clone(), format, wgpu::TextureDimension::D2, extent, 1, 1) };
        let desc = wgpu::TextureDescriptor {
            label: Some("CEF 视图的一帧"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: 底层纹理建在这个设备上，描述和它一致；它在"着色器读"的状态
        let texture = unsafe { gpu.device.create_texture_from_hal::<Dx12>(hal_texture, &desc, wgpu::TextureUses::RESOURCE) };
        Ok((resource, texture))
    }

    /// 建一张 `size`、`format`（sRGB 的 8 位格式）的 2D 纹理资源，开始在 `state` 状态
    fn create_resource(&self, size: [u32; 2], format: wgpu::TextureFormat, state: D3D12_RESOURCE_STATES) -> Result<ID3D12Resource, String> {
        let dxgi = match format {
            wgpu::TextureFormat::Bgra8UnormSrgb => DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
            wgpu::TextureFormat::Rgba8UnormSrgb => DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            other => return Err(format!("不支持的格式 {other:?}")),
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(size[0]),
            Height: size[1],
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: dxgi,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: 描述合法（2D、一层、一级、单采样，大小没超过上限）
        unsafe { self.raw.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut resource) }
            .map_err(|e| format!("建不了纹理（{e}）"))?;
        resource.ok_or_else(|| "纹理建好以后是空的".to_owned())
    }
}

/// 录一次区域复制：`source`（大小 `size`）整张复制到 `target` 的 `at`，超出 `bounds`（目标的大小）的部分不复制
///
/// # Safety
///
/// `list` 在录制中，`target` 在 COPY_DEST 状态、`source` 在 COPY_SOURCE 状态，两个格式同一族
unsafe fn copy_region(list: &ID3D12GraphicsCommandList, target: &ID3D12Resource, at: [u32; 2], source: &ID3D12Resource, size: [u32; 2], bounds: [u32; 2]) {
    let width = size[0].min(bounds[0].saturating_sub(at[0]));
    let height = size[1].min(bounds[1].saturating_sub(at[1]));
    if width == 0 || height == 0 {
        return;
    }
    let location = |resource: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(resource.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    let (dst, src) = (location(target), location(source));
    let area = D3D12_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
    // SAFETY: 调用方保证列表在录制、状态和格式对得上；区域没超出两边
    unsafe { list.CopyTextureRegion(&dst, at[0], at[1], 0, &src, Some(&area)) };
    // 放掉上面为两个位置多拿的引用
    drop(ManuallyDrop::into_inner(dst.pResource));
    drop(ManuallyDrop::into_inner(src.pResource));
}

impl Drop for Copier {
    /// 放掉之前等最后一次复制做完（最多一会儿）：命令还在 GPU 上时不能放掉队列和分配器
    fn drop(&mut self) {
        // SAFETY: 只读 fence 的值、等自己的事件
        unsafe {
            if self.copied.GetCompletedValue() < self.copied_value && self.copied.SetEventOnCompletion(self.copied_value, self.event.0).is_ok() {
                WaitForSingleObject(self.event.0, 1000);
            }
        }
    }
}

/// 录一条状态转换
///
/// # Safety
///
/// `list` 在录制中，`resource` 此刻在 `before` 状态
unsafe fn transition(list: &ID3D12GraphicsCommandList, resource: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: 调用方保证列表在录制、状态对得上
    unsafe { list.ResourceBarrier(std::slice::from_ref(&barrier)) };
    // 放掉上面为这条转换多拿的引用
    // SAFETY: 联合体里放的就是上面写进去的转换
    let transition = ManuallyDrop::into_inner(unsafe { barrier.Anonymous.Transition });
    drop(ManuallyDrop::into_inner(transition.pResource));
}

/// 拿着 `Arc<Gpu>` 的复制：设备换了的时候重新建
pub(super) fn copier<'a>(slot: &'a mut Option<Copier>, gpu: &Arc<Gpu>) -> Result<&'a mut Copier, String> {
    if slot.as_ref().is_none_or(|c| c.device() != gpu.id()) {
        *slot = Some(Copier::new(gpu)?);
    }
    Ok(slot.as_mut().expect("刚建好"))
}
