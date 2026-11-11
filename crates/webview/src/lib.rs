//! 网页视图（试验性）：内嵌 CEF 内核，网页离屏画进一张 GPU 纹理，框架把它当成普通元素合成。
//! 所以圆角裁剪、透明度、变换、动画对它都有效，框架的菜单、弹窗能盖在网页上面。
//!
//! ```no_run
//! use aureole_webview::{WebController, web_view};
//!
//! let page = WebController::new("https://example.com");
//! let view = web_view(&page).size(800, 600);
//! page.load("https://example.org");
//! ```
//!
//! 不用的时候不加载：第一个网页视图挂载时才加载内核、启动 CEF（几百毫秒），没用到网页视图的运行
//! 不占内存、不多开进程。CEF 在一个进程里只能启动一次，启动以后 GPU、网络进程留到应用退出。
//!
//! CEF 的界面线程：macOS、Linux 上就是框架的主线程（外部消息泵）；Windows 上是 CEF 自己开的线程，
//! 回调在那个线程上，复制每一帧也在那里做，要写信号的投递回主线程（主线程不替 CEF 跑消息泵，帧不被挤掉）。
//!
//! Windows 上 CEF 的程序要带清单（声明支持 Windows 10/11、按显示器感知 DPI）：子进程程序在编译时嵌进去
//! （`build.rs`，`src/bin/aureole-webview.manifest`）；用到网页视图的应用自己的程序最好也嵌这一份（`cargo bundle` 打包时嵌）。
//!
//! 页面要应用出面的事（对话框、选文件、下载、权限申请、要去哪个地址、开新窗口、右键菜单、认证、证书错误）都是控制器上的钩子，
//! 不接时有默认的行为；页面的状态（标题、地址、图标、进度、出错、崩溃、历史记录……）是信号；页面用哪一套数据
//! （Cookie、缓存、语言、代理、请求拦截、用户脚本）由配置（[`WebProfile`]）定；
//! 页面和应用用 `window.aureole` 互发消息，应用自带的页面走 `app://`（[`serve_app`]）。都是原语：查找框、下载列表、
//! 全屏的样子由应用自己画。
//!
//! 打包：应用直接依赖这个 crate 时，`cargo bundle` 编子进程程序、带上 CEF：Windows 上运行文件（内核、资源、语言包）
//! 装在应用旁边；macOS 上内核框架和 5 个辅助程序的包装进 `.app` 的 `Contents/Frameworks`。Linux 上还不能打包。
//!
//! 沙箱：macOS 上默认开着（辅助程序启动时、加载内核之前进沙箱，应用不用改结构；`AUREOLE_WEBVIEW_NO_SANDBOX=1` 关掉）。
//! Windows 上默认子进程不在沙箱里，开沙箱时应用编成动态库（`[lib] crate-type = ["cdylib", "rlib"]`，库名和程序名不一样），
//! 主逻辑放进库里的一个函数，库里写 `aureole_webview::bootstrap_entry!(那个函数);`，`main.rs` 调同一个函数（开发时照常跑，不开沙箱）；
//! 由 CEF 带的 `bootstrap.exe` 改名成应用名来加载，主进程和子进程都是这一个程序（[`bootstrap_entry!`]、[`is_sandboxed`]）。
//! `cargo bundle` 在 `[package.metadata.bundle.webview] sandbox = true` 时这样拼。Linux 的沙箱还没做。
//!
//! 模块的分工：
//!
//! | 模块 | 做什么 |
//! |---|---|
//! | `engine` | 按需启动 CEF、界面线程（消息泵或者 CEF 自己的线程）、子进程、Windows 上开沙箱的入口、macOS 的应用类、对齐显卡、退出时关掉 |
//! | `state` | 一个网页的共享状态：主线程上的一半（信号），和回调拿着、能跨线程的一半（大小、离屏绘制） |
//! | `client` | 一个网页的各种回调：加载状态、标题、地址、弹出窗口、执行脚本、导航过滤 |
//! | `render` | 离屏绘制：CEF 画好的纹理复制进框架登记的外部纹理，弹出层（下拉框）盖在上面，通知元素重画；出帧的节奏 |
//! | `input` | 指针、滚轮、按键、输入法、光标、焦点转给 CEF；页面里的拖放、从外面拖进来 |
//! | `kind` | 元素种类：量大小、画外部纹理、大小变了告诉 CEF、挂上时网页回到前台、卸载时转到后台 |
//! | `lifecycle` | 网页的生命周期：前台、后台、冻结、丢弃、关掉 |
//! | `profile` | 配置（一套数据）：数据目录、隐身、语言、代理、User-Agent、用户脚本、清除数据 |
//! | `cookies` | 一套配置的 Cookie：读、写、删 |
//! | `intercept` | 请求发出之前过一遍：拦掉、改地址、改请求头 |
//! | `reply` | 等内核答复的 future |
//! | `scripts` | 用户脚本：经 DevTools 协议注册，跟着页面走 |
//! | `dialogs` | 页面要应用出面的事：脚本的对话框、选文件、下载、权限申请（钩子和默认行为） |
//! | `bridge` | 页面和应用互发消息（`window.aureole`）；子进程程序的应用对象 |
//! | `scheme` | 应用自带的页面：`app://` 协议 |
//! | `accessibility` | 读屏：内核的无障碍树转成框架的虚拟子节点，辅助技术的点击转成页面里的点击 |
//! | `tools` | 网页全屏、缩放、页内查找、开发者工具、打印 |
//! | `popup` | 页面要开的新窗口：内核建好交给应用（`window.opener` 照样能用） |
//! | `menu` | 右键菜单（点到了什么交给应用）、编辑命令 |
//! | `status` | 网站图标、悬停的链接、加载进度和出错、提示文字、崩溃、历史记录、静音、控制台消息 |

mod accessibility;
mod bridge;
mod client;
mod cookies;
mod dialogs;
mod engine;
mod input;
mod intercept;
mod kind;
mod lifecycle;
mod menu;
mod popup;
mod profile;
mod render;
mod reply;
mod scheme;
mod scripts;
mod state;
mod status;
mod tools;

pub use bridge::WebMessage;
pub use dialogs::{
    AuthRequest, CertificateError, Dialog, DialogKind, Download, DownloadRequest, DownloadState, FileChooser, FileChooserMode, Permission, PermissionRequest,
};
pub use menu::{ContextMenu, EditFlags, MediaKind};
pub use popup::{NewWindow, NewWindowKind};
#[cfg(target_os = "windows")]
pub use engine::run_bootstrapped;
pub use engine::{EngineError, is_sandboxed, run_helper};
pub use cookies::Cookie;
pub use intercept::{InterceptedRequest, ResourceKind};
pub use lifecycle::Lifecycle;
pub use profile::{Proxy, ScriptTime, UserScript, WebProfile};
pub use reply::Reply;
pub use scheme::{APP_SCHEME, AppRequest, AppResponse, mime_type_of, serve_app, serve_app_directory};
pub use state::{Navigation, WebController};
pub use status::{ConsoleLevel, ConsoleMessage, Crash, CrashReason, History, HistoryEntry, LoadError};
pub use tools::{FindResult, PdfDone};

use aureole::Element;

/// Windows 上开沙箱时应用的入口：在应用编成的动态库（`[lib] crate-type = ["cdylib"]`）里写一行
/// `aureole_webview::bootstrap_entry!(app_main);`，导出改了名的 `bootstrap.exe` 要找的 `RunWinMain`（窗口程序）、
/// `RunConsoleMain`（控制台程序），里面经 `run_bootstrapped`（只在 Windows 上有）调 `app_main`（应用本来的入口：`App::new(..).run()` 的那个函数）。
/// 别的系统上什么都不生成。打包工具按 `[package.metadata.bundle.webview] sandbox = true` 拼出程序（见打包的说明）
#[macro_export]
macro_rules! bootstrap_entry {
    ($main:path) => {
        /// 改了名的 `bootstrap.exe` 加载动态库以后调的入口（窗口程序）
        ///
        /// # Safety
        ///
        /// 只由 `bootstrap.exe` 调，参数是它给的
        #[cfg(target_os = "windows")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn RunWinMain(
            _instance: *mut ::std::ffi::c_void,
            _command_line: *const u16,
            _show: i32,
            sandbox_info: *mut ::std::ffi::c_void,
            _version_info: *mut ::std::ffi::c_void,
        ) -> i32 {
            $crate::run_bootstrapped(sandbox_info, $main)
        }

        /// 改了名的 `bootstrapc.exe` 加载动态库以后调的入口（控制台程序）
        ///
        /// # Safety
        ///
        /// 只由 `bootstrapc.exe` 调，参数是它给的
        #[cfg(target_os = "windows")]
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn RunConsoleMain(
            _argc: i32,
            _argv: *mut *mut ::std::ffi::c_char,
            sandbox_info: *mut ::std::ffi::c_void,
            _version_info: *mut ::std::ffi::c_void,
        ) -> i32 {
            $crate::run_bootstrapped(sandbox_info, $main)
        }
    };
}

/// 显示 `controller` 管着的网页的元素。同一个控制器同时只挂在一个元素上；元素卸载时网页转到后台（不关），
/// 再挂上时接着显示（多标签：每个标签一个控制器，只挂当前的那个）。控制器和元素都放掉时网页关掉
pub fn web_view(controller: &WebController) -> Element {
    kind::element(controller)
}
