//! 把工作区里的一个应用打包：macOS 的 .app（编译、Info.plist、图标、临时签名，可选做成 .dmg，可选通用二进制），
//! Windows 的安装程序、Linux 的 .deb 和 AppImage（交给 cargo-packager，见 `installers.rs`）。
//!
//! ```text
//! cargo bundle --release
//! cargo bundle --release --name 待办 --id com.example.todo --version 1.2.0 --dmg
//! cargo bundle --release --universal                # macOS：arm64 和 x86_64 合成一个
//! cargo bundle --release --format deb,appimage      # 在 Linux 上（默认就是这两种）
//! cargo bundle --release --format nsis              # 在 Windows 上（默认就是这种）
//! ```
//!
//! 设置写在应用的 Cargo.toml 里，命令行参数优先：
//!
//! ```toml
//! [package.metadata.bundle]
//! name = "待办"                       # 应用名，访达、程序坞、菜单栏里显示
//! identifier = "com.example.todo"     # 包标识符，系统通知等功能需要
//! category = "productivity"           # 应用分类
//! icon = { path = "M4 12 L9 17 L20 6", view-box = [0, 0, 24, 24], stroke = 2.2, background = ["#5e5ce6", "#3634a3"] }
//! url-schemes = ["todo"]               # 能打开 todo://…
//! document-types = [{ name = "待办清单", extensions = ["todolist"], role = "editor" }]
//! ```
//!
//! 自己写安装程序时要写进系统的东西（注册表、`.desktop`、MIME 声明）用 `--emit` 生成。
//!
//! 应用直接依赖 CEF 视图（`aureole-webview`）时，Windows 的安装包里多带它的子进程程序和 CEF 的运行文件（见 `webview.rs`）。

mod associations;
mod exe_icon;
mod installers;
mod options;
mod icon;
mod plist;
mod self_installer;
mod stub;
mod webview;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::Value;

use crate::installers::Format;
use crate::options::{Overrides, BundleOptions};

/// `--universal` 的编译目标；输出目录按这个名字
const UNIVERSAL_TARGETS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];
const UNIVERSAL: &str = "universal-apple-darwin";

const HELP: &str = "\
把应用打包：macOS 的 .app 和 .dmg，Windows 的安装程序，Linux 的 .deb 和 AppImage

用法：cargo bundle -p <包> [选项]

  -p, --package <包>       要打包的包（工作区里只有一个能打包的包时可以省略）
      --bin <名字>         包里有多个可执行文件时选一个
  -r, --release            用 release 配置编译（发给别人用的应用都应该加）
      --target <三元组>    编译到别的架构，例如 x86_64-apple-darwin
      --universal          macOS：同时编 arm64 和 x86_64，合成一个通用二进制
      --format <种类>      要打的包，逗号分开：app、deb、appimage、nsis、installer。默认按系统：macOS 是 app，
                           Linux 是 deb 和 appimage，Windows 是 nsis。除 app 以外要在目标系统上打；
                           deb、appimage、nsis 要先装 cargo-packager，installer 不用（用 Aureole 画的安装程序）
      --keep-symbols       --release 时默认去掉包里可执行文件的符号和调试信息（target 里的不动），加了就不去
      --features <特性>    传给 cargo build
      --name <名字>        应用名：访达、程序坞、菜单栏里显示，可以是中文
      --id <包标识符>      例如 com.example.todo
      --version <版本>     默认用 Cargo.toml 里的版本
      --icon <文件>        图标：方形的 PNG（建议 1024x1024）或现成的 .icns
      --out <目录>         输出目录，默认 <target>/<配置>/bundle
      --dmg                再做一个 .dmg 磁盘映像（拖进\"应用程序\"安装）
      --sign <身份>        代码签名的身份，默认 \"-\"（临时签名，只在本机有效）
      --no-sign            不签名
      --icon-preview <png> 只把图标画成一张 1024x1024 的 PNG，不编译、不打包
      --emit <种类>        不编译、不打包，只生成安装程序要用的文件，放进输出目录：
                           windows-reg（注册表：协议、文档类型）、linux-desktop（.desktop 文件）、
                           linux-mime（自定义文档类型的 MIME 声明）
      --exe <路径>         --emit 时命令行里的可执行文件（默认 Windows 是 C:\\Program Files\\<应用名>\\<可执行文件>.exe，
                           Linux 是 <可执行文件>）
  -h, --help               显示这段说明

设置也可以写在包的 Cargo.toml 的 [package.metadata.bundle] 里，见 README。";

#[derive(Default)]
struct Args {
    package: Option<String>,
    release: bool,
    target: Option<String>,
    features: Option<String>,
    out: Option<PathBuf>,
    dmg: bool,
    /// `--universal`
    universal: bool,
    /// `--format`；`None` 时按系统选
    formats: Option<Vec<Format>>,
    /// `--keep-symbols`
    keep_symbols: bool,
    /// `None` 表示不签名
    sign: Option<String>,
    icon_preview: Option<PathBuf>,
    emit: Option<Emit>,
    exe: Option<String>,
    overrides: Overrides,
}

/// `--emit` 生成的文件
#[derive(Clone, Copy, Debug, PartialEq)]
enum Emit {
    WindowsRegistry,
    LinuxDesktop,
    LinuxMime,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args { sign: Some("-".into()), ..Args::default() };
    while let Some(arg) = args.next() {
        // 同时支持 `--name 值` 和 `--name=值`
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| args.next()).ok_or_else(|| format!("{flag} 后面需要一个值"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "-p" | "--package" => parsed.package = Some(value()?),
            "--bin" => parsed.overrides.bin = Some(value()?),
            "-r" | "--release" => parsed.release = true,
            "--target" => parsed.target = Some(value()?),
            "--features" => parsed.features = Some(value()?),
            "--name" => parsed.overrides.name = Some(value()?),
            "--id" | "--identifier" => parsed.overrides.identifier = Some(value()?),
            "--version" => parsed.overrides.version = Some(value()?),
            "--icon" => parsed.overrides.icon = Some(value()?.into()),
            "--out" => parsed.out = Some(value()?.into()),
            "--dmg" => parsed.dmg = true,
            "--universal" => parsed.universal = true,
            "--keep-symbols" => parsed.keep_symbols = true,
            "--format" => parsed.formats = Some(Format::parse_list(&value()?)?),
            "--sign" => parsed.sign = Some(value()?),
            "--no-sign" => parsed.sign = None,
            "--icon-preview" => parsed.icon_preview = Some(value()?.into()),
            "--emit" => {
                parsed.emit = Some(match value()?.as_str() {
                    "windows-reg" => Emit::WindowsRegistry,
                    "linux-desktop" => Emit::LinuxDesktop,
                    "linux-mime" => Emit::LinuxMime,
                    other => return Err(format!("--emit 不认识 {other}（windows-reg、linux-desktop、linux-mime）")),
                })
            }
            "--exe" => parsed.exe = Some(value()?),
            other => return Err(format!("不认识的参数 {other}（--help 看用法）")),
        }
    }
    Ok(Some(parsed))
}

fn main() -> ExitCode {
    // 作为 cargo 的别名运行时，第一个参数可能是子命令名 `bundle`
    let args = std::env::args().skip(1).skip_while(|a| a == "bundle");
    let args = match parse_args(args) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("错误：{error}");
            return ExitCode::FAILURE;
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn run(args: Args) -> Result<(), String> {
    let metadata = cargo_metadata()?;
    let package = pick_package(&metadata, args.package.as_deref())?;
    let (options, notes) = BundleOptions::resolve(package, args.overrides)?;
    for note in notes {
        eprintln!("提示：{note}");
    }
    if let Some(out) = &args.icon_preview {
        icon::write_preview(&options.icon, 1024, out)?;
        println!("图标预览已保存到 {}", out.display());
        return Ok(());
    }
    if args.universal && args.target.is_some() {
        return Err("--universal 已经定了要编哪两个架构，不用再写 --target".into());
    }
    let triple = if args.universal { Some(UNIVERSAL) } else { args.target.as_deref() };
    let out_dir = |metadata: &Value| -> Result<PathBuf, String> {
        if let Some(out) = &args.out {
            return Ok(out.clone());
        }
        let target_dir = PathBuf::from(metadata["target_directory"].as_str().ok_or("cargo metadata 没有 target_directory")?);
        let profile = if args.release { "release" } else { "debug" };
        Ok(match triple {
            Some(triple) => target_dir.join(triple).join(profile).join("bundle"),
            None => target_dir.join(profile).join("bundle"),
        })
    };
    if let Some(kind) = args.emit {
        let path = emit(kind, &options, args.exe.as_deref(), &out_dir(&metadata)?)?;
        println!("已生成：{}", path.display());
        return Ok(());
    }
    let os = std::env::consts::OS;
    let formats = args.formats.clone().unwrap_or_else(|| if args.dmg { vec![Format::App] } else { Format::default_for(os) });
    Format::check(&formats, os)?;
    if args.universal && os != "macos" {
        return Err("--universal 只能在 macOS 上用".into());
    }
    if options.webview.is_some() {
        webview::check(os, &formats)?;
        if args.universal {
            return Err("用到 CEF 视图的应用打不了通用二进制：CEF 的内核框架分 Apple 芯片和 Intel 两份，各打各的（--target）".into());
        }
    }
    // 开沙箱：应用要有编成动态库的库（导出 bootstrap 找的入口）
    let sandbox = options.webview.as_ref().is_some_and(|w| w.sandbox);
    let library = match sandbox {
        true => Some(webview::library_target(package).ok_or(
            "开沙箱要把应用编成动态库：Cargo.toml 里加 [lib] crate-type = [\"cdylib\", \"rlib\"]（库名和程序名不要一样），\
             库里用 aureole_webview::bootstrap_entry!(应用的入口函数) 导出入口",
        )?),
        false => None,
    };

    let package_name = package["name"].as_str().unwrap_or_default();
    let out_dir = out_dir(&metadata)?;
    let strip = args.release && !args.keep_symbols;
    // 开沙箱时编动态库，程序是改了名的 bootstrap（CEF 的运行文件、bootstrap 在动态库旁边）
    let (exe, runtime_dir, library) = if args.universal {
        let exe = build_universal(package_name, &options.bin, args.release, args.features.as_deref(), &out_dir)?;
        (exe, None, None)
    } else if let Some(library) = &library {
        let dll = build_library(package_name, library, args.release, args.target.as_deref(), args.features.as_deref())?;
        let exe = bootstrap_program(&dll, &options, &out_dir)?;
        let dir = dll.parent().map(Path::to_path_buf);
        (exe, dir, Some(dll))
    } else {
        let exe = build(package_name, &options.bin, args.release, args.target.as_deref(), args.features.as_deref())?;
        let dir = exe.parent().map(Path::to_path_buf);
        (exe, dir, None)
    };
    let exe = packaged_exe(&exe, &options, os, strip, &out_dir)?;
    if os == "windows" {
        exe_icon::embed_version(&exe, &version_info(&options, &exe))?;
    }
    let webview_files = webview_runtime(&options, runtime_dir.as_deref(), library.as_deref(), &exe, os, args.release, args.target.as_deref(), strip, &out_dir)?;
    if formats.contains(&Format::App) {
        if os != "macos" {
            eprintln!("提示：在 {os} 上只能生成 .app 的目录结构；签名和 .dmg 需要 macOS 的 codesign、hdiutil");
        }
        let app = assemble(&options, &exe, &out_dir)?;
        if let Some(runtime) = &webview_files
            && let (Some(framework), Some(helper)) = (&runtime.framework, &runtime.helper)
        {
            let bytes = webview::install_macos(&app, &options.bin, &options.identifier, helper, framework)?;
            println!("CEF 视图：内核框架和 5 个辅助程序的包装进了 Contents/Frameworks（{}）", human_size(bytes));
        }
        if let Some(identity) = &args.sign
            && cfg!(target_os = "macos")
        {
            sign(&app, identity, options.entitlements.as_deref())?;
        }
        println!("已打包：{}（{}）", app.display(), human_size(dir_size(&app)));
        if args.dmg && cfg!(target_os = "macos") {
            let dmg = make_dmg(&app, &options, &out_dir)?;
            println!("磁盘映像：{}（{}）", dmg.display(), human_size(std::fs::metadata(&dmg).map(|m| m.len()).unwrap_or(0)));
        }
        println!("打开：open {:?}", app.display().to_string());
    }
    installers::package(&options, &exe, webview_files.as_ref(), &formats, args.target.as_deref(), &out_dir)?;
    if formats.contains(&Format::Installer) {
        let (stub_package, stub_bin) = options.installer.stub();
        let features = args.features.as_deref().filter(|_| stub_package == package_name);
        let stub = if options.installer.package.is_some() {
            build(stub_package, stub_bin, args.release, args.target.as_deref(), features)?
        } else {
            build_default_stub(&metadata, &out_dir, args.release, args.target.as_deref())?
        };
        let stub = packaged_exe(&stub, &options, os, args.release && !args.keep_symbols, &out_dir.join("installer-stub"))?;
        exe_icon::embed_manifest(&stub, exe_icon::AS_INVOKER_MANIFEST)?;
        let setup = self_installer::package(&options, &exe, webview_files.as_ref(), &stub, args.target.as_deref(), os, &out_dir)?;
        println!("安装程序：{}（{}）", setup.display(), human_size(std::fs::metadata(&setup).map(|m| m.len()).unwrap_or(0)));
    }
    if !args.release {
        eprintln!("提示：这是 debug 版，发给别人用的应用请加 --release");
    }
    Ok(())
}

fn cargo_metadata() -> Result<Value, String> {
    metadata_with(&["--no-deps"])
}

/// 编默认的安装程序：依赖图里有就直接编，没有就生成一个小项目（见 [`stub`]）
fn build_default_stub(metadata: &Value, out_dir: &Path, release: bool, target: Option<&str>) -> Result<PathBuf, String> {
    let (package, bin) = self_installer::DEFAULT_STUB;
    match stub::source(&metadata_with(&[])?, env!("CARGO_PKG_VERSION")) {
        stub::Source::Direct => build(package, bin, release, target, None),
        stub::Source::Project(dependency) => {
            let lock = metadata["workspace_root"].as_str().map(|root| Path::new(root).join("Cargo.lock"));
            let manifest = stub::write_project(&out_dir.join("installer-stub").join("project"), &dependency, lock.as_deref())?;
            let mut command = build_command(package, bin, release, target, None);
            // 用应用的 target 目录：依赖相同的部分不用再编一遍
            command.arg("--manifest-path").arg(&manifest);
            if let Some(directory) = metadata["target_directory"].as_str() {
                command.args(["--target-dir", directory]);
            }
            run_build(command, bin)
        }
    }
}

fn metadata_with(extra: &[&str]) -> Result<Value, String> {
    let output = Command::new(cargo())
        .args(["metadata", "--format-version", "1"])
        .args(extra)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("运行 cargo metadata 失败：{e}"))?;
    if !output.status.success() {
        return Err("cargo metadata 失败".into());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("看不懂 cargo metadata 的输出：{e}"))
}

/// 按名字找包；没指定时，工作区里只有一个写了 `[package.metadata.bundle]` 的包就用它
fn pick_package<'a>(metadata: &'a Value, name: Option<&str>) -> Result<&'a Value, String> {
    let packages = metadata["packages"].as_array().ok_or("cargo metadata 里没有包")?;
    match name {
        Some(name) => packages.iter().find(|p| p["name"] == name).ok_or_else(|| format!("工作区里没有包 {name}")),
        None => {
            let candidates: Vec<&Value> = packages.iter().filter(|p| !p["metadata"]["bundle"].is_null()).collect();
            match (candidates.as_slice(), packages.as_slice()) {
                ([only], _) => Ok(only),
                // 工作区只有一个包：没写打包设置也是它（设置都有默认值）
                ([], [only]) => Ok(only),
                _ => {
                    let names: Vec<&str> = candidates.iter().filter_map(|p| p["name"].as_str()).collect();
                    Err(format!("用 -p 指定要打包的包（写了打包设置的有：{}）", if names.is_empty() { "没有".into() } else { names.join("、") }))
                }
            }
        }
    }
}

/// Windows 上把 C 运行库静态编进去：默认是动态的（`vcruntime140.dll`），没装过 VC++ 运行库的机器上程序打不开。
/// 写成 cargo 的 `--config`，别的目标不受影响。`RUSTFLAGS` 环境变量优先于它
const STATIC_CRT: &str = r#"target.'cfg(all(windows, target_env = "msvc"))'.rustflags = ["-C", "target-feature=+crt-static"]"#;

/// 编译的命令
fn build_command(package: &str, bin: &str, release: bool, target: Option<&str>, features: Option<&str>) -> Command {
    let mut command = Command::new(cargo());
    command.args(["build", "-p", package, "--bin", bin, "--message-format=json-render-diagnostics", "--config", STATIC_CRT]);
    if release {
        command.arg("--release");
    }
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    command
}

/// 编译，返回可执行文件的路径（从 cargo 的 JSON 消息里读，`--target`、自定义配置都对）
fn build(package: &str, bin: &str, release: bool, target: Option<&str>, features: Option<&str>) -> Result<PathBuf, String> {
    if cfg!(windows) && std::env::var_os("RUSTFLAGS").is_some() {
        eprintln!("提示：设了 RUSTFLAGS 环境变量，cargo 不再读 --config 里的 rustflags：要自己加上 -C target-feature=+crt-static，不然程序依赖 vcruntime140.dll");
    }
    run_build(build_command(package, bin, release, target, features), bin)
}

/// 运行编译命令，返回可执行文件的路径
fn run_build(mut command: Command, bin: &str) -> Result<PathBuf, String> {
    let output = command.stderr(Stdio::inherit()).output().map_err(|e| format!("运行 cargo build 失败：{e}"))?;
    if !output.status.success() {
        return Err("编译失败".into());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == bin)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .ok_or_else(|| format!("编译成功了，但没找到可执行文件 {bin}"))
}

/// 要放进包里的可执行文件：一份拷贝，`target` 里的不动（性能分析还要用）。
/// Windows 上把应用的图标（资源管理器、任务栏、快捷方式显示它）和包标识符（运行时声明成进程的应用标识）嵌进去。Linux、macOS 上 `--release` 时去掉符号和调试信息：
/// 发布配置留着行号，Linux 上这能占到八成多（125 MB → 19 MB）；Windows 的调试信息本来就在单独的 .pdb 里
fn packaged_exe(exe: &Path, options: &BundleOptions, os: &str, strip: bool, out_dir: &Path) -> Result<PathBuf, String> {
    let name = exe.file_name().ok_or_else(|| format!("可执行文件的路径 {} 不对", exe.display()))?;
    let directory = out_dir.join("stripped");
    std::fs::create_dir_all(&directory).map_err(|e| format!("创建 {} 失败：{e}", directory.display()))?;
    let copy = directory.join(name);
    std::fs::copy(exe, &copy).map_err(|e| format!("拷贝 {} 失败：{e}", exe.display()))?;
    if os == "windows" {
        exe_icon::embed(&copy, &options.icon)?;
        exe_icon::embed_identifier(&copy, &options.identifier)?;
        return Ok(copy);
    }
    if !strip {
        return Ok(copy);
    }
    let mut command = Command::new("strip");
    // macOS 上不加时连链接器要的符号都去掉，`-x` 只去局部符号
    if os == "macos" {
        command.arg("-x");
    }
    match run_tool(command.arg(&copy), "strip 去掉符号") {
        Ok(()) => {
            let size = |path: &Path| human_size(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
            println!("去掉符号：{} → {}（--keep-symbols 不去）", size(exe), size(&copy));
            Ok(copy)
        }
        Err(error) => {
            eprintln!("提示：{error}；包里的可执行文件带着符号和调试信息");
            std::fs::copy(exe, &copy).map_err(|e| format!("拷贝 {} 失败：{e}", exe.display()))?;
            Ok(copy)
        }
    }
}

/// 用到 CEF 视图时：挑出 CEF 的运行文件（在 `dir` 里：编出来的应用、动态库旁边），应用嵌上 CEF 要的清单；
/// 开沙箱时（有 `library`）带上应用的动态库（改名成和程序同名），不开时编子进程程序（和应用一样处理过，放在应用旁边）。
/// `exe` 是要放进包里的程序
fn webview_runtime(
    options: &BundleOptions,
    dir: Option<&Path>,
    library: Option<&Path>,
    exe: &Path,
    os: &str,
    release: bool,
    target: Option<&str>,
    strip: bool,
    out_dir: &Path,
) -> Result<Option<webview::Runtime>, String> {
    let Some(settings) = &options.webview else { return Ok(None) };
    let dir = dir.ok_or("找不到编出来的应用在哪个目录（CEF 的运行文件在它旁边）")?;
    if os == "macos" {
        // 内核框架和 5 个辅助程序的包在拼 .app 时装进 Contents/Frameworks（`webview::install_macos`）
        let framework = webview::find_framework(dir)?;
        let helper = build(webview::PACKAGE, webview::HELPER, release, target, None)?;
        let helper = packaged_exe(&helper, options, os, strip, out_dir)?;
        println!("CEF 视图：带上子进程程序和内核框架（{}）", framework.display());
        let framework = webview::MacFramework { path: framework, locales: settings.locales.clone() };
        return Ok(Some(webview::Runtime { helper: Some(helper), files: Vec::new(), framework: Some(framework) }));
    }
    let mut files = webview::runtime_files(dir, &settings.locales)?;
    let helper = match library {
        Some(library) => {
            files.push((format!("{}.dll", options.bin), library.to_path_buf()));
            None
        }
        None => {
            // 子进程程序不带应用的特性：它只跑 CEF 的子进程
            let helper = build(webview::PACKAGE, webview::HELPER, release, target, None)?;
            let helper = packaged_exe(&helper, options, os, strip, out_dir)?;
            if os == "windows" {
                exe_icon::embed_version(&helper, &version_info(options, &helper))?;
            }
            Some(helper)
        }
    };
    exe_icon::embed_manifest(exe, webview::MANIFEST)?;
    let size: u64 = files.iter().filter_map(|(_, path)| std::fs::metadata(path).ok()).map(|m| m.len()).sum();
    let what = if library.is_some() { "开了沙箱：带上应用的动态库" } else { "带上子进程程序" };
    println!("CEF 视图：{what}和 CEF 的运行文件（{} 个，{}）", files.len(), human_size(size));
    Ok(Some(webview::Runtime { helper, files, framework: None }))
}

/// 程序的版本信息：应用名、版本、发布者、版权、文件名
fn version_info<'a>(options: &'a BundleOptions, exe: &'a Path) -> exe_icon::VersionInfo<'a> {
    static PUBLISHER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    exe_icon::VersionInfo {
        name: &options.name,
        version: &options.version,
        display_version: &options.full_version,
        company: PUBLISHER.get_or_init(|| self_installer::publisher(options)),
        copyright: options.copyright.as_deref().unwrap_or_default(),
        file_name: exe.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
    }
}

/// 开沙箱时的程序：CEF 的 `bootstrap.exe`（在动态库旁边）拷成 `<可执行文件>.exe`，去掉它自己的图标、版本信息、清单
/// （之后写应用的）。它启动时加载旁边同名的 `.dll`
fn bootstrap_program(dll: &Path, options: &BundleOptions, out_dir: &Path) -> Result<PathBuf, String> {
    let source = dll.parent().map(|dir| dir.join(webview::BOOTSTRAP)).filter(|p| p.is_file()).ok_or_else(|| {
        format!("找不到 {}（CEF 带的，编译 CEF 视图时 cef-dll-sys 拷到动态库旁边）", webview::BOOTSTRAP)
    })?;
    let directory = out_dir.join("bootstrap");
    std::fs::create_dir_all(&directory).map_err(|e| format!("创建 {} 失败：{e}", directory.display()))?;
    let exe = directory.join(format!("{}.exe", options.bin));
    std::fs::copy(&source, &exe).map_err(|e| format!("拷贝 {} 失败：{e}", source.display()))?;
    exe_icon::remove_resources(&exe, &[exe_icon::RT_ICON, exe_icon::RT_GROUP_ICON, exe_icon::RT_VERSION, exe_icon::RT_MANIFEST])?;
    Ok(exe)
}

/// 编应用的动态库（开沙箱时），返回 `.dll` 的路径
fn build_library(package: &str, library: &str, release: bool, target: Option<&str>, features: Option<&str>) -> Result<PathBuf, String> {
    let mut command = Command::new(cargo());
    command.args(["build", "-p", package, "--lib", "--message-format=json-render-diagnostics", "--config", STATIC_CRT]);
    if release {
        command.arg("--release");
    }
    if let Some(target) = target {
        command.args(["--target", target]);
    }
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    let output = command.stderr(Stdio::inherit()).output().map_err(|e| format!("运行 cargo build 失败：{e}"))?;
    if !output.status.success() {
        return Err("编译失败".into());
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == library)
        .find_map(|m| m["filenames"].as_array()?.iter().filter_map(Value::as_str).find(|f| f.ends_with(".dll")).map(PathBuf::from))
        .ok_or_else(|| format!("编译成功了，但没找到动态库 {library}.dll"))
}

/// 编 arm64 和 x86_64 两份，用 `lipo` 合成一个可执行文件，放在输出目录的 `universal/` 下
fn build_universal(package: &str, bin: &str, release: bool, features: Option<&str>, out_dir: &Path) -> Result<PathBuf, String> {
    let installed = Command::new("rustup").args(["target", "list", "--installed"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    if let Some(missing) = UNIVERSAL_TARGETS.iter().find(|t| !installed.contains(**t)) {
        return Err(format!("没有 {missing} 的标准库，先装一下：rustup target add {missing}"));
    }
    let parts = UNIVERSAL_TARGETS.iter().map(|target| build(package, bin, release, Some(target), features)).collect::<Result<Vec<_>, _>>()?;
    let directory = out_dir.join("universal");
    std::fs::create_dir_all(&directory).map_err(|e| format!("创建 {} 失败：{e}", directory.display()))?;
    let exe = directory.join(bin);
    run_tool(Command::new("lipo").arg("-create").arg("-output").arg(&exe).args(&parts), "lipo 合成通用二进制")?;
    println!("通用二进制：{}（{}）", exe.display(), human_size(std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0)));
    Ok(exe)
}

/// 拼出 .app：
///
/// ```text
/// 名字.app/Contents/Info.plist
///                  /PkgInfo
///                  /MacOS/<可执行文件>
///                  /Resources/AppIcon.icns、资源文件
/// ```
fn assemble(options: &BundleOptions, exe: &Path, out_dir: &Path) -> Result<PathBuf, String> {
    let app = out_dir.join(format!("{}.app", options.name));
    if app.exists() {
        // 只删自己生成过的（有 Info.plist 的）包，不误删别的东西
        if !app.join("Contents/Info.plist").is_file() {
            return Err(format!("{} 已经存在，而且不像是打包生成的，不覆盖", app.display()));
        }
        std::fs::remove_dir_all(&app).map_err(|e| format!("删除旧的 {} 失败：{e}", app.display()))?;
    }
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    for dir in [&macos, &resources] {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    }
    std::fs::copy(exe, macos.join(&options.bin)).map_err(|e| format!("拷贝可执行文件 {} 失败：{e}", exe.display()))?;
    icon::write_icns(&options.icon, &resources.join("AppIcon.icns"))?;
    for doc in &options.document_types {
        if let (Some(path), Some(name)) = (&doc.icon, doc.icon_name()) {
            let source = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("icns")) {
                icon::IconSource::Icns(path.clone())
            } else {
                icon::IconSource::Image(path.clone())
            };
            icon::write_icns(&source, &resources.join(format!("{name}.icns")))?;
        }
    }
    for resource in &options.resources {
        let name = resource.file_name().ok_or_else(|| format!("资源路径 {} 没有文件名", resource.display()))?;
        copy_recursive(resource, &resources.join(name))?;
    }
    write(&contents.join("Info.plist"), options.info_plist().to_xml().as_bytes())?;
    write(&contents.join("PkgInfo"), b"APPL????")?;
    Ok(app)
}

/// `--emit`：Windows 的 .reg（UTF-16 带字节序标记，注册表编辑器和 reg import 都认）、Linux 的 .desktop 和 MIME 声明
fn emit(kind: Emit, options: &BundleOptions, exe: Option<&str>, out_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建 {} 失败：{e}", out_dir.display()))?;
    let (schemes, docs) = (&options.url_schemes, &options.document_types);
    if schemes.is_empty() && docs.is_empty() && kind != Emit::LinuxDesktop {
        return Err("没有声明 url-schemes 和 document-types，没什么要生成的".into());
    }
    let id = &options.identifier;
    match kind {
        Emit::WindowsRegistry => {
            let exe = exe.map(str::to_owned).unwrap_or_else(|| format!("C:\\Program Files\\{}\\{}.exe", options.name, options.bin));
            let text = associations::windows_registry(&options.name, id, &exe, schemes, docs);
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            let path = out_dir.join(format!("{id}.reg"));
            write(&path, &bytes)?;
            Ok(path)
        }
        Emit::LinuxDesktop => {
            let path = out_dir.join(format!("{id}.desktop"));
            write(&path, associations::linux_desktop_entry(&options.name, id, exe.unwrap_or(&options.bin), schemes, docs).as_bytes())?;
            Ok(path)
        }
        Emit::LinuxMime => {
            let text = associations::linux_mime_info(docs).ok_or("没有自定义文档类型（document-types 里没写 uti 的），不需要 MIME 声明")?;
            let path = out_dir.join(format!("{id}.xml"));
            write(&path, text.as_bytes())?;
            Ok(path)
        }
    }
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
}

fn copy_recursive(from: &Path, to: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(from).map_err(|e| format!("资源 {} 读不了：{e}", from.display()))?;
    if meta.is_dir() {
        std::fs::create_dir_all(to).map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
        for entry in std::fs::read_dir(from).map_err(|e| format!("读取 {} 失败：{e}", from.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ()).map_err(|e| format!("拷贝 {} 失败：{e}", from.display()))
    }
}

/// 代码签名并检查。`-` 是临时签名：不需要证书，只在本机有效（别的机器上会被门禁拦下）
fn sign(app: &Path, identity: &str, entitlements: Option<&Path>) -> Result<(), String> {
    let mut command = Command::new("codesign");
    command.args(["--force", "--deep", "--sign", identity]);
    if let Some(entitlements) = entitlements {
        command.arg("--entitlements").arg(entitlements);
    }
    run_tool(command.arg(app), "codesign 签名")?;
    run_tool(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(app), "codesign 检查签名")?;
    println!("已签名（{}），codesign --verify 通过", if identity == "-" { "临时签名" } else { identity });
    Ok(())
}

/// 做 .dmg：里面是应用和一个指向"应用程序"文件夹的替身，打开后拖进去就装好了
fn make_dmg(app: &Path, options: &BundleOptions, out_dir: &Path) -> Result<PathBuf, String> {
    let staging = out_dir.join(format!(".dmg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("创建 {} 失败：{e}", staging.display()))?;
    let result = (|| {
        // ditto 保留签名用到的扩展属性
        run_tool(Command::new("ditto").arg(app).arg(staging.join(app.file_name().unwrap_or(OsStr::new("App.app")))), "ditto 拷贝应用")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("/Applications", staging.join("Applications")).map_err(|e| format!("创建替身失败：{e}"))?;
        let dmg = out_dir.join(format!("{}-{}.dmg", options.name, options.version));
        let mut create = Command::new("hdiutil");
        create.args(["create", "-quiet", "-ov", "-format", "UDZO", "-volname", &options.name, "-srcfolder"]).arg(&staging).arg(&dmg);
        run_tool(&mut create, "hdiutil 做磁盘映像")?;
        run_tool(Command::new("hdiutil").args(["verify", "-quiet"]).arg(&dmg), "hdiutil 检查磁盘映像")?;
        Ok(dmg)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn run_tool(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("{what}：启动失败：{e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{what}失败：{}", String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn dir_size(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| dir_size(&e.path())).sum(),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1 << 20 { format!("{:.1} MB", bytes as f64 / (1 << 20) as f64) } else { format!("{:.0} KB", bytes as f64 / 1024.0) }
}
