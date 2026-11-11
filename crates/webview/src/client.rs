//! 一个网页的回调：加载状态、标题、地址、前进后退、弹出窗口、执行脚本；控制器的方法落到这里。
//!
//! 回调写 `Shared` 里的信号（见 `handlers`）；控制器的方法在网页打开着时调 CEF，没打开时 `load` 只记下地址，
//! 引擎打开网页时按它打开，别的方法什么都不做。

mod address;
mod handlers;

use std::rc::Rc;
use std::sync::Arc;

use cef::{CefString, ImplBrowser, ImplFrame};

use crate::state::{Page, Shared};

pub(crate) use address::fix as fix_url;

/// 给 `shared` 的网页建 CEF 的客户端（各种回调的集合）。离屏绘制的回调由 [`crate::render::handler`] 提供，
/// 还没有时照样建。回调只拿着能跨线程的那一半（`shared.page`），不和网页形成环
pub(crate) fn build(shared: &Rc<Shared>) -> Option<cef::Client> {
    build_for_page(&shared.page)
}

/// 给只有能跨线程的那一半的网页建客户端（内核替页面开的新窗口，在 CEF 的界面线程上）
pub(crate) fn build_for_page(page: &Arc<Page>) -> Option<cef::Client> {
    Some(handlers::client(page, crate::render::handler(page)))
}

/// 打开 `url`（先按 [`fix_url`] 整理）。网页打开着时让主框架跳过去，`shared.url` 等 CEF 报来新地址再变；
/// 没打开时只记进 `shared.url`（页面自己关了的，元素再按它打开）
pub(crate) fn load(shared: &Shared, url: String) {
    let url = fix_url(&url);
    match browser(shared).and_then(|browser| browser.main_frame()) {
        Some(frame) => frame.load_url(Some(&CefString::from(url.as_str()))),
        None => aureole::batch(|| {
            shared.url.set_if_changed(url);
            shared.closed.set_if_changed(false);
        }),
    }
}

/// 重新加载；页面自己关了的，元素按记着的地址重新打开
pub(crate) fn reload(shared: &Shared) {
    match browser(shared) {
        Some(browser) => browser.reload(),
        None => {
            shared.closed.set_if_changed(false);
        }
    }
}

pub(crate) fn stop(shared: &Shared) {
    if let Some(browser) = browser(shared) {
        browser.stop_load();
    }
}

pub(crate) fn back(shared: &Shared) {
    if let Some(browser) = browser(shared) {
        browser.go_back();
    }
}

pub(crate) fn forward(shared: &Shared) {
    if let Some(browser) = browser(shared) {
        browser.go_forward();
    }
}

pub(crate) fn eval(shared: &Shared, script: &str) {
    if let Some(frame) = browser(shared).and_then(|browser| browser.main_frame()) {
        frame.execute_java_script(Some(&CefString::from(script)), None, 0);
    }
}

/// 打开着的网页。克隆出来再用：调 CEF 时不借着 `shared.browser`，CEF 可能同步地回调，回调里还要借它
fn browser(shared: &Shared) -> Option<cef::Browser> {
    shared.browser.borrow().clone()
}
