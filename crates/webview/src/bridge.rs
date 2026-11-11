//! 页面和应用互发消息。
//!
//! 页面里有一个 `window.aureole`（渲染进程在主框架的脚本环境建好时注入，见 [`helper_app`]）：
//!
//! ```text
//! aureole.postMessage('文字')            // 发给应用；不是字符串的先转成 JSON
//! aureole.onmessage = (event) => { event.data }
//! aureole.addEventListener('message', (event) => { event.data })
//! ```
//!
//! 应用这边：[`crate::WebController::on_message`] 收（主线程上，带着发消息的页面的地址，先看来源再信），
//! [`crate::WebController::post_message`] 发（页面里按收到的顺序交给 `onmessage` 和登记的处理器）。
//! 页面发来的在渲染进程里变成进程间消息（`aureole.message`），到浏览器进程的客户端（[`received`]）再投递到主线程。
//!
//! 子进程程序的应用对象（[`helper_app`]）同时登记 `app://` 协议（见 `scheme`）：自定义协议要在每个进程里登记。

use std::os::raw::c_int;
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::sys::cef_process_id_t;
use cef::{
    App, Browser, CefString, Frame, ImplApp, ImplFrame, ImplListValue, ImplProcessMessage, ImplRenderProcessHandler, ImplV8Context, ImplV8Handler,
    ImplV8Value, ProcessId, ProcessMessage, RenderProcessHandler, SchemeRegistrar, V8Context, V8Handler, V8Value, WrapApp, WrapRenderProcessHandler,
    WrapV8Handler, wrap_app, wrap_render_process_handler, wrap_v8_handler,
};

use crate::state::{Page, Shared};

/// 页面发给应用的进程间消息的名字
const MESSAGE: &str = "aureole.message";

/// 注入页面的脚本：参数是渲染进程里的发送函数，建好 `window.aureole`（已经有了就不动）
const BRIDGE: &str = r#"(function (send) {
  if (window.aureole) return;
  const listeners = new Set();
  const bridge = {
    postMessage(message) { send(typeof message === 'string' ? message : JSON.stringify(message)); },
    addEventListener(type, listener) { if (type === 'message' && typeof listener === 'function') listeners.add(listener); },
    removeEventListener(type, listener) { if (type === 'message') listeners.delete(listener); },
    onmessage: null,
  };
  Object.defineProperty(bridge, '__deliver', {
    value(data) {
      const event = { data };
      if (typeof bridge.onmessage === 'function') bridge.onmessage(event);
      for (const listener of [...listeners]) listener(event);
    },
  });
  Object.defineProperty(window, 'aureole', { value: bridge, enumerable: false });
})"#;

/// 页面发来的一条消息（[`crate::WebController::on_message`]）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebMessage {
    text: String,
    url: String,
}

impl WebMessage {
    /// 页面发的文字（`postMessage` 的不是字符串时是它的 JSON）
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 发消息的页面的地址：先看来源再信它（网页视图打开的任何页面都能发）
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// 浏览器进程收到渲染进程发来的消息（客户端的回调，CEF 的界面线程上）：页面发给应用的投递到主线程。是这个模块的消息时返回真
pub(crate) fn received(page: &Arc<Page>, frame: Option<&mut Frame>, message: Option<&mut ProcessMessage>) -> bool {
    let Some(message) = message else { return false };
    if CefString::from(&message.name()).to_string() != MESSAGE {
        return false;
    }
    let text = message.argument_list().map(|args| CefString::from(&args.string(0)).to_string()).unwrap_or_default();
    let url = frame.map(|frame| CefString::from(&frame.url()).to_string()).unwrap_or_default();
    page.on_main(move |shared| {
        let handler = shared.hooks.borrow().message.clone();
        if let Some(handler) = handler {
            handler(WebMessage { text, url });
        }
    });
    true
}

/// 发给页面：在主框架里把 `text` 交给 `window.aureole`（页面还没建好、没有主框架时丢掉）
pub(crate) fn post(shared: &Shared, text: &str) {
    let frame = shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::main_frame);
    if let Some(frame) = frame {
        let script = format!("window.aureole&&window.aureole.__deliver({})", json_string(text));
        frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
    }
}

/// 写成 JSON 的字符串字面量（也是合法的脚本字符串：行分隔符也转义）
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------- 子进程（渲染进程）里 ----------

/// 子进程程序的应用对象：登记 `app://` 协议，渲染进程里给主框架注入 `window.aureole`
pub(crate) fn helper_app() -> App {
    HelperApp::new(Renderer::new())
}

wrap_app! {
    struct HelperApp {
        renderer: RenderProcessHandler,
    }

    impl App {
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            crate::scheme::register(registrar);
        }

        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(self.renderer.clone())
        }
    }
}

wrap_render_process_handler! {
    struct Renderer;

    impl RenderProcessHandler {
        fn on_context_created(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, context: Option<&mut V8Context>) {
            let (Some(frame), Some(context)) = (frame, context) else { return };
            // 只给主框架：嵌进来的框架（广告、第三方的页面）不能直接给应用发消息
            if frame.is_main() != 0 {
                inject(context);
            }
        }
    }
}

/// 在 `context` 里建 `window.aureole`：把发送函数交给注入的脚本
fn inject(context: &mut V8Context) {
    let Some(send) = cef::v8_value_create_function(Some(&CefString::from("send")), Some(&mut Sender::new())) else { return };
    let mut factory: Option<V8Value> = None;
    let mut exception = None;
    let source = CefString::from("aureole://bridge");
    if context.eval(Some(&CefString::from(BRIDGE)), Some(&source), 1, Some(&mut factory), Some(&mut exception)) == 0 {
        return;
    }
    if let Some(factory) = factory {
        factory.execute_function(None, Some(&[Some(send)]));
    }
}

wrap_v8_handler! {
    struct Sender;

    impl V8Handler {
        /// `aureole.postMessage` 里调的发送函数：第一个参数是字符串，发给浏览器进程
        fn execute(
            &self,
            _name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            exception: Option<&mut CefString>,
        ) -> c_int {
            let text = arguments.and_then(|a| a.first()).and_then(Option::as_ref).filter(|v| v.is_string() != 0).map(|v| CefString::from(&v.string_value()));
            let Some(text) = text else {
                if let Some(exception) = exception {
                    *exception = CefString::from("aureole.postMessage 要一个参数");
                }
                return 1;
            };
            let frame = cef::v8_context_get_current_context().and_then(|context| context.frame());
            let message = cef::process_message_create(Some(&CefString::from(MESSAGE)));
            if let (Some(frame), Some(mut message)) = (frame, message) {
                if let Some(args) = message.argument_list() {
                    args.set_string(0, Some(&text));
                }
                frame.send_process_message(ProcessId::from(cef_process_id_t::PID_BROWSER), Some(&mut message));
            }
            1
        }
    }
}
