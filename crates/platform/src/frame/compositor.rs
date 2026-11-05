//! Windows 的帧时钟：单独一个线程等刷新（见 [`BlockingClock`]），等到了交给主线程。
//!
//! - 合成器时钟：`DCompositionWaitForCompositorClock`（Windows 11 起）。用 `GetProcAddress` 找：
//!   Windows 10 的 dcomp.dll 没有这个函数，直接导入的话进程启动不了。等到以后用
//!   `DCompositionGetStatistics` 读合成器这一帧的目标上屏时间和帧周期（读不到时按估计的间隔）。
//! - 垂直消隐：找不到合成器时钟时，等窗口所在显示器的 DXGI 输出的 `WaitForVBlank`；
//!   每秒看一次窗口是不是挪到了别的显示器上。
//!
//! 等待都有上限（合成器时钟 100 毫秒，垂直消隐最多一个刷新），线程能及时看到暂停和停止。
//! 时间换算：性能计数器的读数就是统一时钟域（[`super::ticks_to_seconds`]）。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use super::pacing::{CompositorWait, compositor_frame, compositor_wait};
use super::{BlockingClock, BlockingSource, FrameClockKind, FrameClockOptions, IntervalEstimate, SourceTick};
use super::{performance_frequency, performance_ticks, ticks_to_seconds};

/// `DCompositionWaitForCompositorClock(count, handles, timeout) -> DWORD`
type WaitFn = unsafe extern "system" fn(u32, *const *mut c_void, u32) -> u32;
/// `DCompositionGetFrameId(type, *mut id) -> HRESULT`
type FrameIdFn = unsafe extern "system" fn(i32, *mut u64) -> i32;
/// `DCompositionGetStatistics(id, *mut stats, target_count, *mut targets, *mut actual_count) -> HRESULT`
type StatisticsFn = unsafe extern "system" fn(u64, *mut FrameStatistics, u32, *mut c_void, *mut u32) -> i32;

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

#[derive(Clone, Copy)]
struct Compositor {
    wait: WaitFn,
    frame_id: Option<FrameIdFn>,
    statistics: Option<StatisticsFn>,
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
            Some(Compositor {
                wait: std::mem::transmute::<unsafe extern "system" fn() -> isize, WaitFn>(wait),
                frame_id: frame_id.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, FrameIdFn>(f)),
                statistics: statistics.map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, StatisticsFn>(f)),
            })
        }
    })
}

/// 等合成器时钟的源
struct CompositorSource {
    functions: Compositor,
    frequency: u64,
    fallback_interval: Duration,
}

impl CompositorSource {
    /// 合成器这一帧的目标上屏时间和帧周期（性能计数器的读数）
    fn statistics(&self) -> Option<(u64, u64)> {
        let (frame_id, statistics) = (self.functions.frame_id?, self.functions.statistics?);
        let mut id = 0u64;
        let mut stats = FrameStatistics::default();
        // SAFETY: 两个指针都指向这里可写的局部变量；不要目标列表（数量 0、指针为空）。返回负数是失败
        let ok = unsafe {
            frame_id(FRAME_ID_CREATED, &mut id) >= 0 && statistics(id, &mut stats, 0, std::ptr::null_mut(), std::ptr::null_mut()) >= 0
        };
        ok.then_some((stats.target_time, stats.frame_period))
    }
}

impl BlockingSource for CompositorSource {
    fn wait(&mut self) -> Result<Option<SourceTick>, String> {
        // SAFETY: 不传句柄（数量 0、指针为空），最多等 WAIT_LIMIT_MS 毫秒
        let code = unsafe { (self.functions.wait)(0, std::ptr::null(), WAIT_LIMIT_MS) };
        match compositor_wait(code, 0) {
            CompositorWait::Tick => {}
            CompositorWait::Timeout | CompositorWait::Handle(_) => return Ok(None),
            CompositorWait::Failed => {
                // SAFETY: 没有参数，读这个线程最近的错误码
                let error = unsafe { GetLastError() };
                return Err(format!("DCompositionWaitForCompositorClock 失败（返回 {code:#x}，错误码 {error}）"));
            }
        }
        Ok(Some(compositor_frame(performance_ticks(), self.statistics(), self.frequency, self.fallback_interval)))
    }
}

/// 等 DXGI 输出垂直消隐的源（在时钟线程里建，COM 对象只在这个线程里用）
struct VerticalBlankSource {
    hwnd: isize,
    monitor: HMONITOR,
    output: IDXGIOutput,
    estimate: IntervalEstimate,
    frequency: u64,
    checked: Instant,
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
        Ok(Self { hwnd, monitor, output, estimate: IntervalEstimate::new(interval), frequency: performance_frequency(), checked: Instant::now() })
    }
}

impl BlockingSource for VerticalBlankSource {
    fn wait(&mut self) -> Result<Option<SourceTick>, String> {
        if self.checked.elapsed() >= Duration::from_secs(1) {
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
        // SAFETY: 输出对象在这个线程里建、只在这个线程里用；WaitForVBlank 阻塞到下一次垂直消隐
        unsafe { self.output.WaitForVBlank() }.map_err(|e| format!("WaitForVBlank 失败：{e}"))?;
        let ticks = performance_ticks();
        self.estimate.observe(Instant::now());
        let now = ticks_to_seconds(ticks, self.frequency);
        Ok(Some(SourceTick::from_seconds(now, now + self.estimate.get().as_secs_f64())))
    }
}

/// 这台机器上窗口能拿到的帧时钟：有合成器时钟（Windows 11）就是它，没有是垂直消隐
pub(crate) fn windows_clock_kind() -> FrameClockKind {
    if compositor().is_some() { FrameClockKind::CompositorClock } else { FrameClockKind::VerticalBlank }
}

/// 给 Win32 窗口建帧时钟：有合成器时钟用它，没有用垂直消隐。不是 Win32 窗口、没给 `waker` 时是 `None`
pub(crate) fn windows_clock(window: &dyn HasWindowHandle, options: &FrameClockOptions) -> Option<BlockingClock> {
    let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else { return None };
    let waker = options.waker.clone()?;
    let (hwnd, rate, interval) = (handle.hwnd.get(), options.rate, options.rate.interval());
    Some(match compositor() {
        Some(functions) => BlockingClock::spawn(FrameClockKind::CompositorClock, rate, waker, move || {
            Ok(CompositorSource { functions, frequency: performance_frequency(), fallback_interval: interval })
        }),
        None => BlockingClock::spawn(FrameClockKind::VerticalBlank, rate, waker, move || VerticalBlankSource::new(hwnd, interval)),
    })
}
