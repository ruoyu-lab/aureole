//! 按刷新率的计时器：不开线程，由事件循环按 [`FrameClock::deadline`] 醒来、在 [`FrameClock::dispatch`] 里出帧。
//! 暂停时截止时间是 `None`，事件循环不为它醒来。X11、拿不到更好的时钟时用它；无头后端也用它（时间由调用方给）

use std::cell::RefCell;
use std::time::Instant;

use super::{ClockAnchor, FrameCallback, FrameClock, FrameClockKind, FrameRateRange, FrameTime, PresentFeedback, TimerPacing};

/// 计时器帧时钟。时间全由 [`dispatch`](FrameClock::dispatch) 传进来：真实的事件循环传 `Instant::now()`，
/// 测试可以传虚拟时间，结果是确定的
pub struct TimerClock {
    kind: FrameClockKind,
    pacing: RefCell<TimerPacing>,
    callback: Option<FrameCallback>,
    feedback: PresentFeedback,
}

impl TimerClock {
    /// 按 `rate` 的 `preferred` 出帧的计时器
    pub fn new(rate: FrameRateRange) -> Self {
        Self::with_kind(rate, FrameClockKind::Timer)
    }

    /// 无头后端用：同一个计时器，种类报成"手动"
    pub(crate) fn manual(rate: FrameRateRange) -> Self {
        Self::with_kind(rate, FrameClockKind::Manual)
    }

    fn with_kind(rate: FrameRateRange, kind: FrameClockKind) -> Self {
        Self {
            kind,
            pacing: RefCell::new(TimerPacing::new(rate.interval())),
            callback: None,
            feedback: PresentFeedback::default(),
        }
    }
}

impl FrameClock for TimerClock {
    fn kind(&self) -> FrameClockKind {
        self.kind
    }

    fn start(&mut self, on_frame: Box<dyn FnMut(FrameTime)>) {
        self.callback = Some(on_frame);
        self.pacing.borrow_mut().start();
    }

    fn set_paused(&self, paused: bool) {
        self.pacing.borrow_mut().set_paused(paused);
    }

    fn is_paused(&self) -> bool {
        self.pacing.borrow().is_paused()
    }

    fn set_rate_hint(&self, range: FrameRateRange) {
        self.pacing.borrow_mut().set_interval(range.interval());
    }

    fn dispatch(&mut self, now: Instant) -> usize {
        let tick = {
            let mut pacing = self.pacing.borrow_mut();
            if let Some(at) = self.feedback.take() {
                pacing.record_present(at);
            }
            pacing.poll(now)
        };
        let (Some(tick), Some(callback)) = (tick, self.callback.as_mut()) else { return 0 };
        callback(ClockAnchor::process().frame(tick));
        1
    }

    fn deadline(&self) -> Option<Instant> {
        self.pacing.borrow().deadline()
    }

    fn present_feedback(&self) -> Option<PresentFeedback> {
        Some(self.feedback.clone())
    }
}
