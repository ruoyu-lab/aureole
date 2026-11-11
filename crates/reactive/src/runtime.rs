//! 响应式运行时：节点存储、依赖收集、变更传播。
//!
//! 每个线程一个运行时。节点分五种：信号（存值）、派生值（存值 + 计算）、
//! 副作用（只有计算）、作用域（只拥有其他节点）、清理函数（随拥有者释放时执行）。
//!
//! 传播用三色标记：信号写入时，直接观察者标 Dirty，更下游标 Check，
//! 沿途遇到的副作用排队。之后逐个"按需更新"副作用：Check 的节点先检查
//! 上游派生值是否真的变了，只有确实变了才重新计算。这样同一次写入里
//! 每个节点最多计算一次，也不会读到一半新一半旧的值。
//!
//! 出错时恢复状态：依赖收集栈、当前拥有者、批量更新的层数、"正在执行副作用"的标记
//! 都由守卫（`Defer`、`Walk`）在离开时复位，正常返回和 panic 展开走同一条路，panic 照样往外传。
//! 计算被 panic 打断的节点标成 [`State::Interrupted`]，下次有用时重算。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
#[cfg(debug_assertions)]
use std::panic::Location;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;

use slotmap::{SlotMap, new_key_type};

new_key_type! {
    pub(crate) struct NodeId;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// 值是最新的
    Clean,
    /// 某个更上游的节点变了，用之前要先检查直接依赖的派生值
    Check,
    /// 直接依赖变了，必须重新计算
    Dirty,
    /// 上一次计算没有正常结束（计算 panic 了，或者 `flush` 到了轮数上限，排着的更新被丢下）：值不可信。
    /// 通知到它时像干净的节点一样往下传、副作用重新排队；要用它时一定重新计算
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Signal,
    Memo,
    Effect,
    Scope,
    /// [`crate::on_cleanup`] 登记的清理函数：值是 [`Cleanup`]
    Cleanup,
}

/// 派生值或副作用的计算体
pub(crate) trait Compute {
    /// 重新执行；返回值是否变化（副作用总是返回 true）。派生值把新值交给 [`Runtime::store`]
    fn compute(&self, rt: &Runtime, id: NodeId) -> bool;
}

/// 节点的值。读的时候复制一份 `Rc` 再调用户闭包，不拿着任何借用：闭包里可以写同一个信号
/// （那时换一份新的值，读的一方手里的旧值读完才释放）
pub(crate) type Value = Rc<dyn Any>;

/// 句柄的类型标记：`fn() -> T` 让句柄不受 T 的 Send/Sync 影响，
/// `*const ()` 让句柄本身不能跨线程（运行时是线程局部的）
pub(crate) type HandleMarker<T> = PhantomData<(fn() -> T, *const ())>;

const MISMATCH: &str = "响应式节点的值类型不匹配";

/// 句柄是在哪创建的（只有调试构建记）：读不出值时报出来，好找到是哪一个信号
#[derive(Clone, Copy)]
pub(crate) struct DefinedAt {
    #[cfg(debug_assertions)]
    location: &'static Location<'static>,
}

impl DefinedAt {
    #[track_caller]
    pub(crate) fn here() -> Self {
        Self {
            #[cfg(debug_assertions)]
            location: Location::caller(),
        }
    }
}

impl fmt::Display for DefinedAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(debug_assertions)]
        return write!(f, "（在 {} 创建）", self.location);
        #[cfg(not(debug_assertions))]
        {
            let _ = f;
            Ok(())
        }
    }
}

/// 节点读不出值的原因
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unreadable {
    /// 节点释放了
    Disposed,
    /// 在信号自己的 `update` 闭包里又读它
    InOwnUpdate,
    /// 派生值还没算出值（在它自己的计算里读了它）
    NotComputed,
    NoValue,
}

/// 把节点的值交给 `f`；读不出来时 panic，位置是读的那一行，说明原因和句柄是在哪创建的
#[track_caller]
pub(crate) fn read_value<T: 'static, R>(value: Result<Value, Unreadable>, defined: DefinedAt, f: impl FnOnce(&T) -> R) -> R {
    match value {
        Ok(value) => f(value.downcast_ref::<T>().expect(MISMATCH)),
        Err(reason) => unreadable(reason, defined),
    }
}

/// 读得出值时交给 `f`，节点释放了是 `None`；别的原因照样 panic（那是用错了，不是作用域的先后）
#[track_caller]
pub(crate) fn try_read_value<T: 'static, R>(value: Result<Value, Unreadable>, defined: DefinedAt, f: impl FnOnce(&T) -> R) -> Option<R> {
    match value {
        Err(Unreadable::Disposed) => None,
        value => Some(read_value(value, defined, f)),
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn unreadable(reason: Unreadable, defined: DefinedAt) -> ! {
    match reason {
        Unreadable::Disposed => panic!(
            "读了已释放的信号或派生值{defined}。它归创建时的作用域，作用域释放了它也就没了：常见于在浮层、对话框、列表项、\
             child_when 的内容里创建，却在外面用。要活得久的状态在外面创建；确实可能已经释放时用 try_get、try_with"
        ),
        Unreadable::InOwnUpdate => panic!("在信号自己的 update 闭包里又读写它{defined}：闭包的参数就是它现在的值"),
        Unreadable::NotComputed => panic!("派生值还没有算出值{defined}（在它自己的计算里读了它？）"),
        Unreadable::NoValue => panic!("这个响应式节点没有值{defined}"),
    }
}

/// `flush` 一次最多跑几轮。一轮是"上一轮的副作用写信号、排进来的副作用"，正常的连锁几轮就停；
/// 两个副作用互相写对方读的信号时会一直转下去，到这里停下、报一次错误
pub(crate) const MAX_FLUSH_ROUNDS: u32 = 100;

struct Node {
    kind: Kind,
    state: State,
    /// 信号、派生值的值；派生值还没算过时是 `None`，信号在它自己的 `update` 闭包里时被暂时拿走
    value: Option<Value>,
    compute: Option<Rc<dyn Compute>>,
    /// 本节点上次计算时读过的节点，无重复
    sources: Vec<NodeId>,
    /// 读过本节点的计算；其中已释放的节点惰性清理
    observers: Vec<NodeId>,
    /// 本节点拥有的节点：本节点释放或重新计算时一起释放
    children: Vec<NodeId>,
    owner: Option<NodeId>,
}

/// 依赖列表超过这个长度后改用哈希集合去重
const SMALL: usize = 32;

/// 一次计算执行期间的依赖收集
struct Tracker {
    node: NodeId,
    /// 上一次的依赖
    old: Vec<NodeId>,
    /// 与上一次依赖顺序完全一致的前缀长度。依赖不变时（最常见的情况）
    /// 每次读取只是前缀加一，整个计算不改动任何链接。
    matched: usize,
    /// 偏离上一次顺序之后读到的依赖
    new: Vec<NodeId>,
    /// 依赖很多时用于去重
    seen: Option<HashSet<NodeId>>,
}

impl Tracker {
    fn record(&mut self, source: NodeId) {
        if self.new.is_empty() && self.old.get(self.matched) == Some(&source) {
            self.matched += 1;
            return;
        }
        if let Some(seen) = &mut self.seen {
            if seen.insert(source) {
                self.new.push(source);
            }
            return;
        }
        if self.old[..self.matched].contains(&source) || self.new.contains(&source) {
            return;
        }
        self.new.push(source);
        if self.matched + self.new.len() > SMALL {
            self.seen = Some(self.old[..self.matched].iter().chain(&self.new).copied().collect());
        }
    }
}

/// 离开作用域时执行 `f`：正常返回和 panic 展开都会执行。用来复位运行时的状态，不接住 panic
struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Defer<F> {
    fn new(f: F) -> Self {
        Self(Some(f))
    }
}

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// [`crate::on_cleanup`] 的清理函数。释放时（[`Runtime::dispose_all`] 的第二步）取出来执行，那时同一作用域的节点都还在。
/// 没经过释放就析构的（线程结束时运行时自己析构）不执行，闭包直接丢掉：运行时已经在析构，闭包一读信号就 panic，
/// 线程局部量的析构函数里 panic 会让整个进程退出
pub(crate) struct Cleanup(RefCell<Option<Box<dyn FnOnce()>>>);

/// 一轮里接住的 panic：其余的副作用、清理函数照常执行，这一轮做完以后把第一个原样往外传
#[derive(Default)]
struct Panics {
    first: Option<Box<dyn Any + Send>>,
    more: usize,
}

impl Panics {
    fn catch(&mut self, f: impl FnOnce()) {
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(f)) {
            if self.first.is_none() {
                self.first = Some(payload);
            } else {
                self.more += 1;
            }
        }
    }

    /// 有接住的就接着展开第一个（panic 的消息在发生时已经打印过）
    fn resume(self) {
        if let Some(payload) = self.first {
            if self.more > 0 {
                tracing::error!("同一轮更新里另外还有 {} 个副作用或清理回调 panic，只往外传第一个", self.more);
            }
            panic::resume_unwind(payload);
        }
    }
}

pub(crate) struct Runtime {
    nodes: RefCell<SlotMap<NodeId, Node>>,
    /// 正在执行的计算的依赖收集栈；栈顶为 `None` 表示处在 untracked 里
    trackers: RefCell<Vec<Option<Tracker>>>,
    /// 新建节点的拥有者
    owner: Cell<Option<NodeId>>,
    batch_depth: Cell<u32>,
    /// 待执行的副作用
    pending: RefCell<Vec<NodeId>>,
    /// 复用的缓冲，避免每次写入都分配
    spare: RefCell<Vec<NodeId>>,
    /// 批量更新或副作用执行期间请求释放的节点，等这一轮跑完再释放
    pending_dispose: RefCell<Vec<NodeId>>,
    mark_stack: RefCell<Vec<(NodeId, State, NodeId)>>,
    running_effects: Cell<bool>,
    /// 作用域上提供的值（[`crate::provide_context`]）：(作用域, 值)。很少（每个窗口一两个），线性查找
    contexts: RefCell<Vec<(NodeId, Rc<dyn Any>)>>,
}

thread_local! {
    static RUNTIME: Runtime = Runtime::new();
}

pub(crate) fn with_runtime<R>(f: impl FnOnce(&Runtime) -> R) -> R {
    RUNTIME.with(f)
}

impl Runtime {
    fn new() -> Self {
        Self {
            nodes: RefCell::new(SlotMap::with_key()),
            trackers: RefCell::new(Vec::new()),
            owner: Cell::new(None),
            batch_depth: Cell::new(0),
            pending: RefCell::new(Vec::new()),
            spare: RefCell::new(Vec::new()),
            pending_dispose: RefCell::new(Vec::new()),
            mark_stack: RefCell::new(Vec::new()),
            running_effects: Cell::new(false),
            contexts: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn create(&self, kind: Kind, value: Option<Value>, compute: Option<Rc<dyn Compute>>) -> NodeId {
        let owner = self.owner.get();
        let state = if compute.is_some() { State::Dirty } else { State::Clean };
        let mut nodes = self.nodes.borrow_mut();
        let id = nodes.insert(Node {
            kind,
            state,
            value,
            compute,
            sources: Vec::new(),
            observers: Vec::new(),
            children: Vec::new(),
            owner,
        });
        if let Some(owner) = owner.and_then(|owner| nodes.get_mut(owner)) {
            owner.children.push(id);
        }
        id
    }

    /// 在当前作用域上登记清理函数
    pub(crate) fn on_cleanup(&self, f: Box<dyn FnOnce()>) {
        let value: Value = Rc::new(Cleanup(RefCell::new(Some(f))));
        self.create(Kind::Cleanup, Some(value), None);
    }

    pub(crate) fn node_count(&self) -> usize {
        self.nodes.borrow().len()
    }

    /// 节点现在的值（一份 `Rc`）。句柄拿着它在运行时的借用之外调用户的闭包：闭包里写同一个信号时信号换一份新的值，
    /// 这次读看到的还是读开始时的值
    pub(crate) fn snapshot(&self, id: NodeId) -> Result<Value, Unreadable> {
        match self.nodes.borrow().get(id) {
            Some(Node { value: Some(value), .. }) => Ok(value.clone()),
            Some(Node { kind: Kind::Signal, .. }) => Err(Unreadable::InOwnUpdate),
            Some(Node { kind: Kind::Memo, .. }) => Err(Unreadable::NotComputed),
            Some(_) => Err(Unreadable::NoValue),
            None => Err(Unreadable::Disposed),
        }
    }

    /// 信号写入新值，返回是否写了（信号已释放时什么都不做）。
    /// 信号正在被读（在它自己的 `with` 闭包里）时换一份新的值，读的一方手里的旧值读完才释放
    pub(crate) fn set<T: 'static>(&self, id: NodeId, value: T) -> bool {
        let mut old_value = None;
        let mut old_snapshot = None;
        {
            let mut nodes = self.nodes.borrow_mut();
            let Some(node) = nodes.get_mut(id) else { return false };
            let Some(slot) = node.value.as_mut() else {
                drop(nodes);
                panic!("在信号自己的 update 闭包里又读写它：闭包的参数就是它现在的值");
            };
            match Rc::get_mut(slot) {
                Some(current) => old_value = Some(mem::replace(current.downcast_mut::<T>().expect(MISMATCH), value)),
                None => old_snapshot = Some(mem::replace(slot, Rc::new(value) as Value)),
            }
        }
        // 旧值在放开借用之后才析构，析构代码可以使用运行时
        drop((old_value, old_snapshot));
        true
    }

    /// 和现在的值不同时才写入，返回是否写了。比较时不拿着运行时的借用（`PartialEq` 是用户代码）
    pub(crate) fn set_if_changed<T: PartialEq + 'static>(&self, id: NodeId, value: T) -> bool {
        let current = match self.nodes.borrow().get(id) {
            Some(node) => node.value.clone(),
            None => return false,
        };
        let Some(current) = current else { panic!("在信号自己的 update 闭包里又读写它：闭包的参数就是它现在的值") };
        let same = *current.downcast_ref::<T>().expect(MISMATCH) == value;
        drop(current);
        !same && self.set(id, value)
    }

    /// 原地修改信号的值，返回是否改了（信号已释放时什么都不做）。
    ///
    /// 修改期间值从节点里拿出来，放开借用再调 `f`：`f` 里可以读写别的信号，不能读写这一个。
    /// 信号正在被读（在它自己的 `with` 闭包里）时没法原地修改，panic；那时用 [`Runtime::set`]
    pub(crate) fn update<T: 'static>(&self, id: NodeId, f: impl FnOnce(&mut T)) -> bool {
        let taken = match self.nodes.borrow_mut().get_mut(id) {
            Some(node) => node.value.take(),
            None => return false,
        };
        let Some(value) = taken else { panic!("在信号自己的 update 闭包里又读写它：闭包的参数就是它现在的值") };
        // 正常返回和 panic 展开都把值放回去
        let mut slot = Taken { rt: self, id, value: Some(value) };
        let Some(current) = slot.value.as_mut().and_then(Rc::get_mut) else {
            drop(slot);
            panic!("这个信号正在被读（在它自己的 with、get 闭包里），没法原地修改：改用 set，或者读完再改");
        };
        f(current.downcast_mut::<T>().expect(MISMATCH));
        true
    }

    /// 派生值算出了新值：和旧值不同时存下，返回是否变了。比较时不拿着运行时的借用
    pub(crate) fn store<T: PartialEq + 'static>(&self, id: NodeId, new: T) -> bool {
        let old = self.nodes.borrow().get(id).and_then(|node| node.value.clone());
        let changed = old.as_ref().and_then(|old| old.downcast_ref::<T>()).is_none_or(|old| *old != new);
        drop(old);
        if !changed {
            return false;
        }
        let mut old_value = None;
        let mut old_snapshot = None;
        {
            let mut nodes = self.nodes.borrow_mut();
            // 计算期间被释放了：新值直接丢掉
            if let Some(node) = nodes.get_mut(id) {
                match node.value.as_mut().and_then(Rc::get_mut).and_then(|v| v.downcast_mut::<T>()) {
                    Some(current) => old_value = Some(mem::replace(current, new)),
                    // 还没算过，或者正在被读（在它自己的 with 闭包里触发了重算）：换一份新的
                    None => old_snapshot = node.value.replace(Rc::new(new)),
                }
            }
        }
        drop((old_value, old_snapshot));
        true
    }

    /// 当前计算读取了 `source`，记录为依赖
    pub(crate) fn track(&self, source: NodeId) {
        if let Some(Some(tracker)) = self.trackers.borrow_mut().last_mut() {
            tracker.record(source);
        }
    }

    pub(crate) fn untracked<R>(&self, f: impl FnOnce() -> R) -> R {
        self.trackers.borrow_mut().push(None);
        let _pop = Defer::new(|| {
            self.trackers.borrow_mut().pop();
        });
        f()
    }

    pub(crate) fn owner(&self) -> Option<NodeId> {
        self.owner.get()
    }

    pub(crate) fn with_owner<R>(&self, owner: NodeId, f: impl FnOnce() -> R) -> R {
        let previous = self.owner.replace(Some(owner));
        let _restore = Defer::new(|| self.owner.set(previous));
        f()
    }

    /// 在当前作用域上放一个值；同一个作用域上同一种类型的值以最后一次为准
    pub(crate) fn provide(&self, value: Rc<dyn Any>) {
        // 放在最近的作用域上（在副作用、派生值里调用时是它所在的作用域）：释放节点时只有作用域要查这张表
        let owner = {
            let nodes = self.nodes.borrow();
            let mut current = self.owner.get();
            while let Some(id) = current.filter(|&id| nodes.get(id).is_some_and(|n| n.kind != Kind::Scope)) {
                current = nodes.get(id).and_then(|n| n.owner);
            }
            current
        };
        let Some(owner) = owner else { return };
        let mut contexts = self.contexts.borrow_mut();
        let id = (*value).type_id();
        contexts.retain(|(o, v)| *o != owner || (**v).type_id() != id);
        contexts.push((owner, value));
    }

    /// 从当前作用域往外（沿拥有者）找最近提供的 `T`
    pub(crate) fn context<T: Clone + 'static>(&self) -> Option<T> {
        let contexts = self.contexts.borrow();
        if contexts.is_empty() {
            return None;
        }
        let nodes = self.nodes.borrow();
        let mut current = self.owner.get();
        while let Some(id) = current {
            if let Some(value) = contexts.iter().rev().filter(|(o, _)| *o == id).find_map(|(_, v)| v.downcast_ref::<T>()) {
                return Some(value.clone());
            }
            current = nodes.get(id).and_then(|node| node.owner);
        }
        None
    }

    /// 在所有作用域之外执行：里面创建的节点不归任何作用域所有，也不订阅
    pub(crate) fn detached<R>(&self, f: impl FnOnce() -> R) -> R {
        let previous = self.owner.replace(None);
        let _restore = Defer::new(|| self.owner.set(previous));
        self.untracked(f)
    }

    pub(crate) fn batch<R>(&self, f: impl FnOnce() -> R) -> R {
        let result = {
            self.batch_depth.set(self.batch_depth.get() + 1);
            // panic 时也减回去；排着的副作用留到下一次 flush（展开途中不执行用户代码）
            let _depth = Defer::new(|| self.batch_depth.set(self.batch_depth.get() - 1));
            f()
        };
        self.flush();
        result
    }

    /// 信号 `source` 被写入：标记下游并执行受影响的副作用
    pub(crate) fn notify(&self, source: NodeId) {
        {
            let mut nodes = self.nodes.borrow_mut();
            let mut pending = self.pending.borrow_mut();
            let mut stack = self.mark_stack.borrow_mut();
            let Some(node) = nodes.get(source) else { return };
            stack.extend(node.observers.iter().map(|&o| (o, State::Dirty, source)));

            let mut has_dead = Vec::new();
            while let Some((id, state, parent)) = stack.pop() {
                let Some(node) = nodes.get_mut(id) else {
                    if !has_dead.contains(&parent) {
                        has_dead.push(parent);
                    }
                    continue;
                };
                let next = match (node.state, state) {
                    (State::Clean, _) => state,
                    // 上次没算完：一定重算，并且像干净的节点一样往下传
                    (State::Interrupted, _) => State::Dirty,
                    // 已经标记过的节点，其下游也已标记过
                    (State::Check, State::Dirty) => {
                        node.state = State::Dirty;
                        continue;
                    }
                    _ => continue,
                };
                node.state = next;
                if node.kind == Kind::Effect {
                    pending.push(id);
                }
                stack.extend(node.observers.iter().map(|&o| (o, State::Check, id)));
            }
            for id in has_dead {
                let alive: Vec<NodeId> = nodes[id].observers.iter().copied().filter(|&o| nodes.contains_key(o)).collect();
                nodes[id].observers = alive;
            }
        }
        self.flush();
    }

    /// 现在就执行待执行的副作用，即使在批量更新中间（正在执行副作用时除外）
    pub(crate) fn flush_now(&self) {
        let depth = self.batch_depth.replace(0);
        let _restore = Defer::new(|| self.batch_depth.set(depth));
        self.flush();
    }

    /// 不在批量更新里、也没有正在执行副作用时，执行所有待执行的副作用。
    ///
    /// 一个副作用 panic 不影响别的：这一轮照样跑完、推迟的释放照样做完，然后把第一个 panic 原样往外传。
    /// 最多跑 [`MAX_FLUSH_ROUNDS`] 轮
    fn flush(&self) {
        if self.batch_depth.get() > 0 || self.running_effects.get() {
            return;
        }
        let mut panics = Panics::default();
        {
            self.running_effects.set(true);
            let _running = Defer::new(|| self.running_effects.set(false));
            let mut rounds = 0;
            loop {
                let mut batch = mem::take(&mut *self.spare.borrow_mut());
                mem::swap(&mut batch, &mut *self.pending.borrow_mut());
                if batch.is_empty() {
                    *self.spare.borrow_mut() = batch;
                    break;
                }
                rounds += 1;
                if rounds > MAX_FLUSH_ROUNDS {
                    tracing::error!(
                        "副作用循环：副作用写信号又触发副作用，连着 {MAX_FLUSH_ROUNDS} 轮还没停（多半是两个或更多副作用互相写对方读的信号）。\
                         这次更新停在这里，还排着的 {} 个副作用等它们读的值再变时才执行",
                        batch.len()
                    );
                    self.interrupt(batch.drain(..));
                    *self.spare.borrow_mut() = batch;
                    break;
                }
                // 副作用执行时可能写入信号、再排入新的副作用，留到下一轮
                for &id in &batch {
                    panics.catch(|| self.update_if_necessary(id));
                }
                batch.clear();
                *self.spare.borrow_mut() = batch;
            }
        }
        // 这一轮更新里请求释放的节点：依赖它们的界面已经更新（卸载）完了
        let disposals = mem::take(&mut *self.pending_dispose.borrow_mut());
        for id in disposals {
            panics.catch(|| self.dispose(id));
        }
        panics.resume();
    }

    /// 让计算节点变成最新：Check 时先把上游过期的派生值更新好，Dirty 时重新计算。
    ///
    /// 用显式栈向上游走，很深的派生链也不会栈溢出。上游先于下游更新，
    /// 所以重新计算时读到的派生值都已是最新，不会再递归。
    /// 中途 panic 时，栈里还没更新完的节点标成 [`State::Interrupted`]
    pub(crate) fn update_if_necessary(&self, root: NodeId) {
        if self.state(root) == State::Clean {
            return;
        }
        // (节点, 下一个要检查的依赖下标)
        let mut walk = Walk { rt: self, stack: vec![(root, 0)] };
        'walk: while let Some(&(id, start)) = walk.stack.last() {
            // 上游派生值变了会把本节点标成 Dirty，这时不用再检查剩下的依赖
            if self.state(id) == State::Check {
                let mut i = start;
                loop {
                    let next = {
                        let nodes = self.nodes.borrow();
                        nodes.get(id).and_then(|node| node.sources.get(i)).map(|&source| {
                            let stale = nodes.get(source).is_some_and(|s| s.kind == Kind::Memo && s.state != State::Clean);
                            (source, stale)
                        })
                    };
                    match next {
                        None => break,
                        Some((source, true)) => {
                            if let Some(top) = walk.stack.last_mut() {
                                top.1 = i + 1;
                            }
                            walk.stack.push((source, 0));
                            continue 'walk;
                        }
                        Some((_, false)) => i += 1,
                    }
                }
            }
            if matches!(self.state(id), State::Dirty | State::Interrupted) && self.run(id) {
                self.mark_observers_dirty(id);
            }
            if let Some(node) = self.nodes.borrow_mut().get_mut(id) {
                node.state = State::Clean;
            }
            walk.stack.pop();
        }
    }

    fn state(&self, id: NodeId) -> State {
        self.nodes.borrow().get(id).map_or(State::Clean, |node| node.state)
    }

    /// 派生值重新计算后变了：直接观察者必须重新计算
    fn mark_observers_dirty(&self, id: NodeId) {
        let mut nodes = self.nodes.borrow_mut();
        let mut pending = self.pending.borrow_mut();
        let Some(node) = nodes.get(id) else { return };
        let observers = node.observers.clone();
        for o in observers {
            let Some(observer) = nodes.get_mut(o) else { continue };
            // 通常它已经是 Check 并且排过队；从 Clean（或上次没算完）直接变 Dirty 的副作用要补排
            if matches!(observer.state, State::Clean | State::Interrupted) && observer.kind == Kind::Effect {
                pending.push(o);
            }
            observer.state = State::Dirty;
        }
    }

    /// 这些节点的计算被打断了：连同它们上游还没更新的派生值标成 [`State::Interrupted`]。
    ///
    /// 上游也要标：停在 Check、Dirty 的派生值挡住通知（"已经标过的节点不再往下走"），
    /// 不改的话它下游这些节点以后再也收不到通知
    fn interrupt(&self, ids: impl IntoIterator<Item = NodeId>) {
        // 只在 panic 展开或出错的路径上调用：借用不到就算了，不能在展开途中再 panic
        let Ok(mut nodes) = self.nodes.try_borrow_mut() else { return };
        let mut stack: Vec<NodeId> = ids.into_iter().collect();
        while let Some(id) = stack.pop() {
            let Some(node) = nodes.get_mut(id) else { continue };
            if matches!(node.state, State::Clean | State::Interrupted) {
                continue;
            }
            node.state = State::Interrupted;
            // 信号总是 Clean，副作用不会是别人的源：往上走到的只有派生值
            stack.extend_from_slice(&node.sources);
        }
    }

    /// 重新执行计算并收集依赖，返回值是否变化
    fn run(&self, id: NodeId) -> bool {
        // 上一次执行里创建的节点随之释放
        self.dispose_children(id);
        let (compute, old) = {
            let mut nodes = self.nodes.borrow_mut();
            let Some(node) = nodes.get_mut(id) else { return false };
            let compute = node.compute.clone().expect("只有派生值和副作用可以重新计算");
            (compute, mem::take(&mut node.sources))
        };
        self.trackers.borrow_mut().push(Some(Tracker { node: id, old, matched: 0, new: Vec::new(), seen: None }));
        let previous_owner = self.owner.replace(Some(id));
        // 正常结束和 panic 展开都要：恢复拥有者、弹出依赖收集、按这次读到的依赖更新链接
        // （panic 时是 panic 之前读到的那些，以后它们变了这个节点会再算一次）
        let _finish = Defer::new(|| {
            self.owner.set(previous_owner);
            let tracker = self.trackers.try_borrow_mut().ok().and_then(|mut trackers| trackers.pop()).flatten();
            if let Some(tracker) = tracker.filter(|tracker| tracker.node == id) {
                self.finish_tracking(tracker);
            }
        });
        compute.compute(self, id)
    }

    /// 根据本次读到的依赖更新链接：只改动和上一次不同的部分
    fn finish_tracking(&self, tracker: Tracker) {
        let Tracker { node: id, mut old, matched, new, .. } = tracker;
        // 也在 panic 展开时调用：借用不到就不改链接，不能在展开途中再 panic
        let Ok(mut nodes) = self.nodes.try_borrow_mut() else { return };
        if new.is_empty() && matched == old.len() {
            if let Some(node) = nodes.get_mut(id) {
                node.sources = old;
            }
            return;
        }

        let stale = &old[matched..];
        let stale_set: Option<HashSet<NodeId>> = (stale.len() > SMALL).then(|| stale.iter().copied().collect());
        let new_set: Option<HashSet<NodeId>> = (new.len() > SMALL).then(|| new.iter().copied().collect());
        let in_stale = |s: &NodeId| stale_set.as_ref().map_or_else(|| stale.contains(s), |set| set.contains(s));
        let in_new = |s: &NodeId| new_set.as_ref().map_or_else(|| new.contains(s), |set| set.contains(s));

        for source in stale.iter().filter(|s| !in_new(s)) {
            if let Some(source) = nodes.get_mut(*source)
                && let Some(i) = source.observers.iter().position(|&o| o == id)
            {
                source.observers.swap_remove(i);
            }
        }
        for source in new.iter().filter(|s| !in_stale(s)) {
            if let Some(source) = nodes.get_mut(*source) {
                source.observers.push(id);
            }
        }

        old.truncate(matched);
        old.extend(new);
        if let Some(node) = nodes.get_mut(id) {
            node.sources = old;
        }
    }

    /// 释放节点；批量更新或副作用执行期间调用时推迟到这一轮结束，
    /// 保证依赖它的副作用先跑完（例如列表先卸载对应的行）
    pub(crate) fn dispose_later(&self, id: NodeId) {
        if self.batch_depth.get() > 0 || self.running_effects.get() {
            self.pending_dispose.borrow_mut().push(id);
        } else {
            self.dispose(id);
        }
    }

    /// 释放节点及其拥有的所有节点
    pub(crate) fn dispose(&self, id: NodeId) {
        self.dispose_all(vec![id]);
    }

    fn dispose_children(&self, id: NodeId) {
        let children = match self.nodes.borrow_mut().get_mut(id) {
            Some(node) if !node.children.is_empty() => mem::take(&mut node.children),
            _ => return,
        };
        self.dispose_all(children);
    }

    /// 释放 `roots` 和它们拥有的全部节点。不递归，分三步：
    ///
    /// 1. 沿所有权树收集（先序：和登记的先后一致，子作用域、副作用拥有的在它们所在的位置）；
    /// 2. 按这个先后执行清理函数：这时节点都还在，清理函数里能读同一作用域的信号、找得到上下文；
    /// 3. 摘掉节点，最后析构值和闭包（放开借用之后，析构代码可以使用运行时）。
    ///
    /// 整个过程在批量更新里：清理函数里的写入等释放做完再执行副作用，已经摘掉的副作用不会再跑。
    /// 一个清理函数 panic 不影响别的，全部做完后把第一个往外传
    fn dispose_all(&self, mut roots: Vec<NodeId>) {
        let mut panics = Panics::default();
        let mut garbage = Vec::new();
        {
            self.batch_depth.set(self.batch_depth.get() + 1);
            let _depth = Defer::new(|| self.batch_depth.set(self.batch_depth.get() - 1));
            let (mut order, mut cleanups) = (Vec::new(), Vec::new());
            while !roots.is_empty() {
                order.clear();
                cleanups.clear();
                self.collect(&roots, &mut order, &mut cleanups);
                for &id in &cleanups {
                    if let Some(f) = self.take_cleanup(id) {
                        // 不订阅：清理函数里读的信号不算正在执行的计算的依赖
                        panics.catch(|| self.untracked(f));
                    }
                }
                // 清理函数里新建、归这些节点所有的节点（收集之后才出现的）：再来一轮
                roots = self.remove(&order, &mut garbage);
            }
            // 节点里的值和闭包在释放借用之后再析构，析构代码可以安全地使用运行时
            drop(garbage);
        }
        self.flush();
        panics.resume();
    }

    /// 先序收集 `roots` 拥有的节点（含自己）；清理函数另记一份
    fn collect(&self, roots: &[NodeId], order: &mut Vec<NodeId>, cleanups: &mut Vec<NodeId>) {
        let nodes = self.nodes.borrow();
        let mut stack: Vec<NodeId> = roots.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            let Some(node) = nodes.get(id) else { continue };
            order.push(id);
            if node.kind == Kind::Cleanup {
                cleanups.push(id);
            }
            stack.extend(node.children.iter().rev());
        }
    }

    /// 取出清理函数（只取一次）
    fn take_cleanup(&self, id: NodeId) -> Option<Box<dyn FnOnce()>> {
        let value = self.nodes.borrow().get(id)?.value.clone()?;
        let cleanup = value.downcast_ref::<Cleanup>()?;
        cleanup.0.borrow_mut().take()
    }

    /// 按先序摘掉节点（拥有者先于它拥有的节点，后者的拥有者已经不在，不用去它的子节点列表里找），
    /// 节点放进 `garbage`。返回收集之后才新建的、拥有者已被摘掉的节点
    fn remove(&self, order: &[NodeId], garbage: &mut Vec<Node>) -> Vec<NodeId> {
        let start = garbage.len();
        let mut nodes = self.nodes.borrow_mut();
        let mut contexts = self.contexts.borrow_mut();
        for &id in order {
            // 清理函数里已经释放了的
            let Some(node) = nodes.remove(id) else { continue };
            if node.kind == Kind::Scope && !contexts.is_empty() {
                contexts.retain(|(owner, _)| *owner != id);
            }
            // 读过本节点的计算：从它们的依赖里去掉。
            // 本节点读过的源：它们观察者列表里的记录在下次通知时惰性清理。
            for &observer in &node.observers {
                if let Some(observer) = nodes.get_mut(observer) {
                    observer.sources.retain(|&s| s != id);
                }
            }
            // 拥有者还在的（这次释放的根）：从它的子节点里去掉
            if let Some(owner) = node.owner.and_then(|owner| nodes.get_mut(owner))
                && let Some(i) = owner.children.iter().position(|&c| c == id)
            {
                owner.children.swap_remove(i);
            }
            garbage.push(node);
        }
        garbage[start..].iter().flat_map(|node| &node.children).copied().filter(|&c| nodes.contains_key(c)).collect()
    }
}

/// [`Runtime::update`] 拿出来的值：离开时放回节点（节点已释放时丢掉）
struct Taken<'a> {
    rt: &'a Runtime,
    id: NodeId,
    value: Option<Value>,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else { return };
        let leftover = match self.rt.nodes.try_borrow_mut() {
            Ok(mut nodes) => match nodes.get_mut(self.id) {
                Some(node) if node.value.is_none() => {
                    node.value = Some(value);
                    None
                }
                _ => Some(value),
            },
            Err(_) => Some(value),
        };
        // 放不回去的值在放开借用之后析构
        drop(leftover);
    }
}

/// [`Runtime::update_if_necessary`] 的显式栈：离开时栈里还有节点（中途 panic 了），把它们标成被打断
struct Walk<'a> {
    rt: &'a Runtime,
    stack: Vec<(NodeId, usize)>,
}

impl Drop for Walk<'_> {
    fn drop(&mut self) {
        if !self.stack.is_empty() {
            self.rt.interrupt(self.stack.iter().map(|&(id, _)| id));
        }
    }
}
