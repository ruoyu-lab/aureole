//! 一个窗口的出帧节奏：平台给的帧时钟（[`FrameClock`]），加上空闲暂停和按时刻唤醒。
//!
//! - 时钟每帧在主线程上回调：运行时推进这个窗口的一帧，记下这一帧有没有做事、接下来要什么时候的帧；
//! - 连续 [`IDLE_FRAMES`] 帧什么都没做时暂停时钟（[`FramePacer::maybe_sleep`]）：暂停的时钟不再回调，
//!   也不再要事件循环为它醒来（截止时间是 `None`，等刷新的后台线程睡在条件变量上）；
//! - 窗口事件、到期的定时器、做完的任务、后台线程的消息让它恢复（[`FramePacer::wake_up`]），下一次刷新就出帧；
//! - **按时刻唤醒**：最后一帧说只要某个时刻的一帧（[`NextFrame::At`]，比如插入点下一次换亮灭）时，暂停着的时钟的
//!   截止时间就是那个时刻：事件循环睡到那时，[`FramePacer::dispatch`] 恢复时钟，下一次刷新出一帧；这一帧之后
//!   还是只要某个时刻的帧（或者什么都不要）时马上暂停，不再等 [`IDLE_FRAMES`] 帧。插入点闪烁每秒只醒两次。
//!
//! 事件循环每轮结束时对每个窗口调用 [`FramePacer::dispatch`]，再按 [`next_wake`] 决定睡到什么时候：
//! 所有窗口的时钟都暂停、没有定时器、也没有要按时刻出的帧时一直睡，画面静止的应用不再被唤醒。各平台用的是哪种时钟见 `platform::frame`。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use platform::{FrameClock, FrameTime, monotonic_time};

use aureole_core::host::{FrameReport, IDLE_FRAMES, clock};
use aureole_core::kinds::NextFrame;

/// 一个窗口的帧时钟和空闲计数，见[模块说明](self)
pub(crate) struct FramePacer {
    /// 必须在窗口之前释放：关窗口时 [`FramePacer::stop`] 先把它放掉
    clock: Option<Box<dyn FrameClock>>,
    state: Rc<State>,
    /// 出一帧：时钟每帧调用，[`FramePacer::frame_now`] 也调用
    frame: Rc<RefCell<dyn FnMut(FrameTime)>>,
}

/// 帧回调和事件循环共用的状态
#[derive(Default)]
struct State {
    /// 连续几帧什么都没做
    idle: Cell<u32>,
    /// 最后一帧要的那个时刻的帧：时钟暂停着时事件循环睡到这时
    wake_at: Cell<Option<Instant>>,
    /// 这一阵是到点醒来的（不是事件叫醒的）
    timed: Cell<bool>,
    /// 到点醒来出的那一帧之后不要每帧了：马上暂停
    sleep_now: Cell<bool>,
    /// 上一帧预计上屏的时刻：帧的时间不往回走（不按时钟出的帧在两次刷新之间）
    last_target: Cell<Duration>,
}

impl FramePacer {
    /// 开始出帧：之后每帧在主线程上调用 `frame`，它返回这一帧的结果（有没有做事、接下来要什么时候的帧）
    pub(crate) fn new(mut clock: Box<dyn FrameClock>, mut frame: impl FnMut(FrameTime) -> FrameReport + 'static) -> Self {
        let state = Rc::new(State::default());
        let s = state.clone();
        let run: Rc<RefCell<dyn FnMut(FrameTime)>> = Rc::new(RefCell::new(move |time: FrameTime| {
            let target = time.target.max(s.last_target.get());
            s.last_target.set(target);
            let report = frame(FrameTime { target, ..time });
            s.idle.set(if report.busy { 0 } else { s.idle.get().saturating_add(1) });
            s.wake_at.set(match report.next {
                NextFrame::At(at) => Some(clock::origin() + at),
                NextFrame::Idle | NextFrame::Continuous => None,
            });
            s.sleep_now.set(s.timed.get() && report.next != NextFrame::Continuous);
        }));
        let on_clock = run.clone();
        clock.start(Box::new(move |time| {
            // 帧里面开了嵌套的事件循环（模态对话框）：外面这一帧没做完，这一次跳过
            if let Ok(mut frame) = on_clock.try_borrow_mut() {
                frame(time);
            }
        }));
        Self { clock: Some(clock), state, frame: run }
    }

    /// 不等时钟，现在就出一帧（窗口刚改了大小：这一帧要和窗口的新大小一起生效）。
    /// 正在出帧（帧里面引起的）时什么也不做，返回 `false`
    pub(crate) fn frame_now(&self) -> bool {
        let Ok(mut frame) = self.frame.try_borrow_mut() else { return false };
        let now = monotonic_time();
        frame(FrameTime { timestamp: now, target: now });
        true
    }

    /// 有事了（输入、定时器、任务、后台的消息之后界面可能要变）：空闲计数清零，暂停着的时钟恢复
    pub(crate) fn wake_up(&self) {
        let state = &self.state;
        state.idle.set(0);
        state.timed.set(false);
        state.sleep_now.set(false);
        if let Some(clock) = &self.clock
            && clock.is_paused()
        {
            clock.set_paused(false);
        }
    }

    /// 连续几帧什么都没做、或者到点出的那一帧之后不要每帧了，就暂停时钟。`hold` 时不暂停（拖着文件悬停：要每帧问指针在哪）
    pub(crate) fn maybe_sleep(&self, hold: bool) {
        if hold || (self.state.idle.get() < IDLE_FRAMES && !self.state.sleep_now.get()) {
            return;
        }
        if let Some(clock) = &self.clock
            && !clock.is_paused()
        {
            clock.set_paused(true);
        }
    }

    /// 时钟暂停着（关掉以后也算）
    pub(crate) fn is_paused(&self) -> bool {
        self.clock.as_ref().is_none_or(|clock| clock.is_paused())
    }

    /// 把已经送到、已经到期的帧交给回调，返回交了几帧。暂停着、要的那个时刻到了时先恢复时钟（下一次刷新出帧）。
    /// 回调里接住的 panic 在这里重新抛出
    pub(crate) fn dispatch(&mut self, now: Instant) -> usize {
        let Some(clock) = self.clock.as_mut() else { return 0 };
        let state = &self.state;
        if clock.is_paused()
            && let Some(at) = state.wake_at.get()
            && at <= now
        {
            state.wake_at.set(None);
            state.idle.set(0);
            state.timed.set(true);
            state.sleep_now.set(false);
            clock.set_paused(false);
        }
        clock.dispatch(now)
    }

    /// 事件循环最晚什么时候要为这个窗口醒来：时钟走着时是时钟的截止时间（帧会自己叫醒事件循环时是 `None`）；
    /// 暂停着时是要按时刻出的那一帧，没有时是 `None`
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let clock = self.clock.as_ref()?;
        if clock.is_paused() { self.state.wake_at.get() } else { clock.deadline() }
    }

    /// 关窗口：暂停并放掉时钟（必须在窗口之前）
    pub(crate) fn stop(&mut self) {
        if let Some(clock) = self.clock.take() {
            clock.set_paused(true);
        }
    }
}

/// 事件循环下一次什么时候醒：几个截止时间（每个窗口的时钟、下一个定时器、问显示器的时刻）里最早的；
/// 都没有时是 `None`，一直睡到有事件
pub(crate) fn next_wake(deadlines: impl IntoIterator<Item = Option<Instant>>) -> Option<Instant> {
    deadlines.into_iter().flatten().min()
}
