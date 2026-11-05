//! Windows 的系统集成：
//!
//! - 文件对话框：COM 的 IFileOpenDialog / IFileSaveDialog，在自己的线程里弹出（自己的单线程套间），
//!   主线程的事件循环照常运行；有父窗口时对话框是它的模态窗口（父窗口暂时不能操作，和 macOS 的表单一样）。
//! - 通知：WinRT 的 ToastNotificationManager。它要应用标识（AppUserModelID）：进程自己设过的、
//!   从开始菜单快捷方式启动时快捷方式上的，或者 MSIX 包的身份；都没有时返回 `false`（调用方把通知的内容记下来）。
//! - 外观、强调色：注册表（`AppsUseLightTheme`、`SystemUsesLightTheme`、`DWM\AccentColor`），后台线程监视变化。
//! - 打开：ShellExecuteW；在资源管理器里选中：SHOpenFolderAndSelectItems。

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::HWND;
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::PropertiesSystem::{GPS_DEFAULT, IPropertyStore, SHGetPropertyStoreFromParsingName};
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
    FileSaveDialog, IFileDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, IInspectable, Interface, PCWSTR, Ref};
use windows_sys::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_SUCCESS, LPARAM};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, RRF_RT_REG_DWORD, RegCloseKey, RegGetValueW, RegNotifyChangeKeyValue,
    RegOpenKeyExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, GetStartupInfoW, STARTF_TITLEISAPPID, STARTF_TITLEISLINKNAME, STARTUPINFOW};
use windows_sys::Win32::UI::Shell::{
    GetCurrentProcessExplicitAppUserModelID, ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName, SetCurrentProcessExplicitAppUserModelID,
    ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, PostMessageW, SW_SHOWNORMAL, WM_CLOSE};

use super::{DialogDone, DialogKind, DialogOptions, SettingsChanged, portable};
use crate::{NotificationRequest, OnNotificationResponse, PlatformError};

/// 以 0 结尾的 UTF-16
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

/// 读以 0 结尾的 UTF-16
///
/// # Safety
/// `ptr` 指向以 0 结尾的 UTF-16
unsafe fn from_wide(ptr: *const u16) -> OsString {
    let mut len = 0;
    // SAFETY: 调用方保证以 0 结尾
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        OsString::from_wide(std::slice::from_raw_parts(ptr, len))
    }
}

/// 在这个线程上初始化 COM（单线程套间），离开作用域时配对地释放。已经以别的方式初始化过时什么都不做
struct Com(bool);

impl Com {
    fn init() -> Self {
        // SAFETY: 和 Drop 里的 CoUninitialize 配对
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Com(hr.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 和 init 里成功的 CoInitializeEx 配对
            unsafe { CoUninitialize() };
        }
    }
}

/// 开着的文件对话框所在的线程
static OPEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 弹出文件对话框，马上返回；对话框在自己的线程里，用户选好或取消后在那个线程调用 `done`
pub fn file_dialog(options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
    let owner = parent.and_then(hwnd);
    let chinese = super::chinese();
    let slot = Arc::new(Mutex::new(Some(done)));
    let finish = |slot: &Mutex<Option<DialogDone>>, paths: Vec<PathBuf>| {
        if let Some(done) = lock(slot).take() {
            done(paths);
        }
    };
    let thread_slot = slot.clone();
    let spawned = std::thread::Builder::new().name("文件对话框".into()).spawn(move || {
        // SAFETY: GetCurrentThreadId 没有参数、不会失败
        let thread = unsafe { GetCurrentThreadId() };
        lock(&OPEN).push(thread);
        let result = {
            let _com = Com::init();
            // SAFETY: COM 在这个线程上初始化好了；对话框对象只在这个线程里用
            unsafe { show(&options, owner, chinese) }
        };
        lock(&OPEN).retain(|t| *t != thread);
        let paths = result.unwrap_or_else(|error| {
            // 用户取消是 ERROR_CANCELLED，不算错
            if error.code() != windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
                tracing::error!("文件对话框出错：{error}");
            }
            Vec::new()
        });
        finish(&thread_slot, paths);
    });
    if let Err(error) = spawned {
        tracing::error!("文件对话框的线程开不出来：{error}");
        finish(&slot, Vec::new());
    }
}

fn hwnd(window: &dyn HasWindowHandle) -> Option<isize> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// # Safety
/// 这个线程上已经初始化了 COM
unsafe fn show(options: &DialogOptions, owner: Option<isize>, chinese: bool) -> windows::core::Result<Vec<PathBuf>> {
    let save = options.kind == DialogKind::Save;
    // SAFETY: 调用方初始化了 COM；下面都是对这个对象的普通调用，字符串在调用期间有效
    unsafe {
        let dialog: IFileDialog = if save {
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?
        } else {
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?
        };
        let mut flags = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        flags |= match options.kind {
            DialogKind::OpenDirectory => FOS_PICKFOLDERS,
            DialogKind::OpenFile if options.multiple => FOS_FILEMUSTEXIST | FOS_ALLOWMULTISELECT,
            DialogKind::OpenFile => FOS_FILEMUSTEXIST,
            DialogKind::Save => FOS_OVERWRITEPROMPT,
        };
        dialog.SetOptions(flags)?;
        let title = options.title.as_deref().unwrap_or(portable::default_title(options.kind, options.multiple, chinese));
        dialog.SetTitle(&HSTRING::from(title))?;
        if let Some(dir) = options.directory.as_deref().and_then(|d| std::path::absolute(d).ok())
            && let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir.as_os_str()), None)
        {
            dialog.SetFolder(&folder)?;
        }
        if let Some(name) = &options.file_name {
            dialog.SetFileName(&HSTRING::from(name.as_str()))?;
        }
        if options.kind != DialogKind::OpenDirectory {
            let specs = portable::windows_filter_specs(&options.filters, portable::all_files_name(chinese));
            let strings: Vec<(Vec<u16>, Vec<u16>)> = specs.iter().map(|(name, spec)| (wide(name), wide(spec))).collect();
            let raw: Vec<COMDLG_FILTERSPEC> =
                strings.iter().map(|(name, spec)| COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }).collect();
            if !raw.is_empty() {
                dialog.SetFileTypes(&raw)?;
                dialog.SetFileTypeIndex(1)?;
            }
            // 保存时用户只打了名字，补上第一种类型的扩展名
            if save && let Some(ext) = options.filters.iter().flat_map(|f| portable::clean_extensions(&f.extensions)).next() {
                dialog.SetDefaultExtension(&HSTRING::from(ext.as_str()))?;
            }
        }
        dialog.Show(owner.map(|h| HWND(h as *mut c_void)))?;

        let mut paths = Vec::new();
        if options.kind == DialogKind::OpenFile && options.multiple {
            let items = dialog.cast::<IFileOpenDialog>()?.GetResults()?;
            for i in 0..items.GetCount()? {
                paths.push(item_path(&items.GetItemAt(i)?)?);
            }
        } else {
            paths.push(item_path(&dialog.GetResult()?)?);
        }
        Ok(paths)
    }
}

/// # Safety
/// 这个线程上已经初始化了 COM
unsafe fn item_path(item: &IShellItem) -> windows::core::Result<PathBuf> {
    // SAFETY: 返回的字符串由 COM 分配，读完用 CoTaskMemFree 释放
    unsafe {
        let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = PathBuf::from(OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0 as *const c_void));
        Ok(path)
    }
}

/// 像用户点"取消"一样关掉所有开着的文件对话框，返回关掉了几个。测试用
#[doc(hidden)]
pub fn cancel_file_dialogs() -> usize {
    unsafe extern "system" fn close(window: windows_sys::Win32::Foundation::HWND, _: LPARAM) -> windows_sys::core::BOOL {
        // 对话框收到 WM_CLOSE 等于点了"取消"
        // SAFETY: 窗口句柄来自 EnumThreadWindows
        unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
        1
    }
    let threads = lock(&OPEN).clone();
    for &thread in &threads {
        // SAFETY: close 的签名就是 EnumThreadWindows 要的回调类型，只给枚举到的窗口投递消息、不保存句柄；
        // thread 是还开着对话框的线程（对话框关掉前不会从 OPEN 里去掉），线程已经结束时函数只是什么都枚举不到
        unsafe { EnumThreadWindows(thread, Some(close), 0) };
    }
    threads.len()
}

/// 用系统默认的应用打开网址
pub fn open_url(url: &str) -> bool {
    shell_open(OsStr::new(url))
}

/// 用默认应用打开文件或文件夹；不存在时返回 `false`
pub fn open_path(path: &Path) -> bool {
    match std::path::absolute(path) {
        Ok(path) if path.exists() => shell_open(path.as_os_str()),
        _ => false,
    }
}

fn shell_open(target: &OsStr) -> bool {
    // ShellExecute 可能交给用 COM 实现的外壳扩展，先初始化 COM
    let _com = Com::init();
    let (verb, file) = (wide("open"), wide(target));
    // SAFETY: 字符串以 0 结尾，在调用期间有效；其余参数可以为空
    let result = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    // 大于 32 表示成功
    result as isize > 32
}

/// 在资源管理器里显示并选中；不存在时返回 `false`。在后台线程里打开（资源管理器启动可能要一会儿）
pub fn reveal_path(path: &Path) -> bool {
    let Some(path) = std::path::absolute(path).ok().filter(|p| p.exists()) else { return false };
    let spawned = std::thread::Builder::new().name("在资源管理器中显示".into()).spawn(move || {
        let _com = Com::init();
        let name = wide(path.as_os_str());
        let mut pidl = std::ptr::null_mut();
        // SAFETY: 路径以 0 结尾；拿到的 PIDL 用 ILFree 释放
        let shown = unsafe {
            let parsed = SHParseDisplayName(name.as_ptr(), std::ptr::null_mut(), &mut pidl, 0, std::ptr::null_mut()) >= 0 && !pidl.is_null();
            let shown = parsed && SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0) >= 0;
            if !pidl.is_null() {
                ILFree(pidl);
            }
            shown
        };
        if !shown {
            // 退回到命令行：explorer /select,"路径"（参数原样传，不让标准库加引号）
            let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
        }
    });
    spawned.is_ok()
}

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// 读当前用户的一个 DWORD 注册表值
fn read_dword(key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let (mut data, mut size) = (0u32, size_of::<u32>() as u32);
    // SAFETY: 字符串以 0 结尾；data 有 size 个字节
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&raw mut data).cast(), &mut size)
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// 应用是不是深色外观（设置 → 个性化 → 颜色 → 默认应用模式）
pub fn is_dark_mode() -> Option<bool> {
    portable::dark_from_light_theme_value(read_dword(PERSONALIZE, "AppsUseLightTheme"))
}

/// 任务栏是不是深色（默认 Windows 模式）：托盘图标按它选颜色
pub fn is_panel_dark() -> Option<bool> {
    portable::dark_from_light_theme_value(read_dword(PERSONALIZE, "SystemUsesLightTheme"))
}

/// 系统的强调色（设置 → 个性化 → 颜色 → 强调色）
pub fn accent_color() -> Option<[u8; 3]> {
    read_dword(r"Software\Microsoft\Windows\DWM", "AccentColor").map(portable::rgb_from_abgr)
}

/// 系统关掉了窗口内的动画（设置 → 辅助功能 → 视觉效果 → 动画效果）
pub fn is_motion_reduced() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut enabled: windows_sys::core::BOOL = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION 往 pvparam 写一个 BOOL
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut enabled).cast(), 0) };
    ok != 0 && enabled == 0
}

/// 在后台线程监视"个性化"的注册表项，深浅色变了就调用 `on_change`（没有窗口、只有托盘时也收得到）
pub fn watch_settings(on_change: SettingsChanged) {
    let _ = std::thread::Builder::new().name("系统设置".into()).spawn(move || {
        let mut key: HKEY = std::ptr::null_mut();
        let name = wide(PERSONALIZE);
        // SAFETY: 名字以 0 结尾；打开的键在线程结束前关闭
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, KEY_NOTIFY, &mut key) != ERROR_SUCCESS {
                return;
            }
            // 同步等：这个键下的值变了才返回
            while RegNotifyChangeKeyValue(key, 0, REG_NOTIFY_CHANGE_LAST_SET, std::ptr::null_mut(), 0) == ERROR_SUCCESS {
                on_change();
            }
            RegCloseKey(key);
        }
    });
}

/// 通知用的应用身份
enum AppId {
    /// MSIX 包：系统知道是谁
    Package,
    /// AppUserModelID
    Id(HSTRING),
}

/// 找应用标识：进程自己设过的 → 启动方带过来的（`STARTF_TITLEISAPPID`）→ 从快捷方式启动时快捷方式上的
/// `System.AppUserModel.ID`（系统在启动信息里给的是快捷方式的路径，`STARTF_TITLEISLINKNAME`）→ MSIX 包。
/// 从启动信息里找到的顺手设成进程的，任务栏分组也对得上
fn app_id() -> Option<&'static AppId> {
    static ID: OnceLock<Option<AppId>> = OnceLock::new();
    ID.get_or_init(|| {
        // SAFETY: 返回的字符串用 CoTaskMemFree 释放；启动信息里的标题以 0 结尾
        unsafe {
            let mut explicit = std::ptr::null_mut();
            if GetCurrentProcessExplicitAppUserModelID(&mut explicit) >= 0 && !explicit.is_null() {
                let id = from_wide(explicit);
                windows_sys::Win32::System::Com::CoTaskMemFree(explicit.cast());
                if !id.is_empty() {
                    return Some(AppId::Id(HSTRING::from(id.as_os_str())));
                }
            }
            let mut info: STARTUPINFOW = std::mem::zeroed();
            info.cb = size_of::<STARTUPINFOW>() as u32;
            GetStartupInfoW(&mut info);
            if !info.lpTitle.is_null() {
                let title = from_wide(info.lpTitle);
                let id = if info.dwFlags & STARTF_TITLEISAPPID != 0 {
                    Some(title)
                } else if info.dwFlags & STARTF_TITLEISLINKNAME != 0 {
                    shortcut_app_id(&title)
                } else {
                    None
                };
                if let Some(id) = id.filter(|id| !id.is_empty()) {
                    SetCurrentProcessExplicitAppUserModelID(wide(&id).as_ptr());
                    return Some(AppId::Id(HSTRING::from(id.as_os_str())));
                }
            }
            let mut len = 0u32;
            if GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) != APPMODEL_ERROR_NO_PACKAGE {
                return Some(AppId::Package);
            }
        }
        None
    })
    .as_ref()
}

/// 快捷方式（`.lnk`）上的 `System.AppUserModel.ID`；没设时是 `None`
fn shortcut_app_id(link: &OsStr) -> Option<OsString> {
    let _com = Com::init();
    // SAFETY: COM 初始化好了；PROPVARIANT 用 PropVariantClear 释放，转出来的字符串用 CoTaskMemFree 释放
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(&HSTRING::from(link), None, GPS_DEFAULT).ok()?;
        let mut value = store.GetValue(&PKEY_AppUserModel_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok();
        let _ = PropVariantClear(&mut value);
        let text = text?;
        let id = OsString::from_wide(text.as_wide());
        CoTaskMemFree(Some(text.0 as *const c_void));
        (!id.is_empty()).then_some(id)
    }
}

/// 有应用标识才能发通知
pub fn notification_available() -> bool {
    app_id().is_some()
}

/// 排进"通知"线程的一条通知
struct QueuedToast {
    request: NotificationRequest,
    on_response: Option<OnNotificationResponse>,
}

/// 带回调的通知：对象要留着，激活的事件才收得到。太老的忘掉（系统也早就收走了）
static LIVE_TOASTS: Mutex<std::collections::VecDeque<ToastNotification>> = Mutex::new(std::collections::VecDeque::new());
const KEPT_TOASTS: usize = 64;

fn show_toast(app: &AppId, queued: QueuedToast) -> windows::core::Result<()> {
    let QueuedToast { request, on_response } = queued;
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(portable::toast_xml(&request.title, &request.body, &request.actions)))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    if let Some(on_response) = on_response {
        // 在系统的线程上调用：回调自己转回主线程。点通知本身和点按钮都走这里，参数说明是哪个
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(move |_toast, args: Ref<IInspectable>| {
            let arguments = args.as_ref().and_then(|args| args.cast::<ToastActivatedEventArgs>().ok()).and_then(|args| args.Arguments().ok());
            if let Some(response) = arguments.and_then(|arguments| portable::notification_response(&arguments.to_string_lossy())) {
                on_response(response);
            }
            Ok(())
        }))?;
        let mut live = lock(&LIVE_TOASTS);
        if live.len() >= KEPT_TOASTS {
            live.pop_front();
        }
        live.push_back(toast.clone());
    }
    let notifier = match app {
        AppId::Package => ToastNotificationManager::CreateToastNotifier()?,
        AppId::Id(id) => ToastNotificationManager::CreateToastNotifierWithId(id)?,
    };
    notifier.Show(&toast)
}

/// "通知"线程的队列：WinRT 的激活和发送在这个线程上做（多线程套间），调用方不等
fn notification_queue() -> Option<&'static Sender<QueuedToast>> {
    static QUEUE: OnceLock<Option<Sender<QueuedToast>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (sender, receiver) = channel::<QueuedToast>();
            let spawned = std::thread::Builder::new().name("通知".into()).spawn(move || {
                // SAFETY: 这个线程进入多线程套间，一直到线程结束（不配对释放：线程结束时系统收回）
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                for queued in receiver {
                    let Some(app) = app_id() else { continue };
                    let (title, body) = (queued.request.title.clone(), queued.request.body.clone());
                    if let Err(error) = show_toast(app, queued) {
                        tracing::info!(%title, %body, "系统通知没发出去（{error}），通知的内容记在这里");
                    }
                }
            });
            spawned.ok().map(|_| sender)
        })
        .as_ref()
}

fn enqueue(queued: QueuedToast) -> Result<(), PlatformError> {
    if app_id().is_none() {
        return Err(PlatformError::NotPackaged("没有应用标识（AppUserModelID）".into()));
    }
    let queue = notification_queue().ok_or_else(|| PlatformError::System("通知的线程开不出来".into()))?;
    queue.send(queued).map_err(|_| PlatformError::System("通知的线程已经结束".into()))
}

/// 发通知（toast），在"通知"线程上发，调用方不等。没有应用标识（直接运行的可执行文件，比如 `cargo run`）时返回"需要打包"；
/// 排进队列以后的失败在那个线程上记下通知的内容
pub fn notify(title: &str, body: &str) -> Result<(), PlatformError> {
    enqueue(QueuedToast { request: NotificationRequest { title: title.to_owned(), body: body.to_owned(), actions: Vec::new() }, on_response: None })
}

/// 发带按钮的通知，用户点了（进程还活着时）在系统的线程上调用 `on_response`。前提和 [`notify`] 一样
pub fn show_notification(request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError> {
    enqueue(QueuedToast { request, on_response: Some(on_response) })
}

/// Windows 上通知按应用标识显示，不用应用名
pub fn set_app_name(_name: &str) {}
