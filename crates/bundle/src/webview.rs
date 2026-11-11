//! 用到 CEF 视图（`aureole-webview`）的应用：多编一个子进程程序，带上 CEF 的运行文件，主程序嵌上 CEF 要的清单。
//!
//! Windows 上 CEF 的运行文件要放在主程序旁边（`libcef.dll` 按导入表加载，资源和语言包在它旁边找），
//! 子进程程序 `aureole-webview-helper` 也是（CEF 视图按主程序的路径找它）。cef-dll-sys 的编译脚本已经把运行文件
//! 拷到了目标目录（编好的可执行文件所在的目录），这里按清单挑出要的：不要编译用的 `libcef.lib`、CMake 文件、
//! `bootstrap` 程序。
//!
//! macOS 上内核是 `Contents/Frameworks` 里的一个框架（[`MAC_FRAMEWORK`]），旁边是 5 个辅助程序的包
//! （`<程序名> Helper.app`，还有 GPU、Renderer、Plugin、Alerts 四种），可执行文件都是子进程程序，CEF 按主程序的名字
//! 找它们。内核框架从编译时用的地方找（`CEF_PATH`，或者 cef-dll-sys 下载到的目标目录），语言包按设置挑。
//! Linux 上 .deb 把可执行文件和资源分开放，还没做，打包时直接报错，不打出打不开页面的包

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::installers::Format;

/// CEF 视图的包、它的子进程程序
pub const PACKAGE: &str = "aureole-webview";
pub const HELPER: &str = "aureole-webview-helper";

/// Windows 上主程序旁边要有的 CEF 文件（CEF 156 验证过）：内核、崩溃处理、ICU 数据、V8 快照、资源包、
/// 显卡用的着色器编译器，没有显卡加速时 WebGL 用的软件 Vulkan
const WINDOWS_FILES: [&str; 12] = [
    "libcef.dll",
    "chrome_elf.dll",
    "icudtl.dat",
    "v8_context_snapshot.bin",
    "resources.pak",
    "chrome_100_percent.pak",
    "chrome_200_percent.pak",
    "d3dcompiler_47.dll",
    "dxcompiler.dll",
    "vk_swiftshader.dll",
    "vk_swiftshader_icd.json",
    "vulkan-1.dll",
];
/// 语言包的目录；找不到界面语言的语言包时 CEF 用这一个
const LOCALES: &str = "locales";
const FALLBACK_LOCALE: &str = "en-US";
/// Chromium 用到的开源组件的许可（发给别人的程序里要带上）：CEF 给的文件名以它开头
const CREDITS_PREFIX: &str = "CREDITS.";
/// 装到应用旁边换个一看就知道是什么的名字（双击就能打开看）
const CREDITS_NAME: &str = "LICENSES.chromium.htm";

/// 目录里 CEF 的许可清单（文件名以 [`CREDITS_PREFIX`] 开头的那个）
fn credits_file(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).find(|path| {
        path.is_file() && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(CREDITS_PREFIX))
    })
}

/// 和 `crates/webview/src/bin/aureole-webview.manifest` 一样（两边一起改）：声明支持到 Windows 10/11、
/// 按显示器感知 DPI、用新版的系统控件、不用提权
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
pub const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <maxversiontested Id="10.0.18362.0"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

/// CEF 带的 bootstrap 程序：开沙箱时改名成应用名，加载旁边同名的动态库（cef-dll-sys 也拷到了目标目录里）
pub const BOOTSTRAP: &str = "bootstrap.exe";

/// 要放进包里的：子进程程序（处理过的拷贝，和主程序在同一个目录；开沙箱时没有），CEF 的运行文件和开沙箱时应用的动态库
/// （包里的相对路径、源文件）
#[derive(Clone, Debug, Default)]
pub struct Runtime {
    pub helper: Option<PathBuf>,
    pub files: Vec<(String, PathBuf)>,
    /// macOS：内核框架（装进 `.app` 的 `Contents/Frameworks`）和要带的语言包
    pub framework: Option<MacFramework>,
}

/// macOS 上要装进 `.app` 的内核框架
#[derive(Clone, Debug)]
pub struct MacFramework {
    pub path: PathBuf,
    /// 设置里的 `webview.locales`（空的时候全带）
    pub locales: Vec<String>,
}

/// 包里编成动态库（`cdylib`）的库的名字：开沙箱时要它
pub fn library_target(package: &Value) -> Option<String> {
    package["targets"]
        .as_array()?
        .iter()
        .find(|t| t["crate_types"].as_array().or_else(|| t["kind"].as_array()).is_some_and(|k| k.iter().any(|k| k == "cdylib")))
        .and_then(|t| t["name"].as_str())
        .map(str::to_owned)
}

/// 包直接依赖 CEF 视图（开发、编译时的依赖不算）
pub fn used_by(package: &Value) -> bool {
    package["dependencies"].as_array().into_iter().flatten().any(|d| d["name"] == PACKAGE && d["kind"].is_null())
}

/// 在 `os` 上能不能把 CEF 视图打进 `formats` 里的包。编译之前查，不白编
pub fn check(os: &str, formats: &[Format]) -> Result<(), String> {
    match os {
        "windows" if formats.contains(&Format::App) => Err("用到 CEF 视图的应用的 .app 要在 macOS 上打".into()),
        "windows" => Ok(()),
        // macOS：Contents/Frameworks 里放内核框架，旁边 5 个辅助程序的包（见 `install_macos`）
        "macos" if formats.iter().all(|f| *f == Format::App) => Ok(()),
        "macos" => Err("用到 CEF 视图的应用在 macOS 上只能打 .app（可以再加 --dmg）".into()),
        other => Err(format!("用到 CEF 视图的应用在 {other} 上还不能打包（现在只有 Windows、macOS 可以）")),
    }
}

/// 挑出 CEF 的运行文件：`dir` 是编好的可执行文件所在的目录。`locales` 不空时只带这些语言包（加上找不到时用的英文），
/// 一种语言的几个变体（`_FEMININE` 之类）一起带上
pub fn runtime_files(dir: &Path, locales: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    let mut files = Vec::new();
    for name in WINDOWS_FILES {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(format!("找不到 CEF 的运行文件 {}（编译 CEF 视图时 cef-dll-sys 会拷过去）", path.display()));
        }
        files.push((name.to_owned(), path));
    }
    if let Some(credits) = credits_file(dir) {
        files.push((CREDITS_NAME.to_owned(), credits));
    }
    let directory = dir.join(LOCALES);
    let mut packs: Vec<(String, PathBuf)> = std::fs::read_dir(&directory)
        .map_err(|e| format!("读不了 CEF 的语言包目录 {}：{e}", directory.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "pak"))
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_owned(), path)))
        .collect();
    packs.sort();
    let language = |stem: &str| stem.split('_').next().unwrap_or(stem).to_owned();
    if !locales.is_empty() {
        let available: Vec<String> = packs.iter().map(|(stem, _)| language(stem)).collect();
        if let Some(missing) = locales.iter().find(|l| !available.contains(l)) {
            let mut names = available;
            names.dedup();
            return Err(format!("webview.locales 里的 {missing} 没有语言包（有：{}）", names.join("、")));
        }
        packs.retain(|(stem, _)| {
            let language = language(stem);
            language == FALLBACK_LOCALE || locales.contains(&language)
        });
    }
    if !packs.iter().any(|(stem, _)| stem == FALLBACK_LOCALE) {
        return Err(format!("{} 里没有 {FALLBACK_LOCALE}.pak", directory.display()));
    }
    files.extend(packs.into_iter().map(|(stem, path)| (format!("{LOCALES}/{stem}.pak"), path)));
    Ok(files)
}

/// macOS 上的内核框架（目录名）
pub const MAC_FRAMEWORK: &str = "Chromium Embedded Framework.framework";

/// macOS 上的辅助程序：包名的后缀、包标识符的后缀。CEF 按主程序的名字找 `<程序名> Helper<后缀>.app`
const MAC_HELPERS: [(&str, &str); 5] = [("", ""), (" (GPU)", ".gpu"), (" (Renderer)", ".renderer"), (" (Plugin)", ".plugin"), (" (Alerts)", ".alerts")];

/// 找 macOS 上的内核框架：先找 `CEF_PATH` 下面（编译时用的同一个，CEF 视图的说明里推荐设它），再找 cef-dll-sys
/// 下载到的地方（`dir` 是编出来的程序所在的目录，下面的 `build/cef-dll-sys-*/out`）。有几个时用最新的
pub fn find_framework(dir: &Path) -> Result<PathBuf, String> {
    let mut found = Vec::new();
    if let Some(root) = std::env::var_os("CEF_PATH").map(PathBuf::from) {
        collect_frameworks(&root, 3, &mut found);
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("build")) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("cef-dll-sys-") {
                collect_frameworks(&entry.path().join("out"), 2, &mut found);
            }
        }
    }
    let modified = |path: &PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
    found.into_iter().max_by_key(modified).ok_or_else(|| {
        format!("找不到 CEF 的内核框架 {MAC_FRAMEWORK}（编译 CEF 视图时下载的）：设 CEF_PATH 指向编译时用的目录，或者先编译一次")
    })
}

/// `root` 下面 `depth` 层以内叫 [`MAC_FRAMEWORK`] 的目录
fn collect_frameworks(root: &Path, depth: u32, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry.file_name() == MAC_FRAMEWORK {
            found.push(path);
        } else if depth > 1 {
            collect_frameworks(&path, depth - 1, found);
        }
    }
}

/// macOS 的语言包目录名（`zh_CN.lproj`）换成设置里的写法（`zh-CN`）：英文的 `en` 就是 [`FALLBACK_LOCALE`]
fn mac_locale(lproj: &str) -> String {
    let stem = lproj.trim_end_matches(".lproj").replace('_', "-");
    if stem == "en" { FALLBACK_LOCALE.to_owned() } else { stem }
}

/// 把内核框架、5 个辅助程序的包装进 `app`（已经拼好的 `.app`）的 `Contents/Frameworks`。
/// 语言包按 `locales` 挑（空的时候全带，英文总带上），返回装进去的字节数
pub fn install_macos(app: &Path, bin: &str, identifier: &str, helper: &Path, framework: &MacFramework) -> Result<u64, String> {
    let frameworks = app.join("Contents/Frameworks");
    let target = frameworks.join(MAC_FRAMEWORK);
    let resources = framework.path.join("Resources");
    if !framework.locales.is_empty() {
        let available: Vec<String> = std::fs::read_dir(&resources)
            .map_err(|e| format!("读不了 {}：{e}", resources.display()))?
            .flatten()
            .filter_map(|e| e.file_name().to_str().filter(|n| n.ends_with(".lproj")).map(mac_locale))
            .collect();
        if let Some(missing) = framework.locales.iter().find(|l| !available.contains(l)) {
            return Err(format!("webview.locales 里的 {missing} 没有语言包（有：{}）", available.join("、")));
        }
    }
    let keep = |path: &Path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.ends_with(".lproj") || framework.locales.is_empty() {
            return true;
        }
        let locale = mac_locale(name);
        locale == FALLBACK_LOCALE || framework.locales.contains(&locale)
    };
    let mut bytes = copy_tree(&framework.path, &target, &keep)?;
    for (suffix, id_suffix) in MAC_HELPERS {
        let name = format!("{bin} Helper{suffix}");
        let contents = frameworks.join(format!("{name}.app")).join("Contents");
        let macos = contents.join("MacOS");
        std::fs::create_dir_all(&macos).map_err(|e| format!("创建 {} 失败：{e}", macos.display()))?;
        let exe = macos.join(&name);
        bytes += std::fs::copy(helper, &exe).map_err(|e| format!("拷贝子进程程序到 {} 失败：{e}", exe.display()))?;
        let mut plist = crate::plist::Dict::default();
        plist.set("CFBundleExecutable", name.as_str());
        plist.set("CFBundleIdentifier", format!("{identifier}.helper{id_suffix}").as_str());
        plist.set("CFBundleName", name.as_str());
        plist.set("CFBundlePackageType", "APPL");
        plist.set("CFBundleInfoDictionaryVersion", "6.0");
        // 不在程序坞里显示
        plist.set("LSUIElement", true);
        plist.set("NSSupportsAutomaticGraphicsSwitching", true);
        let path = contents.join("Info.plist");
        std::fs::write(&path, plist.to_xml()).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    }
    Ok(bytes)
}

/// 拷一个目录（`keep` 为假的跳过），返回拷了多少字节。框架里的符号链接照原样建
fn copy_tree(from: &Path, to: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<u64, String> {
    let meta = std::fs::symlink_metadata(from).map_err(|e| format!("读不了 {}：{e}", from.display()))?;
    if meta.file_type().is_symlink() {
        let link = std::fs::read_link(from).map_err(|e| format!("读不了链接 {}：{e}", from.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link, to).map_err(|e| format!("建链接 {} 失败：{e}", to.display()))?;
        #[cfg(not(unix))]
        let _ = link;
        return Ok(0);
    }
    if !meta.is_dir() {
        return std::fs::copy(from, to).map_err(|e| format!("拷贝 {} 失败：{e}", from.display()));
    }
    std::fs::create_dir_all(to).map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
    let mut bytes = 0;
    for entry in std::fs::read_dir(from).map_err(|e| format!("读不了 {}：{e}", from.display()))?.flatten() {
        let path = entry.path();
        if keep(&path) {
            bytes += copy_tree(&path, &to.join(entry.file_name()), keep)?;
        }
    }
    Ok(bytes)
}
