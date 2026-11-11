//! 页面要开新窗口（`window.open`、带 `target="_blank"` 的链接）。
//!
//! 应用接手时（[`crate::WebController::on_new_window`]），在 CEF 的界面线程上让内核照常建出新页面：离屏，用新的一套回调
//! （[`Page::sibling`]：能跨线程的那一半先建好），建好以后（`on_after_created`）在主线程上包成控制器交给应用。
//! 内核建的新页面和打开它的页面在同一个"浏览上下文组"里，`window.opener`、`postMessage` 照样能用
//! （第三方登录的弹窗靠它把结果传回来）。应用收下以后没挂到元素上就放掉了的，控制器释放时关掉（`Shared` 的 `Drop`）。
//!
//! 应用不接手时在原来的视图里打开（见 `client::handlers` 的 `popup_target`）。

use std::os::raw::c_int;
use std::sync::Arc;

use cef::{BrowserSettings, Client, PopupFeatures, WindowInfo, WindowOpenDisposition};

use crate::state::{Geometry, Page};
use crate::WebController;

/// 页面想怎么开新窗口（内核按点击的方式、脚本给的参数定）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NewWindowKind {
    /// 新标签，切过去（普通的 `target="_blank"`）
    ForegroundTab,
    /// 新标签，留在后台（按着 Ctrl / ⌘ 点、中键点）
    BackgroundTab,
    /// 小的弹出窗口（`window.open` 给了大小这类参数）
    Popup,
    /// 新窗口（按着 Shift 点）
    Window,
}

/// 页面要开的新窗口（[`crate::WebController::on_new_window`]）
#[non_exhaustive]
pub struct NewWindow {
    /// 内核已经建好、正在加载 [`NewWindow::url`] 的新页面：挂到一个网页视图上就显示，放掉就关掉
    pub controller: WebController,
    /// 要打开的地址（脚本先开空窗口再往里写内容时是空的）
    pub url: String,
    /// 页面想怎么开
    pub kind: NewWindowKind,
    /// 是用户点出来的（不是脚本自己弹的：没有用户操作就弹的多半是广告）
    pub user_gesture: bool,
    /// 脚本要的大小（`window.open` 的 `width`、`height`），逻辑像素
    pub size: Option<[f32; 2]>,
}

/// 内核建着、还没交给应用的新窗口的来历（记在新页面的 [`Page`] 上）
pub(crate) struct Pending {
    /// 打开它的页面的编号
    opener: u64,
    url: String,
    kind: NewWindowKind,
    user_gesture: bool,
    size: Option<[f32; 2]>,
}

/// 内核的开法换成应用看的；另存（按着 Alt 点）、忽略的不是新窗口
fn kind(disposition: WindowOpenDisposition) -> Option<NewWindowKind> {
    Some(match disposition {
        WindowOpenDisposition::NEW_BACKGROUND_TAB => NewWindowKind::BackgroundTab,
        WindowOpenDisposition::NEW_POPUP | WindowOpenDisposition::NEW_PICTURE_IN_PICTURE => NewWindowKind::Popup,
        WindowOpenDisposition::NEW_WINDOW | WindowOpenDisposition::OFF_THE_RECORD => NewWindowKind::Window,
        WindowOpenDisposition::SAVE_TO_DISK | WindowOpenDisposition::IGNORE_ACTION => return None,
        _ => NewWindowKind::ForegroundTab,
    })
}

/// CEF 的界面线程上，应用接手新窗口时：填好新页面的窗口设置、设置、客户端，让内核建（返回 0）；
/// 按着 Alt 点的是另存，下载它，不开（返回 1）
pub(crate) fn before_popup(
    opener: &Arc<Page>,
    browser: Option<&cef::Browser>,
    url: String,
    disposition: WindowOpenDisposition,
    user_gesture: bool,
    features: Option<&PopupFeatures>,
    window_info: Option<&mut WindowInfo>,
    client: Option<&mut Option<Client>>,
    settings: Option<&mut BrowserSettings>,
    _extra_info: Option<&mut Option<cef::DictionaryValue>>,
) -> c_int {
    let Some(kind) = kind(disposition) else {
        if disposition == WindowOpenDisposition::SAVE_TO_DISK
            && let Some(host) = browser.and_then(cef::ImplBrowser::host)
        {
            cef::ImplBrowserHost::start_download(&host, Some(&cef::CefString::from(url.as_str())));
        }
        return 1;
    };
    let (Some(window_info), Some(client)) = (window_info, client) else { return 1 };
    let size = features.and_then(|f| (f.width_set != 0 && f.height_set != 0 && f.width > 0 && f.height > 0).then_some([f.width as f32, f.height as f32]));
    let page = Arc::new(opener.sibling());
    // 挂到元素上之前按打开它的页面的大小画（内核一建好就要大小），脚本给了大小时按脚本的
    let geometry = opener.geometry();
    page.set_geometry(Geometry { size: size.unwrap_or(geometry.size), ..geometry });
    page.set_gpu(opener.gpu());
    page.render().opened();
    *window_info = crate::engine::window_info();
    if let Some(settings) = settings {
        *settings = crate::engine::browser_settings(0);
    }
    *client = crate::client::build_for_page(&page);
    // 新窗口用打开它的页面的那一套数据（内核自己就这样）；用户脚本在建好时注册（`scripts`）
    page.set_popup(Pending { opener: opener.id(), url, kind, user_gesture, size });
    0
}

/// 主线程上：内核建好了替页面开的新窗口，包成控制器交给打开它的页面的应用。那个页面已经没了、
/// 不再接手新窗口时放掉（关掉）
pub(crate) fn adopt(page: Arc<Page>, browser: cef::Browser, pending: Pending) {
    let hook = crate::state::find(pending.opener).and_then(|opener| opener.hooks.borrow().new_window.clone());
    let controller = WebController::with_page(page, pending.url.clone());
    crate::engine::adopt(&controller.shared, browser);
    match hook {
        Some(hook) => hook(NewWindow { controller, url: pending.url, kind: pending.kind, user_gesture: pending.user_gesture, size: pending.size }),
        None => tracing::debug!(url = %pending.url, "打开新窗口的页面已经不在了，新窗口关掉"),
    }
}
