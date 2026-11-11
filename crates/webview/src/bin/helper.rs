//! CEF 的子进程（渲染、GPU、网络……）跑的程序：只调 CEF 的子进程入口，不碰界面。
//! 打包时放在应用旁边（macOS 上做成应用包里的几个辅助程序）

fn main() {
    std::process::exit(aureole_webview::run_helper());
}
