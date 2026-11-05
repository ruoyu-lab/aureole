//! macOS 的帧时钟：`CADisplayLink`（macOS 14 起），从窗口的视图创建，跟着窗口所在的显示器。
//!
//! 回调在主线程的运行循环里触发，每次刷新一次，时间本来就在统一时钟域（`CACurrentMediaTime`）。
//! 用 `preferredFrameRateRange` 告诉系统需要的帧率，ProMotion 屏才会切到 120Hz；不声明时系统可能一直停在 60Hz。
//! 加到运行循环的 common 模式里，拖动、缩放窗口时也继续出帧。
//!
//! 回调外面包着 `catch_unwind`：回调是从系统的运行循环里调进来的，panic 不能穿过系统的栈帧。
//! 接住以后暂停显示链接、记一条错误，下一次 [`FrameClock::dispatch`] 在调用方的栈上重新抛出。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSView;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{FrameCallback, FrameClock, FrameClockKind, FrameRateRange, FrameTime, panic_message};

/// 显示链接和回调之间共享的东西
#[derive(Default)]
struct TickerState {
    callback: RefCell<Option<FrameCallback>>,
    /// 回调里接住的 panic，等调用方来取
    panic: Cell<Option<Box<dyn Any + Send>>>,
    /// 暂停着（调用方暂停的，或者回调 panic 后自动暂停的）
    paused: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；Ticker 没有实现 Drop
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Rc<TickerState>]
    struct Ticker;

    impl Ticker {
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            let state = self.ivars();
            let time = FrameTime::from_seconds(link.timestamp(), link.targetTimestamp());
            // 先拿出来再调用：回调里可能换回调（start）、暂停时钟
            let Some(mut callback) = state.callback.borrow_mut().take() else { return };
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| callback(time)));
            {
                let mut slot = state.callback.borrow_mut();
                if slot.is_none() {
                    *slot = Some(callback);
                }
            }
            if let Err(payload) = result {
                link.setPaused(true);
                state.paused.set(true);
                tracing::error!("帧时钟的回调 panic 了：{}；显示链接已暂停，下一次交帧时在调用方重新抛出", panic_message(&*payload));
                state.panic.set(Some(payload));
            }
        }
    }
);

impl Ticker {
    fn new(mtm: MainThreadMarker, state: Rc<TickerState>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(state);
        // SAFETY: init 是父类 NSObject 的初始化方法，不带参数，返回初始化好的同一个对象，和返回类型 Retained<Self> 一致；
        // 实例变量在上一行已经设好
        unsafe { msg_send![super(this), init] }
    }
}

/// 包着 `CADisplayLink` 的帧时钟。必须在窗口之前释放
pub(crate) struct DisplayLinkClock {
    link: Retained<CADisplayLink>,
    _ticker: Retained<Ticker>,
    state: Rc<TickerState>,
}

impl DisplayLinkClock {
    /// 不在主线程、不是 AppKit 窗口、系统低于 macOS 14 时返回 `None`。建好时停着，[`FrameClock::start`] 后才出帧
    pub(crate) fn new(window: &dyn HasWindowHandle, rate: FrameRateRange) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView；这里只在这次调用里借用
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        if !view.respondsToSelector(sel!(displayLinkWithTarget:selector:)) {
            return None;
        }
        let state = Rc::new(TickerState { paused: Cell::new(true), ..TickerState::default() });
        let ticker = Ticker::new(mtm, state.clone());
        // SAFETY: Ticker 实现了 tick:，参数正是 CADisplayLink；显示链接强引用 ticker，ticker 和它一起活着
        let link = unsafe { view.displayLinkWithTarget_selector(&ticker, sel!(tick:)) };
        link.setPaused(true);
        let clock = Self { link, _ticker: ticker, state };
        clock.set_rate_hint(rate);
        // 加到 common 模式，拖动、缩放窗口时也继续出帧
        // SAFETY: 运行循环的线程要求：开头拿到了 MainThreadMarker，这里就在主线程上，加到的正是主线程的运行循环；
        // NSRunLoopCommonModes 是 Foundation 导出的不可变常量
        unsafe { clock.link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        Some(clock)
    }

    /// 回调里接住的 panic：在调用方的栈上重新抛出
    pub(crate) fn rethrow(&self) {
        if let Some(payload) = self.state.panic.take() {
            std::panic::resume_unwind(payload);
        }
    }
}

impl FrameClock for DisplayLinkClock {
    fn kind(&self) -> FrameClockKind {
        FrameClockKind::DisplayLink
    }

    fn start(&mut self, on_frame: Box<dyn FnMut(FrameTime)>) {
        *self.state.callback.borrow_mut() = Some(on_frame);
        self.set_paused(false);
    }

    fn set_paused(&self, paused: bool) {
        self.state.paused.set(paused);
        self.link.setPaused(paused);
    }

    fn is_paused(&self) -> bool {
        self.state.paused.get()
    }

    fn set_rate_hint(&self, range: FrameRateRange) {
        let range = range.sanitized();
        self.link.setPreferredFrameRateRange(CAFrameRateRange { minimum: range.min, maximum: range.max, preferred: range.preferred });
    }

    fn dispatch(&mut self, _now: Instant) -> usize {
        // 帧由显示链接直接在运行循环里交，这里只把接住的 panic 抛回给调用方
        self.rethrow();
        0
    }

    fn deadline(&self) -> Option<Instant> {
        None
    }
}

impl Drop for DisplayLinkClock {
    fn drop(&mut self) {
        // 运行循环持有 link，link 持有 ticker，必须 invalidate 才能断开
        self.link.invalidate();
    }
}
