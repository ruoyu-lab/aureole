//! 输入目标（[`InputTarget`]）、文字输入客户端（[`TextInputClient`]）和它们的上下文。

use std::ops::Range;
use std::time::Duration;

use crate::editor::EditReport;
use crate::geometry::Rect;
use crate::input::ImePurpose;
use crate::interaction::Cursor;
use crate::property::registry::Dirty;
use crate::text::TextSystem;
use crate::{KeyEvent, PointerEvent};

/// 接收指针、按键的能力：带内容的元素自己拿焦点、接指针和按键。输入框、可选中的文字、字符网格、画布都实现它，
/// 自己定义的元素实现它就能像输入框一样拿焦点、接按键、选字。
///
/// 元素树在这几个时候调用它（都在元素上登记的处理器之后，是元素的内置行为，处理器能取消）：
///
/// | 时候 | 方法 | 取消它的内置行为 |
/// |---|---|---|
/// | 指针按下：找拿焦点的元素 | [`InputTarget::hit`]、[`InputTarget::focusable`] | [`crate::Builtin::FOCUS`] |
/// | 拿到焦点后按下、拖动、松开 | [`InputTarget::on_pointer`] | [`crate::Builtin::TEXT_SELECT`] |
/// | 手指拖动：内容能滚吗、滚多少 | [`InputTarget::can_scroll_content`]、[`InputTarget::scroll_content`] | [`crate::Builtin::SCROLL`] |
/// | 焦点在它上面时按键（目标阶段，元素上用户的处理器之后） | [`InputTarget::on_key_down`] | [`crate::Builtin::EDIT`] |
/// | 得到、失去焦点 | [`InputTarget::on_focus`] | |
/// | Tab 走不走得到 | [`InputTarget::focusable`] | |
/// | 指针形状 | [`InputTarget::cursor`] | |
/// | 有焦点时开不开输入法 | [`InputTarget::wants_text_input`] | |
///
/// 坐标都是元素自己的：左上角是原点，逻辑像素（和处理器收到的 [`crate::PointerEvent::position`] 一样），
/// 内容区（去掉内边距和边框）由上下文给出（[`TargetContext::content_rect`]）。
///
/// 方法都有默认实现：默认不拿焦点、不处理事件
pub trait InputTarget {
    /// 元素坐标里的 `position` 落在哪：整个元素、里面的一部分（链接、画布登记的区域），或者不归它（`None`：
    /// 按下时不拿焦点、不选字，像没有输入目标一样往外找）。`content` 是内容区。默认整个元素都算
    fn hit(&self, position: [f32; 2], content: Rect) -> Option<SubTarget> {
        let _ = (position, content);
        Some(SubTarget::Whole)
    }

    /// 能不能获得焦点。元素上的 [`crate::Element::focusable`] 设成 `true` 时 Tab 也走得到
    fn focusable(&self) -> Focusable {
        Focusable::No
    }

    /// 指针：拿到焦点的那次按下（[`crate::PointerEventKind::Down`]，带着连击数、修饰键），之后的移动和松开。
    /// 按下返回 [`Handled::Yes`] 时，之后的移动、松开都交给它，直到松开（拖着选字），指针移出元素也一样
    fn on_pointer(&mut self, event: &PointerEvent, cx: &mut TargetContext) -> Handled {
        let _ = (event, cx);
        Handled::No
    }

    /// 按键：焦点在这个元素上时，在元素上用户的处理器之后调用。返回 [`Handled::Yes`] 时停止传播，
    /// 外层、快捷键、内置的 Tab 和激活都收不到。正在组字时（[`KeyEvent::composing`]）不调用
    fn on_key_down(&mut self, event: &KeyEvent, cx: &mut TargetContext) -> Handled {
        let _ = (event, cx);
        Handled::No
    }

    /// 得到（`true`）、失去焦点
    fn on_focus(&mut self, focused: bool, cx: &mut TargetContext) {
        let _ = (focused, cx);
    }

    /// 指针在 `position` 时的形状；`None` 时按元素上设的（[`crate::Element::cursor`]），都没有是箭头
    fn cursor(&self, position: [f32; 2], content: Rect) -> Option<Cursor> {
        let _ = (position, content);
        None
    }

    /// 手指按在这个元素上往 `delta` 的方向拖（滚动量：正数让偏移变大，内容往上、往左走）时，内容自己能往那边滚吗。
    /// 元素自己不是滚动视口、内容又会溢出时实现（单行输入框）；能滚时手指归它（[`InputTarget::scroll_content`]），
    /// 按下处的路径收到取消，外面的视口不滚。`content` 是内容区。默认不能
    fn can_scroll_content(&self, delta: [f32; 2], content: Rect) -> bool {
        let _ = (delta, content);
        false
    }

    /// 手指拖着内容走了 `delta`（滚动量）。没有惯性，要滚动的物理用滚动视口
    fn scroll_content(&mut self, delta: [f32; 2], cx: &mut TargetContext) {
        let _ = (delta, cx);
    }

    /// 有焦点时要接文字输入：打开系统的输入法，输入法、听写的文字交给种类的 [`crate::kinds::TextInputClient`]。
    /// 也表示它自己处理点击、方向键（点击不往外传，方向键不滚外面的视口）
    fn wants_text_input(&self) -> bool {
        false
    }
}

/// 接收输入法、听写、辅助技术改文字的能力。输入框、字符网格实现它，自己定义的编辑器实现它就能接输入法。
///
/// 输入法、听写、辅助技术（[`crate::kinds::AccessibilityProvider`] 收到的改值）都走这几个方法，
/// 改动按普通的编辑记进撤销（不像写信号那样清掉撤销记录）。只有焦点在元素上、它的输入目标
/// [`InputTarget::wants_text_input`] 时才调用。
///
/// **组字的生命周期归元素树管**：
///
/// - 预编辑（[`TextInputClient::set_preedit`]）可能来好几次，空文字表示清掉；
/// - 提交（[`TextInputClient::commit`]）时自己先清掉预编辑再插入，不依赖平台先送一个空的预编辑；
/// - 焦点换走、在别处按下、元素（或祖先）被禁用、窗口失去焦点、平台关了输入法时，元素树调用
///   [`TextInputClient::cancel_composition`]，并让平台的输入法重置（丢掉它那边没确定的字），之后的提交不会落到别的元素上；
/// - 组字期间的按键（[`KeyEvent::composing`]）不交给 [`InputTarget::on_key_down`]。
///
/// 偏移都是字节偏移，在客户端自己的文字里（输入框是全文）。坐标是元素自己的
pub trait TextInputClient {
    /// 用途：密码不开输入法、请求系统的安全输入；终端、数字给屏幕键盘提示
    fn purpose(&self) -> ImePurpose {
        ImePurpose::Normal
    }

    /// 插入点（组字时是预编辑文字）的区域，元素坐标：输入法的候选框避开它
    fn caret_area(&self) -> Rect;

    /// 插入点周围的文字和其中的选区（字节范围），听写、输入法联想用；不想给（密码）时是 `None`
    fn surrounding(&self) -> Option<(String, Range<usize>)> {
        None
    }

    /// 预编辑文字（例如拼音 "ni"）和其中的插入点（预编辑文字里的字节范围，`None` 时不画插入点）；空文字表示清掉预编辑。
    /// 预编辑期间内容不算改了（不写回、不调用改值的处理器）
    fn set_preedit(&mut self, text: &str, cursor: Option<Range<usize>>, cx: &mut TargetContext);

    /// 提交文字：先清掉预编辑，再在原来的位置插入（替换选区）
    fn commit(&mut self, text: &str, cx: &mut TargetContext);

    /// 删掉插入点前 `before`、后 `after` 个字节（不含预编辑文字）
    fn delete_surrounding(&mut self, before: usize, after: usize, cx: &mut TargetContext) {
        let _ = (before, after, cx);
    }

    /// 放弃正在组的字：预编辑文字去掉，插入点回到开始组字的地方
    fn cancel_composition(&mut self, cx: &mut TargetContext);

    /// 把 `range`（字节范围）换成 `text`：辅助技术改值、替换选中的文字，听写改字
    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut TargetContext) {
        let _ = (range, text, cx);
    }
}

/// 落在元素的哪一部分（[`InputTarget::hit`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubTarget {
    /// 整个元素
    Whole,
    /// 里面的第几部分：富文本的第几个链接、画布登记的区域编号
    Part(usize),
}

/// 能不能获得焦点（[`InputTarget::focusable`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Focusable {
    /// 不能
    #[default]
    No,
    /// 只在指针按下时得到，Tab 走不到（可选中的文字、字符网格）
    Pointer,
    /// 按下和 Tab 都能得到（输入框）
    Keyboard,
}

/// 事件处理了没有
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Handled {
    /// 没处理：照常往外传
    #[default]
    No,
    /// 处理了
    Yes,
}

impl Handled {
    /// 是 [`Handled::Yes`]
    pub fn is_handled(self) -> bool {
        self == Self::Yes
    }
}

impl From<bool> for Handled {
    fn from(handled: bool) -> Self {
        if handled { Self::Yes } else { Self::No }
    }
}

/// 输入目标、文字输入客户端被调用时能用的（[`InputTarget`]、[`crate::kinds::TextInputClient`]）。
/// 坐标是元素自己的：左上角是原点，逻辑像素
pub struct TargetContext<'a> {
    pub(crate) text: &'a mut TextSystem,
    /// 元素树的帧时间，秒
    pub(crate) seconds: f64,
    /// 元素左上角在布局坐标里的位置（内置的种类按布局坐标记位置）
    pub(crate) origin: [f32; 2],
    pub(crate) size: [f32; 2],
    /// 内容区，元素坐标
    pub(crate) content: Rect,
    pub(crate) focused: bool,
    /// 要重做什么
    pub(crate) dirty: Dirty,
    /// 要交出焦点
    pub(crate) release_focus: bool,
    /// 输入框改了什么（内置的输入框填：写回信号、调用改值和提交的处理器）
    pub(crate) edit: EditReport,
}

impl<'a> TargetContext<'a> {
    pub(crate) fn new(text: &'a mut TextSystem, seconds: f64, origin: [f32; 2], size: [f32; 2], content: Rect, focused: bool) -> Self {
        Self { text, seconds, origin, size, content, focused, dirty: Dirty::empty(), release_focus: false, edit: EditReport::default() }
    }

    /// 元素的大小（含内边距），逻辑像素
    pub fn size(&self) -> [f32; 2] {
        self.size
    }

    /// 内容区（去掉内边距和边框）：在元素自己的坐标里
    pub fn content_rect(&self) -> Rect {
        self.content
    }

    /// 元素树此刻的时间（和 [`crate::kinds::TickContext::now`] 同一个时钟；测试工具里是虚拟时间）
    pub fn now(&self) -> Duration {
        Duration::from_secs_f64(self.seconds.max(0.0))
    }

    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub fn scale_factor(&self) -> f32 {
        self.text.scale()
    }

    /// 元素有键盘焦点
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// 内容变了：重画这个元素
    pub fn request_paint(&mut self) {
        self.dirty |= Dirty::PAINT;
    }

    /// 内容的大小变了：重新量这个元素（[`crate::ElementKind::measure`]）
    pub fn request_layout(&mut self) {
        self.dirty |= Dirty::LAYOUT;
    }

    /// 给辅助技术的描述变了（值、选区）：重建这个元素的无障碍节点
    pub fn request_accessibility(&mut self) {
        self.dirty |= Dirty::ACCESSIBILITY;
    }

    /// 交出焦点（输入框里按 Esc）
    pub fn unfocus(&mut self) {
        self.release_focus = true;
    }
}
