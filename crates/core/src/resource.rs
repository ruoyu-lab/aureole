//! 数据资源 [`resource`]：依赖变了自动重新加载，过期结果丢弃；防抖 [`debounced`]。

use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use reactive::{Effect, Signal, signal, untracked};

use crate::task::{Task, spawn, spawn_local};
use crate::timer::after;

/// 异步加载的数据：依赖变了自动重新加载，加载中、数据、错误都是响应式的，直接绑定到界面上。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # async fn fetch_user(id: u32) -> Result<String, String> { Ok(String::new()) }
/// let id = signal(1);
/// let user = resource(move || id.get(), |id| async move { fetch_user(id).await });
/// text(move || match (user.get(), user.error()) {
///     (_, Some(error)) => format!("出错了：{error}"),
///     (Some(name), _) => name,
///     (None, _) => "加载中…".into(),
/// })
/// .opacity(move || if user.is_loading() { 0.5 } else { 1.0 });
/// ```
///
/// 和 [`resource`] 一起看：
/// - `source` 在副作用里执行，读到的信号就是依赖；依赖变了就用新值调用 `fetcher`。
/// - `fetcher` 在主线程调用（不追踪依赖），返回的 future 在后台线程池上运行（必须是 `Send`），结果交回主线程。
/// - **竞态安全**：依赖变化或 [`Resource::refetch`] 时，上一次还没完成的请求被取消（future 被丢弃），
///   晚到的旧结果不会覆盖新结果。
/// - 加载新数据期间 [`Resource::get`] 仍是上一次成功的结果（界面不闪），用 [`Resource::is_loading`] 显示加载状态；
///   失败时 [`Resource::error`] 有值，数据保留上一次成功的；之后成功时清掉错误。
/// - 归创建时的作用域所有：作用域释放（比如元素卸载）时取消进行中的请求。
/// - `fetcher` 返回的 future panic 时只结束加载状态（panic 信息打印在终端），不影响界面
pub struct Resource<T: 'static, E: 'static = String> {
    value: Signal<Option<T>>,
    error: Signal<Option<E>>,
    loading: Signal<bool>,
    version: Signal<u64>,
}

/// 创建 [`Resource`]：`source` 决定加载什么（依赖），`fetcher` 根据它返回加载数据的 future
pub fn resource<S, T, E, Fut>(source: impl Fn() -> S + 'static, fetcher: impl Fn(S) -> Fut + 'static) -> Resource<T, E>
where
    S: 'static,
    T: Send + 'static,
    E: Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    build(source, fetcher, spawn)
}

/// 同 [`resource`]，`fetcher` 返回的 future 在 tokio 运行时上运行（`tokio` 特性，见 [`crate::spawn_tokio`]）：
/// 用依赖 tokio 的库（reqwest、sqlx……）加载数据。依赖变化时上一次的请求用 tokio 的 abort 取消
///
/// ```no_run
/// # use aureole::prelude::*;
/// # async fn get(url: String) -> Result<String, String> { Ok(url) }
/// let id = signal(1);
/// let user = aureole::resource_tokio(move || id.get(), |id| get(format!("https://example.com/users/{id}")));
/// ```
#[cfg(feature = "tokio")]
pub fn resource_tokio<S, T, E, Fut>(source: impl Fn() -> S + 'static, fetcher: impl Fn(S) -> Fut + 'static) -> Resource<T, E>
where
    S: 'static,
    T: Send + 'static,
    E: Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    build(source, fetcher, crate::task::spawn_tokio)
}

/// `run` 把请求交给后台（框架的线程池或者 tokio 运行时）
fn build<S, T, E, Fut>(
    source: impl Fn() -> S + 'static,
    fetcher: impl Fn(S) -> Fut + 'static,
    run: fn(Fut) -> Task<Result<T, E>>,
) -> Resource<T, E>
where
    S: 'static,
    T: Send + 'static,
    E: Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
{
    let r = Resource { value: signal(None), error: signal(None), loading: signal(false), version: signal(0) };
    // 每次请求的编号：取消已经拦住了旧请求，这里再保险一次
    let latest = Rc::new(Cell::new(0u64));
    Effect::new(move || {
        r.version.with(|_| ());
        let input = source();
        let future = untracked(|| fetcher(input));
        let request = latest.get() + 1;
        latest.set(request);
        r.loading.set_if_changed(true);
        let latest = latest.clone();
        // 两个任务都归这一次执行所有：副作用重新执行（依赖变了）或作用域释放时一起取消
        let task = run(future);
        spawn_local(async move {
            let result = task.result().await;
            if latest.get() != request {
                return;
            }
            match result {
                Ok(Ok(value)) => {
                    r.value.set(Some(value));
                    if r.error.with_untracked(Option::is_some) {
                        r.error.set(None);
                    }
                }
                Ok(Err(error)) => r.error.set(Some(error)),
                Err(_) => {}
            }
            r.loading.set(false);
        });
    });
    r
}

impl<T: 'static, E: 'static> Resource<T, E> {
    /// 正在加载（第一次或重新加载）
    pub fn is_loading(&self) -> bool {
        self.loading.get()
    }

    /// 最近一次成功加载的数据；还没成功过时是 `None`
    pub fn get(&self) -> Option<T>
    where
        T: Clone,
    {
        self.value.get()
    }

    /// 借用读取数据
    pub fn with<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        self.value.with(|value| f(value.as_ref()))
    }

    /// 最近一次加载的错误；之后加载成功时清掉
    pub fn error(&self) -> Option<E>
    where
        E: Clone,
    {
        self.error.get()
    }

    /// 借用读取错误
    pub fn with_error<R>(&self, f: impl FnOnce(Option<&E>) -> R) -> R {
        self.error.with(|error| f(error.as_ref()))
    }

    /// 用同样的依赖重新加载（比如"刷新"按钮）
    pub fn refetch(&self) {
        self.version.update(|v| *v += 1);
    }

    /// 直接改数据（比如乐观更新），不发请求
    pub fn set(&self, value: T) {
        self.value.set(Some(value));
    }
}

impl<T, E> Clone for Resource<T, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, E> Copy for Resource<T, E> {}

impl<T, E> fmt::Debug for Resource<T, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resource").field("value", &self.value).field("loading", &self.loading).finish()
    }
}

/// 防抖：`source` 的值停止变化 `delay` 之后才跟着变（比如边打字边搜索，停下来才发请求）。
/// 返回的信号初始就是 `source` 当前的值。用定时器实现，测试工具里按虚拟时间推进
pub fn debounced<T: Clone + PartialEq + 'static>(source: impl Fn() -> T + 'static, delay: Duration) -> Signal<T> {
    let output = signal(untracked(&source));
    Effect::new(move || {
        let value = source();
        // 定时器归这一次执行所有：值再变时副作用重新执行，上一个定时器随之取消。
        // 变回当前输出的值时不用定时器（取消上一个就够了）
        if !output.with_untracked(|current| *current == value) {
            after(delay, move || {
                output.set_if_changed(value);
            });
        }
    });
    output
}
