//! 合成层。
//!
//! 声明了变换或透明度动画的元素是一个合成层：它和子孙的图元放在层内坐标里，
//! 上屏时再经过层的变换和透明度。层的动画由渲染线程推进，主线程不用重建图元，
//! 主线程卡住时动画照样流畅。
//!
//! 主线程在显示列表里给出层的结构和当前值（[`LayerInfo`]），属性目标变化时把动画的轨道（[`CurveTrack`]，
//! 纯数据的描述：曲线、锚点的时刻、位置、速度、目标）整份发给渲染线程（[`LayerTrack`]）；渲染线程的
//! [`Compositor`] 换上它，每帧按同一个动画时钟、同一个宿主时刻采样，算出每层的变换（[`compose`]）。
//! 主线程按同一个时刻采样同一条轨道，只用于命中测试和判断退场何时结束，两边的值一样。
//!
//! 滚动偏移（和滚动条的位置、透明度）是层的额外平移和透明度：滚动时不重新布局、不重建图元。
//! 滚动的物理（[`Physics`]）由渲染线程在主线程的时间线上推进（主线程每帧给出这一帧的时间），
//! 主线程在帧与帧之间改了状态时把整份发过来（[`ScrollState`]）；主线程卡住时渲染线程按自己的钟接着推进，
//! 把推进到的状态交回主线程（[`Handback`]）。
//!
//! 绕 x、y 轴的旋转和透视让层的变换成为投影变换（[`Transform`]），共享元素的飞行是层在屏幕上的额外平移、缩放和透明度
//! （[`LayerUpdates::flights`]）。这些只有用到的层才有（[`LayerDepth`]），平的层和以前一样只算仿射变换。
//!
//! 关键帧时间线里动合成层变换和透明度的轨道也交给渲染线程（[`LayerTimeline`]）：主线程只在开始、暂停、
//! 跳转这些时候发一次轨道连同播放头（锚在动画时钟上），渲染线程每帧按同一个时刻算值，主线程不用每帧参与。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use animation::{Clock, CurveTrack, KeyframePlacement, KeyframePlayback, MAX_COMPONENTS, TrackState, Playhead, Track};
use scene::LayerTransform;

use crate::property::PropId;
use crate::geometry::{Affine, DEFAULT_PERSPECTIVE, Local, Transform, inverse_distance};
use crate::scroll::{Physics, snap};

/// 由渲染线程执行的属性（属性表里标了 `compositor` 的那些）：只影响变换和透明度，不影响布局和图元内容。
/// 这里定的是每层动画槽位的排法：前 [`FLAT`] 个是平面的，后面是 3D 的（只有用到的层才分配它们的位置）。
/// 和属性表一致由测试核对
const PROPERTIES: [PropId; 8] = [
    PropId::OPACITY,
    PropId::SCALE,
    PropId::ROTATION,
    PropId::TRANSLATION_X,
    PropId::TRANSLATION_Y,
    PropId::ROTATION_X,
    PropId::ROTATION_Y,
    PropId::PERSPECTIVE,
];
const FLAT: usize = 5;
/// 3D 的位置：3 个属性加上共享元素的飞行
const DEPTH: usize = PROPERTIES.len() - FLAT + 1;
const FLIGHT: usize = DEPTH - 1;

/// 共享元素飞行的分量：屏幕上的平移 (dx, dy)、缩放 (sx, sy)、透明度。缩放和透明度相对 1 的差放大这么多倍，
/// 和平移（逻辑像素）共用一个停稳的误差（0.01）
const FLIGHT_GAIN: f32 = 100.0;
/// 不飞时的值：不平移、不缩放、不透明
pub(crate) const FLIGHT_IDENTITY: [f32; 5] = [0.0, 0.0, 1.0, 1.0, 1.0];
/// 飞行动画停稳的误差
pub(crate) const FLIGHT_TOLERANCE: f32 = 1e-2;

/// 飞行的值 -> 动画分量
pub(crate) fn flight_components([dx, dy, sx, sy, alpha]: [f32; 5]) -> [f32; 5] {
    [dx, dy, (sx - 1.0) * FLIGHT_GAIN, (sy - 1.0) * FLIGHT_GAIN, (1.0 - alpha) * FLIGHT_GAIN]
}

/// 动画分量 -> 飞行的值
pub(crate) fn flight_value(c: &[f32]) -> [f32; 5] {
    [c[0], c[1], 1.0 + c[2] / FLIGHT_GAIN, 1.0 + c[3] / FLIGHT_GAIN, (1.0 - c[4] / FLIGHT_GAIN).clamp(0.0, 1.0)]
}

/// 飞行分量的速度 -> 平移、缩放、透明度每秒的变化
pub(crate) fn flight_rate(v: &[f32]) -> [f32; 5] {
    [v[0], v[1], v[2] / FLIGHT_GAIN, v[3] / FLIGHT_GAIN, -v[4] / FLIGHT_GAIN]
}

/// 平移、缩放、透明度每秒的变化 -> 飞行分量的速度
pub(crate) fn flight_velocity(rate: [f32; 5]) -> [f32; 5] {
    [rate[0], rate[1], rate[2] * FLIGHT_GAIN, rate[3] * FLIGHT_GAIN, -rate[4] * FLIGHT_GAIN]
}

pub(crate) fn is_compositor(property: PropId) -> bool {
    property.metadata().animation.is_some_and(|a| a.composited)
}

/// 层自身的变换和透明度
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LayerValues {
    pub translate: [f32; 2],
    pub scale: f32,
    pub rotation: f32,
    pub opacity: f32,
    /// 滚动带来的额外平移（父层坐标），和 `translate` 相加；不做动画，主线程每帧给出
    pub offset: [f32; 2],
}

impl LayerValues {
    fn set(&mut self, property: PropId, value: f32) {
        match property {
            PropId::OPACITY => self.opacity = value,
            PropId::SCALE => self.scale = value,
            PropId::ROTATION => self.rotation = value,
            PropId::TRANSLATION_X => self.translate[0] = value,
            PropId::TRANSLATION_Y => self.translate[1] = value,
            _ => {}
        }
    }
}

/// 层的 3D 旋转、透视和飞行的值
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DepthValues {
    /// 绕 x 轴、绕 y 轴，弧度
    pub rotate: [f32; 2],
    /// 自己的观察距离，逻辑像素；0 表示没设
    pub perspective: f32,
    /// 共享元素飞行时在屏幕上额外的平移、缩放和透明度（[`FLIGHT_IDENTITY`] 是不飞）
    pub flight: [f32; 5],
}

impl DepthValues {
    fn set(&mut self, index: usize, value: &[f32]) {
        match index {
            0 => self.rotate[0] = value[0],
            1 => self.rotate[1] = value[0],
            2 => self.perspective = value[0],
            FLIGHT => self.flight = flight_value(value),
            _ => {}
        }
    }
}

/// 层的 3D 设置：只有转过 x、y 轴、背面不可见、正在飞的层才有，平的层不用为它付出什么
#[derive(Clone, Debug)]
pub(crate) struct LayerDepth {
    /// 生成显示列表时的值；渲染线程有这一项的动画时以动画为准
    pub values: DepthValues,
    /// 父元素给子元素的透视（[`crate::Element::child_perspective`]）：视点（层坐标）和观察距离的倒数
    pub eye: Option<([f32; 2], f32)>,
    /// 元素自己的框的中心（层坐标），飞行时绕它缩放
    pub middle: [f32; 2],
    /// 背朝屏幕时不画（[`crate::Element::backface_hidden`]）
    pub backface_hidden: bool,
    /// 正在飞：画在最上面、不受祖先的裁剪，透明度不继承祖先的
    pub lifted: bool,
}

impl LayerDepth {
    /// 这一层自己的变换（父层坐标里）
    fn local(&self, center: [f32; 2], translate: [f32; 2], v: &LayerValues, d: &DepthValues) -> Transform {
        let (eye, k) = match inverse_distance(d.perspective) {
            Some(k) => (center, k),
            None => self.eye.unwrap_or((center, 1.0 / DEFAULT_PERSPECTIVE)),
        };
        Local { origin: center, translate, scale: v.scale, rotation: v.rotation, rotate: d.rotate, eye, k }.transform()
    }
}

/// 飞行：在屏幕上绕框的中心（投到屏幕上的位置）缩放，再平移
pub(crate) fn fly(world: Transform, middle: [f32; 2], [dx, dy, sx, sy, _]: [f32; 5]) -> Transform {
    if [dx, dy, sx, sy] == [0.0, 0.0, 1.0, 1.0] {
        return world;
    }
    let [x, y] = world.apply(middle);
    let shift = Affine { linear: [sx, 0.0, 0.0, sy], offset: [x + dx - sx * x, y + dy - sy * y] };
    Transform::from(shift).compose(&world)
}

/// 显示列表里的一个层（层号从 1 开始，0 是根层）
#[derive(Clone, Debug)]
pub struct LayerInfo {
    /// 稳定的标识（元素节点），渲染线程用它关联动画
    pub(crate) key: u64,
    /// 父层的层号
    pub(crate) parent: u32,
    /// 把层坐标（未做自身变换）映射到父层坐标的静态变换：父层之内、本层之外的祖先变换
    pub(crate) base: Affine,
    /// 父层之内、本层之外的祖先透明度之积
    pub(crate) base_opacity: f32,
    /// 自身变换的中心，层坐标
    pub(crate) center: [f32; 2],
    /// 生成显示列表时的值；渲染线程有这一属性的动画时以动画为准
    pub(crate) values: LayerValues,
    /// 这一层是一个分组：先画到离屏纹理再整体合成，组内图元的透明度从 1 重新累积
    pub(crate) group: bool,
    /// 3D 旋转、背面、飞行（用到时才有）
    pub(crate) depth: Option<Box<LayerDepth>>,
}

/// 发给渲染线程的一条合成层动画的轨道（改了目标、跳到新值、从时间线接手时）：渲染线程整个换掉自己那条。
/// 轨道是时刻的纯函数，两边按同一个时刻采样，所以换了也不跳
#[derive(Clone, Debug)]
pub struct LayerTrack {
    pub(crate) key: u64,
    pub(crate) property: PropId,
    pub(crate) track: CurveTrack,
}

/// 时间线在这个窗口里由渲染线程画的部分：合成层上的变换、透明度的目标（主线程每个目标的 `KeyframePlayback`
/// 拆成各自的几段关键帧和共用的一个播放头）。只在开始、暂停、跳转、轨道增减时发，渲染线程整个换掉这条时间线
#[derive(Clone, Debug)]
pub struct LayerTimeline {
    pub(crate) id: u64,
    /// 主线程此刻的播放头（锚在动画时钟上）
    pub(crate) playhead: Playhead,
    /// 空的表示这条时间线不再动这个窗口的合成层
    pub(crate) targets: Arc<[LayerTarget]>,
}

/// 时间线动的一个合成层属性：它在时间线上的几段关键帧（减少动态效果时直接是最后的值）
#[derive(Clone, Debug)]
pub(crate) struct LayerTarget {
    pub key: u64,
    pub property: PropId,
    pub reduced: bool,
    pub segments: Arc<[KeyframePlacement]>,
}

impl LayerTarget {
    /// 时间线的位置 `position` 上的值
    #[inline]
    fn value(&self, position: f64) -> f32 {
        let mut out = [0.0; MAX_COMPONENTS];
        KeyframePlayback::sample_segments(&self.segments, self.reduced, position, &mut out);
        out[0]
    }
}

/// 自上一帧以来合成层的变化，随每帧消息交给渲染线程
#[derive(Debug, Default)]
pub struct LayerUpdates {
    pub(crate) tracks: Vec<LayerTrack>,
    /// 时间线的轨道和播放头（见 [`LayerTimeline`]）
    pub(crate) timelines: Vec<LayerTimeline>,
    /// 卸载了的层：它们的动画一起丢掉（否则一直转的动画会让渲染线程永远停不下来）
    pub(crate) removed: Vec<u64>,
    /// 层的新额外平移（[`LayerValues::offset`]，布局位置的过渡、浮层的位置）：只更新层变换、不重建图元。
    /// 滚动视口的内容层不含滚动偏移，那一部分由渲染线程按 [`ScrollState`] 加上
    pub(crate) offsets: Vec<(u64, [f32; 2])>,
    /// 层的新透明度（静态地改了半透明元素的透明度，没重建图元也没重建层表）：换层表之前以它为准
    pub(crate) opacities: Vec<(u64, f32)>,
    /// 共享元素开始飞（或者飞到一半换了方向）：整个替换掉渲染线程那边的飞行
    pub(crate) flights: Vec<(u64, CurveTrack)>,
    /// 这一帧的宿主时刻（秒，主线程的时间线）：滚动的物理推进到这里，合成层的动画按动画时钟换算它
    pub time: f64,
    /// 主线程此刻的动画时钟：渲染线程用它把宿主时刻换算成动画时间，和主线程算出同一个时间
    pub(crate) clock: Clock,
    /// 主线程发出的第几帧
    pub(crate) frame: u64,
    /// 在帧与帧之间改过的滚动状态（推进到了 `time`），渲染线程照单全收
    pub(crate) scrolls: Vec<ScrollState>,
}

/// 一个滚动视口的完整物理状态，主线程改过它时随这一帧发给渲染线程
#[derive(Clone, Debug)]
pub struct ScrollState {
    /// 视口节点的键；滚动条的层键由它算出（见 `tree::scrolling::thumb_key`）
    pub(crate) viewport: u64,
    /// 内容层的键
    pub(crate) content: u64,
    pub(crate) physics: Physics,
}

/// 一个节点除了自己的层（键就是节点的键）以外，还可能同时有的几种层。键是节点的键加上最高四位的标记
/// （slotmap 的版本号到不了这么大）：标记只在这里取，别处不另外取位，一个节点的几个层的键才不会撞
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DerivedLayer {
    /// 拖放影子的投影
    GhostShadow = 0b0001,
    /// 拖放的影子
    Ghost = 0b0010,
    /// 自己滚的元素（多行输入框）画内容的那一层
    OwnContent = 0b0011,
    /// 横向滚动条
    ThumbX = 0b1000,
    /// 竖向滚动条
    ThumbY = 0b1100,
}

impl DerivedLayer {
    /// 节点的键是 `node` 时这一层的键
    pub(crate) const fn key(self, node: u64) -> u64 {
        node | (self as u64) << 60
    }
}

/// 滚动条的层键里的标记：最高位说明是滚动条，下一位是方向（见 [`DerivedLayer`]）
const THUMB_BITS: u64 = 0b11 << 62;

/// 主线程卡住时渲染线程推进到的滚动状态，交回主线程：恢复后命中测试、挂载虚拟列表的行用屏幕上的偏移，
/// 接着推进时也从这里开始，不回跳
#[derive(Debug, Default)]
pub struct Handback {
    fresh: AtomicBool,
    snapshot: Mutex<Option<Snapshot>>,
}

/// 渲染线程某一刻的滚动状态
#[derive(Debug)]
pub(crate) struct Snapshot {
    /// 渲染线程收到的最后一帧：主线程在那之后又发过帧（里面可能有新的输入）时，这份已经过时
    pub frame: u64,
    /// 推进到的时间（主线程的时间线）
    pub time: f64,
    /// 自那一帧以来渲染线程自己推进过的视口：(视口的键, 物理状态)
    pub scrolls: Vec<(u64, Physics)>,
}

impl Handback {
    /// 渲染线程：放进最新的状态（覆盖上一份）
    pub(crate) fn publish(&self, snapshot: Snapshot) {
        if let Ok(mut slot) = self.snapshot.lock() {
            *slot = Some(snapshot);
            self.fresh.store(true, Ordering::Release);
        }
    }

    /// 主线程：取走最新的状态；没有新的时只读一个原子量
    pub(crate) fn take(&self) -> Option<Snapshot> {
        if !self.fresh.swap(false, Ordering::Acquire) {
            return None;
        }
        self.snapshot.lock().ok()?.take()
    }
}

/// 层表里有没有带 3D 设置的层（没有时用 [`compose_flat`]）
pub(crate) fn has_depth(layers: &[LayerInfo]) -> bool {
    layers.iter().any(|layer| layer.depth.is_some())
}

/// 算出每层上屏的变换。返回值下标 0 是根层，下标 k 对应 `layers[k - 1]`。
/// `values` 给出层此刻的值，`depth` 给出有 3D 设置的层此刻的 3D 值。层表里没有 3D 设置时用 [`compose_flat`]
pub(crate) fn compose(
    layers: &[LayerInfo],
    values: impl Fn(usize, &LayerInfo) -> LayerValues,
    depth: impl Fn(usize, &LayerInfo, &LayerDepth) -> DepthValues,
) -> Vec<LayerTransform> {
    let mut transforms = Vec::with_capacity(layers.len() + 1);
    let mut worlds = Vec::with_capacity(layers.len() + 1);
    transforms.push(LayerTransform::IDENTITY);
    worlds.push((Transform::IDENTITY, 1.0));
    for (i, layer) in layers.iter().enumerate() {
        let v = values(i, layer);
        let (parent, parent_opacity) = worlds[layer.parent as usize];
        let translate = [v.translate[0] + v.offset[0], v.translate[1] + v.offset[1]];
        let base = parent.then_affine(&layer.base);
        let (world, opacity) = match layer.depth.as_deref() {
            None => (base.then_local(layer.center, translate, v.scale, v.rotation), parent_opacity * layer.base_opacity * v.opacity),
            Some(d) => {
                let dv = depth(i, layer, d);
                let world = fly(base.compose(&d.local(layer.center, translate, &v, &dv)), d.middle, dv.flight);
                // 飞着的元素不跟着祖先淡入淡出；背面不可见的层翻过去时连同子孙一起不画
                let inherited = if d.lifted { 1.0 } else { parent_opacity * layer.base_opacity };
                let hidden = d.backface_hidden && world.faces_back(layer.center);
                (world, if hidden { 0.0 } else { inherited * v.opacity * dv.flight[4] })
            }
        };
        // 分组：整组按 `opacity` 合成，组内从 1 开始
        let inner = if layer.group { 1.0 } else { opacity };
        worlds.push((world, inner));
        transforms.push(LayerTransform { opacity: inner, group_opacity: opacity, ..world.layer() });
    }
    transforms
}

/// 只有平的层：和以前一样只算仿射变换，平的界面不为 3D 付出什么。父层的变换和透明度直接读已经算好的结果
/// （平的层的结果就是它的仿射变换和组内的透明度），不另存一份
#[inline(always)]
pub(crate) fn compose_flat(layers: &[LayerInfo], values: impl Fn(usize, &LayerInfo) -> LayerValues) -> Vec<LayerTransform> {
    let mut transforms = Vec::with_capacity(layers.len() + 1);
    transforms.push(LayerTransform::IDENTITY);
    for (i, layer) in layers.iter().enumerate() {
        let v = values(i, layer);
        let p = &transforms[layer.parent as usize];
        let (parent, parent_opacity) = (Affine { linear: p.linear, offset: p.offset }, p.opacity);
        let translate = [v.translate[0] + v.offset[0], v.translate[1] + v.offset[1]];
        let world = parent.compose(&layer.base).then_local(layer.center, translate, v.scale, v.rotation);
        let opacity = parent_opacity * layer.base_opacity * v.opacity;
        let inner = if layer.group { 1.0 } else { opacity };
        transforms.push(LayerTransform {
            linear: world.linear,
            offset: world.offset,
            opacity: inner,
            group_opacity: opacity,
            projective: LayerTransform::FLAT,
        });
    }
    transforms
}

/// 渲染线程这一侧：保存层结构和动画，用自己的时钟推进。
///
/// 层的状态（动画此刻的值、主线程给的额外平移、时间线接管的属性）放在密集的槽位里（`LayerState`），槽位跨显示列表稳定：
/// 按层的键找槽位不查散列表（普通的层按节点在存储里的下标，滚动条、拖放的影子这类派生的键才查一张小表），
/// 换层表时把每层对上它的槽位（`bind`），每帧算变换时直接按下标读。
/// 动画的轨道另放一张表（`TrackSlot`），只推进在动的那些（`active`）
#[derive(Default)]
pub struct Compositor {
    layers: Arc<Vec<LayerInfo>>,
    /// 层表里没有带 3D 设置的层
    flat: bool,
    /// 每层的状态，下标是槽位；空出来的槽位记在 `free_states`
    states: Vec<LayerState>,
    free_states: Vec<u32>,
    /// 层的键 → 槽位
    index: SlotIndex,
    /// 轨道，下标记在层的状态里；空出来的记在 `free_tracks`
    tracks: Vec<TrackSlot>,
    free_tracks: Vec<u32>,
    /// 在动的轨道：每帧只推进它们
    active: Vec<u32>,
    /// 层表里每层对应的状态（和 `layers` 一一对应）：层表、槽位、滚动视口变了以后重新对上
    bound: Vec<Bound>,
    /// `bound` 要重新对上
    unbound: bool,
    /// 有主线程给的额外平移、透明度的槽位（换层表时清掉）
    with_offset: Vec<u32>,
    /// 有被时间线接管的属性的槽位（时间线变了时清掉重记）
    with_owned: Vec<u32>,
    /// 时间线：渲染线程自己的播放头
    timelines: Vec<RenderTimeline>,
    /// 滚动视口的物理状态，按视口的键
    scrolls: HashMap<u64, Scroll>,
    /// 内容层的键 → 视口的键
    contents: HashMap<u64, u64>,
    /// 滚动推进到的时间（主线程的时间线）
    time: Option<f64>,
    /// 收到的最后一帧
    frame: u64,
    /// 主线程卡住时把推进到的滚动状态交回去
    handback: Option<Arc<Handback>>,
    /// 主线程给的动画时钟（每帧随消息来）
    clock: Clock,
    /// 合成层的动画推进到的动画时间
    animated: Option<f64>,
}

/// 没有（槽位、轨道）
const NONE: u32 = u32::MAX;

/// 层的键 → 槽位。普通的层（键是节点的键）按节点在存储里的下标直接找，不查散列表；滚动条、拖放的影子、
/// 自己滚的元素的内容层这些派生的键（高 4 位有标记）很少，放进一张小表
#[derive(Default)]
struct SlotIndex {
    by_node: Vec<u32>,
    derived: HashMap<u64, u32>,
}

impl SlotIndex {
    /// 派生的键：高 4 位有标记（滚动条、拖放的影子、自己滚的内容层）
    #[inline(always)]
    fn derived(key: u64) -> bool {
        key >> 60 != 0
    }

    #[inline(always)]
    fn get(&self, key: u64, states: &[LayerState]) -> Option<u32> {
        if Self::derived(key) {
            return self.derived.get(&key).copied();
        }
        // 同一个下标上可能是这个节点之前的版本：键要完全一样
        let slot = *self.by_node.get(key as u32 as usize)?;
        (slot != NONE && states[slot as usize].key == key).then_some(slot)
    }

    fn insert(&mut self, key: u64, slot: u32) {
        if Self::derived(key) {
            self.derived.insert(key, slot);
            return;
        }
        let i = key as u32 as usize;
        if self.by_node.len() <= i {
            self.by_node.resize(i + 1, NONE);
        }
        self.by_node[i] = slot;
    }

    fn remove(&mut self, key: u64, slot: u32) {
        if Self::derived(key) {
            self.derived.remove(&key);
        } else if let Some(s) = self.by_node.get_mut(key as u32 as usize).filter(|s| **s == slot) {
            *s = NONE;
        }
    }
}

/// 一个层被时间线接管的属性：（时间线下标，目标下标），按 [`PROPERTIES`] 的顺序
type Owners = [Option<(u32, u32)>; PROPERTIES.len()];

/// 一个层在渲染线程上的状态
struct LayerState {
    key: u64,
    /// 平面属性里有动画的（按 [`PROPERTIES`] 的顺序一位一个）和它们推进到那一刻的值
    animated: u8,
    values: [f32; FLAT],
    /// 平面属性的轨道（[`Compositor::tracks`] 的下标），没有是 [`NONE`]
    tracks: [u32; FLAT],
    /// 主线程给的额外平移，比显示列表里的新
    offset: Option<[f32; 2]>,
    /// 主线程给的透明度（静态改的），比显示列表里的新
    opacity: Option<f32>,
    /// 被时间线接管的属性：（时间线下标，目标下标），按 [`PROPERTIES`] 的顺序（用到才分配）
    owned: Option<Box<Owners>>,
    /// 3D 属性和飞行（用到才分配）
    depth: Option<Box<DepthState>>,
}

impl LayerState {
    fn new(key: u64) -> Self {
        Self { key, animated: 0, values: [0.0; FLAT], tracks: [NONE; FLAT], offset: None, opacity: None, owned: None, depth: None }
    }
}

/// 3D 属性和飞行的轨道和推进到那一刻的值
struct DepthState {
    animated: u8,
    values: [[f32; 5]; DEPTH],
    tracks: [u32; DEPTH],
}

/// 一条合成层动画：轨道，属于哪个槽位的哪个属性（[`PROPERTIES`] 的下标，3D 的从 [`FLAT`] 起，飞行是 `FLAT + FLIGHT`），
/// 还在不在动。停下的不再采样，值留在槽位里当这一属性的当前值
struct TrackSlot {
    state: u32,
    property: u8,
    track: CurveTrack,
    moving: bool,
}

/// 层表里一层对上的状态：槽位、滚动视口（内容层、滚动条按它的物理状态放）
#[derive(Clone, Copy)]
struct Bound {
    state: u32,
    scroll: ScrollLink,
}

/// 层和滚动视口的关系（换层表时查一次，每帧不用查表）
#[derive(Clone, Copy)]
enum ScrollLink {
    None,
    /// 视口的内容层：按视口的键找物理状态
    Content(u64),
    /// 滚动条：视口的键，哪个方向
    Thumb(u64, usize),
}

struct Scroll {
    physics: Physics,
    /// 收到最后一帧之后渲染线程自己推进过：主线程还不知道，要交回去
    stepped: bool,
}

struct RenderTimeline {
    id: u64,
    targets: Arc<[LayerTarget]>,
    /// 各目标共用的播放头
    playhead: Playhead,
    /// 有不是减少动态效果的目标（那些直接是最后的值，不算在动）
    animated: bool,
    /// 推进到的那一刻播放头的位置
    position: f64,
}

impl RenderTimeline {
    fn new(update: LayerTimeline, time: f64) -> Self {
        let LayerTimeline { id, playhead, targets } = update;
        let animated = targets.iter().any(|t| !t.reduced);
        Self { id, targets, playhead, animated, position: playhead.position_at(time) }
    }

    /// 动画时间 `time` 还在动
    fn is_moving(&self, time: f64) -> bool {
        self.animated && self.playhead.direction_at(time) != 0.0
    }
}

impl Compositor {
    /// 主线程卡住时把推进到的滚动状态放进 `handback`
    pub fn new(handback: Option<Arc<Handback>>) -> Self {
        Self { handback, ..Default::default() }
    }

    /// 新的显示列表里已经是最新的额外平移、透明度，之前收到的作废（同一帧随后到的会再应用一次）
    pub fn set_layers(&mut self, layers: Arc<Vec<LayerInfo>>) {
        self.flat = !has_depth(&layers);
        self.layers = layers;
        for slot in std::mem::take(&mut self.with_offset) {
            if let Some(state) = self.states.get_mut(slot as usize) {
                state.offset = None;
                state.opacity = None;
            }
        }
        self.unbound = true;
        self.bind();
    }

    /// 键对应的槽位，没有就开一个
    fn slot(&mut self, key: u64) -> u32 {
        if let Some(slot) = self.index.get(key, &self.states) {
            return slot;
        }
        let state = LayerState::new(key);
        let slot = match self.free_states.pop() {
            Some(slot) => {
                self.states[slot as usize] = state;
                slot
            }
            None => {
                self.states.push(state);
                self.states.len() as u32 - 1
            }
        };
        self.index.insert(key, slot);
        // 层表里可能有这个键的层：重新对上
        self.unbound = true;
        slot
    }

    /// 槽位 `slot` 的属性 `property`（[`TrackSlot::property`] 的编号）换上新的轨道，在动画时间 `now` 采样一次
    fn set_track(&mut self, slot: u32, property: usize, track: CurveTrack, now: f64) {
        let mut out = [0.0; MAX_COMPONENTS];
        let moving = track.sample(now, &mut out) == TrackState::Moving;
        let state = &mut self.states[slot as usize];
        let existing = if property < FLAT {
            state.animated |= 1 << property;
            state.values[property] = out[0];
            state.tracks[property]
        } else {
            let depth = state.depth.get_or_insert_with(|| Box::new(DepthState { animated: 0, values: [[0.0; 5]; DEPTH], tracks: [NONE; DEPTH] }));
            let index = property - FLAT;
            depth.animated |= 1 << index;
            depth.values[index] = [out[0], out[1], out[2], out[3], out[4]];
            depth.tracks[index]
        };
        let entry = TrackSlot { state: slot, property: property as u8, track, moving };
        let index = if existing != NONE {
            let was = self.tracks[existing as usize].moving;
            self.tracks[existing as usize] = entry;
            if moving && !was {
                self.active.push(existing);
            } else if was && !moving {
                self.active.retain(|&t| t != existing);
            }
            existing
        } else {
            let index = match self.free_tracks.pop() {
                Some(index) => {
                    self.tracks[index as usize] = entry;
                    index
                }
                None => {
                    self.tracks.push(entry);
                    self.tracks.len() as u32 - 1
                }
            };
            if moving {
                self.active.push(index);
            }
            index
        };
        let state = &mut self.states[slot as usize];
        match property {
            p if p < FLAT => state.tracks[p] = index,
            p => state.depth.as_mut().expect("上面分配过").tracks[p - FLAT] = index,
        }
    }

    /// 层卸载了：槽位和它的轨道还回去
    fn free(&mut self, key: u64) {
        let Some(slot) = self.index.get(key, &self.states) else { return };
        self.index.remove(key, slot);
        let state = std::mem::replace(&mut self.states[slot as usize], LayerState::new(0));
        let depth_tracks = state.depth.as_ref().map_or([NONE; DEPTH], |d| d.tracks);
        let mut freed = false;
        for track in state.tracks.into_iter().chain(depth_tracks).filter(|&t| t != NONE) {
            self.tracks[track as usize].moving = false;
            self.tracks[track as usize].state = NONE;
            self.free_tracks.push(track);
            freed = true;
        }
        if freed {
            self.active.retain(|&t| self.tracks[t as usize].state != NONE);
        }
        self.free_states.push(slot);
        self.unbound = true;
    }

    /// 层表里每层对上它的槽位和滚动视口（层表、槽位、滚动视口变了以后）。普通的层按下标找，不查散列表
    fn bind(&mut self) {
        if !self.unbound {
            return;
        }
        self.unbound = false;
        self.bound.clear();
        self.bound.extend(self.layers.iter().map(|layer| {
            let state = self.index.get(layer.key, &self.states).unwrap_or(NONE);
            let scroll = if self.scrolls.is_empty() {
                ScrollLink::None
            } else if layer.key & THUMB_BITS != 0 {
                ScrollLink::Thumb(layer.key & !THUMB_BITS, ((layer.key >> 62) & 1) as usize)
            } else {
                self.contents.get(&layer.key).map_or(ScrollLink::None, |&viewport| ScrollLink::Content(viewport))
            };
            Bound { state, scroll }
        }));
    }

    /// 主线程换了轨道（改目标、跳到新值、从时间线接手）：整个换掉，在推进到的那一刻采样一次。飞行也是。
    /// 卸载了的层丢掉动画。返回滚动偏移或者动画时钟有没有变（变了要重画，即使没有动画在动）
    pub fn apply(&mut self, updates: LayerUpdates) -> bool {
        let clock_changed = self.clock != updates.clock;
        if clock_changed {
            // 时钟改过（拖动、暂停、改倍速）：拖动过的话停下的轨道在新的时刻可能还在动，全部重新采样（下面推进到这一帧的时刻）
            self.clock = updates.clock;
            self.active.clear();
            for (i, track) in self.tracks.iter_mut().enumerate() {
                if track.state != NONE {
                    track.moving = true;
                    self.active.push(i as u32);
                }
            }
        }
        let now = self.animated.unwrap_or(f64::NEG_INFINITY);
        for update in updates.tracks {
            let Some(index) = PROPERTIES.iter().position(|&p| p == update.property) else { continue };
            let slot = self.slot(update.key);
            self.set_track(slot, index, update.track, now);
        }
        if !updates.timelines.is_empty() {
            for update in updates.timelines {
                let existing = self.timelines.iter().position(|t| t.id == update.id);
                match existing {
                    Some(i) if update.targets.is_empty() => {
                        self.timelines.swap_remove(i);
                    }
                    // 换上新的轨道和播放头：位置是时刻的纯函数，和主线程按同一个时刻算出同一个值
                    Some(i) => self.timelines[i] = RenderTimeline::new(update, self.animation_time()),
                    None if update.targets.is_empty() => {}
                    None => self.timelines.push(RenderTimeline::new(update, self.animation_time())),
                }
            }
            for slot in std::mem::take(&mut self.with_owned) {
                if let Some(state) = self.states.get_mut(slot as usize) {
                    state.owned = None;
                }
            }
            let targets: Vec<(u64, usize, u32, u32)> = self
                .timelines
                .iter()
                .enumerate()
                .flat_map(|(t, timeline)| {
                    timeline.targets.iter().enumerate().filter_map(move |(k, target)| {
                        let index = PROPERTIES.iter().position(|&p| p == target.property)?;
                        Some((target.key, index, t as u32, k as u32))
                    })
                })
                .collect();
            for (key, index, t, k) in targets {
                let slot = self.slot(key);
                let state = &mut self.states[slot as usize];
                if state.owned.is_none() {
                    self.with_owned.push(slot);
                }
                state.owned.get_or_insert_with(Default::default)[index] = Some((t, k));
            }
        }
        for (key, track) in updates.flights {
            let slot = self.slot(key);
            self.set_track(slot, FLAT + FLIGHT, track, now);
        }
        let mut moved = false;
        for (key, offset) in updates.offsets {
            let slot = self.slot(key);
            let state = &mut self.states[slot as usize];
            if state.offset.is_none() && state.opacity.is_none() {
                self.with_offset.push(slot);
            }
            moved |= state.offset.replace(offset) != Some(offset);
        }
        for (key, opacity) in updates.opacities {
            let slot = self.slot(key);
            let state = &mut self.states[slot as usize];
            if state.offset.is_none() && state.opacity.is_none() {
                self.with_offset.push(slot);
            }
            moved |= state.opacity.replace(opacity) != Some(opacity);
        }
        // 主线程看到的是这一帧之前的状态：自己推进过的这下都交接完了
        self.frame = updates.frame;
        for scroll in self.scrolls.values_mut() {
            scroll.stepped = false;
        }
        for ScrollState { viewport, content, mut physics } in updates.scrolls {
            // 主线程的状态停在这一帧的时间；渲染线程已经走得更远（主线程卡住时自己出过帧）时补上差的这一段
            if let Some(time) = self.time
                && time > updates.time
            {
                physics.step((time - updates.time) as f32, time);
            }
            self.unbound |= self.contents.insert(content, viewport) != Some(viewport);
            self.unbound |= self.scrolls.insert(viewport, Scroll { physics, stepped: false }).is_none();
            moved = true;
        }
        for key in updates.removed {
            self.free(key);
            let viewport = if key & THUMB_BITS != 0 { Some(key & !THUMB_BITS) } else { self.contents.remove(&key) };
            if let Some(viewport) = viewport
                && self.scrolls.remove(&viewport).is_some()
            {
                self.contents.retain(|_, v| *v != viewport);
                self.unbound = true;
            }
        }
        self.bind();
        // 时钟改过：时钟停着时渲染线程不推进动画，这里推进到这一帧的时刻、重画一次（停着拖动时屏幕也跟着走）
        if clock_changed {
            self.advance(updates.time);
            moved = true;
        }
        moved
    }

    /// 还要每帧推进：合成层动画在动，或者有视口在滚（或者滚动条还显示着）
    pub fn is_animating(&self) -> bool {
        self.has_moving_layers() || self.scrolls.values().any(|s| s.physics.needs_frames())
    }

    /// 合成层动画在动（含时间线、3D 和飞行）。动画时钟停着时什么都不会动
    pub fn has_moving_layers(&self) -> bool {
        if !self.clock.is_running() {
            return false;
        }
        let time = self.animation_time();
        !self.active.is_empty() || self.timelines.iter().any(|t| t.is_moving(time))
    }

    /// 滚动的物理推进到 `time`（主线程的时间线：平时是主线程这一帧的时间，主线程卡住时按渲染线程的钟往后推）。
    /// 返回有没有视口的偏移或者滚动条变了（要重画）
    pub fn advance_scrolls(&mut self, time: f64) -> bool {
        let last = *self.time.get_or_insert(time);
        if time <= last {
            return false;
        }
        self.time = Some(time);
        let dt = (time - last) as f32;
        let mut changed = false;
        for scroll in self.scrolls.values_mut() {
            let physics = &mut scroll.physics;
            if !physics.needs_frames() {
                continue;
            }
            let before = (physics.offset, physics.bar_opacity());
            physics.step(dt, time);
            scroll.stepped = true;
            changed |= before != (physics.offset, physics.bar_opacity());
        }
        changed
    }

    /// 主线程卡住时：把自己推进过的滚动状态交回主线程
    pub fn publish(&self) {
        let Some(handback) = &self.handback else { return };
        let scrolls: Vec<(u64, Physics)> = self.scrolls.iter().filter(|(_, s)| s.stepped).map(|(&key, s)| (key, s.physics.clone())).collect();
        if !scrolls.is_empty() {
            handback.publish(Snapshot { frame: self.frame, time: self.time.unwrap_or(0.0), scrolls });
        }
    }

    /// 合成层的动画推进到宿主时刻 `host`（主线程这一帧的时刻；主线程卡住时按收到它之后过去的时间往后推）：
    /// 按主线程给的动画时钟换算成动画时间，和主线程算出同一个时间。返回是否还有动画在动
    pub fn advance(&mut self, host: f64) -> bool {
        // 宿主时刻由调用方保证不往回走；动画时间可以往回（动画的时钟被拖回去时）
        let time = self.clock.now(host);
        self.animated = Some(time);
        self.step(time)
    }

    /// 合成层的动画推进到的动画时间（自定义着色器的时间也用它）
    pub fn animation_time(&self) -> f64 {
        self.animated.unwrap_or(0.0)
    }

    /// 动画时钟在走（暂停、倍速为 0 时合成层上什么都不会动）
    pub fn is_clock_running(&self) -> bool {
        self.clock.is_running()
    }

    /// 在动画时间 `t` 采样在动的轨道（只推进它们），值写进槽位；停下的从在动的表里拿掉，值留着当这一属性的当前值。
    /// 时间线的值在算层变换时按同一个时刻算。返回是否还有动画在动
    fn step(&mut self, t: f64) -> bool {
        let (tracks, states) = (&mut self.tracks, &mut self.states);
        self.active.retain(|&i| {
            let track = &mut tracks[i as usize];
            let mut out = [0.0; MAX_COMPONENTS];
            track.moving = track.track.sample(t, &mut out) == TrackState::Moving;
            let state = &mut states[track.state as usize];
            match track.property as usize {
                p if p < FLAT => state.values[p] = out[0],
                p => {
                    if let Some(depth) = state.depth.as_mut() {
                        depth.values[p - FLAT] = [out[0], out[1], out[2], out[3], out[4]];
                    }
                }
            }
            track.moving
        });
        let mut active = !self.active.is_empty();
        for timeline in &mut self.timelines {
            timeline.position = timeline.playhead.position_at(t);
            active |= timeline.is_moving(t);
        }
        active
    }

    /// 层此刻的值：显示列表里的值，渲染线程有动画、主线程给了新的额外平移时以它们为准；滚动视口的内容层和滚动条按渲染线程的滚动
    #[inline(always)]
    fn values(&self, layer: &LayerInfo, bound: Bound) -> LayerValues {
        let mut values = layer.values;
        if let Some(state) = self.states.get(bound.state as usize) {
            if let Some(offset) = state.offset {
                values.offset = offset;
            }
            if let Some(opacity) = state.opacity {
                values.opacity = opacity;
            }
            let mut animated = state.animated;
            while animated != 0 {
                let i = animated.trailing_zeros() as usize;
                values.set(PROPERTIES[i], state.values[i]);
                animated &= animated - 1;
            }
            // 时间线动着的属性以时间线为准
            if let Some(owned) = state.owned.as_deref() {
                for (property, owner) in PROPERTIES.iter().zip(&owned[..FLAT]) {
                    if let Some((t, k)) = *owner {
                        let timeline = &self.timelines[t as usize];
                        values.set(*property, timeline.targets[k as usize].value(timeline.position));
                    }
                }
            }
        }
        match bound.scroll {
            ScrollLink::None => {}
            ScrollLink::Content(viewport) => {
                if let Some(scroll) = self.scrolls.get(&viewport) {
                    let shown = scroll.physics.snapped();
                    values.offset = [-shown[0] + values.offset[0], -shown[1] + values.offset[1]];
                }
            }
            ScrollLink::Thumb(viewport, axis) => {
                if let Some(scroll) = self.scrolls.get(&viewport) {
                    let physics = &scroll.physics;
                    if let Some(position) = physics.thumb_position(axis) {
                        let p = snap(position, physics.scale);
                        values.offset = if axis == 0 { [p, 0.0] } else { [0.0, p] };
                    }
                    values.opacity = physics.bar_opacity();
                }
            }
        }
        values
    }

    /// 有 3D 设置的层此刻的 3D 值：显示列表里的值，渲染线程有动画时以它为准，时间线动着的以时间线为准
    fn depth_values(&self, depth: &LayerDepth, bound: Bound) -> DepthValues {
        let mut values = depth.values;
        let Some(state) = self.states.get(bound.state as usize) else { return values };
        if let Some(d) = state.depth.as_deref() {
            for (index, value) in d.values.iter().enumerate() {
                if d.animated & (1 << index) != 0 {
                    values.set(index, value);
                }
            }
        }
        // 时间线也能动绕 x、y 轴的旋转和透视
        if let Some(owned) = state.owned.as_deref() {
            for (index, owner) in owned[FLAT..].iter().enumerate() {
                if let Some((t, k)) = *owner {
                    let timeline = &self.timelines[t as usize];
                    values.set(index, &[timeline.targets[k as usize].value(timeline.position)]);
                }
            }
        }
        values
    }

    /// 每个层此刻的变换和透明度（层号从 1 开始，第 0 个是根层），交给渲染器
    pub fn transforms(&self) -> Vec<LayerTransform> {
        debug_assert!(!self.unbound && self.bound.len() == self.layers.len(), "层表还没对上槽位");
        if self.flat {
            return compose_flat(&self.layers, |i, layer| self.values(layer, self.bound[i]));
        }
        compose(&self.layers, |i, layer| self.values(layer, self.bound[i]), |i, _, depth| self.depth_values(depth, self.bound[i]))
    }
}
