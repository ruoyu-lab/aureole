//! 细粒度响应式：信号、派生值、副作用。
//!
//! - [`Signal`]：可读写的值；在派生值或副作用里读取时自动订阅
//! - [`Memo`]：由其他值算出来的缓存值，惰性计算，结果没变时不通知下游
//! - [`Effect`]：读过的值变了就重新执行，界面上的属性绑定都是它
//! - [`Scope`]：拥有期间创建的节点，释放时一起释放，组件卸载靠它避免泄漏
//! - [`StoredValue`]：不需要响应的值（`Rc` 包着的服务、配置）换一个 `Copy` 的句柄，闭包直接捕获，读写不订阅也不通知
//! - [`batch`]、[`untracked`]、[`on_cleanup`]：批量更新、不订阅地读取、随作用域释放的清理
//! - [`provide_context`]、[`context`]：给作用域里的代码传"环境"（比如它属于哪个窗口）
//!
//! 这个 crate 不依赖界面，`aureole` 在它上面做属性绑定、列表和组件的生命周期。
//!
//! ```
//! use std::cell::Cell;
//! use std::rc::Rc;
//! use aureole_reactive::{Effect, batch, memo, signal};
//!
//! let count = signal(1);
//! let double = memo(move || count.get() * 2);
//! let runs = Rc::new(Cell::new(0));
//! let seen = runs.clone();
//! Effect::new(move || {
//!     seen.set(seen.get() + 1);
//!     println!("double = {}", double.get());
//! });
//! count.set(2); // 只有依赖 count 的部分重新执行
//! assert_eq!(double.get_untracked(), 4);
//! batch(|| {
//!     count.set(3);
//!     count.set(4);
//! }); // 批量更新结束时副作用只执行一次
//! assert_eq!(runs.get(), 3);
//! ```
//!
//! 所有句柄都是 `Copy` 的，存在当前线程的运行时里，不能跨线程。
//! UI 状态在主线程；交给渲染线程的是数据，不是信号。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod effect;
mod memo;
mod runtime;
mod scope;
mod signal;
mod stored;

pub use effect::{Effect, effect};
pub use memo::{Memo, memo};
pub use scope::Scope;
pub use signal::{Signal, signal};
pub use stored::{StoredValue, stored_value};

use runtime::with_runtime;

/// 批量更新：期间的写入只标记，结束时统一执行受影响的副作用，每个最多一次
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    with_runtime(|rt| rt.batch(f))
}

/// 当前作用域（或正在执行的副作用、派生值的这一次执行）释放时调用 `f`。
/// 用来释放作用域之外的资源，比如定时器、外部订阅。
///
/// 只有作用域释放才执行：在所有作用域之外调用时永远不会执行，线程结束时还没释放的作用域也不执行
/// （响应式运行时那时已经在析构，读不了信号），闭包连同捕获的值直接丢掉。
///
/// 执行时同一作用域里的节点都还在（"卸载时保存草稿"可以直接读作用域里的信号），读取不订阅；
/// 清理函数里的写入等这次释放做完再执行副作用
pub fn on_cleanup(f: impl FnOnce() + 'static) {
    with_runtime(|rt| rt.on_cleanup(Box::new(f)))
}

/// 现在就执行已经排队的副作用，即使在 [`batch`] 中间。批量更新结束时如果又有写入，
/// 受影响的副作用会再执行一次。正在执行副作用时调用不做任何事
pub fn flush_effects() {
    with_runtime(|rt| rt.flush_now())
}

/// 在当前作用域上提供一个值（在副作用、派生值里调用时是它所在的作用域）：在这个作用域里创建的节点、子作用域，
/// 包括以后重新执行的副作用，都能用 [`context`] 找到它。同一个作用域上同一种类型以最后一次为准，
/// 作用域释放时一起释放；在所有作用域之外调用时什么都不做。
///
/// 用来给一棵子树传"环境"（比如它属于哪个窗口），不用一层层传参数
pub fn provide_context<T: 'static>(value: T) {
    with_runtime(|rt| rt.provide(std::rc::Rc::new(value)))
}

/// 从当前作用域（或正在执行的副作用、派生值）沿拥有者往外找最近提供的 `T`（见 [`provide_context`]），
/// 找不到时为 `None`。每往外一层查一次表，作用域很深、调用很频繁时注意开销（一般只在绑定第一次执行时调用）
pub fn context<T: Clone + 'static>() -> Option<T> {
    with_runtime(|rt| rt.context::<T>())
}

/// 在所有作用域之外执行 `f`：里面创建的信号、派生值、副作用不归当前的作用域（或正在执行的副作用）所有，
/// 不会随它释放，一直活到线程结束；读取的信号也不订阅。用来懒创建全局状态（比如主题），
/// 不管第一次用到它的地方在哪个作用域里
pub fn detached<R>(f: impl FnOnce() -> R) -> R {
    with_runtime(|rt| rt.detached(f))
}

/// 在闭包里读取信号不会订阅
pub fn untracked<R>(f: impl FnOnce() -> R) -> R {
    with_runtime(|rt| rt.untracked(f))
}

/// 当前线程存活的响应式节点数，用于检查泄漏
pub fn live_node_count() -> usize {
    with_runtime(|rt| rt.node_count())
}
