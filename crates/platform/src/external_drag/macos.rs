//! macOS：给窗口的内容视图加上拖放方法。
//!
//! 窗口库把拖放登记在窗口上（`NSWindow registerForDraggedTypes:`，只有文件），它的窗口代理实现拖放的方法。AppKit 找拖放目标时
//! 先看指针下的视图，所以这里在内容视图的类上加方法（`class_addMethod`：不加视图、不动层，渲染不受影响；也不用换掉窗口库的类）、
//! 在视图上登记更多类型。有文件的拖放原样转给窗口代理，窗口库照旧发文件的窗口事件；别的内容交给回调。
//!
//! 回答（光标）总是交给回调决定：文件的回答也是，窗口代理的返回值只用来转发。
//!
//! 这个进程自己拖出去的（[`ExternalDataKinds::own`]）不转给窗口代理：文件也交给回调（和 Windows 一样），
//! 运行时决定是交给应用自己的窗口之间的拖放，还是发文件的窗口事件。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::PathBuf;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, NSObjectProtocol, Sel};
use objc2::{MainThreadMarker, Message, ffi, msg_send, sel};
use objc2_app_kit::{
    NSDragOperation, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardTypeTIFF,
    NSPasteboardTypeURL, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSString, ns_string};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{ExternalDataKinds, ExternalDrag, ExternalDropData, ExternalDropEffect, OnExternalDrag, guarded};
use crate::PlatformError;
use crate::clipboard::macos::{files_on, has_image, has_type, image_on};

/// 一个视图的回调和这次拖放的状态
struct Listener {
    on_drag: OnExternalDrag,
    /// 这次拖的内容：进来时读一次，拖动期间剪贴板不会变
    kinds: Cell<ExternalDataKinds>,
}

thread_local! {
    /// 登记了的视图（按地址）。视图关窗口前摘掉（[`stop`]）
    static LISTENERS: RefCell<HashMap<usize, Rc<Listener>>> = RefCell::new(HashMap::new());
    /// 已经加过方法的类：同一个类的窗口共用
    static PATCHED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

fn listener(view: &NSView) -> Option<Rc<Listener>> {
    LISTENERS.with(|listeners| listeners.borrow().get(&(std::ptr::from_ref(view) as usize)).cloned())
}

fn pasteboard_of(sender: &AnyObject) -> Retained<NSPasteboard> {
    // SAFETY: AppKit 交来的（或者测试给的）拖放信息都实现了 draggingPasteboard，返回 NSPasteboard
    unsafe { msg_send![sender, draggingPasteboard] }
}

/// 拖放在视图里的位置（逻辑像素，左上角为原点）
fn position_of(view: &NSView, sender: &AnyObject) -> [f32; 2] {
    // SAFETY: 拖放信息都实现了 draggingLocation，返回窗口坐标的 NSPoint
    let location: NSPoint = unsafe { msg_send![sender, draggingLocation] };
    let point = view.convertPoint_fromView(location, None);
    let y = if view.isFlipped() { point.y } else { view.bounds().size.height - point.y };
    [point.x as f32, y as f32]
}

/// 剪贴板上有哪几种内容
fn kinds_on(pasteboard: &NSPasteboard) -> ExternalDataKinds {
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (file, string, url, rtf) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardTypeURL, NSPasteboardTypeRTF) };
    let custom = super::custom_formats().iter().any(|name| has_type(pasteboard, &NSString::from_str(name)));
    let own = crate::drag::dragging_out();
    if has_type(pasteboard, file) {
        return ExternalDataKinds { files: true, custom, own, ..ExternalDataKinds::default() };
    }
    ExternalDataKinds {
        text: has_type(pasteboard, string),
        link: has_type(pasteboard, url),
        image: has_image(pasteboard),
        files: false,
        rich_text: has_type(pasteboard, rtf),
        custom,
        own,
    }
}

/// 有文件的拖放转给窗口代理（窗口库发文件的窗口事件）；自己拖出去的不转（见模块说明）
fn forwards(kinds: ExternalDataKinds) -> bool {
    kinds.files && !kinds.own
}

/// 剪贴板上的内容（文件交给窗口库，不在这里；自己拖出去的除外）
fn data_on(pasteboard: &NSPasteboard, kinds: ExternalDataKinds) -> ExternalDropData {
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let (string, url) = unsafe { (NSPasteboardTypeString, NSPasteboardTypeURL) };
    // SAFETY: 同上
    let rtf = unsafe { NSPasteboardTypeRTF };
    let custom = || {
        super::custom_formats()
            .into_iter()
            .filter_map(|name| {
                let bytes = pasteboard.dataForType(&NSString::from_str(&name))?.to_vec();
                Some((name, bytes))
            })
            .collect()
    };
    ExternalDropData {
        text: kinds.text.then(|| pasteboard.stringForType(string)).flatten().map(|text| text.to_string()),
        link: kinds.link.then(|| pasteboard.stringForType(url)).flatten().map(|link| link.to_string()),
        image: kinds.image.then(|| image_on(pasteboard)).flatten(),
        files: if kinds.own && kinds.files { files_on(pasteboard) } else { Vec::new() },
        rich_text: kinds.rich_text.then(|| pasteboard.dataForType(rtf)).flatten().map(|data| data.to_vec()),
        custom: if kinds.custom { custom() } else { Vec::new() },
    }
}

/// 回答换成系统的操作：来源不许这种操作时换一种它许的（光标不一样，但接得住）
fn operation(effect: Option<ExternalDropEffect>, sender: &AnyObject) -> NSDragOperation {
    let Some(effect) = effect else { return NSDragOperation::None };
    // SAFETY: 拖放信息都实现了 draggingSourceOperationMask，返回 NSDragOperation
    let allowed: NSDragOperation = unsafe { msg_send![sender, draggingSourceOperationMask] };
    let wanted = match effect {
        ExternalDropEffect::Copy => NSDragOperation::Copy,
        ExternalDropEffect::Move => NSDragOperation::Move,
        ExternalDropEffect::Link => NSDragOperation::Link,
    };
    [wanted, NSDragOperation::Copy, NSDragOperation::Generic, NSDragOperation::Move, NSDragOperation::Link]
        .into_iter()
        .find(|operation| allowed.contains(*operation))
        .unwrap_or(NSDragOperation::None)
}

/// 窗口代理（窗口库的）：有文件的拖放转给它
fn delegate_of(view: &NSView, selector: Sel) -> Option<Retained<AnyObject>> {
    let delegate = view.window()?.delegate()?;
    if !delegate.respondsToSelector(selector) {
        return None;
    }
    // SAFETY: 协议对象就是一个 Objective-C 对象，换成 AnyObject 只是为了发消息（窗口库实现的是自己声明的那几个方法）
    Some(unsafe { Retained::cast_unchecked(delegate) })
}

unsafe extern "C-unwind" fn dragging_entered(view: &NSView, _: Sel, sender: &AnyObject) -> NSDragOperation {
    guarded(NSDragOperation::None, || {
        let Some(listener) = listener(view) else { return NSDragOperation::None };
        let pasteboard = pasteboard_of(sender);
        let kinds = kinds_on(&pasteboard);
        listener.kinds.set(kinds);
        if forwards(kinds)
            && let Some(delegate) = delegate_of(view, sel!(draggingEntered:))
        {
            // SAFETY: 响应这个选择子（上面查过），窗口库的实现收拖放信息、返回 BOOL
            let _: bool = unsafe { msg_send![&*delegate, draggingEntered: sender] };
        }
        let entered = if kinds.files && kinds.own { files_on(&pasteboard) } else { Vec::new() };
        over(view, &listener, sender, entered)
    })
}

unsafe extern "C-unwind" fn dragging_updated(view: &NSView, _: Sel, sender: &AnyObject) -> NSDragOperation {
    guarded(NSDragOperation::None, || {
        let Some(listener) = listener(view) else { return NSDragOperation::None };
        over(view, &listener, sender, Vec::new())
    })
}

fn over(view: &NSView, listener: &Listener, sender: &AnyObject, entered_files: Vec<PathBuf>) -> NSDragOperation {
    let kinds = listener.kinds.get();
    if kinds.is_empty() {
        return NSDragOperation::None;
    }
    let effect = (listener.on_drag)(ExternalDrag::Over { kinds, position: position_of(view, sender), entered_files });
    operation(effect, sender)
}

unsafe extern "C-unwind" fn dragging_exited(view: &NSView, _: Sel, sender: Option<&AnyObject>) {
    guarded((), || {
        let Some(listener) = listener(view) else { return };
        if forwards(listener.kinds.get())
            && let Some(delegate) = delegate_of(view, sel!(draggingExited:))
        {
            // SAFETY: 响应这个选择子（上面查过），窗口库的实现收可以为空的拖放信息
            let _: () = unsafe { msg_send![&*delegate, draggingExited: sender] };
        }
        (listener.on_drag)(ExternalDrag::Leave);
    });
}

unsafe extern "C-unwind" fn prepare_for_drag_operation(_view: &NSView, _: Sel, _sender: &AnyObject) -> Bool {
    Bool::YES
}

unsafe extern "C-unwind" fn perform_drag_operation(view: &NSView, _: Sel, sender: &AnyObject) -> Bool {
    let done = guarded(false, || {
        let Some(listener) = listener(view) else { return false };
        let kinds = listener.kinds.get();
        if forwards(kinds) {
            let Some(delegate) = delegate_of(view, sel!(performDragOperation:)) else { return false };
            // SAFETY: 响应这个选择子（上面查过），窗口库的实现收拖放信息、返回 BOOL
            return unsafe { msg_send![&*delegate, performDragOperation: sender] };
        }
        let data = data_on(&pasteboard_of(sender), kinds);
        (listener.on_drag)(ExternalDrag::Drop { data, position: position_of(view, sender) });
        true
    });
    Bool::new(done)
}

unsafe extern "C-unwind" fn conclude_drag_operation(view: &NSView, _: Sel, sender: Option<&AnyObject>) {
    guarded((), || {
        if listener(view).is_some_and(|listener| forwards(listener.kinds.get()))
            && let Some(delegate) = delegate_of(view, sel!(concludeDragOperation:))
        {
            // SAFETY: 响应这个选择子（上面查过），窗口库的实现收可以为空的拖放信息
            let _: () = unsafe { msg_send![&*delegate, concludeDragOperation: sender] };
        }
    });
}

/// 类型编码：返回操作的、无返回的、返回 BOOL 的（BOOL 在 arm64 上是真的布尔，在 x86_64 上是有符号字符）
const RETURNS_OPERATION: &CStr = c"Q@:@";
const RETURNS_NOTHING: &CStr = c"v@:@";
#[cfg(target_arch = "aarch64")]
const RETURNS_BOOL: &CStr = c"B@:@";
#[cfg(not(target_arch = "aarch64"))]
const RETURNS_BOOL: &CStr = c"c@:@";

/// 给这个类加上拖放方法，同一个类只加一次。类已经有这些方法（别的库加的）时不覆盖，报错
fn add_methods(class: &AnyClass) -> Result<(), PlatformError> {
    let key = std::ptr::from_ref(class) as usize;
    if PATCHED.with(|patched| patched.borrow().contains(&key)) {
        return Ok(());
    }
    type Operation = unsafe extern "C-unwind" fn(&NSView, Sel, &AnyObject) -> NSDragOperation;
    type Nothing = unsafe extern "C-unwind" fn(&NSView, Sel, Option<&AnyObject>);
    type Answer = unsafe extern "C-unwind" fn(&NSView, Sel, &AnyObject) -> Bool;
    // SAFETY: 三种函数指针的调用约定和这些方法在 AppKit 里的声明一致；Imp 只是个不透明的函数指针，运行时按选择子调用时按原来的类型用
    let methods: [(Sel, Imp, &CStr); 6] = unsafe {
        [
            (sel!(draggingEntered:), std::mem::transmute::<Operation, Imp>(dragging_entered), RETURNS_OPERATION),
            (sel!(draggingUpdated:), std::mem::transmute::<Operation, Imp>(dragging_updated), RETURNS_OPERATION),
            (sel!(draggingExited:), std::mem::transmute::<Nothing, Imp>(dragging_exited), RETURNS_NOTHING),
            (sel!(prepareForDragOperation:), std::mem::transmute::<Answer, Imp>(prepare_for_drag_operation), RETURNS_BOOL),
            (sel!(performDragOperation:), std::mem::transmute::<Answer, Imp>(perform_drag_operation), RETURNS_BOOL),
            (sel!(concludeDragOperation:), std::mem::transmute::<Nothing, Imp>(conclude_drag_operation), RETURNS_NOTHING),
        ]
    };
    let target = std::ptr::from_ref(class).cast_mut();
    for (selector, imp, types) in methods {
        // SAFETY: `target` 是运行时里活着的类，类不会被释放；类型编码以 0 结尾，方法签名和编码一致
        let added = unsafe { ffi::class_addMethod(target, selector, imp, types.as_ptr()) };
        if !added.as_bool() {
            return Err(PlatformError::Unsupported(format!("视图的类已经有 {} 方法", selector.name().to_string_lossy()).into()));
        }
    }
    PATCHED.with(|patched| patched.borrow_mut().push(key));
    Ok(())
}

fn view_of(window: &dyn HasWindowHandle) -> Option<Retained<NSView>> {
    MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    Some(view.retain())
}

/// 开始接收拖来的内容
pub fn listen(window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
    let view = view_of(window).ok_or_else(|| PlatformError::System("不在主线程，或者不是这个进程的窗口".into()))?;
    add_methods(view.class())?;
    // 登记过的自己定的格式也接受（要在打开窗口之前登记），还有自己拖出去的标记（内容是空的时只有它）
    let custom: Vec<Retained<NSString>> =
        super::custom_formats().iter().map(String::as_str).chain([crate::drag::OWN_TYPE]).map(NSString::from_str).collect();
    // SAFETY: 类型常量是 AppKit 导出的不可变字符串，链接后一直有效
    let mut types: Vec<&NSString> = unsafe {
        vec![NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardTypeURL, NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeRTF, ns_string!("public.jpeg")]
    };
    types.extend(custom.iter().map(|name| &**name));
    view.registerForDraggedTypes(&NSArray::from_slice(&types));
    let listener = Rc::new(Listener { on_drag, kinds: Cell::new(ExternalDataKinds::default()) });
    LISTENERS.with(|listeners| listeners.borrow_mut().insert(std::ptr::from_ref(&*view) as usize, listener));
    Ok(())
}

/// 不再接收（关窗口前）。之后的拖放回到窗口上，窗口库照旧接文件
pub fn stop(window: &dyn HasWindowHandle) {
    let Some(view) = view_of(window) else { return };
    view.unregisterDraggedTypes();
    LISTENERS.with(|listeners| listeners.borrow_mut().remove(&(std::ptr::from_ref(&*view) as usize)));
}
