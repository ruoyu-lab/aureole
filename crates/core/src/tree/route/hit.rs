//! 命中测试：窗口里的一点下面从外到内是哪些元素。先看浮层（模态浮层挡住下面的一切），再看根元素；
//! 按变换后的形状判断，后画的兄弟（叠放次序大的）在上面。用的是绘制遍历写下的位置和屏幕变换。
//!
//! 形状：先看布局矩形，再看画出来的形状——圆角外面（和画圆角的距离场同一个公式）、路径没画到的地方不算点中，
//! 点击落到下面的元素上。这一点上有子元素时（元素没有裁剪子元素），仍然算它和那个子元素；裁剪子元素的圆角元素，
//! 角外面它的子元素也点不到，和画出来的一样。

use smallvec::SmallVec;

use super::super::{NodeId, Tree};
use crate::vector::PathBlock;

/// 命中测试的结果：从外到内，一般不深，放在栈上（指针移动不分配）
pub(in crate::tree) type HitPath = SmallVec<[NodeId; 16]>;

impl Tree {
    /// 指针下从外到内的元素。先看浮层（模态浮层挡住下面的一切），再看根元素
    pub(in crate::tree) fn hit_path(&self, point: [f32; 2]) -> Vec<NodeId> {
        let mut path = HitPath::new();
        self.hit_into(point, &mut path);
        path.into_vec()
    }

    /// 同 [`Self::hit_path`]，结果写进 `out`（先清空）
    pub(in crate::tree) fn hit_into(&self, point: [f32; 2], out: &mut HitPath) {
        out.clear();
        if self.hit_overlays(point, out) {
            return;
        }
        self.hit_from(self.nodes.roots(), point, out);
    }

    /// 从 `roots` 往下找，命中的元素接在 `out` 后面：按变换后的形状判断；后画的兄弟（叠放次序大的）在上面，优先命中
    pub(in crate::tree) fn hit_from(&self, roots: &[NodeId], point: [f32; 2], out: &mut HitPath) {
        self.hit_below(roots, point, out, None);
    }

    /// 同 [`Self::hit_from`]。`holder`：滚动视口里包着内容的托架在滚动方向上大小是 0，不按它的范围判断（视口已经包含这一点）
    fn hit_below(&self, roots: &[NodeId], point: [f32; 2], out: &mut HitPath, mut holder: Option<NodeId>) {
        let mut candidates = roots;
        'descend: loop {
            for &id in candidates.iter().rev() {
                let node = &self.nodes[id];
                let is_holder = holder == Some(id);
                let hit = match node.extra.as_deref() {
                    None => is_holder || node.world.invert(point).is_some_and(|local| node.bounds.contains(local)),
                    Some(extra) => node.hit_extra(extra, point, is_holder),
                };
                if node.exiting || !hit {
                    continue;
                }
                let inner_holder = node.scroll.as_ref().map(|s| s.holder);
                if !is_holder && !self.inside_shape(id, point) {
                    // 矩形里、形状外：裁剪子元素的连子元素一起不算；不裁剪的，这一点上有子元素时仍然算它
                    if node.paint.clip {
                        continue;
                    }
                    let mut inner = HitPath::new();
                    self.hit_below(self.nodes.paint_order(id), point, &mut inner, inner_holder);
                    if inner.is_empty() {
                        continue;
                    }
                    out.push(id);
                    out.extend(inner);
                    return;
                }
                out.push(id);
                candidates = self.nodes.paint_order(id);
                holder = inner_holder;
                continue 'descend;
            }
            return;
        }
    }

    /// 点（窗口坐标）在元素画出来的形状里吗：圆角外面、路径没画到的地方不算。只在布局矩形已经包含这一点时调用
    fn inside_shape(&self, id: NodeId, point: [f32; 2]) -> bool {
        let node = &self.nodes[id];
        let path = self.nodes.kind::<PathBlock>(id);
        let radius = node.paint.radius;
        if radius <= 0.0 && path.is_none() {
            return true;
        }
        let Some(local) = node.world_invert(point) else { return true };
        let b = node.bounds;
        let p = [local[0] - b.x, local[1] - b.y];
        if radius > 0.0 && !in_rounded_rect([b.width, b.height], radius, p) {
            return false;
        }
        match (path, self.target_geometry(id)) {
            (Some(path), Some((_, _, content))) => path.contains(p, content),
            _ => true,
        }
    }
}

/// 元素坐标里的一点在 `size` 大、圆角 `radius` 的圆角矩形里吗：和着色器画圆角用同一个距离场（`sd_rounded_rect`），
/// 圆角大过短边的一半时也和画出来的一样
fn in_rounded_rect(size: [f32; 2], radius: f32, p: [f32; 2]) -> bool {
    let half = [size[0] * 0.5, size[1] * 0.5];
    let q = [(p[0] - half[0]).abs() - half[0] + radius, (p[1] - half[1]).abs() - half[1] + radius];
    let outside = (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2)).sqrt();
    outside + q[0].max(q[1]).min(0.0) - radius <= 0.0
}
