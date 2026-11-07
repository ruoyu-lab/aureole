//! 平台后端：和系统打交道的全部服务放在一个接口后面，能力可以查询，测试里可以整个换成无头后端。
//!
//! - [`PlatformBackend`]：能力（[`Capabilities`]）、平台惯例（[`Conventions`]）、帧时钟，以及各项服务。
//! - 每项服务一个接口：窗口（[`WindowService`]）、外观（[`AppearanceService`]）、运行环境（[`EnvironmentService`]）、
//!   剪贴板（[`ClipboardService`]）、对话框（[`DialogService`]）、外壳（[`ShellService`]）、
//!   通知（[`NotificationService`]）、菜单栏（[`MenuService`]）、托盘（[`TrayService`]）、
//!   单实例和打开请求（[`SingleInstanceService`]）。
//! - 出错统一是 [`PlatformError`]：不支持、被拒绝、需要打包、系统错误，各带原因。
//!
//! 实现有两个：本机的 [`crate::NativeBackend`]（[`crate::native`] 取进程里唯一的那个）和测试用的
//! [`crate::headless::HeadlessBackend`]。crate 根上原来的自由函数（`system::open_url`、`display_color`……）
//! 照旧可以用，里面走的就是本机后端。

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::clipboard::{ClipFormat, ClipboardContent, RgbaImage};
use crate::external_drag::OnExternalDrag;
use crate::frame::{FrameClock, FrameClockKind, FrameClockOptions};
use crate::single_instance::InstanceRole;
use crate::open::{OnOpen, Opened};
use crate::system::{DialogDone, DialogOptions, SettingsChanged};
use crate::title_bar::{OnTitleBarChange, TitleBarAction, TitleBarMetrics, TitleBarOptions, WindowMaterial};
use crate::DisplayColor;

/// 平台层认的窗口：能给出窗口句柄和显示句柄（窗口库的窗口两样都有）。帧时钟要显示句柄（Wayland 要用连接）
pub trait PlatformWindow: HasWindowHandle + HasDisplayHandle {}

impl<T: HasWindowHandle + HasDisplayHandle + ?Sized> PlatformWindow for T {}

/// 平台的服务出错的原因
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformError {
    /// 这个平台、这个环境不支持（比如 Linux 没有菜单栏、没有会话总线）
    Unsupported(Cow<'static, str>),
    /// 用户或系统拒绝了（没有权限）
    Denied(Cow<'static, str>),
    /// 要先打包（macOS 的 .app、Windows 的应用标识）才能用
    NotPackaged(Cow<'static, str>),
    /// 系统调用失败
    System(Cow<'static, str>),
}

impl PlatformError {
    /// 原因
    pub fn reason(&self) -> &str {
        match self {
            Self::Unsupported(reason) | Self::Denied(reason) | Self::NotPackaged(reason) | Self::System(reason) => reason,
        }
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Unsupported(_) => "不支持",
            Self::Denied(_) => "被拒绝",
            Self::NotPackaged(_) => "需要打包",
            Self::System(_) => "系统错误",
        };
        write!(f, "{kind}：{}", self.reason())
    }
}

impl std::error::Error for PlatformError {}

impl From<std::io::Error> for PlatformError {
    fn from(error: std::io::Error) -> Self {
        Self::System(error.to_string().into())
    }
}

/// 这个后端此刻能做什么。应用据此调整界面（比如没有菜单栏时把命令放进窗口里）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// 窗口能拿到的最好的帧时钟；某个窗口实际拿到的以 [`FrameClock::kind`] 为准（可能退到计时器）
    pub frame_clock: FrameClockKind,
    /// 有原生菜单栏（[`PlatformBackend::menu`]）
    pub supports_menu_bar: bool,
    /// 有托盘（[`PlatformBackend::tray`]）
    pub supports_tray: bool,
    /// 能读写系统剪贴板（纯文字）
    pub supports_clipboard: bool,
    /// 剪贴板能放图片
    pub supports_clipboard_image: bool,
    /// 剪贴板能放文件列表
    pub supports_clipboard_files: bool,
    /// 剪贴板能放带格式的文字（不能时带格式的文字只剩纯文字）
    pub supports_clipboard_rich_text: bool,
    /// 一次能放几种格式（不能时只放最优先的一种：文件、图片、文字）
    pub supports_clipboard_multiple_formats: bool,
    /// 有系统的文件对话框
    pub supports_file_dialog: bool,
    /// 此刻能发系统通知（macOS 要打包成 .app，Windows 要有应用标识）
    pub supports_notification: bool,
    /// 通知能有点击回调和动作按钮（前提和发通知一样；按钮怎么显示由系统决定）
    pub supports_notification_actions: bool,
    /// 能把东西拖到应用外面
    pub supports_drag_out: bool,
    /// 能接收别的应用拖来的文字、链接、图片。不能时拖文件照样能用（`on_file_drop`，`on_external_drop` 也收得到文件）
    pub supports_external_drop: bool,
    /// 拖文件时能问到指针位置
    pub supports_pointer_position: bool,
    /// 能问到显示器的色域和 HDR 余量
    pub supports_display_color: bool,
    /// 系统会把打开网址、文档的请求作为事件送来（macOS 打包后）
    pub supports_open_events: bool,
    /// 系统设置（外观、强调色、减少动态效果）变了会通知
    pub supports_settings_watch: bool,
    /// 能检测低电量模式
    pub supports_low_power_detection: bool,
    /// 有单实例（锁和本地通道）
    pub supports_single_instance: bool,
    /// 能隐藏标题栏、让内容铺满整个窗口（`TitleBar::Hidden`）
    pub supports_hidden_title_bar: bool,
    /// 隐藏标题栏以后系统还留着窗口按钮，位置可以调（macOS 的红绿灯）；没有时按钮要应用自己画
    pub supports_title_bar_controls: bool,
    /// 应用自己画的最大化按钮能报告给系统，悬停时弹出贴靠布局（Windows 11）
    pub supports_snap_layouts: bool,
    /// 窗口背后能透出系统的材质（`WindowMaterial`）；没有时照常画不透明的背景
    pub supports_window_backdrop: bool,
}

impl Capabilities {
    /// 什么都不支持，帧时钟是计时器
    pub const NONE: Self = Self {
        frame_clock: FrameClockKind::Timer,
        supports_menu_bar: false,
        supports_tray: false,
        supports_clipboard: false,
        supports_clipboard_image: false,
        supports_clipboard_files: false,
        supports_clipboard_rich_text: false,
        supports_clipboard_multiple_formats: false,
        supports_file_dialog: false,
        supports_notification: false,
        supports_notification_actions: false,
        supports_drag_out: false,
        supports_external_drop: false,
        supports_pointer_position: false,
        supports_display_color: false,
        supports_open_events: false,
        supports_settings_watch: false,
        supports_low_power_detection: false,
        supports_single_instance: false,
        supports_hidden_title_bar: false,
        supports_title_bar_controls: false,
        supports_snap_layouts: false,
        supports_window_backdrop: false,
    };
}

/// 修饰键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModifierKey {
    /// Shift
    Shift,
    /// Control
    Control,
    /// Alt（macOS 上叫 Option）
    Alt,
    /// macOS 的 Command（其他平台是徽标键、Super）
    Command,
}

/// 到行首、行尾用什么键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineEdgeKeys {
    /// 主修饰键加左右方向键（macOS；主修饰键加上下方向键到文档头尾）
    PrimaryArrows,
    /// Home、End（主修饰键加 Home、End 到文档头尾）
    HomeEnd,
}

/// 重做用什么键
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RedoKeys {
    /// 主修饰键加 Shift 加 Z（macOS、Linux）
    ShiftZ,
    /// 主修饰键加 Y，主修饰键加 Shift 加 Z 也认（Windows）
    YOrShiftZ,
}

/// 平台的惯例：同一个操作在各平台上习惯用的键、滚动的步长
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Conventions {
    /// 快捷键的主修饰键：macOS 是 Command，其他平台是 Control
    pub primary_modifier: ModifierKey,
    /// 按词移动、按词删除的修饰键：macOS 是 Option，其他平台是 Control
    pub word_modifier: ModifierKey,
    /// 到行首、行尾
    pub line_edge: LineEdgeKeys,
    /// 重做
    pub redo: RedoKeys,
    /// 滚轮按行报告时一行滚多远（逻辑像素）。macOS 的滚轮事件自带加速，一行短一些；其他平台一格一行，按三行文字算
    pub wheel_line_height: f32,
    /// 按住 Control 点左键算右键点击（macOS）
    pub treats_control_click_as_secondary: bool,
    /// 手指按住不动多久算长按
    pub long_press_duration: Duration,
}

impl Conventions {
    /// macOS 的惯例
    pub const MACOS: Self = Self {
        primary_modifier: ModifierKey::Command,
        word_modifier: ModifierKey::Alt,
        line_edge: LineEdgeKeys::PrimaryArrows,
        redo: RedoKeys::ShiftZ,
        wheel_line_height: 40.0,
        treats_control_click_as_secondary: true,
        long_press_duration: Duration::from_millis(500),
    };

    /// Windows 的惯例
    pub const WINDOWS: Self = Self {
        primary_modifier: ModifierKey::Control,
        word_modifier: ModifierKey::Control,
        line_edge: LineEdgeKeys::HomeEnd,
        redo: RedoKeys::YOrShiftZ,
        wheel_line_height: 100.0,
        treats_control_click_as_secondary: false,
        long_press_duration: Duration::from_millis(800),
    };

    /// Linux 桌面的惯例
    pub const LINUX: Self = Self {
        primary_modifier: ModifierKey::Control,
        word_modifier: ModifierKey::Control,
        line_edge: LineEdgeKeys::HomeEnd,
        redo: RedoKeys::ShiftZ,
        wheel_line_height: 100.0,
        treats_control_click_as_secondary: false,
        long_press_duration: Duration::from_millis(500),
    };

    /// 编译目标的系统的惯例（其他系统按 Linux 的）
    pub const fn current() -> &'static Self {
        if cfg!(target_os = "macos") {
            &Self::MACOS
        } else if cfg!(target_os = "windows") {
            &Self::WINDOWS
        } else {
            &Self::LINUX
        }
    }
}

/// 平台后端。所有服务都能从任何线程拿到；只能在主线程用的方法在说明里写着（不在主线程时什么都不做或返回"没有"）
pub trait PlatformBackend: Send + Sync + 'static {
    /// 后端的名字：`"macos"`、`"windows"`、`"linux"`、`"headless"` 或 `"other"`
    fn name(&self) -> &'static str;

    /// 此刻能做什么
    fn capabilities(&self) -> Capabilities;

    /// 平台的惯例
    fn conventions(&self) -> &Conventions;

    /// 给窗口建帧时钟。总有：拿不到跟着显示器的时钟时是计时器。在主线程上调用；时钟必须在窗口之前释放
    fn frame_clock(&self, window: &dyn PlatformWindow, options: FrameClockOptions) -> Box<dyn FrameClock>;

    /// 窗口相关：显示器的颜色能力、指针位置、带到最前面、拖到应用外面
    fn window(&self) -> &dyn WindowService;

    /// 系统外观、强调色、减少动态效果
    fn appearance(&self) -> &dyn AppearanceService;

    /// 运行环境：首选语言、低电量模式、打包后的应用名
    fn environment(&self) -> &dyn EnvironmentService;

    /// 剪贴板
    fn clipboard(&self) -> &dyn ClipboardService;

    /// 文件对话框、出错提示框
    fn dialog(&self) -> &dyn DialogService;

    /// 用默认应用打开网址和文件、在文件管理器里显示
    fn shell(&self) -> &dyn ShellService;

    /// 系统通知
    fn notification(&self) -> &dyn NotificationService;

    /// 原生菜单栏；没有时是 `None`
    fn menu(&self) -> Option<&dyn MenuService>;

    /// 托盘；没有时是 `None`
    fn tray(&self) -> Option<&dyn TrayService>;

    /// 单实例和应用被要求打开的网址、文件
    fn single_instance(&self) -> &dyn SingleInstanceService;

}

/// 窗口相关的服务。都只在主线程上有效
pub trait WindowService: Send + Sync {
    /// 窗口所在显示器的颜色能力；不知道时是 `None`（调用方按普通显示器处理）
    fn display_color(&self, window: &dyn HasWindowHandle) -> Option<DisplayColor>;

    /// 指针此刻在窗口内容区里的位置（逻辑像素，左上角为原点，可能在窗口外）。拖文件时窗口收不到指针移动事件，用它
    fn pointer_position(&self, window: &dyn HasWindowHandle) -> Option<[f32; 2]>;

    /// 把窗口带到最前面（取消隐藏、还原最小化、成为主窗口）
    fn bring_to_front(&self, window: &dyn HasWindowHandle) -> Result<(), PlatformError>;

    /// 启动、主窗口显示出来以后调用：没打包成 .app（从终端直接运行可执行文件）时把应用设成普通的前台应用并激活，
    /// 不然窗口出来了却拿不到键盘；打包后由系统按 Info.plist 管，不做。返回做了没有。
    /// 只在 macOS 的主线程上有事做，其他平台返回 `false`
    fn activate_if_unpackaged(&self) -> bool;

    /// 现在正在处理的系统事件是不是滚轮事件。只在 macOS 的主线程上可能是 `true`：那里的窗口库处理滚轮事件时，
    /// 先按滚轮事件自己带的位置补发一个指针移动，这个位置可能是旧的，运行时要靠它认出来
    fn handling_wheel_event(&self) -> bool;

    /// 开始把文件或文字拖到应用外面，`at` 是起点（窗口里的逻辑像素）。开始了返回 `Ok`，结束时调用 `on_end`（是否放下了）
    fn begin_drag_out(
        &self,
        window: &dyn HasWindowHandle,
        files: &[PathBuf],
        text: Option<&str>,
        at: [f32; 2],
        on_end: Box<dyn FnOnce(bool)>,
    ) -> Result<(), PlatformError>;

    /// 取消正在进行的拖到应用外面的会话（应用要退出时）：会话按没被接住结束（调用 `on_end(false)`）。
    /// 系统的会话在等指针松开时，应用的事件循环退不出来（macOS），所以退出前先取消。
    /// 取消了返回 `true`；没有进行中的会话、会话不挡着退出的平台什么都不做，返回 `false`
    fn cancel_drag_out(&self) -> bool {
        false
    }

    /// 开始接收别的应用拖来的内容，回调在主线程上。文件窗口库自己接的平台（macOS、Linux）上文件照旧走窗口库的事件。
    /// 窗口库只提供文件的平台（Linux）返回"不支持"，文件照样能拖
    fn listen_external_drag(&self, window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
        let _ = (window, on_drag);
        Err(PlatformError::Unsupported("这个平台的窗口库只提供文件拖放".into()))
    }

    /// 不再接收（关窗口前调用）
    fn stop_external_drag(&self, window: &dyn HasWindowHandle) {
        let _ = window;
    }

    /// 密码框有焦点（窗口在前面）时打开系统的安全输入，`on` 为假时关上：打开期间别的程序的键盘监听读不到按键。
    /// 整个进程一份，重复打开、关上没有影响。macOS 是 `EnableSecureEventInput`；别的平台没有对应的系统设置，
    /// 返回"不支持"（密码框照样不开输入法）
    fn set_secure_input(&self, on: bool) -> Result<(), PlatformError> {
        let _ = on;
        Err(PlatformError::Unsupported("这个平台没有系统的安全输入".into()))
    }

    /// 系统的安全输入此刻开着（本进程或别的进程打开的都算；不支持的平台是 `false`）
    fn is_secure_input_enabled(&self) -> bool {
        false
    }

    /// 窗口隐藏了标题栏（`TitleBar::Hidden`），窗口库的窗口属性已经设好：平台补上窗口库不管的那些
    /// （macOS 摆系统窗口按钮、盯着 AppKit 重新摆放；Windows 回答窗口边缘和最大化按钮的命中）。
    /// 度量变了调用 `on_change`，调用方再读 [`Self::title_bar_metrics`]
    fn attach_title_bar(&self, window: &dyn HasWindowHandle, options: TitleBarOptions, on_change: OnTitleBarChange) -> Result<(), PlatformError> {
        let _ = (window, options, on_change);
        Err(PlatformError::Unsupported("这个平台不能隐藏标题栏".into()))
    }

    /// 窗口要关了：放掉 [`Self::attach_title_bar`] 和材质留着的东西
    fn detach_title_bar(&self, window: &dyn HasWindowHandle) {
        let _ = window;
    }

    /// 改系统窗口按钮的位置。没有系统窗口按钮的平台什么都不做
    fn set_title_bar_options(&self, window: &dyn HasWindowHandle, options: TitleBarOptions) -> Result<(), PlatformError> {
        let _ = (window, options);
        Ok(())
    }

    /// 系统可能重新摆放了标题栏（比如改了窗口标题）：再摆一次
    fn refresh_title_bar(&self, window: &dyn HasWindowHandle) {
        let _ = window;
    }

    /// 隐藏了的标题栏此刻占了多少；不支持时是 `None`
    fn title_bar_metrics(&self, window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
        let _ = window;
        None
    }

    /// 应用自己画的最大化按钮的范围（内容区里的逻辑像素，`[x, y, 宽, 高]`）：系统按最大化按钮对待它（Windows 11 悬停时弹贴靠布局）。
    /// 没有这回事的平台什么都不做
    fn set_maximize_button_area(&self, window: &dyn HasWindowHandle, area: Option<[f32; 4]>) {
        let _ = (window, area);
    }

    /// 系统设置里双击标题栏该做什么
    fn title_bar_action(&self) -> TitleBarAction {
        TitleBarAction::Zoom
    }

    /// 等这次系统事件处理完，在窗口库的事件回调外面执行 `f`（主线程）。
    /// macOS 上窗口缩放的动画在调用里同步跑完：在窗口库的回调里开始的话，动画期间窗口改大小的事件被窗口库攒到回调返回才交出来，
    /// 这期间一帧也出不了。所以投递到主线程的运行循环，下一次转起来时执行。别的平台没有这回事，直接执行
    fn run_outside_event_callback(&self, f: Box<dyn FnOnce()>) {
        f();
    }

    /// 窗口背后的材质。不支持时返回"不支持"，窗口照常不透明
    fn set_window_backdrop(&self, window: &dyn HasWindowHandle, backdrop: WindowMaterial) -> Result<(), PlatformError> {
        let _ = (window, backdrop);
        Err(PlatformError::Unsupported("这个平台没有窗口材质".into()))
    }

    /// 窗口的交换链要放在系统合成的对象上才能带透明时（Windows）给它建一个；窗口的表面本来就能带透明的平台返回 `Ok(None)`
    fn composition_target(&self, window: &dyn HasWindowHandle) -> Result<Option<Box<dyn CompositionTarget>>, PlatformError> {
        let _ = window;
        Ok(None)
    }
}

/// 窗口上一个系统合成的对象（Windows 的 DirectComposition）：渲染的交换链放在它上面才能带透明
pub trait CompositionTarget {
    /// 视觉对象（`IDCompositionVisual`）的指针，建表面时要。对象活着时一直有效
    fn visual(&self) -> *mut std::ffi::c_void;

    /// 表面配置好以后提交，系统才开始合成它
    fn commit(&self) -> Result<(), PlatformError>;
}

/// 系统外观的服务
pub trait AppearanceService: Send + Sync {
    /// 系统是不是深色外观；不知道时是 `None`
    fn is_dark_mode(&self) -> Option<bool>;

    /// 托盘所在的任务栏、面板是不是深色（托盘图标按它着色）；不用知道时是 `None`
    fn is_panel_dark(&self) -> Option<bool>;

    /// 系统的强调色（sRGB）
    fn accent_color(&self) -> Option<[u8; 3]>;

    /// 打开了"减少动态效果"
    fn is_motion_reduced(&self) -> bool;

    /// 在后台监视系统设置，深浅色、强调色、减少动态效果变了时调用 `on_change`（在后台线程里）
    fn watch_settings(&self, on_change: SettingsChanged);
}

/// 运行环境的服务
pub trait EnvironmentService: Send + Sync {
    /// 用户首选的语言（BCP 47，如 `"zh-Hans-CN"`、`"en-US"`）
    fn preferred_language(&self) -> Option<String>;

    /// 开着低电量模式（macOS 上窗口画面会被限制在 60fps）
    fn is_low_power_mode(&self) -> bool;

    /// 打包后的应用名；没打包时是 `None`
    fn bundle_name(&self) -> Option<String>;
}

/// 剪贴板的服务。整个进程共用一个剪贴板实例。读的时候没有这种内容不算错（`Ok(None)`、空列表），
/// 这个平台做不到才是"不支持"
pub trait ClipboardService: Send + Sync {
    /// 剪贴板里的文字；没有文字时是 `Ok(None)`
    fn read_text(&self) -> Result<Option<String>, PlatformError>;

    /// 把文字放进剪贴板
    fn write_text(&self, text: &str) -> Result<(), PlatformError>;

    /// 现在剪贴板上有哪些格式，按文件、图片、带格式的文字、纯文字排
    fn formats(&self) -> Result<Vec<ClipFormat>, PlatformError>;

    /// 剪贴板里带格式的文字（RTF，见 [`crate::clipboard::rtf`]）
    fn read_rich_text(&self) -> Result<Option<Vec<u8>>, PlatformError>;

    /// 剪贴板里的图片。访达、资源管理器复制文件时可能带着文件的图标：有文件时先看文件
    fn read_image(&self) -> Result<Option<RgbaImage>, PlatformError>;

    /// 剪贴板里的文件
    fn read_files(&self) -> Result<Vec<PathBuf>, PlatformError>;

    /// 换掉剪贴板的内容，一次放几种格式；什么都没有时是清空。
    /// 一次只能放一种的平台（[`Capabilities::supports_clipboard_multiple_formats`]）放最优先的一种：文件、图片、文字
    fn write(&self, content: &ClipboardContent) -> Result<(), PlatformError>;
}

/// 对话框的服务
pub trait DialogService: Send + Sync {
    /// 弹出文件对话框，马上返回；用户选好或取消后调用 `done`（取消时是空列表）。
    /// macOS 上只能在主线程打开、在主线程完成；Windows、Linux 在对话框自己的线程里完成
    fn file_dialog(&self, options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone);

    /// 像用户点"取消"一样关掉所有开着的文件对话框，返回关掉了几个
    fn cancel_file_dialogs(&self) -> usize;

    /// 模态的出错提示框，用户关掉后返回（只在主线程；没有提示框的平台什么都不做）
    fn alert(&self, title: &str, message: &str);
}

/// 外壳的服务：用默认应用打开、在文件管理器里显示
pub trait ShellService: Send + Sync {
    /// 用系统默认的应用打开网址（`https:`、`mailto:`、自定义协议……）
    fn open_url(&self, url: &str) -> Result<(), PlatformError>;

    /// 用默认应用打开文件，文件夹在文件管理器里打开。不存在时是错误，不调用系统
    fn open_path(&self, path: &Path) -> Result<(), PlatformError>;

    /// 在文件管理器里显示并选中。不存在时是错误，不调用系统
    fn reveal_path(&self, path: &Path) -> Result<(), PlatformError>;
}

/// 一条要发的通知
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationRequest {
    /// 标题
    pub title: String,
    /// 正文
    pub body: String,
    /// 动作按钮的文字。回答里按下标说是哪一个
    pub actions: Vec<String>,
}

/// 用户对通知做了什么
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotificationResponse {
    /// 点了通知本身
    Click,
    /// 点了第几个动作按钮（从 0 数）
    Action(usize),
}

/// 用户点了通知（在系统的线程或主线程上调用，调用方自己转回主线程）
pub type OnNotificationResponse = Arc<dyn Fn(NotificationResponse) + Send + Sync>;

/// 系统通知的服务
pub trait NotificationService: Send + Sync {
    /// 发通知。交给系统（或者排进发送的队列）了返回 `Ok`
    fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError>;

    /// 发带动作按钮的通知，用户点了调用 `on_response`。前提和 [`Self::notify`] 一样：
    /// macOS 没打包时是 [`PlatformError::NotPackaged`]
    fn show(&self, request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError>;

    /// 通知里显示的应用名（Linux 用；macOS 读 Info.plist，Windows 按应用标识）
    fn set_app_name(&self, name: &str);

    /// 放一下系统的提示音（用户在系统设置里选的那一个），比如按了做不了的键。没有这回事的平台返回"不支持"
    fn beep(&self) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("这个平台没有系统提示音".into()))
    }
}

/// 菜单项的编号：调用方给，点击时原样送回
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MenuItemId(pub u64);

impl MenuItemId {
    /// 平台在应用菜单里补上的"退出"（Windows）：点了送回这个编号，调用方按自己的方式退出（关窗口、结束事件循环）。
    /// 调用方自己的菜单项不要用它
    pub const QUIT: Self = Self(u64::MAX);
}

/// 一份菜单：菜单栏的各个菜单，或者托盘的菜单
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuModel {
    /// 依次排列的项
    pub entries: Vec<MenuEntry>,
}

/// 菜单里的一项
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    /// 能点的项
    Item(MenuItemModel),
    /// 子菜单
    Submenu(SubmenuModel),
    /// 分隔线
    Separator,
}

/// 能点的菜单项
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItemModel {
    /// 编号，点击时送回
    pub id: MenuItemId,
    /// 文字
    pub text: String,
    /// 快捷键（如 `"CmdOrCtrl+S"`）
    pub shortcut: Option<String>,
    /// 不能点
    pub disabled: bool,
    /// 有勾选状态时是 `Some`
    pub checked: Option<bool>,
}

/// 子菜单
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmenuModel {
    /// 标题
    pub title: String,
    /// 里面的项
    pub entries: Vec<MenuEntry>,
    /// 是应用菜单（macOS 菜单栏的第一个，系统补上关于、服务、隐藏、退出）
    pub is_application_menu: bool,
}

/// 改一个菜单项：`None` 的不改
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuItemUpdate {
    /// 新的文字
    pub text: Option<String>,
    /// 能不能点
    pub disabled: Option<bool>,
    /// 勾选
    pub checked: Option<bool>,
}

/// 菜单项被点了（在系统的线程或主线程上调用，调用方自己转回主线程）
pub type OnMenuItem = Arc<dyn Fn(MenuItemId) + Send + Sync>;

/// 菜单栏的服务。只在主线程上用（菜单对象建在主线程上）
pub trait MenuService: Send + Sync {
    /// 装上菜单栏，换掉之前的。macOS 把菜单栏的第一项当作应用菜单：第一项不是应用菜单（[`SubmenuModel::is_application_menu`]）时
    /// 补上一个，名字是 `app_name`。点了菜单项调用 `on_item`（可能不在主线程）
    fn set_menu_bar(&self, menu: MenuModel, app_name: &str, on_item: OnMenuItem) -> Result<(), PlatformError>;

    /// 改一个菜单项
    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError>;

    /// 新窗口创建好了：菜单栏长在窗口里的平台（Windows）把菜单栏挂上去
    fn attach_window(&self, window: &dyn HasWindowHandle) -> Result<(), PlatformError>;

    /// 窗口要关了：摘下菜单栏
    fn detach_window(&self, window: &dyn HasWindowHandle);
}

/// 托盘图标的位图
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayImage {
    /// RGBA 像素，一行接一行，`size × size` 个
    pub rgba: Vec<u8>,
    /// 边长（像素）
    pub size: u32,
    /// 模板图标：系统按菜单栏的深浅着色（macOS），只看透明度
    pub template: bool,
}

/// 托盘图标要的位图：几种边长、单色图标画成什么颜色、是不是模板图标。调用方按它画好再交过来
/// （[`TrayModel::icons`]、[`TrayService::set_icon`]）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayIconFormat {
    /// 边长（像素），几种都要时每种画一张，按这个顺序给
    pub sizes: Vec<u32>,
    /// 单色图标的颜色（sRGB）：按任务栏、面板的深浅是白色或接近黑色；模板图标是黑色
    pub rgb: [u8; 3],
    /// 画成模板图标（系统按菜单栏的深浅着色，只看透明度）
    pub template: bool,
}

/// 一个托盘图标
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayModel {
    /// 图标：同一个图标的几种边长（按 [`TrayService::icon_format`] 画的），没有图标时是空的
    pub icons: Vec<TrayImage>,
    /// 指针停在上面时的提示
    pub tooltip: Option<String>,
    /// 图标旁边的文字（只有 macOS 显示）
    pub title: Option<String>,
    /// 菜单
    pub menu: Option<MenuModel>,
}

/// 托盘图标被点了（左键）
pub type OnTrayClick = Arc<dyn Fn() + Send + Sync>;

/// 托盘的服务。只在主线程上用
pub trait TrayService: Send + Sync {
    /// 托盘图标此刻该画成什么样（几种边长、颜色、是不是模板图标）。任务栏、面板的深浅变了以后再问一次，颜色可能变了
    fn icon_format(&self) -> TrayIconFormat;

    /// 放上托盘图标，换掉之前的。有 `on_click` 时左键点图标调用它，没有时左键也打开菜单。
    /// 点了菜单项、图标时调用的回调可能不在主线程
    fn set_tray(&self, tray: TrayModel, on_item: OnMenuItem, on_click: Option<OnTrayClick>) -> Result<(), PlatformError>;

    /// 改托盘菜单里的一项
    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError>;

    /// 改图标旁边的文字
    fn set_title(&self, title: Option<&str>) -> Result<(), PlatformError>;

    /// 换图标（几种边长）
    fn set_icon(&self, icons: Vec<TrayImage>) -> Result<(), PlatformError>;

    /// 拿掉托盘图标
    fn remove_tray(&self);
}

/// 单实例和打开请求的服务
pub trait SingleInstanceService: Send + Sync {
    /// 按标识决定这个进程是第一个实例还是后启动的（见 [`crate::single_instance::acquire`]）
    fn acquire(&self, id: &str, wait: bool) -> Result<InstanceRole, PlatformError>;

    /// 系统会不会把打开网址、文档的请求作为事件送来（macOS 打包后）
    fn delivers_open_events(&self) -> bool;

    /// 开始接收系统送来的打开请求（主线程上调用，回调也在主线程）
    fn listen_open_events(&self, on_open: OnOpen);

    /// 启动过程中收下系统送来的打开请求，最多等 `timeout`（后启动的进程转发前用）
    fn collect_launch_events(&self, timeout: Duration) -> Opened;
}
