//! 一套配置（[`crate::WebProfile`]）的 Cookie：读、写、删。内核在它的界面线程上做，做完回调，结果经 [`crate::Reply`] 交回来。
//!
//! 读的时候内核每个 Cookie 回调一次，一个都没有时一次都不回调：收集的对象（[`Collected`]）在内核放掉访问者时答复，
//! 所以没有 Cookie、读不了的时候也有结果（空的）。

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use cef::rc::Rc as _;
use cef::{
    CefString, CompletionCallback, CookieVisitor, DeleteCookiesCallback, ImplCompletionCallback, ImplCookieManager, ImplCookieVisitor,
    ImplDeleteCookiesCallback, ImplRequestContext, ImplSetCookieCallback, SetCookieCallback, WrapCompletionCallback, WrapCookieVisitor,
    WrapDeleteCookiesCallback, WrapSetCookieCallback, wrap_completion_callback, wrap_cookie_visitor, wrap_delete_cookies_callback,
    wrap_set_cookie_callback,
};

use crate::reply::{Reply, Responder, reply};

/// 一个 Cookie（[`crate::WebProfile::cookies`]、[`crate::WebProfile::set_cookie`]）
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cookie {
    /// 名字
    pub name: String,
    /// 值
    pub value: String,
    /// 域名（空的是只给设它的那个主机）
    pub domain: String,
    /// 路径（空的是 `/`）
    pub path: String,
    /// 只在 HTTPS 上发
    pub secure: bool,
    /// 页面里的脚本读不到
    pub http_only: bool,
    /// 过期的时刻；`None` 是会话 Cookie（关掉应用就没了）
    pub expires: Option<SystemTime>,
}

impl Cookie {
    /// 名字是 `name`、值是 `value` 的会话 Cookie
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into(), ..Self::default() }
    }
}

/// 内核的时间是从 1601 年起的微秒数
const WINDOWS_EPOCH_TO_UNIX: Duration = Duration::from_secs(11_644_473_600);

fn from_basetime(time: &cef::Basetime) -> Option<SystemTime> {
    let since_1601 = Duration::from_micros(u64::try_from(time.val).ok()?);
    SystemTime::UNIX_EPOCH.checked_add(since_1601.checked_sub(WINDOWS_EPOCH_TO_UNIX)?)
}

fn to_basetime(time: SystemTime) -> cef::Basetime {
    let since_unix = time.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    cef::Basetime { val: i64::try_from((since_unix + WINDOWS_EPOCH_TO_UNIX).as_micros()).unwrap_or(i64::MAX) }
}

fn read(cookie: &cef::Cookie) -> Cookie {
    Cookie {
        name: cookie.name.to_string(),
        value: cookie.value.to_string(),
        domain: cookie.domain.to_string(),
        path: cookie.path.to_string(),
        secure: cookie.secure != 0,
        http_only: cookie.httponly != 0,
        expires: (cookie.has_expires != 0).then(|| from_basetime(&cookie.expires)).flatten(),
    }
}

fn write(cookie: &Cookie) -> cef::Cookie {
    cef::Cookie {
        name: CefString::from(cookie.name.as_str()),
        value: CefString::from(cookie.value.as_str()),
        domain: CefString::from(cookie.domain.as_str()),
        path: CefString::from(cookie.path.as_str()),
        secure: i32::from(cookie.secure),
        httponly: i32::from(cookie.http_only),
        has_expires: i32::from(cookie.expires.is_some()),
        expires: cookie.expires.map(to_basetime).unwrap_or_default(),
        ..Default::default()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 读到的 Cookie：放掉时（内核放掉访问者）把收集到的答复出去
struct Collected {
    cookies: Mutex<Vec<Cookie>>,
    responder: Mutex<Option<Responder<Vec<Cookie>>>>,
}

impl Drop for Collected {
    fn drop(&mut self) {
        if let Some(responder) = lock(&self.responder).take() {
            responder.respond(std::mem::take(&mut *lock(&self.cookies)));
        }
    }
}

wrap_cookie_visitor! {
    struct Visitor {
        collected: Arc<Collected>,
    }

    impl CookieVisitor {
        fn visit(&self, cookie: Option<&cef::Cookie>, _count: std::os::raw::c_int, _total: std::os::raw::c_int, _delete_cookie: Option<&mut std::os::raw::c_int>) -> std::os::raw::c_int {
            if let Some(cookie) = cookie {
                lock(&self.collected.cookies).push(read(cookie));
            }
            1
        }
    }
}

wrap_set_cookie_callback! {
    struct SetDone {
        responder: Arc<Mutex<Option<Responder<bool>>>>,
    }

    impl SetCookieCallback {
        fn on_complete(&self, success: std::os::raw::c_int) {
            if let Some(responder) = lock(&self.responder).take() {
                responder.respond(success != 0);
            }
        }
    }
}

wrap_delete_cookies_callback! {
    struct DeleteDone {
        responder: Arc<Mutex<Option<Responder<u32>>>>,
    }

    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: std::os::raw::c_int) {
            if let Some(responder) = lock(&self.responder).take() {
                responder.respond(u32::try_from(num_deleted).unwrap_or(0));
            }
        }
    }
}

wrap_completion_callback! {
    struct Done {
        responder: Arc<Mutex<Option<Responder<()>>>>,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            if let Some(responder) = lock(&self.responder).take() {
                responder.respond(());
            }
        }
    }
}

/// 内核做完一件事时答复 `responder` 的回调（清缓存这些）
pub(crate) fn done(responder: Responder<()>) -> CompletionCallback {
    Done::new(Arc::new(Mutex::new(Some(responder))))
}

impl crate::WebProfile {
    /// 这一套的 Cookie：`url` 给了时只要发给这个地址的（包括页面脚本读不到的），没给时全部。内核没启动时先启动
    pub fn cookies(&self, url: Option<&str>) -> Reply<Vec<Cookie>> {
        let (reply, responder) = reply();
        let manager = self.started_context().and_then(|context| context.cookie_manager(None));
        let Some(manager) = manager else { return reply };
        let collected = Arc::new(Collected { cookies: Mutex::new(Vec::new()), responder: Mutex::new(Some(responder)) });
        let mut visitor = Visitor::new(collected);
        match url {
            Some(url) => manager.visit_url_cookies(Some(&CefString::from(url)), 1, Some(&mut visitor)),
            None => manager.visit_all_cookies(Some(&mut visitor)),
        };
        reply
    }

    /// 给 `url` 设一个 Cookie（域名、路径没写时按 `url` 的）；地址不对、Cookie 不合法时答复 `false`
    pub fn set_cookie(&self, url: &str, cookie: &Cookie) -> Reply<bool> {
        let (reply, responder) = reply();
        let manager = self.started_context().and_then(|context| context.cookie_manager(None));
        let Some(manager) = manager else { return reply };
        let mut done = SetDone::new(Arc::new(Mutex::new(Some(responder))));
        manager.set_cookie(Some(&CefString::from(url)), Some(&write(cookie)), Some(&mut done));
        reply
    }

    /// 删 Cookie：`url`、`name` 都没给时全删，只给 `url` 时删发给这个地址的，都给时只删这一个。答复删了几个
    pub fn delete_cookies(&self, url: Option<&str>, name: Option<&str>) -> Reply<u32> {
        let (reply, responder) = reply();
        let manager = self.started_context().and_then(|context| context.cookie_manager(None));
        let Some(manager) = manager else { return reply };
        let mut done = DeleteDone::new(Arc::new(Mutex::new(Some(responder))));
        let (url, name) = (url.map(CefString::from), name.map(CefString::from));
        manager.delete_cookies(url.as_ref(), name.as_ref(), Some(&mut done));
        reply
    }
}
