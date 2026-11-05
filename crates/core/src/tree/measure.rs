//! 布局结果写回信号（[`crate::Element::report_bounds`]）：每次重新布局之后，把登记过的元素在窗口里的布局位置和大小
//! 写进它们的信号，变了才写。在同一帧里交给绑定（`advance` 写回之后界面又变了就再做一遍）。
//! 第一次写入不做动画：绑定在挂载时读到的是默认值，第一次量到的才是真实位置，不应该从 0 滑过去。

use reactive::{Signal, batch};

use super::{NodeId, Tree, TreeRef, without_animation};
use crate::geometry::Rect;

/// 登记了测量的元素
#[derive(Default)]
pub(super) struct Measures {
    list: Vec<Measured>,
}

impl Measures {
    /// 登记一个元素：之后每次重新布局都把它的位置和大小写进信号
    pub(super) fn add(&mut self, id: NodeId, signal: Signal<Rect>) {
        self.list.push(Measured { id, signal, last: None });
    }
}

/// 一个登记了测量的元素
struct Measured {
    id: NodeId,
    signal: Signal<Rect>,
    /// 上次写进去的值；`None` 表示还没写过
    last: Option<Rect>,
}

impl Tree {
    /// 元素在窗口里的布局位置（沿布局树累加，不算变换、滚动偏移）和大小
    pub(super) fn layout_rect(&self, id: NodeId) -> Option<Rect> {
        let node = self.nodes.get(id)?;
        let layout = self.layout.try_get(node.layout)?;
        let (mut x, mut y) = (layout.location.x, layout.location.y);
        let mut current = self.layout.parent(node.layout);
        while let Some(parent) = current {
            if let Some(l) = self.layout.try_get(parent) {
                x += l.location.x;
                y += l.location.y;
            }
            current = self.layout.parent(parent);
        }
        // 富文本里的行内元素（和它的子孙）：布局树到它为止，再加上它在文字里的位置和文字的位置
        if let Some([ax, ay]) = self.inline_origin_in_layout(id) {
            x += ax;
            y += ay;
        }
        Some(Rect { x, y, width: layout.size.width, height: layout.size.height })
    }

    /// 变了的测量结果：(信号, 值, 是不是第一次)；顺便丢掉已经卸载的元素
    fn measure_changes(&mut self) -> Vec<(Signal<Rect>, Rect, bool)> {
        let mut measured = std::mem::take(&mut self.measures.list);
        measured.retain(|m| self.nodes.contains_key(m.id));
        let mut changes = Vec::new();
        for m in &mut measured {
            let Some(rect) = self.layout_rect(m.id) else { continue };
            if m.last != Some(rect) {
                changes.push((m.signal, rect, m.last.is_none()));
                m.last = Some(rect);
            }
        }
        measured.append(&mut self.measures.list);
        self.measures.list = measured;
        changes
    }
}

/// 布局之后（已释放树的借用）写回测量结果，返回有没有写
pub(super) fn flush(tree: &TreeRef) -> bool {
    let changes = {
        let mut t = tree.borrow_mut();
        if t.measures.list.is_empty() {
            return false;
        }
        t.measure_changes()
    };
    if changes.is_empty() {
        return false;
    }
    let (first, later): (Vec<_>, Vec<_>) = changes.into_iter().partition(|(_, _, first)| *first);
    if !first.is_empty() {
        without_animation(|| batch(|| first.iter().for_each(|(signal, rect, _)| signal.set(*rect))));
    }
    if !later.is_empty() {
        batch(|| later.iter().for_each(|(signal, rect, _)| signal.set(*rect)));
    }
    true
}
