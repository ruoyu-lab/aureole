//! 在单独的线程上阻塞等刷新的帧时钟（Windows 的合成器时钟、垂直消隐都是这样）：
//! 等到了把帧放进信箱、叫醒主线程，主线程在 [`FrameClock::dispatch`] 里交给回调。
//!
//! - 没开始、暂停着时线程睡在条件变量上，不等刷新，也不叫醒主线程。
//! - 主线程没来取之前只叫一次：新的帧覆盖旧的，主线程忙的时候不会被叫醒好几次。
//! - 源每次刷新都醒，按帧率上限隔整数个刷新交一帧（[`Decimation`]：数刷新，不按醒来的时刻比）。
//! - 等刷新的源按顺序试（[`Stage`]，比如合成器时钟 → 垂直消隐）：一种建不起来或者用着出错，线程换下一种接着出帧；
//!   都不行时主线程下一次 `dispatch` 换成计时器。
//! - 释放时通知线程退出，不等它：线程最多再等一次刷新（源的等待有超时）就结束。
//!
//! 等刷新的源是一个 trait，测试里换成假的源，所以这部分在每个平台上都跑测试（只有 Windows 真的用它）。
#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::Waker;
use std::time::{Duration, Instant};

use super::pacing::Decimation;
use super::{FrameCallback, FrameClock, FrameClockKind, FrameRateRange, FrameTime, TimerClock};

/// 阻塞等刷新的源（在时钟自己的线程里创建和使用）
pub(crate) trait BlockingSource {
    /// 阻塞到下一次刷新，返回这一帧的时间（统一时钟域）。`Ok(None)` 表示这次没等到（超时），
    /// 线程回去看一眼状态再等；`Err` 表示这个源不能用了，线程换下一种
    fn wait(&mut self) -> Result<Option<SourceTick>, String>;

    /// 线程开始等刷新（`true`，开始、恢复时）或者要睡下了（`false`，暂停时）。合成器时钟在这里要、放掉加速。
    /// 成对调用；线程结束、换下一种源时源被释放，没放掉的由源自己在释放时放掉
    fn set_active(&mut self, _active: bool) {}

    /// 最近一帧实际是按哪种时钟出的（合成器时钟的源有时改等窗口所在显示器的垂直消隐）；`None` 时就是这一种源本身
    fn current_kind(&self) -> Option<FrameClockKind> {
        None
    }
}

/// 源等到的一帧
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SourceTick {
    /// 这一帧的时间：`target` 是预计上屏的那次刷新，`timestamp` 是上一次刷新
    pub frame: FrameTime,
    /// 源此刻的刷新周期（合成器的帧周期、量到的间隔）：按它数隔了几次刷新
    pub period: Duration,
}

impl SourceTick {
    /// `now`（统一时钟域的秒数）刚刷新过，下一次在一个周期以后
    pub(crate) fn after(now: f64, period: Duration) -> Self {
        Self { frame: FrameTime::from_seconds(now, now + period.as_secs_f64()), period }
    }
}

/// 建一个源（在时钟线程里调用）
type MakeSource = Box<dyn FnOnce() -> Result<Box<dyn BlockingSource>, String> + Send>;

/// 按顺序要试的一种源
pub(crate) struct Stage {
    kind: FrameClockKind,
    make: MakeSource,
}

impl Stage {
    /// 种类是 `kind`、在时钟线程里用 `make` 建的源
    pub(crate) fn new<S, F>(kind: FrameClockKind, make: F) -> Self
    where
        S: BlockingSource + 'static,
        F: FnOnce() -> Result<S, String> + Send + 'static,
    {
        Self { kind, make: Box::new(move || make().map(|source| Box::new(source) as Box<dyn BlockingSource>)) }
    }
}

struct State {
    started: bool,
    paused: bool,
    stopped: bool,
    /// 线程此刻用的是哪种源
    kind: FrameClockKind,
    /// 等到了、主线程还没取走的帧（只留最新的）
    frame: Option<FrameTime>,
    /// 所有的源都不能用了，主线程换计时器
    failure: Option<String>,
    /// 两帧之间最短的间隔（按帧率提示的上限）
    min_gap: Duration,
}

struct Shared {
    state: Mutex<State>,
    /// 叫醒睡着的时钟线程（开始、恢复、停止）
    resume: Condvar,
    /// 已经叫过主线程、它还没来取
    woken: AtomicBool,
    waker: Waker,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 叫醒主线程；上次叫了还没来取时不再叫
    fn wake_main(&self) {
        if !self.woken.swap(true, Ordering::AcqRel) {
            self.waker.wake_by_ref();
        }
    }
}

/// 后台线程等刷新、主线程交帧的时钟
pub(crate) struct BlockingClock {
    shared: Arc<Shared>,
    callback: Option<FrameCallback>,
    rate: std::cell::Cell<FrameRateRange>,
    /// 所有的源都坏了以后换上的计时器
    fallback: Option<TimerClock>,
}

impl BlockingClock {
    /// 开一个线程，在线程里按顺序试 `stages`（建不起来、用着坏了换下一种，都不行换计时器），开始前一直睡着
    pub(crate) fn spawn(rate: FrameRateRange, waker: Waker, stages: Vec<Stage>) -> Self {
        let kind = stages.first().map_or(FrameClockKind::Timer, |stage| stage.kind);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                started: false,
                paused: false,
                stopped: false,
                kind,
                frame: None,
                failure: None,
                min_gap: rate.min_interval(),
            }),
            resume: Condvar::new(),
            woken: AtomicBool::new(false),
            waker,
        });
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new().name("帧时钟".into()).spawn(move || run(&thread_shared, stages));
        if let Err(error) = spawned {
            shared.lock().failure = Some(format!("线程开不出来：{error}"));
        }
        Self { shared, callback: None, rate: std::cell::Cell::new(rate), fallback: None }
    }

    /// 源都坏了：换成计时器，回调和暂停状态一起带过去
    fn fall_back(&mut self, error: &str) {
        let (kind, paused) = {
            let state = self.shared.lock();
            (state.kind, state.paused)
        };
        tracing::warn!("帧时钟（{}）不能用了：{error}；改用计时器", kind.as_str());
        let mut timer = TimerClock::new(self.rate.get());
        if let Some(callback) = self.callback.take() {
            timer.start(callback);
        }
        timer.set_paused(paused);
        self.fallback = Some(timer);
    }
}

/// 时钟线程：按顺序试每种源，一种坏了换下一种；都不行时记下原因、叫醒主线程（它换计时器）
fn run(shared: &Shared, stages: Vec<Stage>) {
    raise_priority();
    let mut failure = String::from("没有能等刷新的源");
    let mut previous: Option<FrameClockKind> = None;
    for stage in stages {
        shared.lock().kind = stage.kind;
        let mut source = match (stage.make)() {
            Ok(source) => source,
            Err(error) => {
                tracing::warn!("帧时钟（{}）建不起来：{error}", stage.kind.as_str());
                failure = error;
                previous = Some(stage.kind);
                continue;
            }
        };
        if let Some(previous) = previous {
            tracing::warn!("帧时钟改用{}（{}不能用）", stage.kind.as_str(), previous.as_str());
        }
        match drive(shared, source.as_mut()) {
            // 停了
            Ok(()) => return,
            Err(error) => {
                tracing::warn!("帧时钟（{}）不能用了：{error}", stage.kind.as_str());
                failure = error;
                previous = Some(stage.kind);
            }
        }
    }
    shared.lock().failure = Some(failure);
    shared.wake_main();
}

/// 用一个源出帧，直到停下（`Ok`）或者源坏了（`Err`）
fn drive(shared: &Shared, source: &mut dyn BlockingSource) -> Result<(), String> {
    let mut decimation = Decimation::default();
    let mut active = false;
    loop {
        {
            let mut state = shared.lock();
            if !state.stopped && (!state.started || state.paused) {
                if active {
                    // 先告诉源要睡了（放掉加速），再回来睡；不拿着锁调用源
                    drop(state);
                    source.set_active(false);
                    active = false;
                    continue;
                }
                // 暂停中刷新的节拍断了：恢复后的第一帧不按间隔丢掉
                decimation.reset();
                while !state.stopped && (!state.started || state.paused) {
                    state = shared.resume.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            }
            if state.stopped {
                return Ok(());
            }
        }
        if !active {
            source.set_active(true);
            active = true;
        }
        let Some(tick) = source.wait()? else { continue };
        let kind = source.current_kind();
        let mut state = shared.lock();
        if state.stopped {
            return Ok(());
        }
        // 源换了节拍（合成器时钟 ↔ 显示器的垂直消隐）：报告新的种类，数刷新重新开始
        if let Some(kind) = kind
            && kind != state.kind
        {
            state.kind = kind;
            decimation.reset();
        }
        // 暂停期间等到的、比帧率上限还密的不交
        if state.paused || !decimation.admit(tick.frame.target, tick.period, state.min_gap) {
            continue;
        }
        state.frame = Some(tick.frame);
        drop(state);
        shared.wake_main();
    }
}

/// 时钟线程比普通线程优先一档：醒得准，帧按时放进信箱
fn raise_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        // SAFETY: 伪句柄指调用线程自己，不用关；失败时照常以普通优先级运行
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    }
}

impl FrameClock for BlockingClock {
    fn kind(&self) -> FrameClockKind {
        match &self.fallback {
            Some(timer) => timer.kind(),
            None => self.shared.lock().kind,
        }
    }

    fn start(&mut self, on_frame: Box<dyn FnMut(FrameTime)>) {
        if let Some(timer) = &mut self.fallback {
            return timer.start(on_frame);
        }
        self.callback = Some(on_frame);
        let mut state = self.shared.lock();
        state.started = true;
        state.paused = false;
        drop(state);
        self.shared.resume.notify_all();
    }

    fn set_paused(&self, paused: bool) {
        if let Some(timer) = &self.fallback {
            return timer.set_paused(paused);
        }
        let mut state = self.shared.lock();
        if state.paused == paused {
            return;
        }
        state.paused = paused;
        if paused {
            state.frame = None;
        }
        drop(state);
        if !paused {
            self.shared.resume.notify_all();
        }
    }

    fn is_paused(&self) -> bool {
        match &self.fallback {
            Some(timer) => timer.is_paused(),
            None => self.shared.lock().paused,
        }
    }

    fn set_rate_hint(&self, range: FrameRateRange) {
        self.rate.set(range);
        if let Some(timer) = &self.fallback {
            return timer.set_rate_hint(range);
        }
        self.shared.lock().min_gap = range.min_interval();
    }

    fn dispatch(&mut self, now: Instant) -> usize {
        if self.fallback.is_none() {
            // 先清"叫过了"再取：取走之后线程放进来的新帧会再叫一次
            self.shared.woken.store(false, Ordering::Release);
            let (frame, failure, paused) = {
                let mut state = self.shared.lock();
                (state.frame.take(), state.failure.take(), state.paused)
            };
            match failure {
                Some(error) => self.fall_back(&error),
                None => {
                    let (Some(frame), false, Some(callback)) = (frame, paused, self.callback.as_mut()) else { return 0 };
                    callback(frame);
                    return 1;
                }
            }
        }
        self.fallback.as_mut().map_or(0, |timer| timer.dispatch(now))
    }

    fn deadline(&self) -> Option<Instant> {
        self.fallback.as_ref().and_then(TimerClock::deadline)
    }
}

impl Drop for BlockingClock {
    fn drop(&mut self) {
        self.shared.lock().stopped = true;
        self.shared.resume.notify_all();
    }
}
