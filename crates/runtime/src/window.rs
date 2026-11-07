//! 窗口：打开多个窗口、管理窗口、查询当前窗口。
//!
//! 所有窗口在同一个主线程、共享同一个响应式运行时：窗口之间通信就是共享信号，
//! 一个窗口改了信号，另一个窗口里绑定它的属性自动更新。每个窗口有自己的元素树、
//! 帧时钟和渲染线程，GPU 设备共用一个。
//!
//! [`Window::open`] 可以在任何地方调用（包括处理器）：请求排进队列，事件循环下一轮创建窗口。
//! [`WindowHandle`] 像信号一样是 `Copy` 的句柄，窗口的大小、位置、焦点、全屏状态都是响应式的，
//! 可以直接绑定到界面上。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use reactive::{Scope, Signal, signal};
use platform::{TitleBar, TitleBarOptions, WindowMaterial};
use scene::{Color, Number};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::window::{Fullscreen, WindowLevel};

use crate::display_color::{ColorOutput, DisplayColor};
use aureole_core::host::{WindowId, current_window_id, run_in_window};
use aureole_core::{Builtin, Element, IntoHandler, IntoProp, Prop, TitleBarInfo, View};

/// 叫醒事件循环的原因。要在主线程上做的事（后台任务做完、图片解码好、文件读好、菜单和托盘被点了、
/// 系统设置变了、打开请求、渲染线程换不了 GPU）都投递进核心的收件箱（`MainThreadSender`），这里只剩叫醒：
#[derive(Debug)]
pub(crate) enum UserEvent {
    /// 收件箱从空变成非空（核心的 `MainThreadWaker`）；窗口请求（打开、关闭、退出）也用它叫醒
    Wake,
    /// 帧时钟的后台线程等到了一帧（Windows）：只把帧交出去，不恢复别的窗口的时钟
    Frame,
    /// 辅助技术要树、请求操作。无障碍适配器的接口只收事件循环代理，带着它要的事件类型，所以单独一种
    Accessibility(accesskit_winit::Event),
    /// 平台层接到的文件拖放（窗口库不管文件的平台）：换成窗口库一样的窗口事件，走同一条路
    Window(winit::window::WindowId, winit::event::WindowEvent),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Accessibility(event)
    }
}

/// 一个窗口的描述，[`Window::open`] 之后才创建。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn settings() -> impl View { rect() }
/// let count = signal(0);
/// let handle = Window::new(move || text(move || format!("点了 {} 次", count.get())))
///     .title("计数")
///     .size(320, 200)
///     .always_on_top(true)
///     .open();
/// ```
pub struct Window {
    pub(crate) build: Box<dyn FnOnce() -> Element>,
    pub(crate) title: Prop<String>,
    pub(crate) size: [f32; 2],
    pub(crate) position: Option<[f32; 2]>,
    pub(crate) min_size: Option<[f32; 2]>,
    pub(crate) resizable: bool,
    pub(crate) always_on_top: bool,
    pub(crate) fullscreen: bool,
    pub(crate) decorations: bool,
    pub(crate) background: Option<Color>,
    pub(crate) on_close_request: Option<OnCloseRequest>,
    pub(crate) color_output: ColorOutput,
    pub(crate) title_bar: TitleBar,
    pub(crate) title_bar_options: TitleBarOptions,
    pub(crate) backdrop: WindowMaterial,
}

/// 关窗口前问一下的处理器
pub(crate) type OnCloseRequest = Rc<dyn Fn(&CloseRequest)>;

/// 用户请求关闭窗口（点了关闭按钮、按了系统的关闭快捷键），见 [`Window::on_close_request`]
#[derive(Debug, Default)]
pub struct CloseRequest {
    prevented: Cell<Builtin>,
}

impl CloseRequest {
    /// 取消内置行为：`Builtin::CLOSE`（或 `Builtin::ALL`）是不关窗口
    pub fn prevent_builtin(&self, builtin: Builtin) {
        self.prevented.set(self.prevented.get() | builtin);
    }

    /// 这些内置行为是不是已经被取消了
    pub fn is_builtin_prevented(&self, builtin: Builtin) -> bool {
        self.prevented.get().contains(builtin)
    }
}

impl Window {
    /// `build` 在窗口创建后调用一次，返回根视图；这时 [`current_window`] 就是这个窗口
    pub fn new<V: View>(build: impl FnOnce() -> V + 'static) -> Self {
        Self {
            build: Box::new(move || build().into_element()),
            title: Prop::Static("Aureole".into()),
            size: [960.0, 640.0],
            position: None,
            min_size: None,
            resizable: true,
            always_on_top: false,
            fullscreen: false,
            decorations: true,
            background: None,
            on_close_request: None,
            color_output: ColorOutput::Auto,
            title_bar: TitleBar::System,
            title_bar_options: TitleBarOptions::default(),
            backdrop: WindowMaterial::Opaque,
        }
    }

    /// 标题，可以绑定信号
    pub fn title<S: Into<String> + 'static>(mut self, title: impl IntoProp<S>) -> Self {
        self.title = title.into_prop().map(S::into);
        self
    }

    /// 内容区的初始大小，逻辑像素；整数、浮点数都行（`.size(960, 640)`）
    pub fn size(mut self, width: impl Number, height: impl Number) -> Self {
        self.size = [width.to_f32(), height.to_f32()];
        self
    }

    /// 初始位置（整个窗口连同标题栏的左上角，在屏幕上的逻辑像素坐标）；不指定时由系统决定。
    /// 和 [`WindowHandle::position`]、[`WindowHandle::set_position`] 是同一个点：存下的位置下次照样放回去
    pub fn position(mut self, x: impl Number, y: impl Number) -> Self {
        self.position = Some([x.to_f32(), y.to_f32()]);
        self
    }

    /// 用户拖动缩放时的最小大小
    pub fn min_size(mut self, width: impl Number, height: impl Number) -> Self {
        self.min_size = Some([width.to_f32(), height.to_f32()]);
        self
    }

    /// 用户能否拖动改变大小，默认能
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    /// 置顶：总在其他窗口上面
    pub fn always_on_top(mut self, on_top: bool) -> Self {
        self.always_on_top = on_top;
        self
    }

    /// 以全屏打开
    pub fn fullscreen(mut self, fullscreen: bool) -> Self {
        self.fullscreen = fullscreen;
        self
    }

    /// 是否有系统的标题栏和边框
    pub fn decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// 窗口的背景色。不设置时是默认的浅灰底色；有窗口材质（[`Window::backdrop`]）时不设置是透明的，设了就是盖在材质上的一层，可以带透明度
    pub fn background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// 窗口的标题栏。[`TitleBar::Hidden`]：内容铺满整个窗口，标题栏透明、不显示标题文字，标题栏由应用自己画，
    /// 元素上用 `window_drag_area` 声明哪里能拖动窗口，用 `title_bar_info()` 读标题栏占了多少。
    /// macOS 上系统的窗口按钮（红绿灯）还在；Windows、Linux 上按钮也要自己画（有没有见 [`crate::Capabilities::supports_title_bar_controls`]）。
    /// Windows 上这样的窗口不挂菜单栏（菜单栏长在系统的标题栏下面）
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// use aureole::{TitleBar, title_bar_info};
    /// Window::new(|| {
    ///     column().child(
    ///         row()
    ///             .window_drag_area(true)
    ///             .height(move || px(title_bar_info().height))
    ///             .child(text("我的应用").margin_left(move || px(title_bar_info().inset_left))),
    ///     )
    /// })
    /// .title_bar(TitleBar::Hidden)
    /// .open();
    /// ```
    pub fn title_bar(mut self, title_bar: TitleBar) -> Self {
        self.title_bar = title_bar;
        self
    }

    /// 系统窗口按钮（macOS 的红绿灯）的位置：第一个按钮的左边、上边离窗口左上角多远，逻辑像素。窗口缩放、进出全屏后保持。
    /// 标题栏的高度跟着变成"按钮的高度加两倍的上边距"，按钮在这一条里竖直居中。只有隐藏了标题栏（[`Window::title_bar`]）的窗口有用；
    /// 没有系统窗口按钮的平台上不起作用
    pub fn title_bar_controls_position(mut self, x: impl Number, y: impl Number) -> Self {
        self.title_bar_options.controls_position = Some([x.to_f32(), y.to_f32()]);
        self
    }

    /// 窗口背后透出系统的材质（默认 [`WindowMaterial::Opaque`]：不透明，渲染路径和开销都和没有这个功能时一样）。
    /// 窗口清屏成透明（有 [`Window::background`] 时清成它），没被不透明的元素盖住的地方透出后面的东西；窗口失去焦点时的样子跟系统。
    /// 这个平台没有窗口材质（见 [`crate::Capabilities::supports_window_backdrop`]）时照常画不透明的背景。
    /// Windows 上要在打开窗口时就设（表面建在系统合成的对象上才能带透明），之后才能用 [`WindowHandle::set_backdrop`] 切换
    pub fn backdrop(mut self, backdrop: WindowMaterial) -> Self {
        self.backdrop = backdrop;
        self
    }

    /// 表面的颜色输出（默认 [`ColorOutput::Auto`]：画面里有广色域、HDR 的内容，显示器又支持时
    /// 才用扩展范围的表面）。见 [`aureole_core::WideColor`]、[`WindowHandle::display_color`]
    pub fn color_output(mut self, output: ColorOutput) -> Self {
        self.color_output = output;
        self
    }

    /// 用户点关闭按钮时先问一下：调用 `e.prevent_builtin(Builtin::CLOSE)` 就不关（比如有没保存的修改）。
    /// [`WindowHandle::close`] 不经过这里
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # use aureole::{Builtin, CloseRequest};
    /// let dirty = signal(true);
    /// Window::new(|| text("编辑器")).on_close_request(move |e: &CloseRequest| {
    ///     if dirty.get_untracked() {
    ///         e.prevent_builtin(Builtin::CLOSE);
    ///     }
    /// });
    /// ```
    ///
    /// 处理器只收 `Fn(&CloseRequest)`、返回 `()`：以前返回 `false` 表示不关，现在要调
    /// `prevent_builtin(Builtin::CLOSE)`。返回 `bool` 的旧写法编译不过，不会被悄悄丢掉
    pub fn on_close_request(mut self, handler: impl Fn(&CloseRequest) + 'static) -> Self {
        self.on_close_request = Some(IntoHandler::<CloseRequest, fn(&CloseRequest)>::into_handler(handler));
        self
    }

    /// 打开窗口：事件循环下一轮创建。返回的句柄马上就能用（窗口创建前的设置会在创建后补上）
    pub fn open(self) -> WindowHandle {
        let handle = with_registry(|r| {
            r.next += 1;
            let handle = WindowHandle(r.next);
            let scope = r.scope.get_or_insert_with(Scope::new);
            let [w, h] = self.size;
            let state = scope.run(|| State {
                open: signal(true),
                size: signal([w, h]),
                position: signal(self.position.unwrap_or([0.0, 0.0])),
                focused: signal(false),
                fullscreen: signal(self.fullscreen),
                minimized: signal(false),
                maximized: signal(false),
                display: signal(DisplayColor::SDR),
                title_bar: signal(TitleBarInfo::default()),
                backdrop: signal(self.backdrop),
            });
            r.windows.insert(handle.0, Entry { window: None, state, pending: Vec::new() });
            r.requests.push(Request::Open(handle, self));
            handle
        });
        wake();
        handle
    }
}

/// 窗口的句柄。像信号一样是 `Copy` 的，可以随便放进闭包。
/// 读取状态的方法是响应式的：在绑定里读，窗口状态变化时自动更新
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowHandle(u64);

impl WindowHandle {
    /// 核心里的身份
    pub(crate) fn id(self) -> WindowId {
        WindowId(self.0)
    }

    fn state(self) -> Option<State> {
        with_registry(|r| r.windows.get(&self.0).map(|entry| entry.state))
    }

    /// 还开着（关掉以后变成 `false`）
    pub fn is_open(self) -> bool {
        self.state().is_some_and(|s| s.open.get())
    }

    /// 内容区大小，逻辑像素
    pub fn size(self) -> [f32; 2] {
        self.state().map_or([0.0; 2], |s| s.size.get())
    }

    /// 窗口左上角在屏幕上的位置，逻辑像素
    pub fn position(self) -> [f32; 2] {
        self.state().map_or([0.0; 2], |s| s.position.get())
    }

    /// 是否是当前接收键盘输入的窗口
    pub fn is_focused(self) -> bool {
        self.state().is_some_and(|s| s.focused.get())
    }

    /// 是否全屏
    pub fn is_fullscreen(self) -> bool {
        self.state().is_some_and(|s| s.fullscreen.get())
    }

    /// 是否最小化了：[`WindowHandle::set_minimized`] 马上反映出来，用户从系统里最小化、还原时跟着窗口事件更新。
    /// Wayland 不告诉应用窗口最小化了没有，那里一直是最后一次 `set_minimized` 的值
    pub fn is_minimized(self) -> bool {
        self.state().is_some_and(|s| s.minimized.get())
    }

    /// 是否最大化了：[`WindowHandle::set_maximized`] 马上反映出来，用户从系统里最大化、还原时跟着窗口事件更新
    pub fn is_maximized(self) -> bool {
        self.state().is_some_and(|s| s.maximized.get())
    }

    /// 窗口所在显示器的颜色能力（广色域、HDR 的余量）和窗口此刻的输出方式。
    /// 窗口挪到别的显示器、调亮度、开始显示 HDR 内容时会变（每 0.5 秒问一次显示器）
    pub fn display_color(self) -> DisplayColor {
        self.state().map_or(DisplayColor::SDR, |s| s.display.get())
    }

    /// 隐藏了的标题栏（[`Window::title_bar`]）占了多少：高度、系统窗口按钮在哪、标题栏的内容要让开多少。
    /// 窗口缩放、进出全屏、挪了按钮时变。在窗口自己的视图里用 `aureole::title_bar_info()` 更方便（测试工具里也有）
    pub fn title_bar_info(self) -> TitleBarInfo {
        self.state().map_or_else(TitleBarInfo::default, |s| s.title_bar.get())
    }

    /// 窗口此刻的材质（[`Window::backdrop`]）。平台不支持、设不上时是 [`WindowMaterial::Opaque`]
    pub fn backdrop(self) -> WindowMaterial {
        self.state().map_or(WindowMaterial::Opaque, |s| s.backdrop.get())
    }

    /// 换窗口的材质。透明和不透明之间切换时窗口换清屏的颜色，下一帧生效。平台不支持、设不上时记一条警告，窗口保持原样。
    /// Windows 上打开窗口时没有设材质的，之后也设不上
    pub fn set_backdrop(self, backdrop: WindowMaterial) {
        request(Request::Backdrop(self, backdrop));
    }

    /// 改系统窗口按钮（macOS 的红绿灯）的位置，见 [`Window::title_bar_controls_position`]
    pub fn set_title_bar_controls_position(self, x: impl Number, y: impl Number) {
        let options = TitleBarOptions { controls_position: Some([x.to_f32(), y.to_f32()]) };
        self.with_window(move |w| {
            if let Err(error) = crate::system::backend().window().set_title_bar_options(w, options) {
                tracing::warn!("挪不了窗口按钮：{error}");
            }
        });
    }

    /// 关闭窗口（不经过 [`Window::on_close_request`]）
    pub fn close(self) {
        with_registry(|r| r.requests.push(Request::Close(self)));
        wake();
    }

    /// 把窗口带到最前面并获得焦点；最小化了先还原（macOS 上隐藏了的应用也先显示出来）
    pub fn focus(self) {
        self.with_window(|w| {
            if crate::system::backend().window().bring_to_front(w).is_err() {
                w.set_minimized(false);
                w.focus_window();
            }
        });
    }

    /// 改标题
    pub fn set_title(self, title: impl Into<String>) {
        let title = title.into();
        self.with_window(move |w| {
            w.set_title(&title);
            // 改标题时系统可能重新摆放标题栏
            crate::system::backend().window().refresh_title_bar(w);
        });
    }

    /// 改内容区大小，逻辑像素（整数、浮点数都行，同 [`Window::size`]）
    pub fn set_size(self, width: impl Number, height: impl Number) {
        let size = LogicalSize::new(width.to_f64(), height.to_f64());
        self.with_window(move |w| {
            let _ = w.request_inner_size(size);
        });
    }

    /// 把窗口左上角移到屏幕上的 (x, y)，逻辑像素（整数、浮点数都行，同 [`Window::position`]）
    pub fn set_position(self, x: impl Number, y: impl Number) {
        let position = LogicalPosition::new(x.to_f64(), y.to_f64());
        self.with_window(move |w| w.set_outer_position(position));
    }

    /// 进入或退出全屏
    pub fn set_fullscreen(self, fullscreen: bool) {
        self.with_window(move |w| w.set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None))));
        if let Some(state) = self.state() {
            state.fullscreen.set(fullscreen);
        }
    }

    /// 置顶：总在其他窗口上面
    pub fn set_always_on_top(self, on_top: bool) {
        self.with_window(move |w| w.set_window_level(if on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }));
    }

    /// 最小化或还原
    pub fn set_minimized(self, minimized: bool) {
        self.with_window(move |w| w.set_minimized(minimized));
        if let Some(state) = self.state() {
            state.minimized.set_if_changed(minimized);
        }
    }

    /// 最大化或还原
    pub fn set_maximized(self, maximized: bool) {
        // macOS 的缩放动画在调用里同步跑完，要在窗口库的事件回调外面开始，动画的每一步才收得到改大小的事件、出得了帧
        self.with_window(move |_| {
            crate::system::backend().window().run_outside_event_callback(Box::new(move || {
                let Some(window) = native(self) else { return };
                ZOOMING.with(|z| z.set(Some(self)));
                window.set_maximized(maximized);
                ZOOMING.with(|z| z.set(None));
            }));
        });
        if let Some(state) = self.state() {
            state.maximized.set_if_changed(maximized);
        }
    }

    /// 窗口已经创建就马上执行；还在排队就等创建后执行。
    /// 不在借用登记表的时候调用，闭包里可以再用窗口的方法
    fn with_window(self, f: impl FnOnce(&winit::window::Window) + 'static) {
        let mut f = Some(f);
        let window = with_registry(|r| {
            let entry = r.windows.get_mut(&self.0)?;
            match &entry.window {
                Some(window) => Some(window.clone()),
                None => {
                    entry.pending.extend(f.take().map(|f| Box::new(f) as Box<dyn FnOnce(&winit::window::Window)>));
                    None
                }
            }
        });
        if let (Some(window), Some(f)) = (window, f) {
            f(&window);
        }
    }
}

/// 正在构建视图、处理事件或推进帧的窗口。在 `Harness` 里是 `None`
pub fn current_window() -> Option<WindowHandle> {
    current_window_id().map(|id| WindowHandle(id.0))
}

/// 关闭所有窗口，退出应用
pub fn quit() {
    with_registry(|r| r.requests.push(Request::Quit));
    wake();
}

/// 窗口的响应式状态，信号放在登记表自己的作用域里，窗口关了也能读（读到最后的值）
#[derive(Clone, Copy)]
pub(crate) struct State {
    pub open: Signal<bool>,
    pub size: Signal<[f32; 2]>,
    pub position: Signal<[f32; 2]>,
    pub focused: Signal<bool>,
    pub fullscreen: Signal<bool>,
    pub minimized: Signal<bool>,
    pub maximized: Signal<bool>,
    pub display: Signal<DisplayColor>,
    pub title_bar: Signal<TitleBarInfo>,
    pub backdrop: Signal<WindowMaterial>,
}

pub(crate) enum Request {
    Open(WindowHandle, Window),
    Close(WindowHandle),
    Quit,
    /// 换窗口的材质
    Backdrop(WindowHandle, WindowMaterial),
    /// 隐藏了的标题栏的度量可能变了（平台通知过来）：重新读
    TitleBarChanged(WindowHandle),
}

/// 排一个窗口请求（事件循环下一轮处理）
pub(crate) fn request(request: Request) {
    with_registry(|r| r.requests.push(request));
    wake();
}

type Pending = Vec<Box<dyn FnOnce(&winit::window::Window)>>;

struct Entry {
    window: Option<Arc<winit::window::Window>>,
    state: State,
    /// 窗口创建前调用的设置，创建后补上
    pending: Pending,
}

#[derive(Default)]
struct Registry {
    next: u64,
    windows: HashMap<u64, Entry>,
    requests: Vec<Request>,
    /// 最近获得焦点的窗口
    last_focused: Option<WindowHandle>,
    scope: Option<Scope>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::default();
    /// 正在按 [`WindowHandle::set_maximized`] 缩放的窗口（macOS 上缩放动画同步跑完，期间不断收到改大小的事件）
    static ZOOMING: Cell<Option<WindowHandle>> = const { Cell::new(None) };
}

fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    REGISTRY.with(|r| f(&mut r.borrow_mut()))
}

/// 有了窗口请求：叫醒事件循环处理（事件循环还没开始时什么都不做，开始后先处理排着的请求）
fn wake() {
    if let Some(waker) = aureole_core::host::main_thread_waker() {
        waker.wake();
    }
}

pub(crate) fn take_requests() -> Vec<Request> {
    with_registry(|r| std::mem::take(&mut r.requests))
}

pub(crate) fn state(handle: WindowHandle) -> Option<State> {
    handle.state()
}

/// 窗口正在按 [`WindowHandle::set_maximized`] 缩放：动画的中间几步窗口还不算最大化，不要拿来改已经设好的 `is_maximized`
pub(crate) fn is_zooming(handle: WindowHandle) -> bool {
    ZOOMING.with(|z| z.get() == Some(handle))
}

/// 已经创建的窗口（给需要原生窗口的系统功能，比如作为文件对话框的父窗口）
pub(crate) fn native(handle: WindowHandle) -> Option<Arc<winit::window::Window>> {
    with_registry(|r| r.windows.get(&handle.0).and_then(|entry| entry.window.clone()))
}

/// 窗口获得或失去焦点
pub(crate) fn set_focused(handle: WindowHandle, focused: bool) {
    if focused {
        with_registry(|r| r.last_focused = Some(handle));
    }
    if let Some(state) = handle.state() {
        state.focused.set(focused);
    }
}

/// 该带到最前面的窗口：最近获得焦点、还开着的；都没获得过焦点时是最早打开的
pub(crate) fn front_window() -> Option<WindowHandle> {
    with_registry(|r| {
        let open = |h: &WindowHandle| r.windows.get(&h.0).is_some_and(|e| e.window.is_some());
        let earliest = r.windows.iter().filter(|(_, e)| e.window.is_some()).map(|(&id, _)| id).min().map(WindowHandle);
        r.last_focused.filter(open).or(earliest)
    })
}

/// 窗口创建好了：记下它，补上排队的设置
pub(crate) fn attach(handle: WindowHandle, window: Arc<winit::window::Window>) {
    let pending = with_registry(|r| {
        r.windows.get_mut(&handle.0).map(|entry| {
            entry.window = Some(window.clone());
            std::mem::take(&mut entry.pending)
        })
    });
    for f in pending.into_iter().flatten() {
        f(&window);
    }
}

/// 窗口关了：之后读到的是最后的状态，`is_open` 变成 `false`
pub(crate) fn detach(handle: WindowHandle) {
    let state = with_registry(|r| {
        r.windows.get_mut(&handle.0).map(|entry| {
            entry.window = None;
            entry.pending.clear();
            entry.state
        })
    });
    if let Some(state) = state {
        state.open.set(false);
        state.focused.set(false);
    }
}

/// 在 `f` 期间把 `handle` 当作当前窗口
pub(crate) fn with_current<R>(handle: WindowHandle, f: impl FnOnce() -> R) -> R {
    run_in_window(handle.id(), f)
}
