//! 文件对话框：选择文件、文件夹、保存位置。真正弹出由系统那一侧做（[`crate::host::SystemBackend::file_dialog`]）。

use std::future::Future;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// 文件对话框的种类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileDialogKind {
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

/// 交给系统那一侧的请求
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDialogRequest {
    /// 种类
    pub kind: FileDialogKind,
    /// 打开时能否多选
    pub multiple: bool,
    /// 标题
    pub title: Option<String>,
    /// 一开始显示的文件夹
    pub directory: Option<PathBuf>,
    /// 保存时默认的文件名
    pub file_name: Option<String>,
    /// 允许的文件类型，空表示不限
    pub filters: Vec<FileFilter>,
}

/// 对话框完成时的回调：用户选好的路径，取消时是空列表。可能在别的线程调用
pub type FileDialogDone = Box<dyn FnOnce(Vec<PathBuf>) + Send>;

/// 系统的文件对话框，异步：对话框开着时界面照常运行和动画（不是阻塞的模态循环）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::FileDialog;
/// let content = signal(String::new());
/// button("打开…").on_click(move || {
///     aureole::spawn_local(async move {
///         let Some(path) = FileDialog::new().title("选择文本文件").file_filter("文本", &["txt", "md"]).pick_file().await else {
///             return; // 取消了
///         };
///         let text = aureole::spawn_blocking(move || std::fs::read_to_string(path).unwrap_or_default()).await;
///         content.set(text);
///     });
/// });
/// ```
///
/// - 调用 `pick_*` / `save_file` 时马上弹出，返回的 future 在用户选好或取消时完成（取消是 `None` / 空列表）。
///   要在主线程调用（处理器、[`crate::spawn_local`] 里），future 也只能在主线程等（不是 `Send`）。
/// - 在窗口里调用时作为这个窗口的表单（macOS 的 sheet）弹出，否则是独立的面板。
/// - macOS 是 NSOpenPanel / NSSavePanel；Windows 是系统的文件对话框（在自己的线程里，窗口暂时不能操作）；
///   Linux 走 XDG 门户（GNOME、KDE 的对话框），没有门户时用 zenity / kdialog，都没有时马上返回空结果。
/// - 测试工具里不弹出：`Harness::file_dialogs` 看请求，`Harness::respond_file_dialog` 给结果。
#[derive(Clone, Debug)]
pub struct FileDialog {
    title: Option<String>,
    directory: Option<PathBuf>,
    file_name: Option<String>,
    filters: Vec<FileFilter>,
}

impl Default for FileDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl FileDialog {
    /// 默认设置的对话框：没有标题、起始目录和文件类型限制
    pub fn new() -> Self {
        Self { title: None, directory: None, file_name: None, filters: Vec::new() }
    }

    /// 标题（新版 macOS 显示为面板顶部的提示）
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// 一开始显示的文件夹
    pub fn directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    /// 保存时默认的文件名
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    /// 只允许这些扩展名（不带点），可以多次调用。`name` 是给人看的分类名：
    /// Windows、Linux 的对话框按分类显示（多于一类时最前面加一类"所有支持的文件"）；
    /// macOS 的面板不显示分类，所有分类的扩展名合在一起
    pub fn file_filter(mut self, name: &str, extensions: &[&str]) -> Self {
        self.filters.push(FileFilter { name: name.to_owned(), extensions: extensions.iter().map(|e| e.to_string()).collect() });
        self
    }

    /// 选一个文件
    pub fn pick_file(self) -> impl Future<Output = Option<PathBuf>> {
        let dialog = self.show(FileDialogKind::OpenFile, false);
        async move { dialog.await.into_iter().next() }
    }

    /// 选多个文件
    pub fn pick_files(self) -> impl Future<Output = Vec<PathBuf>> {
        self.show(FileDialogKind::OpenFile, true)
    }

    /// 选一个文件夹
    pub fn pick_directory(self) -> impl Future<Output = Option<PathBuf>> {
        let dialog = self.show(FileDialogKind::OpenDirectory, false);
        async move { dialog.await.into_iter().next() }
    }

    /// 选保存位置（文件已存在时系统会问要不要替换）
    pub fn save_file(self) -> impl Future<Output = Option<PathBuf>> {
        let dialog = self.show(FileDialogKind::Save, false);
        async move { dialog.await.into_iter().next() }
    }

    fn show(self, kind: FileDialogKind, multiple: bool) -> Dialog {
        let request = FileDialogRequest { kind, multiple, title: self.title, directory: self.directory, file_name: self.file_name, filters: self.filters };
        let state = Arc::new(Mutex::new(State::default()));
        let s = state.clone();
        // Windows、Linux 上在对话框自己的线程里调用：只放下结果、唤醒等它的任务，回到主线程再继续
        let done: FileDialogDone = Box::new(move |paths: Vec<PathBuf>| {
            let waker = {
                let mut s = lock(&s);
                s.result = Some(paths);
                s.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        });
        crate::system::show_file_dialog(request, done);
        Dialog { state, _main_thread: PhantomData }
    }
}

#[derive(Default)]
struct State {
    result: Option<Vec<PathBuf>>,
    waker: Option<Waker>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用户关掉对话框时完成
struct Dialog {
    state: Arc<Mutex<State>>,
    /// 只在主线程等（不是 `Send`）：结果虽然从别的线程交回来，等它的任务要留在主线程
    _main_thread: PhantomData<*const ()>,
}

impl Future for Dialog {
    type Output = Vec<PathBuf>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Vec<PathBuf>> {
        let mut state = lock(&self.state);
        match state.result.take() {
            Some(paths) => Poll::Ready(paths),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
