//! macOS 上窗口和运行环境的几项服务（AppKit）：指针位置、首选语言、
//! 带到最前面、打包后的应用名、显示器的颜色能力、低电量模式、密码框的安全输入、投递到运行循环。由 [`crate::NativeBackend`] 调用。

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventType, NSView};
use objc2_foundation::{NSArray, NSProcessInfo, NSRunLoop, NSRunLoopCommonModes};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// 指针此刻在窗口内容区里的位置（逻辑像素，左上角为原点），也可能在窗口外（负数或超出大小）。
/// 拖放文件时窗口收不到指针移动事件，用它找出文件落在哪个元素上
pub fn pointer_position(window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
    MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let ns_window = view.window()?;
    let in_window = ns_window.convertPointFromScreen(NSEvent::mouseLocation());
    let point = view.convertPoint_fromView(in_window, None);
    let y = if view.isFlipped() { point.y } else { view.bounds().size.height - point.y };
    Some([point.x as f32, y as f32])
}

/// 窗口的内容视图（只在主线程上）
pub(crate) fn content_view(window: &dyn HasWindowHandle) -> Option<&NSView> {
    MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView
    Some(unsafe { handle.ns_view.cast().as_ref() })
}

/// 用户首选的语言，例如 `"zh-Hans-CN"`、`"en-US"`
pub fn preferred_language() -> Option<String> {
    objc2_foundation::NSLocale::preferredLanguages().firstObject().map(|language| language.to_string())
}

/// 把窗口带到最前面：取消隐藏应用、激活应用、还原最小化、成为主窗口。只在主线程上有效，返回是否做了
pub fn bring_to_front(window: &dyn HasWindowHandle) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Some(ns_window) = content_view(window).and_then(|view| view.window()) else { return false };
    let app = NSApplication::sharedApplication(mtm);
    app.unhide(None);
    if ns_window.isMiniaturized() {
        ns_window.deminiaturize(None);
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    ns_window.makeKeyAndOrderFront(None);
    true
}

/// 没打包成 .app 时（从终端直接运行可执行文件）把应用设成普通的前台应用并激活：不这样做时窗口出来了，
/// 系统却不把它当前台应用，拿不到键盘。打包后由系统按 Info.plist 管（可能是只在菜单栏的应用），不做。
/// 只在主线程上有效，返回做了没有
pub fn activate_if_unpackaged() -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    if objc2_foundation::NSBundle::mainBundle().bundleIdentifier().is_some() {
        return false;
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // macOS 14 起老的 activateIgnoringOtherApps 不再把应用带到前台，要用协作式的 activate；更早的系统没有 activate
    if app.respondsToSelector(sel!(activate)) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    // 已经显示出来的窗口里第一个成为主窗口，拿到键盘
    let windows = app.windows();
    if let Some(window) = (0..windows.count()).map(|i| windows.objectAtIndex(i)).find(|w| w.isVisible()) {
        window.makeKeyAndOrderFront(None);
    }
    true
}

/// 投递到主线程的运行循环（common 模式），这次系统事件处理完、运行循环下一次转起来时执行 `f`，不在窗口库的事件回调里。
/// 不在主线程时直接执行
pub fn run_outside_event_callback(f: Box<dyn FnOnce()>) {
    if MainThreadMarker::new().is_none() {
        f();
        return;
    }
    let f = Cell::new(Some(f));
    let block = RcBlock::new(move || {
        if let Some(f) = f.take() {
            f();
        }
    });
    // SAFETY: NSRunLoopCommonModes 是 Foundation 导出的不可变常量；上面确认了在主线程，块投递到的正是主线程的运行循环，
    // 闭包不会到别的线程上执行
    unsafe {
        let modes = NSArray::from_slice(&[NSRunLoopCommonModes]);
        NSRunLoop::mainRunLoop().performInModes_block(&modes, &block);
    }
}

/// 现在正在处理的系统事件是不是滚轮事件
pub fn handling_wheel_event() -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    NSApplication::sharedApplication(mtm).currentEvent().is_some_and(|event| event.r#type() == NSEventType::ScrollWheel)
}

/// 打包成 .app 时 Info.plist 里的应用名（CFBundleDisplayName，没有就用 CFBundleName）；没打包时是 `None`
pub fn bundle_name() -> Option<String> {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    bundle.bundleIdentifier()?;
    ["CFBundleDisplayName", "CFBundleName"].into_iter().find_map(|key| {
        let value = bundle.objectForInfoDictionaryKey(&objc2_foundation::NSString::from_str(key))?;
        let name = value.downcast::<objc2_foundation::NSString>().ok()?.to_string();
        (!name.is_empty()).then_some(name)
    })
}

/// 窗口所在显示器的颜色能力：能不能显示 Display P3，此刻和最多能比 SDR 白亮几倍（EDR 的余量）。
/// 只在主线程上、窗口在某个显示器上时有结果。窗口挪到另一个显示器、调亮度时会变，调用方隔一会儿再问一次
pub fn display_color(window: &dyn HasWindowHandle) -> Option<crate::DisplayColor> {
    let view = content_view(window)?;
    let ns_window = view.window()?;
    // SAFETY: NSWindow 的 screen 返回 NSScreen 或 nil；下面的选择器先用 respondsToSelector: 确认
    unsafe {
        let screen: Option<Retained<NSObject>> = msg_send![&*ns_window, screen];
        let screen = screen?;
        let responds = |selector| screen.respondsToSelector(selector);
        // NSDisplayGamutP3 = 2（macOS 10.12+）
        let wide_gamut: bool = responds(sel!(canRepresentDisplayGamut:)) && msg_send![&*screen, canRepresentDisplayGamut: 2isize];
        let current: f64 = if responds(sel!(maximumExtendedDynamicRangeColorComponentValue)) {
            msg_send![&*screen, maximumExtendedDynamicRangeColorComponentValue]
        } else {
            1.0
        };
        let potential: f64 = if responds(sel!(maximumPotentialExtendedDynamicRangeColorComponentValue)) {
            msg_send![&*screen, maximumPotentialExtendedDynamicRangeColorComponentValue]
        } else {
            current
        };
        let finite = |v: f64| if v.is_finite() { v.max(1.0) as f32 } else { 1.0 };
        let headroom = finite(current);
        Some(crate::DisplayColor { wide_gamut, headroom, potential_headroom: finite(potential).max(headroom) })
    }
}

/// 系统是否开着低电量模式。开着时 ProMotion 屏上的窗口画面会被限制在 60fps，
/// 即使显示器和显示链接都在 120Hz。
pub fn is_low_power_mode() -> bool {
    NSProcessInfo::processInfo().isLowPowerModeEnabled()
}

// 安全输入（Carbon 的 HIToolbox，系统自带）：系统按进程数打开的次数，开、关要成对
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
    fn IsSecureEventInputEnabled() -> u8;
}

/// 本进程打开着安全输入（系统按次数记，这里保证只开一次）
static SECURE_INPUT: AtomicBool = AtomicBool::new(false);

/// 打开、关上系统的安全输入（密码框有焦点、窗口在前面时打开）。已经是这个状态时什么都不做；返回系统接受了没有
pub fn set_secure_input(on: bool) -> bool {
    if SECURE_INPUT.swap(on, Ordering::SeqCst) == on {
        return true;
    }
    // SAFETY: 两个函数不带参数、只改系统里本进程的安全输入计数；上面的标记保证开、关成对，不会多关
    let status = unsafe { if on { EnableSecureEventInput() } else { DisableSecureEventInput() } };
    if status != 0 {
        SECURE_INPUT.store(!on, Ordering::SeqCst);
    }
    status == 0
}

/// 系统的安全输入此刻开着（本进程或别的进程打开的都算）
pub fn is_secure_input_enabled() -> bool {
    // SAFETY: 只读系统的状态，不带参数
    unsafe { IsSecureEventInputEnabled() != 0 }
}
