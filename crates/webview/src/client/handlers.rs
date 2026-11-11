//! CEF 的各种回调：显示（标题、地址、光标、控制台、图标、悬停的链接、进度、提示文字）、加载状态和出错、网页建好、
//! 新窗口和关闭、右键菜单、认证、证书错误、渲染进程退出。
//!
//! 回调在 CEF 的界面线程上：macOS 上就是框架的主线程，Windows 上是 CEF 自己的线程。回调拿着的是 [`Page`]
//! （能跨线程的那一半），要写信号的经 [`Page::on_main`] 到主线程上做（在主线程上时直接做）。只记日志、
//! 只调 CEF 的在回调的线程上直接做。`Page` 不引用 `Shared`（按编号在主线程的表里找），`Shared` 拿着网页，
//! 网页拿着客户端，不成环：控制器和元素都放掉以后 `Shared` 照常释放，之后的回调什么都不做。
//!
//! 写信号会同步地跑应用的副作用，副作用里可能又调控制器、卸载元素：主线程上不借着 `shared.browser` 调 CEF
//! （克隆出来再调），副作用里的 panic 在这一层接住（macOS 上 panic 穿过 CEF 的 C 接口会直接中止进程）。

use std::any::Any;
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

// 包装回调的宏要用到 CEF 的引用计数的方法
use cef::rc::Rc as _;
use cef::{
    AuthCallback, Browser, BrowserSettings, ResourceRequestHandler, Callback, CefString, CefStringList, Client, ContextMenuHandler, DialogHandler, DownloadHandler, FindHandler, JsdialogHandler, PermissionHandler, ProcessId, ProcessMessage, Request, ImplRequest, ContextMenuParams, CursorInfo, CursorType, DictionaryValue, DisplayHandler, Errorcode, Sslinfo, TransitionType,
    Frame, ImplBrowser, ImplClient, ImplContextMenuHandler, ImplDisplayHandler, ImplFrame, ImplLifeSpanHandler, ImplLoadHandler, ImplMenuModel,
    ImplRequestHandler, LifeSpanHandler, LoadHandler, LogSeverity, MenuModel, PopupFeatures, RenderHandler, RequestHandler, TerminationStatus, WindowInfo,
    WindowOpenDisposition, WrapClient, WrapContextMenuHandler, WrapDisplayHandler, WrapLifeSpanHandler, WrapLoadHandler, WrapRequestHandler,
    wrap_client, wrap_context_menu_handler, wrap_display_handler, wrap_life_span_handler, wrap_load_handler, wrap_request_handler,
};

use crate::state::{Hook, Page, Shared};

/// 网页控制台消息的日志目标：应用可以单独打开、关掉网页的控制台输出
const CONSOLE_TARGET: &str = "aureole_webview::console";

/// 系统光标的句柄（`on_cursor_change` 的参数，各平台的类型不一样；离屏时不用它）
#[cfg(target_os = "macos")]
type CursorHandle = *mut u8;
#[cfg(target_os = "windows")]
type CursorHandle = cef::sys::HCURSOR;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
type CursorHandle = std::os::raw::c_ulong;

/// 给 `page` 的网页建客户端。`render` 是离屏绘制的回调，没有时照样建
pub(super) fn client(page: &Arc<Page>, render: Option<RenderHandler>) -> Client {
    let (jsdialog, dialog, download, permission) = crate::dialogs::handlers(page);
    let find = crate::tools::find_handler(page);
    PageClient::new(
        page.clone(),
        render,
        PageDisplay::new(page.clone()),
        PageLoad::new(page.clone()),
        PageLifeSpan::new(page.clone()),
        PageMenu::new(page.clone()),
        PageRequest::new(page.clone()),
        jsdialog,
        dialog,
        download,
        permission,
        find,
    )
}

wrap_client! {
    struct PageClient {
        page: Arc<Page>,
        render: Option<RenderHandler>,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        menu: ContextMenuHandler,
        request: RequestHandler,
        jsdialog: JsdialogHandler,
        dialog: DialogHandler,
        download: DownloadHandler,
        permission: PermissionHandler,
        find: FindHandler,
    }

    impl Client {
        /// 渲染进程发来的消息：页面发给应用的（`aureole.postMessage`）
        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> c_int {
            c_int::from(crate::bridge::received(&self.page, frame, message))
        }

        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(self.jsdialog.clone())
        }

        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(self.dialog.clone())
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download.clone())
        }

        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permission.clone())
        }

        fn find_handler(&self) -> Option<FindHandler> {
            Some(self.find.clone())
        }

        fn render_handler(&self) -> Option<RenderHandler> {
            self.render.clone()
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }

        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.menu.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
    }
}

wrap_display_handler! {
    struct PageDisplay {
        page: Arc<Page>,
    }

    impl DisplayHandler {
        fn on_address_change(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            // 框架里的子页面跳转不算网页的地址
            if !frame.is_some_and(|frame| frame.is_main() != 0) {
                return;
            }
            let (browser, url) = (id(browser.as_deref()), text(url));
            self.page.on_main(move |shared| {
                guarded("on_address_change", (), || {
                    // 先建成空页面、等用户脚本注册的那一会儿：地址还是要去的那个
                    if is_current(shared, browser) && shared.initial_load.borrow().is_none() {
                        shared.url.set_if_changed(url);
                    }
                })
            });
        }

        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            crate::status::refresh_history(&self.page, browser.as_deref());
            let (browser, title) = (id(browser.as_deref()), text(title));
            self.page.on_main(move |shared| {
                guarded("on_title_change", (), || {
                    if is_current(shared, browser) {
                        shared.title.set_if_changed(title);
                    }
                })
            });
        }

        /// 页面要全屏、退出全屏（视频、`requestFullscreen`）
        fn on_fullscreen_mode_change(&self, browser: Option<&mut Browser>, fullscreen: c_int) {
            let browser = id(browser.as_deref());
            self.page.on_main(move |shared| {
                guarded("on_fullscreen_mode_change", (), || {
                    if is_current(shared, browser) {
                        shared.fullscreen.set_if_changed(fullscreen != 0);
                    }
                })
            });
        }

        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: c_int,
        ) -> c_int {
            guarded("on_console_message", 1, || {
                let (message, source) = (text(message), text(source));
                console(level, &message, &source, line);
                crate::status::console(&self.page, level, &message, &source, line);
                // 已经记进日志了，CEF 自己不再输出一遍
                1
            })
        }

        /// 网站图标的地址变了
        fn on_favicon_urlchange(&self, browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
            guarded("on_favicon_urlchange", (), || crate::status::favicon_changed(&self.page, browser.as_deref(), icon_urls));
        }

        /// 指针停在链接上（状态栏要显示的链接地址），移开时是空的
        fn on_status_message(&self, browser: Option<&mut Browser>, value: Option<&CefString>) {
            crate::status::hovered_link(&self.page, browser.as_deref(), text(value));
        }

        fn on_loading_progress_change(&self, browser: Option<&mut Browser>, progress: f64) {
            crate::status::progress(&self.page, browser.as_deref(), progress);
        }

        /// 页面要显示提示文字：交给应用显示（返回"已处理"，内核不自己弹）
        fn on_tooltip(&self, browser: Option<&mut Browser>, text: Option<&mut CefString>) -> c_int {
            let value = text.map(|text| text.to_string()).unwrap_or_default();
            crate::status::tooltip(&self.page, browser.as_deref(), value);
            1
        }

        fn on_cursor_change(
            &self,
            browser: Option<&mut Browser>,
            _cursor: CursorHandle,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> c_int {
            let browser = id(browser.as_deref());
            self.page.on_main(move |shared| {
                guarded("on_cursor_change", (), || {
                    if is_current(shared, browser) {
                        crate::input::cursor_changed(shared, type_);
                    }
                })
            });
            // 光标由框架换；返回"已处理"，CEF 不去动系统的光标（离屏的网页没有自己的窗口）
            1
        }
    }
}

wrap_load_handler! {
    struct PageLoad {
        page: Arc<Page>,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, browser: Option<&mut Browser>, is_loading: c_int, can_go_back: c_int, can_go_forward: c_int) {
            crate::status::refresh_history(&self.page, browser.as_deref());
            let browser = id(browser.as_deref());
            self.page.on_main(move |shared| {
                guarded("on_loading_state_change", (), || {
                    if !is_current(shared, browser) {
                        return;
                    }
                    // 三个一起变，副作用只跑一次
                    aureole::batch(|| {
                        shared.loading.set_if_changed(is_loading != 0);
                        shared.can_go_back.set_if_changed(can_go_back != 0);
                        shared.can_go_forward.set_if_changed(can_go_forward != 0);
                    });
                })
            });
        }

        /// 主框架开始加载一个新页面：上一页的出错、崩溃、图标清掉
        fn on_load_start(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, _transition_type: TransitionType) {
            if frame.is_some_and(|frame| frame.is_main() != 0) {
                crate::status::load_started(&self.page, browser.as_deref());
            }
        }

        fn on_load_error(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let main = frame.is_some_and(|frame| frame.is_main() != 0);
            let url = text(failed_url);
            if error_code == Errorcode::ABORTED {
                tracing::debug!(url = %url, main, "网页加载中断（停止了、跳到了别处，或者是下载）");
            } else {
                tracing::warn!(url = %url, main, code = error_code.get_raw(), "网页加载失败：{}", text(error_text));
            }
            if main {
                crate::status::load_failed(&self.page, browser.as_deref(), error_code, text(error_text), url);
            }
        }
    }
}

wrap_life_span_handler! {
    struct PageLifeSpan {
        page: Arc<Page>,
    }

    impl LifeSpanHandler {
        /// 网页建好了：交给引擎放进 `shared.browser`（Windows 上网页是异步建的，从这里才拿到）
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser.as_deref().cloned() else { return };
            if let Some(pending) = self.page.take_popup() {
                let page = self.page.clone();
                self.page.run_on_main(move || guarded("on_after_created", (), || crate::popup::adopt(page, browser, pending)));
                return;
            }
            self.page.on_main(move |shared| guarded("on_after_created", (), || crate::engine::created(shared, browser)));
        }

        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: c_int,
            popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            guarded("on_before_popup", 1, || {
                let url = text(target_url);
                // 应用接手新窗口：让内核建，建好交给应用
                if self.page.has_hook(Hook::NewWindow) {
                    return crate::popup::before_popup(
                        &self.page,
                        browser.as_deref(),
                        url,
                        target_disposition,
                        user_gesture != 0,
                        popup_features,
                        window_info,
                        client,
                        settings,
                        extra_info,
                    );
                }
                // 在要开新窗口的那个网页里打开（回调的线程上就能调）
                let target = popup_target(&url, user_gesture != 0);
                let frame = target.and(browser).and_then(|browser| browser.main_frame());
                match (target, frame) {
                    (Some(target), Some(frame)) => frame.load_url(Some(&CefString::from(target))),
                    _ => tracing::debug!(url = %url, user_gesture = user_gesture != 0, "网页要开的新窗口没有打开"),
                }
                // 不开原生的新窗口
                1
            })
        }

        /// 网页要关了。不是引擎关的（引擎关之前已经从 `shared.browser` 拿走了），就是页面自己关的（`window.close()`）：
        /// 告诉应用，元素不再按地址重新打开
        fn do_close(&self, browser: Option<&mut Browser>) -> c_int {
            let Some(browser) = id(browser.as_deref()) else { return 0 };
            self.page.on_main(move |shared| guarded("do_close", (), || closed_by_page(shared, browser)));
            0
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(browser) = id(browser.as_deref()) else { return };
            self.page.on_main(move |shared| guarded("on_before_close", (), || closed(shared, browser)));
        }
    }
}

/// 网页（编号 `browser`）关掉了（主线程上）：引擎关的网页已经从 `shared.browser` 拿走了；网页自己关的
/// （网页里的脚本 `window.close()`）还在里面，拿走，元素再显示时重新打开。里面已经是新打开的网页时不动它
fn closed(shared: &Shared, browser: c_int) {
    let open = shared.browser.try_borrow().ok().and_then(|open| open.clone());
    if let Some(open) = open {
        if open.identifier() != browser {
            return;
        }
        drop(open);
        let closed = shared.browser.try_borrow_mut().ok().and_then(|mut slot| slot.take());
        drop(closed);
    }
    // 历史记录跟着网页没了
    aureole::batch(|| {
        shared.loading.set_if_changed(false);
        shared.can_go_back.set_if_changed(false);
        shared.can_go_forward.set_if_changed(false);
    });
}

/// 页面自己关了（主线程上）：网页还是 `shared` 现在的网页时，记下来（元素不再按地址重新打开），告诉应用
fn closed_by_page(shared: &Shared, browser: c_int) {
    let open = shared.browser.try_borrow().ok().and_then(|open| open.clone());
    if !open.is_some_and(|open| open.identifier() == browser) {
        return;
    }
    shared.closed.set_if_changed(true);
    let hook = shared.hooks.borrow().close_requested.clone();
    if let Some(hook) = hook {
        hook();
    }
}

wrap_context_menu_handler! {
    struct PageMenu {
        page: Arc<Page>,
    }

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            guarded("on_before_context_menu", (), || {
                // 应用接手时把点到了什么交给它，菜单由应用画
                crate::menu::requested(&self.page, params.as_deref());
                // 离屏的网页没有自己的窗口，CEF 的原生菜单弹不到对的地方：清空就不弹
                if let Some(model) = model {
                    model.clear();
                }
            })
        }
    }
}

wrap_request_handler! {
    struct PageRequest {
        page: Arc<Page>,
    }

    impl RequestHandler {
        /// 页面要去一个地址：应用设了过滤（`on_navigate`）时问它，不让去就取消（返回 1）
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            user_gesture: c_int,
            is_redirect: c_int,
        ) -> c_int {
            guarded("on_before_browse", 0, || {
                let Some(request) = request else { return 0 };
                let navigation = crate::Navigation {
                    url: CefString::from(&request.url()).to_string(),
                    main_frame: frame.is_none_or(|frame| frame.is_main() != 0),
                    user_gesture: user_gesture != 0,
                    redirect: is_redirect != 0,
                };
                if self.page.allows(&navigation) {
                    0
                } else {
                    tracing::debug!(url = %navigation.url, "应用不让页面去这个地址");
                    1
                }
            })
        }

        /// 请求发出之前要不要过一遍（这一套数据有请求拦截、改了 User-Agent 时）：在内核的网络线程上
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: c_int,
            _is_download: c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut c_int>,
        ) -> Option<ResourceRequestHandler> {
            crate::intercept::handler(&self.page)
        }

        /// 网站要用户名和密码
        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            is_proxy: c_int,
            host: Option<&CefString>,
            port: c_int,
            realm: Option<&CefString>,
            scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> c_int {
            guarded("auth_credentials", 0, || {
                crate::dialogs::auth(&self.page, text(origin_url), is_proxy != 0, text(host), port, text(realm), text(scheme), callback)
            })
        }

        /// 证书有问题
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            cert_error: Errorcode,
            request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            callback: Option<&mut Callback>,
        ) -> c_int {
            tracing::warn!(url = %text(request_url), code = cert_error.get_raw(), "网站的证书有问题");
            guarded("on_certificate_error", 0, || crate::dialogs::certificate_error(&self.page, cert_error, text(request_url), callback))
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: c_int,
            error_string: Option<&CefString>,
        ) {
            tracing::error!(?status, code = error_code, "网页的渲染进程退出了，重新加载才能恢复：{}", text(error_string));
            crate::status::crashed(&self.page, browser.as_deref(), status, error_code);
        }
    }
}

/// 回调的网页的编号：投递到主线程的只带编号，不带 CEF 的对象（应用退出、CEF 关掉以后才执行的话，放掉对象会出错）
fn id(browser: Option<&Browser>) -> Option<c_int> {
    browser.map(ImplBrowser::identifier)
}

/// 回调来自 `shared` 现在的网页（编号 `browser`），或者网页正在打开、还没放进 `shared.browser`。
/// 元素卸载再挂载时旧网页还在关、新网页已经打开，旧网页关掉之前的回调不能写新网页的状态
fn is_current(shared: &Shared, browser: Option<c_int>) -> bool {
    // 先克隆出来、放开借用再调 CEF
    let open = match shared.browser.try_borrow() {
        Ok(open) => open.clone(),
        Err(_) => return true,
    };
    match (browser, open) {
        (Some(browser), Some(open)) => open.identifier() == browser,
        _ => true,
    }
}

/// 网页要开新窗口（带 `target="_blank"` 的链接、脚本里的 `window.open`）时在同一个视图里打开哪个地址。
/// 用户点出来的才打开；脚本自己弹的（没有用户操作，多半是广告）不理；没有地址或者是空白页的
/// （脚本先开空窗口再往里写内容）在同一个视图里没法照做，也不理
fn popup_target(url: &str, user_gesture: bool) -> Option<&str> {
    let url = url.trim();
    (user_gesture && !url.is_empty() && !url.eq_ignore_ascii_case("about:blank")).then_some(url)
}

/// 网页控制台的一条消息记进日志：`console.error` 记错误，`console.warn` 记警告，`console.log`、`console.info` 记信息，
/// `console.debug` 记调试
fn console(level: LogSeverity, message: &str, source: &str, line: c_int) {
    match console_level(level) {
        tracing::Level::ERROR => tracing::error!(target: CONSOLE_TARGET, source, line, "{message}"),
        tracing::Level::WARN => tracing::warn!(target: CONSOLE_TARGET, source, line, "{message}"),
        tracing::Level::INFO => tracing::info!(target: CONSOLE_TARGET, source, line, "{message}"),
        _ => tracing::debug!(target: CONSOLE_TARGET, source, line, "{message}"),
    }
}

/// 控制台消息的级别对应的日志级别
fn console_level(level: LogSeverity) -> tracing::Level {
    if level == LogSeverity::ERROR || level == LogSeverity::FATAL {
        tracing::Level::ERROR
    } else if level == LogSeverity::WARNING {
        tracing::Level::WARN
    } else if level == LogSeverity::VERBOSE {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    }
}

/// CEF 给的字符串，没有时是空的
fn text(value: Option<&CefString>) -> String {
    value.map(CefString::to_string).unwrap_or_default()
}

/// 跑回调的内容，接住里面的 panic：记一条错误，返回 `fallback`（回调要返回值时，按"已处理"或者默认的处理给）
fn guarded<R>(callback: &'static str, fallback: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        tracing::error!(callback, "网页的回调里 panic 了（多半是写信号时跑的副作用），这一次的回调忽略：{}", panic_message(&*panic));
        fallback
    })
}

/// panic 的信息（`panic!` 的字符串），记日志用
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("（不是字符串）")
}
