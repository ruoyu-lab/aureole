//! 重新编译后接着跑：监视自己的可执行文件，`cargo build` 换掉它之后，把主窗口的位置和大小、
//! 用 [`persist`] 登记过的信号写进一个临时文件，再把进程换成新编译的程序（同样的参数）；
//! 新程序启动时读回这些状态，窗口出现在原来的位置，界面停在原来的状态。
//!
//! 代码热补丁需要专门的编译工具链，这是不依赖外部工具的替代：
//! 改完代码在另一个终端（或编辑器保存时）`cargo build`，程序自己换成新版本。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use reactive::{Scope, Signal, on_cleanup, untracked};

use crate::app::App;
use crate::plugin::Plugin;
use crate::window::WindowHandle;

/// 可执行文件不再变化这么久之后才重启（链接器、签名可能分几次写）
const SETTLE: Duration = Duration::from_millis(250);
/// 带着状态文件路径的环境变量
const RESTORE_VAR: &str = "AUREOLE_RESTORE";

/// 上一个进程留下的状态：`名字 → 值`
fn restored_state() -> &'static HashMap<String, String> {
    static STATE: OnceLock<HashMap<String, String>> = OnceLock::new();
    STATE.get_or_init(|| {
        let Some(path) = std::env::var_os(RESTORE_VAR) else { return HashMap::new() };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        decode(&text)
    })
}

/// 这个进程是不是重新编译后接着上一个进程跑起来的（带着上一个进程留下的状态）
pub fn is_restored() -> bool {
    !restored_state().is_empty()
}

/// 每行 `名字\t值`，值里的反斜杠、换行、制表符转义
fn encode(state: &[(String, String)]) -> String {
    let escape = |s: &str| s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t");
    state.iter().map(|(k, v)| format!("{}\t{}\n", escape(k), escape(v))).collect()
}

fn decode(text: &str) -> HashMap<String, String> {
    let unescape = |s: &str| {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    };
    text.lines().filter_map(|line| line.split_once('\t')).map(|(k, v)| (unescape(k), unescape(v))).collect()
}

/// 登记过的状态：名字和取出当前值的函数
type Saver = (String, Box<dyn Fn() -> String>);

thread_local! {
    /// 监视自己的可执行文件（[`watch_executable`]）：守卫留到线程退出
    static EXECUTABLE: RefCell<Option<aureole_core::CancelGuard<devtools::FileWatch>>> = const { RefCell::new(None) };
    static SAVERS: RefCell<Vec<(u64, Saver)>> = const { RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static MAIN: std::cell::Cell<Option<WindowHandle>> = const { std::cell::Cell::new(None) };
}

/// 重新编译、程序换成新版本时保留这个信号的值：新程序里同名的 `persist` 把值读回来。
/// 值用 `ToString` / `FromStr` 存取（读回失败时保持原值）。归调用时的作用域所有，作用域释放后不再保留
///
/// ```no_run
/// # use aureole::prelude::*;
/// let tab = signal(0usize);
/// aureole::devtools::persist("tab", tab);       // 改完代码重新编译后，还停在原来的标签页
/// ```
///
/// 只在装了 [`HotReloadPlugin`]（或者打开了 [`crate::App::restarts_on_rebuild`]）时有用；平时只是登记一下
pub fn persist<T>(key: &str, signal: Signal<T>)
where
    T: Clone + ToString + FromStr + 'static,
{
    if let Some(value) = restored_state().get(key).and_then(|v| v.parse::<T>().ok()) {
        signal.set(value);
    }
    let id = NEXT.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let saver: Saver = (key.to_string(), Box::new(move || untracked(|| signal.with(T::to_string))));
    SAVERS.with(|s| s.borrow_mut().push((id, saver)));
    if Scope::current().is_some() {
        on_cleanup(move || {
            let _ = SAVERS.try_with(|s| s.borrow_mut().retain(|(i, _)| *i != id));
        });
    }
}

/// 开发时的热加载插件：图片文件改了自动重新解码（开发工具的 `set_image_watching`），重新编译后原地换成新程序
/// （主窗口回到原来的位置和大小，[`persist`] 登记的信号保留原来的值）。两样默认都开，开发时装上：
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn app() -> impl View { rect() }
/// use aureole::devtools::HotReloadPlugin;
/// App::new(app).plugin(HotReloadPlugin::new()).run();
/// ```
///
/// 用的都是插件的接口：装的时候开始监视、主窗口用上一次的大小，窗口创建后（`on_window_open`）放回原来的位置、
/// 记下主窗口（重启时保存它的位置和大小）
#[derive(Clone, Copy, Debug)]
pub struct HotReloadPlugin {
    images: bool,
    restarts_on_rebuild: bool,
}

impl Default for HotReloadPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl HotReloadPlugin {
    /// 两样都开
    pub fn new() -> Self {
        Self { images: true, restarts_on_rebuild: true }
    }

    /// 图片文件改了自动重新解码（默认开）
    pub fn reloads_images(mut self, enabled: bool) -> Self {
        self.images = enabled;
        self
    }

    /// 重新编译后原地换成新程序（默认开）
    pub fn restarts_on_rebuild(mut self, enabled: bool) -> Self {
        self.restarts_on_rebuild = enabled;
        self
    }
}

impl Plugin for HotReloadPlugin {
    fn build(&self, app: App) -> App {
        if self.images {
            devtools::set_image_watching(true);
        }
        if !self.restarts_on_rebuild {
            return app;
        }
        watch_executable();
        // 接着上一个进程跑：主窗口回到原来的大小（位置在窗口创建后设置，和读出来的位置是同一种）
        let app = match restored_geometry() {
            Some((_, [w, h])) => app.size(w, h),
            None => app,
        };
        let first = Cell::new(true);
        app.on_window_open(move |window| {
            // 第一个打开的是主窗口：放回原来的位置，重启时保存它的位置和大小
            if first.replace(false) {
                if let Some(([x, y], _)) = restored_geometry() {
                    window.set_position(x, y);
                }
                set_main_window(window);
            }
        })
    }
}

/// 这个进程是重新编译后由 [`restart`] 换上来的
pub(crate) fn restarted() -> bool {
    std::env::var_os(RESTORE_VAR).is_some()
}

/// 上一个进程里主窗口的位置和大小（逻辑像素）
pub(crate) fn restored_geometry() -> Option<([f32; 2], [f32; 2])> {
    let state = restored_state();
    let pair = |key: &str| -> Option<[f32; 2]> {
        let (a, b) = state.get(key)?.split_once(',')?;
        Some([a.trim().parse().ok()?, b.trim().parse().ok()?])
    };
    Some((pair("窗口.位置")?, pair("窗口.大小")?))
}

/// 记下主窗口（重启时保存它的位置和大小）
pub(crate) fn set_main_window(handle: WindowHandle) {
    MAIN.with(|m| m.set(Some(handle)));
}

/// 开始监视自己的可执行文件：被换掉、并且稳定下来之后重启
pub(crate) fn watch_executable() {
    let Ok(exe) = std::env::current_exe() else { return };
    let check = exe.clone();
    let watch = devtools::watch_metadata(&exe, move || settle(check.clone(), stamp(&check)));
    EXECUTABLE.with(|e| *e.borrow_mut() = Some(watch));
    tracing::info!(exe = %exe.display(), "改完代码重新编译（cargo build）后自动换成新程序");
}

fn stamp(path: &PathBuf) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// 等可执行文件一段时间不再变化（并且不是空的）再重启
fn settle(exe: PathBuf, seen: Option<(u64, std::time::SystemTime)>) {
    aureole_core::after(SETTLE, move || {
        let now = stamp(&exe);
        match now {
            Some((len, _)) if now == seen && len > 0 => restart(),
            _ => settle(exe, now),
        }
    });
}

/// 保存主窗口的位置和大小（装了 [`HotReloadPlugin`] 时）、登记过的信号，把进程换成磁盘上的可执行文件（同样的参数）。
/// 一般由 [`HotReloadPlugin`]（[`crate::App::restarts_on_rebuild`]）在重新编译后调用，也可以手动调用
pub fn restart() {
    let mut state: Vec<(String, String)> = Vec::new();
    if let Some(window) = MAIN.with(|m| m.get()).filter(|w| w.is_open()) {
        let ([x, y], [w, h]) = untracked(|| (window.position(), window.size()));
        state.push(("窗口.位置".into(), format!("{x},{y}")));
        state.push(("窗口.大小".into(), format!("{w},{h}")));
    }
    SAVERS.with(|s| state.extend(s.borrow().iter().map(|(_, (key, save))| (key.clone(), save()))));
    let path = std::env::temp_dir().join(format!("aureole-restart-{}.txt", std::process::id()));
    if let Err(e) = std::fs::write(&path, encode(&state)) {
        tracing::error!("保存状态失败，不重启：{e}");
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    tracing::info!("可执行文件换了，重启（保留 {} 项状态）", state.len());
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1)).env(RESTORE_VAR, &path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 换掉当前进程：进程号、终端输出不变
        let error = command.exec();
        tracing::error!("重启失败：{error}");
    }
    #[cfg(not(unix))]
    match command.spawn() {
        Ok(_) => crate::quit(),
        Err(e) => tracing::error!("重启失败：{e}"),
    }
}
