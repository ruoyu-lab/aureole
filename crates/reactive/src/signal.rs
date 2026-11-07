use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::{DefinedAt, HandleMarker, Kind, NodeId, Value, read_value, try_read_value, with_runtime};

/// 可读写的信号。
///
/// 句柄是 `Copy` 的，闭包可以随意捕获，不会和借用检查器冲突。值存在当前线程的
/// 运行时里，句柄不能跨线程。信号归创建它时的作用域所有，作用域释放后再读会 panic
/// （调试构建里说明它是在哪创建的）；确实可能已经释放时用 [`Signal::try_get`]、[`Signal::try_with`]。
pub struct Signal<T> {
    id: NodeId,
    defined: DefinedAt,
    marker: HandleMarker<T>,
}

/// 创建信号，等同于 [`Signal::new`]
#[track_caller]
pub fn signal<T: 'static>(value: T) -> Signal<T> {
    Signal::new(value)
}

impl<T: 'static> Signal<T> {
    /// 创建信号，归当前作用域所有
    #[track_caller]
    pub fn new(value: T) -> Self {
        let value: Value = Rc::new(value);
        let id = with_runtime(|rt| rt.create(Kind::Signal, Some(value), None));
        Self { id, defined: DefinedAt::here(), marker: PhantomData }
    }

    /// 借用读取并订阅：在派生值或副作用里调用时，信号变化会让它们重新执行。
    ///
    /// 闭包里可以用 [`Signal::set`]、[`Signal::set_if_changed`] 写这个信号：写入立即生效、通知下游，
    /// 闭包手里的还是读开始时的值。原地修改（[`Signal::update`]）要等读完
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        read_value(self.tracked(), self.defined, f)
    }

    /// 借用读取，不订阅
    #[track_caller]
    pub fn with_untracked<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        read_value(with_runtime(|rt| rt.snapshot(self.id)), self.defined, f)
    }

    /// 读取并订阅
    #[track_caller]
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    /// 读取，不订阅
    #[track_caller]
    pub fn get_untracked(&self) -> T
    where
        T: Clone,
    {
        self.with_untracked(T::clone)
    }

    /// 和 [`Signal::with`] 一样借用读取并订阅，信号已经释放时是 `None`（比如异步任务做完时界面已经关了）
    #[track_caller]
    pub fn try_with<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        try_read_value(self.tracked(), self.defined, f)
    }

    /// 和 [`Signal::get`] 一样读取并订阅，信号已经释放时是 `None`
    #[track_caller]
    pub fn try_get(&self) -> Option<T>
    where
        T: Clone,
    {
        self.try_with(T::clone)
    }

    fn tracked(&self) -> Result<Value, crate::runtime::Unreadable> {
        with_runtime(|rt| {
            rt.track(self.id);
            rt.snapshot(self.id)
        })
    }

    /// 写入并通知下游，即使新值和旧值相等。信号已释放时什么都不做
    pub fn set(&self, value: T) {
        with_runtime(|rt| {
            if rt.set(self.id, value) {
                rt.notify(self.id);
            }
        });
    }

    /// 只有新值和旧值不同时才写入并通知，返回是否写入
    pub fn set_if_changed(&self, value: T) -> bool
    where
        T: PartialEq,
    {
        with_runtime(|rt| {
            let changed = rt.set_if_changed(self.id, value);
            if changed {
                rt.notify(self.id);
            }
            changed
        })
    }

    /// 释放信号。用于随数据创建的信号（例如列表里每一项的状态），在删除数据时调用。
    ///
    /// 在批量更新或副作用执行期间调用（例如在点击的处理器里先删数据、再释放）时，
    /// 等这一轮更新跑完才真正释放：依赖它的界面会先卸载，不会读到已释放的信号
    pub fn dispose(self) {
        with_runtime(|rt| rt.dispose_later(self.id));
    }

    /// 原地修改并通知下游。信号已释放时什么都不做。
    ///
    /// 闭包里可以读写别的信号，不能读写这一个（闭包的参数就是它的值）；正在读这个信号的时候
    /// （在它自己的 [`Signal::with`] 闭包里）也不能原地修改它：这两种情况 panic，读的时候改用 [`Signal::set`]
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        with_runtime(|rt| {
            if rt.update(self.id, f) {
                rt.notify(self.id);
            }
        });
    }
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Signal<T> {}

impl<T> PartialEq for Signal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Signal<T> {}

impl<T> fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Signal").field(&self.id).finish()
    }
}
