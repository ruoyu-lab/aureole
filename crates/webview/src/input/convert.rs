//! 输入换成 CEF 的约定：修饰键和按着的按键的标记、坐标、滚轮的方向和单位、光标形状、输入法的偏移。都是纯函数。
//!
//! 坐标：框架给输入目标和处理器的位置是元素自己的坐标（左上角是原点，逻辑像素，已经去掉了元素和祖先的变换）；
//! 网页铺满内容区，CEF 要的是网页里的逻辑像素（DIP，左上角是原点的整数），所以减掉内容区在元素里的偏移（内边距和边框）。
//! CEF 的 DIP 和框架的逻辑像素是同一个单位：网页的大小按内容区的逻辑大小、缩放按窗口缩放告诉 CEF。

use aureole_core::{Buttons, Cursor, Modifiers, MouseButton, Rect, WheelDelta};
use cef::sys::cef_cursor_type_t as CursorKind;
use cef::sys::cef_event_flags_t as Flag;

/// 换算按哪个平台的约定（按键的原生键码、macOS 的功能键字符）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Platform {
    /// macOS
    Mac,
    /// Windows
    Windows,
    /// Linux
    Linux,
}

impl Platform {
    /// 编译目标的平台
    pub(super) const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::Mac
    } else if cfg!(target_os = "windows") {
        Self::Windows
    } else {
        Self::Linux
    };
}

/// 标记的值：CEF 的绑定在 Windows 上是 `i32`，别的平台是 `u32`，位都一样
#[allow(clippy::unnecessary_cast, reason = "macOS、Linux 上本来就是 u32；Windows 上要从 i32 换过来")]
const fn flag(value: Flag0) -> u32 {
    value as u32
}

/// 绑定里标记的类型：Windows 上是 `c_int`，别的平台是 `c_uint`
#[cfg(windows)]
type Flag0 = std::os::raw::c_int;
#[cfg(not(windows))]
type Flag0 = std::os::raw::c_uint;

/// Shift
pub(super) const SHIFT: u32 = flag(Flag::EVENTFLAG_SHIFT_DOWN.0);
/// Control
pub(super) const CONTROL: u32 = flag(Flag::EVENTFLAG_CONTROL_DOWN.0);
/// Alt（Option）
pub(super) const ALT: u32 = flag(Flag::EVENTFLAG_ALT_DOWN.0);
/// Command（Windows 键、Super）
pub(super) const COMMAND: u32 = flag(Flag::EVENTFLAG_COMMAND_DOWN.0);
/// 按着左键
pub(super) const LEFT_BUTTON: u32 = flag(Flag::EVENTFLAG_LEFT_MOUSE_BUTTON.0);
/// 按着中键
pub(super) const MIDDLE_BUTTON: u32 = flag(Flag::EVENTFLAG_MIDDLE_MOUSE_BUTTON.0);
/// 按着右键
pub(super) const RIGHT_BUTTON: u32 = flag(Flag::EVENTFLAG_RIGHT_MOUSE_BUTTON.0);
/// 小键盘上的键
pub(super) const IS_KEY_PAD: u32 = flag(Flag::EVENTFLAG_IS_KEY_PAD.0);
/// 左边的修饰键
pub(super) const IS_LEFT: u32 = flag(Flag::EVENTFLAG_IS_LEFT.0);
/// 右边的修饰键
pub(super) const IS_RIGHT: u32 = flag(Flag::EVENTFLAG_IS_RIGHT.0);
/// 按住不放时的重复
pub(super) const IS_REPEAT: u32 = flag(Flag::EVENTFLAG_IS_REPEAT.0);
/// 滚动量是精确的像素（触控板），不是一格一格的
pub(super) const PRECISE: u32 = flag(Flag::EVENTFLAG_PRECISION_SCROLLING_DELTA.0);

/// 修饰键的标记
pub(super) fn modifier_flags(modifiers: Modifiers) -> u32 {
    [(Modifiers::SHIFT, SHIFT), (Modifiers::CONTROL, CONTROL), (Modifiers::ALT, ALT), (Modifiers::SUPER, COMMAND)]
        .into_iter()
        .filter(|(m, _)| modifiers.contains(*m))
        .fold(0, |flags, (_, flag)| flags | flag)
}

/// 按着的指针按键的标记：CEF 只有左、中、右
pub(super) fn button_flags(buttons: Buttons) -> u32 {
    [(Buttons::LEFT, LEFT_BUTTON), (Buttons::MIDDLE, MIDDLE_BUTTON), (Buttons::RIGHT, RIGHT_BUTTON)]
        .into_iter()
        .filter(|(b, _)| buttons.contains(*b))
        .fold(0, |flags, (_, flag)| flags | flag)
}

/// 框架的按键换成 CEF 的：只有左、中、右；侧键（后退、前进）网页收不到，由输入模块换成后退、前进
pub(super) fn mouse_button(button: MouseButton) -> Option<cef::MouseButtonType> {
    match button {
        MouseButton::Left => Some(cef::MouseButtonType::LEFT),
        MouseButton::Middle => Some(cef::MouseButtonType::MIDDLE),
        MouseButton::Right => Some(cef::MouseButtonType::RIGHT),
        _ => None,
    }
}

/// 元素坐标换成网页里的坐标（`inset` 是内容区在元素里的偏移）
pub(super) fn web_position(position: [f32; 2], inset: [f32; 2]) -> [f32; 2] {
    [position[0] - inset[0], position[1] - inset[1]]
}

/// 网页里的坐标换成 CEF 的整数 DIP：往下取整（落在哪个像素里），指针在网页外面（拖到外面）时是负数
pub(super) fn mouse_event(web: [f32; 2], flags: u32) -> cef::MouseEvent {
    cef::MouseEvent { x: web[0].floor() as i32, y: web[1].floor() as i32, modifiers: flags }
}

/// 一次滚轮换算的结果
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Wheel {
    /// 交给 CEF 的位移（整数 DIP）
    pub delta: [i32; 2],
    /// 不到一个 DIP 的零头，留到下一次（触控板的位移常常不到一个像素，丢掉的话慢慢划动不了）
    pub rest: [f32; 2],
    /// 是精确的像素（触控板）
    pub precise: bool,
}

/// 滚动量换成 CEF 的滚轮位移。方向相反：框架的正数让偏移变大（往下、往右滚，内容往上、往左走），
/// CEF 的正数是往上、往左滚。单位：滚轮按格的换成逻辑像素（`notch` 是一格多少，跟着系统设置），触控板本来就是逻辑像素。
/// `rest` 是上一次留下的零头
pub(super) fn wheel(delta: WheelDelta, notch: [f32; 2], rest: [f32; 2]) -> Wheel {
    let (pixels, precise) = match delta {
        WheelDelta::Lines([x, y]) => ([x * notch[0], y * notch[1]], false),
        WheelDelta::Pixels(pixels) => (pixels, true),
    };
    let total = [rest[0] - pixels[0], rest[1] - pixels[1]];
    // 往零取整：零头和位移同号，不会越积越多
    let whole = [total[0].trunc(), total[1].trunc()];
    Wheel { delta: [whole[0] as i32, whole[1] as i32], rest: [total[0] - whole[0], total[1] - whole[1]], precise }
}

/// CEF 要的光标形状换成框架的。框架没有的：隐藏（`NONE`）和自定义图片是箭头，单方向的改大小按它的轴，
/// 各种平移是四向滚动
pub(super) fn cursor(cursor: cef::CursorType) -> Cursor {
    match CursorKind::from(cursor) {
        CursorKind::CT_CROSS => Cursor::Crosshair,
        CursorKind::CT_HAND => Cursor::Pointer,
        CursorKind::CT_IBEAM => Cursor::Text,
        CursorKind::CT_VERTICALTEXT => Cursor::VerticalText,
        CursorKind::CT_WAIT => Cursor::Wait,
        CursorKind::CT_PROGRESS => Cursor::Progress,
        CursorKind::CT_HELP => Cursor::Help,
        CursorKind::CT_EASTRESIZE | CursorKind::CT_WESTRESIZE | CursorKind::CT_EASTWESTRESIZE => Cursor::ResizeHorizontal,
        CursorKind::CT_NORTHRESIZE | CursorKind::CT_SOUTHRESIZE | CursorKind::CT_NORTHSOUTHRESIZE => Cursor::ResizeVertical,
        CursorKind::CT_NORTHWESTRESIZE | CursorKind::CT_SOUTHEASTRESIZE | CursorKind::CT_NORTHWESTSOUTHEASTRESIZE => Cursor::ResizeDiagonal,
        CursorKind::CT_NORTHEASTRESIZE | CursorKind::CT_SOUTHWESTRESIZE | CursorKind::CT_NORTHEASTSOUTHWESTRESIZE => Cursor::ResizeAntiDiagonal,
        CursorKind::CT_COLUMNRESIZE => Cursor::ColumnResize,
        CursorKind::CT_ROWRESIZE => Cursor::RowResize,
        CursorKind::CT_MIDDLEPANNING
        | CursorKind::CT_MIDDLE_PANNING_VERTICAL
        | CursorKind::CT_MIDDLE_PANNING_HORIZONTAL
        | CursorKind::CT_EASTPANNING
        | CursorKind::CT_NORTHPANNING
        | CursorKind::CT_NORTHEASTPANNING
        | CursorKind::CT_NORTHWESTPANNING
        | CursorKind::CT_SOUTHPANNING
        | CursorKind::CT_SOUTHEASTPANNING
        | CursorKind::CT_SOUTHWESTPANNING
        | CursorKind::CT_WESTPANNING => Cursor::AllScroll,
        CursorKind::CT_MOVE => Cursor::Move,
        CursorKind::CT_CELL => Cursor::Cell,
        CursorKind::CT_CONTEXTMENU => Cursor::ContextMenu,
        CursorKind::CT_ALIAS | CursorKind::CT_DND_LINK => Cursor::Alias,
        CursorKind::CT_COPY | CursorKind::CT_DND_COPY => Cursor::Copy,
        CursorKind::CT_NODROP | CursorKind::CT_NOTALLOWED => Cursor::NotAllowed,
        CursorKind::CT_ZOOMIN => Cursor::ZoomIn,
        CursorKind::CT_ZOOMOUT => Cursor::ZoomOut,
        CursorKind::CT_GRAB => Cursor::Grab,
        CursorKind::CT_GRABBING => Cursor::Grabbing,
        _ => Cursor::Default,
    }
}

/// 字符串里字节偏移 `byte` 处是第几个 UTF-16 单元（CEF 的范围按 UTF-16 数）。不在字符边界上时往前退到边界
pub(super) fn utf16_offset(text: &str, byte: usize) -> u32 {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    text[..byte].encode_utf16().count() as u32
}

/// 没有范围（CEF 的 `InvalidRange`）：替换的范围给它表示替换正在组的字（没有时是选区）
pub(super) const NO_RANGE: cef::Range = cef::Range { from: u32::MAX, to: u32::MAX };

/// 组字的每个字的位置（网页里的 DIP）并成一个区域：输入法的候选框避开它。没有字时是 `None`
pub(super) fn composition_area(bounds: &[cef::Rect]) -> Option<Rect> {
    let first = bounds.first()?;
    let (mut left, mut top, mut right, mut bottom) = (first.x, first.y, first.x + first.width, first.y + first.height);
    for b in &bounds[1..] {
        left = left.min(b.x);
        top = top.min(b.y);
        right = right.max(b.x + b.width);
        bottom = bottom.max(b.y + b.height);
    }
    Some(Rect { x: left as f32, y: top as f32, width: (right - left) as f32, height: (bottom - top) as f32 })
}
