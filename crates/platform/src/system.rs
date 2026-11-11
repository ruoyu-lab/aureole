//! 系统集成：文件对话框、用默认应用打开网址和文件、系统外观和强调色、减少动态效果、系统通知。
//!
//! - macOS：AppKit（NSOpenPanel、NSSavePanel、NSWorkspace）和 UserNotifications。
//! - Windows：COM 的 IFileOpenDialog / IFileSaveDialog（在自己的线程里）、WinRT 的通知（toast）、
//!   ShellExecuteW / SHOpenFolderAndSelectItems、注册表里的外观和强调色。
//! - Linux：D-Bus 上的 XDG 门户（文件对话框、外观设置）、`org.freedesktop.Notifications`、
//!   `org.freedesktop.FileManager1`；门户不可用时文件对话框退回到 zenity / kdialog，打开文件用 `xdg-open`。
//!
//! 不调用系统的纯逻辑（过滤器、网址编码、通知的 XML、读回来的值怎么解释）在 `portable` 里，各平台都能测试。

use std::path::{Path, PathBuf};

use raw_window_handle::HasWindowHandle;

use crate::{PlatformBackend, native};

#[doc(hidden)]
pub mod portable;
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

/// 这个平台的实现，由 [`crate::NativeBackend`] 调用
#[cfg(target_os = "macos")]
pub(crate) use macos as imp;
#[cfg(target_os = "linux")]
pub(crate) use linux as imp;
#[cfg(target_os = "windows")]
pub(crate) use windows as imp;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) use fallback as imp;

/// 弹出文件对话框，马上返回；用户选好或取消后调用 `done`（取消时是空列表）。
/// macOS：有 `parent` 时作为它的表单弹出，否则是独立的面板；只能在主线程打开（不在主线程时直接以空列表调用 `done`），
/// 在主线程完成。Windows、Linux：对话框在自己的线程里，`done` 也在那个线程里调用
pub fn file_dialog(options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
    native().dialog().file_dialog(options, parent, done);
}

/// 像用户点"取消"一样关掉所有开着的文件对话框，返回关掉了几个。测试用
#[doc(hidden)]
pub fn cancel_file_dialogs() -> usize {
    native().dialog().cancel_file_dialogs()
}

/// 用系统默认的应用打开网址，返回是否交给了系统。Windows：要有协议名（`https:`、`mailto:`、自定义协议……），
/// 本机路径、共享路径、`file:` 都不收（外壳会直接运行它们；打开文件用 [`open_path`]），在后台线程上交给外壳
pub fn open_url(url: &str) -> bool {
    native().shell().open_url(url).is_ok()
}

/// 用默认应用打开文件或文件夹；不存在时返回 `false`，不调用系统
pub fn open_path(path: &Path) -> bool {
    native().shell().open_path(path).is_ok()
}

/// 在文件管理器里显示并选中；不存在时返回 `false`，不调用系统
pub fn reveal_path(path: &Path) -> bool {
    native().shell().reveal_path(path).is_ok()
}

/// 系统现在是不是深色外观；不知道时是 `None`。macOS 读用户默认设置（窗口创建后以窗口报告的为准），
/// Windows 读注册表，Linux 读 XDG 门户（在后台线程上读，第一次最多等一小会儿，没读到时先按不知道算，读到后通知 [`watch_settings`] 的回调）
pub fn is_dark_mode() -> Option<bool> {
    native().appearance().is_dark_mode()
}

/// 托盘所在的任务栏、面板是不是深色：托盘图标按它选颜色。macOS 的模板图标由系统着色，是 `None`
pub fn is_panel_dark() -> Option<bool> {
    native().appearance().is_panel_dark()
}

/// 系统打开了"减少动态效果"
pub fn is_motion_reduced() -> bool {
    native().appearance().is_motion_reduced()
}

/// 系统的强调色，换算到 sRGB
pub fn accent_color() -> Option<[u8; 3]> {
    native().appearance().accent_color()
}

/// 发系统通知，返回是否交给了系统。macOS 要打包成 .app，Windows 要有应用标识，Linux 要有会话总线；
/// Windows、Linux 在后台线程上发送，发送失败时把通知的内容记在日志里
pub fn notify(title: &str, body: &str) -> bool {
    native().notification().notify(title, body).is_ok()
}

/// 通知里显示的应用名（Linux 用；macOS 读 Info.plist，Windows 按应用标识）
pub fn set_app_name(name: &str) {
    native().notification().set_app_name(name);
}

/// 声明进程的应用标识（Windows 的 AppUserModelID），用打包时的包标识符（安装程序建的开始菜单快捷方式上写的就是它）。
/// 要在建第一个窗口之前调用：任务栏按它分组，系统通知按它显示，不管应用是怎么启动的（快捷方式、双击文档、协议链接、命令行）。
/// 通知还要这个标识在系统里登记过（开始菜单里有带它的快捷方式，或者注册表里有 `Software\Classes\AppUserModelId\<标识>`），
/// 没登记过时（`cargo run`）照旧不发、把内容记在日志里。标识不能有空格、最长 128 个字符；写得不对、系统不收时返回 `false`。
/// 别的平台按包的信息认应用，什么都不做，返回 `false`
pub fn set_app_user_model_id(id: &str) -> bool {
    #[cfg(target_os = "windows")]
    return windows::set_app_user_model_id(id);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        false
    }
}

/// 打包时写进可执行文件的包标识符（Windows：`cargo bundle` 把 `identifier` 写成一个资源，见 `aureole-bundle`）。
/// 运行时在建第一个窗口之前用它声明应用标识（[`set_app_user_model_id`]）。没打包（`cargo run`）、别的平台是 `None`
pub fn embedded_app_id() -> Option<String> {
    #[cfg(target_os = "windows")]
    return windows::embedded_app_id();
    #[cfg(not(target_os = "windows"))]
    None
}

/// 在后台监视系统设置，深浅色、减少动态效果、强调色变了时调用 `on_change`（在后台线程里）。
/// macOS 上外观变化由窗口报告，这里什么都不做
pub fn watch_settings(on_change: SettingsChanged) {
    native().appearance().watch_settings(on_change);
}

/// 本地的日期和时间：年、月、日、时、分、秒。系统问不到时是 `None`
pub fn local_time() -> Option<[i32; 6]> {
    #[cfg(unix)]
    {
        // SAFETY: 传空指针时只返回结果，不写内存
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        // SAFETY: tm 是纯数据（整数和指针），全零是有效的值，下面由 localtime_r 填好
        let mut parts: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: 两个指针都指向这里的局部变量；带 _r 的版本写进自己的缓冲区，别的线程同时调用也没事
        let filled = unsafe { libc::localtime_r(&now, &mut parts) };
        if filled.is_null() {
            return None;
        }
        Some([parts.tm_year + 1900, parts.tm_mon + 1, parts.tm_mday, parts.tm_hour, parts.tm_min, parts.tm_sec])
    }
    #[cfg(windows)]
    {
        // SAFETY: SYSTEMTIME 全是整数，全零是有效的值，下面由 GetLocalTime 填好
        let mut parts: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        // SAFETY: 指针指向这里的局部变量
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut parts) };
        Some([parts.wYear as i32, parts.wMonth as i32, parts.wDay as i32, parts.wHour as i32, parts.wMinute as i32, parts.wSecond as i32])
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// 文件对话框的种类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogKind {
    /// 选一个（或多个）已有的文件
    OpenFile,
    /// 选一个文件夹
    OpenDirectory,
    /// 选保存的位置和文件名
    Save,
}

/// 一组文件类型：给人看的名字和扩展名（不带点）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFilter {
    /// 给人看的名字，例如 `"图片"`
    pub name: String,
    /// 扩展名，不带点，例如 `["png", "jpg"]`
    pub extensions: Vec<String>,
}

/// 文件对话框的设置
#[derive(Clone, Debug)]
pub struct DialogOptions {
    /// 对话框的种类
    pub kind: DialogKind,
    /// 打开时能否多选
    pub multiple: bool,
    /// 标题；不给时 Windows、Linux 按种类和界面语言用默认的（例如"打开文件"、"保存"），macOS 用面板自带的
    pub title: Option<String>,
    /// 一开始显示的文件夹
    pub directory: Option<PathBuf>,
    /// 保存时默认的文件名
    pub file_name: Option<String>,
    /// 允许的文件类型，空表示不限。macOS 的面板不分组，所有组的扩展名合在一起；
    /// Windows、Linux 的对话框按组显示，多于一组时最前面加一组"所有支持的文件"
    pub filters: Vec<FileFilter>,
}

impl DialogOptions {
    /// 这一种对话框的默认设置：单选、默认标题、不指定文件夹和文件名、不限文件类型
    pub fn new(kind: DialogKind) -> Self {
        Self { kind, multiple: false, title: None, directory: None, file_name: None, filters: Vec::new() }
    }
}

/// 对话框完成时的回调：用户选好的路径，取消时是空列表。Windows、Linux 上在对话框自己的线程里调用
pub type DialogDone = Box<dyn FnOnce(Vec<PathBuf>) + Send>;

/// 系统设置变了时调用（在后台线程里）
pub type SettingsChanged = Box<dyn Fn() + Send + Sync>;

/// 用户界面是不是中文：系统自带文字（对话框默认标题、"所有支持的文件"）按它写
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn chinese() -> bool {
    crate::preferred_language().is_some_and(|l| l.starts_with("zh"))
}

#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, Once};

    use block2::{DynBlock, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, ProtocolObject};
    use objc2::{AllocAnyThread, MainThreadMarker, define_class, msg_send};
    use objc2_app_kit::{
        NSColor, NSColorSpace, NSModalResponse, NSModalResponseOK, NSOpenPanel, NSSavePanel, NSView, NSWindow, NSWorkspace,
    };
    use objc2_foundation::{NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString, NSURL, NSUserDefaults, ns_string};
    use objc2_uniform_type_identifiers::UTType;
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction, UNNotificationActionOptions, UNNotificationCategory,
        UNNotificationCategoryOptions, UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::{DialogDone, DialogKind, DialogOptions, SettingsChanged, portable};
    use crate::{NotificationRequest, NotificationResponse, OnNotificationResponse, PlatformError};

    thread_local! {
        /// 开着的文件对话框，[`cancel_file_dialogs`] 关掉
        static OPEN: RefCell<Vec<Retained<NSSavePanel>>> = const { RefCell::new(Vec::new()) };
    }

    /// 弹出文件对话框，马上返回；用户选好或取消后在主线程调用 `done`（取消时是空列表）。
    /// 有 `parent` 时作为它的表单（sheet）弹出，否则是独立的面板。不在主线程时直接以空列表调用 `done`
    pub fn file_dialog(options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("文件对话框只能在主线程打开");
            return done(Vec::new());
        };
        let open = (options.kind != DialogKind::Save).then(|| {
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(options.kind == DialogKind::OpenFile);
            panel.setCanChooseDirectories(options.kind == DialogKind::OpenDirectory);
            panel.setAllowsMultipleSelection(options.multiple);
            panel
        });
        let panel: Retained<NSSavePanel> = match &open {
            Some(open) => Retained::into_super(open.clone()),
            None => NSSavePanel::savePanel(mtm),
        };
        if let Some(title) = &options.title {
            let title = NSString::from_str(title);
            panel.setTitle(Some(&title));
            // 新版 macOS 的面板不显示标题栏，提示文字显示在面板顶部
            panel.setMessage(Some(&title));
        }
        if let Some(directory) = options.directory.as_deref().and_then(|dir| NSURL::from_directory_path(std::path::absolute(dir).ok()?)) {
            panel.setDirectoryURL(Some(&directory));
        }
        if let Some(name) = &options.file_name {
            panel.setNameFieldStringValue(&NSString::from_str(name));
        }
        if options.kind != DialogKind::OpenDirectory {
            let types: Vec<Retained<UTType>> = options
                .filters
                .iter()
                .flat_map(|f| &f.extensions)
                .filter_map(|ext| UTType::typeWithFilenameExtension(&NSString::from_str(ext.trim_start_matches('.'))))
                .collect();
            if !types.is_empty() {
                panel.setAllowedContentTypes(&NSArray::from_retained_slice(&types));
            }
        }
        panel.setCanCreateDirectories(true);

        let done = RefCell::new(Some(done));
        let this = panel.clone();
        // 面板持有这个块直到完成，块里持有面板：完成后一起释放
        let handler = RcBlock::new(move |response: NSModalResponse| {
            let Some(done) = done.borrow_mut().take() else { return };
            OPEN.with(|o| o.borrow_mut().retain(|p| !std::ptr::eq(&**p, &*this)));
            let paths = if response != NSModalResponseOK {
                Vec::new()
            } else if let Some(open) = &open {
                open.URLs().to_vec().iter().filter_map(|url| url.to_file_path()).collect()
            } else {
                this.URL().and_then(|url| url.to_file_path()).into_iter().collect()
            };
            done(paths);
        });
        OPEN.with(|o| o.borrow_mut().push(panel.clone()));
        match parent.and_then(ns_window) {
            Some(window) => panel.beginSheetModalForWindow_completionHandler(&window, &handler),
            None => panel.beginWithCompletionHandler(&handler),
        }
    }

    pub(crate) fn ns_window(window: &dyn HasWindowHandle) -> Option<Retained<NSWindow>> {
        let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: raw-window-handle 保证窗口存活期间 ns_view 指向有效的 NSView
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        view.window()
    }

    /// 像用户点"取消"一样关掉所有开着的文件对话框，返回关掉了几个。测试用
    #[doc(hidden)]
    pub fn cancel_file_dialogs() -> usize {
        // 先拷出来：取消时完成回调会同步执行，回调里要改这个列表
        let panels = OPEN.with(|o| o.borrow().clone());
        for panel in &panels {
            // SAFETY: cancel: 的 sender 可以是空
            unsafe { panel.cancel(None) };
        }
        panels.len()
    }

    /// 用系统默认的应用打开网址
    pub fn open_url(url: &str) -> bool {
        let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else { return false };
        NSWorkspace::sharedWorkspace().openURL(&url)
    }

    /// 用默认应用打开文件或文件夹；不存在时返回 `false`
    pub fn open_path(path: &Path) -> bool {
        file_url(path).is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url))
    }

    /// 在访达里显示并选中；不存在时返回 `false`
    pub fn reveal_path(path: &Path) -> bool {
        let Some(url) = file_url(path) else { return false };
        NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
        true
    }

    fn file_url(path: &Path) -> Option<Retained<NSURL>> {
        let path = std::path::absolute(path).ok().filter(|p| p.exists())?;
        if path.is_dir() { NSURL::from_directory_path(path) } else { NSURL::from_file_path(path) }
    }

    /// 系统现在是不是深色外观。读用户默认设置，任何线程都能调用；窗口创建后以窗口报告的为准
    pub fn is_dark_mode() -> Option<bool> {
        let style = NSUserDefaults::standardUserDefaults().stringForKey(ns_string!("AppleInterfaceStyle"));
        Some(style.is_some_and(|style| style.to_string().eq_ignore_ascii_case("dark")))
    }

    /// 辅助功能里打开了"减少动态效果"
    pub fn is_motion_reduced() -> bool {
        NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }

    /// 有包标识符（打包成了 .app）才能发通知
    pub fn notification_available() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// 系统没给权限：记下来，带上系统说的原因
    fn denied(error: *mut NSError) {
        // SAFETY: 系统传来的要么是空，要么是这次回调期间有效的 NSError
        let reason = unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string());
        tracing::error!(reason = reason.as_deref().unwrap_or("用户没有允许"), "没有发通知的权限（系统设置 → 通知）");
    }

    /// 系统的提示音（`NSBeep`：系统设置 → 声音里选的那个）。要在主线程上
    pub fn beep() -> Result<(), PlatformError> {
        if MainThreadMarker::new().is_none() {
            return Err(PlatformError::System("提示音要在主线程上放".into()));
        }
        objc2_app_kit::NSBeep();
        Ok(())
    }

    /// 发系统通知（第一次会请求权限）。UNUserNotificationCenter 要求应用打包成 .app：
    /// 没有包标识符时直接调用会抛异常，所以这时什么都不做，返回"需要打包"
    pub fn notify(title: &str, body: &str) -> Result<(), PlatformError> {
        if !notification_available() {
            return Err(PlatformError::NotPackaged("没有打包成 .app（没有包标识符）".into()));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = format!("aureole-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let (title, body) = (title.to_owned(), body.to_owned());
        // 授权的回调在系统的后台队列里执行，这里只用线程安全的对象
        let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if !granted.as_bool() {
                denied(error);
                return;
            }
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&title));
            content.setBody(&NSString::from_str(&body));
            content.setSound(Some(&UNNotificationSound::defaultSound()));
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
            UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &handler);
        Ok(())
    }

    /// 等用户点的通知：请求的标识 → 回调。系统不一定告诉我们通知被划掉了，太老的忘掉
    static WAITING: Mutex<VecDeque<(String, OnNotificationResponse)>> = Mutex::new(VecDeque::new());
    /// 回调最多留多少条
    const KEPT: usize = 256;
    /// 用过的动作按钮组合：类别要整张表一起交给系统，每种组合一个类别
    static CATEGORIES: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|e| e.into_inner())
    }

    define_class!(
        // SAFETY: NSObject 没有子类化要求；这个类没有实例变量，没有实现 Drop
        #[unsafe(super(NSObject))]
        #[thread_kind = AllocAnyThread]
        struct NotificationDelegate;

        // SAFETY: 协议的方法都由父类 NSObject 实现
        unsafe impl NSObjectProtocol for NotificationDelegate {}

        // SAFETY: 这两个方法的参数和返回类型都和协议的声明一致；都调用了系统给的完成回调
        unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
            /// 应用在前面时系统默认不显示通知：照样显示
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                done.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List | UNNotificationPresentationOptions::Sound,));
            }

            /// 用户点了通知或者按钮：系统在它自己的队列上调用，回调自己转回主线程
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(&self, _center: &UNUserNotificationCenter, response: &UNNotificationResponse, done: &DynBlock<dyn Fn()>) {
                let id = response.notification().request().identifier().to_string();
                let action = response.actionIdentifier();
                // SAFETY: 这个常量是系统导出的不可变字符串，链接后一直有效
                let answer = if action.isEqualToString(unsafe { UNNotificationDefaultActionIdentifier }) {
                    Some(NotificationResponse::Click)
                } else {
                    portable::notification_response(&action.to_string())
                };
                // 划掉（系统的 dismiss 标识）不算点，留着也没用了：一起忘掉
                let waiting = {
                    let mut waiting = lock(&WAITING);
                    waiting.iter().position(|(key, _)| *key == id).and_then(|index| waiting.remove(index)).map(|(_, on_response)| on_response)
                };
                if let (Some(on_response), Some(answer)) = (waiting, answer) {
                    on_response(answer);
                }
                done.call(());
            }
        }
    );

    /// 装上通知中心的代理（整个进程一次）。中心对代理是弱引用，这个对象留到进程结束
    fn install_delegate() {
        static INSTALLED: Once = Once::new();
        INSTALLED.call_once(|| {
            // SAFETY: init 是 NSObject 的初始化方法，返回初始化好的同一个对象；这个类没有实例变量要设
            let delegate: Retained<NotificationDelegate> = unsafe { msg_send![NotificationDelegate::alloc(), init] };
            UNUserNotificationCenter::currentNotificationCenter().setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            std::mem::forget(delegate);
        });
    }

    /// 这组按钮的类别的标识；用过的组合一起交给系统（`setNotificationCategories` 是整张表替换）
    fn category_for(actions: &[String]) -> String {
        let mut categories = lock(&CATEGORIES);
        let index = categories.iter().position(|known| known == actions).unwrap_or_else(|| {
            categories.push(actions.to_vec());
            let all: Vec<Retained<UNNotificationCategory>> = categories
                .iter()
                .enumerate()
                .map(|(index, titles)| {
                    let buttons: Vec<Retained<UNNotificationAction>> = titles
                        .iter()
                        .enumerate()
                        .map(|(button, title)| {
                            // 点了按钮把应用带到前面：处理器多半要显示什么
                            UNNotificationAction::actionWithIdentifier_title_options(
                                &NSString::from_str(&portable::notification_action_id(button)),
                                &NSString::from_str(title),
                                UNNotificationActionOptions::Foreground,
                            )
                        })
                        .collect();
                    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                        &NSString::from_str(&format!("aureole-category-{index}")),
                        &NSArray::from_retained_slice(&buttons),
                        &NSArray::new(),
                        UNNotificationCategoryOptions::empty(),
                    )
                })
                .collect();
            UNUserNotificationCenter::currentNotificationCenter().setNotificationCategories(&NSSet::from_retained_slice(&all));
            categories.len() - 1
        });
        format!("aureole-category-{index}")
    }

    /// 发带按钮的通知。要打包成 .app：没有包标识符时直接调用通知中心会抛异常，所以这时什么都不做，返回"需要打包"
    pub fn show_notification(request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError> {
        if !notification_available() {
            return Err(PlatformError::NotPackaged("没有打包成 .app（没有包标识符）".into()));
        }
        install_delegate();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = format!("aureole-action-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        {
            let mut waiting = lock(&WAITING);
            if waiting.len() >= KEPT {
                waiting.pop_front();
            }
            waiting.push_back((id.clone(), on_response));
        }
        let category = (!request.actions.is_empty()).then(|| category_for(&request.actions));
        let NotificationRequest { title, body, .. } = request;
        // 授权的回调在系统的后台队列里执行，这里只用线程安全的对象
        let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if !granted.as_bool() {
                denied(error);
                return;
            }
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&title));
            content.setBody(&NSString::from_str(&body));
            content.setSound(Some(&UNNotificationSound::defaultSound()));
            if let Some(category) = &category {
                content.setCategoryIdentifier(&NSString::from_str(category));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);
            UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound, &handler);
        Ok(())
    }

    /// 系统的强调色（系统设置 → 外观 → 强调色），换算到 sRGB
    pub fn accent_color() -> Option<[u8; 3]> {
        let color = NSColor::controlAccentColor().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
        let channel = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some([channel(color.redComponent()), channel(color.greenComponent()), channel(color.blueComponent())])
    }

    /// 托盘图标是模板图标，系统按菜单栏着色：不用知道菜单栏的深浅
    pub fn is_panel_dark() -> Option<bool> {
        None
    }

    /// 应用名只在 Windows、Linux 上用到（macOS 读 Info.plist）
    pub fn set_app_name(_name: &str) {}

    /// macOS 上外观变化由窗口报告，"减少动态效果"在窗口获得焦点时重新读：不用另外监视
    pub fn watch_settings(_on_change: SettingsChanged) {}
}

/// 其他系统：什么都没有
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) mod fallback {
    use std::path::Path;

    use raw_window_handle::HasWindowHandle;

    use super::{DialogDone, DialogOptions, SettingsChanged};

    pub fn file_dialog(_options: DialogOptions, _parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
        tracing::warn!("这个平台没有文件对话框");
        done(Vec::new());
    }

    pub fn cancel_file_dialogs() -> usize {
        0
    }

    pub fn open_url(_url: &str) -> bool {
        false
    }

    pub fn open_path(_path: &Path) -> bool {
        false
    }

    pub fn reveal_path(_path: &Path) -> bool {
        false
    }

    pub fn is_dark_mode() -> Option<bool> {
        None
    }

    pub fn is_motion_reduced() -> bool {
        false
    }

    pub fn accent_color() -> Option<[u8; 3]> {
        None
    }

    pub fn is_panel_dark() -> Option<bool> {
        None
    }

    pub fn notification_available() -> bool {
        false
    }

    pub fn notify(_title: &str, _body: &str) -> Result<(), crate::PlatformError> {
        Err(crate::PlatformError::Unsupported("这个平台没有系统通知".into()))
    }

    pub fn beep() -> Result<(), crate::PlatformError> {
        Err(crate::PlatformError::Unsupported("这个平台没有系统提示音".into()))
    }

    pub fn show_notification(_request: crate::NotificationRequest, _on_response: crate::OnNotificationResponse) -> Result<(), crate::PlatformError> {
        Err(crate::PlatformError::Unsupported("这个平台没有系统通知".into()))
    }

    pub fn set_app_name(_name: &str) {}

    pub fn watch_settings(_on_change: SettingsChanged) {}
}
