//! Windows：真的写注册表、建快捷方式

use std::path::{Path, PathBuf};

use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink};
use windows::core::{GUID, HSTRING, Interface};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW};
use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

use super::windows_plan::{RegistryPlan, RegistryValue, plan};
use super::{Registered, safe_file_name};
use crate::Error;
use crate::engine::InstallOptions;
use crate::manifest::Manifest;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn system_error(what: &str, code: u32) -> Error {
    Error::System(format!("{what}失败：{}", std::io::Error::from_raw_os_error(code as i32)))
}

fn write_registry(plan: &RegistryPlan) -> Result<(), Error> {
    for write in &plan.writes {
        let (key, name) = (wide(&write.key), wide(&write.name));
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: 字符串以 0 结尾；句柄写进栈上的变量
        let status = unsafe {
            RegCreateKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut handle, std::ptr::null_mut())
        };
        if status != ERROR_SUCCESS {
            return Err(system_error(&format!("写注册表 {}", write.key), status));
        }
        // SAFETY: 句柄是上面打开的；数据的长度按字节算，文字带结尾的 0
        let status = unsafe {
            match &write.value {
                RegistryValue::Text(text) => {
                    let data = wide(text);
                    RegSetValueExW(handle, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
                }
                RegistryValue::Number(number) => RegSetValueExW(handle, name.as_ptr(), 0, REG_DWORD, std::ptr::from_ref(number).cast(), 4),
            }
        };
        // SAFETY: 句柄是上面打开的，之后不再用
        unsafe { RegCloseKey(handle) };
        if status != ERROR_SUCCESS {
            return Err(system_error(&format!("写注册表 {}", write.key), status));
        }
    }
    Ok(())
}

/// 删登记过的注册表项和值，出错不管
pub(super) fn remove_registry(keys: &[String], values: &[(String, String)]) {
    for key in keys {
        let key = wide(key);
        // SAFETY: 字符串以 0 结尾
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
    }
    for (key, name) in values {
        let (key, name) = (wide(key), wide(name));
        let mut handle: HKEY = std::ptr::null_mut();
        // SAFETY: 字符串以 0 结尾；句柄写进栈上的变量，用完关掉
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut handle) == ERROR_SUCCESS {
                RegDeleteValueW(handle, name.as_ptr());
                RegCloseKey(handle);
            }
        }
    }
    notify_associations_changed();
}

/// 告诉资源管理器协议和文档类型变了
fn notify_associations_changed() {
    // SAFETY: 这个事件不用带路径
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

/// 这个线程上的 COM：用完退出
struct Com(bool);

impl Com {
    fn init() -> Self {
        // SAFETY: 成对调用，见 Drop；已经初始化过时返回失败，那就不用退出
        Self(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 和 init 成对
            unsafe { CoUninitialize() };
        }
    }
}

fn known_folder(id: &GUID) -> Result<PathBuf, Error> {
    // SAFETY: 返回的字符串用 CoTaskMemFree 释放
    unsafe {
        let text = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).map_err(|e| Error::System(format!("找不到系统文件夹：{e}")))?;
        let path = text.to_string().map_err(|e| Error::System(format!("系统文件夹的路径不对：{e}")))?;
        CoTaskMemFree(Some(text.0 as *const _));
        Ok(PathBuf::from(path))
    }
}

/// 建快捷方式。带上应用标识：任务栏分组、系统通知都靠它（和框架读的是同一个属性）
fn create_shortcut(link: &Path, exe: &Path, directory: &Path, icon: &Path, manifest: &Manifest) -> Result<(), Error> {
    let failed = |e: windows::core::Error| Error::System(format!("建快捷方式 {} 失败：{e}", link.display()));
    // SAFETY: COM 在 `register` 里初始化好了；字符串在调用期间活着
    unsafe {
        let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(failed)?;
        shell.SetPath(&HSTRING::from(exe.as_os_str())).map_err(failed)?;
        shell.SetWorkingDirectory(&HSTRING::from(directory.as_os_str())).map_err(failed)?;
        shell.SetIconLocation(&HSTRING::from(icon.as_os_str()), 0).map_err(failed)?;
        shell.SetDescription(&HSTRING::from(manifest.name.as_str())).map_err(failed)?;
        let store: IPropertyStore = shell.cast().map_err(failed)?;
        let id = PROPVARIANT::from(manifest.identifier.as_str());
        store.SetValue(&PKEY_AppUserModel_ID, &id).map_err(failed)?;
        store.Commit().map_err(failed)?;
        let file: IPersistFile = shell.cast().map_err(failed)?;
        file.Save(&HSTRING::from(link.as_os_str()), true).map_err(failed)?;
    }
    Ok(())
}

pub(super) fn register(manifest: &Manifest, options: &InstallOptions) -> Result<Registered, Error> {
    let directory = &options.directory;
    let plan = plan(manifest, directory);
    let mut registered = Registered { registry_keys: plan.keys.clone(), registry_values: plan.values.clone(), ..Registered::default() };
    write_registry(&plan)?;
    notify_associations_changed();

    let _com = Com::init();
    let exe = directory.join(manifest.executable.replace('/', "\\"));
    let icon = manifest.icon.as_ref().map_or_else(|| exe.clone(), |icon| directory.join(icon.replace('/', "\\")));
    let link_name = format!("{}.lnk", safe_file_name(&manifest.name));
    let mut folders = Vec::new();
    if options.menu_shortcut {
        folders.push(FOLDERID_Programs);
    }
    if options.desktop_shortcut {
        folders.push(FOLDERID_Desktop);
    }
    for folder in folders {
        let link = known_folder(&folder)?.join(&link_name);
        create_shortcut(&link, &exe, directory, &icon, manifest)?;
        registered.files.push(link);
    }
    Ok(registered)
}
