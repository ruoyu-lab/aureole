//! 元素树里的滚动：把滚轮和触控板交给视口、每帧推进滚动、布局后更新滚动范围、画滚动条、拖滚动条。
//! 物理和对外的 API 在 [`crate::scroll`]。
//!
//! 滚动的物理主线程和渲染线程各推进一份（同一条时间线）。主线程在帧与帧之间改了某个视口的状态
//! （输入、程序化滚动、布局后的范围、锚点挪动）时，这一帧把整份交给渲染线程（`take_scroll_states`）；
//! 惯性、弹簧、回弹、滚动条的淡入淡出两边各走各的，不用每帧传偏移。主线程卡住时渲染线程接着推进，
//! 恢复后（处理积压的输入之前、推进下一帧之前）收回它推进到的状态（`adopt_scrolls`）。
//!
//! 手指拖动（触摸屏）和触控板共用这份物理：跟手、橡皮筋、松手的回弹和惯性都是同一段代码（`Pan`），
//! 手指的识别在 `route/touch.rs`。

use std::cell::RefCell;
use std::mem;
use std::rc::Rc;

use reactive::{Effect, Signal, batch};
use scene::{DisplayList, Quad};
use taffy::{AlignItems, Dimension, LengthPercentageAuto, Overflow, Point, Position};

use super::interaction::{KeyScroll, sub};
use super::route::{HitPath, Options, dispatch};
use super::{NodeId, Tree, TreeRef, layer_key};
use crate::event::{Builtin, PropagationPhase, ScrollEvent, WheelEvent};
use crate::handlers::{EventType, Handler, On};
use crate::input::Modifiers;
use crate::compositor::{DerivedLayer, LayerInfo, LayerValues, ScrollState};
use crate::editor::Viewport;
use crate::geometry::Affine;
use crate::input::GesturePhase;
use crate::scroll::{self, BAR_HOVER, BAR_THICKNESS, MOMENTUM_WINDOW, Metrics, Request, ScrollHandle, Scroller, WHEEL_LATCH, WheelDelta};
use crate::geometry::Rect;
use crate::virtual_list::{ListState, RowLayout};

#[cfg(debug_assertions)]
thread_local! {
    /// 调试构建：提示过几次"视口在滚动方向上是 0"（测试用）
    pub(crate) static COLLAPSED_WARNINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 滚动的全局状态：所有视口、需要每帧推进的视口、进行中的手势、拖着的滚动条、要写回句柄的信号
#[derive(Default)]
pub(crate) struct Scrolls {
    all: Vec<NodeId>,
    awake: Vec<NodeId>,
    gesture: Gesture,
    drag: Option<ThumbDrag>,
    writes: Vec<Write>,
    /// 虚拟列表（滚到第几行时按下标找位置；行高量出来的列表在布局之后量）
    lists: Vec<Rc<RefCell<ListState>>>,
    /// 物理状态在帧与帧之间改过的视口：这一帧把整份交给渲染线程
    dirty: Vec<NodeId>,
    /// 有 `on_scroll` 的视口和上一次交给它的偏移：帧末比一下，变了就发一次
    reported: Vec<(NodeId, [f32; 2])>,
    /// 手指正拖着滚动
    pan: Option<Pan>,
    /// 句柄要求滚进视口的元素（是否带动画）：定位之后执行
    reveals: Vec<(NodeId, bool)>,
}

/// 手指拖动要滚的东西
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PanTarget {
    /// 滚动视口，和它接哪些方向的位移
    Viewport(NodeId, [bool; 2]),
    /// 输入目标自己的内容（单行输入框）
    Content(NodeId),
}

impl PanTarget {
    pub(super) fn node(self) -> NodeId {
        match self {
            Self::Viewport(id, _) | Self::Content(id) => id,
        }
    }
}

/// 手指正拖着滚动：一次拖动锁定开始时选中的那一个，手指上一次的位置（窗口坐标）
struct Pan {
    target: PanTarget,
    last: [f32; 2],
}

#[derive(Default)]
struct Gesture {
    stage: Stage,
    /// 这次触控板手势锁定的视口，和它接哪些方向的位移
    target: Option<(NodeId, [bool; 2])>,
    /// 滚轮（和没有阶段的位移）锁定的视口：(视口, 方向, 主方向, 最近一次的时间)
    wheel: Option<(NodeId, [bool; 2], usize, f64)>,
    /// 按住 Control 转滚轮的缩放（平台惯例）交给了捏合，还没结束：松开 Control 时结束
    zooming: bool,
}

/// 触控板手势进行到哪一步
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Stage {
    #[default]
    None,
    /// 手指在触控板上
    Touch,
    /// 手指刚抬起（时间）：紧接着开始的事件是系统的惯性
    Lifted(f64),
    /// 系统的惯性：视口已经带着松手的速度自己滑出去了，这一轮惯性事件不再管
    Momentum,
}

struct ThumbDrag {
    viewport: NodeId,
    axis: usize,
    /// 按下时指针在视口坐标里的位置（沿这个方向）和当时的偏移
    start: f32,
    offset: f32,
}

/// 要在释放树的借用之后写回的信号
enum Write {
    Offset(Signal<[f32; 2]>, [f32; 2]),
    Metrics(Signal<Metrics>, Metrics),
    /// 运动还要走多远（虚拟列表往运动方向多挂行）
    Lead(Signal<[f32; 2]>, [f32; 2]),
    /// 计数加一：虚拟列表的行高量出来变了
    Bump(Signal<u64>),
}

/// 滚动条的层标识（见 [`DerivedLayer`]）
pub(super) fn thumb_key(id: NodeId, axis: usize) -> u64 {
    (if axis == 0 { DerivedLayer::ThumbX } else { DerivedLayer::ThumbY }).key(layer_key(id))
}

/// 主方向：横向位移更大时是 x（0），否则是 y（1）
fn dominant(delta: [f32; 2]) -> usize {
    usize::from(delta[1].abs() >= delta[0].abs())
}

/// 挂好的视口的三层和设置（见 [`crate::scroll::ScrollOptions`]）
pub(crate) struct Parts {
    pub axes: [bool; 2],
    pub handle: Option<ScrollHandle>,
    pub surface: Option<bool>,
    /// 托架：滚动方向上大小为 0
    pub holder: NodeId,
    /// 跟着滚的内容
    pub content: NodeId,
}

/// 挂载视口（子元素都挂好之后）：视口不被内容撑大，内容在滚动方向上不被压缩，内容做成合成层；
/// 有句柄时建一个副作用接收程序化滚动
pub(crate) fn mount_viewport(tree: &TreeRef, id: NodeId, parts: Parts) {
    let Parts { axes, handle, surface, holder, content } = parts;
    {
        let mut t = tree.borrow_mut();
        let overflow = |scrolls: bool| if scrolls { Overflow::Hidden } else { Overflow::Visible };
        t.update_style(id, |s| {
            s.overflow = Point { x: overflow(axes[0]), y: overflow(axes[1]) };
            if axes == [true, true] {
                s.align_items = AlignItems::START;
            }
        });
        t.update_style(holder, |s| s.flex_shrink = 0.0);
        t.update_style(content, |s| s.flex_shrink = 0.0);
        t.nodes[content].layer = true;
        t.nodes[content].scrolled = true;
        t.nodes[content].refresh_flags();
        let mut scroller = Scroller::new(axes, content, handle, None);
        scroller.holder = holder;
        scroller.surface = surface;
        scroller.scale = t.text.scale();
        t.nodes[id].scroll = Some(Box::new(scroller));
        t.nodes[id].refresh_flags();
        t.scrolls.all.push(id);
        // 渲染线程从第一帧起就要有这个视口的状态（内容层的偏移、滚动条由它算）
        t.scrolls.dirty.push(id);
        if t.listens(id, EventType::Scroll) {
            t.scrolls.reported.push((id, [0.0; 2]));
        }
    }
    if let Some(handle) = handle {
        watch_requests(tree, id, handle);
    }
}

/// 挂载自己滚的元素（多行输入框）：内容是它自己画的，没有托架和内容节点。元素不被内容撑大（竖着溢出时隐藏），
/// 滚动的物理、滚轮、滚动条、键盘翻页和滚动视口是同一套；内容画在它自己的内容层里，渲染线程照样推进
pub(crate) fn mount_own_viewport(tree: &TreeRef, id: NodeId, handle: Option<ScrollHandle>) {
    {
        let mut t = tree.borrow_mut();
        t.update_style(id, |s| s.overflow = Point { x: Overflow::Visible, y: Overflow::Hidden });
        let mut scroller = Scroller::new([false, true], id, handle, None);
        scroller.holder = id;
        scroller.own = true;
        scroller.scale = t.text.scale();
        t.nodes[id].scroll = Some(Box::new(scroller));
        t.nodes[id].refresh_flags();
        t.scrolls.all.push(id);
        t.scrolls.dirty.push(id);
        if t.listens(id, EventType::Scroll) {
            t.scrolls.reported.push((id, [0.0; 2]));
        }
    }
    if let Some(handle) = handle {
        watch_requests(tree, id, handle);
    }
}

/// 有句柄的视口：建一个副作用接收程序化滚动
fn watch_requests(tree: &TreeRef, id: NodeId, handle: ScrollHandle) {
    let tree = Rc::downgrade(tree);
    Effect::new(move || {
        let requests = handle.take_requests();
        if requests.is_empty() {
            return;
        }
        if let Some(tree) = tree.upgrade() {
            let mut t = tree.borrow_mut();
            for request in requests {
                t.scroll_request(id, request);
            }
        }
    });
}

/// 写回句柄的信号（树已经释放，信号触发的副作用可以改树）。返回有没有写
pub(super) fn flush(tree: &TreeRef) -> bool {
    let writes = mem::take(&mut tree.borrow_mut().scrolls.writes);
    if writes.is_empty() {
        return false;
    }
    batch(|| {
        for write in writes {
            match write {
                Write::Offset(signal, value) => signal.set_if_changed(value),
                Write::Metrics(signal, value) => signal.set_if_changed(value),
                Write::Lead(signal, value) => signal.set_if_changed(value),
                Write::Bump(signal) => {
                    signal.update(|n| *n += 1);
                    true
                }
            };
        }
    });
    true
}

/// 主线程卡住后处理积压的输入之前：收回渲染线程推进到的滚动状态，命中测试按屏幕上的偏移，
/// 句柄的偏移随即写回（虚拟列表跟着挂载屏幕上的行）
pub fn adopt_scrolls(tree: &TreeRef) {
    tree.borrow_mut().adopt_scrolls();
    flush(tree);
}

/// 按住 Control 转滚轮缩放时，一格放大（缩小）这么多（捏合的比例，0.1 是 10%）
const ZOOM_PER_NOTCH: f32 = 0.1;

/// 滚轮和触控板事件：先交给指针下最内层的元素冒泡（`on_wheel`），没被取消 `SCROLL` 时再交给视口滚动。
/// 按平台惯例（[`crate::Conventions`]）：
/// - 按住 Shift 转滚轮是横着滚：处理器收到的就是横着的位移，和 macOS 的系统替应用换好的一样；
/// - 按住 Control 转滚轮是缩放（Windows，精确式触控板的双指捏合也这样报来）：视口不滚，换成捏合交给指针下的元素
///   （往上滚放大，一格 [`ZOOM_PER_NOTCH`]），松开 Control 时这次捏合结束（[`wheel_modifiers`]）。
///   `on_wheel` 里自己按 Control 缩放的取消 `SCROLL`，就不再换成捏合
pub(crate) fn wheel(tree: &TreeRef, delta: WheelDelta, phase: GesturePhase, modifiers: Modifiers) {
    let conventions = crate::system::conventions();
    let zoom = conventions.control_wheel_zooms && modifiers.has_control() && matches!(delta, WheelDelta::Lines(_));
    let delta = match delta {
        WheelDelta::Lines([x, y]) if x == 0.0 && !zoom && conventions.shift_wheel_scrolls_horizontally && modifiers.has_shift() => {
            WheelDelta::Lines([y, 0.0])
        }
        delta => delta,
    };
    let routed = {
        let mut t = tree.borrow_mut();
        t.wheel_event(delta, phase, modifiers)
    };
    let prevented = match routed {
        Some((path, mut event)) => {
            dispatch(tree, &path, EventType::Wheel, &mut event, Options::default(), |_, _, _, _| {});
            event.is_builtin_prevented(Builtin::SCROLL)
        }
        None => false,
    };
    if prevented {
        return;
    }
    if zoom {
        if let WheelDelta::Lines([_, y]) = delta
            && y != 0.0
        {
            // 捏合的开始不带变化：第一格要在开始之后再交
            if !mem::replace(&mut tree.borrow_mut().scrolls.gesture.zooming, true) {
                super::pinch(tree, 0.0, GesturePhase::Start);
            }
            super::pinch(tree, -y * ZOOM_PER_NOTCH, GesturePhase::Move);
        }
        return;
    }
    tree.borrow_mut().wheel(delta, phase);
}

/// 修饰键变了：按住 Control 转滚轮的缩放在松开 Control 时结束（交给捏合的结束）
pub(crate) fn wheel_modifiers(tree: &TreeRef, modifiers: Modifiers) {
    if modifiers.has_control() {
        return;
    }
    let zooming = mem::take(&mut tree.borrow_mut().scrolls.gesture.zooming);
    if zooming {
        super::pinch(tree, 0.0, GesturePhase::End);
    }
}

/// 一帧结束时：有 `on_scroll` 的视口偏移变了的，各发一次（合并了这一帧里所有的变化）。返回有没有发
pub(super) fn emit_scroll_events(tree: &TreeRef) -> bool {
    let calls: Vec<(Vec<On<ScrollEvent>>, ScrollEvent)> = {
        let mut t = tree.borrow_mut();
        if t.scrolls.reported.is_empty() {
            return false;
        }
        let mut reported = mem::take(&mut t.scrolls.reported);
        reported.retain(|&(id, _)| t.nodes.contains_key(id));
        let mut calls = Vec::new();
        for (id, last) in &mut reported {
            let Some(shown) = t.scroller(*id).map(|s| s.shown) else { continue };
            if shown != *last {
                let event = ScrollEvent { offset: shown, delta: [shown[0] - last[0], shown[1] - last[1]] };
                *last = shown;
                calls.push((t.handlers_of(*id, EventType::Scroll, Handler::scroll), event));
            }
        }
        t.scrolls.reported = reported;
        calls
    };
    if calls.is_empty() {
        return false;
    }
    batch(|| {
        for (handlers, event) in &calls {
            handlers.iter().for_each(|h| h(event));
        }
    });
    true
}

/// 行高量出来的虚拟列表的数据变了（行高索引已经重建）：视口跟着锚点挪 `delta`，修正滚到第几行的目标，
/// 新的偏移马上写回句柄（要挂载的范围按它算）
pub(crate) fn list_changed(tree: &TreeRef, content: NodeId, delta: f32) {
    {
        let mut t = tree.borrow_mut();
        let Some(viewport) = t.viewport_of(content) else { return };
        if delta != 0.0 {
            t.shift_scroll(viewport, delta);
        }
        t.apply_seek(viewport);
    }
    flush(tree);
}

impl Tree {
    /// 滚动条的颜色（属性 `scrollbar_color`，`None` 是默认的半透明灰）：滚动条在主线程上画，只重画
    pub(super) fn set_scrollbar_color(&mut self, id: NodeId, color: Option<scene::Color>) {
        let Some(scroll) = self.nodes.get_mut(id).and_then(|n| n.scroll.as_deref_mut()) else { return };
        if scroll.bar != color {
            scroll.bar = color;
            self.paint.invalidate();
        }
    }

    /// 虚拟列表：把一行放到内容里的 (0, top)，宽度占满；高度固定，或者（`None`）按内容
    pub(crate) fn place_row(&mut self, id: NodeId, top: f32, height: Option<f32>) {
        self.update_style(id, |s| {
            s.position = Position::Absolute;
            s.inset.left = LengthPercentageAuto::length(0.0);
            s.inset.top = LengthPercentageAuto::length(top);
            s.size.width = Dimension::percent(1.0);
            s.size.height = height.map_or(Dimension::auto(), Dimension::length);
        });
    }

    /// 登记虚拟列表（列表卸载后自动清掉）
    pub(crate) fn register_list(&mut self, list: Rc<RefCell<ListState>>) {
        self.scrolls.lists.push(list);
    }

    /// 内容节点所在的视口：往上找记着它是内容的视口（虚拟列表的内容）；自己滚的元素（多行输入框）是它自己
    pub(super) fn viewport_of(&self, content: NodeId) -> Option<NodeId> {
        let node = self.nodes.get(content)?;
        if node.scroll.as_ref().is_some_and(|s| s.own) {
            return Some(content);
        }
        let mut current = node.parent;
        while let Some(id) = current {
            let node = self.nodes.get(id)?;
            if node.scroll.as_ref().is_some_and(|s| s.content == content) {
                return Some(id);
            }
            current = node.parent;
        }
        None
    }

    /// 列表所在视口现在的偏移（布局过之后才有）
    pub(crate) fn list_offset(&self, content: NodeId) -> Option<f32> {
        let s = self.scroller(self.viewport_of(content)?)?;
        s.laid_out.then_some(s.offset[1])
    }

    /// 内容在视口的偏移底下挪了 `delta`：偏移和进行中的运动（弹簧、手指、拖着的滚动条）一起挪，屏幕上的内容不动
    fn shift_scroll(&mut self, viewport: NodeId, delta: f32) {
        let Some(s) = self.scroller_mut(viewport) else { return };
        if !s.laid_out {
            return;
        }
        s.shift(1, delta);
        if let Some(drag) = &mut self.scrolls.drag
            && drag.viewport == viewport
            && drag.axis == 1
        {
            drag.offset += delta;
        }
        let now = self.anim.host();
        self.sync_scroll(viewport, now);
        self.wake(viewport);
    }

    /// 视口里第 `index` 行的顶（内容坐标）：虚拟列表按行的位置，普通视口按内容的第几个子元素
    fn index_top(&self, viewport: NodeId, index: usize) -> Option<f32> {
        let content = self.scroller(viewport)?.content;
        if let Some(list) = self.scrolls.lists.iter().find(|l| l.borrow().content == content) {
            let list = list.borrow();
            return (list.layout.len() > 0).then(|| list.layout.top(index));
        }
        let node = self.nodes.get(content)?;
        let &child = node.children.get(index.min(node.children.len().checked_sub(1)?))?;
        let layout = self.layout.try_get(self.nodes.get(child)?.layout)?;
        Some(layout.location.y)
    }

    /// 滚到第几行的请求还在（用户没有自己滚过）：目标行的位置变了时跟着修正（在弹的接着滑过去，停着的直接跳）
    fn apply_seek(&mut self, viewport: NodeId) {
        let Some(index) = self.scroller(viewport).and_then(|s| s.seek) else { return };
        let Some(top) = self.index_top(viewport, index) else { return };
        let Some(s) = self.scroller_mut(viewport) else { return };
        s.retarget(1, top);
        s.seek = Some(index);
        let now = self.anim.host();
        self.sync_scroll(viewport, now);
        self.wake(viewport);
    }

    /// 布局之后：行高量出来的虚拟列表读挂载着的行的实际高度，和索引里的不一样时更新索引、重新摆放挂载着的行、
    /// 更新内容高度；视口顶上那一行（锚点）留在屏幕上原来的位置（偏移跟着挪）。返回有没有改动（要再布局一次）
    pub(super) fn measure_lists(&mut self) -> bool {
        if self.scrolls.lists.is_empty() {
            return false;
        }
        let mut lists = mem::take(&mut self.scrolls.lists);
        lists.retain(|l| self.nodes.contains_key(l.borrow().content));
        let mut changed = false;
        let mut seeking = Vec::new();
        for list in &lists {
            let mut guard = list.borrow_mut();
            let ListState { content, layout, mounted, changed: bump } = &mut *guard;
            let RowLayout::Measured(index) = layout else { continue };
            let viewport = self.viewport_of(*content);
            let offset = viewport.and_then(|v| self.scroller(v)).filter(|s| s.laid_out).map(|s| s.offset[1]);
            // 锚点：视口顶上那一行，记下它的顶
            let anchor = offset.filter(|_| index.len() > 0).map(|y| {
                let row = index.row_at(y);
                (row, index.top(row))
            });
            let mut any = false;
            for &(i, node) in mounted.iter() {
                if let Some(height) = self.nodes.get(node).and_then(|n| self.layout.try_get(n.layout)).map(|l| l.size.height)
                    && i < index.len()
                {
                    any |= index.set(i, height);
                }
            }
            if !any {
                continue;
            }
            changed = true;
            for &(i, node) in mounted.iter() {
                self.place_row(node, index.top(i) as f32, None);
            }
            self.set_content_height(*content, index.total() as f32);
            self.scrolls.writes.push(Write::Bump(*bump));
            let Some(viewport) = viewport else { continue };
            if let Some((row, before)) = anchor {
                let delta = (index.top(row) - before) as f32;
                if delta != 0.0 {
                    self.shift_scroll(viewport, delta);
                }
            }
            seeking.push(viewport);
        }
        // 量的时候不会登记新的列表；万一有，接在后面
        lists.append(&mut self.scrolls.lists);
        self.scrolls.lists = lists;
        // 滚到第几行的目标跟着修正（按新的行高）
        for viewport in seeking {
            self.apply_seek(viewport);
        }
        changed
    }

    /// 虚拟列表：内容的高度是所有行的高度之和
    pub(crate) fn set_content_height(&mut self, id: NodeId, height: f32) {
        self.update_style(id, |s| s.size.height = Dimension::length(height));
    }

    fn scroller(&self, id: NodeId) -> Option<&Scroller> {
        self.nodes.get(id)?.scroll.as_deref()
    }

    pub(super) fn scroller_mut(&mut self, id: NodeId) -> Option<&mut Scroller> {
        self.nodes.get_mut(id)?.scroll.as_deref_mut()
    }

    /// 多行输入框的插入点（内容坐标里的纵向范围）在视口外：立即滚到刚好看得见（贴着视口的上边或下边）
    pub(super) fn reveal(&mut self, viewport: NodeId, [y0, y1]: [f32; 2], now: f64) {
        let Some(s) = self.scroller_mut(viewport) else { return };
        if !s.laid_out {
            return;
        }
        let (offset, height) = (s.offset[1], s.metrics.viewport[1]);
        let target = if y1 - y0 > height || y0 < offset {
            y0
        } else if y1 > offset + height {
            y1 - height
        } else {
            return;
        };
        s.scroll_to(1, target, false);
        self.sync_scroll(viewport, now);
        self.wake(viewport);
    }

    /// 这个视口的状态在帧与帧之间改过：这一帧交给渲染线程；之后每帧推进它，直到它停下、滚动条也淡出
    pub(super) fn wake(&mut self, id: NodeId) {
        let Some(s) = self.nodes.get_mut(id).and_then(|n| n.scroll.as_deref_mut()) else { return };
        if !mem::replace(&mut s.dirty, true) {
            self.scrolls.dirty.push(id);
        }
        if !mem::replace(&mut s.awake, true) {
            self.scrolls.awake.push(id);
        }
    }

    /// 没有视口在动、也没有改过的状态要交给渲染线程（也没有等着滚进视口的元素）
    pub(super) fn scrolls_idle(&self) -> bool {
        self.scrolls.awake.is_empty() && self.scrolls.dirty.is_empty() && self.scrolls.reveals.is_empty()
    }

    /// 帧与帧之间改过的视口：整份物理状态交给渲染线程
    pub(super) fn take_scroll_states(&mut self) -> Vec<ScrollState> {
        let mut states = Vec::with_capacity(self.scrolls.dirty.len());
        for id in mem::take(&mut self.scrolls.dirty) {
            let Some(s) = self.scroller_mut(id) else { continue };
            s.dirty = false;
            let content = if s.own { scroll::own_content_key(id) } else { layer_key(s.content) };
            states.push(ScrollState { viewport: layer_key(id), content, physics: s.phys.clone() });
        }
        states
    }

    /// 主线程卡住时渲染线程接着推进了滚动：收回它推进到的状态。只收渲染线程收到了主线程最近一帧之后的，
    /// 主线程自己在那之后又改过的视口（新的输入）不收，交给渲染线程照单全收。之后从收回的时间接着推进
    pub(super) fn adopt_scrolls(&mut self) {
        let Some(snapshot) = self.commit.take_handback() else { return };
        if snapshot.frame != self.commit.sent() {
            return;
        }
        for (key, physics) in snapshot.scrolls {
            let id = super::layer_node(key);
            let Some(s) = self.scroller_mut(id) else { continue };
            if s.dirty {
                continue;
            }
            s.phys = physics;
            s.clock = Some(snapshot.time);
            let awake = mem::replace(&mut s.awake, true);
            if !awake {
                self.scrolls.awake.push(id);
            }
            self.sync_scroll(id, snapshot.time);
        }
    }

    /// 窗口缩放变了：偏移按新的物理像素取整（渲染线程那份也跟着）
    pub(super) fn set_scroll_scale(&mut self, scale: f32) {
        for id in self.scrolls.all.clone() {
            if let Some(s) = self.scroller_mut(id) {
                s.scale = scale;
                self.wake(id);
            }
        }
    }

    /// 视口里的程序化滚动；第一次布局之前不知道能滚多远，先存着
    fn scroll_request(&mut self, id: NodeId, request: Request) {
        let Some(s) = self.scroller_mut(id) else { return };
        if !s.laid_out {
            s.pending.push(request);
            return;
        }
        for axis in 0..2 {
            if !s.axes[axis] {
                continue;
            }
            match request {
                Request::To { target, animated } => s.scroll_to(axis, target[axis], animated),
                Request::By(delta) if delta[axis] != 0.0 => s.wheel(axis, delta[axis]),
                Request::By(_) | Request::ToIndex { .. } => {}
            }
        }
        if let Request::ToIndex { index, animated } = request
            && let Some(top) = self.index_top(id, index)
            && let Some(s) = self.scroller_mut(id)
            && s.axes[1]
        {
            s.scroll_to(1, top, animated);
            s.seek = Some(index);
        }
        self.wake(id);
    }

    /// 每帧最先调用：推进滚动的物理（渲染线程那份自己推进，不用传偏移），滚动条淡入淡出。
    /// 返回是否有视口的偏移变了
    pub(super) fn step_scroll(&mut self, now: f64) -> bool {
        if self.scrolls.awake.is_empty() {
            return false;
        }
        let last = self.anim.last_host();
        let mut moved = false;
        for id in mem::take(&mut self.scrolls.awake) {
            let Some(s) = self.scroller_mut(id) else { continue };
            // 收回过渲染线程的状态：从它停在的时间接着推进
            let from = s.clock.take().or(last).unwrap_or(now);
            s.step((now - from).max(0.0) as f32, now);
            if let Some(handle) = s.handle
                && let Some(lead) = s.lead()
            {
                self.scrolls.writes.push(Write::Lead(handle.lead_signal(), lead));
            }
            moved |= self.sync_scroll(id, now);
            let Some(s) = self.scroller_mut(id) else { continue };
            if s.busy() || s.needs_frames() {
                self.scrolls.awake.push(id);
            } else {
                s.awake = false;
            }
        }
        moved
    }

    /// 偏移（按物理像素取整后）变了：内容节点跟着平移（命中测试、无障碍），写回句柄，显示滚动条。返回是否变了。
    /// 屏幕上的平移由渲染线程按它那份物理状态算，这里不发偏移
    fn sync_scroll(&mut self, id: NodeId, now: f64) -> bool {
        let Some(s) = self.scroller_mut(id) else { return false };
        s.phys.sync(now);
        let shown = s.snapped();
        if shown == s.shown {
            return false;
        }
        s.shown = shown;
        let (content, handle, viewport, own) = (s.content, s.handle, s.metrics.viewport, s.own);
        // 内容节点跟着平移；自己滚的元素不动，由它自己换算（命中、插入点、候选框）
        if own {
            if let Some(editor) = self.nodes.kind_mut::<crate::editor::EditorBlock>(id) {
                editor.set_scroll_y(shown[1]);
            }
        } else if let Some(node) = self.nodes.get_mut(content) {
            node.shift = [-shown[0], -shown[1]];
        }
        // 无障碍树：内容的变换、视口的滚动位置
        self.accessibility.touch(content);
        self.accessibility.touch(id);
        if let Some(handle) = handle {
            self.scrolls.writes.push(Write::Offset(handle.offset_signal(), shown));
        }
        // 多行输入框只画了视口附近的段：滚出画过的范围才重画
        if let Some(editor) = self.nodes.kind::<crate::editor::EditorBlock>(content)
            && editor.needs_repaint(Viewport { top: shown[1], height: viewport[1] })
        {
            self.paint.invalidate();
        }
        self.paint.mark_worlds();
        true
    }

    /// 布局之后：更新每个视口的滚动范围（内容变短时把偏移夹回来），写回句柄；
    /// 第一次布局后执行之前排队的程序化滚动
    pub(super) fn update_scroll_metrics(&mut self, now: f64) {
        let mut i = 0;
        while i < self.scrolls.all.len() {
            let id = self.scrolls.all[i];
            let Some(node) = self.nodes.get(id) else {
                self.scrolls.all.swap_remove(i);
                continue;
            };
            i += 1;
            let Some(s) = node.scroll.as_deref() else { continue };
            let layout = self.layout.get(node.layout);
            let size = [layout.size.width, layout.size.height];
            let viewport = [layout.content_box_width(), layout.content_box_height()];
            let content = match s.own {
                // 自己滚的元素：内容是它自己排出来的高度
                true => [viewport[0], self.nodes.kind::<crate::editor::EditorBlock>(id).map_or(0.0, |e| e.content_height())],
                false => self.nodes.get(s.content).map_or([0.0; 2], |c| {
                    let size = self.layout.get(c.layout).size;
                    [size.width, size.height]
                }),
            };
            let metrics = Metrics { viewport, content };
            if s.laid_out && metrics == s.metrics && size == s.size {
                continue;
            }
            let handle = s.handle;
            let Some(s) = self.scroller_mut(id) else { continue };
            let pending = if s.laid_out { Vec::new() } else { mem::take(&mut s.pending) };
            s.laid_out = true;
            s.set_metrics(metrics, size);
            // 浮层的表面：内容放得下时不裁剪（子元素的投影、焦点环、拖出去的部分照常画出来）
            let clip = s.surface.map(|always| always || s.max[1] >= 0.5);
            let seeking = s.seek.is_some();
            self.accessibility.touch(id);
            if let Some(clip) = clip {
                self.update_paint(id, |p| p.clip = clip);
            }
            if let Some(handle) = handle {
                self.scrolls.writes.push(Write::Metrics(handle.metrics_signal(), metrics));
            }
            for request in pending {
                self.scroll_request(id, request);
            }
            if seeking {
                self.apply_seek(id);
            }
            self.sync_scroll(id, now);
            self.wake(id);
            #[cfg(debug_assertions)]
            self.warn_collapsed(id);
        }
    }

    /// 调试构建：视口在滚动方向上是 0、内容却不是空的（多半是忘了给 `height` / `grow`，或者父元素在这个方向上没有大小），
    /// 记一条警告（每个视口一次）。自己或祖先在这个方向上明确设成 0（收起的面板、退场动画）时是故意的，不提示
    #[cfg(debug_assertions)]
    fn warn_collapsed(&mut self, id: NodeId) {
        let Some(s) = self.scroller(id) else { return };
        if s.warned {
            return;
        }
        let Some(axis) = (0..2).find(|&a| s.axes[a] && s.size[a] < 0.5 && s.metrics.content[a] >= 1.0) else { return };
        let content = s.metrics.content[axis];
        if self.collapsed_on_purpose(id, axis) {
            return;
        }
        if let Some(s) = self.scroller_mut(id) {
            s.warned = true;
        }
        COLLAPSED_WARNINGS.with(|n| n.set(n.get() + 1));
        let name = self.nodes.name(id).map_or(String::new(), |name| format!(" {name} "));
        let (side, fix) = if axis == 1 {
            ("高度", ".height(..)、.grow(1)（父元素也要有高度），或者改用 scroll_y_fit 跟着内容长高")
        } else {
            ("宽度", ".width(..) 或 .grow(1)（父元素也要有宽度）")
        };
        tracing::warn!("滚动视口{name}的{side}是 0，内容有 {content:.0} 像素却看不到。视口不会被内容撑大，给它 {fix}。（只在调试构建里提示）");
    }

    /// 自己或某个祖先在这个方向上明确设成了 0（滚动视口里那层托架不算），或者正在退场
    #[cfg(debug_assertions)]
    fn collapsed_on_purpose(&self, id: NodeId, axis: usize) -> bool {
        let (zero, zero_max) = (Dimension::length(0.0), LengthPercentageAuto::length(0.0));
        let mut current = Some(id);
        while let Some(node_id) = current {
            let Some(node) = self.nodes.get(node_id) else { return false };
            if node.exiting {
                return true;
            }
            let holder = node.parent.and_then(|p| self.nodes.get(p)?.scroll.as_ref()).is_some_and(|s| s.holder == node_id);
            if !holder {
                let style = self.layout.style(node.layout);
                let (size, max) = if axis == 1 { (style.size.height, style.max_size.height) } else { (style.size.width, style.max_size.width) };
                if size == zero || max == zero_max {
                    return true;
                }
            }
            current = node.parent;
        }
        false
    }

    /// 滚轮事件对象和它的路径（指针下最内层的元素往外）；路径上没有元素要滚轮时是 `None`（不分配、不分发）
    fn wheel_event(&mut self, delta: WheelDelta, phase: GesturePhase, modifiers: Modifiers) -> Option<(super::route::RoutePath, WheelEvent)> {
        let point = self.router.pointer()?;
        // 整棵树都没有要滚轮的元素时不做命中测试（视口滚动自己会锁定视口，大多时候不用命中测试）
        if !self.nodes.ever_listens(EventType::Wheel.bit() | EventType::Wheel.capture_bit()) {
            return None;
        }
        self.refresh_worlds();
        let mut hit = HitPath::new();
        self.hit_into(point, &mut hit);
        let path = self.route_path(*hit.last()?);
        if !path.iter().any(|&id| self.handlers(id).is_some_and(|h| h.listens_in(EventType::Wheel, PropagationPhase::Target))) {
            return None;
        }
        let momentum = self.wheel_momentum(delta, phase);
        Some((path, WheelEvent::new(delta, phase, momentum, point, modifiers)))
    }

    /// 这一个触控板事件是不是系统的惯性（和下面 [`Self::wheel`] 的判断一样：抬手之后紧接着带位移开始的、惯性里接着来的）
    fn wheel_momentum(&self, delta: WheelDelta, phase: GesturePhase) -> bool {
        let WheelDelta::Pixels(d) = delta else { return false };
        let g = &self.scrolls.gesture;
        match (phase, g.stage) {
            (GesturePhase::Start, Stage::Lifted(at)) => crate::timer::now() - at <= MOMENTUM_WINDOW && g.target.is_some() && d != [0.0; 2],
            (_, Stage::Momentum) => true,
            _ => false,
        }
    }

    /// 滚轮和触控板：交给指针下能往这个方向滚的最内层视口，它滚到头了交给外层；
    /// 都滚不动时交给最内层的（拉出橡皮筋）。一次手势（连同之后的惯性）锁定同一个视口；
    /// 只滚一个方向的视口只接主方向的位移，竖着划过横向轮播时交给外面的列表。
    /// 手势的先后（锁定多久、抬手后多久算惯性）按事件到来的真实时间算：显示链接停着时元素树的时间停在最后一帧
    fn wheel(&mut self, delta: WheelDelta, phase: GesturePhase) {
        let now = crate::timer::now();
        match delta {
            // 一格滚多远按系统设置（可能是一屏）。整格平滑过去；不足一格的（精确式触控板、高分辨率滚轮）直接跟着走，
            // 不然手指停了内容还在追
            WheelDelta::Lines(lines) => {
                let conventions = crate::system::conventions();
                let notch = conventions.wheel_notch;
                let delta = [lines[0] * notch[0], lines[1] * notch[1]];
                let Some((id, mask)) = self.route_latched(delta, now) else { return };
                let precise = conventions.fractional_wheel_is_precise && lines.iter().any(|l| l.fract() != 0.0);
                if let Some(s) = self.scroller_mut(id) {
                    for axis in (0..2).filter(|&a| mask[a] && delta[a] != 0.0) {
                        let distance = if conventions.wheel_scrolls_pages { lines[axis] * scroll::page(s.metrics.viewport[axis]) } else { delta[axis] };
                        if precise {
                            s.precise_wheel(axis, distance);
                        } else {
                            s.wheel(axis, distance);
                        }
                    }
                }
                self.wake(id);
            }
            WheelDelta::Pixels(delta) => match phase {
                GesturePhase::Start => {
                    // 抬手之后紧接着、并且一开始就带着位移的是系统的惯性；
                    // 新的一次触摸从"手指放上"开始，没有位移
                    let g = &mut self.scrolls.gesture;
                    let (stage, target) = (g.stage, g.target);
                    match stage {
                        Stage::Lifted(at) if now - at <= MOMENTUM_WINDOW && target.is_some() && delta != [0.0; 2] => {
                            // 系统的惯性开始了：视口带着松手的速度自己滑出去（渲染线程也照样滑，主线程卡住也不停），
                            // 之后的惯性事件不再管
                            g.stage = Stage::Momentum;
                            if let Some((id, mask)) = target {
                                if let Some(s) = self.scroller_mut(id) {
                                    for axis in (0..2).filter(|&a| mask[a]) {
                                        s.momentum(axis, delta[axis]);
                                    }
                                }
                                self.wake(id);
                            }
                        }
                        // 系统先发"手指放上"再发"开始移动"，都是开始
                        Stage::Touch => {
                            if delta != [0.0; 2] {
                                self.wheel(WheelDelta::Pixels(delta), GesturePhase::Move);
                            }
                        }
                        _ => {
                            // 新的一次触摸：上一次手势滑着的惯性停在原地
                            g.stage = Stage::Touch;
                            g.target = None;
                            if let Some((id, _)) = target
                                && let Some(s) = self.scroller_mut(id)
                                && s.moving()
                            {
                                s.catch(0);
                                s.catch(1);
                                self.wake(id);
                            }
                            if delta != [0.0; 2] {
                                self.wheel(WheelDelta::Pixels(delta), GesturePhase::Move);
                            }
                        }
                    }
                }
                GesturePhase::Move => match self.scrolls.gesture.stage {
                    Stage::Touch => {
                        if self.scrolls.gesture.target.is_none() {
                            self.scrolls.gesture.target = self.route(delta);
                        }
                        let Some((id, mask)) = self.scrolls.gesture.target else { return };
                        if let Some(s) = self.scroller_mut(id) {
                            for axis in (0..2).filter(|&a| mask[a] && delta[a] != 0.0) {
                                s.touch(axis, delta[axis]);
                            }
                        }
                        self.wake(id);
                    }
                    // 惯性由视口自己推进（减速和系统的一样），系统接着发来的位移不再管
                    Stage::Momentum => {}
                    // 没有阶段的精确位移（有些鼠标）
                    Stage::None | Stage::Lifted(_) => {
                        let Some((id, mask)) = self.route_latched(delta, now) else { return };
                        if let Some(s) = self.scroller_mut(id) {
                            for axis in (0..2).filter(|&a| mask[a] && delta[a] != 0.0) {
                                s.nudge(axis, delta[axis]);
                            }
                        }
                        self.wake(id);
                    }
                },
                GesturePhase::End | GesturePhase::Cancel => match self.scrolls.gesture.stage {
                    Stage::Touch => {
                        if let Some((id, _)) = self.scrolls.gesture.target {
                            if let Some(s) = self.scroller_mut(id) {
                                s.release(0);
                                s.release(1);
                            }
                            self.wake(id);
                        }
                        self.scrolls.gesture.stage = Stage::Lifted(now);
                    }
                    // 系统的惯性结束了；视口自己的可能还在滑（留着目标：手指再放上时停住它）
                    Stage::Momentum => self.scrolls.gesture.stage = Stage::None,
                    Stage::None | Stage::Lifted(_) => {}
                },
            },
        }
    }

    /// 选视口：指针下从内到外第一个在主方向上能滚、并且还能往这边滚的视口；
    /// 都滚不动时用最内层能在主方向上滚的（拉出橡皮筋）
    fn route(&mut self, delta: [f32; 2]) -> Option<(NodeId, [bool; 2])> {
        let path = self.wheel_path(delta)?;
        self.pick(&path, delta)
    }

    /// 指针下从外到内的元素（滚动的命中测试）；没有指针、没有位移时为 `None`
    fn wheel_path(&mut self, delta: [f32; 2]) -> Option<Vec<NodeId>> {
        let point = self.router.pointer()?;
        if delta == [0.0; 2] {
            return None;
        }
        self.refresh_worlds();
        Some(self.hit_path(point))
    }

    /// 在指针下的元素（从外到内）里选视口，见 [`Self::route`]
    fn pick(&self, path: &[NodeId], delta: [f32; 2]) -> Option<(NodeId, [bool; 2])> {
        let axis = dominant(delta);
        let mut fallback = None;
        for &id in path.iter().rev() {
            let Some(s) = self.scroller(id) else { continue };
            // 放得下的浮层表面不接滚轮（菜单不拉出橡皮筋）
            if !s.axes[axis] || (s.surface.is_some() && s.max[axis] < 0.5) {
                continue;
            }
            let mask = if s.axes == [true, true] { [true; 2] } else { s.axes };
            if s.can_scroll(axis, delta[axis]) {
                return Some((id, mask));
            }
            fallback.get_or_insert((id, mask));
        }
        fallback
    }

    /// 手指按在 `path`（从外到内的元素）上、移动了 `moved`（窗口坐标，内容跟着手指走）：要滚的东西。视口的选法和滚轮一样（[`Self::route`]）；
    /// 选中的视口里面有能往这边滚内容的输入目标（单行输入框）时先滚它
    pub(super) fn pan_target(&self, path: &[NodeId], moved: [f32; 2]) -> Option<PanTarget> {
        let delta = [-moved[0], -moved[1]];
        if delta == [0.0; 2] {
            return None;
        }
        let picked = self.pick(path, delta);
        for &id in path.iter().rev() {
            if picked.is_some_and(|(viewport, _)| viewport == id) {
                break;
            }
            let Some(target) = self.input_target(id).filter(|_| !self.disabled(id)) else { continue };
            let Some((_, _, content)) = self.target_geometry(id) else { continue };
            if target.can_scroll_content(delta, content) {
                return Some(PanTarget::Content(id));
            }
        }
        picked.map(|(id, mask)| PanTarget::Viewport(id, mask))
    }

    /// 手指开始拖着滚动：内容从 `from` 开始跟手（手指上一次的位置，越过阈值之前那一段不算，内容不跳一下），手指现在在 `point`
    pub(super) fn begin_pan(&mut self, target: PanTarget, from: [f32; 2], point: [f32; 2]) {
        self.scrolls.pan = Some(Pan { target, last: from });
        self.pan_to(point);
    }

    /// 手指正拖着滚动时移到了 `point`：内容跟手。没在滚动时返回 `false`
    pub(super) fn pan_to(&mut self, point: [f32; 2]) -> bool {
        let Some(pan) = &mut self.scrolls.pan else { return false };
        let (target, last) = (pan.target, mem::replace(&mut pan.last, point));
        // 按元素自己的坐标算：祖先有缩放时内容还是贴着手指
        let moved = sub(self.local_point(target.node(), point), self.local_point(target.node(), last));
        match target {
            PanTarget::Viewport(id, mask) => {
                if let Some(s) = self.scroller_mut(id) {
                    for axis in (0..2).filter(|&a| mask[a] && moved[a] != 0.0) {
                        s.touch(axis, -moved[axis]);
                    }
                }
                self.wake(id);
            }
            PanTarget::Content(id) => {
                let delta = [-moved[0], -moved[1]];
                self.with_kind(id, |kind, cx| {
                    kind.input_target_mut()?.scroll_content(delta, cx);
                    Some(())
                });
            }
        }
        true
    }

    /// 手指不再拖着滚动：抬起（`fling`：带着松手的速度滑出去）或者系统收走了。拉过边缘的弹回去。没在滚动时返回 `false`
    pub(super) fn end_pan(&mut self, fling: bool) -> bool {
        let Some(pan) = self.scrolls.pan.take() else { return false };
        match pan.target {
            PanTarget::Viewport(id, mask) => {
                if let Some(s) = self.scroller_mut(id) {
                    for axis in (0..2).filter(|&a| mask[a]) {
                        s.lift(axis, fling);
                    }
                }
                self.wake(id);
            }
            PanTarget::Content(_) => {}
        }
        true
    }

    /// 手指正拖着滚动
    pub(super) fn panning(&self) -> bool {
        self.scrolls.pan.is_some()
    }

    /// 手指放上：它下面滑着的视口（惯性）停在原地。返回有没有停住的（这一下只是停住它，不算点按）
    pub(super) fn catch_under_finger(&mut self) -> bool {
        let mut caught = false;
        for index in 0..self.router.hovered().len() {
            let id = self.router.hovered()[index];
            let Some(s) = self.scroller_mut(id) else { continue };
            if s.flinging() {
                s.catch(0);
                s.catch(1);
                caught = true;
                self.wake(id);
            }
        }
        caught
    }

    /// 滚轮（和没有阶段的位移）：每个事件交给它发生时指针下最内层能往这边滚的视口（[`Self::route`]）。
    /// 短时间内连续的滚动锁定同一个视口只为一件事：它滚到头了，这一串滚动不中途交给外面的视口。
    /// 指针离开了锁定的视口、或者它里面能滚的视口到了指针下面，都交给指针下的那个
    fn route_latched(&mut self, delta: [f32; 2], now: f64) -> Option<(NodeId, [bool; 2])> {
        let axis = dominant(delta);
        let path = self.wheel_path(delta)?;
        let routed = self.pick(&path, delta);
        let depth = |id: NodeId| path.iter().position(|&n| n == id);
        if let Some((id, mask, latched, at)) = self.scrolls.gesture.wheel
            && latched == axis
            && now - at < WHEEL_LATCH
            && let Some(inner) = depth(id)
            && routed.and_then(|(r, _)| depth(r)).is_some_and(|outer| outer < inner)
        {
            self.scrolls.gesture.wheel = Some((id, mask, axis, now));
            return Some((id, mask));
        }
        let (id, mask) = routed?;
        self.scrolls.gesture.wheel = Some((id, mask, axis, now));
        Some((id, mask))
    }

    /// 画视口的滚动条（在子元素之后，盖在内容上）。每条是一个合成层，位置和透明度在层上，
    /// 滚动、淡入淡出都不重建图元
    pub(super) fn paint_bars(
        &self,
        id: NodeId,
        parent: u32,
        clip: u32,
        local: Affine,
        opacity: f32,
        list: &mut DisplayList,
        layers: &mut Vec<LayerInfo>,
    ) {
        let node = &self.nodes[id];
        let Some(s) = node.scroll.as_deref() else { return };
        for axis in 0..2 {
            let Some((rect, position)) = scroll::thumb(s, node.bounds, axis) else { continue };
            // 和渲染线程算的一样（它按自己那份状态覆盖这里的位置和透明度）
            let position = scroll::snap(position, s.scale);
            layers.push(LayerInfo {
                key: thumb_key(id, axis),
                parent,
                base: local,
                base_opacity: opacity,
                center: rect.center(),
                values: LayerValues {
                    translate: [0.0; 2],
                    scale: 1.0,
                    rotation: 0.0,
                    opacity: s.bar_opacity(),
                    offset: if axis == 0 { [position, 0.0] } else { [0.0, position] },
                },
                group: false,
                depth: None,
            });
            let mut quad = Quad::new(rect.x, rect.y, rect.width, rect.height).radius(BAR_THICKNESS * 0.5).fill(s.bar.unwrap_or(self.paint.colors().scrollbar));
            quad.layer = layers.len() as u32;
            quad.clip = clip;
            list.quads.push(quad);
        }
    }

    /// 视口里滚动条此刻的矩形（视口的布局坐标），看不见时为 `None`
    fn visible_thumb(&self, id: NodeId, axis: usize) -> Option<Rect> {
        let node = self.nodes.get(id)?;
        let s = node.scroll.as_deref()?;
        if s.bar_hiding() && s.bar_opacity() < 0.05 {
            return None;
        }
        let (mut rect, position) = scroll::thumb(s, node.bounds, axis)?;
        if axis == 0 {
            rect.x += position;
        } else {
            rect.y += position;
        }
        Some(rect)
    }

    /// 按下时在看得见的滚动条上（四周放宽几像素，好按）：开始拖它，不再当作点击
    pub(super) fn begin_thumb_drag(&mut self) -> bool {
        let Some(point) = self.router.pointer() else { return false };
        for &id in self.router.hovered().iter().rev() {
            let Some(local) = self.nodes.get(id).and_then(|n| n.scroll.as_ref().and(n.world_invert(point))) else { continue };
            for axis in 0..2 {
                let Some(rect) = self.visible_thumb(id, axis) else { continue };
                let grab = Rect { x: rect.x - 3.0, y: rect.y - 3.0, width: rect.width + 6.0, height: rect.height + 6.0 };
                if grab.contains(local) {
                    let Some(s) = self.scroller_mut(id) else { return false };
                    let offset = s.offset[axis];
                    // 拖着时一直显示
                    s.hold = true;
                    self.scrolls.drag = Some(ThumbDrag { viewport: id, axis, start: local[axis], offset });
                    self.wake(id);
                    return true;
                }
            }
        }
        false
    }

    /// 拖着滚动条移动：滚动条跟着指针，内容按比例滚
    pub(super) fn drag_thumb(&mut self) {
        let Some(&ThumbDrag { viewport, axis, start, offset }) = self.scrolls.drag.as_ref() else { return };
        let Some(point) = self.router.pointer() else { return };
        let Some(node) = self.nodes.get(viewport) else {
            self.scrolls.drag = None;
            return;
        };
        let Some(local) = node.world_invert(point) else { return };
        let Some(s) = node.scroll.as_deref() else { return };
        let ratio = scroll::thumb_ratio(s, axis);
        if let Some(s) = self.scroller_mut(viewport) {
            s.scroll_to(axis, offset + (local[axis] - start) * ratio, false);
        }
        self.wake(viewport);
    }

    pub(super) fn end_thumb_drag(&mut self) {
        let Some(drag) = self.scrolls.drag.take() else { return };
        let now = self.anim.host();
        if let Some(s) = self.scroller_mut(drag.viewport) {
            s.hold = false;
            s.reveal(now);
            self.wake(drag.viewport);
        }
    }

    /// 正在拖滚动条
    pub(super) fn dragging_thumb(&self) -> bool {
        self.scrolls.drag.is_some()
    }

    /// 键盘滚动：交给 `chain`（从内到外）里第一个在这个方向上能滚的视口。方向键一行，翻页键一屏（留一行重叠），
    /// Home / End 到头尾。返回有没有视口接
    pub(super) fn key_scroll(&mut self, chain: &[NodeId], axis: usize, action: KeyScroll) -> bool {
        for &id in chain {
            let Some(s) = self.scroller_mut(id) else { continue };
            if !s.axes[axis] || s.max[axis] <= 0.0 {
                continue;
            }
            let page = scroll::page(s.metrics.viewport[axis]);
            match action {
                KeyScroll::Line(sign) => s.wheel(axis, sign * scroll::line()),
                KeyScroll::Page(sign) => s.wheel(axis, sign * page),
                KeyScroll::Start => s.scroll_to(axis, 0.0, true),
                KeyScroll::End => s.scroll_to(axis, s.max[axis], true),
            }
            self.wake(id);
            return true;
        }
        false
    }

    /// 元素的句柄要求滚进视口：记下，这一帧定位之后执行（[`Self::run_reveals`]）
    pub(super) fn request_reveal(&mut self, id: NodeId, animated: bool) {
        self.scrolls.reveals.push((id, animated));
    }

    /// 定位之后：句柄要求的元素滚进视口（用这一帧的布局：刚挂上、刚改过的元素也在新的位置上）
    pub(super) fn run_reveals(&mut self) {
        for (id, animated) in mem::take(&mut self.scrolls.reveals) {
            if self.nodes.get(id).is_some_and(|node| !node.exiting) {
                self.scroll_into_view(id, animated);
            }
        }
    }

    /// 键盘把焦点移到滚动视口里看不见的元素上：由内到外，每个视口滚到正好露出它（四周留一点）。
    /// `animated` 为假时立即跳过去（浮层打开时把选中的项滚进来）；浮层的表面总是立即跳
    pub(super) fn scroll_into_view(&mut self, id: NodeId, animated: bool) {
        let Some(rect) = self.nodes.get(id).map(|node| node.bounds) else { return };
        self.scroll_rect_into_view(id, rect, animated);
    }

    /// 同 [`Self::scroll_into_view`]，露出的是元素里的一块（布局坐标），比如选区的活动端
    pub(super) fn scroll_rect_into_view(&mut self, id: NodeId, mut rect: Rect, animated: bool) {
        const PAD: f32 = 8.0;
        let Some(node) = self.nodes.get(id) else { return };
        // 自己和祖先（到滚动视口的内容为止）的平移也算上：靠平移摆放的元素（能拖动排序的列表里的行）
        let mut moved = Some(id);
        while let Some(m) = moved {
            let Some(n) = self.nodes.get(m) else { break };
            if n.scroll.is_some() || n.shift != [0.0; 2] {
                break;
            }
            rect.x += n.paint.translate[0];
            rect.y += n.paint.translate[1];
            moved = n.parent;
        }
        let mut current = node.parent;
        while let Some(viewport) = current {
            let Some(v) = self.nodes.get(viewport) else { return };
            current = v.parent;
            let Some(s) = v.scroll.as_deref() else { continue };
            let Some(content) = self.nodes.get(s.content) else { continue };
            // 内容里的位置（布局坐标，不含滚动）
            let start = [rect.x - content.bounds.x, rect.y - content.bounds.y];
            let size = [rect.width, rect.height];
            let mut target = s.offset;
            for axis in 0..2 {
                if !s.axes[axis] {
                    continue;
                }
                let view = s.metrics.viewport[axis];
                if start[axis] - PAD < target[axis] {
                    target[axis] = start[axis] - PAD;
                } else if start[axis] + size[axis] + PAD > target[axis] + view {
                    target[axis] = (start[axis] + size[axis] + PAD - view).min(start[axis] - PAD);
                }
            }
            let bounds = v.bounds;
            let Some(s) = self.scroller_mut(viewport) else { continue };
            // 浮层的表面（菜单、选项列表）立即跟上：按住方向键时焦点项一直看得见
            let animated = animated && s.surface.is_none();
            let mut shifted = [0.0; 2];
            for axis in 0..2 {
                if s.axes[axis] && target[axis] != s.offset[axis] {
                    s.scroll_to(axis, target[axis], animated);
                }
                shifted[axis] = if s.axes[axis] { target[axis].clamp(0.0, s.max[axis]) } else { 0.0 };
            }
            if !animated {
                let now = self.anim.host();
                self.sync_scroll(viewport, now);
            }
            self.wake(viewport);
            // 外层视口要露出的：滚动之后的位置，夹在这个视口里
            let x0 = (rect.x - shifted[0]).max(bounds.x);
            let y0 = (rect.y - shifted[1]).max(bounds.y);
            let x1 = (rect.x - shifted[0] + rect.width).min(bounds.x + bounds.width);
            let y1 = (rect.y - shifted[1] + rect.height).min(bounds.y + bounds.height);
            rect = Rect { x: x0, y: y0, width: (x1 - x0).max(0.0), height: (y1 - y0).max(0.0) };
        }
    }

    /// 指针靠近能滚的视口的边缘：显示滚动条（可以拖）
    pub(super) fn reveal_bars(&mut self) {
        let Some(point) = self.router.pointer() else { return };
        let now = self.anim.host();
        for index in (0..self.router.hovered().len()).rev() {
            let id = self.router.hovered()[index];
            let Some(node) = self.nodes.get(id) else { continue };
            let Some(s) = node.scroll.as_deref() else { continue };
            let Some(local) = node.world_invert(point) else { continue };
            let b = node.bounds;
            let near = (0..2).any(|axis| {
                scroll::thumb(s, b, axis).is_some()
                    && if axis == 0 { b.y + b.height - local[1] < BAR_HOVER } else { b.x + b.width - local[0] < BAR_HOVER }
            });
            if near && let Some(s) = self.scroller_mut(id) {
                s.reveal(now);
                self.wake(id);
            }
        }
    }
}
