//! macOS 上页面对话框的默认样子：NSAlert 挂在应用此刻的窗口上（表单式：不挡别的窗口，事件循环、页面、框架照常跑），
//! 关掉时把结果交回来。应用不在前台时挂在它看得见的第一个窗口上；一个窗口都没有时弹应用级的模态框。
//!
//! `alert` 只有"好"；`confirm`、离开页面前的确认有"好"和"取消"；`prompt` 多一个输入框，里面是页面给的默认文字。

use std::cell::Cell;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSModalResponse, NSTextField};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// 输入框的宽度，点
const INPUT_WIDTH: f64 = 300.0;

/// 弹一个对话框：`input` 是输入框的初始文字（`None` 时没有输入框），`cancellable` 时多一个"取消"。
/// 关掉以后在主线程上调用 `done`：按了"好"时是 `Some(输入框的文字)`（没有输入框时是空的），取消时是 `None`。
/// 不在主线程上时直接按取消
pub(super) fn show(title: &str, message: &str, cancellable: bool, input: Option<&str>, done: impl FnOnce(Option<String>) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        done(None);
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("好"));
    if cancellable {
        alert.addButtonWithTitle(&NSString::from_str("取消"));
    }
    let field: Option<Retained<NSTextField>> = input.map(|text| {
        let field = NSTextField::textFieldWithString(&NSString::from_str(text), mtm);
        field.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(INPUT_WIDTH, 24.0)));
        alert.setAccessoryView(Some(&field));
        alert.window().setInitialFirstResponder(Some(&field));
        field
    });
    let done = Cell::new(Some(done));
    let finish = move |response: NSModalResponse| {
        let accepted = response == NSAlertFirstButtonReturn;
        let text = accepted.then(|| field.as_ref().map(|f| f.stringValue().to_string()).unwrap_or_default());
        if let Some(done) = done.take() {
            done(text);
        }
    };
    let app = NSApplication::sharedApplication(mtm);
    // 挂在应用此刻的窗口上；应用不在前台时没有"此刻的窗口"，挂在它看得见的第一个窗口上（用户切回来就看到）。
    // 都没有、或者那个窗口已经挂着别的表单（另一个对话框）时弹应用级的模态框
    let window = app
        .keyWindow()
        .or_else(|| app.mainWindow())
        .or_else(|| app.windows().iter().find(|w| w.isVisible() && w.canBecomeKeyWindow()))
        .filter(|w| w.attachedSheet().is_none());
    match window {
        Some(window) => {
            let handler = RcBlock::new(finish);
            alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
        }
        None => {
            app.activate();
            finish(alert.runModal());
        }
    }
}
