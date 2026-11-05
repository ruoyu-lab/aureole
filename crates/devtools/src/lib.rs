//! Aureole 的开发工具：热加载。[`watch_file`] 把文件内容变成信号，文件保存后界面跟着变（配色、文案、调参数）；
//! [`set_image_watching`] 让图片文件改了以后自动重新解码。
//!
//! 监视在后台线程上做，读好的文件经核心的主线程投递（`MainThreadSender`）交回主线程，和任务共用一个收件箱；
//! 图片的绑定按文件的修订号换缓存的键（核心的注入点 `internal::set_image_revision_source`）。
//!
//! 重新编译后接着跑（`App::restarts_on_rebuild`、`persist`）要换掉整个进程、记住主窗口，在运行时里，
//! 运行时用这里的 [`watch_metadata`] 监视可执行文件；门面 `aureole::devtools` 把两边合在一起。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod watch;

pub use watch::{watch_file, watch_file_with, set_image_watching};
#[doc(hidden)]
pub use watch::{FileWatch, watch_metadata};
