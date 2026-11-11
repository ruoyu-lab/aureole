//! Windows：窗口的大小真正变之前，先按新大小出一帧（[`watch`]）。
//!
//! 系统改窗口大小（拖窗口的边、贴靠、最大化）是一次 `SetWindowPos`：先发 `WM_NCCALCSIZE` 问新的内容区多大，
//! 然后把新的窗口大小交给合成器，最后发 `WM_SIZE`。在 `WM_SIZE` 里才画的话，这中间的几毫秒（配置交换链、画、呈现）里
//! 合成器已经按新大小合成，翻转模型的交换链里还是旧画面，被系统拉伸到新大小：拖着窗口的边时画面在微微伸缩，看起来在抖。
//!
//! 这里给窗口挂一个子类，在 `WM_NCCALCSIZE` 里先让窗口原来的过程（窗口库、隐藏标题栏的子类）算出新的内容区，
//! 内容区的大小变了就回调运行时：它按这个大小出一帧、呈现、等 GPU 画完，然后才返回让系统去改。合成器看到的总是
//! 旧大小加旧画面，或者新大小加新画面。回调里再引起的改大小不再回调（[`BUSY`]）。
//!
//! 画面放在系统合成的对象上时（不拉伸，没赶上时窗口边上露出窗口自己的位图），位图用窗口的背景色填（[`set_background`]）：
//! `WM_ERASEBKGND` 里填，窗口的大小变了以后当场重填一次，露出来的那条和背景一样，不是黑的。

use std::cell::Cell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, HDC, RDW_ERASE, RDW_ERASENOW, RDW_INVALIDATE, RedrawWindow,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetClientRect, GetPropW, GetWindowLongPtrW, IsIconic, NCCALCSIZE_PARAMS, RemovePropW, SWP_NOSIZE,
    SetPropW, SetWindowLongPtrW, WINDOWPOS, WM_ERASEBKGND, WM_NCCALCSIZE, WM_NCDESTROY, WM_WINDOWPOSCHANGED, WNDPROC,
};
use windows_sys::core::{PCWSTR, w};

use crate::PlatformError;
use crate::backend::OnLiveResize;

/// 窗口属性的名字：子类的状态挂在这里
const STATE_PROPERTY: PCWSTR = w!("aureole.live_resize");

thread_local! {
    /// 回调正在跑：出帧时又引起的改大小（应用在帧里改了窗口大小）照常走，不再回调
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

struct Subclass {
    previous: WNDPROC,
    on_resize: OnLiveResize,
    /// 窗口自己的位图填什么颜色（`COLORREF`）；`None` 时不管，交给原来的过程
    background: Cell<Option<u32>>,
}

fn hwnd_of(window: &dyn HasWindowHandle) -> Result<HWND, PlatformError> {
    let handle = window.window_handle().map_err(|e| PlatformError::System(e.to_string().into()))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get() as HWND),
        _ => Err(PlatformError::System("不是 Win32 窗口".into())),
    }
}

/// 窗口上的子类的状态，没挂时是空指针。指针是 [`watch`] 里 `Box::into_raw` 的，在 `WM_NCDESTROY` 之前一直有效
fn state_of(hwnd: HWND) -> *const Subclass {
    // SAFETY: 只读这个线程上的窗口的属性，名字以 0 结尾
    unsafe { GetPropW(hwnd, STATE_PROPERTY) }.cast_const().cast()
}

/// 内容区的大小要变时先调用 `on_resize`（见模块说明）。已经挂着时什么也不做。窗口销毁时自己放掉
pub(crate) fn watch(window: &dyn HasWindowHandle, on_resize: OnLiveResize) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window)?;
    if !state_of(hwnd).is_null() {
        return Ok(());
    }
    // SAFETY: `hwnd` 是这个线程上活着的窗口；窗口过程这个值就是这个函数指针类型，0 是 `None`
    let previous = unsafe { std::mem::transmute::<isize, WNDPROC>(GetWindowLongPtrW(hwnd, GWLP_WNDPROC)) };
    let state = Box::into_raw(Box::new(Subclass { previous, on_resize, background: Cell::new(None) }));
    // 属性先挂好：换了窗口过程以后，第一条消息就要读它
    // SAFETY: 名字以 0 结尾；`state` 一直活到 `WM_NCDESTROY`；换上的是签名一致的窗口过程
    unsafe {
        if SetPropW(hwnd, STATE_PROPERTY, state.cast()) == 0 {
            drop(Box::from_raw(state));
            return Err(PlatformError::System("挂不上窗口属性".into()));
        }
        if SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize) == 0 {
            RemovePropW(hwnd, STATE_PROPERTY);
            drop(Box::from_raw(state));
            return Err(PlatformError::System("挂不上窗口消息的子类".into()));
        }
    }
    Ok(())
}

/// 窗口自己的位图用这个颜色填（8 位 sRGB）；`None` 不填。要先 [`watch`]，没挂子类时什么也不做
pub(crate) fn set_background(window: &dyn HasWindowHandle, color: Option<[u8; 3]>) {
    let Ok(hwnd) = hwnd_of(window) else { return };
    // SAFETY: 见 `state_of`；只在窗口的线程上调用
    let Some(state) = (unsafe { state_of(hwnd).as_ref() }) else { return };
    let color = color.map(colorref);
    if state.background.replace(color) != color {
        repaint(hwnd);
    }
}

/// 8 位 sRGB 换成 `COLORREF`（0x00bbggrr）
fn colorref([r, g, b]: [u8; 3]) -> u32 {
    u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16
}

/// 当场重填窗口的位图（发 `WM_ERASEBKGND`，不等 `WM_PAINT`）
fn repaint(hwnd: HWND) {
    // SAFETY: `hwnd` 是这个线程上活着的窗口；整个内容区
    unsafe { RedrawWindow(hwnd, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ERASENOW) };
}

/// 内容区的物理大小
fn size_of_rect(rect: &RECT) -> [u32; 2] {
    [(rect.right - rect.left).max(0) as u32, (rect.bottom - rect.top).max(0) as u32]
}

/// 系统新算出的内容区（`after`）和现在的（`before`）比，要不要先出一帧：大小变了，而且不是缩成 0（最小化）
fn needs_frame(before: [u32; 2], after: [u32; 2]) -> bool {
    after != before && after[0] > 0 && after[1] > 0
}

/// # Safety
/// 由系统调用；窗口属性里的 `Subclass` 是 [`watch`] 里挂的，`WM_NCDESTROY` 时放掉
unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let raw = state_of(hwnd);
    // SAFETY: 见 `state_of`；窗口消息都在窗口的线程上
    let Some(state) = (unsafe { raw.as_ref() }) else {
        // SAFETY: 参数都是系统交来的
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let previous = state.previous;
    match msg {
        WM_NCCALCSIZE if wparam != 0 => {
            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口
            unsafe { GetClientRect(hwnd, &mut client) };
            let before = size_of_rect(&client);
            // SAFETY: 参数都是系统交来的
            let result = unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) };
            // SAFETY: `wparam` 不为 0 时 `lparam` 指向 `NCCALCSIZE_PARAMS`，第一项此刻是新的内容区，这次消息处理完之前有效
            let after = size_of_rect(unsafe { &(*(lparam as *const NCCALCSIZE_PARAMS)).rgrc[0] });
            // SAFETY: 只读窗口的状态
            let minimized = unsafe { IsIconic(hwnd) } != 0;
            if needs_frame(before, after) && !minimized && !BUSY.with(Cell::get) {
                BUSY.with(|busy| busy.set(true));
                (state.on_resize)(after);
                BUSY.with(|busy| busy.set(false));
            }
            result
        }
        WM_ERASEBKGND if state.background.get().is_some() => {
            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: `wparam` 是系统交来的设备上下文；画刷用完就删
            unsafe {
                GetClientRect(hwnd, &mut client);
                let brush = CreateSolidBrush(state.background.get().unwrap_or_default());
                FillRect(wparam as HDC, &client, brush);
                DeleteObject(brush);
            }
            1
        }
        WM_WINDOWPOSCHANGED => {
            // SAFETY: 参数都是系统交来的
            let result = unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) };
            // 大小变了：新露出来的那块当场填上背景色，不等 `WM_PAINT`（拖着窗口的边时它要等到消息队列空了才来）
            // SAFETY: `lparam` 指向系统交来的 `WINDOWPOS`，这次消息处理完之前有效；窗口这时还活着
            let resized = unsafe { (*(lparam as *const WINDOWPOS)).flags } & SWP_NOSIZE == 0;
            // SAFETY: 窗口过程处理时窗口可能没了：重新取状态
            if resized && unsafe { state_of(hwnd).as_ref() }.is_some_and(|s| s.background.get().is_some()) {
                repaint(hwnd);
            }
            result
        }
        WM_NCDESTROY => {
            // 摘掉自己：窗口过程换回去、属性摘掉、状态放掉，再交给原来的过程
            // SAFETY: 窗口还活着；`raw` 是 `watch` 里 `Box::into_raw` 的，之后不再用
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous.map_or(0, |f| f as *const () as isize));
                RemovePropW(hwnd, STATE_PROPERTY);
                drop(Box::from_raw(raw.cast_mut()));
                CallWindowProcW(previous, hwnd, msg, wparam, lparam)
            }
        }
        // SAFETY: 参数都是系统交来的
        _ => unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) },
    }
}
