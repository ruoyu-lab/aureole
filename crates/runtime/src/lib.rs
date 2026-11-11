//! Aureole 的运行时：把核心（`aureole-core`）接到真实的窗口和 GPU 上。
//!
//! - [`App`] 和事件循环：每个窗口一个元素树、一个渲染线程，所有窗口共享主线程和响应式运行时；
//!   每个窗口一个跟着显示器的帧时钟（平台层给的），画面静止时暂停，事件循环不再被帧唤醒
//! - [`Window`]、[`WindowHandle`]：打开、管理、查询窗口；窗口事件换算成核心的输入事件（`translate`），
//!   交给唯一的输入入口和帧管线（核心里的窗口宿主）
//! - 渲染线程：收下每帧的显示列表和层变化，推进合成层动画和滚动，交给渲染器（`aureole-render`）画到表面上
//! - 桌面集成：菜单栏和托盘（`menus` 特性的 `Menu`、`Tray`）、文件对话框（[`FileDialog`]）、打开网址和文件、单实例、
//!   系统设置、剪贴板、通知，把平台层（`aureole-platform`）装进核心的注入点
//! - 插件（[`Plugin`]）：在 [`App`] 上挂钩子（窗口创建后、平台事件过滤、每帧开始和结束、退出前）、换执行器；
//!   开发工具的热加载（`devtools` 特性的 `restart::HotReloadPlugin`）、`tokio` 特性的 `TokioPlugin` 都是插件
//! - 开发时的原地重启（`devtools` 特性的 `restart::persist`）、录制输入、测试用的打开请求（[`testing`]）
//!
//! 这是唯一同时认识 winit、平台层、渲染器和核心的 crate。应用不直接依赖它，用门面 `aureole`。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod app;
pub mod clipboard;
mod debug_log;
mod display_color;
mod external_drag;
#[cfg(feature = "menus")]
mod menu;
mod notification;
mod open;
mod pacing;
pub mod performance;
mod plugin;
mod print;
mod render_thread;
#[cfg(feature = "devtools")]
pub mod restart;
mod system;
mod title_bar;
pub mod testing;
mod translate;
mod window;

pub use app::App;
pub use aureole_core::FileDialog;
pub use display_color::{ColorOutput, DisplayColor, display_color};
pub use external_drag::accept_drop_format;
#[cfg(feature = "menus")]
pub use menu::{Menu, MenuItem, Submenu, Tray};
pub use notification::Notification;
pub use open::{OpenRequest, OpenSource, SingleInstance};
#[cfg(feature = "tokio")]
pub use plugin::TokioPlugin;
pub use plugin::{FrameInfo, PlatformEvent, PlatformEventKind, Plugin, at_exit};
pub use print::{PageSetup, PrintJob, PrintOutcome, shared_gpu};
pub use render_thread::{GroupCacheStats, group_cache_stats, set_group_cache_budget};
pub use render::{GpuError, GpuPreference};
pub use platform::{Capabilities, FrameClockKind, PlatformError, TitleBar, WindowMaterial};
pub use system::{appearance, beep, capabilities, displays, notify, open_path, open_url, is_low_power_mode, is_motion_reduced, reveal_path, system_accent_color, system_appearance};
pub use window::{CloseRequest, Window, WindowHandle, current_window, quit};
