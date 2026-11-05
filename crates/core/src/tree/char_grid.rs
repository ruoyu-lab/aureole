//! 字符网格（[`crate::char_grid`]）在元素树里的部分：数据改了重画、行列数变了重新布局。
//! 它是一种元素种类（量大小、插入点闪烁、画、选格子和 ⌘C ⌘A 见 `tree/kinds/char_grid.rs`），生成图元见 `char_grid.rs`。

use super::{NodeId, Tree};
use crate::char_grid::CharGridBlock;
use crate::gradient::Fill;

impl Tree {
    /// 网格的数据改了：重画；行列数变了重新布局
    pub(super) fn char_grid_changed(&mut self, id: NodeId) {
        self.paint.invalidate();
        self.accessibility.touch(id);
        if let Some(block) = self.nodes.kind_mut::<CharGridBlock>(id)
            && block.take_resized()
        {
            self.remeasure_kind(id);
        }
    }

    /// 网格底下看得见的颜色：自己或最近的祖先的不透明背景，都没有时是默认的窗口底色
    pub(super) fn backdrop(&self, id: NodeId) -> scene::Color {
        let mut current = Some(id);
        while let Some(node) = current.and_then(|id| self.nodes.get(id)) {
            if let Fill::Color(color) = node.paint.background
                && color.0[3] == 255
            {
                return color;
            }
            current = node.parent;
        }
        self.paint.colors().rich.background
    }
}
