//! 系统设置和系统服务的核心一侧：系统外观（深浅色）、强调色、减少动态效果、剪贴板、用默认应用打开网址、平台的惯例。
//!
//! 这里只有数据和注入点：读到的设置是信号，宿主报告变化时写进去；剪贴板、打开网址、平台的惯例（[`Conventions`]：
//! 主修饰键、按词移动的修饰键、滚轮一行滚多远……）交给装上的实现（[`SystemBackend`]）。
//! 运行时装上平台的实现（macOS：AppKit；Windows：注册表、Shell；Linux：D-Bus 上的 XDG 门户），
//! 测试工具在自己的线程上换成进程内的替身，不碰用户的剪贴板，也不真的打开网址。
//!
//! ```no_run
//! # use aureole::prelude::*;
//! # use aureole::Appearance;
//! // 跟着系统外观变色：用户在系统设置里切换深浅色时，绑定它的属性自动更新
//! rect().background(move || if aureole::system_appearance() == Appearance::Dark { Color::hex(0x1c1c1e) } else { Color::WHITE });
//! button("文档").on_click(|| aureole::open_url("https://www.rust-lang.org"));
//! aureole::clipboard::write_text("复制的文字");
//! aureole::notify("导出完成", "已保存到下载文件夹");
//! ```

use crate::date::{Date, Time};
use crate::file_dialog::{FileDialogDone, FileDialogRequest};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use reactive::{Signal, signal};
use scene::Color;

use crate::input::Modifiers;

/// 系统外观（深色 / 浅色）。框架自带的样子只有浅色一套，不跟着它变；应用想跟着系统换颜色时在绑定里读 [`crate::system_appearance`]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Appearance {
    /// 浅色（默认）
    #[default]
    Light,
    /// 深色
    Dark,
}

impl Appearance {
    /// 是否是深色
    pub fn is_dark(self) -> bool {
        self == Appearance::Dark
    }
}

/// 平台的惯例：同一个操作在各平台上习惯用的键、滚轮一行滚多远。输入、编辑、滚动按它走，不再各自按编译目标判断。
///
/// 运行时从平台层读（[`SystemBackend::conventions`]）；测试工具固定成 macOS 的一套（测试按 Command 写快捷键，
/// 在哪个系统上跑结果都一样），也可以换成别的平台的。没有装实现时是编译目标的系统的（[`Conventions::current`]）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Conventions {
    /// 快捷键的主修饰键（快捷键写法里的 `CmdOrCtrl`）：macOS 是 Command（[`Modifiers::SUPER`]），其他平台是 Control
    pub primary_modifier: Modifiers,
    /// 按词移动、按词删除的修饰键：macOS 是 Option（[`Modifiers::ALT`]），其他平台是 Control
    pub word_modifier: Modifiers,
    /// 主修饰键加左右方向键到行首行尾、加上下方向键到文档头尾（macOS）；为假时用 Home、End，主修饰键加 Home、End 到文档头尾
    pub uses_arrows_for_line_edges: bool,
    /// 主修饰键加 Y 也是重做（Windows）；主修饰键加 Shift 加 Z 在所有平台都是
    pub accepts_y_for_redo: bool,
    /// 滚轮按行报告时一行滚多远（逻辑像素）。macOS 的滚轮事件自带加速（转得快时一格有好几行），一行短一些；
    /// 其他平台一格就是一行，按常见的三行文字算
    pub wheel_line_height: f32,
    /// 按住 Control 点左键算右键点击（macOS，有元素接右键时）
    pub treats_control_click_as_secondary: bool,
    /// 手指按住不动多久算长按（弹右键菜单、选词）。Windows 的按住比别的系统慢（系统设置里是档位，没有公开的毫秒数，这里是默认档大约的时长）
    pub long_press_duration: Duration,
}

impl Conventions {
    /// macOS 的惯例
    pub const MACOS: Self = Self {
        primary_modifier: Modifiers::SUPER,
        word_modifier: Modifiers::ALT,
        uses_arrows_for_line_edges: true,
        accepts_y_for_redo: false,
        wheel_line_height: 40.0,
        treats_control_click_as_secondary: true,
        long_press_duration: Duration::from_millis(500),
    };

    /// Windows 的惯例
    pub const WINDOWS: Self = Self {
        primary_modifier: Modifiers::CONTROL,
        word_modifier: Modifiers::CONTROL,
        uses_arrows_for_line_edges: false,
        accepts_y_for_redo: true,
        wheel_line_height: 100.0,
        treats_control_click_as_secondary: false,
        long_press_duration: Duration::from_millis(800),
    };

    /// Linux 桌面的惯例
    pub const LINUX: Self = Self {
        primary_modifier: Modifiers::CONTROL,
        word_modifier: Modifiers::CONTROL,
        uses_arrows_for_line_edges: false,
        accepts_y_for_redo: false,
        wheel_line_height: 100.0,
        treats_control_click_as_secondary: false,
        long_press_duration: Duration::from_millis(500),
    };

    /// 编译目标的系统的惯例（其他系统按 Linux 的）
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MACOS
        } else if cfg!(target_os = "windows") {
            Self::WINDOWS
        } else {
            Self::LINUX
        }
    }
}

/// 一个显示器（[`crate::displays`]）。位置和大小是逻辑像素：显示器的物理像素按它自己的缩放比例换算，
/// 窗口在这个显示器上时和 `WindowHandle::position`、`set_position` 是同一套坐标（macOS 上就是系统的点坐标）
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayInfo {
    /// 系统给的名字；没有时是空的
    pub name: String,
    /// 左上角在桌面上的位置
    pub position: [f32; 2],
    /// 宽和高
    pub size: [f32; 2],
    /// 一个逻辑像素是几个物理像素
    pub scale_factor: f32,
    /// 是不是主显示器（macOS 上带菜单栏的那个）
    pub primary: bool,
}

impl DisplayInfo {
    /// 桌面上的点 `[x, y]`（逻辑像素）在不在这个显示器上
    pub fn contains(&self, point: [f32; 2]) -> bool {
        let [x, y] = point;
        x >= self.position[0] && y >= self.position[1] && x < self.position[0] + self.size[0] && y < self.position[1] + self.size[1]
    }
}

/// 系统那一侧的实现：系统设置、剪贴板、用默认应用打开网址、平台的惯例。
///
/// 运行时启动时装上平台的实现（[`set_system_backend`]），整个进程一份；测试工具在自己的线程上换成进程内的替身
/// （[`set_thread_system_backend`]）。都没有时（只用核心、没有宿主）外观是浅色、不减少动态效果、没有强调色，
/// 剪贴板读不到也写不进，网址打不开。
///
/// 设置只在第一次读、还没有值时问这里；之后的变化由宿主报告（[`set_system_appearance`] 等）
pub trait SystemBackend: Send + Sync {
    /// 系统外观；问不到时按浅色
    fn appearance(&self) -> Appearance;
    /// 系统打开了"减少动态效果"
    fn is_motion_reduced(&self) -> bool;
    /// 系统的强调色；读不到时是 `None`
    fn accent_color(&self) -> Option<Color>;
    /// 剪贴板里的文字；没有文字或读不了时是 `None`
    fn read_clipboard(&self) -> Option<String>;
    /// 把文字放进剪贴板，返回是否成功
    fn write_clipboard(&self, text: String) -> bool;
    /// 用系统默认的应用打开网址，返回是否交给了系统
    fn open_url(&self, url: &str) -> bool;
    /// 平台的惯例；默认是编译目标的系统的
    fn conventions(&self) -> Conventions {
        Conventions::current()
    }
    /// 弹出文件对话框，用户选好或取消时调用 `done`（可以在别的线程）。默认没有对话框：马上以空列表完成
    fn file_dialog(&self, request: FileDialogRequest, done: FileDialogDone) {
        let _ = request;
        done(Vec::new());
    }
    /// 本地的日期和时间；问不到时是 `None`，这时按世界协调时算
    fn local_time(&self) -> Option<(Date, Time)> {
        None
    }
    /// 接着的显示器，主显示器在最前面；问不到时是空的
    fn displays(&self) -> Vec<DisplayInfo> {
        Vec::new()
    }
}

static BACKEND: OnceLock<Box<dyn SystemBackend>> = OnceLock::new();

thread_local! {
    /// 这个线程换上的实现（测试工具的替身）；`None` 时用整个进程装上的
    static THREAD_BACKEND: RefCell<Option<Rc<dyn SystemBackend>>> = const { RefCell::new(None) };
}

/// 装上系统那一侧的实现（运行时在第一次用到之前调用）。整个进程一份，先装的算数，之后再装不起作用
pub fn set_system_backend(backend: impl SystemBackend + 'static) {
    let _ = BACKEND.set(Box::new(backend));
}

/// 在这个线程上换掉系统那一侧的实现（测试工具换成进程内的替身：测试复制粘贴时不碰用户的剪贴板，
/// 并行的测试各在各的线程上互不干扰）；`None` 换回整个进程装上的
pub fn set_thread_system_backend(backend: Option<Rc<dyn SystemBackend>>) {
    THREAD_BACKEND.with(|b| *b.borrow_mut() = backend);
}

/// 用这个线程该用的实现；都没有时是 `None`
fn with_backend<R>(f: impl FnOnce(&dyn SystemBackend) -> R) -> Option<R> {
    let local = THREAD_BACKEND.with(|b| b.borrow().clone());
    match local {
        Some(backend) => Some(f(&*backend)),
        None => BACKEND.get().map(|backend| f(&**backend)),
    }
}

thread_local! {
    static THEME: Cell<Option<Signal<Appearance>>> = const { Cell::new(None) };
    static REDUCE_MOTION: Cell<Option<Signal<bool>>> = const { Cell::new(None) };
    static ACCENT: Cell<Option<Signal<Option<Color>>>> = const { Cell::new(None) };
}

/// 这个线程的设置信号：第一次用到时建好，初值问 `detect`（读可能在某个作用域里：建在所有作用域之外，不随它释放）
fn setting<T: Clone + 'static>(cell: &'static std::thread::LocalKey<Cell<Option<Signal<T>>>>, detect: impl FnOnce() -> T) -> Signal<T> {
    cell.with(|cell| {
        cell.get().unwrap_or_else(|| {
            let s = reactive::detached(|| signal(detect()));
            cell.set(Some(s));
            s
        })
    })
}

/// 改一个设置：还没建好时直接用这个值建（不再问系统）
fn report<T: Clone + PartialEq + 'static>(cell: &'static std::thread::LocalKey<Cell<Option<Signal<T>>>>, value: T) {
    match cell.with(Cell::get) {
        Some(s) => {
            s.set_if_changed(value);
        }
        None => {
            let s = reactive::detached(|| signal(value));
            cell.with(|cell| cell.set(Some(s)));
        }
    }
}

fn theme_signal() -> Signal<Appearance> {
    setting(&THEME, || with_backend(|b| b.appearance()).unwrap_or_default())
}

fn reduce_signal() -> Signal<bool> {
    setting(&REDUCE_MOTION, || with_backend(|b| b.is_motion_reduced()).unwrap_or(false))
}

fn accent_signal() -> Signal<Option<Color>> {
    setting(&ACCENT, || with_backend(|b| b.accent_color()).flatten())
}

/// 系统外观（深色 / 浅色），响应式：在绑定里读，用户在系统设置里切换时界面跟着变。
/// 全应用一个，不分窗口。测试工具里固定为浅色，用 `Harness::set_system_appearance` 模拟切换。
///
/// macOS、Windows 由窗口报告变化（Windows 另外监视注册表，只有托盘、没有窗口时也收得到）；
/// Linux 读 XDG 门户的 `color-scheme`，后台线程听它的变化（GNOME、KDE 都支持）
pub fn system_appearance() -> Appearance {
    theme_signal().get()
}

/// 系统打开了"减少动态效果"（辅助功能设置），响应式。应用自己的动画按它决定要不要用移动和缩放；
/// 框架里本来就动的东西（拖动排序的行让位、共享元素的飞行）在它打开时立即到位或只淡入淡出。
///
/// macOS 读 `NSWorkspace` 的辅助功能设置，Windows 读"动画效果"的系统参数，Linux 读 XDG 门户的
/// `reduced-motion`（以及 GNOME 关掉动画、KDE 把动画速度调到 0）。
/// 启动时读一次，之后窗口重新获得焦点、系统外观变化时再读（在系统设置里改了、回到应用就生效；Linux 上改了马上生效）。
/// 测试工具里固定为 `false`，用 `Harness::set_motion_reduced` 模拟
pub fn is_motion_reduced() -> bool {
    reduce_signal().get()
}

/// 系统的强调色（macOS：系统设置 → 外观；Windows：个性化 → 颜色；Linux：门户的 `accent-color`，GNOME 47+、KDE），
/// 响应式；读不到时是 `None`。和"减少动态效果"一样在启动时、窗口获得焦点时、系统外观变化时重新读。
///
/// 框架自带的样子不用它（有自己固定的强调色）；想跟着系统时在样式对象、绑定里读它：
/// `move || aureole::system_accent_color().unwrap_or(默认色)`。测试工具里固定为 `None`
pub fn system_accent_color() -> Option<Color> {
    accent_signal().get()
}

/// 宿主报告系统外观（运行时：窗口报告的外观、后台监视到的变化；测试工具：固定的浅色、模拟切换）
pub fn set_system_appearance(appearance: Appearance) {
    report(&THEME, appearance);
}

/// 宿主报告"减少动态效果"
pub fn set_motion_reduced(reduced: bool) {
    report(&REDUCE_MOTION, reduced);
}

/// 宿主报告系统的强调色
pub fn set_system_accent_color(color: Option<Color>) {
    report(&ACCENT, color);
}

/// 平台的惯例：这个线程装上的实现给的；都没有时是编译目标的系统的
pub fn conventions() -> Conventions {
    with_backend(|b| b.conventions()).unwrap_or_else(Conventions::current)
}

/// 弹出文件对话框（[`crate::FileDialog`] 用）：没装系统那一侧的实现时马上以空列表完成
pub(crate) fn show_file_dialog(request: FileDialogRequest, done: FileDialogDone) {
    let mut done = Some(done);
    with_backend(|b| {
        if let Some(done) = done.take() {
            b.file_dialog(request, done);
        }
    });
    if let Some(done) = done {
        done(Vec::new());
    }
}

/// 现在的本地日期和时间。系统那一侧问不到时（只用核心、没有宿主）按世界协调时算。
/// 测试工具里是固定的（2026-01-15 09:30），用 `Harness::set_local_time` 换。不是响应式的：过了午夜不会通知
pub fn local_date_time() -> (Date, Time) {
    with_backend(|b| b.local_time()).flatten().unwrap_or_else(|| {
        let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        (Date::from_days_from_epoch(seconds.div_euclid(86_400)), Time::from_seconds_from_midnight(seconds))
    })
}

/// 今天（本地日期），见 [`local_date_time`]
pub fn today() -> Date {
    local_date_time().0
}

/// 接着的显示器，主显示器在最前面。每次调用时问系统（不是响应式的）；还没有打开过窗口时是空的。
/// 找窗口在哪个显示器上：`displays().into_iter().find(|d| d.contains(window.position()))`。
/// 测试工具里是一个 1440 × 900 的主显示器，`Harness::set_displays` 换
pub fn displays() -> Vec<DisplayInfo> {
    with_backend(|b| b.displays()).unwrap_or_default()
}

/// 用系统默认的应用打开网址，例如 `https://…`、`mailto:…`。返回是否交给了系统
pub fn open_url(url: &str) -> bool {
    with_backend(|b| b.open_url(url)).unwrap_or(false)
}

/// 剪贴板（纯文字）。输入框、可选中文字的复制粘贴走同一个系统剪贴板
pub mod clipboard {
    use super::with_backend;

    /// 剪贴板里的文字；没有文字或读不了时是 `None`
    pub fn read_text() -> Option<String> {
        with_backend(|b| b.read_clipboard()).flatten()
    }

    /// 把文字放进剪贴板，返回是否成功
    pub fn write_text(text: impl Into<String>) -> bool {
        let text = text.into();
        with_backend(|b| b.write_clipboard(text)).unwrap_or(false)
    }
}
