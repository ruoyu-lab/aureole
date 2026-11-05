//! 窗口事件 -> 输入事件：纯函数 [`translate`]，负责缩放（物理像素换成逻辑像素）、跟踪修饰键和按着的键、
//! 滚轮的行和像素、时间戳。整个框架只有这里（和 `app.rs`）认识窗口库的事件类型，元素树只收 [`InputEvent`]。
//!
//! 拖文件时窗口收不到指针移动，文件下面的位置由调用方向平台要了写进 [`TranslateState::file_cursor`]；
//! 一次拖进几个文件就有几个"放下"事件，攒在状态里，这一轮事件处理完由 [`TranslateState::take_drop`] 一起交出去。
//! 悬停去重：位置和上次交出去的一样时不再交（拖着文件停住不动时每帧都问一次位置，一次录制里多出几百条一样的悬停），
//! 布局、画面变了以后（文件下面的元素可能换了）调用方让下一次照交（[`TranslateState::retarget_file_hover`]）。
//!
//! 触摸屏的手指换成指针输入，设备是触摸，带上系统给的编号。系统还可能把同一次触摸再报成鼠标
//! （Windows 在手指抬起前后补发鼠标消息，窗口库没把"这是手指转来的"的标记转出来；X11 上第一根手指还会带着指针一起移动）：
//! 手指在屏幕上和抬起后不久（[`TOUCH_MUTES_MOUSE`]）的鼠标移动、按下不交出去，不然一次点按会点两下

use std::path::PathBuf;
use std::time::Duration;

use smallvec::SmallVec;
use smol_str::SmolStr;
use winit::event::{ElementState, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::keyboard::{Key as WinitKey, NamedKey as WinitNamedKey, PhysicalKey as WinitPhysicalKey};

use aureole_core::{
    Buttons, ExternalDataKinds, GestureInput, Ime, InputEvent, InputKind, Key, KeyInput, Modifiers, MouseButton, NamedKey, PhysicalKey, PointerId, PointerInput,
    GesturePhase, WheelDelta, WheelInput,
};

/// 手指抬起以后这么久之内的鼠标移动、按下不算数：是系统把这次触摸转成的鼠标消息（见模块说明）。
/// 真的鼠标在这之内也被挡一下，但手刚离开屏幕就要用鼠标的情况不会这么快
const TOUCH_MUTES_MOUSE: Duration = Duration::from_millis(500);

/// 一个窗口的换算状态：缩放、修饰键、指针位置、按着的指针键、拖文件
#[derive(Debug)]
pub(crate) struct TranslateState {
    /// 这次事件的时间戳（秒）：调用方在换算之前写好
    pub time: Duration,
    /// 拖文件时平台告诉的指针位置（逻辑像素）：调用方在换算文件事件、调用 [`Self::file_hover`]、[`Self::take_drop`] 之前写好
    pub file_cursor: Option<[f32; 2]>,
    scale: f64,
    modifiers: Modifiers,
    cursor: Option<[f32; 2]>,
    buttons: Buttons,
    file_hovering: bool,
    /// 最近一次交出去的文件悬停的位置（`Some(None)` 是交过"不知道位置"）；位置一样时不再交
    file_hovered_at: Option<Option<[f32; 2]>>,
    dropped: Vec<PathBuf>,
    /// 屏幕上的手指（系统给的编号）：只数的话丢一次抬起，鼠标就一直被挡着
    fingers: SmallVec<[u64; 4]>,
    /// 最后一根手指离开屏幕的时间
    lifted_at: Option<Duration>,
    /// 最近一次交出去的、别的应用拖来的内容的经过（位置、种类）；一样时不再交
    external_hovered_at: Option<([f32; 2], ExternalDataKinds)>,
}

impl TranslateState {
    /// 窗口此刻的缩放（逻辑像素 -> 物理像素）
    pub fn new(scale: f64) -> Self {
        Self {
            time: Duration::ZERO,
            file_cursor: None,
            scale,
            modifiers: Modifiers::empty(),
            cursor: None,
            buttons: Buttons::empty(),
            file_hovering: false,
            file_hovered_at: None,
            dropped: Vec::new(),
            fingers: SmallVec::new(),
            lifted_at: None,
            external_hovered_at: None,
        }
    }

    /// 交给系统拖动窗口了：系统的移动循环收走松开，这边再也等不到，按着的键都算松开了
    pub fn release_buttons(&mut self) {
        self.buttons = Buttons::empty();
    }

    /// 拖着文件在窗口上
    pub fn file_hovering(&self) -> bool {
        self.file_hovering
    }

    /// 拖着文件悬停时每帧调用：系统这时不发移动事件，按 [`Self::file_cursor`] 更新文件下面的元素。
    /// 位置和上次交出去的一样时是 `None`
    pub fn file_hover(&mut self) -> Option<InputEvent> {
        if !self.file_hovering {
            return None;
        }
        self.hover_at(self.file_cursor).map(|kind| InputEvent::new(self.time, kind))
    }

    /// 布局、画面变了：文件下面的元素可能换了，下一次悬停位置没变也照交（别的应用拖来的内容也一样）
    pub fn retarget_file_hover(&mut self) {
        self.file_hovered_at = None;
        self.external_hovered_at = None;
    }

    /// 别的应用拖来的内容经过 `position`：系统每隔一会儿问一次，位置和种类都和上次交出去的一样时是 `None`
    pub fn external_over(&mut self, position: [f32; 2], kinds: ExternalDataKinds) -> Option<InputEvent> {
        (self.external_hovered_at.replace((position, kinds)) != Some((position, kinds)))
            .then(|| InputEvent::new(self.time, InputKind::ExternalDragOver { position, kinds }))
    }

    /// 别的应用拖来的内容离开窗口：交过经过才要交离开
    pub fn external_left(&mut self) -> Option<InputEvent> {
        self.external_hovered_at.take().map(|_| InputEvent::new(self.time, InputKind::ExternalDragLeave))
    }

    /// 别的应用拖来的内容放下了：下一次拖放重新算起
    pub fn external_ended(&mut self) {
        self.external_hovered_at = None;
    }

    /// 文件悬停在 `at`：和上次交出去的位置一样时不再交
    fn hover_at(&mut self, at: Option<[f32; 2]>) -> Option<InputKind> {
        (self.file_hovered_at.replace(at) != Some(at)).then_some(InputKind::FileHover(at))
    }

    /// 这一轮事件放下的文件，一起交给 [`Self::file_cursor`] 处的元素；没有时为 `None`
    pub fn take_drop(&mut self) -> Option<InputEvent> {
        if self.dropped.is_empty() {
            return None;
        }
        self.file_hovering = false;
        self.file_hovered_at = None;
        let paths = std::mem::take(&mut self.dropped);
        Some(InputEvent::new(self.time, InputKind::FileDrop { position: self.file_cursor, paths }))
    }

    fn pointer(&self, button: Option<MouseButton>) -> PointerInput {
        PointerInput { position: self.cursor, button, buttons: self.buttons, modifiers: self.modifiers, ..PointerInput::mouse(None) }
    }

    /// 触摸屏上的一根手指
    fn touch(&mut self, touch: &winit::event::Touch) -> InputKind {
        let position = self.logical(touch.location.x, touch.location.y);
        let finger = PointerInput { modifiers: self.modifiers, ..PointerInput::touch(PointerId(touch.id), position) };
        let lifted = |state: &mut Self| {
            state.fingers.retain(|id| *id != touch.id);
            state.lifted_at = Some(state.time);
        };
        match touch.phase {
            TouchPhase::Started => {
                if !self.fingers.contains(&touch.id) {
                    self.fingers.push(touch.id);
                }
                InputKind::PointerDown(PointerInput { button: Some(MouseButton::Left), ..finger })
            }
            TouchPhase::Moved => InputKind::PointerMove(finger),
            TouchPhase::Ended => {
                lifted(self);
                InputKind::PointerUp(PointerInput { button: Some(MouseButton::Left), buttons: Buttons::empty(), ..finger })
            }
            TouchPhase::Cancelled => {
                lifted(self);
                InputKind::PointerCancel(PointerInput { buttons: Buttons::empty(), ..finger })
            }
        }
    }

    /// 现在的鼠标事件多半是系统从触摸转来的（见模块说明）
    fn mouse_muted(&self) -> bool {
        !self.fingers.is_empty() || self.lifted_at.is_some_and(|at| self.time.saturating_sub(at) < TOUCH_MUTES_MOUSE)
    }

    fn logical(&self, x: f64, y: f64) -> [f32; 2] {
        [(x / self.scale) as f32, (y / self.scale) as f32]
    }
}

/// 一个窗口事件换算成输入事件（和输入无关的窗口事件、暂不支持的事件换算成空的）。
/// 时间戳取 [`TranslateState::time`]；位置、滚动的像素按状态里的缩放换算；按键、指针事件带上状态里的修饰键
pub(crate) fn translate(event: &WindowEvent, state: &mut TranslateState) -> SmallVec<[InputEvent; 2]> {
    let mut out = SmallVec::new();
    let kind = match event {
        WindowEvent::Resized(size) => {
            let scale = state.scale as f32;
            let [w, h] = [size.width.max(1), size.height.max(1)];
            Some(InputKind::Resize { size: [w as f32 / scale, h as f32 / scale], scale_factor: scale })
        }
        // 缩放变了之后系统接着发一次改大小（带上新的缩放）
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            state.scale = *scale_factor;
            None
        }
        WindowEvent::Focused(focused) => {
            // 切到别的应用时可能收不到抬起：元素树那边也把手指都取消了
            if !focused && !state.fingers.is_empty() {
                state.fingers.clear();
                state.lifted_at = Some(state.time);
            }
            Some(InputKind::WindowFocus(*focused))
        }
        WindowEvent::CursorMoved { position, .. } => {
            state.cursor = Some(state.logical(position.x, position.y));
            (!state.mouse_muted()).then(|| InputKind::PointerMove(state.pointer(None)))
        }
        WindowEvent::CursorEntered { .. } => Some(InputKind::PointerEnter(state.pointer(None))),
        WindowEvent::CursorLeft { .. } => {
            state.cursor = None;
            Some(InputKind::PointerLeave(state.pointer(None)))
        }
        // 换成"滚动量"：正数让偏移变大（窗口库里正数表示内容往下、往右走）；触控板的位移是物理像素
        WindowEvent::MouseWheel { delta, phase, .. } => {
            let delta = match *delta {
                MouseScrollDelta::LineDelta(x, y) => WheelDelta::Lines([-x, -y]),
                MouseScrollDelta::PixelDelta(p) => WheelDelta::Pixels(state.logical(-p.x, -p.y)),
            };
            Some(InputKind::Wheel(WheelInput { delta, phase: gesture_phase(*phase), position: state.cursor, modifiers: state.modifiers }))
        }
        WindowEvent::MouseInput { state: pressed, button, .. } => {
            let button = mouse_button(*button);
            let pressed = *pressed == ElementState::Pressed;
            // 真的按着的键松开时照交（按下时没被挡的），不然这个键一直算按着
            if state.mouse_muted() && (pressed || !state.buttons.contains(Buttons::from_button(button))) {
                None
            } else {
                state.buttons.set(Buttons::from_button(button), pressed);
                let pointer = state.pointer(Some(button));
                Some(if pressed { InputKind::PointerDown(pointer) } else { InputKind::PointerUp(pointer) })
            }
        }
        WindowEvent::Touch(touch) => Some(state.touch(touch)),
        // 触控板手势：正值放大；旋转的正值是逆时针
        WindowEvent::PinchGesture { delta, phase, .. } => {
            Some(InputKind::Pinch(GestureInput { delta: *delta as f32, phase: gesture_phase(*phase), position: state.cursor }))
        }
        // 窗口库给的是角度，换成弧度
        WindowEvent::RotationGesture { delta, phase, .. } => {
            Some(InputKind::Rotate(GestureInput { delta: delta.to_radians(), phase: gesture_phase(*phase), position: state.cursor }))
        }
        WindowEvent::ModifiersChanged(modifiers) => {
            state.modifiers = from_winit_modifiers(modifiers.state());
            Some(InputKind::Modifiers(state.modifiers))
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let input = KeyInput {
                key: key(&event.logical_key),
                base: base_key(event).map(|k| key(&k)),
                physical: Some(physical_key(event.physical_key)),
                text: event.text.clone(),
                modifiers: state.modifiers,
                repeated: event.repeat,
            };
            Some(if event.state == ElementState::Pressed { InputKind::KeyDown(input) } else { InputKind::KeyUp(input) })
        }
        WindowEvent::Ime(event) => Some(InputKind::Ime(ime(event))),
        // 拖进几个文件，系统就发几次：位置一样的只交一次
        WindowEvent::HoveredFile(_) => {
            state.file_hovering = true;
            state.hover_at(state.file_cursor)
        }
        WindowEvent::HoveredFileCancelled => {
            state.file_hovering = false;
            state.file_hovered_at = None;
            Some(InputKind::FileCancel)
        }
        WindowEvent::DroppedFile(path) => {
            state.dropped.push(path.clone());
            None
        }
        // 平移和双击手势、侧键之外的输入设备：暂不支持
        _ => None,
    };
    if let Some(kind) = kind {
        out.push(InputEvent::new(state.time, kind));
    }
    out
}

fn gesture_phase(phase: winit::event::TouchPhase) -> GesturePhase {
    match phase {
        winit::event::TouchPhase::Started => GesturePhase::Start,
        winit::event::TouchPhase::Moved => GesturePhase::Move,
        winit::event::TouchPhase::Ended => GesturePhase::End,
        winit::event::TouchPhase::Cancelled => GesturePhase::Cancel,
    }
}

fn mouse_button(button: winit::event::MouseButton) -> MouseButton {
    match button {
        winit::event::MouseButton::Left => MouseButton::Left,
        winit::event::MouseButton::Right => MouseButton::Right,
        winit::event::MouseButton::Middle => MouseButton::Middle,
        winit::event::MouseButton::Back => MouseButton::Back,
        winit::event::MouseButton::Forward => MouseButton::Forward,
        winit::event::MouseButton::Other(n) => MouseButton::Other(n),
    }
}

fn from_winit_modifiers(state: winit::keyboard::ModifiersState) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    modifiers.set(Modifiers::SHIFT, state.shift_key());
    modifiers.set(Modifiers::CONTROL, state.control_key());
    modifiers.set(Modifiers::ALT, state.alt_key());
    modifiers.set(Modifiers::SUPER, state.super_key());
    modifiers
}

fn key(key: &WinitKey) -> Key {
    match key {
        WinitKey::Named(named) => named_key(*named).map_or(Key::Unidentified, Key::Named),
        WinitKey::Character(c) => Key::Character(c.clone()),
        WinitKey::Dead(c) => Key::Dead(*c),
        WinitKey::Unidentified(_) => Key::Unidentified,
    }
}

macro_rules! named_key_from_winit {
    ($($(#[$doc:meta])* $name:ident,)*) => {
        /// 清单里的键一一对应，清单外的是 `None`
        fn named_key(key: WinitNamedKey) -> Option<NamedKey> {
            match key {
                $(WinitNamedKey::$name => Some(NamedKey::$name),)*
                _ => None,
            }
        }
    };
}
aureole_core::named_keys!(named_key_from_winit);

/// 物理键的名字：认得的键是键位名（`KeyA`、`ShiftLeft`），认不出的带上平台的编码
fn physical_key(key: WinitPhysicalKey) -> PhysicalKey {
    match key {
        WinitPhysicalKey::Code(code) => PhysicalKey::new(SmolStr::new(format!("{code:?}"))),
        WinitPhysicalKey::Unidentified(native) => PhysicalKey::new(SmolStr::new(format!("Unidentified:{native:?}"))),
    }
}

/// 不算修饰键时的按键（快捷键按它匹配：Shift+/ 按出来的是 `?`）
fn base_key(event: &winit::event::KeyEvent) -> Option<WinitKey> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        Some(event.key_without_modifiers())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = event;
        None
    }
}

fn ime(event: &winit::event::Ime) -> Ime {
    match event {
        winit::event::Ime::Enabled => Ime::Enabled,
        winit::event::Ime::Preedit(text, cursor) => Ime::Preedit(text.clone(), *cursor),
        winit::event::Ime::Commit(text) => Ime::Commit(text.clone()),
        winit::event::Ime::Disabled => Ime::Disabled,
    }
}
