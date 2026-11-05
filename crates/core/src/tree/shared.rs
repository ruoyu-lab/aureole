//! 共享元素（[`crate::Element::shared`]）：同一个窗口里带着同一个标签的元素是"同一个东西"。
//!
//! 每个标签记着挂着它的元素（按挂载顺序）和正在显示的那个。最后挂载的显示，其余的藏起来。
//! 显示的元素换了（新的挂载、显示着的离开）时，新的从旧的那个最后在屏幕上的样子飞过来：
//!
//! - 离开（卸载、自己或祖先开始退场）时按此刻的变换、滚动、合成层的值记下它在屏幕上的外接矩形和变化速度；
//!   还挂着、只是被接替的旧元素在决定的那一刻量
//! - 每帧画之前决定每个有变化的标签显示谁（[`Tree::resolve_shared`]），新的元素第一次画的时候
//!   （这时才知道它的位置）开始飞（[`Tree::start_flight`]）
//! - 飞行是层在屏幕上额外的平移和缩放（`compositor::fly`），一条轨道整份交给渲染线程，两条线程按同一个时刻采样；
//!   飞的元素不受祖先的裁剪，不继承祖先的透明度，画在别的东西上面：起飞时已经在的都在它下面；
//!   它起飞之后才挂上、画在它后面的子树（新打开的详情页、浮层）盖住它（[`Tree::flight_covers`]），
//!   所以飞回网格的缩略图不会盖住紧接着打开的详情页和新的大图
//! - 系统打开"减少动态效果"时不飞：新的淡入，还挂着的旧元素淡出后再藏起来

use std::collections::HashMap;
use std::rc::Rc;

use animation::{Curve, CurveTrack, MAX_COMPONENTS, TrackState, Spring, Track};

use super::{NodeId, Tree, layer_key, is_motion_reduced};
use crate::compositor::{FLIGHT_IDENTITY, FLIGHT_TOLERANCE, flight_components, flight_rate, flight_value, flight_velocity};
use crate::geometry::{Transform, bounding};
use crate::geometry::Rect;

/// 飞行默认的曲线：临界阻尼（不回弹），约 0.35 秒到位
pub(crate) const FLIGHT_SPRING: Spring = Spring::new(std::time::Duration::from_millis(400), 1.0);

/// 节点上共享元素的状态
pub(crate) struct SharedNode {
    pub tag: Rc<str>,
    pub curve: Curve,
    /// 同一个标签有更新的元素在显示：不画、不接收指针
    pub hidden: bool,
    /// 刚接替了旧元素、还没画过：画的时候从这里飞过来
    pending: Option<Departure>,
    /// 飞行（或者淡入淡出）的轨道和这一帧采样的值，分量见 `compositor::flight_components`
    pub flight: Option<Flight>,
    /// 飞的时候画在最上面、不受祖先的裁剪（淡入淡出时不用）
    pub lift: bool,
    /// 动画停下以后藏起来（减少动态效果时被接替的旧元素先淡出）
    hide_after: bool,
    /// 第几次决定（[`Shared::frame`]）时接替的：之后才挂上的子树画在它上面时盖住它
    since: u64,
}

/// 一次飞行：轨道（渲染线程有同样的一份），和主线程这一帧采样的分量
pub(crate) struct Flight {
    track: CurveTrack,
    value: [f32; 5],
}

impl Flight {
    fn new(track: CurveTrack, t: f64) -> (Self, bool) {
        let mut flight = Self { track, value: [0.0; 5] };
        let moving = flight.sample(t);
        (flight, moving)
    }

    /// 在动画时间 `t` 采样，返回还在不在动
    fn sample(&mut self, t: f64) -> bool {
        let mut out = [0.0; MAX_COMPONENTS];
        let motion = self.track.sample(t, &mut out);
        self.value.copy_from_slice(&out[..5]);
        motion == TrackState::Moving
    }

    /// 此刻在屏幕上额外的平移、缩放、透明度
    pub fn value(&self) -> [f32; 5] {
        flight_value(&self.value)
    }
}

/// 显示着的元素离开（或被接替）时在屏幕上的样子
#[derive(Clone, Copy, Debug)]
pub(crate) struct Departure {
    /// 离开的元素（开始退场时还挂着）
    node: NodeId,
    /// 外接矩形 `[x0, y0, x1, y1]`
    rect: [f32; 4],
    /// 中心 (x, y) 和宽高每秒的变化：飞到一半被打断时接着这个速度
    velocity: [f32; 4],
    /// 第几帧（[`Shared::frame`]）记下的
    frame: u64,
    /// 记下时的动画时间：位置和速度是这一刻的，接着飞的轨道从这一刻起算
    time: f64,
}

/// 一个窗口里所有的共享标签
#[derive(Default)]
pub(crate) struct Shared {
    tags: HashMap<Rc<str>, Tag>,
    /// 有变化、下一帧要重新决定显示谁的标签
    dirty: Vec<Rc<str>>,
    /// 留着离开记录的标签（过了下一帧就作废）
    departed: Vec<Rc<str>>,
    /// 正在飞（或淡入淡出）的元素
    flights: Vec<NodeId>,
    /// 有元素在飞的时候新挂上的子树的根（动态子元素、浮层），和挂上时的 `frame`
    mounted: Vec<(NodeId, u64)>,
    /// 决定过几次（每帧一次）
    frame: u64,
}

#[derive(Default)]
struct Tag {
    /// 挂着这个标签的元素，按挂载顺序；最后一个应该显示
    members: Vec<NodeId>,
    shown: Option<NodeId>,
    /// 显示着的元素离开了、还没有接替的
    departed: Option<Departure>,
}

impl Shared {
    /// 有元素在飞：每帧都要推进
    #[inline]
    pub fn flying(&self) -> usize {
        self.flights.len()
    }

    fn touch(&mut self, tag: &Rc<str>) {
        if !self.dirty.contains(tag) {
            self.dirty.push(tag.clone());
        }
    }

    /// 新挂上了一棵子树：有元素在飞时记下来（画在飞的元素后面时盖住它），没有时什么都不做
    #[inline]
    pub fn mounted(&mut self, roots: impl IntoIterator<Item = NodeId>) {
        if !self.flights.is_empty() {
            let frame = self.frame;
            self.mounted.extend(roots.into_iter().map(|root| (root, frame)));
        }
    }
}

impl Tree {
    /// 属性 `shared`：换到这个标签下（空的是不参加）；和原来的一样时什么都不做
    pub(super) fn set_shared(&mut self, id: NodeId, tag: &str) {
        let current = self.nodes.get(id).and_then(|n| n.extra.as_deref()).and_then(|e| e.shared.as_ref()).map(|s| s.tag.clone());
        if current.as_deref() == Some(tag) || (current.is_none() && tag.is_empty()) {
            return;
        }
        if let Some(old) = current {
            if let Some(entry) = self.shared.tags.get_mut(&old) {
                entry.members.retain(|&m| m != id);
                if entry.shown == Some(id) {
                    entry.shown = None;
                }
            }
            self.shared.touch(&old);
            if let Some(extra) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()) {
                extra.shared = None;
            }
        }
        if !tag.is_empty() {
            self.register_shared(id, Rc::from(tag), None);
        }
    }

    /// 属性 `shared_curve`：飞行的曲线
    pub(super) fn set_shared_curve(&mut self, id: NodeId, curve: Curve) {
        if let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) {
            shared.curve = curve;
        }
    }

    /// 挂载时登记标签
    pub(super) fn register_shared(&mut self, id: NodeId, tag: Rc<str>, curve: Option<Curve>) {
        let shared = SharedNode {
            tag: tag.clone(),
            curve: curve.unwrap_or(FLIGHT_SPRING.into()),
            hidden: false,
            pending: None,
            flight: None,
            lift: false,
            hide_after: false,
            since: 0,
        };
        let Some(node) = self.nodes.get_mut(id) else { return };
        node.extra.get_or_insert_with(Default::default).shared = Some(shared);
        node.refresh_flags();
        self.shared.tags.entry(tag.clone()).or_default().members.push(id);
        self.shared.touch(&tag);
    }

    /// `id` 连同子孙要离开了（卸载、开始退场）：带标签的从标签里拿掉，正在显示的记下它此刻在屏幕上的样子。
    /// 要在卸载之前调用（量位置要用到完整的父节点链）
    pub(super) fn depart_subtree(&mut self, id: NodeId) {
        if self.shared.tags.is_empty() {
            return;
        }
        let mut stack = vec![id];
        let mut leaving = Vec::new();
        while let Some(n) = stack.pop() {
            let Some(node) = self.nodes.get(n) else { continue };
            stack.extend(node.children.iter().copied());
            if let Some(input) = &node.input {
                stack.extend(input.overlays.iter().copied());
            }
            if let Some(shared) = node.extra.as_deref().and_then(|e| e.shared.as_ref()) {
                leaving.push((n, shared.tag.clone()));
            }
        }
        for (n, tag) in leaving {
            let Some(entry) = self.shared.tags.get(&tag) else { continue };
            if !entry.members.contains(&n) {
                continue;
            }
            let departure = if entry.shown == Some(n) { self.capture_shared(n) } else { None };
            let entry = self.shared.tags.get_mut(&tag).expect("刚找到");
            entry.members.retain(|&m| m != n);
            if entry.shown == Some(n) {
                entry.shown = None;
                entry.departed = departure;
                if departure.is_some() && !self.shared.departed.contains(&tag) {
                    self.shared.departed.push(tag.clone());
                }
            }
            self.shared.touch(&tag);
        }
    }

    /// 元素此刻在屏幕上的样子（外接矩形、飞到一半时的速度）
    fn capture_shared(&self, id: NodeId) -> Option<Departure> {
        let node = self.nodes.get(id)?;
        let b = node.bounds;
        let rect = bounding(&self.screen_transform(id)?.corners(b.x, b.y, b.width, b.height)?);
        let mut velocity = [0.0; 4];
        if let Some(flight) = node.extra.as_deref().and_then(|e| e.shared.as_ref()).and_then(|s| s.flight.as_ref()) {
            let mut v = [0.0; MAX_COMPONENTS];
            flight.track.velocity(self.anim.now(), &mut v);
            let [vx, vy, vsx, vsy, _] = flight_rate(&v[..5]);
            let [_, _, sx, sy, _] = flight.value();
            // 宽高的变化 = 不飞时的宽高 × 缩放的变化
            let size = [(rect[2] - rect[0]) / sx.max(1e-3), (rect[3] - rect[1]) / sy.max(1e-3)];
            velocity = [vx, vy, size[0] * vsx, size[1] * vsy];
        }
        Some(Departure { node: id, rect, velocity, frame: self.shared.frame, time: self.anim.now() })
    }

    /// 每帧画之前：有变化的标签重新决定显示谁。新显示的元素记下从哪里飞过来（第一次画时开始飞），
    /// 被接替、还挂着的旧元素藏起来；过了下一帧还没人接的离开记录作废
    pub(super) fn resolve_shared(&mut self) {
        self.shared.frame += 1;
        if !self.shared.mounted.is_empty() {
            self.prune_mounted();
        }
        if self.shared.dirty.is_empty() && self.shared.departed.is_empty() {
            return;
        }
        for tag in std::mem::take(&mut self.shared.dirty) {
            let Some(entry) = self.shared.tags.get(&tag) else { continue };
            let next = entry.members.last().copied();
            if next == entry.shown {
                continue;
            }
            let old = entry.shown;
            let source = match old {
                Some(old) => self.capture_shared(old),
                None if next.is_some() => entry.departed,
                None => None,
            };
            // 被接替的旧元素藏起来：还挂着的（被新的盖过），或者正在退场的（随祖先淡出的那一份不再画）
            let leaving = old.or(source.filter(|_| next.is_some()).map(|d| d.node)).filter(|&n| self.nodes.contains_key(n));
            if let Some(old) = leaving {
                self.retire(old);
            }
            if let Some(new) = next {
                self.reveal_shared(new, source);
            }
            let entry = self.shared.tags.get_mut(&tag).expect("刚找到");
            entry.shown = next;
            if next.is_some() {
                entry.departed = None;
            }
            self.paint.invalidate();
        }
        let frame = self.shared.frame;
        let tags = &mut self.shared.tags;
        self.shared.departed.retain(|tag| {
            let Some(entry) = tags.get_mut(tag) else { return false };
            if entry.departed.is_some_and(|d| frame > d.frame + 1) {
                entry.departed = None;
            }
            entry.departed.is_some()
        });
        tags.retain(|_, t| !t.members.is_empty() || t.departed.is_some());
    }

    /// 被接替、还挂着的旧元素：藏起来；减少动态效果时先淡出
    fn retire(&mut self, id: NodeId) {
        let reduced = is_motion_reduced();
        let fade = crate::defaults::FADE;
        let now = self.anim.now();
        self.accessibility_touch_parent(id);
        let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) else { return };
        shared.pending = None;
        if !reduced {
            shared.hidden = true;
            if shared.flight.take().is_some() {
                // 渲染线程那边的飞行也停下：以后它不飞地重新出现时，不能接着旧的飞行
                let stopped = CurveTrack::new(shared.curve, &flight_components(FLIGHT_IDENTITY), FLIGHT_TOLERANCE);
                self.commit.fly(layer_key(id), stopped);
            }
            return;
        }
        let from = shared.flight.as_ref().map_or(FLIGHT_IDENTITY, Flight::value);
        let mut flight = CurveTrack::new(fade, &flight_components(from), FLIGHT_TOLERANCE);
        flight.retarget(now, &flight_components([0.0, 0.0, 1.0, 1.0, 0.0]));
        shared.lift = false;
        shared.hide_after = true;
        self.begin_flight(id, flight);
    }

    /// 接替的新元素：显示出来，记下从哪里飞过来
    fn reveal_shared(&mut self, id: NodeId, source: Option<Departure>) {
        let frame = self.shared.frame;
        let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) else { return };
        shared.hidden = false;
        shared.hide_after = false;
        shared.pending = source;
        shared.since = frame;
        self.accessibility_touch_parent(id);
    }

    /// 画到一个刚接替旧元素的共享元素：`target` 是它此刻（不算飞行）到屏幕的变换。开始飞，
    /// 返回是不是开始了（旧的、新的有一个看不见时不飞）
    pub(super) fn start_flight(&mut self, id: NodeId, target: &Transform, bounds: Rect) -> bool {
        let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) else {
            return false;
        };
        let Some(departure) = shared.pending.take() else { return false };
        let Some(corners) = target.corners(bounds.x, bounds.y, bounds.width, bounds.height) else { return false };
        let to = bounding(&corners);
        let from = departure.rect;
        let size = [to[2] - to[0], to[3] - to[1]];
        let source = [from[2] - from[0], from[3] - from[1]];
        if size.iter().chain(&source).any(|&v| v < 0.5) {
            return false;
        }
        let reduced = is_motion_reduced();
        let now = self.anim.now();
        // 飞：从离开的那一刻（位置和速度是那一刻量的）起算，这一帧已经接着那时的速度走了一段，不在原地停一帧；
        // 淡入从此刻起
        let start = if reduced { now } else { departure.time.min(now) };
        let (value, rate, curve) = if reduced {
            // 不飞：在自己的位置淡入
            ([0.0, 0.0, 1.0, 1.0, 0.0], [0.0; 5], crate::defaults::FADE.into())
        } else {
            // 屏幕上绕框的中心（投影后）缩放，再平移：缩放后外接矩形的中心正好落在旧元素的中心上
            let middle = target.apply(bounds.center());
            let scale = [source[0] / size[0], source[1] / size[1]];
            let center = |r: [f32; 4], axis: usize| (r[axis] + r[axis + 2]) * 0.5;
            let shift = |axis: usize| center(from, axis) - middle[axis] - scale[axis] * (center(to, axis) - middle[axis]);
            let v = departure.velocity;
            ([shift(0), shift(1), scale[0], scale[1], 1.0], [v[0], v[1], v[2] / size[0], v[3] / size[1], 0.0], shared.curve)
        };
        // 从旧元素的样子带着它的速度出发，落到不飞（自己的位置）
        let mut flight = CurveTrack::new(curve, &flight_components(FLIGHT_IDENTITY), FLIGHT_TOLERANCE);
        flight.restart(start, &flight_components(value), &flight_velocity(rate));
        if !flight.is_moving(now) {
            return false;
        }
        shared.lift = !reduced;
        self.begin_flight(id, flight);
        true
    }

    /// 开始飞：轨道整份交给渲染线程，主线程这份在此刻采样（两边按同一个时刻采样，这一帧都在出发的地方）
    fn begin_flight(&mut self, id: NodeId, flight: CurveTrack) {
        self.commit.fly(layer_key(id), flight.clone());
        let (flight, _) = Flight::new(flight, self.anim.now());
        let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) else { return };
        shared.flight = Some(flight);
        if !self.shared.flights.contains(&id) {
            self.shared.flights.push(id);
        }
        self.paint.mark_worlds();
    }

    /// 飞行在这一帧的动画时间 `t` 采样：停下的不再是合成层（重画一次，回到原来的位置和裁剪里）
    pub(super) fn step_flights(&mut self, t: f64) {
        if self.shared.flights.is_empty() {
            return;
        }
        let mut flights = std::mem::take(&mut self.shared.flights);
        flights.retain(|&id| {
            let Some(shared) = self.nodes.get_mut(id).and_then(|n| n.extra.as_deref_mut()).and_then(|e| e.shared.as_mut()) else {
                return false;
            };
            let Some(flight) = &mut shared.flight else { return false };
            self.paint.mark_worlds();
            if flight.sample(t) {
                return true;
            }
            shared.flight = None;
            if std::mem::take(&mut shared.hide_after) {
                shared.hidden = true;
                self.accessibility_touch_parent(id);
            }
            self.paint.invalidate();
            self.accessibility.touch(id);
            false
        });
        flights.append(&mut self.shared.flights);
        self.shared.flights = flights;
    }

    /// 飞着的元素起飞之前挂上的子树记录不再有用：只留最早起飞的那个之后的；都落地了就清空
    fn prune_mounted(&mut self) {
        let nodes = &self.nodes;
        let since = |id: &NodeId| nodes.get(*id)?.extra.as_deref()?.shared.as_ref().map(|s| s.since);
        let oldest = self.shared.flights.iter().filter_map(since).min();
        match oldest {
            Some(oldest) => self.shared.mounted.retain(|&(root, frame)| frame >= oldest && nodes.contains_key(root)),
            None => self.shared.mounted.clear(),
        }
    }

    /// 飞着、画在最上面的元素被谁盖住：它接替旧元素之后才挂上的子树（新打开的详情页、浮层），
    /// 不是它的祖先，也不在它所在的滚动视口里（视口里新挂上的行不盖住飞回来的缩略图）。
    /// 返回（盖住它的子树的根，飞的元素）；绘制时画到那棵子树之前先画飞的元素。平时是空的
    pub(super) fn flight_covers(&self) -> Vec<(NodeId, NodeId)> {
        let mut covers = Vec::new();
        if self.shared.mounted.is_empty() {
            return covers;
        }
        for &id in &self.shared.flights {
            let Some(shared) = self.nodes.get(id).and_then(|n| n.extra.as_deref()).and_then(|e| e.shared.as_ref()) else { continue };
            if !shared.lift {
                continue;
            }
            // 它所在的滚动视口（最近的）
            let mut viewport = self.nodes[id].parent;
            while let Some(v) = viewport.filter(|&v| self.nodes[v].scroll.is_none()) {
                viewport = self.nodes[v].parent;
            }
            for &(root, frame) in &self.shared.mounted {
                if frame >= shared.since
                    && self.nodes.contains_key(root)
                    && !self.nodes.is_within(id, root)
                    && viewport.is_none_or(|v| !self.nodes.is_within(root, v))
                {
                    covers.push((root, id));
                }
            }
        }
        covers
    }

    /// 刚接替旧元素、这一帧画的时候要开始飞
    #[inline]
    pub(super) fn flight_pending(&self, id: NodeId) -> bool {
        self.nodes[id].extra.as_deref().and_then(|e| e.shared.as_ref()).is_some_and(|s| s.pending.is_some())
    }
}
