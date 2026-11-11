//! Windows 上页面对话框的默认样子：`alert`、`confirm`、离开页面前的确认用系统的消息框，`prompt` 用一个小对话框
//! （提示文字、一个输入框、确定和取消，模板在内存里拼）。都在调用的线程上阻塞到用户关掉，所以由调用方放到单独的线程上；
//! 主人窗口（应用此刻的活动窗口；应用不在前台时是它自己的一个窗口）在对话框开着时不能操作。

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetActiveWindow, IsWindowEnabled, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    DLGTEMPLATE, DialogBoxIndirectParamW, EndDialog, EnumThreadWindows, GW_OWNER, GetDlgItem, GetDlgItemTextW, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    IsWindowVisible, MESSAGEBOX_STYLE, MessageBoxW, SendDlgItemMessageW, SetDlgItemTextW, SetWindowLongPtrW, WINDOW_LONG_PTR_INDEX,
};
use windows::core::PCWSTR;

/// 主线程上：对话框的主人。应用此刻的活动窗口；应用不在前台时（没有活动窗口）是主线程上第一个看得见、能操作的顶层窗口，
/// 不然对话框按屏幕居中、离应用的窗口很远。都没有时为 0。存成整数，好交给别的线程
pub(super) fn owner() -> isize {
    // SAFETY: 没有参数，只读调用线程的活动窗口
    let active = unsafe { GetActiveWindow() };
    if !active.is_invalid() {
        return active.0 as isize;
    }
    unsafe extern "system" fn first(window: HWND, found: LPARAM) -> windows::core::BOOL {
        // SAFETY: `found` 是下面 `owner` 里那个局部变量的地址，枚举期间一直有效；窗口句柄是系统给的
        unsafe {
            let unowned = GetWindow(window, GW_OWNER).is_err();
            if IsWindowVisible(window).as_bool() && IsWindowEnabled(window).as_bool() && unowned {
                *(found.0 as *mut isize) = window.0 as isize;
                return false.into();
            }
        }
        true.into()
    }
    let mut found = 0isize;
    // SAFETY: 回调只在枚举期间调用，写的是 `found`
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(first), LPARAM(&mut found as *mut isize as isize));
    }
    found
}

fn hwnd(owner: isize) -> Option<HWND> {
    (owner != 0).then_some(HWND(owner as *mut _))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// 系统的消息框：`confirm` 时有确定和取消（返回按了确定），否则只有确定
pub(super) fn message(owner: isize, title: &str, text: &str, confirm: bool) -> bool {
    const MB_OKCANCEL: u32 = 0x1;
    const MB_ICONQUESTION: u32 = 0x20;
    const MB_ICONINFORMATION: u32 = 0x40;
    const MB_SETFOREGROUND: u32 = 0x10000;
    const IDOK: i32 = 1;
    let style = if confirm { MB_OKCANCEL | MB_ICONQUESTION } else { MB_ICONINFORMATION } | MB_SETFOREGROUND;
    let (text, title) = (wide(text), wide(title));
    // SAFETY: 两个字符串以 0 结尾，调用期间一直活着
    let result = unsafe { MessageBoxW(hwnd(owner), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MESSAGEBOX_STYLE(style)) };
    result.0 == IDOK
}

/// 输入框的编号
const EDIT: i32 = 100;
const IDOK: usize = 1;
const IDCANCEL: usize = 2;

/// 对话框过程拿着的：一开始的文字、按了确定以后的文字
struct PromptState {
    default: Vec<u16>,
    result: RefCell<Option<String>>,
}

/// `DWLP_USER`：对话框留给自己用的那个指针的位置（消息结果、对话框过程之后）
const DWLP_USER: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX((std::mem::size_of::<isize>() * 2) as i32);

/// 带输入框的小对话框：按了确定返回输入的文字，取消、关掉是 `None`
pub(super) fn prompt(owner: isize, title: &str, text: &str, default: &str) -> Option<String> {
    let chinese = {
        // SAFETY: 没有参数
        let language = unsafe { GetUserDefaultUILanguage() };
        language & 0x3FF == 0x04
    };
    let (ok, cancel) = if chinese { ("确定", "取消") } else { ("OK", "Cancel") };
    let template = template(title, text, ok, cancel);
    let state = PromptState { default: wide(default), result: RefCell::new(None) };
    // SAFETY: 模板按 DLGTEMPLATE 的格式拼好、按 4 字节对齐，调用期间一直活着；`state` 在对话框关掉之前一直活着，
    // 对话框过程在这个线程上（DialogBoxIndirectParamW 阻塞到对话框关掉）
    unsafe {
        let instance = GetModuleHandleW(None).ok().map(Into::into);
        DialogBoxIndirectParamW(instance, template.as_ptr().cast::<DLGTEMPLATE>(), hwnd(owner), Some(prompt_proc), LPARAM(&state as *const PromptState as isize));
    }
    state.result.into_inner()
}

unsafe extern "system" fn prompt_proc(dialog: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> isize {
    const WM_INITDIALOG: u32 = 0x0110;
    const WM_COMMAND: u32 = 0x0111;
    const EM_SETSEL: u32 = 0x00B1;
    match message {
        WM_INITDIALOG => {
            // SAFETY: `lparam` 是 `prompt` 传进来的 `PromptState`，对话框开着时一直活着；记进对话框留给自己用的位置
            unsafe {
                SetWindowLongPtrW(dialog, DWLP_USER, lparam.0);
                let state = &*(lparam.0 as *const PromptState);
                let _ = SetDlgItemTextW(dialog, EDIT, PCWSTR(state.default.as_ptr()));
                SendDlgItemMessageW(dialog, EDIT, EM_SETSEL, WPARAM(0), LPARAM(-1));
                if let Ok(edit) = GetDlgItem(Some(dialog), EDIT) {
                    let _ = SetFocus(Some(edit));
                }
            }
            // 自己给了焦点
            0
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            if id == IDOK {
                // SAFETY: 留给自己用的位置在 WM_INITDIALOG 里存了 `PromptState`；读输入框的文字进足够长的缓冲区
                unsafe {
                    let state = &*(GetWindowLongPtrW(dialog, DWLP_USER) as *const PromptState);
                    let length = GetDlgItem(Some(dialog), EDIT).map(|edit| GetWindowTextLengthW(edit)).unwrap_or(0).max(0) as usize;
                    let mut buffer = vec![0u16; length + 1];
                    let read = GetDlgItemTextW(dialog, EDIT, &mut buffer) as usize;
                    *state.result.borrow_mut() = Some(String::from_utf16_lossy(&buffer[..read.min(length)]));
                    let _ = EndDialog(dialog, 1);
                }
                1
            } else if id == IDCANCEL {
                // SAFETY: 关掉自己
                let _ = unsafe { EndDialog(dialog, 0) };
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// 拼对话框的模板（对话框单位）：提示文字、输入框、确定、取消。格式见 DLGTEMPLATE、DLGITEMTEMPLATE
fn template(title: &str, text: &str, ok: &str, cancel: &str) -> Vec<u32> {
    const DS_SETFONT: u32 = 0x40;
    const DS_MODALFRAME: u32 = 0x80;
    const DS_CENTER: u32 = 0x0800;
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const WS_SYSMENU: u32 = 0x0008_0000;
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_BORDER: u32 = 0x0080_0000;
    const WS_TABSTOP: u32 = 0x0001_0000;
    const SS_NOPREFIX: u32 = 0x80;
    const ES_AUTOHSCROLL: u32 = 0x80;
    const BS_DEFPUSHBUTTON: u32 = 0x1;
    const BUTTON: u16 = 0x0080;
    const EDIT_CLASS: u16 = 0x0081;
    const STATIC: u16 = 0x0082;

    let mut words: Vec<u16> = Vec::new();
    let push32 = |words: &mut Vec<u16>, value: u32| words.extend([value as u16, (value >> 16) as u16]);
    let push_text = |words: &mut Vec<u16>, text: &str| words.extend(text.encode_utf16().chain(Some(0)));
    let align = |words: &mut Vec<u16>| {
        if words.len() % 2 == 1 {
            words.push(0);
        }
    };
    // 对话框：样式、扩展样式、控件数、位置大小，没有菜单、用默认的类，标题，字体
    push32(&mut words, DS_SETFONT | DS_MODALFRAME | DS_CENTER | WS_POPUP | WS_CAPTION | WS_SYSMENU);
    push32(&mut words, 0);
    words.extend([4, 0, 0, 280, 92, 0, 0]);
    push_text(&mut words, title);
    words.push(9);
    push_text(&mut words, "Segoe UI");
    let item = |words: &mut Vec<u16>, style: u32, rect: [u16; 4], id: u16, class: u16, text: &str| {
        align(words);
        push32(words, style | WS_CHILD | WS_VISIBLE);
        push32(words, 0);
        words.extend(rect);
        words.extend([id, 0xFFFF, class]);
        push_text(words, text);
        words.push(0);
    };
    item(&mut words, SS_NOPREFIX, [7, 7, 266, 38], 0xFFFF, STATIC, text);
    item(&mut words, WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL, [7, 49, 266, 14], EDIT as u16, EDIT_CLASS, "");
    item(&mut words, WS_TABSTOP | BS_DEFPUSHBUTTON, [169, 71, 50, 14], IDOK as u16, BUTTON, ok);
    item(&mut words, WS_TABSTOP, [223, 71, 50, 14], IDCANCEL as u16, BUTTON, cancel);
    align(&mut words);
    // 按 4 字节对齐地交出去
    words.chunks(2).map(|pair| u32::from(pair[0]) | (u32::from(pair.get(1).copied().unwrap_or(0)) << 16)).collect()
}
