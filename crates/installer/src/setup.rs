//! 安装程序的入口：读自己带的安装包，看命令行，开窗口或者不开窗口地运行。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use aureole::prelude::*;

use crate::engine::{InstallOptions, Step, UNINSTALLER_NAME};
use crate::package::Package;
use crate::session::{Mode, Session, State, StepFactory, run_engine};
use crate::system::default_directory;
use crate::{CancelToken, Error};

/// 命令行
#[derive(Debug, Default, PartialEq, Eq)]
struct Command {
    /// `--silent`、`/S`：不开窗口
    silent: bool,
    /// `--dir 目录`、`/D=目录`：装到哪里（卸载时是装在哪里）
    directory: Option<PathBuf>,
    /// `--no-desktop-shortcut`
    no_desktop_shortcut: bool,
    /// `--no-menu-shortcut`
    no_menu_shortcut: bool,
    /// 内部用：卸载程序把自己拷到临时目录之后再启动时带的
    relocated: bool,
}

fn parse_command(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut command = Command::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--silent" | "/S" => command.silent = true,
            "--dir" => command.directory = Some(args.next().ok_or("--dir 后面要写目录")?.into()),
            "--no-desktop-shortcut" => command.no_desktop_shortcut = true,
            "--no-menu-shortcut" => command.no_menu_shortcut = true,
            "--relocated" => command.relocated = true,
            other => match other.strip_prefix("--dir=").or_else(|| other.strip_prefix("/D=")) {
                Some(directory) => command.directory = Some(directory.into()),
                None => return Err(format!("不认识的参数 {other}（--silent、--dir 目录、--no-desktop-shortcut、--no-menu-shortcut）")),
            },
        }
    }
    Ok(command)
}

/// 安装程序的入口
///
/// ```no_run
/// use aureole_installer::{Setup, Wizard};
///
/// fn main() -> std::process::ExitCode {
///     Setup::new().run(|session| Wizard::new().build(session))
/// }
/// ```
///
/// 命令行：`--silent`（或 `/S`）不开窗口，`--dir 目录` 指定目录，`--no-desktop-shortcut`、`--no-menu-shortcut`。退出码 0 是成功
#[must_use]
pub struct Setup {
    package: Option<Package>,
    directory: Option<PathBuf>,
    args: Option<Vec<String>>,
    factories: Vec<StepFactory>,
    configure: Option<Box<dyn FnOnce(Window) -> Window>>,
}

impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

impl Setup {
    /// 用这个程序自己带的安装包（`cargo bundle --format installer` 接在后面的）
    pub fn new() -> Self {
        Self { package: None, directory: None, args: None, factories: Vec::new(), configure: None }
    }

    /// 用指定的安装包：写界面时不打包就能运行
    pub fn package(mut self, package: Package) -> Self {
        self.package = Some(package);
        self
    }

    /// 用这些参数代替进程的命令行：程序自己还有别的参数时（不认识的参数会报错）
    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = Some(args.into_iter().collect());
        self
    }

    /// 默认装到哪里。命令行的 `--dir` 优先
    pub fn directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    /// 加一个自定义的步骤：解压和系统集成之后运行，失败时跟着回滚，卸载时也会调用。
    /// 传的是怎么新建它：重试时要一个新的
    pub fn step<S: Step + 'static>(mut self, make: impl Fn() -> S + Send + Sync + 'static) -> Self {
        self.factories.push(Arc::new(move || Box::new(make())));
        self
    }

    /// 改窗口：大小、标题、材质
    pub fn window(mut self, configure: impl FnOnce(Window) -> Window + 'static) -> Self {
        self.configure = Some(Box::new(configure));
        self
    }

    /// 运行，`ui` 建界面（默认的 [`Wizard`](crate::Wizard) 或者自己的）。`--silent` 时不开窗口，不调用它
    pub fn run(self, ui: impl FnOnce(Session) -> Element + 'static) -> ExitCode {
        match self.prepare() {
            Ok(Prepared::Handled(code)) => code,
            Ok(Prepared::Window { session, relocated }) => {
                let session = *session;
                let title = format!("{} {}", session.manifest().name, session.manifest().version);
                let configure = self.configure;
                App::new(move || ui(session))
                    .window(move |window| {
                        // 关窗口时还在装：先取消，撤销完了再关。不然进程没了，装到一半的留在那里
                        let window = window.title(title).size(620, 440).resizable(false).on_close_request(move |request: &aureole::CloseRequest| {
                            if session.state().get_untracked() == State::Running {
                                session.cancel();
                                request.prevent_builtin(aureole::Builtin::CLOSE);
                            }
                        });
                        match configure {
                            Some(configure) => configure(window),
                            None => window,
                        }
                    })
                    .run();
                if let Some(directory) = relocated {
                    delete_self_later(&directory);
                }
                ExitCode::SUCCESS
            }
            Err(message) => {
                report(&message);
                ExitCode::from(2)
            }
        }
    }

    fn prepare(&self) -> Result<Prepared, String> {
        let command = match &self.args {
            Some(args) => parse_command(args.iter().cloned())?,
            None => parse_command(std::env::args().skip(1))?,
        };
        let package = match &self.package {
            Some(package) => package.clone(),
            None => Package::current().map_err(|e| e.to_string())?,
        };
        let mode = if package.manifest().uninstaller { Mode::Uninstall } else { Mode::Install };
        let directory = match (&command.directory, mode) {
            (Some(directory), _) => Some(directory.clone()),
            (None, Mode::Uninstall) => Some(std::env::current_exe().ok().and_then(|exe| exe.parent().map(PathBuf::from)).ok_or("找不到程序自己在哪")?),
            (None, Mode::Install) => self.directory.clone(),
        };
        // 卸载程序要删掉自己所在的目录：Windows 上运行中的程序删不掉，拷到临时目录再运行
        if mode == Mode::Uninstall
            && !command.relocated
            && cfg!(windows)
            && let Some(directory) = &directory
        {
            return relocate(directory, &command).map(Prepared::Handled).map_err(|e| e.to_string());
        }
        if command.silent {
            let directory = directory.unwrap_or_else(|| default_directory(package.manifest()));
            let code = run_silent(package, mode, directory.clone(), &command, &self.factories);
            if command.relocated {
                delete_self_later(&directory);
            }
            return Ok(Prepared::Handled(code));
        }
        let session = Session::new(package, mode, directory, self.factories.clone());
        session.menu_shortcut().set(!command.no_menu_shortcut);
        session.desktop_shortcut().set(!command.no_desktop_shortcut);
        let relocated = command.relocated.then(|| session.directory().get_untracked());
        Ok(Prepared::Window { session: Box::new(session), relocated })
    }
}

enum Prepared {
    Handled(ExitCode),
    Window {
        session: Box<Session>,
        /// 这是卸载程序拷到临时目录的那份：结束时收拾这个目录
        relocated: Option<PathBuf>,
    },
}

fn run_silent(package: Package, mode: Mode, directory: PathBuf, command: &Command, factories: &[StepFactory]) -> ExitCode {
    let options = InstallOptions { directory, menu_shortcut: !command.no_menu_shortcut, desktop_shortcut: !command.no_desktop_shortcut, register: true };
    let steps = factories.iter().map(|make| make()).collect();
    match run_engine(mode, package, options, steps, &CancelToken::default(), |_| {}) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

/// 把卸载程序拷到临时目录，从那里再启动，这个进程结束
fn relocate(directory: &std::path::Path, command: &Command) -> Result<ExitCode, Error> {
    let exe = std::env::current_exe().map_err(|e| Error::NotAPackage(format!("找不到程序自己：{e}")))?;
    let copy = std::env::temp_dir().join(format!("aureole-{}-{UNINSTALLER_NAME}", std::process::id()));
    std::fs::copy(&exe, &copy).map_err(|e| Error::io(&copy, e))?;
    let mut child = std::process::Command::new(&copy);
    child.arg("--relocated").arg("--dir").arg(directory);
    if command.silent {
        child.arg("--silent");
    }
    let mut child = child.spawn().map_err(|e| Error::io(&copy, e))?;
    if command.silent {
        // 调用方（脚本）靠退出码判断：等它结束
        let status = child.wait().map_err(|e| Error::io(&copy, e))?;
        return Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }
    Ok(ExitCode::SUCCESS)
}

/// 临时目录里的卸载程序结束之后收拾：删掉自己；原来的 `uninstall.exe`（`--silent` 时它一直开着，等这边的退出码，
/// 卸载时删不掉）和空了的安装目录。Windows 上运行中的程序删不掉，让命令行等它们都退出了再删
fn delete_self_later(directory: &std::path::Path) {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = cleanup_script(&exe, directory);
        // 引号要原样交给 cmd：标准库的转义（\"）它不认
        // 输出不接到调用的人那里：否则它要等这个命令结束才读完（多等两秒），还会看到 rmdir 的报错
        let _ = std::process::Command::new("cmd")
            .raw_arg(format!("/c {script}"))
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(windows))]
    let _ = directory;
}

/// 收拾用的命令：等两秒，删临时的这份、原来的卸载程序，目录空了就删目录（`rmdir` 不带 `/s`，有别的文件时不动）
#[cfg_attr(not(windows), allow(dead_code, reason = "只有 Windows 的卸载程序要这样收拾"))]
fn cleanup_script(exe: &std::path::Path, directory: &std::path::Path) -> String {
    let uninstaller = directory.join(UNINSTALLER_NAME);
    format!("ping -n 3 127.0.0.1 >nul & del /q \"{}\" & del /q \"{}\" & rmdir \"{}\"", exe.display(), uninstaller.display(), directory.display())
}

/// 打印原因（`--silent` 时调用方看标准错误；开窗口之前出的错，比如不是安装包）
fn report(message: &str) {
    #[allow(clippy::print_stderr, reason = "安装程序没有日志的接收方，这是唯一留下原因的地方")]
    {
        eprintln!("{message}");
    }
}
