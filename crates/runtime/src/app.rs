use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use accesskit_winit::{Adapter as AccessibilityAdapter, WindowEvent as AccessibilityEvent};
use platform::{CompositionTarget, FrameClockOptions, FrameRateRange, FrameTime, TitleBar, TitleBarAction, WindowMaterial};
use reactive::{Effect, Scope};
use render::{Gpu, GpuPreference};
use scene::{Color, Viewport};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalSize};
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Fullscreen, WindowId, WindowLevel};

use aureole_core::host::{self, DragOutRequest, FrameReport, HostWindow, Recorder, RenderFrame, WindowHost, WindowTree};
use aureole_core::{Appearance, AppearanceMode, Cursor, ImePurpose, ImeRequest, IntoProp, Number, Prop, TitleBarInfo, View};

use crate::display_color::{DisplayColor, OutputState};
#[cfg(feature = "menus")]
use crate::menu::{self, Menu, Tray};
use crate::plugin::{FrameInfo, Hooks, PlatformEvent, Plugin};
use crate::open::{self, OpenRequest, SingleInstance};
use crate::pacing::{FramePacer, next_wake};
use crate::render_thread::{self, RenderMsg, RenderThread};
use crate::translate::{TranslateState, translate};
use crate::window::{self, Request, UserEvent, Window, WindowHandle};

/// 应用入口，同时描述主窗口：
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn app() -> impl View { rect() }
/// App::new(app).title("示例").size(960, 640).run();
/// ```
///
/// 更多窗口用 [`Window::open`] 打开。默认关掉最后一个窗口时退出应用。
///
/// 没有能用的 GPU 时（远程桌面、虚拟机、驱动坏了；选适配器的先后见 [`App::gpu`]），记一条错误说明原因（`tracing`，没装接收方时写到标准错误）；
/// 不是从终端启动时（双击打开的应用）在 macOS、Windows 上弹出提示框，然后以退出码 1 结束
pub struct App {
    main: Window,
    quits_on_last_window_closed: bool,
    #[cfg(feature = "menus")]
    menu: Option<Menu>,
    #[cfg(feature = "menus")]
    tray: Option<Tray>,
    gpu: GpuPreference,
    on_open: Vec<open::Handler>,
    single_instance: SingleInstance,
    /// 换上执行器（[`App::spawner`]）：在 [`App::run`] 里、主线程上装
    pub(crate) spawner: Option<Box<dyn FnOnce()>>,
    /// 还没装上的插件：[`App::run`] 开始时按顺序装
    plugins: Vec<Box<dyn Plugin>>,
    /// 插件和应用挂的钩子
    pub(crate) hooks: Hooks,
}

impl App {
    /// `build` 在主窗口创建后调用一次，返回根视图
    pub fn new<V: View>(build: impl FnOnce() -> V + 'static) -> Self {
        Self {
            main: Window::new(build),
            quits_on_last_window_closed: true,
            #[cfg(feature = "menus")]
            menu: None,
            #[cfg(feature = "menus")]
            tray: None,
            gpu: GpuPreference::LowPower,
            on_open: Vec::new(),
            single_instance: SingleInstance::Off,
            spawner: None,
            plugins: Vec::new(),
            hooks: Hooks::default(),
        }
    }

    /// 装一个插件（[`Plugin`]）：在 [`App::run`] 开始时（事件循环创建之前、主线程上）按装的顺序调用它的
    /// [`Plugin::build`]，插件在那里挂钩子、换执行器、改主窗口，也能再装别的插件。函数 `Fn(App) -> App` 也是插件
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # fn app() -> impl View { rect() }
    /// App::new(app).plugin(|app: App| app.title("例子")).run();
    /// ```
    pub fn plugin(mut self, plugin: impl Plugin) -> Self {
        self.plugins.push(Box::new(plugin));
        self
    }

    /// 窗口创建好以后调用（根视图挂好、渲染线程开始了），在主线程、在这个窗口里（`current_window` 就是它）。
    /// 每个窗口一次，主窗口是第一个
    pub fn on_window_open(mut self, handler: impl Fn(WindowHandle) + 'static) -> Self {
        self.hooks.window_open.push(Box::new(handler));
        self
    }

    /// 窗口系统交来的事件，在换算成输入事件、交给元素树之前，按挂的顺序调用（[`PlatformEvent`]）。
    /// 处理器用 [`PlatformEvent::stop_propagation`] 把它吃掉，元素树就收不到；窗口自己的处理（改大小、关闭请求……）照常
    pub fn on_platform_event(mut self, handler: impl Fn(&PlatformEvent) + 'static) -> Self {
        self.hooks.platform_event.push(Box::new(handler));
        self
    }

    /// 每个窗口每帧推进之前调用（[`FrameInfo`]，`stats` 是 `None`），在这个窗口里。
    /// 画面静止、帧时钟停着时不出帧，也不调用
    pub fn on_frame_start(mut self, handler: impl Fn(&FrameInfo) + 'static) -> Self {
        self.hooks.frame_start.push(Box::new(handler));
        self
    }

    /// 每个窗口每帧推进之后调用（[`FrameInfo`]，`stats` 是这一帧做了什么），在这个窗口里
    pub fn on_frame_end(mut self, handler: impl Fn(&FrameInfo) + 'static) -> Self {
        self.hooks.frame_end.push(Box::new(handler));
        self
    }

    /// 应用退出前调用一次（关掉最后一个窗口、[`crate::quit`]），这时窗口都已经关了，事件循环还没结束。
    /// 开发工具重新编译后原地换成新程序时进程直接换掉，不调用
    pub fn on_exit(mut self, handler: impl FnOnce() + 'static) -> Self {
        self.hooks.exit.get_mut().push(Box::new(handler));
        self
    }

    /// 按装的顺序装上插件（装的时候装的插件排在后面，接着装），直到装完
    pub(crate) fn build_plugins(mut self) -> Self {
        loop {
            let plugins = std::mem::take(&mut self.plugins);
            if plugins.is_empty() {
                return self;
            }
            for plugin in plugins {
                self = plugin.build(self);
            }
        }
    }

    /// 后台任务的执行器（[`aureole_core::Spawner`]）：[`aureole_core::spawn`]、[`aureole_core::spawn_blocking`]、
    /// `resource` 的请求都交给它。默认是内置的线程池（[`aureole_core::PoolSpawner`]）；
    /// 用 tokio 的生态时换成 `TokioSpawner`（`tokio` 特性），依赖 tokio 的 future 直接 `spawn` 就行。
    /// 主线程上的任务（`spawn_local`）、定时器、`resource` 交回主线程的那一半不受影响
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # fn app() -> impl View { rect() }
    /// # #[cfg(feature = "tokio")]
    /// App::new(app).spawner(aureole::TokioSpawner::default()).run();
    /// ```
    pub fn spawner(mut self, spawner: impl aureole_core::Spawner) -> Self {
        self.spawner = Some(Box::new(move || aureole_core::set_spawner(spawner)));
        self
    }

    /// 选 GPU 的偏好。默认 [`GpuPreference::LowPower`]：双显卡的机器上用集成显卡，不唤醒独立显卡，
    /// 界面应用用它足够快；大量 3D、很重的自定义着色器用 [`GpuPreference::HighPerformance`]。
    /// 环境变量 `AUREOLE_GPU`（`low-power`、`high-performance`、`software`）优先，方便排查驱动问题。
    ///
    /// 偏好的适配器用不了时依次退到：另一种显卡 → 软件渲染（Windows 的 WARP、Linux 的 lavapipe）→
    /// 次要后端（GL）上的显卡和软件渲染（llvmpipe）。选中的适配器见 [`crate::performance::gpu_info`]
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # fn app() -> impl View { rect() }
    /// App::new(app).gpu(GpuPreference::HighPerformance).run();
    /// ```
    pub fn gpu(mut self, preference: GpuPreference) -> Self {
        self.gpu = preference;
        self
    }

    /// 主窗口标题，可以绑定信号；也用作应用菜单的名字
    pub fn title<S: Into<String> + 'static>(mut self, title: impl IntoProp<S>) -> Self {
        self.main = self.main.title(title);
        self
    }

    /// 主窗口初始大小，逻辑像素；整数、浮点数都行（`.size(960, 640)`），同 [`Window::size`]
    pub fn size(mut self, width: impl Number, height: impl Number) -> Self {
        self.main = self.main.size(width, height);
        self
    }

    /// 主窗口的背景色。不设置时是调色板的窗口底色（跟着应用的外观）
    pub fn background(mut self, color: Color) -> Self {
        self.main = self.main.background(color);
        self
    }

    /// 应用的外观：跟着系统（默认），或者固定成浅色、深色，同 [`aureole_core::set_appearance_mode`]（运行中也能换）。
    /// 框架自带的样子（窗口底色、默认的文字颜色、基础控件、窗口的标题栏和材质）按它画
    pub fn appearance_mode(self, mode: AppearanceMode) -> Self {
        aureole_core::set_appearance_mode(mode);
        self
    }

    /// 主窗口的标题栏：内容铺满整个窗口、自己画标题栏，同 [`Window::title_bar`]
    pub fn title_bar(mut self, title_bar: platform::TitleBar) -> Self {
        self.main = self.main.title_bar(title_bar);
        self
    }

    /// 主窗口背后透出的系统材质，同 [`Window::backdrop`]
    pub fn backdrop(mut self, backdrop: WindowMaterial) -> Self {
        self.main = self.main.backdrop(backdrop);
        self
    }

    /// 开发时用：重新编译（`cargo build` 换掉了可执行文件）后自动换成新程序，主窗口留在原来的位置和大小，
    /// [`crate::restart::persist`] 登记的信号保留原来的值。见 [`crate::restart`]。
    /// 就是装上只开这一项的热加载插件（[`crate::restart::HotReloadPlugin`]）。要打开 `devtools` 特性
    #[cfg(feature = "devtools")]
    pub fn restarts_on_rebuild(self, enabled: bool) -> Self {
        if enabled { self.plugin(crate::restart::HotReloadPlugin::new().reloads_images(false)) } else { self }
    }

    /// 对主窗口做更多设置
    pub fn window(mut self, f: impl FnOnce(Window) -> Window) -> Self {
        self.main = f(self.main);
        self
    }

    /// 关掉最后一个窗口时是否退出（默认是）；只有托盘的应用设成 `false`，用 [`crate::quit`] 退出
    pub fn quits_on_last_window_closed(mut self, quit: bool) -> Self {
        self.quits_on_last_window_closed = quit;
        self
    }

    /// 菜单栏（macOS 屏幕顶部，Windows 窗口标题栏下面）。第一项不是 [`crate::Submenu::app`] 时，
    /// macOS 上自动补上应用菜单；不设菜单栏时 macOS 上也有一个只有应用菜单的菜单栏。
    /// 应用菜单里的应用名：打包成 .app 时用 Info.plist 里的名字，没打包时用主窗口的标题。要打开 `menus` 特性（默认开）
    #[cfg(feature = "menus")]
    pub fn menu(mut self, menu: Menu) -> Self {
        self.menu = Some(menu);
        self
    }

    /// 托盘图标。要打开 `menus` 特性（默认开）
    #[cfg(feature = "menus")]
    pub fn tray(mut self, tray: Tray) -> Self {
        self.tray = Some(tray);
        self
    }

    /// 应用被要求打开网址（自定义协议，如 `myapp://note/42`）或文件时调用，在主线程、批量更新里：
    ///
    /// - 启动时：命令行参数里的网址和存在的文件（以 `-` 开头的选项不算）；macOS 打包后系统为了打开它们而启动应用时交来的；
    /// - 运行中：macOS 打包后系统交来的（在访达里打开文档、点了链接）；开了 [`App::single_instance`] 时再次启动转来的。
    ///
    /// 主窗口建好之后才调用（之前到的先排队），所以处理器里可以直接写界面绑定的信号、打开新窗口。
    /// 协议和文档类型要在打包设置里声明（`url-schemes`、`document-types`，见 `cargo bundle`）。
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # use aureole::OpenSource;
    /// # fn app() -> impl View { rect() }
    /// App::new(app)
    ///     .single_instance(true)
    ///     .on_open(|request| {
    ///         if let OpenSource::SecondInstance(args) = &request.source {
    ///             println!("又启动了一次，参数 {args:?}");
    ///         }
    ///         for url in &request.urls {
    ///             println!("打开 {url}");
    ///         }
    ///     })
    ///     .run();
    /// ```
    pub fn on_open(mut self, handler: impl Fn(&OpenRequest) + 'static) -> Self {
        self.on_open.push(Rc::new(handler));
        self
    }

    /// 单实例：同一个用户只运行一个。再次启动时，新进程把命令行参数（和里面的网址、文件）转给正在运行的实例，
    /// 然后退出；正在运行的实例把最近用过的窗口带到最前面，再把请求交给 [`App::on_open`]
    /// （[`crate::OpenSource::SecondInstance`]）。
    ///
    /// `true` 时标识是可执行文件名（打包的和 `cargo run` 的一样），也可以给一个字符串当标识（发布的应用最好给，
    /// 比如包标识符：别的应用的可执行文件可能重名）。用每个用户自己的本地套接字（Windows 是命名管道）和一把锁实现：
    /// 两个同时启动也只有一个留下；上次崩溃留下的套接字文件会被清掉。
    /// macOS 上从访达、`open` 启动打包的应用时系统本来就只开一个（之后的文档、网址交给它，是 [`crate::OpenSource::System`]），
    /// 这个设置管的是直接运行可执行文件（`cargo run`、终端里）、`open -n` 和同一个应用的多份拷贝。
    /// 在 [`App::run`] 里、创建事件循环之前决定
    pub fn single_instance(mut self, single: impl Into<SingleInstance>) -> Self {
        self.single_instance = single.into();
        self
    }

    /// 创建主窗口并运行事件循环，直到应用退出。必须在主线程调用。
    /// 开了 [`App::single_instance`] 而已经有实例在运行时，把参数转给它后退出进程，不返回
    pub fn run(mut self) {
        crate::performance::startup_mark("进入 App::run");
        // 插件最先装：它们可能换执行器、改主窗口、挂钩子
        self = self.build_plugins();
        // 应用和插件都没装订阅者：调试构建里框架的警告打到终端
        crate::debug_log::install();
        if let Some(install) = self.spawner.take() {
            install();
        }
        // 单实例、命令行参数里的网址和文件：要在创建事件循环之前
        let _instance = open::startup(&self.single_instance, std::mem::take(&mut self.on_open));
        let start = Instant::now();
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        #[cfg(feature = "menus")]
        let menu = self.menu;
        #[cfg(all(feature = "menus", target_os = "macos"))]
        let menu = {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            // 总是用自己的菜单栏：没设菜单栏时也装一个只有应用菜单的，系统项按用户的语言写、
            // 带上应用名（winit 默认的菜单是英文的，用的是可执行文件名）。关掉 `menus` 特性时用 winit 默认的
            builder.with_default_menu(false);
            Some(menu.unwrap_or_default())
        };
        #[cfg(all(feature = "menus", target_os = "windows"))]
        {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_msg_hook(|msg| platform::native().translate_menu_accelerator(msg));
        }
        let event_loop = builder.build().expect("创建事件循环失败");
        crate::performance::startup_step("建事件循环", start);
        let proxy = event_loop.create_proxy();
        // 主线程的收件箱从空变成非空时（后台任务做完、图片解码好、文件读好、别的线程投递了工作）叫醒事件循环
        let wake_proxy = proxy.clone();
        aureole_core::host::set_main_thread_waker(aureole_core::host::MainThreadWaker::new(move || {
            let _ = wake_proxy.send_event(UserEvent::Wake);
        }));
        open::attach();
        // 系统设置、剪贴板、打开网址交给平台；设置的信号在所有作用域之外建好，初值问系统
        let start = Instant::now();
        crate::system::install();
        let _ = (aureole_core::system_appearance(), aureole_core::is_motion_reduced(), aureole_core::system_accent_color());
        // Windows、Linux：后台线程监视系统设置（外观、减少动态效果、强调色），变了投递到主线程重新读
        let main = aureole_core::main_thread_sender();
        crate::system::backend().appearance().watch_settings(Box::new(move || {
            main.send(settings_changed);
        }));
        let app_name = crate::system::backend().environment().bundle_name().unwrap_or_else(|| match &self.main.title {
            Prop::Static(title) => title.clone(),
            Prop::Dynamic(f) => f(),
        });
        // Linux 的通知里显示应用名
        crate::system::backend().notification().set_app_name(&app_name);
        crate::performance::startup_step("系统设置、外观、应用名", start);
        let start = Instant::now();
        render::set_pipeline_cache_dir(pipeline_cache_dir());
        let instance = render::instance_with_display(event_loop.owned_display_handle());
        crate::performance::startup_step("GPU 实例", start);
        let mut runner = Runner {
            #[cfg(feature = "menus")]
            pending_menu: Some((menu, self.tray, app_name)),
            proxy,
            main: Some(self.main),
            hooks: Rc::new(self.hooks),
            quits_on_last_window_closed: self.quits_on_last_window_closed,
            windows: HashMap::new(),
            ids: HashMap::new(),
            instance,
            gpu: None,
            gpu_preference: GpuPreference::from_env().unwrap_or(self.gpu),
            warned_low_power: false,
            record: std::env::var_os("AUREOLE_RECORD").map(PathBuf::from),
        };
        event_loop.run_app(&mut runner).expect("事件循环出错");
    }
}

/// 编译好的管线存在哪（后端支持时，见渲染器的 `set_pipeline_cache_dir`）：用户的缓存目录下，按可执行文件的名字分开
fn pipeline_cache_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.file_stem()?.to_owned();
    Some(platform::user_cache_dir()?.join(app).join("gpu-pipelines"))
}

/// 菜单栏、托盘、应用名
#[cfg(feature = "menus")]
type PendingMenu = (Option<Menu>, Option<Tray>, String);

/// 系统设置（外观、减少动态效果、强调色）变了（后台线程投递过来）：重新读，托盘图标按新的深浅重画
fn settings_changed() {
    crate::system::settings_changed();
    #[cfg(feature = "menus")]
    menu::refresh_appearance();
}

struct Runner {
    /// 事件循环开始后装上菜单栏和托盘
    #[cfg(feature = "menus")]
    pending_menu: Option<PendingMenu>,
    /// 无障碍适配器、帧时钟的后台线程叫醒事件循环用
    proxy: EventLoopProxy<UserEvent>,
    main: Option<Window>,
    /// 插件和应用挂的钩子
    hooks: Rc<Hooks>,
    quits_on_last_window_closed: bool,
    windows: HashMap<WindowId, OpenWindow>,
    ids: HashMap<WindowHandle, WindowId>,
    instance: wgpu::Instance,
    /// 所有窗口共用一个 GPU 设备，第一个窗口创建时选适配器
    gpu: Option<Arc<Gpu>>,
    gpu_preference: GpuPreference,
    warned_low_power: bool,
    /// 环境变量 `AUREOLE_RECORD`：第一个打开的窗口把收到的输入录进这个文件
    record: Option<PathBuf>,
}

/// 一个打开的窗口。主线程：元素树、布局、输入；渲染线程：上传、绘制、呈现
struct OpenWindow {
    handle: WindowHandle,
    /// 帧时钟和空闲暂停。时钟必须在窗口之前释放（[`OpenWindow::shutdown`] 先放掉它）
    pacer: FramePacer,
    /// 元素树和它唯一的输入入口、帧管线（和测试工具共用）；窗口这一半见 [`WinitWindow`]
    host: Rc<WindowHost<WinitWindow>>,
    scope: Scope,
    render_thread: Option<JoinHandle<()>>,
    /// 窗口事件换算成输入事件的状态（修饰键、指针、拖进来的文件）
    translate: Rc<RefCell<TranslateState>>,
    on_close_request: Option<crate::window::OnCloseRequest>,
}

/// 真实窗口这一半：渲染线程、系统输入法和指针、无障碍适配器、表面的颜色输出
struct WinitWindow {
    handle: WindowHandle,
    window: Arc<winit::window::Window>,
    sender: Sender<RenderMsg>,
    /// 上次交给系统的文字输入（`None` 时没有元素要文字输入）
    ime: Cell<Option<ImeRequest>>,
    /// 窗口在前面（有键盘焦点）：密码框的安全输入只在这时开着
    focused: Cell<bool>,
    /// 打开了系统的安全输入
    secure: Cell<bool>,
    /// 上次设置的指针形状
    cursor: Cell<Cursor>,
    /// 无障碍适配器：辅助技术来要树之前什么都不做
    accessibility: RefCell<AccessibilityAdapter>,
    /// 表面的颜色输出：显示器的能力、画面里有没有扩展范围的内容（每帧在 [`HostWindow::submit`] 里更新）
    output: RefCell<OutputState>,
    /// 设的背景色：材质换了时按它换清屏的颜色
    background: Option<Color>,
    /// 此刻的材质
    backdrop: Cell<WindowMaterial>,
    /// 表面能带透明（有预乘透明度的模式）
    can_be_transparent: bool,
    /// 系统合成的对象（Windows，有材质的窗口的表面建在它上面）：和窗口同生共死
    composition: Option<Box<dyn CompositionTarget>>,
    /// 隐藏了标题栏
    hidden_title_bar: bool,
    /// 这次输入交给系统拖动窗口了：换算的状态里按着的键要清掉
    window_dragged: Cell<bool>,
    /// Linux：指针在窗口边缘（指针形状是调整大小的）
    #[cfg(target_os = "linux")]
    on_resize_edge: Cell<bool>,
}

impl WinitWindow {
    fn output(&self) -> Output<'_> {
        Output { state: &self.output, window: &self.window, handle: self.handle }
    }

    /// 换窗口的材质（[`crate::WindowHandle::set_backdrop`]）：透明和不透明之间换了时告诉渲染线程。设不上时记一条警告，保持原样
    fn set_backdrop(&self, backdrop: WindowMaterial) {
        let last = self.backdrop.get();
        if last == backdrop {
            return;
        }
        if !backdrop.is_opaque() && !self.can_be_transparent {
            tracing::warn!("这个窗口的表面不能透明，设不上窗口材质（Windows 上要在打开窗口时就设）");
            return;
        }
        if let Err(error) = crate::system::backend().window().set_window_backdrop(&*self.window, backdrop) {
            tracing::warn!("设不上窗口材质：{error}");
            return;
        }
        self.backdrop.set(backdrop);
        if let Some(state) = window::state(self.handle) {
            state.backdrop.set_if_changed(backdrop);
        }
        if last.is_opaque() != backdrop.is_opaque() {
            let background = crate::title_bar::clear_color(self.background, backdrop);
            let _ = self.sender.send(RenderMsg::Backdrop { transparent: !backdrop.is_opaque(), background });
        }
    }

    /// 应用的外观（`mode` 是设置，`appearance` 是生效的）：窗口库的主题管标题栏（跟着系统时交给系统），
    /// 平台层管 Windows 的外框和材质的深浅；没设背景色的不透明窗口换清屏的颜色
    fn apply_appearance(&self, mode: AppearanceMode, appearance: Appearance) {
        self.window.set_theme(winit_theme(mode));
        let backdrop = self.backdrop.get();
        if let Err(error) = crate::system::backend().window().set_window_dark(&*self.window, appearance.is_dark(), backdrop) {
            tracing::debug!("设不上窗口外框的深浅：{error}");
        }
        if self.background.is_none() && backdrop.is_opaque() {
            let background = crate::title_bar::clear_color(None, backdrop);
            let _ = self.sender.send(RenderMsg::Backdrop { transparent: false, background });
        }
    }

    /// 密码框有焦点、窗口在前面时打开系统的安全输入，否则关上（只有 macOS 有，别的平台什么都不做）
    fn update_secure_input(&self) {
        let wanted = self.focused.get() && self.ime.get().is_some_and(|r| r.purpose == ImePurpose::Password);
        if self.secure.get() != wanted && platform::set_secure_input(wanted) {
            self.secure.set(wanted);
        }
    }
}

/// 开不开输入法：有元素要文字输入、不是密码
fn ime_allowed(request: Option<ImeRequest>) -> bool {
    request.is_some_and(|r| r.purpose != ImePurpose::Password)
}

/// 外观设置换成窗口库的主题：跟着系统时不指定，交给系统
fn winit_theme(mode: AppearanceMode) -> Option<winit::window::Theme> {
    match mode {
        AppearanceMode::Light => Some(winit::window::Theme::Light),
        AppearanceMode::Dark => Some(winit::window::Theme::Dark),
        _ => None,
    }
}

/// 用途换成窗口库的（窗口库没有数字这一种，按普通的）
fn winit_purpose(purpose: ImePurpose) -> winit::window::ImePurpose {
    match purpose {
        ImePurpose::Password => winit::window::ImePurpose::Password,
        ImePurpose::Terminal => winit::window::ImePurpose::Terminal,
        _ => winit::window::ImePurpose::Normal,
    }
}

impl Drop for WinitWindow {
    /// 窗口关了：安全输入还给系统
    fn drop(&mut self) {
        if self.secure.get() {
            platform::set_secure_input(false);
        }
    }
}

impl HostWindow for WinitWindow {
    fn scale_factor(&self) -> f32 {
        self.window.scale_factor() as f32
    }

    fn submit(&self, _now: f64, frame: Option<RenderFrame>) {
        match frame {
            Some(RenderFrame { scene, layers, atlas }) => {
                // 表面的颜色输出要在这一帧的内容之前定下来：第一次出现广色域的颜色时就画在扩展范围的表面上
                self.output().update(scene.as_ref().map(|s| s.list.extended), &self.sender);
                let _ = self.sender.send(RenderMsg::Frame { scene, layers, atlas });
            }
            None => self.output().update(None, &self.sender),
        }
    }

    /// 要文字输入的元素有焦点时打开系统输入法，候选框跟着插入点走；用途变了告诉系统（Wayland 的屏幕键盘用它）。
    /// 密码不开输入法，窗口在前面时打开系统的安全输入
    fn set_ime(&self, ime: Option<ImeRequest>) {
        let last = self.ime.replace(ime);
        if ime_allowed(last) != ime_allowed(ime) {
            self.window.set_ime_allowed(ime_allowed(ime));
        }
        if let Some(r) = ime
            && last.map(|l| l.purpose) != Some(r.purpose)
        {
            self.window.set_ime_purpose(winit_purpose(r.purpose));
        }
        if let Some(r) = ime.filter(|_| ime_allowed(ime))
            && last.map(|l| l.area) != Some(r.area)
        {
            let a = r.area;
            self.window.set_ime_cursor_area(LogicalPosition::new(a.x, a.y), LogicalSize::new(a.width, a.height));
        }
        self.update_secure_input();
    }

    /// 放弃了组字：关一次再开一次输入法，平台丢掉它那边没确定的字（macOS 上关输入法时会同步送回"输入法关闭"，
    /// 窗口宿主排队处理）。输入法没开着时什么都不用做
    fn reset_ime(&self) {
        if let Some(r) = self.ime.get().filter(|r| r.purpose != ImePurpose::Password) {
            let a = r.area;
            self.window.set_ime_allowed(false);
            self.window.set_ime_allowed(true);
            self.window.set_ime_cursor_area(LogicalPosition::new(a.x, a.y), LogicalSize::new(a.width, a.height));
        }
    }

    fn set_cursor(&self, cursor: Cursor) {
        if self.cursor.replace(cursor) != cursor {
            self.window.set_cursor(cursor_icon(cursor));
        }
    }

    fn accessibility(&self, update: accesskit::TreeUpdate) {
        self.accessibility.borrow_mut().update_if_active(|| update);
    }

    /// 交给系统的拖动会话，结束时的结果下一帧交给源元素；开始不了（其他平台）时直接以 `false` 结束
    fn begin_drag_out(&self, request: DragOutRequest) {
        let at = request.at;
        // 系统的回调里不碰元素树：结果写进请求带的格子，下一帧交给源元素
        let result = request.result.clone();
        let on_end = Box::new(move |accepted| result.set(Some(accepted)));
        let (files, text) = (request.out.paths(), request.out.text_content());
        if let Err(error) = crate::system::backend().window().begin_drag_out(&*self.window, files, text, at, on_end) {
            tracing::debug!("拖不到应用外面：{error}");
            request.result.set(Some(false));
        }
    }

    fn focus_changed(&self, focused: bool) {
        window::set_focused(self.handle, focused);
        // 切到别的应用时还回安全输入，回来时（密码框还有焦点）再打开
        self.focused.set(focused);
        self.update_secure_input();
        // "减少动态效果"在系统设置里改，回到应用时重新读
        if focused {
            crate::system::refresh_settings();
        }
    }

    /// 指针按在拖动区域上拖动了：交给系统的移动循环（macOS 上循环结束才返回，期间显示链接照常出帧）
    fn begin_window_drag(&self) {
        self.window_dragged.set(true);
        if let Err(error) = self.window.drag_window() {
            tracing::debug!("拖不动窗口：{error}");
        }
    }

    /// 双击了拖动区域：按系统设置缩放或最小化窗口
    fn perform_title_bar_double_click(&self) {
        match crate::system::backend().window().title_bar_action() {
            TitleBarAction::Zoom => self.handle.set_maximized(!self.window.is_maximized()),
            TitleBarAction::Minimize => self.handle.set_minimized(true),
            _ => {}
        }
    }

    fn set_maximize_button_area(&self, area: Option<aureole_core::Rect>) {
        let area = area.map(|r| [r.x, r.y, r.width, r.height]);
        crate::system::backend().window().set_maximize_button_area(&*self.window, area);
    }
}

impl ApplicationHandler<UserEvent> for Runner {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            crate::performance::woke_on_schedule();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(theme) = event_loop.system_theme() {
            aureole_core::host::set_system_appearance(crate::system::reported_appearance(theme));
        }
        #[cfg(feature = "menus")]
        if let Some((menu, tray, app_name)) = self.pending_menu.take()
            && (menu.is_some() || tray.is_some())
        {
            menu::install(menu, tray, &app_name);
        }
        if let Some(main) = self.main.take() {
            main.open();
        }
        self.process(event_loop);
        // macOS 上没打包（`cargo run`、从终端运行）时系统不把这个进程当前台应用，窗口拿不到键盘：设成前台应用并激活。
        // 打包后由系统管，后端什么都不做
        if crate::system::backend().window().activate_if_unpackaged() {
            tracing::debug!("没有打包成 .app：把应用设成前台应用并激活");
        }
        // 主窗口建好了：交出启动时排着的打开请求（处理器里可能打开窗口）
        open::set_ready();
        self.process(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        // 收件箱里有事（投递的工作在 `about_to_wait` 里执行）、无障碍请求：界面可能要变。
        // 帧时钟送来的帧只要交出去（在 `about_to_wait` 里），不恢复别的窗口的时钟
        if !matches!(event, UserEvent::Frame) {
            self.wake_all();
        }
        match event {
            UserEvent::Wake | UserEvent::Frame => {}
            UserEvent::Accessibility(event) => self.accessibility(event),
            UserEvent::Window(id, event) => self.window_event(event_loop, id, event),
        }
        self.process(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CursorMoved { .. }) && self.is_wheel_echo(id) {
            return;
        }
        // 插件先看：吃掉的不再换算成输入事件
        let stopped = match self.windows.get(&id) {
            Some(w) => {
                w.wake_up();
                window::with_current(w.handle, || self.hooks.platform_event(w.handle, &event))
            }
            None => false,
        };
        if let WindowEvent::CloseRequested = event {
            let veto = self.windows.get(&id).and_then(|w| w.on_close_request.clone());
            let close = match (veto, self.windows.get(&id)) {
                (Some(ask), Some(w)) => {
                    let request = window::CloseRequest::default();
                    window::with_current(w.handle, || ask(&request));
                    !request.is_builtin_prevented(aureole_core::Builtin::CLOSE)
                }
                _ => true,
            };
            if close {
                self.close(event_loop, id);
            }
        } else if let Some(w) = self.windows.get_mut(&id) {
            let handle = w.handle;
            window::with_current(handle, || w.handle_event(event, stopped));
        }
        self.process(event_loop);
    }

    /// 应用退出前：插件和应用挂的退出钩子
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.hooks.exit();
    }

    /// 一轮事件处理完：执行就绪的异步任务和到期的定时器，放下拖进来的文件，把帧时钟送到、到期的帧交出去，
    /// 暂停空闲的时钟，然后睡到最早的截止时间（时钟、定时器、问显示器）；都没有时一直睡，直到有事件
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        crate::performance::woke();
        let ran = host::run_ready_tasks();
        // 渲染线程出错后交回来的（投递过来，刚执行过）：重建
        self.restart_render_threads();
        let (fired, next_timer) = host::run_due_timers();
        if ran || fired {
            self.wake_all();
        }
        self.process(event_loop);
        let now = Instant::now();
        for w in self.windows.values_mut() {
            window::with_current(w.handle, || w.drop_files());
            // 计时器、等刷新的后台线程送来的帧在这里交（显示链接的帧由系统的运行循环直接回调）
            w.pacer.dispatch(now);
        }
        // 连续几帧什么都没做的窗口暂停时钟：画面静止时不再被帧唤醒
        for w in self.windows.values() {
            w.maybe_sleep();
        }
        // 时钟停着、又在用扩展范围的表面的窗口：隔一会儿问一次显示器（调亮度时 HDR 的余量会变）
        let now = Instant::now();
        let next_poll = self.windows.values().filter_map(|w| w.poll_display()).min();
        let clocks = self.windows.values().map(|w| w.pacer.deadline());
        let wake = next_wake(clocks.chain([next_timer.map(|d| now + d), next_poll.map(|d| now + d)]));
        event_loop.set_control_flow(match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
        self.process(event_loop);
    }
}

impl Runner {
    /// 这个指针移动是不是滚轮事件带来的、可以不要的。
    ///
    /// macOS 上窗口库处理每个滚轮事件时，先按滚轮事件自己带的位置补发一个指针移动。这个位置可能是旧的：滚轮一直滚着，
    /// 指针移到另一个元素上，真的移动说在新位置，补发的说在旧位置，悬停就在两个元素之间来回跳。
    ///
    /// 有键盘焦点的窗口收得到真的移动，不缺这个；没有的窗口收不到，滚轮带来的位置是它知道指针在哪的唯一来源，要留着：
    /// 不然滚后台窗口时滚的是指针上次离开时的位置下面的东西。
    fn is_wheel_echo(&self, id: WindowId) -> bool {
        crate::system::backend().window().handling_wheel_event()
            && self.windows.get(&id).is_some_and(|w| w.window().has_focus())
    }

    /// 所有窗口恢复帧时钟
    fn wake_all(&self) {
        for w in self.windows.values() {
            w.wake_up();
        }
    }

    /// 渲染线程出错（panic）后交回来的窗口：重建它的渲染线程（见 `render.rs`）；
    /// 同一个窗口短时间内反复出错时按没有可用的 GPU 处理（说明原因后退出）
    fn restart_render_threads(&mut self) {
        for crashed in render_thread::take_crashed() {
            // 窗口已经关了：交回来的东西直接丢掉
            let Some(w) = self.ids.get(&crashed.window()).and_then(|id| self.windows.get_mut(id)) else { continue };
            // 出错的线程交回以后就结束了
            if let Some(thread) = w.render_thread.take() {
                let _ = thread.join();
            }
            match crashed.restart() {
                Ok(thread) => {
                    w.render_thread = Some(thread);
                    // 帧时钟可能停着：下一帧把图集和图片的完整内容交给新的渲染线程
                    w.wake_up();
                }
                Err(reason) => gpu_failed(&reason),
            }
        }
    }

    /// 处理窗口请求（打开、关闭、退出）。处理器里可能又发出请求，处理到没有为止
    fn process(&mut self, event_loop: &ActiveEventLoop) {
        loop {
            let requests = window::take_requests();
            if requests.is_empty() {
                return;
            }
            for request in requests {
                match request {
                    Request::Open(handle, options) => self.open(event_loop, handle, options),
                    Request::Close(handle) => {
                        if let Some(&id) = self.ids.get(&handle) {
                            self.close(event_loop, id);
                        }
                    }
                    Request::Quit => {
                        cancel_drag_out();
                        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
                        for id in ids {
                            self.close(event_loop, id);
                        }
                        event_loop.exit();
                    }
                    Request::Backdrop(handle, backdrop) => {
                        if let Some(w) = self.ids.get(&handle).and_then(|id| self.windows.get(id)) {
                            w.host.window().set_backdrop(backdrop);
                        }
                    }
                    Request::TitleBarChanged(handle) => {
                        if let Some(w) = self.ids.get(&handle).and_then(|id| self.windows.get(id)) {
                            w.refresh_title_bar();
                        }
                    }
                }
            }
        }
    }

    fn open(&mut self, event_loop: &ActiveEventLoop, handle: WindowHandle, options: Window) {
        let Window {
            build,
            title,
            size,
            position,
            min_size,
            resizable,
            always_on_top,
            fullscreen,
            decorations,
            background,
            on_close_request,
            color_output,
            title_bar,
            title_bar_options,
            backdrop,
        } = options;
        let initial_title = match &title {
            Prop::Static(title) => title.clone(),
            Prop::Dynamic(f) => f(),
        };
        let mut attributes = winit::window::Window::default_attributes()
            .with_title(initial_title.clone())
            .with_inner_size(LogicalSize::new(size[0], size[1]))
            .with_resizable(resizable)
            .with_decorations(decorations)
            .with_window_level(if always_on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal })
            .with_theme(winit_theme(reactive::untracked(aureole_core::appearance_mode)))
            // 无障碍适配器要在窗口第一次显示之前建好
            .with_visible(false);
        // 打包时嵌进可执行文件的图标（`cargo bundle`，资源编号 1）：标题栏、任务栏显示它。没有这个资源时用系统默认的
        #[cfg(target_os = "windows")]
        if let Ok(icon) = <winit::window::Icon as winit::platform::windows::IconExtWindows>::from_resource(1, None) {
            attributes = attributes.with_window_icon(Some(icon.clone()));
            attributes = winit::platform::windows::WindowAttributesExtWindows::with_taskbar_icon(attributes, Some(icon));
        }
        // 桌面靠这个名字把窗口和 `<名字>.desktop` 对上（任务栏的图标、分组）：安装包里的 .desktop 用可执行文件名
        #[cfg(target_os = "linux")]
        if let Some(name) = std::env::current_exe().ok().and_then(|exe| exe.file_stem().map(|stem| stem.to_string_lossy().into_owned())) {
            attributes = winit::platform::x11::WindowAttributesExtX11::with_name(attributes, name.clone(), name);
        }
        if let Some([x, y]) = position {
            attributes = attributes.with_position(LogicalPosition::new(x, y));
        }
        if let Some([w, h]) = min_size {
            attributes = attributes.with_min_inner_size(LogicalSize::new(w, h));
        }
        if fullscreen {
            attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        // 一个窗口只能登记一个拖放目标：文件也由平台层接（见 `external_drag.rs`），窗口库的关掉
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes = attributes.with_drag_and_drop(false);
        }
        let hidden_title_bar = title_bar == TitleBar::Hidden;
        let attributes = crate::title_bar::window_attributes(attributes, title_bar, backdrop);
        let start = Instant::now();
        let window = Arc::new(event_loop.create_window(attributes).expect("创建窗口失败"));
        // macOS 上窗口库建窗口时把这个位置当成内容区的左上角，`position()`、`set_position` 用的却是整个窗口的：
        // 显示之前按整个窗口再放一次，存下的位置下次照样放回去，不会每次往上漂一个标题栏
        if let Some([x, y]) = position {
            window.set_outer_position(LogicalPosition::new(x, y));
        }
        #[cfg(feature = "menus")]
        if decorations && !hidden_title_bar {
            menu::attach_window(&window);
        }
        if hidden_title_bar {
            attach_title_bar(handle, &window, title_bar_options);
        }
        // 外框的深浅要在设材质之前写上（Windows 的材质默认按浅色画）
        let dark = reactive::untracked(aureole_core::appearance).is_dark();
        if let Err(error) = crate::system::backend().window().set_window_dark(&*window, dark, WindowMaterial::Opaque) {
            tracing::debug!("设不上窗口外框的深浅：{error}");
        }
        // 材质视图要在表面的图层之前放好（在它下面）
        let mut backdrop = backdrop;
        if !backdrop.is_opaque()
            && let Err(error) = crate::system::backend().window().set_window_backdrop(&*window, backdrop)
        {
            tracing::warn!("设不上窗口材质，照常画不透明的背景：{error}");
            backdrop = WindowMaterial::Opaque;
        }
        let accessibility = AccessibilityAdapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone());
        window.set_visible(true);
        crate::performance::startup_step("建窗口、无障碍适配器", start);

        let start = Instant::now();
        let mut surface = self.instance.create_surface(window.clone()).expect("创建 surface 失败");
        crate::performance::startup_step("建表面", start);
        let start = Instant::now();
        // 第一个窗口选适配器；之后的窗口共用（设备丢失过时用重新创建的那个）
        let gpu = match self.gpu.clone() {
            Some(gpu) if !gpu.is_lost() => Ok(gpu),
            Some(lost) => lost.recover(Some(&surface)),
            None => Gpu::request(&self.instance, Some(&surface), self.gpu_preference).map(Arc::new),
        };
        let gpu = gpu.unwrap_or_else(|error| {
            window.set_visible(false);
            gpu_failed(&error)
        });
        self.gpu = Some(gpu.clone());
        crate::performance::startup_step("选 GPU、要设备", start);

        // 有材质的窗口：表面要能带透明。Windows 上 HWND 上的交换链只能不透明，换成系统合成的对象上的；
        // 这样也不行（用的不是 DX12）时照常画不透明的背景
        let mut composition: Option<Box<dyn CompositionTarget>> = None;
        if !backdrop.is_opaque()
            && let Some((composition_surface, target)) = composition_surface(&self.instance, &gpu, &window)
        {
            surface = composition_surface;
            composition = Some(target);
        }
        let caps = surface.get_capabilities(&gpu.adapter);
        let can_be_transparent = render::transparent_alpha_mode(&caps).is_some();
        if !backdrop.is_opaque() && !can_be_transparent {
            tracing::warn!("这个窗口的表面不能透明，照常画不透明的背景");
            let _ = crate::system::backend().window().set_window_backdrop(&*window, WindowMaterial::Opaque);
            backdrop = WindowMaterial::Opaque;
        }

        let start = Instant::now();
        let viewport = physical_viewport(&window, window.inner_size());
        let tree = WindowTree::new([viewport.width as f32 / viewport.scale_factor, viewport.height as f32 / viewport.scale_factor]);
        tree.set_scale(viewport.scale_factor);
        window::attach(handle, window.clone());
        sync_state(handle, &window);

        // 窗口自己的作用域：关窗口时，里面的绑定、信号、副作用一起释放
        let scope = Scope::new();
        tree.set_accessibility_title(initial_title.clone());
        window::with_current(handle, || {
            scope.run(|| {
                // 窗口大小的信号：这个窗口里的绑定按它切换尺寸档
                tree.attach_metrics(Some(handle.id()));
                if hidden_title_bar {
                    tree.set_title_bar_info(read_title_bar(handle, &window));
                }
                if let Prop::Dynamic(f) = title {
                    let window = window.clone();
                    let tree = tree.downgrade();
                    Effect::new(move || {
                        let title = f();
                        window.set_title(&title);
                        // 改标题时系统可能重新摆放标题栏
                        if hidden_title_bar {
                            crate::system::backend().window().refresh_title_bar(&*window);
                        }
                        if let Some(tree) = tree.upgrade() {
                            tree.set_accessibility_title(title);
                        }
                    });
                }
                tree.mount(build());
            })
        });
        crate::performance::startup_step("建元素树、挂载界面", start);

        let (sender, receiver) = mpsc::channel();
        // 没有固定的背景色时：不透明的窗口是默认的底色，有材质的窗口是透明的
        let initial = crate::title_bar::clear_color(background, backdrop);
        let handback = tree.handback();
        // 表面支不支持扩展范围的输出（广色域、HDR）；用不用由主线程按显示器和画面内容决定，见 `display`
        let display = crate::system::backend().window().display_color(&*window).unwrap_or_default();
        let output = OutputState::new(color_output, render::supports_extended(&caps), display);
        if let Some(state) = window::state(handle) {
            state.display.set(DisplayColor::from_platform(display, false));
            state.backdrop.set_if_changed(backdrop);
        }
        let refill = Arc::new(AtomicBool::new(false));
        // 帧时钟：跟着窗口所在的显示器（macOS 的显示链接、Windows 的合成器时钟或垂直消隐、Wayland 的表面帧回调），
        // 拿不到时是计时器。等刷新的后台线程（Windows）等到一帧时叫醒事件循环；计时器要渲染线程回报呈现的时刻
        let options = FrameClockOptions { rate: FrameRateRange::up_to(120.0), waker: Some(frame_waker(self.proxy.clone())) };
        let clock = crate::system::backend().frame_clock(&*window, options);
        tracing::debug!(window = %initial_title, clock = clock.kind().as_str(), "帧时钟");
        let feedback = clock.present_feedback();
        let main = aureole_core::main_thread_sender();
        let start = Instant::now();
        let render = RenderThread::new(surface, caps, gpu, viewport, initial, !backdrop.is_opaque(), initial_title.clone(), handback, refill.clone(), feedback, main);
        // 表面配置好了（交换链放到了系统合成的对象上）：提交，系统才开始合成它
        if let Some(Err(error)) = composition.as_ref().map(|target| target.commit()) {
            tracing::warn!("提交不了系统合成的对象：{error}");
        }
        crate::performance::startup_step("配置表面、建渲染器", start);
        let render_thread = render.spawn(handle, receiver, false);

        let host_window = WinitWindow {
            handle,
            window: window.clone(),
            sender,
            ime: Cell::new(None),
            focused: Cell::new(window.has_focus()),
            secure: Cell::new(false),
            cursor: Cell::new(Cursor::Default),
            accessibility: RefCell::new(accessibility),
            output: RefCell::new(output),
            background,
            backdrop: Cell::new(backdrop),
            can_be_transparent,
            composition,
            hidden_title_bar,
            window_dragged: Cell::new(false),
            #[cfg(target_os = "linux")]
            on_resize_edge: Cell::new(false),
        };
        let translate = Rc::new(RefCell::new(TranslateState::new(window.scale_factor())));
        let host = Rc::new(WindowHost::new(tree, host_window));
        // 应用的外观换了：窗口的外框（标题栏、材质）和没设背景色时的底色跟着换（元素树里的颜色由树自己换）
        {
            let host = Rc::downgrade(&host);
            scope.run(|| {
                Effect::new(move || {
                    let (mode, appearance) = (aureole_core::appearance_mode(), aureole_core::appearance());
                    if let Some(host) = host.upgrade() {
                        host.window().apply_appearance(mode, appearance);
                    }
                })
            });
        }
        if let Some(path) = self.record.take() {
            let size = [viewport.width as f32 / viewport.scale_factor, viewport.height as f32 / viewport.scale_factor];
            match Recorder::create(&path, size, viewport.scale_factor, Some(std::env::consts::OS)) {
                Ok(recorder) => {
                    tracing::info!(path = %path.display(), "开始录制输入（关掉窗口时结束）");
                    host.record(recorder);
                }
                Err(error) => tracing::error!(path = %path.display(), "录制输入失败：{error}"),
            }
        }
        // 时钟每帧在主线程回调：推进这个窗口的一帧 UI，把变化了的图元交给它的渲染线程
        let pacer = FramePacer::new(clock, ticker(handle, &host, &translate, &refill, &self.hooks));
        crate::external_drag::listen(&window, handle, &host, &translate, &self.proxy);
        let open = OpenWindow {
            handle,
            pacer,
            host,
            scope,
            render_thread: Some(render_thread),
            translate,
            on_close_request,
        };
        if !self.warned_low_power && crate::system::backend().environment().is_low_power_mode() {
            self.warned_low_power = true;
            tracing::warn!("系统开着低电量模式，窗口画面会被限制在 60fps（系统设置 → 电池 → 能耗模式）");
        }
        self.ids.insert(handle, window.id());
        self.windows.insert(window.id(), open);
        // 窗口建好了：插件的钩子（可能又打开窗口，请求排进队列，下一轮处理）
        window::with_current(handle, || self.hooks.window_open(handle));
    }

    fn close(&mut self, event_loop: &ActiveEventLoop, id: WindowId) {
        let Some(mut w) = self.windows.remove(&id) else { return };
        self.ids.remove(&w.handle);
        #[cfg(feature = "menus")]
        menu::detach_window(w.window());
        // 平台层拿着回调（回调拿着窗口宿主）：先摘掉，才释放得了
        crate::system::backend().window().stop_external_drag(&*w.host.window().window);
        crate::system::backend().window().detach_title_bar(w.window());
        if let Some((path, count)) = w.host.finish_recording(host::clock::now()) {
            tracing::info!(path = %path.display(), "录制结束：{count} 次输入");
        }
        w.shutdown();
        window::detach(w.handle);
        drop(w);
        if self.windows.is_empty() && self.quits_on_last_window_closed {
            cancel_drag_out();
            event_loop.exit();
        }
    }

    /// 辅助技术来要树（第一次：打开无障碍、发整棵树）、请求操作、不用了
    fn accessibility(&mut self, event: accesskit_winit::Event) {
        let Some(w) = self.windows.get(&event.window_id) else { return };
        window::with_current(w.handle, || match event.window_event {
            AccessibilityEvent::InitialTreeRequested => {
                let update = w.tree().activate_accessibility();
                w.host.window().accessibility.borrow_mut().update_if_active(|| update);
            }
            AccessibilityEvent::ActionRequested(request) => w.tree().perform_accessibility_action(request),
            AccessibilityEvent::AccessibilityDeactivated => w.tree().deactivate_accessibility(),
        });
    }
}

impl OpenWindow {
    fn tree(&self) -> &WindowTree {
        self.host.tree()
    }

    fn window(&self) -> &winit::window::Window {
        &self.host.window().window
    }

    /// 隐藏了的标题栏的度量可能变了（平台通知、窗口改大小）：重新读，告诉界面
    fn refresh_title_bar(&self) {
        let info = read_title_bar(self.handle, self.window());
        self.tree().set_title_bar_info(info);
    }

    /// Linux：内容铺满的窗口没有系统的边框。指针在窗口边缘时换成调整大小的形状，按下开始调整大小（这次按下不再交给元素）。
    /// 吃掉了这次输入返回 `true`
    #[cfg(target_os = "linux")]
    fn resize_at_edge(&self, event: &WindowEvent) -> bool {
        let w = self.host.window();
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f32>(w.window.scale_factor());
                match crate::title_bar::resize_edge_at(&w.window, Some([logical.x, logical.y])) {
                    Some(edge) => {
                        w.on_resize_edge.set(true);
                        w.window.set_cursor(crate::title_bar::resize_cursor(edge));
                    }
                    None => {
                        if w.on_resize_edge.take() {
                            w.window.set_cursor(cursor_icon(w.cursor.get()));
                        }
                    }
                }
                false
            }
            WindowEvent::MouseInput { state: winit::event::ElementState::Pressed, button: winit::event::MouseButton::Left, .. } => {
                let Some(edge) = crate::title_bar::resize_edge_at(&w.window, self.tree().pointer()) else { return false };
                if let Err(error) = w.window.drag_resize_window(crate::title_bar::resize_direction(edge)) {
                    tracing::debug!("改不了窗口大小：{error}");
                }
                true
            }
            _ => false,
        }
    }

    /// 恢复帧时钟（输入、定时器、任务之后界面可能要变）
    fn wake_up(&self) {
        self.pacer.wake_up();
    }

    /// 时钟停着时（不出帧，不会问显示器），用着扩展范围的表面的窗口照样隔一会儿问一次显示器，
    /// 余量变了渲染线程重画。返回离下次该问还有多久；没停、没用扩展范围的表面时为 `None`
    fn poll_display(&self) -> Option<Duration> {
        let w = self.host.window();
        if !self.pacer.is_paused() || !w.output.borrow().is_extended() {
            return None;
        }
        // 在这个窗口里更新：读 `display_color()` 的绑定重新执行时拿到的是这个窗口
        window::with_current(self.handle, || w.output().update(None, &w.sender));
        let now = host::clock::now();
        Some(Duration::from_secs_f64(w.output.borrow().until_due(now)))
    }

    /// 连续几帧什么都没做：暂停帧时钟。拖着文件悬停时要每帧问指针在哪，不暂停
    fn maybe_sleep(&self) {
        self.pacer.maybe_sleep(self.translate.borrow().file_hovering());
    }

    /// 窗口事件：和输入有关的换算成输入事件交给窗口宿主（插件吃掉了的，`stopped`，不换算），
    /// 其余的（移动、外观、遮挡）在这里处理
    fn handle_event(&mut self, event: WindowEvent, stopped: bool) {
        let w = self.host.window();
        w.accessibility.borrow_mut().process_event(&w.window, &event);
        #[cfg(target_os = "linux")]
        let stopped = stopped || (w.hidden_title_bar && self.resize_at_edge(&event));
        if !stopped {
            let inputs = {
                let mut state = self.translate.borrow_mut();
                state.time = host::clock::now_duration();
                // 拖文件时窗口收不到移动事件：位置向系统要
                if matches!(event, WindowEvent::HoveredFile(_)) {
                    state.file_cursor = cursor(&self.host);
                }
                translate(&event, &mut state)
            };
            for input in inputs {
                self.host.input(input);
            }
            if w.window_dragged.take() {
                self.translate.borrow_mut().release_buttons();
            }
        }
        match event {
            WindowEvent::Resized(size) => {
                let _ = w.sender.send(RenderMsg::Resize(physical_viewport(&w.window, size)));
                sync_state(self.handle, &w.window);
                if w.hidden_title_bar {
                    self.refresh_title_bar();
                }
                self.frame_for_new_size();
            }
            WindowEvent::Moved(_) => {
                sync_state(self.handle, &w.window);
                // 可能挪到了另一个显示器上：下一帧重新问它的颜色能力
                w.output.borrow_mut().repoll();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                w.output.borrow_mut().repoll();
                if w.hidden_title_bar {
                    self.refresh_title_bar();
                }
            }
            // 系统外观是全应用的：哪个窗口报告都一样
            WindowEvent::ThemeChanged(theme) => {
                aureole_core::host::set_system_appearance(crate::system::reported_appearance(theme));
                crate::system::refresh_settings();
            }
            // 窗口被完全挡住时系统会把帧时钟降到约 1Hz；告诉渲染线程这不是主线程卡住
            WindowEvent::Occluded(occluded) => {
                let _ = w.sender.send(RenderMsg::Occluded(occluded));
                // 看不见时动图不换帧
                self.tree().set_occluded(occluded);
                // 最小化、还原没有自己的事件（macOS 上大小也不变）：窗口挡没挡住、有没有焦点变了时问一次
                sync_state(self.handle, &w.window);
            }
            WindowEvent::Focused(_) => sync_state(self.handle, &w.window),
            _ => {}
        }
    }

    /// 窗口刚改了大小（渲染线程在等，没有用旧布局抢着画）：在这次事件里按新大小出一帧。
    /// macOS 上这个回调在系统改窗口大小的事务里：画好的这一帧交回这里呈现，进同一个事务，画面和窗口的新大小同时生效。
    /// 别的平台没有这样的事务，渲染线程画完直接呈现
    fn frame_for_new_size(&self) {
        let sender = &self.host.window().sender;
        if !self.pacer.frame_now() {
            tracing::debug!("改窗口大小发生在一帧里面：新布局下一帧才到");
        }
        if !cfg!(target_os = "macos") {
            let _ = sender.send(RenderMsg::ResizeFrameDone(None));
            return;
        }
        let (handoff, drawn) = sync_channel(0);
        let _ = sender.send(RenderMsg::ResizeFrameDone(Some(handoff)));
        match drawn.recv_timeout(RESIZE_FRAME_WAIT) {
            Ok(frame) => frame.present(),
            // 这一帧画不了（窗口看不见、表面给不出纹理、在等完整内容），渲染线程放开了这一头
            Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => tracing::debug!("改窗口大小：渲染线程这一帧没赶上，不等了"),
        }
    }

    /// 这一轮放下的文件交给指针下接收文件的元素
    fn drop_files(&self) {
        let input = {
            let mut state = self.translate.borrow_mut();
            state.time = host::clock::now_duration();
            state.file_cursor = cursor(&self.host);
            state.take_drop()
        };
        if let Some(input) = input {
            self.host.input(input);
        }
    }

    /// 关窗口：先暂停并放掉帧时钟（必须在窗口之前），再让渲染线程退出、释放窗口的作用域
    fn shutdown(&mut self) {
        self.pacer.stop();
        let _ = self.host.window().sender.send(RenderMsg::Quit);
        if let Some(thread) = self.render_thread.take() {
            let _ = thread.join();
        }
        self.scope.dispose();
    }
}

/// 要退出了：拖到应用外面的系统会话还开着时先取消它。会话在自己的事件循环里等指针松开，这个循环套在应用的事件循环里面，
/// 不取消的话退出要等到指针真的松开才生效（macOS）
fn cancel_drag_out() {
    if crate::system::backend().window().cancel_drag_out() {
        tracing::debug!("要退出了：取消还开着的拖动会话");
    }
}

/// 指针在窗口里的位置：拖放时窗口收不到指针移动事件，向系统要；要不到就用最后已知的位置
fn cursor(host: &WindowHost<WinitWindow>) -> Option<[f32; 2]> {
    let window = &*host.window().window;
    crate::system::backend().window().pointer_position(window).or_else(|| host.tree().pointer())
}

/// 改窗口大小时主线程最多等渲染线程这么久（再久拖动就不跟手了）。到了就走，那一帧由渲染线程呈现，只是不和窗口同步
const RESIZE_FRAME_WAIT: Duration = Duration::from_millis(100);

/// 帧时钟每帧调用的闭包：推进这个窗口的一帧 UI，返回这一帧的结果（有没有做事、接下来要什么时候的帧）。
/// 帧时间用预计上屏的时刻，和输入事件的时间戳、定时器、`sleep` 一样读统一的时钟（核心的 `timer::now`）
fn ticker(
    handle: WindowHandle,
    host: &Rc<WindowHost<WinitWindow>>,
    translate: &Rc<RefCell<TranslateState>>,
    refill: &Arc<AtomicBool>,
    hooks: &Rc<Hooks>,
) -> impl FnMut(FrameTime) -> FrameReport + 'static {
    let (host, translate, refill, hooks) = (host.clone(), translate.clone(), refill.clone(), hooks.clone());
    let mut first = true;
    move |time| {
        let now = host::clock::time_at(time.target_instant());
        window::with_current(handle, || {
            // 插件每帧的钩子：没有时不用算帧的信息
            let frame = hooks.has_frame_hooks().then(|| FrameInfo { window: handle, time: Duration::from_secs_f64(now), stats: None });
            if let Some(frame) = &frame {
                hooks.frame_start(frame);
            }
            let start = Instant::now();
            let report = tick(&host, &translate, &refill, now);
            if std::mem::take(&mut first) {
                crate::performance::startup_step("第一帧：主线程（布局、排版、生成图元）", start);
            }
            if let Some(frame) = frame {
                hooks.frame_end(&FrameInfo { stats: Some(report.stats), ..frame });
            }
            report
        })
    }
}

/// 帧时钟的后台线程（Windows 等合成器时钟、垂直消隐的线程）等到一帧时叫醒事件循环：
/// 发一个 [`UserEvent::Frame`]，事件循环在这一轮的 `about_to_wait` 里把帧交出去
fn frame_waker(proxy: EventLoopProxy<UserEvent>) -> std::task::Waker {
    struct FrameWaker(EventLoopProxy<UserEvent>);
    impl std::task::Wake for FrameWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let _ = self.0.send_event(UserEvent::Frame);
        }
    }
    std::task::Waker::from(Arc::new(FrameWaker(proxy)))
}

/// 推进窗口的一帧（帧时钟的回调）：
/// 拖着文件悬停时更新文件下面的元素、换了 GPU 设备时补交图集，然后是唯一的帧管线。
/// 返回这一帧的结果（做了什么、有没有做事）
fn tick(host: &WindowHost<WinitWindow>, translate: &RefCell<TranslateState>, refill: &AtomicBool, now: f64) -> FrameReport {
    let hover = {
        let mut state = translate.borrow_mut();
        state.file_hovering().then(|| {
            state.time = host::clock::now_duration();
            state.file_cursor = cursor(host);
            state.file_hover()
        })
    };
    if let Some(input) = hover.flatten() {
        host.input(input);
    }
    send_refill(host, refill);
    let report = host.frame(now);
    // 布局、画面变了：文件下面的元素可能换了，下一帧悬停的位置没变也照交一次
    if report.stats.laid_out || report.stats.repainted {
        translate.borrow_mut().retarget_file_hover();
    }
    report
}

/// 渲染线程换了新的 GPU 设备：把图集和图片的完整内容交给它。还没交出去的增量更新作废（完整内容里都有），
/// 之后的帧只带新的增量
fn send_refill(host: &WindowHost<WinitWindow>, refill: &AtomicBool) {
    if refill.load(Ordering::Relaxed) && refill.swap(false, Ordering::AcqRel) {
        let tree = host.tree();
        drop(tree.take_atlas_updates());
        let _ = host.window().sender.send(RenderMsg::Refill { atlas: tree.atlas_snapshot(), list: tree.display_list() });
        // 新设备上的交换链换到了系统合成的对象上：提交，系统才开始合成它
        if let Some(Err(error)) = host.window().composition.as_ref().map(|target| target.commit()) {
            tracing::warn!("提交不了系统合成的对象：{error}");
        }
    }
}

/// 没有能用的 GPU（启动时找不到，设备丢失后换不了，渲染线程投递过来；或者窗口的渲染线程短时间内反复出错）：
/// 记一条错误说明原因，应用没装日志的接收方时直接写到标准错误（马上就要退出，不能什么都不留）；
/// 不是从终端启动时（双击打开的应用）在 macOS、Windows 上弹出提示框；以退出码 1 结束
pub(crate) fn gpu_failed(error: &dyn std::fmt::Display) -> ! {
    use std::io::IsTerminal;
    if tracing::dispatcher::has_been_set() {
        tracing::error!("无法显示窗口：{error}");
    } else {
        // 马上就要退出，应用又没装日志的接收方：这是唯一能留下原因的地方
        #[allow(clippy::print_stderr, reason = "退出前的最后一句话，tracing 没有接收方")]
        {
            eprintln!("无法显示窗口：{error}");
        }
    }
    if !std::io::stderr().is_terminal() && std::env::var_os("CI").is_none() {
        // macOS 没有软件渲染的适配器，不提示
        let hint = if cfg!(target_os = "macos") { "" } else { "\n\n可以设置环境变量 AUREOLE_GPU=software 改用软件渲染试试。" };
        crate::system::backend().dialog().alert("没有可用的 GPU", &format!("{error}{hint}"));
    }
    std::process::exit(1)
}

/// 一个窗口的颜色输出，给 [`frame`] 用
struct Output<'a> {
    state: &'a RefCell<OutputState>,
    window: &'a winit::window::Window,
    handle: WindowHandle,
}

impl Output<'_> {
    /// 隔一会儿问一次显示器，按画面里有没有扩展范围的内容（这一帧重画了时 `content` 有值）决定表面；
    /// 决定变了就告诉渲染线程（在这一帧的内容之前），并更新窗口的 [`WindowHandle::display_color`]
    fn update(&self, content: Option<bool>, sender: &Sender<RenderMsg>) {
        // 帧时钟暂停时也要问显示器：读统一的时钟，不用帧给的时间
        let now = host::clock::now();
        let mut state = self.state.borrow_mut();
        let before = state.display;
        if state.due(now) {
            state.polled(now, crate::system::backend().window().display_color(self.window));
        }
        let decided = state.update(now, content);
        let display = state.display;
        drop(state);
        if let Some((extended, headroom)) = decided {
            let _ = sender.send(RenderMsg::Output { extended, headroom });
        }
        if (decided.is_some() || display != before)
            && let Some(state) = window::state(self.handle)
        {
            let extended = decided.map_or_else(|| state.display.get_untracked().extended, |d| d.0);
            state.display.set(DisplayColor::from_platform(display, extended));
        }
    }
}

/// 窗口隐藏了标题栏：告诉平台（摆窗口按钮、回答窗口边缘的命中）。度量变了时平台通知过来，投递回主线程重新读
fn attach_title_bar(handle: WindowHandle, window: &winit::window::Window, options: platform::TitleBarOptions) {
    let main = aureole_core::main_thread_sender();
    let on_change: platform::OnTitleBarChange = Arc::new(move || {
        main.send(move || window::request(Request::TitleBarChanged(handle)));
    });
    if let Err(error) = crate::system::backend().window().attach_title_bar(window, options, on_change) {
        tracing::warn!("隐藏不了标题栏：{error}");
    }
}

/// 读隐藏了的标题栏此刻占了多少，写进窗口的响应式状态。全屏时标题栏不占地方
fn read_title_bar(handle: WindowHandle, window: &winit::window::Window) -> TitleBarInfo {
    let metrics = crate::system::backend().window().title_bar_metrics(window).filter(|_| window.fullscreen().is_none()).unwrap_or_default();
    let width = window.inner_size().to_logical::<f32>(window.scale_factor()).width;
    let info = crate::title_bar::info(metrics, width);
    if let Some(state) = window::state(handle) {
        state.title_bar.set_if_changed(info);
    }
    info
}

/// Windows：建在 DirectComposition 视觉对象上的表面，交换链才能带透明。别的平台、用的不是 DX12 时是 `None`（窗口的表面照旧）
fn composition_surface(instance: &wgpu::Instance, gpu: &Gpu, window: &winit::window::Window) -> Option<(wgpu::Surface<'static>, Box<dyn CompositionTarget>)> {
    #[cfg(target_os = "windows")]
    {
        if gpu.adapter.get_info().backend != wgpu::Backend::Dx12 {
            return None;
        }
        let target = crate::system::backend().window().composition_target(window).map_err(|error| tracing::warn!("建不了系统合成的对象：{error}")).ok().flatten()?;
        // SAFETY: 视觉对象是刚建的，表面持有它的引用；窗口活着时 `target` 留在窗口的另一半里
        let surface = unsafe { instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(target.visual())) };
        Some((surface.map_err(|error| tracing::warn!("建不了系统合成对象上的表面：{error}")).ok()?, target))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (instance, gpu, window);
        None
    }
}

/// 把窗口此刻的大小、位置、全屏、最小化、最大化状态写进它的响应式状态
fn sync_state(handle: WindowHandle, window: &winit::window::Window) {
    let Some(state) = window::state(handle) else { return };
    let scale = window.scale_factor();
    let size = window.inner_size().to_logical::<f32>(scale);
    state.size.set([size.width, size.height]);
    if let Ok(position) = window.outer_position() {
        let position = position.to_logical::<f32>(scale);
        state.position.set([position.x, position.y]);
    }
    state.fullscreen.set(window.fullscreen().is_some());
    if !window::is_zooming(handle) {
        state.maximized.set_if_changed(window.is_maximized());
    }
    // 不告诉（Wayland）时留着最后一次设的值
    if let Some(minimized) = window.is_minimized() {
        state.minimized.set_if_changed(minimized);
    }
}

fn physical_viewport(window: &winit::window::Window, size: PhysicalSize<u32>) -> Viewport {
    Viewport { width: size.width.max(1), height: size.height.max(1), scale_factor: window.scale_factor() as f32 }
}

/// 指针形状对应的系统指针
fn cursor_icon(cursor: Cursor) -> CursorIcon {
    match cursor {
        Cursor::Default => CursorIcon::Default,
        Cursor::Pointer => CursorIcon::Pointer,
        Cursor::Text => CursorIcon::Text,
        Cursor::VerticalText => CursorIcon::VerticalText,
        Cursor::Grab => CursorIcon::Grab,
        Cursor::Grabbing => CursorIcon::Grabbing,
        Cursor::Move => CursorIcon::Move,
        Cursor::Crosshair => CursorIcon::Crosshair,
        Cursor::NotAllowed => CursorIcon::NotAllowed,
        Cursor::ResizeHorizontal => CursorIcon::EwResize,
        Cursor::ResizeVertical => CursorIcon::NsResize,
        Cursor::ResizeDiagonal => CursorIcon::NwseResize,
        Cursor::ResizeAntiDiagonal => CursorIcon::NeswResize,
        Cursor::ColumnResize => CursorIcon::ColResize,
        Cursor::RowResize => CursorIcon::RowResize,
        Cursor::ZoomIn => CursorIcon::ZoomIn,
        Cursor::ZoomOut => CursorIcon::ZoomOut,
        Cursor::Wait => CursorIcon::Wait,
        Cursor::Progress => CursorIcon::Progress,
        Cursor::Help => CursorIcon::Help,
        Cursor::ContextMenu => CursorIcon::ContextMenu,
        Cursor::Copy => CursorIcon::Copy,
        Cursor::Alias => CursorIcon::Alias,
        Cursor::Cell => CursorIcon::Cell,
        Cursor::AllScroll => CursorIcon::AllScroll,
        // 新加的形状窗口库还没有对应的：先用默认的（核心不能依赖窗口库，这里查不出漏了哪个）
        _ => CursorIcon::Default,
    }
}
