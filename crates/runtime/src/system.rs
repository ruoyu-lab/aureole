//! 系统集成的平台一侧：运行时用的平台后端（[`backend`]），把它装进核心的注入点（[`SystemBackend`]：系统设置、剪贴板、
//! 打开网址），外观、减少动态效果、强调色变了时报告给核心；另有只在桌面上才有的服务：用默认应用打开文件、在访达里显示、系统通知。
//!
//! 平台相关的都从后端的服务走（三个平台各有实现：macOS 是 AppKit；Windows 是注册表、Shell、WinRT 的通知；
//! Linux 是 D-Bus 上的 XDG 门户和通知服务），各平台的差别见各函数的说明。服务出错时带着原因（`PlatformError`），
//! 这里把原因记进日志，对应用照旧返回 `bool`。
//!
//! 对应用公开的读设置、剪贴板、打开网址的函数先装上平台的实现再交给核心：在 `App::run` 之前（比如 `main` 里）调用也问得到系统

use std::path::Path;
use std::sync::Once;

use platform::system::{DialogKind, DialogOptions, FileFilter};
use platform::{LineEdgeKeys, ModifierKey, PlatformBackend, RedoKeys};
use scene::Color;

use aureole_core::host::SystemBackend;
use aureole_core::{Appearance, Conventions, Date, FileDialogDone, FileDialogKind, FileDialogRequest, Modifiers, Time};
use winit::raw_window_handle::HasWindowHandle;

/// 运行时用的平台后端：进程里唯一的本机后端。平台相关的事（帧时钟、系统设置、剪贴板、对话框、通知、菜单托盘……）都从它走
pub(crate) fn backend() -> &'static dyn PlatformBackend {
    platform::native()
}

/// 平台的实现：设置、剪贴板、打开网址都问后端。剪贴板是后端里整个进程唯一的实例（X11 上最后一个实例释放时内容会没了）
struct PlatformSystem;

impl SystemBackend for PlatformSystem {
    fn appearance(&self) -> Appearance {
        detect()
    }

    fn is_motion_reduced(&self) -> bool {
        backend().appearance().is_motion_reduced()
    }

    fn accent_color(&self) -> Option<Color> {
        detect_accent()
    }

    fn read_clipboard(&self) -> Option<String> {
        backend().clipboard().read_text().unwrap_or_else(|error| {
            tracing::debug!("读不了剪贴板：{error}");
            None
        })
    }

    fn write_clipboard(&self, text: String) -> bool {
        backend().clipboard().write_text(&text).map_err(|error| tracing::warn!("写不进剪贴板：{error}")).is_ok()
    }

    fn open_url(&self, url: &str) -> bool {
        backend().shell().open_url(url).map_err(|error| tracing::warn!(%url, "打不开网址：{error}")).is_ok()
    }

    fn conventions(&self) -> Conventions {
        conventions(backend().conventions())
    }

    fn file_dialog(&self, request: FileDialogRequest, done: FileDialogDone) {
        // 在窗口里调用时是这个窗口的表单
        let parent = crate::window::current_window().and_then(crate::window::native);
        backend().dialog().file_dialog(dialog_options(request), parent.as_deref().map(|w| w as &dyn HasWindowHandle), done);
    }

    fn local_time(&self) -> Option<(Date, Time)> {
        let [year, month, day, hour, minute, second] = platform::system::local_time()?;
        // 闰秒可能报告 60：停在 59
        let time = Time::checked(u8::try_from(hour).ok()?, u8::try_from(minute).ok()?, u8::try_from(second).ok()?.min(59))?;
        Some((Date::checked(year, u8::try_from(month).ok()?, u8::try_from(day).ok()?)?, time))
    }
}

/// 核心的请求换成平台层的设置
pub(crate) fn dialog_options(request: FileDialogRequest) -> DialogOptions {
    DialogOptions {
        kind: match request.kind {
            FileDialogKind::OpenFile => DialogKind::OpenFile,
            FileDialogKind::OpenDirectory => DialogKind::OpenDirectory,
            FileDialogKind::Save => DialogKind::Save,
        },
        multiple: request.multiple,
        title: request.title,
        directory: request.directory,
        file_name: request.file_name,
        filters: request.filters.into_iter().map(|f| FileFilter { name: f.name, extensions: f.extensions }).collect(),
    }
}

/// 这个平台此刻能做什么（有没有菜单栏、托盘、系统通知，拖不拖得到应用外面……）：
/// 没有的时候把入口放进窗口里，而不是让用户点了没反应。关掉 `menus` 特性时菜单栏和托盘都报告没有。
///
/// ```no_run
/// let can = aureole::capabilities();
/// if !can.supports_tray {
///     // 没有托盘：常驻的状态放在窗口里
/// }
/// ```
pub fn capabilities() -> platform::Capabilities {
    platform::with_backend(|backend| backend.capabilities())
}

/// 平台层的惯例换成核心的
fn conventions(platform: &platform::Conventions) -> Conventions {
    let modifier = |key| match key {
        ModifierKey::Shift => Modifiers::SHIFT,
        ModifierKey::Control => Modifiers::CONTROL,
        ModifierKey::Alt => Modifiers::ALT,
        ModifierKey::Command => Modifiers::SUPER,
    };
    Conventions {
        primary_modifier: modifier(platform.primary_modifier),
        word_modifier: modifier(platform.word_modifier),
        uses_arrows_for_line_edges: platform.line_edge == LineEdgeKeys::PrimaryArrows,
        accepts_y_for_redo: platform.redo == RedoKeys::YOrShiftZ,
        wheel_line_height: platform.wheel_line_height,
        treats_control_click_as_secondary: platform.treats_control_click_as_secondary,
        long_press_duration: platform.long_press_duration,
    }
}

/// 装上平台的实现（整个进程一次）：`App` 创建时、应用第一次直接用系统设置和剪贴板时
pub(crate) fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| aureole_core::host::set_system_backend(PlatformSystem));
}

fn detect() -> Appearance {
    if backend().appearance().is_dark_mode() == Some(true) { Appearance::Dark } else { Appearance::Light }
}

fn detect_accent() -> Option<Color> {
    backend().appearance().accent_color().map(|[r, g, b]| Color::rgb(r, g, b))
}

/// 窗口报告的外观
pub(crate) fn appearance_from_winit(theme: winit::window::Theme) -> Appearance {
    match theme {
        winit::window::Theme::Light => Appearance::Light,
        winit::window::Theme::Dark => Appearance::Dark,
    }
}

/// 重新问系统"减少动态效果"和强调色：窗口获得焦点、外观变化时调用（几次系统调用，没变时什么都不做）
pub(crate) fn refresh_settings() {
    aureole_core::host::set_motion_reduced(backend().appearance().is_motion_reduced());
    aureole_core::host::set_system_accent_color(detect_accent());
}

/// 后台监视到系统设置变了（Windows 的注册表、Linux 的门户）：重新读外观、减少动态效果、强调色
pub(crate) fn settings_changed() {
    aureole_core::host::set_system_appearance(detect());
    refresh_settings();
}

/// 系统外观（深色 / 浅色），响应式：在绑定里读，用户在系统设置里切换时界面跟着变。
/// 全应用一个，不分窗口。测试工具里固定为浅色，用 `Harness::set_system_appearance` 模拟切换。
///
/// macOS、Windows 由窗口报告变化（Windows 另外监视注册表，只有托盘、没有窗口时也收得到）；
/// Linux 读 XDG 门户的 `color-scheme`，后台线程听它的变化（GNOME、KDE 都支持）
pub fn system_appearance() -> Appearance {
    install();
    aureole_core::system_appearance()
}

/// 系统打开了"减少动态效果"（辅助功能设置），响应式。应用自己的动画按它决定要不要用移动和缩放；
/// 框架里本来就动的东西（拖动排序的行让位、共享元素的飞行）在它打开时立即到位或只淡入淡出。
///
/// macOS 读 `NSWorkspace` 的辅助功能设置，Windows 读"动画效果"的系统参数，Linux 读 XDG 门户的
/// `reduced-motion`（以及 GNOME 关掉动画、KDE 把动画速度调到 0）。
/// 启动时读一次，之后窗口重新获得焦点、系统外观变化时再读（在系统设置里改了、回到应用就生效；Linux 上改了马上生效）。
/// 测试工具里固定为 `false`，用 `Harness::set_motion_reduced` 模拟
pub fn is_motion_reduced() -> bool {
    install();
    aureole_core::is_motion_reduced()
}

/// 系统的强调色（macOS：系统设置 → 外观；Windows：个性化 → 颜色；Linux：门户的 `accent-color`，GNOME 47+、KDE），
/// 响应式；读不到时是 `None`。和"减少动态效果"一样在启动时、窗口获得焦点时、系统外观变化时重新读。
///
/// 框架自带的样子不用它（有自己固定的强调色）；想跟着系统时在样式对象、绑定里读它：
/// `move || aureole::system_accent_color().unwrap_or(默认色)`。测试工具里固定为 `None`
pub fn system_accent_color() -> Option<Color> {
    install();
    aureole_core::system_accent_color()
}

/// 用系统默认的应用打开网址，例如 `https://…`、`mailto:…`。返回是否交给了系统。
/// 测试工具里不真的打开，记下来（`Harness::opened_urls`）
pub fn open_url(url: &str) -> bool {
    install();
    aureole_core::open_url(url)
}

/// 用默认应用打开文件，文件夹在访达（文件管理器）里打开。不存在时返回 `false`。
/// macOS 用 NSWorkspace，Windows 用 ShellExecuteW，Linux 用 `xdg-open`
pub fn open_path(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    backend().shell().open_path(path).map_err(|error| tracing::warn!(path = %path.display(), "打不开：{error}")).is_ok()
}

/// 在访达（文件管理器）里显示并选中文件。不存在时返回 `false`。
/// Windows 用 SHOpenFolderAndSelectItems；Linux 用 `org.freedesktop.FileManager1`（GNOME、KDE 的文件管理器都支持），
/// 不支持时打开所在的文件夹
pub fn reveal_path(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    backend().shell().reveal_path(path).map_err(|error| tracing::warn!(path = %path.display(), "没能在文件管理器里显示：{error}")).is_ok()
}

/// 发系统通知，返回是否交给了系统。系统通知不可用时把不可用的原因和通知的内容记下来（`tracing` 的 info），返回 `false`：
///
/// - macOS：要求应用打包成 .app（有包标识符），第一次发时系统会请求权限；没打包时（比如 `cargo run`）不可用。
/// - Windows：要有应用标识（AppUserModelID）：从带标识的开始菜单快捷方式启动（安装程序建的）、MSIX 包，
///   或者进程自己设过；直接运行可执行文件（比如 `cargo run`）时不可用。
/// - Linux：发给桌面的通知服务（`org.freedesktop.Notifications`），不用打包；没有通知服务时不可用
pub fn notify(title: &str, body: &str) -> bool {
    match platform::with_backend(|backend| backend.notification().notify(title, body)) {
        Ok(()) => true,
        Err(error) => {
            // 原因由平台给：没打包成 .app、没有应用标识、没有会话总线……
            tracing::info!(%title, %body, "系统通知不可用（{}），通知的内容记在这里", error.reason());
            false
        }
    }
}
