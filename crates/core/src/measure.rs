//! 测量：把布局的结果（元素在窗口里的位置和大小）写进信号，界面可以按它来摆别的东西
//! （跟着选中项滑动的指示条、和触发按钮一样宽的下拉列表、按内容高度展开的折叠面板）。
//! 元素树里的实现见 `tree/measure.rs`。

use reactive::Signal;

use crate::element::Element;
use crate::geometry::Rect;

impl Element {
    /// 每次重新布局之后，把元素的布局位置（窗口坐标，不算变换和滚动偏移）和大小写进 `rect`，变了才写。
    /// 写回在同一帧里生效：依赖它的绑定在这一帧画出来之前就更新了。第一次写入不做动画
    /// （绑定在挂载时读到的是默认值，第一次量到的才是真的）。可以给同一个元素加几个信号
    /// （组件自己量了自己，外面还想知道它在哪）。
    ///
    /// 两个元素的位置相减就是它们之间的相对位置，比如指示条要移到选中的那一项下面：
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (bar, tab) = (signal(Rect::default()), signal(Rect::default()));
    /// row()
    ///     .report_bounds(bar)
    ///     .child(text("第二项").report_bounds(tab))
    ///     .child(rect().position(0, 30).height(2).width(move || tab.get().width)
    ///         .translation_x(move || tab.get().x - bar.get().x)
    ///         .transition(Property::TranslationX, Spring::snappy()));
    /// ```
    pub fn report_bounds(mut self, rect: Signal<Rect>) -> Self {
        self.interaction().measure.push(rect);
        self
    }
}
