//! 定时器：过一段时间执行一次，或每隔一段时间执行。还有统一的时钟（[`now`]）：定时器、`sleep`、帧时间、
//! 输入事件的时间戳、拖动的速度都读它，测试工具里换成虚拟时间。
//!
//! 闭包在主线程、在创建定时器时的作用域里执行（和处理器一样），可以随意读写信号。
//! 创建它的作用域释放时（比如元素卸载、窗口关闭）定时器自动取消。精度大约一帧。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reactive::{Scope, batch, on_cleanup};

/// 定时器的句柄。在作用域里创建的归作用域，作用域释放时取消；句柄丢掉不影响定时器。
/// 不在作用域里创建的一直有效，直到执行完（一次性的）或者 [`Timer::cancel`]；
/// 要让它跟着一个值走（库自己的结构体、插件），收成守卫：`every(..).guard()`（[`crate::Cancel::guard`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer(u64);

impl Timer {
    /// 取消；已经执行过（一次性的）或已经取消的不做任何事
    pub fn cancel(self) {
        // 线程退出时（守卫析构）定时器表可能已经析构
        let _ = TIMERS.try_with(|t| t.borrow_mut().entries.retain(|e| e.id != self.0));
    }
}

/// 过 `delay` 之后执行一次。闭包的返回值不用（`aureole::after(d, || aureole::open_url(..))` 可以直接写）
pub fn after<R>(delay: Duration, f: impl FnOnce() -> R + 'static) -> Timer {
    let f = RefCell::new(Some(f));
    schedule(delay, None, move || {
        if let Some(f) = f.borrow_mut().take() {
            f();
        }
    })
}

/// 每隔 `interval` 执行一次，直到取消或所在作用域释放。闭包的返回值不用
pub fn every<R>(interval: Duration, f: impl Fn() -> R + 'static) -> Timer {
    schedule(interval, Some(interval.as_secs_f64().max(1e-3)), move || {
        f();
    })
}

fn schedule(delay: Duration, interval: Option<f64>, f: impl Fn() + 'static) -> Timer {
    let scope = Scope::current();
    let callback: Rc<dyn Fn()> = match scope {
        Some(scope) => Rc::new(move || scope.run(&f)),
        None => Rc::new(f),
    };
    let timer = with_timers(|t| {
        t.next += 1;
        let deadline = now() + delay.as_secs_f64();
        t.entries.push(Entry { id: t.next, deadline, interval, callback });
        Timer(t.next)
    });
    if scope.is_some() {
        on_cleanup(move || timer.cancel());
    }
    timer
}

struct Entry {
    id: u64,
    /// 秒，见 [`now`]
    deadline: f64,
    interval: Option<f64>,
    callback: Rc<dyn Fn()>,
}

#[derive(Default)]
struct Timers {
    next: u64,
    entries: Vec<Entry>,
}

thread_local! {
    static TIMERS: RefCell<Timers> = RefCell::default();
    /// 测试工具用虚拟时间推进定时器
    static VIRTUAL: Cell<Option<f64>> = const { Cell::new(None) };
}

fn with_timers<R>(f: impl FnOnce(&mut Timers) -> R) -> R {
    TIMERS.with(|t| f(&mut t.borrow_mut()))
}

/// 统一的时钟从这个时刻算起（进程里第一次读时钟的时刻），所有线程、所有窗口一样
pub fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// 统一的时钟，秒：定时器、`sleep`、帧时间、输入事件的时间戳、拖动的速度都读它，所以几样东西之间的先后、间隔对得上。
/// 从 [`origin`] 算起；测试工具里是这个线程的虚拟时间
pub fn now() -> f64 {
    VIRTUAL.with(Cell::get).unwrap_or_else(|| origin().elapsed().as_secs_f64())
}

/// [`now`] 换成时长：事件的时间戳是它
pub fn now_duration() -> Duration {
    Duration::from_secs_f64(now().max(0.0))
}

/// 时长换成秒，整数纳秒除以 10 亿：`secs(1.2)`、录下的微秒时间戳换回来和原来的字面量一模一样
pub fn seconds(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1e9
}

/// 一个真实时刻在统一时钟上的读数（帧时钟给的预计上屏时刻换成帧时间）
pub fn time_at(instant: Instant) -> f64 {
    instant.saturating_duration_since(origin()).as_secs_f64()
}

/// 测试工具：这个线程改用虚拟时间。定时器、动画、`sleep`（包括这个线程发出的后台任务里的）都跟着它走
pub fn set_virtual_time(seconds: f64) {
    VIRTUAL.with(|v| v.set(Some(seconds)));
    crate::task::set_virtual_time(seconds);
}

/// 测试工具：这个线程的虚拟时钟从 0 重新开始，还没到期的定时器跟着挪，剩下的等待时间不变。
///
/// 新的测试工具调用它。它的视图在调用之前就构建好了（是 `Harness::new` 的参数），构建时建的定时器按旧的时钟
/// （上一个测试工具推进到的虚拟时间，或者还没有测试工具时的真实时间）定了到期时间；不跟着挪的话，
/// 要在新的时钟上多等旧时钟走过的那么久，等于一直不触发。挪过以后就和先拨好钟再构建视图一样
pub fn restart_virtual_time() {
    let elapsed = now();
    with_timers(|t| t.entries.iter_mut().for_each(|e| e.deadline -= elapsed));
    set_virtual_time(0.0);
    // 动画时钟也回到起点：上一个测试工具暂停过、拖动过的不留到这里
    crate::clock::reset();
}

/// 执行到期的定时器、唤醒到期的（跟虚拟时间走的）`sleep`，返回有没有做事、下一个到期时间离现在还有多久
/// （没有定时器和虚拟时间的 `sleep` 时为 `None`）
pub fn run_due_timers() -> (bool, Option<Duration>) {
    let now = now();
    let due: Vec<Rc<dyn Fn()>> = with_timers(|t| {
        let mut due = Vec::new();
        t.entries.retain_mut(|e| {
            if e.deadline > now {
                return true;
            }
            due.push(e.callback.clone());
            match e.interval {
                Some(interval) => {
                    // 卡顿之后不补执行错过的次数，从现在起重新计时
                    e.deadline = (e.deadline + interval).max(now + interval * 0.5);
                    true
                }
                None => false,
            }
        });
        due
    });
    if !due.is_empty() {
        batch(|| due.iter().for_each(|f| f()));
    }
    // 虚拟时间的 sleep：真实时间的由计时线程唤醒，不在这里
    let (woke, next_sleep) = crate::task::wake_virtual_sleeps(now);
    let next = with_timers(|t| t.entries.iter().map(|e| e.deadline).reduce(f64::min))
        .into_iter()
        .chain(next_sleep)
        .reduce(f64::min)
        .map(|deadline| Duration::from_secs_f64((deadline - now).max(0.0)));
    (!due.is_empty() || woke, next)
}
