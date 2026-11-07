//! Linux 的系统集成，都走 D-Bus（纯 Rust 的 zbus，不链接系统库）：
//!
//! - 文件对话框：XDG 门户的 `org.freedesktop.portal.FileChooser`（GNOME、KDE 都有，沙盒里也能用），
//!   在自己的线程里等 `Response` 信号；门户不可用时退回到 zenity / kdialog 命令。
//! - 外观、减少动态效果、强调色：门户的 `org.freedesktop.portal.Settings`。第一次用到时在"系统设置"线程上
//!   连会话总线、读一次，之后这个线程听 `SettingChanged` 信号（winit 在 Linux 上不报告外观变化）。
//!   主线程不调 D-Bus：只读缓存，第一次最多等一小会儿（[`FIRST_READ_WAIT`]），没等到先按不知道算，
//!   读到以后通知 [`watch_settings`] 的回调。
//! - 通知：`org.freedesktop.Notifications`，在"通知"线程上发。不需要打包，有通知服务就能发。
//! - 在文件管理器里选中：`org.freedesktop.FileManager1.ShowItems`，不支持时打开所在的文件夹；打开文件和网址用 `xdg-open`。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, OnceLock};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

use super::portable::{self, DesktopSettings, SettingValue};
use super::{DialogDone, DialogKind, DialogOptions, SettingsChanged};
use crate::{NotificationRequest, OnNotificationResponse, PlatformError};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
/// 要读的设置所在的命名空间
const NAMESPACES: [&str; 4] = ["org.freedesktop.appearance", "org.gnome.desktop.interface", "org.gnome.desktop.wm.preferences", "org.kde.kdeglobals.KDE"];
/// 第一次读外观设置时调用方最多等多久（门户一般几毫秒就回）
const FIRST_READ_WAIT: Duration = Duration::from_millis(250);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

static BUS: OnceLock<Option<Connection>> = OnceLock::new();

/// 会话总线，整个进程共用一个连接；连不上（没有桌面会话）时是 `None`，以后也不再试。
/// 方法调用最多等 2 秒：服务卡住时不至于一直卡着调用方。连接要和总线握手，会阻塞：只在后台线程上调用
fn bus() -> Option<&'static Connection> {
    BUS.get_or_init(|| zbus::blocking::connection::Builder::session().and_then(|b| b.method_timeout(Duration::from_secs(2)).build()).ok())
        .as_ref()
}

/// 不阻塞地看一眼会话总线：连好了是 `Some(Some)`，确定没有是 `Some(None)`，还在连（或者还没开始连）是 `None`
fn bus_if_ready() -> Option<Option<&'static Connection>> {
    BUS.get().map(Option::as_ref)
}

// ---------- 外观设置 ----------

struct Desktop {
    settings: DesktopSettings,
    /// 门户的设置接口能用
    portal: bool,
    /// 第一次读完了（读到或者确定读不到）
    loaded: bool,
    /// 读完之前有人拿了默认值：读完后要通知监视的回调
    served_early: bool,
}

static DESKTOP: Mutex<Desktop> =
    Mutex::new(Desktop { settings: DesktopSettings::EMPTY, portal: false, loaded: false, served_early: false });
static LOADED: Condvar = Condvar::new();
/// 监视设置的回调
static WATCHERS: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>> = Mutex::new(Vec::new());
/// 读完时有人拿过默认值、可那时还没有回调：第一个登记的回调马上补一次
static PENDING_CHANGE: AtomicBool = AtomicBool::new(false);

/// 通知监视设置的回调（在"系统设置"线程上）
fn notify_watchers() {
    let watchers = lock(&WATCHERS).clone();
    if watchers.is_empty() {
        PENDING_CHANGE.store(true, Ordering::SeqCst);
    }
    for watcher in watchers {
        watcher();
    }
}

/// 开"系统设置"线程（只开一个）：连会话总线、读一次门户的设置，之后听 `SettingChanged`，更新缓存、通知回调
fn start_settings_thread() {
    static START: Once = Once::new();
    START.call_once(|| {
        let spawned = std::thread::Builder::new().name("系统设置".into()).spawn(settings_thread);
        if spawned.is_err() {
            // 线程开不出来：按读不到算，免得调用方每次都等
            lock(&DESKTOP).loaded = true;
            LOADED.notify_all();
        }
    });
}

fn settings_thread() {
    let bus = bus();
    let all = bus.and_then(|bus| {
        let reply = bus.call_method(Some(PORTAL), PORTAL_PATH, Some(SETTINGS), "ReadAll", &(NAMESPACES.to_vec(),)).ok()?;
        reply.body().deserialize::<HashMap<String, HashMap<String, OwnedValue>>>().ok()
    });
    let early = {
        let mut desktop = lock(&DESKTOP);
        desktop.portal = all.is_some();
        for (namespace, values) in all.into_iter().flatten() {
            for (key, value) in values {
                desktop.settings.apply(&namespace, &key, &setting_value(&value));
            }
        }
        desktop.loaded = true;
        desktop.served_early
    };
    LOADED.notify_all();
    if early {
        notify_watchers();
    }
    let Some(bus) = bus else { return };
    let rule = || -> zbus::Result<MatchRule<'static>> {
        Ok(MatchRule::builder().msg_type(MessageType::Signal).interface(SETTINGS)?.member("SettingChanged")?.path(PORTAL_PATH)?.build())
    };
    let Ok(signals) = rule().and_then(|rule| MessageIterator::for_match_rule(rule, bus, Some(16))) else { return };
    for message in signals.flatten() {
        let Ok((namespace, key, value)) = message.body().deserialize::<(String, String, OwnedValue)>() else { continue };
        let changed = lock(&DESKTOP).settings.apply(&namespace, &key, &setting_value(&value));
        if changed {
            notify_watchers();
        }
    }
}

/// 外观设置的缓存。还没读完时第一次最多等 [`FIRST_READ_WAIT`]；没等到就记下"有人拿了默认值"，
/// 之后不再等（读完后通知回调）
fn desktop() -> MutexGuard<'static, Desktop> {
    start_settings_thread();
    let guard = lock(&DESKTOP);
    if guard.loaded || guard.served_early {
        return guard;
    }
    let (mut guard, _) = LOADED.wait_timeout_while(guard, FIRST_READ_WAIT, |d| !d.loaded).unwrap_or_else(|e| e.into_inner());
    if !guard.loaded {
        guard.served_early = true;
    }
    guard
}

/// D-Bus 的值 → 和平台无关的形式；老版本的门户把值多包了一层变体
fn setting_value(value: &Value) -> SettingValue {
    match value {
        Value::U32(v) => SettingValue::U32(*v),
        Value::I32(v) => SettingValue::I32(*v),
        Value::Bool(v) => SettingValue::Bool(*v),
        Value::F64(v) => SettingValue::F64(*v),
        Value::Str(s) => SettingValue::Str(s.to_string()),
        Value::Value(inner) => setting_value(inner),
        Value::Structure(s) => match s.fields() {
            [Value::F64(r), Value::F64(g), Value::F64(b)] => SettingValue::Rgb([*r, *g, *b]),
            _ => SettingValue::Other,
        },
        _ => SettingValue::Other,
    }
}

/// 深色外观：门户的 `color-scheme`；没有偏好时看 `GTK_THEME`；门户能用但没说深色时是浅色
pub fn is_dark_mode() -> Option<bool> {
    let desktop = desktop();
    desktop
        .settings
        .dark()
        .or_else(|| std::env::var("GTK_THEME").ok().filter(|t| portable::gtk_theme_is_dark(t)).map(|_| true))
        .or(desktop.portal.then_some(false))
}

/// 减少动态效果：门户的 `reduced-motion`，或 GNOME 关掉了动画、KDE 把动画速度调到 0
pub fn is_motion_reduced() -> bool {
    desktop().settings.is_motion_reduced()
}

/// 双击标题栏该做什么：GNOME 的设置（经门户读到的）；别的桌面读不到，是缩放
pub fn title_bar_action() -> crate::TitleBarAction {
    desktop().settings.title_bar_action.unwrap_or_default()
}

/// 门户的 `accent-color`（GNOME 47+、KDE）
pub fn accent_color() -> Option<[u8; 3]> {
    desktop().settings.accent
}

/// 托盘所在的面板是不是深色：GNOME 的顶栏总是深色，其他桌面跟着系统外观
pub fn is_panel_dark() -> Option<bool> {
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    Some(portable::linux_panel_dark(&current, is_dark_mode()))
}

/// 深浅色、减少动态效果、强调色变了就在"系统设置"线程上调用 `on_change`。
/// 第一次读得比调用方晚（调用方先拿了默认值）时，读到后也调用一次
pub fn watch_settings(on_change: SettingsChanged) {
    start_settings_thread();
    let on_change: Arc<dyn Fn() + Send + Sync> = Arc::from(on_change);
    lock(&WATCHERS).push(on_change.clone());
    if PENDING_CHANGE.swap(false, Ordering::SeqCst) {
        on_change();
    }
}

// ---------- 通知 ----------

static APP_NAME: Mutex<Option<String>> = Mutex::new(None);

/// 通知里显示的应用名（主窗口标题）。没设时用可执行文件名
pub fn set_app_name(name: &str) {
    *lock(&APP_NAME) = Some(name.to_owned());
}

/// 通知、托盘用的应用名：设过的（主窗口标题），没设时是可执行文件名
pub(crate) fn app_name() -> String {
    lock(&APP_NAME).clone().unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_default()
    })
}

/// 能发通知：会话总线还没确定连不上就算能（真发不出去时在"通知"线程上记下内容）
pub fn notification_available() -> bool {
    !matches!(bus_if_ready(), Some(None))
}

/// 排进"通知"线程的一条通知
struct QueuedNotification {
    request: NotificationRequest,
    on_response: Option<OnNotificationResponse>,
}

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// 等用户点的通知：服务给的编号 → 回调。关掉（`NotificationClosed`）时删掉
static WAITING: Mutex<Option<HashMap<u32, OnNotificationResponse>>> = Mutex::new(None);

/// "通知"线程的队列：第一次发通知时开线程，线程上连总线、调用 `Notify`
fn notification_queue() -> Option<&'static Sender<QueuedNotification>> {
    static QUEUE: OnceLock<Option<Sender<QueuedNotification>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (sender, receiver) = channel::<QueuedNotification>();
            let spawned = std::thread::Builder::new().name("通知".into()).spawn(move || {
                for queued in receiver {
                    let (title, body) = (queued.request.title.clone(), queued.request.body.clone());
                    if let Err(error) = send_notification(queued) {
                        tracing::info!(%title, %body, "系统通知没发出去（{error}），通知的内容记在这里");
                    }
                }
            });
            spawned.ok().map(|_| sender)
        })
        .as_ref()
}

fn send_notification(queued: QueuedNotification) -> Result<(), String> {
    let QueuedNotification { request, on_response } = queued;
    let bus = bus().ok_or("没有会话总线")?;
    let mut hints: HashMap<&str, Value> = HashMap::new();
    // 沙盒里的应用按 .desktop 文件的名字认
    if let Ok(id) = std::env::var("FLATPAK_ID") {
        hints.insert("desktop-entry", Value::from(id));
    }
    // 只发内容的通知不要点击：有动作的话有的桌面点了会有反应，但没人接
    let actions = match on_response {
        Some(_) => portable::dbus_notification_actions(&request.actions, if super::chinese() { "查看" } else { "Open" }),
        None => Vec::new(),
    };
    if on_response.is_some() {
        start_notification_listener();
    }
    let reply = bus
        .call_method(
            Some(NOTIFICATIONS),
            NOTIFICATIONS_PATH,
            Some(NOTIFICATIONS),
            "Notify",
            // 应用名、替换的编号、图标、标题、正文、动作、提示、超时（-1 由服务决定）
            &(app_name().as_str(), 0u32, "", request.title.as_str(), request.body.as_str(), actions, hints, -1i32),
        )
        .map_err(|error| error.to_string())?;
    if let Some(on_response) = on_response {
        let id: u32 = reply.body().deserialize().map_err(|error| error.to_string())?;
        lock(&WAITING).get_or_insert_default().insert(id, on_response);
    }
    Ok(())
}

/// 开"通知回调"线程（整个进程一个）：听 `ActionInvoked`、`NotificationClosed`。先开它再发通知，免得信号比订阅先到
fn start_notification_listener() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new().name("通知回调".into()).spawn(|| {
            let Some(bus) = bus() else { return };
            let rule = || -> zbus::Result<MatchRule<'static>> {
                Ok(MatchRule::builder().msg_type(MessageType::Signal).interface(NOTIFICATIONS)?.path(NOTIFICATIONS_PATH)?.build())
            };
            let signals = match rule().and_then(|rule| MessageIterator::for_match_rule(rule, bus, Some(16))) {
                Ok(signals) => signals,
                Err(error) => {
                    tracing::warn!("听不了通知的动作：{error}");
                    return;
                }
            };
            for message in signals.flatten() {
                let header = message.header();
                let member = header.member().map(|member| member.as_str().to_owned());
                match member.as_deref() {
                    Some("ActionInvoked") => {
                        let Ok((id, key)) = message.body().deserialize::<(u32, String)>() else { continue };
                        let on_response = lock(&WAITING).get_or_insert_default().get(&id).cloned();
                        if let (Some(on_response), Some(response)) = (on_response, portable::notification_response(&key)) {
                            on_response(response);
                        }
                    }
                    Some("NotificationClosed") => {
                        let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() else { continue };
                        lock(&WAITING).get_or_insert_default().remove(&id);
                    }
                    _ => {}
                }
            }
        });
        if let Err(error) = spawned {
            tracing::warn!("通知回调的线程开不出来：{error}");
        }
    });
}

fn enqueue(queued: QueuedNotification) -> Result<(), PlatformError> {
    if !notification_available() {
        return Err(PlatformError::Unsupported("没有会话总线".into()));
    }
    let queue = notification_queue().ok_or_else(|| PlatformError::System("通知的线程开不出来".into()))?;
    queue.send(queued).map_err(|_| PlatformError::System("通知的线程已经结束".into()))
}

/// Linux 的桌面没有统一的系统提示音接口（X11 的响铃、各桌面的声音服务各管各的）
pub fn beep() -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported("Linux 的桌面没有统一的系统提示音".into()))
}

/// 发通知（`org.freedesktop.Notifications.Notify`），在"通知"线程上发，调用方不等。
/// 已经确定没有会话总线时返回"不支持"；排进队列以后的失败在那个线程上记下通知的内容
pub fn notify(title: &str, body: &str) -> Result<(), PlatformError> {
    enqueue(QueuedNotification { request: NotificationRequest { title: title.to_owned(), body: body.to_owned(), actions: Vec::new() }, on_response: None })
}

/// 发带按钮的通知，用户点了在"通知回调"线程上调用 `on_response`（通知服务发 `ActionInvoked` 信号）。前提和 [`notify`] 一样
pub fn show_notification(request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError> {
    enqueue(QueuedNotification { request, on_response: Some(on_response) })
}

// ---------- 打开、在文件管理器里显示 ----------

/// 用 `xdg-open` 打开；在后台线程里等它结束（不留僵尸进程）
fn xdg_open(target: &OsStr) -> bool {
    let spawned = Command::new("xdg-open").arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match spawned {
        Ok(mut child) => {
            let _ = std::thread::Builder::new().name("xdg-open".into()).spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// 用系统默认的应用打开网址
pub fn open_url(url: &str) -> bool {
    xdg_open(OsStr::new(url))
}

/// 用默认应用打开文件或文件夹；不存在时返回 `false`
pub fn open_path(path: &Path) -> bool {
    match std::path::absolute(path) {
        Ok(path) if path.exists() => xdg_open(path.as_os_str()),
        _ => false,
    }
}

/// 在文件管理器里显示并选中；不存在时返回 `false`。在后台线程里调用（可能要等文件管理器启动），
/// 文件管理器不支持 `FileManager1` 时打开所在的文件夹
pub fn reveal_path(path: &Path) -> bool {
    let Some(path) = std::path::absolute(path).ok().filter(|p| p.exists()) else { return false };
    let spawned = std::thread::Builder::new().name("在文件管理器中显示".into()).spawn(move || {
        // 单独的连接，多等一会儿：文件管理器可能要先启动
        let shown = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.method_timeout(Duration::from_secs(15)).build())
            .and_then(|bus| {
                bus.call_method(
                    Some("org.freedesktop.FileManager1"),
                    "/org/freedesktop/FileManager1",
                    Some("org.freedesktop.FileManager1"),
                    "ShowItems",
                    &(vec![portable::file_uri(&path)], ""),
                )
            })
            .is_ok();
        if !shown && let Some(parent) = path.parent() {
            xdg_open(parent.as_os_str());
        }
    });
    spawned.is_ok()
}

// ---------- 文件对话框 ----------

type Cancel = Box<dyn Fn() + Send>;

/// 开着的文件对话框，[`cancel_file_dialogs`] 用
static OPEN: Mutex<Vec<(u64, Cancel)>> = Mutex::new(Vec::new());

/// 完成回调只调用一次：对话框的线程和取消都可能先到
type Slot = Arc<Mutex<Option<DialogDone>>>;

fn finish(slot: &Slot, paths: Vec<PathBuf>) {
    if let Some(done) = lock(slot).take() {
        done(paths);
    }
}

/// 弹出文件对话框，马上返回；在自己的线程里等用户选好或取消，然后在那个线程调用 `done`
pub fn file_dialog(options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let parent = parent.and_then(x11_window);
    let chinese = super::chinese();
    let slot: Slot = Arc::new(Mutex::new(Some(done)));
    let thread_slot = slot.clone();
    let spawned = std::thread::Builder::new().name("文件对话框".into()).spawn(move || {
        let paths = match portal_dialog(&options, parent, chinese, id, &thread_slot) {
            Ok(paths) => paths,
            // 门户不可用（没有会话总线、没装门户）：退回到命令行的对话框
            Err(error) => command_dialog(&options, parent, chinese, id, &thread_slot).unwrap_or_else(|| {
                tracing::error!("没有可用的文件对话框（XDG 门户：{error}；也没有 zenity、kdialog）");
                Vec::new()
            }),
        };
        lock(&OPEN).retain(|(open, _)| *open != id);
        finish(&thread_slot, paths);
    });
    if let Err(error) = spawned {
        tracing::error!("文件对话框的线程开不出来：{error}");
        finish(&slot, Vec::new());
    }
}

/// X11 的窗口号（门户和 kdialog 用它把对话框放在窗口上面）；Wayland 要另外导出窗口，这里不做
fn x11_window(window: &dyn HasWindowHandle) -> Option<u64> {
    match window.window_handle().ok()?.as_raw() {
        // c_ulong 在 32 位系统上是 u32
        #[allow(clippy::unnecessary_cast)]
        RawWindowHandle::Xlib(handle) => Some(handle.window as u64),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get() as u64),
        _ => None,
    }
}

fn response_rule(path: &str) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder().msg_type(MessageType::Signal).interface(REQUEST)?.member("Response")?.path(path.to_owned())?.build())
}

/// 用门户弹对话框。门户不可用时返回错误（调用方退回到命令行的对话框）；用户取消是空列表
fn portal_dialog(options: &DialogOptions, parent: Option<u64>, chinese: bool, id: u64, slot: &Slot) -> zbus::Result<Vec<PathBuf>> {
    // 每个对话框一个连接：取消时关掉它，等待的线程就结束了。门户先回请求的路径、再弹对话框，调用本身很快
    let bus = zbus::blocking::connection::Builder::session()?.method_timeout(Duration::from_secs(10)).build()?;
    let token = format!("aureole{}_{id}", std::process::id());
    let unique = bus.unique_name().map(|n| n.to_string()).unwrap_or_default();
    let expected = portable::portal_request_path(&unique, &token);
    // 先订阅再调用，免得回复比订阅先到
    let mut responses = MessageIterator::for_match_rule(response_rule(&expected)?, &bus, Some(4))?;

    let all_name = portable::all_files_name(chinese);
    let title = options.title.clone().unwrap_or_else(|| portable::default_title(options.kind, options.multiple, chinese).to_owned());
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("handle_token", Value::from(token.as_str()));
    opts.insert("modal", Value::from(true));
    let method = match options.kind {
        DialogKind::OpenFile => {
            opts.insert("multiple", Value::from(options.multiple));
            "OpenFile"
        }
        DialogKind::OpenDirectory => {
            opts.insert("directory", Value::from(true));
            "OpenFile"
        }
        DialogKind::Save => {
            if let Some(name) = &options.file_name {
                opts.insert("current_name", Value::from(name.as_str()));
            }
            "SaveFile"
        }
    };
    if let Some(dir) = options.directory.as_deref().and_then(|d| std::path::absolute(d).ok()) {
        // 字节串，以 0 结尾
        let mut bytes = dir.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        opts.insert("current_folder", Value::from(bytes));
    }
    if options.kind != DialogKind::OpenDirectory {
        let filters = portable::portal_filters(&options.filters, all_name);
        if let Some(first) = filters.first().cloned() {
            opts.insert("current_filter", Value::from(first));
        }
        if !filters.is_empty() {
            opts.insert("filters", Value::from(filters));
        }
    }
    let parent = parent.map(portable::portal_parent_x11).unwrap_or_default();
    let reply = bus.call_method(Some(PORTAL), PORTAL_PATH, Some("org.freedesktop.portal.FileChooser"), method, &(parent.as_str(), title.as_str(), opts))?;
    let handle: OwnedObjectPath = reply.body().deserialize()?;
    if handle.as_str() != expected {
        // 很老的门户不认 handle_token：改听它给的路径
        responses = MessageIterator::for_match_rule(response_rule(handle.as_str())?, &bus, Some(4))?;
    }

    let (cancel_bus, cancel_slot) = (bus.clone(), slot.clone());
    lock(&OPEN).push((
        id,
        Box::new(move || {
            let _ = cancel_bus.call_method(Some(PORTAL), handle.as_str(), Some(REQUEST), "Close", &());
            finish(&cancel_slot, Vec::new());
            let _ = cancel_bus.clone().close();
        }),
    ));

    for message in responses {
        // 取消时连接被关掉，这里结束
        let Ok(message) = message else { break };
        let Ok((code, mut results)) = message.body().deserialize::<(u32, HashMap<String, OwnedValue>)>() else { continue };
        // 0 是选好了，1 是取消，2 是别的原因结束
        if code != 0 {
            return Ok(Vec::new());
        }
        let uris = results.remove("uris").and_then(|v| Vec::<String>::try_from(v).ok()).unwrap_or_default();
        return Ok(uris.iter().filter_map(|uri| portable::uri_to_path(uri)).collect());
    }
    Ok(Vec::new())
}

fn in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// 用 zenity 或 kdialog 弹对话框（KDE 上先试 kdialog）。两个都没有时是 `None`
fn command_dialog(options: &DialogOptions, parent: Option<u64>, chinese: bool, id: u64, slot: &Slot) -> Option<Vec<PathBuf>> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("kde")));
    let order = if kde { ["kdialog", "zenity"] } else { ["zenity", "kdialog"] };
    let program = order.into_iter().find(|p| in_path(p))?;
    let title = options.title.clone().unwrap_or_else(|| portable::default_title(options.kind, options.multiple, chinese).to_owned());
    let all_name = portable::all_files_name(chinese);
    let args = match program {
        "kdialog" => portable::kdialog_args(options, &title, parent, all_name),
        _ => portable::zenity_args(options, &title, all_name),
    };
    let mut child = Command::new(program).args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let child = Arc::new(Mutex::new(child));
    let (cancel_child, cancel_slot) = (child.clone(), slot.clone());
    lock(&OPEN).push((
        id,
        Box::new(move || {
            let _ = lock(&cancel_child).kill();
            finish(&cancel_slot, Vec::new());
        }),
    ));
    let mut output = String::new();
    // 进程结束（选好、取消或被关掉）时输出管道关闭，这里返回
    let _ = stdout.read_to_string(&mut output);
    let status = lock(&child).wait().ok()?;
    // 退出码不是 0 表示取消
    Some(if status.success() { portable::parse_dialog_output(&output) } else { Vec::new() })
}

/// 像用户点"取消"一样关掉所有开着的文件对话框，返回关掉了几个。测试用
#[doc(hidden)]
pub fn cancel_file_dialogs() -> usize {
    let open = std::mem::take(&mut *lock(&OPEN));
    for (_, cancel) in &open {
        cancel();
    }
    open.len()
}
