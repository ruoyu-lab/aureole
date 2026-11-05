//! macOS：隐藏的标题栏和窗口材质（AppKit）。
//!
//! **隐藏的标题栏**：内容铺满整个窗口、标题栏透明、不显示标题文字由窗口库的窗口属性做，这里管系统窗口按钮（红绿灯）。
//! AppKit 没有公开改按钮位置的接口，做法是改按钮和它们所在的标题栏容器的框架。AppKit 每次重新摆放标题栏
//! （改窗口大小的每一步、进出全屏、成为主窗口、改标题）都会把它们放回原位，所以盯着它们的框架变化：一动就在
//! 同一次布局里放回去，画面上看不到中间状态。设了位置时度量不读按钮此刻的框架（可能正好是被放回原位的那一刻），
//! 按设置和按钮的固定大小算，改窗口大小的途中也是同一个值。
//!
//! **材质**：一个 `NSVisualEffectView` 放在内容视图的最下面（透明的表面在它上面，透出它），`hitTest:` 返回空，
//! 指针事件照常交给内容视图。跟着窗口的前后台变化（`followsWindowActiveState`）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindow, NSWindowButton, NSWindowDidBecomeKeyNotification, NSWindowDidEndLiveResizeNotification,
    NSWindowDidEnterFullScreenNotification, NSWindowDidExitFullScreenNotification, NSWindowDidResignKeyNotification, NSWindowDidResizeNotification,
    NSViewFrameDidChangeNotification, NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSPoint, NSRect, NSString, NSUserDefaults};
use raw_window_handle::HasWindowHandle;

use super::{OnTitleBarChange, TitleBarAction, TitleBarMetrics, TitleBarOptions, WindowMaterial};
use crate::PlatformError;

define_class!(
    /// 不接指针事件的材质视图：`hitTest:` 返回空，窗口按它下面的内容视图算命中（点第一下就能点到内容）
    #[unsafe(super(NSVisualEffectView))]
    #[thread_kind = MainThreadOnly]
    struct BackdropView;

    impl BackdropView {
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

/// 隐藏了标题栏的窗口
struct Attached {
    window: Retained<NSWindow>,
    options: TitleBarOptions,
    /// 窗口按钮的大小和间距：建窗口时量一次（之后 AppKit 可能正在摆放，读到一半的位置）
    controls: Option<ControlsSize>,
    on_change: OnTitleBarChange,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

/// 三个窗口按钮的大小和相邻按钮的间距
#[derive(Clone, Copy, Debug, PartialEq)]
struct ControlsSize {
    width: f32,
    height: f32,
    spacing: f32,
}

impl ControlsSize {
    /// 按钮放在 `[x, y]` 时标题栏的度量
    fn metrics_at(self, [x, y]: [f32; 2]) -> TitleBarMetrics {
        TitleBarMetrics { height: self.height + 2.0 * y, controls: Some([x, y, 2.0 * self.spacing + self.width, self.height]) }
    }
}

thread_local! {
    /// 正在摆窗口按钮：自己改框架发出的变化通知不再摆一次
    static LAYING_OUT: Cell<bool> = const { Cell::new(false) };
    /// 隐藏了标题栏的窗口，按窗口对象的地址
    static TITLE_BARS: RefCell<HashMap<usize, Attached>> = RefCell::default();
    /// 有材质的窗口的材质视图，按窗口对象的地址
    static BACKDROPS: RefCell<HashMap<usize, Retained<BackdropView>>> = RefCell::default();
}

fn window_of(window: &dyn HasWindowHandle) -> Result<Retained<NSWindow>, PlatformError> {
    crate::macos::content_view(window)
        .and_then(NSView::window)
        .ok_or_else(|| PlatformError::System("不在主线程，或者不是这个进程的窗口".into()))
}

fn key(window: &NSWindow) -> usize {
    std::ptr::from_ref(window) as usize
}

/// 窗口隐藏了标题栏：摆好窗口按钮，之后 AppKit 重新摆放时再摆一次，度量变了调用 `on_change`
pub(crate) fn attach(window: &dyn HasWindowHandle, options: TitleBarOptions, on_change: OnTitleBarChange) -> Result<(), PlatformError> {
    let ns_window = window_of(window)?;
    let id = key(&ns_window);
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: 这些是 AppKit 导出的常量，进程的整个生命周期都有效
    let names = unsafe {
        [
            NSWindowDidResizeNotification,
            NSWindowDidEndLiveResizeNotification,
            NSWindowDidEnterFullScreenNotification,
            NSWindowDidExitFullScreenNotification,
            NSWindowDidBecomeKeyNotification,
            NSWindowDidResignKeyNotification,
        ]
    };
    let mut observers: Vec<_> = names
        .into_iter()
        .map(|name| {
            let block = RcBlock::new(move |_: NonNull<NSNotification>| refresh_by_key(id));
            // SAFETY: 只收这个窗口的通知；队列是空的，回调同步地在发通知的线程（AppKit 的这些通知都在主线程）上执行。
            // 观察者在 `detach` 里注销，块里只有窗口的地址
            unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), Some(&ns_window), None, &block) }
        })
        .collect();
    // 按钮和容器的框架一变（AppKit 放回原位）就放回去：通知是同步的，还在 AppKit 这次布局里，画面上看不到中间状态
    for view in control_views(&ns_window) {
        view.setPostsFrameChangedNotifications(true);
        let block = RcBlock::new(move |_: NonNull<NSNotification>| keep_in_place(id));
        // SAFETY: 常量是 AppKit 导出的，进程的整个生命周期都有效；只收这个视图的通知，同步地在主线程（视图的框架只在主线程改）。
        // 观察者在 `detach` 里注销，块里只有窗口的地址
        observers.push(unsafe { center.addObserverForName_object_queue_usingBlock(Some(NSViewFrameDidChangeNotification), Some(&view), None, &block) });
    }
    let controls = controls_size(&ns_window);
    layout_controls(&ns_window, options);
    TITLE_BARS.with(|t| t.borrow_mut().insert(id, Attached { window: ns_window, options, controls, on_change, observers }));
    Ok(())
}

/// 窗口要关了：注销通知
pub(crate) fn detach(window: &dyn HasWindowHandle) {
    let Ok(ns_window) = window_of(window) else { return };
    let id = key(&ns_window);
    let attached = TITLE_BARS.with(|t| t.borrow_mut().remove(&id));
    if let Some(attached) = attached {
        let center = NSNotificationCenter::defaultCenter();
        for observer in &attached.observers {
            // SAFETY: 这些观察者是 `attach` 里登记的
            unsafe { center.removeObserver(observer.as_ref()) };
        }
    }
    BACKDROPS.with(|b| b.borrow_mut().remove(&id));
}

/// 换窗口按钮的位置
pub(crate) fn set_options(window: &dyn HasWindowHandle, options: TitleBarOptions) -> Result<(), PlatformError> {
    let ns_window = window_of(window)?;
    let id = key(&ns_window);
    let found = TITLE_BARS.with(|t| match t.borrow_mut().get_mut(&id) {
        Some(attached) => {
            attached.options = options;
            true
        }
        None => false,
    });
    if !found {
        return Err(PlatformError::System("这个窗口的标题栏没有隐藏".into()));
    }
    refresh_by_key(id);
    Ok(())
}

/// AppKit 可能把按钮放回了原位（比如改了标题）：再摆一次
pub(crate) fn refresh(window: &dyn HasWindowHandle) {
    if let Ok(ns_window) = window_of(window) {
        refresh_by_key(key(&ns_window));
    }
}

fn refresh_by_key(id: usize) {
    // 先放开登记表的借用：摆按钮可能同步发出别的通知
    let attached = TITLE_BARS.with(|t| t.borrow().get(&id).map(|a| (a.window.clone(), a.options, a.on_change.clone())));
    if let Some((window, options, on_change)) = attached {
        layout_controls(&window, options);
        on_change();
    }
}

/// AppKit 动了窗口按钮或者它们的容器：放回去。度量没变，不用通知
fn keep_in_place(id: usize) {
    if LAYING_OUT.get() {
        return;
    }
    let attached = TITLE_BARS.with(|t| t.borrow().get(&id).map(|a| (a.window.clone(), a.options)));
    if let Some((window, options)) = attached {
        layout_controls(&window, options);
    }
}

/// 三个窗口按钮和它们的容器
fn control_views(window: &NSWindow) -> Vec<Retained<NSView>> {
    let mut views: Vec<Retained<NSView>> = Vec::new();
    for kind in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
        if let Some(button) = window.standardWindowButton(kind) {
            views.push(Retained::into_super(Retained::into_super(button)));
        }
    }
    views.extend(controls_container(window));
    views
}

/// 按钮所在的标题栏视图的容器
fn controls_container(window: &NSWindow) -> Option<Retained<NSView>> {
    let close = window.standardWindowButton(NSWindowButton::CloseButton)?;
    // SAFETY: 主线程上读视图的父视图，视图存活期间有效。按钮在标题栏视图里，标题栏视图在它的容器里
    unsafe { close.superview() }.and_then(|bar| unsafe { bar.superview() })
}

/// 量一次窗口按钮的大小和间距
fn controls_size(window: &NSWindow) -> Option<ControlsSize> {
    let close = window.standardWindowButton(NSWindowButton::CloseButton)?.frame();
    let miniaturize = window.standardWindowButton(NSWindowButton::MiniaturizeButton)?.frame();
    Some(ControlsSize { width: close.size.width as f32, height: close.size.height as f32, spacing: (miniaturize.origin.x - close.origin.x) as f32 })
}

/// 框架和要设的值差不到这么多就算一样：不再设，免得发出变化通知
const SAME: f64 = 0.01;

fn is_fullscreen(window: &NSWindow) -> bool {
    window.styleMask().contains(NSWindowStyleMask::FullScreen)
}

/// 把三个窗口按钮摆到设置的位置：标题栏容器加高到"按钮的高度加两倍的上边距"，按钮在里面竖直居中。全屏时系统自己管
fn layout_controls(window: &NSWindow, options: TitleBarOptions) {
    let Some([x, y]) = options.controls_position else { return };
    if is_fullscreen(window) {
        return;
    }
    let Some(size) = TITLE_BARS.with(|t| t.borrow().get(&key(window)).and_then(|a| a.controls)).or_else(|| controls_size(window)) else {
        return;
    };
    let button = |kind| window.standardWindowButton(kind);
    let (Some(close), Some(miniaturize), Some(zoom)) = (button(NSWindowButton::CloseButton), button(NSWindowButton::MiniaturizeButton), button(NSWindowButton::ZoomButton)) else {
        return;
    };
    let Some(container) = controls_container(window) else { return };
    let (x, y) = (f64::from(x), f64::from(y));
    let height = f64::from(size.height) + 2.0 * y;
    let top = window.frame().size.height - height;
    let was_laying_out = LAYING_OUT.replace(true);
    let mut frame = container.frame();
    if (frame.size.height - height).abs() > SAME || (frame.origin.y - top).abs() > SAME {
        frame.size.height = height;
        frame.origin.y = top;
        container.setFrame(frame);
    }
    for (i, button) in [&close, &miniaturize, &zoom].into_iter().enumerate() {
        let target = NSPoint::new(x + i as f64 * f64::from(size.spacing), y);
        let at = button.frame().origin;
        if (at.x - target.x).abs() > SAME || (at.y - target.y).abs() > SAME {
            button.setFrameOrigin(target);
        }
    }
    LAYING_OUT.set(was_laying_out);
}

/// 标题栏此刻占了多少；全屏时标题栏不占地方（鼠标移到屏幕顶上时系统把它盖在内容上面）
pub(crate) fn metrics(window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
    let ns_window = window_of(window).ok()?;
    if is_fullscreen(&ns_window) {
        return Some(TitleBarMetrics::default());
    }
    let (options, size) = TITLE_BARS.with(|t| t.borrow().get(&key(&ns_window)).map(|a| (a.options, a.controls))).unwrap_or_default();
    if let (Some(position), Some(size)) = (options.controls_position, size) {
        return Some(size.metrics_at(position));
    }
    let window_height = ns_window.frame().size.height;
    let controls = button_rect(&ns_window, window_height);
    let height = match (options.controls_position, controls) {
        (Some([_, y]), Some([_, _, _, button])) => button + 2.0 * y,
        // 系统的位置：窗口的高度减去不被标题栏盖住的那块
        _ => (window_height - ns_window.contentLayoutRect().size.height) as f32,
    };
    Some(TitleBarMetrics { height, controls })
}

/// 三个按钮在窗口内容区里的范围：`[x, y, 宽, 高]`，左上角为原点
fn button_rect(window: &NSWindow, window_height: f64) -> Option<[f32; 4]> {
    let close = window.standardWindowButton(NSWindowButton::CloseButton)?;
    let zoom = window.standardWindowButton(NSWindowButton::ZoomButton)?;
    let in_window = |button: &NSView| -> NSRect { button.convertRect_toView(button.bounds(), None) };
    let (first, last) = (in_window(&close), in_window(&zoom));
    let top = window_height - (first.origin.y + first.size.height);
    Some([first.origin.x as f32, top as f32, (last.origin.x + last.size.width - first.origin.x) as f32, first.size.height as f32])
}

/// 系统设置里双击标题栏该做什么（"桌面与程序坞"里的设置，存在全局的用户默认设置里）
pub(crate) fn double_click_action() -> TitleBarAction {
    let defaults = NSUserDefaults::standardUserDefaults();
    let action = defaults.stringForKey(&NSString::from_str("AppleActionOnDoubleClick")).map(|s| s.to_string());
    let miniaturize = defaults.boolForKey(&NSString::from_str("AppleMiniaturizeOnDoubleClick"));
    TitleBarAction::from_macos_defaults(action.as_deref(), miniaturize)
}

fn system_backdrop(backdrop: WindowMaterial) -> Option<NSVisualEffectMaterial> {
    match backdrop {
        WindowMaterial::Opaque => None,
        WindowMaterial::Window => Some(NSVisualEffectMaterial::UnderWindowBackground),
        WindowMaterial::Sidebar => Some(NSVisualEffectMaterial::Sidebar),
        WindowMaterial::Header => Some(NSVisualEffectMaterial::HeaderView),
        WindowMaterial::Menu => Some(NSVisualEffectMaterial::Menu),
        WindowMaterial::Popover => Some(NSVisualEffectMaterial::Popover),
    }
}

/// 窗口背后的材质：有材质时窗口透明、内容视图最下面放一个材质视图；不透明时拿掉
pub(crate) fn set_backdrop(window: &dyn HasWindowHandle, backdrop: WindowMaterial) -> Result<(), PlatformError> {
    let mtm = MainThreadMarker::new().ok_or_else(|| PlatformError::System("不在主线程".into()))?;
    let view = crate::macos::content_view(window).ok_or_else(|| PlatformError::System("不是这个进程的窗口".into()))?;
    let ns_window = view.window().ok_or_else(|| PlatformError::System("视图不在窗口里".into()))?;
    let id = key(&ns_window);
    let existing = BACKDROPS.with(|b| b.borrow().get(&id).cloned());
    match system_backdrop(backdrop) {
        Some(system) => {
            let effect = existing.unwrap_or_else(|| {
                // SAFETY: 主线程上建视图；父类的初始化方法收框架，返回初始化好的对象
                let effect: Retained<BackdropView> = unsafe { msg_send![BackdropView::alloc(mtm), initWithFrame: view.bounds()] };
                effect.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
                effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                effect.setState(NSVisualEffectState::FollowsWindowActiveState);
                view.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, None);
                // 表面的图层是内容视图的图层下面的一个子图层，不归视图管，AppKit 把子视图的图层排在它上面：明确排到它下面
                effect.setWantsLayer(true);
                if let Some(layer) = effect.layer() {
                    layer.setZPosition(-1.0);
                }
                BACKDROPS.with(|b| b.borrow_mut().insert(id, effect.clone()));
                effect
            });
            effect.setMaterial(system);
            ns_window.setOpaque(false);
            ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
        }
        None => {
            if let Some(effect) = existing {
                effect.removeFromSuperview();
                BACKDROPS.with(|b| b.borrow_mut().remove(&id));
            }
            ns_window.setOpaque(true);
            ns_window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
        }
    }
    ns_window.invalidateShadow();
    Ok(())
}
