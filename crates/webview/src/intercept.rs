//! 请求发出之前过一遍（[`crate::WebProfile::set_interceptor`]）：应用能拦掉、改地址、改请求头；配置改了 User-Agent、
//! 语言的，请求头在这里换。
//!
//! 内核在它的网络线程上、每个请求发出之前问一次页面要不要过一遍（`resource_request_handler`），配置既没拦截也没改
//! User-Agent 时不过（没有开销）。

use std::sync::Arc;

use cef::rc::Rc as _;
use cef::{
    Browser, Callback, CefString, Frame, ImplRequest, ImplResourceRequestHandler, Request, ResourceRequestHandler, ResourceType, ReturnValue, WrapResourceRequestHandler,
    wrap_resource_request_handler,
};

use crate::state::Page;

/// 拦截请求的函数（在内核的网络线程上调用）
pub(crate) type Interceptor = dyn Fn(&mut InterceptedRequest) + Send + Sync;

/// 请求要的是什么
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceKind {
    /// 页面本身
    MainFrame,
    /// 嵌进来的框架
    SubFrame,
    /// 样式表
    Stylesheet,
    /// 脚本
    Script,
    /// 图片
    Image,
    /// 字体
    Font,
    /// 音频、视频
    Media,
    /// 脚本发的请求（`fetch`、`XMLHttpRequest`）
    Fetch,
    /// 别的（预取、`ping`、Service Worker……）
    Other,
}

fn kind(resource: ResourceType) -> ResourceKind {
    match resource {
        ResourceType::MAIN_FRAME => ResourceKind::MainFrame,
        ResourceType::SUB_FRAME => ResourceKind::SubFrame,
        ResourceType::STYLESHEET => ResourceKind::Stylesheet,
        ResourceType::SCRIPT => ResourceKind::Script,
        ResourceType::IMAGE | ResourceType::FAVICON => ResourceKind::Image,
        ResourceType::FONT_RESOURCE => ResourceKind::Font,
        ResourceType::MEDIA => ResourceKind::Media,
        ResourceType::XHR => ResourceKind::Fetch,
        _ => ResourceKind::Other,
    }
}

/// 要发出去的一个请求（[`crate::WebProfile::set_interceptor`]）：读它，拦掉、改地址、改请求头
pub struct InterceptedRequest<'a> {
    request: &'a Request,
    url: String,
    method: String,
    kind: ResourceKind,
    blocked: bool,
    redirect: Option<String>,
}

impl InterceptedRequest<'_> {
    /// 地址
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 方法（`GET`、`POST`……）
    pub fn method(&self) -> &str {
        &self.method
    }

    /// 要的是什么
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }

    /// 请求头 `name` 的值（没有时是空的）
    pub fn header(&self, name: &str) -> String {
        CefString::from(&self.request.header_by_name(Some(&CefString::from(name)))).to_string()
    }

    /// 设请求头（有同名的换掉）
    pub fn set_header(&mut self, name: &str, value: &str) {
        self.request.set_header_by_name(Some(&CefString::from(name)), Some(&CefString::from(value)), 1);
    }

    /// 不发了（页面看到的是请求失败）
    pub fn block(&mut self) {
        self.blocked = true;
    }

    /// 改成请求 `url`（页面看到的是重定向）
    pub fn redirect(&mut self, url: &str) {
        self.redirect = Some(url.to_owned());
    }
}

/// 内核的网络线程上：这个页面的请求要不要过一遍
pub(crate) fn handler(page: &Arc<Page>) -> Option<ResourceRequestHandler> {
    page.profile().watches_requests().then(|| PageResources::new(page.clone()))
}

wrap_resource_request_handler! {
    struct PageResources {
        page: Arc<Page>,
    }

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let Some(request) = request else { return ReturnValue::CONTINUE };
            let profile = self.page.profile();
            if let Some(user_agent) = profile.user_agent() {
                request.set_header_by_name(Some(&CefString::from("User-Agent")), Some(&CefString::from(user_agent.as_str())), 1);
            }
            if let Some(languages) = profile.accept_languages() {
                request.set_header_by_name(Some(&CefString::from("Accept-Language")), Some(&CefString::from(languages.as_str())), 1);
            }
            let Some(intercept) = profile.interceptor() else { return ReturnValue::CONTINUE };
            let request: &Request = request;
            let mut intercepted = InterceptedRequest {
                request,
                url: CefString::from(&request.url()).to_string(),
                method: CefString::from(&request.method()).to_string(),
                kind: kind(request.resource_type()),
                blocked: false,
                redirect: None,
            };
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| intercept(&mut intercepted)));
            if caught.is_err() {
                tracing::error!(url = %intercepted.url, "拦截请求的函数 panic 了，请求照常发");
                return ReturnValue::CONTINUE;
            }
            if intercepted.blocked {
                return ReturnValue::CANCEL;
            }
            if let Some(url) = intercepted.redirect.take() {
                request.set_url(Some(&CefString::from(url.as_str())));
            }
            ReturnValue::CONTINUE
        }
    }
}
