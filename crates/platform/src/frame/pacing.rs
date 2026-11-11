//! 帧时钟里不碰系统的部分：计时器的节拍、刷新间隔的估计、表面帧回调的状态机，
//! 等刷新的源按帧率上限隔几次刷新交一帧、合成器时钟的返回值和节拍的对齐。
//! 时间都由调用方传进来，所以在任何平台上都能单元测试（无头后端的虚拟时间也走这里）。

use std::time::{Duration, Instant};

/// 一次心跳：这一帧开始的时刻和预计上屏的时刻
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tick {
    pub timestamp: Instant,
    pub target: Instant,
}

/// 帧间隔的上下限：超出这个范围的测量值不算（暂停过、漏了很多帧、时钟跳了）
const SHORTEST: Duration = Duration::from_micros(2_000);
const LONGEST: Duration = Duration::from_millis(50);

/// 刷新间隔的估计：最近几次相邻两帧间隔的中位数（偶尔漏一帧不影响）。还没量到时用给定的初值
#[derive(Clone, Debug)]
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) struct IntervalEstimate {
    initial: Duration,
    recent: Vec<Duration>,
    last: Option<Instant>,
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
impl IntervalEstimate {
    const WINDOW: usize = 9;

    pub(crate) fn new(initial: Duration) -> Self {
        Self { initial: initial.clamp(SHORTEST, LONGEST), recent: Vec::with_capacity(Self::WINDOW), last: None }
    }

    /// 改初值（帧率提示变了）：已经量到的不动
    pub(crate) fn set_initial(&mut self, initial: Duration) {
        self.initial = initial.clamp(SHORTEST, LONGEST);
    }

    /// 在 `at` 来了一帧
    pub(crate) fn observe(&mut self, at: Instant) {
        if let Some(last) = self.last
            && at > last
        {
            let delta = at - last;
            if (SHORTEST..=LONGEST).contains(&delta) {
                if self.recent.len() == Self::WINDOW {
                    self.recent.remove(0);
                }
                self.recent.push(delta);
            }
        }
        self.last = Some(at);
    }

    /// 中间断开过（暂停后恢复）：下一帧和上一帧之间的间隔不算
    pub(crate) fn break_sequence(&mut self) {
        self.last = None;
    }

    pub(crate) fn get(&self) -> Duration {
        if self.recent.is_empty() {
            return self.initial;
        }
        let mut sorted = self.recent.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }
}

/// 计时器的节拍：开始、暂停、恢复，按间隔排下一帧；落后了不补帧；有呈现回报时对齐到它
#[derive(Clone, Debug)]
pub(crate) struct TimerPacing {
    interval: Duration,
    started: bool,
    paused: bool,
    /// 下一帧的时刻；`None` 表示马上
    next: Option<Instant>,
    /// 最近一次 `poll` 传进来的时刻。马上要出帧时 `deadline` 返回它：在调用方的时间线上（虚拟时间也对）
    last_now: Option<Instant>,
    /// 最近一次呈现返回的时刻
    presented: Option<Instant>,
}

impl TimerPacing {
    pub(crate) fn new(interval: Duration) -> Self {
        Self { interval: clamp_interval(interval), started: false, paused: false, next: None, last_now: None, presented: None }
    }

    pub(crate) fn start(&mut self) {
        self.started = true;
        self.paused = false;
        self.next = None;
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if !paused {
            // 恢复后马上出一帧，不等一个间隔
            self.next = None;
        }
    }

    pub(crate) fn set_interval(&mut self, interval: Duration) {
        self.interval = clamp_interval(interval);
    }

    /// 一帧在 `at` 呈现返回了：之后的帧对齐到它之后的整数个间隔
    pub(crate) fn record_present(&mut self, at: Instant) {
        self.presented = Some(at);
    }

    /// 最晚什么时候要再 `poll`；没开始、暂停着时是 `None`
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if !self.started || self.paused {
            return None;
        }
        Some(match self.next {
            Some(next) => next,
            None => self.last_now.unwrap_or_else(Instant::now),
        })
    }

    /// 到点了就出一帧
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Tick> {
        self.last_now = Some(now);
        if !self.started || self.paused {
            return None;
        }
        if let Some(next) = self.next
            && now < next
        {
            return None;
        }
        let tick = Tick { timestamp: now, target: now + self.interval };
        self.next = Some(self.following(now));
        Some(tick)
    }

    /// 刚在 `now` 出了一帧，下一帧排在什么时候
    fn following(&self, now: Instant) -> Instant {
        let interval = self.interval;
        // 有最近的呈现回报：排在它之后第一个离现在至少半个间隔的整数倍上（不会紧挨着刚出的这帧再出一帧）
        if let Some(presented) = self.presented
            && presented <= now
            && now - presented < interval * 8
        {
            let elapsed = (now - presented).as_nanos();
            let step = interval.as_nanos().max(1);
            let mut k = (elapsed / step + 1) as u32;
            while presented + interval * k < now + interval / 2 {
                k += 1;
            }
            return presented + interval * k;
        }
        match self.next {
            // 准点：按原来的节拍走，不随调度的误差漂
            Some(previous) if now < previous + interval => previous + interval,
            // 落后超过一个间隔（主线程忙、刚恢复）：从现在重新起算，不补帧
            _ => now + interval,
        }
    }
}

fn clamp_interval(interval: Duration) -> Duration {
    interval.clamp(Duration::from_millis(1), Duration::from_secs(1))
}

/// 表面帧回调（Wayland 的 `wl_surface.frame`）的状态机。
///
/// 帧回调只在表面提交（commit）之后、合成器觉得该画下一帧时到。所以：
/// - 每次出帧之前先要下一个回调（这一帧的提交会带上它），已经有一个在路上就不再要；
/// - 开始、恢复时不等回调，马上出一帧（那时可能根本没有提交，回调不会来）；
/// - 出了一帧却没有提交（画面没变）时回调不会来，过一段时间还没来就按计时器补一帧（`stall_after`）。
///   窗口被藏起来时合成器也不发回调，这时按这个慢节奏走，直到运行时因为空闲暂停时钟。
#[derive(Clone, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct CallbackPacing {
    started: bool,
    paused: bool,
    /// 要了回调、还没到
    in_flight: bool,
    /// 回调到了，下一次 `poll` 出帧
    arrived: bool,
    /// 开始、恢复后马上出帧
    due_now: bool,
    last_frame: Option<Instant>,
    last_now: Option<Instant>,
    estimate: IntervalEstimate,
}

/// [`CallbackPacing::poll`] 的结果
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct CallbackStep {
    /// 要出的帧
    pub tick: Option<Tick>,
    /// 出帧之前先要一个新的帧回调
    pub request: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl CallbackPacing {
    pub(crate) fn new(interval: Duration) -> Self {
        Self {
            started: false,
            paused: false,
            in_flight: false,
            arrived: false,
            due_now: false,
            last_frame: None,
            last_now: None,
            estimate: IntervalEstimate::new(interval),
        }
    }

    pub(crate) fn start(&mut self) {
        self.started = true;
        self.paused = false;
        self.due_now = true;
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused
    }

    pub(crate) fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.due_now = false;
            self.arrived = false;
        } else {
            self.due_now = true;
            self.estimate.break_sequence();
        }
    }

    pub(crate) fn set_interval_hint(&mut self, interval: Duration) {
        self.estimate.set_initial(interval);
    }

    /// 回调没到多久就按计时器补一帧：四个间隔，至少 100 毫秒（藏起来的窗口最多每秒 10 帧）
    pub(crate) fn stall_after(&self) -> Duration {
        (self.estimate.get() * 4).max(Duration::from_millis(100))
    }

    /// 要的回调在 `at` 到了
    pub(crate) fn arrived(&mut self, at: Instant) {
        self.in_flight = false;
        if self.started && !self.paused {
            self.arrived = true;
            self.estimate.observe(at);
        }
    }

    /// 要的回调不会来了（对象没了、连接出错）
    pub(crate) fn lost(&mut self) {
        self.in_flight = false;
    }

    pub(crate) fn poll(&mut self, now: Instant) -> CallbackStep {
        self.last_now = Some(now);
        let idle = CallbackStep { tick: None, request: false };
        if !self.started || self.paused {
            return idle;
        }
        let stalled = self.last_frame.is_none_or(|last| now >= last + self.stall_after());
        if !(self.arrived || self.due_now || stalled) {
            return idle;
        }
        self.arrived = false;
        self.due_now = false;
        self.last_frame = Some(now);
        let request = !self.in_flight;
        self.in_flight = true;
        CallbackStep { tick: Some(Tick { timestamp: now, target: now + self.estimate.get() }), request }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        if !self.started || self.paused {
            return None;
        }
        if self.arrived || self.due_now {
            return Some(self.last_now.unwrap_or_else(Instant::now));
        }
        Some(self.last_frame.map_or_else(|| self.last_now.unwrap_or_else(Instant::now), |last| last + self.stall_after()))
    }
}

/// 刷新几次交一帧：两帧之间至少隔 `min_gap`（帧率上限的倒数），而且只能隔整数个刷新——隔 1.4 个刷新的话
/// 有的帧隔一个、有的隔两个，看着一顿一顿。上限不低于刷新率时每次刷新都交；只低一点（5% 以内）也算每次都交
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn refresh_divisor(period: Duration, min_gap: Duration) -> u32 {
    let ratio = min_gap.as_secs_f64() / period.as_secs_f64().max(1e-6);
    (ratio - 0.05).ceil().clamp(1.0, 1000.0) as u32
}

/// 等刷新的源每次刷新都醒，按帧率上限隔几次刷新交一帧（[`refresh_divisor`]）。
///
/// 数的是刷新：两帧的目标时刻之差除以刷新周期、四舍五入，不按醒来的时刻和上限比——线程醒得早一点晚一点
/// （半个周期以内）不会让一帧被错丢。合成器时钟给的目标时刻对齐在合成器的节拍上，根本不抖
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Decimation {
    /// 上一次交的那一帧的目标时刻
    last: Option<Duration>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Decimation {
    /// 节拍断了（暂停过、换了源）：下一帧照交
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }

    /// 目标时刻是 `target` 的这一帧交不交：离上一次交的够不够整数个刷新。时间往回走了（换了源、时钟跳了）当成重新开始
    pub(crate) fn admit(&mut self, target: Duration, period: Duration, min_gap: Duration) -> bool {
        if let Some(last) = self.last
            && target >= last
        {
            let refreshes = ((target - last).as_secs_f64() / period.as_secs_f64().max(1e-6)).round();
            if refreshes < f64::from(refresh_divisor(period, min_gap)) {
                return false;
            }
        }
        self.last = Some(target);
        true
    }
}

/// `WaitForVBlank` 没真的等：显示器睡着、远程桌面、基本显示适配器和某些虚拟显示器上它马上返回。
/// 时钟线程交完一帧立刻再等，正常时总要等将近一个刷新，不到 1 毫秒（刷新特别快时是四分之一个刷新）就返回只能是没等
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn vblank_returned_early(waited: Duration, interval: Duration) -> bool {
    waited < (interval / 4).min(Duration::from_millis(1))
}

/// 两个刷新率（赫兹）算不算一样：差在 2% 以内，或者不到 1Hz（系统报告显示器的刷新率只给整数，59.94Hz 报成 59）
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn rates_match(compositor_hz: f64, monitor_hz: f64) -> bool {
    (compositor_hz - monitor_hz).abs() <= (compositor_hz * 0.02).max(1.0)
}

/// 合成器时钟的窗口按哪个节拍出帧
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum ClockChoice {
    /// 合成器时钟
    #[default]
    Compositor,
    /// 窗口所在显示器的垂直消隐：合成器时钟跟着另一个刷新率不同的显示器
    WindowMonitor,
}

/// 显示器的刷新率不一样时，合成器时钟只跟其中一个（这台机器上：主显示器 160Hz、副显示器 165Hz，它按 165Hz 走）。
/// 窗口在另一个显示器上的话每秒多出几帧、看着一顿一顿，这时改等窗口所在显示器的垂直消隐；对上了（窗口挪过去了、
/// 合成器换了跟的显示器）换回合成器时钟。
///
/// 每隔一会儿看一次（[`observe`](Self::observe)）：连续 [`SUSTAIN`](Self::SUSTAIN) 次都说该换才换，偶尔一次对不上不换；
/// 哪一边读不到时不做决定
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct RefreshFollow {
    /// 一开始用合成器时钟
    choice: ClockChoice,
    /// 连续几次说该换
    streak: u32,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl RefreshFollow {
    pub(crate) const SUSTAIN: u32 = 4;

    pub(crate) fn choice(&self) -> ClockChoice {
        self.choice
    }

    /// 又看了一次：合成器时钟的帧周期、窗口所在显示器的刷新率（赫兹）。返回现在该用哪个
    pub(crate) fn observe(&mut self, compositor_period: Option<Duration>, monitor_hz: Option<f64>) -> ClockChoice {
        let (Some(period), Some(monitor_hz)) = (compositor_period.filter(|p| !p.is_zero()), monitor_hz) else {
            self.streak = 0;
            return self.choice;
        };
        let wanted = if rates_match(1.0 / period.as_secs_f64(), monitor_hz) { ClockChoice::Compositor } else { ClockChoice::WindowMonitor };
        if wanted == self.choice {
            self.streak = 0;
        } else {
            self.streak += 1;
            if self.streak >= Self::SUSTAIN {
                self.choice = wanted;
                self.streak = 0;
            }
        }
        self.choice
    }
}

/// `DCompositionWaitForCompositorClock` 的返回值是什么意思（`count` 是传进去的句柄数）。返回的是 NTSTATUS，
/// 成功、超时的值和 `WaitForMultipleObjects` 的一样
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum CompositorWait {
    /// 合成器时钟走了一格
    Tick,
    /// 第几个句柄有信号了
    Handle(u32),
    /// 到了超时还没走
    Timeout,
    /// 显示器关着（没人动、到了关屏的时间，或者远程桌面关了本机的屏幕）：不等就返回。显示器亮了又会走，不算坏了
    Occluded,
    /// 出错
    Failed,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn compositor_wait(code: u32, count: u32) -> CompositorWait {
    /// `STATUS_TIMEOUT`（就是 `WAIT_TIMEOUT`）
    const TIMEOUT: u32 = 0x102;
    /// `STATUS_GRAPHICS_PRESENT_OCCLUDED`
    const OCCLUDED: u32 = 0xC01E_0006;
    match code {
        // STATUS_WAIT_0（0）加句柄数：时钟走了；比句柄数小：那个句柄有信号
        code if code == count => CompositorWait::Tick,
        code if code < count => CompositorWait::Handle(code),
        TIMEOUT => CompositorWait::Timeout,
        OCCLUDED => CompositorWait::Occluded,
        _ => CompositorWait::Failed,
    }
}

/// 合成器的节拍：统计里某一帧的目标上屏时刻和帧周期（性能计数器的读数）。之后的刷新落在 `target + k × period` 上
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct CompositorGrid {
    pub target: u64,
    pub period: u64,
}

/// 合成器时钟走了一格时的帧时间和刷新周期。`now` 和节拍都是性能计数器的读数。
///
/// 有节拍时目标时刻对齐到节拍上：取节拍上第一个不早于"此刻加四分之一个周期"的点（醒来晚一点也还是下一次刷新），
/// `timestamp` 是它前一个点（上一次刷新），所以 [`FrameTime::interval`](super::FrameTime::interval) 正好是合成器的帧周期。
/// 节拍读不到、周期不合理、或者离此刻超过一秒（统计是旧的）时，从此刻起按 `fallback`（量到的间隔）往后推
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn compositor_frame(now: u64, grid: Option<CompositorGrid>, frequency: u64, fallback: Duration) -> (super::FrameTime, Duration) {
    let seconds = |ticks: u64| super::ticks_to_seconds(ticks, frequency);
    let sane = |g: &CompositorGrid| {
        let period = Duration::from_secs_f64(seconds(g.period));
        (SHORTEST..=LONGEST).contains(&period) && g.target.abs_diff(now) <= frequency
    };
    let Some(grid) = grid.filter(sane) else {
        let period = fallback.clamp(SHORTEST, LONGEST);
        let timestamp = seconds(now);
        return (super::FrameTime::from_seconds(timestamp, timestamp + period.as_secs_f64()), period);
    };
    let period = i128::from(grid.period);
    let offset = i128::from(now) + period / 4 - i128::from(grid.target);
    // 向上取整的除法：节拍上第一个不早于 now + period / 4 的点
    let steps = offset.div_euclid(period) + i128::from(offset.rem_euclid(period) > 0);
    let target = i128::from(grid.target) + steps * period;
    let ticks = |t: i128| u64::try_from(t).unwrap_or(0);
    let frame = super::FrameTime::from_seconds(seconds(ticks(target - period)), seconds(ticks(target)));
    (frame, Duration::from_secs_f64(seconds(grid.period)))
}
