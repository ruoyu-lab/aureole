use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::{Compute, Kind, NodeId, Runtime, with_runtime};

/// 副作用：创建时立即执行一次，之后读过的信号或派生值变了就重新执行。
///
/// 执行期间创建的信号、派生值、副作用归它所有，下一次执行前全部释放。
/// UI 里属性绑定、往渲染数据里写值都靠它。
#[derive(Clone, Copy, Debug)]
pub struct Effect {
    id: NodeId,
    marker: PhantomData<*const ()>,
}

/// 创建副作用，等同于 [`Effect::new`]
pub fn effect(f: impl FnMut() + 'static) -> Effect {
    Effect::new(f)
}

struct EffectCompute<F>(RefCell<F>);

impl<F: FnMut()> Compute for EffectCompute<F> {
    fn compute(&self, _rt: &Runtime, _id: NodeId) -> bool {
        (self.0.borrow_mut())();
        true
    }
}

impl Effect {
    /// 创建副作用并立即执行一次；归当前作用域所有，作用域释放时一起停止
    pub fn new(f: impl FnMut() + 'static) -> Self {
        let compute: Rc<dyn Compute> = Rc::new(EffectCompute(RefCell::new(f)));
        let id = with_runtime(|rt| {
            let id = rt.create(Kind::Effect, None, Some(compute));
            rt.update_if_necessary(id);
            id
        });
        Self { id, marker: PhantomData }
    }

    /// 停止并释放，连同它拥有的节点
    pub fn dispose(self) {
        with_runtime(|rt| rt.dispose(self.id));
    }
}
