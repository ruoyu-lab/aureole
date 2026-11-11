//! 一个网页的共享状态：控制器（应用拿着）、元素种类、CEF 的各种回调都引用同一份。
//!
//! 分两半：
//!
//! - [`Shared`]：只在主线程上用（信号、当前打开的网页、出帧的节奏）；
//! - [`Page`]：CEF 的回调拿着的那一半，能在任何线程上用：网页的位置和大小（CEF 同步地问）、离屏绘制的状态
//!   （在回调里复制每一帧）、投递回主线程。macOS 上 CEF 的界面线程就是主线程（外部消息泵），回调直接在主线程上；
//!   Windows 上 CEF 有自己的界面线程，回调在那个线程上，要写信号的投递回主线程（见 [`Page::on_main`]）。
//!
//! 字段是各模块之间的约定，改字段要一起改用到它的模块。

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::Instant;

use aureole::{Signal, signal};
use aureole_core::{MainThreadSender, main_thread_sender};
use aureole_render::Gpu;

/// 网页在窗口里的位置和大小：CEF 要逻辑大小（`view_rect`）、缩放（`screen_info`）、
/// 屏幕坐标（`screen_point`，输入法的候选框、弹出层用）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Geometry {
    /// 内容区的大小，逻辑像素
    pub size: [f32; 2],
    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub scale: f32,
    /// 内容区左上角在窗口里的位置，逻辑像素
    pub origin: [f32; 2],
}

pub(crate) struct Shared {
    /// 回调拿着的那一半
    pub page: Arc<Page>,
    /// 打开着的网页；元素没挂载、CEF 还没启动好时为 `None`
    pub browser: RefCell<Option<cef::Browser>>,
    /// 网页在建、还没交回来（Windows 上是异步建的，见 `engine::created`）；建好之前卸载了的话清掉，建好时直接关掉
    pub opening: Cell<bool>,
    /// 应用要打开的地址：挂载时按它打开；网页里跳转以后由 `client` 更新成现在的地址
    pub url: Signal<String>,
    /// 网页的标题
    pub title: Signal<String>,
    /// 正在加载
    pub loading: Signal<bool>,
    /// 能后退、能前进
    pub can_go_back: Signal<bool>,
    pub can_go_forward: Signal<bool>,
    /// CEF 画好一帧加一：元素种类在 `update` 里读它，变了就重画（`render` 写）
    pub frame: Signal<u64>,
    /// 到这个时刻之前每帧都发帧开始的信号（`render` 管）：有输入、改了大小、刚出过一帧时往后续，静止时已经过去了
    pub awake_until: Cell<Option<Instant>>,
    /// 内核自己出帧时（`engine::Pacing::Kernel`）告诉它的帧率：窗口所在屏幕的刷新率，0 是还没对过（`kind` 管）
    pub frame_rate: Cell<i32>,
    /// 画好的帧一直没人取（窗口被挡住、最小化，框架不出帧）：让内核降到了很低的帧率（`render::throttle`），
    /// 框架又跟得上时由 `kind` 升回去
    pub throttled: Cell<bool>,
    /// 元素有键盘焦点（`input` 写）
    pub focused: Cell<bool>,
    /// CEF 要的光标形状（`input` 写，元素种类读）
    pub cursor: Signal<aureole_core::Cursor>,
    /// 应用接手的事（对话框、选文件、下载、权限、页面发来的消息）
    pub hooks: RefCell<Hooks>,
    /// 这个网页的下载（`dialogs` 写）
    pub downloads: Signal<Vec<crate::Download>>,
    /// 页面要全屏（网页全屏，`tools` 写）
    pub fullscreen: Signal<bool>,
    /// 缩放的倍数（`tools` 写）
    pub zoom: Signal<f64>,
    /// 辅助技术来问过（以后建的网页都打开无障碍，`accessibility` 管）、页面的无障碍树、树变了一次加一
    pub accessibility: Cell<bool>,
    pub ax_tree: RefCell<crate::accessibility::AxTree>,
    pub ax_version: Signal<u64>,
    /// 页内查找的结果（`tools` 管）
    pub find_result: Signal<crate::FindResult>,
    /// 网站图标、悬停的链接、加载进度（0 到 1）、主框架加载出错、页面给的提示文字、渲染进程退出了、历史记录（`status` 写）
    pub favicon: Signal<Option<aureole::RgbaImage>>,
    pub hovered_link: Signal<String>,
    pub progress: Signal<f64>,
    pub load_error: Signal<Option<crate::LoadError>>,
    pub tooltip: Signal<String>,
    pub crash: Signal<Option<crate::Crash>>,
    pub history: Signal<crate::History>,
    /// 静音（`status` 管；页面打开之前设的打开时生效）
    pub muted: Signal<bool>,
    /// 网页关了（页面自己 `window.close()`、应用 `close`）：元素不再按地址重新打开，`load`、`reload` 时才开（元素种类读它）
    pub closed: Signal<bool>,
    /// 挂在元素上、在前台（`lifecycle` 管）：没挂上时网页在后台
    pub shown: Cell<bool>,
    /// 生命周期（`lifecycle` 管）
    pub lifecycle: Signal<crate::Lifecycle>,
    /// 用户脚本经 DevTools 协议注册过的（`scripts` 管）、收结果的观察者（留着它才一直收）
    pub scripts_registered: RefCell<Vec<crate::scripts::Registered>>,
    pub devtools_observer: RefCell<Option<cef::Registration>>,
    /// 建成空页面、注册完用户脚本再加载的地址（`engine` 管）
    pub initial_load: RefCell<Option<String>>,
    /// 是内核替页面开的新窗口（`engine::adopt` 记）
    pub popup: Cell<bool>,
}

impl Drop for Shared {
    /// 控制器和元素都放掉了，网页还开着（应用收下新窗口以后没挂到元素上）：关掉
    fn drop(&mut self) {
        if let Some(browser) = self.browser.get_mut().take() {
            crate::engine::discard(browser);
        }
    }
}

/// 应用接手的事：都在主线程上调用
#[derive(Default)]
pub(crate) struct Hooks {
    pub dialog: Option<Rc<dyn Fn(crate::Dialog)>>,
    pub file_chooser: Option<Rc<dyn Fn(crate::FileChooser)>>,
    pub download: Option<Rc<dyn Fn(crate::DownloadRequest)>>,
    pub permission: Option<Rc<dyn Fn(crate::PermissionRequest)>>,
    pub message: Option<Rc<dyn Fn(crate::WebMessage)>>,
    pub new_window: Option<Rc<dyn Fn(crate::NewWindow)>>,
    pub context_menu: Option<Rc<dyn Fn(crate::ContextMenu)>>,
    pub auth: Option<Rc<dyn Fn(crate::AuthRequest)>>,
    pub certificate_error: Option<Rc<dyn Fn(crate::CertificateError)>>,
    pub close_requested: Option<Rc<dyn Fn()>>,
    pub console: Option<Rc<dyn Fn(crate::ConsoleMessage)>>,
}

/// 页面要去的一个地址（[`WebController::on_navigate`]）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Navigation {
    /// 要去的地址
    pub url: String,
    /// 是整个页面跳转（不是嵌进来的框架）
    pub main_frame: bool,
    /// 是用户点出来的（点链接、提交表单），不是脚本自己跳的
    pub user_gesture: bool,
    /// 是服务器的重定向
    pub redirect: bool,
}

/// 决定让不让去的函数：在 CEF 的界面线程上调用
pub(crate) type NavigationFilter = dyn Fn(&Navigation) -> bool + Send + Sync;

/// 回调的线程上要同步知道应用接没接手的（`Page::has_hook`）
#[derive(Clone, Copy)]
pub(crate) enum Hook {
    Permission,
    NewWindow,
    ContextMenu,
    Auth,
    CertificateError,
    Console,
}

/// CEF 的回调拿着的网页：在任何线程上都能用
pub(crate) struct Page {
    /// 在主线程的表里找 [`Shared`] 用
    id: u64,
    /// 主线程：回调在别的线程上时投递过去
    main: ThreadId,
    sender: MainThreadSender,
    /// 位置和大小（`kind` 写，CEF 的回调读）
    geometry: Mutex<Geometry>,
    /// 离屏绘制的状态（`render` 独占）
    render: Mutex<crate::render::RenderState>,
    /// 元素画的时候引用的外部纹理的编号，0 是还没有（`render` 写，元素种类每帧读，不用等复制的锁）
    texture: AtomicU32,
    /// 从上次元素推进以来换上了几帧新画面、有几帧来不及显示被顶掉（`render` 加，元素每帧取走，做统计）
    arrived: AtomicU32,
    dropped: AtomicU32,
    /// 窗口共用的 GPU 设备（主线程上才拿得到，元素每帧交过来），和它的编号（没有时为 0）
    gpu: Mutex<Option<Arc<Gpu>>>,
    gpu_id: AtomicU64,
    /// 应用接手了哪些事（[`Hook`] 的位）
    hooks: AtomicU32,
    /// 能取消、暂停、继续的下载
    downloads: Mutex<crate::dialogs::DownloadControls>,
    /// 决定页面能不能去某个地址（[`WebController::on_navigate`]）
    navigate: Mutex<Option<Arc<NavigationFilter>>>,
    /// 内核替别的页面开的新窗口：建好以后交给应用之前记着从哪来、要怎么开（`popup` 管）
    popup: Mutex<Option<crate::popup::Pending>>,
    /// 用的哪一套数据（建好以后不变）
    profile: crate::WebProfile,
    /// 页面自己的用户脚本（加上配置的一起注入，见 `profile`）
    scripts: Mutex<Vec<crate::UserScript>>,
}

thread_local! {
    /// 主线程上：编号 -> 网页的共享状态。释放了的在下次登记时清掉
    static PAGES: RefCell<Vec<(u64, Weak<Shared>)>> = const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Page {
    /// 一个新的网页（还没有主线程上的那一半）：在 `main` 线程上用，投递经 `sender`
    fn new(main: ThreadId, sender: MainThreadSender, profile: crate::WebProfile) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            main,
            sender,
            geometry: Mutex::new(Geometry::default()),
            render: Mutex::new(crate::render::RenderState::default()),
            texture: AtomicU32::new(0),
            arrived: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
            gpu: Mutex::new(None),
            gpu_id: AtomicU64::new(0),
            hooks: AtomicU32::new(0),
            downloads: Mutex::new(Default::default()),
            navigate: Mutex::new(None),
            popup: Mutex::new(None),
            profile,
            scripts: Mutex::new(Vec::new()),
        }
    }

    /// 和这个网页同一个主线程、同一套数据的新网页（在 CEF 的线程上替页面开新窗口时建）
    pub(crate) fn sibling(&self) -> Self {
        Self::new(self.main, self.sender.clone(), self.profile.clone())
    }

    /// 用的哪一套数据
    pub(crate) fn profile(&self) -> &crate::WebProfile {
        &self.profile
    }

    /// 页面自己的用户脚本
    pub(crate) fn scripts(&self) -> Vec<crate::UserScript> {
        lock(&self.scripts).clone()
    }

    /// 编号（在主线程上找 [`Shared`] 用，见 [`find`]）
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 内核替别的页面开的新窗口的来历（`popup` 管）：交给应用时拿走
    pub(crate) fn set_popup(&self, pending: crate::popup::Pending) {
        *lock(&self.popup) = Some(pending);
    }

    pub(crate) fn take_popup(&self) -> Option<crate::popup::Pending> {
        lock(&self.popup).take()
    }

    /// 在主线程上做 `f`：已经在主线程上时直接做，否则投递过去（不找 [`Shared`]）
    pub(crate) fn run_on_main(&self, f: impl FnOnce() + Send + 'static) {
        if std::thread::current().id() == self.main {
            f();
        } else {
            self.sender.send(f);
        }
    }

    /// 位置和大小
    pub(crate) fn geometry(&self) -> Geometry {
        *lock(&self.geometry)
    }

    pub(crate) fn set_geometry(&self, geometry: Geometry) {
        *lock(&self.geometry) = geometry;
    }

    /// 离屏绘制的状态
    pub(crate) fn render(&self) -> MutexGuard<'_, crate::render::RenderState> {
        lock(&self.render)
    }

    /// 元素画的时候引用的外部纹理的编号；还没收到过一帧时为 `None`
    pub(crate) fn texture(&self) -> Option<u32> {
        Some(self.texture.load(Ordering::Acquire)).filter(|&id| id != 0)
    }

    pub(crate) fn set_texture(&self, id: Option<u32>) {
        self.texture.store(id.unwrap_or(0), Ordering::Release);
    }

    /// 窗口共用的 GPU 设备；还没开过窗口时为 `None`
    /// 换上了一帧新画面（复制进了元素引用的纹理）
    pub(crate) fn note_arrival(&self) {
        self.arrived.fetch_add(1, Ordering::Relaxed);
    }

    /// 有一帧来不及显示、被后面的顶掉了
    #[cfg_attr(not(target_os = "macos"), allow(dead_code, reason = "只有 macOS 上排队换帧"))]
    pub(crate) fn note_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// 主线程上：从上次取走以来换上了几帧、顶掉了几帧
    pub(crate) fn take_arrivals(&self) -> (u32, u32) {
        (self.arrived.swap(0, Ordering::Relaxed), self.dropped.swap(0, Ordering::Relaxed))
    }

    pub(crate) fn gpu(&self) -> Option<Arc<Gpu>> {
        lock(&self.gpu).clone()
    }

    /// 主线程上：交过来此刻的设备（设备丢失、重新创建以后是新的）。和上次是同一个时不动
    pub(crate) fn set_gpu(&self, gpu: Option<Arc<Gpu>>) {
        let id = gpu.as_ref().map_or(0, |gpu| gpu.id() + 1);
        if self.gpu_id.swap(id, Ordering::AcqRel) != id {
            *lock(&self.gpu) = gpu;
        }
    }

    /// 应用接手了 `hook` 这件事
    pub(crate) fn has_hook(&self, hook: Hook) -> bool {
        self.hooks.load(Ordering::Acquire) & (1 << hook as u32) != 0
    }

    pub(crate) fn set_hook(&self, hook: Hook, on: bool) {
        let bit = 1 << hook as u32;
        if on {
            self.hooks.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.hooks.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// 页面能不能去 `navigation` 说的地方（没设过滤时都能）
    pub(crate) fn allows(&self, navigation: &Navigation) -> bool {
        let filter = lock(&self.navigate).clone();
        filter.is_none_or(|filter| filter(navigation))
    }

    /// 能取消、暂停、继续的下载
    pub(crate) fn downloads<R>(&self, f: impl FnOnce(&mut crate::dialogs::DownloadControls) -> R) -> R {
        f(&mut lock(&self.downloads))
    }

    /// 在主线程上对这个网页的 [`Shared`] 做 `f`：已经在主线程上时直接做，否则投递过去。
    /// 网页的共享状态已经释放（控制器和元素都没了）时什么都不做
    pub(crate) fn on_main(&self, f: impl FnOnce(&Rc<Shared>) + Send + 'static) {
        let id = self.id;
        let run = move || {
            if let Some(shared) = find(id) {
                f(&shared);
            }
        };
        if std::thread::current().id() == self.main {
            run();
        } else {
            self.sender.send(run);
        }
    }

    /// 同 [`Page::on_main`]，但在主线程上也不就地执行，排到事件循环的下一轮：在 CEF 的回调里要调回 CEF 时用
    #[cfg_attr(not(target_os = "macos"), allow(dead_code, reason = "只有 macOS 上排队换帧时用"))]
    pub(crate) fn post_main(&self, f: impl FnOnce(&Rc<Shared>) + Send + 'static) {
        let id = self.id;
        self.sender.send(move || {
            if let Some(shared) = find(id) {
                f(&shared);
            }
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 主线程上：还在的所有网页的共享状态
pub(crate) fn all() -> Vec<Rc<Shared>> {
    PAGES.with_borrow(|pages| pages.iter().filter_map(|(_, shared)| shared.upgrade()).collect())
}

/// 主线程上：编号是 `id` 的网页的共享状态（还在的话）
pub(crate) fn find(id: u64) -> Option<Rc<Shared>> {
    PAGES.with_borrow(|pages| pages.iter().find(|(page, _)| *page == id).and_then(|(_, shared)| shared.upgrade()))
}

/// 管着一个网页：打开哪个地址、前进后退、读标题和加载状态。克隆出来的是同一个网页。
/// 只在主线程上用。
///
/// 第一次挂到网页视图上时打开网页。视图卸载以后网页不关，转到后台（[`crate::Lifecycle`]），再挂上时接着显示；
/// 控制器的克隆和视图都放掉以后网页才关掉，或者调 [`WebController::close`]
#[derive(Clone)]
pub struct WebController {
    pub(crate) shared: Rc<Shared>,
}

impl WebController {
    /// 一个要打开 `url` 的网页；挂到 [`crate::web_view`] 上以后才真正打开
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_profile(url, &crate::WebProfile::default())
    }

    /// 用 `profile` 这一套数据（Cookie、缓存、本地存储……）的网页（[`crate::WebProfile`]）
    pub fn with_profile(url: impl Into<String>, profile: &crate::WebProfile) -> Self {
        Self::with_page(Arc::new(Page::new(std::thread::current().id(), main_thread_sender(), profile.clone())), url.into())
    }

    /// 用的哪一套数据
    pub fn profile(&self) -> crate::WebProfile {
        self.shared.page.profile().clone()
    }

    /// 给这个网页加一段用户脚本（同名的换掉；和配置的一起注入，同名时用这里的）。打开着的网页下次加载时生效
    pub fn add_user_script(&self, script: crate::UserScript) {
        crate::profile::upsert_script(&mut lock(&self.shared.page.scripts), script);
        crate::scripts::sync(&self.shared);
    }

    /// 删掉这个网页自己的、名字是 `name` 的用户脚本
    pub fn remove_user_script(&self, name: &str) {
        lock(&self.shared.page.scripts).retain(|s| s.name() != name);
        crate::scripts::sync(&self.shared);
    }

    /// 主线程上：给已经有的 `page`（内核替页面开的新窗口）建主线程上的那一半
    pub(crate) fn with_page(page: Arc<Page>, url: String) -> Self {
        let id = page.id;
        let shared = Rc::new(Shared {
            page,
            browser: RefCell::new(None),
            opening: Cell::new(false),
            url: signal(crate::client::fix_url(&url)),
            title: signal(String::new()),
            loading: signal(false),
            can_go_back: signal(false),
            can_go_forward: signal(false),
            frame: signal(0),
            awake_until: Cell::new(None),
            frame_rate: Cell::new(0),
            throttled: Cell::new(false),
            focused: Cell::new(false),
            cursor: signal(aureole_core::Cursor::Default),
            hooks: RefCell::new(Hooks::default()),
            downloads: signal(Vec::new()),
            fullscreen: signal(false),
            zoom: signal(1.0),
            find_result: signal(crate::FindResult::default()),
            accessibility: Cell::new(false),
            ax_tree: RefCell::new(Default::default()),
            ax_version: signal(0),
            favicon: signal(None),
            hovered_link: signal(String::new()),
            progress: signal(0.0),
            load_error: signal(None),
            tooltip: signal(String::new()),
            crash: signal(None),
            history: signal(crate::History::default()),
            muted: signal(false),
            closed: signal(false),
            shown: Cell::new(false),
            lifecycle: signal(crate::Lifecycle::Active),
            scripts_registered: RefCell::new(Vec::new()),
            devtools_observer: RefCell::new(None),
            initial_load: RefCell::new(None),
            popup: Cell::new(false),
        });
        PAGES.with_borrow_mut(|pages| {
            pages.retain(|(_, shared)| shared.strong_count() > 0);
            pages.push((id, Rc::downgrade(&shared)));
        });
        Self { shared }
    }

    /// 现在的地址（网页里跳转以后跟着变）
    pub fn url(&self) -> Signal<String> {
        self.shared.url
    }

    /// 网页的标题
    pub fn title(&self) -> Signal<String> {
        self.shared.title
    }

    /// 正在加载
    pub fn loading(&self) -> Signal<bool> {
        self.shared.loading
    }

    /// 能后退
    pub fn can_go_back(&self) -> Signal<bool> {
        self.shared.can_go_back
    }

    /// 能前进
    pub fn can_go_forward(&self) -> Signal<bool> {
        self.shared.can_go_forward
    }

    /// 打开 `url`
    pub fn load(&self, url: impl Into<String>) {
        crate::client::load(&self.shared, url.into());
    }

    /// 重新加载
    pub fn reload(&self) {
        crate::client::reload(&self.shared);
    }

    /// 停止加载
    pub fn stop(&self) {
        crate::client::stop(&self.shared);
    }

    /// 后退
    pub fn back(&self) {
        crate::client::back(&self.shared);
    }

    /// 前进
    pub fn forward(&self) {
        crate::client::forward(&self.shared);
    }

    /// 在网页的主框架里执行一段脚本（不等结果）
    pub fn eval(&self, script: &str) {
        crate::client::eval(&self.shared, script);
    }

    /// 页面要弹对话框（`alert`、`confirm`、`prompt`、离开页面前的确认）时交给 `handler`，应用用自己的界面问，
    /// 答复 [`crate::Dialog`]。不设时 Windows 上用系统的对话框
    pub fn on_dialog(&self, handler: impl Fn(crate::Dialog) + 'static) {
        self.shared.hooks.borrow_mut().dialog = Some(Rc::new(handler));
    }

    /// 页面要选文件（`<input type=file>`；下载时问存到哪里也是）时交给 `handler`，答复 [`crate::FileChooser`]。
    /// 不设时用系统的文件对话框
    pub fn on_file_chooser(&self, handler: impl Fn(crate::FileChooser) + 'static) {
        self.shared.hooks.borrow_mut().file_chooser = Some(Rc::new(handler));
    }

    /// 页面开始下载时交给 `handler` 决定存到哪里，答复 [`crate::DownloadRequest`]。不设时问用户存到哪里
    pub fn on_download(&self, handler: impl Fn(crate::DownloadRequest) + 'static) {
        self.shared.hooks.borrow_mut().download = Some(Rc::new(handler));
    }

    /// 这个网页的下载（进行中的、完成的、取消的），按开始的先后
    pub fn downloads(&self) -> Signal<Vec<crate::Download>> {
        self.shared.downloads
    }

    /// 取消编号是 `id` 的下载
    pub fn cancel_download(&self, id: u32) {
        crate::dialogs::control_download(&self.shared.page, id, crate::dialogs::DownloadAction::Cancel);
    }

    /// 暂停编号是 `id` 的下载
    pub fn pause_download(&self, id: u32) {
        crate::dialogs::control_download(&self.shared.page, id, crate::dialogs::DownloadAction::Pause);
    }

    /// 继续编号是 `id` 的下载
    pub fn resume_download(&self, id: u32) {
        crate::dialogs::control_download(&self.shared.page, id, crate::dialogs::DownloadAction::Resume);
    }

    /// 页面申请权限（摄像头、麦克风、位置、通知……）时交给 `handler`，答复 [`crate::PermissionRequest`]。不设时一律拒绝
    pub fn on_permission(&self, handler: impl Fn(crate::PermissionRequest) + 'static) {
        self.shared.hooks.borrow_mut().permission = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::Permission, true);
    }

    /// 页面发来消息（`aureole.postMessage(...)`）时交给 `handler`。网页视图打开的任何页面都能发：
    /// 先看 [`crate::WebMessage::url`] 再信它
    pub fn on_message(&self, handler: impl Fn(crate::WebMessage) + 'static) {
        self.shared.hooks.borrow_mut().message = Some(Rc::new(handler));
    }

    /// 发给页面：页面里 `aureole.onmessage`、`aureole.addEventListener('message', ...)` 收到 `event.data`（就是 `text`）。
    /// 页面还没打开时丢掉
    pub fn post_message(&self, text: &str) {
        crate::bridge::post(&self.shared, text);
    }

    /// 页面要去一个地址（点链接、脚本跳转、提交表单、重定向，嵌进来的框架也算）以前问 `allow`，返回假就不去
    /// （[`Navigation`]）。
    /// 在 CEF 的界面线程上同步地调用（Windows 上不是主线程），所以要能跨线程；不能读写信号
    pub fn on_navigate(&self, allow: impl Fn(&Navigation) -> bool + Send + Sync + 'static) {
        *lock(&self.shared.page.navigate) = Some(Arc::new(allow));
    }

    /// 页面要全屏（视频、`requestFullscreen`）：为真时页面里那个元素已经铺满了网页视图，窗口要不要全屏、
    /// 网页视图要不要铺满窗口由应用定
    pub fn fullscreen(&self) -> Signal<bool> {
        self.shared.fullscreen
    }

    /// 让页面退出全屏
    pub fn exit_fullscreen(&self) {
        let host = self.shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
        if let Some(host) = host {
            cef::ImplBrowserHost::exit_fullscreen(&host, 1);
        }
    }

    /// 缩放的倍数（1.0 是 100%）
    pub fn zoom(&self) -> Signal<f64> {
        self.shared.zoom
    }

    /// 缩放到 `factor` 倍（1.0 是 100%）。网页还没打开时记下，打开时按它
    pub fn set_zoom(&self, factor: f64) {
        crate::tools::set_zoom(&self.shared, factor);
    }

    /// 在页面里找 `text`：和上次是同一段文字时找下一处（`forward` 为假时找上一处），结果看 [`WebController::find_result`]。
    /// 空文字等于 [`WebController::stop_finding`]
    pub fn find(&self, text: &str, forward: bool, match_case: bool) {
        crate::tools::find(&self.shared, text, forward, match_case);
    }

    /// 停止查找，去掉高亮；`keep_selection` 时当前那处留着选中
    pub fn stop_finding(&self, keep_selection: bool) {
        crate::tools::stop_finding(&self.shared, keep_selection);
    }

    /// 页内查找的结果：一共几处、当前第几处
    pub fn find_result(&self) -> Signal<crate::FindResult> {
        self.shared.find_result
    }

    /// 打开开发者工具（单独的窗口）
    pub fn show_dev_tools(&self) {
        crate::tools::show_dev_tools(&self.shared);
    }

    /// 生命周期：活动、冻结、丢弃（[`crate::Lifecycle`]）。挂到视图上时自动变回活动
    pub fn lifecycle(&self) -> Signal<crate::Lifecycle> {
        self.shared.lifecycle
    }

    /// 冻结、丢弃后台的网页（不在视图上的；在前台时不理），或者让它变回活动（冻结的解冻，丢弃的按地址重新打开）
    pub fn set_lifecycle(&self, state: crate::Lifecycle) {
        crate::lifecycle::set(&self.shared, state);
    }

    /// 关掉网页，释放渲染进程和纹理。之后视图不再按地址自动打开它，[`WebController::load`]、[`WebController::reload`] 时才打开
    pub fn close(&self) {
        crate::lifecycle::close(&self.shared);
    }

    /// 关掉开发者工具
    pub fn close_dev_tools(&self) {
        crate::tools::close_dev_tools(&self.shared);
    }

    /// 打印网页：内核弹系统的打印对话框，按页面的打印样式排版
    pub fn print(&self) {
        crate::tools::print(&self.shared);
    }

    /// 把网页存成 PDF（A4、默认边距、带背景），存到 `path`；等它写完，成功是 `true`
    pub fn print_to_pdf(&self, path: impl AsRef<std::path::Path>) -> crate::PdfDone {
        crate::tools::print_to_pdf(&self.shared, path.as_ref())
    }

    /// 页面要开新窗口（`window.open`、带 `target="_blank"` 的链接）时交给 `handler`。[`crate::NewWindow`] 里是内核已经建好的
    /// 新页面的控制器：挂到一个网页视图上就显示（开成新标签还是新窗口由应用定），放掉就关掉。新页面和这个页面之间的
    /// `window.opener` 照样能用（第三方登录的弹窗靠它把结果传回来）。不设时在这个网页视图里打开（用户点出来的才开）
    pub fn on_new_window(&self, handler: impl Fn(crate::NewWindow) + 'static) {
        self.shared.hooks.borrow_mut().new_window = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::NewWindow, true);
    }

    /// 页面里点了右键时交给 `handler`：点在哪、链接、图片、选中的文字、能不能编辑（[`crate::ContextMenu`]）。菜单由应用画，
    /// 菜单项用这里的编辑命令（[`WebController::copy`] 这些）、[`WebController::download`]、[`WebController::inspect`] 实现。
    /// 不设时不弹菜单
    pub fn on_context_menu(&self, handler: impl Fn(crate::ContextMenu) + 'static) {
        self.shared.hooks.borrow_mut().context_menu = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::ContextMenu, true);
    }

    /// 复制页面里选中的内容
    pub fn copy(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Copy);
    }

    /// 剪切输入框里选中的文字
    pub fn cut(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Cut);
    }

    /// 粘贴到有焦点的输入框
    pub fn paste(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Paste);
    }

    /// 粘贴，只要文字，不要格式
    pub fn paste_as_plain_text(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::PastePlain);
    }

    /// 删除输入框里选中的文字
    pub fn delete(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Delete);
    }

    /// 全选（有焦点的输入框里的，不在输入框里时是整个页面的）
    pub fn select_all(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::SelectAll);
    }

    /// 撤销输入框里的上一次编辑
    pub fn undo(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Undo);
    }

    /// 重做
    pub fn redo(&self) {
        crate::menu::edit(&self.shared, crate::menu::Edit::Redo);
    }

    /// 把拼错的词（右键菜单给的 [`crate::ContextMenu::misspelled_word`]）换成 `word`
    pub fn replace_misspelling(&self, word: &str) {
        crate::menu::replace_misspelling(&self.shared, word);
    }

    /// 把 `word` 加进拼写检查的词典，以后不再标成拼错
    pub fn add_to_dictionary(&self, word: &str) {
        crate::menu::add_to_dictionary(&self.shared, word);
    }

    /// 下载 `url`（右键菜单里的"另存图片""链接另存为"），和页面里开始的下载一样经过 [`WebController::on_download`]
    pub fn download(&self, url: &str) {
        crate::menu::download(&self.shared, url);
    }

    /// 打开开发者工具，选中页面里 `at`（网页视图内容区里的位置，逻辑像素）处的元素
    pub fn inspect(&self, at: [f32; 2]) {
        crate::tools::inspect(&self.shared, at);
    }

    /// 网站图标（内核下载好的，按窗口的缩放）；换页面时先变成 `None`，网站没有图标时一直是 `None`
    pub fn favicon(&self) -> Signal<Option<aureole::RgbaImage>> {
        self.shared.favicon
    }

    /// 指针停在链接上时的链接地址（不在链接上时是空的）
    pub fn hovered_link(&self) -> Signal<String> {
        self.shared.hovered_link
    }

    /// 加载进度：0 到 1
    pub fn progress(&self) -> Signal<f64> {
        self.shared.progress
    }

    /// 主框架上一次加载出错（地址打不开、断网、证书有问题……），下次开始加载时清掉
    pub fn load_error(&self) -> Signal<Option<crate::LoadError>> {
        self.shared.load_error
    }

    /// 页面要显示的提示文字（元素的 `title`），指针移开时是空的。显示在哪、什么样由应用定
    pub fn tooltip(&self) -> Signal<String> {
        self.shared.tooltip
    }

    /// 页面的渲染进程退出了（崩溃、内存不够、被结束）：页面停在那里不动，[`WebController::reload`] 恢复；下次开始加载时清掉
    pub fn crash(&self) -> Signal<Option<crate::Crash>> {
        self.shared.crash
    }

    /// 历史记录：能后退、前进到的页面，现在在第几个
    pub fn history(&self) -> Signal<crate::History> {
        self.shared.history
    }

    /// 跳到历史记录（[`WebController::history`]）里的第 `index` 个
    pub fn go_to_history(&self, index: usize) {
        crate::status::go_to_history(&self.shared, index);
    }

    /// 网站要用户名和密码（HTTP 认证、代理认证）时交给 `handler`，答复 [`crate::AuthRequest`]。不设时取消（页面显示没有权限）
    pub fn on_auth(&self, handler: impl Fn(crate::AuthRequest) + 'static) {
        self.shared.hooks.borrow_mut().auth = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::Auth, true);
    }

    /// 网站的证书有问题（过期、不受信任、和域名对不上……）时交给 `handler` 决定还要不要打开，答复 [`crate::CertificateError`]。
    /// 不设时不打开（页面显示证书错误）
    pub fn on_certificate_error(&self, handler: impl Fn(crate::CertificateError) + 'static) {
        self.shared.hooks.borrow_mut().certificate_error = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::CertificateError, true);
    }

    /// 页面关掉了自己（脚本里的 `window.close()`，一般是脚本开的新窗口）时调用 `handler`，应用把标签、窗口收起来。
    /// 之后元素不再按地址重新打开它，[`WebController::load`] 时才打开
    pub fn on_close_requested(&self, handler: impl Fn() + 'static) {
        self.shared.hooks.borrow_mut().close_requested = Some(Rc::new(handler));
    }

    /// 静音了
    pub fn muted(&self) -> Signal<bool> {
        self.shared.muted
    }

    /// 静音、取消静音。网页还没打开时记下，打开时按它
    pub fn set_muted(&self, muted: bool) {
        crate::status::set_muted(&self.shared, muted);
    }

    /// 页面的控制台消息（`console.log` 这些）交给 `handler`（主线程上）。设不设都照样记进日志（目标 `aureole_webview::console`）
    pub fn on_console(&self, handler: impl Fn(crate::ConsoleMessage) + 'static) {
        self.shared.hooks.borrow_mut().console = Some(Rc::new(handler));
        self.shared.page.set_hook(Hook::Console, true);
    }
}
