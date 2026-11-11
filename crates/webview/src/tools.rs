//! 页面的几样工具：网页全屏、缩放、页内查找、开发者工具、打印网页（系统的打印对话框、存成 PDF）。
//!
//! 都是原语：网页全屏只告诉应用页面要全屏了（[`crate::WebController::fullscreen`]），窗口要不要全屏、视图要不要铺满由应用定；
//! 查找给出匹配的个数和第几个，查找框由应用自己画。

use std::future::Future;
use std::os::raw::c_int;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use cef::rc::Rc as _;
use cef::{
    Browser, CefString, FindHandler, ImplBrowserHost, ImplFindHandler, ImplPdfPrintCallback, PdfPrintCallback, PdfPrintSettings, Rect, WrapFindHandler,
    WrapPdfPrintCallback, wrap_find_handler, wrap_pdf_print_callback,
};

use crate::state::{Page, Shared};

/// 页内查找的结果（[`crate::WebController::find_result`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindResult {
    /// 一共几处匹配
    pub matches: u32,
    /// 当前选中的是第几处（从 1 起；没有匹配时是 0）
    pub active: u32,
}

/// 缩放的倍数换成 CEF 的缩放级别（每一级是 1.2 倍，0 是 100%）
pub(crate) fn zoom_level(factor: f64) -> f64 {
    if factor > 0.0 { factor.ln() / 1.2f64.ln() } else { 0.0 }
}

fn host(shared: &Shared) -> Option<cef::BrowserHost> {
    shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host)
}

/// 缩放：`factor` 是倍数（1.0 是 100%）
pub(crate) fn set_zoom(shared: &Shared, factor: f64) {
    let factor = if factor > 0.0 { factor } else { 1.0 };
    shared.zoom.set_if_changed(factor);
    if let Some(host) = host(shared) {
        host.set_zoom_level(zoom_level(factor));
    }
}

/// 网页打开以后：按记着的倍数缩放（打开之前设的）
pub(crate) fn apply_zoom(shared: &Shared) {
    let factor = shared.zoom.get_untracked();
    if factor != 1.0
        && let Some(host) = host(shared)
    {
        host.set_zoom_level(zoom_level(factor));
    }
}

/// 页内查找：和上次是同一段文字时找下一处（`forward` 是往后），否则从头找。
/// 总是按"接着找"发：文字、大小写变了时内核自己从头开始；按"重新找"发的第一次只数匹配，不选中任何一处
pub(crate) fn find(shared: &Shared, text: &str, forward: bool, match_case: bool) {
    let Some(host) = host(shared) else { return };
    if text.is_empty() {
        return stop_finding(shared, false);
    }
    host.find(Some(&CefString::from(text)), c_int::from(forward), c_int::from(match_case), 1);
}

/// 停止查找；`keep_selection` 时当前选中的那处留着选中
pub(crate) fn stop_finding(shared: &Shared, keep_selection: bool) {
    shared.find_result.set_if_changed(FindResult::default());
    if let Some(host) = host(shared) {
        host.stop_finding(c_int::from(!keep_selection));
    }
}

/// 开发者工具：单独的窗口（Windows 上是系统的弹出窗口）
pub(crate) fn show_dev_tools(shared: &Shared) {
    open_dev_tools(shared, None);
}

/// 打开开发者工具，选中 `at`（网页视图内容区里的位置，逻辑像素）处的元素
pub(crate) fn inspect(shared: &Shared, at: [f32; 2]) {
    open_dev_tools(shared, Some(cef::Point { x: at[0].floor() as c_int, y: at[1].floor() as c_int }));
}

fn open_dev_tools(shared: &Shared, at: Option<cef::Point>) {
    let Some(host) = host(shared) else { return };
    // 开发者工具只能用内核默认的样式（不是网页视图用的 Alloy）
    #[cfg(target_os = "windows")]
    let window = cef::WindowInfo::default().set_as_popup(cef::sys::HWND(std::ptr::null_mut()), "开发者工具");
    #[cfg(not(target_os = "windows"))]
    let window = cef::WindowInfo::default();
    host.show_dev_tools(Some(&window), None, Some(&cef::BrowserSettings::default()), at.as_ref());
}

pub(crate) fn close_dev_tools(shared: &Shared) {
    if let Some(host) = host(shared) {
        host.close_dev_tools();
    }
}

/// 打印网页：内核自己弹系统的打印对话框、排版、交给打印机
pub(crate) fn print(shared: &Shared) {
    if let Some(host) = host(shared) {
        host.print();
    }
}

/// 存成 PDF：页面按打印的样式排版（A4、默认边距、带背景），存到 `path`。返回的 future 在写完时给出成不成功
pub(crate) fn print_to_pdf(shared: &Shared, path: &Path) -> PdfDone {
    let done = PdfDone::default();
    match host(shared) {
        Some(host) => {
            let settings = PdfPrintSettings { print_background: 1, ..Default::default() };
            let mut callback = PdfFinished::new(done.clone());
            host.print_to_pdf(Some(&CefString::from(path.to_string_lossy().as_ref())), Some(&settings), Some(&mut callback));
        }
        None => done.finish(false),
    }
    done
}

/// 存成 PDF 的结果：写完时是 `true`，失败、网页没打开时是 `false`
#[derive(Clone, Default)]
pub struct PdfDone {
    state: Arc<Mutex<(Option<bool>, Option<Waker>)>>,
}

impl PdfDone {
    fn finish(&self, ok: bool) {
        let waker = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.0 = Some(ok);
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Future for PdfDone {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.0 {
            Some(ok) => Poll::Ready(ok),
            None => {
                state.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

wrap_pdf_print_callback! {
    struct PdfFinished {
        done: PdfDone,
    }

    impl PdfPrintCallback {
        fn on_pdf_print_finished(&self, _path: Option<&CefString>, ok: c_int) {
            self.done.finish(ok != 0);
        }
    }
}

wrap_find_handler! {
    struct PageFind {
        page: Arc<Page>,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: c_int,
            count: c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: c_int,
            _final_update: c_int,
        ) {
            let result = FindResult { matches: count.max(0) as u32, active: active_match_ordinal.max(0) as u32 };
            self.page.on_main(move |shared| {
                shared.find_result.set_if_changed(result);
            });
        }
    }
}

/// 给网页的客户端：查找的结果
pub(crate) fn find_handler(page: &Arc<Page>) -> FindHandler {
    PageFind::new(page.clone())
}
