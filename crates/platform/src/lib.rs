//! 平台层：和窗口系统、桌面打交道的部分，都放在一个后端接口后面（[`PlatformBackend`]）。
//!
//! - **后端**：能力查询（[`Capabilities`]）、平台惯例（[`Conventions`]）、帧时钟（[`frame`]），
//!   以及窗口、外观、运行环境、剪贴板、对话框、外壳、通知、菜单栏、托盘、单实例几项服务，出错统一是 [`PlatformError`]。
//!   本机的后端是 [`NativeBackend`]（[`native`] 取进程里唯一的那个），测试用的是 [`headless::HeadlessBackend`]。
//! - **帧时钟**（[`frame`]）：macOS 的 `CADisplayLink`、Windows 的合成器时钟（或垂直消隐）、
//!   Wayland 的表面帧回调，其余按刷新率的计时器；静止时暂停，不再唤醒。
//! - 系统集成（[`system`]：文件对话框、通知、外观、打开文件），应用被要求打开的网址和文件（[`open`]）、
//!   单实例（[`single_instance`]），本进程的资源用量（[`process`]：常驻内存、CPU 时间、唤醒次数）。
//!
//! crate 根上和 [`system`]、[`open`] 里原来的自由函数（[`display_color`]、[`system::open_url`]……）照旧导出、
//! 行为不变，里面走的是本机后端。不支持的平台上它们返回"没有"（`None`、`false`、空列表），调用方各有退路；
//! 后端的服务则返回带原因的 [`PlatformError`]。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

#[cfg(target_os = "windows")]
mod display;
mod alert;
mod backend;
pub mod clipboard;
#[cfg(target_os = "macos")]
mod drag;
// Windows 的拖到应用外面（`DoDragDrop`），和 macOS 的同名同接口
#[cfg(target_os = "windows")]
#[path = "drag/windows.rs"]
mod drag;
pub mod external_drag;
pub mod print;
pub mod frame;
pub mod headless;
pub mod single_instance;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod live_resize;
mod menu;
#[cfg(target_os = "windows")]
mod modal;
mod native;
pub mod open;
pub mod process;
pub mod system;
pub mod title_bar;

use std::path::PathBuf;

use raw_window_handle::HasWindowHandle;

pub use title_bar::{OnTitleBarChange, ResizeEdge, TitleBar, TitleBarAction, TitleBarMetrics, TitleBarOptions, WindowMaterial};
pub use backend::{
    AppearanceService, Capabilities, ClipboardService, CompositionTarget, Conventions, DialogService, EnvironmentService,
    SingleInstanceService, LineEdgeKeys, MenuEntry, MenuItemId, MenuItemModel, MenuItemUpdate, MenuModel, MenuService, ModifierKey,
    NotificationRequest, NotificationResponse, NotificationService, OnLiveResize, OnMenuItem, OnNotificationResponse, OnTrayClick, PlatformBackend, PlatformError, PlatformWindow, RedoKeys, ShellService,
    SubmenuModel, TrayIconFormat, TrayImage, TrayModel, TrayService, WindowService,
};
pub use frame::{FrameClock, FrameClockKind, FrameClockOptions, FrameRateRange, FrameTime, PresentFeedback, monotonic_time};
pub use clipboard::{ClipFormat, ClipboardContent, RgbaImage};
pub use external_drag::accept_custom_format;
pub use print::{PrintDialogDone, PrintDone, PrintSetup};
pub use external_drag::{ExternalDataKinds, ExternalDrag, ExternalDropData, ExternalDropEffect, OnExternalDrag};
pub use native::{NativeBackend, native, set_thread_backend, with_backend};

/// 窗口所在显示器的颜色能力
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayColor {
    /// 能显示 Display P3 色域
    pub wide_gamut: bool,
    /// 此刻比 SDR 白还能亮几倍（EDR 的余量，1 表示没有）：随亮度、环境光、画面里有没有 HDR 内容变化
    pub headroom: f32,
    /// 最多能有多少余量（显示器的上限，1 表示不支持 HDR）
    pub potential_headroom: f32,
}

impl DisplayColor {
    /// 普通显示器：sRGB 色域，没有余量
    pub const SDR: Self = Self { wide_gamut: false, headroom: 1.0, potential_headroom: 1.0 };

    /// 值得用扩展范围的输出：广色域，或者能显示比 SDR 白亮的颜色
    pub fn is_capable(&self) -> bool {
        self.wide_gamut || self.potential_headroom > 1.0
    }
}

/// 驱动某个显示器的 GPU：PCI 的厂商号、设备号（和图形接口报告的适配器对得上）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayAdapter {
    /// PCI 厂商号
    pub vendor: u32,
    /// PCI 设备号
    pub device: u32,
}

impl Default for DisplayColor {
    fn default() -> Self {
        Self::SDR
    }
}

/// 由显示器刷新驱动的帧时钟（`CADisplayLink`，macOS 14+）。新代码用 [`PlatformBackend::frame_clock`]，
/// 它在每个平台上都有；这个类型保留原来的用法：只在拿到显示链接时才有。
///
/// 回调在主线程的运行循环里触发，每次刷新一次。回调里的 panic 被接住（不穿过系统的栈帧），
/// 显示链接随之暂停，下一次调用 [`set_paused`](Self::set_paused) 时在调用方的栈上重新抛出
pub struct DisplayLink {
    #[cfg(target_os = "macos")]
    clock: frame::DisplayLinkClock,
    #[cfg(not(target_os = "macos"))]
    never: std::convert::Infallible,
}

impl DisplayLink {
    /// 不在主线程、不是 AppKit 窗口或系统低于 macOS 14 时返回 `None`；其他平台总是 `None`
    /// （调用方退回到自己的重绘循环）。必须在窗口之前释放
    pub fn new(window: &impl HasWindowHandle, max_fps: f32, callback: impl Fn(FrameTime) + 'static) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut clock = native().display_link(window, FrameRateRange::up_to(max_fps))?;
            clock.start(Box::new(callback));
            Some(Self { clock })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (window, max_fps, callback);
            None
        }
    }

    /// 暂停（`true`）或继续出帧：暂停期间不调用回调
    pub fn set_paused(&self, paused: bool) {
        #[cfg(target_os = "macos")]
        {
            self.clock.rethrow();
            self.clock.set_paused(paused);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = paused;
            match self.never {}
        }
    }
}

/// 系统是否开着低电量模式。macOS 上开着时 ProMotion 屏上的窗口画面会被限制在 60fps，即使显示器和显示链接都在 120Hz；
/// Windows 上是节电模式（用电池、电量低于设定时系统打开，也可以手动打开）。Linux 总是 `false`
pub fn is_low_power_mode() -> bool {
    native().environment().is_low_power_mode()
}

/// 窗口所在显示器的颜色能力：能不能显示广色域，此刻和最多能比 SDR 白亮几倍（余量）。
/// macOS 上在主线程、窗口在某个显示器上时有结果（EDR）；Windows 上看系统给这台显示器开没开 HDR，开着时余量是显示器的最高亮度比
/// 系统设置里 SDR 内容的亮度，没开时是普通显示器。窗口挪到另一个显示器、调亮度时会变，调用方隔一会儿再问一次；
/// Linux 是 `None`，调用方按普通显示器（sRGB、没有余量）处理
pub fn display_color(window: &impl HasWindowHandle) -> Option<DisplayColor> {
    native().window().display_color(window)
}

/// 用户首选的语言，例如 `"zh-Hans-CN"`、`"en-US"`。macOS 读 `NSLocale`；Windows 读用户界面语言的第一项
/// （如 `"zh-CN"`）；其他系统读语言环境变量（`LANGUAGE`、`LC_ALL`、`LC_MESSAGES`、`LANG`，如 `zh_CN.UTF-8` 写成 `"zh-CN"`）
pub fn preferred_language() -> Option<String> {
    native().environment().preferred_language()
}

/// 当前用户放缓存的目录：macOS 是 `~/Library/Caches`，Windows 是 `%LOCALAPPDATA%`，
/// 其他系统是 `$XDG_CACHE_HOME`（不是绝对路径时不算），没有时是 `~/.cache`。找不到家目录时是 `None`。
/// 里面的东西删了也没关系，下次用到时重新生成（编译好的管线这类）
pub fn user_cache_dir() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Caches"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).filter(|p| p.is_absolute())
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home().map(|h| h.join(".cache")))
    }
}

/// 打包成 .app 时 Info.plist 里的应用名（CFBundleDisplayName，没有就用 CFBundleName）；没打包时、其他平台是 `None`
pub fn bundle_name() -> Option<String> {
    native().environment().bundle_name()
}

/// 密码框有焦点（窗口在前面）时打开系统的安全输入，`on` 为假时关上：打开期间别的程序的键盘监听读不到按键。
/// 只有 macOS 有（`EnableSecureEventInput`），返回做了没有；其他平台返回 `false`（密码框照样不开输入法）
pub fn set_secure_input(on: bool) -> bool {
    native().window().set_secure_input(on).is_ok()
}

/// 系统的安全输入此刻开着（macOS；其他平台是 `false`）
pub fn is_secure_input_enabled() -> bool {
    native().window().is_secure_input_enabled()
}

/// 把窗口带到最前面：取消隐藏应用、激活应用、还原最小化、成为主窗口，返回是否做了。macOS 上只在主线程上有效；
/// Windows 上还原最小化、放到前面，系统不让抢前台时任务栏按钮闪一下；Linux 返回 `false`，调用方用窗口库的办法（还原最小化、聚焦）
pub fn bring_to_front(window: &impl HasWindowHandle) -> bool {
    native().window().bring_to_front(window).is_ok()
}

/// 指针此刻在窗口内容区里的位置（逻辑像素，左上角为原点），也可能在窗口外（负数或超出大小）。
/// 拖放文件时窗口收不到指针移动事件，用它找出文件落在哪个元素上。macOS、Windows 有；Linux 是 `None`：调用方退回到最后已知的指针位置
pub fn pointer_position(window: &impl HasWindowHandle) -> Option<[f32; 2]> {
    native().window().pointer_position(window)
}

/// 开始把内容（文件、文字、带格式的文字、图片、应用自己定的格式）拖到应用外面（macOS、Windows），开始了返回 `true`，
/// 结束时调用 `on_end`（是否放下了）。内容可以是空的：只有这个进程自己的窗口认得
/// （[`external_drag::ExternalDataKinds::own`]）。Linux 还没实现：什么都不做，返回 `false`（不调用 `on_end`）
pub fn begin_drag_out(window: &impl HasWindowHandle, content: &ClipboardContent, at: [f32; 2], on_end: Box<dyn FnOnce(bool)>) -> bool {
    native().window().begin_drag_out(window, content, at, on_end).is_ok()
}
