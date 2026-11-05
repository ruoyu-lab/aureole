//! 处理器表：元素上登记的所有处理器按事件种类存在一张表里。
//!
//! 构建元素时链式方法把处理器放进 [`HandlerList`]（`element/events.rs`、`interaction.rs`、`drag_drop.rs`……），
//! 挂载时整张表交给节点（节点的冷存储）。表头是一个位掩码 `listens`：每种事件一位，捕获阶段的处理器另有一位。
//! 分发时每经过一个元素只读这个字，没有对应处理器的元素不碰后面的列表。
//!
//! 处理器在创建元素时的作用域里执行（[`owned`]）：处理器里创建的信号、副作用归元素所在的作用域，
//! 元素卸载时一起释放。

use std::any::TypeId;
use std::rc::Rc;

use reactive::Scope;

use crate::drag_drop::DropEvent;
use crate::event::{
    ChangeEvent, ClickEvent, DragEvent, ExternalDragEvent, ExternalDropEvent, FieldChangeEvent, FileDropEvent, KeyEvent, PropagationPhase, PinchEvent, PointerEvent, RotateEvent, ScrollEvent, WheelEvent,
};

/// 事件的种类：表里按它找处理器，`listens` 里一种一位。会传播的排在前面（捕获阶段的处理器另有一位，见 [`EventType::capture_bit`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum EventType {
    // ---- 会传播的 ----
    /// 指针按下
    PointerDown,
    /// 指针松开
    PointerUp,
    /// 指针在元素上移动
    PointerMove,
    /// 点击（左键按下并松开、回车或空格、辅助技术、标签）
    Click,
    /// 右键点击
    SecondaryClick,
    /// 双击
    DoubleClick,
    /// 中键点击
    MiddleClick,
    /// 按键按下
    KeyDown,
    /// 按键松开
    KeyUp,
    /// 滚轮、触控板滚动
    Wheel,
    /// 拖动（`Start` 传播、争手势，之后只交给拿到的元素）
    Drag,
    /// 触控板捏合（同拖动）
    Pinch,
    /// 触控板旋转（同拖动）
    Rotate,
    /// 表单级的改值汇总：控件被用户改了值，从控件往外冒泡
    FieldChange,
    /// 进行中的按下被打断（只冒泡，没有捕获阶段的处理器）
    PointerCancel,
    // ---- 只发给元素自己的 ----
    /// 指针进入元素
    PointerEnter,
    /// 指针离开元素
    PointerLeave,
    /// 指针进入、离开（`on_hover` 的简写）
    Hover,
    /// 按下、松开（`on_press` 的简写）
    Press,
    /// 得到、失去焦点
    Focus,
    /// 用键盘得到、失去焦点
    KeyboardFocus,
    /// 拖着的数据进入放置目标
    DragEnter,
    /// 拖着的数据在放置目标上移动
    DragOver,
    /// 拖着的数据离开放置目标（放下、取消也算）
    DragLeave,
    /// 放下（按数据的类型）
    Drop,
    /// 拖到应用外面结束
    DragOutEnd,
    /// 拖着文件进入、离开
    FileHover,
    /// 文件放下
    FileDrop,
    /// 别的应用拖来的内容经过
    ExternalDragOver,
    /// 别的应用拖来的内容离开（放下、取消也算）
    ExternalDragLeave,
    /// 别的应用拖来的内容放下
    ExternalDrop,
    /// 输入框提交
    Submit,
    /// 输入框：用户改了内容
    Change,
    /// 输入框：改完了
    Commit,
    /// 字符网格：输入法、听写提交的文字要插入
    TextInsert,
    /// 富文本里的链接被点了
    LinkClick,
    /// 辅助技术要求改数值
    ValueRequest,
    /// 滚动视口的偏移变了
    Scroll,
}

impl EventType {
    /// 这种事件（冒泡和目标阶段）在 `listens` 里的位
    #[inline]
    pub const fn bit(self) -> u64 {
        1 << self as u8
    }

    /// 点击类事件：在浮层里时只在浮层里传播，不传回声明浮层的元素（见 `tree/route/dispatch.rs`）
    #[inline]
    pub const fn stays_in_overlay(self) -> bool {
        matches!(self, Self::Click | Self::SecondaryClick | Self::DoubleClick | Self::MiddleClick)
    }

    /// 这种事件捕获阶段的处理器在 `listens` 里的位（只有会传播的有，都在高 16 位；别的是 0）
    #[inline]
    pub const fn capture_bit(self) -> u64 {
        if (self as u8) < CAPTURED { 1 << (self as u8 + 48) } else { 0 }
    }
}

/// 会传播的事件排在前面，最多这么多种（捕获阶段的位在高 16 位）
const CAPTURED: u8 = 16;
const _: () = assert!((EventType::PointerCancel as u8) < CAPTURED, "会传播的事件超过了捕获阶段的位数");
const _: () = assert!((EventType::Scroll as u8) < 48, "事件的种类太多，和捕获阶段的位重叠了");

thread_local! {
    /// 正在登记内置处理器（见 [`crate::Element::builtin_handlers`]）
    static BUILTIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 在 `f` 里登记的处理器是内置处理器：同一个元素上排在用户的处理器之后调用。`f` panic 了也恢复（响应式出错后界面还要接着用）
pub(crate) fn builtin_handlers<R>(f: impl FnOnce() -> R) -> R {
    /// 离开时（正常返回或者 panic 展开）恢复进来时的状态
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = BUILTIN.try_with(|b| b.set(self.0));
        }
    }
    let _restore = Restore(BUILTIN.with(|b| b.replace(true)));
    f()
}

/// 一个处理器。按参数分几种，事件种类见 [`EventType`]
#[derive(Clone)]
pub(crate) enum Handler {
    /// 点击：左键、右键、中键、双击
    Click(On<ClickEvent>),
    /// 状态：悬停、按下、焦点、键盘焦点、拖着文件经过、拖到应用外面的结果
    Flag(Rc<dyn Fn(bool)>),
    /// 文字：提交、改动、改完、链接
    Text(Rc<dyn Fn(String)>),
    /// 数值：辅助技术要求改值
    Number(Rc<dyn Fn(f64)>),
    /// 指针：移动
    Pointer(On<PointerEvent>),
    /// 按键：按下、松开
    Key(On<KeyEvent>),
    Drag(On<DragEvent>),
    Pinch(On<PinchEvent>),
    Rotate(On<RotateEvent>),
    /// 拖着的数据进入、经过、离开放置目标
    DropEvent(On<DropEvent>),
    /// 放下：只接这个类型的数据
    TypedDrop(TypeId, On<DropEvent>),
    /// 放下的文件
    FileDrop(On<FileDropEvent>),
    /// 别的应用拖来的内容经过、离开
    ExternalDrag(On<ExternalDragEvent>),
    /// 别的应用拖来的内容放下
    ExternalDrop(On<ExternalDropEvent>),
    /// 滚轮
    Wheel(On<WheelEvent>),
    /// 滚动视口的偏移变了
    Scroll(On<ScrollEvent>),
    /// 输入框改了、改完了
    TextChange(On<ChangeEvent<String>>),
    /// 表单级的改值汇总
    Field(On<FieldChangeEvent>),
}

/// 收事件对象（按引用）的处理器
pub(crate) type On<E> = Rc<dyn Fn(&E)>;

/// 每一类处理器的取出方法：是这一类就返回它
macro_rules! accessors {
    ($($name:ident: $variant:ident => $ty:ty;)*) => {
        impl Handler {
            $(
                pub fn $name(&self) -> Option<&$ty> {
                    match self {
                        Self::$variant(f) => Some(f),
                        _ => None,
                    }
                }
            )*
        }
    };
}

accessors! {
    click: Click => On<ClickEvent>;
    flag: Flag => Rc<dyn Fn(bool)>;
    text: Text => Rc<dyn Fn(String)>;
    number: Number => Rc<dyn Fn(f64)>;
    pointer: Pointer => On<PointerEvent>;
    key: Key => On<KeyEvent>;
    drag: Drag => On<DragEvent>;
    pinch: Pinch => On<PinchEvent>;
    rotate: Rotate => On<RotateEvent>;
    drop_event: DropEvent => On<DropEvent>;
    file_drop: FileDrop => On<FileDropEvent>;
    external_drag: ExternalDrag => On<ExternalDragEvent>;
    external_drop: ExternalDrop => On<ExternalDropEvent>;
    wheel: Wheel => On<WheelEvent>;
    scroll: Scroll => On<ScrollEvent>;
    text_change: TextChange => On<ChangeEvent<String>>;
    field: Field => On<FieldChangeEvent>;
}

/// 表里的一项
#[derive(Clone)]
pub(crate) struct Entry {
    pub kind: EventType,
    /// 捕获阶段的处理器
    pub capture: bool,
    /// 内置处理器（[`crate::Element::builtin_handlers`]）：排在用户的处理器之后
    pub builtin: bool,
    pub handler: Handler,
}

/// 一个元素的处理器表：位掩码加上按登记顺序的列表（没有处理器时不分配）
#[derive(Clone, Default)]
pub(crate) struct HandlerList {
    listens: u64,
    entries: Vec<Entry>,
}

impl HandlerList {
    /// 登记一个冒泡（和目标）阶段的处理器
    pub fn push(&mut self, kind: EventType, handler: Handler) {
        self.add(kind, false, handler);
    }

    /// 登记一个捕获阶段的处理器
    pub fn push_capture(&mut self, kind: EventType, handler: Handler) {
        self.add(kind, true, handler);
    }

    fn add(&mut self, kind: EventType, capture: bool, handler: Handler) {
        self.listens |= if capture { kind.capture_bit() } else { kind.bit() };
        let builtin = BUILTIN.with(std::cell::Cell::get);
        self.entries.push(Entry { kind, capture, builtin, handler });
    }

    /// 登记了的事件种类的位掩码（每种事件一位，捕获阶段另有一位）
    #[inline]
    pub fn mask(&self) -> u64 {
        self.listens
    }

    /// 有没有这种事件的处理器（冒泡和目标阶段）
    #[inline]
    pub fn listens(&self, kind: EventType) -> bool {
        self.listens & kind.bit() != 0
    }

    /// 有没有这种事件在 `phase` 那一段的处理器（目标阶段两种都算）
    #[inline]
    pub fn listens_in(&self, kind: EventType, phase: PropagationPhase) -> bool {
        let mask = match phase {
            PropagationPhase::Capture => kind.capture_bit(),
            PropagationPhase::Target => kind.bit() | kind.capture_bit(),
            PropagationPhase::Bubble => kind.bit(),
        };
        self.listens & mask != 0
    }

    /// 这种事件（冒泡和目标阶段）的处理器，按登记的顺序，取出 `pick` 认的那一类（复制一份 `Rc`，释放树的借用之后调用）
    pub fn of<T: ?Sized>(&self, kind: EventType, pick: impl Fn(&Handler) -> Option<&Rc<T>>) -> Vec<Rc<T>> {
        if !self.listens(kind) {
            return Vec::new();
        }
        self.entries.iter().filter(|e| e.kind == kind && !e.capture).filter_map(|e| pick(&e.handler).cloned()).collect()
    }

    /// 传到这个元素时要调用的处理器：捕获阶段是捕获的那些；目标阶段先捕获的、再冒泡的；冒泡阶段是冒泡的那些。
    /// 每一段里用户的处理器在前，内置处理器（[`crate::Element::builtin_handlers`]）在后
    pub fn in_phase<T: ?Sized>(&self, kind: EventType, phase: PropagationPhase, pick: impl Fn(&Handler) -> Option<&Rc<T>>, out: &mut Vec<Rc<T>>) {
        if !self.listens_in(kind, phase) {
            return;
        }
        let stages: &[bool] = match phase {
            PropagationPhase::Capture => &[true],
            PropagationPhase::Target => &[true, false],
            PropagationPhase::Bubble => &[false],
        };
        for &capture in stages {
            for builtin in [false, true] {
                out.extend(
                    self.entries
                        .iter()
                        .filter(|e| e.kind == kind && e.capture == capture && e.builtin == builtin)
                        .filter_map(|e| pick(&e.handler).cloned()),
                );
            }
        }
    }

    /// 这种事件的全部项（冒泡和目标阶段）
    pub fn entries(&self, kind: EventType) -> impl Iterator<Item = &Entry> + '_ {
        let listens = self.listens(kind);
        self.entries.iter().filter(move |e| listens && e.kind == kind && !e.capture)
    }
}

/// 处理器在创建元素时的作用域里执行：处理器里创建的信号、副作用归元素所在的作用域，
/// 元素卸载时一起释放，不会因为"在处理器里创建、没有归属"而一直存活。
///
/// 处理器可以有返回值（`|| aureole::open_url(..)`、`|| aureole::after(..)`），调用后直接丢掉
pub(crate) fn owned<A: 'static, R>(handler: impl Fn(A) -> R + 'static) -> Rc<dyn Fn(A)> {
    match Scope::current() {
        Some(scope) => Rc::new(move |arg| {
            scope.run(|| {
                handler(arg);
            })
        }),
        None => Rc::new(move |arg| {
            handler(arg);
        }),
    }
}

/// 收引用参数的处理器在创建元素时的作用域里执行，返回值丢掉
pub(crate) fn owned_ref<E: ?Sized + 'static, R>(handler: impl Fn(&E) -> R + 'static) -> Rc<dyn Fn(&E)> {
    match Scope::current() {
        Some(scope) => Rc::new(move |e: &E| {
            scope.run(|| {
                handler(e);
            })
        }),
        None => Rc::new(move |e: &E| {
            handler(e);
        }),
    }
}
