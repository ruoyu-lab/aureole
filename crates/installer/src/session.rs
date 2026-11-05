//! 界面和引擎之间的一层：界面读它的信号、调它的方法，引擎在后台线程上跑。
//!
//! 默认的向导（[`Wizard`](crate::Wizard)）只用这里的公开接口。自己画界面时，这些就是全部要接的东西：
//!
//! ```no_run
//! use aureole::prelude::*;
//! use aureole_installer::{Session, Setup, State};
//!
//! fn my_ui(session: Session) -> Element {
//!     column()
//!         .gap(12)
//!         .child(text(format!("安装 {}", session.manifest().name)))
//!         .child(progress_bar(move || session.progress().get().fraction))
//!         .child(button("安装").on_click(move || session.start()))
//!         .child_when(move || session.state().get() == State::Done, move || button("完成").on_click(move || session.finish()))
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     Setup::new().run(my_ui)
//! }
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aureole::prelude::*;
use aureole::{FileDialog, Timer, every, signal};

use crate::engine::{Activity, InstallOptions, Installer, Progress, Receipt, Step, Uninstaller};
use crate::manifest::Manifest;
use crate::package::Package;
use crate::system::{default_directory, safe_file_name};
use crate::{CancelToken, Error};

/// 多久看一次后台线程的进度
const POLL: Duration = Duration::from_millis(33);

pub(crate) type StepFactory = Arc<dyn Fn() -> Box<dyn Step> + Send + Sync>;

/// 这个程序是来安装还是来卸载的
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 安装（或者升级）
    Install,
    /// 卸载
    Uninstall,
}

/// 进行到哪了
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum State {
    /// 还没开始
    Ready,
    /// 正在进行
    Running,
    /// 完成了
    Done,
    /// 用户取消了，已经撤销
    Cancelled,
    /// 失败了，已经撤销
    Failed {
        /// 原因，能直接显示
        message: String,
        /// 文件被占用（应用还开着）：关掉以后重试可能就好了
        file_in_use: bool,
    },
}

/// 后台线程交回来的：进度和结果
#[derive(Default)]
struct Mailbox {
    progress: Option<Progress>,
    result: Option<Result<(), Error>>,
}

/// 安装或卸载的一次会话。可以复制，复制的是句柄
#[derive(Clone, Copy)]
pub struct Session {
    mode: Mode,
    package: &'static Package,
    factories: &'static [StepFactory],
    directory: Signal<PathBuf>,
    menu_shortcut: Signal<bool>,
    desktop_shortcut: Signal<bool>,
    launch_after: Signal<bool>,
    register: Signal<bool>,
    state: Signal<State>,
    progress: Signal<Progress>,
    previous_version: Signal<Option<String>>,
    cancel: Signal<CancelToken>,
    timer: Signal<Option<Timer>>,
}

impl Session {
    /// 新的会话。`directory`：卸载时是装在哪里，安装时不写就是默认的目录，或者上次装的地方
    pub(crate) fn new(package: Package, mode: Mode, directory: Option<PathBuf>, factories: Vec<StepFactory>) -> Self {
        let directory = directory.unwrap_or_else(|| default_directory(package.manifest()));
        let previous = Receipt::read(&directory).ok().map(|receipt| receipt.manifest.version);
        let is_preview = package.is_preview();
        Self {
            mode,
            // 一个进程只有一次安装，留到进程结束
            package: Box::leak(Box::new(package)),
            factories: Box::leak(factories.into_boxed_slice()),
            directory: signal(directory),
            menu_shortcut: signal(true),
            desktop_shortcut: signal(true),
            launch_after: signal(true),
            // 预览的包没有文件：不碰系统
            register: signal(!is_preview),
            state: signal(State::Ready),
            progress: signal(Progress { fraction: 0.0, activity: Activity::Finishing }),
            previous_version: signal(previous),
            cancel: signal(CancelToken::default()),
            timer: signal(None),
        }
    }

    /// 写界面时预览、测试用：真的解压，但不碰系统（快捷方式、注册表）。`package` 可以是 [`Package::preview`]
    pub fn preview(package: Package, directory: impl Into<PathBuf>) -> Self {
        let session = Self::new(package, Mode::Install, Some(directory.into()), Vec::new());
        session.register.set(false);
        session
    }

    /// 预览卸载：`directory` 是装在哪里
    pub fn preview_uninstall(package: Package, directory: impl Into<PathBuf>) -> Self {
        Self::new(package, Mode::Uninstall, Some(directory.into()), Vec::new())
    }

    /// 安装还是卸载
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 清单：应用名、版本、发布者
    pub fn manifest(&self) -> &'static Manifest {
        self.package.manifest()
    }

    /// 装到哪里。可以直接改，也可以用 [`Self::choose_directory`]
    pub fn directory(&self) -> Signal<PathBuf> {
        self.directory
    }

    /// 开始菜单（Linux 的应用列表）里放快捷方式
    pub fn menu_shortcut(&self) -> Signal<bool> {
        self.menu_shortcut
    }

    /// 桌面上放快捷方式（Windows）
    pub fn desktop_shortcut(&self) -> Signal<bool> {
        self.desktop_shortcut
    }

    /// [`Self::finish`] 时打开应用
    pub fn launch_after(&self) -> Signal<bool> {
        self.launch_after
    }

    /// 进行到哪了
    pub fn state(&self) -> Signal<State> {
        self.state
    }

    /// 进度
    pub fn progress(&self) -> Signal<Progress> {
        self.progress
    }

    /// 这个目录里已经装的版本（升级时）
    pub fn previous_version(&self) -> Signal<Option<String>> {
        self.previous_version
    }

    /// 解压之后一共多大，比如 `19.1 MB`
    pub fn size_text(&self) -> String {
        format_size(self.manifest().total_size())
    }

    /// 弹出系统的对话框选目录。选的是放在哪个文件夹下：后面接上应用名
    pub fn choose_directory(self) {
        let name = safe_file_name(&self.manifest().name);
        let current = self.directory.get_untracked();
        aureole::spawn_local(async move {
            let start = current.parent().map(PathBuf::from).unwrap_or_else(|| current.clone());
            let Some(picked) = FileDialog::new().directory(start).pick_directory().await else { return };
            self.directory.set(with_app_folder(picked, &name));
            self.previous_version.set(Receipt::read(&self.directory.get_untracked()).ok().map(|receipt| receipt.manifest.version));
        });
    }

    /// 开始：在后台线程上跑，进度和结果写回信号。已经在跑、已经完成时什么也不做。失败、取消之后可以再调用，就是重试
    pub fn start(self) {
        if matches!(self.state.get_untracked(), State::Running | State::Done) {
            return;
        }
        let cancel = CancelToken::default();
        self.cancel.set(cancel.clone());
        self.state.set(State::Running);
        self.progress.set(Progress { fraction: 0.0, activity: Activity::Finishing });
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        self.spawn_worker(mailbox.clone(), cancel);
        // 不属于当前的作用域：一般是按钮的处理器调用的，状态一变那一页就卸载了，定时器不能跟着没
        let timer = aureole::detached(|| every(POLL, move || self.collect(&mailbox)));
        self.timer.set(Some(timer));
    }

    /// 请求取消：引擎停下、撤销之后状态变成 [`State::Cancelled`]。系统集成已经开始时可能取消不了
    pub fn cancel(self) {
        self.cancel.get_untracked().cancel();
    }

    /// 打开装好的应用
    pub fn launch(self) -> std::io::Result<()> {
        let directory = self.directory.get_untracked();
        std::process::Command::new(directory.join(&self.manifest().executable)).current_dir(directory).spawn().map(|_| ())
    }

    /// 结束：装好了并且 [`Self::launch_after`] 时打开应用，然后退出
    pub fn finish(self) {
        if self.mode == Mode::Install && self.state.get_untracked() == State::Done && self.launch_after.get_untracked() && let Err(error) = self.launch() {
            tracing_free_warn(&format!("打不开装好的应用：{error}"));
        }
        aureole::quit();
    }

    fn spawn_worker(self, mailbox: Arc<Mutex<Mailbox>>, cancel: CancelToken) {
        let directory = self.directory.get_untracked();
        let steps: Vec<Box<dyn Step>> = self.factories.iter().map(|make| make()).collect();
        let (mode, package) = (self.mode, self.package.clone());
        let options = InstallOptions {
            directory: directory.clone(),
            menu_shortcut: self.menu_shortcut.get_untracked(),
            desktop_shortcut: self.desktop_shortcut.get_untracked(),
            register: self.register.get_untracked(),
        };
        std::thread::spawn(move || {
            let report = |progress: Progress| lock(&mailbox).progress = Some(progress);
            let result = run_engine(mode, package, options, steps, &cancel, report);
            lock(&mailbox).result = Some(result);
        });
    }

    /// 主线程上看后台线程有什么交回来
    fn collect(self, mailbox: &Mutex<Mailbox>) {
        let (progress, result) = {
            let mut mailbox = lock(mailbox);
            (mailbox.progress.take(), mailbox.result.take())
        };
        if let Some(progress) = progress {
            self.progress.set(progress);
        }
        let Some(result) = result else { return };
        if let Some(timer) = self.timer.get_untracked() {
            timer.cancel();
        }
        self.timer.set(None);
        self.state.set(match result {
            Ok(()) => {
                self.progress.update(|p| p.fraction = 1.0);
                State::Done
            }
            Err(error) if error.is_cancelled() => State::Cancelled,
            Err(error) => State::Failed { file_in_use: error.is_file_in_use(), message: error.to_string() },
        });
    }
}

fn lock(mailbox: &Mutex<Mailbox>) -> std::sync::MutexGuard<'_, Mailbox> {
    mailbox.lock().unwrap_or_else(|e| e.into_inner())
}

/// 运行引擎（界面的后台线程和 `--silent` 都走这里）
pub(crate) fn run_engine(mode: Mode, package: Package, options: InstallOptions, steps: Vec<Box<dyn Step>>, cancel: &CancelToken, report: impl FnMut(Progress)) -> Result<(), Error> {
    match mode {
        Mode::Install => {
            let installer = steps.into_iter().fold(Installer::new(package, options), Installer::step);
            installer.run(cancel, report).map(|_| ())
        }
        Mode::Uninstall => {
            let uninstaller = steps.into_iter().fold(Uninstaller::new(options.directory), Uninstaller::step);
            uninstaller.run(report)
        }
    }
}

/// 选的文件夹后面接上应用名；已经是这个名字了就不接
fn with_app_folder(picked: PathBuf, name: &str) -> PathBuf {
    if picked.file_name().is_some_and(|last| last == name) { picked } else { picked.join(name) }
}

fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB { format!("{:.1} MB", bytes as f64 / MB) } else { format!("{} KB", bytes.div_ceil(1024)) }
}

/// 打印到标准错误：这个 crate 不依赖日志库，安装程序也不会装日志的接收方
fn tracing_free_warn(message: &str) {
    #[allow(clippy::print_stderr, reason = "安装程序没有日志的接收方，这是唯一留下原因的地方")]
    {
        eprintln!("{message}");
    }
}
