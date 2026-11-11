//! 应用自带的页面：`app://<主机>/<路径>` 的请求交给应用（[`serve_app`]、[`serve_app_directory`]），不走网络。
//!
//! 协议名固定是 `app`，按标准、安全的协议登记（同源、跨域请求、`fetch` 和 `https` 一样能用）：自定义协议要在
//! 每个进程里、启动之前登记，子进程程序不知道应用自己起的名字。主机名随应用定（`app://local/index.html`），
//! 交给应用的请求里有。
//!
//! 应用给的处理函数在 CEF 的网络线程上调用，要能跨线程（`Send + Sync`）；可以在网页视图挂载前后任何时候设，
//! 后设的换掉先设的。没设时 `app://` 的请求都是 404。

use std::os::raw::c_int;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use cef::rc::Rc as _;
use cef::{
    Browser, Callback, CefString, Frame, ImplRequest, ImplResourceHandler, ImplResponse, ImplSchemeHandlerFactory, ImplSchemeRegistrar, Request,
    ResourceHandler, ResourceReadCallback, Response, SchemeHandlerFactory, SchemeRegistrar, WrapResourceHandler, WrapSchemeHandlerFactory,
    wrap_resource_handler, wrap_scheme_handler_factory,
};

/// 协议名
pub const APP_SCHEME: &str = "app";

/// `app://` 的一个请求（[`serve_app`]）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppRequest {
    /// 整个地址
    pub url: String,
    /// 主机名（`app://local/a.html` 里的 `local`）
    pub host: String,
    /// 路径，解开了百分号编码，以 `/` 开头（不含查询和片段）
    pub path: String,
    /// 查询（`?` 后面的，没有时是空的）
    pub query: String,
    /// 请求的方法（`GET`、`POST`……）
    pub method: String,
}

/// `app://` 的一个回应
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppResponse {
    /// 状态码，默认 200
    pub status: u16,
    /// 内容类型（`text/html`……）
    pub mime_type: String,
    /// 内容
    pub body: Vec<u8>,
    /// 别的响应头
    pub headers: Vec<(String, String)>,
}

impl AppResponse {
    /// 200，内容是 `body`，类型是 `mime_type`
    pub fn new(body: impl Into<Vec<u8>>, mime_type: impl Into<String>) -> Self {
        Self { status: 200, mime_type: mime_type.into(), body: body.into(), headers: Vec::new() }
    }

    /// 读文件 `path`，类型按扩展名猜（[`mime_type_of`]）；读不了时 `None`
    pub fn file(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref();
        let body = std::fs::read(path).ok()?;
        Some(Self::new(body, mime_type_of(path)))
    }

    /// 换状态码
    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// 加一个响应头
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn not_found() -> Self {
        Self::new(b"not found".to_vec(), "text/plain").status(404)
    }
}

type Serve = dyn Fn(&AppRequest) -> Option<AppResponse> + Send + Sync;

static SERVE: RwLock<Option<Arc<Serve>>> = RwLock::new(None);

/// `app://` 的请求交给 `serve`：返回 `None` 是 404。在 CEF 的网络线程上调用
pub fn serve_app(serve: impl Fn(&AppRequest) -> Option<AppResponse> + Send + Sync + 'static) {
    *SERVE.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(serve));
}

/// `app://<任意主机>/<路径>` 是目录 `root` 下的文件：以 `/` 结尾、没有路径时是那个目录的 `index.html`。
/// 路径里的 `..` 出不了这个目录
pub fn serve_app_directory(root: impl Into<PathBuf>) {
    let root = root.into();
    serve_app(move |request| AppResponse::file(file_in(&root, &request.path)?));
}

/// 目录 `root` 下 `path`（以 `/` 开头）对应的文件；跳出目录的（`..`、绝对路径、盘符）是 `None`
fn file_in(root: &Path, path: &str) -> Option<PathBuf> {
    let mut file = root.to_path_buf();
    for component in Path::new(path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => file.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if path.is_empty() || path.ends_with('/') {
        file.push("index.html");
    }
    Some(file)
}

/// 回应的内容类型拆成类型和字符集：写了 `; charset=…` 的用它；文本类的（`text/*`、脚本、JSON、SVG、XML）没写时按 UTF-8，
/// 不然内核按西欧的编码解读，没写编码声明的中文页面成了乱码
fn content_type(mime: &str) -> (&str, Option<&str>) {
    let mut parts = mime.split(';');
    let base = parts.next().unwrap_or_default().trim();
    let given = parts.find_map(|p| {
        let (key, value) = p.split_once('=')?;
        key.trim().eq_ignore_ascii_case("charset").then(|| value.trim().trim_matches('"'))
    });
    let lower = base.to_ascii_lowercase();
    let textual = lower.starts_with("text/") || ["application/javascript", "application/json", "application/xml", "image/svg+xml"].contains(&lower.as_str());
    (base, given.or(textual.then_some("utf-8")))
}

/// 按扩展名猜内容类型；认不出时是 `application/octet-stream`
pub fn mime_type_of(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "txt" => "text/plain",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

/// 拆开 `app://` 的地址：主机、路径（解开百分号编码）、查询
fn parse(url: &str, method: &str) -> AppRequest {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split('#').next().unwrap_or_default();
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };
    AppRequest { url: url.to_owned(), host: host.to_ascii_lowercase(), path: percent_decode(path), query: query.to_owned(), method: method.to_owned() }
}

/// 解开百分号编码（不是合法的 UTF-8 时按有损的办）
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 每个进程启动时登记 `app://`（CEF 的应用对象的 `on_register_custom_schemes`）
pub(crate) fn register(registrar: Option<&mut SchemeRegistrar>) {
    // 标准（有主机、相对地址能解析）、安全（和 https 一样算安全的来源）、能跨域请求、能 fetch
    const OPTIONS: c_int = 1 | 8 | 16 | 64;
    if let Some(registrar) = registrar {
        registrar.add_custom_scheme(Some(&CefString::from(APP_SCHEME)), OPTIONS);
    }
}

/// 浏览器进程里（启动以后）：`app://` 的请求交给 [`serve_app`] 给的处理函数
pub(crate) fn install() {
    let mut factory = AppFactory::new();
    if cef::register_scheme_handler_factory(Some(&CefString::from(APP_SCHEME)), None, Some(&mut factory)) != 1 {
        tracing::warn!("登记不了 {APP_SCHEME}:// 的处理");
    }
}

/// 给另建的一套数据（请求上下文）也登记 `app://`：全局登记的只管全局的那一套
pub(crate) fn install_for(context: &cef::RequestContext) {
    let mut factory = AppFactory::new();
    if cef::ImplRequestContext::register_scheme_handler_factory(context, Some(&CefString::from(APP_SCHEME)), None, Some(&mut factory)) != 1 {
        tracing::warn!("给另一套数据登记不了 {APP_SCHEME}:// 的处理");
    }
}

wrap_scheme_handler_factory! {
    struct AppFactory;

    impl SchemeHandlerFactory {
        fn create(&self, _browser: Option<&mut Browser>, _frame: Option<&mut Frame>, _scheme_name: Option<&CefString>, request: Option<&mut Request>) -> Option<ResourceHandler> {
            let request = request?;
            let request = parse(&CefString::from(&request.url()).to_string(), &CefString::from(&request.method()).to_string());
            let serve = SERVE.read().unwrap_or_else(PoisonError::into_inner).clone();
            let response = serve.and_then(|serve| serve(&request)).unwrap_or_else(AppResponse::not_found);
            Some(AppResource::new(Arc::new(response), Arc::new(Mutex::new(0))))
        }
    }
}

wrap_resource_handler! {
    struct AppResource {
        response: Arc<AppResponse>,
        offset: Arc<Mutex<usize>>,
    }

    impl ResourceHandler {
        fn open(&self, _request: Option<&mut Request>, handle_request: Option<&mut c_int>, _callback: Option<&mut Callback>) -> c_int {
            // 内容已经在手里：马上处理
            if let Some(handle) = handle_request {
                *handle = 1;
            }
            1
        }

        fn response_headers(&self, response: Option<&mut Response>, response_length: Option<&mut i64>, _redirect_url: Option<&mut CefString>) {
            if let Some(response) = response {
                response.set_status(c_int::from(self.response.status));
                let (mime_type, charset) = content_type(&self.response.mime_type);
                response.set_mime_type(Some(&CefString::from(mime_type)));
                if let Some(charset) = charset {
                    response.set_charset(Some(&CefString::from(charset)));
                }
                for (name, value) in &self.response.headers {
                    response.set_header_by_name(Some(&CefString::from(name.as_str())), Some(&CefString::from(value.as_str())), 1);
                }
            }
            if let Some(length) = response_length {
                *length = self.response.body.len() as i64;
            }
        }

        fn read(&self, data_out: *mut u8, bytes_to_read: c_int, bytes_read: Option<&mut c_int>, _callback: Option<&mut ResourceReadCallback>) -> c_int {
            let mut offset = self.offset.lock().unwrap_or_else(PoisonError::into_inner);
            let rest = &self.response.body[(*offset).min(self.response.body.len())..];
            let count = rest.len().min(bytes_to_read.max(0) as usize);
            if count > 0 && !data_out.is_null() {
                // SAFETY: CEF 保证 `data_out` 至少有 `bytes_to_read` 个字节可写；只写 `count` 个
                unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), data_out, count) };
            }
            *offset += count;
            if let Some(read) = bytes_read {
                *read = count as c_int;
            }
            // 读完了返回 0：响应结束
            c_int::from(count > 0)
        }
    }
}
