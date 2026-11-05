//! 拖到应用外面（macOS）：用 AppKit 的拖动会话（`beginDraggingSessionWithItems:event:source:`）
//! 把文件、文字交给访达和其他应用。拖动的图像是文件（或纯文本）的系统图标。
//!
//! 会话开始以后由系统接管指针，在自己的事件循环里等指针松开；这个循环套在应用的事件循环里面，
//! 它不返回，应用的退出请求就不生效。应用要退出时先取消会话（[`cancel_drag_out`]）。

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent,
    NSEventModifierFlags, NSEventType, NSImage, NSView, NSWorkspace,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
use objc2_uniform_type_identifiers::UTType;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// 拖动图像的边长（点）
const ICON: f64 = 48.0;

/// 会话结束时调用一次：参数是有没有被接住
type OnEnd = Box<dyn FnOnce(bool)>;

struct Ivars {
    on_end: RefCell<Option<OnEnd>>,
}

define_class!(
    // SAFETY: NSObject 没有子类化要求；DragSource 没有实现 Drop
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct DragSource;

    // SAFETY: NSObject 协议的方法都由父类 NSObject 实现，这里一个也不覆盖
    unsafe impl NSObjectProtocol for DragSource {}

    // SAFETY: 协议里唯一必须实现的 draggingSession:sourceOperationMaskForDraggingContext: 在下面，
    // 两个方法的参数和返回类型都和协议的声明一致；其余方法是可选的
    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            // 只许拷贝：拖到访达里不会把原来的文件挪走
            NSDragOperation(1)
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            let on_end = self.ivars().on_end.borrow_mut().take();
            if let Some(on_end) = on_end {
                on_end(operation.0 != 0);
            }
        }
    }
);

impl DragSource {
    fn new(mtm: MainThreadMarker, on_end: Box<dyn FnOnce(bool)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { on_end: RefCell::new(Some(on_end)) });
        // SAFETY: init 是父类 NSObject 的初始化方法，不带参数，返回初始化好的同一个对象，和返回类型 Retained<Self> 一致；
        // 实例变量在上一行已经设好
        unsafe { msg_send![super(this), init] }
    }
}

thread_local! {
    /// 正在进行的会话的源：留到下一次拖动（不在它自己的回调里释放）
    static SOURCE: RefCell<Option<Retained<DragSource>>> = const { RefCell::new(None) };
    /// 最近开始的会话和它所在窗口的编号（取消时用）：和源一样留到下一次拖动
    static SESSION: RefCell<Option<(Retained<NSDraggingSession>, isize)>> = const { RefCell::new(None) };
}

/// 开始把文件（要已经存在）或纯文本拖到应用外面，`at` 是指针在窗口内容区里的位置（逻辑像素）。
/// 要在处理指针拖动事件时调用（系统的会话从这个事件开始，接管之后的指针移动和松开）；
/// 当前事件不是指针事件时在 `at` 造一个。会话结束时调用 `on_end`，参数是有没有被接住。
/// 开始不了（没有窗口、没有东西可拖）时返回 `false`，不调用 `on_end`
pub fn begin_drag_out(
    window: &dyn HasWindowHandle,
    files: &[PathBuf],
    text: Option<&str>,
    at: [f32; 2],
    on_end: Box<dyn FnOnce(bool)>,
) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Ok(handle) = window.window_handle() else { return false };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return false };
    // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let Some(ns_window) = view.window() else { return false };
    let [x, y] = at.map(f64::from);
    let y = if view.isFlipped() { y } else { view.bounds().size.height - y };
    let point = NSPoint::new(x, y);

    let mut items = Vec::new();
    let workspace = NSWorkspace::sharedWorkspace();
    for path in files.iter().filter(|p| p.exists()) {
        let url = file_url(path);
        let icon = workspace.iconForFile(&NSString::from_str(&path.to_string_lossy()));
        items.push(item(ProtocolObject::from_ref(&*url), &icon, point, items.len()));
    }
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        let string = NSString::from_str(text);
        let plain = UTType::typeWithIdentifier(&NSString::from_str("public.plain-text"));
        let icon = plain.map_or_else(|| workspace.iconForFile(&NSString::from_str("/")), |t| workspace.iconForContentType(&t));
        items.push(item(ProtocolObject::from_ref(&*string), &icon, point, items.len()));
    }
    if items.is_empty() {
        return false;
    }

    // 会话从当前的指针拖动（按下）事件开始；没有的话在指针处造一个
    let app = NSApplication::sharedApplication(mtm);
    let current = app.currentEvent().filter(|e| matches!(e.r#type(), NSEventType::LeftMouseDragged | NSEventType::LeftMouseDown));
    let event = match current {
        Some(event) => event,
        None => {
            let location = view.convertPoint_toView(point, None);
            let Some(event) = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::LeftMouseDragged,
                location,
                NSEventModifierFlags::empty(),
                objc2_foundation::NSProcessInfo::processInfo().systemUptime(),
                ns_window.windowNumber(),
                None,
                0,
                1,
                1.0,
            ) else {
                return false;
            };
            event
        }
    };
    let source = DragSource::new(mtm, on_end);
    let array = NSArray::from_retained_slice(&items);
    let session = view.beginDraggingSessionWithItems_event_source(&array, &event, ProtocolObject::from_ref(&*source));
    // 没被接住时图像滑回原处
    session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    SOURCE.with(|s| *s.borrow_mut() = Some(source));
    SESSION.with(|s| *s.borrow_mut() = Some((session, ns_window.windowNumber())));
    true
}

/// 取消正在进行的拖动会话（应用要退出时）：会话按没被接住结束（调用 `on_end(false)`），图像不滑回原处。
/// 没有进行中的会话、不在主线程时什么都不做，返回 `false`。
///
/// 会话在自己的事件循环里等指针松开，等到之前应用的事件循环退不出来。AppKit 没有取消会话的接口，
/// 这里像用户按了 Esc 那样往应用的事件队列里放一次 Esc 的按下和松开：会话的循环从同一个队列取事件，收到 Esc 就取消
/// （不用指针松开：那样会在指针所在的地方放下，放进访达就真的拷贝了文件）
pub fn cancel_drag_out() -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let active = SOURCE.with(|s| s.borrow().as_ref().is_some_and(|source| source.ivars().on_end.borrow().is_some()));
    let Some((session, window)) = SESSION.with(|s| s.borrow().clone()).filter(|_| active) else { return false };
    session.setAnimatesToStartingPositionsOnCancelOrFail(false);
    let app = NSApplication::sharedApplication(mtm);
    let escape = NSString::from_str("\u{1b}");
    let now = objc2_foundation::NSProcessInfo::processInfo().systemUptime();
    for kind in [NSEventType::KeyDown, NSEventType::KeyUp] {
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            kind,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            now,
            window,
            None,
            &escape,
            &escape,
            false,
            ESCAPE_KEY_CODE,
        );
        match event {
            Some(event) => app.postEvent_atStart(&event, false),
            None => return false,
        }
    }
    true
}

/// Esc 键的虚拟键码（`kVK_Escape`）
const ESCAPE_KEY_CODE: u16 = 0x35;

fn file_url(path: &Path) -> Retained<NSURL> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    NSURL::fileURLWithPath(&NSString::from_str(&absolute.to_string_lossy()))
}

/// 一个拖动的东西：内容，加上它的图标（以指针为中心，几个的时候错开一点）
fn item(
    writer: &ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>,
    icon: &NSImage,
    point: NSPoint,
    index: usize,
) -> Retained<NSDraggingItem> {
    let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
    icon.setSize(NSSize::new(ICON, ICON));
    let step = index as f64 * 6.0;
    let frame = NSRect::new(NSPoint::new(point.x - ICON * 0.5 + step, point.y - ICON * 0.5 - step), NSSize::new(ICON, ICON));
    // SAFETY: contents 要求是图像对象，这里给的是 NSImage
    unsafe { item.setDraggingFrame_contents(frame, Some(icon)) };
    item
}
