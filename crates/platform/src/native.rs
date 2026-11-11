//! 本机的平台后端：把各平台的实现（`macos.rs`、`drag.rs`、`system/*`、`open.rs`、`single_instance.rs`、`frame/*`、`menu/*`）
//! 收在 [`PlatformBackend`] 后面。平台的选择只在这个文件和各模块的 `imp` 别名里，不再有按平台各一份的导出清单

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use raw_window_handle::HasWindowHandle;

use crate::backend::{
    AppearanceService, Capabilities, ClipboardService, Conventions, DialogService, EnvironmentService,
    SingleInstanceService, MenuItemId, MenuItemUpdate, MenuModel, MenuService, NotificationRequest, NotificationService, OnMenuItem, OnNotificationResponse, OnTrayClick, PlatformBackend,
    PlatformError, PlatformWindow, ShellService, TrayIconFormat, TrayImage, TrayModel, TrayService, WindowService,
};
use crate::clipboard::{ClipFormat, ClipboardContent, RgbaImage};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::external_drag::OnExternalDrag;
use crate::frame::{FrameClock, FrameClockKind, FrameClockOptions, TimerClock};
use crate::single_instance::InstanceRole;
use crate::open::{OnOpen, Opened};
use crate::system::{self, DialogDone, DialogOptions, SettingsChanged};
use crate::title_bar::{OnTitleBarChange, TitleBarAction, TitleBarMetrics, TitleBarOptions, WindowMaterial};
use crate::{DisplayColor, alert};

#[cfg(target_os = "macos")]
use crate::macos as imp;
#[cfg(not(target_os = "macos"))]
use generic as imp;

/// 本机的平台后端。进程里只有一个（[`native`]），所有服务都能从任何线程拿到
pub struct NativeBackend {
    /// 剪贴板实例，第一次用时建、之后一直留着：X11 上最后一个实例释放时剪贴板的内容会没了，
    /// 释放时还要等剪贴板管理器来取（会卡住调用方）
    clipboard: Mutex<Option<arboard::Clipboard>>,
}

/// 进程里唯一的本机后端
pub fn native() -> &'static NativeBackend {
    static NATIVE: OnceLock<NativeBackend> = OnceLock::new();
    NATIVE.get_or_init(|| NativeBackend { clipboard: Mutex::new(None) })
}

thread_local! {
    /// 这个线程换上的后端（测试工具的无头后端）
    static THREAD_BACKEND: RefCell<Option<Arc<dyn PlatformBackend>>> = const { RefCell::new(None) };
}

/// 在这个线程上换掉应用直接用的服务（剪贴板、通知）的后端：测试工具换成无头后端，测试不碰系统、并行的测试互不干扰。
/// `None` 换回本机后端。窗口和帧时钟不走这里：真实窗口总是本机后端
pub fn set_thread_backend(backend: Option<Arc<dyn PlatformBackend>>) {
    THREAD_BACKEND.with(|b| *b.borrow_mut() = backend);
}

/// 用这个线程该用的后端：换上的（[`set_thread_backend`]），没换时是本机后端
pub fn with_backend<R>(f: impl FnOnce(&dyn PlatformBackend) -> R) -> R {
    let local = THREAD_BACKEND.with(|b| b.borrow().clone());
    match local {
        Some(backend) => f(&*backend),
        None => f(native()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl NativeBackend {
    /// macOS 的显示链接（[`crate::DisplayLink`] 和帧时钟共用）
    #[cfg(target_os = "macos")]
    pub(crate) fn display_link(&self, window: &dyn HasWindowHandle, rate: crate::FrameRateRange) -> Option<crate::frame::DisplayLinkClock> {
        crate::frame::DisplayLinkClock::new(window, rate)
    }

    /// Windows 的消息钩子（装在窗口库的事件循环上）：分发消息之前把菜单栏的快捷键翻译成菜单命令，翻译了返回 `true`，
    /// 这条消息就不再分发。没装菜单栏时什么都不做
    #[cfg(all(feature = "menus", target_os = "windows"))]
    pub fn translate_menu_accelerator(&self, msg: *const std::ffi::c_void) -> bool {
        crate::menu::translate_accelerator(msg)
    }

    /// 编译目标上最好的帧时钟
    fn best_frame_clock(&self) -> FrameClockKind {
        if timer_requested() {
            return FrameClockKind::Timer;
        }
        #[cfg(target_os = "macos")]
        let kind = if objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion().majorVersion >= 14 {
            FrameClockKind::DisplayLink
        } else {
            FrameClockKind::Timer
        };
        #[cfg(target_os = "windows")]
        let kind = crate::frame::windows_clock_kind();
        #[cfg(target_os = "linux")]
        let kind = if std::env::var_os("WAYLAND_DISPLAY").is_some() { FrameClockKind::SurfaceFrame } else { FrameClockKind::Timer };
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let kind = FrameClockKind::Timer;
        kind
    }

    fn with_clipboard<T>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Result<T, PlatformError> {
        let mut slot = lock(&self.clipboard);
        if slot.is_none() {
            *slot = Some(arboard::Clipboard::new().map_err(clipboard_error)?);
        }
        let clipboard = slot.as_mut().ok_or_else(|| PlatformError::System("剪贴板没建起来".into()))?;
        f(clipboard).map_err(clipboard_error)
    }
}

/// 环境变量 `AUREOLE_FRAME_CLOCK` 是 `timer`：不要跟着显示器的时钟，用计时器
fn timer_requested() -> bool {
    std::env::var_os("AUREOLE_FRAME_CLOCK").is_some_and(|v| v == "timer")
}

fn clipboard_error(error: arboard::Error) -> PlatformError {
    match error {
        arboard::Error::ClipboardNotSupported => PlatformError::Unsupported("这个环境没有剪贴板".into()),
        arboard::Error::ClipboardOccupied => PlatformError::System("剪贴板被别的程序占着".into()),
        other => PlatformError::System(other.to_string().into()),
    }
}

/// 路径转成绝对路径，不存在时是错误（不调用系统）
fn existing(path: &Path) -> Result<PathBuf, PlatformError> {
    let absolute = std::path::absolute(path).map_err(PlatformError::from)?;
    if absolute.exists() { Ok(absolute) } else { Err(PlatformError::System(format!("不存在：{}", absolute.display()).into())) }
}

impl PlatformBackend for NativeBackend {
    fn name(&self) -> &'static str {
        if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "other"
        }
    }

    fn capabilities(&self) -> Capabilities {
        let macos = cfg!(target_os = "macos");
        // macOS、Windows 都有的：拖到应用外面、指针位置、低电量（省电模式）
        let macos_or_windows = cfg!(any(target_os = "macos", target_os = "windows"));
        let desktop = cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux"));
        Capabilities {
            frame_clock: self.best_frame_clock(),
            supports_menu_bar: self.menu().is_some(),
            supports_tray: self.tray().is_some(),
            supports_clipboard: desktop,
            supports_clipboard_image: desktop,
            supports_clipboard_files: desktop,
            supports_clipboard_rich_text: cfg!(any(target_os = "macos", target_os = "windows")),
            supports_clipboard_multiple_formats: cfg!(any(target_os = "macos", target_os = "windows")),
            supports_clipboard_custom: cfg!(any(target_os = "macos", target_os = "windows")),
            supports_file_dialog: desktop,
            supports_notification: system::imp::notification_available(),
            supports_notification_actions: system::imp::notification_available(),
            supports_drag_out: macos_or_windows,
            supports_external_drop: cfg!(any(target_os = "macos", target_os = "windows")),
            supports_pointer_position: macos_or_windows,
            supports_display_color: macos,
            supports_open_events: crate::open::imp::delivers_events(),
            supports_settings_watch: cfg!(any(target_os = "windows", target_os = "linux")),
            supports_low_power_detection: macos_or_windows,
            supports_single_instance: cfg!(any(unix, windows)),
            supports_hidden_title_bar: desktop,
            supports_title_bar_controls: macos,
            supports_snap_layouts: snap_layouts_available(),
            supports_window_backdrop: macos || backdrop_available(),
            supports_print: macos_or_windows,
        }
    }

    /// Windows 上滚轮、双击、拖动的那几项跟着此刻的系统设置
    fn conventions(&self) -> &Conventions {
        #[cfg(target_os = "windows")]
        return system::imp::conventions();
        #[cfg(not(target_os = "windows"))]
        Conventions::current()
    }

    /// 环境变量 `AUREOLE_FRAME_CLOCK=timer` 时总是计时器（在任何平台上试退路；显示器睡着、显示链接不走时也能测出帧）
    fn frame_clock(&self, window: &dyn PlatformWindow, options: FrameClockOptions) -> Box<dyn FrameClock> {
        if timer_requested() {
            return Box::new(TimerClock::new(options.rate));
        }
        #[cfg(target_os = "macos")]
        if let Some(clock) = self.display_link(window, options.rate) {
            return Box::new(clock);
        }
        #[cfg(target_os = "windows")]
        if let Some(clock) = crate::frame::windows_clock(window, &options) {
            return Box::new(clock);
        }
        #[cfg(target_os = "linux")]
        if let Some(clock) = crate::frame::WaylandClock::new(window, options.rate) {
            return Box::new(clock);
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let _ = window;
        Box::new(TimerClock::new(options.rate))
    }

    fn window(&self) -> &dyn WindowService {
        self
    }

    fn appearance(&self) -> &dyn AppearanceService {
        self
    }

    fn environment(&self) -> &dyn EnvironmentService {
        self
    }

    fn clipboard(&self) -> &dyn ClipboardService {
        self
    }

    fn dialog(&self) -> &dyn DialogService {
        self
    }

    fn shell(&self) -> &dyn ShellService {
        self
    }

    fn notification(&self) -> &dyn NotificationService {
        self
    }

    /// macOS、Windows 有（muda）；Linux 上窗口库的窗口挂不上菜单栏，没有；关掉 `menus` 特性时都没有
    fn menu(&self) -> Option<&dyn MenuService> {
        if cfg!(all(feature = "menus", any(target_os = "macos", target_os = "windows"))) { Some(self) } else { None }
    }

    /// macOS、Windows 是 tray-icon，Linux 是 D-Bus 上的 StatusNotifierItem（ksni）；关掉 `menus` 特性时都没有
    fn tray(&self) -> Option<&dyn TrayService> {
        if cfg!(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux"))) { Some(self) } else { None }
    }

    fn single_instance(&self) -> &dyn SingleInstanceService {
        self
    }

}

impl WindowService for NativeBackend {
    fn display_color(&self, window: &dyn HasWindowHandle) -> Option<DisplayColor> {
        #[cfg(target_os = "windows")]
        return crate::display::display_color(window);
        #[cfg(not(target_os = "windows"))]
        imp::display_color(window)
    }

    #[cfg(target_os = "windows")]
    fn display_adapter(&self, window: &dyn HasWindowHandle) -> Option<crate::DisplayAdapter> {
        crate::display::display_adapter(window)
    }

    fn pointer_position(&self, window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
        // 拖放目标是自己接的，知道拖放时指针在哪
        #[cfg(target_os = "windows")]
        if let Some(position) = crate::external_drag::windows::drag_position(window) {
            return Some(position);
        }
        imp::pointer_position(window)
    }

    #[cfg(target_os = "macos")]
    fn listen_external_drag(&self, window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
        crate::external_drag::macos::listen(window, on_drag)
    }

    #[cfg(target_os = "macos")]
    fn stop_external_drag(&self, window: &dyn HasWindowHandle) {
        crate::external_drag::macos::stop(window);
    }

    #[cfg(target_os = "windows")]
    fn listen_external_drag(&self, window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
        crate::external_drag::windows::listen(window, on_drag)
    }

    #[cfg(target_os = "windows")]
    fn stop_external_drag(&self, window: &dyn HasWindowHandle) {
        crate::external_drag::windows::stop(window);
    }

    fn activate_if_unpackaged(&self) -> bool {
        imp::activate_if_unpackaged()
    }

    fn handling_wheel_event(&self) -> bool {
        imp::handling_wheel_event()
    }

    fn bring_to_front(&self, window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
        if imp::bring_to_front(window) {
            Ok(())
        } else if cfg!(any(target_os = "macos", target_os = "windows")) {
            Err(PlatformError::System("不在主线程，或者不是这个进程的窗口".into()))
        } else {
            Err(PlatformError::Unsupported("这个平台由窗口库自己把窗口带到前面".into()))
        }
    }

    fn begin_drag_out(
        &self,
        window: &dyn HasWindowHandle,
        content: &ClipboardContent,
        at: [f32; 2],
        on_end: Box<dyn FnOnce(bool)>,
    ) -> Result<(), PlatformError> {
        // macOS 是 AppKit 的拖动会话，Windows 是外壳的 `SHDoDragDrop`（在事件回调外面开始），接口一样
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            if crate::drag::begin_drag_out(window, content, at, on_end) {
                Ok(())
            } else {
                Err(PlatformError::System("开始不了拖动（不在主线程、没有窗口，或者左键已经松开）".into()))
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (window, content, at, on_end);
            Err(PlatformError::Unsupported("这个平台还没实现拖到应用外面".into()))
        }
    }

    fn cancel_drag_out(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            crate::drag::cancel_drag_out()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }

    #[cfg(target_os = "macos")]
    fn set_secure_input(&self, on: bool) -> Result<(), PlatformError> {
        if imp::set_secure_input(on) { Ok(()) } else { Err(PlatformError::System("系统拒绝了安全输入的开关".into())) }
    }

    #[cfg(target_os = "macos")]
    fn is_secure_input_enabled(&self) -> bool {
        imp::is_secure_input_enabled()
    }

    #[cfg(target_os = "windows")]
    fn cancel_ime_composition(&self, window: &dyn HasWindowHandle) -> bool {
        system::imp::cancel_ime_composition(window)
    }

    fn attach_title_bar(&self, window: &dyn HasWindowHandle, options: TitleBarOptions, on_change: OnTitleBarChange) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::title_bar::macos::attach(window, options, on_change);
        #[cfg(target_os = "windows")]
        {
            let _ = (options, on_change);
            crate::title_bar::windows::attach(window)
        }
        // 窗口库管去装饰、拖动和改大小（窗口边缘的命中在运行时里），平台层没有要做的
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (window, options, on_change);
            Ok(())
        }
    }

    fn detach_title_bar(&self, window: &dyn HasWindowHandle) {
        #[cfg(target_os = "macos")]
        crate::title_bar::macos::detach(window);
        // Windows 的子类在窗口销毁时自己放掉
        #[cfg(not(target_os = "macos"))]
        let _ = window;
    }

    fn set_title_bar_options(&self, window: &dyn HasWindowHandle, options: TitleBarOptions) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::title_bar::macos::set_options(window, options);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (window, options);
            Ok(())
        }
    }

    fn refresh_title_bar(&self, window: &dyn HasWindowHandle) {
        #[cfg(target_os = "macos")]
        crate::title_bar::macos::refresh(window);
        #[cfg(not(target_os = "macos"))]
        let _ = window;
    }

    fn title_bar_metrics(&self, window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
        #[cfg(target_os = "macos")]
        return crate::title_bar::macos::metrics(window);
        #[cfg(target_os = "windows")]
        return crate::title_bar::windows::metrics(window);
        // Linux 上系统不给标题栏的高度（标题栏是窗口管理器或者客户端自己画的），给一个常见的高度
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = window;
            Some(TitleBarMetrics { height: 36.0, controls: None })
        }
    }

    #[cfg(target_os = "windows")]
    fn hidden_title_bar_frame(&self, window: &dyn HasWindowHandle) -> Option<[u32; 2]> {
        crate::title_bar::windows::frame_extra(window)
    }

    fn set_maximize_button_area(&self, window: &dyn HasWindowHandle, area: Option<[f32; 4]>) {
        #[cfg(target_os = "windows")]
        crate::title_bar::windows::set_maximize_button_area(window, area);
        #[cfg(not(target_os = "windows"))]
        let _ = (window, area);
    }

    fn title_bar_action(&self) -> TitleBarAction {
        #[cfg(target_os = "macos")]
        return crate::title_bar::macos::double_click_action();
        #[cfg(target_os = "linux")]
        return system::imp::title_bar_action();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        TitleBarAction::Zoom
    }

    /// Windows：菜单栏也换成同样的深浅；老的 Windows 10（1809–1909）的深色外框属性是另一个编号，新的写不上时用老的
    fn set_window_dark(&self, window: &dyn HasWindowHandle, dark: bool, backdrop: WindowMaterial) -> Result<(), PlatformError> {
        #[cfg(target_os = "windows")]
        {
            #[cfg(feature = "menus")]
            crate::menu::menu_bar::set_theme(window, dark);
            let result = crate::title_bar::windows::set_dark(window, dark, backdrop);
            if result.is_err() && system::imp::set_dark_frame_legacy(window, dark) {
                return Ok(());
            }
            result
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (window, dark, backdrop);
            Ok(())
        }
    }

    /// Windows：投递给没有窗口的计时器，由外层的消息循环在窗口库的回调外面执行（拖到应用外面的模态循环要这样开始）
    fn run_outside_event_callback(&self, f: Box<dyn FnOnce()>) {
        #[cfg(target_os = "macos")]
        imp::run_outside_event_callback(f);
        #[cfg(target_os = "windows")]
        crate::drag::run_outside_event_callback(f);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        f();
    }

    #[cfg(target_os = "windows")]
    fn watch_live_resize(&self, window: &dyn HasWindowHandle, on_resize: crate::OnLiveResize) -> Result<(), PlatformError> {
        crate::live_resize::watch(window, on_resize)
    }

    #[cfg(target_os = "windows")]
    fn set_window_background(&self, window: &dyn HasWindowHandle, color: Option<[u8; 3]>) {
        crate::live_resize::set_background(window, color);
    }

    #[cfg(target_os = "windows")]
    fn in_modal_loop(&self) -> bool {
        crate::modal::in_modal_loop()
    }

    #[cfg(target_os = "windows")]
    fn wake_in_modal_loop(&self, after: Option<Duration>, waker: &std::task::Waker) {
        crate::modal::wake_after(after, waker);
    }

    fn set_window_backdrop(&self, window: &dyn HasWindowHandle, backdrop: WindowMaterial) -> Result<(), PlatformError> {
        #[cfg(target_os = "macos")]
        return crate::title_bar::macos::set_backdrop(window, backdrop);
        #[cfg(target_os = "windows")]
        return crate::title_bar::windows::set_backdrop(window, backdrop);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (window, backdrop);
            Err(PlatformError::Unsupported("这个平台没有窗口材质".into()))
        }
    }

    fn composition_target(&self, window: &dyn HasWindowHandle) -> Result<Option<Box<dyn crate::backend::CompositionTarget>>, PlatformError> {
        #[cfg(target_os = "windows")]
        return crate::title_bar::windows::composition_target(window).map(Some);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = window;
            Ok(None)
        }
    }
}

/// 有贴靠布局（Windows 11）
fn snap_layouts_available() -> bool {
    #[cfg(target_os = "windows")]
    return crate::title_bar::windows::supports_snap_layouts();
    #[cfg(not(target_os = "windows"))]
    false
}

/// Windows 上系统的版本够不够窗口材质（macOS 总有）
fn backdrop_available() -> bool {
    #[cfg(target_os = "windows")]
    return crate::title_bar::windows::supports_backdrop();
    #[cfg(not(target_os = "windows"))]
    false
}

impl AppearanceService for NativeBackend {
    fn is_dark_mode(&self) -> Option<bool> {
        system::imp::is_dark_mode()
    }

    fn is_panel_dark(&self) -> Option<bool> {
        system::imp::is_panel_dark()
    }

    fn accent_color(&self) -> Option<[u8; 3]> {
        system::imp::accent_color()
    }

    fn is_motion_reduced(&self) -> bool {
        system::imp::is_motion_reduced()
    }

    fn watch_settings(&self, on_change: SettingsChanged) {
        system::imp::watch_settings(on_change);
    }
}

impl EnvironmentService for NativeBackend {
    fn preferred_language(&self) -> Option<String> {
        imp::preferred_language()
    }

    fn is_low_power_mode(&self) -> bool {
        imp::is_low_power_mode()
    }

    fn bundle_name(&self) -> Option<String> {
        imp::bundle_name()
    }
}

impl ClipboardService for NativeBackend {
    fn read_text(&self) -> Result<Option<String>, PlatformError> {
        self.with_clipboard(|clipboard| match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            // 剪贴板里没有文字
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error),
        })
    }

    /// Windows 的换行写成 `\r\n`（Windows 的程序认这个）
    fn write_text(&self, text: &str) -> Result<(), PlatformError> {
        #[cfg(target_os = "windows")]
        let text = crate::clipboard::windows::to_crlf(text);
        self.with_clipboard(|clipboard| clipboard.set_text(text))
    }

    #[cfg(target_os = "macos")]
    fn formats(&self) -> Result<Vec<ClipFormat>, PlatformError> {
        Ok(crate::clipboard::macos::formats())
    }

    #[cfg(target_os = "windows")]
    fn formats(&self) -> Result<Vec<ClipFormat>, PlatformError> {
        Ok(crate::clipboard::windows::formats())
    }

    /// arboard 问不了有哪些格式，只能每种读一次（要把数据传过来）。不常用，先这样
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn formats(&self) -> Result<Vec<ClipFormat>, PlatformError> {
        let mut formats = Vec::new();
        if !self.read_files()?.is_empty() {
            formats.push(ClipFormat::Files);
        }
        if self.read_image()?.is_some() {
            formats.push(ClipFormat::Image);
        }
        if self.read_text()?.is_some() {
            formats.push(ClipFormat::Text);
        }
        Ok(formats)
    }

    #[cfg(target_os = "macos")]
    fn read_rich_text(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        Ok(crate::clipboard::macos::read_rich_text())
    }

    #[cfg(target_os = "windows")]
    fn read_rich_text(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        crate::clipboard::windows::read_rich_text()
    }

    /// arboard 没有带格式文字的通用接口，读到的总是没有
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn read_rich_text(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        Ok(None)
    }

    #[cfg(target_os = "macos")]
    fn read_image(&self) -> Result<Option<RgbaImage>, PlatformError> {
        Ok(crate::clipboard::macos::read_image())
    }

    #[cfg(target_os = "windows")]
    fn read_image(&self) -> Result<Option<RgbaImage>, PlatformError> {
        crate::clipboard::windows::read_image()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn read_image(&self) -> Result<Option<RgbaImage>, PlatformError> {
        self.with_clipboard(|clipboard| match clipboard.get_image() {
            Ok(image) => Ok(Some(RgbaImage { width: image.width as u32, height: image.height as u32, rgba: Arc::from(image.bytes.into_owned()) })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error),
        })
    }

    #[cfg(target_os = "macos")]
    fn read_files(&self) -> Result<Vec<PathBuf>, PlatformError> {
        Ok(crate::clipboard::macos::read_files())
    }

    #[cfg(target_os = "windows")]
    fn read_files(&self) -> Result<Vec<PathBuf>, PlatformError> {
        crate::clipboard::windows::read_files()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn read_files(&self) -> Result<Vec<PathBuf>, PlatformError> {
        self.with_clipboard(|clipboard| match clipboard.get().file_list() {
            Ok(files) => Ok(files),
            Err(arboard::Error::ContentNotAvailable) => Ok(Vec::new()),
            Err(error) => Err(error),
        })
    }

    #[cfg(target_os = "macos")]
    fn read_custom(&self, format: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        Ok(crate::clipboard::macos::read_custom(format))
    }

    #[cfg(target_os = "windows")]
    fn read_custom(&self, format: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        crate::clipboard::windows::read_custom(format)
    }

    #[cfg(target_os = "macos")]
    fn write(&self, content: &ClipboardContent) -> Result<(), PlatformError> {
        crate::clipboard::macos::write(content)
    }

    #[cfg(target_os = "windows")]
    fn write(&self, content: &ClipboardContent) -> Result<(), PlatformError> {
        crate::clipboard::windows::write(content)
    }

    /// arboard 一次只放一种：文件、图片、文字里最优先的一种（带格式的文字只剩它的纯文字）；应用自己定的格式放不了
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn write(&self, content: &ClipboardContent) -> Result<(), PlatformError> {
        if !content.custom.is_empty() {
            tracing::debug!("这个平台的剪贴板放不了应用自己定的格式，只放别的");
        }
        if let Some(image) = content.image.as_ref().filter(|image| !image.is_valid()) {
            return Err(PlatformError::System(format!("图片的像素数不对：{}×{}，{} 字节", image.width, image.height, image.rgba.len()).into()));
        }
        self.with_clipboard(|clipboard| {
            if !content.files.is_empty() {
                clipboard.set().file_list(&content.files)
            } else if let Some(image) = &content.image {
                clipboard.set_image(arboard::ImageData {
                    width: image.width as usize,
                    height: image.height as usize,
                    bytes: std::borrow::Cow::Borrowed(&image.rgba),
                })
            } else if let Some(text) = &content.text {
                clipboard.set_text(text)
            } else {
                clipboard.clear()
            }
        })
    }
}

impl DialogService for NativeBackend {
    fn file_dialog(&self, options: DialogOptions, parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
        system::imp::file_dialog(options, parent, done);
    }

    fn cancel_file_dialogs(&self) -> usize {
        system::imp::cancel_file_dialogs()
    }

    fn alert(&self, title: &str, message: &str) {
        alert::show(title, message);
    }

    // macOS 是 NSPrintPanel / NSPrintOperation，Windows 是 PrintDlgEx / StartDoc（见 print.rs）
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn print_dialog(&self, parent: Option<&dyn HasWindowHandle>, done: crate::PrintDialogDone) {
        #[cfg(target_os = "macos")]
        crate::print::macos::print_dialog(parent, done);
        #[cfg(target_os = "windows")]
        crate::print::windows::print_dialog(parent, done);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn print(&self, setup: crate::PrintSetup, title: &str, pages: Vec<crate::RgbaImage>, done: crate::PrintDone) {
        #[cfg(target_os = "macos")]
        crate::print::macos::print(setup, title, pages, done);
        #[cfg(target_os = "windows")]
        crate::print::windows::print(setup, title, pages, done);
    }
}

impl ShellService for NativeBackend {
    fn open_url(&self, url: &str) -> Result<(), PlatformError> {
        // Windows 的外壳什么都打开（本机路径、共享路径上的程序会被直接运行）：不是带协议的网址就不交出去
        #[cfg(target_os = "windows")]
        if !system::imp::is_shell_url(url) {
            return Err(PlatformError::System(format!("不是能交给系统打开的网址（要有协议名，不收本机路径和 file:）：{url}").into()));
        }
        if system::imp::open_url(url) { Ok(()) } else { Err(PlatformError::System(format!("系统没能打开网址 {url}").into())) }
    }

    fn open_path(&self, path: &Path) -> Result<(), PlatformError> {
        let path = existing(path)?;
        if system::imp::open_path(&path) { Ok(()) } else { Err(PlatformError::System(format!("系统没能打开 {}", path.display()).into())) }
    }

    fn reveal_path(&self, path: &Path) -> Result<(), PlatformError> {
        let path = existing(path)?;
        if system::imp::reveal_path(&path) {
            Ok(())
        } else {
            Err(PlatformError::System(format!("没能在文件管理器里显示 {}", path.display()).into()))
        }
    }
}

impl NotificationService for NativeBackend {
    fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError> {
        system::imp::notify(title, body)
    }

    fn show(&self, request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError> {
        system::imp::show_notification(request, on_response)
    }

    fn set_app_name(&self, name: &str) {
        system::imp::set_app_name(name);
    }

    fn beep(&self) -> Result<(), PlatformError> {
        system::imp::beep()
    }
}

/// 没有菜单栏、托盘的平台上的回答
#[cfg_attr(all(feature = "menus", any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn no_menu() -> PlatformError {
    PlatformError::Unsupported("这个平台没有原生菜单栏".into())
}

#[cfg_attr(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")), allow(dead_code))]
fn no_tray() -> PlatformError {
    PlatformError::Unsupported("这个平台没有托盘".into())
}

impl MenuService for NativeBackend {
    fn set_menu_bar(&self, menu: MenuModel, app_name: &str, on_item: OnMenuItem) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
        return crate::menu::menu_bar::set(menu, app_name, on_item);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows"))))]
        {
            let _ = (menu, app_name, on_item);
            Err(no_menu())
        }
    }

    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
        return crate::menu::menu_bar::update(id, update);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows"))))]
        {
            let _ = (id, update);
            Err(no_menu())
        }
    }

    fn attach_window(&self, window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
        return crate::menu::menu_bar::attach(window);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows"))))]
        {
            let _ = window;
            Err(no_menu())
        }
    }

    fn detach_window(&self, window: &dyn HasWindowHandle) {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
        crate::menu::menu_bar::detach(window);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows"))))]
        let _ = window;
    }
}

impl TrayService for NativeBackend {
    fn icon_format(&self) -> TrayIconFormat {
        crate::menu::icon_format()
    }

    fn set_tray(&self, tray: TrayModel, on_item: OnMenuItem, on_click: Option<OnTrayClick>) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        return crate::menu::tray::set(tray, on_item, on_click);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
        {
            let _ = (tray, on_item, on_click);
            Err(no_tray())
        }
    }

    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        return crate::menu::tray::update(id, update);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
        {
            let _ = (id, update);
            Err(no_tray())
        }
    }

    fn set_title(&self, title: Option<&str>) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        return crate::menu::tray::set_title(title);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
        {
            let _ = title;
            Err(no_tray())
        }
    }

    fn set_icon(&self, icons: Vec<TrayImage>) -> Result<(), PlatformError> {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        return crate::menu::tray::set_icon(icons);
        #[cfg(not(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
        {
            let _ = icons;
            Err(no_tray())
        }
    }

    fn remove_tray(&self) {
        #[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        crate::menu::tray::remove();
    }
}

impl SingleInstanceService for NativeBackend {
    fn acquire(&self, id: &str, wait: bool) -> Result<InstanceRole, PlatformError> {
        crate::single_instance::acquire(id, wait).map_err(PlatformError::from)
    }

    fn delivers_open_events(&self) -> bool {
        crate::open::imp::delivers_events()
    }

    fn listen_open_events(&self, on_open: OnOpen) {
        crate::open::imp::listen(on_open);
    }

    fn collect_launch_events(&self, timeout: Duration) -> Opened {
        crate::open::imp::collect_launch(timeout)
    }
}

/// macOS 以外：窗口和运行环境的几项服务大多没有（返回"没有"，调用方各有退路），首选语言有
#[cfg(not(target_os = "macos"))]
mod generic {
    use raw_window_handle::HasWindowHandle;

    /// Windows：省电模式（节电模式、节能模式）开着；其他平台总是 `false`
    pub fn is_low_power_mode() -> bool {
        #[cfg(target_os = "windows")]
        return crate::system::windows::is_low_power_mode();
        #[cfg(not(target_os = "windows"))]
        false
    }

    /// Windows 的在 `display.rs`
    #[cfg(not(target_os = "windows"))]
    pub fn display_color(_window: &dyn HasWindowHandle) -> Option<crate::DisplayColor> {
        None
    }

    /// Windows：用户界面语言的第一项（如 `"zh-CN"`）；其他系统：语言环境变量
    pub fn preferred_language() -> Option<String> {
        #[cfg(target_os = "windows")]
        if let Some(language) = windows_language() {
            return Some(language);
        }
        crate::system::portable::language_from_env(|key| std::env::var(key).ok())
    }

    #[cfg(target_os = "windows")]
    fn windows_language() -> Option<String> {
        use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
        let (mut count, mut len) = (0u32, 0u32);
        // 先问需要多大的缓冲区，再取；结果是以两个 0 结尾的多个字符串
        // SAFETY: 第一次缓冲区为空、只写长度；第二次缓冲区有 len 个 u16
        unsafe {
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, std::ptr::null_mut(), &mut len) == 0 || len == 0 {
                return None;
            }
            let mut buffer = vec![0u16; len as usize];
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, buffer.as_mut_ptr(), &mut len) == 0 {
                return None;
            }
            let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            (end > 0).then(|| String::from_utf16_lossy(&buffer[..end]))
        }
    }

    pub fn bundle_name() -> Option<String> {
        None
    }

    /// Windows：还原最小化、设成前台窗口（系统不许抢前台时闪任务栏按钮）；其他平台由窗口库做，返回 `false`
    pub fn bring_to_front(window: &dyn HasWindowHandle) -> bool {
        #[cfg(target_os = "windows")]
        return crate::system::windows::bring_to_front(window);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = window;
            false
        }
    }

    /// 只有 macOS 要：别的平台上从终端启动的窗口照常拿得到键盘
    pub fn activate_if_unpackaged() -> bool {
        false
    }

    /// 只有 macOS 的窗口库在滚轮事件里补发指针移动
    pub fn handling_wheel_event() -> bool {
        false
    }

    /// Windows：问系统指针在哪（`GetCursorPos`），换到窗口里的逻辑像素；其他平台是 `None`
    pub fn pointer_position(window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
        #[cfg(target_os = "windows")]
        return crate::system::windows::pointer_position(window);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = window;
            None
        }
    }

}
