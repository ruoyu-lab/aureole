use std::marker::PhantomData;

use crate::runtime::{Kind, NodeId, with_runtime};

/// 作用域：拥有在 [`Scope::run`] 里创建的所有节点，释放时一起释放。
///
/// UI 组件挂载时开一个作用域，卸载时释放，组件里的信号和绑定就不会泄漏。
#[derive(Clone, Copy, Debug)]
pub struct Scope {
    id: NodeId,
    marker: PhantomData<*const ()>,
}

impl Scope {
    /// 新作用域归当前作用域（或正在执行的计算）所有
    pub fn new() -> Self {
        let id = with_runtime(|rt| rt.create(Kind::Scope, None, None));
        Self { id, marker: PhantomData }
    }

    /// 当前的作用域（或正在执行的派生值、副作用）；在所有作用域之外时为 `None`
    pub fn current() -> Option<Self> {
        with_runtime(|rt| rt.owner()).map(|id| Self { id, marker: PhantomData })
    }

    /// 在这个作用域里执行 `f`：期间创建的节点归它所有
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        with_runtime(|rt| rt.with_owner(self.id, f))
    }

    /// 释放作用域及它拥有的所有节点（包括子作用域）
    pub fn dispose(self) {
        with_runtime(|rt| rt.dispose(self.id));
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}
