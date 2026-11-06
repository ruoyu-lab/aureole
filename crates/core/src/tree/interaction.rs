//! 元素树里输入的树上那一半：节点上的输入设置（叠放次序、指针形状、禁用、手势和按键的处理器，挂载时交给节点）、
//! 键盘焦点（Tab 的顺序、焦点环、焦点句柄、激活）、指针形状、拖动和手势的记录、按着的键。
//! 窗口交来的事件怎么分发在 `tree/route/`（先求值、再借用树），指针和键盘的状态在 `tree/router.rs`。
//! 对外的类型和链式方法在 [`crate::input`]，浮层在 `overlays.rs`。

use std::rc::{Rc, Weak};

use reactive::{Effect, untracked};
use scene::{DisplayList, Quad};

use super::{FocusCallbacks, Leave, NodeId, Tree, TreeRef, run_focus_callbacks};
use crate::property::PropId;
use crate::handlers::{EventType, Handler, HandlerList};
use crate::geometry::Affine;
use crate::input::{Key, KeyInput, MouseButton, PhysicalKey};
use crate::event::{ClickSource, ClickEvent, DragEvent, KeyEvent, PointerEvent, PointerEventKind};
use crate::input::GesturePhase;
use crate::interaction::{Cursor, FocusHandle, FocusRing, Interaction};
use crate::overlay::OverlaySpec;
use crate::geometry::Rect;
use crate::layout::Direction;

/// 按下后移动超过这么远才算拖动，逻辑像素
pub(super) const DRAG_THRESHOLD: f32 = 4.0;
/// 拖动速度按最近这么久的移动估计，秒
pub(super) const VELOCITY_WINDOW: f64 = 0.1;

/// 节点上和输入有关的状态，用到时才分配
#[derive(Default)]
pub(crate) struct NodeInput {
    /// 兄弟之间的叠放次序
    pub z: i32,
    /// 有子元素的叠放次序不是 0 时，子元素按叠放次序排好的顺序（画和命中测试用；布局、Tab 仍按 `children`）
    pub order: Option<Box<[NodeId]>>,
    pub cursor: Option<Cursor>,
    pub disabled: bool,
    pub focusable: bool,
    pub arrows: Option<Direction>,
    pub ring: Option<FocusRing>,
    /// 声明在这个元素上、正开着的浮层
    pub overlays: Vec<NodeId>,
    /// 拖放：拖动源、放置目标、拖到应用外面
    pub drag_drop: Option<Rc<crate::drag_drop::DragDropSpec>>,
    /// 拖动时跟着指针走的样子（属性 `drag_preview`）
    pub drag_preview: crate::drag_drop::DragPreview,
    /// 跟着指针走的样子只沿这个方向走（属性 `drag_preview_axis`），`None` 是两个方向都走
    pub drag_preview_axis: Option<Direction>,
    /// 跟着指针走的样子不出父元素（属性 `drag_preview_contained`）
    pub drag_preview_contained: bool,
    /// 设了属性 `window_drag_area`：是不是窗口的拖动区域（见 `route/window_drag.rs`）
    pub window_drag_area: Option<bool>,
}

impl NodeInput {
    /// 拖放设置
    pub fn drag_drop(&self) -> Option<&Rc<crate::drag_drop::DragDropSpec>> {
        self.drag_drop.as_ref()
    }
}

/// 透明度（在动时按目标算：正在淡入的对话框里的元素算看得见）
fn tab_opacity(node: &super::Node) -> f32 {
    node.animations
        .iter()
        .find(|slot| slot.property == PropId::OPACITY)
        .and_then(|slot| slot.track.as_ref())
        .map_or(node.paint.opacity, |a| a.target()[0])
}

/// Tab 的候选：按 Tab 的顺序（树的顺序，声明在元素上的浮层跟在它的子孙后面）排好的、可能获得焦点的元素，
/// 和每个候选在里面的位置。树的结构版本变了才重建；会变的条件（禁用、透明度、大小、浮层开没开）按 Tab 时一个个看
#[derive(Default)]
pub(super) struct FocusCache {
    structure: Option<u64>,
    order: Vec<NodeId>,
    index: slotmap::SecondaryMap<NodeId, u32>,
}

impl FocusCache {
    /// 遍历整棵树（从根元素，经过所有声明的浮层）
    fn rebuild(&mut self, tree: &Tree) {
        self.structure = Some(tree.nodes.structure());
        self.order.clear();
        self.index.clear();
        let mut stack: Vec<NodeId> = tree.nodes.roots().iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            let Some(node) = tree.nodes.get(id) else { continue };
            if tree.focusable(id) || tree.text_block(id).is_some_and(|b| b.rich().is_some()) {
                self.index.insert(id, self.order.len() as u32);
                self.order.push(id);
            }
            if let Some(input) = node.input.as_deref() {
                stack.extend(input.overlays.iter().rev());
            }
            stack.extend(node.children.iter().rev());
        }
    }
}

/// 按着的一个键：松开时交给按下时的那条链
pub(super) struct Held {
    /// 物理键（窗口给的时候）；没有时按不算修饰键的按键认
    pub(super) physical: Option<PhysicalKey>,
    pub(super) key: Key,
    pub(super) base: Key,
    /// 按下时冒泡的起点：焦点（没有焦点时是模态浮层的内容或第一个根元素）
    pub(super) start: Option<NodeId>,
}

impl Held {
    pub(super) fn same(&self, physical: Option<&PhysicalKey>, base: &Key) -> bool {
        match (self.physical.as_ref(), physical) {
            (Some(a), Some(b)) => a == b,
            _ => match (&self.base, base) {
                (Key::Character(a), Key::Character(b)) => a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase)),
                (a, b) => a == b,
            },
        }
    }
}

/// 进行中的拖动（按下了还没越过阈值时 `started` 是 false）
pub(super) struct Drag {
    /// 拿到这个拖动的元素：越过阈值、仲裁之后才有（见 `route/gesture.rs`）
    pub(super) node: Option<NodeId>,
    /// 它在哪一段拿到的：之后的事件交给它那一段的处理器（在捕获阶段抢到的交给 `on_drag_capture`）
    pub(super) phase: crate::event::PropagationPhase,
    /// 按下处从目标往外的路径：越过阈值时 `Start` 沿它传播
    pub(super) path: super::route::RoutePath,
    pub(super) button: MouseButton,
    pub(super) start: [f32; 2],
    pub(super) last: [f32; 2],
    pub(super) started: bool,
    /// 最近的位置和时间，估计速度
    pub(super) samples: Vec<(f64, [f32; 2])>,
    /// 拿到拖动的元素能拖到放置目标上（`draggable`）、能拖到应用外面（`drag_out`）
    pub(super) drag_drop: bool,
    pub(super) out: bool,
}

impl Drag {
    /// 拿到了拖动、已经开始的拖动的元素
    pub(super) fn owner(&self) -> Option<NodeId> {
        self.node.filter(|_| self.started)
    }
}

/// 进行中的捏合、旋转手势：开始时拿到它的元素和它拿到时的那一段（没有元素拿到时是 `None`，等这次手势结束）
pub(super) struct Gesture {
    pub(super) node: Option<NodeId>,
    pub(super) phase: crate::event::PropagationPhase,
    pub(super) total: f32,
}

pub(super) fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

pub(super) fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

impl Tree {
    pub(super) fn node_input(&self, id: NodeId) -> Option<&NodeInput> {
        self.nodes.get(id)?.input.as_deref()
    }

    // ---- 处理器 ----

    /// 元素的处理器表
    pub(super) fn handlers(&self, id: NodeId) -> Option<&HandlerList> {
        self.nodes.cold(id).map(|cold| &cold.handlers)
    }

    /// 元素有没有这种事件的处理器
    #[inline]
    pub(super) fn listens(&self, id: NodeId, kind: EventType) -> bool {
        self.nodes.cold(id).is_some_and(|cold| cold.handlers.listens(kind))
    }

    /// 最后一次问拖来的内容时接受的操作
    pub(crate) fn external_drop_effect(&self) -> Option<crate::event::ExternalDropEffect> {
        self.router.external_drag().target.map(|(_, effect)| effect)
    }

    /// 最后一次问拖来的内容时，指针下有元素关心它
    pub(crate) fn is_external_drag_claimed(&self) -> bool {
        self.router.external_drag().claimed
    }

    /// 元素上这种事件的处理器（复制一份，释放树的借用之后调用）
    pub(super) fn handlers_of<T: ?Sized>(&self, id: NodeId, kind: EventType, pick: impl Fn(&Handler) -> Option<&Rc<T>>) -> Vec<Rc<T>> {
        self.handlers(id).map(|h| h.of(kind, pick)).unwrap_or_default()
    }

    /// 指针下（从内到外）第一个有这种事件的处理器、没被禁用的元素
    pub(super) fn innermost_listening(&self, path: &[NodeId], kind: EventType) -> Option<NodeId> {
        path.iter().rev().copied().find(|&id| self.listens(id, kind) && !self.disabled(id))
    }

    /// 改节点的输入状态（没有就分配）；节点已经卸载时什么都不做
    pub(super) fn with_input<R>(&mut self, id: NodeId, f: impl FnOnce(&mut NodeInput) -> R) -> Option<R> {
        let node = self.nodes.get_mut(id)?;
        Some(f(node.input.get_or_insert_with(Box::default)))
    }

    // ---- 叠放次序 ----

    /// 子元素变了或者某个子元素的叠放次序变了：按叠放次序稳定排序（都是 0 时不排，不分配）
    pub(super) fn sort_children(&mut self, id: NodeId) {
        let Some(node) = self.nodes.get(id) else { return };
        // 抬起的元素（拖放）画在兄弟上面
        let lifted = self.drag_drop.lifted();
        let z = |c: &NodeId| if lifted == Some(*c) { i32::MAX } else { self.node_input(*c).map_or(0, |i| i.z) };
        let order = node.children.iter().any(|c| z(c) != 0).then(|| {
            let mut order = node.children.clone();
            order.sort_by_key(z);
            order.into_boxed_slice()
        });
        if order.is_some() || node.input.as_ref().is_some_and(|i| i.order.is_some()) {
            if order.is_some() {
                self.nodes.set_sorted();
            }
            self.with_input(id, |i| i.order = order);
            self.paint.invalidate();
        }
    }

    pub(super) fn set_z(&mut self, id: NodeId, z: i32) {
        if self.with_input(id, |i| std::mem::replace(&mut i.z, z)) == Some(z) {
            return;
        }
        if let Some(parent) = self.nodes.get(id).and_then(|n| n.parent) {
            self.sort_children(parent);
        }
    }

    // ---- 状态查询 ----

    /// 自己或祖先被禁用
    pub(super) fn disabled(&self, mut id: NodeId) -> bool {
        loop {
            let Some(node) = self.nodes.get(id) else { return true };
            if node.input.as_ref().is_some_and(|i| i.disabled) {
                return true;
            }
            match node.parent {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }

    /// Tab 能走到：输入目标说按下和 Tab 都能获得焦点（输入框、有链接的富文本），或者元素设了 `focusable`
    pub(super) fn focusable(&self, id: NodeId) -> bool {
        self.target_focusable(id) == crate::kinds::Focusable::Keyboard || self.node_input(id).is_some_and(|i| i.focusable)
    }

    /// 从根到 `id` 的路径
    pub(super) fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut path = Vec::new();
        let mut current = Some(id);
        while let Some(id) = current.filter(|&id| self.nodes.contains_key(id)) {
            path.push(id);
            current = self.nodes[id].parent;
        }
        path.reverse();
        path
    }

    /// 窗口坐标换算到元素自己的坐标（去掉变换，左上角是原点）
    pub(super) fn local_point(&self, id: NodeId, window: [f32; 2]) -> [f32; 2] {
        match self.nodes.get(id) {
            Some(node) => node.world_invert(window).map_or(window, |p| [p[0] - node.bounds.x, p[1] - node.bounds.y]),
            None => window,
        }
    }

    /// 主指针的一个事件：修饰键、按着的键、设备、编号都带上
    pub(super) fn primary_event(&self, kind: PointerEventKind, window: [f32; 2], button: Option<MouseButton>, clicks: u32) -> PointerEvent {
        let mut event = PointerEvent::new(kind, window, button, self.router.modifiers(), clicks);
        event.buttons = self.router.buttons();
        event.device = self.router.device();
        event.pointer_id = self.router.pointer_id();
        event
    }

    pub(super) fn pointer_event(&self, id: NodeId, kind: PointerEventKind, window: [f32; 2], button: Option<MouseButton>, clicks: u32) -> PointerEvent {
        let mut event = self.primary_event(kind, window, button, clicks);
        event.position = self.local_point(id, window);
        event
    }

    /// 点击事件：指针来的在指针的位置，别的（键盘、辅助技术、标签）在元素的正中
    pub(super) fn click_event(&self, id: NodeId, source: ClickSource, button: MouseButton, clicks: u32) -> ClickEvent {
        let window = match (source, self.router.pointer()) {
            (ClickSource::Pointer, Some(point)) => point,
            _ => self.screen_rect(id).map_or([0.0; 2], |r| r.center()),
        };
        let mut event = ClickEvent::new(source, button, window, self.router.modifiers(), clicks);
        event.position = match source {
            ClickSource::Pointer => self.local_point(id, window),
            _ => self.nodes.get(id).map_or([0.0; 2], |n| [n.bounds.width * 0.5, n.bounds.height * 0.5]),
        };
        event
    }

    /// 此刻的指针形状：拖着滚动条时是箭头；拖动中由被拖的元素（和它的祖先）决定；
    /// 否则是指针下最内层设置了形状的元素，输入框默认是文字形状的指针
    pub(crate) fn cursor(&self) -> Cursor {
        if self.dragging_thumb() {
            return Cursor::Default;
        }
        let captured;
        let path: &[NodeId] = match self.router.drag().and_then(Drag::owner) {
            Some(owner) => {
                captured = self.ancestry(owner);
                &captured
            }
            None => self.router.hovered(),
        };
        for &id in path.iter().rev() {
            if let Some(cursor) = self.node_input(id).and_then(|i| i.cursor) {
                return cursor;
            }
            // 富文本里的链接是手指
            if self.over_link(id) {
                return Cursor::Pointer;
            }
            // 输入目标给的形状：输入框、可选中的文字、多行输入框的视口（内边距里、最后一行下面）默认是文字形状的指针
            if let Some(point) = self.router.pointer()
                && let Some(cursor) = self.target_cursor(id, point)
            {
                return cursor;
            }
        }
        Cursor::Default
    }

    /// 测试工具：有焦点的元素的名字（没起名字的是空字符串）
    pub(crate) fn focused_name(&self) -> Option<String> {
        let id = self.router.focused().filter(|&id| self.nodes.contains_key(id))?;
        Some(self.nodes.name(id).unwrap_or_default().to_owned())
    }

    /// 测试工具：焦点环此刻显示着（在元素上，或者在富文本的链接上）
    pub(crate) fn is_focus_ring_visible(&self) -> bool {
        let link = self.router.focus_visible() && self.focused_link().is_some();
        link || self.router.focused().is_some_and(|id| self.ring_on(id))
    }

    /// 测试工具：命名元素此刻在窗口里的外接矩形（算上变换、滚动、浮层的位置）
    pub(crate) fn window_bounds(&self, name: &str) -> Option<Rect> {
        let id = self.nodes.named(name)?;
        self.screen_rect(id)
    }

    /// 窗口要不要重新设置指针形状（指针形状的绑定变了），取走标记
    pub(crate) fn take_cursor_dirty(&mut self) -> bool {
        self.router.take_cursor_dirty()
    }

    // ---- 焦点 ----

    /// 失去焦点时改完了的输入框，"怎么改的"按最近一次输入算（Tab、Esc 是键盘，点别处是指针）
    pub(super) fn change_source(&self) -> crate::event::ChangeSource {
        if self.router.keyboard() { crate::event::ChangeSource::Keyboard } else { crate::event::ChangeSource::Pointer }
    }

    /// 切换焦点；`visible` 表示是键盘带来的焦点（画焦点环）。返回要调用的 on_focus 处理器
    pub(super) fn focus_to(&mut self, target: Option<NodeId>, visible: bool) -> FocusCallbacks {
        let ring = self.router.keyboard_focus();
        if self.router.set_focus_visible(visible) {
            self.paint.invalidate();
        }
        let mut callbacks = self.set_focus(target);
        self.router.keyboard_focus_changed(&self.nodes, ring, &mut callbacks);
        callbacks
    }

    /// 指针按下：焦点给指针下最内层能获得焦点的元素（输入目标或 `focusable`），不画焦点环；按在别处时失去焦点
    /// 返回得到焦点的元素：按在多行输入框的视口上（内边距里、最后一行下面）也给里面的输入框，
    /// 可选中的文字也能得到焦点（选区只在它有焦点时显示）
    pub(super) fn focus_on_press(&mut self) -> (Option<NodeId>, FocusCallbacks) {
        let target = self.focus_target_under_pointer();
        (target, self.focus_to(target, false))
    }

    /// 指针下最内层能获得焦点的元素。输入目标说按在哪里算（有链接的富文本不能选中时只有按在链接上才获得焦点，
    /// 按在别的字上和按在空白处一样）
    pub(super) fn focus_target_under_pointer(&self) -> Option<NodeId> {
        let point = self.router.pointer();
        self.router.hovered().iter().rev().find_map(|&id| {
            let target = self.press_target(id, point).or_else(|| self.node_input(id).is_some_and(|i| i.focusable).then_some(id))?;
            (!self.disabled(id)).then_some(target)
        })
    }

    /// 能用 Tab 走到的元素，按树的顺序（声明在元素上的浮层跟在元素的子孙后面）。
    /// 跳过退场中、禁用、看不见（透明度为 0、大小为 0、被同标签的共享元素接替了）的元素；有 `scope` 时只在它里面找
    pub(super) fn focus_order(&self, scope: Option<NodeId>) -> Vec<NodeId> {
        let starts = match scope {
            Some(scope) => vec![scope],
            None => self.nodes.roots().to_vec(),
        };
        let mut stack: Vec<(NodeId, f32)> = starts.into_iter().rev().map(|id| (id, 1.0)).collect();
        let mut order = Vec::new();
        while let Some((id, opacity)) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            let input = node.input.as_deref();
            if node.exiting || input.is_some_and(|i| i.disabled) || node.extra.as_deref().is_some_and(|e| e.hidden()) {
                continue;
            }
            let opacity = opacity * tab_opacity(node);
            if opacity <= 0.0 {
                continue;
            }
            if self.focusable(id) && node.bounds.width > 0.0 && node.bounds.height > 0.0 {
                order.push(id);
            }
            if let Some(input) = input {
                stack.extend(input.overlays.iter().rev().filter(|&&host| self.tabbable_overlay(host)).map(|&host| (host, 1.0)));
            }
            stack.extend(node.children.iter().rev().map(|&child| (child, opacity)));
        }
        order
    }

    /// Tab / Shift+Tab：按树的顺序走到下一个（上一个），到头了绕回来；有模态浮层时只在它里面转。
    /// 焦点在富文本的链接上时先在这段文字的链接之间走。
    ///
    /// 没有模态浮层时用缓存的候选（[`FocusCache`]）：树的结构没变就不重新遍历整棵树，从焦点往后一个个看
    /// 能不能走到，一般看一两个就找到了
    pub(super) fn move_focus(&mut self, forward: bool) -> FocusCallbacks {
        if let Some(callbacks) = self.step_inline(forward) {
            return callbacks;
        }
        if let Some(callbacks) = self.step_link(forward) {
            return callbacks;
        }
        if let Some(trap) = self.trap() {
            let order = self.focus_order(Some(trap));
            return self.step_focus(&order, forward);
        }
        match self.next_tab_stop(forward) {
            Some(target) => self.tab_to(target, forward),
            None => Vec::new(),
        }
    }

    /// 候选里（从焦点往前或往后，绕回来）下一个现在走得到的元素
    fn next_tab_stop(&mut self, forward: bool) -> Option<NodeId> {
        // 结构变了才重建：重建要同时读整棵树，先把缓存取下来（取下时留的空缓存要分配，所以只在这时取）
        if self.router.focus_cache.structure != Some(self.nodes.structure()) {
            let mut cache = std::mem::take(&mut self.router.focus_cache);
            cache.rebuild(self);
            self.router.focus_cache = cache;
        }
        let cache = &self.router.focus_cache;
        let n = cache.order.len();
        let current = self.router.focused().and_then(|f| cache.index.get(f).copied()).filter(|&i| self.tab_reachable(cache.order[i as usize]));
        (0..n)
            .map(|k| match (current, forward) {
                (Some(i), true) => (i as usize + 1 + k) % n,
                (Some(i), false) => (i as usize + n - 1 - k % n) % n,
                (None, true) => k,
                (None, false) => n - 1 - k,
            })
            .map(|i| cache.order[i])
            .find(|&id| self.tab_reachable(id))
    }

    /// Tab 此刻能不能走到这个候选：能获得焦点、有大小，沿逻辑上的祖先（浮层接到声明它的元素上）都没在退场、
    /// 没被禁用、看得见（透明度按目标算，没被共享元素接替），经过的浮层开着、接收指针。和 [`Self::focus_order`] 的遍历一样
    fn tab_reachable(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else { return false };
        if !(self.focusable(id) && node.bounds.width > 0.0 && node.bounds.height > 0.0) {
            return false;
        }
        let mut current = Some(id);
        while let Some(n) = current {
            let Some(node) = self.nodes.get(n) else { return false };
            if node.exiting || node.input.as_deref().is_some_and(|i| i.disabled) || node.extra.as_deref().is_some_and(|e| e.hidden()) || tab_opacity(node) <= 0.0 {
                return false;
            }
            current = match node.parent {
                Some(parent) => Some(parent),
                None => match self.overlay_declared_by(n) {
                    Some(owner) if self.tabbable_overlay(n) => Some(owner),
                    Some(_) => return false,
                    None => None,
                },
            };
        }
        true
    }

    /// 方向键在 `scope` 的子孙之间移动焦点
    pub(super) fn move_focus_within(&mut self, scope: NodeId, forward: bool) -> Option<FocusCallbacks> {
        let order: Vec<NodeId> = self.focus_order(Some(scope)).into_iter().filter(|&id| id != scope).collect();
        (!order.is_empty()).then(|| self.step_focus(&order, forward))
    }

    fn step_focus(&mut self, order: &[NodeId], forward: bool) -> FocusCallbacks {
        let n = order.len();
        if n == 0 {
            return Vec::new();
        }
        let current = self.router.focused().and_then(|f| order.iter().position(|&id| id == f));
        let next = match (current, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        self.tab_to(order[next], forward)
    }

    /// 用键盘把焦点给 `target`：画焦点环，落在有链接的富文本的第一个（往回走时最后一个）链接上，滚进视口
    pub(super) fn tab_to(&mut self, target: NodeId, forward: bool) -> FocusCallbacks {
        // 走进又有链接又有能拿焦点的行内元素的富文本：按它们在文字里的先后，落在第一个（往回走时最后一个）
        if let Some(callbacks) = self.enter_inline_stops(target, forward) {
            return callbacks;
        }
        let callbacks = self.focus_to(Some(target), true);
        // 走到有链接的富文本上：落在第一个（往回走时最后一个）链接上
        self.enter_links(target, forward, None);
        self.scroll_into_view(target, true);
        callbacks
    }

    /// 焦点句柄的请求
    fn focus_request(&mut self, id: NodeId, focus: bool) -> FocusCallbacks {
        if !self.nodes.contains_key(id) || self.nodes[id].exiting {
            return Vec::new();
        }
        if focus {
            let callbacks = self.focus_to(Some(id), self.router.keyboard());
            self.enter_links(id, true, None);
            // 指针正指着的元素（菜单项在悬停时拿焦点）已经看得见，不滚：滚了它会从指针下面跑开，
            // 停在半露出来的那一项上时会一项接一项地一直滚下去
            if !self.router.hovered().contains(&id) {
                self.scroll_into_view(id, true);
            }
            callbacks
        } else if self.router.focused() == Some(id) {
            self.focus_to(None, false)
        } else {
            Vec::new()
        }
    }

    /// 这个元素此刻要不要画焦点环
    /// `id` 有焦点时要不要给它画焦点环
    pub(super) fn ring_on(&self, id: NodeId) -> bool {
        self.router.focus_visible()
            && self.router.focused() == Some(id)
            && self.focused_link().is_none()
            && self.node_input(id).and_then(|i| i.ring).unwrap_or_default().visible()
    }

    /// 焦点环：元素外面一圈边框，和元素在同一个层、用父元素的裁剪，画在元素的子孙之后。
    /// 出现、消失都是立即的，不做动画
    #[cold]
    #[inline(never)]
    pub(super) fn paint_ring(&self, id: NodeId, layer: u32, clip: u32, local: Affine, opacity: f32, list: &mut DisplayList) {
        let node = &self.nodes[id];
        let ring = node.input.as_ref().and_then(|i| i.ring).unwrap_or_default();
        let grow = ring.offset + ring.width;
        let b = node.bounds;
        let rect = Rect { x: b.x - grow, y: b.y - grow, width: b.width + grow * 2.0, height: b.height + grow * 2.0 };
        let radius = ring.radius.unwrap_or(node.paint.radius + grow).max(0.0);
        let [cx, cy] = local.apply(rect.center());
        let mut quad = Quad::new(cx - rect.width * 0.5, cy - rect.height * 0.5, rect.width, rect.height)
            .radius(radius)
            .fill(scene::Color::TRANSPARENT)
            .border(ring.width, ring.color)
            .opacity(opacity);
        quad.transform = local.linear;
        quad.layer = layer;
        quad.clip = clip;
        list.quads.push(quad);
    }

    // ---- 卸载、退场 ----

    /// 节点开始退场、浮层开始关闭、节点被卸载：路由状态（焦点、悬停、按下、拖选、拖动、手势……）和拖放里
    /// 指向它们的都清掉。返回焦点是不是因此清掉了
    pub(super) fn forget_nodes(&mut self, leave: Leave) -> bool {
        let lost_focus = self.router.forget(&self.nodes, leave);
        self.forget_drag_drop(leave);
        lost_focus
    }
}

// ---- 挂载 ----

/// 挂载时把元素上的输入设置交给节点，绑定会变的（叠放次序、指针形状、禁用）；返回声明的浮层，子元素挂好后再挂
pub(super) fn attach(tree: &TreeRef, id: NodeId, interaction: Interaction) -> Vec<OverlaySpec> {
    let Interaction {
        focus_handle,
        overlays,
        measure,
        drag_drop,
        states: _,
        memo: _,
        field_handle,
    } = interaction;
    if let Some(drag_drop) = drag_drop {
        tree.borrow_mut().with_input(id, |i| i.drag_drop = Some(Rc::new(*drag_drop)));
    }
    if let Some(handle) = field_handle {
        handle.bind(tree, id);
    }
    if let Some(handle) = focus_handle {
        bind_focus_handle(tree, id, handle);
    }
    for signal in measure {
        tree.borrow_mut().measures.add(id, signal);
    }
    overlays
}

/// 焦点句柄：节点的焦点变化写进句柄；句柄的请求交给树
fn bind_focus_handle(tree: &TreeRef, id: NodeId, handle: FocusHandle) {
    let tree_ref = tree;
    tree.borrow_mut().nodes.add_handler(
        id,
        EventType::Focus,
        Handler::Flag(Rc::new(move |focused| {
            handle.focused.set_if_changed(focused);
        })),
    );
    let tree: Weak<_> = Rc::downgrade(tree);
    Effect::new(move || {
        let request = handle.request.get();
        if request.serial <= handle.handled.get_untracked() {
            return;
        }
        handle.handled.set(request.serial);
        let Some(tree) = tree.upgrade() else { return };
        // 标签点了：禁用的元素什么都不做；获得焦点之后在它上面合成一次标签来的点击（只交给它自己，用户的处理器在前，
        // 控件的内置处理器在后：复选框切换、下拉框打开……；没有点击处理器的输入框、滑块只获得焦点）
        if request.activate && tree.borrow().disabled(id) {
            return;
        }
        let callbacks = untracked(|| tree.borrow_mut().focus_request(id, request.focus));
        run_focus_callbacks(callbacks);
        if request.activate {
            let mut event = tree.borrow().click_event(id, ClickSource::Label, MouseButton::Left, 1);
            super::route::deliver(&tree, id, EventType::Click, crate::event::PropagationPhase::Target, &mut event);
        }
    });
    tree_ref.borrow_mut().router.add_handle(handle, id);
}

// ---- 指针 ----

impl Tree {
    pub(super) fn drag_event(&self, drag: &Drag, phase: GesturePhase, delta: [f32; 2], now: f64) -> DragEvent {
        let recent: Vec<&(f64, [f32; 2])> = drag.samples.iter().filter(|(time, _)| now - time <= VELOCITY_WINDOW).collect();
        let velocity = match (recent.first(), recent.last()) {
            (Some(&&(t0, p0)), Some(&&(t1, p1))) if recent.len() >= 2 => {
                let dt = (t1 - t0).max(1.0 / 240.0) as f32;
                [(p1[0] - p0[0]) / dt, (p1[1] - p0[1]) / dt]
            }
            _ => [0.0; 2],
        };
        DragEvent {
            phase,
            position: drag.node.map_or(drag.last, |node| self.local_point(node, drag.last)),
            window_position: drag.last,
            start_position: drag.start,
            delta,
            translation: sub(drag.last, drag.start),
            velocity,
            button: drag.button,
            modifiers: self.router.modifiers(),
            flow: Default::default(),
        }
    }
}

// ---- 键盘 ----

impl Tree {
    /// 按下：记下按着的键和冒泡的起点（系统重复的按下不改）
    pub(super) fn hold(&mut self, input: &KeyInput, event: &KeyEvent) {
        if let Some(held) = self.router.held().iter().find(|h| h.same(input.physical.as_ref(), &event.base)) {
            if input.repeated {
                return;
            }
            // 没收到松开又按了一次（松开丢了）：换成这一次
            let base = held.base.clone();
            self.router.held_mut().retain(|h| !h.same(input.physical.as_ref(), &base));
        }
        let start = self.router.focused().or_else(|| self.trap_content()).or_else(|| self.nodes.roots().first().copied());
        crate::interaction::set_held(&event.base, true);
        self.router.held_mut().push(Held { physical: input.physical.clone(), key: event.key.clone(), base: event.base.clone(), start });
    }

}

/// 键盘滚动的动作
#[derive(Clone, Copy, Debug)]
pub(super) enum KeyScroll {
    Line(f32),
    Page(f32),
    Start,
    End,
}

/// 一帧结束后（已释放树的借用）：调用借用树时产生的焦点处理器。返回有没有调用
pub(super) fn flush(tree: &TreeRef) -> bool {
    let (callbacks, drag_drop) = {
        let mut t = tree.borrow_mut();
        (t.router.take_callbacks(), t.drag_drop.pending())
    };
    let drag_drop = drag_drop && super::drag_drop::flush(tree);
    if callbacks.is_empty() {
        return drag_drop;
    }
    run_focus_callbacks(callbacks);
    true
}
