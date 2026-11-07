//! 元素上的交互：叠放次序、指针形状、鼠标按键和手势（右键、双击、拖动、捏合、旋转）、
//! 键盘焦点（Tab 切换、焦点环、焦点句柄）、按键处理、快捷键。窗口交给元素树的原始输入在 `input.rs`。
//!
//! 处理器都在创建元素时的作用域里、批量更新中执行。
//! 元素树里怎么分发见 `tree/interaction.rs`。

use std::cell::RefCell;
use std::rc::Rc;

use reactive::{Memo, Scope, Signal, on_cleanup, signal};
use scene::Color;

use crate::element::Element;
use crate::event::{ClickEvent, DragEvent, IntoHandler, KeyEvent, PinchEvent, PointerEvent, RotateEvent, ScrollEvent, WheelEvent};
use crate::handlers::{EventType, Handler, owned};
use crate::input::{Key, Modifiers, NamedKey};
use crate::overlay::OverlaySpec;
use crate::property::registry::identity;
use crate::property::{IntoProp, Prop};
use crate::geometry::Rect;

use crate::host::{WindowId, current_window_id};

/// 指针的形状。元素上用 [`Element::cursor`] 设置，指针下最内层设置了形状的元素决定；
/// 输入框默认是文字形状的指针，拖动时由被拖的元素决定
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cursor {
    /// 系统默认的箭头
    #[default]
    Default,
    /// 可以点的东西（手指）
    Pointer,
    /// 文字（竖线）
    Text,
    /// 竖排文字
    VerticalText,
    /// 可以抓起来拖（张开的手）
    Grab,
    /// 正在拖（握拳的手）
    Grabbing,
    /// 可以移动（四向箭头）
    Move,
    /// 十字准星，精确选取
    Crosshair,
    /// 不允许的操作
    NotAllowed,
    /// 左右拖动改宽度
    ResizeHorizontal,
    /// 上下拖动改高度
    ResizeVertical,
    /// 左上 ↔ 右下
    ResizeDiagonal,
    /// 右上 ↔ 左下
    ResizeAntiDiagonal,
    /// 拖动改列宽
    ColumnResize,
    /// 拖动改行高
    RowResize,
    /// 放大
    ZoomIn,
    /// 缩小
    ZoomOut,
    /// 忙，不能操作
    Wait,
    /// 后台忙，仍可操作
    Progress,
    /// 帮助
    Help,
    /// 有右键菜单
    ContextMenu,
    /// 拖放时会复制
    Copy,
    /// 拖放时会建立链接或快捷方式
    Alias,
    /// 选取表格单元格
    Cell,
    /// 可以向任意方向滚动
    AllScroll,
}

impl IntoProp<Cursor> for Cursor {
    fn into_prop(self) -> Prop<Cursor> {
        Prop::Static(self)
    }
}

/// 叠放次序的值：整数、返回整数的闭包、`Signal<i32>`、`Memo<i32>`。
/// 单独一个 trait 而不是 `IntoProp<i32>`：整数字面量已经是 `IntoProp<f32>`（长度），再加一个会让 `.width(200)` 有歧义
pub trait IntoStackOrder {
    /// 转成属性：静态值或响应式闭包
    fn into_stack_order(self) -> Prop<i32>;
}

impl IntoStackOrder for i32 {
    fn into_stack_order(self) -> Prop<i32> {
        Prop::Static(self)
    }
}

impl<F: Fn() -> i32 + 'static> IntoStackOrder for F {
    fn into_stack_order(self) -> Prop<i32> {
        Prop::Dynamic(Box::new(self))
    }
}

impl IntoStackOrder for Signal<i32> {
    fn into_stack_order(self) -> Prop<i32> {
        Prop::Dynamic(Box::new(move || self.get()))
    }
}

impl IntoStackOrder for Memo<i32> {
    fn into_stack_order(self) -> Prop<i32> {
        Prop::Dynamic(Box::new(move || self.get()))
    }
}

/// 焦点环：元素用键盘（Tab）得到焦点时画在它外面的一圈，用指针点的不画。
/// 默认是 1.5 像素的强调色细线，离元素 2 像素，圆角是元素的圆角加上间距，所以紧贴形状；不发光
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FocusRing {
    /// 颜色
    pub color: Color,
    /// 线宽，逻辑像素
    pub width: f32,
    /// 环和元素边缘之间的间距
    pub offset: f32,
    /// 环自己的圆角；`None` 时跟着元素
    pub radius: Option<f32>,
}

impl FocusRing {
    /// 不画焦点环（元素自己用 `on_focus` 表现焦点时）
    pub const NONE: Self = Self { color: Color::TRANSPARENT, width: 0.0, offset: 0.0, radius: None };

    /// 指定颜色，线宽 1.5、间距 2 逻辑像素，圆角跟着元素
    pub const fn new(color: Color) -> Self {
        Self { color, width: 1.5, offset: 2.0, radius: None }
    }

    /// 线宽，逻辑像素
    pub const fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// 环和元素边缘之间的间距，逻辑像素
    pub const fn offset(mut self, offset: f32) -> Self {
        self.offset = offset;
        self
    }

    /// 固定环的圆角，不再跟着元素
    pub const fn radius(mut self, radius: f32) -> Self {
        self.radius = Some(radius);
        self
    }

    pub(crate) fn visible(&self) -> bool {
        self.width > 0.0 && !self.color.is_transparent()
    }
}

/// 默认的焦点环：强调色（浅色、深色两套调色板一样），宽 1.5，离元素 2
impl Default for FocusRing {
    fn default() -> Self {
        Self::new(crate::defaults::Palette::LIGHT.accent)
    }
}

/// 焦点句柄：在别处（快捷键、按钮、`on_mount`）让元素获得或失去焦点，读它现在有没有焦点（响应式）。
/// 和信号一样是 `Copy` 的；先创建，再用 [`Element::focus_handle`] 交给元素。挂载前发出的请求挂载后执行。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let search = FocusHandle::new();
/// let query = signal(String::new());
/// text_field(query).focus_handle(search).width(240);
/// aureole::shortcut("CmdOrCtrl+K", move || search.focus());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusHandle {
    pub(crate) request: Signal<FocusRequest>,
    pub(crate) focused: Signal<bool>,
    /// 已经执行过的请求编号：元素重新挂载（比如每次打开的对话框里的输入框）时不再执行旧请求
    pub(crate) handled: Signal<u64>,
}

/// 句柄发出的请求：编号递增，只执行比 `handled` 新的请求（挂载前发出的在挂载后执行）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FocusRequest {
    pub serial: u64,
    pub focus: bool,
    /// 标签点了：获得焦点，再在元素上合成一次标签来的点击
    pub activate: bool,
}

impl FocusHandle {
    /// 新句柄，归当前作用域所有
    pub fn new() -> Self {
        Self { request: signal(FocusRequest::default()), focused: signal(false), handled: signal(0) }
    }

    /// 让元素获得焦点（滚进视口里）。最近一次输入是键盘时画焦点环
    pub fn focus(&self) {
        self.request.update(|r| *r = FocusRequest { serial: r.serial + 1, focus: true, activate: false });
    }

    /// 让元素获得焦点，再在它上面合成一次点击（[`crate::ClickEvent::source`] 是 [`crate::ClickSource::Label`]，
    /// 只交给它自己、不冒泡）：复选框、开关切换，下拉框打开，按钮被点；没有点击处理器的元素（输入框、滑块）只获得焦点。
    /// 元素禁用时什么都不做。点标签（`controls::Label::target`）时就是这样
    pub fn activate(&self) {
        self.request.update(|r| *r = FocusRequest { serial: r.serial + 1, focus: true, activate: true });
    }

    /// 元素有焦点时让它失去焦点
    pub fn unfocus(&self) {
        self.request.update(|r| *r = FocusRequest { serial: r.serial + 1, focus: false, activate: false });
    }

    /// 现在有没有焦点，响应式
    pub fn is_focused(&self) -> bool {
        self.focused.get()
    }
}

impl Default for FocusHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// 元素上和输入有关的设置，用到时才分配（`Element` 在链式调用里一直被移动，不宜变大）
#[derive(Default)]
pub(crate) struct Interaction {
    pub focus_handle: Option<FocusHandle>,
    pub overlays: Vec<OverlaySpec>,
    /// 布局之后把位置和大小写进这些信号（见 `measure.rs`）
    pub measure: Vec<Signal<Rect>>,
    /// 拖放：拖动源、放置目标、拖到应用外面（见 `drag_drop.rs`）
    pub drag_drop: Option<Box<crate::drag_drop::DragDropSpec>>,
    /// 样式对象、控件记的状态（悬停、焦点、按下、禁用，见 `style.rs`）：挂载时把禁用写进去
    pub states: Option<crate::style::OwnStates>,
    /// 样式对象建的有状态的属性：后面再用样式对象时按状态合起来（只在构建时用）
    pub memo: Option<Box<crate::style::StyleMemo>>,
    /// 报告用户改值的句柄（见 `field.rs`）：挂载时记下节点
    pub field_handle: Option<crate::field::FieldHandle>,
}

impl Element {
    pub(crate) fn interaction(&mut self) -> &mut Interaction {
        self.input.get_or_insert_with(Box::default)
    }

    // ---- 叠放次序 ----

    /// 兄弟之间的叠放次序（默认 0）：大的画在上面，指针也先点到它。相同的保持声明顺序。
    /// 只在兄弟之间比较；要盖住整个窗口用 [`Element::overlay`]
    pub fn stack_order(mut self, z: impl IntoStackOrder) -> Self {
        self.put(crate::property::STACK_ORDER.id(), z.into_stack_order().into_slot(identity));
        self
    }

    // ---- 指针 ----

    /// 右键点击（macOS 上按住 Control 点左键也算）：按下时调用，用来弹出右键菜单
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let menu_at = signal(None::<[f32; 2]>);
    /// rect().size(200, 120).on_secondary_click(move |e: &ClickEvent| menu_at.set(Some(e.window_position)));
    /// ```
    pub fn on_secondary_click<M>(self, handler: impl IntoHandler<ClickEvent, M>) -> Self {
        self.listen(EventType::SecondaryClick, Handler::Click(handler.into_handler()))
    }

    /// 双击：第二次按下时调用（两次按下间隔 0.4 秒、距离 4 像素以内）。两次点击各自照常触发 `on_click`
    pub fn on_double_click<M>(self, handler: impl IntoHandler<ClickEvent, M>) -> Self {
        self.listen(EventType::DoubleClick, Handler::Click(handler.into_handler()))
    }

    /// 中键点击（按下和松开在同一个元素上）
    pub fn on_middle_click<M>(self, handler: impl IntoHandler<ClickEvent, M>) -> Self {
        self.listen(EventType::MiddleClick, Handler::Click(handler.into_handler()))
    }

    /// 指针按下：交给指针下最内层的元素，往外冒泡（禁用的元素跳过）。处理器之后是没被取消的内置行为：
    /// 关外面的浮层（`Builtin::DISMISS`）、拿焦点（`FOCUS`）、选字（`TEXT_SELECT`）、准备拖动（`DRAG`）、
    /// 右键点击（`CONTEXT_MENU`）；取消 `ACTIVATE` 时松开不再合成点击。[`PointerEvent::capture_pointer`]
    /// 让之后的移动、松开都交给这个元素
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// // 画板：按下就捕获指针，拖出画板也照样收到移动；不让外层拿焦点
    /// rect().size(300, 200).on_pointer_down(|e: &PointerEvent| {
    ///     e.capture_pointer();
    ///     e.prevent_builtin(Builtin::FOCUS);
    /// });
    /// ```
    pub fn on_pointer_down<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerDown, Handler::Pointer(handler.into_handler()))
    }

    /// 指针按下的捕获阶段：从外往里，在里面的元素之前收到，见 [`Element::on_pointer_down`]
    pub fn on_pointer_down_capture<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen_capture(EventType::PointerDown, Handler::Pointer(handler.into_handler()))
    }

    /// 指针松开：交给指针下最内层的元素（指针被捕获时是捕获它的元素），往外冒泡
    pub fn on_pointer_up<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerUp, Handler::Pointer(handler.into_handler()))
    }

    /// 指针松开的捕获阶段，见 [`Element::on_pointer_up`]
    pub fn on_pointer_up_capture<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen_capture(EventType::PointerUp, Handler::Pointer(handler.into_handler()))
    }

    /// 指针在元素上移动：交给指针下最内层的元素（指针被捕获时是捕获它的元素），往外冒泡；拖动时不调用
    pub fn on_pointer_move<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerMove, Handler::Pointer(handler.into_handler()))
    }

    /// 指针移动的捕获阶段，见 [`Element::on_pointer_move`]
    pub fn on_pointer_move_capture<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen_capture(EventType::PointerMove, Handler::Pointer(handler.into_handler()))
    }

    /// 进行中的按下被打断（窗口失去焦点）：交给捕获了指针的元素或者按下处最内层的元素，往外冒泡
    pub fn on_pointer_cancel<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerCancel, Handler::Pointer(handler.into_handler()))
    }

    /// 滚轮、触控板滚动：交给指针下最内层的元素，往外冒泡，之后滚动视口才按它滚（锁定、惯性、橡皮筋照旧）。
    /// 事件里有滚动量（行或像素）、手势阶段和是不是惯性、修饰键、位置；取消 `Builtin::SCROLL` 视口就不滚
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// // 按住 Command（其他平台是 Ctrl）滚动是缩放，不滚动外面的视口
    /// let zoom = signal(1.0f32);
    /// canvas(|_| {}).size(400, 300).on_wheel(move |e: &WheelEvent| {
    ///     if e.modifiers.has_super() || e.modifiers.has_control() {
    ///         let dy = match e.delta { aureole::WheelDelta::Lines(d) => d[1] * 40.0, aureole::WheelDelta::Pixels(d) => d[1] };
    ///         zoom.update(|z| *z = (*z * (1.0 - dy / 400.0)).clamp(0.25, 4.0));
    ///         e.prevent_builtin(Builtin::SCROLL);
    ///     }
    /// });
    /// ```
    pub fn on_wheel<M>(self, handler: impl IntoHandler<WheelEvent, M>) -> Self {
        self.listen(EventType::Wheel, Handler::Wheel(handler.into_handler()))
    }

    /// 滚轮的捕获阶段，见 [`Element::on_wheel`]
    pub fn on_wheel_capture<M>(self, handler: impl IntoHandler<WheelEvent, M>) -> Self {
        self.listen_capture(EventType::Wheel, Handler::Wheel(handler.into_handler()))
    }

    /// 滚动视口（[`crate::scroll_y`] 这些返回的元素）的偏移变了：一帧最多一次，合并了这一帧里的所有变化
    /// （滚轮、拖滚动条、惯性、程序化滚动、渲染线程推进的都算）。只发给视口自己
    pub fn on_scroll<M>(self, handler: impl IntoHandler<ScrollEvent, M>) -> Self {
        self.listen(EventType::Scroll, Handler::Scroll(handler.into_handler()))
    }

    /// 指针进入元素（子元素上方也算在元素里）：每个进入的元素各收到一次，不传播。只要知道进没进用 [`Element::on_hover`]
    pub fn on_pointer_enter<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerEnter, Handler::Pointer(handler.into_handler()))
    }

    /// 指针离开元素：每个离开的元素各收到一次，不传播
    pub fn on_pointer_leave<M>(self, handler: impl IntoHandler<PointerEvent, M>) -> Self {
        self.listen(EventType::PointerLeave, Handler::Pointer(handler.into_handler()))
    }

    /// 拖动：按下后移动超过 4 像素开始（`Start`），之后每次移动（`Move`），松开时结束（`End`）。
    /// 开始后指针被拿到拖动的元素捕获：移出元素、移出窗口都继续收到事件，期间悬停状态不变、不触发点击。
    /// 按在滚动条上、输入框里时不拖。
    ///
    /// 谁拿到拖动：`Start` 沿按下处的路径走捕获（[`Element::on_drag_capture`]）→ 目标 → 冒泡，默认是按下处最内层有
    /// 这个处理器的元素（能拖放、能拖到应用外面的元素也算）；处理器可以 [`DragEvent::decline_gesture`] 让给外面的元素，
    /// 外面的元素可以在捕获阶段 [`DragEvent::claim_gesture`] 抢走。之后的 `Move`、`End` 只交给拿到的元素；
    /// 收到过 `Start` 没拿到的元素收到 `Cancel`，拿到的元素里面的元素收到 `on_pointer_cancel`、`on_press(false)`
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (x, y) = (signal(0.0f32), signal(0.0f32));
    /// rect().size(120, 80).translation(x, y).on_drag(move |e: &DragEvent| {
    ///     x.update(|x| *x += e.delta[0]);
    ///     y.update(|y| *y += e.delta[1]);
    /// });
    /// // 只要横着拖的卡片：竖着拖时让给外面（外面的视口拿去滚动）
    /// let offset = signal(0.0f32);
    /// rect().size(300, 60).translation_x(offset).on_drag(move |e: &DragEvent| {
    ///     if e.phase == GesturePhase::Start && e.translation[1].abs() > e.translation[0].abs() {
    ///         e.decline_gesture();
    ///         return;
    ///     }
    ///     offset.update(|x| *x += e.delta[0]);
    /// });
    /// ```
    pub fn on_drag<M>(self, handler: impl IntoHandler<DragEvent, M>) -> Self {
        self.listen(EventType::Drag, Handler::Drag(handler.into_handler()))
    }

    /// 拖动的捕获阶段：拖动开始时在里面的元素之前看到 `Start`，可以 [`DragEvent::claim_gesture`] 抢走
    /// （之后的 `Move`、`End` 也交给这个处理器）；不抢就收到 `Cancel`。见 [`Element::on_drag`]
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// // 能平移的画布：按住 Option（其他平台是 Alt）拖的时候，里面的元素能拖也让画布平移
    /// let (pan_x, pan_y) = (signal(0.0f32), signal(0.0f32));
    /// rect().size(600, 400).clip(true).on_drag_capture(move |e: &DragEvent| match e.phase {
    ///     GesturePhase::Start if e.modifiers.has_alt() => e.claim_gesture(),
    ///     GesturePhase::Move => {
    ///         pan_x.update(|x| *x += e.delta[0]);
    ///         pan_y.update(|y| *y += e.delta[1]);
    ///     }
    ///     _ => {}
    /// });
    /// ```
    pub fn on_drag_capture<M>(self, handler: impl IntoHandler<DragEvent, M>) -> Self {
        self.listen_capture(EventType::Drag, Handler::Drag(handler.into_handler()))
    }

    /// 捏合：触控板双指，触摸屏上两根手指（距离变了 8 逻辑像素开始）。谁拿到同 [`Element::on_drag`]：`Start` 沿指针处的路径传播，默认是最内层有这个处理器的元素，
    /// 一次手势锁定同一个元素
    pub fn on_pinch<M>(self, handler: impl IntoHandler<PinchEvent, M>) -> Self {
        self.listen(EventType::Pinch, Handler::Pinch(handler.into_handler()))
    }

    /// 捏合的捕获阶段，见 [`Element::on_drag_capture`]
    pub fn on_pinch_capture<M>(self, handler: impl IntoHandler<PinchEvent, M>) -> Self {
        self.listen_capture(EventType::Pinch, Handler::Pinch(handler.into_handler()))
    }

    /// 旋转：触控板双指，触摸屏上两根手指（转过的弧长到 8 逻辑像素开始）。规则同 [`Element::on_pinch`]
    pub fn on_rotate<M>(self, handler: impl IntoHandler<RotateEvent, M>) -> Self {
        self.listen(EventType::Rotate, Handler::Rotate(handler.into_handler()))
    }

    /// 旋转的捕获阶段，见 [`Element::on_drag_capture`]
    pub fn on_rotate_capture<M>(self, handler: impl IntoHandler<RotateEvent, M>) -> Self {
        self.listen_capture(EventType::Rotate, Handler::Rotate(handler.into_handler()))
    }

    // ---- 键盘 ----

    /// 用键盘得到焦点（Tab、方向键，键盘操作之后用程序给的焦点，也就是画焦点环的时候）时 `true`，
    /// 失去焦点或者改用指针（焦点环消失）时 `false`。用指针点得到的焦点不算，见 [`Element::on_focus`]
    pub fn on_keyboard_focus<R>(mut self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.events.handlers.push(EventType::KeyboardFocus, Handler::Flag(owned(handler)));
        self
    }

    /// 用句柄在别处控制这个元素的焦点，见 [`FocusHandle`]。同时让元素能获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.interaction().focus_handle = Some(handle);
        self.put(crate::property::FOCUSABLE.id(), true.into_slot(identity));
        self
    }

    /// 按键按下：目标是有焦点的元素（没有焦点时是最上面的模态浮层的内容，没有模态浮层时是根元素），
    /// 先捕获（[`Element::on_key_down_capture`]，从外往里），到目标（先是用户在它上面的处理器，再是输入框的编辑），
    /// 再从它往外冒泡；处理了就调用 [`KeyEvent::stop_propagation`]，不再往外传。浮层里的按键冒泡到声明浮层的元素
    /// （模态浮层到此为止）；Esc 由能关的浮层在捕获阶段处理。都没停止时是 [`shortcut`]，
    /// 最后是内置的行为（Tab 切换焦点、回车 / 空格激活、方向键和翻页键滚动），可以用 `prevent_builtin` 取消
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # let selected = signal(0usize);
    /// column().focusable(true).on_key_down(move |e: &KeyEvent| {
    ///     if e.is("ArrowDown") {
    ///         selected.update(|i| *i += 1);
    ///         e.stop_propagation();
    ///     }
    /// });
    /// ```
    pub fn on_key_down(self, handler: impl Fn(&KeyEvent) + 'static) -> Self {
        self.listen(EventType::KeyDown, Handler::Key(key_handler(handler)))
    }

    /// 按键松开：和 [`Element::on_key_down`] 一样从焦点往外冒泡，停止传播就不再往外传，
    /// 没有元素停止传播时交给窗口的 [`key_up_listener`]。
    ///
    /// 松开总是交给按下时的那条链：按住期间焦点换了（Tab、指针点了别处、对话框打开）、窗口失去焦点、
    /// 输入法开始组字时，按下时收到过的元素照样收到松开，不会"卡住"。按下交给了输入法（组字）的键，
    /// 松开也不交给这里。事件里的按键和按下时一样（按着 Shift 按下的 `A`，先松开 Shift 再松开时仍是 `A`），
    /// 修饰键是松开时的
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let held = signal(false);
    /// rect()
    ///     .focusable(true)
    ///     .on_key_down(move |e: &KeyEvent| {
    ///         if e.is("Space") {
    ///             held.set(true);
    ///             e.stop_propagation();
    ///         }
    ///     })
    ///     .on_key_up(move |e: &KeyEvent| {
    ///         if e.is("Space") {
    ///             held.set(false);
    ///             e.stop_propagation();
    ///         }
    ///     });
    /// ```
    pub fn on_key_up(self, handler: impl Fn(&KeyEvent) + 'static) -> Self {
        self.listen(EventType::KeyUp, Handler::Key(key_handler(handler)))
    }

    /// 按键按下的捕获阶段：从外往里，在焦点（包括有焦点的输入框）之前收到。祖先在这里截下按键
    /// （比如自动补全截下 ↑ ↓、回车），停止传播后里面的元素和输入框都收不到，见 [`Element::on_key_down`]
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (query, active) = (signal(String::new()), signal(0usize));
    /// column()
    ///     .on_key_down_capture(move |e: &KeyEvent| {
    ///         if e.is("ArrowDown") {
    ///             active.update(|i| *i += 1);
    ///             e.stop_propagation(); // 输入框不再把它当成移动插入点
    ///         }
    ///     })
    ///     .child(text_field(query));
    /// ```
    pub fn on_key_down_capture(self, handler: impl Fn(&KeyEvent) + 'static) -> Self {
        self.listen_capture(EventType::KeyDown, Handler::Key(key_handler(handler)))
    }

    /// 按键松开的捕获阶段，见 [`Element::on_key_up`]
    pub fn on_key_up_capture(self, handler: impl Fn(&KeyEvent) + 'static) -> Self {
        self.listen_capture(EventType::KeyUp, Handler::Key(key_handler(handler)))
    }
}

// ---- 按键松开、按着的键 ----

/// 窗口的按键松开监听，见 [`key_up_listener`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyUpListener(u64);

impl KeyUpListener {
    /// 取消；已经取消的不做任何事
    pub fn cancel(self) {
        let _ = KEY_UP_LISTENERS.try_with(|l| l.borrow_mut().entries.retain(|e| e.0 != self.0));
    }
}

/// 窗口的按键松开：元素的 [`Element::on_key_up`] 都没停止传播时调用（和快捷键排在 `on_key_down` 之后一样）。
/// 在窗口里注册的只管这个窗口，在窗口之外注册的管所有窗口；归创建时的作用域所有，作用域释放时自动取消。
/// 处理器的返回值不用
pub fn key_up_listener<R>(handler: impl Fn(&KeyEvent) -> R + 'static) -> KeyUpListener {
    let callback: Rc<dyn Fn(&KeyEvent)> = match Scope::current() {
        Some(scope) => Rc::new(move |e: &KeyEvent| {
            scope.run(|| {
                handler(e);
            })
        }),
        None => Rc::new(move |e: &KeyEvent| {
            handler(e);
        }),
    };
    let window = current_window_id();
    let listener = KEY_UP_LISTENERS.with(|l| {
        let mut l = l.borrow_mut();
        l.next += 1;
        let id = l.next;
        l.entries.push((id, window, callback));
        KeyUpListener(id)
    });
    if Scope::current().is_some() {
        on_cleanup(move || listener.cancel());
    }
    listener
}

/// 窗口的松开监听（这个窗口的和全应用的）
pub(crate) fn key_up_listeners() -> Vec<OnKeyUp> {
    let window = current_window_id();
    KEY_UP_LISTENERS.with(|l| {
        l.borrow().entries.iter().filter(|(_, w, _)| w.is_none() || *w == window).map(|(.., f)| f.clone()).collect()
    })
}

type OnKeyUp = Rc<dyn Fn(&KeyEvent)>;
type KeyUpEntry = (u64, Option<WindowId>, OnKeyUp);

#[derive(Default)]
struct KeyUpListeners {
    next: u64,
    entries: Vec<KeyUpEntry>,
}

thread_local! {
    static KEY_UP_LISTENERS: RefCell<KeyUpListeners> = RefCell::default();
    /// 按着的键：(窗口, 不算修饰键时的按键)，[`is_key_down`] 读它
    static HELD: RefCell<Vec<(Option<WindowId>, Key)>> = const { RefCell::new(Vec::new()) };
}

/// 按着的键变了（元素树按下、松开时调用）
pub(crate) fn set_held(key: &Key, down: bool) {
    let window = current_window_id();
    HELD.with(|h| {
        let mut h = h.borrow_mut();
        h.retain(|(w, k)| !(*w == window && k == key));
        if down {
            h.push((window, key.clone()));
        }
    });
}

/// 这个键此刻是不是按着：`"Space"`、`"a"`、`"ArrowLeft"`、`"Shift"`、`"Cmd"`（写法同 [`shortcut`] 里的按键，
/// 修饰键也可以单独问）。在窗口里（处理器、绑定）问的是这个窗口，在窗口之外问的是所有窗口。
/// 不是响应式的（不订阅），按键时读一下很便宜：拖动时按住空格平移、按住 Option 复制这类
pub fn is_key_down(name: &str) -> bool {
    let modifier = match name.to_ascii_lowercase().as_str() {
        "shift" => Some(NamedKey::Shift),
        "ctrl" | "control" => Some(NamedKey::Control),
        "alt" | "option" => Some(NamedKey::Alt),
        "cmd" | "command" | "super" | "meta" => Some(NamedKey::Super),
        _ => None,
    };
    let chord = match modifier {
        Some(_) => None,
        None => match Chord::parse(name) {
            Some(chord) => Some(chord),
            None => return false,
        },
    };
    let window = current_window_id();
    HELD.with(|h| {
        h.borrow().iter().any(|(w, key)| {
            (window.is_none() || *w == window)
                && match (&chord, modifier) {
                    (Some(chord), _) => chord.same_key(key),
                    // macOS 上 Command 是 Super，平台也可能报成 Meta
                    (None, Some(NamedKey::Super)) => matches!(key, Key::Named(NamedKey::Super | NamedKey::Meta)),
                    (None, Some(m)) => *key == Key::Named(m),
                    (None, None) => false,
                }
        })
    })
}

// ---- 快捷键 ----

/// 一个组合键：修饰键 + 按键
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    modifiers: Modifiers,
    key: ChordKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChordKey {
    /// 小写的字符
    Char(String),
    Named(NamedKey),
}

impl Chord {
    /// `"CmdOrCtrl+K"`、`"Shift+Alt+ArrowUp"`、`"Escape"`、`"F5"`。修饰键：`Cmd`（`Command`、`Super`、`Meta`）、
    /// `Ctrl`（`Control`）、`CmdOrCtrl`（macOS 上是 Cmd，其他平台是 Ctrl）、`Alt`（`Option`）、`Shift`。
    /// 加号本身写 `Plus`，空格写 `Space`
    pub fn parse(spec: &str) -> Option<Self> {
        let mut modifiers = Modifiers::empty();
        let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
        let (last, mods) = parts.split_last()?;
        for m in mods {
            modifiers |= match m.to_ascii_lowercase().as_str() {
                "cmd" | "command" | "super" | "meta" => Modifiers::SUPER,
                "ctrl" | "control" => Modifiers::CONTROL,
                "cmdorctrl" | "commandorcontrol" | "primary" => primary(),
                "alt" | "option" => Modifiers::ALT,
                "shift" => Modifiers::SHIFT,
                _ => return None,
            };
        }
        let lower = last.to_ascii_lowercase();
        let named = match lower.as_str() {
            "enter" | "return" => Some(NamedKey::Enter),
            "escape" | "esc" => Some(NamedKey::Escape),
            "tab" => Some(NamedKey::Tab),
            "space" => Some(NamedKey::Space),
            "backspace" => Some(NamedKey::Backspace),
            "delete" | "del" => Some(NamedKey::Delete),
            "up" | "arrowup" => Some(NamedKey::ArrowUp),
            "down" | "arrowdown" => Some(NamedKey::ArrowDown),
            "left" | "arrowleft" => Some(NamedKey::ArrowLeft),
            "right" | "arrowright" => Some(NamedKey::ArrowRight),
            "home" => Some(NamedKey::Home),
            "end" => Some(NamedKey::End),
            "pageup" => Some(NamedKey::PageUp),
            "pagedown" => Some(NamedKey::PageDown),
            "insert" => Some(NamedKey::Insert),
            _ => function_key(&lower),
        };
        let key = match named {
            Some(named) => ChordKey::Named(named),
            None if lower == "plus" => ChordKey::Char("+".into()),
            None if last.chars().count() == 1 => ChordKey::Char(lower),
            None => return None,
        };
        Some(Self { modifiers, key })
    }

    /// 修饰键要完全一致；字符不分大小写，也和不算修饰键的按键比较（Shift+/ 按出来的是 `?`）
    pub fn matches(&self, key: &Key, base: &Key, modifiers: Modifiers) -> bool {
        let mask = Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER;
        if modifiers & mask != self.modifiers {
            return false;
        }
        self.same_key(key) || self.same_key(base)
    }

    /// 只比较按键，不管修饰键
    pub fn same_key(&self, key: &Key) -> bool {
        match (&self.key, key) {
            (ChordKey::Named(a), Key::Named(b)) => a == b,
            (ChordKey::Named(NamedKey::Space), Key::Character(c)) => c.as_str() == " ",
            (ChordKey::Char(a), Key::Character(c)) => c.to_lowercase() == *a,
            _ => false,
        }
    }

    /// 测试工具：按下这个组合键时窗口会给的按键和修饰键
    pub fn key(&self) -> (Key, Modifiers) {
        let key = match &self.key {
            ChordKey::Named(named) => Key::Named(*named),
            ChordKey::Char(c) => Key::Character(c.as_str().into()),
        };
        (key, self.modifiers)
    }
}

fn function_key(name: &str) -> Option<NamedKey> {
    const KEYS: [NamedKey; 12] = [
        NamedKey::F1,
        NamedKey::F2,
        NamedKey::F3,
        NamedKey::F4,
        NamedKey::F5,
        NamedKey::F6,
        NamedKey::F7,
        NamedKey::F8,
        NamedKey::F9,
        NamedKey::F10,
        NamedKey::F11,
        NamedKey::F12,
    ];
    let n: usize = name.strip_prefix('f')?.parse().ok()?;
    KEYS.get(n.checked_sub(1)?).copied()
}

/// 平台的主修饰键（平台惯例里的）：macOS 上是 Command，其他平台是 Control
pub(crate) fn primary() -> Modifiers {
    crate::system::conventions().primary_modifier
}

/// 快捷键的句柄
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut(u64);

impl Shortcut {
    /// 取消；已经取消的不做任何事
    pub fn cancel(self) {
        let _ = SHORTCUTS.try_with(|s| s.borrow_mut().entries.retain(|e| e.id != self.0));
    }

    /// 抢先：在所有元素之前处理（全局搜索、命令面板这类不管焦点在哪都要生效的），有焦点的输入框、
    /// 元素的 `on_key_down` 都收不到这个组合键。默认的快捷键排在元素之后（输入框里的 `CmdOrCtrl+A` 仍然是全选）
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// aureole::shortcut("CmdOrCtrl+K", || println!("命令面板")).preemptive(true);
    /// ```
    pub fn preemptive(self, preemptive: bool) -> Self {
        let _ = SHORTCUTS.try_with(|s| {
            if let Some(entry) = s.borrow_mut().entries.iter_mut().find(|e| e.id == self.0) {
                entry.preemptive = preemptive;
            }
        });
        self
    }
}

/// 应用里的快捷键：`aureole::shortcut("CmdOrCtrl+K", || ...)`，写法见 [`KeyEvent::is`]。
///
/// - 在窗口里（构建视图、处理器里）注册的只在这个窗口有焦点时生效，在窗口之外注册的对所有窗口生效。
/// - 归创建时的作用域所有：作用域释放（元素卸载、窗口关闭）时自动取消，也可以 [`Shortcut::cancel`]。
/// - 原生菜单的快捷键先于窗口收到按键，优先于这里；有焦点的输入框、元素的 `on_key_down` 也先于这里
///   （输入框里的 `CmdOrCtrl+A` 仍然是全选）。同一个组合键注册了多个时，最后注册的生效。
///
/// 写错的组合键记一条警告（`tracing`），不会生效。处理器的返回值不用
pub fn shortcut<R>(chord: &str, f: impl Fn() -> R + 'static) -> Shortcut {
    let parsed = Chord::parse(chord);
    if parsed.is_none() {
        tracing::warn!(chord, "快捷键写法不对，忽略");
    }
    let callback: Rc<dyn Fn()> = match Scope::current() {
        Some(scope) => Rc::new(move || {
            scope.run(|| {
                f();
            })
        }),
        None => Rc::new(move || {
            f();
        }),
    };
    let window = current_window_id();
    let shortcut = SHORTCUTS.with(|s| {
        let mut s = s.borrow_mut();
        s.next += 1;
        let id = s.next;
        if let Some(chord) = parsed {
            s.entries.push(Entry { id, chord, window, callback, preemptive: false });
        }
        Shortcut(id)
    });
    if Scope::current().is_some() {
        on_cleanup(move || shortcut.cancel());
    }
    shortcut
}

struct Entry {
    id: u64,
    chord: Chord,
    window: Option<WindowId>,
    callback: Rc<dyn Fn()>,
    /// 抢在元素之前（[`Shortcut::preemptive`]）
    preemptive: bool,
}

#[derive(Default)]
struct Shortcuts {
    next: u64,
    entries: Vec<Entry>,
}

thread_local! {
    static SHORTCUTS: RefCell<Shortcuts> = RefCell::default();
}

/// 找这个按键对应的快捷键（当前窗口的或全应用的，最后注册的优先）：`preemptive` 时只找抢先的，否则只找默认的
pub(crate) fn find_shortcut(event: &KeyEvent, preemptive: bool) -> Option<Rc<dyn Fn()>> {
    let window = current_window_id();
    SHORTCUTS.with(|s| {
        s.borrow()
            .entries
            .iter()
            .rev()
            .find(|e| {
                e.preemptive == preemptive && (e.window.is_none() || e.window == window) && e.chord.matches(&event.key, &event.base, event.modifiers)
            })
            .map(|e| e.callback.clone())
    })
}

/// 按键处理器只收 `Fn(&KeyEvent)`，返回值必须是 `()`：以前"返回 `true` 表示处理了"，现在要调
/// [`KeyEvent::stop_propagation`]。返回 `bool` 的旧写法在这里编译不过，不会被悄悄丢掉。
/// 只有一种写法，闭包的参数类型也推断得出来（`|e| ..` 不用写类型）
fn key_handler(handler: impl Fn(&KeyEvent) + 'static) -> std::rc::Rc<dyn Fn(&KeyEvent)> {
    IntoHandler::<KeyEvent, fn(&KeyEvent)>::into_handler(handler)
}
