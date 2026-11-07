use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::{DefinedAt, HandleMarker, Kind, NodeId, Value, read_value, try_read_value, with_runtime};

/// 存值：不需要响应的数据（`Rc` 包着的服务、配置、缓存）放在运行时里，换一个 `Copy` 的句柄。
///
/// 和 [`Signal`](crate::Signal) 一样归创建时的作用域、句柄能随意被闭包捕获，所以不用为每个闭包先克隆一份；
/// 不同的是读不订阅、写不通知，值变了界面也不会更新（要更新就用信号）。作用域释放后再读会 panic，
/// 确实可能已经释放时用 [`StoredValue::try_with`]。
pub struct StoredValue<T> {
    id: NodeId,
    defined: DefinedAt,
    marker: HandleMarker<T>,
}

/// 创建存值，等同于 [`StoredValue::new`]
#[track_caller]
pub fn stored_value<T: 'static>(value: T) -> StoredValue<T> {
    StoredValue::new(value)
}

impl<T: 'static> StoredValue<T> {
    /// 创建存值，归当前作用域所有
    #[track_caller]
    pub fn new(value: T) -> Self {
        let value: Value = Rc::new(value);
        let id = with_runtime(|rt| rt.create(Kind::Signal, Some(value), None));
        Self { id, defined: DefinedAt::here(), marker: PhantomData }
    }

    /// 借用读取（不订阅）。闭包里可以 [`StoredValue::set`]：手里的还是读开始时的值
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        read_value(with_runtime(|rt| rt.snapshot(self.id)), self.defined, f)
    }

    /// 读取一份（不订阅）
    #[track_caller]
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    /// 和 [`StoredValue::with`] 一样借用读取，已经释放时是 `None`
    #[track_caller]
    pub fn try_with<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        try_read_value(with_runtime(|rt| rt.snapshot(self.id)), self.defined, f)
    }

    /// 换成新的值（不通知谁）。已经释放时什么都不做
    pub fn set(&self, value: T) {
        with_runtime(|rt| rt.set(self.id, value));
    }

    /// 原地修改（不通知谁）。已经释放时什么都不做；正在读它（在它自己的 [`StoredValue::with`] 闭包里）时 panic
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        with_runtime(|rt| rt.update(self.id, f));
    }

    /// 释放。用于随数据创建的存值，删除数据时调用；平时交给作用域
    pub fn dispose(self) {
        with_runtime(|rt| rt.dispose_later(self.id));
    }
}

impl<T> Clone for StoredValue<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for StoredValue<T> {}

impl<T> PartialEq for StoredValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for StoredValue<T> {}

impl<T> fmt::Debug for StoredValue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StoredValue").field(&self.id).finish()
    }
}
