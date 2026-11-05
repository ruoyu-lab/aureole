//! 交给处理器的事件对象：指针、点击、按键、拖动、手势、拖入文件、改值、表单级的改值汇总，以及控制传播的方法。
//!
//! 一次输入走一条管线：命中（或指针捕获）得到路径，
//! 先从外往里走一遍（捕获阶段，`on_*_capture`），到目标元素（目标阶段），再从里往外走一遍（冒泡阶段，`on_*`），
//! 最后执行没被取消的内置行为。处理器拿到的是事件对象的引用，用它上面的方法控制传播：
//!
//! - [`PointerEvent::stop_propagation`]：外面（冒泡）或里面（捕获）的元素不再收到；这个元素上剩下的处理器照样调用；
//! - [`PointerEvent::prevent_builtin`]：取消内置行为（拿焦点、选字、拖动、滚动、激活……见 [`Builtin`]）；
//! - [`PointerEvent::capture_pointer`]：按下时调用，之后的移动、松开都交给这个元素，指针移出它、移出窗口也一样。
//!
//! 处理器可以写成收事件对象的闭包，也可以写成不带参数的闭包（[`IntoHandler`]）：
//! `.on_click(|| save())` 和 `.on_click(|e: &ClickEvent| ..)` 收到的是同一个事件。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use reactive::Scope;
use scene::Angle;

use crate::image::RgbaImage;
use crate::input::{Buttons, GesturePhase, Key, Modifiers, MouseButton, PhysicalKey, PointerId, PointerKind, WheelDelta};
use crate::interaction::{Chord, FocusHandle};

/// 事件传到了哪一段
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PropagationPhase {
    /// 从外往里，还没到目标（`on_*_capture` 的处理器）
    Capture,
    /// 在目标元素上
    #[default]
    Target,
    /// 从目标往外
    Bubble,
}

bitflags::bitflags! {
    /// 事件之后的内置行为，处理器用 `prevent_builtin(..)` 取消
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Builtin: u16 {
        /// 按下时让元素获得焦点
        const FOCUS = 1;
        /// 在文字上按下、拖动时放插入点、选字
        const TEXT_SELECT = 1 << 1;
        /// 按下后移动时开始拖动（`on_drag`、拖放、拖到应用外面）
        const DRAG = 1 << 2;
        /// 滚轮、方向键、翻页键滚动视口；手指拖动滚动视口（在 `on_pointer_down` 里取消）
        const SCROLL = 1 << 3;
        /// 点击：松开时合成点击；回车、空格激活有焦点的元素
        const ACTIVATE = 1 << 4;
        /// 在浮层外面按下、按 Esc 时请求关闭浮层
        const DISMISS = 1 << 5;
        /// Tab、Shift+Tab 切换焦点
        const TAB = 1 << 6;
        /// `focus_arrows` 的方向键在子孙之间移动焦点；输入框里的方向键移动插入点
        const ARROWS = 1 << 7;
        /// 右键按下、手指长按时的右键点击（`on_secondary_click`）
        const CONTEXT_MENU = 1 << 8;
        /// 有焦点的输入框处理按键：打字、删除、移动插入点、剪贴板、撤销
        const EDIT = 1 << 9;
        /// 关窗口（窗口的 `on_close_request`）
        const CLOSE = 1 << 10;
        /// 按在窗口的拖动区域（`window_drag_area`）上拖动窗口、双击缩放窗口
        const WINDOW_DRAG = 1 << 11;
        /// 全部
        const ALL = (1 << 12) - 1;
    }
}

/// 传播的状态：处理器拿到的是事件的引用，用格子记下它们的要求
#[derive(Clone, Debug, Default)]
pub(crate) struct Flow {
    stopped: Cell<bool>,
    prevented: Cell<Builtin>,
    capture: Cell<bool>,
    phase: Cell<PropagationPhase>,
    /// 手势开始时，这个元素的处理器要不要这个手势
    claim: Cell<Claim>,
}

/// 手势开始时一个元素的处理器的表态（见 [`DragEvent::claim_gesture`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Claim {
    /// 没表态：目标和冒泡阶段第一个有处理器的元素按默认拿到
    #[default]
    Undecided,
    /// 要
    Claimed,
    /// 不要，交给外面的元素
    Declined,
}

/// 传播的状态只在分发的那一刻有意义：比较事件时不算它
impl PartialEq for Flow {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Flow {
    pub fn stopped(&self) -> bool {
        self.stopped.get()
    }

    /// 停止传播（分发器替元素做的，比如手势有了主人）
    pub fn stop(&self) {
        self.stopped.set(true);
    }

    /// 取走这个元素的表态（每个元素的处理器之后取一次）
    pub fn take_claim(&self) -> Claim {
        self.claim.replace(Claim::Undecided)
    }

    pub fn prevented(&self) -> Builtin {
        self.prevented.get()
    }

    pub fn prevent(&self, builtin: Builtin) {
        self.prevented.set(self.prevented.get() | builtin);
    }

    pub fn set_phase(&self, phase: PropagationPhase) {
        self.phase.set(phase);
    }

    /// 处理器要求捕获指针了（取走要求）
    pub fn take_capture(&self) -> bool {
        self.capture.replace(false)
    }
}

/// 控制传播的几个方法，每种会传播的事件都有
macro_rules! propagation {
    ($($ty:ident),*) => {
        $(
            impl $ty {
                /// 停止传播：冒泡阶段外面的元素、捕获阶段里面的元素不再收到；这个元素上剩下的处理器照样调用
                pub fn stop_propagation(&self) {
                    self.flow.stopped.set(true);
                }

                /// 已经有处理器停止了传播
                pub fn is_propagation_stopped(&self) -> bool {
                    self.flow.stopped.get()
                }

                /// 取消内置行为，见 [`Builtin`]
                pub fn prevent_builtin(&self, builtin: Builtin) {
                    self.flow.prevent(builtin);
                }

                /// 这些内置行为是不是已经被取消了（都取消了才是 `true`）
                pub fn is_builtin_prevented(&self, builtin: Builtin) -> bool {
                    self.flow.prevented.get().contains(builtin)
                }

                /// 事件传到了哪一段
                pub fn phase(&self) -> PropagationPhase {
                    self.flow.phase.get()
                }

                pub(crate) fn flow(&self) -> &Flow {
                    &self.flow
                }
            }
        )*
    };
}

propagation!(PointerEvent, ClickEvent, KeyEvent, WheelEvent);

/// 手势事件（拖动、捏合、旋转）控制传播、争手势的方法
macro_rules! gesture {
    ($($ty:ident),*) => {
        $(
            impl $ty {
                /// 停止传播：冒泡阶段外面的元素、捕获阶段里面的元素不再收到这次 `Start`；这个元素上剩下的处理器照样调用。
                /// 停止了又没有元素拿到手势时，这个手势不开始（收到过 `Start` 的元素收到 `Cancel`）
                pub fn stop_propagation(&self) {
                    self.flow.stopped.set(true);
                }

                /// 已经有处理器停止了传播
                pub fn is_propagation_stopped(&self) -> bool {
                    self.flow.stopped.get()
                }

                /// 事件传到了哪一段。`Start` 按捕获 → 目标 → 冒泡走；之后的 `Move`、`End`、`Cancel` 只交给拿到手势的元素，
                /// 是它拿到时的那一段（在捕获阶段抢到的是捕获阶段）
                pub fn phase(&self) -> PropagationPhase {
                    self.flow.phase.get()
                }

                /// 这个手势我要（只在 `Start` 时有效）：这个元素拿到手势，传播停止，之后的 `Move`、`End` 只交给它；
                /// 收到过 `Start` 的别的元素收到 `Cancel`。父元素在捕获阶段（`on_*_capture`）调用就是抢在里面的元素之前拿走；
                /// 子元素在目标阶段调用是明说要它（不调用也是它的，见 [`Self::decline_gesture`]）
                pub fn claim_gesture(&self) {
                    self.flow.claim.set(Claim::Claimed);
                }

                /// 这个手势我不要（只在 `Start` 时有效）：目标和冒泡阶段第一个有处理器的元素默认拿到手势，
                /// 调用了这个就接着往外传，交给外面的元素（比如横着拖的卡片把竖着的拖动让给外面的视口）。
                /// 之后收到 `Cancel`
                pub fn decline_gesture(&self) {
                    self.flow.claim.set(Claim::Declined);
                }

                pub(crate) fn flow(&self) -> &Flow {
                    &self.flow
                }
            }
        )*
    };
}

gesture!(DragEvent, PinchEvent, RotateEvent);

impl FieldChangeEvent {
    pub(crate) fn new(name: Option<String>, focus_handle: Option<FocusHandle>, value: FieldValue, source: ChangeSource) -> Self {
        Self { name, focus_handle, value, source, flow: Flow::default() }
    }

    /// 停止传播：外面（冒泡）或里面（捕获）的元素不再收到；这个元素上剩下的处理器照样调用
    pub fn stop_propagation(&self) {
        self.flow.stopped.set(true);
    }

    /// 已经有处理器停止了传播
    pub fn is_propagation_stopped(&self) -> bool {
        self.flow.stopped.get()
    }

    /// 事件传到了哪一段：控件自己是目标阶段，外面的容器是冒泡阶段
    pub fn phase(&self) -> PropagationPhase {
        self.flow.phase.get()
    }

    pub(crate) fn flow(&self) -> &Flow {
        &self.flow
    }
}

/// 指针事件是哪一种
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PointerEventKind {
    /// 按下一个键
    Down,
    /// 松开一个键
    Up,
    /// 移动
    #[default]
    Move,
    /// 进入元素（不传播）
    Enter,
    /// 离开元素（不传播）
    Leave,
    /// 进行中的按下被打断（窗口失去焦点、拖动交给了系统）
    Cancel,
}

/// 指针事件：按下、松开、移动、进入、离开、取消共用。位置有两份，元素自己的坐标（左上角是 (0, 0)，
/// 已经去掉了元素和祖先的变换，传播时每一层都换算成那一层元素的坐标）和窗口坐标
#[derive(Clone, Debug)]
pub struct PointerEvent {
    /// 哪一种
    pub kind: PointerEventKind,
    /// 元素自己的坐标，逻辑像素
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    /// 按下、松开的键；移动、进入、离开时是 `None`
    pub button: Option<MouseButton>,
    /// 此刻按着的键
    pub buttons: Buttons,
    /// 此刻按着的修饰键
    pub modifiers: Modifiers,
    /// 连续点击的次数：1 是单击，2 是双击……（按下、松开时有，别的是 0）
    pub clicks: u32,
    /// 什么设备
    pub device: PointerKind,
    /// 这种设备上的哪一个指针：触摸屏上每根手指一个（一根手指从按下到抬起不变），鼠标是 [`PointerId::MOUSE`]
    pub pointer_id: PointerId,
    /// 时间戳（和定时器同一个时钟，测试工具里是虚拟时间）
    pub time: Duration,
    pub(crate) flow: Flow,
}

impl PointerEvent {
    pub(crate) fn new(kind: PointerEventKind, window_position: [f32; 2], button: Option<MouseButton>, modifiers: Modifiers, clicks: u32) -> Self {
        Self {
            kind,
            position: window_position,
            window_position,
            button,
            buttons: Buttons::empty(),
            modifiers,
            clicks,
            device: PointerKind::Mouse,
            pointer_id: PointerId::MOUSE,
            time: crate::timer::now_duration(),
            flow: Flow::default(),
        }
    }

    /// 捕获指针（只在按下时有效）：这次按下之后的移动、松开都交给这个元素，指针移出它、移出窗口也一样
    pub fn capture_pointer(&self) {
        self.flow.capture.set(true);
    }
}

/// 点击是怎么来的
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClickSource {
    /// 指针：按下和松开都在元素上
    #[default]
    Pointer,
    /// 键盘：有焦点时按回车、空格
    Keyboard,
    /// 辅助技术请求点击
    Accessibility,
    /// 点了它的标签（`controls::Label`）
    Label,
}

/// 点击：左键（`on_click`）、右键（`on_secondary_click`）、中键（`on_middle_click`）、双击（`on_double_click`）。
/// 不是指针来的点击（键盘、辅助技术、标签）位置是元素的正中
#[derive(Clone, Debug)]
pub struct ClickEvent {
    /// 怎么来的
    pub source: ClickSource,
    /// 哪个键；不是指针来的是左键
    pub button: MouseButton,
    /// 元素自己的坐标，逻辑像素
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    /// 此刻按着的修饰键
    pub modifiers: Modifiers,
    /// 连续点击的次数：1 是单击，2 是双击……
    pub clicks: u32,
    /// 时间戳，秒
    pub time: Duration,
    pub(crate) flow: Flow,
}

impl ClickEvent {
    pub(crate) fn new(source: ClickSource, button: MouseButton, window_position: [f32; 2], modifiers: Modifiers, clicks: u32) -> Self {
        Self {
            source,
            button,
            position: window_position,
            window_position,
            modifiers,
            clicks,
            time: crate::timer::now_duration(),
            flow: Flow::default(),
        }
    }
}

/// 按键事件，见 [`crate::Element::on_key_down`]
#[derive(Clone, Debug)]
pub struct KeyEvent {
    /// 逻辑按键（按当前键盘布局和修饰键）
    pub key: Key,
    /// 不算修饰键时的按键（Shift+/ 的这里是 `/`），平台不提供时等于 `key`
    pub base: Key,
    /// 物理键（键位）；平台不提供时为 `None`
    pub physical: Option<PhysicalKey>,
    /// 这个键输入的文字
    pub text: Option<String>,
    /// 按下时按住的修饰键
    pub modifiers: Modifiers,
    /// 按住不放时系统发的重复
    pub repeated: bool,
    /// 焦点上的元素正在输入法组字：这个键是给输入法的，输入框这类的内置编辑不处理它
    pub composing: bool,
    /// 时间戳，秒
    pub time: Duration,
    /// Windows、Linux 上的 AltGr（报成 Ctrl+Alt）：打出来的是字，不是组合键
    altgr: bool,
    pub(crate) flow: Flow,
}

impl KeyEvent {
    pub(crate) fn new(key: Key, base: Key, physical: Option<PhysicalKey>, text: Option<String>, modifiers: Modifiers, repeated: bool) -> Self {
        // 主修饰键是 Command 的平台（macOS）没有 AltGr 这回事；别的平台上 Ctrl+Alt（不带 Win 键）是 AltGr
        let altgr = modifiers.has_control()
            && modifiers.has_alt()
            && !modifiers.has_super()
            && crate::system::conventions().primary_modifier == Modifiers::CONTROL;
        Self { key, base, physical, text, modifiers, repeated, composing: false, time: crate::timer::now_duration(), altgr, flow: Flow::default() }
    }

    /// 是否是这个组合键，写法同 [`crate::shortcut`]：`e.is("Escape")`、`e.is("CmdOrCtrl+Enter")`
    pub fn is(&self, chord: &str) -> bool {
        Chord::parse(chord).is_some_and(|c| c.matches(&self.key, &self.base, self.modifiers))
    }

    /// 按着平台的主修饰键（macOS 是 Command，别的平台是 Ctrl；AltGr 不算）：快捷键，⌘C、⌘V 这类
    pub fn has_primary_modifier(&self) -> bool {
        self.modifiers.intersects(crate::system::conventions().primary_modifier) && !self.altgr
    }

    /// 是组合键（按着 Command，或者按着不是 AltGr 的 Ctrl）：不打字，留给快捷键和处理器。
    /// 所有输入目标共用这一条规则（见 [`KeyEvent::typed_text`]）
    pub fn is_chord(&self) -> bool {
        self.modifiers.has_super() || (self.modifiers.has_control() && !self.altgr)
    }

    /// 这个键打出的字：不是组合键、不在组字、不是控制字符（退格、回车、Esc 这类另外处理）时才有。
    /// 输入框、字符网格、自己定义的编辑器都按它决定打不打字
    pub fn typed_text(&self) -> Option<&str> {
        let text = self.text.as_deref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control))?;
        (!self.is_chord() && !self.composing).then_some(text)
    }
}

/// 滚轮、触控板滚动，见 [`crate::Element::on_wheel`]。交给指针下最内层的元素往外冒泡，之后滚动视口才滚
/// （取消 [`Builtin::SCROLL`] 就不滚）
#[derive(Clone, Debug)]
pub struct WheelEvent {
    /// 滚动量：鼠标滚轮按格（行），触控板按逻辑像素；正数让偏移变大（向下、向右滚，内容往上、往左走）
    pub delta: WheelDelta,
    /// 触控板手势进行到哪一步（鼠标滚轮总是 `Move`）
    pub phase: GesturePhase,
    /// 是系统在手指抬起后接着发的惯性
    pub inertial: bool,
    /// 元素自己的坐标，逻辑像素
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    /// 此刻按着的修饰键（按住 Ctrl、Command 滚动常用来缩放）
    pub modifiers: Modifiers,
    /// 时间戳，秒
    pub time: Duration,
    pub(crate) flow: Flow,
}

impl WheelEvent {
    pub(crate) fn new(delta: WheelDelta, phase: GesturePhase, inertial: bool, window_position: [f32; 2], modifiers: Modifiers) -> Self {
        Self {
            delta,
            phase,
            inertial,
            position: window_position,
            window_position,
            modifiers,
            time: crate::timer::now_duration(),
            flow: Flow::default(),
        }
    }
}

/// 滚动视口的偏移变了，见 [`crate::Element::on_scroll`]：一帧最多一次，合并了这一帧里所有的变化
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollEvent {
    /// 现在的偏移（内容往上、往左滚过的距离），逻辑像素
    pub offset: [f32; 2],
    /// 和上一次的偏移（第一次是 0）差多少
    pub delta: [f32; 2],
}

/// 拖动事件，见 [`crate::Element::on_drag`]。位移、速度都是窗口坐标（逻辑像素），不受元素自己缩放的影响。
///
/// `Start` 沿按下处的路径走捕获 → 目标 → 冒泡，决定谁拿到这个拖动（[`DragEvent::claim_gesture`]、
/// [`DragEvent::decline_gesture`]）；之后只交给拿到的元素
#[derive(Clone, Debug, PartialEq)]
pub struct DragEvent {
    /// 进行到哪一步
    pub phase: GesturePhase,
    /// 元素自己的坐标
    pub position: [f32; 2],
    /// 窗口坐标；拖出窗口后可以是负数或超过窗口大小
    pub window_position: [f32; 2],
    /// 按下的位置，窗口坐标
    pub start_position: [f32; 2],
    /// 和上一个事件之间的位移。`Start` 的位移是从按下到现在（包括越过阈值的那一小段），所以各次位移加起来就是总位移
    pub delta: [f32; 2],
    /// 从按下到现在的总位移
    pub translation: [f32; 2],
    /// 最近约 0.1 秒的平均速度，逻辑像素每秒；松手前停了一会儿时 `End` 的速度是 0
    pub velocity: [f32; 2],
    /// 拖动用的鼠标键
    pub button: MouseButton,
    /// 此刻按住的修饰键
    pub modifiers: Modifiers,
    pub(crate) flow: Flow,
}

/// 捏合（触控板、触摸屏上两根手指），见 [`crate::Element::on_pinch`]。`Start` 的传播和争手势同 [`DragEvent`]
#[derive(Clone, Debug, PartialEq)]
pub struct PinchEvent {
    /// 进行到哪一步
    pub phase: GesturePhase,
    /// 从手势开始到现在的缩放倍数（开始时是 1）
    pub scale: f32,
    /// 这一次的倍数：乘到自己的缩放上就是跟手的缩放
    pub delta: f32,
    /// 元素自己的坐标（双指中心：触控板是指针的位置，触摸屏是两根手指的中点）
    pub position: [f32; 2],
    /// 同一位置的窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    pub(crate) flow: Flow,
}

/// 旋转（触控板、触摸屏上两根手指），见 [`crate::Element::on_rotate`]。正值顺时针（和 [`crate::Element::rotation`] 一致）。
/// `Start` 的传播和争手势同 [`DragEvent`]
#[derive(Clone, Debug, PartialEq)]
pub struct RotateEvent {
    /// 进行到哪一步
    pub phase: GesturePhase,
    /// 从手势开始到现在转过的角度
    pub rotation: Angle,
    /// 这一次转过的角度
    pub delta: Angle,
    /// 元素自己的坐标（双指中心：触控板是指针的位置，触摸屏是两根手指的中点）
    pub position: [f32; 2],
    /// 同一位置的窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    pub(crate) flow: Flow,
}

impl PinchEvent {
    pub(crate) fn new(phase: GesturePhase, scale: f32, delta: f32, window_position: [f32; 2]) -> Self {
        Self { phase, scale, delta, position: window_position, window_position, flow: Flow::default() }
    }
}

impl RotateEvent {
    pub(crate) fn new(phase: GesturePhase, rotation: Angle, delta: Angle, window_position: [f32; 2]) -> Self {
        Self { phase, rotation, delta, position: window_position, window_position, flow: Flow::default() }
    }
}

/// 从系统拖进来的文件放下，见 [`crate::Element::on_file_drop`]
#[derive(Clone, Debug, PartialEq)]
pub struct FileDropEvent {
    /// 放下的文件
    pub paths: Vec<PathBuf>,
    /// 元素自己的坐标
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
}

bitflags::bitflags! {
    /// 别的应用拖来的内容有哪几种，见 [`crate::Element::on_external_drag_over`]。有文件时只算文件（访达拖文件时也带着文件名的文字）
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct ExternalDataKinds: u8 {
        /// 文字
        const TEXT = 1;
        /// 链接
        const LINK = 1 << 1;
        /// 图片
        const IMAGE = 1 << 2;
        /// 文件
        const FILES = 1 << 3;
    }
}

/// 接受拖来的内容算什么操作：系统按它显示光标，也告诉拖动的来源。来源不许这种操作时换一种它许的
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExternalDropEffect {
    /// 拷贝（默认）
    #[default]
    Copy,
    /// 移动：来源可能删掉原来的
    Move,
    /// 建链接
    Link,
}

/// 别的应用拖来的内容经过元素、离开元素，见 [`crate::Element::on_external_drag_over`]
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalDragEvent {
    /// 内容有哪几种。放下之前读不到内容本身
    pub kinds: ExternalDataKinds,
    /// 元素自己的坐标
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    pub(crate) effect: Cell<Option<ExternalDropEffect>>,
}

impl ExternalDragEvent {
    pub(crate) fn new(kinds: ExternalDataKinds, position: [f32; 2], window_position: [f32; 2]) -> Self {
        Self { kinds, position, window_position, effect: Cell::new(None) }
    }

    /// 接受（算拷贝）：系统显示能放下的光标，放下时这个元素收到 [`crate::Element::on_external_drop`]。
    /// 不调用就是不接受，接着问外面一层的元素
    pub fn accept(&self) {
        self.effect.set(Some(ExternalDropEffect::Copy));
    }

    /// 接受，并且说明算什么操作（光标不一样）
    pub fn accept_with(&self, effect: ExternalDropEffect) {
        self.effect.set(Some(effect));
    }

    /// 已经接受了
    pub fn is_accepted(&self) -> bool {
        self.effect.get().is_some()
    }
}

/// 别的应用拖来、放下的内容，从哪里来都一样：系统的拖放（见 [`crate::Element::on_external_drop`]），测试工具也用它模拟
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExternalDropData {
    /// 文字
    pub text: Option<String>,
    /// 链接（网址）
    pub link: Option<String>,
    /// 图片，能直接给 [`crate::image()`]
    pub image: Option<RgbaImage>,
    /// 文件
    pub files: Vec<PathBuf>,
}

impl ExternalDropData {
    /// 里面有哪几种内容
    pub fn kinds(&self) -> ExternalDataKinds {
        let mut kinds = ExternalDataKinds::empty();
        kinds.set(ExternalDataKinds::TEXT, self.text.is_some());
        kinds.set(ExternalDataKinds::LINK, self.link.is_some());
        kinds.set(ExternalDataKinds::IMAGE, self.image.is_some());
        kinds.set(ExternalDataKinds::FILES, !self.files.is_empty());
        kinds
    }
}

/// 别的应用拖来的内容放下了，见 [`crate::Element::on_external_drop`]
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalDropEvent {
    /// 文字
    pub text: Option<String>,
    /// 链接（网址）
    pub link: Option<String>,
    /// 图片，能直接给 [`crate::image()`]
    pub image: Option<RgbaImage>,
    /// 文件
    pub files: Vec<PathBuf>,
    /// 元素自己的坐标
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
}

// ---- 改值 ----

/// 值是怎么被用户改的（改值事件只在用户改动时发，代码里改绑定的信号不发）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChangeSource {
    /// 指针：点、拖
    #[default]
    Pointer,
    /// 键盘：打字、删除、粘贴、撤销、输入法提交，方向键、回车、空格
    Keyboard,
    /// 辅助技术要求改值
    Accessibility,
    /// 点了它的标签
    Label,
}

impl From<ClickSource> for ChangeSource {
    fn from(activation: ClickSource) -> Self {
        match activation {
            ClickSource::Pointer => Self::Pointer,
            ClickSource::Keyboard => Self::Keyboard,
            ClickSource::Accessibility => Self::Accessibility,
            ClickSource::Label => Self::Label,
        }
    }
}

/// 改值事件：输入框和有值的基础控件的 `on_change`（用户每改一次）、`on_commit`（改完了）。只发给控件自己，不冒泡。
/// 处理器可以只收值（`|v| ..`），也可以收整个事件（`|e: &ChangeEvent<bool>| ..`，带着是怎么改的），见 [`IntoChangeHandler`]
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeEvent<T> {
    /// 新的值
    pub value: T,
    /// 怎么改的
    pub source: ChangeSource,
}

/// 表单级的改值事件里新值的描述（[`FieldChangeEvent::value`]）：各种控件的值的类型不一样，这里统一成几种。
/// 显示出来（`{}`）是值本身的文字：`true`、`0.6`、输入框的内容、选项的文字
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum FieldValue {
    /// 开关量：复选框、开关
    Bool(bool),
    /// 数：滑块、数字输入
    Number(f64),
    /// 文字：输入框、多行输入框
    Text(String),
    /// 密码框：不带内容（要内容用它自己的 `on_change`），免得汇总到表单的日志里
    Secret,
    /// 选项：单选组、下拉选择。第几项（从 0 开始）和它显示的文字
    Choice {
        /// 第几项
        index: usize,
        /// 显示的文字
        text: String,
    },
}

impl std::fmt::Display for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(v) => write!(f, "{v}"),
            Self::Number(v) => write!(f, "{v}"),
            Self::Text(v) => f.write_str(v),
            Self::Secret => f.write_str("••••"),
            Self::Choice { text, .. } => f.write_str(text),
        }
    }
}

/// 表单级的改值事件（[`crate::Element::on_field_change`]）：一个控件的值被用户改了，从控件往外冒泡
/// （沿逻辑父元素，模态浮层是边界），容器上能收到里面任何控件的改动。代码里改绑定的信号时不发。
///
/// 控件自己的 `on_change`、`on_commit` 照旧只发给控件自己；这一个是给表单用的
#[derive(Clone, Debug)]
pub struct FieldChangeEvent {
    /// 控件的名字（`.name(..)`）；没起名字时是 `None`
    pub name: Option<String>,
    /// 控件的焦点句柄（交给控件、或者控件里面的输入框的那个）；没给时是 `None`
    pub focus_handle: Option<FocusHandle>,
    /// 新的值
    pub value: FieldValue,
    /// 怎么改的
    pub source: ChangeSource,
    pub(crate) flow: Flow,
}

/// 改值事件的处理器：只收值（`Fn(T)`）或者收整个事件（`Fn(&ChangeEvent<T>)`），返回值丢掉。
///
/// 只收值的闭包，参数的用法定下了类型时（传给收这个类型的函数、写进同类型的信号）不用写类型；
/// 只拿去打印、格式化时要写出来：`|v: f32| println!("{v}")`
pub trait IntoChangeHandler<T, Marker> {
    /// 转成收事件引用的处理器，在创建控件时的作用域里执行
    #[allow(clippy::type_complexity, reason = "处理器的类型就是这样，起个别名反而要多找一处")]
    fn into_change_handler(self) -> Rc<dyn Fn(&ChangeEvent<T>)>;
}

impl<T: Clone + 'static, R, F: Fn(T) -> R + 'static> IntoChangeHandler<T, fn(T) -> R> for F {
    fn into_change_handler(self) -> Rc<dyn Fn(&ChangeEvent<T>)> {
        IntoHandler::<ChangeEvent<T>, fn(&ChangeEvent<T>)>::into_handler(move |e: &ChangeEvent<T>| {
            self(e.value.clone());
        })
    }
}

impl<T: 'static, R, F: Fn(&ChangeEvent<T>) -> R + 'static> IntoChangeHandler<T, fn(&ChangeEvent<T>) -> R> for F {
    fn into_change_handler(self) -> Rc<dyn Fn(&ChangeEvent<T>)> {
        IntoHandler::<ChangeEvent<T>, fn(&ChangeEvent<T>)>::into_handler(move |e: &ChangeEvent<T>| {
            self(e);
        })
    }
}

// ---- 处理器的两种写法 ----

/// 处理器：收事件对象（`|e: &ClickEvent| ..`）或者不带参数（`|| ..`）的闭包，返回值丢掉。
///
/// 收事件对象的闭包要写出参数的类型：同一个方法接受两种闭包，编译器不能从方法上推断出参数的类型
pub trait IntoHandler<E, Marker> {
    /// 转成收事件对象引用的处理器，在创建元素时的作用域里执行
    fn into_handler(self) -> Rc<dyn Fn(&E)>;
}

impl<E: 'static, R, F: Fn() -> R + 'static> IntoHandler<E, fn() -> R> for F {
    fn into_handler(self) -> Rc<dyn Fn(&E)> {
        match Scope::current() {
            Some(scope) => Rc::new(move |_: &E| {
                scope.run(|| {
                    self();
                })
            }),
            None => Rc::new(move |_: &E| {
                self();
            }),
        }
    }
}

impl<E: 'static, R, F: Fn(&E) -> R + 'static> IntoHandler<E, fn(&E) -> R> for F {
    fn into_handler(self) -> Rc<dyn Fn(&E)> {
        match Scope::current() {
            Some(scope) => Rc::new(move |e: &E| {
                scope.run(|| {
                    self(e);
                })
            }),
            None => Rc::new(move |e: &E| {
                self(e);
            }),
        }
    }
}
