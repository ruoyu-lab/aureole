//! Wayland 的帧时钟：表面的帧回调（`wl_surface.frame`）。
//!
//! 窗口库已经连着合成器，这里不另开连接：从窗口的原始句柄拿到 `wl_display` 和 `wl_surface`，
//! 以"客人"的身份挂在同一个连接上（不拥有连接，释放时不关），帧回调放在自己的事件队列里。
//! 不开线程：窗口库的事件循环读连接时，属于这个队列的事件也被读进来排着；事件循环每轮结束时
//! [`FrameClock::dispatch`] 把它们取出来。所以帧回调到了一定会叫醒事件循环（连接可读），不用另外的唤醒。
//!
//! libwayland-client 是运行时加载的（和窗口库一样），不链接任何原生界面工具包。
//! 节拍见 [`CallbackPacing`]：每次出帧前要下一个回调，回调没来时按慢计时器补帧。

use std::cell::{Cell, RefCell};
use std::time::Instant;

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use super::{CallbackPacing, ClockAnchor, FrameCallback, FrameClock, FrameClockKind, FrameRateRange, FrameTime, TimerClock};
use crate::PlatformWindow;

/// 事件队列分发时写进来：要的回调到了
#[derive(Default)]
struct Received {
    done: bool,
}

impl Dispatch<WlCallback, ()> for Received {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.done = true;
        }
    }
}

/// 表面帧回调驱动的帧时钟。必须在窗口之前释放（借用的 `wl_surface` 随窗口销毁）
pub(crate) struct WaylandClock {
    connection: Connection,
    queue: EventQueue<Received>,
    surface: WlSurface,
    received: Received,
    pacing: RefCell<CallbackPacing>,
    callback: Option<FrameCallback>,
    rate: Cell<FrameRateRange>,
    /// 连接出错以后换上的计时器
    fallback: Option<TimerClock>,
}

impl WaylandClock {
    /// 窗口不是 Wayland 的、拿不到表面时返回 `None`
    pub(crate) fn new(window: &dyn PlatformWindow, rate: FrameRateRange) -> Option<Self> {
        let RawWindowHandle::Wayland(surface) = window.window_handle().ok()?.as_raw() else { return None };
        let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else { return None };
        // SAFETY: 窗口库给的 wl_display 在窗口（事件循环）存活期间一直有效；这个时钟必须在窗口之前释放，
        // 客人身份的连接不拥有它、释放时不关
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: 窗口库给的 wl_surface 是这个连接上的有效代理，窗口存活期间一直有效（同上，时钟先于窗口释放）；
        // from_ptr 先核对代理的接口名是 wl_surface，对不上返回错误
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast()) }.ok()?;
        let surface = WlSurface::from_id(&connection, id).ok()?;
        let queue = connection.new_event_queue();
        let mut pacing = CallbackPacing::new(rate.interval());
        pacing.set_interval_hint(rate.interval());
        Some(Self {
            connection,
            queue,
            surface,
            received: Received::default(),
            pacing: RefCell::new(pacing),
            callback: None,
            rate: Cell::new(rate),
            fallback: None,
        })
    }

    fn fall_back(&mut self, error: &str) {
        tracing::warn!("Wayland 的帧回调不能用了：{error}；改用计时器");
        let mut timer = TimerClock::new(self.rate.get());
        let paused = self.pacing.get_mut().is_paused();
        if let Some(callback) = self.callback.take() {
            timer.start(callback);
        }
        timer.set_paused(paused);
        self.fallback = Some(timer);
    }
}

impl FrameClock for WaylandClock {
    fn kind(&self) -> FrameClockKind {
        match &self.fallback {
            Some(timer) => timer.kind(),
            None => FrameClockKind::SurfaceFrame,
        }
    }

    fn start(&mut self, on_frame: Box<dyn FnMut(FrameTime)>) {
        if let Some(timer) = &mut self.fallback {
            return timer.start(on_frame);
        }
        self.callback = Some(on_frame);
        self.pacing.get_mut().start();
    }

    fn set_paused(&self, paused: bool) {
        match &self.fallback {
            Some(timer) => timer.set_paused(paused),
            None => self.pacing.borrow_mut().set_paused(paused),
        }
    }

    fn is_paused(&self) -> bool {
        match &self.fallback {
            Some(timer) => timer.is_paused(),
            None => self.pacing.borrow().is_paused(),
        }
    }

    fn set_rate_hint(&self, range: FrameRateRange) {
        self.rate.set(range);
        match &self.fallback {
            Some(timer) => timer.set_rate_hint(range),
            None => self.pacing.borrow_mut().set_interval_hint(range.interval()),
        }
    }

    fn dispatch(&mut self, now: Instant) -> usize {
        if self.fallback.is_none() {
            // 取出窗口库读连接时排进这个队列的事件（不读连接、不阻塞）
            if let Err(error) = self.queue.dispatch_pending(&mut self.received) {
                self.fall_back(&error.to_string());
            }
        }
        if let Some(timer) = &mut self.fallback {
            return timer.dispatch(now);
        }
        let pacing = self.pacing.get_mut();
        if std::mem::take(&mut self.received.done) {
            pacing.arrived(now);
        }
        let step = pacing.poll(now);
        let Some(tick) = step.tick else { return 0 };
        if step.request {
            // 要下一个回调：这一帧的提交（渲染线程呈现时）会带上它
            let callback = self.surface.frame(&self.queue.handle(), ());
            if !callback.is_alive() {
                // 表面已经没了（窗口正在关）：回调不会来，下一帧再要
                pacing.lost();
            }
            if let Err(error) = self.connection.flush() {
                tracing::debug!("Wayland 连接写不进去（稍后窗口库会再写）：{error}");
            }
        }
        let Some(callback) = self.callback.as_mut() else { return 0 };
        callback(ClockAnchor::process().frame(tick));
        1
    }

    fn deadline(&self) -> Option<Instant> {
        match &self.fallback {
            Some(timer) => timer.deadline(),
            None => self.pacing.borrow().deadline(),
        }
    }
}
