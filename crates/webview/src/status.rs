//! 页面的状态：网站图标、悬停的链接、加载进度、加载出错、提示文字、渲染进程退出了、历史记录、静音、控制台消息。
//!
//! 内核在它的界面线程上报这些，这里换成应用看的值，投递到主线程写进信号（`Shared` 里的）。网站图标由内核下载、解码，
//! 按窗口的缩放取合适的那一张；历史记录在加载状态、标题变了时向内核要一遍（只能在内核的界面线程上要）。

use std::os::raw::c_int;
use std::sync::{Arc, Mutex, PoisonError};

use cef::rc::Rc as _;
use cef::{
    AlphaType, BinaryValue, Browser, CefString, CefStringList, ColorType, DownloadImageCallback, Errorcode, Image, ImplBinaryValue, ImplBrowser,
    ImplBrowserHost, ImplDownloadImageCallback, ImplImage, ImplNavigationEntry, ImplNavigationEntryVisitor, LogSeverity, NavigationEntry,
    NavigationEntryVisitor, TerminationStatus, WrapDownloadImageCallback, WrapNavigationEntryVisitor, wrap_download_image_callback,
    wrap_navigation_entry_visitor,
};

use crate::state::{Hook, Page, Shared};

/// 主框架加载出错（[`crate::WebController::load_error`]）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoadError {
    /// 内核的错误码（负数，和 Chromium 的 `net::Error` 一样，比如 -105 是域名解析不了）
    pub code: i32,
    /// 错误码的名字（`ERR_NAME_NOT_RESOLVED` 这样）
    pub text: String,
    /// 没打开的地址
    pub url: String,
}

/// 渲染进程为什么退出了
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrashReason {
    /// 崩溃了
    Crashed,
    /// 内存不够
    OutOfMemory,
    /// 被结束了（任务管理器、系统）
    Killed,
    /// 别的原因（启动失败、完整性检查没过、不正常地退出）
    Other,
}

/// 页面的渲染进程退出了（[`crate::WebController::crash`]）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Crash {
    /// 为什么退出
    pub reason: CrashReason,
    /// 进程的退出码
    pub code: i32,
}

/// 历史记录里的一项
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HistoryEntry {
    /// 地址
    pub url: String,
    /// 标题
    pub title: String,
}

/// 历史记录（[`crate::WebController::history`]）：从最早的到最新的，现在在第 `current` 个
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct History {
    /// 从最早的到最新的
    pub entries: Vec<HistoryEntry>,
    /// 现在在第几个（从 0 起）
    pub current: usize,
}

/// 控制台消息的级别
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConsoleLevel {
    /// `console.debug`
    Debug,
    /// `console.log`、`console.info`
    Info,
    /// `console.warn`
    Warning,
    /// `console.error`
    Error,
}

/// 页面的一条控制台消息（[`crate::WebController::on_console`]）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsoleMessage {
    /// 级别
    pub level: ConsoleLevel,
    /// 内容
    pub message: String,
    /// 哪个脚本
    pub source: String,
    /// 第几行
    pub line: u32,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 回调来自 `shared` 现在的网页（或者网页正在打开）
fn current(shared: &Shared, browser: Option<c_int>) -> bool {
    let open = shared.browser.try_borrow().ok().and_then(|open| open.clone());
    match (browser, open) {
        (Some(browser), Some(open)) => open.identifier() == browser,
        _ => true,
    }
}

/// 在主线程上对 `browser` 是 `shared` 现在的网页时做 `f`
fn on_main(page: &Page, browser: Option<&Browser>, f: impl FnOnce(&Shared) + Send + 'static) {
    let browser = browser.map(ImplBrowser::identifier);
    page.on_main(move |shared| {
        if current(shared, browser) {
            f(shared);
        }
    });
}

/// 主框架开始加载：上一页的出错、崩溃、图标、进度清掉
pub(crate) fn load_started(page: &Page, browser: Option<&Browser>) {
    on_main(page, browser, |shared| {
        aureole::batch(|| {
            shared.load_error.set_if_changed(None);
            shared.crash.set_if_changed(None);
            shared.favicon.set_if_changed(None);
            shared.hovered_link.set_if_changed(String::new());
        });
    });
}

/// 主框架加载出错（中断的不算：停止了、跳到了别处、其实是下载）
pub(crate) fn load_failed(page: &Page, browser: Option<&Browser>, code: Errorcode, text: String, url: String) {
    if code == Errorcode::ABORTED {
        return;
    }
    let error = LoadError { code: code.get_raw(), text, url };
    on_main(page, browser, move |shared| {
        shared.load_error.set_if_changed(Some(error));
    });
}

pub(crate) fn progress(page: &Page, browser: Option<&Browser>, progress: f64) {
    on_main(page, browser, move |shared| {
        shared.progress.set_if_changed(progress.clamp(0.0, 1.0));
    });
}

pub(crate) fn hovered_link(page: &Page, browser: Option<&Browser>, link: String) {
    on_main(page, browser, move |shared| {
        shared.hovered_link.set_if_changed(link);
    });
}

pub(crate) fn tooltip(page: &Page, browser: Option<&Browser>, text: String) {
    on_main(page, browser, move |shared| {
        shared.tooltip.set_if_changed(text);
    });
}

/// 渲染进程退出了
pub(crate) fn crashed(page: &Page, browser: Option<&Browser>, status: TerminationStatus, code: c_int) {
    let crash = Crash { reason: crash_reason(status), code };
    on_main(page, browser, move |shared| {
        aureole::batch(|| {
            shared.loading.set_if_changed(false);
            shared.crash.set_if_changed(Some(crash));
        });
    });
}

fn crash_reason(status: TerminationStatus) -> CrashReason {
    match status {
        TerminationStatus::PROCESS_CRASHED => CrashReason::Crashed,
        TerminationStatus::PROCESS_OOM => CrashReason::OutOfMemory,
        TerminationStatus::PROCESS_WAS_KILLED => CrashReason::Killed,
        _ => CrashReason::Other,
    }
}

/// 一条控制台消息：应用接手时交给它
pub(crate) fn console(page: &Page, level: LogSeverity, message: &str, source: &str, line: c_int) {
    if !page.has_hook(Hook::Console) {
        return;
    }
    let message = ConsoleMessage { level: console_level(level), message: message.to_owned(), source: source.to_owned(), line: line.max(0) as u32 };
    page.on_main(move |shared| {
        let hook = shared.hooks.borrow().console.clone();
        if let Some(hook) = hook {
            hook(message);
        }
    });
}

fn console_level(level: LogSeverity) -> ConsoleLevel {
    if level == LogSeverity::ERROR || level == LogSeverity::FATAL {
        ConsoleLevel::Error
    } else if level == LogSeverity::WARNING {
        ConsoleLevel::Warning
    } else if level == LogSeverity::VERBOSE {
        ConsoleLevel::Debug
    } else {
        ConsoleLevel::Info
    }
}

// ---------- 网站图标 ----------

/// 内核给的字符串列表（回调的参数：借来的，不归这里释放）
fn strings(list: &mut CefStringList) -> Vec<String> {
    let raw: *mut cef::sys::_cef_string_list_t = list.into();
    if raw.is_null() {
        return Vec::new();
    }
    // SAFETY: `raw` 是内核在回调期间给的有效列表；每一项读出来是一份拷贝，转成 Rust 的字符串以后释放
    unsafe {
        (0..cef::sys::cef_string_list_size(raw))
            .filter_map(|i| {
                let mut value: cef::sys::cef_string_t = std::mem::zeroed();
                if cef::sys::cef_string_list_value(raw, i, &mut value) == 0 {
                    return None;
                }
                let text = CefString::from(std::ptr::from_ref(&value)).to_string();
                cef::sys::cef_string_utf16_clear(&mut value);
                Some(text)
            })
            .collect()
    }
}

/// 网站图标的地址变了：让内核下载第一个，解码好交回来（[`FaviconDone`]）；没有图标时清掉
pub(crate) fn favicon_changed(page: &Arc<Page>, browser: Option<&Browser>, urls: Option<&mut CefStringList>) {
    let url = urls.map(strings).unwrap_or_default().into_iter().find(|url| !url.is_empty());
    let Some(url) = url else {
        on_main(page, browser, |shared| {
            shared.favicon.set_if_changed(None);
        });
        return;
    };
    let Some(host) = browser.and_then(ImplBrowser::host) else { return };
    let scale = page.geometry().scale.max(1.0);
    // 标签上显示的大小（16 逻辑像素）的两倍，够清楚
    let max_size = (32.0 * scale).ceil() as u32;
    let mut done = FaviconDone::new(page.clone(), browser.map(ImplBrowser::identifier), scale);
    host.download_image(Some(&CefString::from(url.as_str())), 1, max_size, 0, Some(&mut done));
}

wrap_download_image_callback! {
    struct FaviconDone {
        page: Arc<Page>,
        browser: Option<c_int>,
        scale: f32,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(&self, _image_url: Option<&CefString>, _http_status_code: c_int, image: Option<&mut Image>) {
            let picture = image.and_then(|image| bitmap(image, self.scale));
            let browser = self.browser;
            self.page.on_main(move |shared| {
                if current(shared, browser) {
                    shared.favicon.set_if_changed(picture);
                }
            });
        }
    }
}

/// 内核解码好的图片，取最接近 `scale` 的那一张，换成框架的位图（RGBA，不预乘）
fn bitmap(image: &Image, scale: f32) -> Option<aureole::RgbaImage> {
    let (mut width, mut height) = (0, 0);
    let data: BinaryValue = image.as_bitmap(scale, ColorType::RGBA_8888, AlphaType::POSTMULTIPLIED, Some(&mut width), Some(&mut height))?;
    let mut pixels = vec![0u8; data.size()];
    let read = data.data(Some(&mut pixels), 0);
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    (read == pixels.len() && pixels.len() == width as usize * height as usize * 4 && width > 0).then(|| aureole::RgbaImage::new(width, height, pixels))
}

// ---------- 历史记录 ----------

/// 向内核要一遍历史记录（内核的界面线程上：加载状态、标题变了时），要到了写进信号
pub(crate) fn refresh_history(page: &Arc<Page>, browser: Option<&Browser>) {
    let Some(host) = browser.and_then(ImplBrowser::host) else { return };
    let mut visitor = HistoryVisitor::new(page.clone(), browser.map(ImplBrowser::identifier), Arc::default());
    host.navigation_entries(Some(&mut visitor), 0);
}

wrap_navigation_entry_visitor! {
    struct HistoryVisitor {
        page: Arc<Page>,
        browser: Option<c_int>,
        entries: Arc<Mutex<History>>,
    }

    impl NavigationEntryVisitor {
        fn visit(&self, entry: Option<&mut NavigationEntry>, current: c_int, index: c_int, total: c_int) -> c_int {
            let text = |value: cef::CefStringUserfree| CefString::from(&value).to_string();
            let mut history = lock(&self.entries);
            if let Some(entry) = entry {
                history.entries.push(HistoryEntry { url: text(entry.url()), title: text(entry.title()) });
            }
            if current != 0 {
                history.current = index.max(0) as usize;
            }
            if index + 1 >= total {
                let history = std::mem::take(&mut *history);
                let browser = self.browser;
                self.page.on_main(move |shared| {
                    if self::current(shared, browser) {
                        shared.history.set_if_changed(history);
                    }
                });
            }
            1
        }
    }
}

/// 跳到历史记录里的第 `index` 个：内核没有按序号跳的接口，在页面里走 `history.go`
pub(crate) fn go_to_history(shared: &Shared, index: usize) {
    let history = shared.history.get_untracked();
    if index >= history.entries.len() || index == history.current {
        return;
    }
    let delta = index as i64 - history.current as i64;
    crate::client::eval(shared, &format!("history.go({delta})"));
}

// ---------- 静音 ----------

pub(crate) fn set_muted(shared: &Shared, muted: bool) {
    shared.muted.set_if_changed(muted);
    apply_muted(shared);
}

/// 把记着的静音交给打开着的网页（打开以后、改了以后）
pub(crate) fn apply_muted(shared: &Shared) {
    let host = shared.browser.borrow().as_ref().and_then(ImplBrowser::host);
    if let Some(host) = host {
        host.set_audio_muted(c_int::from(shared.muted.get_untracked()));
    }
}
