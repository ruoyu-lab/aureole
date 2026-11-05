//! 动画：元素上声明的属性动画（动画槽）、在动的动画、退场、布局位置的过渡和这一帧的时刻，
//! 以及每帧采样动画、改动画目标、开始和结束退场。
//!
//! 每个动画槽 [`AnimSlot`] 有一条按时间求值的轨道（[`CurveTrack`]：弹簧或补间）：每帧按这一帧的动画时间采样，
//! 不逐帧累加；改目标在"此刻"（[`AnimState::now`]）换锚点，位置连续，弹簧的速度也连续。
//! 合成层的变换和透明度把轨道整份交给渲染线程，两边按同一个时刻采样，值一样。
//! [`AnimState`] 的字段私有，别的模块通过方法登记要采样的动画、读时刻。

use std::mem;

use animation::{Curve, CurveTrack, MAX_COMPONENTS, TrackState, Track, Tween};

use taffy::ResolveOrZero;

use super::{ChildSlot, Leave, NodeId, Tree, layer_key, timeline};
use crate::property::{Mix, PropId, PropertyValue};
use crate::compositor::{LayerTrack, is_compositor};
use crate::layout::Length;
use crate::geometry::Rect;

/// 这一帧的时刻和在动的动画
#[derive(Default)]
pub(super) struct AnimState {
    /// 这一帧的宿主时刻（显示链接给的预计上屏时刻，测试工具里是虚拟时间），秒；还没出过帧时是 `None`
    host: Option<f64>,
    /// 这一帧的动画时间：宿主时刻经动画时钟（[`crate::clock`]）换算，秒
    time: Option<f64>,
    /// 正在运动的动画：(节点, 动画下标)
    active: Vec<(NodeId, usize)>,
    /// 正在退场的节点
    exiting: Vec<NodeId>,
    /// 声明了 [`Property::LayoutPosition`] 的元素和它上次相对父元素的布局位置
    slides: Vec<(NodeId, Option<[f32; 2]>)>,
    /// 上一帧结束时还在动的动画数（含时间线、拖放的影子、共享元素的飞行），见 [`super::FrameStats::animations`]
    moving: usize,
    /// 看到的动画时钟拖动过几次：拖过（时间跳了，可能往回跳）时所有的轨道重新采样一遍
    seeks: u64,
}

impl AnimState {
    /// 这一帧的宿主时刻（还没推进过时是 0）：滚动、插入点闪烁这些不跟动画时钟走的用它
    pub(super) fn host(&self) -> f64 {
        self.host.unwrap_or(0.0)
    }

    /// 上一帧的宿主时刻，还没推进过时是 `None`
    pub(super) fn last_host(&self) -> Option<f64> {
        self.host
    }

    /// 一帧开始：宿主时刻是 `host`，返回这一帧的动画时间（按动画时钟换算）
    pub(super) fn tick(&mut self, host: f64) -> f64 {
        let time = crate::clock::frame(host);
        self.host = Some(host);
        self.time = Some(time);
        time
    }

    /// 这一帧的动画时间（还没推进过时是 0）：每帧按它采样轨道
    pub(super) fn time(&self) -> f64 {
        self.time.unwrap_or(0.0)
    }

    /// 此刻的动画时间：改目标、开始飞、开始落这些从它起算。帧里面就是这一帧的时间；帧与帧之间（事件处理器、定时器里）
    /// 是统一时钟的读数，但不早于这一帧（帧的时刻是预计上屏的时刻，常常比读数晚）：从屏幕上最近的样子出发，不回到它之前。
    /// 主线程卡住之后、画面静止很久之后才改的目标从改的那一刻起算，不会一上来就跳过一大段
    pub(super) fn now(&self) -> f64 {
        let host = crate::timer::now().max(self.host.unwrap_or(f64::NEG_INFINITY));
        crate::clock::time_at(host)
    }

    /// 这个动画开始运动，每帧推进它
    pub(super) fn queue(&mut self, id: NodeId, index: usize) {
        self.active.push((id, index));
    }

    /// 这个动画停下了（或者被换掉），不再推进
    pub(super) fn dequeue(&mut self, id: NodeId, index: usize) {
        self.active.retain(|&entry| entry != (id, index));
    }

    /// 正在运动的动画
    pub(super) fn active(&self) -> &[(NodeId, usize)] {
        &self.active
    }

    /// 登记要做布局位置过渡的元素
    pub(super) fn track_slide(&mut self, id: NodeId) {
        self.slides.push((id, None));
    }

    /// 有没有要做布局位置过渡的元素
    pub(super) fn sliding(&self) -> bool {
        !self.slides.is_empty()
    }

    /// 这一帧结束时还在动的动画数
    pub(super) fn set_moving(&mut self, moving: usize) {
        self.moving = moving;
    }

    /// 还有动画要推进：上一帧结束时还在动的、在动的属性动画、在退场的。动画时钟停着时什么都不会动，不算
    pub(super) fn busy(&self) -> bool {
        (self.moving > 0 || !self.active.is_empty() || !self.exiting.is_empty()) && crate::clock::running()
    }

    /// 新的元素树：从此刻的动画时钟看起
    pub(super) fn new() -> Self {
        Self { seeks: crate::clock::seeks(), ..Self::default() }
    }

    /// 动画时钟拖动过以后的第一帧：记下来，返回 `true`（暂停、继续、改倍速时时间是连续的，不用重新采样）
    pub(super) fn take_seek(&mut self) -> bool {
        let seeks = crate::clock::seeks();
        seeks != std::mem::replace(&mut self.seeks, seeks)
    }
}

/// 元素上声明的一个属性动画
pub(super) struct AnimSlot {
    pub(super) property: PropId,
    pub(super) curve: Curve,
    /// 收到第一个值时创建；初始值不做动画
    pub(super) track: Option<CurveTrack>,
    /// 是否已在 `Tree::active` 里（轨道还在动）
    pub(super) queued: bool,
    /// 渐变、几层投影、内阴影、滤镜参与的过渡：`track` 是 0 -> 1 的进度，值是两端的混合
    pub(super) mix: Option<Box<Mix>>,
    /// 布局位置的过渡此刻的值（离新的布局位置还差多少）：绘制、命中测试读它（别的属性的值直接写进节点）
    pub(super) slide: [f32; 2],
}

impl AnimSlot {
    pub(super) fn new(property: PropId, curve: Curve) -> Self {
        Self { property, curve, track: None, queued: false, mix: None, slide: [0.0; 2] }
    }

    /// 新的轨道，静止在 `value`
    fn rest(&self, value: &[f32]) -> CurveTrack {
        CurveTrack::new(self.curve, value, self.property.tolerance())
    }
}

/// 没有为退场属性声明曲线时用的默认曲线
const EXIT_CURVE: Tween = Tween::new(std::time::Duration::from_millis(200)).ease_out();

thread_local! {
    /// 大于 0 时，属性变化不做动画，直接跳到新值（见 [`without_animation`]）
    static WITHOUT_ANIMATION: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// 在 `f` 里引起的属性变化不做动画，直接跳到新值，即使属性声明了 [`crate::Element::transition`]。
/// `f` 里的写入在返回前就生效（即使在处理器的批量更新里）。用来"先复位、再播放"：
///
/// ```no_run
/// # use aureole::prelude::*;
/// # let (seed, drawn) = (signal(0), signal(true));
/// aureole::without_animation(|| {
///     seed.update(|s| *s += 1);
///     drawn.set(false); // 描线进度直接回到 0
/// });
/// drawn.set(true); // 再从 0 描到 1
/// ```
pub fn without_animation<R>(f: impl FnOnce() -> R) -> R {
    WITHOUT_ANIMATION.with(|depth| depth.set(depth.get() + 1));
    let result = f();
    // 处理器在批量更新里，副作用要等处理器结束才执行；这里先执行，写入才能"不做动画"
    reactive::flush_effects();
    WITHOUT_ANIMATION.with(|depth| depth.set(depth.get() - 1));
    result
}

/// 在 `f` 里的写入不做动画（覆盖层用）。和 [`without_animation`] 不同，不执行积压的副作用
#[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
pub(super) fn instantly<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            WITHOUT_ANIMATION.with(|depth| depth.set(depth.get() - 1));
        }
    }
    WITHOUT_ANIMATION.with(|depth| depth.set(depth.get() + 1));
    let _restore = Restore;
    f()
}

pub(super) fn animations_disabled() -> bool {
    WITHOUT_ANIMATION.with(|depth| depth.get() > 0)
}

/// 系统打开了"减少动态效果"（不订阅：在绑定的副作用里读也不建立依赖）
pub(super) fn is_motion_reduced() -> bool {
    reactive::untracked(crate::system::is_motion_reduced)
}

impl Tree {
    /// 动画时钟拖动过（比如拖回去了）：停下的轨道在新的时刻可能还在动，所有的轨道都重新采样一遍（只在拖动之后做一次）
    pub(super) fn requeue_animations(&mut self) {
        let ids: Vec<NodeId> = self.nodes.keys().collect();
        for id in ids {
            for (index, slot) in self.nodes[id].animations.iter_mut().enumerate() {
                if slot.track.is_some() && !slot.queued {
                    slot.queued = true;
                    self.anim.active.push((id, index));
                }
            }
        }
    }

    /// 每帧：在动的动画按这一帧的动画时间 `t` 采样，写进节点；停下的不再采样
    pub(super) fn step_animations(&mut self, t: f64) {
        if self.anim.active.is_empty() {
            return;
        }
        let mut active = mem::take(&mut self.anim.active);
        let mut out = [0.0; MAX_COMPONENTS];
        active.retain(|&(id, index)| {
            let Some(node) = self.nodes.get_mut(id) else { return false };
            let layer = node.layer;
            let Some(slot) = node.animations.get_mut(index) else { return false };
            let Some(track) = &slot.track else { return false };
            let len = track.len();
            let moving = track.sample(t, &mut out) == TrackState::Moving;
            slot.queued = moving;
            let value = match &slot.mix {
                // 停稳时正好是目标；目标是纯色、一层外投影时回到按分量做动画
                Some(_) if !moving => settle_mix(slot),
                Some(mix) => mix.at(out[0]),
                None => PropertyValue::from_components(slot.property, &out[..len]),
            };
            let property = slot.property;
            if property == PropId::LAYOUT_POSITION {
                slot.slide = [out[0], out[1]];
                // 布局位置的过渡：只改合成层的额外平移，不重建图元
                self.commit.offset(layer_key(id), node.layer_offset());
                self.paint.mark_worlds();
                if !moving {
                    self.accessibility.touch(id);
                }
                return moving;
            }
            if layer && is_compositor(property) {
                // 合成层的变换、透明度由渲染线程推进上屏；主线程只记下当前值，
                // 用于命中测试和判断退场结束，不重建图元
                node.set_value(property, value);
                self.paint.mark_worlds();
                // 无障碍树里的变换在动画停下时才更新
                if !moving {
                    self.accessibility.touch(id);
                }
            } else {
                self.write(id, property, value);
            }
            moving
        });
        active.append(&mut self.anim.active);
        self.anim.active = active;
    }

    /// 重新布局之后：声明了 [`Property::LayoutPosition`] 的元素相对父元素的位置变了，就从原来的位置滑过去
    /// （动画的值是离新位置还差多少，平移到原来的位置再向 0 运动；正在滑的接着滑，速度不变）。
    /// `animate` 为假（只是窗口大小变了）或者系统要求减少动态效果时直接跟上，只记下新位置
    pub(super) fn detect_slides(&mut self, animate: bool) {
        let animate = animate && !animations_disabled() && !reactive::untracked(crate::system::is_motion_reduced);
        let time = self.anim.now();
        let mut slides = mem::take(&mut self.anim.slides);
        slides.retain_mut(|(id, last)| {
            let Some(node) = self.nodes.get(*id) else { return false };
            let Some(layout) = self.layout.try_get(node.layout) else { return true };
            let at = self.inline_position(*id).unwrap_or_default();
            let now = [at[0] + layout.location.x, at[1] + layout.location.y];
            let Some(before) = last.replace(now) else { return true };
            if !animate || before == now || node.exiting {
                return true;
            }
            let id = *id;
            let node = &mut self.nodes[id];
            let Some(index) = node.animations.iter().position(|s| s.property == PropId::LAYOUT_POSITION) else { return false };
            let slot = &mut node.animations[index];
            if slot.track.is_none() {
                slot.track = Some(slot.rest(&[0.0, 0.0]));
            }
            let track = slot.track.as_mut().expect("刚放进去");
            // 从此刻的位置平移到原来在屏幕上的地方，保留速度接着向 0 走
            track.shift(time, &[before[0] - now[0], before[1] - now[1]]);
            let mut out = [0.0; MAX_COMPONENTS];
            track.sample(time, &mut out);
            slot.slide = [out[0], out[1]];
            if !slot.queued {
                slot.queued = true;
                self.anim.active.push((id, index));
            }
            self.commit.offset(layer_key(id), node.layer_offset());
            self.paint.mark_worlds();
            true
        });
        slides.append(&mut self.anim.slides);
        self.anim.slides = slides;
    }

    pub(crate) fn has_exit(&self, id: NodeId) -> bool {
        self.nodes.cold(id).is_some_and(|cold| !cold.exit.is_empty())
    }

    /// 开始退场：把声明的属性过渡到退场值，动画都停下后由 `finish_exits` 卸载
    pub(crate) fn begin_exit(&mut self, id: NodeId) {
        if self.nodes.contains_key(id) {
            self.start_exit(id);
            self.anim.exiting.push(id);
        }
    }

    /// 退场动画：不再参与交互，属性过渡到退场值（不负责卸载）
    pub(super) fn start_exit(&mut self, id: NodeId) {
        // 退场动画从屏幕上此刻的样子出发
        self.sync_timeline_layers();
        let Some(node) = self.nodes.get_mut(id) else { return };
        node.exiting = true;
        let exit = self.nodes.cold(id).map(|cold| cold.exit.clone()).unwrap_or_default();
        self.accessibility_touch_parent(id);
        // 共享元素开始退场就算离开：接替它的元素马上从它此刻的位置飞过去
        self.depart_subtree(id);
        // 退场的元素不再参与交互：它的处理器和信号已随作用域释放，不能再收到事件。
        // 焦点、悬停、按下、拖选、拖动、拖放里指向它（或它的子孙）的都清掉
        if self.forget_nodes(Leave::Exit(id)) {
            self.paint.invalidate();
        }
        for (property, value, exit_curve) in exit {
            // 没有为这个属性声明曲线：补一个，从当前值出发
            let current = self.current_value(id, property);
            let node = &mut self.nodes[id];
            let index = match node.animations.iter().position(|slot| slot.property == property) {
                Some(index) => index,
                None => {
                    let curve = exit_curve.unwrap_or(EXIT_CURVE.into());
                    node.animations.push(AnimSlot::new(property, curve));
                    node.animations.len() - 1
                }
            };
            let slot = &mut node.animations[index];
            // 退场有自己的曲线：从当前值重新开始（进行中的入场动画停在这里）
            if let Some(curve) = exit_curve
                && curve != slot.curve
            {
                slot.curve = curve;
                slot.track = None;
                slot.mix = None;
                if mem::take(&mut slot.queued) {
                    self.anim.active.retain(|&entry| entry != (id, index));
                }
            }
            let slot = &mut self.nodes[id].animations[index];
            if slot.track.is_none() {
                if !current.is_simple() {
                    // 渐变、几层投影按进度混合：先停在当前的值上
                    slot.track = Some(slot.rest(&[1.0]));
                    slot.mix = Some(Box::new(Mix::new(current.clone(), current)));
                } else {
                    let mut components = [0.0; MAX_COMPONENTS];
                    let len = current.to_components(&mut components);
                    slot.track = Some(slot.rest(&components[..len]));
                }
            }
            self.set_property(id, property, value);
        }
    }

    /// 动画都停下的退场节点：从父元素里移除并卸载
    pub(super) fn finish_exits(&mut self) {
        if self.anim.exiting.is_empty() {
            return;
        }
        let exiting = mem::take(&mut self.anim.exiting);
        for id in exiting {
            let Some(node) = self.nodes.get(id) else { continue };
            if node.animations.iter().any(|slot| slot.queued) {
                self.anim.exiting.push(id);
                continue;
            }
            if let Some(parent) = node.parent.filter(|&p| self.nodes.contains_key(p)) {
                for slot in self.nodes.cold_mut(parent).map_or(&mut [][..], |cold| &mut cold.slots[..]) {
                    if let ChildSlot::Dynamic(children) = slot {
                        children.retain(|&child| child != id);
                    }
                }
                self.rebuild_children(parent);
            }
            self.unmount(id);
        }
    }

    /// 属性的当前值（退场动画的起点）
    pub(super) fn current_value(&self, id: NodeId, property: PropId) -> PropertyValue {
        let node = &self.nodes[id];
        let p = &node.paint;
        match property {
            PropId::BACKGROUND => PropertyValue::from(p.background.clone()),
            PropId::BORDER_COLOR => PropertyValue::from(p.border_fill()),
            PropId::COLOR => PropertyValue::from(p.color.clone()),
            PropId::SHADOW => PropertyValue::from(p.shadows()),
            PropId::BORDER_WIDTH => PropertyValue::Float(p.border_width),
            PropId::RADIUS => PropertyValue::Float(p.radius),
            PropId::OPACITY => PropertyValue::Float(p.opacity),
            PropId::SCALE => PropertyValue::Float(p.scale),
            PropId::ROTATION => PropertyValue::Float(p.rotation),
            PropId::TRANSLATION_X => PropertyValue::Float(p.translate[0]),
            PropId::TRANSLATION_Y => PropertyValue::Float(p.translate[1]),
            PropId::ROTATION_X | PropId::ROTATION_Y | PropId::PERSPECTIVE => PropertyValue::Float(node.depth_value(property)),
            PropId::BLUR => PropertyValue::Float(p.blur),
            PropId::BACKDROP_BLUR => PropertyValue::Float(p.backdrop_blur),
            PropId::BACKDROP_SATURATION => PropertyValue::Float(p.backdrop_saturation),
            PropId::FILTER => PropertyValue::Filter(p.effects.as_ref().map(|e| e.filter.clone()).unwrap_or_default()),
            PropId::STROKE_START => PropertyValue::Float(p.trim[0]),
            PropId::STROKE_END => PropertyValue::Float(p.trim[1]),
            PropId::LAYOUT_POSITION => PropertyValue::Float(0.0),
            PropId::PATH => PropertyValue::Float(self.nodes.kind::<crate::vector::PathBlock>(id).and_then(|b| b.morph.as_ref()).map_or(1.0, |m| m.progress)),
            PropId::WIDTH => PropertyValue::Float(node.bounds.width),
            PropId::HEIGHT => PropertyValue::Float(node.bounds.height),
            PropId::PADDING | PropId::GAP => {
                let style = self.layout.style(node.layout);
                let length = match property {
                    PropId::PADDING => style.padding.top,
                    _ => style.gap.width,
                };
                PropertyValue::Float(length.resolve_or_zero(None, |_, _| 0.0))
            }
            // 不能做动画的属性没有退场动画（声明时已经挡掉）
            _ => PropertyValue::Float(0.0),
        }
    }

    /// 可动画属性的新值：有动画就改动画目标，没有就直接写入
    pub(crate) fn set_property(&mut self, id: NodeId, property: PropId, value: PropertyValue) {
        // 被时间线接管着：新值和接管前的一样就不用管；不一样就交还这个属性，下面从此刻的值接着过渡
        if !self.interrupt_timeline(id, property, timeline::Incoming::Value(&value)) {
            return;
        }
        // 透明度决定元素在不在无障碍树里
        if property == PropId::OPACITY {
            self.accessibility_touch_parent(id);
        }
        let Some(node) = self.nodes.get_mut(id) else { return };
        if let Some(index) = node.animations.iter().position(|slot| slot.property == property) {
            // 滤镜之间的过渡：按进度混合两端
            if let PropertyValue::Filter(target) = value {
                self.animate_filter(id, index, target);
                return;
            }
            // 渐变、几层投影、内阴影参与的过渡：按进度混合两端
            if property.mixes() && (!value.is_simple() || node.animations[index].mix.is_some()) {
                self.animate_mix(id, index, value);
                return;
            }
            let now = self.anim.now();
            let slot = &mut node.animations[index];
            let mut components = [0.0; MAX_COMPONENTS];
            let len = value.to_components(&mut components);
            match &mut slot.track {
                None => slot.track = Some(CurveTrack::new(slot.curve, &components[..len], property.tolerance())),
                // 系统打开了"减少动态效果"：绕 x、y 轴的旋转和透视直接到位
                Some(track) if animations_disabled() || (property.is_depth() && is_motion_reduced()) => {
                    // 直接跳到新值；合成层的动画让渲染线程也跳过去
                    track.jump(&components[..len]);
                    if node.layer && is_compositor(property) {
                        let track = track.clone();
                        self.commit.track(LayerTrack { key: layer_key(id), property, track });
                        // 和动画推进一样：主线程只记下当前值（命中测试用），不重建图元。
                        // 拖动时每帧直接跳到新位置的元素因此只动合成层
                        node.set_value(property, value);
                        self.paint.mark_worlds();
                        self.accessibility.touch(id);
                        return;
                    }
                }
                Some(track) => {
                    // 从此刻的位置（弹簧还有速度）改去新目标；目标没变时什么都不做
                    let retarget = track.target() != &components[..len];
                    track.retarget(now, &components[..len]);
                    if track.is_moving(now) && !slot.queued {
                        slot.queued = true;
                        self.anim.active.push((id, index));
                    }
                    // 合成层的变换、透明度动画：把轨道整份交给渲染线程，两边按同一个时刻采样
                    if retarget && node.layer && is_compositor(property) {
                        let track = track.clone();
                        self.commit.track(LayerTrack { key: layer_key(id), property, track });
                    }
                    return;
                }
            }
        }
        self.write(id, property, value);
    }

    /// 宽高：像素值照常走 `set_property`（声明了动画就过渡）；百分比、auto 等直接写进样式，
    /// 并停下进行中的动画。之后再切回像素值时，从当前的布局尺寸出发过渡（auto → 像素也能平滑过渡）
    pub(super) fn set_size(&mut self, id: NodeId, property: PropId, length: Length) {
        if !matches!(length, Length::Px(_)) && !self.interrupt_timeline(id, property, timeline::Incoming::Size(length)) {
            return;
        }
        let Some(node) = self.nodes.get_mut(id) else { return };
        let height = property == PropId::HEIGHT;
        let index = node.animations.iter().position(|slot| slot.property == property);
        match (length, index) {
            (Length::Px(v), Some(index)) => {
                // 挂载后还没布局过（`bounds` 是默认值）的初始值不做动画
                let slot = &mut node.animations[index];
                if slot.track.is_none() && node.bounds != Rect::default() {
                    let current = if height { node.bounds.height } else { node.bounds.width };
                    slot.track = Some(slot.rest(&[current]));
                }
                self.set_property(id, property, PropertyValue::Float(v));
            }
            (Length::Px(v), None) => self.write(id, property, PropertyValue::Float(v)),
            (length, index) => {
                if let Some(index) = index {
                    let slot = &mut node.animations[index];
                    slot.track = None;
                    if mem::take(&mut slot.queued) {
                        self.anim.active.retain(|&entry| entry != (id, index));
                    }
                }
                self.update_style(id, |s| crate::layout::set_dimension(s, height, length));
            }
        }
    }

    /// 新值是渐变、几层投影、内阴影（或者正在和它们过渡）：从此刻的样子按进度混合到新值。
    /// 打断时把此刻的变化速度投影到新方向上，弹簧不会突然停下再出发
    pub(super) fn animate_mix(&mut self, id: NodeId, index: usize, target: PropertyValue) {
        let property = self.nodes[id].animations[index].property;
        let current = self.current_value(id, property);
        let now = self.anim.now();
        let slot = &mut self.nodes[id].animations[index];
        if slot.track.is_none() || animations_disabled() {
            // 初始值不做动画；without_animation 里直接跳到新值
            slot.track = Some(slot.rest(&[1.0]));
            slot.mix = Some(Box::new(Mix::new(target.clone(), target.clone())));
            self.write(id, property, target);
            return;
        }
        if slot.mix.as_ref().is_some_and(|mix| mix.target() == target) {
            return;
        }
        // 此刻的进度和速度（按分量的动画是各分量的速度）
        let (mut value, mut rate) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
        let velocity = match (&slot.mix, &slot.track) {
            (Some(mix), Some(track)) => {
                track.sample(now, &mut value);
                track.velocity(now, &mut rate);
                mix.retarget(&current, &target, value[0], rate[0])
            }
            (None, Some(track)) => {
                track.velocity(now, &mut rate);
                Mix::retarget_components(&current, &target, &rate[..track.len()])
            }
            _ => 0.0,
        };
        // 进度从 0 带着投影过来的速度奔向 1
        let mut track = slot.rest(&[1.0]);
        track.restart(now, &[0.0], &[velocity]);
        slot.track = Some(track);
        slot.mix = Some(Box::new(Mix::new(current, target)));
        if !slot.queued {
            slot.queued = true;
            self.anim.active.push((id, index));
        }
    }
}

/// 按进度混合的动画停稳了：值正好是目标；目标是纯色、一层外投影时回到按分量做动画
#[cold]
#[inline(never)]
fn settle_mix(slot: &mut AnimSlot) -> PropertyValue {
    let to = slot.mix.as_ref().expect("正在混合").target();
    if to.is_simple() {
        let mut components = [0.0; MAX_COMPONENTS];
        let len = to.to_components(&mut components);
        slot.track = Some(slot.rest(&components[..len]));
        slot.mix = None;
    }
    to
}
