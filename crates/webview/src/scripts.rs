//! 用户脚本（[`crate::UserScript`]）：在浏览器进程这边经 DevTools 协议注册（`Page.addScriptToEvaluateOnNewDocument`）。
//!
//! 注册跟着页面走：跳到别的网站、换了渲染进程也照样在每个新文档开始时跑（不能放在渲染进程里记：跨站跳转换进程时，
//! 新进程只拿得到建页面时的那一份）。内核给每段注册的脚本一个编号（命令的结果，经 DevTools 消息的观察者交回来），
//! 改了、删了的按编号撤掉再注册。
//!
//! 页面刚建好时注册：建页面时已经有脚本的，先加载空页面（`about:blank`），注册的结果都回来了再加载要去的地址
//! （命令是异步的，发出去就加载的话主框架的文档会先建好），第一个文档也赶得上；结果一直不来时最多等 [`READY_TIMEOUT`]；
//! 内核替页面开的新窗口一建好就在加载，注册时让脚本在已经有的文档里也跑一次。
//!
//! 只在主框架里跑的脚本外面包一层判断；文档解析完时跑的包在一个函数里，等 `DOMContentLoaded`。
//! 和页面同一个渲染进程的框架都跑得到，不同进程的（跨站的 iframe）跑不到。

use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use cef::rc::Rc as _;
use cef::{
    Browser, CefString, DevToolsMessageObserver, ImplBrowser, ImplBrowserHost, ImplDevToolsMessageObserver, ImplDictionaryValue,
    WrapDevToolsMessageObserver, wrap_dev_tools_message_observer,
};

use crate::profile::{ScriptTime, UserScript};
use crate::state::{Page, Shared};

/// 注册过的一段脚本
pub(crate) struct Registered {
    script: UserScript,
    /// 注册命令的编号（等结果用）
    message: c_int,
    /// 内核给的编号（结果回来以前没有）
    identifier: Option<String>,
    /// 结果回来以前就不要了：回来时撤掉
    dropped: bool,
}

/// DevTools 命令的编号：自己分配，从很大的数起，不和内核自动分配的撞（内核在别的线程上被调用时不返回编号）
static NEXT_MESSAGE: AtomicI32 = AtomicI32::new(1 << 24);

/// 先建成空页面的，最多等注册的结果这么久就加载
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn host(shared: &Shared) -> Option<cef::BrowserHost> {
    shared.browser.borrow().as_ref().and_then(ImplBrowser::host)
}

/// 页面要的用户脚本：配置的，加上页面自己的（同名时用页面的）
pub(crate) fn wanted(shared: &Shared) -> Vec<UserScript> {
    crate::profile::scripts_for(shared.page.profile(), &shared.page.scripts())
}

/// 包好的源码：只在主框架里跑的加判断，文档解析完时跑的等 `DOMContentLoaded`
fn wrapped(script: &UserScript) -> String {
    let source = match script.time() {
        ScriptTime::DocumentStart => script.source().to_owned(),
        ScriptTime::DocumentEnd => format!(
            "(function () {{ const run = () => {{\n{}\n}}; if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', run, {{ once: true }}); else run(); }})();",
            script.source()
        ),
    };
    if script.all_frames() { source } else { format!("if (window === window.top) {{\n{source}\n}}") }
}

/// 发一条 DevTools 命令（不打开开发者工具），返回命令的编号。所有的命令都经过这里，编号才不会撞
pub(crate) fn send(host: &cef::BrowserHost, method: &str, params: &[(&str, Param)]) -> Option<c_int> {
    let dict = cef::dictionary_value_create()?;
    for (key, value) in params {
        let key = CefString::from(*key);
        match value {
            Param::Text(text) => dict.set_string(Some(&key), Some(&CefString::from(text.as_str()))),
            Param::Bool(flag) => dict.set_bool(Some(&key), c_int::from(*flag)),
        };
    }
    let message = NEXT_MESSAGE.fetch_add(1, Ordering::Relaxed);
    let mut dict = dict;
    // 不在内核的界面线程上调用时内核投递过去、返回 0：照样会执行，结果按这个编号回来
    host.execute_dev_tools_method(message, Some(&CefString::from(method)), Some(&mut dict));
    Some(message)
}

/// DevTools 命令的参数
pub(crate) enum Param {
    Text(String),
    Bool(bool),
}

/// 页面建好了（`engine::created`）：装上 DevTools 结果的观察者，注册脚本。`immediately` 时让脚本在已经有的文档里也跑一次
/// （内核替页面开的新窗口：一建好就在加载）
pub(crate) fn attach(shared: &Shared, host: &cef::BrowserHost, immediately: bool) {
    shared.scripts_registered.borrow_mut().clear();
    let mut observer = ResultObserver::new(shared.page.clone());
    *shared.devtools_observer.borrow_mut() = host.add_dev_tools_message_observer(Some(&mut observer));
    // 注册的脚本要页面的 DevTools 代理开着才往新文档里注入
    send(host, "Page.enable", &[]);
    sync_with(shared, immediately);
    if shared.initial_load.borrow().is_some() {
        load_when_ready(shared);
        // 结果一直不来时也加载
        let page = Arc::downgrade(&shared.page);
        aureole::after(READY_TIMEOUT, move || {
            if let Some(page) = page.upgrade() {
                page.on_main(|shared| {
                    if let Some(url) = shared.initial_load.borrow_mut().take() {
                        tracing::debug!("等不到用户脚本注册的结果，照样加载");
                        load(shared, &url);
                    }
                });
            }
        });
    }
}

/// 先建成空页面的：注册的结果都回来了就加载要去的地址
fn load_when_ready(shared: &Shared) {
    let ready = shared.scripts_registered.borrow().iter().all(|r| r.identifier.is_some() || r.dropped);
    if !ready {
        return;
    }
    let url = shared.initial_load.borrow_mut().take();
    if let Some(url) = url {
        load(shared, &url);
    }
}

fn load(shared: &Shared, url: &str) {
    let frame = shared.browser.borrow().as_ref().and_then(ImplBrowser::main_frame);
    if let Some(frame) = frame {
        cef::ImplFrame::load_url(&frame, Some(&CefString::from(url)));
    }
}

/// 用户脚本改了（主线程上）：撤掉不要的、注册新的
pub(crate) fn sync(shared: &Shared) {
    sync_with(shared, false);
}

fn sync_with(shared: &Shared, immediately: bool) {
    let Some(host) = host(shared) else { return };
    let wanted = wanted(shared);
    let mut registered = shared.scripts_registered.borrow_mut();
    // 不要了的、改了的撤掉（结果还没回来的，回来时撤）
    for entry in registered.iter_mut().filter(|r| !r.dropped && !wanted.contains(&r.script)) {
        entry.dropped = true;
        if let Some(identifier) = &entry.identifier {
            send(&host, "Page.removeScriptToEvaluateOnNewDocument", &[("identifier", Param::Text(identifier.clone()))]);
        }
    }
    registered.retain(|r| !(r.dropped && r.identifier.is_some()));
    for script in wanted {
        if registered.iter().any(|r| !r.dropped && r.script == script) {
            continue;
        }
        let params = [("source", Param::Text(wrapped(&script))), ("runImmediately", Param::Bool(immediately))];
        if let Some(message) = send(&host, "Page.addScriptToEvaluateOnNewDocument", &params) {
            registered.push(Registered { script, message, identifier: None, dropped: false });
        }
    }
}

/// 注册命令的结果回来了（主线程上）：记下内核给的编号；已经不要了的撤掉
fn registered(shared: &Shared, message: c_int, identifier: Option<String>) {
    let Some(host) = host(shared) else { return };
    let mut registered = shared.scripts_registered.borrow_mut();
    let Some(index) = registered.iter().position(|r| r.message == message) else { return };
    let Some(identifier) = identifier else {
        tracing::warn!(name = registered[index].script.name(), "内核没收下用户脚本");
        registered.remove(index);
        drop(registered);
        load_when_ready(shared);
        return;
    };
    if registered[index].dropped {
        send(&host, "Page.removeScriptToEvaluateOnNewDocument", &[("identifier", Param::Text(identifier))]);
        registered.remove(index);
    } else {
        registered[index].identifier = Some(identifier);
    }
    drop(registered);
    load_when_ready(shared);
}

/// 结果里的 `"identifier":"…"`
fn identifier(result: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(result).ok()?;
    let rest = &text[text.find("\"identifier\"")? + "\"identifier\"".len()..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(rest[..rest.find('"')?].to_owned())
}

wrap_dev_tools_message_observer! {
    struct ResultObserver {
        page: Arc<Page>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(&self, _browser: Option<&mut Browser>, message_id: c_int, success: c_int, result: Option<&[u8]>) {
            let identifier = if success != 0 { result.and_then(identifier) } else { None };
            if success == 0 {
                tracing::debug!(message_id, result = %String::from_utf8_lossy(result.unwrap_or_default()), "DevTools 命令失败");
            }
            self.page.on_main(move |shared| registered(shared, message_id, identifier));
        }
    }
}
