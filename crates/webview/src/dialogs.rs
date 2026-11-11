//! 页面要应用出面的事：对话框（`alert`、`confirm`、`prompt`、离开页面前的确认）、选文件、下载、权限申请。
//!
//! 每种都能交给应用（控制器上的 `on_dialog`、`on_file_chooser`、`on_download`、`on_permission`），在主线程上调用，
//! 拿到的对象答复一次（答复之前可以先显示自己的界面，过一会儿再答）；对象丢掉时按"取消、拒绝"答复。应用没接手时：
//!
//! - 对话框：Windows 上用系统的对话框（消息框；`prompt` 是带输入框的小对话框），在单独的线程上弹，界面照常画，
//!   应用此刻的活动窗口（应用不在前台时是它自己的一个窗口）在对话框开着时不能操作。别的平台上 `alert` 直接关掉，别的按取消；
//! - 选文件：框架的系统文件对话框（[`aureole::FileDialog`]）；
//! - 下载：问存到哪里（同一个文件对话框），起始在用户的下载文件夹；
//! - 权限：拒绝。
//!
//! CEF 的回调在它的界面线程上（Windows 上是 CEF 自己的线程），交给应用的都投递到主线程；答复用的 CEF 对象在哪个线程上都能调。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use std::os::raw::c_int;
use std::path::PathBuf;
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::sys::{cef_file_dialog_mode_t, cef_jsdialog_type_t, cef_permission_request_result_t};
use cef::{
    BeforeDownloadCallback, Browser, CefString, CefStringList, DialogHandler, DownloadHandler, DownloadItem, DownloadItemCallback, FileDialogCallback,
    FileDialogMode, Frame, ImplBeforeDownloadCallback, ImplDialogHandler, ImplDownloadHandler, ImplDownloadItem, ImplDownloadItemCallback,
    ImplFileDialogCallback, ImplJsdialogCallback, ImplJsdialogHandler, ImplMediaAccessCallback, ImplPermissionHandler, ImplPermissionPromptCallback,
    JsdialogCallback, JsdialogHandler, JsdialogType, MediaAccessCallback, PermissionHandler, PermissionPromptCallback, PermissionRequestResult,
    WrapDialogHandler, WrapDownloadHandler, WrapJsdialogHandler, WrapPermissionHandler, wrap_dialog_handler, wrap_download_handler,
    wrap_jsdialog_handler, wrap_permission_handler,
};

use crate::state::{Hook, Page, Shared};

// ---------- 对话框 ----------

/// 页面要弹的对话框是哪一种
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialogKind {
    /// `alert`：只有确定
    Alert,
    /// `confirm`：确定、取消
    Confirm,
    /// `prompt`：输入一段文字，确定、取消
    Prompt,
    /// 离开页面前的确认（页面登记了 `beforeunload`）；`reload` 是重新加载，不是离开
    BeforeUnload {
        /// 是重新加载
        reload: bool,
    },
}

/// 页面要弹的一个对话框（[`crate::WebController::on_dialog`]）。答复一次：[`Dialog::accept`]、[`Dialog::cancel`]；
/// 丢掉没答复的按取消。页面在等答复（脚本停在那里）
pub struct Dialog {
    kind: DialogKind,
    message: String,
    default_text: String,
    origin: String,
    callback: Option<JsdialogCallback>,
}

impl Dialog {
    /// 哪一种
    pub fn kind(&self) -> &DialogKind {
        &self.kind
    }

    /// 页面给的文字（离开页面前的确认时一般是空的：浏览器内核不再用页面给的文字）
    pub fn message(&self) -> &str {
        &self.message
    }

    /// `prompt` 输入框里一开始的文字
    pub fn default_text(&self) -> &str {
        &self.default_text
    }

    /// 弹对话框的页面的地址（来源），显示"某某网站说"时用
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// 按确定（离开页面前的确认：离开）。`prompt` 的输入是 `text`（`None` 时是空的）
    pub fn accept(mut self, text: Option<&str>) {
        if let Some(callback) = self.callback.take() {
            callback.cont(1, Some(&CefString::from(text.unwrap_or_default())));
        }
    }

    /// 按取消（`alert` 也一样是关掉；离开页面前的确认：留下）
    pub fn cancel(mut self) {
        if let Some(callback) = self.callback.take() {
            callback.cont(0, None);
        }
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            callback.cont(0, None);
        }
    }
}

/// 应用没接手时的对话框：Windows 上系统的对话框（单独的线程上），macOS 上挂在窗口上的 NSAlert，别的平台上直接关掉
fn default_dialog(dialog: Dialog) {
    #[cfg(target_os = "windows")]
    {
        let owner = windows::owner();
        std::thread::spawn(move || {
            let title = title(&dialog.origin);
            match dialog.kind {
                DialogKind::Alert => {
                    windows::message(owner, &title, &dialog.message, false);
                    dialog.accept(None);
                }
                DialogKind::Confirm => {
                    if windows::message(owner, &title, &dialog.message, true) {
                        dialog.accept(None)
                    } else {
                        dialog.cancel()
                    }
                }
                DialogKind::BeforeUnload { reload } => {
                    let text = if reload { "重新加载这个页面？你填的内容可能不会保存。" } else { "离开这个页面？你填的内容可能不会保存。" };
                    if windows::message(owner, &title, text, true) {
                        dialog.accept(None)
                    } else {
                        dialog.cancel()
                    }
                }
                DialogKind::Prompt => match windows::prompt(owner, &title, &dialog.message, &dialog.default_text) {
                    Some(text) => dialog.accept(Some(&text)),
                    None => dialog.cancel(),
                },
            }
        });
    }
    #[cfg(target_os = "macos")]
    {
        let title = title(&dialog.origin);
        let (message, cancellable, input) = match &dialog.kind {
            DialogKind::Alert => (dialog.message.clone(), false, None),
            DialogKind::Confirm => (dialog.message.clone(), true, None),
            DialogKind::Prompt => (dialog.message.clone(), true, Some(dialog.default_text.clone())),
            DialogKind::BeforeUnload { reload } => {
                let text = if *reload { "重新加载这个页面？你填的内容可能不会保存。" } else { "离开这个页面？你填的内容可能不会保存。" };
                (text.to_owned(), true, None)
            }
        };
        let prompt = input.is_some();
        macos::show(&title, &message, cancellable, input.as_deref(), move |answer| match answer {
            Some(text) => dialog.accept(prompt.then_some(text.as_str())),
            None => dialog.cancel(),
        });
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    match dialog.kind {
        DialogKind::Alert => dialog.accept(None),
        _ => dialog.cancel(),
    }
}

/// 系统对话框的标题：页面的来源（主机名），没有时是"页面"
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn title(origin: &str) -> String {
    let host = origin.split_once("://").map_or("", |(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
    if host.is_empty() { "页面".to_owned() } else { host.to_owned() }
}

fn show_dialog(shared: &Shared, dialog: Dialog) {
    match shared.hooks.borrow().dialog.clone() {
        Some(handler) => handler(dialog),
        None => default_dialog(dialog),
    }
}

wrap_jsdialog_handler! {
    struct PageDialogs {
        page: Arc<Page>,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut c_int>,
        ) -> c_int {
            let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
            let kind = if dialog_type == JsdialogType::from(cef_jsdialog_type_t::JSDIALOGTYPE_CONFIRM) {
                DialogKind::Confirm
            } else if dialog_type == JsdialogType::from(cef_jsdialog_type_t::JSDIALOGTYPE_PROMPT) {
                DialogKind::Prompt
            } else {
                DialogKind::Alert
            };
            let dialog = Dialog { kind, message: text(message_text), default_text: text(default_prompt_text), origin: text(origin_url), callback: Some(callback) };
            self.page.on_main(move |shared| show_dialog(shared, dialog));
            1
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            is_reload: c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> c_int {
            let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
            let page = self.page.clone();
            let dialog = Dialog {
                kind: DialogKind::BeforeUnload { reload: is_reload != 0 },
                message: text(message_text),
                default_text: String::new(),
                origin: String::new(),
                callback: Some(callback),
            };
            page.on_main(move |shared| {
                let mut dialog = dialog;
                dialog.origin = shared.url.get_untracked();
                show_dialog(shared, dialog);
            });
            1
        }
    }
}

// ---------- 选文件 ----------

/// 页面要选什么（`<input type=file>`，`multiple`，`webkitdirectory`；下载时问存到哪里）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileChooserMode {
    /// 选一个文件
    Open,
    /// 选多个文件
    OpenMultiple,
    /// 选一个文件夹
    OpenFolder,
    /// 选保存的位置
    Save,
}

/// 页面要选文件（[`crate::WebController::on_file_chooser`]）。答复一次：[`FileChooser::select`]、[`FileChooser::cancel`]；
/// 丢掉没答复的按取消
pub struct FileChooser {
    mode: FileChooserMode,
    title: String,
    default_path: String,
    accept: Vec<String>,
    extensions: Vec<Vec<String>>,
    descriptions: Vec<String>,
    callback: Option<FileDialogCallback>,
}

impl FileChooser {
    /// 选什么
    pub fn mode(&self) -> FileChooserMode {
        self.mode
    }

    /// 标题（页面给的，一般是空的）
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 一开始的文件名或路径（保存时是建议的文件名）
    pub fn default_path(&self) -> &str {
        &self.default_path
    }

    /// 页面接受的类型（`accept` 属性）：扩展名（`.png`）或内容类型（`image/*`）
    pub fn accept(&self) -> &[String] {
        &self.accept
    }

    /// 每种接受的类型对应的扩展名（不带点；内容类型换算出来的，没有时是空的）
    pub fn extensions(&self) -> &[Vec<String>] {
        &self.extensions
    }

    /// 选好了：`paths` 交给页面（取消时用 [`FileChooser::cancel`]）
    pub fn select(mut self, paths: &[PathBuf]) {
        if let Some(callback) = self.callback.take() {
            let mut list = CefStringList::new();
            for path in paths {
                list.append(&path.to_string_lossy());
            }
            callback.cont(Some(&mut list));
        }
    }

    /// 不选了
    pub fn cancel(mut self) {
        if let Some(callback) = self.callback.take() {
            callback.cancel();
        }
    }
}

impl Drop for FileChooser {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            callback.cancel();
        }
    }
}

/// 应用没接手时：框架的系统文件对话框
fn default_file_chooser(chooser: FileChooser) {
    let mut dialog = aureole::FileDialog::new();
    if !chooser.title.is_empty() {
        dialog = dialog.title(chooser.title.clone());
    }
    let default = PathBuf::from(&chooser.default_path);
    match chooser.mode {
        FileChooserMode::Save => {
            let name = default.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !name.is_empty() {
                dialog = dialog.file_name(name);
            }
            let directory = default.parent().filter(|p| p.is_absolute()).map(PathBuf::from).or_else(downloads_dir);
            if let Some(directory) = directory {
                dialog = dialog.directory(directory);
            }
        }
        _ => {
            if default.is_absolute() && default.is_dir() {
                dialog = dialog.directory(default.clone());
            }
        }
    }
    for (index, extensions) in chooser.extensions.iter().enumerate().filter(|(_, e)| !e.is_empty()) {
        let name = chooser.descriptions.get(index).filter(|d| !d.is_empty()).or_else(|| chooser.accept.get(index)).cloned().unwrap_or_default();
        let extensions: Vec<&str> = extensions.iter().map(String::as_str).collect();
        dialog = dialog.file_filter(&name, &extensions);
    }
    aureole::spawn_local(async move {
        let paths = match chooser.mode {
            FileChooserMode::Open => dialog.pick_file().await.into_iter().collect(),
            FileChooserMode::OpenMultiple => dialog.pick_files().await,
            FileChooserMode::OpenFolder => dialog.pick_directory().await.into_iter().collect(),
            FileChooserMode::Save => dialog.save_file().await.into_iter().collect::<Vec<_>>(),
        };
        if paths.is_empty() { chooser.cancel() } else { chooser.select(&paths) }
    });
}

/// 用户的下载文件夹（Windows 上是 `%USERPROFILE%\Downloads`）
fn downloads_dir() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(target_os = "windows") { "USERPROFILE" } else { "HOME" })?;
    Some(PathBuf::from(home).join("Downloads")).filter(|p| p.is_dir())
}

/// CEF 给的扩展名列表（`".png;.jpg"` 这样分号隔开的）换成不带点的
fn split_extensions(text: &str) -> Vec<String> {
    text.split(';').map(|e| e.trim().trim_start_matches('.').to_owned()).filter(|e| !e.is_empty()).collect()
}

wrap_dialog_handler! {
    struct PageFiles {
        page: Arc<Page>,
    }

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            accept_filters: Option<&mut CefStringList>,
            accept_extensions: Option<&mut CefStringList>,
            accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> c_int {
            let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
            let list = |list: Option<&mut CefStringList>| -> Vec<String> { list.map(|l| l.clone().into_iter().collect()).unwrap_or_default() };
            let mode = if mode == FileDialogMode::from(cef_file_dialog_mode_t::FILE_DIALOG_OPEN_MULTIPLE) {
                FileChooserMode::OpenMultiple
            } else if mode == FileDialogMode::from(cef_file_dialog_mode_t::FILE_DIALOG_OPEN_FOLDER) {
                FileChooserMode::OpenFolder
            } else if mode == FileDialogMode::from(cef_file_dialog_mode_t::FILE_DIALOG_SAVE) {
                FileChooserMode::Save
            } else {
                FileChooserMode::Open
            };
            let accept = list(accept_filters);
            let mut extensions: Vec<Vec<String>> = list(accept_extensions).iter().map(|e| split_extensions(e)).collect();
            // 只给了扩展名（`.png`）没给换算时，扩展名本身就是
            extensions.resize(accept.len(), Vec::new());
            for (filter, extensions) in accept.iter().zip(extensions.iter_mut()) {
                if extensions.is_empty() && filter.starts_with('.') {
                    *extensions = split_extensions(filter);
                }
            }
            let chooser = FileChooser {
                mode,
                title: text(title),
                default_path: text(default_file_path),
                accept,
                extensions,
                descriptions: list(accept_descriptions),
                callback: Some(callback),
            };
            self.page.on_main(move |shared| match shared.hooks.borrow().file_chooser.clone() {
                Some(handler) => handler(chooser),
                None => default_file_chooser(chooser),
            });
            1
        }
    }
}

// ---------- 下载 ----------

/// 页面开始了一个下载，问存到哪里（[`crate::WebController::on_download`]）。答复一次：[`DownloadRequest::save_to`]、
/// [`DownloadRequest::ask`]、[`DownloadRequest::cancel`]；丢掉没答复的不下载
pub struct DownloadRequest {
    id: u32,
    url: String,
    suggested_name: String,
    mime_type: String,
    total: Option<u64>,
    callback: Option<BeforeDownloadCallback>,
}

impl DownloadRequest {
    /// 这个下载的编号（[`crate::WebController::downloads`] 里的 [`Download::id`]）
    pub fn id(&self) -> u32 {
        self.id
    }

    /// 下载的地址
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 建议的文件名（服务器给的、地址里的）
    pub fn suggested_name(&self) -> &str {
        &self.suggested_name
    }

    /// 内容类型
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// 总大小（字节）；服务器没说时为 `None`
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// 存到 `path`（不再问）
    pub fn save_to(mut self, path: impl Into<PathBuf>) {
        if let Some(callback) = self.callback.take() {
            callback.cont(Some(&CefString::from(path.into().to_string_lossy().as_ref())), 0);
        }
    }

    /// 弹文件对话框问存到哪里（起始在下载文件夹，文件名是建议的）
    pub fn ask(mut self) {
        if let Some(callback) = self.callback.take() {
            let path = downloads_dir().map(|dir| dir.join(&self.suggested_name)).unwrap_or_else(|| PathBuf::from(&self.suggested_name));
            callback.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 1);
        }
    }

    /// 不下载
    pub fn cancel(mut self) {
        // 不执行回调就放掉，CEF 取消这个下载
        drop(self.callback.take());
    }
}

/// 一个下载此刻的样子（[`crate::WebController::downloads`]）
#[derive(Clone, Debug, PartialEq)]
pub struct Download {
    /// 编号
    pub id: u32,
    /// 地址
    pub url: String,
    /// 存在哪（定下来以前是空的）
    pub path: PathBuf,
    /// 收到了多少字节
    pub received: u64,
    /// 总大小；服务器没说时为 `None`
    pub total: Option<u64>,
    /// 进行到哪一步
    pub state: DownloadState,
}

/// 下载进行到哪一步
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DownloadState {
    /// 在下
    InProgress,
    /// 暂停着
    Paused,
    /// 下完了
    Complete,
    /// 取消了
    Canceled,
    /// 出错停了（网络断了、写不进去……）
    Interrupted,
}

fn download_of(item: &DownloadItem) -> Download {
    let total = item.total_bytes();
    let state = if item.is_complete() != 0 {
        DownloadState::Complete
    } else if item.is_canceled() != 0 {
        DownloadState::Canceled
    } else if item.is_interrupted() != 0 {
        DownloadState::Interrupted
    } else if item.is_paused() != 0 {
        DownloadState::Paused
    } else {
        DownloadState::InProgress
    };
    Download {
        id: item.id(),
        url: CefString::from(&item.url()).to_string(),
        path: PathBuf::from(CefString::from(&item.full_path()).to_string()),
        received: item.received_bytes().max(0) as u64,
        total: (total > 0).then_some(total as u64),
        state,
    }
}

wrap_download_handler! {
    struct PageDownloads {
        page: Arc<Page>,
    }

    impl DownloadHandler {
        fn can_download(&self, _browser: Option<&mut Browser>, _url: Option<&CefString>, _request_method: Option<&CefString>) -> c_int {
            1
        }

        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> c_int {
            let (Some(item), Some(callback)) = (download_item, callback.map(|c| c.clone())) else { return 0 };
            let total = item.total_bytes();
            let request = DownloadRequest {
                id: item.id(),
                url: CefString::from(&item.url()).to_string(),
                suggested_name: text(suggested_name),
                mime_type: CefString::from(&item.mime_type()).to_string(),
                total: (total > 0).then_some(total as u64),
                callback: Some(callback),
            };
            let download = download_of(item);
            self.page.on_main(move |shared| {
                update_download(shared, download);
                match shared.hooks.borrow().download.clone() {
                    Some(handler) => handler(request),
                    None => request.ask(),
                }
            });
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let download = download_of(item);
            let finished = matches!(download.state, DownloadState::Complete | DownloadState::Canceled);
            self.page.downloads(|controls| {
                if finished {
                    controls.remove(&download.id);
                } else if let Some(callback) = callback {
                    controls.insert(download.id, callback.clone());
                }
            });
            self.page.on_main(move |shared| update_download(shared, download));
        }
    }
}

/// 下载列表里换上这个下载此刻的样子
fn update_download(shared: &Shared, download: Download) {
    shared.downloads.update(|list| match list.iter_mut().find(|d| d.id == download.id) {
        Some(old) => *old = download,
        None => list.push(download),
    });
}

/// 取消、暂停、继续一个下载（控制器的方法）
pub(crate) fn control_download(page: &Page, id: u32, action: DownloadAction) {
    page.downloads(|controls| {
        if let Some(callback) = controls.get(&id) {
            match action {
                DownloadAction::Cancel => callback.cancel(),
                DownloadAction::Pause => callback.pause(),
                DownloadAction::Resume => callback.resume(),
            }
        }
    });
}

/// 对一个下载做什么
#[derive(Clone, Copy)]
pub(crate) enum DownloadAction {
    Cancel,
    Pause,
    Resume,
}

/// 能取消、暂停、继续的下载的回调（在 [`Page`] 里，跨线程）
pub(crate) type DownloadControls = std::collections::HashMap<u32, DownloadItemCallback>;

// ---------- 权限 ----------

/// 页面申请的一种权限
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Permission {
    /// 摄像头
    Camera,
    /// 麦克风
    Microphone,
    /// 录屏幕（画面或者声音）
    ScreenCapture,
    /// 位置
    Geolocation,
    /// 通知
    Notifications,
    /// 读剪贴板
    Clipboard,
    /// MIDI 设备（带系统独占消息）
    Midi,
    /// 别的（CEF 的权限类型的位）
    Other(u32),
}

/// 页面申请权限（[`crate::WebController::on_permission`]）。答复一次：[`PermissionRequest::allow`]、[`PermissionRequest::deny`]；
/// 丢掉没答复的按拒绝
pub struct PermissionRequest {
    origin: String,
    permissions: Vec<Permission>,
    answer: Option<Answer>,
}

enum Answer {
    /// 摄像头、麦克风、录屏幕：给的时候答复申请了的那些位
    Media(MediaAccessCallback, u32),
    /// 别的权限：一次答复
    Prompt(PermissionPromptCallback),
}

impl PermissionRequest {
    /// 申请的页面的来源
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// 申请了哪些（一次可能申请好几种，比如摄像头和麦克风）
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    /// 给
    pub fn allow(mut self) {
        match self.answer.take() {
            Some(Answer::Media(callback, requested)) => callback.cont(requested),
            Some(Answer::Prompt(callback)) => callback.cont(PermissionRequestResult::from(cef_permission_request_result_t::CEF_PERMISSION_RESULT_ACCEPT)),
            None => {}
        }
    }

    /// 不给
    pub fn deny(mut self) {
        deny(self.answer.take());
    }
}

impl Drop for PermissionRequest {
    fn drop(&mut self) {
        deny(self.answer.take());
    }
}

fn deny(answer: Option<Answer>) {
    match answer {
        Some(Answer::Media(callback, _)) => callback.cancel(),
        Some(Answer::Prompt(callback)) => callback.cont(PermissionRequestResult::from(cef_permission_request_result_t::CEF_PERMISSION_RESULT_DENY)),
        None => {}
    }
}

/// 摄像头、麦克风、录屏幕的位换成权限
fn media_permissions(bits: u32) -> Vec<Permission> {
    let mut out = Vec::new();
    if bits & 2 != 0 {
        out.push(Permission::Camera);
    }
    if bits & 1 != 0 {
        out.push(Permission::Microphone);
    }
    if bits & (4 | 8) != 0 {
        out.push(Permission::ScreenCapture);
    }
    out
}

/// 别的权限的位换成权限（不认识的每一位一个 [`Permission::Other`]）
fn prompt_permissions(bits: u32) -> Vec<Permission> {
    const KNOWN: [(u32, Permission); 6] = [
        (4, Permission::Camera),
        (4096, Permission::Microphone),
        (256, Permission::Geolocation),
        (32768, Permission::Notifications),
        (16, Permission::Clipboard),
        (8192, Permission::Midi),
    ];
    let mut out = Vec::new();
    let mut rest = bits;
    for (bit, permission) in KNOWN {
        if bits & bit != 0 {
            out.push(permission);
            rest &= !bit;
        }
    }
    for shift in 0..32 {
        if rest & (1 << shift) != 0 {
            out.push(Permission::Other(1 << shift));
        }
    }
    out
}

wrap_permission_handler! {
    struct PagePermissions {
        page: Arc<Page>,
    }

    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> c_int {
            let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
            if !self.page.has_hook(Hook::Permission) {
                callback.cancel();
                return 1;
            }
            let request = PermissionRequest {
                origin: text(requesting_origin),
                permissions: media_permissions(requested_permissions),
                answer: Some(Answer::Media(callback, requested_permissions)),
            };
            self.page.on_main(move |shared| ask_permission(shared, request));
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> c_int {
            let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
            if !self.page.has_hook(Hook::Permission) {
                deny(Some(Answer::Prompt(callback)));
                return 1;
            }
            let request = PermissionRequest {
                origin: text(requesting_origin),
                permissions: prompt_permissions(requested_permissions),
                answer: Some(Answer::Prompt(callback)),
            };
            self.page.on_main(move |shared| ask_permission(shared, request));
            1
        }
    }
}

// ---------- 认证、证书错误 ----------

/// 网站要用户名和密码（[`crate::WebController::on_auth`]）。答复一次：[`AuthRequest::login`]、[`AuthRequest::cancel`]；
/// 丢掉没答复的按取消。请求在等答复
pub struct AuthRequest {
    url: String,
    host: String,
    port: u16,
    realm: String,
    scheme: String,
    proxy: bool,
    callback: Option<cef::AuthCallback>,
}

impl AuthRequest {
    /// 要认证的地址（来源）
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 要认证的主机和端口（代理认证时是代理的）
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 端口
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 网站给的说明（"领域"），显示给用户看是哪个系统要登录
    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// 认证的方式（`basic`、`digest`、`ntlm`……）
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// 是代理要认证
    pub fn proxy(&self) -> bool {
        self.proxy
    }

    /// 用 `user`、`password` 登录
    pub fn login(mut self, user: &str, password: &str) {
        if let Some(callback) = self.callback.take() {
            cef::ImplAuthCallback::cont(&callback, Some(&CefString::from(user)), Some(&CefString::from(password)));
        }
    }

    /// 不登录（页面显示没有权限）
    pub fn cancel(mut self) {
        if let Some(callback) = self.callback.take() {
            cef::ImplAuthCallback::cancel(&callback);
        }
    }
}

impl Drop for AuthRequest {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            cef::ImplAuthCallback::cancel(&callback);
        }
    }
}

/// CEF 的界面线程上：网站要认证。应用接手时交给它（返回 1，之后答复）；不接手时返回 0，内核取消
pub(crate) fn auth(page: &Arc<Page>, url: String, proxy: bool, host: String, port: c_int, realm: String, scheme: String, callback: Option<&mut cef::AuthCallback>) -> c_int {
    let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
    if !page.has_hook(Hook::Auth) {
        return 0;
    }
    let request = AuthRequest { url, host, port: u16::try_from(port).unwrap_or(0), realm, scheme, proxy, callback: Some(callback) };
    page.on_main(move |shared| {
        let hook = shared.hooks.borrow().auth.clone();
        if let Some(hook) = hook {
            hook(request);
        }
    });
    1
}

/// 网站的证书有问题（[`crate::WebController::on_certificate_error`]）。答复一次：[`CertificateError::proceed`]、
/// [`CertificateError::cancel`]；丢掉没答复的按取消
pub struct CertificateError {
    url: String,
    code: i32,
    message: &'static str,
    callback: Option<cef::Callback>,
}

impl CertificateError {
    /// 要打开的地址
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 内核的错误码（负数，和 Chromium 的 `net::Error` 一样）
    pub fn code(&self) -> i32 {
        self.code
    }

    /// 出了什么问题，给用户看的一句话
    pub fn message(&self) -> &str {
        self.message
    }

    /// 还是打开（用户明白风险时）
    pub fn proceed(mut self) {
        if let Some(callback) = self.callback.take() {
            cef::ImplCallback::cont(&callback);
        }
    }

    /// 不打开（页面显示证书错误）
    pub fn cancel(mut self) {
        if let Some(callback) = self.callback.take() {
            cef::ImplCallback::cancel(&callback);
        }
    }
}

impl Drop for CertificateError {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.take() {
            cef::ImplCallback::cancel(&callback);
        }
    }
}

/// 证书错误给用户看的话
fn certificate_message(code: cef::Errorcode) -> &'static str {
    use cef::Errorcode as E;
    match code {
        E::CERT_COMMON_NAME_INVALID | E::CERT_NAME_CONSTRAINT_VIOLATION => "证书和网站的域名对不上",
        E::CERT_DATE_INVALID | E::CERT_VALIDITY_TOO_LONG => "证书过期了，或者还没生效",
        E::CERT_AUTHORITY_INVALID => "证书不是受信任的机构签发的",
        E::CERT_REVOKED => "证书已经被吊销",
        E::CERT_WEAK_KEY | E::CERT_WEAK_SIGNATURE_ALGORITHM => "证书用的加密太弱",
        _ => "证书有问题",
    }
}

/// CEF 的界面线程上：证书有问题。应用接手时交给它（返回 1，之后答复）；不接手时返回 0，内核不打开
pub(crate) fn certificate_error(page: &Arc<Page>, code: cef::Errorcode, url: String, callback: Option<&mut cef::Callback>) -> c_int {
    let Some(callback) = callback.map(|c| c.clone()) else { return 0 };
    if !page.has_hook(Hook::CertificateError) {
        return 0;
    }
    let error = CertificateError { url, code: code.get_raw(), message: certificate_message(code), callback: Some(callback) };
    page.on_main(move |shared| {
        let hook = shared.hooks.borrow().certificate_error.clone();
        if let Some(hook) = hook {
            hook(error);
        }
    });
    1
}

/// 给网页的客户端：对话框、选文件、下载、权限的回调
pub(crate) fn handlers(page: &Arc<Page>) -> (JsdialogHandler, DialogHandler, DownloadHandler, PermissionHandler) {
    (PageDialogs::new(page.clone()), PageFiles::new(page.clone()), PageDownloads::new(page.clone()), PagePermissions::new(page.clone()))
}

fn ask_permission(shared: &Shared, request: PermissionRequest) {
    // 应用这期间撤了处理器：丢掉，按拒绝
    if let Some(handler) = shared.hooks.borrow().permission.clone() {
        handler(request);
    }
}

/// CEF 给的字符串，没有时是空的
fn text(value: Option<&CefString>) -> String {
    value.map(CefString::to_string).unwrap_or_default()
}
