//! 绕排的布局（[`crate::flow`]）：绕排的元素是容器的绝对定位子元素，放在下一个子元素的顶上、贴着内容区的左边或右边；
//! 容器里直接放的文字拿到它们占掉的区域（换算到文字自己的坐标），逐行断行时让开（见 `text.rs` 的 `break_lines`）。
//!
//! 元素的大小和段落的位置互相依赖（元素的顶跟着下一段，段落的高度跟着让开了多少），所以和量行高的虚拟列表一样：
//! 布局之后检查一遍，位置或让开的区域变了就再布局一次，通常第二遍就稳定了。

use taffy::{LengthPercentageAuto, Position};

use super::{NodeId, Tree, TreeRef};
use crate::flow::FlowProps;
use crate::overlay::Side;
use crate::geometry::Rect;
use crate::text::Around;

/// 一个绕排容器
pub(super) struct Flow {
    node: NodeId,
    gap: f32,
    /// 绕排的子元素和靠哪边
    wraps: Vec<(NodeId, Side)>,
}

/// 挂载时登记：绕排的子元素改成绝对定位（先放在左上角或右上角，布局之后再挪到位），叠在兄弟上面（指针先点到它）
pub(super) fn attach(tree: &TreeRef, id: NodeId, props: FlowProps, children: &[Option<NodeId>]) {
    let mut t = tree.borrow_mut();
    let wraps: Vec<(NodeId, Side)> =
        props.wraps.iter().filter_map(|&(index, side)| Some((children.get(index).copied().flatten()?, side))).collect();
    for &(child, side) in &wraps {
        t.update_style(child, |s| {
            s.position = Position::Absolute;
            s.inset.top = LengthPercentageAuto::length(0.0);
            let edge = LengthPercentageAuto::length(0.0);
            if side == Side::Right { s.inset.right = edge } else { s.inset.left = edge }
        });
        t.with_input(child, |i| i.z = 1);
    }
    t.sort_children(id);
    t.flows.list.push(Flow { node: id, gap: crate::flow::GAP, wraps });
}

/// 绕排容器的登记
#[derive(Default)]
pub(super) struct Flows {
    list: Vec<Flow>,
}

impl Tree {
    /// 绕排的间距（属性 `flow_gap`）：变了要重新布局
    pub(super) fn set_flow_gap(&mut self, id: NodeId, gap: f32) {
        if let Some(flow) = self.flows.list.iter_mut().find(|f| f.node == id)
            && flow.gap != gap
        {
            flow.gap = gap;
            self.layout.invalidate();
        }
    }

    /// 绕排容器的间距
    pub(super) fn flow_gap(&self, id: NodeId) -> Option<f32> {
        self.flows.list.iter().find(|f| f.node == id).map(|f| f.gap)
    }

    /// 布局之后：把绕排的元素挪到下一个子元素的顶上，算出每段文字要让开的区域。有变化时返回 true（要再布局一次）
    pub(super) fn place_flows(&mut self) -> bool {
        if self.flows.list.is_empty() {
            return false;
        }
        let mut flows = std::mem::take(&mut self.flows.list);
        flows.retain(|f| self.nodes.contains_key(f.node));
        let mut changed = false;
        for flow in &flows {
            changed |= self.place_flow(flow);
        }
        flows.append(&mut self.flows.list);
        self.flows.list = flows;
        changed
    }

    fn place_flow(&mut self, flow: &Flow) -> bool {
        let Some(container) = self.layout.try_get(self.nodes[flow.node].layout) else { return false };
        let (pad, border, size) = (container.padding, container.border, container.size);
        // 内容区的左右边（相对容器的边框盒）；绝对定位的偏移从边框里面（内边距盒）算起
        let (left_edge, right_edge) = (pad.left + border.left, size.width - pad.right - border.right);
        let children = self.nodes[flow.node].children.clone();
        let is_wrap = |id: NodeId| flow.wraps.iter().find(|(w, _)| *w == id).map(|(_, side)| *side);
        let layout_of = |t: &Tree, id: NodeId| t.nodes.get(id).and_then(|n| t.layout.try_get(n.layout)).copied();

        // 绕排元素的位置：下一个不绕排的子元素的顶（没有时是上一个的底），同一边上不和前一个重叠
        let mut boxes: Vec<(Side, Rect)> = Vec::new();
        // 后面还有段落的绕排元素里最低的底边：最后一段延长到这里。最后面的绕排元素不算（它的位置跟着最后一段的底，
        // 再让最后一段跟着它会越推越低）
        let mut lowest = f32::MIN;
        let mut changed = false;
        let mut flow_bottom = pad.top + border.top;
        for (i, &child) in children.iter().enumerate() {
            let Some(layout) = layout_of(self, child) else { continue };
            let Some(side) = is_wrap(child) else {
                flow_bottom = layout.location.y + layout.size.height;
                continue;
            };
            let next = children[i + 1..].iter().find(|&&c| is_wrap(c).is_none()).and_then(|&c| layout_of(self, c));
            let mut top = next.map_or(flow_bottom, |l| l.location.y);
            if let Some((_, above)) = boxes.iter().rev().find(|(s, _)| *s == side) {
                top = top.max(above.y + above.height + flow.gap);
            }
            let (w, h) = (layout.size.width, layout.size.height);
            let x = if side == Side::Right { right_edge - w } else { left_edge };
            boxes.push((side, Rect { x, y: top, width: w, height: h }));
            if next.is_some() {
                lowest = lowest.max(top + h);
            }
            // 偏移相对内边距盒
            let inset = |v: f32| LengthPercentageAuto::length(v);
            let (top_inset, edge) = (top - border.top, if side == Side::Right { pad.right } else { pad.left });
            let before = self.layout.try_style(self.nodes[child].layout).map(|s| s.inset);
            self.update_style(child, |s| {
                s.inset.top = inset(top_inset);
                s.inset.bottom = LengthPercentageAuto::auto();
                if side == Side::Right {
                    s.inset.right = inset(edge);
                    s.inset.left = LengthPercentageAuto::auto();
                } else {
                    s.inset.left = inset(edge);
                    s.inset.right = LengthPercentageAuto::auto();
                }
            });
            changed |= before != self.layout.try_style(self.nodes[child].layout).map(|s| s.inset);
        }

        // 每段文字：换算到它的文字原点，算出让开的区域；最后一段文字的高度至少到最低的绕排元素底下
        let last_text = children.iter().rev().find(|&&c| is_wrap(c).is_none()).copied();
        for &child in &children {
            if is_wrap(child).is_some() {
                continue;
            }
            let Some(layout) = layout_of(self, child) else { continue };
            let origin = [
                layout.location.x + layout.padding.left + layout.border.left,
                layout.location.y + layout.padding.top + layout.border.top,
            ];
            let content_right = origin[0] + layout.content_box_width();
            let around: Vec<Around> = boxes
                .iter()
                .map(|(side, r)| Around {
                    top: r.y - origin[1],
                    bottom: r.y + r.height - origin[1],
                    left: if *side == Side::Left { (r.x + r.width + flow.gap - origin[0]).max(0.0) } else { 0.0 },
                    right: if *side == Side::Right { (content_right - (r.x - flow.gap)).max(0.0) } else { 0.0 },
                })
                .filter(|a| a.bottom > 0.0)
                .collect();
            let clear = if Some(child) == last_text { (lowest - origin[1]).max(0.0) } else { 0.0 };
            let node_layout = self.nodes[child].layout;
            if let Some(block) = self.nodes.kind_mut::<crate::text::TextBlock>(child)
                && block.set_around(around, clear)
            {
                self.layout.mark_dirty(node_layout);
                changed = true;
            }
        }
        changed
    }
}
