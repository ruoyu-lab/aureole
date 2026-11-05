//! 绕排：文章里的图片、卡片放在一边，旁边的段落绕着它排——挨着它的行变短，过了它的底边回到整行宽。
//!
//! ```
//! # use aureole::prelude::*;
//! # const ARTICLE: &str = "";
//! flow()
//!     .flow_left(image("photo.jpg").size(160, 120).radius(8)) // 下一段从它右边开始排
//!     .child(text(ARTICLE))
//!     .child(text(ARTICLE))
//!     .flow_gap(16);
//! ```
//!
//! 布局见 `tree/flow.rs`：绕排的元素用绝对定位放在下一个子元素的顶上、靠左或靠右；段落拿到它们占掉的区域，
//! 逐行断行时让开。

use crate::element::{Child, Element, View, column};
use crate::overlay::Side;

/// 默认的绕排间距：元素和旁边的字之间
pub(crate) const GAP: f32 = 12.0;

/// 绕排容器的设置：哪些子元素是绕排的、放在哪边（和字之间的间距是属性 `flow_gap`）
#[derive(Default)]
pub(crate) struct FlowProps {
    /// (第几个子元素, 靠哪边)
    pub wraps: Vec<(usize, Side)>,
}

/// 绕排容器：纵向排列的段落，可以用 [`Element::flow_left`]、[`Element::flow_right`] 在中间放图片或卡片，
/// 后面的段落绕着它排（挨着它的行从它旁边开始、变短，过了它的底边回到整行宽）。
/// 选中、富文本、链接、对齐、截断都照常工作
pub fn flow() -> Element {
    let mut element = column();
    element.flow.get_or_insert_default();
    element
}

impl Element {
    fn around(mut self, side: Side, child: impl View) -> Self {
        let index = self.children.len();
        self.children.push(Child::Static(Box::new(child.into_element())));
        self.flow.get_or_insert_default().wraps.push((index, side));
        self
    }

    /// 绕排容器（[`flow`]）里放一个靠左的元素：它的顶和下一个子元素的顶对齐，后面的段落从它右边开始排，
    /// 过了它的底边回到整行宽。元素自己的大小照常设（图片给尺寸，卡片可以跟着内容）。
    /// 绕排的元素不占文字的位置；放在最后一段旁边、比字还高时，最后一段的高度会延长到它的底边
    pub fn flow_left(self, child: impl View) -> Self {
        self.around(Side::Left, child)
    }

    /// 绕排容器（[`flow`]）里放一个靠右的元素，见 [`Element::flow_left`]
    pub fn flow_right(self, child: impl View) -> Self {
        self.around(Side::Right, child)
    }
}
