//! 按需启动 CEF：第一个网页视图挂载时加载内核、初始化，接进框架的事件循环；子进程的入口；退出时关掉。
//!
//! 负责：启动参数（缓存目录、不开沙箱、子进程程序的路径）、消息泵（CEF 在任意线程上要求"过多久跑一次"，
//! 投递到主线程执行）、macOS 上加载内核的框架、应用类要实现的接口、应用退出时关掉所有网页再关 CEF。
//!
//! CEF 的界面线程：
//!
//! - macOS、Linux 上就是框架的主线程：不开它自己的消息循环（`external_message_pump`），由它告诉我们什么时候
//!   要跑一轮（[`cef::do_message_loop_work`]），这里投递到主线程、用框架的定时器按时跑；有网页时另外每 1/30 秒
//!   保底跑一轮（macOS 上 CEF 只能这样接）。
//! - Windows 上是 CEF 自己开的线程（`multi_threaded_message_loop`，[`OWN_THREAD`]）：外部消息泵一轮最多要跑
//!   10 毫秒，在主线程上跑会挤掉框架的帧（120 Hz 以上的屏幕上一秒好几次）；回调在 CEF 的线程上，
//!   要写信号的投递回主线程（见 `state::Page`）。网页是异步建的（[`created`] 里交回来）。
//!
//! 一个进程里 CEF 只能初始化一次：失败以后记住错误，关掉以后不再打开网页。
//!
//! 开发时的环境变量：`AUREOLE_WEBVIEW_LOG`（`verbose`、`info`、`warning`、`error`、`disable`）调 CEF 的日志级别，
//! 同时打到标准错误；`AUREOLE_WEBVIEW_DEBUG_PORT` 开远程调试端口（默认不开）。

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use aureole_core::{MainThreadSender, Timer, after, detached, main_thread_sender};
use cef::{
    App, BrowserProcessHandler, CefString, ImplApp, ImplBrowser, ImplBrowserHost, ImplBrowserProcessHandler, ImplCommandLine, WrapApp,
    WrapBrowserProcessHandler, wrap_app, wrap_browser_process_handler,
};
// 包装宏生成的代码要用引用计数的方法
use cef::rc::Rc as _;

use crate::state::Shared;

/// 启动 CEF 失败的原因
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum EngineError {
    /// 找不到或加载不了内核的库、子进程程序
    Load(String),
    /// CEF 初始化失败
    Initialize,
    /// CEF 建不了网页
    Browser,
    /// CEF 已经关了（应用在退出）
    ShutDown,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(reason) => write!(f, "加载不了 CEF 内核：{reason}"),
            Self::Initialize => f.write_str("CEF 初始化失败"),
            Self::Browser => f.write_str("CEF 建不了网页"),
            Self::ShutDown => f.write_str("CEF 已经关了"),
        }
    }
}

impl std::error::Error for EngineError {}

/// 两轮消息泵之间最多隔多久：CEF 要求的延迟超过它时按它算，页面可能在动时保底泵也是这个间隔
const MAX_PUMP_DELAY: Duration = Duration::from_millis(1000 / 30);
/// 页面都静止时保底泵的间隔（每一轮在主线程上要零点几毫秒，每秒 30 轮的话空闲时也占着几个百分点的 CPU）
const IDLE_PUMP_DELAY: Duration = Duration::from_millis(250);
/// 退出时等网页关掉最多等多久
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// 网页都关掉以后、关 CEF 之前再跑几轮，让它放掉资源
const FINAL_PUMPS: u32 = 5;
/// 子进程程序的名字（Windows、Linux 上放在主程序旁边；macOS 上是应用包里几个辅助程序的可执行文件）
#[cfg(not(target_os = "macos"))]
const HELPER: &str = "aureole-webview-helper";
/// 网页画进 GPU 共享纹理（macOS 的 IOSurface、Windows 的 D3D11 共享纹理），还是给 CPU 的像素（Linux）。
/// Windows 上框架用软件渲染（WARP）、不是 D3D12 后端时也走 CPU 的像素：CEF 的 GPU 进程在软件渲染的"显卡"上
/// 建不出共享纹理要的资源，在别的显卡上建的又打不开
fn shared_texture() -> bool {
    #[cfg(target_os = "windows")]
    return aureole_runtime::shared_gpu().is_none_or(|gpu| gpu.info.backend == wgpu::Backend::Dx12 && gpu.info.kind != aureole_render::GpuKind::Software);
    #[cfg(not(target_os = "windows"))]
    cfg!(target_os = "macos")
}
/// CEF 的界面线程是它自己开的（Windows），不是框架的主线程：不跑消息泵，网页异步地建
const OWN_THREAD: bool = cfg!(target_os = "windows");

#[derive(Clone, Default)]
enum Status {
    /// 还没用到
    #[default]
    Idle,
    Running,
    /// 启动失败过，不再重试
    Failed(EngineError),
    /// 关掉了（应用在退出）
    Stopped,
}

#[derive(Default)]
struct Engine {
    status: Status,
    /// 交给 CEF 的命令行：CEF 一直引用着
    args: Option<ProcessArgs>,
    /// 投递到主线程（重入时补一轮泵）
    sender: Option<MainThreadSender>,
    /// 建过、CEF 还没销毁的网页（关掉以后要等它真正销毁，退出时才能关 CEF）
    browsers: Vec<cef::Browser>,
    /// 打开着网页的视图，退出时从里面拿走网页
    opened: Vec<Weak<Shared>>,
    /// 等 CEF 的上下文就绪再打开的视图
    pending: Vec<Weak<Shared>>,
    /// 下一轮泵的定时器和到期时刻
    pump_timer: Option<(Timer, Instant)>,
    /// 有网页时的保底泵
    backup: Option<Timer>,
}

thread_local! {
    static ENGINE: RefCell<Engine> = RefCell::default();
    /// 正在 `do_message_loop_work` 里
    static PUMPING: Cell<bool> = const { Cell::new(false) };
    /// 在里面时又要跑一轮（系统的模态循环这类）：出来以后马上补一轮
    static REENTERED: Cell<bool> = const { Cell::new(false) };
}

/// CEF 的上下文就绪了（`on_context_initialized`）：之后才建网页
static CONTEXT_READY: AtomicBool = AtomicBool::new(false);
/// 已经投递了一轮马上要跑的泵、还没跑：CEF 连着要求时不重复投递
static IMMEDIATE_POSTED: AtomicBool = AtomicBool::new(false);

/// CEF 已经启动好（没启动时启动）。只在主线程上调用；启动失败以后每次都返回同一个错误，不再重试
pub(crate) fn ensure_started() -> Result<(), EngineError> {
    match ENGINE.with_borrow(|e| e.status.clone()) {
        Status::Running => return Ok(()),
        Status::Failed(error) => return Err(error),
        Status::Stopped => return Err(EngineError::ShutDown),
        Status::Idle => {}
    }
    let begin = Instant::now();
    tracing::info!("第一次用到网页视图，启动 CEF");
    match start() {
        Ok((args, sender)) => {
            ENGINE.with_borrow_mut(|e| {
                e.status = Status::Running;
                e.args = Some(args);
                e.sender = Some(sender);
            });
            tracing::info!("CEF 启动好了，用了 {} 毫秒", begin.elapsed().as_millis());
            // 应用退出时关掉网页、再关 CEF
            aureole_runtime::at_exit(shutdown);
            Ok(())
        }
        Err(error) => {
            tracing::error!("{error}");
            ENGINE.with_borrow_mut(|e| e.status = Status::Failed(error.clone()));
            Err(error)
        }
    }
}

/// 加载内核、初始化 CEF
fn start() -> Result<(ProcessArgs, MainThreadSender), EngineError> {
    #[cfg(target_os = "macos")]
    let framework = {
        let dir = mac::framework_dir(false)?;
        mac::load_framework(&dir)?;
        dir
    };
    // Windows 上由改了名的 bootstrap.exe 加载时有沙箱信息：开沙箱，子进程用同一个程序（不用单独的子进程程序）
    let sandbox = SANDBOX_INFO.load(Ordering::Acquire);
    #[cfg(not(target_os = "macos"))]
    let helper = if sandbox.is_null() { Some(helper_path()?) } else { None };
    // 选定接口版本（内核按它给出函数表），要在别的 CEF 函数之前
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    #[cfg(target_os = "macos")]
    mac::install_app_protocol();

    let data = data_dir();
    let data_string = |name: Option<&str>| match (&data, name) {
        (Some(dir), Some(name)) => CefString::from(dir.join(name).to_string_lossy().as_ref()),
        (Some(dir), None) => CefString::from(dir.to_string_lossy().as_ref()),
        (None, _) => CefString::default(),
    };
    let log = log_level();
    // 界面语言（出错的页面、表单的提示）和页面看到的语言（`navigator.language`、请求头）
    let languages = languages();
    let settings = cef::Settings {
        no_sandbox: i32::from(!sandbox_enabled(sandbox)),
        windowless_rendering_enabled: 1,
        external_message_pump: i32::from(!OWN_THREAD),
        multi_threaded_message_loop: i32::from(OWN_THREAD),
        // 应用自己的命令行不归 CEF 管；要的开关在 `on_before_command_line_processing` 里加
        command_line_args_disabled: 1,
        root_cache_path: data_string(None),
        cache_path: data_string(None),
        log_file: data_string(Some("debug.log")),
        log_severity: log.unwrap_or(cef::LogSeverity::ERROR),
        remote_debugging_port: debug_port(),
        locale: CefString::from(languages.first().map_or("", String::as_str)),
        accept_language_list: CefString::from(accept_languages(&languages).as_str()),
        #[cfg(target_os = "macos")]
        framework_dir_path: CefString::from(framework.to_string_lossy().as_ref()),
        #[cfg(not(target_os = "macos"))]
        browser_subprocess_path: helper.map(|helper| CefString::from(helper.to_string_lossy().as_ref())).unwrap_or_default(),
        ..Default::default()
    };
    let sender = main_thread_sender();
    let mut app = EngineApp::new(BrowserProcess::new(sender.clone()), log.is_some(), adapter_luid());
    let args = ProcessArgs::own();
    let initialized = cef::initialize(Some(args.main()), Some(&settings), Some(&mut app), sandbox.cast());
    if initialized != 1 {
        tracing::error!("CEF 初始化失败，退出码 {}（同一个应用已经有一个实例在用数据目录时也会这样）", cef::get_exit_code());
        return Err(EngineError::Initialize);
    }
    // 应用自带的页面（`app://`）
    crate::scheme::install();
    Ok((args, sender))
}

/// 给 `shared` 打开网页（CEF 没启动时先启动）：离屏、按 `shared.geometry` 的大小和缩放、
/// 客户端用 [`crate::client::build`]，地址是 `shared.url`。打开以后放进 `shared.browser`。
/// 元素有了大小、第一次要显示时调用；已经打开时什么都不做。
/// CEF 刚启动、上下文还没就绪时先记下，就绪以后再打开
pub(crate) fn open(shared: &Rc<Shared>) -> Result<(), EngineError> {
    if shared.browser.borrow().is_some() || shared.opening.get() {
        return Ok(());
    }
    ensure_started()?;
    if !CONTEXT_READY.load(Ordering::Acquire) {
        ENGINE.with_borrow_mut(|e| {
            if !e.pending.iter().any(|w| std::ptr::eq(w.as_ptr(), Rc::as_ptr(shared))) {
                e.pending.push(Rc::downgrade(shared));
            }
        });
        return Ok(());
    }
    create(shared)
}

/// 离屏网页的窗口设置（应用打开的和内核替页面开的新窗口一样）
pub(crate) fn window_info() -> cef::WindowInfo {
    cef::WindowInfo {
        windowless_rendering_enabled: 1,
        shared_texture_enabled: i32::from(shared_texture()),
        // 框架发帧开始的信号时（[`Pacing::Signal`]）才开：元素每帧要的时候发，见 `render::begin_frame`
        external_begin_frame_enabled: i32::from(pacing() == Pacing::Signal),
        runtime_style: cef::RuntimeStyle::ALLOY,
        ..Default::default()
    }
}

/// 离屏网页的设置：页面自己没设背景时是白底（不设的话离屏画出来是透明的，透出窗口的底色）。
/// 帧率：框架发信号时节奏由信号定，离屏回调的最高帧率放开到 240（不卡在默认的 60）；内核自己出帧时是 `frame_rate`
/// （窗口所在屏幕的刷新率，0 是还不知道，先按 [`DEFAULT_FRAME_RATE`]，元素挂上以后对上）
pub(crate) fn browser_settings(frame_rate: i32) -> cef::BrowserSettings {
    let rate = match pacing() {
        Pacing::Signal => 240,
        Pacing::Kernel if frame_rate > 0 => frame_rate,
        Pacing::Kernel => DEFAULT_FRAME_RATE,
    };
    cef::BrowserSettings { windowless_frame_rate: rate, background_color: 0xFFFF_FFFF, ..Default::default() }
}

/// 还不知道屏幕的刷新率时内核出帧的帧率
pub(crate) const DEFAULT_FRAME_RATE: i32 = 60;

/// 出帧的节奏由谁定
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pacing {
    /// 框架每帧给内核发帧开始的信号：出帧的时刻跟屏幕的刷新对齐。内核在这种方式下一次只让一帧在路上（上一帧确认完
    /// 才收下一次信号，没改画面的帧确认得慢）：一帧的活超过一次刷新的时间就掉到半速，只跑动画回调的页面只有三分之一的帧
    Signal,
    /// 内核按窗口所在屏幕的刷新率自己出帧：前后两帧能重叠着做，重的页面不掉到半速，只跑动画回调的页面也是满帧。
    /// 新画面常常正好在两次刷新的交界到，所以到了先排队、框架出帧时换上最早的一帧（`render::refresh`）
    Kernel,
}

/// 用哪种节奏：macOS 上默认内核自己出帧，别的平台默认框架发信号（Windows 上那样量过、调过）。
/// 环境变量 `AUREOLE_WEBVIEW_FRAMES=kernel` 或 `signal` 换一种（对比、排查用）
pub(crate) fn pacing() -> Pacing {
    static PACING: std::sync::OnceLock<Pacing> = std::sync::OnceLock::new();
    *PACING.get_or_init(|| match std::env::var("AUREOLE_WEBVIEW_FRAMES").as_deref() {
        Ok("kernel") => Pacing::Kernel,
        Ok("signal") => Pacing::Signal,
        _ if cfg!(target_os = "macos") => Pacing::Kernel,
        _ => Pacing::Signal,
    })
}

/// 真正建网页
fn create(shared: &Rc<Shared>) -> Result<(), EngineError> {
    let window = window_info();
    let settings = browser_settings(shared.frame_rate.get());
    let mut client = crate::client::build(shared).unwrap_or_else(|| fallback::client(shared));
    // 用哪一套数据（默认的是全局的，不用给）
    let profile = shared.page.profile();
    let mut context = (profile.name() != Some("")).then(|| profile.context()).flatten();
    let url = shared.url.get_untracked();
    // 有用户脚本时先建成空页面，建好、注册完脚本再加载（第一个文档也赶得上，见 `scripts`）
    let wait_for_scripts = !crate::scripts::wanted(shared).is_empty();
    *shared.initial_load.borrow_mut() = wait_for_scripts.then(|| url.clone());
    // 空地址时内核不起渲染进程，注册的脚本到不了第一个真正的文档；`about:blank` 有一个真正的空文档
    let address = CefString::from(if wait_for_scripts { "about:blank" } else { url.as_str() });
    shared.page.render().opened();
    shared.opening.set(true);
    ENGINE.with_borrow_mut(|e| {
        e.opened.retain(|w| w.strong_count() > 0);
        e.opened.push(Rc::downgrade(shared));
    });
    if OWN_THREAD {
        // CEF 的界面线程不是这个线程：只能异步地建，建好了从 `on_after_created` 交回来（`created`）
        if cef::browser_host_create_browser(Some(&window), Some(&mut client), Some(&address), Some(&settings), None, context.as_mut()) != 1 {
            shared.opening.set(false);
            tracing::error!("CEF 建不了网页（{url}）");
            return Err(EngineError::Browser);
        }
        tracing::debug!("在打开网页 {url}");
        return Ok(());
    }
    // 建的时候 CEF 会同步回调客户端（量大小这类，`on_after_created` 也是），这里不借着 `ENGINE` 和 `shared.browser`
    let browser = cef::browser_host_create_browser_sync(Some(&window), Some(&mut client), Some(&address), Some(&settings), None, context.as_mut());
    let Some(browser) = browser else {
        shared.opening.set(false);
        tracing::error!("CEF 建不了网页（{url}）");
        return Err(EngineError::Browser);
    };
    created(shared, browser);
    Ok(())
}

/// 网页建好了（主线程上，`on_after_created` 转过来；同步地建时建完也调一次，重复的不算）：放进 `shared.browser`、
/// 记下来退出时关掉。建好之前元素已经卸载（不再等它）时直接关掉
pub(crate) fn created(shared: &Rc<Shared>, browser: cef::Browser) {
    if !ENGINE.with_borrow(|e| matches!(e.status, Status::Running)) {
        // CEF 已经关了（应用在退出）：这时放掉 CEF 的对象会出错，留着它
        std::mem::forget(browser);
        return;
    }
    let open = shared.browser.borrow().clone();
    if open.is_some_and(|open| open.identifier() == browser.identifier()) {
        return;
    }
    // 顺手清掉已经销毁的（没有保底泵时没人清）；放到借用外面再丢：最后一个引用放掉时 CEF 会释放它的客户端
    let destroyed = ENGINE.with_borrow_mut(|e| {
        let (live, destroyed): (Vec<_>, Vec<_>) = std::mem::take(&mut e.browsers).into_iter().partition(|b| b.is_valid() == 1);
        e.browsers = live;
        if !e.browsers.iter().any(|b| b.identifier() == browser.identifier()) {
            e.browsers.push(browser.clone());
        }
        destroyed
    });
    drop(destroyed);
    start_backup_pump();
    if !shared.opening.replace(false) {
        tracing::debug!("网页建好时元素已经卸载了，关掉");
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        return;
    }
    tracing::debug!("打开了网页 {}", shared.url.get_untracked());
    *shared.browser.borrow_mut() = Some(browser.clone());
    crate::render::wake(shared);
    // 打开之前设的缩放、静音
    crate::tools::apply_zoom(shared);
    crate::status::apply_muted(shared);
    // 异步建好时（投递到主线程执行，不在元素的更新里）：元素重画一次，重新问它要不要出帧
    // （网页还没建好时它不要，这时没有别的动静叫醒它）。同步地建时就在元素的更新里，更新完自己会问
    if OWN_THREAD {
        shared.frame.update(|n| *n = n.wrapping_add(1));
    }
    if let Some(host) = browser.host() {
        // 焦点可能在网页打开之前就到了元素上
        if shared.focused.get() {
            host.set_focus(1);
        }
        crate::accessibility::created(shared, &host);
        crate::lifecycle::created(shared, &host);
        crate::profile::apply_overrides(shared);
        // 用户脚本：内核替页面开的新窗口一建好就在加载，让脚本在已经有的文档里也跑一次。
        // 先建成空页面的，注册的结果都回来以后再加载（`scripts::load_when_ready`）
        crate::scripts::attach(shared, &host, shared.popup.get());
    }
}

/// 上下文就绪以后，打开之前记下的视图
fn open_pending() {
    let pending = ENGINE.with_borrow_mut(|e| std::mem::take(&mut e.pending));
    for shared in pending.iter().filter_map(Weak::upgrade) {
        if shared.browser.borrow().is_none()
            && let Err(error) = create(&shared)
        {
            tracing::warn!("{error}");
        }
    }
}

/// 关掉 `shared` 的网页（元素卸载时）：从 `shared.browser` 拿走，让 CEF 关掉它（不问网页的 `beforeunload`）。
/// CEF 之后异步地销毁它，期间消息泵照常跑
pub(crate) fn close(shared: &Shared) {
    let me: *const Shared = shared;
    let _ = ENGINE.try_with(|e| {
        let mut e = e.borrow_mut();
        e.pending.retain(|w| !std::ptr::eq(w.as_ptr(), me) && w.strong_count() > 0);
        e.opened.retain(|w| !std::ptr::eq(w.as_ptr(), me) && w.strong_count() > 0);
    });
    // 还在建的不等了：建好时直接关掉（见 `created`）
    shared.opening.set(false);
    let browser = shared.browser.borrow_mut().take();
    if let Some(host) = browser.and_then(|b| b.host()) {
        tracing::debug!("关掉网页");
        host.close_browser(1);
    }
}

/// 收下内核替页面开的新窗口（主线程上，`popup` 交来）：和应用打开的一样登记，退出时关掉
pub(crate) fn adopt(shared: &Rc<Shared>, browser: cef::Browser) {
    shared.popup.set(true);
    ENGINE.with_borrow_mut(|e| {
        e.opened.retain(|w| w.strong_count() > 0);
        e.opened.push(Rc::downgrade(shared));
    });
    shared.opening.set(true);
    created(shared, browser);
}

/// 放掉一个还开着的网页（它的控制器和元素都没了）：CEF 还在时关掉；已经关了（应用在退出）时留着，
/// 这时放掉 CEF 的对象会出错
pub(crate) fn discard(browser: cef::Browser) {
    let running = ENGINE.try_with(|e| e.try_borrow().is_ok_and(|e| matches!(e.status, Status::Running))).unwrap_or(false);
    if !running {
        std::mem::forget(browser);
        return;
    }
    if let Some(host) = browser.host() {
        tracing::debug!("网页的控制器放掉了，关掉网页");
        host.close_browser(1);
    }
}

/// 给还在的网页各发一次帧开始的信号
fn begin_frames(browsers: &[cef::Browser]) {
    // 内核自己出帧时没开这个信号，不能发
    if pacing() != Pacing::Signal {
        return;
    }
    for browser in browsers {
        if browser.is_valid() == 1
            && let Some(host) = browser.host()
        {
            host.send_external_begin_frame();
        }
    }
}

/// 应用退出时：关掉还开着的网页，等 CEF 销毁它们（最多 [`CLOSE_TIMEOUT`]），再关 CEF。
/// 在主线程上、事件循环结束之前调用（窗口可能还开着：系统的"退出"直接结束进程）；没启动过时什么都不做。
/// 关掉以后再挂载的网页视图打不开（[`EngineError::ShutDown`]）。
/// CEF 有自己的界面线程时（Windows）关网页在那个线程上做，这里只等；没有时这里跑消息泵
pub(crate) fn shutdown() {
    let begin = Instant::now();
    let Some((opened, browsers)) = ENGINE.with_borrow_mut(|e| {
        if !matches!(e.status, Status::Running) {
            return None;
        }
        e.status = Status::Stopped;
        e.pending.clear();
        if let Some((timer, _)) = e.pump_timer.take() {
            timer.cancel();
        }
        if let Some(timer) = e.backup.take() {
            timer.cancel();
        }
        Some((std::mem::take(&mut e.opened), std::mem::take(&mut e.browsers)))
    }) else {
        return;
    };
    // 窗口还开着时视图手里还拿着网页（`browsers` 里有同一个）：拿走，关 CEF 之前放掉所有引用
    for shared in opened.iter().filter_map(Weak::upgrade) {
        drop(shared.browser.try_borrow_mut().ok().and_then(|mut b| b.take()));
    }
    // 已经在关的再关一次没关系
    for browser in &browsers {
        if browser.is_valid() == 1
            && let Some(host) = browser.host()
        {
            host.close_browser(1);
        }
    }
    let deadline = begin + CLOSE_TIMEOUT;
    while browsers.iter().any(|b| b.is_valid() == 1) && Instant::now() < deadline {
        // 出帧跟着框架的帧时钟（帧开始的信号）：退出时框架不再出帧，网页收尾要的帧在这里给
        begin_frames(&browsers);
        if !OWN_THREAD {
            work();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if browsers.iter().any(|b| b.is_valid() == 1) {
        tracing::warn!("退出时还有网页没关掉，照样关 CEF");
    }
    for _ in 0..FINAL_PUMPS {
        begin_frames(&browsers);
        if !OWN_THREAD {
            work();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(browsers);
    cef::shutdown();
    tracing::info!("关掉了 CEF，用了 {} 毫秒", begin.elapsed().as_millis());
}

// ---------- 消息泵 ----------

/// CEF 要求过 `delay_ms` 毫秒跑一轮（任意线程上）：投递到主线程
fn request_pump(sender: &MainThreadSender, delay_ms: i64) {
    if delay_ms <= 0 {
        if !IMMEDIATE_POSTED.swap(true, Ordering::AcqRel) {
            sender.send(|| {
                IMMEDIATE_POSTED.store(false, Ordering::Release);
                pump();
            });
        }
    } else {
        let delay = Duration::from_millis(delay_ms as u64).min(MAX_PUMP_DELAY);
        sender.send(move || schedule_pump(delay));
    }
}

/// 过 `delay` 跑一轮（主线程上）。已经排了更早的就不动，排的更晚就换成这个
fn schedule_pump(delay: Duration) {
    let due = Instant::now() + delay;
    let replaced = ENGINE.with_borrow_mut(|e| {
        if !matches!(e.status, Status::Running) || e.pump_timer.is_some_and(|(_, at)| at <= due) {
            return None;
        }
        Some(e.pump_timer.take())
    });
    let Some(replaced) = replaced else { return };
    if let Some((timer, _)) = replaced {
        timer.cancel();
    }
    // 不归当前的作用域：元素卸载、窗口关闭时不跟着取消
    let timer = detached(|| {
        after(delay, || {
            ENGINE.with_borrow_mut(|e| e.pump_timer = None);
            pump();
        })
    });
    ENGINE.with_borrow_mut(|e| e.pump_timer = Some((timer, due)));
}

/// 有网页时的保底泵：页面可能在动（有动静、在加载）时每 [`MAX_PUMP_DELAY`] 一轮，都静止时每 [`IDLE_PUMP_DELAY`]
/// 一轮（CEF 要做事时自己会要求泵，保底只是防漏）；网页都销毁了就停
fn start_backup_pump() {
    if OWN_THREAD || ENGINE.with_borrow(|e| e.backup.is_some()) {
        return;
    }
    schedule_backup(MAX_PUMP_DELAY);
}

fn schedule_backup(delay: Duration) {
    let timer = detached(|| after(delay, backup_pump));
    ENGINE.with_borrow_mut(|e| e.backup = Some(timer));
}

fn backup_pump() {
    // 销毁了的放到借用外面再丢：最后一个引用放掉时 CEF 会释放它的客户端
    let (idle, destroyed) = ENGINE.with_borrow_mut(|e| {
        let (live, destroyed): (Vec<_>, Vec<_>) = std::mem::take(&mut e.browsers).into_iter().partition(|b| b.is_valid() == 1);
        e.browsers = live;
        (e.browsers.is_empty(), destroyed)
    });
    drop(destroyed);
    if idle {
        tracing::debug!("网页都销毁了，停下保底的消息泵");
        ENGINE.with_borrow_mut(|e| e.backup = None);
        return;
    }
    pump();
    let busy = ENGINE.with_borrow(|e| {
        e.opened.iter().filter_map(Weak::upgrade).any(|s| crate::render::awake(&s) || s.loading.get_untracked())
    });
    schedule_backup(if busy { MAX_PUMP_DELAY } else { IDLE_PUMP_DELAY });
}

/// 跑一轮 CEF 的消息循环（主线程上，CEF 在运行时）
fn pump() {
    if !ENGINE.with_borrow(|e| matches!(e.status, Status::Running)) {
        return;
    }
    if !work() {
        return;
    }
    if REENTERED.replace(false)
        && let Some(sender) = ENGINE.with_borrow(|e| e.sender.clone())
    {
        request_pump(&sender, 0);
    }
}

/// `do_message_loop_work`，不重入：已经在里面时（CEF 的回调里开了系统的模态循环，事件循环在里面接着转）
/// 记下来、返回 `false`，出来以后补一轮
fn work() -> bool {
    if PUMPING.get() {
        REENTERED.set(true);
        return false;
    }
    struct Pumping;
    impl Drop for Pumping {
        fn drop(&mut self) {
            PUMPING.set(false);
        }
    }
    PUMPING.set(true);
    let _pumping = Pumping;
    cef::do_message_loop_work();
    true
}

// ---------- CEF 的应用对象 ----------

wrap_app! {
    struct EngineApp {
        process: BrowserProcessHandler,
        log_to_stderr: bool,
        // 让 CEF 的 GPU 进程用这块显卡（`use-adapter-luid` 的值，见 `adapter_luid`）
        adapter: Option<String>,
    }

    impl App {
        fn on_before_command_line_processing(&self, _process_type: Option<&CefString>, command_line: Option<&mut cef::CommandLine>) {
            let Some(command_line) = command_line else { return };
            // 不自己开窗口
            command_line.append_switch(Some(&CefString::from("no-startup-window")));
            if let Some(luid) = &self.adapter {
                command_line.append_switch_with_value(Some(&CefString::from("use-adapter-luid")), Some(&CefString::from(luid.as_str())));
            }
            // 内核自己开窗口画的只有开发者工具：内核判断窗口被挡住的那一套在有的机器上会误判（桌面上有透明的置顶窗口），
            // 窗口明明露着也停掉出帧。关掉这项判断（只管内核自己的窗口，离屏的网页不受影响）
            #[cfg(target_os = "windows")]
            command_line.append_switch_with_value(Some(&CefString::from("disable-features")), Some(&CefString::from("CalculateNativeWinOcclusion")));
            // 网站要认证时都交给客户端的回调（`on_auth`）：内核自己的登录框在离屏的网页上弹不出来，请求会一直挂着
            command_line.append_switch(Some(&CefString::from("disable-chrome-login-prompt")));
            // 不碰系统的钥匙串：内核要从钥匙串取加密 Cookie 的密钥，程序每次编译签名都变，系统每次运行都弹框要密码；
            // 没人回答时退出也卡住（退出前要把 Cookie 写回磁盘）。Cookie 改用固定的密钥加密。
            // 签好名、发布出去的应用要用钥匙串时设 `AUREOLE_WEBVIEW_KEYCHAIN=1`
            #[cfg(target_os = "macos")]
            if std::env::var_os("AUREOLE_WEBVIEW_KEYCHAIN").is_none_or(|v| v != "1") {
                command_line.append_switch(Some(&CefString::from("use-mock-keychain")));
            }
            if self.log_to_stderr {
                command_line.append_switch_with_value(Some(&CefString::from("enable-logging")), Some(&CefString::from("stderr")));
            }
        }

        fn on_register_custom_schemes(&self, registrar: Option<&mut cef::SchemeRegistrar>) {
            crate::scheme::register(registrar);
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(self.process.clone())
        }
    }
}

wrap_browser_process_handler! {
    struct BrowserProcess {
        sender: MainThreadSender,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            CONTEXT_READY.store(true, Ordering::Release);
            self.sender.send(open_pending);
        }

        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            request_pump(&self.sender, delay_ms);
        }

        // 同一个应用又启动了一个实例、用着同一个数据目录：什么都不做（默认会开一个浏览器窗口）
        fn on_already_running_app_relaunch(&self, _command_line: Option<&mut cef::CommandLine>, _current_directory: Option<&CefString>) -> ::std::os::raw::c_int {
            1
        }
    }
}

// ---------- 子进程 ----------

/// bootstrap 交来的沙箱信息（Windows 上应用编成动态库、由改了名的 `bootstrap.exe` 加载时，见 [`run_bootstrapped`]）
static SANDBOX_INFO: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// 网页的子进程（渲染、GPU……）在沙箱里：Windows 上由改了名的 `bootstrap.exe` 启动、经 `run_bootstrapped`（只在 Windows 上有）进来时是；
/// macOS 上默认是（辅助程序启动时进沙箱，见 [`run_helper`]），设了环境变量 `AUREOLE_WEBVIEW_NO_SANDBOX=1` 时不是
pub fn is_sandboxed() -> bool {
    sandbox_enabled(SANDBOX_INFO.load(Ordering::Acquire))
}

/// 启动内核时开不开沙箱：Windows 上看 bootstrap 交没交来沙箱信息，macOS 上默认开（`AUREOLE_WEBVIEW_NO_SANDBOX=1` 关），
/// 别的平台不开
fn sandbox_enabled(info: *mut std::ffi::c_void) -> bool {
    if cfg!(target_os = "macos") {
        std::env::var_os("AUREOLE_WEBVIEW_NO_SANDBOX").is_none_or(|v| v != "1")
    } else {
        !info.is_null()
    }
}

/// Windows 上开沙箱的入口（一般用 [`crate::bootstrap_entry!`] 生成，不直接调）。
///
/// CEF 在 Windows 上只能这样开沙箱：应用编成动态库（`cdylib`），导出 `RunWinMain`，由改名成应用名的
/// `bootstrap.exe`（CEF 带的，和 `libcef.dll` 放在一起）加载；主进程和所有子进程都是这个程序，不再用单独的子进程程序。
/// 是子进程时跑完子进程返回退出码；是主进程时记下沙箱信息（启动内核时交给它，打开沙箱），调 `main`（应用本来的入口），
/// 返回 0
#[cfg(target_os = "windows")]
pub fn run_bootstrapped(sandbox_info: *mut std::ffi::c_void, main: impl FnOnce()) -> i32 {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let args = ProcessArgs::all();
    let mut app = crate::bridge::helper_app();
    let code = cef::execute_process(Some(args.main()), Some(&mut app), sandbox_info.cast());
    if code >= 0 {
        return code;
    }
    SANDBOX_INFO.store(sandbox_info, Ordering::Release);
    main();
    0
}

/// 子进程程序（`aureole-webview-helper`）的入口：按命令行跑 CEF 的子进程，返回退出码
pub fn run_helper() -> i32 {
    let args = ProcessArgs::all();
    // macOS：主进程开了沙箱时先进沙箱，再加载内核（进了沙箱以后照样能读框架里的文件）
    #[cfg(target_os = "macos")]
    let _sandbox = match mac::framework_dir(true).and_then(|dir| {
        let sandbox = mac::enter_sandbox(&dir, args.main())?;
        mac::load_framework(&dir)?;
        Ok(sandbox)
    }) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    // 子进程也要登记 `app://`，渲染进程里给页面注入 `window.aureole`
    let mut app = crate::bridge::helper_app();
    let code = cef::execute_process(Some(args.main()), Some(&mut app), std::ptr::null_mut());
    if code < 0 {
        eprintln!("这是 CEF 的子进程程序，由应用里的网页视图启动，不直接运行");
        return 1;
    }
    code
}

/// 交给 CEF 的命令行（`argc`、`argv`）：字符串要一直活着。Windows 上 CEF 自己读命令行，这里只有模块句柄
struct ProcessArgs {
    #[cfg(not(target_os = "windows"))]
    _strings: Vec<std::ffi::CString>,
    #[cfg(not(target_os = "windows"))]
    _pointers: Vec<*mut std::ffi::c_char>,
    #[cfg(not(target_os = "windows"))]
    main: cef::MainArgs,
    #[cfg(target_os = "windows")]
    args: cef::args::Args,
}

impl ProcessArgs {
    /// 主进程：只给程序路径，应用自己的参数不交给 CEF
    fn own() -> Self {
        Self::from_os(std::env::args_os().take(1))
    }

    /// 子进程：CEF 给的整个命令行
    fn all() -> Self {
        Self::from_os(std::env::args_os())
    }

    #[cfg(not(target_os = "windows"))]
    fn from_os(args: impl Iterator<Item = std::ffi::OsString>) -> Self {
        use std::os::unix::ffi::OsStringExt;
        // 系统给的参数里没有 0 字节
        let strings: Vec<std::ffi::CString> = args.filter_map(|arg| std::ffi::CString::new(arg.into_vec()).ok()).collect();
        let mut pointers: Vec<*mut std::ffi::c_char> = strings.iter().map(|s| s.as_ptr().cast_mut()).collect();
        let argc = pointers.len() as _;
        // 和 `main` 拿到的一样，最后跟一个空指针
        pointers.push(std::ptr::null_mut());
        let main = cef::MainArgs { argc, argv: pointers.as_mut_ptr() };
        Self { _strings: strings, _pointers: pointers, main }
    }

    #[cfg(target_os = "windows")]
    fn from_os(_args: impl Iterator<Item = std::ffi::OsString>) -> Self {
        Self { args: cef::args::Args::new() }
    }

    fn main(&self) -> &cef::MainArgs {
        #[cfg(not(target_os = "windows"))]
        return &self.main;
        #[cfg(target_os = "windows")]
        return self.args.as_main_args();
    }
}

/// 用户界面语言（按偏好的先后，如 `zh-CN`、`en-US`）。Windows 上内核不看系统设置，不设时界面文字一律是英文、
/// 页面看到的语言一律是 `en-US,en`，所以读系统的；macOS、Linux 上内核自己按系统设置、语言环境变量定，这里不给
fn languages() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
        let (mut count, mut len) = (0u32, 0u32);
        // SAFETY: 第一次缓冲区为空、只写需要的长度；第二次缓冲区有 len 个 u16。结果是以两个 0 结尾的几个字符串
        let buffer = unsafe {
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len).is_err() {
                return Vec::new();
            }
            let mut buffer = vec![0u16; len as usize];
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(windows::core::PWSTR(buffer.as_mut_ptr())), &mut len).is_err() {
                return Vec::new();
            }
            buffer
        };
        buffer.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
    #[cfg(not(target_os = "windows"))]
    Vec::new()
}

/// 请求头里的语言、`navigator.languages`：每种语言后面跟上不带地区的（`zh-CN,zh,en-US,en`），和 Chromium 默认的一样
pub(crate) fn accept_languages(languages: &[String]) -> String {
    let mut list: Vec<&str> = Vec::new();
    for language in languages {
        let base = language.split('-').next().unwrap_or(language);
        for code in [language.as_str(), base] {
            if !code.is_empty() && !list.contains(&code) {
                list.push(code);
            }
        }
    }
    list.join(",")
}

/// 主程序旁边的子进程程序
#[cfg(not(target_os = "macos"))]
fn helper_path() -> Result<PathBuf, EngineError> {
    let exe = std::env::current_exe().map_err(|e| EngineError::Load(format!("找不到主程序的路径：{e}")))?;
    let helper = exe.with_file_name(format!("{HELPER}{}", std::env::consts::EXE_SUFFIX));
    if helper.is_file() {
        Ok(helper)
    } else {
        Err(EngineError::Load(format!("找不到子进程程序 {}，它要放在主程序旁边", helper.display())))
    }
}

// ---------- 数据目录、开发用的设置 ----------

/// 按名字的一套配置的数据目录：`<数据目录>/profiles/<名字>`（内核要求在全局的数据目录下面）
pub(crate) fn profile_dir(name: &str) -> Option<PathBuf> {
    Some(data_dir()?.join("profiles").join(name))
}

/// 网页的数据（Cookie、本地存储、缓存）放在哪，按应用分开：macOS 是 `~/Library/Application Support/<应用>/webview`，
/// Windows 是 `%LOCALAPPDATA%\<应用>\webview`，其他系统是 `$XDG_DATA_HOME/<应用>/webview`（没有时 `~/.local/share`）。
/// 应用在 macOS 上打包了是包标识符，其他时候是可执行文件名（和框架放编译好的管线的目录一样分）。
/// 找不到家目录时是 `None`，CEF 用它自己的默认位置
fn data_dir() -> Option<PathBuf> {
    let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = if cfg!(target_os = "macos") {
        absolute("HOME").map(|home| home.join("Library").join("Application Support"))
    } else if cfg!(target_os = "windows") {
        absolute("LOCALAPPDATA")
    } else {
        absolute("XDG_DATA_HOME").or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))
    }?;
    #[cfg(target_os = "macos")]
    let app = mac::bundle_identifier();
    #[cfg(not(target_os = "macos"))]
    let app = None;
    let app = app.or_else(|| std::env::current_exe().ok()?.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    Some(base.join(app).join("webview"))
}

/// Windows：框架用的显卡（窗口共用的设备所在的适配器）的编号，让 CEF 的 GPU 进程用同一块（`use-adapter-luid`，
/// 值是"高位,低位"）。双显卡的电脑上两边各选各的话（框架按窗口所在的显示器选，CEF 按系统默认的），
/// CEF 画好的共享纹理在框架的设备上打不开，网页一片空白。还没有窗口、不走共享纹理（[`shared_texture`]）时为 `None`
/// （CEF 按默认的选）
fn adapter_luid() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        let gpu = aureole_runtime::shared_gpu().filter(|_| shared_texture())?;
        // SAFETY: 只在 D3D12 后端上取出底层设备，只读它所在适配器的编号
        let luid = unsafe { gpu.device.as_hal::<wgpu::wgc::api::Dx12>()?.raw_device().GetAdapterLuid() };
        Some(format!("{},{}", luid.HighPart, luid.LowPart))
    }
    #[cfg(not(target_os = "windows"))]
    None
}

/// `AUREOLE_WEBVIEW_LOG` 给的 CEF 日志级别；给了时日志也打到标准错误
fn log_level() -> Option<cef::LogSeverity> {
    let level = std::env::var("AUREOLE_WEBVIEW_LOG").ok()?;
    let severity = match level.trim().to_ascii_lowercase().as_str() {
        "verbose" | "debug" => cef::LogSeverity::VERBOSE,
        "info" => cef::LogSeverity::INFO,
        "warning" | "warn" => cef::LogSeverity::WARNING,
        "error" => cef::LogSeverity::ERROR,
        "disable" | "off" => cef::LogSeverity::DISABLE,
        other => {
            tracing::warn!("AUREOLE_WEBVIEW_LOG={other} 不认识，用 info");
            cef::LogSeverity::INFO
        }
    };
    Some(severity)
}

/// `AUREOLE_WEBVIEW_DEBUG_PORT` 给的远程调试端口；默认 0，不开
fn debug_port() -> std::os::raw::c_int {
    std::env::var("AUREOLE_WEBVIEW_DEBUG_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok()).map_or(0, |p| p as _)
}

// ---------- 退路的客户端 ----------

/// [`crate::client::build`] 还没给出客户端时用的最小客户端：离屏绘制要的大小和缩放（按 `shared.geometry`），
/// 加载的结果记日志，画好的帧丢掉
mod fallback {
    use std::rc::Rc;
    use std::sync::Arc;

    use cef::{
        Browser, CefString, Client, Errorcode, Frame, ImplClient, ImplFrame, ImplLoadHandler, ImplRenderHandler, LoadHandler, Rect, RenderHandler, ScreenInfo,
        WrapClient, WrapLoadHandler, WrapRenderHandler, wrap_client, wrap_load_handler, wrap_render_handler,
    };
    use cef::rc::Rc as _;

    use crate::state::{Page, Shared};

    pub(super) fn client(shared: &Rc<Shared>) -> Client {
        FallbackClient::new(ViewSize::new(shared.page.clone()), LoadLog::new())
    }

    wrap_render_handler! {
        struct ViewSize {
            page: Arc<Page>,
        }

        impl RenderHandler {
            fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
                let Some(rect) = rect else { return };
                let size = self.page.geometry().size;
                // CEF 要不是空的大小
                rect.x = 0;
                rect.y = 0;
                rect.width = (size[0].round() as i32).max(1);
                rect.height = (size[1].round() as i32).max(1);
            }

            fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
                let Some(info) = screen_info else { return 0 };
                let scale = self.page.geometry().scale;
                info.device_scale_factor = if scale > 0.0 { scale } else { 1.0 };
                1
            }
        }
    }

    wrap_load_handler! {
        struct LoadLog;

        impl LoadHandler {
            fn on_load_end(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, http_status_code: ::std::os::raw::c_int) {
                if let Some(frame) = frame.filter(|f| f.is_main() == 1) {
                    tracing::info!("网页加载完了：{}（状态码 {http_status_code}）", CefString::from(&frame.url()));
                }
            }

            fn on_load_error(
                &self,
                _browser: Option<&mut Browser>,
                frame: Option<&mut Frame>,
                error_code: Errorcode,
                error_text: Option<&CefString>,
                failed_url: Option<&CefString>,
            ) {
                if frame.is_some_and(|f| f.is_main() == 1) {
                    let text = error_text.map(ToString::to_string).unwrap_or_default();
                    let url = failed_url.map(ToString::to_string).unwrap_or_default();
                    tracing::warn!("网页加载失败：{url}（{error_code:?} {text}）");
                }
            }
        }
    }

    wrap_client! {
        struct FallbackClient {
            render: RenderHandler,
            load: LoadHandler,
        }

        impl Client {
            fn render_handler(&self) -> Option<RenderHandler> {
                Some(self.render.clone())
            }

            fn load_handler(&self) -> Option<LoadHandler> {
                Some(self.load.clone())
            }
        }
    }
}

// ---------- macOS ----------

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use objc2::encode::{Encode, Encoding};
    use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, Sel};
    use objc2::{MainThreadMarker, ffi, msg_send, sel};
    use objc2_app_kit::NSApplication;

    use super::EngineError;

    /// 内核的框架（应用包的 `Contents/Frameworks` 里）
    const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
    /// 框架里的库
    const LIBRARY: &str = "Chromium Embedded Framework";

    /// 内核框架的目录。主程序是 `<应用>.app/Contents/MacOS/<应用>`，框架在 `Contents/Frameworks/` 下；
    /// 辅助程序是 `Contents/Frameworks/<应用> Helper.app/Contents/MacOS/<应用> Helper`，往上三层就是 `Frameworks`
    pub(super) fn framework_dir(helper: bool) -> Result<PathBuf, EngineError> {
        let exe = std::env::current_exe().map_err(|e| EngineError::Load(format!("找不到程序的路径：{e}")))?;
        let exe = exe.canonicalize().unwrap_or(exe);
        let dir = exe.parent().unwrap_or(Path::new("/"));
        let frameworks = if helper { dir.join("../../..") } else { dir.join("../Frameworks") };
        let framework = frameworks.join(FRAMEWORK);
        framework.canonicalize().map_err(|_| {
            EngineError::Load(format!("找不到 {}：macOS 上要把应用打包成 .app，内核框架放在 Contents/Frameworks 里", framework.display()))
        })
    }

    /// 内核框架里的沙箱库
    const SANDBOX_LIBRARY: &str = "Libraries/libcef_sandbox.dylib";

    /// 子进程的沙箱：留到进程结束，丢掉时销毁
    pub(super) struct Sandbox {
        library: libloading::Library,
        context: std::ptr::NonNull<std::ffi::c_void>,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // SAFETY: `cef_sandbox_destroy` 的签名是 `void (void*)`，参数是这个库的 `cef_sandbox_initialize` 返回的句柄，只销毁一次
            unsafe {
                if let Ok(destroy) = self.library.get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(b"cef_sandbox_destroy\0") {
                    destroy(self.context.as_ptr());
                }
            }
        }
    }

    /// 主进程开了沙箱时，它启动子进程时在命令行里给沙箱的策略（`--seatbelt-client=…`）：这时加载框架里的沙箱库、
    /// 进沙箱，要在加载内核之前。没有这个参数（主进程没开沙箱）时是 `Ok(None)`；进不去时报错（不在沙箱外面悄悄跑）
    pub(super) fn enter_sandbox(framework: &Path, args: &cef::MainArgs) -> Result<Option<Sandbox>, EngineError> {
        let argv: &[*mut std::ffi::c_char] = if args.argv.is_null() || args.argc <= 0 {
            &[]
        } else {
            // SAFETY: `ProcessArgs` 建的 `argv` 有 `argc` 个指针，都指向活着的、以 0 结尾的字符串
            unsafe { std::slice::from_raw_parts(args.argv, args.argc as usize) }
        };
        // SAFETY: 同上，每个指针都是以 0 结尾的字符串
        let sandboxed = argv.iter().any(|&arg| unsafe { CStr::from_ptr(arg) }.to_bytes().starts_with(b"--seatbelt-client"));
        if !sandboxed {
            return Ok(None);
        }
        let path = framework.join(SANDBOX_LIBRARY);
        // SAFETY: 加载 CEF 自带的沙箱库，它的初始化代码只登记函数，不碰别的状态
        let library = unsafe { libloading::Library::new(&path) }.map_err(|e| EngineError::Load(format!("加载沙箱库 {} 失败：{e}", path.display())))?;
        // SAFETY: `cef_sandbox_initialize` 的签名是 `void* (int argc, char** argv)`（`cef_sandbox_mac.h`）；`argv` 在调用期间有效
        let context = unsafe {
            let initialize = library
                .get::<unsafe extern "C" fn(std::ffi::c_int, *mut *mut std::ffi::c_char) -> *mut std::ffi::c_void>(b"cef_sandbox_initialize\0")
                .map_err(|e| EngineError::Load(format!("沙箱库里没有 cef_sandbox_initialize：{e}")))?;
            initialize(args.argc, args.argv)
        };
        let context = std::ptr::NonNull::new(context).ok_or_else(|| EngineError::Load("子进程进不了沙箱（cef_sandbox_initialize 失败）".into()))?;
        Ok(Some(Sandbox { library, context }))
    }

    /// 加载内核框架里的库。一个进程只加载一次，不卸载
    pub(super) fn load_framework(dir: &Path) -> Result<(), EngineError> {
        let library = dir.join(LIBRARY);
        let path = CString::new(library.as_os_str().as_bytes()).map_err(|_| EngineError::Load(format!("路径里有 0 字节：{}", library.display())))?;
        // SAFETY: `path` 以 0 结尾，调用期间一直活着；`cef_load_library` 只读这个字符串
        let loaded = cef::load_library(Some(unsafe { &*path.as_ptr() }));
        if loaded == 1 { Ok(()) } else { Err(EngineError::Load(format!("加载 {} 失败", library.display()))) }
    }

    /// 打包了时 Info.plist 里的包标识符
    pub(super) fn bundle_identifier() -> Option<String> {
        let id = objc2_foundation::NSBundle::mainBundle().bundleIdentifier()?.to_string();
        (!id.is_empty()).then_some(id)
    }

    // CEF 要求应用对象（`NSApp`）实现 `CefAppProtocol`：`isHandlingSendEvent`、`setHandlingSendEvent:`，
    // 表示此刻在不在分发系统事件（`sendEvent:` 里）。框架的应用对象是窗口库建的，类不是我们的，
    // 所以在启动 CEF 之前给它的类加上这两个方法和协议，并且在 `sendEvent:` 外面包一层记下这个状态。

    /// 正在 `sendEvent:` 里
    static HANDLING: AtomicBool = AtomicBool::new(false);
    /// 包起来之前的 `sendEvent:`
    static ORIGINAL_SEND_EVENT: OnceLock<SendEvent> = OnceLock::new();

    type SendEvent = extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject);
    type IsHandling = extern "C-unwind" fn(&AnyObject, Sel) -> Bool;
    type SetHandling = extern "C-unwind" fn(&AnyObject, Sel, Bool);

    extern "C-unwind" fn is_handling_send_event(_this: &AnyObject, _cmd: Sel) -> Bool {
        Bool::new(HANDLING.load(Ordering::Relaxed))
    }

    extern "C-unwind" fn set_handling_send_event(_this: &AnyObject, _cmd: Sel, handling: Bool) {
        HANDLING.store(handling.as_bool(), Ordering::Relaxed);
    }

    extern "C-unwind" fn send_event(this: &AnyObject, cmd: Sel, event: *mut AnyObject) {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                HANDLING.store(self.0, Ordering::Relaxed);
            }
        }
        let _restore = Restore(HANDLING.swap(true, Ordering::Relaxed));
        if let Some(original) = ORIGINAL_SEND_EVENT.get() {
            original(this, cmd, event);
        }
    }

    /// 方法的类型编码：返回值、`self`、`_cmd`、参数
    fn types(encodings: &[Encoding]) -> CString {
        let text: String = encodings.iter().map(ToString::to_string).collect();
        CString::new(text).unwrap_or_default()
    }

    /// 给应用对象的类加上 CEF 要的方法和协议（启动 CEF 之前、主线程上调用一次）
    pub(super) fn install_app_protocol() {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("不在主线程上启动 CEF，没给应用对象加 CEF 要的接口");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        // 键值观察会把对象换成临时的子类（`NSKVONotifying_…`），没人观察了又换回去：方法和协议加在 `-class`
        // 给出的类上，临时的子类从它继承
        // SAFETY: `-class` 不带参数，返回对象的类（不是空）
        let class: &'static AnyClass = unsafe { msg_send![&*app, class] };
        let raw = std::ptr::from_ref(class).cast_mut();
        let object = <*mut AnyObject as Encode>::ENCODING;
        let selector = <Sel as Encode>::ENCODING;
        if !class.responds_to(sel!(isHandlingSendEvent)) {
            let types = types(&[Bool::ENCODING, object.clone(), selector.clone()]);
            // SAFETY: 函数的签名和选择器、类型编码一致（返回 BOOL，参数是 self、_cmd）；类在运行时里登记好了
            unsafe {
                let imp = std::mem::transmute::<IsHandling, Imp>(is_handling_send_event);
                ffi::class_addMethod(raw, sel!(isHandlingSendEvent), imp, types.as_ptr());
            }
        }
        if !class.responds_to(sel!(setHandlingSendEvent:)) {
            let types = types(&[Encoding::Void, object.clone(), selector.clone(), Bool::ENCODING]);
            // SAFETY: 同上（不返回值，参数是 self、_cmd、BOOL）
            unsafe {
                let imp = std::mem::transmute::<SetHandling, Imp>(set_handling_send_event);
                ffi::class_addMethod(raw, sel!(setHandlingSendEvent:), imp, types.as_ptr());
            }
        }
        if let Some(method) = class.instance_method(sel!(sendEvent:)) {
            // SAFETY: `sendEvent:` 的签名就是 `SendEvent`（self、_cmd、NSEvent*，不返回值）；换实现之前先记下原来的，
            // 只在主线程上换一次（`ensure_started` 只走一遍）
            unsafe {
                let ours = std::mem::transmute::<SendEvent, Imp>(send_event);
                if !std::ptr::fn_addr_eq(method.implementation(), ours) {
                    let original = std::mem::transmute::<Imp, SendEvent>(method.implementation());
                    if ORIGINAL_SEND_EVENT.set(original).is_ok() {
                        method.set_implementation(ours);
                    }
                }
            }
        }
        let mut adopted = Vec::new();
        for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
            let Some(protocol) = AnyProtocol::get(name) else { continue };
            if !class.conforms_to(protocol) {
                // SAFETY: 协议由内核的框架登记；它要求的方法上面已经加好
                unsafe { ffi::class_addProtocol(raw, protocol) };
            }
            adopted.push(name.to_str().unwrap_or_default());
        }
        tracing::debug!("应用对象的类 {} 实现了 {adopted:?}", class_name(class));
    }

    fn class_name(class: &AnyClass) -> String {
        let name: &CStr = class.name();
        name.to_string_lossy().into_owned()
    }
}
