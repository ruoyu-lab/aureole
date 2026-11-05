use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::{Compute, HandleMarker, Kind, NodeId, Runtime, with_runtime};

/// 派生值：由其他信号或派生值计算而来，结果被缓存。
///
/// 惰性计算：第一次读取时才算，之后只有依赖变了、并且再次被读取时才重算。
/// 重算结果和旧值相等时（`PartialEq`），下游不会被通知。
pub struct Memo<T> {
    id: NodeId,
    marker: HandleMarker<T>,
}

/// 创建派生值，等同于 [`Memo::new`]
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
    pub fn new(f: impl Fn() -> T + 'static) -> Self {
        let compute: Rc<dyn Compute> = Rc::new(MemoCompute { f, marker: PhantomData });
        let id = with_runtime(|rt| rt.create(Kind::Memo, None, Some(compute)));
        Self { id, marker: PhantomData }
    }

    /// 借用读取并订阅
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        with_runtime(|rt| {
            // 先订阅再更新：更新时 panic 了，读它的计算也记着这个依赖，它修好时会再算一次
            rt.track(self.id);
            rt.update_if_necessary(self.id);
            rt.read(self.id, f)
        })
    }

    /// 借用读取，不订阅
    pub fn with_untracked<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        with_runtime(|rt| {
            rt.update_if_necessary(self.id);
            rt.read(self.id, f)
        })
    }

    /// 读取并订阅
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    /// 读取，不订阅
    pub fn get_untracked(&self) -> T
    where
        T: Clone,
    {
        self.with_untracked(T::clone)
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
