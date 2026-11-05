//! 异步任务：在后台线程上运行 future 和耗时的计算，结果交回主线程更新界面。
//!
//! ```no_run
//! # use aureole::prelude::*;
//! # async fn load_user(id: u32) -> String { String::new() }
//! let name = signal(String::new());
//! // 后台线程池上运行，结果在主线程交给闭包，闭包里可以直接写信号
//! aureole::spawn(load_user(7)).then(move |user| name.set(user));
//! // 耗时的同步代码（计算、读大文件）放到阻塞线程池
//! aureole::spawn_blocking(|| (1..=20_000_000u64).filter(|n| n % 7 == 0).count()).then(|n| println!("{n}"));
//! // 主线程上的 async 流程：可以读写信号，用 .await 等后台任务
//! aureole::spawn_local(async move {
//!     let user = aureole::spawn(load_user(7)).await;
//!     name.set(user);
//! });
//! ```
//!
//! - **线程规则**：信号、元素这些界面句柄不是 `Send` 的，带不进后台任务（编译不过）。后台只处理数据，
//!   结果必须是 `Send`；回到主线程（[`Task::then`] 的闭包、[`spawn_local`] 里 `.await` 之后）再写信号。
//! - **归属**：任务和定时器一样归创建时的作用域所有，作用域释放（元素卸载、窗口关闭、副作用重新执行）时自动取消：
//!   后台的 future 被丢弃，结果不会再交回来。丢掉 [`Task`] 句柄不会取消任务。
//! - 主线程上的任务在创建时的作用域和窗口里、在批量更新里执行，和处理器一样。
//! - 完成的后台任务通过事件循环代理唤醒主线程，主线程在这一轮事件处理完时执行就绪的任务。
//!   没有任务时没有任何线程在工作：线程池的线程空闲时睡眠，空闲久了退出。
//! - 后台的工作交给执行器（[`Spawner`]）：默认是内置的线程池（[`PoolSpawner`]），应用可以换掉
//!   （`App::spawner`、[`set_spawner`]），比如 `tokio` 特性的 `TokioSpawner` 让 [`spawn`] 也跑在 tokio 上。
//! - 需要 tokio 运行时的库（reqwest、sqlx、tonic……）用 `tokio` 特性的 `spawn_tokio`：同样的 [`Task`]，
//!   在 tokio 运行时上跑。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use reactive::{Scope, batch, on_cleanup, untracked};

use crate::host::{WindowId, current_window_id, run_in_window};

#[cfg(feature = "tokio")]
mod tokio_bridge;
#[cfg(feature = "tokio")]
pub use tokio_bridge::{TokioSpawner, set_tokio_runtime, spawn_tokio, tokio_handle};

/// 后台任务 panic 时的负载
pub(crate) type Panic = Box<dyn Any + Send>;
type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;
/// 交给执行器的 future（[`Spawner::spawn`]）
pub type SendFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
/// 交给执行器的阻塞工作（[`Spawner::spawn_blocking`]）
pub type BlockingJob = Box<dyn FnOnce() + Send>;
type Job = BlockingJob;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// panic 的信息（`panic!` 的字符串），记日志用
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("（不是字符串）")
}

// ---------- 主线程：本地任务 ----------

/// 一个线程的收件箱：被唤醒的本地任务、投递过来要在这个线程上执行的工作（任何线程都可能放进来），
/// 以及这个线程发出、还没结束的后台工作数（测试工具等它们结束）
#[derive(Default)]
struct Inbox {
    state: Mutex<InboxState>,
    changed: Condvar,
}

#[derive(Default)]
struct InboxState {
    ready: Vec<u64>,
    /// 投递过来的工作（[`MainThreadSender`]），按投递的顺序执行
    posted: Vec<Job>,
    /// 还没结束的后台任务、sleep
    pending: usize,
    /// 有任务就绪时叫醒主线程；测试里没有
    waker: Option<MainThreadWaker>,
    /// 这个线程（和它发出的后台任务）用的执行器；`None` 是内置的线程池
    spawner: Option<Arc<dyn Spawner>>,
    /// 测试工具的虚拟时间（秒）；`None` 是真实时间。后台线程上的 `sleep` 也从这里读发出它的线程的时钟
    virtual_time: Option<f64>,
    /// 跟虚拟时间走的 `sleep`：(到期时间, 编号, 唤醒器)，测试工具推进时间时唤醒（[`wake_virtual_sleeps`]）
    virtual_sleeps: Vec<(f64, u64, Waker)>,
    next_sleep: u64,
}

impl InboxState {
    /// 没有就绪的任务，也没有投递过来的工作
    fn is_idle(&self) -> bool {
        self.ready.is_empty() && self.posted.is_empty()
    }

    /// 只在从空变成非空时唤醒：主线程一次取走全部
    fn wake_if_idle(&self) {
        if self.is_idle()
            && let Some(waker) = &self.waker
        {
            waker.wake();
        }
    }
}

impl Inbox {
    fn push(&self, id: u64) {
        let mut state = lock(&self.state);
        state.wake_if_idle();
        state.ready.push(id);
        drop(state);
        self.changed.notify_all();
    }

    fn post(&self, job: Job) {
        let mut state = lock(&self.state);
        state.wake_if_idle();
        state.posted.push(job);
        drop(state);
        self.changed.notify_all();
    }

    /// 开始一项后台工作，返回交给哪个执行器（同一次加锁里取，发出任务只锁一次收件箱）
    fn begin(&self) -> Option<Arc<dyn Spawner>> {
        let mut state = lock(&self.state);
        state.pending += 1;
        state.spawner.clone()
    }

    /// 一项后台工作结束。调用前先唤醒等它的任务，测试工具才不会在"结束了、还没就绪"的间隙里提前返回
    fn end(&self) {
        let mut state = lock(&self.state);
        state.pending = state.pending.saturating_sub(1);
        drop(state);
        self.changed.notify_all();
    }
}

struct Local {
    inbox: Arc<Inbox>,
    tasks: RefCell<HashMap<u64, LocalEntry>>,
    next: Cell<u64>,
}

struct LocalEntry {
    /// 正在轮询时为 `None`
    future: Option<LocalFuture>,
    scope: Option<Scope>,
    window: Option<WindowId>,
    waker: Waker,
}

thread_local! {
    static LOCAL: Local = Local { inbox: Arc::default(), tasks: RefCell::default(), next: Cell::new(0) };
    /// 正在轮询的后台任务算在哪个线程头上：里面再发出的任务、sleep 也算在它头上
    static CURRENT: RefCell<Option<Arc<Inbox>>> = const { RefCell::new(None) };
}

fn current_inbox() -> Arc<Inbox> {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(|| LOCAL.with(|l| l.inbox.clone()))
}

fn in_background() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

/// 在 `f` 期间把工作算在 `inbox` 的线程头上。`f` panic 了也恢复（守卫在展开时恢复），线程池的线程不会一直顶着别人的收件箱
fn with_inbox<R>(inbox: &Arc<Inbox>, f: impl FnOnce() -> R) -> R {
    /// 离开时（正常返回或者 panic 展开）恢复原来的
    struct Restore(Option<Arc<Inbox>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            let _ = CURRENT.try_with(|c| *c.borrow_mut() = previous);
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.replace(Some(inbox.clone()))));
    f()
}

struct LocalWake {
    id: u64,
    inbox: Arc<Inbox>,
}

impl Wake for LocalWake {
    fn wake(self: Arc<Self>) {
        self.inbox.push(self.id);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.inbox.push(self.id);
    }
}

/// 主线程上的任务（[`spawn_local`]、[`Task::then`] 返回）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalTask(u64);

impl LocalTask {
    /// 取消：丢弃 future，不再执行。已经结束的不做任何事
    pub fn cancel(self) {
        // 线程退出时（守卫析构）也可能走到这里
        let entry = LOCAL.try_with(|l| l.tasks.borrow_mut().remove(&self.0)).ok().flatten();
        drop(entry);
    }

    /// 已经结束（完成或被取消），和 [`Task::is_finished`] 同一个问法
    pub fn is_finished(self) -> bool {
        !LOCAL.with(|l| l.tasks.borrow().contains_key(&self.0))
    }
}

/// 在主线程上运行 future：可以读写信号、更新界面，用 `.await` 等后台任务（[`spawn`]、[`spawn_blocking`]）、
/// 文件对话框、[`sleep`]。只能在主线程调用。
///
/// 在这一轮事件处理完时开始执行（不是马上）；每次被唤醒都在创建时的作用域和窗口里、在批量更新里继续执行。
/// 作用域释放时取消
pub fn spawn_local(future: impl Future<Output = ()> + 'static) -> LocalTask {
    assert!(!in_background(), "spawn_local 只能在主线程调用，后台任务里用 spawn");
    let scope = Scope::current();
    let window = current_window_id();
    let task = LOCAL.with(|local| {
        let id = local.next.get() + 1;
        local.next.set(id);
        let waker = Waker::from(Arc::new(LocalWake { id, inbox: local.inbox.clone() }));
        let entry = LocalEntry { future: Some(Box::pin(future)), scope, window, waker: waker.clone() };
        local.tasks.borrow_mut().insert(id, entry);
        waker.wake();
        LocalTask(id)
    });
    if scope.is_some() {
        on_cleanup(move || task.cancel());
    }
    task
}

// ---------- 叫醒主线程 ----------

/// 叫醒主线程的句柄：后台线程有结果要交回主线程（任务做完、图片解码好、监视的文件读好）时调用，
/// 让睡着的事件循环醒来处理。运行时在事件循环开始前装上（[`set_main_thread_waker`]），实现是给事件循环发一个唤醒事件；
/// 没装时（测试工具）没有人叫：测试工具每帧自己去取结果。可以复制，可以带到别的线程
#[derive(Clone)]
pub struct MainThreadWaker(Arc<dyn Fn() + Send + Sync>);

impl MainThreadWaker {
    /// `wake` 会在任意线程上调用，要很快返回（不等主线程）
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(wake))
    }

    /// 叫醒主线程
    pub fn wake(&self) {
        (self.0)()
    }
}

impl std::fmt::Debug for MainThreadWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MainThreadWaker")
    }
}

thread_local! {
    /// 这个线程（主线程）装上的叫醒句柄
    static WAKER: RefCell<Option<MainThreadWaker>> = const { RefCell::new(None) };
}

/// 在主线程上装上叫醒它的句柄：之后就绪的任务、解码好的图片、读好的文件都用它叫醒事件循环。
/// 运行时在事件循环开始前调用一次
pub fn set_main_thread_waker(waker: MainThreadWaker) {
    LOCAL.with(|l| lock(&l.inbox.state).waker = Some(waker.clone()));
    WAKER.with(|w| *w.borrow_mut() = Some(waker));
}

/// 这个线程装上的叫醒句柄；没装时（测试工具、事件循环开始前）是 `None`
pub fn main_thread_waker() -> Option<MainThreadWaker> {
    WAKER.with(|w| w.borrow().clone())
}

/// 执行投递过来的工作和就绪的本地任务（事件循环每轮、测试工具每帧调用），返回是否做了事
pub fn run_ready_tasks() -> bool {
    LOCAL.with(|local| {
        let mut ran = false;
        // 任务不停唤醒自己时也不能卡住事件循环：最多几轮，剩下的留到下一轮
        for _ in 0..64 {
            let (posted, mut ready) = {
                let mut state = lock(&local.inbox.state);
                (std::mem::take(&mut state.posted), std::mem::take(&mut state.ready))
            };
            if posted.is_empty() && ready.is_empty() {
                return ran;
            }
            ran = true;
            ready.sort_unstable();
            ready.dedup();
            batch(|| {
                run_posted_jobs(posted);
                ready.into_iter().for_each(|id| poll_local(local, id));
            });
        }
        let state = lock(&local.inbox.state);
        if !state.is_idle()
            && let Some(waker) = &state.waker
        {
            waker.wake();
        }
        ran
    })
}

/// 投递过来的工作：不在任何作用域、窗口里，不追踪依赖
fn run_posted_jobs(posted: Vec<Job>) {
    for job in posted {
        untracked(job);
    }
}

/// 只执行投递过来的工作（每帧开始时、借用元素树之前，窗口宿主调用）：后台线程交回的结果
/// （解码好的图片、重新读好的文件）在这一帧就用上，不用等事件循环下一轮
pub(crate) fn run_posted() {
    let posted = LOCAL.with(|l| {
        let mut state = lock(&l.inbox.state);
        if state.posted.is_empty() { Vec::new() } else { std::mem::take(&mut state.posted) }
    });
    if !posted.is_empty() {
        batch(|| run_posted_jobs(posted));
    }
}

/// 等这个线程的收件箱有变化（有投递、有任务就绪、后台工作结束），直到 `done()`；超时返回 `false`。
/// 后台线程先改好 `done` 看的状态再投递，就不会错过（测试工具等图片解码完用）
pub(crate) fn wait_until(timeout: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    let inbox = LOCAL.with(|l| l.inbox.clone());
    let mut state = lock(&inbox.state);
    loop {
        if done() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        state = inbox.changed.wait_timeout(state, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
    }
}

// ---------- 主线程投递 ----------

/// 往主线程投递工作的句柄：任何线程上都能用（可以复制、带到别的线程），投进来的闭包在主线程上按投递的顺序执行，
/// 在批量更新里，不在任何作用域和窗口里。投递时叫醒事件循环，这一轮就执行；测试工具里在下一帧（或 `settle`）执行。
///
/// 给第三方库接主线程回调用：系统、原生库在别的线程上回调，要在主线程上碰界面的那一半。
/// 闭包要 `Send`，带不进信号；要更新界面，在主线程上用 [`spawn_local`] 等一个通道更顺手，
/// 投递适合"在主线程上做一下"（打开窗口、调只能在主线程调的系统接口、交给主线程上登记的处理器）
///
/// ```no_run
/// let sender = aureole::main_thread_sender();
/// std::thread::spawn(move || {
///     // ……别的线程上的回调……
///     sender.send(aureole::quit);
/// });
/// ```
#[derive(Clone)]
pub struct MainThreadSender {
    inbox: Weak<Inbox>,
}

impl MainThreadSender {
    /// 投递 `job`，在主线程上执行。主线程已经退出时直接丢掉，返回 `false`
    pub fn send(&self, job: impl FnOnce() + Send + 'static) -> bool {
        match self.inbox.upgrade() {
            Some(inbox) => {
                inbox.post(Box::new(job));
                true
            }
            None => false,
        }
    }
}

impl std::fmt::Debug for MainThreadSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MainThreadSender")
    }
}

/// 投递到这个线程（一般是主线程）的句柄；在后台任务里调用时是发出它的线程的
pub fn main_thread_sender() -> MainThreadSender {
    MainThreadSender { inbox: Arc::downgrade(&current_inbox()) }
}

fn poll_local(local: &Local, id: u64) {
    let taken = local.tasks.borrow_mut().get_mut(&id).and_then(|e| Some((e.future.take()?, e.scope, e.window, e.waker.clone())));
    // 已经结束、被取消，或者重复唤醒
    let Some((mut future, scope, window, waker)) = taken else { return };
    let mut poll = || untracked(|| future.as_mut().poll(&mut Context::from_waker(&waker)));
    let in_scope = || match scope {
        Some(scope) => scope.run(poll),
        None => poll(),
    };
    let result = match window {
        Some(window) => run_in_window(window, in_scope),
        None => in_scope(),
    };
    if result.is_ready() {
        let entry = local.tasks.borrow_mut().remove(&id);
        drop(entry);
    } else if let Some(entry) = local.tasks.borrow_mut().get_mut(&id) {
        entry.future = Some(future);
        return;
    }
    // 结束了，或者轮询期间被取消了：在借用之外析构
    drop(future);
}

// ---------- 后台：执行器 ----------

/// 后台执行器：[`spawn`]、[`spawn_blocking`]（以及 [`crate::resource`] 的请求）把工作交给它。
///
/// - 内置的是 [`PoolSpawner`]：按需开线程的计算池和阻塞池，没有任务时没有线程在工作；
/// - `tokio` 特性有 `TokioSpawner`：交给 tokio 运行时，依赖 tokio 的 future（reqwest、sqlx……）也能直接 `spawn`；
/// - 别的运行时自己实现这两个方法。
///
/// 每个线程（一般就是主线程）用一个执行器，后台任务里再发出的任务交给发出它的线程的那一个。
/// 应用用 `App::spawner` 换掉，测试和别的线程用 [`set_spawner`]。
///
/// 交来的 future 已经包好了：接住 panic、检查取消、把结果交回主线程、记在发出它的线程头上（测试工具等得到），
/// 执行器只管轮询到完成。取消时 future 会被唤醒，下一次轮询就结束，所以执行器不用实现取消。
/// 运行时关闭、没轮询完就丢掉 future 也可以：等结果的一方按 panic 收到"任务没有完成就被丢弃了"
///
/// ```no_run
/// use std::sync::Arc;
/// use aureole::{PoolSpawner, Spawner, SendFuture, BlockingJob};
///
/// /// 数一数发出了多少任务，再交给内置的线程池
/// struct Counting(Arc<std::sync::atomic::AtomicUsize>);
///
/// impl Spawner for Counting {
///     fn spawn(&self, future: SendFuture) {
///         self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
///         PoolSpawner.spawn(future);
///     }
///     fn spawn_blocking(&self, job: BlockingJob) {
///         PoolSpawner.spawn_blocking(job);
///     }
/// }
/// ```
pub trait Spawner: Send + Sync + 'static {
    /// 在后台轮询 `future` 直到完成；它被唤醒时要再轮询它。可以在任何线程上调用，调用方可能正拿着别的锁，
    /// 所以不要在这里同步地轮询
    fn spawn(&self, future: SendFuture);
    /// 在可以长时间阻塞的线程上运行 `job`（大量计算、同步读写文件），不占运行 future 的线程
    fn spawn_blocking(&self, job: BlockingJob);
}

/// 内置的执行器：两个按需开线程的线程池。future 在计算池上轮询（`aureole-async-N`，线程数是核数，限制在 2 到 8），
/// 阻塞的工作在阻塞池上（`aureole-blocking-N`，最多 64 个）。有活又没有空闲线程时开新线程，空闲久了的线程退出，
/// 没有任务时没有线程在工作。
///
/// 计算池没有 IO 反应器和计时器：依赖 tokio 的 future 放进来会 panic（"没有 tokio 运行时"），
/// 那种要换成 `TokioSpawner` 或者用 `spawn_tokio`。[`sleep`] 在哪个执行器上都能用
#[derive(Clone, Copy, Debug, Default)]
pub struct PoolSpawner;

impl Spawner for PoolSpawner {
    fn spawn(&self, future: SendFuture) {
        let task = Arc::new(BgTask { future: Mutex::new(Some(future)), scheduled: AtomicBool::new(false) });
        task.schedule();
    }

    fn spawn_blocking(&self, job: BlockingJob) {
        blocking_pool().execute(job);
    }
}

/// 为这个线程换上执行器：之后在这个线程上发出的后台任务（和它们里面再发出的）都交给它，已经发出的不受影响。
/// 应用一般用 `App::spawner`（运行时在事件循环开始前在主线程调用这里）；测试可以在自己的线程上换
pub fn set_spawner(spawner: impl Spawner) {
    let spawner: Arc<dyn Spawner> = Arc::new(spawner);
    LOCAL.with(|l| lock(&l.inbox.state).spawner = Some(spawner));
}

/// 这个线程正在用的执行器（后台任务里是发出它的线程的那一个），第三方库可以直接把自己的 future 交给它
/// （结果不交回主线程，要交回用 [`spawn`]）
pub fn spawner() -> Arc<dyn Spawner> {
    lock(&current_inbox().state).spawner.clone().unwrap_or_else(|| Arc::new(PoolSpawner))
}

/// 交给 `spawner`；`None` 是内置的线程池（不经过虚调用）
fn dispatch(spawner: Option<Arc<dyn Spawner>>, future: SendFuture) {
    match spawner {
        Some(spawner) => spawner.spawn(future),
        None => PoolSpawner.spawn(future),
    }
}

/// 包住交给执行器的 future：每次轮询都算在发出它的线程头上（里面再发出的任务、`sleep` 也是），测试工具才等得到
struct Homed<F> {
    future: F,
    inbox: Arc<Inbox>,
}

impl<F: Future + Unpin> Future for Homed<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        with_inbox(&this.inbox, || Pin::new(&mut this.future).poll(cx))
    }
}

// ---------- 后台：线程池 ----------

/// 按需开线程的线程池：有活、又没有空闲线程时开新线程（不超过上限）；空闲超过 `keep_alive` 的线程退出。
///
/// 出错不拖垮应用：每个活外面包着 `catch_unwind`，panic 记一条错误，线程接着干活；万一线程还是因为 panic 退出了
/// （框架自己的代码出错），退出时补上一个新线程，排着的活照样有人做
pub(crate) struct Pool {
    name: &'static str,
    max: usize,
    keep_alive: Duration,
    state: Mutex<PoolState>,
    available: Condvar,
}

#[derive(Default)]
struct PoolState {
    jobs: VecDeque<Job>,
    threads: usize,
    idle: usize,
    spawned: usize,
}

impl Pool {
    pub(crate) const fn new(name: &'static str, max: usize, keep_alive: Duration) -> Self {
        Self {
            name,
            max,
            keep_alive,
            state: Mutex::new(PoolState { jobs: VecDeque::new(), threads: 0, idle: 0, spawned: 0 }),
            available: Condvar::new(),
        }
    }

    pub(crate) fn execute(&'static self, job: Job) {
        let mut state = lock(&self.state);
        state.jobs.push_back(job);
        if state.idle > 0 {
            self.available.notify_one();
        }
        if state.jobs.len() > state.idle && state.threads < self.max {
            self.start_thread(state);
        }
    }

    /// 开一个线程（调用方已经确认没到上限）
    fn start_thread(&'static self, mut state: MutexGuard<'_, PoolState>) {
        state.threads += 1;
        state.spawned += 1;
        let name = format!("{}-{}", self.name, state.spawned);
        drop(state);
        if std::thread::Builder::new().name(name).spawn(move || self.work()).is_err() {
            lock(&self.state).threads -= 1;
        }
    }

    fn work(&'static self) {
        // 线程因为 panic 退出时补上一个（正常退出时它什么都不做）
        let _respawn = Respawn(self);
        let mut state = lock(&self.state);
        loop {
            if let Some(job) = state.jobs.pop_front() {
                drop(state);
                // 交来的活（任务、解码）一般自己接住了 panic；没接住的在这里接住，线程不会因此退出
                if let Err(panic) = catch_unwind(AssertUnwindSafe(job)) {
                    tracing::error!(pool = self.name, "后台线程上的工作 panic 了：{}", panic_message(&*panic));
                }
                state = lock(&self.state);
                continue;
            }
            state.idle += 1;
            let (guard, wait) = self.available.wait_timeout(state, self.keep_alive).unwrap_or_else(|e| e.into_inner());
            state = guard;
            state.idle -= 1;
            if wait.timed_out() && state.jobs.is_empty() {
                state.threads -= 1;
                return;
            }
        }
    }
}

/// 线程池的线程因为 panic 退出（展开到线程函数外面）时，记一条错误、补上一个线程接着做排着的活
struct Respawn(&'static Pool);

impl Drop for Respawn {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let pool = self.0;
        let mut state = lock(&pool.state);
        state.threads -= 1;
        tracing::error!(pool = pool.name, "线程池的线程 panic 退出了，补上一个新线程");
        if !state.jobs.is_empty() && state.threads < pool.max {
            pool.start_thread(state);
        }
    }
}

/// 运行 future 的线程池：future 不该阻塞，线程数不超过核数
fn async_pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Pool::new("aureole-async", cores.clamp(2, 8), Duration::from_secs(30))
    })
}

/// 运行阻塞代码的线程池：可以同时阻塞很多个
fn blocking_pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool::new("aureole-blocking", 64, Duration::from_secs(10)))
}

/// 计算池上的一个 future
struct BgTask {
    future: Mutex<Option<SendFuture>>,
    /// 已经排进线程池、还没开始轮询
    scheduled: AtomicBool,
}

impl Wake for BgTask {
    fn wake(self: Arc<Self>) {
        self.schedule();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.schedule();
    }
}

impl BgTask {
    fn schedule(self: &Arc<Self>) {
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            let task = self.clone();
            async_pool().execute(Box::new(move || task.run()));
        }
    }

    fn run(self: Arc<Self>) {
        // 先清标记：轮询期间被唤醒会再排一次，等这次轮询完再轮询
        self.scheduled.store(false, Ordering::Release);
        let mut slot = lock(&self.future);
        let Some(future) = slot.as_mut() else { return };
        let waker = Waker::from(self.clone());
        if future.as_mut().poll(&mut Context::from_waker(&waker)).is_ready() {
            let finished = slot.take();
            drop(slot);
            drop(finished);
        }
    }
}

/// 后台任务的结果，在后台线程和等它的一方之间传递
struct Slot<T> {
    state: Mutex<SlotState<T>>,
    cancelled: AtomicBool,
    inbox: Arc<Inbox>,
}

struct SlotState<T> {
    result: Option<Result<T, Panic>>,
    finished: bool,
    /// 等结果的一方
    waiter: Option<Waker>,
    /// 取消时唤醒后台的 future，让它尽快被丢弃
    canceller: Option<Waker>,
}

impl<T> Slot<T> {
    /// 在 `inbox` 名下开始一项后台工作，同时取出交给哪个执行器
    fn new(inbox: Arc<Inbox>) -> (Self, Option<Arc<dyn Spawner>>) {
        let spawner = inbox.begin();
        let state = SlotState { result: None, finished: false, waiter: None, canceller: None };
        (Self { state: Mutex::new(state), cancelled: AtomicBool::new(false), inbox }, spawner)
    }

    /// 轮询没完成：记下这次的唤醒器，取消时用它唤醒后台的 future（下一次轮询看到取消标记就结束）。
    /// 已经有了（同一个任务的唤醒器，或者 tokio 的 abort）就不换
    fn park(&self, waker: &Waker) {
        let mut state = lock(&self.state);
        if state.canceller.is_none() && !state.finished {
            state.canceller = Some(waker.clone());
        }
        drop(state);
        // 可能刚好在检查取消标记之后、记下唤醒器之前被取消：那次取消没唤醒任何人，这里补上
        if self.is_cancelled() {
            waker.wake_by_ref();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// 结束；`None` 表示被取消。只有第一次有效
    fn finish(&self, result: Option<Result<T, Panic>>) {
        let mut state = lock(&self.state);
        if state.finished {
            return;
        }
        state.finished = true;
        if !self.is_cancelled() {
            state.result = result;
        }
        let waiter = state.waiter.take().filter(|_| state.result.is_some());
        let canceller = state.canceller.take();
        drop(state);
        drop(canceller);
        if let Some(waiter) = waiter {
            waiter.wake();
        }
        self.inbox.end();
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut state = lock(&self.state);
        let result = state.result.take();
        let canceller = state.canceller.take();
        drop(state);
        drop(result);
        if let Some(canceller) = canceller {
            canceller.wake();
        }
    }

    fn poll_result(&self, cx: &mut Context<'_>) -> Poll<Result<T, Panic>> {
        let mut state = lock(&self.state);
        match state.result.take() {
            Some(result) => Poll::Ready(result),
            None => {
                state.waiter = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// 包住用户的 future：接住 panic、检查取消，结束时把结果放进 [`Slot`]
struct Body<F: Future> {
    future: Option<Pin<Box<F>>>,
    slot: Arc<Slot<F::Output>>,
}

impl<F: Future> Future for Body<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        let Some(future) = this.future.as_mut() else { return Poll::Ready(()) };
        let result = if this.slot.is_cancelled() {
            None
        } else {
            match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
                Ok(Poll::Pending) => {
                    this.slot.park(cx.waker());
                    return Poll::Pending;
                }
                Ok(Poll::Ready(value)) => Some(Ok(value)),
                Err(panic) => Some(Err(panic)),
            }
        };
        // 先丢掉 future（释放它占着的资源），再宣布结束：结束了就保证 future 已经析构
        let future = this.future.take();
        let _ = catch_unwind(AssertUnwindSafe(move || drop(future)));
        this.slot.finish(result);
        Poll::Ready(())
    }
}

/// 没轮询完就被丢弃（tokio 的 abort、运行时关闭）：同样先析构 future 再宣布结束。
/// 不是被取消的（运行时关闭）按 panic 处理，`.await` 的一方不会一直等下去
impl<F: Future> Drop for Body<F> {
    fn drop(&mut self) {
        let Some(future) = self.future.take() else { return };
        let _ = catch_unwind(AssertUnwindSafe(move || drop(future)));
        let result = (!self.slot.is_cancelled()).then(|| Err(Box::new("任务没有完成就被丢弃了（运行时已经关闭）") as Panic));
        self.slot.finish(result);
    }
}

/// 后台任务（[`spawn`]、[`spawn_blocking`] 返回）。
///
/// - `.await` 拿到结果（在 [`spawn_local`] 或另一个后台任务里）；后台 panic 时 `.await` 的一方也 panic。
/// - [`Task::then`] 在主线程处理结果；后台 panic 时不调用（panic 信息已经打印在终端）。
/// - 被取消的任务不会再交出结果，等它的 `.await` 也不会继续。
pub struct Task<T> {
    slot: Arc<Slot<T>>,
}

impl<T: Send + 'static> Task<T> {
    /// 完成后在主线程调用 `f`：在调用 `then` 时的作用域和窗口里、在批量更新里，可以直接写信号。
    /// 只能在主线程调用。作用域释放时连同任务一起取消。`f` 的返回值不用
    pub fn then<R>(self, f: impl FnOnce(T) -> R + 'static) -> LocalTask {
        spawn_local(async move {
            match self.result().await {
                Ok(value) => drop(f(value)),
                Err(panic) => tracing::error!("后台任务 panic 了，then 的处理器不调用：{}", panic_message(&*panic)),
            }
        })
    }

    /// 取消：还没开始的不再开始；`spawn` 的 future 被丢弃；`spawn_blocking` 已经在运行的闭包会跑完，结果丢掉
    pub fn cancel(self) {
        self.slot.cancel();
    }

    /// 已经结束（完成、panic 或被取消）
    pub fn is_finished(&self) -> bool {
        lock(&self.slot.state).finished
    }

    /// 结果；后台 panic 时是 `Err`
    pub(crate) fn result(self) -> impl Future<Output = Result<T, Panic>> {
        std::future::poll_fn(move |cx| self.slot.poll_result(cx))
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.slot.poll_result(cx) {
            Poll::Ready(Ok(value)) => Poll::Ready(value),
            Poll::Ready(Err(panic)) => resume_unwind(panic),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 任务归当前作用域所有：作用域释放时取消（只在主线程上）
fn own<T: Send + 'static>(slot: &Arc<Slot<T>>) {
    if !in_background() && Scope::current().is_some() {
        let slot: Weak<Slot<T>> = Arc::downgrade(slot);
        on_cleanup(move || {
            if let Some(slot) = slot.upgrade() {
                slot.cancel();
            }
        });
    }
}

/// 在后台运行 future，结果必须是 `Send`（界面句柄带不进去）。交给这个线程的执行器（[`Spawner`]，
/// 默认是内置的计算池；后台任务里是发出它的线程的那一个）。
/// 马上开始执行；用 [`Task::then`] 或 `.await` 拿结果。在主线程调用时归当前作用域所有，作用域释放时取消。
/// 在后台任务里也能调用
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let inbox = current_inbox();
    let (slot, spawner) = Slot::new(inbox.clone());
    let slot = Arc::new(slot);
    let body = Body { future: Some(Box::pin(future)), slot: slot.clone() };
    dispatch(spawner, Box::pin(Homed { future: body, inbox }));
    own(&slot);
    Task { slot }
}

/// 在后台线程上运行阻塞的代码（大量计算、同步读写文件），不占用运行 future 的线程
/// （交给执行器的 [`Spawner::spawn_blocking`]，默认是内置的阻塞池）。
/// 结果必须是 `Send`。归属和取消同 [`spawn`]
pub fn spawn_blocking<T, F>(f: F) -> Task<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let inbox = current_inbox();
    let (slot, spawner) = Slot::new(inbox.clone());
    let slot = Arc::new(slot);
    let mut body = BlockingBody { f: Some(f), slot: slot.clone(), inbox };
    let job: BlockingJob = Box::new(move || body.run());
    match spawner {
        Some(spawner) => spawner.spawn_blocking(job),
        None => PoolSpawner.spawn_blocking(job),
    }
    own(&slot);
    Task { slot }
}

/// 包住阻塞的闭包，同 [`Body`]：检查取消、接住 panic，结束时把结果放进 [`Slot`]
struct BlockingBody<F: FnOnce() -> T, T> {
    f: Option<F>,
    slot: Arc<Slot<T>>,
    inbox: Arc<Inbox>,
}

impl<F: FnOnce() -> T, T> BlockingBody<F, T> {
    fn run(&mut self) {
        let Some(f) = self.f.take() else { return };
        let result = if self.slot.is_cancelled() {
            // 先析构闭包再宣布结束，同 Body
            drop(f);
            None
        } else {
            Some(with_inbox(&self.inbox, || catch_unwind(AssertUnwindSafe(f))))
        };
        self.slot.finish(result);
    }
}

/// 执行器没运行就丢掉了（运行时关闭）：同 [`Body`] 的 `Drop`，按 panic 交回
impl<F: FnOnce() -> T, T> Drop for BlockingBody<F, T> {
    fn drop(&mut self) {
        let Some(f) = self.f.take() else { return };
        let _ = catch_unwind(AssertUnwindSafe(move || drop(f)));
        let result = (!self.slot.is_cancelled()).then(|| Err(Box::new("任务没有完成就被丢弃了（运行时已经关闭）") as Panic));
        self.slot.finish(result);
    }
}

// ---------- sleep ----------

/// 等一段时间再继续。主线程任务和后台任务里都能用，等待时不占线程。
///
/// 读统一的时钟（和定时器、动画同一个）：真实时间由一个计时线程唤醒（主线程卡住也不耽误后台任务里的 `sleep`）；
/// 测试工具里跟虚拟时间走，由 `Harness::advance`、`Harness::frame` 推进（后台任务里的也是，按发出它的线程的时钟）。
/// 虚拟时间的 `sleep` 不算没做完的后台工作：`settle` 不推进时间，不等它
pub fn sleep(duration: Duration) -> Sleep {
    let inbox = current_inbox();
    let virtual_now = lock(&inbox.state).virtual_time;
    let deadline = match virtual_now {
        Some(now) => Deadline::Virtual(now + duration.as_secs_f64()),
        None => Deadline::Real(Instant::now() + duration),
    };
    Sleep { deadline, inbox, entry: None }
}

/// [`sleep`] 返回的 future
pub struct Sleep {
    deadline: Deadline,
    /// 发出它的线程的收件箱：虚拟时间的 `sleep` 登记在这里
    inbox: Arc<Inbox>,
    entry: Option<SleepSlot>,
}

#[derive(Clone, Copy)]
enum Deadline {
    Real(Instant),
    /// 虚拟时间，秒
    Virtual(f64),
}

/// 睡下以后在哪里登记着
enum SleepSlot {
    /// 计时线程的表：编号和到期时要唤醒的
    Real(u64, Arc<SleepEntry>),
    /// 收件箱里虚拟时间的表：编号
    Virtual(u64),
}

struct SleepEntry {
    waker: Mutex<Option<Waker>>,
    done: AtomicBool,
    inbox: Arc<Inbox>,
}

impl SleepEntry {
    /// 到期：先唤醒再结束（见 [`Inbox::end`]）
    fn fire(&self) {
        if !self.done.swap(true, Ordering::AcqRel) {
            let waker = lock(&self.waker).take();
            if let Some(waker) = waker {
                waker.wake();
            }
            self.inbox.end();
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.deadline {
            Deadline::Real(deadline) => self.poll_real(deadline, cx),
            Deadline::Virtual(deadline) => self.poll_virtual(deadline, cx),
        }
    }
}

impl Sleep {
    fn poll_real(&mut self, deadline: Instant, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(SleepSlot::Real(_, entry)) = &self.entry {
            if entry.done.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            *lock(&entry.waker) = Some(cx.waker().clone());
            // 可能刚好在换唤醒器之前到期：再看一次
            return if entry.done.load(Ordering::Acquire) { Poll::Ready(()) } else { Poll::Pending };
        }
        if Instant::now() >= deadline {
            return Poll::Ready(());
        }
        self.inbox.begin();
        let entry = Arc::new(SleepEntry { waker: Mutex::new(Some(cx.waker().clone())), done: AtomicBool::new(false), inbox: self.inbox.clone() });
        let id = sleepers().add(deadline, entry.clone());
        self.entry = Some(SleepSlot::Real(id, entry));
        Poll::Pending
    }

    /// 虚拟时间：到了就完成；没到就把唤醒器登记（或者换掉）在收件箱的表里，测试工具推进时间时唤醒
    fn poll_virtual(&mut self, deadline: f64, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = lock(&self.inbox.state);
        let id = match self.entry {
            Some(SleepSlot::Virtual(id)) => Some(id),
            _ => None,
        };
        if state.virtual_time.is_none_or(|now| now >= deadline) {
            if let Some(id) = id {
                state.virtual_sleeps.retain(|(_, i, _)| *i != id);
            }
            drop(state);
            self.entry = None;
            return Poll::Ready(());
        }
        match id.and_then(|id| state.virtual_sleeps.iter_mut().find(|(_, i, _)| *i == id)) {
            Some((_, _, waker)) => waker.clone_from(cx.waker()),
            None => {
                state.next_sleep += 1;
                let id = state.next_sleep;
                state.virtual_sleeps.push((deadline, id, cx.waker().clone()));
                drop(state);
                self.entry = Some(SleepSlot::Virtual(id));
            }
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        match self.entry.take() {
            Some(SleepSlot::Real(id, entry)) if !entry.done.swap(true, Ordering::AcqRel) => {
                lock(&sleepers().state).entries.remove(&id);
                entry.inbox.end();
            }
            Some(SleepSlot::Virtual(id)) => lock(&self.inbox.state).virtual_sleeps.retain(|(_, i, _)| *i != id),
            _ => {}
        }
    }
}

/// 测试工具改了这个线程的虚拟时间：后台线程上的 `sleep` 也读这里
pub(crate) fn set_virtual_time(seconds: f64) {
    LOCAL.with(|l| lock(&l.inbox.state).virtual_time = Some(seconds));
}

/// 唤醒这个线程到期的虚拟时间的 `sleep`（测试工具推进时间后，[`crate::timer::run_due_timers`] 里），
/// 返回有没有唤醒、下一个到期时间（秒）
pub(crate) fn wake_virtual_sleeps(now: f64) -> (bool, Option<f64>) {
    let (due, next) = LOCAL.with(|l| {
        let mut state = lock(&l.inbox.state);
        if state.virtual_sleeps.is_empty() {
            return (Vec::new(), None);
        }
        let mut due = Vec::new();
        state.virtual_sleeps.retain(|(deadline, _, waker)| {
            if *deadline > now {
                return true;
            }
            due.push(waker.clone());
            false
        });
        let next = state.virtual_sleeps.iter().map(|(deadline, ..)| *deadline).reduce(f64::min);
        (due, next)
    });
    // 唤醒本地任务要锁收件箱：在锁外面唤醒
    let woke = !due.is_empty();
    due.into_iter().for_each(Waker::wake);
    (woke, next)
}

/// 一个计时线程管所有 sleep：睡到最早的到期时间，没有 sleep 时一直睡
struct Sleepers {
    state: Mutex<SleepersState>,
    changed: Condvar,
}

#[derive(Default)]
struct SleepersState {
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    entries: HashMap<u64, Arc<SleepEntry>>,
    next: u64,
    started: bool,
}

fn sleepers() -> &'static Sleepers {
    static SLEEPERS: OnceLock<Sleepers> = OnceLock::new();
    SLEEPERS.get_or_init(|| Sleepers { state: Mutex::default(), changed: Condvar::new() })
}

impl Sleepers {
    fn add(&'static self, deadline: Instant, entry: Arc<SleepEntry>) -> u64 {
        let mut state = lock(&self.state);
        state.next += 1;
        let id = state.next;
        state.heap.push(Reverse((deadline, id)));
        state.entries.insert(id, entry);
        if !state.started {
            state.started = true;
            std::thread::Builder::new().name("aureole-sleep".into()).spawn(move || self.run()).expect("启动计时线程失败");
        }
        drop(state);
        self.changed.notify_one();
        id
    }

    fn run(&'static self) {
        let mut state = lock(&self.state);
        loop {
            let now = Instant::now();
            let mut due = Vec::new();
            while let Some(&Reverse((deadline, id))) = state.heap.peek()
                && deadline <= now
            {
                state.heap.pop();
                due.extend(state.entries.remove(&id));
            }
            if !due.is_empty() {
                drop(state);
                due.iter().for_each(|entry| entry.fire());
                state = lock(&self.state);
                continue;
            }
            state = match state.heap.peek() {
                Some(&Reverse((deadline, _))) => self.changed.wait_timeout(state, deadline - now).unwrap_or_else(|e| e.into_inner()).0,
                None => self.changed.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }
}

// ---------- 测试工具 ----------

/// 执行就绪的本地任务，等这个线程发出的后台工作（任务、sleep）全部结束，直到没有事可做。
/// 超时返回 `false`
pub fn settle(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let inbox = LOCAL.with(|l| l.inbox.clone());
    loop {
        run_ready_tasks();
        let mut state = lock(&inbox.state);
        while state.is_idle() {
            if state.pending == 0 {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = inbox.changed.wait_timeout(state, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}
