//! 打开网址和文件、单实例：见 [`crate::App::on_open`]、[`crate::App::single_instance`]。
//!
//! 所有来源走同一个队列：命令行参数（启动时）、macOS 的打开事件（启动时和运行中）、单实例里后启动的进程转来的
//! （后台线程）。主窗口建好之前到的先排着，建好后在主线程按顺序交给处理器；之后到的唤醒事件循环，下一轮交出去。

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use aureole_core::MainThreadSender;
use platform::single_instance::{InstanceRole, Listening, Message};
use platform::open::Opened;

use crate::window;

/// 应用被要求打开的网址和文件，交给 [`crate::App::on_open`]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenRequest {
    /// 网址，比如自定义协议的 `myapp://note/42`（原样，没有解码）
    pub urls: Vec<String>,
    /// 文件和文件夹，绝对路径
    pub files: Vec<PathBuf>,
    /// 从哪里来
    pub source: OpenSource,
}

impl OpenRequest {
    /// 没有网址也没有文件
    pub fn is_empty(&self) -> bool {
        self.urls.is_empty() && self.files.is_empty()
    }
}

/// 打开请求从哪里来
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OpenSource {
    /// 启动这个进程时：命令行参数，或者系统为了打开它们而启动了应用（macOS 打包后的启动事件）
    #[default]
    Launch,
    /// 运行中由系统交来：在访达里打开文档、点了自定义协议的链接（macOS 打包后）。
    /// Windows、Linux 上这时系统会再启动一个进程，开了单实例时转过来，是 [`OpenSource::SecondInstance`]
    System,
    /// 开了单实例（[`crate::App::single_instance`]）时再次启动转来的，带着那次的命令行参数（不含程序名）。
    /// 交给处理器之前，框架已经把最近用过的窗口带到了最前面
    SecondInstance(Vec<String>),
}

/// 单实例的设置，见 [`crate::App::single_instance`]。`true` 是 [`SingleInstance::Auto`]，字符串是 [`SingleInstance::Id`]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SingleInstance {
    /// 不限制（默认）
    #[default]
    Off,
    /// 标识自动取：可执行文件名。打包成 .app 后可执行文件还是这个名字，所以打包的和 `cargo run` 的互相认得
    Auto,
    /// 指定标识：同一个用户、同一个标识的进程只运行一个
    Id(String),
}

impl From<bool> for SingleInstance {
    fn from(on: bool) -> Self {
        if on { SingleInstance::Auto } else { SingleInstance::Off }
    }
}

impl From<&str> for SingleInstance {
    fn from(id: &str) -> Self {
        SingleInstance::Id(id.to_owned())
    }
}

impl From<String> for SingleInstance {
    fn from(id: String) -> Self {
        SingleInstance::Id(id)
    }
}

/// 打开请求的处理器
pub(crate) type Handler = Rc<dyn Fn(&OpenRequest)>;

/// 还没交出去的打开请求。主窗口建好（`ready`）之前只排队
#[derive(Default)]
pub(crate) struct Queue {
    pending: Vec<OpenRequest>,
    ready: bool,
    /// 事件循环开始后：到了新请求，投递到主线程交出去
    sender: Option<MainThreadSender>,
}

impl Queue {
    pub(crate) const fn new() -> Self {
        Self { pending: Vec::new(), ready: false, sender: None }
    }

    /// 排进队列，返回投递用的句柄（排队时不投递：投递要拿收件箱的锁）。
    /// 空的请求只有再次启动转来的才留着（要把窗口带到前面）
    pub(crate) fn push(&mut self, request: OpenRequest) -> Option<MainThreadSender> {
        if request.is_empty() && !matches!(request.source, OpenSource::SecondInstance(_)) {
            return None;
        }
        self.pending.push(request);
        self.sender.clone()
    }

    /// 主窗口建好之后才交出排着的请求，按到达的顺序
    pub(crate) fn take(&mut self) -> Vec<OpenRequest> {
        if self.ready { std::mem::take(&mut self.pending) } else { Vec::new() }
    }

    pub(crate) fn set_ready(&mut self) {
        self.ready = true;
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn set_sender(&mut self, sender: MainThreadSender) {
        self.sender = Some(sender);
    }
}

/// 全进程一个：后台线程（单实例的通道）也往里放
static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());

fn queue<R>(f: impl FnOnce(&mut Queue) -> R) -> R {
    f(&mut QUEUE.lock().unwrap_or_else(PoisonError::into_inner))
}

thread_local! {
    static HANDLERS: RefCell<Vec<Handler>> = const { RefCell::new(Vec::new()) };
    /// 有处理器或开了单实例：要接收打开请求
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// 排进队列；事件循环已经在跑时投递到主线程交出去
pub(crate) fn push(request: OpenRequest) {
    if let Some(sender) = queue(|q| q.push(request)) {
        sender.send(dispatch);
    }
}

/// 事件循环创建之前调用：登记处理器；开了单实例时决定这个进程的角色——已经有实例在运行时，
/// 把这次要打开的东西转给它，然后退出进程（不返回）；把命令行参数里的网址和文件排进队列。
/// 返回第一个实例的监听，要一直留着（释放时停止监听、放开锁）
pub(crate) fn startup(single: &SingleInstance, handlers: Vec<Handler>) -> Option<Listening> {
    let wanted = !handlers.is_empty() || *single != SingleInstance::Off;
    HANDLERS.with(|h| *h.borrow_mut() = handlers);
    ENABLED.with(|e| e.set(wanted));
    if !wanted {
        return None;
    }
    let listening = instance_id(single).and_then(|id| single_instance(&id));
    let launch = Opened::from_command_line();
    push(OpenRequest { urls: launch.urls, files: launch.files, source: OpenSource::Launch });
    listening
}

/// 事件循环创建之后在主线程调用：以后到的请求投递到主线程交出去；macOS 上开始接收系统的打开事件
pub(crate) fn attach() {
    if !ENABLED.with(Cell::get) {
        return;
    }
    let sender = aureole_core::main_thread_sender();
    queue(|q| q.set_sender(sender));
    crate::system::backend().single_instance().listen_open_events(Box::new(|opened: Opened| {
        // 主窗口建好之前到的是启动时的事件（为了打开它们而启动了应用）
        let source = if queue(|q| q.is_ready()) { OpenSource::System } else { OpenSource::Launch };
        push(OpenRequest { urls: opened.urls, files: opened.files, source });
    }));
}

/// 主窗口建好了：交出启动时排着的请求，之后到的随到随交
pub(crate) fn set_ready() {
    queue(Queue::set_ready);
    dispatch();
}

/// 在主线程把就绪的请求交给处理器（在批量更新里）；再次启动转来的先把最近用过的窗口带到最前面
pub(crate) fn dispatch() {
    let requests = queue(Queue::take);
    if requests.is_empty() {
        return;
    }
    let handlers = HANDLERS.with(|h| h.borrow().clone());
    for request in requests {
        if matches!(request.source, OpenSource::SecondInstance(_))
            && let Some(window) = window::front_window()
        {
            window.focus();
        }
        reactive::batch(|| {
            for handler in &handlers {
                handler(&request);
            }
        });
    }
}

fn instance_id(single: &SingleInstance) -> Option<String> {
    match single {
        SingleInstance::Off => None,
        SingleInstance::Id(id) => Some(id.clone()),
        SingleInstance::Auto => {
            let exe = std::env::current_exe().ok()?;
            Some(exe.file_stem()?.to_string_lossy().into_owned())
        }
    }
}

/// 拿到锁就当第一个实例、开始接收；否则把这次的参数转给第一个实例，然后退出
fn single_instance(id: &str) -> Option<Listening> {
    // 开发工具重新编译后重启：新进程起来时旧的可能还没退出，等它放开锁，不要转发
    #[cfg(feature = "devtools")]
    let restarted = crate::restart::restarted();
    #[cfg(not(feature = "devtools"))]
    let restarted = false;
    let mut message = None;
    // macOS 打包后收过系统的启动事件，应用已经启动完了，不能再跑事件循环：转发不成只好退出
    let give_up = |message: &Option<Message>, why: &str| {
        if message.is_some() && crate::system::backend().single_instance().delivers_open_events() {
            tracing::error!("单实例：{why}；请再打开一次");
            std::process::exit(1);
        }
        tracing::warn!("单实例：{why}，照常启动");
    };
    for _ in 0..2 {
        match crate::system::backend().single_instance().acquire(id, restarted) {
            Ok(InstanceRole::Primary(primary)) => {
                if message.is_some() {
                    give_up(&message, "转发失败，正在运行的实例已经退出");
                }
                return Some(primary.listen(|message| push(request_from(message))));
            }
            Ok(InstanceRole::Secondary(secondary)) => {
                let message = message.get_or_insert_with(launch_message);
                match secondary.send(message) {
                    Ok(()) => {
                        tracing::info!("单实例：已经有实例在运行，把参数转给它，这个进程退出");
                        std::process::exit(0);
                    }
                    Err(e) => tracing::warn!("单实例：转给正在运行的实例失败（{e}），再试一次"),
                }
            }
            Err(e) => {
                give_up(&message, &e.to_string());
                return None;
            }
        }
    }
    give_up(&message, "转发失败");
    None
}

/// 后启动的实例要转发的：命令行参数，和里面的网址、文件；macOS 打包后再收下系统的启动事件
fn launch_message() -> Message {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut opened = Opened::from_command_line();
    let instance = crate::system::backend().single_instance();
    if instance.delivers_open_events() {
        opened.extend(instance.collect_launch_events(Duration::from_millis(500)));
    }
    Message { args, urls: opened.urls, files: opened.files }
}

fn request_from(message: Message) -> OpenRequest {
    let args = message.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    OpenRequest { urls: message.urls, files: message.files, source: OpenSource::SecondInstance(args) }
}
