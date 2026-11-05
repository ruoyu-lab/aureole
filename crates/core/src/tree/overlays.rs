//! 元素树里的浮层：打开（挂成单独的根）、每帧按锚点放好位置、命中测试、在外面按下或 Esc 时请求关闭、
//! 模态浮层挡住下面的输入和困住焦点、关闭时播完退场动画再卸载。对外的 API 在 [`crate::overlay`]。
//!
//! 一个浮层是三层节点：宿主（铺满窗口，绝对定位，挂在布局根节点下）→ 可选的遮罩（铺满窗口）+ 包装（左上角在 (0, 0)、
//! 大小由内容决定）→ 内容。包装是一个合成层，它的位置是层的额外平移（和滚动偏移一样）：
//! 锚点移动、滚动时只改层的平移，不重新布局、不重建图元。宿主不在 `roots` 里，画在所有根元素之后，
//! 不受任何祖先的裁剪。
//!
//! 内容的根元素是一个跟着内容长高的竖向视口（[`crate::overlay::scrollable`]），包装是竖排：放好位置之后，
//! 包装的最大高度设成放的那一边能用的高度（离窗口边缘留 `margin`），放不下的内容在根元素里滚动。
//! 第一次放好（高度上限不再变）时，内容自己把焦点给了里面的元素的话把它滚进视口（下拉选择打开时选中的那一项）。

use std::cell::Cell;
use std::rc::Rc;

use animation::Curve;
use reactive::{Effect, Scope, Signal, on_cleanup, signal, untracked};

use taffy::LengthPercentageAuto;

use super::{FocusCallbacks, Leave, NodeId, Tree, TreeRef, bind, layer_key, mount, run_focus_callbacks};
use crate::property::Property;
use crate::element::Element;
use crate::overlay::{AnchorSpec, Dismiss, Edge, Overlay, OverlaySpec, Placement, Side, scrollable};
use crate::property::Prop;
use crate::geometry::Rect;
use crate::layout::Direction;

/// 一个打开着（或正在退场）的浮层
pub(crate) struct Host {
    pub id: NodeId,
    /// 声明它的元素（默认的锚点）
    owner: NodeId,
    wrapper: NodeId,
    backdrop: Option<NodeId>,
    anchor: Anchor,
    placement: Placement,
    offset: f32,
    margin: f32,
    modal: bool,
    passthrough: bool,
    auto_focus: bool,
    /// 打开时有焦点的元素，关闭时还给它
    restore: Option<NodeId>,
    /// 自动聚焦拿走过焦点：关闭时即使焦点已经没了（比如里面没有能获得焦点的元素）也还回去
    took_focus: bool,
    on_dismiss: Option<Rc<dyn Fn(Dismiss)>>,
    placed: Option<Signal<Placement>>,
    last: Option<Placement>,
    /// 包装的最大高度（放的那一边能用的高度）；`None` 表示不限制（不接收指针的浮层）
    cap: Option<f32>,
    /// 第一次放好之后把里面的焦点滚进视口了
    revealed: bool,
    /// 正在退场：不响应输入，`exiting` 里的动画都停下后卸载
    closing: bool,
    exiting: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Anchor {
    Owner,
    Point([f32; 2]),
    Window,
}

/// 树上所有的浮层，从下到上
#[derive(Default)]
pub(crate) struct Overlays {
    list: Vec<Host>,
    /// 实际放的位置，释放树之后写进信号
    writes: Vec<(Signal<Placement>, Placement)>,
    /// 有浮层的高度上限变了：同一帧里要再布局一次
    resized: bool,
}

impl Overlays {
    pub fn hosts(&self) -> impl DoubleEndedIterator<Item = NodeId> + '_ {
        self.list.iter().map(|h| h.id)
    }

    /// 卸载了的浮层不再登记着
    pub fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        self.list.retain(|host| keep(host.id));
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

/// 挂载元素时（子元素都挂好之后）：每个声明的浮层一个副作用，条件为真时打开、变假时关闭。
/// 浮层的内容在自己的作用域里（归元素所有），元素卸载时浮层直接卸载（不播退场动画）
pub(super) fn declare(tree: &TreeRef, owner: NodeId, specs: Vec<OverlaySpec>) {
    for OverlaySpec { show, overlay } in specs {
        let overlay = Rc::new(overlay);
        let show: Rc<dyn Fn() -> bool> = match show {
            Prop::Static(v) => Rc::new(move || v),
            Prop::Dynamic(f) => Rc::from(f),
        };
        // 内容的作用域挂在这个作用域下，而不是挂在副作用上（副作用重新执行时会释放它拥有的节点）
        let owner_scope = Scope::new();
        let current: Rc<Cell<Option<(NodeId, Scope)>>> = Rc::default();
        let open = current.clone();
        let weak = Rc::downgrade(tree);
        Effect::new(move || {
            let visible = show();
            let Some(tree) = weak.upgrade() else { return };
            untracked(|| match (visible, open.get()) {
                (true, None) => {
                    let scope = owner_scope.run(Scope::new);
                    let host = scope.run(|| open_overlay(&tree, owner, &overlay));
                    open.set(Some((host, scope)));
                }
                (false, Some((host, scope))) => {
                    open.set(None);
                    scope.dispose();
                    let callbacks = tree.borrow_mut().close_overlay(host);
                    run_focus_callbacks(callbacks);
                }
                _ => {}
            });
        });
        let weak = Rc::downgrade(tree);
        on_cleanup(move || {
            if let Some((host, _)) = current.take()
                && let Some(tree) = weak.upgrade()
                && let Ok(mut t) = tree.try_borrow_mut()
            {
                t.remove_overlay(host);
            }
        });
    }
}

/// 挂上一个浮层：宿主 → 遮罩 + 包装 → 内容。内容的根元素做成跟着内容长高的视口（不接收指针的除外），
/// 包装先按窗口的高度限制，放好位置之后再按那一边能用的高度限制
fn open_overlay(tree: &TreeRef, owner: NodeId, overlay: &Overlay) -> NodeId {
    let mut host = Element::default().inset(0);
    if let Some(color) = &overlay.backdrop {
        host = host.child(backdrop(color.clone(), overlay.backdrop_blur, overlay.backdrop_transition));
    }
    let content = (overlay.view)();
    let (content, cap) = if overlay.passthrough {
        (content, None)
    } else {
        let window = tree.borrow().layout.viewport()[1];
        (scrollable(content), Some((window - 2.0 * overlay.margin).max(0.0)))
    };
    let mut wrapper = Element::default().direction(Direction::Column).position(0, 0).child(content);
    if let Some(cap) = cap {
        wrapper = wrapper.max_height(cap);
    }
    host = host.child(wrapper);
    let id = mount(tree, host);
    let anchor = match &overlay.anchor {
        AnchorSpec::Owner => Anchor::Owner,
        AnchorSpec::Point(point) => Anchor::Point(untracked(|| point())),
        AnchorSpec::Window => Anchor::Window,
    };
    {
        let mut t = tree.borrow_mut();
        let layout = t.nodes[id].layout;
        t.layout.add_root(layout).expect("挂载浮层失败");
        let children = t.nodes[id].children.clone();
        let wrapper = *children.last().expect("浮层至少有包装");
        let backdrop = (children.len() > 1).then(|| children[0]);
        // 包装的位置是合成层的平移
        t.nodes[wrapper].layer = true;
        t.nodes[wrapper].refresh_flags();
        let auto_focus = overlay.auto_focus.unwrap_or(overlay.modal);
        let restore = t.router.focused();
        // 共享元素在飞的时候打开的浮层盖住它
        t.shared.mounted([id]);
        t.overlays.list.push(Host {
            id,
            owner,
            wrapper,
            backdrop,
            anchor,
            placement: overlay.placement,
            offset: overlay.offset,
            margin: overlay.margin,
            modal: overlay.modal,
            passthrough: overlay.passthrough,
            auto_focus,
            restore,
            took_focus: false,
            on_dismiss: overlay.on_dismiss.clone(),
            placed: overlay.placed,
            last: None,
            cap,
            revealed: false,
            closing: false,
            exiting: Vec::new(),
        });
        t.with_input(owner, |i| i.overlays.push(id));
        t.layout.restyle();
        t.layout.invalidate();
        t.accessibility.touch_root();
    }
    // 锚点可以绑定信号（右键菜单跟着新的位置）
    if let AnchorSpec::Point(point) = &overlay.anchor {
        let point = point.clone();
        bind(tree, Some(Prop::Dynamic(Box::new(move || point()))), move |t, p| t.set_anchor_point(id, p));
    }
    id
}

/// 遮罩：铺满窗口，挡住指针。给了曲线时颜色（和背景模糊）按它渐变上来，关闭时渐变回去；没给时立即出现、消失
fn backdrop(color: Rc<dyn Fn() -> scene::Color>, blur: f32, transition: Option<Curve>) -> Element {
    let Some(curve) = transition else {
        let element = Element::default().inset(0).background(move || color());
        return if blur > 0.0 { element.backdrop_blur(blur) } else { element };
    };
    let shown = signal(false);
    let clear = untracked(|| color()).with_alpha(0.0);
    let mut element = Element::default()
        .inset(0)
        .background(move || {
            let color = color();
            if shown.get() { color } else { color.with_alpha(0.0) }
        })
        .transition(Property::Background, curve)
        .exit(Property::Background, clear)
        .on_mount(move || shown.set(true));
    if blur > 0.0 {
        element = element
            .backdrop_blur(move || if shown.get() { blur } else { 0.0 })
            .transition(Property::BackdropBlur, curve)
            .exit(Property::BackdropBlur, 0.0);
    }
    element
}

impl Tree {
    fn host(&self, id: NodeId) -> Option<&Host> {
        self.overlays.list.iter().find(|h| h.id == id)
    }

    fn set_anchor_point(&mut self, id: NodeId, point: [f32; 2]) {
        if let Some(host) = self.overlays.list.iter_mut().find(|h| h.id == id) {
            host.anchor = Anchor::Point(point);
            self.paint.invalidate();
        }
    }

    /// `id` 是浮层的宿主时，声明它的元素（模态的也算）
    pub(super) fn overlay_declared_by(&self, id: NodeId) -> Option<NodeId> {
        self.host(id).map(|h| h.owner)
    }

    /// `id` 是浮层的宿主时，事件冒泡到声明它的元素；模态浮层是边界（没有）
    pub(super) fn overlay_owner(&self, id: NodeId) -> Option<NodeId> {
        self.host(id).filter(|h| !h.modal).map(|h| h.owner)
    }

    /// 能用 Tab 走进去的浮层：开着、接收指针
    pub(super) fn tabbable_overlay(&self, id: NodeId) -> bool {
        self.host(id).is_some_and(|h| !h.closing && !h.passthrough)
    }

    /// 最上面的模态浮层（困住焦点）
    pub(super) fn trap(&self) -> Option<NodeId> {
        self.overlays.list.iter().rev().find(|h| !h.closing && h.modal).map(|h| h.id)
    }

    /// 最上面的模态浮层的内容（没有焦点时按键从这里开始冒泡）
    pub(super) fn trap_content(&self) -> Option<NodeId> {
        let host = self.overlays.list.iter().rev().find(|h| !h.closing && h.modal)?;
        self.nodes.get(host.wrapper)?.children.first().copied()
    }

    /// Esc 关的是哪个浮层（的宿主）：最上面有 on_dismiss 的浮层；中间隔着不能关的模态浮层时不关下面的
    pub(super) fn dismissable_host(&self) -> Option<NodeId> {
        for host in self.overlays.list.iter().rev().filter(|h| !h.closing) {
            if host.on_dismiss.is_some() {
                return Some(host.id);
            }
            if host.modal {
                return None;
            }
        }
        None
    }

    /// 浮层 `id` 还开着、能关时，它的 on_dismiss
    pub(super) fn dismissable_host_handler(&self, id: NodeId) -> Option<Rc<dyn Fn(Dismiss)>> {
        self.host(id).filter(|h| !h.closing).and_then(|h| h.on_dismiss.clone())
    }

    /// 在 `path`（按下处从外到内的元素）按下：从上往下，按在外面的浮层请求关闭；
    /// 遇到按在里面的（或者按的是它的锚点元素）、或者模态的就停
    pub(super) fn outside_dismissals(&self, path: &[NodeId]) -> Vec<Rc<dyn Fn(Dismiss)>> {
        let mut dismiss = Vec::new();
        for host in self.overlays.list.iter().rev().filter(|h| !h.closing) {
            let inside = path.contains(&host.wrapper) || (host.anchor == Anchor::Owner && path.contains(&host.owner));
            if inside {
                break;
            }
            if let Some(d) = &host.on_dismiss {
                dismiss.push(d.clone());
            }
            if host.modal {
                break;
            }
        }
        dismiss
    }

    /// 命中测试先看浮层（从上往下）：点在浮层的内容（或遮罩）上就是它；模态浮层挡住下面的一切；
    /// 不接收指针、正在退场的跳过。路径写进 `out`（原来是空的）；返回 `false` 表示交给普通的根元素
    pub(super) fn hit_overlays(&self, point: [f32; 2], out: &mut super::route::HitPath) -> bool {
        for host in self.overlays.list.iter().rev() {
            if host.closing || host.passthrough {
                continue;
            }
            out.push(host.id);
            self.hit_from(self.nodes.paint_order(host.id), point, out);
            if out.len() > 1 || host.modal {
                return true;
            }
            out.clear();
        }
        false
    }

    /// 关闭：不再响应输入，焦点在里面的话还给打开前的元素，内容和遮罩开始退场
    pub(super) fn close_overlay(&mut self, id: NodeId) -> FocusCallbacks {
        let Some(index) = self.overlays.list.iter().position(|h| h.id == id) else { return Vec::new() };
        self.overlays.list[index].closing = true;
        // 悬停、按下、拖选、拖动、拖放里指向浮层的清掉（它的处理器已经随作用域释放）
        self.forget_nodes(Leave::Close(id));
        let took = self.overlays.list[index].took_focus && self.router.focused().is_none();
        let callbacks = if took || self.router.focused().is_some_and(|f| self.nodes.is_within(f, id)) {
            let restore = self.overlays.list[index].restore.filter(|&r| self.focus_target_valid(r));
            let visible = self.router.focus_visible();
            self.focus_to(restore, visible && restore.is_some())
        } else {
            Vec::new()
        };
        let host = &self.overlays.list[index];
        let content = self.nodes.get(host.wrapper).and_then(|w| w.children.first().copied());
        let candidates: Vec<NodeId> = host.backdrop.into_iter().chain(content).collect();
        let mut exiting = Vec::new();
        for node in candidates {
            if self.has_exit(node) {
                self.start_exit(node);
                exiting.push(node);
            }
        }
        self.overlays.list[index].exiting = exiting;
        self.paint.invalidate();
        self.accessibility.touch_root();
        callbacks
    }

    /// 关闭时焦点能不能还给这个元素（松开的键能不能交给它）：还在、没在退场、不在正在关的浮层里
    pub(super) fn focus_target_valid(&self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(|n| !n.exiting)
            && !self.overlays.list.iter().any(|h| h.closing && self.nodes.is_within(id, h.id))
    }

    /// 退场动画都停下的浮层卸载
    pub(super) fn finish_overlays(&mut self) {
        let done: Vec<NodeId> = self
            .overlays
            .list
            .iter()
            .filter(|h| {
                h.closing
                    && h.exiting.iter().all(|&n| self.nodes.get(n).is_none_or(|node| node.animations.iter().all(|s| !s.queued)))
            })
            .map(|h| h.id)
            .collect();
        for id in done {
            self.remove_overlay(id);
        }
    }

    /// 立刻卸载浮层（声明它的元素卸载了，或者退场结束）
    pub(super) fn remove_overlay(&mut self, id: NodeId) {
        let Some(index) = self.overlays.list.iter().position(|h| h.id == id) else { return };
        let host = self.overlays.list.remove(index);
        self.with_input(host.owner, |i| i.overlays.retain(|&h| h != id));
        self.unmount(id);
        self.paint.invalidate();
        self.accessibility.touch_root();
    }

    /// 布局、生成图元之后：按锚点此刻在屏幕上的位置放好每个浮层（改包装层的平移）；模态浮层第一次放好后自动聚焦
    pub(super) fn place_overlays(&mut self) {
        let window = Rect { x: 0.0, y: 0.0, width: self.layout.viewport()[0], height: self.layout.viewport()[1] };
        for index in 0..self.overlays.list.len() {
            let host = &self.overlays.list[index];
            let Some(wrapper) = self.nodes.get(host.wrapper) else { continue };
            let size = [wrapper.bounds.width, wrapper.bounds.height];
            let anchor = match host.anchor {
                Anchor::Owner => match self.screen_rect(host.owner) {
                    Some(rect) => rect,
                    None => continue,
                },
                Anchor::Point([x, y]) => Rect { x, y, width: 0.0, height: 0.0 },
                Anchor::Window => window,
            };
            let (position, placement, room) = if host.anchor == Anchor::Window {
                let x = ((window.width - size[0]) * 0.5).max(host.margin);
                let y = ((window.height - size[1]) * 0.5).max(host.margin);
                ([x.round(), y.round()], host.placement, window.height - 2.0 * host.margin)
            } else {
                place(anchor, size, host.placement, host.offset, host.margin, window)
            };
            let (wrapper_id, closing) = (host.wrapper, host.closing);
            // 高度上限：放的那一边能用的高度。只在内容顶到了上限（被限制着，或者放不下了）时改，
            // 改了要再布局一次（同一帧里），这次先不把焦点滚进来
            let room = room.max(0.0).floor();
            let resized = !closing && host.cap.is_some_and(|cap| cap != room && size[1] >= cap.min(room) - 0.5);
            if resized {
                self.overlays.list[index].cap = Some(room);
                self.update_style(wrapper_id, |s| s.max_size.height = LengthPercentageAuto::length(room));
                self.overlays.resized = true;
            }
            let host = &self.overlays.list[index];
            if let Some(placed) = host.placed
                && host.last != Some(placement)
            {
                self.overlays.writes.push((placed, placement));
            }
            self.overlays.list[index].last = Some(placement);
            let node = &mut self.nodes[wrapper_id];
            if node.shift != position {
                node.shift = position;
                self.commit.offset(layer_key(wrapper_id), node.offset());
                self.paint.mark_worlds();
                self.accessibility.touch(wrapper_id);
            }
            // 自动聚焦：等内容第一次布局好（才知道哪些元素看得见）
            let host = &mut self.overlays.list[index];
            if host.auto_focus && !closing && size != [0.0; 2] {
                host.auto_focus = false;
                let (id, modal) = (host.id, host.modal);
                let first = self.focus_order(Some(id)).first().copied();
                // 模态浮层里没有能获得焦点的元素时也把焦点从下面拿走：回车不能再激活下面的按钮
                if first.is_some() || modal {
                    // 自动聚焦的是第一个能获得焦点的元素：不为它滚动（长对话框从顶上开始看，按钮在下面）
                    self.overlays.list[index].took_focus = true;
                    self.overlays.list[index].revealed = true;
                    let visible = first.is_some() && self.router.keyboard();
                    let callbacks = self.focus_to(first, visible);
                    self.router.defer_callbacks(callbacks);
                }
            }
            // 第一次放好（高度上限不再变）：内容自己把焦点给了里面的元素（下拉选择的选中项）时把它滚进视口（不做动画）
            let host = &mut self.overlays.list[index];
            if !host.revealed && !closing && !resized && size != [0.0; 2] {
                host.revealed = true;
                let id = host.id;
                if let Some(focused) = self.router.focused().filter(|&f| self.nodes.is_within(f, id)) {
                    self.scroll_into_view(focused, false);
                }
            }
        }
    }

    /// 无障碍：`id` 是一个浮层的内容（包装里的元素）时返回（是否模态，是否不接收指针）
    pub(super) fn overlay_content(&self, id: NodeId) -> Option<(bool, bool)> {
        let wrapper = self.nodes.get(id)?.parent?;
        let host = self.overlays.list.iter().find(|h| h.wrapper == wrapper)?;
        Some((host.modal, host.passthrough))
    }

    /// 回放快照：开着的浮层（声明它的元素、是不是正在关），从下到上
    pub(super) fn overlay_owners(&self) -> Vec<(NodeId, bool)> {
        self.overlays.list.iter().map(|h| (h.owner, h.closing)).collect()
    }

    /// 无障碍：开着的浮层（正在关的不算），从下到上
    pub(super) fn open_hosts(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.overlays.list.iter().filter(|h| !h.closing).map(|h| h.id)
    }

    /// 元素此刻在窗口里的外接矩形（按祖先当前的变换、滚动、合成层动画值、3D 旋转和透视、共享元素的飞行）
    pub(super) fn screen_rect(&self, id: NodeId) -> Option<Rect> {
        let b = self.nodes.get(id)?.bounds;
        let world = self.screen_transform(id)?;
        // 有角在视点后面（转得太多、离得太近）时按能投到屏幕上的点算
        let corners = [[b.x, b.y], [b.x + b.width, b.y], [b.x, b.y + b.height], [b.x + b.width, b.y + b.height]];
        let [x0, y0, x1, y1] = corners
            .iter()
            .filter_map(|&c| world.project(c))
            .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |r, [x, y]| [r[0].min(x), r[1].min(y), r[2].max(x), r[3].max(y)]);
        (x0 <= x1).then_some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }
}

/// 放的位置：先放在指定的一边，放不下而另一边放得下（或者另一边更宽敞）时翻过去；
/// 沿边按对齐方式放，再沿两个方向挪进窗口（离边缘至少 `margin`）。返回左上角（取整到逻辑像素）、实际的位置，
/// 和浮层最多能有多高（放在上下时是那一边能用的高度，放在左右时是窗口的高度减去两边的 `margin`）。
///
/// 浮层被限制在能用的高度里之后，下一次放的时候它正好放得下那一边，不会来回翻
pub(crate) fn place(anchor: Rect, size: [f32; 2], placement: Placement, offset: f32, margin: f32, window: Rect) -> ([f32; 2], Placement, f32) {
    let main = match placement.side {
        Side::Top | Side::Bottom => 1,
        Side::Left | Side::Right => 0,
    };
    let cross = 1 - main;
    let start = [anchor.x, anchor.y];
    let end = [anchor.x + anchor.width, anchor.y + anchor.height];
    let limit = [window.width, window.height];
    let before = start[main] - offset - margin;
    let after = limit[main] - margin - end[main] - offset;
    let is_after = |side: Side| matches!(side, Side::Bottom | Side::Right);
    let room = |side: Side| if is_after(side) { after } else { before };
    let mut side = placement.side;
    if size[main] > room(side) && (size[main] <= room(side.opposite()) || room(side.opposite()) > room(side)) {
        side = side.opposite();
    }
    let mut position = [0.0; 2];
    position[main] = if is_after(side) { end[main] + offset } else { start[main] - offset - size[main] };
    position[cross] = match placement.edge {
        Edge::Start => start[cross],
        Edge::Center => (start[cross] + end[cross] - size[cross]) * 0.5,
        Edge::End => end[cross] - size[cross],
    };
    for axis in 0..2 {
        let max = limit[axis] - margin - size[axis];
        position[axis] = if max < margin { margin } else { position[axis].clamp(margin, max) };
        position[axis] = position[axis].round();
    }
    let height = if main == 1 { room(side) } else { window.height - 2.0 * margin };
    (position, Placement { side, edge: placement.edge }, height)
}

/// 一帧结束后：把实际放的位置写进信号。返回有没有写
pub(super) fn flush(tree: &TreeRef) -> bool {
    let (writes, resized) = {
        let mut t = tree.borrow_mut();
        (std::mem::take(&mut t.overlays.writes), std::mem::take(&mut t.overlays.resized))
    };
    if writes.is_empty() {
        return resized;
    }
    reactive::batch(|| {
        for (signal, placement) in writes {
            signal.set_if_changed(placement);
        }
    });
    true
}
