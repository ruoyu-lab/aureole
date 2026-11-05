//! 应用能打开的协议和文档类型：从 `url-schemes`、`document-types` 读出来，生成
//! macOS 的 Info.plist 键（`CFBundleURLTypes`、`CFBundleDocumentTypes`、`UTExportedTypeDeclarations`），
//! 以及 Windows 安装程序要写的注册表项（.reg）、Linux 的 `.desktop` 文件和自定义类型的 MIME 声明。
//!
//! ```toml
//! [package.metadata.bundle]
//! url-schemes = ["myapp"]
//! document-types = [
//!     { name = "笔记", extensions = ["mynote"], role = "editor", icon = "assets/note.png" }, # 自定义类型：导出类型声明
//!     { name = "Markdown", extensions = ["md"], uti = "net.daringfireball.markdown", mime = "text/markdown" }, # 系统已有的类型
//! ]
//! ```

use std::path::PathBuf;

use serde_json::Value;

use crate::plist::{Dict, Value as Plist};

/// 应用对这种文档做什么（`CFBundleTypeRole`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Editor,
    Viewer,
    Shell,
    None,
}

/// 系统选默认应用时这个应用排第几（`LSHandlerRank`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rank {
    /// 这种类型是这个应用定义的
    Owner,
    Default,
    /// 只出现在"打开方式"里，不抢默认应用
    Alternate,
    None,
}

/// 一种文档类型
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentType {
    /// 给人看的名字
    pub name: String,
    /// 扩展名，不带点
    pub extensions: Vec<String>,
    /// 类型标识（UTI）
    pub uti: String,
    /// 由这个应用导出类型声明（自定义类型）；否则是系统已有的类型
    pub exported: bool,
    pub mime: Option<String>,
    pub role: Role,
    pub rank: Rank,
    /// 文档图标（PNG 或 .icns），打包成 Resources 里的 `DocumentIcon-<扩展名>.icns`
    pub icon: Option<PathBuf>,
}

impl DocumentType {
    /// 包里的文档图标文件名（不带 .icns）
    pub fn icon_name(&self) -> Option<String> {
        self.icon.as_ref().map(|_| format!("DocumentIcon-{}", self.extensions.first().map_or("doc", String::as_str)))
    }

    /// Linux 上用的 MIME 类型：写了就用；自定义类型没写时是 `application/x-<扩展名>`；系统类型没写时没有
    pub fn linux_mime(&self) -> Option<String> {
        self.mime.clone().or_else(|| self.exported.then(|| format!("application/x-{}", self.extensions[0])))
    }
}

/// 协议名：字母开头，字母、数字、`+`、`.`、`-`；不能占用系统常用的协议
pub fn parse_schemes(value: &Value) -> Result<Vec<String>, String> {
    let Some(list) = value.as_array() else {
        return if value.is_null() { Ok(Vec::new()) } else { Err("url-schemes 应该是协议名的列表，如 [\"myapp\"]".into()) };
    };
    let mut schemes = Vec::new();
    for item in list {
        let scheme = item.as_str().ok_or_else(|| format!("url-schemes 里的 {item} 应该是字符串"))?.trim_end_matches("://").to_ascii_lowercase();
        let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
        if !valid {
            return Err(format!("协议名 {scheme:?} 只能有英文字母、数字和 + . -，以字母开头"));
        }
        if ["http", "https", "file", "mailto", "ftp", "tel", "sms", "data"].contains(&scheme.as_str()) {
            return Err(format!("{scheme} 是系统常用的协议，不要占用；换一个应用自己的名字"));
        }
        if !schemes.contains(&scheme) {
            schemes.push(scheme);
        }
    }
    Ok(schemes)
}

/// `document-types` 表的列表。`identifier` 是包标识符（自定义类型的 UTI 用它开头），`dir` 是包的目录（图标的相对路径）
pub fn parse_document_types(value: &Value, identifier: &str, dir: &std::path::Path) -> Result<Vec<DocumentType>, String> {
    let Some(list) = value.as_array() else {
        return if value.is_null() { Ok(Vec::new()) } else { Err("document-types 应该是表的列表".into()) };
    };
    list.iter().map(|table| parse_document_type(table, identifier, dir)).collect()
}

fn parse_document_type(table: &Value, identifier: &str, dir: &std::path::Path) -> Result<DocumentType, String> {
    let text = |key: &str| table[key].as_str().map(str::to_owned);
    let name = text("name").ok_or("document-types 的每一项都要有 name")?;
    let extensions: Vec<String> = table["extensions"]
        .as_array()
        .ok_or_else(|| format!("文档类型 {name} 要有 extensions（扩展名的列表）"))?
        .iter()
        .map(|e| e.as_str().map(|e| e.trim_start_matches('.').to_ascii_lowercase()))
        .collect::<Option<_>>()
        .ok_or_else(|| format!("文档类型 {name} 的扩展名应该是字符串"))?;
    if extensions.is_empty() || extensions.iter().any(|e| e.is_empty() || e.contains(['.', '/', '\\', ' '])) {
        return Err(format!("文档类型 {name} 的扩展名不对：{extensions:?}（不带点，不能有空格和斜杠）"));
    }
    let given = text("uti");
    let exported = table["export"].as_bool().unwrap_or(given.is_none());
    let uti = given.unwrap_or_else(|| format!("{identifier}.{}", extensions[0]));
    let role = match text("role").as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("viewer") => Role::Viewer,
        Some("editor") => Role::Editor,
        Some("shell") => Role::Shell,
        Some("none") => Role::None,
        Some(other) => return Err(format!("文档类型 {name} 的 role 是 {other}，应该是 editor、viewer、shell 或 none")),
    };
    let rank = match text("rank").as_deref().map(str::to_ascii_lowercase).as_deref() {
        // 自定义类型是自己的；系统已有的类型只进"打开方式"，不改用户的默认应用
        None => {
            if exported {
                Rank::Owner
            } else {
                Rank::Alternate
            }
        }
        Some("owner") => Rank::Owner,
        Some("default") => Rank::Default,
        Some("alternate") => Rank::Alternate,
        Some("none") => Rank::None,
        Some(other) => return Err(format!("文档类型 {name} 的 rank 是 {other}，应该是 owner、default、alternate 或 none")),
    };
    Ok(DocumentType { name, extensions, uti, exported, mime: text("mime"), role, rank, icon: text("icon").map(|p| dir.join(p)) })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Editor => "Editor",
        Role::Viewer => "Viewer",
        Role::Shell => "Shell",
        Role::None => "None",
    }
}

fn rank_name(rank: Rank) -> &'static str {
    match rank {
        Rank::Owner => "Owner",
        Rank::Default => "Default",
        Rank::Alternate => "Alternate",
        Rank::None => "None",
    }
}

fn strings(items: &[String]) -> Plist {
    Plist::Array(items.iter().cloned().map(Plist::String).collect())
}

/// 写进 Info.plist 的键：协议、文档类型、自定义类型的导出声明
pub fn add_to_info_plist(d: &mut Dict, identifier: &str, schemes: &[String], documents: &[DocumentType]) {
    if !schemes.is_empty() {
        let mut url_type = Dict::default();
        url_type.set("CFBundleURLName", identifier);
        url_type.set("CFBundleTypeRole", "Viewer");
        url_type.set("CFBundleURLSchemes", strings(schemes));
        d.set("CFBundleURLTypes", Plist::Array(vec![Plist::Dict(url_type)]));
    }
    if documents.is_empty() {
        return;
    }
    let types = documents
        .iter()
        .map(|doc| {
            let mut t = Dict::default();
            t.set("CFBundleTypeName", doc.name.as_str());
            t.set("CFBundleTypeRole", role_name(doc.role));
            t.set("LSHandlerRank", rank_name(doc.rank));
            t.set("LSItemContentTypes", strings(std::slice::from_ref(&doc.uti)));
            if let Some(icon) = doc.icon_name() {
                t.set("CFBundleTypeIconFile", icon);
            }
            Plist::Dict(t)
        })
        .collect();
    d.set("CFBundleDocumentTypes", Plist::Array(types));
    let exported: Vec<Plist> = documents
        .iter()
        .filter(|doc| doc.exported)
        .map(|doc| {
            let mut tags = Dict::default();
            tags.set("public.filename-extension", strings(&doc.extensions));
            if let Some(mime) = &doc.mime {
                tags.set("public.mime-type", strings(std::slice::from_ref(mime)));
            }
            let mut t = Dict::default();
            t.set("UTTypeIdentifier", doc.uti.as_str());
            t.set("UTTypeDescription", doc.name.as_str());
            t.set("UTTypeConformsTo", strings(&["public.data".into(), "public.content".into()]));
            t.set("UTTypeTagSpecification", Plist::Dict(tags));
            if let Some(icon) = doc.icon_name() {
                t.set("UTTypeIconFile", icon);
            }
            Plist::Dict(t)
        })
        .collect();
    if !exported.is_empty() {
        d.set("UTExportedTypeDeclarations", Plist::Array(exported));
    }
}

/// .reg 文件里的字符串：反斜杠和引号转义
fn reg_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Windows 安装程序要写的注册表项（当前用户，不需要管理员权限），写成 .reg 文件的内容。
/// `exe` 是安装后可执行文件的完整路径。命令行写成 `"程序.exe" "%1"`：路径带空格也是一个参数
/// （框架解析参数时另外处理了 `"%1"` 遇上以反斜杠结尾的路径、和不带引号的 `%1` 两种情况）。
///
/// - 协议：`Software\Classes\<协议>`，`URL Protocol` 标记，`shell\open\command`。
/// - 文档：每种类型一个 ProgID（`包标识符.扩展名`），扩展名的 `OpenWithProgids` 里登记它（进"打开方式"）；
///   只有 `rank = owner`（自定义类型的默认）时才把扩展名的默认值设成它，系统已有的类型不改用户的默认应用。
pub fn windows_registry(name: &str, identifier: &str, exe: &str, schemes: &[String], documents: &[DocumentType]) -> String {
    let classes = "HKEY_CURRENT_USER\\Software\\Classes";
    let command = reg_string(&format!("\"{exe}\" \"%1\""));
    let icon = reg_string(&format!("\"{exe}\",0"));
    let mut out = String::from("Windows Registry Editor Version 5.00\r\n");
    let mut key = |path: String, values: &[(&str, String)]| {
        out.push_str(&format!("\r\n[{classes}\\{path}]\r\n"));
        for (name, value) in values {
            let name = if name.is_empty() { "@".to_owned() } else { reg_string(name) };
            out.push_str(&format!("{name}={value}\r\n"));
        }
    };
    for scheme in schemes {
        key(scheme.clone(), &[("", reg_string(&format!("URL:{name}"))), ("URL Protocol", reg_string(""))]);
        key(format!("{scheme}\\DefaultIcon"), &[("", icon.clone())]);
        key(format!("{scheme}\\shell\\open\\command"), &[("", command.clone())]);
    }
    let exe_name = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    for doc in documents {
        let prog_id = format!("{identifier}.{}", doc.extensions[0]);
        key(prog_id.clone(), &[("", reg_string(&doc.name))]);
        key(format!("{prog_id}\\DefaultIcon"), &[("", icon.clone())]);
        key(format!("{prog_id}\\shell\\open\\command"), &[("", command.clone())]);
        for ext in &doc.extensions {
            if doc.rank == Rank::Owner {
                key(format!(".{ext}"), &[("", reg_string(&prog_id))]);
            }
            key(format!(".{ext}\\OpenWithProgids"), &[(prog_id.as_str(), reg_string(""))]);
        }
    }
    let supported: Vec<(String, String)> = documents.iter().flat_map(|d| &d.extensions).map(|e| (format!(".{e}"), reg_string(""))).collect();
    if !supported.is_empty() {
        let values: Vec<(&str, String)> = supported.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        key(format!("Applications\\{exe_name}\\SupportedTypes"), &values);
    }
    out
}

/// `.desktop` 文件 Exec 里的一个参数：有特殊字符时加引号转义
fn desktop_quote(arg: &str) -> String {
    if arg.chars().any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c)) {
        let escaped: String = arg.chars().flat_map(|c| if "\"`$\\".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
        // .desktop 的字符串值里反斜杠本身还要再转义一次
        format!("\"{}\"", escaped.replace('\\', "\\\\"))
    } else {
        arg.to_owned()
    }
}

/// Linux 的 `.desktop` 文件（装到 `~/.local/share/applications/<包标识符>.desktop`）：
/// `Exec=程序 %U` 把网址和文件（`file://` 网址）作为参数传进来，`MimeType` 列出协议（`x-scheme-handler/…`）和文档类型
pub fn linux_desktop_entry(name: &str, identifier: &str, exec: &str, schemes: &[String], documents: &[DocumentType]) -> String {
    let mut mime: Vec<String> = schemes.iter().map(|s| format!("x-scheme-handler/{s}")).collect();
    mime.extend(documents.iter().filter_map(DocumentType::linux_mime));
    let mut out = format!(
        "[Desktop Entry]\nType=Application\nName={name}\nExec={} %U\nIcon={identifier}\nTerminal=false\n",
        desktop_quote(exec)
    );
    if !mime.is_empty() {
        out.push_str(&format!("MimeType={};\n", mime.join(";")));
    }
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// 自定义文档类型的 MIME 声明（shared-mime-info，装到 `~/.local/share/mime/packages/<包标识符>.xml`
/// 后运行 `update-mime-database ~/.local/share/mime`）；没有自定义类型时是 `None`
pub fn linux_mime_info(documents: &[DocumentType]) -> Option<String> {
    let exported: Vec<&DocumentType> = documents.iter().filter(|d| d.exported).collect();
    if exported.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n",
    );
    for doc in exported {
        out.push_str(&format!("  <mime-type type=\"{}\">\n", xml_escape(&doc.linux_mime().unwrap_or_default())));
        out.push_str(&format!("    <comment>{}</comment>\n", xml_escape(&doc.name)));
        for ext in &doc.extensions {
            out.push_str(&format!("    <glob pattern=\"*.{}\"/>\n", xml_escape(ext)));
        }
        out.push_str("  </mime-type>\n");
    }
    out.push_str("</mime-info>\n");
    Some(out)
}
