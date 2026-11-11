//! Windows 上把清单（`src/bin/aureole-webview.manifest`）嵌进子进程程序和例子：CEF 的 GPU 进程用 DirectComposition
//! 画窗口时，程序没声明支持 Windows 10 和按显示器感知 DPI 的话会出错退出。用到网页视图的应用自己的程序也要嵌这份清单
//! （见 crate 的说明）。

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/bin/aureole-webview.manifest");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if !(windows && msvc) {
        return;
    }
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("cargo 给的包目录")).join("src/bin/aureole-webview.manifest");
    for kind in ["bins", "examples"] {
        println!("cargo::rustc-link-arg-{kind}=/MANIFEST:EMBED");
        println!("cargo::rustc-link-arg-{kind}=/MANIFESTINPUT:{}", manifest.display());
    }
}
