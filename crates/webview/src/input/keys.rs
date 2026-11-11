//! 按键编码：框架的按键换成 CEF 的按键事件（按下、松开、字符）。
//!
//! CEF 在每个平台都要 Windows 的虚拟键码（`windows_key_code`，网页里按键的 `keyCode` 按它算），另外要平台原生的键码
//! （`native_key_code`，网页里按键的 `code` 按它算）：macOS 是 kVK_* 键码；Windows 是按键消息的 lParam
//! （扫描码在 16 到 23 位，扩展键在 24 位）；Linux 是 XKB 键码（内核的键码加 8）。macOS 上 CEF 拿原生键码和字符
//! 造一个系统的按键事件再换算，字符和不算修饰键的字符都是 0 的当成只按了修饰键，所以方向键、功能键要给系统约定的
//! 功能键字符。
//!
//! 都是纯函数：输入是按键事件里的几项（[`Press`]），平台作为参数传进来，在哪个系统上都能测每个平台的结果。

use aureole_core::{Key, Modifiers, NamedKey};

use super::convert::{self, Platform};

/// 一次按键里换算要用的几项（从 [`aureole_core::KeyEvent`] 取，测试里直接写）
#[derive(Clone, Copy, Debug)]
pub(super) struct Press<'a> {
    /// 逻辑按键
    pub key: &'a Key,
    /// 不算修饰键时的按键
    pub base: &'a Key,
    /// 物理键的名字（`KeyA`、`ShiftLeft`）；平台不提供时为 `None`，按逻辑键猜美式键盘上的位置
    pub physical: Option<&'a str>,
    /// 这个键输入的文字（可能是控制字符）
    pub text: Option<&'a str>,
    /// 打出来的字（[`aureole_core::KeyEvent::typed_text`]）：有的才发字符事件
    pub typed: Option<&'a str>,
    /// 是组合键（[`aureole_core::KeyEvent::is_chord`]）
    pub chord: bool,
    /// 按着的修饰键
    pub modifiers: Modifiers,
    /// 按住不放时系统发的重复
    pub repeated: bool,
}

/// 按下还是松开
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stroke {
    /// 按下（CEF 的 RAWKEYDOWN）
    Down,
    /// 松开（KEYUP）
    Up,
}

/// 没有这种编码
const NONE: u16 = 0xFFFF;

/// 一个键位的几种编码：键位名（和物理键的名字一样）、Windows 虚拟键码（美式键盘上这个位置的键）、macOS 键码、
/// Windows 扫描码（扩展键带 0xE0 前缀）、Linux 的 XKB 键码。没有的是 [`NONE`]
type Codes = (&'static str, u16, u16, u16, u16);

#[rustfmt::skip]
const CODES: &[Codes] = &[
    // ---- 字母 ----
    ("KeyA", 0x41, 0x00, 0x1E, 38), ("KeyB", 0x42, 0x0B, 0x30, 56), ("KeyC", 0x43, 0x08, 0x2E, 54),
    ("KeyD", 0x44, 0x02, 0x20, 40), ("KeyE", 0x45, 0x0E, 0x12, 26), ("KeyF", 0x46, 0x03, 0x21, 41),
    ("KeyG", 0x47, 0x05, 0x22, 42), ("KeyH", 0x48, 0x04, 0x23, 43), ("KeyI", 0x49, 0x22, 0x17, 31),
    ("KeyJ", 0x4A, 0x26, 0x24, 44), ("KeyK", 0x4B, 0x28, 0x25, 45), ("KeyL", 0x4C, 0x25, 0x26, 46),
    ("KeyM", 0x4D, 0x2E, 0x32, 58), ("KeyN", 0x4E, 0x2D, 0x31, 57), ("KeyO", 0x4F, 0x1F, 0x18, 32),
    ("KeyP", 0x50, 0x23, 0x19, 33), ("KeyQ", 0x51, 0x0C, 0x10, 24), ("KeyR", 0x52, 0x0F, 0x13, 27),
    ("KeyS", 0x53, 0x01, 0x1F, 39), ("KeyT", 0x54, 0x11, 0x14, 28), ("KeyU", 0x55, 0x20, 0x16, 30),
    ("KeyV", 0x56, 0x09, 0x2F, 55), ("KeyW", 0x57, 0x0D, 0x11, 25), ("KeyX", 0x58, 0x07, 0x2D, 53),
    ("KeyY", 0x59, 0x10, 0x15, 29), ("KeyZ", 0x5A, 0x06, 0x2C, 52),
    // ---- 数字 ----
    ("Digit1", 0x31, 0x12, 0x02, 10), ("Digit2", 0x32, 0x13, 0x03, 11), ("Digit3", 0x33, 0x14, 0x04, 12),
    ("Digit4", 0x34, 0x15, 0x05, 13), ("Digit5", 0x35, 0x17, 0x06, 14), ("Digit6", 0x36, 0x16, 0x07, 15),
    ("Digit7", 0x37, 0x1A, 0x08, 16), ("Digit8", 0x38, 0x1C, 0x09, 17), ("Digit9", 0x39, 0x19, 0x0A, 18),
    ("Digit0", 0x30, 0x1D, 0x0B, 19),
    // ---- 空白、编辑 ----
    ("Enter", 0x0D, 0x24, 0x1C, 36), ("Escape", 0x1B, 0x35, 0x01, 9), ("Backspace", 0x08, 0x33, 0x0E, 22),
    ("Tab", 0x09, 0x30, 0x0F, 23), ("Space", 0x20, 0x31, 0x39, 65), ("Insert", 0x2D, 0x72, 0xE052, 118),
    ("Delete", 0x2E, 0x75, 0xE053, 119),
    // ---- 符号（美式键盘的位置） ----
    ("Minus", 0xBD, 0x1B, 0x0C, 20), ("Equal", 0xBB, 0x18, 0x0D, 21), ("BracketLeft", 0xDB, 0x21, 0x1A, 34),
    ("BracketRight", 0xDD, 0x1E, 0x1B, 35), ("Backslash", 0xDC, 0x2A, 0x2B, 51), ("Semicolon", 0xBA, 0x29, 0x27, 47),
    ("Quote", 0xDE, 0x27, 0x28, 48), ("Backquote", 0xC0, 0x32, 0x29, 49), ("Comma", 0xBC, 0x2B, 0x33, 59),
    ("Period", 0xBE, 0x2F, 0x34, 60), ("Slash", 0xBF, 0x2C, 0x35, 61), ("IntlBackslash", 0xE2, 0x0A, 0x56, 94),
    ("IntlRo", 0xC1, 0x5E, 0x73, 97), ("IntlYen", 0xDC, 0x5D, 0x7D, 132),
    // ---- 移动 ----
    ("ArrowLeft", 0x25, 0x7B, 0xE04B, 113), ("ArrowUp", 0x26, 0x7E, 0xE048, 111), ("ArrowRight", 0x27, 0x7C, 0xE04D, 114),
    ("ArrowDown", 0x28, 0x7D, 0xE050, 116), ("Home", 0x24, 0x73, 0xE047, 110), ("End", 0x23, 0x77, 0xE04F, 115),
    ("PageUp", 0x21, 0x74, 0xE049, 112), ("PageDown", 0x22, 0x79, 0xE051, 117),
    // ---- 修饰键：虚拟键码不分左右（左右由标记说明），Command 是 Windows 键 ----
    ("ShiftLeft", 0x10, 0x38, 0x2A, 50), ("ShiftRight", 0x10, 0x3C, 0x36, 62),
    ("ControlLeft", 0x11, 0x3B, 0x1D, 37), ("ControlRight", 0x11, 0x3E, 0xE01D, 105),
    ("AltLeft", 0x12, 0x3A, 0x38, 64), ("AltRight", 0x12, 0x3D, 0xE038, 108),
    ("MetaLeft", 0x5B, 0x37, 0xE05B, 133), ("MetaRight", 0x5C, 0x36, 0xE05C, 134),
    ("CapsLock", 0x14, 0x39, 0x3A, 66), ("Fn", NONE, 0x3F, NONE, NONE),
    // ---- 功能键 ----
    ("F1", 0x70, 0x7A, 0x3B, 67), ("F2", 0x71, 0x78, 0x3C, 68), ("F3", 0x72, 0x63, 0x3D, 69),
    ("F4", 0x73, 0x76, 0x3E, 70), ("F5", 0x74, 0x60, 0x3F, 71), ("F6", 0x75, 0x61, 0x40, 72),
    ("F7", 0x76, 0x62, 0x41, 73), ("F8", 0x77, 0x64, 0x42, 74), ("F9", 0x78, 0x65, 0x43, 75),
    ("F10", 0x79, 0x6D, 0x44, 76), ("F11", 0x7A, 0x67, 0x57, 95), ("F12", 0x7B, 0x6F, 0x58, 96),
    ("F13", 0x7C, 0x69, 0x64, 191), ("F14", 0x7D, 0x6B, 0x65, 192), ("F15", 0x7E, 0x71, 0x66, 193),
    ("F16", 0x7F, 0x6A, 0x67, 194), ("F17", 0x80, 0x40, 0x68, 195), ("F18", 0x81, 0x4F, 0x69, 196),
    ("F19", 0x82, 0x50, 0x6A, 197), ("F20", 0x83, 0x5A, 0x6B, 198), ("F21", 0x84, NONE, 0x6C, 199),
    ("F22", 0x85, NONE, 0x6D, 200), ("F23", 0x86, NONE, 0x6E, 201), ("F24", 0x87, NONE, 0x76, 202),
    ("PrintScreen", 0x2C, NONE, 0xE037, 107), ("ScrollLock", 0x91, NONE, 0x46, 78), ("Pause", 0x13, NONE, 0x45, 127),
    ("ContextMenu", 0x5D, 0x6E, 0xE05D, 135),
    // ---- 小键盘 ----
    ("NumLock", 0x90, 0x47, 0xE045, 77), ("NumpadDivide", 0x6F, 0x4B, 0xE035, 106),
    ("NumpadMultiply", 0x6A, 0x43, 0x37, 63), ("NumpadSubtract", 0x6D, 0x4E, 0x4A, 82),
    ("NumpadAdd", 0x6B, 0x45, 0x4E, 86), ("NumpadEnter", 0x0D, 0x4C, 0xE01C, 104),
    ("NumpadDecimal", 0x6E, 0x41, 0x53, 91), ("NumpadEqual", 0xBB, 0x51, 0x59, 125),
    ("NumpadComma", 0xC2, 0x5F, 0x7E, 129),
    ("Numpad0", 0x60, 0x52, 0x52, 90), ("Numpad1", 0x61, 0x53, 0x4F, 87), ("Numpad2", 0x62, 0x54, 0x50, 88),
    ("Numpad3", 0x63, 0x55, 0x51, 89), ("Numpad4", 0x64, 0x56, 0x4B, 83), ("Numpad5", 0x65, 0x57, 0x4C, 84),
    ("Numpad6", 0x66, 0x58, 0x4D, 85), ("Numpad7", 0x67, 0x59, 0x47, 79), ("Numpad8", 0x68, 0x5B, 0x48, 80),
    ("Numpad9", 0x69, 0x5C, 0x49, 81),
    // ---- 输入法、媒体 ----
    ("KanaMode", 0x15, NONE, 0x70, 101), ("Lang1", 0x15, 0x68, 0x72, 130), ("Lang2", 0x19, 0x66, 0x71, 131),
    ("Convert", 0x1C, NONE, 0x79, 100), ("NonConvert", 0x1D, NONE, 0x7B, 102),
    ("AudioVolumeMute", 0xAD, 0x4A, 0xE020, 121), ("AudioVolumeDown", 0xAE, 0x49, 0xE02E, 122),
    ("AudioVolumeUp", 0xAF, 0x48, 0xE030, 123), ("MediaTrackNext", 0xB0, NONE, 0xE019, 171),
    ("MediaTrackPrevious", 0xB1, NONE, 0xE010, 173), ("MediaStop", 0xB2, NONE, 0xE024, 174),
    ("MediaPlayPause", 0xB3, NONE, 0xE022, 172),
];

/// 键位名对应的编码
fn lookup(code: &str) -> Option<&'static Codes> {
    // 窗口库把 Command（Windows 键）叫 Super
    let code = match code {
        "SuperLeft" => "MetaLeft",
        "SuperRight" => "MetaRight",
        code => code,
    };
    CODES.iter().find(|c| c.0 == code)
}

/// 这次按键的键位：有物理键时按它，没有时按逻辑键（先看不算修饰键的）猜美式键盘上的位置
fn codes_of(press: &Press) -> Option<&'static Codes> {
    match press.physical {
        Some(code) => lookup(code),
        None => guess(press.base).or_else(|| guess(press.key)),
    }
}

fn guess(key: &Key) -> Option<&'static Codes> {
    match key {
        Key::Named(named) => lookup(match named {
            NamedKey::Shift => "ShiftLeft",
            NamedKey::Control => "ControlLeft",
            NamedKey::Alt => "AltLeft",
            NamedKey::AltGraph => "AltRight",
            NamedKey::Super | NamedKey::Meta => "MetaLeft",
            named => named.name(),
        }),
        Key::Character(_) => {
            let c = single_char(key)?;
            match c {
                'a'..='z' | 'A'..='Z' => lookup(&format!("Key{}", c.to_ascii_uppercase())),
                '0'..='9' => lookup(&format!("Digit{c}")),
                _ => lookup(match c {
                    ' ' => "Space",
                    '-' | '_' => "Minus",
                    '=' | '+' => "Equal",
                    '[' | '{' => "BracketLeft",
                    ']' | '}' => "BracketRight",
                    '\\' | '|' => "Backslash",
                    ';' | ':' => "Semicolon",
                    '\'' | '"' => "Quote",
                    '`' | '~' => "Backquote",
                    ',' | '<' => "Comma",
                    '.' | '>' => "Period",
                    '/' | '?' => "Slash",
                    _ => return None,
                }),
            }
        }
        Key::Dead(_) | Key::Unidentified => None,
    }
}

/// 只有一个字符的按键里的字符
fn single_char(key: &Key) -> Option<char> {
    let Key::Character(s) = key else { return None };
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// 字母、数字按布局打出来的字算虚拟键码（法语键盘上 A 的位置打出 Q，是 Q 键）
fn alphanumeric(key: &Key) -> Option<u16> {
    single_char(key).filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase() as u16)
}

/// Windows 的虚拟键码：小键盘按键位（数字锁定时它打出的也是数字，但不是主键盘上的数字键），字母和数字按打出来的字，
/// 别的按键位；都认不出时是 0
pub(super) fn windows_key_code(press: &Press) -> u16 {
    let codes = codes_of(press);
    let vk = |c: &Codes| (c.1 != NONE).then_some(c.1);
    if let Some(c) = codes
        && c.0.starts_with("Numpad")
    {
        return vk(c).unwrap_or(0);
    }
    alphanumeric(press.base).or_else(|| alphanumeric(press.key)).or_else(|| codes.and_then(vk)).unwrap_or(0)
}

/// 平台原生的键码，见模块的说明。认不出的键：macOS 是 0xFFFF（0 是 A 键），别的平台是 0
pub(super) fn native_key_code(press: &Press, stroke: Stroke, platform: Platform) -> i32 {
    let codes = codes_of(press);
    match platform {
        Platform::Mac => i32::from(codes.map_or(NONE, |c| c.2)),
        Platform::Windows => windows_lparam(codes.map_or(NONE, |c| c.3), stroke, press.repeated),
        Platform::Linux => i32::from(codes.map_or(0, |c| if c.4 == NONE { 0 } else { c.4 })),
    }
}

/// Windows 按键消息的 lParam：重复次数 1，扫描码在 16 到 23 位，扩展键（0xE0 前缀）在 24 位；
/// 按住重复时 30 位（之前就按着）是 1，松开时 30、31 位都是 1
fn windows_lparam(scan: u16, stroke: Stroke, repeated: bool) -> i32 {
    let scan = if scan == NONE { 0 } else { scan };
    let mut lparam = 1 | (u32::from(scan & 0xFF) << 16);
    if scan & 0xFF00 == 0xE000 {
        lparam |= 1 << 24;
    }
    match stroke {
        Stroke::Up => lparam |= 0b11 << 30,
        Stroke::Down if repeated => lparam |= 1 << 30,
        Stroke::Down => {}
    }
    lparam as i32
}

/// 这个键自己的字符（没有打出字的时候用）：回车、Tab、退格、Esc、空格的控制字符；macOS 上还有方向键、功能键这些
/// 系统约定的功能键字符（小键盘的回车是 0x03，退格是 0x7F）。修饰键是 0
fn named_character(key: NamedKey, keypad: bool, platform: Platform) -> u16 {
    match (key, platform) {
        (NamedKey::Enter, Platform::Mac) if keypad => 0x03,
        (NamedKey::Enter, _) => 0x0D,
        (NamedKey::Tab, _) => 0x09,
        (NamedKey::Backspace, Platform::Mac) => 0x7F,
        (NamedKey::Backspace, _) => 0x08,
        (NamedKey::Escape, _) => 0x1B,
        (NamedKey::Space, _) => 0x20,
        (key, Platform::Mac) => mac_function_character(key),
        _ => 0,
    }
}

/// macOS 的功能键字符（系统按键事件的字符里放的）
fn mac_function_character(key: NamedKey) -> u16 {
    match key {
        NamedKey::ArrowUp => 0xF700,
        NamedKey::ArrowDown => 0xF701,
        NamedKey::ArrowLeft => 0xF702,
        NamedKey::ArrowRight => 0xF703,
        NamedKey::Insert => 0xF727,
        NamedKey::Delete => 0xF728,
        NamedKey::Home => 0xF729,
        NamedKey::End => 0xF72B,
        NamedKey::PageUp => 0xF72C,
        NamedKey::PageDown => 0xF72D,
        NamedKey::PrintScreen => 0xF72E,
        NamedKey::ScrollLock => 0xF72F,
        NamedKey::Pause => 0xF730,
        NamedKey::ContextMenu => 0xF735,
        NamedKey::Clear => 0xF739,
        NamedKey::Help => 0xF746,
        // F1 到 F35 是连着的
        key => key.name().strip_prefix('F').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=35).contains(n)).map_or(0, |n| 0xF704 + n - 1),
    }
}

/// 字符串的第一个 UTF-16 单元
fn first_unit(text: &str) -> Option<u16> {
    text.encode_utf16().next()
}

/// 字符的第一个 UTF-16 单元
fn unit(c: char) -> u16 {
    c.encode_utf16(&mut [0; 2])[0]
}

/// 键的字符：打出来的字（可能是控制字符）的第一个 UTF-16 单元，没有时是按键上的字，再没有时是键自己的字符
fn character(press: &Press, keypad: bool, platform: Platform) -> u16 {
    let typed = press.text.filter(|t| !t.is_empty()).and_then(first_unit);
    typed.or_else(|| key_character(press.key, keypad, platform)).unwrap_or(0)
}

fn key_character(key: &Key, keypad: bool, platform: Platform) -> Option<u16> {
    match key {
        Key::Character(s) => first_unit(s),
        Key::Named(named) => Some(named_character(*named, keypad, platform)),
        Key::Dead(_) | Key::Unidentified => None,
    }
}

/// 不算修饰键（Shift 除外）时的字符：只按着 Shift 时就是逻辑键的字，按着别的修饰键时用不算修饰键的键（字母按 Shift 换成大写）。
/// 死键的字符是 0，这里还有键位上的字
fn unmodified_character(press: &Press, keypad: bool, platform: Platform) -> u16 {
    let others = press.modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
    let from_key = if others { None } else { key_character(press.key, keypad, platform) };
    from_key
        .or_else(|| match single_char(press.base) {
            Some(c) if press.modifiers.has_shift() && c.is_alphabetic() => c.to_uppercase().next().map(unit),
            _ => key_character(press.base, keypad, platform),
        })
        .unwrap_or(0)
}

/// 修饰键自己对应的修饰键标记
fn modifier_of(key: &Key) -> Option<Modifiers> {
    match key {
        Key::Named(NamedKey::Shift) => Some(Modifiers::SHIFT),
        Key::Named(NamedKey::Control) => Some(Modifiers::CONTROL),
        Key::Named(NamedKey::Alt) => Some(Modifiers::ALT),
        Key::Named(NamedKey::Super | NamedKey::Meta) => Some(Modifiers::SUPER),
        _ => None,
    }
}

/// 按着的修饰键，算上修饰键自己：按下时算按着、松开时不算（网页里 Shift 的按下事件里 Shift 是按着的）
fn effective_modifiers(press: &Press, stroke: Stroke) -> Modifiers {
    let mut modifiers = press.modifiers;
    if let Some(own) = modifier_of(press.key) {
        modifiers.set(own, stroke == Stroke::Down);
    }
    modifiers
}

/// 按键事件的标记：修饰键、小键盘、修饰键在左边还是右边、按住重复
fn key_flags(press: &Press, stroke: Stroke, codes: Option<&Codes>) -> u32 {
    let mut flags = convert::modifier_flags(effective_modifiers(press, stroke));
    if let Some(codes) = codes {
        if codes.0.starts_with("Numpad") {
            flags |= convert::IS_KEY_PAD;
        }
        if modifier_of(press.key).is_some() {
            if codes.0.ends_with("Left") {
                flags |= convert::IS_LEFT;
            } else if codes.0.ends_with("Right") {
                flags |= convert::IS_RIGHT;
            }
        }
    }
    if press.repeated && stroke == Stroke::Down {
        flags |= convert::IS_REPEAT;
    }
    flags
}

/// 按下（RAWKEYDOWN）或松开（KEYUP）的事件
pub(super) fn key_event(press: &Press, stroke: Stroke, platform: Platform) -> cef::KeyEvent {
    let codes = codes_of(press);
    let keypad = codes.is_some_and(|c| c.0.starts_with("Numpad"));
    let modifiers = effective_modifiers(press, stroke);
    cef::KeyEvent {
        type_: match stroke {
            Stroke::Down => cef::KeyEventType::RAWKEYDOWN,
            Stroke::Up => cef::KeyEventType::KEYUP,
        },
        modifiers: key_flags(press, stroke, codes),
        windows_key_code: i32::from(windows_key_code(press)),
        native_key_code: native_key_code(press, stroke, platform),
        // 系统键：Windows 上按着 Alt（不是 AltGr 那样连着 Ctrl）时的按键，Alt 自己的按下也是
        is_system_key: i32::from(platform == Platform::Windows && modifiers.has_alt() && !modifiers.has_control()),
        character: character(press, keypad, platform),
        unmodified_character: unmodified_character(press, keypad, platform),
        ..Default::default()
    }
}

/// 按下之后的字符事件（CHAR）：打出来的字每个 UTF-16 单元一个；回车（不是组合键时）是一个 `\r`，
/// 网页里的换行、提交表单靠它。虚拟键码在 Windows 上是这个字符（和系统的字符消息一样），别的平台和按下时一样
pub(super) fn char_events(press: &Press, platform: Platform) -> Vec<cef::KeyEvent> {
    let enter = matches!(press.key, Key::Named(NamedKey::Enter)) && !press.chord;
    let units: Vec<u16> = match press.typed {
        Some(text) => text.encode_utf16().collect(),
        None if enter => vec![0x0D],
        None => return Vec::new(),
    };
    let down = key_event(press, Stroke::Down, platform);
    units
        .into_iter()
        .map(|unit| cef::KeyEvent {
            type_: cef::KeyEventType::CHAR,
            windows_key_code: if platform == Platform::Windows { i32::from(unit) } else { down.windows_key_code },
            character: unit,
            unmodified_character: unit,
            ..down.clone()
        })
        .collect()
}

/// 编辑命令：macOS 上 CEF 离屏时不经过系统的菜单，⌘C、⌘V 这些网页自己不处理，要让网页的框架执行
/// （别的平台上网页自己按 Ctrl+C 这些执行）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edit {
    /// 撤销
    Undo,
    /// 重做
    Redo,
    /// 剪切
    Cut,
    /// 拷贝
    Copy,
    /// 粘贴
    Paste,
    /// 全选
    SelectAll,
}

/// 这次按键是不是 macOS 上的编辑命令：只按着 Command（重做再加 Shift）加 Z、X、C、V、A。
/// 按不算修饰键时的字认，认不出（别的文字的布局）时按键位
pub(super) fn edit_command(press: &Press, platform: Platform) -> Option<Edit> {
    let m = press.modifiers;
    if platform != Platform::Mac || !m.has_super() || m.intersects(Modifiers::CONTROL | Modifiers::ALT) {
        return None;
    }
    let letter = single_char(press.base)
        .filter(char::is_ascii_alphabetic)
        .or_else(|| press.physical.and_then(|p| p.strip_prefix("Key")).and_then(|k| k.chars().next()))?
        .to_ascii_lowercase();
    match (letter, m.has_shift()) {
        ('z', false) => Some(Edit::Undo),
        ('z', true) => Some(Edit::Redo),
        ('x', false) => Some(Edit::Cut),
        ('c', false) => Some(Edit::Copy),
        ('v', false) => Some(Edit::Paste),
        ('a', false) => Some(Edit::SelectAll),
        _ => None,
    }
}
