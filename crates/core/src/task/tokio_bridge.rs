//! 和 tokio 的生态一起用（`tokio` 特性）：需要 tokio 运行时的库（reqwest、sqlx、tonic……）在这里跑，
//! 结果照样用 [`Task`] 交回主线程。
//!
//! 运行时默认第一次用时创建（多线程，最多 8 个工作线程，开着计时器和网络）；应用已经有自己的运行时时，
//! 在第一次用之前用 [`set_tokio_runtime`] 交给框架（在 `#[tokio::main]` 里第一次用时自动用那个）。
//! 任务的归属、取消、panic 和 [`super::spawn`] 一样：作用域释放时用 tokio 的 abort 取消
//! （future 在运行时的线程上被丢弃），panic 被接住，`.await` 的一方跟着 panic。

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::task::{Wake, Waker};

use tokio::runtime::{Builder, Handle, Runtime, RuntimeFlavor};
use tokio::task::AbortHandle;

use super::{BlockingJob, Body, Homed, SendFuture, Slot, Spawner, Task, current_inbox, lock, own};

static HANDLE: OnceLock<Handle> = OnceLock::new();
/// 框架自己创建的运行时：进程结束前一直留着
static OWNED: OnceLock<Runtime> = OnceLock::new();

/// 在 tokio 运行时上运行 future（`tokio` 特性）：里面可以用 `tokio::time`、`tokio::net`、`tokio::spawn`，
/// 以及依赖 tokio 的库（reqwest、sqlx、tonic……）。结果必须是 `Send`，用 [`Task::then`] 或 `.await` 拿。
///
/// 归属和取消同 [`super::spawn`]：在主线程调用时归当前作用域所有，作用域释放时取消（tokio 的 abort，
/// future 在运行时的线程上被丢弃）。panic 时 `then` 的闭包不调用，`.await` 的一方跟着 panic。
/// 运行时关闭（应用自己的运行时被析构）时没完成的任务按 panic 处理
///
/// ```no_run
/// # async fn fetch(url: &str) -> Result<String, String> { Ok(String::new()) }
/// let body = aureole::signal(String::new());
/// aureole::spawn_tokio(async { fetch("https://example.com").await }).then(move |r| body.set(r.unwrap_or_default()));
/// ```
pub fn spawn_tokio<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let inbox = current_inbox();
    // 不管这个线程换上了哪个执行器，都在 tokio 上跑
    let (slot, _) = Slot::new(inbox.clone());
    let slot = Arc::new(slot);
    let body = Body { future: Some(Box::pin(future)), slot: slot.clone() };
    let join = tokio_handle().spawn(Homed { future: body, inbox });
    {
        let mut state = lock(&slot.state);
        if !state.finished {
            state.canceller = Some(Waker::from(Arc::new(Abort(join.abort_handle()))));
        }
    }
    own(&slot);
    Task { slot }
}

/// 用应用自己的 tokio 运行时（多线程的）代替框架创建的。要在第一次 [`spawn_tokio`]、[`tokio_handle`] 之前调用；
/// 已经有运行时（设置过，或者已经创建了默认的）时原样退回 `Err`。单线程的运行时（`current_thread`）也退回：
/// 主线程要跑事件循环，没有线程推进它
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn app() -> impl View { rect() }
/// let runtime = tokio::runtime::Runtime::new().unwrap();
/// aureole::set_tokio_runtime(runtime.handle().clone()).unwrap();
/// App::new(app).run();
/// ```
pub fn set_tokio_runtime(handle: Handle) -> Result<(), Handle> {
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return Err(handle);
    }
    HANDLE.set(handle)
}

/// 正在用的 tokio 运行时。没有设置过时第一次调用决定：调用的线程已经在一个多线程的运行时里
/// （`#[tokio::main]` 的 `main` 里跑 `App::run`）就用它，否则创建一个。可以直接 `tokio_handle().spawn(..)`
/// （结果不交回主线程），或者在别的线程上 `block_on`
pub fn tokio_handle() -> Handle {
    HANDLE
        .get_or_init(|| {
            if let Ok(current) = Handle::try_current()
                && current.runtime_flavor() == RuntimeFlavor::MultiThread
            {
                return current;
            }
            let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
            let runtime = Builder::new_multi_thread()
                .worker_threads(cores.clamp(2, 8))
                .thread_name("aureole-tokio")
                .enable_all()
                .build()
                .expect("创建 tokio 运行时失败");
            let handle = runtime.handle().clone();
            let _ = OWNED.set(runtime);
            handle
        })
        .clone()
}

/// 交给 tokio 运行时的执行器（[`Spawner`]）：装上以后 [`super::spawn`]、[`super::spawn_blocking`]、
/// [`crate::resource`] 都在 tokio 上跑，依赖 tokio 的 future 直接 `spawn` 也行。
///
/// 应用用 `App::spawner(TokioSpawner::default())` 或者装上 `TokioPlugin`；只有几处要 tokio 时用 [`spawn_tokio`]，
/// 别的任务留在内置的线程池上。运行时默认是 [`tokio_handle`] 给的那个：在 `#[tokio::main]` 里就是调用方的运行时
///
/// ```no_run
/// # async fn fetch(url: &str) -> Result<String, String> { Ok(String::new()) }
/// aureole::set_spawner(aureole::TokioSpawner::default());
/// let body = aureole::signal(String::new());
/// // 不用 spawn_tokio：执行器就是 tokio
/// aureole::spawn(async { fetch("https://example.com").await }).then(move |r| body.set(r.unwrap_or_default()));
/// ```
#[derive(Clone, Debug)]
pub struct TokioSpawner(Handle);

impl TokioSpawner {
    /// 交给 `handle` 的运行时（多线程的；单线程的运行时没人推进，future 不会完成）
    pub fn new(handle: Handle) -> Self {
        Self(handle)
    }

    /// 用的运行时
    pub fn handle(&self) -> &Handle {
        &self.0
    }
}

/// [`tokio_handle`] 的运行时：调用方所在的、[`set_tokio_runtime`] 设的，或者框架创建的
impl Default for TokioSpawner {
    fn default() -> Self {
        Self(tokio_handle())
    }
}

impl Spawner for TokioSpawner {
    fn spawn(&self, future: SendFuture) {
        drop(self.0.spawn(future));
    }

    fn spawn_blocking(&self, job: BlockingJob) {
        drop(self.0.spawn_blocking(job));
    }
}

/// 取消时（[`Slot::cancel`] 唤醒它）abort 运行时上的任务
struct Abort(AbortHandle);

impl Wake for Abort {
    fn wake(self: Arc<Self>) {
        self.0.abort();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.abort();
    }
}
