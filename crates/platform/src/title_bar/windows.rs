//! Windows：隐藏的标题栏和窗口材质（Win32、DWM、DirectComposition）。
//!
//! **隐藏的标题栏**：窗口库的无边框窗口（`with_decorations(false)`、`with_undecorated_shadow(true)`）留着标题栏和边框的样式
//! （贴靠、`Win+方向键`、最小化和最大化的动画、阴影和圆角都靠它），但它自己的 `WM_NCCALCSIZE` 把内容区铺满整个窗口：窗口外面没有
//! 改大小的边框，上边空出 1 像素的线、最下面 1 像素在窗口外面，从最大化进的全屏还按最大化收进工作区。这里换掉窗口过程，挂一个窗口消息的子类
//! （只用 user32。comctl32 的子类函数在程序没带系统清单时启动就找不到入口，应用和测试程序都不一定带）：
//!
//! - 自己回答 `WM_NCCALCSIZE`，不交给窗口库：左、右、下留着系统的边框（Windows 10 起看不见，是窗口外面那一圈拖着改大小的地方，
//!   和别的窗口一样），上边不留（[`super::framed_client`]）；最大化时是显示器的工作区，自动隐藏的任务栏那条边让出 2 像素
//!   （[`super::maximized_client`]）；全屏（窗口库去掉了边框的样式）时是整个窗口；
//! - 窗口库按"无边框的窗口内容区就是整个窗口"换算大小，三处补上边框，内容区还是应用要的大小：挂上时把窗口放大一圈边框，
//!   `WM_GETMINMAXINFO` 里窗口库设的最小、最大的大小，换 DPI 时窗口库定的新大小（`WM_DPICHANGED` 里的那次 `WM_WINDOWPOSCHANGING`）。
//!   应用改内容区大小（`request_inner_size`）时要调用方加上 [`frame_extra`]；
//! - `WM_NCHITTEST` 先问系统（边框上是改大小），内容区里按 [`super::hit_test`]：上边一条是改大小；应用自己画的最大化按钮
//!   （元素的 `window_maximize_button`）报告成 `HTMAXBUTTON`，Windows 11 在悬停时弹出贴靠布局；
//! - 报告成 `HTMAXBUTTON` 以后系统把这块当非客户区，指针消息变成 `WM_NC*`，窗口库收不到。把它们换成客户区的消息转给窗口库，
//!   元素照常收到悬停、按下、点击（最大化由应用的点击处理器做）；按下、松开不交给 `DefWindowProc`，不然系统按它自己的按钮处理。
//!
//! 拖动窗口、双击缩放不经过这里：运行时调用窗口库的 `drag_window`（发 `WM_NCLBUTTONDOWN`、`HTCAPTION`，系统的移动循环，带贴靠）。
//!
//! **材质**：`DWMWA_SYSTEMBACKDROP_TYPE`（Windows 11 22H2 起），把窗口框架扩到整个内容区（`DwmExtendFrameIntoClientArea`）才透得出来。
//! 隐藏了标题栏的窗口这时整个是"玻璃"：系统画在框架上的最小化、最大化、关闭按钮从内容透明的地方透出来，和应用自己画的叠在一起。
//! 系统只在窗口带着 `WS_SYSMENU` 时画它们，所以有材质时去掉这个样式（子类在 `WM_STYLECHANGING` 里挡着，窗口库重设样式时也不加回来）。
//! 它管着的几样由子类接过来：Alt+F4 发关闭，Alt+空格和任务栏上 Shift+右键（`WM_POPUPSYSTEMMENU`）自己弹窗口菜单
//! （[`show_system_menu`]）；最小化、最大化、贴靠和系统命令本身不靠它。
//! 材质默认按浅色画：深色要先在窗口上写 `DWMWA_USE_IMMERSIVE_DARK_MODE`（`set_dark`），改了深浅再设一次材质类型。
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
use windows_sys::Win32::Graphics::Dwm::{DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromRect, ScreenToClient};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent, VK_F4};
use windows_sys::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETAUTOHIDEBAREX, ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, EnableMenuItem, GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC, GetClientRect, GetCursorPos, GetPropW, GetSystemMenu,
    GetWindowLongPtrW, HTBORDER, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTMAXBUTTON, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsIconic,
    IsZoomed, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MINMAXINFO, PostMessageW, RemovePropW, SC_CLOSE, SC_KEYMENU, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE,
    SC_RESTORE, SC_SIZE, SM_CXPADDEDBORDER, SM_CYCAPTION, SM_CYSIZEFRAME, STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SetMenuDefaultItem, SetPropW, SetWindowLongPtrW, SetWindowPos, TPM_LEFTALIGN, TPM_RETURNCMD, TrackPopupMenu, WINDOWPOS,
    WM_DPICHANGED, WM_GETMINMAXINFO, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK,
    WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_STYLECHANGING, WM_SYSCOMMAND, WM_SYSKEYDOWN, WM_WINDOWPOSCHANGING, WNDPROC,
    WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME, WindowFromPoint,
};

use super::{Edges, ResizeEdge, TitleBarMetrics, WindowHit, WindowMaterial, framed_client, hit_test, maximized_client};
use crate::PlatformError;
use crate::backend::CompositionTarget;

/// 子类的状态挂在窗口的这个属性上
const STATE_PROPERTY: PCWSTR = w!("AureoleTitleBarSubclass");
/// 鼠标消息的 `wparam` 里"左键按着"（`MK_LBUTTON`）
const MK_LBUTTON: WPARAM = 1;
/// 外壳要窗口在这个屏幕位置弹窗口菜单（任务栏上 Shift+右键）。系统的头文件里没有这个名字
const WM_POPUPSYSTEMMENU: u32 = 0x0313;

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
    /// 窗口库在处理 `WM_DPICHANGED`：它接下来定的窗口大小没算边框，要补上
    dpi_changing: Cell<bool>,
    /// 不让系统画窗口按钮（有材质时）：去掉了 `WS_SYSMENU`，它管的几样由子类做
    no_system_menu: Cell<bool>,
}

/// 窗口上的子类的状态，没挂时是空指针。指针是 [`attach`] 里 `Box::into_raw` 的，在 `WM_NCDESTROY` 之前一直有效
fn state_of(hwnd: HWND) -> *const Subclass {
    // SAFETY: 只读这个线程上的窗口的属性，名字以 0 结尾
    unsafe { GetPropW(hwnd, STATE_PROPERTY) }.cast_const().cast()
}

/// 窗口隐藏了标题栏：挂上子类，系统按它重新算内容区；没有最大化、全屏时窗口放大一圈边框，内容区还是窗口库建窗口时的大小
/// （应用要的大小）。已经挂着时什么也不做
pub(crate) fn attach(window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window)?;
    if !state_of(hwnd).is_null() {
        return Ok(());
    }
    // 窗口库按"内容区就是整个窗口"定的大小：换成自己的内容区以后保持这个大小
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: 写进栈上的结构；`hwnd` 是这个线程上活着的窗口
    unsafe { GetClientRect(hwnd, &mut client) };
    // SAFETY: 同上；窗口过程这个值就是这个函数指针类型，0 是 `None`
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
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE;
    // 位置不动：窗口的位置（`outer_position`）和别的窗口一样含着左边看不见的边框，存下的位置下次放回去不会漂
    let (flags, [width, height]) = match frame_extra(window) {
        Some([width, height]) => (flags, [client.right + width as i32, client.bottom + height as i32]),
        None => (flags | SWP_NOSIZE, [0, 0]),
    };
    // SAFETY: `hwnd` 是这个线程上活着的窗口，不改层次
    unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, width, height, flags) };
    Ok(())
}

/// 隐藏了标题栏的窗口在内容区外面还有多宽的系统边框：左右两条加起来、下边一条（物理像素）。窗口库按"无边框的窗口内容区就是整个窗口"
/// 换算 `request_inner_size`，要内容区是某个大小时把这个加上再要。没挂子类、最大化、全屏时是 `None`
pub(crate) fn frame_extra(window: &dyn HasWindowHandle) -> Option<[u32; 2]> {
    let hwnd = hwnd_of(window).ok()?;
    if state_of(hwnd).is_null() {
        return None;
    }
    extra_of(hwnd).map(|[width, height]| [width as u32, height as u32])
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

/// 窗口的样式和扩展样式
fn styles_of(hwnd: HWND) -> (u32, u32) {
    // SAFETY: 只读活着的窗口的样式
    unsafe { (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32, GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) }
}

/// 窗口还带着标题栏的样式：窗口库全屏时把边框的样式都去掉了
fn has_caption(style: u32) -> bool {
    style & WS_CAPTION == WS_CAPTION
}

fn is_zoomed(hwnd: HWND) -> bool {
    // SAFETY: `hwnd` 是活着的窗口
    unsafe { IsZoomed(hwnd) != 0 }
}

/// 系统给这个窗口此刻的样式的边框（物理像素，按窗口的 DPI）：左、上、右、下各多宽，上边含标题栏。全屏（没有标题栏的样式）时是 0
fn frame_of(hwnd: HWND) -> Edges {
    let (style, ex_style) = styles_of(hwnd);
    if !has_caption(style) {
        return [0; 4];
    }
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: 写进栈上的结构；样式是这个窗口此刻的，没有菜单栏（隐藏了标题栏的窗口不挂菜单栏）
    if unsafe { AdjustWindowRectExForDpi(&mut rect, style, 0, ex_style, dpi_of(hwnd)) } == 0 {
        return [0; 4];
    }
    [-rect.left, -rect.top, rect.right, rect.bottom]
}

/// 窗口库漏算的边框：内容区要多大，窗口就要比它宽左右两条边框、高下边一条边框（物理像素）。最大化、全屏时没有
fn extra_of(hwnd: HWND) -> Option<[i32; 2]> {
    if is_zoomed(hwnd) {
        return None;
    }
    let [left, _, right, bottom] = frame_of(hwnd);
    (left + right + bottom > 0).then_some([left + right, bottom])
}

/// 标题栏的高度（系统的标题栏有多高）。Windows 上窗口按钮要应用自己画
pub(crate) fn metrics(window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
    let hwnd = hwnd_of(window).ok()?;
    let dpi = dpi_of(hwnd);
    Some(TitleBarMetrics { height: caption_height(dpi) as f32 * 96.0 / dpi as f32, controls: None })
}

/// 系统的标题栏有多高（物理像素，按 `dpi`）
fn caption_height(dpi: u32) -> i32 {
    // SAFETY: 只读系统度量
    unsafe { GetSystemMetricsForDpi(SM_CYCAPTION, dpi) + GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) }
}

/// 在屏幕上的 `at` 弹出窗口菜单（还原、移动、大小、最小化、最大化、关闭），按窗口此刻的状态启用各项，选了的交给系统命令。
/// 和窗口库的 `show_window_menu` 一样（它也不靠 `WS_SYSMENU`）；菜单开着时是系统的模态循环，运行时照常出帧
fn show_system_menu(hwnd: HWND, at: POINT) {
    // SAFETY: `hwnd` 是这个线程上活着的窗口；系统菜单归窗口，不用释放
    let menu = unsafe { GetSystemMenu(hwnd, 0) };
    if menu.is_null() {
        return;
    }
    let (style, _) = styles_of(hwnd);
    // SAFETY: `hwnd` 是活着的窗口
    let (maximized, minimized) = (is_zoomed(hwnd), unsafe { IsIconic(hwnd) != 0 });
    let resizable = style & WS_THICKFRAME != 0;
    let enable = |command: u32, on: bool| {
        // SAFETY: 菜单是上面取到的系统菜单
        unsafe { EnableMenuItem(menu, command, MF_BYCOMMAND | if on { MF_ENABLED } else { MF_GRAYED }) };
    };
    enable(SC_RESTORE, maximized || minimized);
    enable(SC_MOVE, !maximized && !minimized);
    enable(SC_SIZE, resizable && !maximized && !minimized);
    enable(SC_MINIMIZE, style & WS_MINIMIZEBOX != 0 && !minimized);
    enable(SC_MAXIMIZE, style & WS_MAXIMIZEBOX != 0 && !maximized);
    enable(SC_CLOSE, true);
    // SAFETY: 同上；`TrackPopupMenu` 转一个模态循环，选好或者关上才返回，带着 `TPM_RETURNCMD` 时返回选了的命令
    unsafe {
        SetMenuDefaultItem(menu, SC_CLOSE, 0);
        let command = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN, at.x, at.y, 0, hwnd, std::ptr::null());
        if command != 0 {
            PostMessageW(hwnd, WM_SYSCOMMAND, command as WPARAM, 0);
        }
    }
}

/// 有材质时不让系统画窗口按钮（去掉 `WS_SYSMENU`），没有时还原。只管挂着子类（隐藏了标题栏）的窗口
fn hide_system_buttons(hwnd: HWND, hide: bool) {
    // SAFETY: 见 `state_of`
    let Some(state) = (unsafe { state_of(hwnd).as_ref() }) else { return };
    if state.no_system_menu.replace(hide) == hide {
        return;
    }
    let (style, _) = styles_of(hwnd);
    let style = if hide { style & !WS_SYSMENU } else { style | WS_SYSMENU };
    let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE;
    // SAFETY: `hwnd` 是这个线程上活着的窗口；改了样式要 `SWP_FRAMECHANGED` 系统才重画框架
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
        SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, flags);
    }
}

fn edges_of(rect: RECT) -> Edges {
    [rect.left, rect.top, rect.right, rect.bottom]
}

fn rect_of([left, top, right, bottom]: Edges) -> RECT {
    RECT { left, top, right, bottom }
}

/// 窗口矩形是 `window` 时的内容区，见模块说明
fn client_edges(hwnd: HWND, window: Edges) -> Edges {
    let (style, _) = styles_of(hwnd);
    // 全屏：内容区是整个窗口。从最大化进的全屏还带着最大化的样式，不能按最大化收进工作区
    if !has_caption(style) {
        return window;
    }
    let frame = frame_of(hwnd);
    if !is_zoomed(hwnd) {
        return framed_client(window, frame);
    }
    let bounds = rect_of(window);
    // SAFETY: 矩形在栈上
    let monitor = unsafe { MonitorFromRect(&bounds, MONITOR_DEFAULTTONULL) };
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: 写进栈上的结构，大小填对了；显示器句柄不是空的
    let found = !monitor.is_null() && unsafe { GetMonitorInfoW(monitor, &mut info) } != 0;
    let work = found.then(|| edges_of(info.rcWork));
    let autohide = if found { autohide_taskbars(info.rcMonitor) } else { [false; 4] };
    maximized_client(window, frame, work, autohide)
}

/// 自动隐藏的任务栏在这个显示器（`monitor` 是它的范围）的哪几条边上：左、上、右、下
fn autohide_taskbars(monitor: RECT) -> [bool; 4] {
    let mut query = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    // SAFETY: 结构在栈上，大小填对了
    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut query) } as u32;
    if state & ABS_AUTOHIDE == 0 {
        return [false; 4];
    }
    [ABE_LEFT, ABE_TOP, ABE_RIGHT, ABE_BOTTOM].map(|edge| {
        let mut query = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, uEdge: edge, rc: monitor, ..Default::default() };
        // SAFETY: 同上；系统按 `uEdge`、`rc` 找这个显示器这条边上自动隐藏的任务栏，有就返回它的窗口
        unsafe { SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut query) != 0 }
    })
}

/// 屏幕坐标（消息的 `lparam` 是有符号的两个 16 位）换成内容区的物理像素
fn client_point(hwnd: HWND, lparam: LPARAM) -> POINT {
    let mut point = POINT { x: i32::from(lparam as u16 as i16), y: i32::from((lparam >> 16) as u16 as i16) };
    // SAFETY: `hwnd` 是活着的窗口
    unsafe { ScreenToClient(hwnd, &mut point) };
    point
}

/// 内容区的坐标放进客户区消息的 `lparam`
fn client_lparam(point: POINT) -> LPARAM {
    (((point.y as u16 as u32) << 16) | u32::from(point.x as u16)) as LPARAM
}

/// 内容区里的点（物理像素）算哪一块
fn hit_at(hwnd: HWND, state: &Subclass, point: POINT) -> WindowHit {
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口
    unsafe { GetClientRect(hwnd, &mut client) };
    let (style, _) = styles_of(hwnd);
    let scale = dpi_of(hwnd) as f32 / 96.0;
    let button = state.maximize_button.get().map(|[x, y, w, h]| [x * scale, y * scale, w * scale, h * scale]);
    // 全屏（窗口库去掉了边框的样式）、不能改大小的窗口没有边框；最大化的窗口也不能拖着边改
    let framed = style & WS_THICKFRAME != 0 && !is_zoomed(hwnd);
    // 上边改大小的那一条和下边的边框一样宽
    let border = frame_of(hwnd)[3] as f32;
    hit_test([point.x as f32, point.y as f32], client.right as f32, border, framed, button)
}

/// 指针此刻在这个窗口的内容区里的哪一块。不在内容区里（窗口外面、系统的边框上、被别的窗口挡着，比如贴靠布局的浮层）时是 `None`
fn pointer_hit(hwnd: HWND, state: &Subclass) -> Option<WindowHit> {
    let mut point = POINT { x: 0, y: 0 };
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: 写进栈上的结构；`hwnd` 是活着的窗口。`WindowFromPoint` 可能给这个窗口发 `WM_NCHITTEST`，子类照常回答
    let on_window = unsafe { GetCursorPos(&mut point) != 0 && WindowFromPoint(point) == hwnd && ScreenToClient(hwnd, &mut point) != 0 && GetClientRect(hwnd, &mut client) != 0 };
    let inside = point.x >= 0 && point.y >= 0 && point.x < client.right && point.y < client.bottom;
    (on_window && inside).then(|| hit_at(hwnd, state, point))
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

/// `WM_NCHITTEST`：系统先按窗口矩形和内容区回答（左、右、下的边框上是改大小），内容区里按 [`hit_at`]
fn nc_hit_test(hwnd: HWND, state: &Subclass, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 窗口库不处理这条消息，直接问系统
    // SAFETY: 参数都是系统交来的
    let hit = unsafe { DefWindowProcW(hwnd, WM_NCHITTEST, wparam, lparam) };
    if hit == HTCLIENT as LRESULT {
        return hit_code(hit_at(hwnd, state, client_point(hwnd, lparam))) as LRESULT;
    }
    // 最大化时内容区外面只剩自动隐藏的任务栏那边让出的一条：不是改大小的地方
    if (HTLEFT as LRESULT..=HTBOTTOMRIGHT as LRESULT).contains(&hit) && is_zoomed(hwnd) {
        return HTBORDER as LRESULT;
    }
    hit
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
    let raw = state_of(hwnd);
    // SAFETY: 见 `state_of`；窗口消息都在窗口的线程上
    let Some(state) = (unsafe { raw.as_ref() }) else {
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
        WM_NCCALCSIZE => {
            // SAFETY: `wparam` 为 0 时 `lparam` 指向窗口矩形，不为 0 时指向 `NCCALCSIZE_PARAMS`，它的第一项也是窗口矩形；
            // 都在这次消息处理完之前有效。改成内容区，返回 0：系统照常保留原来的内容
            let rect = unsafe { &mut *(lparam as *mut RECT) };
            *rect = rect_of(client_edges(hwnd, edges_of(*rect)));
            return 0;
        }
        WM_NCHITTEST => return nc_hit_test(hwnd, state, wparam, lparam),
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            // SAFETY: `lparam` 指向系统交来的 `MINMAXINFO`，这次消息处理完之前一直有效
            let (min, max) = unsafe { ((*info).ptMinTrackSize, (*info).ptMaxTrackSize) };
            let result = forward(msg, wparam, lparam);
            // 窗口库改了的（应用设了最小、最大的内容区大小）是按"内容区就是整个窗口"算的：补上边框
            if let Some([width, height]) = extra_of(hwnd) {
                // SAFETY: 同上；窗口库已经处理完，这里独占
                let info = unsafe { &mut *info };
                if (info.ptMinTrackSize.x, info.ptMinTrackSize.y) != (min.x, min.y) {
                    info.ptMinTrackSize.x += width;
                    info.ptMinTrackSize.y += height;
                }
                if (info.ptMaxTrackSize.x, info.ptMaxTrackSize.y) != (max.x, max.y) {
                    info.ptMaxTrackSize.x += width;
                    info.ptMaxTrackSize.y += height;
                }
            }
            return result;
        }
        // 窗口库重设样式（进出全屏、改能不能缩放）时不把 `WS_SYSMENU` 加回来
        WM_STYLECHANGING if state.no_system_menu.get() && wparam as i32 == GWL_STYLE => {
            // SAFETY: 改的是样式时 `lparam` 指向系统交来的 `STYLESTRUCT`，这次消息处理完之前有效
            let change = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
            change.styleNew &= !WS_SYSMENU;
        }
        // 没有 `WS_SYSMENU` 时系统不管 Alt+F4：窗口库照常收到按键，之后发关闭（和系统做的一样，应用的关闭询问照常）
        WM_SYSKEYDOWN if state.no_system_menu.get() && wparam == WPARAM::from(VK_F4) => {
            let result = forward(msg, wparam, lparam);
            // SAFETY: 往这个窗口自己的消息队列放一条系统命令
            unsafe { PostMessageW(hwnd, WM_SYSCOMMAND, SC_CLOSE as WPARAM, 0) };
            return result;
        }
        // Alt+空格：系统要弹窗口菜单，没有 `WS_SYSMENU` 时它不弹，自己弹在内容区左上角、标题栏下面
        WM_SYSCOMMAND if state.no_system_menu.get() && wparam & 0xFFF0 == SC_KEYMENU as WPARAM && lparam == LPARAM::from(b' ') => {
            let mut point = POINT { x: 0, y: caption_height(dpi_of(hwnd)) };
            // SAFETY: `hwnd` 是活着的窗口，`point` 在栈上
            unsafe { ClientToScreen(hwnd, &mut point) };
            show_system_menu(hwnd, point);
            return 0;
        }
        // 任务栏上 Shift+右键：`lparam` 是屏幕坐标
        WM_POPUPSYSTEMMENU if state.no_system_menu.get() => {
            show_system_menu(hwnd, POINT { x: i32::from(lparam as i16), y: i32::from((lparam >> 16) as i16) });
            return 0;
        }
        WM_DPICHANGED => {
            state.dpi_changing.set(true);
            let result = forward(msg, wparam, lparam);
            // 窗口库处理的时候窗口可能没了：重新取
            // SAFETY: 见 `state_of`
            if let Some(state) = unsafe { state_of(hwnd).as_ref() } {
                state.dpi_changing.set(false);
            }
            return result;
        }
        // 窗口库换 DPI 时按内容区定了新的窗口大小（保持逻辑大小）：放大一圈边框（新 DPI 下的），内容区才是它要的大小。只补这一次
        WM_WINDOWPOSCHANGING if state.dpi_changing.get() => {
            // SAFETY: `lparam` 指向系统交来的 `WINDOWPOS`，这次消息处理完之前一直有效
            let pos = unsafe { &mut *(lparam as *mut WINDOWPOS) };
            if pos.flags & SWP_NOSIZE == 0 {
                state.dpi_changing.set(false);
                if let Some([width, height]) = extra_of(hwnd) {
                    pos.cx += width;
                    pos.cy += height;
                }
            }
        }
        WM_NCMOUSEMOVE if wparam == HTMAXBUTTON as WPARAM => {
            state.over_button.set(true);
            forward(WM_MOUSEMOVE, 0, client_lparam(client_point(hwnd, lparam)));
            // 每次都重新要，而且在转给窗口库之后：指针从窗口外面直接进到按钮上时，窗口库会改成盯着客户区
            // （一个线程只有一份），之后要的才算数
            track_leave(hwnd, TME_LEAVE | TME_NONCLIENT);
            // 贴靠布局靠系统看到这个消息：移动照常往下交（按下、松开不交）
            return forward(msg, wparam, lparam);
        }
        // 从按钮移到别的非客户区（上边改大小的那一条、系统的边框）：按钮上的悬停该收起
        WM_NCMOUSEMOVE if state.over_button.get() => {
            state.over_button.set(false);
            forward(WM_MOUSELEAVE, 0, 0);
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
        WM_MOUSELEAVE if pointer_hit(hwnd, state) == Some(WindowHit::MaximizeButton) => return 0,
        WM_NCMOUSELEAVE if state.over_button.get() => {
            match pointer_hit(hwnd, state) {
                // 还在按钮上：下一次移动会接着盯着
                Some(WindowHit::MaximizeButton) => {}
                // 从按钮移回内容：接着盯着客户区
                Some(WindowHit::Client) => {
                    state.over_button.set(false);
                    track_leave(hwnd, TME_LEAVE);
                }
                // 移出了窗口、到了改大小的地方、被别的窗口挡住（贴靠布局的浮层）：告诉窗口库
                _ => {
                    state.over_button.set(false);
                    forward(WM_MOUSELEAVE, 0, 0);
                }
            }
            return 0;
        }
        WM_NCDESTROY => {
            // SAFETY: 窗口要没了：换回原来的窗口过程，之后不再有这个子类的消息；`raw` 是 `attach` 里 `Box::into_raw` 的指针，
            // 从它放掉，下面不再用 `state`（`forward` 只拿着原来的窗口过程）
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous.map_or(0, |proc| proc as *const () as isize));
                RemovePropW(hwnd, STATE_PROPERTY);
                drop(Box::from_raw(raw.cast_mut()));
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
    // SAFETY: `hwnd` 是活着的窗口；边距在栈上
    let status = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
    if status < 0 {
        return Err(PlatformError::System(format!("DwmExtendFrameIntoClientArea 失败：{status:#x}").into()));
    }
    // SAFETY: 同上；属性的值是 4 字节的枚举，在栈上
    let status = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, std::ptr::from_ref(&backdrop_type).cast(), size_of::<i32>() as u32) };
    if status < 0 {
        return Err(PlatformError::System(format!("DwmSetWindowAttribute 失败：{status:#x}").into()));
    }
    // 隐藏了标题栏的窗口整个是玻璃：系统的窗口按钮会透出来，和应用自己画的叠在一起（见模块说明）
    hide_system_buttons(hwnd, !backdrop.is_opaque());
    Ok(())
}

/// 窗口的外框用深色还是浅色：写 `DWMWA_USE_IMMERSIVE_DARK_MODE`（标题栏、边框，和材质的底色）。材质默认按浅色画，
/// 这个标记要在设材质类型之前写；窗口已经有材质时，改了深浅要再设一次材质类型，颜色才换
pub(crate) fn set_dark(window: &dyn HasWindowHandle, dark: bool, backdrop: WindowMaterial) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window)?;
    let value = i32::from(dark);
    // SAFETY: `hwnd` 是活着的窗口；属性的值是 4 字节的布尔，在栈上
    let status = unsafe { DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, std::ptr::from_ref(&value).cast(), size_of::<i32>() as u32) };
    if status < 0 {
        return Err(PlatformError::System(format!("设不上深色的窗口外框：{status:#x}").into()));
    }
    if !backdrop.is_opaque() && supports_backdrop() {
        let backdrop_type = backdrop.windows_backdrop();
        // SAFETY: 同上，属性的值是 4 字节的枚举
        let status =
            unsafe { DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, std::ptr::from_ref(&backdrop_type).cast(), size_of::<i32>() as u32) };
        if status < 0 {
            return Err(PlatformError::System(format!("换了深浅以后重设材质失败：{status:#x}").into()));
        }
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
