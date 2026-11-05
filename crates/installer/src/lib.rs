//! 用 Aureole 写安装程序。
//!
//! 大多数应用不用写代码：`cargo bundle -p 应用 --release --format installer` 用默认的向导打出安装程序，设置写在
//! `[package.metadata.bundle.installer]`。想要自己的样子时写一个很小的程序：
//!
//! ```no_run
//! use aureole::prelude::*;
//! use aureole_installer::{Setup, Wizard};
//!
//! fn main() -> std::process::ExitCode {
//!     // 换颜色、图片、文字
//!     Setup::new().run(|session| Wizard::new().accent(Color::hex(0x138a84)).build(session))
//! }
//! ```
//!
//! 分三层，每层都能单独用：
//!
//! - **界面**：[`Wizard`] 是默认的向导。它只用 [`Session`] 的公开接口，所以界面完全自己画时能做到它做到的一切。
//! - **引擎**：[`Installer`]、[`Uninstaller`] 解压、校验、提交、回滚、升级、卸载，不依赖界面，也能不开窗口运行（`--silent`）。
//! - **包格式**：[`Builder`]、[`Package`]，打包工具用它把文件接在安装程序后面。
//!
//! 常见的事都做好了：装给当前用户（不要管理员权限）、快捷方式、"应用和功能"里的卸载项、协议和文档类型。
//! 少见的（注册服务、装驱动、改防火墙、先装依赖……）写一个 [`Step`] 加进去，失败时跟着回滚，卸载时也会调用。

mod engine;
mod manifest;
mod package;
mod system;

#[cfg(feature = "ui")]
mod session;
#[cfg(feature = "ui")]
mod setup;
#[cfg(feature = "ui")]
mod wizard;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use engine::{Activity, InstallOptions, Installer, Progress, Receipt, Step, StepContext, Uninstaller, RECEIPT_FILE, UNINSTALLER_NAME};
pub use manifest::{DocumentType, FileEntry, Manifest};
pub use package::{Builder, Package};
pub use system::default_directory;

#[cfg(feature = "ui")]
pub use session::{Mode, Session, State};
#[cfg(feature = "ui")]
pub use setup::Setup;
#[cfg(feature = "ui")]
pub use wizard::{Texts, Wizard};

/// 取消：界面上按"取消"时置位，引擎在两个文件之间、每解压一块时看它
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// 请求取消
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// 请求过取消了
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 安装、卸载出错
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 读写文件失败
    Io {
        /// 哪个文件
        path: PathBuf,
        /// 系统说的原因
        source: std::io::Error,
    },
    /// 文件正被使用（应用还开着）：关掉以后可以重试
    FileInUse(PathBuf),
    /// 安装包里的数据坏了（下载不完整、被改过）
    Corrupt(String),
    /// 用户取消了。已经做的都撤销了
    Cancelled,
    /// 这个程序后面没有接着安装包，或者安装包的格式不对
    NotAPackage(String),
    /// [`Step`] 失败
    Step {
        /// 哪一步
        step: String,
        /// 它说的原因
        message: String,
    },
    /// 系统集成失败（快捷方式、注册表）
    System(String),
    /// 找不到安装记录：这个目录不是这个安装程序装的
    NotInstalled(PathBuf),
}

impl Error {
    /// 读写文件失败。系统说"正被使用"时换成 [`Error::FileInUse`]，这个可以让用户关掉应用再试
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        if is_in_use(&source) { Self::FileInUse(path.to_path_buf()) } else { Self::Io { path: path.to_path_buf(), source } }
    }

    /// 用户取消的，不算失败
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// 关掉应用以后重试可能就好了
    pub fn is_file_in_use(&self) -> bool {
        matches!(self, Self::FileInUse(_))
    }
}

/// 文件正被使用：Windows 的共享冲突（32），Linux 的正在运行的程序（`ETXTBSY`，26）
fn is_in_use(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(32) => cfg!(windows),
        Some(26) => cfg!(unix),
        _ => false,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "读写 {} 失败：{source}", path.display()),
            Self::FileInUse(path) => write!(f, "{} 正被使用，关掉应用后再试", path.display()),
            Self::Corrupt(path) => write!(f, "安装包里的 {path} 坏了，重新下载试试"),
            Self::Cancelled => f.write_str("已取消"),
            Self::NotAPackage(message) => f.write_str(message),
            Self::Step { step, message } => write!(f, "{step}失败：{message}"),
            Self::System(message) => f.write_str(message),
            Self::NotInstalled(path) => write!(f, "{} 里没有安装记录", path.display()),
        }
    }
}

impl std::error::Error for Error {}
