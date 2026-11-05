//! 动画的时钟：所有窗口的过渡、退场、时间线、合成层动画、共享元素的飞行都按它的时间求值。
//!
//! **宿主时间**只有一个：统一时钟（[`crate::timer::now`]）的秒数。窗口里一帧的宿主时刻是显示链接给的预计上屏时刻，
//! 测试工具里是虚拟时间。**动画时间**由 [`Clock`] 从宿主时间换算：平时两者相等；暂停、慢放、拖动（[`AnimationClock`]）时不等。
//!
//! 主线程有一份时钟（这个线程上的所有窗口共用），每帧随消息交给各窗口的渲染线程（`LayerUpdates::clock`）：
//! 渲染线程跟着主线程时按那一帧的宿主时刻换算，主线程卡住时按收到那一帧之后过去的时间往后推，两边对同一个宿主时刻
//! 得到同一个动画时间，轨道按它采样，所以值一样。
//!
//! **此刻**：帧与帧之间（事件处理器里、定时器里）改目标、发时间线的命令时，宿主时刻是统一时钟的读数，但不早于
//! 最近一帧的时刻（帧的时刻是预计上屏的时刻，往往比读数晚一点）：从屏幕上最近的样子出发，不回到它之前。

use std::cell::Cell;

use animation::Clock;

#[derive(Clone, Copy)]
struct State {
    clock: Clock,
    /// 时钟每改一次（暂停、继续、改倍速、拖动）加一：窗口据此知道要把新的时钟交给渲染线程
    version: u64,
    /// 拖动过几次：动画时间跳过（可能往回跳），停下的轨道在新的时刻也许还在动，要重新采样
    seeks: u64,
    /// 最近一帧的宿主时刻（所有窗口里最晚的）
    frame: f64,
}

thread_local! {
    static STATE: Cell<State> = const { Cell::new(State { clock: Clock::new(0.0), version: 0, seeks: 0, frame: f64::NEG_INFINITY }) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|cell| {
        let mut state = cell.get();
        let result = f(&mut state);
        cell.set(state);
        result
    })
}

/// 主线程此刻的时钟
pub(crate) fn clock() -> Clock {
    STATE.with(|cell| cell.get().clock)
}

/// 时钟在走（没暂停、倍速大于 0）：停着时什么动画都不会动，窗口不用为它们出帧
pub(crate) fn running() -> bool {
    clock().is_running()
}

/// 时钟改过几次
pub(crate) fn version() -> u64 {
    STATE.with(|cell| cell.get().version)
}

/// 时钟拖动过几次
pub(crate) fn seeks() -> u64 {
    STATE.with(|cell| cell.get().seeks)
}

/// 此刻的宿主时间：统一时钟的读数，不早于最近一帧的时刻
pub(crate) fn host_now() -> f64 {
    let frame = STATE.with(|cell| cell.get().frame);
    crate::timer::now().max(frame)
}

/// 此刻的动画时间（帧与帧之间发时间线的命令用；元素树里改目标用各自的 `AnimState::now`，不早于各自最近的一帧）
pub(crate) fn now() -> f64 {
    clock().now(host_now())
}

/// 一帧开始：记下这一帧的宿主时刻，返回这一帧的动画时间
pub(crate) fn frame(host: f64) -> f64 {
    with(|state| {
        state.frame = state.frame.max(host);
        state.clock.now(host)
    })
}

/// 宿主时刻 `host` 对应的动画时间
pub(crate) fn time_at(host: f64) -> f64 {
    clock().now(host)
}

/// 测试工具：虚拟时钟从 0 重新开始时，动画时钟也回到起点（正常速度、在走），忘掉最近一帧的时刻。
/// 时钟本来就是这样（没人停过、拨过）时不算改过：同一个测试里再建一个测试工具，不打扰前一个
pub(crate) fn reset() {
    with(|state| {
        if state.clock != Clock::new(0.0) {
            state.clock = Clock::new(0.0);
            state.version += 1;
            state.seeks += 1;
        }
        state.frame = f64::NEG_INFINITY;
    });
}

/// 改时钟：在此刻的宿主时刻上改，动画时间不跳；版本加一，各窗口下一帧把新的时钟交给渲染线程
fn update(f: impl FnOnce(&mut Clock, f64)) {
    let host = host_now();
    with(|state| {
        f(&mut state.clock, host);
        state.version += 1;
    });
}

/// 动画的时钟（[`animation_clock`] 取）：这个线程上所有窗口的声明的过渡、退场、关键帧时间线、合成层动画、
/// 共享元素的飞行、拖放的影子都按它的时间求值，渲染线程上的那一份跟着它。调动画时用它暂停、慢放、拖动时间：
///
/// ```no_run
/// use std::time::Duration;
///
/// let clock = aureole::animation_clock();
/// clock.set_speed(0.25); // 慢放到四分之一
/// clock.pause(); // 停住：所有动画停在这一刻，渲染线程也不再出帧
/// clock.seek(clock.time() + Duration::from_millis(16)); // 停着时往后拨一帧
/// clock.resume(); // 从停住的地方接着走
/// ```
///
/// 不跟它走的：滚动（惯性、回弹、滚动条，是直接操作，暂停动画时滚动照常）、动图的播放、输入框的插入点闪烁。
///
/// 拖动只能拖到每条轨道最近一次改目标（时间线最近一次播放、暂停、跳转）之后：轨道不记更早的轨迹，
/// 拖到那之前时停在那一刻的样子；已经放完的时间线停在结尾
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnimationClock {
    _private: (),
}

/// 动画的时钟，见 [`AnimationClock`]
pub fn animation_clock() -> AnimationClock {
    AnimationClock { _private: () }
}

impl AnimationClock {
    /// 此刻的动画时间（平时等于统一时钟的读数：从进程第一次读时钟算起）
    pub fn time(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(now().max(0.0))
    }

    /// 倍速：1 是正常，小于 1 慢放，0 和暂停一样
    pub fn speed(&self) -> f64 {
        clock().speed()
    }

    /// 暂停着
    pub fn is_paused(&self) -> bool {
        clock().is_paused()
    }

    /// 从此刻起按 `speed` 倍速走（负数、非数按 0）。动画时间不跳
    pub fn set_speed(&self, speed: f64) {
        update(|clock, host| clock.set_speed(host, speed));
    }

    /// 停住：动画停在此刻的样子，之后改的目标、发的时间线命令也从这一刻起算，继续以后才动
    pub fn pause(&self) {
        update(|clock, host| clock.pause(host));
    }

    /// 从停住的地方接着走
    pub fn resume(&self) {
        update(|clock, host| clock.resume(host));
    }

    /// 把动画时间拨到 `time`（停着时停在那里，走着时从那里接着走）
    pub fn seek(&self, time: std::time::Duration) {
        update(|clock, host| clock.seek(host, time.as_secs_f64()));
        with(|state| state.seeks += 1);
    }
}
