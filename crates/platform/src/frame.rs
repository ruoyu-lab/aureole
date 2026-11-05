//! 帧时钟：每个窗口一个，跟着显示器的节奏在主线程上回调；没事可做时暂停，暂停期间不再唤醒任何线程。
//!
//! 各平台用什么（[`FrameClockKind`]）：
//!
//! - macOS：`CADisplayLink`，从窗口的视图创建（macOS 14 起）。
//! - Windows：单独一个线程等合成器的时钟（`DCompositionWaitForCompositorClock`，Windows 11 起）；
//!   拿不到时等窗口所在输出的垂直消隐（DXGI 的 `WaitForVBlank`）。等到了把帧放进信箱、叫醒主线程，
//!   主线程在 [`FrameClock::dispatch`] 里交给回调。暂停时线程睡在条件变量上，不轮询。
//! - Linux 的 Wayland：表面的帧回调（`wl_surface.frame`），挂在窗口库的连接上、用自己的事件队列。
//! - X11、拿不到上面这些时：按刷新率的计时器（见 [`FrameClockKind::Timer`]），可以用 [`PresentFeedback`]
//!   按呈现返回的时刻对齐相位。
//! - 无头后端：调用方手动推进（[`FrameClockKind::Manual`]）。
//!
//! # 统一的时钟域
//!
//! [`FrameTime`] 里的时间都在同一个时钟域：系统的单调时钟，从这个时钟的原点（一般是开机）算起。
//! macOS 上就是 `CACurrentMediaTime`（`mach_absolute_time`），Linux 上是 `CLOCK_MONOTONIC`，
//! Windows 上是 `QueryPerformanceCounter`。这三个也正是 [`std::time::Instant`] 在各平台读的时钟，
//! 和 `Instant` 之间按进程里取一次的对应关系换算（[`FrameTime::target_instant`]、[`FrameTime::from_instants`]）。
//!
//! # 和事件循环怎么配合
//!
//! 1. 用 [`crate::PlatformBackend::frame_clock`] 给窗口要一个时钟（总有，最差是计时器），再 [`FrameClock::start`]。
//! 2. 事件循环每轮结束（一批事件处理完、要睡之前）对每个时钟调用 [`FrameClock::dispatch`]，
//!    再睡到所有时钟的 [`FrameClock::deadline`] 里最早的那个（都是 `None` 时一直睡，直到有事件）。
//! 3. 后台线程等到的帧通过 [`FrameClockOptions::waker`] 叫醒事件循环；没给 `waker` 时这类实现退到计时器。
//! 4. 回调里的 panic 被接住（macOS 的回调在系统的运行循环里，panic 不能穿过系统的栈帧）：时钟暂停，
//!    下一次 [`FrameClock::dispatch`] 在调用方的栈上重新抛出。

use std::any::Any;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Waker;
use std::time::{Duration, Instant};

mod blocking;
#[cfg(target_os = "windows")]
mod compositor;
#[cfg(target_os = "macos")]
mod display_link;
mod pacing;
mod timer;
#[cfg(target_os = "linux")]
mod wayland;

#[cfg_attr(not(windows), allow(unused_imports))]
pub(crate) use blocking::{BlockingClock, BlockingSource, SourceTick};
#[cfg(target_os = "windows")]
pub(crate) use compositor::{windows_clock, windows_clock_kind};
#[cfg(target_os = "macos")]
pub(crate) use display_link::DisplayLinkClock;
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
pub(crate) use pacing::CallbackPacing;
#[cfg_attr(not(windows), allow(unused_imports))]
pub(crate) use pacing::IntervalEstimate;
pub(crate) use pacing::{Tick, TimerPacing};
pub use timer::TimerClock;
#[cfg(target_os = "linux")]
pub(crate) use wayland::WaylandClock;

/// 一次显示刷新的时间信息，在统一的时钟域里（见[模块说明](self)；macOS 上和 `CACurrentMediaTime` 是同一个时钟）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameTime {
    /// 上一帧上屏的时间（拿不到时是这一帧开始的时间）
    pub timestamp: Duration,
    /// 本帧预计上屏的时间；动画应该按这个时间推进
    pub target: Duration,
}

impl FrameTime {
    /// 当前的刷新间隔，120Hz 时约为 8.33 毫秒
    pub fn interval(&self) -> Duration {
        self.target.saturating_sub(self.timestamp)
    }

    /// 从统一时钟域的秒数（系统给的读数）换过来
    pub(crate) fn from_seconds(timestamp: f64, target: f64) -> Self {
        Self { timestamp: clock_time(timestamp), target: clock_time(target) }
    }

    /// 从两个 [`Instant`] 换算过来
    pub fn from_instants(timestamp: Instant, target: Instant) -> Self {
        let anchor = ClockAnchor::process();
        Self::from_seconds(anchor.to_seconds(timestamp), anchor.to_seconds(target))
    }

    /// 预计上屏的时刻换成 [`Instant`]
    pub fn target_instant(&self) -> Instant {
        ClockAnchor::process().to_instant(self.target.as_secs_f64())
    }

    /// 上一帧上屏的时刻换成 [`Instant`]
    pub fn timestamp_instant(&self) -> Instant {
        ClockAnchor::process().to_instant(self.timestamp.as_secs_f64())
    }
}

/// 帧时钟的回调
pub(crate) type FrameCallback = Box<dyn FnMut(FrameTime)>;

/// 统一时钟域里的此刻（见[模块说明](self)）
pub fn monotonic_time() -> Duration {
    clock_time(clock::now())
}

/// 统一时钟域的秒数换成时长；原点之前（负数）、不是数的当 0
fn clock_time(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds).unwrap_or_default()
}

#[cfg(unix)]
mod clock {
    /// macOS 的 `CLOCK_UPTIME_RAW` 就是 `mach_absolute_time`（`CACurrentMediaTime` 读的那个），睡眠时不走
    #[cfg(target_vendor = "apple")]
    const ID: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    const ID: libc::clockid_t = libc::CLOCK_MONOTONIC;

    pub fn now() -> f64 {
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: ID 是这个系统支持的时钟；指针指向上面这个可写的 timespec，clock_gettime 只往里写
        let ok = unsafe { libc::clock_gettime(ID, &mut time) } == 0;
        if ok { time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9 } else { 0.0 }
    }
}

#[cfg(windows)]
mod clock {
    use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    /// 性能计数器的频率（每秒的计数），开机后不变
    pub fn frequency() -> u64 {
        static FREQUENCY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *FREQUENCY.get_or_init(|| {
            let mut frequency = 0i64;
            // SAFETY: 指针指向上面这个可写的 i64，函数只往里写；XP 以后不会失败
            unsafe { QueryPerformanceFrequency(&mut frequency) };
            frequency.max(1) as u64
        })
    }

    /// 性能计数器此刻的值
    pub fn ticks() -> u64 {
        let mut ticks = 0i64;
        // SAFETY: 指针指向上面这个可写的 i64，函数只往里写；XP 以后不会失败
        unsafe { QueryPerformanceCounter(&mut ticks) };
        ticks.max(0) as u64
    }

    pub fn now() -> f64 {
        super::ticks_to_seconds(ticks(), frequency())
    }
}

#[cfg(not(any(unix, windows)))]
mod clock {
    /// 其他系统：按进程里第一次调用算起
    pub fn now() -> f64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
    }
}

#[cfg(windows)]
pub(crate) use clock::{frequency as performance_frequency, ticks as performance_ticks};

/// 计数器的读数换成秒：整数部分先除，免得大读数乘出来丢精度
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn ticks_to_seconds(ticks: u64, frequency: u64) -> f64 {
    let frequency = frequency.max(1);
    (ticks / frequency) as f64 + (ticks % frequency) as f64 / frequency as f64
}

/// 统一时钟域和 [`Instant`] 的一组对应：同一时刻在两边的读数
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClockAnchor {
    instant: Instant,
    seconds: f64,
}

impl ClockAnchor {
    pub(crate) fn new(instant: Instant, seconds: f64) -> Self {
        Self { instant, seconds }
    }

    /// 进程里取一次：两边是同一个时钟，之后不会漂
    pub(crate) fn process() -> &'static ClockAnchor {
        static ANCHOR: OnceLock<ClockAnchor> = OnceLock::new();
        ANCHOR.get_or_init(Self::sample)
    }

    /// 取三次，用前后两次 `Instant` 夹得最紧的那次（中间被打断时误差大）
    fn sample() -> Self {
        let mut best: Option<(Duration, Self)> = None;
        for _ in 0..3 {
            let before = Instant::now();
            let seconds = clock::now();
            let after = Instant::now();
            let spread = after - before;
            let anchor = Self { instant: before + spread / 2, seconds };
            if best.is_none_or(|(s, _)| spread < s) {
                best = Some((spread, anchor));
            }
        }
        best.map(|(_, anchor)| anchor).unwrap_or_else(|| Self::new(Instant::now(), clock::now()))
    }

    /// 统一时钟域的秒数 → `Instant`。不是有限数、超出 `Instant` 能表示的范围时取锚点本身
    pub(crate) fn to_instant(self, seconds: f64) -> Instant {
        let delta = seconds - self.seconds;
        let Ok(offset) = Duration::try_from_secs_f64(delta.abs()) else { return self.instant };
        let moved = if delta >= 0.0 { self.instant.checked_add(offset) } else { self.instant.checked_sub(offset) };
        moved.unwrap_or(self.instant)
    }

    /// `Instant` → 统一时钟域的秒数
    pub(crate) fn to_seconds(self, instant: Instant) -> f64 {
        if instant >= self.instant {
            self.seconds + (instant - self.instant).as_secs_f64()
        } else {
            self.seconds - (self.instant - instant).as_secs_f64()
        }
    }

    /// 一次心跳（`Instant` 表示）换成 [`FrameTime`]
    pub(crate) fn frame(self, tick: Tick) -> FrameTime {
        FrameTime::from_seconds(self.to_seconds(tick.timestamp), self.to_seconds(tick.target))
    }
}

/// 想要的帧率范围，单位赫兹。显示链接把它交给系统（ProMotion 屏据此切到 120Hz）；
/// 计时器按 `preferred` 走；等合成器、垂直消隐的实现不会比 `max` 更频繁地交帧
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRateRange {
    /// 最低能接受的帧率
    pub min: f32,
    /// 最高的帧率
    pub max: f32,
    /// 最想要的帧率
    pub preferred: f32,
}

impl FrameRateRange {
    /// 从 `min` 到 `max`，最想要 `preferred`
    pub fn new(min: f32, max: f32, preferred: f32) -> Self {
        Self { min, max, preferred }
    }

    /// 最高 `max`：最低是 `max` 和 60 里小的那个，最想要 `max`（和原来的显示链接一样）
    pub fn up_to(max: f32) -> Self {
        Self { min: max.min(60.0), max, preferred: max }
    }

    /// 修整成能用的值：都是正的有限数，`min ≤ preferred ≤ max`，不超过 1000Hz
    pub fn sanitized(self) -> Self {
        let valid = |v: f32| v.is_finite() && v > 0.0;
        let max = if valid(self.max) { self.max.min(1000.0) } else { 120.0 };
        let min = if valid(self.min) { self.min.min(max) } else { max.min(60.0) };
        let preferred = if valid(self.preferred) { self.preferred.clamp(min, max) } else { max };
        Self { min, max, preferred }
    }

    /// 按 `preferred` 算的帧间隔
    pub fn interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.sanitized().preferred))
    }

    /// 两帧之间最短的间隔（按 `max` 算）
    pub fn min_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.sanitized().max))
    }
}

impl Default for FrameRateRange {
    /// 60 到 120Hz，最想要 120Hz
    fn default() -> Self {
        Self::up_to(120.0)
    }
}

/// 帧时钟用的是哪种实现
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameClockKind {
    /// macOS 的 `CADisplayLink`
    DisplayLink,
    /// Windows 的合成器时钟（`DCompositionWaitForCompositorClock`）
    CompositorClock,
    /// Windows 的 DXGI 输出的垂直消隐（`WaitForVBlank`）
    VerticalBlank,
    /// Wayland 表面的帧回调（`wl_surface.frame`）
    SurfaceFrame,
    /// 按刷新率的计时器：由事件循环按 [`FrameClock::deadline`] 醒来推进
    Timer,
    /// 无头后端：调用方手动推进
    Manual,
}

impl FrameClockKind {
    /// 跟着显示器的实际刷新：显示链接、合成器时钟、垂直消隐、表面的帧回调
    pub fn follows_display(self) -> bool {
        matches!(self, Self::DisplayLink | Self::CompositorClock | Self::VerticalBlank | Self::SurfaceFrame)
    }

    /// 给人看的名字
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisplayLink => "CADisplayLink",
            Self::CompositorClock => "合成器时钟",
            Self::VerticalBlank => "垂直消隐",
            Self::SurfaceFrame => "表面帧回调",
            Self::Timer => "计时器",
            Self::Manual => "手动",
        }
    }
}

/// 创建帧时钟的设置
#[derive(Clone, Debug, Default)]
pub struct FrameClockOptions {
    /// 想要的帧率范围
    pub rate: FrameRateRange,
    /// 叫醒主线程的事件循环。后台线程等到帧时调用（Windows 的合成器时钟、垂直消隐），
    /// 同一帧没被取走之前不会再叫。没给时这类实现退到计时器
    pub waker: Option<Waker>,
}

/// 渲染线程回报"呈现返回了"：没有呈现回调的时钟（计时器）把下一帧对齐到它之后。可以复制、跨线程用
#[derive(Clone, Debug, Default)]
pub struct PresentFeedback {
    last: Arc<Mutex<Option<Instant>>>,
}

impl PresentFeedback {
    /// 一帧在 `at` 呈现返回了（用垂直同步时，返回的时刻接近垂直消隐）
    pub fn record_present(&self, at: Instant) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
    }

    /// 取走最近一次回报
    pub(crate) fn take(&self) -> Option<Instant> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// 一个窗口的帧时钟。只在主线程上用（不是 `Send`）；必须在窗口之前释放
pub trait FrameClock {
    /// 用的是哪种实现。出错退到计时器之后返回 [`FrameClockKind::Timer`]
    fn kind(&self) -> FrameClockKind;

    /// 开始出帧：之后每帧在主线程上以统一时钟域的 [`FrameTime`] 调用 `on_frame`。
    /// 暂停着时一并恢复；再调用一次换掉回调
    fn start(&mut self, on_frame: Box<dyn FnMut(FrameTime)>);

    /// 暂停（`true`）或恢复出帧。暂停后不再回调，也不再唤醒主线程，后台线程睡住；恢复后尽快出下一帧
    fn set_paused(&self, paused: bool);

    /// 暂停着（包括回调 panic 之后自动暂停）
    fn is_paused(&self) -> bool;

    /// 想要的帧率范围
    fn set_rate_hint(&self, range: FrameRateRange);

    /// 在主线程上把已经送到、已经到期的帧交给回调，返回交了几帧。事件循环每轮结束时调用。
    /// 之前回调里接住的 panic 在这里重新抛出
    fn dispatch(&mut self, now: Instant) -> usize;

    /// 事件循环最晚什么时候要醒来调用 [`dispatch`](Self::dispatch)；`None` 表示不用为这个时钟醒来
    /// （暂停着，或者帧会自己叫醒事件循环）
    fn deadline(&self) -> Option<Instant>;

    /// 渲染线程回报呈现时刻用的句柄：只有按它对齐相位的实现（计时器）才有
    fn present_feedback(&self) -> Option<PresentFeedback> {
        None
    }
}

/// panic 的载荷里的文字
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("（不是文字）")
}
