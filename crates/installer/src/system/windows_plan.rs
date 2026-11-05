//! Windows 的注册表里要写什么。只算不写，哪个系统上都能测

use std::path::Path;

use crate::engine::UNINSTALLER_NAME;
use crate::manifest::Manifest;

/// 注册表的值
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RegistryValue {
    Text(String),
    Number(u32),
}

/// 写一个值：项（`HKEY_CURRENT_USER` 下面）、值的名字（空的是默认值）、值
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RegistryWrite {
    pub key: String,
    pub name: String,
    pub value: RegistryValue,
}

/// 要写的和卸载时要删的
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RegistryPlan {
    pub writes: Vec<RegistryWrite>,
    pub keys: Vec<String>,
    pub values: Vec<(String, String)>,
}

impl RegistryPlan {
    fn text(&mut self, key: &str, name: &str, value: impl Into<String>) {
        self.writes.push(RegistryWrite { key: key.to_owned(), name: name.to_owned(), value: RegistryValue::Text(value.into()) });
    }

    fn number(&mut self, key: &str, name: &str, value: u32) {
        self.writes.push(RegistryWrite { key: key.to_owned(), name: name.to_owned(), value: RegistryValue::Number(value) });
    }
}

/// 卸载项、协议、文档类型。`directory` 是安装目录
pub(super) fn plan(manifest: &Manifest, directory: &Path) -> RegistryPlan {
    let mut plan = RegistryPlan::default();
    let in_dir = |relative: &str| directory.join(relative.replace('/', "\\")).display().to_string();
    let exe = in_dir(&manifest.executable);
    let icon = manifest.icon.as_deref().map_or_else(|| exe.clone(), in_dir);
    let open = format!("\"{exe}\" \"%1\"");
    let classes = "Software\\Classes";

    // "应用和功能"里的卸载项
    let uninstall = format!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}", manifest.identifier);
    let uninstaller = in_dir(UNINSTALLER_NAME);
    plan.text(&uninstall, "DisplayName", manifest.name.as_str());
    plan.text(&uninstall, "DisplayVersion", manifest.version.as_str());
    plan.text(&uninstall, "Publisher", manifest.publisher.as_str());
    plan.text(&uninstall, "InstallLocation", directory.display().to_string());
    plan.text(&uninstall, "DisplayIcon", icon.as_str());
    plan.text(&uninstall, "UninstallString", format!("\"{uninstaller}\""));
    plan.text(&uninstall, "QuietUninstallString", format!("\"{uninstaller}\" --silent"));
    plan.number(&uninstall, "NoModify", 1);
    plan.number(&uninstall, "NoRepair", 1);
    plan.number(&uninstall, "EstimatedSize", (manifest.total_size() / 1024).min(u64::from(u32::MAX)) as u32);
    plan.keys.push(uninstall);

    // 协议
    for scheme in &manifest.url_schemes {
        let key = format!("{classes}\\{scheme}");
        plan.text(&key, "", format!("URL:{}", manifest.name));
        plan.text(&key, "URL Protocol", "");
        plan.text(&format!("{key}\\DefaultIcon"), "", icon.as_str());
        plan.text(&format!("{key}\\shell\\open\\command"), "", open.as_str());
        plan.keys.push(key);
    }

    // 文档类型：每种扩展名一个 ProgID，扩展名的 OpenWithProgids 里登记它（"打开方式"里有这个应用）。
    // 只有应用自己的类型才把它设成默认的，系统已有的类型不抢
    for document in &manifest.documents {
        for extension in &document.extensions {
            let prog_id = format!("{}.{extension}", manifest.identifier);
            let key = format!("{classes}\\{prog_id}");
            plan.text(&key, "", document.name.as_str());
            plan.text(&format!("{key}\\DefaultIcon"), "", icon.as_str());
            plan.text(&format!("{key}\\shell\\open\\command"), "", open.as_str());
            plan.keys.push(key);
            let extension_key = format!("{classes}\\.{extension}");
            let open_with = format!("{extension_key}\\OpenWithProgids");
            plan.text(&open_with, &prog_id, "");
            plan.values.push((open_with, prog_id.clone()));
            if document.owner {
                plan.text(&extension_key, "", prog_id.as_str());
                plan.values.push((extension_key, String::new()));
            }
        }
    }
    plan
}
