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
pub(crate) struct Com(bool);

impl Com {
    pub(crate) fn init() -> Self {
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

pub(crate) fn hwnd(window: &dyn HasWindowHandle) -> Option<isize> {
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
        if let Some(dir) = options.directory.as_deref().and_then(|d| std::path::absolute(d).ok()).map(|d| shell_path(&d))
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

/// 能交给外壳打开的网址：有协议名（两个字符以上，盘符 `C:` 不算），没有控制字符，不是 `file:`（打开文件走 [`open_path`]）。
/// 外壳打开的东西什么都可以是：不挡住的话，`C:\…\程序.exe`、`\\服务器\共享\…` 这样的"链接"会被直接运行
pub(crate) fn is_shell_url(url: &str) -> bool {
    crate::open::is_url(url)
        && !url.chars().any(char::is_control)
        && url.split_once(':').is_some_and(|(scheme, _)| !scheme.eq_ignore_ascii_case("file"))
}

/// 外壳的接口（解析路径、资源管理器、文件对话框）不认 `\\?\` 开头的写法（`std::fs::canonicalize` 给的就是这种）：
/// `\\?\C:\…` 换成 `C:\…`，`\\?\UNC\服务器\…` 换成 `\\服务器\…`；别的（卷的编号这类）原样
pub(crate) fn shell_path(path: &Path) -> PathBuf {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let Some(rest) = units.strip_prefix(verbatim.as_slice()) else { return path.to_path_buf() };
    let unc: Vec<u16> = r"UNC\".encode_utf16().collect();
    if rest.len() > unc.len() && rest[..unc.len()].iter().zip(&unc).all(|(a, b)| u8::try_from(*a).is_ok_and(|a| a.eq_ignore_ascii_case(&(*b as u8)))) {
        let mut plain: Vec<u16> = r"\\".encode_utf16().collect();
        plain.extend_from_slice(&rest[unc.len()..]);
        return PathBuf::from(OsString::from_wide(&plain));
    }
    let drive = rest.len() >= 3 && char::from_u32(u32::from(rest[0])).is_some_and(|c| c.is_ascii_alphabetic()) && rest[1] == u16::from(b':') && rest[2] == u16::from(b'\\');
    if drive { PathBuf::from(OsString::from_wide(rest)) } else { path.to_path_buf() }
}

/// 用系统默认的应用打开网址（在后台线程上交给外壳）。不是能交给外壳的网址（见 [`is_shell_url`]）时返回 `false`
pub fn open_url(url: &str) -> bool {
    is_shell_url(url) && shell_execute(OsString::from(url), false)
}

/// 用默认应用打开文件或文件夹（在后台线程上交给外壳）；不存在时返回 `false`。
/// 没有关联的应用时弹出系统的"打开方式"，让用户挑一个（和 macOS 一样）
pub fn open_path(path: &Path) -> bool {
    match std::path::absolute(path) {
        Ok(path) if path.exists() => shell_execute(shell_path(&path).into_os_string(), true),
        _ => false,
    }
}

/// 交给外壳打开（`ShellExecuteExW`，默认的动作）：在自己的线程里（单线程套间），外壳的扩展、慢的处理程序不卡住调用方。
/// 等外壳做完再退出线程（`SEE_MASK_NOASYNC`），出错不弹系统的提示框（`SEE_MASK_FLAG_NO_UI`），记一条日志。
/// 返回交出去了没有（线程开不出来时是 `false`）
fn shell_execute(target: OsString, open_with: bool) -> bool {
    use windows_sys::Win32::Foundation::ERROR_NO_ASSOCIATION;
    use windows_sys::Win32::UI::Shell::{
        OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OAIF_REGISTER_EXT, OPENASINFO, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHOpenWithDialog,
        ShellExecuteExW,
    };
    let spawned = std::thread::Builder::new().name("交给外壳打开".into()).spawn(move || {
        // 外壳可能交给用 COM 实现的扩展
        let _com = Com::init();
        let file = wide(&target);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: file.as_ptr(),
            nShow: SW_SHOWNORMAL,
            ..Default::default()
        };
        // SAFETY: 结构体的大小填好了；字符串以 0 结尾、活到调用结束；动作为空：用默认的
        if unsafe { ShellExecuteExW(&mut info) } != 0 {
            return;
        }
        let error = std::io::Error::last_os_error();
        if open_with && error.raw_os_error() == Some(ERROR_NO_ASSOCIATION as i32) {
            let ask = OPENASINFO { pcszFile: file.as_ptr(), pcszClass: std::ptr::null(), oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC };
            // SAFETY: 路径以 0 结尾、活到调用结束；没有父窗口
            let shown = unsafe { SHOpenWithDialog(std::ptr::null_mut(), &ask) };
            if shown < 0 {
                tracing::warn!(target = %target.to_string_lossy(), "没有能打开它的应用，也弹不出\"打开方式\"：{shown:#x}");
            }
            return;
        }
        tracing::warn!(target = %target.to_string_lossy(), "系统没能打开：{error}");
    });
    spawned.is_ok()
}

/// 在资源管理器里显示并选中；不存在时返回 `false`。在后台线程里打开（资源管理器启动可能要一会儿）
pub fn reveal_path(path: &Path) -> bool {
    let Some(path) = std::path::absolute(path).ok().filter(|p| p.exists()).map(|p| shell_path(&p)) else { return false };
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

/// 监视的注册表项：深浅色（个性化）、强调色（DWM）、"动画效果"（`Control Panel\Desktop` 的 `UserPreferencesMask`，
/// `SPI_SETCLIENTAREAANIMATION` 写在这里）
const WATCHED_KEYS: [&str; 3] = [PERSONALIZE, r"Software\Microsoft\Windows\DWM", r"Control Panel\Desktop"];

/// 一次改动常常连着写好几个值（切深浅色时先后写两个）：收到第一个通知后等这么久，合成一次
const SETTINGS_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// 监视着的一个注册表项：变了时系统把事件置位（手动复位的事件）
struct WatchedKey {
    key: HKEY,
    event: windows_sys::Win32::Foundation::HANDLE,
}

impl WatchedKey {
    fn open(path: &str) -> Option<Self> {
        use windows_sys::Win32::System::Threading::CreateEventW;
        let name = wide(path);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: 名字以 0 结尾；打开的键、建的事件由 Drop 关掉
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, KEY_NOTIFY, &mut key) != ERROR_SUCCESS {
                return None;
            }
            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if event.is_null() {
                RegCloseKey(key);
                return None;
            }
            Some(Self { key, event })
        }
    }

    /// 登记一次通知（异步：变了时事件置位）。通知一次就失效，变了以后要重新登记
    fn arm(&self) -> bool {
        use windows_sys::Win32::System::Threading::ResetEvent;
        // SAFETY: 键和事件都是 `open` 打开的、还没关
        unsafe {
            ResetEvent(self.event);
            RegNotifyChangeKeyValue(self.key, 0, REG_NOTIFY_CHANGE_LAST_SET, self.event, 1) == ERROR_SUCCESS
        }
    }

    /// 事件置位了（这个键变过）
    fn fired(&self) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: 事件是 `open` 建的、还没关；不等，只看一眼
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for WatchedKey {
    fn drop(&mut self) {
        // SAFETY: 键和事件是 `open` 打开的，各关一次
        unsafe {
            RegCloseKey(self.key);
            windows_sys::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

/// 在后台线程监视系统设置的注册表项（深浅色、强调色、动画效果），变了就调用 `on_change`（没有窗口、只有托盘时也收得到）。
/// 连着的几次改动合成一次；变过的键先重新登记再调用回调，回调期间的改动不会漏掉
pub fn watch_settings(on_change: SettingsChanged) {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
    let _ = std::thread::Builder::new().name("系统设置".into()).spawn(move || {
        let keys: Vec<WatchedKey> = WATCHED_KEYS.iter().filter_map(|path| WatchedKey::open(path)).collect();
        if keys.is_empty() || !keys.iter().all(WatchedKey::arm) {
            return;
        }
        let events: Vec<windows_sys::Win32::Foundation::HANDLE> = keys.iter().map(|key| key.event).collect();
        loop {
            // SAFETY: 事件句柄都活着（`keys` 拿着），个数如实给出；等到任意一个置位
            let woke = unsafe { WaitForMultipleObjects(events.len() as u32, events.as_ptr(), 0, INFINITE) };
            if woke >= WAIT_OBJECT_0 + events.len() as u32 {
                return;
            }
            std::thread::sleep(SETTINGS_SETTLE);
            for key in keys.iter().filter(|key| key.fired()) {
                if !key.arm() {
                    return;
                }
            }
            on_change();
        }
    });
}

/// 通知用的应用身份
#[derive(Clone)]
enum AppId {
    /// MSIX 包：系统知道是谁
    Package,
    /// AppUserModelID
    Id(HSTRING),
}

/// 应用自己声明的标识（[`set_app_user_model_id`]），没声明过是空
static DECLARED: Mutex<Option<String>> = Mutex::new(None);

/// 应用标识最长多少个 UTF-16 字符（系统的限制）
const MAX_APP_ID: usize = 128;

/// 应用标识写得对不对：不空、不超过 128 个字符、没有空白和控制字符（系统不收带空格的）
pub(crate) fn is_valid_app_id(id: &str) -> bool {
    !id.is_empty() && id.encode_utf16().count() <= MAX_APP_ID && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// 打包时写进可执行文件的包标识符资源（`RT_RCDATA`，名字是 [`APP_ID_RESOURCE`]，内容是 UTF-8）；没有时是 `None`
pub fn embedded_app_id() -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{FindResourceW, GetModuleHandleW, LoadResource, LockResource, SizeofResource};
    let name: Vec<u16> = APP_ID_RESOURCE.encode_utf16().chain(Some(0)).collect();
    // 资源的类型是"整数当指针传"（`RT_RCDATA` 是 10），系统不会去读它指向的地方
    let rt_rcdata = std::ptr::without_provenance::<u16>(10);
    // SAFETY: 空的模块名是这个进程的可执行文件；名字以 0 结尾。资源和模块一样活着，`LockResource` 给的指针指向 `size` 个字节，只读
    let bytes = unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let found = FindResourceW(module, name.as_ptr(), rt_rcdata);
        if found.is_null() {
            return None;
        }
        let size = SizeofResource(module, found) as usize;
        let data = LockResource(LoadResource(module, found));
        if data.is_null() || size == 0 {
            return None;
        }
        std::slice::from_raw_parts(data.cast::<u8>(), size)
    };
    let id = std::str::from_utf8(bytes).ok()?.trim_end_matches(char::from(0)).trim();
    is_valid_app_id(id).then(|| id.to_owned())
}

/// `cargo bundle` 写进可执行文件的包标识符资源的名字（打包工具里写的是同一个）
pub const APP_ID_RESOURCE: &str = "AUREOLE_APP_ID";

/// 声明进程的应用标识（AppUserModelID），用打包时的包标识符（安装程序建的开始菜单快捷方式上写的就是它）。
/// 要在建第一个窗口之前调用：任务栏按它给窗口分组、固定到任务栏，不管应用是从快捷方式、双击文档、协议链接还是命令行启动的。
/// 系统通知要这个标识在系统里登记过：从带这个标识的快捷方式启动、开始菜单里有带它的快捷方式，或者
/// `Software\Classes\AppUserModelId\<标识>` 这一项存在；都没有时（`cargo run`）照旧不发通知、把内容记在日志里。
/// 标识写得不对、系统不收时返回 `false`
pub fn set_app_user_model_id(id: &str) -> bool {
    if !is_valid_app_id(id) {
        return false;
    }
    // SAFETY: 字符串以 0 结尾，调用期间活着
    let ok = unsafe { SetCurrentProcessExplicitAppUserModelID(wide(id).as_ptr()) } >= 0;
    if ok {
        *lock(&DECLARED) = Some(id.to_owned());
    }
    ok
}

/// 进程是 MSIX 包里的
fn packaged() -> bool {
    static PACKAGED: OnceLock<bool> = OnceLock::new();
    *PACKAGED.get_or_init(|| {
        let mut len = 0u32;
        // SAFETY: 缓冲区为空、长度为 0：只问有没有包
        unsafe { GetCurrentPackageFullName(&mut len, std::ptr::null_mut()) != APPMODEL_ERROR_NO_PACKAGE }
    })
}

/// 启动方带过来的应用标识：`STARTF_TITLEISAPPID`，或者从快捷方式启动时快捷方式上的 `System.AppUserModel.ID`
/// （系统在启动信息里给的是快捷方式的路径，`STARTF_TITLEISLINKNAME`）
fn launch_app_id() -> Option<&'static OsString> {
    static ID: OnceLock<Option<OsString>> = OnceLock::new();
    ID.get_or_init(|| {
        // SAFETY: 启动信息是全零的整数和指针，系统填好；里面的标题以 0 结尾
        unsafe {
            let mut info: STARTUPINFOW = std::mem::zeroed();
            info.cb = size_of::<STARTUPINFOW>() as u32;
            GetStartupInfoW(&mut info);
            if info.lpTitle.is_null() {
                return None;
            }
            let title = from_wide(info.lpTitle);
            let id = if info.dwFlags & STARTF_TITLEISAPPID != 0 {
                Some(title)
            } else if info.dwFlags & STARTF_TITLEISLINKNAME != 0 {
                shortcut_app_id(&title)
            } else {
                None
            };
            id.filter(|id| !id.is_empty())
        }
    })
    .as_ref()
}

/// 应用没声明标识时：进程别处设过的 → 启动方带过来的（顺手设成进程的，任务栏分组也对得上）→ MSIX 包
fn undeclared_app_id() -> Option<&'static AppId> {
    static ID: OnceLock<Option<AppId>> = OnceLock::new();
    ID.get_or_init(|| {
        // SAFETY: 返回的字符串用 CoTaskMemFree 释放
        unsafe {
            let mut explicit = std::ptr::null_mut();
            if GetCurrentProcessExplicitAppUserModelID(&mut explicit) >= 0 && !explicit.is_null() {
                let id = from_wide(explicit);
                windows_sys::Win32::System::Com::CoTaskMemFree(explicit.cast());
                if !id.is_empty() {
                    return Some(AppId::Id(HSTRING::from(id.as_os_str())));
                }
            }
            if let Some(id) = launch_app_id() {
                SetCurrentProcessExplicitAppUserModelID(wide(id).as_ptr());
                return Some(AppId::Id(HSTRING::from(id.as_os_str())));
            }
        }
        packaged().then_some(AppId::Package)
    })
    .as_ref()
}

/// 发通知用的应用标识。应用声明过的（[`set_app_user_model_id`]）要在系统里登记过才算（见那里），不然系统收下通知却不显示
fn app_id() -> Option<AppId> {
    let declared = lock(&DECLARED).clone();
    let Some(id) = declared else { return undeclared_app_id().cloned() };
    if packaged() {
        return Some(AppId::Package);
    }
    let known = launch_app_id().is_some_and(|launch| launch.as_os_str() == OsStr::new(&id)) || app_id_registered(&id);
    known.then(|| AppId::Id(HSTRING::from(id.as_str())))
}

/// 应用标识在系统里登记过：注册表里有 `Software\Classes\AppUserModelId\<标识>`（当前用户或者整台机器），
/// 或者开始菜单里（两层以内）有带这个标识的快捷方式。查一次记下来
fn app_id_registered(id: &str) -> bool {
    static CHECKED: Mutex<Option<(String, bool)>> = Mutex::new(None);
    let mut checked = lock(&CHECKED);
    if let Some((known, registered)) = &*checked
        && known == id
    {
        return *registered;
    }
    let registered = app_id_in_registry(id) || app_id_in_start_menu(id);
    *checked = Some((id.to_owned(), registered));
    registered
}

fn app_id_in_registry(id: &str) -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, KEY_READ};
    let name = wide(format!(r"Software\Classes\AppUserModelId\{id}"));
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().any(|root| {
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: 名字以 0 结尾；打开了就马上关掉
        unsafe {
            let opened = RegOpenKeyExW(root, name.as_ptr(), 0, KEY_READ, &mut key) == ERROR_SUCCESS;
            if opened {
                RegCloseKey(key);
            }
            opened
        }
    })
}

/// 开始菜单（当前用户的和所有用户的"程序"文件夹，两层以内，最多看这么多个快捷方式）里有没有带这个标识的快捷方式
fn app_id_in_start_menu(id: &str) -> bool {
    use windows::Win32::UI::Shell::{FOLDERID_CommonPrograms, FOLDERID_Programs, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    const MAX_LINKS: usize = 400;
    let folder = |known: &windows::core::GUID| {
        // SAFETY: 返回的字符串由 COM 分配，读完用 CoTaskMemFree 释放
        unsafe {
            let text = SHGetKnownFolderPath(known, KF_FLAG_DEFAULT, None).ok()?;
            let path = PathBuf::from(OsString::from_wide(text.as_wide()));
            CoTaskMemFree(Some(text.0 as *const c_void));
            Some(path)
        }
    };
    let mut links = Vec::new();
    let mut pending: Vec<(PathBuf, u32)> = [FOLDERID_Programs, FOLDERID_CommonPrograms].iter().filter_map(folder).map(|dir| (dir, 0)).collect();
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if depth < 1 {
                    pending.push((path, depth + 1));
                }
            } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("lnk")) && links.len() < MAX_LINKS {
                links.push(path);
            }
        }
    }
    let id = OsStr::new(id);
    links.iter().any(|link| shortcut_app_id(link.as_os_str()).is_some_and(|found| found == id))
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
                    if let Err(error) = show_toast(&app, queued) {
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

/// 系统的提示音（`MessageBeep(MB_OK)`：声音设置里"默认响声"那一个）
pub fn beep() -> Result<(), PlatformError> {
    use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows_sys::Win32::UI::WindowsAndMessaging::MB_OK;
    // SAFETY: MessageBeep 只把声音排进系统的队列，参数是常量，没有指针
    if unsafe { MessageBeep(MB_OK) } != 0 { Ok(()) } else { Err(PlatformError::System("MessageBeep 失败".into())) }
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

// ---- 窗口：指针位置、带到最前面、深色外框的老写法；低电量 ----

/// 这台机器的系统版本号（`CurrentBuildNumber`，不受程序清单里声明的版本影响），读不到时是 0
pub(crate) fn os_build() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| {
        use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
        let (key, name) = (wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"), wide("CurrentBuildNumber"));
        let mut buffer = [0u16; 16];
        let mut size = size_of_val(&buffer) as u32;
        // SAFETY: 键名和值名以 0 结尾；缓冲区的大小写在 `size` 里，系统不会写出界
        let status = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buffer.as_mut_ptr().cast(), &mut size)
        };
        if status != ERROR_SUCCESS {
            return 0;
        }
        String::from_utf16_lossy(&buffer).trim_end_matches('\0').trim().parse().unwrap_or(0)
    })
}

/// 屏幕上的物理像素换成窗口内容区里的逻辑像素（左上角为原点，可能在窗口外）
fn client_logical(hwnd: windows_sys::Win32::Foundation::HWND, mut point: windows_sys::Win32::Foundation::POINT) -> Option<[f32; 2]> {
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    // SAFETY: `hwnd` 是活着的窗口；`point` 是栈上的结构体
    let (ok, dpi) = unsafe { (ScreenToClient(hwnd, &mut point) != 0, GetDpiForWindow(hwnd)) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    ok.then(|| [point.x as f32 / scale, point.y as f32 / scale])
}

/// 指针此刻在窗口内容区里的位置（逻辑像素）：`GetCursorPos` 换到窗口里
pub fn pointer_position(window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let hwnd = hwnd(window)? as windows_sys::Win32::Foundation::HWND;
    let mut point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    // SAFETY: 只写栈上的结构体
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return None;
    }
    client_logical(hwnd, point)
}

/// 把窗口带到最前面：最小化了先还原，再设成前台窗口。别的程序在前台、系统不许抢时（没有拿到前台的许可）
/// 让任务栏上的按钮闪动，等用户点（这是系统的规矩，不硬抢）。返回做了没有（不是这个线程的窗口时是 `false`）
pub fn bring_to_front(window: &dyn HasWindowHandle) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOW,
        SetForegroundWindow, ShowWindow,
    };
    let Some(hwnd) = hwnd(window).map(|h| h as windows_sys::Win32::Foundation::HWND) else { return false };
    // SAFETY: `hwnd` 是活着的窗口；下面只读写它的状态，结构体在栈上、大小填好了
    unsafe {
        if GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) != GetCurrentThreadId() {
            return false;
        }
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOW);
        }
        if SetForegroundWindow(hwnd) == 0 || GetForegroundWindow() != hwnd {
            let flash = FLASHWINFO { cbSize: size_of::<FLASHWINFO>() as u32, hwnd, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 0, dwTimeout: 0 };
            FlashWindowEx(&flash);
        }
    }
    true
}

/// Windows 10 1809–1909（版本号 17763 到 19040）的深色外框属性编号是 19，之后才是 20（`DWMWA_USE_IMMERSIVE_DARK_MODE`）
const LEGACY_DARK_MODE_ATTRIBUTE: u32 = 19;

/// 深色外框属性该用哪个编号：新系统 20，老的 Windows 10 是 19，更老的没有
pub(crate) fn dark_mode_attribute(build: u32) -> Option<u32> {
    match build {
        19041.. => Some(windows_sys::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE as u32),
        17763..19041 => Some(LEGACY_DARK_MODE_ATTRIBUTE),
        _ => None,
    }
}

/// 老的 Windows 10 上写深色外框（属性 19）：新编号的写法失败时调用。系统太老、写不上时返回 `false`
pub fn set_dark_frame_legacy(window: &dyn HasWindowHandle, dark: bool) -> bool {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    let Some(attribute) = dark_mode_attribute(os_build()).filter(|a| *a == LEGACY_DARK_MODE_ATTRIBUTE) else { return false };
    let Some(hwnd) = hwnd(window).map(|h| h as windows_sys::Win32::Foundation::HWND) else { return false };
    let value = i32::from(dark);
    // SAFETY: `hwnd` 是活着的窗口；属性的值是 4 字节的布尔，在栈上
    unsafe { DwmSetWindowAttribute(hwnd, attribute, std::ptr::from_ref(&value).cast(), size_of::<i32>() as u32) >= 0 }
}

/// 系统开着省电模式（Windows 10 的"节电模式"、Windows 11 的"节能模式"）：`GetSystemPowerStatus` 的 `SystemStatusFlag`
pub fn is_low_power_mode() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    // SAFETY: 结构体全是整数，全零有效，系统填好
    let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: 只写栈上的结构体
    let ok = unsafe { GetSystemPowerStatus(&mut status) } != 0;
    ok && status.SystemStatusFlag == 1
}

// ---- 输入的系统设置（滚轮、双击、拖动）和输入法 ----

/// 惯例里跟着系统设置的几项最多隔这么久重新读一次
const CONVENTIONS_REREAD: std::time::Duration = std::time::Duration::from_secs(1);

/// 读到的惯例和读的时间
struct ReadConventions {
    at: std::time::Instant,
    current: &'static crate::Conventions,
}

static CONVENTIONS: Mutex<Option<ReadConventions>> = Mutex::new(None);

/// 按此刻的系统设置填好的惯例：滚轮一格滚多远、双击的间隔和范围、拖动的阈值，别的是 [`crate::Conventions::WINDOWS`]。
/// 按键、滚轮、点击都要问惯例，读系统设置要好几次系统调用：最多隔一秒重新读一次（用户在设置里改了，一秒内生效）。
/// 后端的接口借出去的是引用，设置变了时换一份新的、旧的不释放（别处可能还拿着）：只有用户真的改了设置才多一份，一个进程里改不了几次
pub fn conventions() -> &'static crate::Conventions {
    let now = std::time::Instant::now();
    let mut cache = lock(&CONVENTIONS);
    if let Some(read) = &*cache
        && now.duration_since(read.at) < CONVENTIONS_REREAD
    {
        return read.current;
    }
    let fresh = read_conventions();
    let current: &'static crate::Conventions = match &*cache {
        Some(read) if *read.current == fresh => read.current,
        _ => Box::leak(Box::new(fresh)),
    };
    *cache = Some(ReadConventions { at: now, current });
    current
}

/// 读一次系统设置。读不到的项用默认设置（[`crate::Conventions::WINDOWS`]）的
fn read_conventions() -> crate::Conventions {
    use windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SM_CXDOUBLECLK, SM_CXDRAG, SM_CYDOUBLECLK, SM_CYDRAG, SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES, SystemParametersInfoW,
        USER_DEFAULT_SCREEN_DPI,
    };
    /// "一次滚动一个屏幕"时"一次滚动的行数"是这个值（`WHEEL_PAGESCROLL`，`UINT_MAX`）
    const PAGE_SCROLL: u32 = u32::MAX;
    let defaults = crate::Conventions::WINDOWS;
    // 按 96 DPI 问，得到的就是逻辑像素
    // SAFETY: 只读系统度量，没有指针
    let metric = |index| unsafe { GetSystemMetricsForDpi(index, USER_DEFAULT_SCREEN_DPI) };
    let parameter = |action| {
        let mut value: u32 = 0;
        // SAFETY: 这两项往 pvparam 写一个 UINT，指向栈上的 u32
        let ok = unsafe { SystemParametersInfoW(action, 0, (&raw mut value).cast(), 0) };
        (ok != 0).then_some(value)
    };
    let lines = parameter(SPI_GETWHEELSCROLLLINES);
    // 一行是默认一格（三行）的三分之一；先乘再除，默认的三行正好是默认的一格
    let notch = |count: Option<u32>, default: f32| match count {
        Some(n) if n != PAGE_SCROLL => n as f32 * defaults.wheel_line_height / 3.0,
        _ => default,
    };
    let wheel_notch = [notch(parameter(SPI_GETWHEELSCROLLCHARS), defaults.wheel_notch[0]), notch(lines, defaults.wheel_notch[1])];
    // SAFETY: 没有参数，只读系统设置
    let interval = unsafe { GetDoubleClickTime() };
    let [width, height] = [metric(SM_CXDOUBLECLK), metric(SM_CYDOUBLECLK)];
    // 双击范围是以第一次按下为中心的矩形，取外接圆：半径是对角线的一半
    let double_click_distance =
        if width > 0 && height > 0 { ((width * width + height * height) as f32).sqrt() / 2.0 } else { defaults.double_click_distance };
    // 拖动阈值是"按下处往每边各能挪几个像素"
    let drag = metric(SM_CXDRAG).max(metric(SM_CYDRAG));
    crate::Conventions {
        wheel_notch,
        wheel_scrolls_pages: lines == Some(PAGE_SCROLL),
        double_click_interval: if interval > 0 { std::time::Duration::from_millis(interval.into()) } else { defaults.double_click_interval },
        double_click_distance,
        drag_threshold: if drag > 0 { drag as f32 } else { defaults.drag_threshold },
        ..defaults
    }
}

/// 输入法的上下文（`HIMC`）
type Himc = *mut c_void;

// 输入法（IMM32，系统自带的 imm32.dll）。windows-sys 的这一组要另开特性，这里只用三个函数，自己声明
#[link(name = "imm32")]
unsafe extern "system" {
    fn ImmGetContext(hwnd: windows_sys::Win32::Foundation::HWND) -> Himc;
    fn ImmReleaseContext(hwnd: windows_sys::Win32::Foundation::HWND, himc: Himc) -> windows_sys::core::BOOL;
    fn ImmNotifyIME(himc: Himc, action: u32, index: u32, value: u32) -> windows_sys::core::BOOL;
}

/// 让窗口的输入法放弃正在组的字（`ImmNotifyIME(NI_COMPOSITIONSTR, CPS_CANCEL)`）：没确定的字丢掉、候选框收起。
/// 返回通知到了没有（窗口没有输入法的上下文时是 `false`）。在窗口的线程上调用
pub fn cancel_ime_composition(window: &dyn HasWindowHandle) -> bool {
    const NI_COMPOSITIONSTR: u32 = 0x0015;
    const CPS_CANCEL: u32 = 0x0004;
    let Some(hwnd) = hwnd(window) else { return false };
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    // SAFETY: `hwnd` 是活着的窗口；拿到的上下文在同一个块里还回去
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_null() {
            return false;
        }
        let ok = ImmNotifyIME(himc, NI_COMPOSITIONSTR, CPS_CANCEL, 0) != 0;
        ImmReleaseContext(hwnd, himc);
        ok
    }
}
