//! 命中测试：窗口里的一点下面从外到内是哪些元素。先看浮层（模态浮层挡住下面的一切），再看根元素；
//! 按变换后的形状判断，后画的兄弟（叠放次序大的）在上面。用的是绘制遍历写下的位置和屏幕变换。

use smallvec::SmallVec;

use super::super::{NodeId, Tree};

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
        let mut candidates = roots;
        // 滚动视口里包着内容的托架在滚动方向上大小是 0，不按它的范围判断（视口已经包含这一点）
        let mut holder = None;
        'descend: loop {
            for &id in candidates.iter().rev() {
                let node = &self.nodes[id];
                let hit = match node.extra.as_deref() {
                    None => holder == Some(id) || node.world.invert(point).is_some_and(|local| node.bounds.contains(local)),
                    Some(extra) => node.hit_extra(extra, point, holder == Some(id)),
                };
                if !node.exiting && hit {
                    out.push(id);
                    candidates = self.nodes.paint_order(id);
                    holder = node.scroll.as_ref().map(|s| s.holder);
                    continue 'descend;
                }
            }
            return;
        }
    }
}
