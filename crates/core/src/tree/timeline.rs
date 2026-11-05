//! 元素树这一侧的关键帧时间线：句柄有哪些元素、按元素排期、接管和交还属性、每帧写值、
//! 把合成层上的轨道交给渲染线程。时间线的定义和播放头在 [`crate::timeline`] 的登记表里。
//!
//! - **接管**：时间线开始时，它动的每个（元素，属性）记下绑定的值，停掉这个属性进行中的过渡，
//!   之后显示时间线算出的值。动变换、透明度的元素变成合成层（如果还不是）。
//! - **交还**：绑定的值变了（和接管时记下的不一样）时只交还这一个属性：声明了过渡的从此刻的值和速度接着过渡，
//!   没声明的直接跳到新值；时间线停止、被释放时交还全部，按声明的过渡回到绑定的值。
//! - **按时刻采样**：每个（元素，属性）是一个 [`KeyframePlayback`]（时间线上的几段关键帧加上播放头），值是动画时钟时刻的
//!   纯函数。只动合成层变换、透明度的连同播放头交给渲染线程，两边按同一个时刻采样；主线程每帧只看播放头走没走到头，
//!   命中测试、生成图元、退场动画要用到时才算一次（[`Tree::sync_timeline_layers`]）。

use std::collections::HashMap;

use slotmap::SecondaryMap;
use std::mem;
use std::sync::Arc;

use animation::{CurveTrack, KeyframePlacement, KeyframePlayback, MAX_COMPONENTS, Track};
use taffy::{Dimension, LengthPercentage, Size, Style};

use super::{NodeId, Tree, TreeRef, layer_key};
use crate::property::PropertyValue;
use crate::property::registry::{PropId, PropSet};
use crate::compositor::{LayerTarget, LayerTimeline, LayerTrack, is_compositor};
use crate::gradient::Fill;
use crate::layout::Length;
use crate::timeline::{Entry, Events, Placed, is_movement, with_registry};

/// 节点 `motion` 的标记：它是某个 [`crate::MotionHandle`] 的成员
pub(super) const MEMBER: u8 = 1;
/// 节点 `motion` 的标记：有属性被时间线接管着（是哪几个记在 [`Host`] 的 `taken` 里，按属性编号的集合，不限个数）
pub(super) const TAKEN: u8 = 1 << 1;

/// 属性被接管之前绑定的值，交还时回到它
#[derive(Clone, Debug, PartialEq)]
enum Base {
    Value(PropertyValue),
    /// 文字颜色：元素自己设的（`None` 是继承来的）
    Color(Option<Fill>),
    Size(Dimension),
    Padding(taffy::Rect<LengthPercentage>),
    Gap(Size<LengthPercentage>),
}

/// 时间线动的一个（元素，属性）
struct Target {
    node: NodeId,
    property: PropId,
    base: Base,
    /// 时间线里动它的各段（按定义的顺序）加上播放头；减少动态效果时移动、缩放直接是最后的值
    playback: KeyframePlayback,
    /// 合成层的变换、透明度：渲染线程按同一个时刻采样，主线程用到时才算
    layer: bool,
}

/// 一条时间线落在这个窗口里的部分
struct Bound {
    generation: u64,
    version: u64,
    placed: Vec<Placed>,
    /// 句柄的成员在这条时间线里的序号：开始时按界面里的先后排，之后加入的接在后面（不超过开始时的最大序号，
    /// 排好的时间不会变）
    indices: HashMap<(u64, NodeId), f64>,
    limits: HashMap<u64, f64>,
    targets: Vec<Target>,
    /// 上次写值时播放头的位置
    sampled: Option<f64>,
    /// 渲染线程有这条时间线（停掉时要告诉它）
    on_render: bool,
    /// 合成层上的轨道少了（被别的时间线拿走、被交还）：要重发给渲染线程
    resend: bool,
    /// 不在合成层上的目标的下标（每帧主线程要写值的只有这些），和有没有合成层上的目标；目标增减时作废，用到时再分
    split: Option<(Vec<u32>, bool)>,
}

/// 不在合成层上的目标的下标，和有没有合成层上的目标
fn split(targets: &[Target]) -> (Vec<u32>, bool) {
    let plain = targets.iter().enumerate().filter(|(_, t)| !t.layer).map(|(i, _)| i as u32).collect();
    (plain, targets.iter().any(|t| t.layer))
}

/// 树上的时间线状态
pub(crate) struct Host {
    /// 这棵树的编号：推进播放头的窗口记的是它
    id: u64,
    /// 句柄 → 成员（挂载的先后）
    members: HashMap<u64, Vec<NodeId>>,
    /// 成员有变化的句柄
    changed: Vec<u64>,
    /// 卸载了的成员：下次同步时一起从句柄和时间线里去掉（一次清掉一大批时不用逐个查找）
    removed: Vec<NodeId>,
    bound: HashMap<u64, Bound>,
    /// 看过的登记表变化次数
    seen: u64,
    /// 合成层上的值主线程还没算
    layers_stale: bool,
    /// 放完的处理器、进度信号，帧结束后在借用之外执行
    events: Events,
    /// 每个节点被时间线接管着的属性（节点上只有一个 `TAKEN` 标记，绘制遍历先看标记）。
    /// 按节点的下标存：开始播放时一万个元素各记一次，比散列表快
    taken: SecondaryMap<NodeId, PropSet>,
}

impl Host {
    /// 节点的这个属性被时间线接管着（先看节点上的 `TAKEN` 标记再来查）
    pub(super) fn is_taken(&self, id: NodeId, property: PropId) -> bool {
        self.taken.get(id).is_some_and(|set| set.contains(property))
    }
}

impl Default for Host {
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            members: HashMap::new(),
            changed: Vec::new(),
            removed: Vec::new(),
            bound: HashMap::new(),
            seen: u64::MAX,
            layers_stale: false,
            events: Events::default(),
            taken: SecondaryMap::new(),
        }
    }
}

/// 帧结束后执行放完的处理器、唤醒等待的任务、写回进度信号；返回有没有做事
pub(crate) fn flush(tree: &TreeRef) -> bool {
    let events = mem::take(&mut tree.borrow_mut().motion.events);
    if events.is_empty() {
        return false;
    }
    events.run();
    true
}

/// 目标在动画时间 `t` 的值和速度（每秒）；`moving` 为假时速度按 0 算（停止、不再动它时从此刻的样子静止地出发）
fn motion(target: &Target, t: f64, moving: bool) -> ([f32; MAX_COMPONENTS], [f32; MAX_COMPONENTS]) {
    let mut value = [0.0; MAX_COMPONENTS];
    target.playback.sample(t, &mut value);
    let mut velocity = [0.0; MAX_COMPONENTS];
    if moving {
        target.playback.velocity(t, &mut velocity);
    }
    (value, velocity)
}

/// 宽高的新值换成样式里的尺寸
fn dimension(length: Length) -> Dimension {
    let mut style = Style::default();
    crate::layout::set_dimension(&mut style, false, length);
    style.size.width
}

impl Tree {
    /// 元素挂载时加入句柄
    pub(super) fn join_motion(&mut self, id: NodeId, handles: &[u64]) {
        self.nodes[id].motion |= MEMBER;
        for &handle in handles {
            self.motion.members.entry(handle).or_default().push(id);
            if !self.motion.changed.contains(&handle) {
                self.motion.changed.push(handle);
            }
        }
    }

    /// 元素卸载：下次同步时从句柄里去掉，时间线不再动它（节点已经删了，不用交还）
    pub(super) fn forget_motion(&mut self, id: NodeId) {
        self.motion.removed.push(id);
        self.motion.taken.remove(id);
    }

    /// 这个属性被时间线接管着
    fn taken_by_timeline(&self, id: NodeId, property: PropId) -> bool {
        self.nodes.get(id).is_some_and(|n| n.motion & TAKEN != 0) && self.motion.is_taken(id, property)
    }

    fn mark_taken(&mut self, id: NodeId, property: PropId) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        node.motion |= TAKEN;
        if let Some(set) = self.motion.taken.entry(id) {
            set.or_default().insert(property);
        }
    }

    fn unmark_taken(&mut self, id: NodeId, property: PropId) {
        let Some(set) = self.motion.taken.get_mut(id) else { return };
        set.remove(property);
        if set.is_empty() {
            self.motion.taken.remove(id);
            if let Some(node) = self.nodes.get_mut(id) {
                node.motion &= !TAKEN;
            }
        }
    }

    /// 把卸载了的成员一起去掉
    fn purge_removed(&mut self) {
        if self.motion.removed.is_empty() {
            return;
        }
        let removed: std::collections::HashSet<NodeId> = self.motion.removed.drain(..).collect();
        let host = &mut self.motion;
        for (handle, members) in host.members.iter_mut() {
            let before = members.len();
            members.retain(|m| !removed.contains(m));
            if members.len() != before && !host.changed.contains(handle) {
                host.changed.push(*handle);
            }
        }
        for bound in host.bound.values_mut() {
            let before = bound.targets.len();
            bound.targets.retain(|t| !removed.contains(&t.node));
            bound.split = None;
            bound.resend |= bound.targets.len() != before;
        }
    }

    /// 每帧：看登记表有没有变化（开始、暂停、跳转、新的时间线、成员变化），这个窗口负责的播放头走到头了就停下、
    /// 报告进度，在这一帧的动画时间 `t` 写不在合成层上的值。返回正在动的目标数
    pub(super) fn step_timelines(&mut self, t: f64) -> usize {
        let changes = with_registry(|_, changes| changes);
        if changes != self.motion.seen || !self.motion.changed.is_empty() || !self.motion.removed.is_empty() {
            self.sync_timelines();
        }
        if self.motion.bound.is_empty() {
            return 0;
        }
        let host = self.motion.id;
        let mut bound = mem::take(&mut self.motion.bound);
        let mut moving = 0;
        let mut out = [0.0; MAX_COMPONENTS];
        for (id, b) in bound.iter_mut() {
            let events = &mut self.motion.events;
            let state = with_registry(|entries, _| {
                let entry = entries.get_mut(id)?;
                if entry.driver == Some(host) && entry.playhead.playing {
                    // 播放头是时刻的纯函数，不用推进；走到头了在走到头的那一刻停下
                    if entry.playhead.finish(t) {
                        events.finished(entry);
                    }
                    events.report(entry, entry.playhead.position_at(t));
                }
                Some((entry.playhead.position_at(t), entry.playhead.direction_at(t) != 0.0))
            });
            let Some((position, playing)) = state else { continue };
            if playing {
                moving += b.targets.len();
            }
            if b.sampled == Some(position) {
                continue;
            }
            let first = b.sampled.is_none();
            b.sampled = Some(position);
            // 合成层上的不用每帧写：用到时再算（[`Tree::sync_timeline_layers`]）。停下时（放完、暂停）无障碍树里的位置更新一次
            if b.split.is_none() {
                b.split = Some(split(&b.targets));
            }
            let (plain, layered) = b.split.as_ref().expect("上面刚分过");
            if *layered {
                self.motion.layers_stale = true;
                if !playing || first {
                    for target in b.targets.iter().filter(|t| t.layer) {
                        self.accessibility.touch(target.node);
                    }
                }
            }
            for &i in plain {
                let target = &b.targets[i as usize];
                target.playback.sample_position(position, t, &mut out);
                let value = PropertyValue::from_components(target.property, &out);
                self.write(target.node, target.property, value);
            }
        }
        self.motion.bound = bound;
        if self.motion.layers_stale {
            self.paint.mark_worlds();
        }
        moving
    }

    /// 动画时钟拖动过：时间线的值都重新算一遍
    pub(super) fn resample_timelines(&mut self) {
        self.motion.layers_stale = true;
        for b in self.motion.bound.values_mut() {
            b.sampled = None;
        }
    }

    /// 合成层上被时间线动的值，主线程用到时才算（命中测试、生成图元、退场动画的起点、浮层的位置）
    pub(super) fn sync_timeline_layers(&mut self) {
        if !mem::take(&mut self.motion.layers_stale) {
            return;
        }
        let mut out = [0.0; MAX_COMPONENTS];
        let t = self.anim.time();
        for bound in self.motion.bound.values() {
            // 同一条时间线的目标共用一个播放头：位置算一次
            let Some(position) = bound.targets.first().map(|target| target.playback.playhead().position_at(t)) else { continue };
            for target in bound.targets.iter().filter(|t| t.layer) {
                target.playback.sample_position(position, t, &mut out);
                if let Some(node) = self.nodes.get_mut(target.node) {
                    node.set_value(target.property, PropertyValue::Float(out[0]));
                }
            }
        }
    }

    /// 登记表变了：新开始的时间线排期并接管属性，停掉、释放了的交还，命令交给渲染线程
    fn sync_timelines(&mut self) {
        self.purge_removed();
        // 先把合成层上的值算好：交还、接管时从屏幕上此刻的样子出发
        self.sync_timeline_layers();
        let changed = mem::take(&mut self.motion.changed);
        let mut bound = mem::take(&mut self.motion.bound);
        with_registry(|entries, changes| {
            self.motion.seen = changes;
            // 释放了、停止了、这个窗口里已经没有它的元素了：交还全部
            let gone: Vec<u64> = bound
                .keys()
                .copied()
                .filter(|id| !entries.get(id).is_some_and(|e| e.active && self.hosts(e)))
                .collect();
            for id in gone {
                let b = bound.remove(&id).expect("刚找到");
                // 停止时登记表里的播放头已经回到开头：目标手里还是原来的播放头，从屏幕上此刻的样子静止地出发
                for target in b.targets {
                    self.hand_back(target, false, true);
                }
                if b.on_render {
                    self.commit.timeline(LayerTimeline { id, playhead: Default::default(), targets: Arc::from([]) });
                }
                if let Some(entry) = entries.get_mut(&id)
                    && entry.driver == Some(self.motion.id)
                {
                    entry.driver = None;
                }
            }
            for (&id, entry) in entries.iter_mut() {
                if !entry.active || !self.hosts(entry) {
                    continue;
                }
                let rebuild = bound.get(&id).is_none_or(|b| b.generation != entry.generation);
                let regroup = !rebuild && entry.handles.iter().any(|h| changed.contains(h));
                if rebuild {
                    let old = bound.remove(&id);
                    let b = self.schedule(id, entry, old, &mut bound);
                    bound.insert(id, b);
                } else if regroup {
                    let mut b = bound.remove(&id).expect("上面判断过");
                    let old = mem::take(&mut b.targets);
                    b.split = None;
                    self.regroup(id, entry, &mut b, old, &mut bound);
                    self.send(id, &mut b);
                    bound.insert(id, b);
                } else if let Some(b) = bound.get_mut(&id)
                    && b.version != entry.version
                {
                    if mem::take(&mut entry.from_end) {
                        entry.playhead.position = entry.playhead.duration;
                    }
                    b.version = entry.version;
                    b.sampled = None;
                    // 播放、暂停、倒放、跳转：换上新的播放头
                    for target in &mut b.targets {
                        target.playback.set_playhead(entry.playhead);
                    }
                    self.send(id, b);
                }
            }
            // 被别的时间线拿走了属性的：重发
            for (id, b) in bound.iter_mut() {
                if mem::take(&mut b.resend) && entries.contains_key(id) {
                    self.send(*id, b);
                }
            }
        });
        self.motion.bound = bound;
    }

    /// 时间线动的句柄里有没有这个窗口的元素
    fn hosts(&self, entry: &Entry) -> bool {
        entry.handles.iter().any(|h| self.motion.members.get(h).is_some_and(|m| !m.is_empty()))
    }

    /// 从头排期：按此刻各句柄的元素数（错开的时长取决于它）排好每条轨道，成员按界面里的先后编号
    fn schedule(&mut self, id: u64, entry: &mut Entry, old: Option<Bound>, others: &mut HashMap<u64, Bound>) -> Bound {
        let members = &self.motion.members;
        let counts = |h: u64| members.get(&h).map_or(0, Vec::len);
        let (placed, duration) = entry.schedule(&counts);
        let mut indices = HashMap::new();
        let mut limits = HashMap::new();
        for &handle in &entry.handles {
            let mut list = members.get(&handle).cloned().unwrap_or_default();
            list.sort_by_cached_key(|&n| self.tree_order(n));
            for (i, node) in list.iter().enumerate() {
                indices.insert((handle, *node), i as f64);
            }
            limits.insert(handle, list.len().saturating_sub(1) as f64);
        }
        let landed = entry.scheduled_with(duration);
        if mem::take(&mut entry.from_end) || entry.playhead.position > duration {
            entry.playhead.position = duration;
        }
        // 排好期才落的跳转：停着的时间线不会再报告位置，这里写回信号
        if landed {
            self.motion.events.report(entry, entry.playhead.position);
        }
        if entry.driver.is_none() {
            entry.driver = Some(self.motion.id);
        }
        let on_render = old.as_ref().is_some_and(|b| b.on_render);
        let previous = old.map(|b| b.targets).unwrap_or_default();
        let mut b = Bound {
            generation: entry.generation,
            version: entry.version,
            placed,
            indices,
            limits,
            targets: Vec::new(),
            sampled: None,
            on_render,
            resend: false,
            split: None,
        };
        self.regroup(id, entry, &mut b, previous, others);
        self.send(id, &mut b);
        b
    }

    /// 按排好的轨道和成员算出每个（元素，属性）的轨道：之前就在的保留绑定的值，新的接管，不在了的交还
    fn regroup(&mut self, id: u64, entry: &Entry, b: &mut Bound, previous: Vec<Target>, others: &mut HashMap<u64, Bound>) {
        // 之后加入的成员接在后面
        for &handle in &entry.handles {
            let Some(list) = self.motion.members.get(&handle) else { continue };
            let limit = b.limits.get(&handle).copied().unwrap_or(0.0);
            let mut next = b.indices.iter().filter(|((h, _), _)| *h == handle).map(|(_, &i)| i + 1.0).fold(0.0, f64::max);
            for &node in list {
                b.indices.entry((handle, node)).or_insert_with(|| {
                    let i = next.min(limit);
                    next += 1.0;
                    i
                });
            }
        }
        // 按（元素，属性）分组，保持定义的顺序
        let mut grouped: Vec<((NodeId, PropId), Vec<KeyframePlacement>)> = Vec::new();
        let mut slots: HashMap<(NodeId, PropId), usize> = HashMap::new();
        for placed in &b.placed {
            let Some(list) = self.motion.members.get(&placed.handle) else { continue };
            for &node in list {
                if self.nodes.get(node).is_none_or(|n| n.exiting) {
                    continue;
                }
                let index = b.indices.get(&(placed.handle, node)).copied().unwrap_or(0.0);
                let track = KeyframePlacement { keys: placed.track.clone(), levels: placed.levels.clone(), index };
                let key = (node, placed.property);
                match slots.get(&key) {
                    Some(&i) => grouped[i].1.push(track),
                    None => {
                        slots.insert(key, grouped.len());
                        grouped.push((key, vec![track]));
                    }
                }
            }
        }
        let mut previous: HashMap<(NodeId, PropId), Target> = previous.into_iter().map(|t| ((t.node, t.property), t)).collect();
        b.split = None;
        b.targets.reserve(grouped.len());
        for ((node, property), tracks) in grouped {
            let base = match previous.remove(&(node, property)) {
                Some(target) => target.base,
                None => self.take_over(id, node, property, others),
            };
            let layer = is_compositor(property);
            let reduced = entry.reduced && is_movement(property);
            let playback = KeyframePlayback::timeline(tracks.into(), entry.playhead, reduced);
            b.targets.push(Target { node, property, base, playback, layer });
        }
        // 不再动的：交还，从屏幕上此刻的样子静止地出发
        for target in previous.into_values() {
            if self.nodes.contains_key(target.node) {
                self.hand_back(target, false, true);
            }
        }
        b.sampled = None;
    }

    /// 把这条时间线在这个窗口里的合成层轨道交给渲染线程：各目标的几段关键帧，加上共用的播放头
    fn send(&mut self, id: u64, b: &mut Bound) {
        let targets: Arc<[LayerTarget]> = b
            .targets
            .iter()
            .filter(|t| t.layer)
            .map(|t| LayerTarget {
                key: layer_key(t.node),
                property: t.property,
                reduced: t.playback.is_reduced(),
                segments: t.playback.segments().clone(),
            })
            .collect();
        if targets.is_empty() && !b.on_render {
            return;
        }
        b.on_render = !targets.is_empty();
        let playhead = b.targets.first().map(|t| *t.playback.playhead()).unwrap_or_default();
        self.commit.timeline(LayerTimeline { id, playhead, targets });
    }

    /// 界面里的先后：从根到它的一串子元素序号
    fn tree_order(&self, mut id: NodeId) -> Vec<u32> {
        let mut path = Vec::new();
        while let Some(node) = self.nodes.get(id) {
            match node.parent {
                Some(parent) => {
                    path.push(self.nodes[parent].children.iter().position(|&c| c == id).unwrap_or(0) as u32);
                    id = parent;
                }
                None => {
                    path.push(self.nodes.roots().iter().position(|&r| r == id).unwrap_or(usize::MAX) as u32);
                    break;
                }
            }
        }
        path.reverse();
        path
    }

    /// 时间线 `timeline` 接管（元素，属性）：记下绑定的值，停掉进行中的过渡。别的时间线正在动它时从那条手里拿过来
    fn take_over(&mut self, timeline: u64, id: NodeId, property: PropId, others: &mut HashMap<u64, Bound>) -> Base {
        let mut base = None;
        if self.taken_by_timeline(id, property) {
            for (&other, bound) in others.iter_mut() {
                if other == timeline {
                    continue;
                }
                if let Some(i) = bound.targets.iter().position(|t| t.node == id && t.property == property) {
                    let target = bound.targets.remove(i);
                    bound.split = None;
                    // 那条时间线的合成层轨道少了一个：重发
                    bound.resend |= target.layer;
                    base = Some(target.base);
                }
            }
        }
        let base = base.unwrap_or_else(|| self.capture(id, property));
        self.mark_taken(id, property);
        let node = &mut self.nodes[id];
        if let Some(index) = node.animations.iter().position(|slot| slot.property == property) {
            let slot = &mut node.animations[index];
            if mem::take(&mut slot.queued) {
                self.anim.dequeue(id, index);
            }
        }
        if is_compositor(property) && !node.layer {
            node.layer = true;
            node.refresh_flags();
            self.paint.invalidate();
        }
        // 绕 x、y 轴的旋转、透视：层表里要带 3D 设置，渲染线程才能按时间线的值画
        if property.is_depth() && !node.extra.as_deref().is_some_and(|e| e.animated) {
            node.extra.get_or_insert_with(Default::default).animated = true;
            node.refresh_flags();
            self.paint.invalidate();
        }
        // 透明度在动的层要按分组画
        if property == PropId::OPACITY {
            self.paint.invalidate();
        }
        base
    }

    /// 接管前绑定的值
    fn capture(&self, id: NodeId, property: PropId) -> Base {
        let node = &self.nodes[id];
        let style = || self.layout.style(node.layout);
        match property {
            PropId::WIDTH => Base::Size(style().size.width),
            PropId::HEIGHT => Base::Size(style().size.height),
            PropId::PADDING => Base::Padding(style().padding),
            PropId::GAP => Base::Gap(style().gap),
            PropId::COLOR => Base::Color(node.text.color.clone()),
            _ => {
                if let Some(slot) = node.animations.iter().find(|slot| slot.property == property) {
                    if let Some(mix) = &slot.mix {
                        return Base::Value(mix.target());
                    }
                    if let Some(track) = &slot.track {
                        return Base::Value(PropertyValue::from_components(property, track.target()));
                    }
                }
                Base::Value(self.current_value(id, property))
            }
        }
    }

    /// 交还一个属性。`moving`：带着时间线此刻的速度交出去（时间线还在放，只是不再动它）；否则静止地交出去。
    /// `restore`：回到绑定的值（声明了过渡的按过渡回去）；否则调用方接着写新的绑定值，这里只把过渡的起点设成此刻的值和速度
    fn hand_back(&mut self, target: Target, moving: bool, restore: bool) {
        let (id, property) = (target.node, target.property);
        self.unmark_taken(id, property);
        let now = self.anim.now();
        let Some(node) = self.nodes.get_mut(id) else { return };
        let (value, velocity) = motion(&target, now, moving);
        let len = PropertyValue::from_components(property, &value).to_components(&mut [0.0; MAX_COMPONENTS]);
        if target.layer {
            node.set_value(property, PropertyValue::Float(value[0]));
            self.paint.mark_worlds();
            self.accessibility.touch(id);
        }
        // 声明了过渡：从此刻的值和速度出发
        let slot = node.animations.iter().position(|slot| slot.property == property);
        // 没声明过渡的合成层属性：渲染线程之后用层表里的值，层表要重新生成（里面还是接管那时的值）
        if target.layer && slot.is_none() {
            self.paint.invalidate();
        }
        if let Some(index) = slot {
            // 此刻从时间线的值和速度出发（目标先是这个值，下面改去绑定的值）；合成层上的整份交给渲染线程，
            // 它换掉时间线的值时按同一个时刻采样，不跳
            let slot = &mut node.animations[index];
            let mut track = CurveTrack::new(slot.curve, &value[..len], property.tolerance());
            track.restart(now, &value[..len], &velocity[..len]);
            let moving = track.is_moving(now);
            if target.layer {
                self.commit.track(LayerTrack { key: layer_key(id), property, track: track.clone() });
            }
            slot.track = Some(track);
            slot.mix = None;
            if moving && !slot.queued {
                slot.queued = true;
                self.anim.queue(id, index);
            }
        }
        if !restore {
            return;
        }
        match target.base {
            Base::Value(v) if slot.is_some() => self.set_property(id, property, v),
            Base::Value(v) => self.write(id, property, v),
            Base::Color(own) => match own {
                Some(fill) if slot.is_some() => self.set_property(id, property, PropertyValue::from(fill)),
                Some(fill) => self.write(id, property, PropertyValue::from(fill)),
                None => {
                    self.nodes[id].text.color = None;
                    self.layout.restyle();
                }
            },
            Base::Size(d) => match (d.into_option(), slot) {
                (Some(px), Some(_)) if d == Dimension::length(px) => self.set_property(id, property, PropertyValue::Float(px)),
                _ => self.update_style(id, |s| {
                    if property == PropId::HEIGHT {
                        s.size.height = d;
                    } else {
                        s.size.width = d;
                    }
                }),
            },
            Base::Padding(padding) => self.update_style(id, |s| s.padding = padding),
            Base::Gap(gap) => self.update_style(id, |s| s.gap = gap),
        }
    }

    /// 被时间线接管着的属性来了新的绑定值。和接管时记下的一样就什么都不做（返回 `false`，调用方不用再写）；
    /// 不一样就交还这一个属性（之后时间线不再动它），返回 `true`，调用方照常写新值：声明了过渡的从此刻接着过渡
    pub(super) fn interrupt_timeline(&mut self, id: NodeId, property: PropId, incoming: Incoming) -> bool {
        if !self.taken_by_timeline(id, property) {
            return true;
        }
        let mut bound = mem::take(&mut self.motion.bound);
        let mut proceed = true;
        for (timeline, b) in bound.iter_mut() {
            let Some(i) = b.targets.iter().position(|t| t.node == id && t.property == property) else { continue };
            if incoming.matches(&b.targets[i].base) {
                proceed = false;
                break;
            }
            let mut target = b.targets.remove(i);
            b.split = None;
            // 登记表里的播放头可能刚换过（命令在这一帧之前发的，还没同步到目标上）
            if let Some(playhead) = with_registry(|entries, _| entries.get(timeline).map(|e| e.playhead)) {
                target.playback.set_playhead(playhead);
            }
            // 渲染线程那边少了这一条轨道
            b.resend |= target.layer;
            self.hand_back(target, true, false);
        }
        if proceed {
            with_registry(|entries, _| {
                for (id, b) in bound.iter_mut() {
                    if mem::take(&mut b.resend) && entries.contains_key(id) {
                        self.send(*id, b);
                    }
                }
            });
        }
        self.motion.bound = bound;
        proceed
    }
}

/// 新来的绑定值，和接管时记下的比较
pub(super) enum Incoming<'a> {
    Value(&'a PropertyValue),
    Size(Length),
}

impl Incoming<'_> {
    fn matches(&self, base: &Base) -> bool {
        match (self, base) {
            (Incoming::Value(v), Base::Value(b)) => *v == b,
            (Incoming::Value(PropertyValue::Color(c)), Base::Color(Some(Fill::Color(b)))) => c == b,
            (Incoming::Value(PropertyValue::Gradient(g)), Base::Color(Some(Fill::Gradient(b)))) => g == b,
            (Incoming::Value(PropertyValue::Float(px)), Base::Size(d)) => *d == Dimension::length(*px),
            (Incoming::Size(length), Base::Size(d)) => dimension(*length) == *d,
            (Incoming::Value(PropertyValue::Float(px)), Base::Padding(p)) => {
                let v = LengthPercentage::length(*px);
                [p.left, p.right, p.top, p.bottom].iter().all(|&side| side == v)
            }
            (Incoming::Value(PropertyValue::Float(px)), Base::Gap(g)) => {
                let v = LengthPercentage::length(*px);
                g.width == v && g.height == v
            }
            _ => false,
        }
    }
}
