//! 原始输入：窗口事件换算之后（`translate.rs`）交给窗口宿主的唯一形式。
//! 真实窗口、测试工具、录下的输入回放都走 `WindowHost::input`，元素树只收这里的类型

use std::path::PathBuf;
use std::time::Duration;

use smol_str::SmolStr;

use super::keys::{Key, Modifiers, PhysicalKey};
use crate::event::{ExternalDataKinds, ExternalDropData};

/// 一次输入
#[derive(Clone, Debug, PartialEq)]
pub struct InputEvent {
    /// 时间戳：真实窗口里是统一时钟的读数（从进程第一次读时钟算起，所有窗口一样），测试工具和回放里是虚拟时间
    pub time: Duration,
    /// 什么输入
    pub kind: InputKind,
}

impl InputEvent {
    /// `time` 时的一次输入
    pub fn new(time: Duration, kind: InputKind) -> Self {
        Self { time, kind }
    }
}

/// 输入的种类。坐标都是窗口坐标，逻辑像素
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum InputKind {
    /// 按下一个指针按键；手指放上触摸屏（[`PointerKind::Touch`]，按键是左键）
    PointerDown(PointerInput),
    /// 松开一个指针按键；手指抬起
    PointerUp(PointerInput),
    /// 指针移动；按着的手指移动
    PointerMove(PointerInput),
    /// 指针进入窗口（之后紧跟着一次移动）
    PointerEnter(PointerInput),
    /// 指针离开窗口
    PointerLeave(PointerInput),
    /// 进行中的指针操作被打断（拖动、手势取消，按下的状态清掉）；系统收走了这根手指
    PointerCancel(PointerInput),
    /// 滚轮、触控板滚动
    Wheel(WheelInput),
    /// 触控板捏合
    Pinch(GestureInput),
    /// 触控板双指旋转
    Rotate(GestureInput),
    /// 按下一个键（按住不放时系统重复发，`repeated` 为真）
    KeyDown(KeyInput),
    /// 松开一个键
    KeyUp(KeyInput),
    /// 按住的修饰键变了
    Modifiers(Modifiers),
    /// 输入法
    Ime(Ime),
    /// 窗口得到（`true`）、失去键盘焦点
    WindowFocus(bool),
    /// 拖着文件经过窗口；位置未知时为 `None`
    FileHover(Option<[f32; 2]>),
    /// 文件放下
    FileDrop {
        /// 放下的位置；未知时为 `None`
        position: Option<[f32; 2]>,
        /// 拖进来的文件
        paths: Vec<PathBuf>,
    },
    /// 拖着文件离开窗口或取消
    FileCancel,
    /// 别的应用拖来的文字、链接、图片经过窗口（文件走 [`Self::FileHover`]）
    ExternalDragOver {
        /// 位置
        position: [f32; 2],
        /// 内容有哪几种
        kinds: ExternalDataKinds,
    },
    /// 别的应用拖来的内容离开窗口或取消
    ExternalDragLeave,
    /// 别的应用拖来的文字、链接、图片放下（文件走 [`Self::FileDrop`]）
    ExternalDrop {
        /// 位置
        position: [f32; 2],
        /// 内容
        data: ExternalDropData,
    },
    /// 窗口大小或缩放变了
    Resize {
        /// 内容区大小，逻辑像素
        size: [f32; 2],
        /// 缩放（逻辑像素 -> 物理像素）
        scale_factor: f32,
    },
}

/// 指针输入的内容
#[derive(Clone, Debug, PartialEq)]
pub struct PointerInput {
    /// 指针的位置；离开窗口、不知道时为 `None`
    pub position: Option<[f32; 2]>,
    /// 按下、松开的键；移动时为 `None`
    pub button: Option<MouseButton>,
    /// 此刻按着的键
    pub buttons: Buttons,
    /// 此刻按着的修饰键
    pub modifiers: Modifiers,
    /// 什么设备
    pub device: PointerKind,
    /// 这种设备上的哪一个指针：触摸屏上每根手指一个，鼠标是 [`PointerId::MOUSE`]
    pub pointer_id: PointerId,
}

impl PointerInput {
    /// 指针在 `position`，没有按着的键和修饰键
    pub fn mouse(position: Option<[f32; 2]>) -> Self {
        Self { position, button: None, buttons: Buttons::empty(), modifiers: Modifiers::empty(), device: PointerKind::Mouse, pointer_id: PointerId::MOUSE }
    }

    /// 触摸屏上的一根手指在 `position`：按着（`buttons` 是左键，手指当主键算），没有修饰键
    pub fn touch(pointer_id: PointerId, position: [f32; 2]) -> Self {
        Self { position: Some(position), button: None, buttons: Buttons::LEFT, modifiers: Modifiers::empty(), device: PointerKind::Touch, pointer_id }
    }
}

/// 指针的编号：同一种设备上区分是哪一个指针。鼠标只有一个（[`PointerId::MOUSE`]）；触摸屏上每根手指一个，
/// 系统给的，手指抬起后可能重用。不同设备的编号可能相同，要一起看 [`PointerKind`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PointerId(pub u64);

impl PointerId {
    /// 鼠标（和别的只有一个指针的设备）
    pub const MOUSE: Self = Self(0);
}

bitflags::bitflags! {
    /// 按着的指针按键
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Buttons: u8 {
        /// 左键（主键）
        const LEFT = 1;
        /// 右键
        const RIGHT = 1 << 1;
        /// 中键
        const MIDDLE = 1 << 2;
        /// 后退键
        const BACK = 1 << 3;
        /// 前进键
        const FORWARD = 1 << 4;
    }
}

impl Buttons {
    /// 这个键对应的位；其他键没有位
    pub fn from_button(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Self::LEFT,
            MouseButton::Right => Self::RIGHT,
            MouseButton::Middle => Self::MIDDLE,
            MouseButton::Back => Self::BACK,
            MouseButton::Forward => Self::FORWARD,
            MouseButton::Other(_) => Self::empty(),
        }
    }
}

/// 指针设备的种类
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PointerKind {
    /// 指针（平台不区分时触控板的点按也是它）
    #[default]
    Mouse,
    /// 触控板
    Trackpad,
    /// 笔
    Pen,
    /// 触摸屏上的一根手指
    Touch,
}

/// 一次滚动输入。位移是"滚动量"：正数让偏移变大（向下、向右滚，内容往上、往左走）
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WheelDelta {
    /// 鼠标滚轮，按格（行）
    Lines([f32; 2]),
    /// 触控板等精确设备，逻辑像素
    Pixels([f32; 2]),
}

/// 手势、滚动、拖动进行到哪一步
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GesturePhase {
    /// 开始：手指放上触控板（系统的惯性开始），拖动越过阈值，手势的第一帧
    Start,
    /// 进行中（鼠标滚轮也是它）
    Move,
    /// 正常结束：手指抬起（系统的惯性结束），松手
    End,
    /// 被打断：系统取消，窗口失去焦点，元素被卸载（应用可以把状态退回去）
    Cancel,
}

/// 滚轮、触控板滚动
#[derive(Clone, Debug, PartialEq)]
pub struct WheelInput {
    /// 位移
    pub delta: WheelDelta,
    /// 手势进行到哪一步
    pub phase: GesturePhase,
    /// 指针的位置；不知道时为 `None`
    pub position: Option<[f32; 2]>,
    /// 此刻按着的修饰键
    pub modifiers: Modifiers,
}

/// 触控板捏合、旋转
#[derive(Clone, Debug, PartialEq)]
pub struct GestureInput {
    /// 这一次的变化：捏合是放大的比例（0.02 是放大 2%），旋转是弧度（正值逆时针，和触控板报告的方向一样）
    pub delta: f32,
    /// 手势进行到哪一步
    pub phase: GesturePhase,
    /// 指针的位置；不知道时为 `None`
    pub position: Option<[f32; 2]>,
}

/// 按下、松开一个键
#[derive(Clone, Debug, PartialEq)]
pub struct KeyInput {
    /// 逻辑按键
    pub key: Key,
    /// 不算修饰键时的按键（Shift+/ 的这里是 `/`）；平台不提供时为 `None`，按 `key` 算
    pub base: Option<Key>,
    /// 物理键；平台不提供时为 `None`
    pub physical: Option<PhysicalKey>,
    /// 这个键输入的文字
    pub text: Option<SmolStr>,
    /// 此刻按着的修饰键
    pub modifiers: Modifiers,
    /// 按住不放时系统发的重复
    pub repeated: bool,
}

impl KeyInput {
    /// 只有按键和修饰键（没有文字、物理键）
    pub fn new(key: Key, modifiers: Modifiers) -> Self {
        Self { key, base: None, physical: None, text: None, modifiers, repeated: false }
    }
}

/// 输入法事件。组字时先来预编辑（可能多次），确定时先来一个空的预编辑再来提交
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ime {
    /// 输入法打开了
    Enabled,
    /// 预编辑文字（例如拼音 "ni"）和其中的插入点（字节范围）；空文字表示清掉预编辑
    Preedit(String, Option<(usize, usize)>),
    /// 提交的文字
    Commit(String),
    /// 输入法关掉了
    Disabled,
}

/// 文字输入的用途（[`crate::kinds::TextInputClient::purpose`]）：窗口按它设置系统的输入法
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImePurpose {
    /// 普通的文字
    #[default]
    Normal,
    /// 密码：不用输入法，请求系统的安全输入（别的程序读不到按键，见参考文档的平台支持）
    Password,
    /// 终端：输入法照常用，有的平台的屏幕键盘会多出终端用的键
    Terminal,
    /// 数字：有的平台的屏幕键盘只显示数字
    Number,
}

/// 窗口要怎么设系统的文字输入（有输入目标要文字输入、有焦点时）：输入法候选框要避开的区域、用途
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImeRequest {
    /// 插入点（组字时是预编辑文字）的区域，窗口坐标，逻辑像素
    pub area: crate::geometry::Rect,
    /// 用途：密码不开输入法，请求安全输入
    pub purpose: ImePurpose,
}

/// 鼠标按键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MouseButton {
    /// 左键（主键）
    Left,
    /// 右键
    Right,
    /// 中键
    Middle,
    /// 后退键（侧键）
    Back,
    /// 前进键（侧键）
    Forward,
    /// 其他键，平台给的编号
    Other(u16),
}
