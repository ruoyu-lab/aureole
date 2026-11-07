use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::{Compute, DefinedAt, HandleMarker, Kind, NodeId, Runtime, Unreadable, Value, read_value, try_read_value, with_runtime};

/// 派生值：由其他信号或派生值计算而来，结果被缓存。
///
/// 惰性计算：第一次读取时才算，之后只有依赖变了、并且再次被读取时才重算。
/// 重算结果和旧值相等时（`PartialEq`），下游不会被通知。和信号一样归创建时的作用域，释放后再读会 panic，
/// 确实可能已经释放时用 [`Memo::try_get`]、[`Memo::try_with`]。
pub struct Memo<T> {
    id: NodeId,
    defined: DefinedAt,
    marker: HandleMarker<T>,
}

/// 创建派生值，等同于 [`Memo::new`]
#[track_caller]
pub fn memo<T: PartialEq + 'static>(f: impl Fn() -> T + 'static) -> Memo<T> {
    Memo::new(f)
}

struct MemoCompute<T, F> {
    f: F,
    marker: PhantomData<fn() -> T>,
}

impl<T: PartialEq + 'static, F: Fn() -> T> Compute for MemoCompute<T, F> {
    fn compute(&self, rt: &Runtime, id: NodeId) -> bool {
        // 计算期间不拿着任何借用，计算里可以读别的派生值；算完和旧值比较，不同才存下
        let new = (self.f)();
        rt.store(id, new)
    }
}

impl<T: PartialEq + 'static> Memo<T> {
    /// 创建派生值；不会立即计算，第一次读取时才执行 `f`
    #[track_caller]
    pub fn new(f: impl Fn() -> T + 'static) -> Self {
        let compute: Rc<dyn Compute> = Rc::new(MemoCompute { f, marker: PhantomData });
        let id = with_runtime(|rt| rt.create(Kind::Memo, None, Some(compute)));
        Self { id, defined: DefinedAt::here(), marker: PhantomData }
    }

    /// 借用读取并订阅
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        read_value(self.current(true), self.defined, f)
    }

    /// 借用读取，不订阅
    #[track_caller]
    pub fn with_untracked<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        read_value(self.current(false), self.defined, f)
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

    /// 和 [`Memo::with`] 一样借用读取并订阅，派生值已经释放时是 `None`
    #[track_caller]
    pub fn try_with<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        try_read_value(self.current(true), self.defined, f)
    }

    /// 和 [`Memo::get`] 一样读取并订阅，派生值已经释放时是 `None`
    #[track_caller]
    pub fn try_get(&self) -> Option<T>
    where
        T: Clone,
    {
        self.try_with(T::clone)
    }

    /// 需要时先算一遍，返回此刻的值
    fn current(&self, track: bool) -> Result<Value, Unreadable> {
        with_runtime(|rt| {
            // 先订阅再更新：更新时 panic 了，读它的计算也记着这个依赖，它修好时会再算一次
            if track {
                rt.track(self.id);
            }
            rt.update_if_necessary(self.id);
            rt.snapshot(self.id)
        })
    }
}

impl<T> Clone for Memo<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Memo<T> {}

impl<T> fmt::Debug for Memo<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Memo").field(&self.id).finish()
    }
}
