//! 元素树里的拖放：开始拖（取数据，准备影子或抬起元素）、每次移动找放置目标（进入、经过、离开）、放下或取消、
//! 影子和抬起的元素落到位的动画、靠近滚动视口边缘时自动滚动、拖到应用外面的请求。对外的 API 在 [`crate::drag_drop`]。
//!
//! 影子（[`DragPreview::Copy`]）是源元素的子树在最上面再画一遍：放在一个单独的合成层里，层的偏移跟着指针，
//! 拖的时候不重新布局、不重画；源元素自己变淡（它的层的透明度）。抬起（[`DragPreview::Lift`]）是源元素自己的合成层
//! 加一个额外平移（和滚动偏移同一个字段），叠在兄弟上面。两种都按开始拖时源元素的设置限制位移（只沿一个方向、不出父元素），
//! 在父元素坐标里算；找放置目标、自动滚动只看指针。
//!
//! 拖着元素出了窗口、指针下面是同一个应用的另一个窗口时（窗口宿主问 [`crate::host::HostWindow::app_window_at`]），
//! 和拖到应用外面一样交给系统，请求带着这份数据（[`DragOutRequest::data`]）；那个窗口（包括交给系统以后又回到的这个窗口）
//! 把系统的拖放换成这里的"来访"（[`visit_over`] 这几个）：找目标、进入、经过、离开、放下和窗口里的一样，只是没有影子。

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;

use animation::{CurveTrack, MAX_COMPONENTS, TrackState, Track};
use reactive::{batch, untracked};
use scene::{DisplayList, Quad};

use super::{Frame, Leave, NodeId, Tree, TreeRef, layer_key};
use crate::property::PropId;
use crate::compositor::{DerivedLayer, LayerInfo, LayerTrack, LayerValues};
use crate::drag_drop::{DragDropSpec, DragOut, DragPreview, DropEvent};
use crate::geometry::Affine;
use crate::handlers::{EventType, Handler, On};
use crate::layout::Direction;

/// 影子的透明度
const GHOST_OPACITY: f32 = 0.85;
/// 用影子拖时原来的元素变淡成这样
const SOURCE_DIM: f32 = 0.4;
/// 自动滚动：指针离视口边缘这么近（逻辑像素）开始滚，贴着边缘或者出了边缘时最快，每秒这么多
const EDGE: f32 = 48.0;
const EDGE_SPEED: f32 = 900.0;

/// 影子的层标识（见 [`DerivedLayer`]）
fn ghost_key(id: NodeId) -> u64 {
    DerivedLayer::Ghost.key(layer_key(id))
}

/// 影子投影的层标识
fn shadow_key(id: NodeId) -> u64 {
    DerivedLayer::GhostShadow.key(layer_key(id))
}

fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

/// 树上拖放的状态
#[derive(Default)]
pub(crate) struct DragDropState {
    /// 进行中的元素之间的拖放
    session: Option<Box<Session>>,
    ghost: Option<Box<Ghost>>,
    lift: Option<Box<Lift>>,
    /// 等窗口交给系统的拖到应用外面
    out: Option<DragOutRequest>,
    /// 交给了系统、还没结束的拖到应用外面：源元素，系统的结果写在这里（下一帧交给源元素）
    out_source: Option<(NodeId, Rc<Cell<Option<bool>>>)>,
    /// 借用树时没法调用、留到这一帧结束的离开处理器（源元素在拖的时候被卸载了）
    deferred: Vec<(On<DropEvent>, DropEvent)>,
    /// 同一个应用的窗口交给系统的元素之间拖放正经过这个窗口
    visit: Option<Box<Visit>>,
}

impl DragDropState {
    /// 正在元素之间拖放
    pub fn dragging(&self) -> bool {
        self.session.is_some()
    }

    /// 正在元素之间拖放时拖着的数据
    pub fn data(&self) -> Option<Rc<dyn Any>> {
        self.session.as_ref().map(|s| s.data.clone())
    }

    /// 用影子拖、影子还在（拖动中或者正在落）：源元素（变淡，并且在最上面再画一遍）
    pub fn ghost_source(&self) -> Option<NodeId> {
        self.ghost.as_ref().map(|g| g.source)
    }

    /// 这一帧结束时有没有要调用的处理器（留下的离开处理器、拖到应用外面的结果）
    pub fn pending(&self) -> bool {
        !self.deferred.is_empty() || self.out_source.as_ref().is_some_and(|(_, result)| result.get().is_some())
    }

    /// 拖放还没完：拖着（指针停在边缘时要每帧自动滚动）、影子在动、等系统的拖到应用外面的结果、有留下的处理器。
    /// 这时窗口不暂停显示链接
    pub fn busy(&self) -> bool {
        self.session.is_some() || self.animating() || self.out.is_some() || self.out_source.is_some() || !self.deferred.is_empty()
    }

    /// 有没有影子、抬起的元素在动
    pub fn animating(&self) -> bool {
        self.ghost.is_some() || self.lift.is_some()
    }

    /// 抬起的元素：兄弟之间画在最上面
    pub fn lifted(&self) -> Option<NodeId> {
        self.lift.as_ref().map(|l| l.node)
    }

    /// 主线程这一份影子层的值（测试、截图用；窗口里由渲染线程按动画算）
    pub fn layer_values(&self, key: u64) -> Option<LayerValues> {
        let ghost = self.ghost.as_deref()?;
        if key == ghost_key(ghost.source) {
            Some(ghost.values())
        } else if key == shadow_key(ghost.source) {
            Some(LayerValues { opacity: ghost.shadow(), ..ghost.identity() })
        } else {
            None
        }
    }

    /// 窗口里的拖放（`visit` 为假）或者来访的此刻落在哪
    fn hover_of(&mut self, visit: bool) -> Option<&mut Hover> {
        if visit { self.visit.as_deref_mut().map(|v| &mut v.hover) } else { self.session.as_deref_mut().map(|s| &mut s.hover) }
    }
}

/// 进行中的元素之间的拖放
pub(crate) struct Session {
    pub source: NodeId,
    data: Rc<dyn Any>,
    /// 指针相对源元素左上角（窗口坐标）的偏移
    grab: [f32; 2],
    hover: Hover,
    /// 源元素也能拖到应用外面：拖出窗口时交给系统
    out: bool,
    /// 影子、抬起的元素只沿这个方向走（属性 `drag_preview_axis`）
    axis: Option<Direction>,
    /// 影子、抬起的元素不出父元素（属性 `drag_preview_contained`）
    contained: bool,
}

/// 拖着的数据此刻落在哪个目标上
#[derive(Default)]
struct Hover {
    /// 接收这份数据、指针在它上面的元素
    target: Option<NodeId>,
    /// 上次交给目标的位置（目标自己的坐标）
    over: Option<[f32; 2]>,
}

/// 来访：同一个应用的窗口交给系统的元素之间拖放（[`DragOutRequest::data`]）正经过这个窗口
struct Visit {
    data: Rc<dyn Any>,
    /// 指针上次的位置（窗口坐标）
    point: [f32; 2],
    hover: Hover,
}

/// 影子：源元素的子树在最上面的合成层里再画一遍
struct Ghost {
    source: NodeId,
    /// 源元素本来不是合成层（为了变淡临时做成合成层）：结束后改回去
    forced: bool,
    /// 影子左上角此刻应该在的窗口位置：拖动中是指针减去抓的位置；松手后固定，之后的移动由平移动画给
    screen: [f32; 2],
    /// 上次画影子时源元素左上角的窗口位置（影子的图元按它烘焙，层的偏移是两者之差）
    painted: [f32; 2],
    phase: GhostPhase,
}

/// 影子落到位的动画：平移、透明度、投影的透明度四条轨道（渲染线程有同样的一份），和主线程这一帧采样的值
struct Settle {
    tracks: [CurveTrack; 4],
    /// x、y、透明度、投影的透明度
    values: [f32; 4],
}

impl Settle {
    /// 在动画时间 `t` 采样，返回还有没有在动的
    fn sample(&mut self, t: f64) -> bool {
        let mut moving = false;
        let mut out = [0.0; MAX_COMPONENTS];
        for (track, value) in self.tracks.iter().zip(&mut self.values) {
            moving |= track.sample(t, &mut out) == TrackState::Moving;
            *value = out[0];
        }
        moving
    }
}

enum GhostPhase {
    Dragging,
    /// 松手了：等这一帧布局、重画之后知道源元素在哪
    Pending,
    /// 落到源元素的（新）位置或者飞回去：平移、透明度到 1、投影淡出
    Settling(Box<Settle>),
}

impl Ghost {
    fn identity(&self) -> LayerValues {
        LayerValues { translate: [0.0; 2], scale: 1.0, rotation: 0.0, opacity: 1.0, offset: [0.0; 2] }
    }

    fn values(&self) -> LayerValues {
        let (translate, opacity) = match &self.phase {
            GhostPhase::Dragging | GhostPhase::Pending => ([0.0; 2], GHOST_OPACITY),
            GhostPhase::Settling(s) => ([s.values[0], s.values[1]], s.values[2]),
        };
        LayerValues { translate, opacity, offset: sub(self.screen, self.painted), ..self.identity() }
    }

    fn shadow(&self) -> f32 {
        match &self.phase {
            GhostPhase::Settling(s) => s.values[3],
            _ => 1.0,
        }
    }
}

/// 抬起的元素：自己的合成层加一个额外平移跟着指针
struct Lift {
    node: NodeId,
    /// 抬起前不是合成层：结束后改回去
    forced: bool,
    /// 松手后从指针处落到位的弹簧（额外平移 → 0）；拖动中是 `None`
    settle: Option<CurveTrack>,
    /// 上一次的布局位置：松手后数据重排、元素换了位置时，把平移换算到新位置上，画面不跳
    origin: [f32; 2],
}

/// 元素要拖到应用外面：窗口交给系统的拖动会话（[`crate::host::HostWindow::begin_drag_out`]）
pub struct DragOutRequest {
    /// 要拖出去的元素
    pub(crate) node: NodeId,
    /// 拖出去的内容
    pub out: DragOut,
    /// 指针的位置（窗口坐标）
    pub at: [f32; 2],
    /// 系统的会话结束时把有没有被接住写进来，下一帧交给源元素
    pub result: Rc<Cell<Option<bool>>>,
    /// 从元素之间的拖放交给系统时（拖出了窗口）是拖着的数据：拖到同一个应用的窗口上时，宿主交给那个窗口的
    /// [`crate::host::WindowTree::element_drag_over`] 这几个，那里的元素按元素之间的拖放接收。`out` 这时可以是空的
    pub data: Option<Rc<dyn Any>>,
}

/// 开始拖（越过阈值时）：取数据，准备影子或抬起元素
pub(super) fn start(tree: &TreeRef, node: NodeId, press: [f32; 2]) {
    let Some(spec) = tree.borrow().node_input(node).and_then(|i| i.drag_drop().cloned()) else { return };
    let Some(payload) = spec.payload.clone() else { return };
    let data = untracked(|| payload());
    let mut t = tree.borrow_mut();
    t.finish_preview();
    let Some(origin) = t.screen_origin(node, true) else { return };
    let (mut preview, axis, contained) =
        t.node_input(node).map_or((DragPreview::Copy, None, false), |i| (i.drag_preview, i.drag_preview_axis, i.drag_preview_contained));
    if preview == DragPreview::Lift && !t.can_lift(node) {
        preview = DragPreview::Copy;
    }
    match preview {
        DragPreview::Copy => {
            // 源元素做成合成层，变淡的透明度放在它的层上（生成图元的循环里不用多判断）
            let n = &mut t.nodes[node];
            let forced = !n.layer;
            n.layer = true;
            n.refresh_flags();
            let ghost = Ghost { source: node, forced, screen: origin, painted: origin, phase: GhostPhase::Dragging };
            t.drag_drop.ghost = Some(Box::new(ghost));
            t.paint.invalidate();
        }
        DragPreview::Lift => {
            let n = &mut t.nodes[node];
            let forced = !n.layer;
            n.layer = true;
            n.refresh_flags();
            let origin = [n.bounds.x, n.bounds.y];
            let parent = n.parent;
            t.drag_drop.lift = Some(Box::new(Lift { node, forced, settle: None, origin }));
            if let Some(parent) = parent {
                t.sort_children(parent);
            }
            t.paint.invalidate();
        }
        DragPreview::None => {}
    }
    let out = spec.drag_out.is_some();
    let session = Session { source: node, data, grab: sub(press, origin), hover: Hover::default(), out, axis, contained };
    t.drag_drop.session = Some(Box::new(session));
}

/// 指针移动了（或者内容在指针下滚过去了）：影子、抬起的元素跟着走，找放置目标，交给它进入、经过、离开。
/// 返回 `true` 表示拖出了窗口、源元素能拖到应用外面，要交给系统
pub(super) fn update(tree: &TreeRef) -> bool {
    let (data, point, source) = {
        let mut t = tree.borrow_mut();
        let Some(point) = t.router.pointer() else { return false };
        let Some(session) = t.drag_drop.session.as_deref() else { return false };
        if session.out && t.outside(point) {
            return true;
        }
        let (source, data) = (session.source, session.data.clone());
        t.follow(point);
        (data, point, source)
    };
    hover(tree, point, &data, Some(source), false);
    false
}

/// 拖着 `data` 经过 `point`（窗口坐标）：找放置目标（从内到外第一个接收这份数据的；`source` 自己和子孙不算），
/// 交给它进入、经过，交给原来的目标离开。`visit` 说的是来访（[`Visit`]）还是窗口里的拖放。返回有没有目标
fn hover(tree: &TreeRef, point: [f32; 2], data: &Rc<dyn Any>, source: Option<NodeId>, visit: bool) -> bool {
    let candidates: Vec<(NodeId, Rc<DragDropSpec>)> = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        let path = t.hit_path(point);
        // 从内到外有放置设置的元素
        path.iter()
            .rev()
            .filter(|&&id| source.is_none_or(|source| !t.nodes.is_within(id, source)) && !t.disabled(id))
            .filter_map(|&id| t.node_input(id)?.drag_drop().cloned().filter(|spec| spec.is_target()).map(|spec| (id, spec)))
            .collect()
    };
    // 接收条件是用户代码，在借用树之外调用
    let target = untracked(|| candidates.iter().find(|(_, spec)| spec.accepts(&**data)).map(|(id, _)| *id));
    let calls = {
        let mut t = tree.borrow_mut();
        let local = target.map(|id| t.local_point(id, point));
        let modifiers = t.router.modifiers();
        let Some(previous) = t.drag_drop.hover_of(visit).map(|h| h.target) else { return false };
        let leave = match previous {
            Some(old) if Some(old) != target => t.leave_calls(old, data, point),
            _ => Vec::new(),
        };
        let Some(hover) = t.drag_drop.hover_of(visit) else { return false };
        let entered = target != previous;
        let moved = local != hover.over;
        hover.target = target;
        hover.over = local;
        let event = local.map(|position| DropEvent { position, window_position: point, modifiers, data: data.clone() });
        let handlers = target.map(|id| (t.handlers_of(id, EventType::DragEnter, Handler::drop_event), t.handlers_of(id, EventType::DragOver, Handler::drop_event)));
        (leave, entered, moved, event, handlers)
    };
    let (leave, entered, moved, event, handlers) = calls;
    batch(|| {
        leave.iter().for_each(|(h, e)| h(e));
        if let (Some(event), Some((enter, over))) = (event, handlers) {
            if entered {
                enter.iter().for_each(|h| h(&event));
            }
            if entered || moved {
                over.iter().for_each(|h| h(&event));
            }
        }
    });
    target.is_some()
}

/// 放下的处理器：目标的 on_drop（类型对得上的），接着是 on_drag_leave
type DropCalls = (Vec<On<DropEvent>>, Vec<On<DropEvent>>, DropEvent);

/// 交给目标放下的处理器，返回有没有被接住
fn run_drop(call: Option<DropCalls>) -> bool {
    let Some((drops, leave, event)) = call else { return false };
    batch(|| {
        drops.iter().for_each(|h| h(&event));
        leave.iter().for_each(|h| h(&event));
    });
    true
}

/// 松开：交给目标的 on_drop，接着是 on_drag_leave；影子落到位（或飞回去），抬起的元素落下。返回有没有被接住
pub(super) fn drop_on_target(tree: &TreeRef) -> bool {
    let call = {
        let mut t = tree.borrow_mut();
        let Some(session) = t.drag_drop.session.take() else { return false };
        let point = t.router.pointer().unwrap_or_default();
        let call = t.drop_calls(session.hover.target, &session.data, point);
        t.end_preview();
        call
    };
    run_drop(call)
}

/// 取消（Esc、窗口失去焦点、拖出窗口交给系统）：目标收到离开；影子飞回去（交给系统时直接去掉）
pub(super) fn cancel(tree: &TreeRef, immediately: bool) {
    let leave = {
        let mut t = tree.borrow_mut();
        let Some(session) = t.drag_drop.session.take() else { return };
        if immediately {
            t.finish_preview();
        } else {
            t.end_preview();
        }
        let point = t.router.pointer().unwrap_or_default();
        session.hover.target.map(|id| t.leave_calls(id, &session.data, point)).unwrap_or_default()
    };
    if !leave.is_empty() {
        batch(|| leave.iter().for_each(|(h, e)| h(e)));
    }
}

/// 交给系统：取拖到应用外面的内容，等窗口交给系统。`data` 是元素之间拖放的数据（从元素之间的拖放交过去时有），
/// 拖到同一个应用的窗口上时那里按元素之间的拖放接收。什么都没有可拖时直接以 `false` 结束
pub(super) fn begin_out(tree: &TreeRef, node: NodeId, at: [f32; 2], data: Option<Rc<dyn Any>>) {
    let Some(spec) = tree.borrow().node_input(node).and_then(|i| i.drag_drop().cloned()) else { return };
    let out = spec.drag_out.clone().map(|items| untracked(|| items()).existing()).unwrap_or_default();
    if out.is_empty() && data.is_none() {
        let handlers = tree.borrow().handlers_of(node, EventType::DragOutEnd, Handler::flag);
        batch(|| handlers.iter().for_each(|h| h(false)));
        return;
    }
    tree.borrow_mut().drag_drop.out = Some(DragOutRequest { node, out, at, result: Rc::default(), data });
}

/// 来访的元素之间拖放（同一个应用的窗口交给系统的，见模块说明）经过 `point`（窗口坐标）：
/// 找放置目标，交给它进入、经过、离开。换了一份数据时先结束上一次来访。返回有没有元素接收这份数据
pub(crate) fn visit_over(tree: &TreeRef, point: [f32; 2], data: &Rc<dyn Any>) -> bool {
    let same = tree.borrow().drag_drop.visit.as_ref().is_some_and(|v| Rc::ptr_eq(&v.data, data));
    if !same {
        visit_leave(tree);
        tree.borrow_mut().drag_drop.visit = Some(Box::new(Visit { data: data.clone(), point, hover: Hover::default() }));
    }
    let source = {
        let mut t = tree.borrow_mut();
        if let Some(visit) = t.drag_drop.visit.as_deref_mut() {
            visit.point = point;
        }
        // 交给系统以后回到了源元素的窗口：它自己和子孙不算（和窗口里一样）
        t.drag_drop.out_source.as_ref().map(|(id, _)| *id)
    };
    hover(tree, point, data, source, true)
}

/// 来访的拖放离开了窗口（或者取消了）：目标收到离开
pub(crate) fn visit_leave(tree: &TreeRef) {
    let leave = {
        let mut t = tree.borrow_mut();
        let Some(visit) = t.drag_drop.visit.take() else { return };
        visit.hover.target.map(|id| t.leave_calls(id, &visit.data, visit.point)).unwrap_or_default()
    };
    if !leave.is_empty() {
        batch(|| leave.iter().for_each(|(h, e)| h(e)));
    }
}

/// 来访的拖放在 `point` 放下：交给指针下接收这份数据的元素的 on_drop，接着是 on_drag_leave。返回有没有被接住
pub(crate) fn visit_drop(tree: &TreeRef, point: [f32; 2], data: &Rc<dyn Any>) -> bool {
    visit_over(tree, point, data);
    let call = {
        let mut t = tree.borrow_mut();
        let Some(visit) = t.drag_drop.visit.take() else { return false };
        t.drop_calls(visit.hover.target, &visit.data, point)
    };
    run_drop(call)
}

/// 拖到应用外面结束了（系统的会话结束、平台不支持、测试工具模拟）：交给源元素的 on_drag_out_end
pub fn finish_out(tree: &TreeRef, accepted: bool) {
    let handlers = {
        let mut t = tree.borrow_mut();
        let Some((node, _)) = t.drag_drop.out_source.take() else { return };
        t.handlers_of(node, EventType::DragOutEnd, Handler::flag)
    };
    if !handlers.is_empty() {
        batch(|| handlers.iter().for_each(|h| h(accepted)));
    }
}

/// 一帧结束后：调用借用树时留下的离开处理器，交出拖到应用外面的结果。返回有没有调用
pub(super) fn flush(tree: &TreeRef) -> bool {
    let (deferred, result) = {
        let mut t = tree.borrow_mut();
        let result = t.drag_drop.out_source.as_ref().and_then(|(_, result)| result.get());
        (std::mem::take(&mut t.drag_drop.deferred), result)
    };
    let any = !deferred.is_empty() || result.is_some();
    if !deferred.is_empty() {
        batch(|| deferred.iter().for_each(|(h, e)| h(e)));
    }
    if let Some(accepted) = result {
        finish_out(tree, accepted);
    }
    any
}

impl Tree {
    /// 元素左上角此刻在窗口里的位置（按自己和祖先当前的变换、滚动、浮层位置）；`own_shift` 为假时不算自己的额外平移
    fn screen_origin(&self, id: NodeId, own_shift: bool) -> Option<[f32; 2]> {
        let node = self.nodes.get(id)?;
        let world = self.parent_world(id).then_local(
            node.bounds.center(),
            if own_shift { [node.paint.translate[0] + node.shift[0], node.paint.translate[1] + node.shift[1]] } else { node.paint.translate },
            node.paint.scale,
            node.paint.rotation,
        );
        Some(world.apply([node.bounds.x, node.bounds.y]))
    }

    /// 父元素此刻的屏幕变换（沿祖先链用当前的值算，不依赖 `world` 是否刷新过）
    fn parent_world(&self, id: NodeId) -> Affine {
        let mut chain = Vec::new();
        let mut current = self.nodes.get(id).and_then(|n| n.parent);
        while let Some(parent) = current {
            chain.push(parent);
            current = self.nodes.get(parent).and_then(|n| n.parent);
        }
        let mut world = Affine::IDENTITY;
        for &n in chain.iter().rev() {
            let node = &self.nodes[n];
            let p = &node.paint;
            world = world.then_local(node.bounds.center(), [p.translate[0] + node.shift[0], p.translate[1] + node.shift[1]], p.scale, p.rotation);
        }
        world
    }

    /// 能不能抬起：额外平移这个字段没被滚动（视口的内容）、浮层的位置占着
    fn can_lift(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else { return false };
        // 视口的内容：额外平移归滚动
        let scroll_content = node.scrolled;
        let wrapper = self.overlays.hosts().any(|host| host == id || node.parent == Some(host));
        !scroll_content && !wrapper && node.shift == [0.0; 2]
    }

    /// 影子、抬起的元素跟着指针
    fn follow(&mut self, point: [f32; 2]) {
        let Some(grab) = self.drag_drop.session.as_ref().map(|s| s.grab) else { return };
        let screen = sub(point, grab);
        let dragging = self.drag_drop.ghost.as_deref().filter(|g| matches!(g.phase, GhostPhase::Dragging)).map(|g| g.source);
        if let Some(source) = dragging {
            let screen = self.ghost_screen(source, screen);
            let ghost = self.drag_drop.ghost.as_deref_mut().expect("上面检查过");
            if ghost.screen != screen {
                ghost.screen = screen;
                let offset = sub(screen, ghost.painted);
                self.commit.offset(ghost_key(source), offset);
            }
        }
        if let Some(lift) = self.drag_drop.lift.as_deref()
            && lift.settle.is_none()
        {
            let node = lift.node;
            self.lift_to(node, screen);
        }
    }

    /// 影子的左上角想去窗口里的 `screen`：按这次拖动的限制改过的位置
    fn ghost_screen(&self, id: NodeId, screen: [f32; 2]) -> [f32; 2] {
        if self.drag_drop.session.as_deref().is_none_or(|s| s.axis.is_none() && !s.contained) {
            return screen;
        }
        let Some(base) = self.screen_origin(id, true) else { return screen };
        let parent = self.parent_world(id);
        let (Some(a), Some(b)) = (parent.invert(screen), parent.invert(base)) else { return screen };
        let [dx, dy] = self.constrain(id, b, sub(a, b));
        parent.apply([b[0] + dx, b[1] + dy])
    }

    /// 抬起的元素的左上角放到窗口里的 `screen`（按这次拖动的限制改过）：算出额外平移（父元素坐标里的），变了才发给渲染线程
    fn lift_to(&mut self, id: NodeId, screen: [f32; 2]) {
        let Some(base) = self.screen_origin(id, false) else { return };
        let parent = self.parent_world(id);
        let (Some(a), Some(b)) = (parent.invert(screen), parent.invert(base)) else { return };
        let shift = self.constrain(id, b, sub(a, b));
        self.set_shift(id, shift);
    }

    /// 按这次拖动的限制改影子、抬起的元素的位移 `shift`（父元素坐标）：只沿一个方向走（另一个方向不动），不出父元素。
    /// `origin` 是元素左上角此刻在父元素坐标里的位置
    fn constrain(&self, id: NodeId, origin: [f32; 2], mut shift: [f32; 2]) -> [f32; 2] {
        let Some(session) = self.drag_drop.session.as_deref() else { return shift };
        let moves = match session.axis {
            Some(Direction::Row) => [true, false],
            Some(Direction::Column) => [false, true],
            None => [true, true],
        };
        for axis in 0..2 {
            if !moves[axis] {
                shift[axis] = 0.0;
            }
        }
        let Some(node) = self.nodes.get(id) else { return shift };
        let Some(parent) = node.parent.and_then(|p| self.nodes.get(p)).filter(|_| session.contained) else { return shift };
        // 元素相对左上角占的范围：缩放、旋转过的按转过以后的外框
        let b = node.bounds;
        let local = Affine::IDENTITY.then_local(b.center(), [0.0; 2], node.paint.scale, node.paint.rotation);
        let corner = local.apply([b.x, b.y]);
        let corners = [[b.x, b.y], [b.x + b.width, b.y], [b.x, b.y + b.height], [b.x + b.width, b.y + b.height]].map(|p| sub(local.apply(p), corner));
        let p = parent.bounds;
        for axis in (0..2).filter(|&axis| moves[axis]) {
            let low = corners.iter().map(|c| c[axis]).fold(f32::INFINITY, f32::min);
            let high = corners.iter().map(|c| c[axis]).fold(f32::NEG_INFINITY, f32::max);
            let (start, end) = if axis == 0 { (p.x, p.x + p.width) } else { (p.y, p.y + p.height) };
            // 比父元素还大时在两头对齐之间走
            let (first, last) = (start - origin[axis] - low, end - origin[axis] - high);
            shift[axis] = shift[axis].clamp(first.min(last), first.max(last));
        }
        shift
    }

    fn set_shift(&mut self, id: NodeId, shift: [f32; 2]) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        if node.shift != shift {
            node.shift = shift;
            self.commit.offset(layer_key(id), shift);
            self.paint.mark_worlds();
        }
    }

    /// 松手了：影子等布局之后落到位（或飞回去），抬起的元素从此刻的平移落回 0
    fn end_preview(&mut self) {
        if let Some(ghost) = self.drag_drop.ghost.as_deref_mut() {
            ghost.phase = GhostPhase::Pending;
        }
        if let Some(lift) = self.drag_drop.lift.as_deref_mut()
            && lift.settle.is_none()
        {
            let shift = self.nodes.get(lift.node).map_or([0.0; 2], |n| n.shift);
            let mut spring = CurveTrack::new(crate::defaults::slide(), &shift, 0.25);
            spring.retarget(self.anim.now(), &[0.0, 0.0]);
            lift.settle = Some(spring);
        }
    }

    /// 立刻去掉影子、放下抬起的元素（开始新的拖动、交给系统、源元素被卸载）
    fn finish_preview(&mut self) {
        self.finish_preview_ghost();
        if let Some(lift) = self.drag_drop.lift.take() {
            self.drop_lift(&lift);
        }
    }

    fn drop_lift(&mut self, lift: &Lift) {
        self.set_shift(lift.node, [0.0; 2]);
        if let Some(node) = self.nodes.get_mut(lift.node) {
            if lift.forced {
                node.layer = false;
                node.refresh_flags();
            }
            if let Some(parent) = node.parent {
                self.sort_children(parent);
            }
        }
        self.paint.invalidate();
    }

    /// 拖着的数据离开 `target`：它的 `on_drag_leave` 和事件（位置是指针 `point` 在它上面的坐标）
    fn leave_calls(&self, target: NodeId, data: &Rc<dyn Any>, point: [f32; 2]) -> Vec<(On<DropEvent>, DropEvent)> {
        let handlers = self.handlers_of(target, EventType::DragLeave, Handler::drop_event);
        if handlers.is_empty() {
            return Vec::new();
        }
        let event = DropEvent { position: self.local_point(target, point), window_position: point, modifiers: self.router.modifiers(), data: data.clone() };
        handlers.into_iter().map(|h| (h, event.clone())).collect()
    }

    /// 在 `target`（还在、没在退场）上放下 `data`：它类型对得上的 `on_drop` 和 `on_drag_leave`，指针在 `point`
    fn drop_calls(&self, target: Option<NodeId>, data: &Rc<dyn Any>, point: [f32; 2]) -> Option<DropCalls> {
        let id = target.filter(|&id| self.nodes.get(id).is_some_and(|n| !n.exiting))?;
        let event = DropEvent { position: self.local_point(id, point), window_position: point, modifiers: self.router.modifiers(), data: data.clone() };
        let ty = (*event.data).type_id();
        let drops: Vec<_> = self
            .handlers(id)
            .map(|h| {
                h.entries(EventType::Drop)
                    .filter_map(|e| match &e.handler {
                        Handler::TypedDrop(t, f) if *t == ty => Some(f.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some((drops, self.handlers_of(id, EventType::DragLeave, Handler::drop_event), event))
    }

    /// 指针在窗口外面
    fn outside(&self, point: [f32; 2]) -> bool {
        let [w, h] = self.layout.viewport();
        point[0] < 0.0 || point[1] < 0.0 || point[0] >= w || point[1] >= h
    }

    /// 正在元素之间拖放、指针出了窗口：指针的位置（窗口坐标）。窗口宿主问这里是不是同一个应用的另一个窗口
    pub(crate) fn drag_outside(&self) -> Option<[f32; 2]> {
        self.drag_drop.session.as_ref()?;
        self.router.pointer().filter(|&point| self.outside(point))
    }

    /// 节点开始退场或被卸载（在借用树时）：源元素没了就结束拖放（目标的离开处理器留到这一帧结束），
    /// 目标没了就不再是目标
    pub(super) fn forget_drag_drop(&mut self, leave: Leave) {
        let gone = |t: &Tree, id: NodeId| leave.gone(&t.nodes, id);
        for visit in [false, true] {
            let target = self.drag_drop.hover_of(visit).and_then(|h| h.target);
            if target.is_some_and(|t| gone(self, t))
                && let Some(hover) = self.drag_drop.hover_of(visit)
            {
                *hover = Hover::default();
            }
        }
        if let Some(session) = self.drag_drop.session.as_deref()
            && gone(self, session.source)
        {
            let point = self.router.pointer().unwrap_or_default();
            let leave = session.hover.target.map(|id| self.leave_calls(id, &session.data, point)).unwrap_or_default();
            self.drag_drop.deferred.extend(leave);
            self.drag_drop.session = None;
            self.router.set_drag(None);
        }
        // 影子是源元素再画一遍：源元素没了（比如被接住后数据挪到了别处）影子也就没了，新位置上的元素直接出现
        if self.drag_drop.ghost.as_ref().is_some_and(|g| gone(self, g.source)) {
            self.finish_preview_ghost();
        }
        if self.drag_drop.lift.as_ref().is_some_and(|l| gone(self, l.node)) {
            self.drag_drop.lift = None;
        }
        if self.drag_drop.out_source.as_ref().is_some_and(|(id, _)| gone(self, *id)) {
            self.drag_drop.out_source = None;
        }
        if self.drag_drop.out.as_ref().is_some_and(|o| gone(self, o.node)) {
            self.drag_drop.out = None;
        }
    }

    /// 窗口：要交给系统的拖到应用外面（取走；之后系统的结果交给这个源元素）
    pub(crate) fn take_drag_out(&mut self) -> Option<DragOutRequest> {
        let request = self.drag_drop.out.take()?;
        self.drag_drop.out_source = Some((request.node, request.result.clone()));
        Some(request)
    }

    /// 每帧布局之前：影子落到位、淡出的动画在这一帧的动画时间 `t` 采样，结束了就去掉（这一帧重画，源元素恢复）
    pub(super) fn step_drag_preview(&mut self, t: f64) {
        let Some(ghost) = self.drag_drop.ghost.as_deref_mut() else { return };
        let done = match &mut ghost.phase {
            GhostPhase::Dragging | GhostPhase::Pending => false,
            GhostPhase::Settling(s) => !s.sample(t),
        };
        if done {
            self.finish_preview_ghost();
        }
    }

    fn finish_preview_ghost(&mut self) {
        if let Some(ghost) = self.drag_drop.ghost.take() {
            self.commit.remove_all([ghost_key(ghost.source), shadow_key(ghost.source)]);
            if ghost.forced
                && let Some(node) = self.nodes.get_mut(ghost.source)
            {
                node.layer = false;
                node.refresh_flags();
            }
            self.paint.invalidate();
        }
    }

    /// 生成显示列表之后：源元素第一遍画出的层透明度乘上 [`SOURCE_DIM`]（影子里再画的那一层不变）
    #[cold]
    #[inline(never)]
    pub(super) fn dim_source(&self, source: NodeId, layers: &mut [LayerInfo]) {
        if let Some(desc) = layers.iter_mut().find(|d| d.key == layer_key(source)) {
            desc.base_opacity *= SOURCE_DIM;
        }
    }

    /// 每帧画完之后（布局位置都是新的）：影子决定落向哪里；抬起的元素跟着指针，或者在这一帧的动画时间 `t` 落到位
    pub(super) fn place_drag_preview(&mut self, t: f64) {
        if self.drag_drop.ghost.as_ref().is_some_and(|g| matches!(g.phase, GhostPhase::Pending)) {
            self.settle_ghost();
        }
        let Some(lift) = self.drag_drop.lift.as_deref() else { return };
        let id = lift.node;
        let Some(node) = self.nodes.get(id) else {
            self.drag_drop.lift = None;
            return;
        };
        let origin = [node.bounds.x, node.bounds.y];
        let shift = node.shift;
        let lift = self.drag_drop.lift.as_deref_mut().expect("上面检查过");
        // 布局位置变了（数据重排）：平移换算到新位置上，画面不跳
        let moved = sub(lift.origin, origin);
        lift.origin = origin;
        match &mut lift.settle {
            None => {
                if let (Some(point), Some(grab)) = (self.router.pointer(), self.drag_drop.session.as_ref().map(|s| s.grab)) {
                    self.lift_to(id, sub(point, grab));
                }
            }
            Some(spring) => {
                if moved != [0.0; 2] {
                    // 平移挪到新的位置上，保留速度接着落
                    spring.shift(t, &moved);
                } else if shift == [0.0; 2] && !spring.is_moving(t) {
                    let lift = self.drag_drop.lift.take().expect("上面检查过");
                    self.drop_lift(&lift);
                    return;
                }
                let mut value = [0.0; MAX_COMPONENTS];
                spring.sample(t, &mut value);
                self.set_shift(id, [value[0], value[1]]);
            }
        }
    }

    /// 松手后（布局、重画之后）：源元素还在就把影子落到它此刻的位置（被接住时是新位置，取消时是原来的位置），
    /// 透明度到 1、投影淡出，正好盖住它再去掉；源元素没了（正在退场）就直接去掉。动画交给渲染线程，主线程也推进一份
    fn settle_ghost(&mut self) {
        let Some(ghost) = self.drag_drop.ghost.as_deref() else { return };
        if !matches!(ghost.phase, GhostPhase::Pending) {
            return;
        }
        let source = ghost.source;
        let alive = self.nodes.get(source).is_some_and(|n| !n.exiting);
        let slide = crate::defaults::slide();
        let now = self.anim.now();
        let target = alive.then(|| self.screen_origin(source, true)).flatten();
        let key = ghost_key(source);
        let ghost = self.drag_drop.ghost.as_deref_mut().expect("上面检查过");
        let mut updates = Vec::new();
        match target {
            Some(target) => {
                let [dx, dy] = sub(target, ghost.screen);
                let animate = |from: f32, to: f32| {
                    let mut track = CurveTrack::new(slide, &[from], 0.01);
                    track.retarget(now, &[to]);
                    track
                };
                let tracks = [animate(0.0, dx), animate(0.0, dy), animate(GHOST_OPACITY, 1.0), animate(1.0, 0.0)];
                let properties = [PropId::TRANSLATION_X, PropId::TRANSLATION_Y, PropId::OPACITY, PropId::OPACITY];
                for (i, (track, property)) in tracks.iter().zip(properties).enumerate() {
                    let key = if i == 3 { shadow_key(source) } else { key };
                    updates.push(LayerTrack { key, property, track: track.clone() });
                }
                let mut settle = Settle { tracks, values: [0.0; 4] };
                settle.sample(now);
                ghost.phase = GhostPhase::Settling(Box::new(settle));
            }
            None => {
                self.finish_preview_ghost();
                return;
            }
        }
        self.commit.tracks(updates);
    }

    /// 生成显示列表：影子的合成层（偏移跟着指针）、投影（单独一层，落到位时淡出），返回在影子层里再画一遍源元素的起点。
    /// 源元素的屏幕变换（父元素的部分）烘焙进影子的图元，所以影子从它此刻的样子出发
    #[cold]
    #[inline(never)]
    pub(super) fn begin_ghost(&mut self, id: NodeId, list: &mut DisplayList, layers: &mut Vec<LayerInfo>) -> Option<Frame> {
        let node = self.nodes.get(id)?;
        let parent_world = node.parent.and_then(|p| self.nodes.get(p)).map_or(Affine::IDENTITY, |parent| parent.world);
        let b = node.bounds;
        let painted = node.world().apply([b.x, b.y]);
        let radius = node.paint.radius;
        let ghost = self.drag_drop.ghost.as_deref_mut()?;
        ghost.painted = painted;
        let values = ghost.values();
        let shadow_opacity = ghost.shadow();
        let center = [painted[0] + b.width * 0.5, painted[1] + b.height * 0.5];
        let base = |key, parent, values| LayerInfo { key, parent, base: Affine::IDENTITY, base_opacity: 1.0, center, values, group: false, depth: None };
        layers.push(base(ghost_key(id), 0, values));
        let layer = layers.len() as u32;
        let identity = LayerValues { translate: [0.0; 2], scale: 1.0, rotation: 0.0, opacity: shadow_opacity, offset: [0.0; 2] };
        layers.push(base(shadow_key(id), layer, identity));
        // 投影：和浮层一样很淡的一层，形状是源元素的圆角矩形
        let shadow = crate::defaults::OVERLAY_SHADOW;
        let mut quad = Quad::new(painted[0], painted[1], b.width, b.height)
            .radius(radius)
            .fill(scene::Color::TRANSPARENT)
            .shadow(shadow.x, shadow.y, shadow.blur, shadow.color);
        quad.layer = layers.len() as u32;
        list.quads.push(quad);
        Some(Frame { local: parent_world, opacity: 1.0, layer, clip: 0 })
    }

    /// 拖放中每帧（滚动之前）：指针靠近指针下滚动视口的边缘时自动滚动，越近越快；出了窗口按贴着边缘算
    pub(super) fn auto_scroll(&mut self, now: f64) {
        if self.drag_drop.session.is_none() {
            return;
        }
        let Some(point) = self.router.pointer() else { return };
        let dt = self.anim.last_host().map_or(0.0, |last| (now - last).max(0.0) as f32).min(0.05);
        if dt <= 0.0 {
            return;
        }
        let [w, h] = self.layout.viewport();
        let inside = [point[0].clamp(0.0, (w - 1.0).max(0.0)), point[1].clamp(0.0, (h - 1.0).max(0.0))];
        self.refresh_worlds();
        let path = self.hit_path(inside);
        let mut done = [false; 2];
        for &id in path.iter().rev() {
            let Some(node) = self.nodes.get(id) else { continue };
            let Some(s) = node.scroll.as_deref() else { continue };
            let Some(local) = node.world_invert(point) else { continue };
            let b = node.bounds;
            let (start, size) = ([b.x, b.y], [b.width, b.height]);
            let mut deltas = [0.0f32; 2];
            for axis in 0..2 {
                if done[axis] || !s.axes[axis] {
                    continue;
                }
                let before = local[axis] - start[axis];
                let after = start[axis] + size[axis] - local[axis];
                let zone = EDGE.min(size[axis] * 0.25);
                let speed = |d: f32| EDGE_SPEED * ((zone - d) / zone).clamp(0.0, 1.0).powi(2);
                if before < zone && s.can_scroll(axis, -1.0) {
                    deltas[axis] = -speed(before) * dt;
                } else if after < zone && s.can_scroll(axis, 1.0) {
                    deltas[axis] = speed(after) * dt;
                }
            }
            if deltas == [0.0; 2] {
                continue;
            }
            if let Some(s) = self.scroller_mut(id) {
                for (axis, &delta) in deltas.iter().enumerate() {
                    if delta != 0.0 {
                        s.nudge(axis, delta);
                        done[axis] = true;
                    }
                }
            }
            self.wake(id);
            if done == [true; 2] {
                break;
            }
        }
    }
}
