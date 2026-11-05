//! 定位：布局之后把布局的结果落到节点上。每个节点的绝对位置 `bounds`（逻辑像素，不含变换）、带内容的节点的内容区
//! （去掉内边距和边框），和到屏幕的累积变换 `world`（命中测试用：含滚动、合成层此刻的值、3D 和共享元素的飞行）。
//!
//! 绘制遍历只读这些结果，不再查布局树、不再回写节点；命中测试、无障碍、浮层的放置读到的是这一次布局的位置，
//! 不用等这一帧画完（画布推迟了重画时也一样）。
//!
//! 位置只在布局之后变；到屏幕的变换还随变换属性、滚动、合成层的动画变：
//! - 静态地改了变换属性（平移、缩放、旋转、3D、变换中心）：只重算那个元素的子树（[`PaintState::moved`]），绘制之前做完；
//! - 合成层的动画值在主线程推进、滚动：先只记下（[`PaintState::mark_worlds`]），命中测试之前再整棵重算（[`Tree::refresh_worlds`]）。
//!
//! [`PaintState::moved`]: super::paint::PaintState::moved
//! [`PaintState::mark_worlds`]: super::paint::PaintState::mark_worlds

use super::depth;
use super::{NodeId, Tree};
use crate::geometry::{Affine, Rect, Transform};

impl Tree {
    /// 定位：全部节点的绝对位置、内容区和到屏幕的变换（布局之后、绘制之前）。
    /// 从根元素和浮层往下走（和绘制的遍历同一批起点），遮罩元素跟着它的元素
    pub(super) fn place_nodes(&mut self) {
        // 时间线在合成层上的值：到屏幕的变换要用
        self.sync_timeline_layers();
        let mut stack = self.paint.take_places();
        stack.extend(self.nodes.roots().iter().copied().chain(self.overlays.hosts()).map(|id| (id, [0.0; 2], Affine::IDENTITY)));
        self.walk(stack);
        self.place_inlines();
        self.paint.placed();
    }

    /// 富文本里的行内元素：布局树里它们没有父节点，上面的遍历把它们放在了文字的左上角；
    /// 按排版给的位置再放一遍（连同子树），外层的文字先放（它里面还可能有文字带着行内元素）
    fn place_inlines(&mut self) {
        if self.inlines.is_empty() {
            return;
        }
        for host in self.inline_hosts() {
            let Some(node) = self.nodes.get(host) else { continue };
            let outer = node.world();
            let flat = outer.is_affine() && node.extra.is_none();
            for (child, at) in self.inline_origins(host) {
                if flat {
                    let mut stack = self.paint.take_places();
                    stack.push((child, at, outer.affine));
                    self.walk(stack);
                } else {
                    self.walk_depth::<true>(child, at, outer);
                }
            }
        }
    }

    /// 合成层的动画在主线程也推进（只记值，不重建图元），滚动只改合成层的平移：命中测试之前按此刻的值刷新到屏幕的变换
    /// （位置不变）。没有记下变化时什么都不做
    pub(super) fn refresh_worlds(&mut self) {
        if !self.paint.worlds_dirty() {
            return;
        }
        self.sync_timeline_layers();
        let mut stack = self.paint.take_worlds();
        stack.extend(self.nodes.roots().iter().copied().chain(self.overlays.hosts()).map(|id| (id, Affine::IDENTITY)));
        self.walk_worlds(stack);
        self.paint.placed();
    }

    /// 静态地改了变换属性的元素：只重算它们子树的到屏幕的变换（绘制之前）。整棵重算过、或者还要整棵重算时跳过
    pub(super) fn refresh_moved(&mut self) {
        let moved = self.paint.take_moved();
        if moved.is_empty() {
            return;
        }
        for id in moved {
            let Some(node) = self.nodes.get(id) else { continue };
            // 父元素此刻到屏幕的变换（根元素、浮层是单位变换）；位置是现成的
            let (origin, outer) = match node.parent.and_then(|p| self.nodes.get(p)) {
                Some(parent) => ([parent.bounds.x, parent.bounds.y], parent.world()),
                None => ([0.0; 2], Transform::IDENTITY),
            };
            if outer.is_affine() && node.extra.is_none() {
                let mut stack = self.paint.take_worlds();
                stack.push((id, outer.affine));
                self.walk_worlds(stack);
            } else {
                self.walk_depth::<false>(id, origin, outer);
            }
        }
    }

    /// 定位：从栈里的起点（节点，父元素的位置，父元素到屏幕的变换）往下走，按布局写位置和内容区、按此刻的值写到屏幕的变换
    /// （用完把栈还回去）。平的元素只算仿射变换；遇到有 3D 设置的元素，它连同子树交给 [`Self::walk_depth`]
    fn walk(&mut self, mut stack: Vec<(NodeId, [f32; 2], Affine)>) {
        while let Some((id, origin, parent)) = stack.pop() {
            let node = &mut self.nodes[id];
            let layout = self.layout.get(node.layout);
            node.bounds = Rect { x: origin[0] + layout.location.x, y: origin[1] + layout.location.y, width: layout.size.width, height: layout.size.height };
            let insets = node.has_kind.then(|| insets(layout));
            if node.extra.is_some() {
                if let Some(insets) = insets {
                    self.nodes.set_insets(id, insets);
                }
                // 位置已经写好，子树按完整的变换算
                self.walk_depth::<true>(id, origin, Transform::from(parent));
                continue;
            }
            let p = &node.paint;
            let shift = node.offset();
            let translate = [p.translate[0] + shift[0], p.translate[1] + shift[1]];
            node.world = parent.then_local(node.bounds.center(), translate, p.scale, p.rotation);
            let (world, at) = (node.world, [node.bounds.x, node.bounds.y]);
            // 倒着压栈：先出栈的是第一个子元素，按前序走（节点大多按挂载的先后存放，前序访问内存是顺着的）
            stack.extend(node.mask().into_iter().chain(node.children.iter().rev().copied()).map(|child| (child, at, world)));
            if let Some(insets) = insets {
                self.nodes.set_insets(id, insets);
            }
        }
        self.paint.put_places(stack);
    }

    /// 只刷新到屏幕的变换（位置不变）：从栈里的起点（节点，父元素到屏幕的变换）往下走，用完把栈还回去。
    /// 遮罩元素不用（不参加命中测试）
    fn walk_worlds(&mut self, mut stack: Vec<(NodeId, Affine)>) {
        while let Some((id, parent)) = stack.pop() {
            let node = &mut self.nodes[id];
            if node.extra.is_some() {
                // 只刷新变换时不用父元素的位置
                self.walk_depth::<false>(id, [0.0; 2], Transform::from(parent));
                continue;
            }
            let p = &node.paint;
            let shift = node.offset();
            let translate = [p.translate[0] + shift[0], p.translate[1] + shift[1]];
            node.world = parent.then_local(node.bounds.center(), translate, p.scale, p.rotation);
            let world = node.world;
            // 倒着压栈：按前序走，访问内存是顺着的
            stack.extend(node.children.iter().rev().map(|&child| (child, world)));
        }
        self.paint.put_worlds(stack);
    }

    /// 遇到有 3D 设置的元素（或者父元素的变换有透视）：它和整棵子树按完整的变换算（可能有透视，第三行一路往下传）；
    /// `BOUNDS` 时也写位置（起点的位置调用方写过了，再写一遍是同样的值）
    #[cold]
    #[inline(never)]
    fn walk_depth<const BOUNDS: bool>(&mut self, id: NodeId, origin: [f32; 2], outer: Transform) {
        let mut stack = vec![(id, origin, outer)];
        while let Some((id, origin, outer)) = stack.pop() {
            if BOUNDS {
                self.place_bounds(id, origin);
            }
            // 3D 旋转可能用到父元素给的透视（父元素的位置已经放好）
            let eye = depth::parent_eye(&self.nodes, &self.nodes[id]);
            let node = &mut self.nodes[id];
            let world = node.place(&outer, node.bounds, eye, true);
            node.set_world(world);
            let at = [node.bounds.x, node.bounds.y];
            // 倒着压栈：先出栈的是第一个子元素，按前序走（节点大多按挂载的先后存放，前序访问内存是顺着的）
            stack.extend(node.mask().into_iter().chain(node.children.iter().rev().copied()).map(|child| (child, at, world)));
        }
    }

    /// 按布局写节点的绝对位置（父元素的位置 `origin` 加上相对父元素的位置）；带内容的节点再记下内容区离四边多远
    fn place_bounds(&mut self, id: NodeId, origin: [f32; 2]) {
        let node = &mut self.nodes[id];
        let layout = self.layout.get(node.layout);
        node.bounds = Rect { x: origin[0] + layout.location.x, y: origin[1] + layout.location.y, width: layout.size.width, height: layout.size.height };
        if node.has_kind {
            let insets = insets(layout);
            self.nodes.set_insets(id, insets);
        }
    }
}

/// 内容区离四边多远：内边距加边框（左、上、右、下）
#[inline(always)]
fn insets(layout: &taffy::Layout) -> [f32; 4] {
    [
        layout.padding.left + layout.border.left,
        layout.padding.top + layout.border.top,
        layout.padding.right + layout.border.right,
        layout.padding.bottom + layout.border.bottom,
    ]
}
