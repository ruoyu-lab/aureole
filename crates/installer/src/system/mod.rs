//! 在系统里登记：快捷方式、卸载项、协议、文档类型。
//!
//! 只做"装给当前用户"这一种，不要管理员权限：注册表只写 `HKEY_CURRENT_USER`，文件只写用户自己的目录。
//! 要写到系统级别的（所有用户、服务）用 [`Step`](crate::Step) 自己做，提权也由它决定。
//!
//! 要写什么由纯函数算出来（`windows_plan`、`linux`），在哪个系统上都能测；真正调系统接口的只有很薄一层。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::engine::InstallOptions;
use crate::manifest::Manifest;
use crate::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_plan;

/// 在系统里登记过的，卸载时照着撤销
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registered {
    /// 快捷方式、`.desktop`、图标这些文件
    pub files: Vec<PathBuf>,
    /// 要整个删掉的注册表项（`HKEY_CURRENT_USER` 下面）
    pub registry_keys: Vec<String>,
    /// 要删掉的注册表值：（项，值的名字）。别的应用也可能在用这一项，不能整个删
    pub registry_values: Vec<(String, String)>,
}

impl Registered {
    pub(crate) fn to_json(&self) -> Value {
        let files: Vec<String> = self.files.iter().map(|f| f.display().to_string()).collect();
        let values: Vec<Value> = self.registry_values.iter().map(|(key, name)| json!([key, name])).collect();
        json!({ "files": files, "registry_keys": self.registry_keys, "registry_values": values })
    }

    pub(crate) fn from_json(value: &Value) -> Self {
        let strings = |key: &str| value[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        let registry_values = value["registry_values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned())))
            .collect();
        Self { files: strings("files").into_iter().map(PathBuf::from).collect(), registry_keys: strings("registry_keys"), registry_values }
    }
}

/// 名字里系统不让用的字符换掉
pub(crate) fn safe_file_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '-' } else { c }).collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_owned();
    if cleaned.is_empty() { "app".to_owned() } else { cleaned }
}

/// 默认装到哪里，都是用户自己的目录，不要管理员权限：
/// Windows 是 `%LOCALAPPDATA%\Programs\<应用名>`，Linux 是 `~/.local/share/<包标识符>`，macOS 是 `~/Applications/<应用名>`
pub fn default_directory(manifest: &Manifest) -> PathBuf {
    default_directory_on(std::env::consts::OS, manifest, &|name| std::env::var_os(name).map(PathBuf::from))
}

fn default_directory_on(os: &str, manifest: &Manifest, env: &dyn Fn(&str) -> Option<PathBuf>) -> PathBuf {
    let name = safe_file_name(&manifest.name);
    let home = || env("USERPROFILE").or_else(|| env("HOME")).unwrap_or_else(std::env::temp_dir);
    match os {
        "windows" => env("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local")).join("Programs").join(name),
        "linux" => env("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local").join("share")).join(&manifest.identifier),
        _ => home().join("Applications").join(name),
    }
}

/// 登记
pub(crate) fn register(manifest: &Manifest, options: &InstallOptions) -> Result<Registered, Error> {
    #[cfg(windows)]
    {
        windows::register(manifest, options)
    }
    #[cfg(target_os = "linux")]
    {
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
        match data {
            Some(data) => linux::register(&data, manifest, options),
            None => Err(Error::System("找不到用户目录（HOME）".into())),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (manifest, options);
        Ok(Registered::default())
    }
}

/// 撤销登记。出错也继续，能撤多少撤多少：卸载不该因为少了一个快捷方式就停下
pub(crate) fn unregister(registered: &Registered) {
    for file in &registered.files {
        let _ = std::fs::remove_file(file);
    }
    #[cfg(windows)]
    windows::remove_registry(&registered.registry_keys, &registered.registry_values);
    #[cfg(target_os = "linux")]
    linux::refresh_databases(registered);
}

/// 升级：上一个版本登记了、这个版本没有的（比如这次没勾桌面快捷方式）
pub(crate) fn remove_stale(previous: &Registered, current: &Registered) {
    let stale = Registered {
        files: previous.files.iter().filter(|f| !current.files.contains(f)).cloned().collect(),
        registry_keys: previous.registry_keys.iter().filter(|k| !current.registry_keys.contains(k)).cloned().collect(),
        registry_values: previous.registry_values.iter().filter(|v| !current.registry_values.contains(v)).cloned().collect(),
    };
    unregister(&stale);
}
