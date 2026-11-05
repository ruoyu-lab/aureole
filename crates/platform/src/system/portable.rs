//! 不调用系统、各平台通用的纯逻辑：拼对话框的过滤器和命令行、文件网址的编码解码、通知的 XML、
//! 从注册表和 D-Bus 读回来的值怎么解释。放在这里是为了在任何平台上都能单元测试；
//! 某个平台用不到的函数在那个平台上不报"没用到"。
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::{DialogKind, DialogOptions, FileFilter};
use crate::NotificationResponse;

/// 去掉扩展名前面的点和空白，丢掉空的
pub fn clean_extensions(extensions: &[String]) -> Vec<String> {
    extensions.iter().map(|e| e.trim().trim_start_matches('.').to_owned()).filter(|e| !e.is_empty()).collect()
}

/// 对话框里显示的过滤器：去掉空的组；多于一组时在最前面加一组"所有支持的文件"（并集），默认选中它，
/// 和 macOS 上所有扩展名合在一起一致
pub fn dialog_filters(filters: &[FileFilter], all_name: &str) -> Vec<FileFilter> {
    let mut groups: Vec<FileFilter> = filters
        .iter()
        .map(|f| FileFilter { name: f.name.clone(), extensions: clean_extensions(&f.extensions) })
        .filter(|f| !f.extensions.is_empty())
        .collect();
    if groups.len() > 1 {
        let mut all: Vec<String> = Vec::new();
        for ext in groups.iter().flat_map(|f| &f.extensions) {
            if !all.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                all.push(ext.clone());
            }
        }
        groups.insert(0, FileFilter { name: all_name.to_owned(), extensions: all });
    }
    groups
}

/// Windows 文件对话框的过滤器（`COMDLG_FILTERSPEC` 的名字和模式）：`("文本 (*.txt; *.md)", "*.txt;*.md")`
pub fn windows_filter_specs(filters: &[FileFilter], all_name: &str) -> Vec<(String, String)> {
    dialog_filters(filters, all_name)
        .into_iter()
        .map(|f| {
            let patterns: Vec<String> = f.extensions.iter().map(|e| format!("*.{e}")).collect();
            let name = if f.name.trim().is_empty() { patterns.join("; ") } else { format!("{} ({})", f.name.trim(), patterns.join("; ")) };
            (name, patterns.join(";"))
        })
        .collect()
}

/// 不分大小写的通配符：`txt` → `*.[tT][xX][tT]`（门户和 zenity 的通配符分大小写）
pub fn case_insensitive_glob(extension: &str) -> String {
    let mut glob = String::from("*.");
    for c in extension.chars() {
        if c.is_ascii_alphabetic() {
            glob.push('[');
            glob.push(c.to_ascii_lowercase());
            glob.push(c.to_ascii_uppercase());
            glob.push(']');
        } else {
            glob.push(c);
        }
    }
    glob
}

/// XDG 门户的过滤器（D-Bus 类型 `a(sa(us))`）：每组是 (名字, [(0 表示通配符, 模式)])
pub fn portal_filters(filters: &[FileFilter], all_name: &str) -> Vec<(String, Vec<(u32, String)>)> {
    dialog_filters(filters, all_name)
        .into_iter()
        .map(|f| {
            let name = if f.name.trim().is_empty() { f.extensions.join(", ") } else { f.name.trim().to_owned() };
            (name, f.extensions.iter().map(|e| (0, case_insensitive_glob(e))).collect())
        })
        .collect()
}

/// 对话框没给标题时用的标题
pub fn default_title(kind: DialogKind, multiple: bool, chinese: bool) -> &'static str {
    match (kind, multiple, chinese) {
        (DialogKind::OpenFile, false, true) => "打开文件",
        (DialogKind::OpenFile, true, true) => "打开文件（可多选）",
        (DialogKind::OpenDirectory, _, true) => "选择文件夹",
        (DialogKind::Save, _, true) => "保存",
        (DialogKind::OpenFile, false, false) => "Open File",
        (DialogKind::OpenFile, true, false) => "Open Files",
        (DialogKind::OpenDirectory, _, false) => "Select Folder",
        (DialogKind::Save, _, false) => "Save",
    }
}

/// "所有支持的文件"这一组的名字
pub fn all_files_name(chinese: bool) -> &'static str {
    if chinese { "所有支持的文件" } else { "All supported files" }
}

/// 文件路径 → `file://` 网址：除了不需要转义的字符和 `/`，每个字节都写成 `%XX`
pub fn file_uri(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') { text.into_bytes() } else { format!("/{text}").into_bytes() }
    };
    let mut uri = String::from("file://");
    for b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// `file://` 网址 → 路径。主机名只接受空的和 `localhost`；不是 `file://`、编码不对时是 `None`
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let bytes = percent_decode(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        let text = String::from_utf8(bytes).ok()?;
        // file:///C:/a → C:/a
        let text = match text.as_bytes() {
            [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => text[1..].to_owned(),
            _ => text,
        };
        Some(PathBuf::from(text))
    }
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

/// 门户请求对象的路径：`/org/freedesktop/portal/desktop/request/发送方/令牌`，
/// 发送方是连接的唯一名去掉开头的 `:`、点换成下划线（`:1.42` → `1_42`）
pub fn portal_request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

/// 门户的父窗口标识：X11 窗口是 `x11:` 加十六进制的窗口号
pub fn portal_parent_x11(window: u64) -> String {
    format!("x11:{window:x}")
}

/// 用 zenity 弹对话框的参数
pub fn zenity_args(options: &DialogOptions, title: &str, all_name: &str) -> Vec<String> {
    let mut args = vec!["--file-selection".to_owned(), format!("--title={title}")];
    match options.kind {
        DialogKind::OpenDirectory => args.push("--directory".into()),
        DialogKind::Save => args.push("--save".into()),
        DialogKind::OpenFile => {
            if options.multiple {
                args.push("--multiple".into());
                args.push("--separator=\n".into());
            }
        }
    }
    let start = match (&options.directory, &options.file_name) {
        (Some(dir), Some(name)) if options.kind == DialogKind::Save => Some(dir.join(name).to_string_lossy().into_owned()),
        (None, Some(name)) if options.kind == DialogKind::Save => Some(name.clone()),
        // 结尾的 / 表示打开这个文件夹，而不是选中它
        (Some(dir), _) => Some(format!("{}/", dir.to_string_lossy().trim_end_matches('/'))),
        _ => None,
    };
    if let Some(start) = start {
        args.push(format!("--filename={start}"));
    }
    if options.kind != DialogKind::OpenDirectory {
        for f in dialog_filters(&options.filters, all_name) {
            let globs: Vec<String> = f.extensions.iter().map(|e| case_insensitive_glob(e)).collect();
            // zenity 的写法是"名字 | 模式 模式"，名字里不能有 |
            args.push(format!("--file-filter={} | {}", f.name.replace('|', "/").trim(), globs.join(" ")));
        }
    }
    args
}

/// 用 kdialog 弹对话框的参数。`parent` 是 X11 窗口号
pub fn kdialog_args(options: &DialogOptions, title: &str, parent: Option<u64>, all_name: &str) -> Vec<String> {
    let dir = options.directory.as_ref().map_or_else(|| ".".to_owned(), |d| d.to_string_lossy().into_owned());
    let mut args = Vec::new();
    match options.kind {
        DialogKind::OpenDirectory => args.extend(["--getexistingdirectory".to_owned(), dir]),
        DialogKind::OpenFile | DialogKind::Save => {
            let start = match (&options.file_name, options.kind) {
                (Some(name), DialogKind::Save) => Path::new(&dir).join(name).to_string_lossy().into_owned(),
                _ => dir,
            };
            let command = if options.kind == DialogKind::Save { "--getsavefilename" } else { "--getopenfilename" };
            args.extend([command.to_owned(), start]);
            // KDE 的写法：每行"模式 模式|名字"，不分大小写
            let filters: Vec<String> = dialog_filters(&options.filters, all_name)
                .iter()
                .map(|f| {
                    let patterns: Vec<String> = f.extensions.iter().map(|e| format!("*.{e}")).collect();
                    format!("{}|{}", patterns.join(" "), f.name.replace(['|', '\n'], " ").trim())
                })
                .collect();
            if !filters.is_empty() {
                args.push(filters.join("\n"));
            }
            if options.kind == DialogKind::OpenFile && options.multiple {
                args.extend(["--multiple".to_owned(), "--separate-output".to_owned()]);
            }
        }
    }
    args.extend(["--title".to_owned(), title.to_owned()]);
    if let Some(parent) = parent {
        args.extend(["--attach".to_owned(), parent.to_string()]);
    }
    args
}

/// zenity / kdialog 的输出：每行一个路径
pub fn parse_dialog_output(stdout: &str) -> Vec<PathBuf> {
    stdout.lines().map(|line| line.trim_end_matches('\r')).filter(|line| !line.is_empty()).map(PathBuf::from).collect()
}

/// XML 转义（通知的标题和正文）
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 不允许的控制字符去掉（换行、制表符保留）
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\t' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// 第 `index` 个动作按钮的标识（macOS 的类别、Windows 的激活参数都用它，点了之后原样送回来）
pub fn notification_action_id(index: usize) -> String {
    format!("aureole-action:{index}")
}

/// 送回来的标识是哪个按钮：动作按钮的标识，Linux 的 `default` 是点了通知本身。别的（系统自己的、划掉）是 `None`
pub fn notification_response(id: &str) -> Option<NotificationResponse> {
    match id {
        "default" | TOAST_CLICK => Some(NotificationResponse::Click),
        _ => id.strip_prefix("aureole-action:")?.parse().ok().map(NotificationResponse::Action),
    }
}

/// Windows 的通知被点了（不是按钮）时送回来的参数
const TOAST_CLICK: &str = "aureole-click";

/// Windows 通知（toast）的内容：标题一行、正文一行，可以带动作按钮。点通知本身和点按钮都激活前台，参数原样送回来
pub fn toast_xml(title: &str, body: &str, actions: &[String]) -> String {
    let buttons: String = actions
        .iter()
        .enumerate()
        .map(|(index, action)| format!("<action content=\"{}\" arguments=\"{}\" activationType=\"foreground\"/>", xml_escape(action), notification_action_id(index)))
        .collect();
    let actions = if buttons.is_empty() { String::new() } else { format!("<actions>{buttons}</actions>") };
    format!(
        "<toast launch=\"{TOAST_CLICK}\" activationType=\"foreground\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{actions}</toast>",
        xml_escape(title),
        xml_escape(body)
    )
}

/// Linux 通知（`org.freedesktop.Notifications.Notify`）的动作：两两一组（键、文字），`default` 是点通知本身
pub fn dbus_notification_actions(actions: &[String], default_label: &str) -> Vec<String> {
    let mut out = vec!["default".to_owned(), default_label.to_owned()];
    for (index, action) in actions.iter().enumerate() {
        out.push(notification_action_id(index));
        out.push(action.clone());
    }
    out
}

/// 注册表里的 `AppsUseLightTheme` / `SystemUsesLightTheme`（1 浅色、0 深色）→ 是不是深色；读不到时 `None`
pub fn dark_from_light_theme_value(value: Option<u32>) -> Option<bool> {
    value.map(|v| v == 0)
}

/// 注册表里的 `AccentColor`（0xAABBGGRR）→ RGB
pub fn rgb_from_abgr(value: u32) -> [u8; 3] {
    [value as u8, (value >> 8) as u8, (value >> 16) as u8]
}

/// 从 D-Bus 读回来的一项设置，换成和平台无关的形式
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    U32(u32),
    I32(i32),
    Bool(bool),
    F64(f64),
    Rgb([f64; 3]),
    Str(String),
    Other,
}

/// 桌面的外观设置（XDG 门户的 Settings 接口）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DesktopSettings {
    /// `org.freedesktop.appearance color-scheme`：0 没有偏好、1 深色、2 浅色
    pub color_scheme: Option<u32>,
    /// `org.freedesktop.appearance reduced-motion`：1 减少动态效果
    pub reduced_motion: Option<bool>,
    /// GNOME 的 `org.gnome.desktop.interface enable-animations`
    pub enable_animations: Option<bool>,
    /// KDE 的 `org.kde.kdeglobals.KDE AnimationDurationFactor`，0 是关掉动画
    pub animation_factor: Option<f64>,
    /// `org.freedesktop.appearance accent-color`
    pub accent: Option<[u8; 3]>,
    /// GNOME 的 `org.gnome.desktop.wm.preferences action-double-click-titlebar`
    pub title_bar_action: Option<crate::TitleBarAction>,
}

impl DesktopSettings {
    /// 什么都还没读到
    pub const EMPTY: Self =
        Self { color_scheme: None, reduced_motion: None, enable_animations: None, animation_factor: None, accent: None, title_bar_action: None };

    /// 记下一项设置，返回它是不是我们关心的
    pub fn apply(&mut self, namespace: &str, key: &str, value: &SettingValue) -> bool {
        let number = || match value {
            SettingValue::U32(v) => Some(*v as f64),
            SettingValue::I32(v) => Some(*v as f64),
            SettingValue::F64(v) => Some(*v),
            SettingValue::Str(s) => s.trim().parse().ok(),
            _ => None,
        };
        match (namespace, key) {
            ("org.freedesktop.appearance", "color-scheme") => self.color_scheme = number().map(|v| v as u32),
            ("org.freedesktop.appearance", "reduced-motion") => self.reduced_motion = number().map(|v| v != 0.0),
            ("org.freedesktop.appearance", "accent-color") => {
                self.accent = match value {
                    // 超出 0..=1 表示没有设置
                    SettingValue::Rgb(rgb) if rgb.iter().all(|c| (0.0..=1.0).contains(c)) => {
                        Some(rgb.map(|c| (c * 255.0).round() as u8))
                    }
                    _ => None,
                }
            }
            ("org.gnome.desktop.interface", "enable-animations") => {
                self.enable_animations = match value {
                    SettingValue::Bool(b) => Some(*b),
                    _ => None,
                }
            }
            ("org.kde.kdeglobals.KDE", "AnimationDurationFactor") => self.animation_factor = number(),
            ("org.gnome.desktop.wm.preferences", "action-double-click-titlebar") => {
                self.title_bar_action = match value {
                    SettingValue::Str(action) => Some(crate::TitleBarAction::from_gnome_setting(Some(action))),
                    _ => None,
                }
            }
            _ => return false,
        }
        true
    }

    /// 深色（1）、浅色（2）；没有偏好或没读到时 `None`
    pub fn dark(&self) -> Option<bool> {
        match self.color_scheme {
            Some(1) => Some(true),
            Some(2) => Some(false),
            _ => None,
        }
    }

    /// 任何一处说要减少动态效果
    pub fn is_motion_reduced(&self) -> bool {
        self.reduced_motion == Some(true) || self.enable_animations == Some(false) || self.animation_factor == Some(0.0)
    }
}

/// `GTK_THEME` 环境变量说的是不是深色主题（`Adwaita:dark`、`Foo-dark`）
pub fn gtk_theme_is_dark(theme: &str) -> bool {
    let theme = theme.to_ascii_lowercase();
    theme.ends_with(":dark") || theme.ends_with("-dark") || theme.ends_with("_dark")
}

/// 托盘所在的面板是不是深色的（深色时图标画成白色）。GNOME 的顶栏总是深色；其他桌面跟着系统外观，读不到时当作深色
pub fn linux_panel_dark(current_desktop: &str, dark: Option<bool>) -> bool {
    current_desktop.split(':').any(|d| d.eq_ignore_ascii_case("gnome")) || dark.unwrap_or(true)
}

/// RGBA（不预乘）→ StatusNotifierItem 要的 ARGB32（网络字节序）
pub fn argb_from_rgba(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect()
}

/// 语言环境变量 → 语言标签：按 gettext 的顺序看 `LANGUAGE`、`LC_ALL`、`LC_MESSAGES`、`LANG`，
/// `zh_CN.UTF-8` 写成 `zh-CN`；`C`、`POSIX` 和空的跳过
pub fn language_from_env(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"].into_iter().find_map(|key| {
        let value = get(key)?;
        // LANGUAGE 可以是用冒号隔开的列表；去掉编码和修饰（zh_CN.UTF-8@xxx）
        value.split(':').find_map(|item| {
            let tag = item.split(['.', '@']).next()?.trim().replace('_', "-");
            (!tag.is_empty() && tag != "C" && tag != "POSIX").then_some(tag)
        })
    })
}
