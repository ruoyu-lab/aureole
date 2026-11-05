//! Windows：隐藏的标题栏和窗口材质（Win32、DWM、DirectComposition）。
//!
//! **隐藏的标题栏**：窗口库的无边框窗口（`with_decorations(false)`、`with_undecorated_shadow(true)`）留着边框的样式、
//! 阴影和贴靠，把内容区铺满整个窗口，但不回答 `WM_NCHITTEST`，所以改不了大小。这里换掉窗口过程，挂一个窗口消息的子类
//! （只用 user32。comctl32 的子类函数在程序没带系统清单时启动就找不到入口，应用和测试程序都不一定带）：
//!
//! - 按 [`super::hit_test`] 回答 `WM_NCHITTEST`：窗口边缘是调整大小；应用自己画的最大化按钮（元素的 `window_maximize_button`）
//!   报告成 `HTMAXBUTTON`，Windows 11 在悬停时弹出贴靠布局；
//! - 报告成 `HTMAXBUTTON` 以后系统把这块当非客户区，指针消息变成 `WM_NC*`，窗口库收不到。把它们换成客户区的消息转给窗口库，
//!   元素照常收到悬停、按下、点击（最大化由应用的点击处理器做）；按下、松开不交给 `DefWindowProc`，不然系统按它自己的按钮处理。
//!
//! 拖动窗口、双击缩放不经过这里：运行时调用窗口库的 `drag_window`（发 `WM_NCLBUTTONDOWN`、`HTCAPTION`，系统的移动循环，带贴靠）。
//!
//! **材质**：`DWMWA_SYSTEMBACKDROP_TYPE`（Windows 11 22H2 起），把窗口框架扩到整个内容区（`DwmExtendFrameIntoClientArea`）才透得出来。
//! 窗口的交换链要能带透明：HWND 上的交换链只能不透明，所以有材质的窗口的表面建在 DirectComposition 的视觉对象上（[`composition_target`]），
//! 没有材质的窗口不碰这条路。

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::OnceLock;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Graphics::DirectComposition::{DCompositionCreateDevice2, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual};
use windows::core::Interface;
use windows_sys::core::{PCWSTR, w};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_SYSTEMBACKDROP_TYPE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWL_STYLE, GWLP_WNDPROC, GetClientRect, GetCursorPos, GetPropW, GetWindowLongPtrW, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTMAXBUTTON, HTRIGHT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, IsZoomed, RemovePropW, SM_CXPADDEDBORDER, SM_CXSIZEFRAME, SM_CYCAPTION, SM_CYSIZEFRAME, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    SetPropW, SetWindowLongPtrW, WNDPROC, WS_THICKFRAME,
};

use super::{ResizeEdge, TitleBarMetrics, WindowHit, WindowMaterial, hit_test};
use crate::PlatformError;
use crate::backend::CompositionTarget;

/// 子类的状态挂在窗口的这个属性上
const STATE_PROPERTY: PCWSTR = w!("AureoleTitleBarSubclass");
/// 鼠标消息的 `wparam` 里"左键按着"（`MK_LBUTTON`）
const MK_LBUTTON: WPARAM = 1;

/// 这台机器的系统版本号（`CurrentBuildNumber`），读不到时是 0
fn build_number() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| {
        let key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\0".encode_utf16().collect();
        let name: Vec<u16> = "CurrentBuildNumber\0".encode_utf16().collect();
        let mut buffer = [0u16; 16];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        // SAFETY: 键名和值名以 0 结尾；缓冲区的大小写在 `size` 里，系统不会写出界
        let status = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size)
        };
        if status != ERROR_SUCCESS {
            return 0;
        }
        String::from_utf16_lossy(&buffer).trim_end_matches('\0').trim().parse().unwrap_or(0)
    })
}

/// 有 `DWMWA_SYSTEMBACKDROP_TYPE`（Windows 11 22H2 起）
pub(crate) fn supports_backdrop() -> bool {
    build_number() >= 22621
}

/// 有贴靠布局（Windows 11 起）
pub(crate) fn supports_snap_layouts() -> bool {
    build_number() >= 22000
}

fn hwnd_of(window: &dyn HasWindowHandle) -> Result<HWND, PlatformError> {
    let handle = window.window_handle().map_err(|e| PlatformError::System(e.to_string().into()))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get() as HWND),
        _ => Err(PlatformError::System("不是 Win32 窗口".into())),
    }
}

/// 一个窗口的子类的状态。窗口消息都在窗口的线程（主线程）上
#[derive(Default)]
struct Subclass {
    /// 挂上之前的窗口过程（窗口库的）：自己不处理的消息都交给它
    previous: WNDPROC,
    /// 应用自己画的最大化按钮的范围：`[x, y, 宽, 高]`，内容区里的逻辑像素
    maximize_button: Cell<Option<[f32; 4]>>,
    /// 指针在最大化按钮上（系统把它当非客户区）：窗口库以为指针还在窗口里
    over_button: Cell<bool>,
}

/// 窗口上的子类的状态，没挂时是空指针。指针在 `WM_NCDESTROY` 之前一直有效
fn state_of(hwnd: HWND) -> *const Subclass {
    // SAFETY: 只读这个线程上的窗口的属性，名字以 0 结尾
    unsafe { GetPropW(hwnd, STATE_PROPERTY) }.cast_const().cast()
}

/// 窗口隐藏了标题栏：挂上子类。已经挂着时什么也不做
pub(crate) fn attach(window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window)?;
    if !state_of(hwnd).is_null() {
        return Ok(());
    }
    // SAFETY: `hwnd` 是这个线程上活着的窗口；窗口过程这个值就是这个函数指针类型，0 是 `None`
    let previous = unsafe { std::mem::transmute::<isize, WNDPROC>(GetWindowLongPtrW(hwnd, GWLP_WNDPROC)) };
    let state = Box::into_raw(Box::new(Subclass { previous, ..Subclass::default() }));
    // 属性先挂好：换了窗口过程以后，第一条消息就要读它
    // SAFETY: 同上；`state` 由子类在 `WM_NCDESTROY` 时放掉，名字以 0 结尾
    let installed = unsafe { SetPropW(hwnd, STATE_PROPERTY, state.cast()) != 0 && SetWindowLongPtrW(hwnd, GWLP_WNDPROC, subclass_proc as *const () as isize) != 0 };
    if !installed {
        // SAFETY: 没挂上，窗口过程没换，没有别人拿着 `state`
        unsafe {
            RemovePropW(hwnd, STATE_PROPERTY);
            drop(Box::from_raw(state));
        }
        return Err(PlatformError::System("挂不上窗口消息的子类".into()));
    }
    Ok(())
}

/// 应用自己画的最大化按钮的范围变了（逻辑像素）
pub(crate) fn set_maximize_button_area(window: &dyn HasWindowHandle, area: Option<[f32; 4]>) {
    let Ok(hwnd) = hwnd_of(window) else { return };
    // SAFETY: 见 `state_of`；窗口消息都在这个线程上，`Cell` 不跨线程
    if let Some(state) = unsafe { state_of(hwnd).as_ref() } {
        state.maximize_button.set(area);
    }
}

fn dpi_of(hwnd: HWND) -> u32 {
    // SAFETY: `hwnd` 是活着的窗口
    unsafe { GetDpiForWindow(hwnd) }.max(96)
}

/// 窗口边框的宽度（物理像素）：系统给可以改大小的窗口的边框
fn border_of(hwnd: HWND) -> f32 {
    let dpi = dpi_of(hwnd);
    // SAFETY: 只读系统度量
    (unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) }) as f32
}

/// 标题栏的高度（系统的标题栏有多高）。Windows 上窗口按钮要应用自己画
pub(crate) fn metrics(window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
    let hwnd = hwnd_of(window).ok()?;
    let dpi = dpi_of(hwnd);
    // SAFETY: 只读系统度量
    let height = unsafe { GetSystemMetricsForDpi(SM_CYCAPTION, dpi) + GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    Some(TitleBarMetrics { height: height as f32 * 96.0 / dpi as f32, controls: None })
}

/// 屏幕坐标（消息的 `lparam` 是有符号的两个 16 位）换成内容区的物理像素
fn client_point(hwnd: HWND, lparam: LPARAM) -> POINT {
    let mut point = POINT { x: i32::from(lparam as u16 as i16), y: i32::from((lparam >> 16) as u16 as i16) };
    // SAFETY: `hwnd` 是活着的窗口
    unsafe { ScreenToClient(hwnd, &mut point) };
    point
}

/// 指针此刻在内容区里的位置
fn cursor_point(hwnd: HWND) -> POINT {
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口
    unsafe {
        GetCursorPos(&mut point);
        ScreenToClient(hwnd, &mut point);
    }
    point
}

/// 内容区的坐标放进客户区消息的 `lparam`
fn client_lparam(point: POINT) -> LPARAM {
    (((point.y as u16 as u32) << 16) | u32::from(point.x as u16)) as LPARAM
}

fn hit_at(hwnd: HWND, state: &Subclass, point: POINT) -> WindowHit {
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口；只读窗口的样式
    let (style, zoomed) = unsafe {
        GetClientRect(hwnd, &mut client);
        (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32, IsZoomed(hwnd) != 0)
    };
    let scale = dpi_of(hwnd) as f32 / 96.0;
    let button = state.maximize_button.get().map(|[x, y, w, h]| [x * scale, y * scale, w * scale, h * scale]);
    // 全屏（窗口库去掉了边框的样式）、不能改大小的窗口没有边框；最大化的窗口也不能拖着边改
    let framed = style & WS_THICKFRAME != 0 && !zoomed;
    hit_test([point.x as f32, point.y as f32], [client.right as f32, client.bottom as f32], border_of(hwnd), framed, button)
}

fn hit_code(hit: WindowHit) -> u32 {
    match hit {
        WindowHit::Client => HTCLIENT,
        WindowHit::MaximizeButton => HTMAXBUTTON,
        WindowHit::Edge(ResizeEdge::Top) => HTTOP,
        WindowHit::Edge(ResizeEdge::Bottom) => HTBOTTOM,
        WindowHit::Edge(ResizeEdge::Left) => HTLEFT,
        WindowHit::Edge(ResizeEdge::Right) => HTRIGHT,
        WindowHit::Edge(ResizeEdge::TopLeft) => HTTOPLEFT,
        WindowHit::Edge(ResizeEdge::TopRight) => HTTOPRIGHT,
        WindowHit::Edge(ResizeEdge::BottomLeft) => HTBOTTOMLEFT,
        WindowHit::Edge(ResizeEdge::BottomRight) => HTBOTTOMRIGHT,
    }
}

/// 要系统在指针离开这个窗口的某一块时发消息
fn track_leave(hwnd: HWND, flags: u32) {
    let mut track = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: flags, hwndTrack: hwnd, dwHoverTime: 0 };
    // SAFETY: 结构在栈上，大小填对了；`hwnd` 是活着的窗口
    unsafe { TrackMouseEvent(&mut track) };
}

/// 子类的窗口过程，见模块说明
///
/// # Safety
/// 由系统调用；窗口属性里的 `Subclass` 是 [`attach`] 里挂的，`WM_NCDESTROY` 时放掉
unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: 见 `state_of`；窗口消息都在窗口的线程上
    let Some(state) = (unsafe { state_of(hwnd).as_ref() }) else {
        // 属性已经摘了：窗口要没了，这里不该再有消息，有的话交给系统的默认处理
        // SAFETY: 参数都是系统交来的
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    let previous = state.previous;
    let forward = |msg: u32, wparam: WPARAM, lparam: LPARAM| {
        // SAFETY: 交给挂上之前的窗口过程（窗口库的）
        unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) }
    };
    match msg {
        WM_NCHITTEST => return hit_code(hit_at(hwnd, state, client_point(hwnd, lparam))) as LRESULT,
        WM_NCMOUSEMOVE if wparam == HTMAXBUTTON as WPARAM => {
            if !state.over_button.replace(true) {
                track_leave(hwnd, TME_LEAVE | TME_NONCLIENT);
            }
            forward(WM_MOUSEMOVE, 0, client_lparam(client_point(hwnd, lparam)));
            // 贴靠布局靠系统看到这个消息：移动照常往下交（按下、松开不交）
            return forward(msg, wparam, lparam);
        }
        // 第二下按下也算按下：双击由元素的处理器决定，不让系统按它的最大化按钮处理
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == HTMAXBUTTON as WPARAM => {
            forward(WM_LBUTTONDOWN, MK_LBUTTON, client_lparam(client_point(hwnd, lparam)));
            return 0;
        }
        WM_NCLBUTTONUP if wparam == HTMAXBUTTON as WPARAM => {
            forward(WM_LBUTTONUP, 0, client_lparam(client_point(hwnd, lparam)));
            return 0;
        }
        // 指针从内容移到按钮上：客户区算离开了，但窗口库该以为指针还在窗口里
        WM_MOUSELEAVE if hit_at(hwnd, state, cursor_point(hwnd)) == WindowHit::MaximizeButton => return 0,
        WM_NCMOUSELEAVE if state.over_button.replace(false) => {
            // 从按钮移回内容：接着盯着客户区；移出了窗口：告诉窗口库
            let point = cursor_point(hwnd);
            let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口
            unsafe { GetClientRect(hwnd, &mut client) };
            if point.x >= 0 && point.y >= 0 && point.x < client.right && point.y < client.bottom {
                track_leave(hwnd, TME_LEAVE);
            } else {
                forward(WM_MOUSELEAVE, 0, 0);
            }
            return 0;
        }
        WM_NCDESTROY => {
            let data = std::ptr::from_ref(state).cast_mut();
            // SAFETY: 窗口要没了：换回原来的窗口过程，之后不再有这个子类的消息；`data` 是 `attach` 里的 `Box`，
            // 下面不再用 `state`（`forward` 只拿着原来的窗口过程）
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous.map_or(0, |proc| proc as *const () as isize));
                RemovePropW(hwnd, STATE_PROPERTY);
                drop(Box::from_raw(data));
            }
            return forward(msg, wparam, lparam);
        }
        _ => {}
    }
    forward(msg, wparam, lparam)
}

/// 窗口背后的材质：系统的背景类型，框架扩到整个内容区才透得出来；不透明时都改回去
pub(crate) fn set_backdrop(window: &dyn HasWindowHandle, backdrop: WindowMaterial) -> Result<(), PlatformError> {
    if !supports_backdrop() {
        return Err(PlatformError::Unsupported("窗口材质要 Windows 11 22H2 起".into()));
    }
    let hwnd = hwnd_of(window)?;
    let backdrop_type = backdrop.windows_backdrop();
    let inset = if backdrop.is_opaque() { 0 } else { -1 };
    let margins = MARGINS { cxLeftWidth: inset, cxRightWidth: inset, cyTopHeight: inset, cyBottomHeight: inset };
    // SAFETY: `hwnd` 是活着的窗口；属性的值是 4 字节的枚举，在栈上
    let status = unsafe {
        DwmExtendFrameIntoClientArea(hwnd, &margins);
        DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, std::ptr::from_ref(&backdrop_type).cast(), size_of::<i32>() as u32)
    };
    if status < 0 {
        return Err(PlatformError::System(format!("DwmSetWindowAttribute 失败：{status:#x}").into()));
    }
    Ok(())
}

/// 窗口上的一个 DirectComposition 视觉对象：能带透明的交换链要放在它上面。设备、目标留着，和窗口同生共死
struct DirectComposition {
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
}

impl CompositionTarget for DirectComposition {
    fn visual(&self) -> *mut c_void {
        self.visual.as_raw()
    }

    fn commit(&self) -> Result<(), PlatformError> {
        // SAFETY: 设备在这个线程上建的
        unsafe { self.device.Commit() }.map_err(|e| PlatformError::System(format!("IDCompositionDevice::Commit 失败：{e}").into()))
    }
}

/// 给窗口建 DirectComposition 的目标和视觉对象
pub(crate) fn composition_target(window: &dyn HasWindowHandle) -> Result<Box<dyn CompositionTarget>, PlatformError> {
    let hwnd = windows::Win32::Foundation::HWND(hwnd_of(window)?);
    let system = |what: &str, e: windows::core::Error| PlatformError::System(format!("{what} 失败：{e}").into());
    // SAFETY: 窗口是这个线程上活着的窗口；设备不要 GPU 设备（交换链自己带着）
    unsafe {
        let device: IDCompositionDevice = DCompositionCreateDevice2(None).map_err(|e| system("DCompositionCreateDevice2", e))?;
        let target = device.CreateTargetForHwnd(hwnd, true).map_err(|e| system("CreateTargetForHwnd", e))?;
        let visual = device.CreateVisual().map_err(|e| system("CreateVisual", e))?;
        target.SetRoot(&visual).map_err(|e| system("SetRoot", e))?;
        Ok(Box::new(DirectComposition { device, _target: target, visual }))
    }
}
