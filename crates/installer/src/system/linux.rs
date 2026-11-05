//! Linux：应用列表里的 `.desktop`、图标、自定义文档类型的 MIME 声明，都放在用户自己的目录（`~/.local/share`）

use std::path::Path;

use super::Registered;
use crate::Error;
use crate::engine::InstallOptions;
use crate::manifest::Manifest;

/// `.desktop` 里的路径：有特殊字符时加引号，反斜杠、引号转义
fn desktop_quote(text: &str) -> String {
    if text.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '`' | '&' | ';' | '(' | ')')) {
        format!("\"{}\"", text.replace('\\', "\\\\\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"))
    } else {
        text.to_owned()
    }
}

/// `.desktop` 的内容
fn desktop_entry(manifest: &Manifest, directory: &Path) -> String {
    let exe = directory.join(&manifest.executable);
    let mut text = format!(
        "[Desktop Entry]\nType=Application\nName={}\nExec={} %U\nIcon={}\nTerminal=false\nStartupWMClass={}\n",
        manifest.name,
        desktop_quote(&exe.display().to_string()),
        manifest.identifier,
        Path::new(&manifest.executable).file_stem().map_or_else(String::new, |stem| stem.to_string_lossy().into_owned()),
    );
    let mut mime: Vec<String> = manifest.url_schemes.iter().map(|s| format!("x-scheme-handler/{s}")).collect();
    mime.extend(manifest.documents.iter().filter_map(|d| d.mime.clone()));
    if !mime.is_empty() {
        text.push_str(&format!("MimeType={};\n", mime.join(";")));
    }
    text
}

/// 自定义文档类型的 MIME 声明；没有时是 `None`
fn mime_info(manifest: &Manifest) -> Option<String> {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let types: String = manifest
        .documents
        .iter()
        .filter(|d| d.owner)
        .filter_map(|d| {
            let globs: String = d.extensions.iter().map(|e| format!("    <glob pattern=\"*.{}\"/>\n", escape(e))).collect();
            Some(format!("  <mime-type type=\"{}\">\n    <comment>{}</comment>\n{globs}  </mime-type>\n", escape(d.mime.as_deref()?), escape(&d.name)))
        })
        .collect();
    (!types.is_empty()).then(|| format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n{types}</mime-info>\n"))
}

/// 登记到 `data`（一般是 `~/.local/share`）
pub(super) fn register(data: &Path, manifest: &Manifest, options: &InstallOptions) -> Result<Registered, Error> {
    let mut registered = Registered::default();
    let mut write = |path: std::path::PathBuf, bytes: &[u8]| -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| Error::io(&path, e))?;
        registered.files.push(path);
        Ok(())
    };
    if options.menu_shortcut {
        write(data.join("applications").join(format!("{}.desktop", manifest.identifier)), desktop_entry(manifest, &options.directory).as_bytes())?;
    }
    if let Some(icon) = &manifest.icon {
        let bytes = std::fs::read(options.directory.join(icon)).map_err(|e| Error::io(&options.directory.join(icon), e))?;
        write(data.join("icons/hicolor/256x256/apps").join(format!("{}.png", manifest.identifier)), &bytes)?;
    }
    if let Some(info) = mime_info(manifest) {
        write(data.join("mime/packages").join(format!("{}.xml", manifest.identifier)), info.as_bytes())?;
    }
    refresh_databases(&registered);
    Ok(registered)
}

/// 让桌面知道变了。这两个工具不一定有，没有也不算失败：下次登录时桌面会重新扫描
pub(super) fn refresh_databases(registered: &Registered) {
    for file in &registered.files {
        let Some(directory) = file.parent() else { continue };
        let (tool, target) = match file.extension().and_then(|e| e.to_str()) {
            Some("desktop") => ("update-desktop-database", directory.to_path_buf()),
            Some("xml") => ("update-mime-database", directory.parent().unwrap_or(directory).to_path_buf()),
            _ => continue,
        };
        let _ = std::process::Command::new(tool).arg(target).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
}
