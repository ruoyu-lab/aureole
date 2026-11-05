//! 默认的安装程序：`cargo bundle --format installer` 没指定自己的安装程序时用它。
//! 单独运行时后面没有接着安装包，说明一下就退出

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use aureole_installer::{Setup, Wizard};

fn main() -> std::process::ExitCode {
    Setup::new().run(|session| Wizard::new().build(session))
}
