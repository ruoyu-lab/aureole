//! 在单独的线程上阻塞等刷新的帧时钟（Windows 的合成器时钟、垂直消隐都是这样）：
//! 等到了把帧放进信箱、叫醒主线程，主线程在 [`FrameClock::dispatch`] 里交给回调。
//!
//! - 没开始、暂停着时线程睡在条件变量上，不等刷新，也不叫醒主线程。
//! - 主线程没来取之前只叫一次：新的帧覆盖旧的，主线程忙的时候不会被叫醒好几次。
//! - 等刷新的源出错（或者根本建不起来）时，主线程下一次 `dispatch` 换成计时器，接着出帧。
//! - 释放时通知线程退出，不等它：线程最多再等一次刷新（源的等待有超时）就结束。
//!
//! 等刷新的源是一个 trait，测试里换成假的源，所以这部分在每个平台上都跑测试（只有 Windows 真的用它）。
#![cfg_attr(not(windows), allow(dead_code))]

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::Waker;
use std::time::{Duration, Instant};

use super::{FrameCallback, FrameClock, FrameClockKind, FrameRateRange, FrameTime, TimerClock};

/// 阻塞等刷新的源（在时钟自己的线程里创建和使用）
pub(crate) trait BlockingSource {
    /// 阻塞到下一次刷新，返回这一帧的时间（统一时钟域）。`Ok(None)` 表示这次没等到（超时），
    /// 线程回去看一眼状态再等；`Err` 表示这个源不能用了
    fn wait(&mut self) -> Result<Option<SourceTick>, String>;
}

/// 源等到的一帧
pub(crate) type SourceTick = FrameTime;

struct State {
    started: bool,
    paused: bool,
    stopped: bool,
    /// 等到了、主线程还没取走的帧（只留最新的）
    frame: Option<FrameTime>,
    /// 源出错了，主线程换计时器
    failure: Option<String>,
    /// 两帧之间最短的间隔（秒，按帧率提示的上限）
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
    kind: FrameClockKind,
    shared: Arc<Shared>,
    callback: Option<FrameCallback>,
    rate: Cell<FrameRateRange>,
    /// 源坏了以后换上的计时器
    fallback: Option<TimerClock>,
}

impl BlockingClock {
    /// 开一个线程，在线程里用 `make` 建源（建不起来就换计时器），开始前一直睡着
    pub(crate) fn spawn<S, F>(kind: FrameClockKind, rate: FrameRateRange, waker: Waker, make: F) -> Self
    where
        S: BlockingSource,
        F: FnOnce() -> Result<S, String> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                started: false,
                paused: false,
                stopped: false,
                frame: None,
                failure: None,
                min_gap: rate.min_interval(),
            }),
            resume: Condvar::new(),
            woken: AtomicBool::new(false),
            waker,
        });
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new().name("帧时钟".into()).spawn(move || run(&thread_shared, make));
        if let Err(error) = spawned {
            shared.lock().failure = Some(format!("线程开不出来：{error}"));
        }
        Self { kind, shared, callback: None, rate: Cell::new(rate), fallback: None }
    }

    /// 源坏了：换成计时器，回调和暂停状态一起带过去
    fn fall_back(&mut self, error: &str) {
        tracing::warn!("帧时钟（{}）不能用了：{error}；改用计时器", self.kind.as_str());
        let mut timer = TimerClock::new(self.rate.get());
        let paused = self.shared.lock().paused;
        if let Some(callback) = self.callback.take() {
            timer.start(callback);
        }
        timer.set_paused(paused);
        self.fallback = Some(timer);
    }
}

fn run<S: BlockingSource>(shared: &Shared, make: impl FnOnce() -> Result<S, String>) {
    let mut source = match make() {
        Ok(source) => source,
        Err(error) => {
            shared.lock().failure = Some(error);
            shared.wake_main();
            return;
        }
    };
    let mut last: Option<Duration> = None;
    loop {
        {
            let mut state = shared.lock();
            while !state.stopped && (!state.started || state.paused) {
                // 暂停中刷新的节拍断了：恢复后的第一帧不按间隔丢掉
                last = None;
                state = shared.resume.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.stopped {
                return;
            }
        }
        match source.wait() {
            Ok(Some(frame)) => {
                let mut state = shared.lock();
                if state.stopped {
                    return;
                }
                // 暂停期间等到的、比帧率上限还密的不交
                if state.paused || last.is_some_and(|last| frame.timestamp.saturating_sub(last) < state.min_gap.mul_f64(0.9)) {
                    continue;
                }
                last = Some(frame.timestamp);
                state.frame = Some(frame);
                drop(state);
                shared.wake_main();
            }
            Ok(None) => {}
            Err(error) => {
                shared.lock().failure = Some(error);
                shared.wake_main();
                return;
            }
        }
    }
}

impl FrameClock for BlockingClock {
    fn kind(&self) -> FrameClockKind {
        match &self.fallback {
            Some(timer) => timer.kind(),
            None => self.kind,
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
