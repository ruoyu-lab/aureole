//! 性能测量里要问系统和渲染器的部分：本进程的资源用量、渲染器分配的显存、选中的 GPU、从启动到第一帧上屏的时间、
//! 事件循环醒来的次数。
//! 每帧的耗时在核心里记；门面 `aureole::performance` 把两边合在一起。
//! 资源用量只用不需要权限的系统接口（见 `platform::process`），这个平台拿不到的项是 `None`。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use render::{GpuInfo, GpuKind, GpuMemory};

/// 本进程此刻的资源用量；这个平台拿不到的项是 `None`
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// 常驻内存，字节
    pub resident: Option<u64>,
    /// 物理内存占用（macOS 的 `phys_footprint`，系统按它算这个进程用了多少内存），字节
    pub footprint: Option<u64>,
    /// 到现在用掉的 CPU 时间（所有线程的用户态 + 内核态）
    pub cpu: Option<Duration>,
    /// 到现在被唤醒的次数（中断唤醒 + 从空闲状态唤醒，macOS）
    pub wakeups: Option<u64>,
}

/// 读本进程此刻的资源用量
pub fn usage() -> Usage {
    use platform::process;
    Usage { resident: process::resident_memory(), footprint: process::footprint(), cpu: process::cpu_time(), wakeups: process::wakeup_count() }
}

/// 从进程启动到现在过了多久：一般从内核创建进程算起；进程先跑过别的程序才换成这个程序时（`cargo run`
/// 先构建再换成要运行的程序），从这个程序装入算起，不把构建的时间算进来（见 `platform::process::since_start`）
pub fn since_start() -> Option<Duration> {
    platform::process::since_start()
}

/// 本进程所有窗口的渲染器此刻分配的显存（纹理和缓冲区，不含交换链）
pub fn gpu_memory() -> GpuMemory {
    render::gpu_memory()
}

/// 选中的 GPU 适配器：名字、后端（Metal、DX12、Vulkan、GL）、种类（集成、独立、软件渲染……）。
/// 还没有窗口（也没有离屏渲染过）时为 `None`。选的先后见 [`crate::App::gpu`]
///
/// ```no_run
/// if let Some(gpu) = aureole::performance::gpu_info() {
///     println!("GPU：{gpu}"); // 比如"Apple M4 Pro（Metal，集成显卡）"
/// }
/// ```
pub fn gpu_info() -> Option<GpuInfo> {
    render::active_adapter()
}

/// 从进程启动到第一个窗口的第一帧上屏（交给系统呈现）用了多久；还没有上屏时为 `None`
pub fn first_present() -> Option<Duration> {
    FIRST_PRESENT.get().copied().flatten()
}

static FIRST_PRESENT: OnceLock<Option<Duration>> = OnceLock::new();

/// 事件循环醒来处理事件的次数（处理完一批事件算一次，包括被帧时钟、定时器、后台线程叫醒的）。
/// 画面静止、没有定时器和后台消息时不再增加：每个平台都能这样数，不像 [`Usage::wakeups`] 只有 macOS 有。
/// 能从任何线程读。
///
/// macOS 上窗口显示着时，系统的运行循环自己大约每 0.5 秒醒一下（没有事件、不出帧，只有窗口库的空窗口也一样），
/// 这些也算在内；要看框架自己要求的唤醒用 [`scheduled_wakeup_count`]
pub fn event_loop_wakeup_count() -> u64 {
    WAKEUPS.load(Ordering::Relaxed)
}

/// 事件循环睡到框架要求的时刻醒来的次数（帧时钟的截止时间、定时器、问显示器）。
/// 画面静止、没有定时器时一次都不该有。能从任何线程读
pub fn scheduled_wakeup_count() -> u64 {
    SCHEDULED.load(Ordering::Relaxed)
}

static WAKEUPS: AtomicU64 = AtomicU64::new(0);
static SCHEDULED: AtomicU64 = AtomicU64::new(0);

/// 事件循环睡到要求的时刻醒了
pub(crate) fn woke_on_schedule() {
    SCHEDULED.fetch_add(1, Ordering::Relaxed);
}

/// 事件循环处理完一批事件
pub(crate) fn woke() {
    WAKEUPS.fetch_add(1, Ordering::Relaxed);
}

/// 渲染线程：一帧交给系统呈现了
pub(crate) fn presented() {
    if FIRST_PRESENT.get().is_none() {
        let first = since_start();
        if FIRST_PRESENT.set(first).is_ok() {
            tracing::debug!(target: STARTUP, "第一帧上屏：启动后 {}", millis(first));
        }
    }
}

/// 启动的分段计时记在这个目标下（debug 级别）：从进程启动到第一帧上屏，每一段用了多久、在启动后多久结束。
/// 管线和着色器的编译另记在渲染器的 `aureole::pipeline` 下
pub(crate) const STARTUP: &str = "aureole::startup";

/// 启动的一段做完了：记下它从 `start` 起用了多久、此刻在启动后多久（debug 级别，见 [`STARTUP`]）
pub(crate) fn startup_step(name: &str, start: std::time::Instant) {
    tracing::debug!(target: STARTUP, "{name}：{:.1} ms（启动后 {}）", start.elapsed().as_secs_f64() * 1000.0, millis(since_start()));
}

/// 启动到了某一步：记下此刻在启动后多久（debug 级别，见 [`STARTUP`]）
pub(crate) fn startup_mark(name: &str) {
    tracing::debug!(target: STARTUP, "{name}：启动后 {}", millis(since_start()));
}

fn millis(duration: Option<Duration>) -> String {
    duration.map_or_else(|| "?".to_owned(), |d| format!("{:.0} ms", d.as_secs_f64() * 1000.0))
}
