//! `--format installer`：用 Aureole 写的安装程序。把安装程序本身、应用的文件拼成一个可执行文件（格式见 `aureole-installer`）

use std::path::{Path, PathBuf};

use aureole_installer::{Builder, DocumentType, Manifest};

use crate::icon;
use crate::options::BundleOptions;
use crate::webview::Runtime;

/// 默认的安装程序：应用没写自己的时用它
pub const DEFAULT_STUB: (&str, &str) = ("aureole-installer", "aureole-setup");

/// 发布者：设置里写的，或者维护者的名字（去掉邮箱），都没有时是应用名
pub fn publisher(options: &BundleOptions) -> String {
    if let Some(publisher) = &options.installer.publisher {
        return publisher.clone();
    }
    match &options.maintainer {
        Some(maintainer) => maintainer.split('<').next().unwrap_or_default().trim().to_owned(),
        None => options.name.clone(),
    }
}

/// 清单：文件由 [`Builder`] 填
pub fn manifest(options: &BundleOptions, executable: &str, icon: Option<String>) -> Manifest {
    let documents = options
        .document_types
        .iter()
        .map(|doc| DocumentType { name: doc.name.clone(), extensions: doc.extensions.clone(), mime: doc.linux_mime(), owner: doc.exported })
        .collect();
    Manifest {
        name: options.name.clone(),
        identifier: options.identifier.clone(),
        version: options.full_version.clone(),
        publisher: publisher(options),
        executable: executable.to_owned(),
        icon,
        url_schemes: options.url_schemes.clone(),
        documents,
        ..Manifest::default()
    }
}

/// 文件名：`<可执行文件>_<版本>_<架构>-installer`
fn file_name(options: &BundleOptions, target: Option<&str>, os: &str) -> String {
    let arch = target.and_then(|t| t.split('-').next()).unwrap_or(std::env::consts::ARCH);
    let extension = if os == "windows" { ".exe" } else { "" };
    format!("{}_{}_{arch}-installer{extension}", options.bin, options.full_version)
}

/// 拼出安装程序。`exe` 是要放进去的应用，`stub` 是安装程序本身（都处理过了：去符号、嵌图标），
/// `webview` 是用到 CEF 视图时要多带的（子进程程序和 CEF 的运行文件都装在应用旁边）
pub fn package(options: &BundleOptions, exe: &Path, webview: Option<&Runtime>, stub: &Path, target: Option<&str>, os: &str, out_dir: &Path) -> Result<PathBuf, String> {
    let work = out_dir.join("installer-files");
    std::fs::create_dir_all(&work).map_err(|e| format!("创建 {} 失败：{e}", work.display()))?;
    let executable = exe.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("可执行文件的路径 {} 不对", exe.display()))?.to_owned();

    // 图标放进去：Windows 的快捷方式、卸载项、Linux 的应用列表都要。现成的 .icns 画不了：提示，不放
    let (icon_name, icon_file) = if os == "windows" { ("icon.ico", work.join("app-icon.ico")) } else { ("icon.png", work.join("app-icon.png")) };
    let icon_written = if os == "windows" { icon::write_ico_from(&options.icon, &icon_file) } else { icon::write_preview(&options.icon, 256, &icon_file) };
    let icon = match icon_written {
        Ok(()) => Some(icon_name.to_owned()),
        Err(error) => {
            eprintln!("提示：{error}；安装程序里不放图标");
            None
        }
    };

    let mut builder = Builder::new(manifest(options, &executable, icon.clone()));
    builder.add(&executable, exe, true);
    if let Some(runtime) = webview {
        if let Some(helper) = &runtime.helper {
            let name = helper.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("子进程程序的路径 {} 不对", helper.display()))?;
            builder.add(name, helper, true);
        }
        for (path, source) in &runtime.files {
            builder.add(path, source, false);
        }
    }
    if icon.is_some() {
        builder.add(icon_name, &icon_file, false);
    }
    for resource in &options.resources {
        let name = resource.file_name().and_then(|n| n.to_str()).ok_or_else(|| format!("资源路径 {} 没有文件名", resource.display()))?;
        if resource.is_dir() {
            builder.add_directory(name, resource).map_err(|e| e.to_string())?;
        } else {
            builder.add(name, resource, false);
        }
    }
    let out = out_dir.join(file_name(options, target, os));
    builder.write(stub, &out).map_err(|e| e.to_string())?;
    Ok(out)
}
