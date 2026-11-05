//! 显示列表的原地修补：只改了颜色、圆角、边框、一层外投影这类不影响结构和图元个数的属性时，不重新生成整个显示列表，
//! 只改受影响的那几个图元；交给渲染线程时只带改动的几段（[`ScenePatch`]），渲染线程只上传这几段。
//!
//! 生成显示列表时顺手记一张"节点 → 图元"的表（[`Spans`]，按节点在存储里的下标）：自己的形状正好是一个普通的形状图元
//! （[`super::walk`] 里 `make_quad` 写的那个）的节点记下它的下标；没画（透明、看不见）的记下没画；走了别的画法的不记。
//! 表跟着这一份列表：重新生成一次就换一个编号（`epoch`），之前记的全部作废。
//!
//! 修补时按表找到图元，用和生成时同一个函数（`style_quad`）重写它，所以两条路写出来的逐位相同（有测试核对）。
//! 改不了的时候（表里没有这个节点、它要多画或少画图元了、它不再是普通的形状、这个图元被祖先的滤镜改过、拖放的影子
//! 把子树画了两遍……）交回调用方整个重新生成。

use std::ops::Range;
use std::sync::Arc;

use scene::DisplayList;

use super::NodeStore;
use super::node::{NodeId, slot_index};
use super::paint::PaintFlags;
use super::walk::style_quad;

/// 一个节点在这份显示列表里的形状图元
#[derive(Clone, Copy, Default)]
pub(super) struct Span {
    /// 记下它时显示列表的编号；和 [`Spans::epoch`] 不一样就是作废了
    epoch: u32,
    /// 图元的下标，或者 [`Span::HIDDEN`]、[`Span::INVISIBLE`]
    quad: u32,
}

impl Span {
    pub(super) fn new(epoch: u32, quad: u32) -> Self {
        Self { epoch, quad }
    }

    /// 透明度大于 0，但没有要画的（没有背景、边框、投影）
    pub(super) const HIDDEN: u32 = u32::MAX - 1;
    /// 透明度是 0：什么都不画，改颜色这类属性也不会画
    pub(super) const INVISIBLE: u32 = u32::MAX;
}

/// 自己的形状不是一个普通形状图元的特征（有这些时不记，修补时也不认）
const IRREGULAR: PaintFlags = PaintFlags::union(&[
    PaintFlags::CONTENT,
    PaintFlags::EFFECTS,
    PaintFlags::BACKDROP,
    PaintFlags::SHADER,
    PaintFlags::LAYERED,
    PaintFlags::GRADIENT,
]);

/// 节点的形状是一个普通形状图元（或者什么都不画）：可以记进表里、可以原地修补
#[inline(always)]
pub(super) fn regular(flags: PaintFlags) -> bool {
    !flags.intersects(IRREGULAR)
}

/// 能原地修补的属性：只改形状图元里的圆角、填充色、边框、一层外投影
pub(super) fn patches(property: crate::property::PropId) -> bool {
    use crate::property::PropId;
    matches!(property, PropId::BACKGROUND | PropId::BORDER_COLOR | PropId::BORDER_WIDTH | PropId::RADIUS | PropId::SHADOW)
}

/// "节点 → 图元"的表和等着修补的节点
#[derive(Default)]
pub(super) struct Spans {
    /// 这一份显示列表的编号：重新生成一次加一
    epoch: u32,
    /// 按节点在存储里的下标
    table: Vec<Span>,
    /// 生成之后又被改过的图元（祖先的滤镜直接改了它的颜色）：不能按表修补
    folded: Vec<u32>,
    /// 这一份能不能修补：拖放的影子把源元素的子树画了两遍（同一个节点两个图元）时不能
    usable: bool,
    /// 等着修补的节点；`true` 的是只改了透明度（半透明的元素还是半透明：只改它的层的透明度）
    pending: Vec<(NodeId, bool)>,
    /// 修补时用的：要改的图元和它的节点（留着复用，修补一次不分配）
    work: Vec<(u32, NodeId)>,
}

/// 显示列表只改了这几段图元（按下标排好、不重叠、相邻的合成一段）：结构、图元个数、裁剪、分组、渐变、合成层表都没变
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScenePatch {
    /// 改了的图元
    pub quads: Vec<Range<u32>>,
}

impl ScenePatch {
    /// 一组图元下标（可能重复、没排序）合成几段；`indices` 清空（容量留着）
    pub(super) fn from_indices(indices: &mut Vec<u32>) -> Self {
        indices.sort_unstable();
        indices.dedup();
        let mut quads: Vec<Range<u32>> = Vec::new();
        for i in indices.drain(..) {
            match quads.last_mut() {
                Some(last) if last.end == i => last.end = i + 1,
                _ => quads.push(i..i + 1),
            }
        }
        Self { quads }
    }

    /// 改了几个图元
    pub fn len(&self) -> usize {
        self.quads.iter().map(|r| (r.end - r.start) as usize).sum()
    }

    /// 什么都没改
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }
}

impl Spans {
    /// 要重新生成一份显示列表：换一个编号（之前记的全部作废），表长到放得下每个节点，等着修补的都不用了。
    /// 借出表（生成时写），返回它和新的编号
    pub(super) fn begin(&mut self, slots: usize) -> (Vec<Span>, u32) {
        self.epoch = self.epoch.wrapping_add(1).max(1);
        let mut table = std::mem::take(&mut self.table);
        if table.len() < slots {
            table.resize(slots, Span::default());
        }
        self.folded.clear();
        self.pending.clear();
        (table, self.epoch)
    }

    /// 生成完：还回表；`folded` 是生成之后又被改过的图元，`usable` 为假时这一份不能修补
    pub(super) fn finish(&mut self, table: Vec<Span>, folded: &[u32], usable: bool) {
        self.table = table;
        self.folded.extend_from_slice(folded);
        self.usable = usable;
    }

    /// 这个节点的颜色、圆角、边框、一层外投影变了，等着修补
    pub(super) fn request(&mut self, id: NodeId) {
        self.pending.push((id, false));
    }

    /// 这个半透明元素的透明度变了（还是半透明），等着改它的层的透明度
    pub(super) fn request_opacity(&mut self, id: NodeId) {
        self.pending.push((id, true));
    }

    /// 有等着修补的节点
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 按表修补 `list`，改了的图元下标接在 `touched` 后面，返回改了几个；只改层的透明度的节点接在 `layers` 后面。
    /// 改不了时返回 `None`（等着的都丢掉，调用方整个重新生成）。
    /// 先查完能不能改，再去动列表：`list` 被渲染线程或者别处拿着时要复制一份，改不了的时候不白复制
    pub(super) fn apply(&mut self, nodes: &NodeStore, list: &mut Arc<DisplayList>, touched: &mut Vec<u32>, layers: &mut Vec<NodeId>) -> Option<usize> {
        let mut pending = std::mem::take(&mut self.pending);
        let mut work = std::mem::take(&mut self.work);
        work.clear();
        let start = layers.len();
        let ok = self.usable && pending.iter().all(|&(id, opacity)| if opacity { self.check_opacity(nodes, id, layers) } else { self.check(nodes, id, &mut work) });
        if !ok {
            layers.truncate(start);
        }
        pending.clear();
        self.pending = pending;
        if !ok {
            self.work = work;
            return None;
        }
        // 同一个节点一次改了几个属性（颜色和圆角……）时只改一次
        work.sort_unstable_by_key(|&(q, _)| q);
        work.dedup_by_key(|&mut (q, _)| q);
        let count = work.len();
        if count > 0 {
            let list = Arc::make_mut(list);
            let mut moved = false;
            for &(q, id) in &work {
                let quad = &mut list.quads[q as usize];
                let before = quad.bounds();
                style_quad(quad, &nodes[id].paint);
                moved |= quad.bounds() != before;
                touched.push(q);
            }
            // 有分组时每层图元的范围要跟着变（投影变了，图元的外接矩形就变了）
            if moved && !list.groups.is_empty() {
                super::walk::layer_bounds(list);
            }
        }
        self.work = work;
        Some(count)
    }

    /// 只改了透明度的半透明元素：这一份里它是一个层（半透明的元素都是分组的候选，有自己的层），还是半透明，就只改层的透明度
    fn check_opacity(&self, nodes: &NodeStore, id: NodeId, layers: &mut Vec<NodeId>) -> bool {
        let (Some(node), Some(span)) = (nodes.get(id), self.table.get(slot_index(id))) else { return false };
        let opacity = node.paint.opacity;
        if span.epoch != self.epoch || !regular(node.flags) || !(opacity > 0.0 && opacity < 1.0) {
            return false;
        }
        layers.push(id);
        true
    }

    /// 看一个等着修补的节点改不改得了：改得了时要改的图元记进 `work`（不用改的不记）
    fn check(&self, nodes: &NodeStore, id: NodeId, work: &mut Vec<(u32, NodeId)>) -> bool {
        let (Some(node), Some(span)) = (nodes.get(id), self.table.get(slot_index(id))) else { return false };
        if span.epoch != self.epoch || !regular(node.flags) {
            return false;
        }
        match span.quad {
            // 透明度是 0：改什么都不画
            Span::INVISIBLE => true,
            // 原来没画：现在还是没东西画才不用动
            Span::HIDDEN => !node.paint.visible(),
            q => {
                // 现在没东西画了（图元要少一个）、或者这个图元生成之后被改过：改不了
                if !node.paint.visible() || self.folded.contains(&q) {
                    return false;
                }
                work.push((q, id));
                true
            }
        }
    }
}
