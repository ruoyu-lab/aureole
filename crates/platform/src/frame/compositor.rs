//! Windows 的帧时钟：单独一个线程等刷新（见 [`BlockingClock`]），等到了交给主线程。按顺序试下面几种，
//! 前一种建不起来或者用着出错就换下一种（线程里换，主线程照常收帧）：
//!
//! 1. 合成器时钟：`DCompositionWaitForCompositorClock`（Windows 11 起）。用 `GetProcAddress` 找：
//!    Windows 10 的 dcomp.dll 没有这个函数，直接导入的话进程启动不了。
//!    - 它返回 NTSTATUS。**显示器关着**（没人动、到了关屏的时间，或者远程桌面关了本机的屏幕）时不等就返回
//!      `STATUS_GRAPHICS_PRESENT_OCCLUDED`：这不算坏了（以前当成出错，窗口从此一直用计时器）。这期间按垂直消隐出帧
//!      （它也不等时按估计的间隔睡），每次照样先问合成器时钟，显示器亮了接着用它。
//!    - 帧的时间对齐到合成器的节拍：`DCompositionGetStatistics` 读最近一帧的目标上屏时刻和帧周期
//!      （刚开始合成的那一帧常常还没有统计，退一帧读），目标取节拍上的下一次刷新，帧间隔就是合成器的帧周期。
//!    - **显示器的刷新率不一样时**合成器时钟只跟其中一个（这台机器上：主显示器 160Hz、副显示器 165Hz，它按 165Hz 走）。
//!      每 250 毫秒看一次窗口所在显示器的刷新率（`EnumDisplaySettingsW`）和合成器的帧周期，差超过 2%、连续一秒都这样
//!      （[`RefreshFollow`](super::pacing::RefreshFollow)）就改等那个显示器的垂直消隐，对上了换回来；
//!      这期间 [`FrameClock::kind`](super::FrameClock::kind) 报告垂直消隐。
//!    - 时钟走着时要合成器加速（`DCompositionBoostCompositorClock`：开了动态刷新率的显示器平时跑在低档），
//!      暂停、换源、线程结束时放掉。系统按次数算，每个时钟最多要一次。
//! 2. 垂直消隐：窗口所在显示器的 DXGI 输出的 `WaitForVBlank`；每 250 毫秒看一次窗口是不是挪到了别的显示器上。
//!    显示器睡着、远程桌面、基本显示适配器和某些虚拟显示器上它不等就返回：这时按估计的间隔睡，不空转。
//!    出错时重新找一次输出（一秒里最多一次），还不行才算坏了。
//! 3. 都不行：主线程换计时器（[`BlockingClock`] 管）。
//!
//! 等待都有上限（合成器时钟 100 毫秒，垂直消隐最多一个刷新），线程能及时看到暂停和停止；时钟线程的优先级比普通线程高一档。
//! 时间换算：性能计数器的读数就是统一时钟域（[`super::ticks_to_seconds`]）。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
};
use windows::core::PCWSTR;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use super::pacing::{ClockChoice, CompositorGrid, CompositorWait, RefreshFollow, compositor_frame, compositor_wait, vblank_returned_early};
use super::{BlockingClock, BlockingSource, FrameClockKind, FrameClockOptions, IntervalEstimate, SourceTick, Stage};
use super::{performance_frequency, performance_ticks, ticks_to_seconds};

/// `DCompositionWaitForCompositorClock(count, handles, timeout) -> NTSTATUS`
type WaitFn = unsafe extern "system" fn(u32, *const *mut c_void, u32) -> u32;
/// `DCompositionGetFrameId(type, *mut id) -> HRESULT`
type FrameIdFn = unsafe extern "system" fn(i32, *mut u64) -> i32;
/// `DCompositionGetStatistics(id, *mut stats, target_count, *mut targets, *mut actual_count) -> HRESULT`
type StatisticsFn = unsafe extern "system" fn(u64, *mut FrameStatistics, u32, *mut c_void, *mut u32) -> i32;
/// `DCompositionBoostCompositorClock(enable: BOOL) -> HRESULT`
type BoostFn = unsafe extern "system" fn(i32) -> i32;

/// `COMPOSITION_FRAME_STATS`：时间都是性能计数器的读数
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameStatistics {
    start_time: u64,
    target_time: u64,
    frame_period: u64,
}

/// `COMPOSITION_FRAME_ID_CREATED`：合成器最近开始合成的那一帧
const FRAME_ID_CREATED: i32 = 0;
/// 合成器时钟一次最多等多久（毫秒）：到点没等到就回去看一眼暂停和停止
const WAIT_LIMIT_MS: u32 = 100;
/// 垂直消隐隔多久看一次窗口在哪个显示器上
const MONITOR_CHECK: Duration = Duration::from_millis(250);
/// 垂直消隐出错后重新找输出，这么久里最多一次：再错就算坏了
const REFIND_AT_MOST_EVERY: Duration = Duration::from_secs(1);
/// 显示器没换时，隔这么久才重新问一次它的刷新率（用户可能在设置里改了）
const RATE_RECHECK: Duration = Duration::from_secs(2);

/// 显示器此刻的刷新率（赫兹，系统只给整数）。问不到、系统说"硬件默认"（0、1）时是 `None`
fn refresh_rate(monitor: HMONITOR) -> Option<f64> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    // SAFETY: 两个结构都在栈上、大小填好了；`GetMonitorInfoW` 收 `MONITORINFOEXW` 时写进设备名（以 0 结尾），
    // `EnumDisplaySettingsW` 只读这个名字、只往 `mode` 里写
    let ok = unsafe {
        GetMonitorInfoW(monitor, (&raw mut info).cast()).as_bool()
            && EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool()
    };
    (ok && mode.dmDisplayFrequency > 1).then(|| f64::from(mode.dmDisplayFrequency))
}

#[derive(Clone, Copy)]
struct Compositor {
    wait: WaitFn,
    frame_id: Option<FrameIdFn>,
    statistics: Option<StatisticsFn>,
    boost: Option<BoostFn>,
}

/// 以 0 结尾的 UTF-16
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// dcomp.dll 里合成器时钟的函数；系统没有（Windows 10）时是 `None`。进程里只找一次，模块不释放
fn compositor() -> Option<Compositor> {
    static FUNCTIONS: OnceLock<Option<Compositor>> = OnceLock::new();
    *FUNCTIONS.get_or_init(|| {
        let name = wide("dcomp.dll");
        // SAFETY: 名字以 0 结尾；只在系统目录里找，不会加载到别处的同名文件。模块一直不释放，下面的函数指针一直有效
        let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() {
            return None;
        }
        // SAFETY: 模块有效，名字是以 0 结尾的 ASCII；找到的地址按微软文档里的签名转换（见上面的类型），
        // 找不到时是 None
        unsafe {
            let wait = GetProcAddress(module, c"DCompositionWaitForCompositorClock".as_ptr().cast())?;
            let frame_id = GetProcAddress(module, c"DCompositionGetFrameId".as_ptr().cast());
            let statistics = GetProcAddress(module, c"DCompositionGetStatistics".as_ptr().cast());
            let boost = GetProcAddress(module, c"DCompositionBoostCompositorClock".as_ptr().cast());
            Some(Compositor {
                wait: std::mem::transmute::<unsafe extern "system" fn() -> isize, WaitFn>(wait),
                frame_id: frame_id.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, FrameIdFn>(f)),
                statistics: statistics.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, StatisticsFn>(f)),
                boost: boost.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, BoostFn>(f)),
            })
        }
    })
}

/// 等合成器时钟的源
struct CompositorSource {
    functions: Compositor,
    hwnd: isize,
    frequency: u64,
    /// 量到的时钟间隔：读不到统计时按它
    estimate: IntervalEstimate,
    /// 最近读到的合成器节拍
    grid: Option<CompositorGrid>,
    /// 显示器关着（合成器时钟说 OCCLUDED）
    occluded: bool,
    /// 窗口所在显示器的垂直消隐：显示器关着、合成器时钟跟着别的显示器时用。第一次要用时建；
    /// 建不起来、坏了是 `Err`，之后显示器关着时按间隔睡，刷新率对不上时照样用合成器时钟
    substitute: Option<Result<VerticalBlankSource, String>>,
    /// 合成器时钟和窗口所在显示器的刷新率对不对得上
    follow: RefreshFollow,
    /// 上次看对不对得上；`None` 是还没看过
    followed: Option<Instant>,
    /// 窗口所在的显示器、它的刷新率、什么时候问的
    monitor: Option<(HMONITOR, Option<f64>, Instant)>,
    /// 最近一帧是按哪种时钟出的
    paced_by: FrameClockKind,
    /// 要了合成器加速、还没放掉
    boosted: bool,
}

impl CompositorSource {
    fn new(functions: Compositor, hwnd: isize, interval: Duration) -> Self {
        Self {
            functions,
            hwnd,
            frequency: performance_frequency(),
            estimate: IntervalEstimate::new(interval),
            grid: None,
            occluded: false,
            substitute: None,
            follow: RefreshFollow::default(),
            followed: None,
            monitor: None,
            paced_by: FrameClockKind::CompositorClock,
            boosted: false,
        }
    }

    /// 窗口所在显示器的刷新率。显示器没换时 [`RATE_RECHECK`] 才重新问一次
    fn monitor_hz(&mut self) -> Option<f64> {
        // SAFETY: 同 find_output
        let monitor = unsafe { MonitorFromWindow(HWND(self.hwnd as *mut c_void), MONITOR_DEFAULTTONEAREST) };
        if let Some((cached, hz, at)) = self.monitor
            && cached == monitor
            && at.elapsed() < RATE_RECHECK
        {
            return hz;
        }
        let hz = refresh_rate(monitor);
        self.monitor = Some((monitor, hz, Instant::now()));
        hz
    }

    /// 每 [`MONITOR_CHECK`] 看一次合成器的帧周期和窗口所在显示器的刷新率对不对得上（不等合成器时钟也读得到统计）
    fn follow_window(&mut self) {
        if self.followed.is_some_and(|at| at.elapsed() < MONITOR_CHECK) {
            return;
        }
        self.followed = Some(Instant::now());
        self.refresh_grid();
        let period = self.grid.map(|grid| Duration::from_secs_f64(ticks_to_seconds(grid.period, self.frequency)));
        let monitor_hz = self.monitor_hz();
        let before = self.follow.choice();
        let after = self.follow.observe(period, monitor_hz);
        if after != before {
            let compositor_hz = period.map_or(0.0, |p| 1.0 / p.as_secs_f64());
            let monitor_hz = monitor_hz.unwrap_or_default();
            match after {
                ClockChoice::WindowMonitor => {
                    tracing::debug!("合成器时钟按 {compositor_hz:.1}Hz 走，窗口所在的显示器是 {monitor_hz}Hz：改等这个显示器的垂直消隐")
                }
                ClockChoice::Compositor => tracing::debug!("合成器时钟（{compositor_hz:.1}Hz）和窗口所在的显示器对上了：换回合成器时钟"),
            }
        }
    }

    /// 等窗口所在显示器的垂直消隐（它不等时按间隔睡，见 [`VerticalBlankSource`]）；建不起来、坏了时是 `None`
    fn wait_vertical_blank(&mut self) -> Option<SourceTick> {
        let (hwnd, interval) = (self.hwnd, self.estimate.get());
        let substitute = self.substitute.get_or_insert_with(|| VerticalBlankSource::new(hwnd, interval));
        let waited = match substitute {
            Ok(vertical_blank) => vertical_blank.wait(),
            Err(_) => return None,
        };
        match waited {
            Ok(tick) => {
                if tick.is_some() {
                    self.paced_by = FrameClockKind::VerticalBlank;
                }
                tick
            }
            Err(error) => {
                tracing::debug!("窗口所在显示器的垂直消隐不能用：{error}；按合成器时钟走");
                *substitute = Err(error);
                None
            }
        }
    }

    /// 读合成器最近一帧的目标上屏时刻和帧周期。刚开始合成的那一帧常常还没有统计（`STATUS_NOT_FOUND`），退一帧读；
    /// 都读不到时留着上次的
    fn refresh_grid(&mut self) {
        let (Some(frame_id), Some(statistics)) = (self.functions.frame_id, self.functions.statistics) else { return };
        let read = |id: u64| {
            let mut stats = FrameStatistics::default();
            // SAFETY: 指针指向这里可写的局部变量；不要目标列表（数量 0、指针为空）。返回负数是失败
            let ok = unsafe { statistics(id, &mut stats, 0, std::ptr::null_mut(), std::ptr::null_mut()) } >= 0;
            (ok && stats.frame_period > 0).then_some(CompositorGrid { target: stats.target_time, period: stats.frame_period })
        };
        let mut id = 0u64;
        // SAFETY: 指针指向这里可写的局部变量；返回负数是失败
        if unsafe { frame_id(FRAME_ID_CREATED, &mut id) } < 0 {
            return;
        }
        if let Some(grid) = read(id).or_else(|| read(id.saturating_sub(1))) {
            self.grid = Some(grid);
        }
    }

    /// 显示器关着时的一帧：按垂直消隐等（它也不等时按间隔睡），垂直消隐用不了时自己按间隔睡
    fn wait_while_occluded(&mut self) -> SourceTick {
        if !std::mem::replace(&mut self.occluded, true) {
            tracing::debug!("合成器时钟说显示器关着（STATUS_GRAPHICS_PRESENT_OCCLUDED）：先按垂直消隐出帧，显示器亮了换回来");
            self.estimate.break_sequence();
        }
        if let Some(tick) = self.wait_vertical_blank() {
            return tick;
        }
        let interval = self.estimate.get();
        std::thread::sleep(interval);
        SourceTick::after(ticks_to_seconds(performance_ticks(), self.frequency), interval)
    }

    /// 要合成器加速（动态刷新率的显示器从低档换到高档）或者放掉。没有这个函数、调用失败时什么也不做
    fn boost(&mut self, enable: bool) {
        let Some(boost) = self.functions.boost else { return };
        if self.boosted == enable {
            return;
        }
        // SAFETY: 参数是 BOOL；返回负数是失败，这时不算要到了（也就不用放掉）
        if unsafe { boost(i32::from(enable)) } >= 0 {
            self.boosted = enable;
        }
    }
}

impl BlockingSource for CompositorSource {
    fn wait(&mut self) -> Result<Option<SourceTick>, String> {
        // 合成器时钟跟着别的显示器：等窗口所在显示器的垂直消隐（用不了时照样用合成器时钟）
        self.follow_window();
        if self.follow.choice() == ClockChoice::WindowMonitor
            && let Some(tick) = self.wait_vertical_blank()
        {
            return Ok(Some(tick));
        }
        // SAFETY: 不传句柄（数量 0、指针为空），最多等 WAIT_LIMIT_MS 毫秒
        let code = unsafe { (self.functions.wait)(0, std::ptr::null(), WAIT_LIMIT_MS) };
        match compositor_wait(code, 0) {
            CompositorWait::Tick => {
                if std::mem::take(&mut self.occluded) {
                    tracing::debug!("合成器时钟又走了：换回它");
                }
                self.paced_by = FrameClockKind::CompositorClock;
                let now = performance_ticks();
                self.estimate.observe(Instant::now());
                self.refresh_grid();
                let (frame, period) = compositor_frame(now, self.grid, self.frequency, self.estimate.get());
                Ok(Some(SourceTick { frame, period }))
            }
            CompositorWait::Timeout | CompositorWait::Handle(_) => Ok(None),
            CompositorWait::Occluded => Ok(Some(self.wait_while_occluded())),
            // 返回的是 NTSTATUS，不设线程的错误码
            CompositorWait::Failed => Err(format!("DCompositionWaitForCompositorClock 失败（NTSTATUS {code:#010x}）")),
        }
    }

    fn set_active(&mut self, active: bool) {
        self.boost(active);
    }

    fn current_kind(&self) -> Option<FrameClockKind> {
        Some(self.paced_by)
    }
}

impl Drop for CompositorSource {
    fn drop(&mut self) {
        self.boost(false);
    }
}

/// 等 DXGI 输出垂直消隐的源（在时钟线程里建，COM 对象只在这个线程里用）
struct VerticalBlankSource {
    hwnd: isize,
    monitor: HMONITOR,
    output: IDXGIOutput,
    estimate: IntervalEstimate,
    frequency: u64,
    /// 上次看窗口在哪个显示器上
    checked: Instant,
    /// 上次因为出错重新找输出
    refound: Option<Instant>,
}

/// 窗口所在显示器对应的 DXGI 输出；对不上时用第一个
fn find_output(hwnd: isize) -> Result<(IDXGIOutput, HMONITOR), String> {
    // SAFETY: MonitorFromWindow 只读窗口的位置，窗口句柄无效时按 MONITOR_DEFAULTTONEAREST 返回最近的显示器；
    // DXGI 的工厂、适配器、输出是普通的 COM 调用，不需要先初始化 COM
    unsafe {
        let monitor = MonitorFromWindow(HWND(hwnd as *mut c_void), MONITOR_DEFAULTTONEAREST);
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("建不了 DXGI 工厂：{e}"))?;
        let mut first = None;
        let mut adapter_index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            adapter_index += 1;
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                if output.GetDesc().is_ok_and(|desc| desc.Monitor == monitor) {
                    return Ok((output, monitor));
                }
                if first.is_none() {
                    first = Some(output);
                }
            }
        }
        first.map(|output| (output, monitor)).ok_or_else(|| "没有显示输出".to_owned())
    }
}

impl VerticalBlankSource {
    fn new(hwnd: isize, interval: Duration) -> Result<Self, String> {
        let (output, monitor) = find_output(hwnd)?;
        Ok(Self {
            hwnd,
            monitor,
            output,
            estimate: IntervalEstimate::new(interval),
            frequency: performance_frequency(),
            checked: Instant::now(),
            refound: None,
        })
    }

    /// 窗口挪到了别的显示器上：换成那个显示器的输出
    fn follow_window(&mut self) {
        if self.checked.elapsed() < MONITOR_CHECK {
            return;
        }
        self.checked = Instant::now();
        // SAFETY: 同 find_output
        let monitor = unsafe { MonitorFromWindow(HWND(self.hwnd as *mut c_void), MONITOR_DEFAULTTONEAREST) };
        if monitor != self.monitor
            && let Ok((output, monitor)) = find_output(self.hwnd)
        {
            self.output = output;
            self.monitor = monitor;
            self.estimate.break_sequence();
        }
    }

    /// `WaitForVBlank` 出错（显示器拔了、换了显卡的模式）：重新找一次输出。刚找过又错时算坏了
    fn recover(&mut self, error: &windows::core::Error) -> Result<(), String> {
        let failed = format!("WaitForVBlank 失败：{error}");
        if self.refound.is_some_and(|at| at.elapsed() < REFIND_AT_MOST_EVERY) {
            return Err(failed);
        }
        self.refound = Some(Instant::now());
        let (output, monitor) = find_output(self.hwnd).map_err(|e| format!("{failed}；重新找输出也不行：{e}"))?;
        tracing::debug!("{failed}；重新找到窗口所在显示器的输出");
        self.output = output;
        self.monitor = monitor;
        self.estimate.break_sequence();
        Ok(())
    }
}

impl BlockingSource for VerticalBlankSource {
    fn wait(&mut self) -> Result<Option<SourceTick>, String> {
        self.follow_window();
        let interval = self.estimate.get();
        let started = Instant::now();
        // SAFETY: 输出对象在这个线程里建、只在这个线程里用；WaitForVBlank 阻塞到下一次垂直消隐
        if let Err(error) = unsafe { self.output.WaitForVBlank() } {
            self.recover(&error)?;
            return Ok(None);
        }
        let now = Instant::now();
        if vblank_returned_early(now - started, interval) {
            // 没真的等（显示器睡着、远程桌面、虚拟显示器）：按估计的间隔睡，不空转；这次不算量到的间隔
            self.estimate.break_sequence();
            std::thread::sleep(interval);
            return Ok(Some(SourceTick::after(ticks_to_seconds(performance_ticks(), self.frequency), interval)));
        }
        let ticks = performance_ticks();
        self.estimate.observe(now);
        Ok(Some(SourceTick::after(ticks_to_seconds(ticks, self.frequency), self.estimate.get())))
    }
}

/// 这台机器上窗口能拿到的帧时钟：有合成器时钟（Windows 11）就是它，没有是垂直消隐
pub(crate) fn windows_clock_kind() -> FrameClockKind {
    if compositor().is_some() { FrameClockKind::CompositorClock } else { FrameClockKind::VerticalBlank }
}

/// 给 Win32 窗口建帧时钟：合成器时钟 → 垂直消隐（→ 计时器）。不是 Win32 窗口、没给 `waker` 时是 `None`
pub(crate) fn windows_clock(window: &dyn HasWindowHandle, options: &FrameClockOptions) -> Option<BlockingClock> {
    let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else { return None };
    let waker = options.waker.clone()?;
    let (hwnd, interval) = (handle.hwnd.get(), options.rate.interval());
    let vertical_blank = Stage::new(FrameClockKind::VerticalBlank, move || VerticalBlankSource::new(hwnd, interval));
    let stages = match compositor() {
        Some(functions) => {
            vec![Stage::new(FrameClockKind::CompositorClock, move || Ok(CompositorSource::new(functions, hwnd, interval))), vertical_blank]
        }
        None => vec![vertical_blank],
    };
    Some(BlockingClock::spawn(options.rate, waker, stages))
}
