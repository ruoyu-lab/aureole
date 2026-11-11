//! 插件：装进应用的一组设置和钩子（[`Plugin`]、[`crate::App::plugin`]）。
//!
//! 插件在 [`crate::App::run`] 开始时（事件循环创建之前、主线程上）按装的顺序调用一次 [`Plugin::build`]，
//! 拿到应用、返回装好的应用：和直接在 `App` 上链式调用一样，能换执行器、改主窗口、再装别的插件，
//! 也能挂这几个钩子（都在主线程上调用）：
//!
//! - [`crate::App::on_window_open`]：窗口创建好以后（根视图挂好、渲染线程开始了）；
//! - [`crate::App::on_platform_event`]：窗口系统交来的事件，换算成输入事件之前，能把它吃掉；
//! - [`crate::App::on_frame_start`]、[`crate::App::on_frame_end`]：每个窗口每帧推进的前后；
//! - [`crate::App::on_exit`]：应用退出前（事件循环结束之前）。
//!
//! 后台线程上的回调要回到主线程，用核心的主线程投递（`MainThreadSender`）。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::time::Duration;

use aureole_core::FrameStats;
use winit::event::WindowEvent;

use crate::app::App;
use crate::window::WindowHandle;

/// 插件：在 [`App::run`] 开始时按装的顺序调用一次 [`Plugin::build`]（事件循环创建之前、主线程上）。
/// 函数 `Fn(App) -> App` 也是插件
///
/// ```no_run
/// # use aureole::prelude::*;
/// use aureole::Plugin;
///
/// /// 每个窗口每帧推进完记一下用了多久
/// struct FrameLog;
///
/// impl Plugin for FrameLog {
///     fn build(&self, app: App) -> App {
///         app.on_window_open(|window| println!("开了窗口 {window:?}"))
///             .on_frame_end(|frame| {
///                 if let Some(stats) = frame.stats {
///                     println!("{:?} 一帧：布局 {}，绘制 {}", frame.window, stats.laid_out, stats.repainted);
///                 }
///             })
///             .on_exit(|| println!("退出"))
///     }
/// }
///
/// # fn app() -> impl View { rect() }
/// App::new(app).plugin(FrameLog).run();
/// ```
pub trait Plugin: 'static {
    /// 装进应用：拿到应用，返回装好的应用
    fn build(&self, app: App) -> App;
}

impl<F: Fn(App) -> App + 'static> Plugin for F {
    fn build(&self, app: App) -> App {
        self(app)
    }
}

/// 一帧的信息，交给每帧开始、结束的钩子（[`App::on_frame_start`]、[`App::on_frame_end`]）
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// 推进的是哪个窗口
    pub window: WindowHandle,
    /// 这一帧的时间：预计上屏的时刻，统一的时钟（和定时器、`sleep`、输入事件的时间戳同一个）的读数
    pub time: Duration,
    /// 这一帧做了什么（布局、绘制、耗时）；开始的钩子里是 `None`
    pub stats: Option<FrameStats>,
}

/// 窗口系统交来的一个事件，换算成输入事件、交给元素树之前（[`App::on_platform_event`]）。
///
/// [`PlatformEvent::stop_propagation`] 把它吃掉：不再换算成输入事件，元素树收不到（插件自己的全局快捷键、
/// 嵌在窗口里的原生视图拿走了键盘时用）。窗口自己的处理照常：改大小、挪位置、外观、遮挡、关闭请求
pub struct PlatformEvent<'a> {
    window: WindowHandle,
    event: &'a WindowEvent,
    stopped: Cell<bool>,
}

/// 窗口系统事件的大类（[`PlatformEvent::kind`]）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformEventKind {
    /// 按键、修饰键
    Keyboard,
    /// 指针：移动、进入、离开、按下、松开、触摸
    Pointer,
    /// 滚轮、触控板滚动
    Wheel,
    /// 触控板手势：捏合、旋转
    Gesture,
    /// 输入法
    Ime,
    /// 窗口得到、失去焦点
    Focus,
    /// 拖进来的文件：悬停、放下、取消
    File,
    /// 窗口大小、缩放变了
    Resize,
    /// 用户点了关闭按钮
    Close,
    /// 别的：挪位置、外观、遮挡、重画请求……
    Other,
}

impl<'a> PlatformEvent<'a> {
    pub(crate) fn new(window: WindowHandle, event: &'a WindowEvent) -> Self {
        Self { window, event, stopped: Cell::new(false) }
    }

    /// 发生在哪个窗口
    pub fn window(&self) -> WindowHandle {
        self.window
    }

    /// 大类
    pub fn kind(&self) -> PlatformEventKind {
        match self.event {
            WindowEvent::KeyboardInput { .. } | WindowEvent::ModifiersChanged(_) => PlatformEventKind::Keyboard,
            WindowEvent::CursorMoved { .. }
            | WindowEvent::CursorEntered { .. }
            | WindowEvent::CursorLeft { .. }
            | WindowEvent::MouseInput { .. }
            | WindowEvent::Touch(_) => PlatformEventKind::Pointer,
            WindowEvent::MouseWheel { .. } => PlatformEventKind::Wheel,
            WindowEvent::PinchGesture { .. }
            | WindowEvent::RotationGesture { .. }
            | WindowEvent::PanGesture { .. }
            | WindowEvent::DoubleTapGesture { .. } => PlatformEventKind::Gesture,
            WindowEvent::Ime(_) => PlatformEventKind::Ime,
            WindowEvent::Focused(_) => PlatformEventKind::Focus,
            WindowEvent::HoveredFile(_) | WindowEvent::DroppedFile(_) | WindowEvent::HoveredFileCancelled => PlatformEventKind::File,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => PlatformEventKind::Resize,
            WindowEvent::CloseRequested => PlatformEventKind::Close,
            _ => PlatformEventKind::Other,
        }
    }

    /// 原始的事件，类型是窗口库的窗口事件：插件自己依赖同一版窗口库时向下转型取出细节
    /// （`event.raw().downcast_ref::<winit::event::WindowEvent>()`）。窗口库的类型不进框架的公开接口，所以这里是 `Any`
    pub fn raw(&self) -> &dyn Any {
        self.event
    }

    /// 吃掉这个事件：不再换算成输入事件，元素树收不到。之后的插件照样看得到它
    pub fn stop_propagation(&self) {
        self.stopped.set(true);
    }

    /// 被吃掉了
    pub fn is_propagation_stopped(&self) -> bool {
        self.stopped.get()
    }
}

/// 接入 tokio 的插件（`tokio` 特性）：后台任务的执行器换成 tokio（[`aureole_core::TokioSpawner`]），
/// `spawn`、`spawn_blocking`、`resource` 都在 tokio 上跑，依赖 tokio 的库（reqwest、sqlx……）直接 `spawn`；
/// `spawn_tokio`、`resource_tokio` 用的也是同一个运行时。
///
/// 装的时候（`App::run` 开始、事件循环创建之前）定下运行时：给了应用自己的就用它，没给时在 `#[tokio::main]` 里
/// 用调用方的、否则框架创建一个（见 `tokio_handle`）
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn app() -> impl View { rect() }
/// App::new(app).plugin(aureole::TokioPlugin::new()).run();
/// ```
#[cfg(feature = "tokio")]
#[derive(Clone, Debug, Default)]
pub struct TokioPlugin {
    runtime: Option<tokio::runtime::Handle>,
}

#[cfg(feature = "tokio")]
impl TokioPlugin {
    /// 用 `tokio_handle` 给的运行时
    pub fn new() -> Self {
        Self::default()
    }

    /// 用应用自己的运行时（多线程的）。之前已经定下了运行时（用过 tokio）时记一条警告，仍用原来那个
    pub fn with_runtime(handle: tokio::runtime::Handle) -> Self {
        Self { runtime: Some(handle) }
    }
}

#[cfg(feature = "tokio")]
impl Plugin for TokioPlugin {
    fn build(&self, app: App) -> App {
        if let Some(handle) = &self.runtime
            && aureole_core::set_tokio_runtime(handle.clone()).is_err()
        {
            tracing::warn!("tokio 运行时已经定下了（或者给的是单线程的运行时），后台任务交给原来那个");
        }
        app.spawner(aureole_core::TokioSpawner::default())
    }
}

type WindowHook = Box<dyn Fn(WindowHandle)>;
type PlatformEventHook = Box<dyn Fn(&PlatformEvent)>;
type FrameHook = Box<dyn Fn(&FrameInfo)>;
type ExitHook = Box<dyn FnOnce()>;

thread_local! {
    /// 应用运行中挂的退出钩子（[`at_exit`]）
    static RUNNING_EXIT: RefCell<Vec<ExitHook>> = const { RefCell::new(Vec::new()) };
}

/// 应用运行中挂一个退出钩子：和 [`crate::App::on_exit`] 一样在应用退出前（主线程上）调用一次，排在 `App` 上挂的后面。
/// 给用到时才启动的东西用（内嵌的 CEF 内核第一次用到时才启动，那时应用早就建好了）。在主线程上调用；
/// 退出的钩子里再挂的也会调用
pub fn at_exit(handler: impl FnOnce() + 'static) {
    RUNNING_EXIT.with_borrow_mut(|hooks| hooks.push(Box::new(handler)));
}

/// 应用的钩子：插件和应用在 [`App`] 上挂的，事件循环开始后交给运行时
#[derive(Default)]
pub(crate) struct Hooks {
    pub(crate) window_open: Vec<WindowHook>,
    pub(crate) platform_event: Vec<PlatformEventHook>,
    pub(crate) frame_start: Vec<FrameHook>,
    pub(crate) frame_end: Vec<FrameHook>,
    pub(crate) exit: RefCell<Vec<ExitHook>>,
}

impl Hooks {
    /// 窗口创建好了
    pub(crate) fn window_open(&self, window: WindowHandle) {
        self.window_open.iter().for_each(|hook| hook(window));
    }

    /// 窗口系统交来一个事件：按挂的顺序交给每个钩子，返回有没有被吃掉
    pub(crate) fn platform_event(&self, window: WindowHandle, event: &WindowEvent) -> bool {
        if self.platform_event.is_empty() {
            return false;
        }
        let event = PlatformEvent::new(window, event);
        self.platform_event.iter().for_each(|hook| hook(&event));
        event.is_propagation_stopped()
    }

    /// 有每帧的钩子（没有时不用算帧的信息）
    pub(crate) fn has_frame_hooks(&self) -> bool {
        !self.frame_start.is_empty() || !self.frame_end.is_empty()
    }

    pub(crate) fn frame_start(&self, frame: &FrameInfo) {
        self.frame_start.iter().for_each(|hook| hook(frame));
    }

    pub(crate) fn frame_end(&self, frame: &FrameInfo) {
        self.frame_end.iter().for_each(|hook| hook(frame));
    }

    /// 应用退出前：只调用一次
    pub(crate) fn exit(&self) {
        let hooks = std::mem::take(&mut *self.exit.borrow_mut());
        hooks.into_iter().for_each(|hook| hook());
        // 运行中挂的：钩子里还可能再挂，取到空为止
        loop {
            let hooks = RUNNING_EXIT.with_borrow_mut(std::mem::take);
            if hooks.is_empty() {
                break;
            }
            hooks.into_iter().for_each(|hook| hook());
        }
    }
}
