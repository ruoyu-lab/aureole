//! 配置（一套数据）：Cookie、缓存、本地存储、证书例外、权限……各页面按用的配置分开。
//!
//! 三种：默认的（[`WebProfile::default`]，数据在应用的数据目录里，见 `engine::data_dir`）、按名字的
//! （[`WebProfile::named`]，数据在 `<数据目录>/profiles/<名字>`）、隐身的（[`WebProfile::incognito`]，数据只放内存，
//! 最后一个用它的页面关掉就没了）。每套对应内核的一个请求上下文，第一次用到时建（内核要先启动）。
//!
//! 每套各自的：语言（请求头和 `navigator.languages`）、代理、User-Agent、请求拦截（`intercept`）、用户脚本（`scripts`）。
//! 代理是请求上下文的偏好，只能在内核的界面线程上改（Windows 上是内核自己的线程，投递过去）；语言、User-Agent
//! 在请求头里改（拦截的地方），页面里的 `navigator.language`、`navigator.userAgent` 经 DevTools 协议改
//! （内核不按偏好改语言：它先看建这一套时给的，再看全局的）。隐身的一套用默认那一套的代理（内核这样定的）。
//!
//! 新窗口用打开它的页面的那一套。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use cef::rc::Rc as _;
use cef::{CefString, ImplBrowser, ImplDictionaryValue, ImplPreferenceManager, ImplTask, ImplValue, Task, ThreadId, WrapTask, wrap_task};

use crate::intercept::Interceptor;
use crate::state::Shared;

/// 用户脚本什么时候跑
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptTime {
    /// 页面的脚本跑之前（文档刚建好，`document.body` 还没有）
    #[default]
    DocumentStart,
    /// 文档解析完（`DOMContentLoaded`）；脚本包在一个函数里跑
    DocumentEnd,
}

/// 注入页面的一段脚本（[`WebProfile::add_user_script`]、[`crate::WebController::add_user_script`]）：
/// 在页面自己的环境里跑（和页面的脚本共用全局对象）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserScript {
    name: String,
    source: String,
    time: ScriptTime,
    all_frames: bool,
}

impl UserScript {
    /// 名字为 `name`（同名的换掉，按名字删）的脚本，默认在页面的脚本之前、只在主框架里跑
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self { name: name.into(), source: source.into(), time: ScriptTime::DocumentStart, all_frames: false }
    }

    /// 什么时候跑
    pub fn at(mut self, time: ScriptTime) -> Self {
        self.time = time;
        self
    }

    /// 嵌进来的框架里也跑
    pub fn in_all_frames(mut self) -> Self {
        self.all_frames = true;
        self
    }

    /// 名字
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 源码
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 什么时候跑
    pub fn time(&self) -> ScriptTime {
        self.time
    }

    /// 嵌进来的框架里也跑
    pub fn all_frames(&self) -> bool {
        self.all_frames
    }
}

/// 代理（[`WebProfile::set_proxy`]）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Proxy {
    /// 跟着系统的设置（默认）
    System,
    /// 不用代理
    Direct,
    /// 自动检测（WPAD）
    AutoDetect,
    /// 用这些代理服务器：`servers` 写成 `host:port`、`http=host:port;https=host:port`、`socks5://host:port` 这样，
    /// `bypass` 是不走代理的地址（逗号隔开，比如 `localhost,*.example.com`）
    Servers {
        /// 代理服务器
        servers: String,
        /// 不走代理的地址
        bypass: String,
    },
    /// 按 PAC 脚本选
    Pac(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Default,
    Named(String),
    Incognito,
}

struct Inner {
    id: u64,
    kind: Kind,
    /// 内核的请求上下文：第一次用到时建
    context: Mutex<Option<cef::RequestContext>>,
    languages: Mutex<Vec<String>>,
    proxy: Mutex<Option<Proxy>>,
    user_agent: Mutex<Option<String>>,
    interceptor: Mutex<Option<Arc<Interceptor>>>,
    scripts: Mutex<Vec<UserScript>>,
}

/// 一套数据（[`crate::WebController::with_profile`]）。克隆出来的是同一套；在任何线程上都能拿着，改设置、读写 Cookie
/// 在主线程上
#[derive(Clone)]
pub struct WebProfile {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for WebProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WebProfile").field(&self.inner.kind).finish()
    }
}

impl PartialEq for WebProfile {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for WebProfile {
    /// 默认的那一套：数据在应用的数据目录里，没指定配置的页面都用它
    fn default() -> Self {
        static DEFAULT: OnceLock<WebProfile> = OnceLock::new();
        DEFAULT.get_or_init(|| Self::new(Kind::Default)).clone()
    }
}

impl WebProfile {
    fn new(kind: Kind) -> Self {
        Self {
            inner: Arc::new(Inner {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                kind,
                context: Mutex::new(None),
                languages: Mutex::new(Vec::new()),
                proxy: Mutex::new(None),
                user_agent: Mutex::new(None),
                interceptor: Mutex::new(None),
                scripts: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 按名字的一套：数据存在 `<数据目录>/profiles/<名字>`，应用下次启动还在。同一个名字拿到的是同一套。
    /// 名字里英文字母、数字、`-`、`_` 以外的字换成 `_`
    pub fn named(name: &str) -> Self {
        static NAMED: OnceLock<Mutex<HashMap<String, WebProfile>>> = OnceLock::new();
        let name = sanitize(name);
        lock(NAMED.get_or_init(Default::default)).entry(name.clone()).or_insert_with(|| Self::new(Kind::Named(name))).clone()
    }

    /// 隐身的一套：数据只放内存，不写磁盘。每次调用是新的一套
    pub fn incognito() -> Self {
        Self::new(Kind::Incognito)
    }

    /// 是隐身的
    pub fn is_incognito(&self) -> bool {
        self.inner.kind == Kind::Incognito
    }

    /// 名字（默认的是空的，隐身的是 `None`）
    pub fn name(&self) -> Option<&str> {
        match &self.inner.kind {
            Kind::Default => Some(""),
            Kind::Named(name) => Some(name),
            Kind::Incognito => None,
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.inner.id
    }

    /// 内核的请求上下文（内核在跑时）：第一次要时建，建好时登记 `app://`、设语言和代理。默认的那一套是内核全局的
    pub(crate) fn context(&self) -> Option<cef::RequestContext> {
        let mut slot = lock(&self.inner.context);
        if slot.is_none() {
            let context = match &self.inner.kind {
                Kind::Default => cef::request_context_get_global_context(),
                Kind::Named(name) => {
                    let dir = crate::engine::profile_dir(name);
                    if dir.is_none() {
                        tracing::warn!(name, "找不到数据目录，这一套配置的数据只放内存");
                    }
                    create(dir.as_deref(), &lock(&self.inner.languages))
                }
                Kind::Incognito => create(None, &lock(&self.inner.languages)),
            };
            if let Some(context) = &context {
                if self.inner.kind != Kind::Default {
                    crate::scheme::install_for(context);
                }
                if let Some(proxy) = lock(&self.inner.proxy).clone() {
                    apply_proxy(context, proxy);
                }
            }
            *slot = context;
        }
        slot.clone()
    }

    /// 页面请求头里的语言、`navigator.languages`（按偏好的先后，如 `["zh-CN", "en-US"]`）。空的是跟着系统。
    /// 打开着的页面下次请求、下次加载时生效
    pub fn set_languages(&self, languages: &[&str]) {
        *lock(&self.inner.languages) = languages.iter().map(|l| (*l).to_owned()).collect();
        for shared in self.pages() {
            apply_overrides(&shared);
        }
    }

    /// 请求头里的语言（没设时 `None`）
    pub(crate) fn accept_languages(&self) -> Option<String> {
        let languages = lock(&self.inner.languages);
        (!languages.is_empty()).then(|| crate::engine::accept_languages(&languages))
    }

    /// 代理。之后的请求按它走（已经连上的连接不变）。隐身的一套跟着默认那一套的代理，在它上面设不起作用
    pub fn set_proxy(&self, proxy: Proxy) {
        *lock(&self.inner.proxy) = Some(proxy.clone());
        if let Some(context) = lock(&self.inner.context).clone() {
            apply_proxy(&context, proxy);
        }
    }

    /// User-Agent：请求头和页面里的 `navigator.userAgent` 都换成它；`None` 是内核默认的。打开着的页面下次请求、
    /// 下次加载时生效
    pub fn set_user_agent(&self, user_agent: Option<&str>) {
        *lock(&self.inner.user_agent) = user_agent.map(str::to_owned);
        for shared in self.pages() {
            apply_overrides(&shared);
        }
    }

    pub(crate) fn user_agent(&self) -> Option<String> {
        lock(&self.inner.user_agent).clone()
    }

    /// 每个请求发出之前（页面、图片、脚本、`fetch`……）交给 `intercept`：能拦掉、改地址、改请求头
    /// （[`crate::InterceptedRequest`]）。在内核的网络线程上同步地调用，要快、要能跨线程，不能读写信号。用这一套的页面都算
    pub fn set_interceptor(&self, intercept: impl Fn(&mut crate::InterceptedRequest) + Send + Sync + 'static) {
        *lock(&self.inner.interceptor) = Some(Arc::new(intercept));
    }

    /// 不再拦截
    pub fn clear_interceptor(&self) {
        *lock(&self.inner.interceptor) = None;
    }

    pub(crate) fn interceptor(&self) -> Option<Arc<Interceptor>> {
        lock(&self.inner.interceptor).clone()
    }

    /// 要不要在请求发出之前过一遍（有拦截、改了 User-Agent 或者语言）
    pub(crate) fn watches_requests(&self) -> bool {
        lock(&self.inner.interceptor).is_some() || lock(&self.inner.user_agent).is_some() || !lock(&self.inner.languages).is_empty()
    }

    /// 加一段用户脚本（同名的换掉），用这一套的页面都注入（[`UserScript`]）。打开着的页面下次加载时生效
    pub fn add_user_script(&self, script: UserScript) {
        upsert(&mut lock(&self.inner.scripts), script);
        self.scripts_changed();
    }

    /// 删掉名字是 `name` 的用户脚本
    pub fn remove_user_script(&self, name: &str) {
        lock(&self.inner.scripts).retain(|s| s.name != name);
        self.scripts_changed();
    }

    /// 这一套的用户脚本
    pub fn user_scripts(&self) -> Vec<UserScript> {
        lock(&self.inner.scripts).clone()
    }

    fn scripts_changed(&self) {
        for shared in self.pages() {
            crate::scripts::sync(&shared);
        }
    }

    /// 主线程上：用这一套、打开着的页面
    fn pages(&self) -> Vec<Rc<Shared>> {
        crate::state::all().into_iter().filter(|shared| shared.page.profile().id() == self.id()).collect()
    }

    /// 清掉缓存（HTTP 缓存）
    pub fn clear_cache(&self) -> crate::Reply<()> {
        let (reply, responder) = crate::reply::reply();
        if let Some(context) = self.started_context() {
            cef::ImplRequestContext::clear_http_cache(&context, Some(&mut crate::cookies::done(responder)));
        }
        reply
    }

    /// 清掉"证书有问题也继续打开"的记录（之后再遇到时重新问，见 [`crate::WebController::on_certificate_error`]）
    pub fn clear_certificate_exceptions(&self) -> crate::Reply<()> {
        let (reply, responder) = crate::reply::reply();
        if let Some(context) = self.started_context() {
            cef::ImplRequestContext::clear_certificate_exceptions(&context, Some(&mut crate::cookies::done(responder)));
        }
        reply
    }

    /// 清掉记着的 HTTP 认证（之后网站要认证时重新问）
    pub fn clear_auth_credentials(&self) -> crate::Reply<()> {
        let (reply, responder) = crate::reply::reply();
        if let Some(context) = self.started_context() {
            cef::ImplRequestContext::clear_http_auth_credentials(&context, Some(&mut crate::cookies::done(responder)));
        }
        reply
    }

    /// 主线程上：内核没启动时先启动，再拿请求上下文
    pub(crate) fn started_context(&self) -> Option<cef::RequestContext> {
        if let Err(error) = crate::engine::ensure_started() {
            tracing::warn!("内核启动不了：{error}");
            return None;
        }
        self.context()
    }
}

/// 名字里只留英文字母、数字、`-`、`_`（数据目录的名字）
fn sanitize(name: &str) -> String {
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if name.is_empty() { "_".into() } else { name }
}

fn upsert(scripts: &mut Vec<UserScript>, script: UserScript) {
    match scripts.iter_mut().find(|s| s.name == script.name) {
        Some(old) => *old = script,
        None => scripts.push(script),
    }
}

/// 加、换掉同名的用户脚本（页面自己的，`state` 用）
pub(crate) fn upsert_script(scripts: &mut Vec<UserScript>, script: UserScript) {
    upsert(scripts, script);
}

/// 新建一个请求上下文：`dir` 是数据目录，没有时数据只放内存；`languages` 不空时是这一套的语言
fn create(dir: Option<&std::path::Path>, languages: &[String]) -> Option<cef::RequestContext> {
    let settings = cef::RequestContextSettings {
        cache_path: dir.map(|d| CefString::from(d.to_string_lossy().as_ref())).unwrap_or_default(),
        persist_session_cookies: i32::from(dir.is_some()),
        accept_language_list: if languages.is_empty() { CefString::default() } else { CefString::from(crate::engine::accept_languages(languages).as_str()) },
        ..Default::default()
    };
    let context = cef::request_context_create_context(Some(&settings), None);
    if context.is_none() {
        tracing::warn!(dir = ?dir, "内核建不了请求上下文");
    }
    context
}

// ---------- 偏好（内核的界面线程上） ----------

/// 在内核的界面线程上设代理
fn apply_proxy(context: &cef::RequestContext, proxy: Proxy) {
    let context = context.clone();
    on_ui(move || {
        if let Some(value) = proxy_value(&proxy) {
            set_preference(&context, "proxy", value);
        }
    });
}

fn set_preference(context: &cef::RequestContext, name: &str, mut value: cef::Value) {
    // 错误是必须给的参数：空的字符串传过去是空指针，内核直接拒绝（什么都不设），所以给个初值
    let mut error = CefString::from(" ");
    if context.set_preference(Some(&CefString::from(name)), Some(&mut value), Some(&mut error)) == 0 {
        tracing::warn!(name, "内核没收下偏好：{error}");
    }
}

/// 代理的偏好：`{"mode": …, "server": …, "bypass_list": …, "pac_url": …}`
fn proxy_value(proxy: &Proxy) -> Option<cef::Value> {
    let dict = cef::dictionary_value_create()?;
    let set = |key: &str, text: &str| {
        dict.set_string(Some(&CefString::from(key)), Some(&CefString::from(text)));
    };
    match proxy {
        Proxy::System => set("mode", "system"),
        Proxy::Direct => set("mode", "direct"),
        Proxy::AutoDetect => set("mode", "auto_detect"),
        Proxy::Servers { servers, bypass } => {
            set("mode", "fixed_servers");
            set("server", servers);
            if !bypass.is_empty() {
                set("bypass_list", bypass);
            }
        }
        Proxy::Pac(url) => {
            set("mode", "pac_script");
            set("pac_url", url);
        }
    }
    let value = cef::value_create()?;
    let mut dict = dict;
    value.set_dictionary(Some(&mut dict));
    Some(value)
}

/// 在内核的界面线程上做 `f`：已经在上面时直接做（macOS 上就是主线程），否则投递过去
fn on_ui(f: impl FnOnce() + Send + 'static) {
    if cef::currently_on(ThreadId::UI) != 0 {
        f();
        return;
    }
    let f: Box<dyn FnOnce() + Send> = Box::new(f);
    let mut task = UiTask::new(Arc::new(Mutex::new(Some(f))));
    if cef::post_task(ThreadId::UI, Some(&mut task)) == 0 {
        tracing::warn!("投递不到内核的界面线程");
    }
}

type Work = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

wrap_task! {
    struct UiTask {
        work: Work,
    }

    impl Task {
        fn execute(&self) {
            let work = lock(&self.work).take();
            if let Some(work) = work {
                work();
            }
        }
    }
}

// ---------- User-Agent、语言交给打开着的页面 ----------

/// 打开着的页面：页面里的 `navigator.userAgent`、`navigator.language` 换成这一套的（经 DevTools 协议；
/// 请求头在拦截的地方换）。都没设时不动
pub(crate) fn apply_overrides(shared: &Shared) {
    let profile = shared.page.profile();
    let (user_agent, languages) = (profile.user_agent(), profile.accept_languages());
    if user_agent.is_none() && languages.is_none() {
        return;
    }
    let Some(host) = shared.browser.borrow().as_ref().and_then(ImplBrowser::host) else { return };
    use crate::scripts::Param;
    // 空的 User-Agent 是不改它
    let mut params = vec![("userAgent", Param::Text(user_agent.unwrap_or_default()))];
    if let Some(languages) = languages {
        params.push(("acceptLanguage", Param::Text(languages)));
    }
    crate::scripts::send(&host, "Emulation.setUserAgentOverride", &params);
}

/// 页面要注入的用户脚本：这一套的，加上页面自己的
pub(crate) fn scripts_for(profile: &WebProfile, page: &[UserScript]) -> Vec<UserScript> {
    let mut scripts = profile.user_scripts();
    for script in page {
        upsert(&mut scripts, script.clone());
    }
    scripts
}
