//! Windows、Linux 的安装包：交给外部工具 cargo-packager。
//!
//! 设置还是只写在 `[package.metadata.bundle]` 一处：这里把它换成 cargo-packager 的配置（JSON），生成它要的图标
//! （各尺寸的 PNG、Windows 的 .ico），然后调用它。.app 和 .dmg 还是自己做，见 `main.rs`。
//! cargo-packager 只能在目标系统上打它的包，第一次打时还会下载 NSIS、linuxdeploy（这个工具不下载，是它下载）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value, json};

use crate::associations::{self, Role};
use crate::icon;
use crate::options::BundleOptions;
use crate::webview::Runtime;

/// 验证过的 cargo-packager 版本：配置的键和它的版本有关
pub const PACKAGER_VERSION: &str = "0.11.8";
/// Linux 上放进安装包的图标尺寸（装到 hicolor 主题里，桌面按需要的大小选）
const LINUX_ICON_SIZES: [u32; 6] = [32, 48, 64, 128, 256, 512];
/// Windows 的 .ico 里的尺寸
const WINDOWS_ICON_SIZES: [u32; 5] = [16, 32, 48, 64, 256];
/// NSIS 安装界面顶部的图、侧边的图的大小
const HEADER_IMAGE: [u32; 2] = [150, 57];
const SIDEBAR_IMAGE: [u32; 2] = [164, 314];
/// .deb 依赖的运行库：窗口库和图形库运行时才打开它们（`dlopen`），只看可执行文件链接了什么是看不出来的。
/// 用户的 `linux-depends` 接在后面
const DEB_DEPENDS: [&str; 9] =
    ["libfontconfig1", "libvulkan1", "libx11-6", "libx11-xcb1", "libxcursor1", "libxi6", "libxkbcommon0", "libxkbcommon-x11-0", "libwayland-client0"];

/// 要打的包
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// macOS 的 .app（`main.rs` 自己做，在哪个系统上都能拼出目录结构）
    App,
    /// Debian、Ubuntu 的软件包
    Deb,
    /// Linux 上不用安装、下载就能运行的单个文件
    AppImage,
    /// Windows 的安装程序（NSIS）
    Nsis,
    /// 用 Aureole 写的安装程序（Windows、Linux），`main.rs` 拼
    Installer,
}

impl Format {
    pub fn parse_list(text: &str) -> Result<Vec<Self>, String> {
        let mut formats = Vec::new();
        for name in text.split(',').map(str::trim) {
            let format = match name {
                "app" => Self::App,
                "deb" => Self::Deb,
                "appimage" => Self::AppImage,
                "nsis" => Self::Nsis,
                "installer" => Self::Installer,
                other => return Err(format!("--format 不认识 {other}（app、deb、appimage、nsis、installer）")),
            };
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        Ok(formats)
    }

    /// 没写 `--format` 时打什么
    pub fn default_for(os: &str) -> Vec<Self> {
        match os {
            "linux" => vec![Self::Deb, Self::AppImage],
            "windows" => vec![Self::Nsis],
            _ => vec![Self::App],
        }
    }

    /// cargo-packager 里的名字；.app 不交给它
    fn packager_name(self) -> Option<&'static str> {
        match self {
            Self::App | Self::Installer => None,
            Self::Deb => Some("deb"),
            Self::AppImage => Some("appimage"),
            Self::Nsis => Some("nsis"),
        }
    }

    /// 只能在哪个系统上打；`None` 表示哪里都行
    fn system(self) -> Option<&'static str> {
        match self {
            Self::App | Self::Installer => None,
            Self::Deb | Self::AppImage => Some("linux"),
            Self::Nsis => Some("windows"),
        }
    }

    /// 能不能在 `os` 上打这些包
    pub fn check(formats: &[Self], os: &str) -> Result<(), String> {
        if formats.contains(&Self::Installer) && os == "macos" {
            return Err("macOS 上的应用拖进\"应用程序\"就装好了，用 --dmg；installer 是给 Windows、Linux 的".into());
        }
        match formats.iter().find(|f| f.system().is_some_and(|system| system != os)) {
            Some(format) => Err(format!("{} 要在 {} 上打，这里是 {os}", format.packager_name().unwrap_or("这种包"), format.system().unwrap_or(""))),
            None => Ok(()),
        }
    }
}

/// .deb 的包名：只能有小写字母、数字、连字符、点、加号，应用名是中文时不能用。用 Cargo 包名
fn deb_package_name(package: &str) -> String {
    package.chars().map(|c| if c == '_' { '-' } else { c.to_ascii_lowercase() }).collect()
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Editor => "editor",
        Role::Viewer => "viewer",
        Role::Shell => "shell",
        Role::None => "none",
    }
}

/// .deb 的维护者。Debian 要求必须有：都没写时用占位的
fn maintainer(options: &BundleOptions) -> String {
    options.maintainer.clone().unwrap_or_else(|| format!("{} <noreply@example.com>", options.name))
}

/// 准备好的文件：图标（PNG 和 .ico）、MIME 声明、安装界面的图，CEF 视图的子进程程序和 CEF 的运行文件
#[derive(Default)]
pub struct Prepared {
    pub icons: Vec<PathBuf>,
    pub mime: Option<PathBuf>,
    pub header_bmp: Option<PathBuf>,
    pub sidebar_bmp: Option<PathBuf>,
    pub installer_icon: Option<PathBuf>,
    pub webview: Option<Runtime>,
}

/// cargo-packager 的配置。`exe` 是编好的可执行文件
pub fn config(options: &BundleOptions, exe: &Path, prepared: &Prepared, target: Option<&str>) -> Result<Value, String> {
    let (icons, mime) = (&prepared.icons, prepared.mime.as_deref());
    let file_name = exe.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("可执行文件的路径 {} 不对", exe.display()))?;
    let directory = exe.parent().ok_or_else(|| format!("可执行文件的路径 {} 不对", exe.display()))?;
    let text = |path: &Path| path.display().to_string();
    let ico = icons.iter().find(|p| p.extension().is_some_and(|e| e == "ico"));

    let mut config = Map::new();
    config.insert("productName".into(), json!(options.name));
    config.insert("version".into(), json!(options.version));
    config.insert("identifier".into(), json!(options.identifier));
    // .deb 必须有说明
    config.insert("description".into(), json!(options.description.clone().unwrap_or_else(|| options.name.clone())));
    config.insert("binariesDir".into(), json!(text(directory)));
    let mut binaries = vec![json!({ "path": file_name, "main": true })];
    if let Some(helper) = prepared.webview.as_ref().and_then(|runtime| runtime.helper.as_ref()) {
        // 子进程程序和应用在同一个目录里（都是处理过的拷贝），装到应用旁边
        let name = helper.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("子进程程序的路径 {} 不对", helper.display()))?;
        if helper.parent() != Some(directory) {
            return Err(format!("子进程程序 {} 要和应用在同一个目录里", helper.display()));
        }
        binaries.push(json!({ "path": name, "main": false }));
    }
    config.insert("binaries".into(), json!(binaries));
    config.insert("icons".into(), json!(icons.iter().map(|p| text(p)).collect::<Vec<_>>()));
    config.insert("authors".into(), json!([maintainer(options)]));
    if let Some(copyright) = &options.copyright {
        config.insert("copyright".into(), json!(copyright));
    }
    if let Some(category) = &options.category {
        config.insert("category".into(), json!(category));
    }
    if let Some(target) = target {
        config.insert("targetTriple".into(), json!(target));
    }
    if !options.url_schemes.is_empty() {
        config.insert("deepLinkProtocols".into(), json!([{ "schemes": options.url_schemes }]));
    }
    if !options.document_types.is_empty() {
        let documents: Vec<Value> = options
            .document_types
            .iter()
            .map(|doc| {
                let mut document = json!({ "extensions": doc.extensions, "name": doc.name, "description": doc.name, "role": role_name(doc.role) });
                if let Some(mime) = doc.linux_mime() {
                    document["mimeType"] = json!(mime);
                }
                document
            })
            .collect();
        config.insert("fileAssociations".into(), json!(documents));
    }
    let mut resources = Vec::new();
    for resource in &options.resources {
        let name = resource.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("资源路径 {} 没有文件名", resource.display()))?;
        resources.push(json!({ "src": text(resource), "target": name }));
    }
    // CEF 的运行文件按资源放：Windows 上资源装在安装目录里，就在应用旁边
    for (target, source) in prepared.webview.iter().flat_map(|runtime| &runtime.files) {
        resources.push(json!({ "src": text(source), "target": target }));
    }
    if !resources.is_empty() {
        config.insert("resources".into(), json!(resources));
    }
    let depends: Vec<&str> = DEB_DEPENDS.iter().copied().chain(options.linux_depends.iter().map(String::as_str)).collect();
    let mut deb = json!({ "packageName": deb_package_name(&options.package), "depends": depends });
    if let Some(mime) = mime {
        // shared-mime-info 盯着这个目录，装好、卸载时自动更新数据库
        deb["files"] = json!({ text(mime): format!("usr/share/mime/packages/{}.xml", options.identifier) });
    }
    config.insert("deb".into(), deb);
    let languages = if options.nsis.languages.is_empty() { vec!["SimpChinese".to_owned(), "English".to_owned()] } else { options.nsis.languages.clone() };
    let mut nsis = json!({ "installMode": options.nsis.install_mode.as_deref().unwrap_or("currentUser"), "languages": languages });
    if let Some(ico) = prepared.installer_icon.as_deref().or(ico.map(PathBuf::as_path)) {
        nsis["installerIcon"] = json!(text(ico));
    }
    if let Some(header) = &prepared.header_bmp {
        nsis["headerImage"] = json!(text(header));
    }
    if let Some(sidebar) = &prepared.sidebar_bmp {
        nsis["sidebarImage"] = json!(text(sidebar));
    }
    if let Some(template) = &options.nsis.template {
        nsis["template"] = json!(text(template));
    }
    config.insert("nsis".into(), nsis);
    Ok(Value::Object(config))
}

/// 打 `formats` 里的包，放进 `out_dir`。`webview` 是用到 CEF 视图时要多带的
pub fn package(options: &BundleOptions, exe: &Path, webview: Option<&Runtime>, formats: &[Format], target: Option<&str>, out_dir: &Path) -> Result<(), String> {
    let names: Vec<&str> = formats.iter().filter_map(|f| f.packager_name()).collect();
    if names.is_empty() {
        return Ok(());
    }
    check_packager()?;
    if formats.contains(&Format::Deb) && options.maintainer.is_none() {
        eprintln!("提示：没有写维护者，.deb 里暂用 {}；发布前在 [package.metadata.bundle] 里写 maintainer = \"名字 <邮箱>\"", maintainer(options));
    }
    let work = out_dir.join("installer-files");
    std::fs::create_dir_all(&work).map_err(|e| format!("创建 {} 失败：{e}", work.display()))?;
    let icons = icon::write_installer_icons(&options.icon, &LINUX_ICON_SIZES, &WINDOWS_ICON_SIZES, &work)?;
    let mime = match associations::linux_mime_info(&options.document_types) {
        Some(info) => {
            let path = work.join(format!("{}.xml", options.identifier));
            std::fs::write(&path, info).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
            Some(path)
        }
        None => None,
    };
    let mut prepared = Prepared { icons, mime, webview: webview.cloned(), ..Prepared::default() };
    if formats.contains(&Format::Nsis) {
        // 安装界面的图只认 24 位的 BMP：用户给什么格式都换成它
        let bmp = |source: &Option<PathBuf>, size: [u32; 2], name: &str| -> Result<Option<PathBuf>, String> {
            source.as_ref().map(|source| { let out = work.join(name); icon::write_bmp(source, size, &out).map(|()| out) }).transpose()
        };
        prepared.header_bmp = bmp(&options.nsis.header_image, HEADER_IMAGE, "nsis-header.bmp")?;
        prepared.sidebar_bmp = bmp(&options.nsis.sidebar_image, SIDEBAR_IMAGE, "nsis-sidebar.bmp")?;
        if let Some(source) = &options.nsis.installer_icon {
            let out = work.join("nsis-icon.ico");
            icon::write_ico(source, &WINDOWS_ICON_SIZES, &out)?;
            prepared.installer_icon = Some(out);
        }
    }
    let config = config(options, exe, &prepared, target)?;
    // 配置直接放在命令行上（它认 JSON 文本；给文件路径时会把工作目录换到那里，再把路径当目录用）。
    // 另外写一份文件：打不出来时可以照着看
    let path = work.join("packager.json");
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&path, &text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    let status = Command::new(crate::cargo())
        .arg("packager")
        .arg("--config")
        .arg(&text)
        .args(["--formats", &names.join(",")])
        .arg("--out-dir")
        .arg(out_dir)
        .status()
        .map_err(|e| format!("运行 cargo packager 失败：{e}"))?;
    if !status.success() {
        return Err(format!("cargo packager 失败（配置在 {}）", path.display()));
    }
    Ok(())
}

/// 工具装了没有，版本是不是验证过的
fn check_packager() -> Result<(), String> {
    let install = format!("cargo install cargo-packager --version {PACKAGER_VERSION} --locked");
    let output = Command::new(crate::cargo()).args(["packager", "--version"]).stderr(Stdio::null()).output().map_err(|e| format!("运行 cargo 失败：{e}"))?;
    if !output.status.success() {
        return Err(format!("Windows、Linux 的安装包要用 cargo-packager，先装一下：{install}"));
    }
    let version = String::from_utf8_lossy(&output.stdout);
    if !version.contains(PACKAGER_VERSION) {
        eprintln!("提示：装的 cargo-packager 是 {}，验证过的是 {PACKAGER_VERSION}（{install}）", version.trim());
    }
    Ok(())
}
