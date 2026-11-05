//! 传播：一个事件沿一条路径走三段——捕获（从外往里，到目标的父元素为止）、目标、冒泡（从目标的父元素往外）。
//!
//! - **路径**是分发之前拍下的快照（[`Tree::route_path`]），从目标往外沿逻辑父元素：浮层的宿主接到声明它的元素上，
//!   模态浮层是边界（[`Tree::logical_parent`]）。每一跳只读节点处理器表的位掩码，路径用 `SmallVec`，不分配。
//! - **处理器在释放树的借用之后调用**（约定见 [`super`]），一个元素的处理器放在一个 `batch` 里。
//!   处理器里改了树、卸载了节点：每一跳先检查节点还在（slotmap 的键带版本，卸载了的查不到）、没在退场、
//!   不在正在关的浮层里，不在的跳过。
//! - 事件对象上的格子记着处理器的要求（停止传播、取消内置行为、捕获指针），分发结束后由调用的地方执行没被取消的内置行为。
//! - **点击不出浮层**：点击类事件（左键、右键、中键、双击）的路径到浮层的宿主为止，浮层里的点击不传回声明它的元素
//!   和外面（菜单项点完不用再停止传播，打开菜单的按钮不会把菜单又打开）。按键、指针、滚轮、手势、改值汇总照常沿逻辑父元素走。

use reactive::batch;
use smallvec::SmallVec;

use super::super::{NodeId, Tree, TreeRef};
use crate::event::{ClickSource, ClickEvent, DragEvent, FieldChangeEvent, Flow, KeyEvent, PropagationPhase, PinchEvent, PointerEvent, RotateEvent, WheelEvent};
use crate::handlers::{EventType, Handler, On};

/// 传播的路径：从目标往外
pub(in crate::tree) type RoutePath = SmallVec<[NodeId; 16]>;

/// 会传播的事件：处理器表里的哪一类处理器，传到一个元素时怎么换算成它的坐标
pub(in crate::tree) trait Routed: Sized + 'static {
    fn flow(&self) -> &Flow;
    fn pick(handler: &Handler) -> Option<&On<Self>>;
    /// 传到 `id` 之前：位置换算成它自己的坐标
    fn localize(&mut self, _tree: &Tree, _id: NodeId) {}
}

impl Routed for PointerEvent {
    fn flow(&self) -> &Flow {
        PointerEvent::flow(self)
    }

    fn pick(handler: &Handler) -> Option<&On<Self>> {
        handler.pointer()
    }

    fn localize(&mut self, tree: &Tree, id: NodeId) {
        self.position = tree.local_point(id, self.window_position);
    }
}

impl Routed for ClickEvent {
    fn flow(&self) -> &Flow {
        ClickEvent::flow(self)
    }

    fn pick(handler: &Handler) -> Option<&On<Self>> {
        handler.click()
    }

    fn localize(&mut self, tree: &Tree, id: NodeId) {
        self.position = match self.source {
            ClickSource::Pointer => tree.local_point(id, self.window_position),
            _ => tree.nodes.get(id).map_or([0.0; 2], |n| [n.bounds.width * 0.5, n.bounds.height * 0.5]),
        };
    }
}

impl Routed for WheelEvent {
    fn flow(&self) -> &Flow {
        WheelEvent::flow(self)
    }

    fn pick(handler: &Handler) -> Option<&On<Self>> {
        handler.wheel()
    }

    fn localize(&mut self, tree: &Tree, id: NodeId) {
        self.position = tree.local_point(id, self.window_position);
    }
}

impl Routed for KeyEvent {
    fn flow(&self) -> &Flow {
        KeyEvent::flow(self)
    }

    fn pick(handler: &Handler) -> Option<&On<Self>> {
        handler.key()
    }
}

impl Routed for FieldChangeEvent {
    fn flow(&self) -> &Flow {
        FieldChangeEvent::flow(self)
    }

    fn pick(handler: &Handler) -> Option<&On<Self>> {
        handler.field()
    }
}

/// 手势事件：都按窗口坐标换算成元素自己的坐标
macro_rules! routed_gesture {
    ($($ty:ident => $pick:ident),*) => {
        $(
            impl Routed for $ty {
                fn flow(&self) -> &Flow {
                    $ty::flow(self)
                }

                fn pick(handler: &Handler) -> Option<&On<Self>> {
                    handler.$pick()
                }

                fn localize(&mut self, tree: &Tree, id: NodeId) {
                    self.position = tree.local_point(id, self.window_position);
                }
            }
        )*
    };
}

routed_gesture!(DragEvent => drag, PinchEvent => pinch, RotateEvent => rotate);

/// 一次分发的结果
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::tree) struct Outcome {
    /// 处理器调用了 `capture_pointer` 的元素（最先要求的那个）
    pub captured: Option<NodeId>,
    /// 冒泡阶段走到了路径里的第几个（目标是 1；被停止时停在那里，没被停止时是路径的长度）
    pub reached: usize,
}

/// 分发的选项
#[derive(Clone, Copy, Default)]
pub(in crate::tree) struct Options {
    /// 跳过禁用的元素（自己或祖先禁用）：指针、点击
    pub skip_disabled: bool,
    /// 捕获的处理器已经走过了（[`dispatch_capture`]）：只走目标上其余的处理器和冒泡
    pub captured: bool,
}

impl Tree {
    /// 逻辑上的父元素：节点树里的父元素；浮层的宿主接到声明它的元素上（模态浮层是边界，没有）
    pub(in crate::tree) fn logical_parent(&self, id: NodeId) -> Option<NodeId> {
        let node = self.nodes.get(id)?;
        match node.parent {
            Some(parent) => Some(parent),
            None => self.overlay_owner(id),
        }
    }

    /// 从 `target` 往外的传播路径（含 `target`）
    pub(in crate::tree) fn route_path(&self, target: NodeId) -> RoutePath {
        let mut path = RoutePath::new();
        let mut current = Some(target);
        while let Some(id) = current {
            path.push(id);
            current = self.logical_parent(id);
        }
        path
    }

    /// 事件还能交给这个元素：还在、没在退场、不在正在关的浮层里
    pub(in crate::tree) fn routable(&self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(|n| !n.exiting) && (self.overlays.is_empty() || self.focus_target_valid(id))
    }

    /// 点击类事件的路径到浮层为止：从目标往外数，第一个浮层宿主（含）之前的部分。不在浮层里时是整条路径
    pub(in crate::tree) fn within_overlay(&self, path: &[NodeId]) -> usize {
        if self.overlays.is_empty() {
            return path.len();
        }
        path.iter()
            .position(|&id| self.nodes.get(id).is_some_and(|n| n.parent.is_none()) && self.overlay_declared_by(id).is_some())
            .map_or(path.len(), |host| host + 1)
    }

    /// 路径上每个元素禁用了没有（自己或者逻辑上的祖先禁用）
    fn disabled_along(&self, path: &[NodeId]) -> SmallVec<[bool; 16]> {
        let mut flags: SmallVec<[bool; 16]> = SmallVec::from_elem(false, path.len());
        let mut blocked = false;
        for (i, &id) in path.iter().enumerate().rev() {
            blocked |= self.node_input(id).is_some_and(|input| input.disabled);
            flags[i] = blocked;
        }
        flags
    }
}

/// 沿 `path`（从目标往外）分发：捕获 → 目标 → 冒泡。`after` 在每个元素（没跳过的）的处理器之后调用，
/// 三段都调用，这个元素的处理器停止了传播也调用：用来执行这个元素自己的内置行为（浮层宿主在捕获阶段处理 Esc、
/// 输入框的编辑、`focus_arrows`、输入框不让点击往外传……），它也可以停止传播。要用到节点的先自己查
/// [`Tree::routable`]（处理器可能刚把它卸载了）
pub(in crate::tree) fn dispatch<E: Routed>(
    tree: &TreeRef,
    path: &[NodeId],
    kind: EventType,
    event: &mut E,
    options: Options,
    mut after: impl FnMut(&TreeRef, NodeId, PropagationPhase, &E),
) -> Outcome {
    let mut outcome = Outcome::default();
    // 点击不出浮层：浮层里的点击（左键、右键、中键、双击）只在浮层里传播，不传回声明它的元素
    let path = if kind.stays_in_overlay() { &path[..tree.borrow().within_overlay(path)] } else { path };
    if path.is_empty() {
        return outcome;
    }
    let disabled = if options.skip_disabled { tree.borrow().disabled_along(path) } else { SmallVec::new() };
    let skip = |i: usize| options.skip_disabled && disabled[i];
    let mut handlers: Vec<On<E>> = Vec::new();
    // 捕获：从外往里，到目标的父元素
    if !options.captured {
        for i in (1..path.len()).rev() {
            if event.flow().stopped() {
                return outcome;
            }
            if skip(i) {
                continue;
            }
            if visit(tree, path[i], kind, PropagationPhase::Capture, PropagationPhase::Capture, event, &mut handlers) {
                call(&handlers, event);
                note_capture(event, path[i], &mut outcome);
            }
            after(tree, path[i], PropagationPhase::Capture, event);
        }
    }
    // 目标，然后冒泡
    for (i, &id) in path.iter().enumerate() {
        if event.flow().stopped() {
            return outcome;
        }
        outcome.reached = i + 1;
        if skip(i) {
            continue;
        }
        let phase = if i == 0 { PropagationPhase::Target } else { PropagationPhase::Bubble };
        // 捕获的走过了时，目标上只剩冒泡的那些
        let stage = if options.captured { PropagationPhase::Bubble } else { phase };
        if visit(tree, id, kind, phase, stage, event, &mut handlers) {
            call(&handlers, event);
            note_capture(event, id, &mut outcome);
        }
        after(tree, id, phase, event);
    }
    outcome
}

/// 只走捕获的处理器：从外往里，目标自己的也算（事件的阶段是目标）。之后可以带上 [`Options::captured`] 接着 [`dispatch`]。
/// 内置行为要排在所有捕获的处理器之后、目标上其余的处理器之前时用（焦点在浮层外面时 Esc 关浮层）
pub(in crate::tree) fn dispatch_capture<E: Routed>(tree: &TreeRef, path: &[NodeId], kind: EventType, event: &mut E) {
    let mut handlers: Vec<On<E>> = Vec::new();
    for (i, &id) in path.iter().enumerate().rev() {
        if event.flow().stopped() {
            return;
        }
        let phase = if i == 0 { PropagationPhase::Target } else { PropagationPhase::Capture };
        if visit(tree, id, kind, phase, PropagationPhase::Capture, event, &mut handlers) {
            call(&handlers, event);
        }
    }
}

/// 准备交给 `id`：借着树取出 `stage` 那一段的处理器、换算坐标，事件的阶段记成 `phase`；没有处理器或者节点不在了返回 `false`
fn visit<E: Routed>(tree: &TreeRef, id: NodeId, kind: EventType, phase: PropagationPhase, stage: PropagationPhase, event: &mut E, out: &mut Vec<On<E>>) -> bool {
    out.clear();
    let t = tree.borrow();
    let Some(cold) = t.nodes.cold(id) else { return false };
    if !cold.handlers.listens_in(kind, stage) || !t.routable(id) {
        return false;
    }
    cold.handlers.in_phase(kind, stage, E::pick, out);
    event.localize(&t, id);
    event.flow().set_phase(phase);
    !out.is_empty()
}

/// 只交给 `id` 一个元素（不传播）：它在 `phase` 那一段的处理器。手势的 `Move`、`End` 交给拿到手势的元素（目标阶段）、
/// `Cancel` 交给收到过 `Start` 的元素（按当时的那一段）用它
pub(in crate::tree) fn deliver<E: Routed>(tree: &TreeRef, id: NodeId, kind: EventType, phase: PropagationPhase, event: &mut E) {
    let mut handlers = Vec::new();
    if visit(tree, id, kind, phase, phase, event, &mut handlers) {
        call(&handlers, event);
    }
}

fn call<E>(handlers: &[On<E>], event: &E) {
    batch(|| handlers.iter().for_each(|h| h(event)));
}

fn note_capture<E: Routed>(event: &E, id: NodeId, outcome: &mut Outcome) {
    if event.flow().take_capture() && outcome.captured.is_none() {
        outcome.captured = Some(id);
    }
}
