//! 把图标嵌进 Windows 的可执行文件：资源管理器、任务栏、快捷方式显示的都是它。
//!
//! 用系统的更新资源接口改编好的文件，不用资源编译器，应用也不用写编译脚本。图标的数据和 .ico 一样是几张 PNG，
//! 一张放一个 `RT_ICON` 资源，一个 `RT_GROUP_ICON` 资源（编号 1，运行时按这个编号读）列出它们。

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::path::Path;

use crate::icon::IconSource;

/// 嵌进去的图标有这些尺寸
#[cfg(windows)]
const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];

/// 图标组资源的内容：头（类型 1、张数），每张一项（边长，256 写成 0；32 位色；数据的长度；图标资源的编号，从 1 开始）。
/// 和 .ico 的目录几乎一样，只是最后一项是编号，不是文件里的位置
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用，测试在每个系统上都跑"))]
fn group_entry_bytes(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (i, (size, png)) in entries.iter().enumerate() {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[side, side, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u16 + 1).to_le_bytes());
    }
    out
}

/// 把图标嵌进 `exe`。现成的 .icns 画不了时说明一下，不嵌（不影响打包）
#[cfg(windows)]
pub fn embed(exe: &Path, source: &IconSource) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    // 资源的类型和编号是"整数当指针传"，系统不会去读它指向的地方
    let number = std::ptr::without_provenance::<u16>;
    let (rt_icon, rt_group_icon) = (number(3), number(14));
    /// 不分语言（`MAKELANGID(LANG_NEUTRAL, SUBLANG_NEUTRAL)`）
    const LANGUAGE: u16 = 0;

    let entries = match SIZES.iter().map(|&size| Ok((size, crate::icon::png_at(source, size)?))).collect::<Result<Vec<_>, String>>() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("提示：{error}；可执行文件里不嵌图标");
            return Ok(());
        }
    };
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除已有的资源（别的资源留着）
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    let group = group_entry_bytes(&entries);
    // SAFETY: `update` 是上面打开的；数据在 EndUpdateResourceW 之前一直活着，长度填对了
    let written = unsafe {
        entries.iter().enumerate().all(|(i, (_, png))| UpdateResourceW(update, rt_icon, number(i + 1), LANGUAGE, png.as_ptr().cast(), png.len() as u32) != 0)
            && UpdateResourceW(update, rt_group_icon, number(1), LANGUAGE, group.as_ptr().cast(), group.len() as u32) != 0
    };
    // SAFETY: 写失败时放弃改动（第二个参数非 0），成功时写回文件；之后不再用 `update`
    let finished = unsafe { EndUpdateResourceW(update, if written { 0 } else { 1 }) } != 0;
    if !written || !finished {
        return Err(format!("写 {} 的图标失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// 只有 Windows 的可执行文件要嵌（资源的格式是 Windows 的）
#[cfg(not(windows))]
pub fn embed(_exe: &std::path::Path, _source: &IconSource) -> Result<(), String> {
    Ok(())
}

/// 系统看文件名里有 setup、install 就当成要管理员权限的安装程序，弹出提权窗口。清单里写明不用提权，它就不猜了。
/// 这个安装程序只装给当前用户，不需要管理员权限
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
pub const AS_INVOKER_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

/// 把清单嵌进可执行文件（资源类型 24，编号 1，不分语言）：安装程序嵌"不用提权"的（[`AS_INVOKER_MANIFEST`]），
/// 用到 CEF 视图的应用嵌 CEF 要的（[`crate::webview::MANIFEST`]）
#[cfg(windows)]
pub fn embed_manifest(exe: &Path, manifest: &str) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    let number = std::ptr::without_provenance::<u16>;
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除已有的资源
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: `update` 是上面打开的；清单在 EndUpdateResourceW 之前一直活着，长度填对了；之后不再用 `update`
    let finished = unsafe {
        let written = UpdateResourceW(update, number(24), number(1), 0, manifest.as_ptr().cast(), manifest.len() as u32) != 0;
        EndUpdateResourceW(update, if written { 0 } else { 1 }) != 0 && written
    };
    if !finished {
        return Err(format!("写 {} 的清单失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn embed_manifest(_exe: &std::path::Path, _manifest: &str) -> Result<(), String> {
    Ok(())
}

/// 包标识符资源的名字：运行时（`aureole_platform::system::embedded_app_id`）按这个名字读，两边要一样
#[cfg(windows)]
pub const APP_ID_RESOURCE: &str = "AUREOLE_APP_ID";

/// 把包标识符写进可执行文件（`RT_RCDATA` 资源，名字 [`APP_ID_RESOURCE`]，内容是 UTF-8）。运行时在建第一个窗口之前读它，
/// 声明成进程的应用标识：任务栏分组、系统通知认的都是它，和安装程序建的开始菜单快捷方式上写的一样，不管应用是怎么启动的
#[cfg(windows)]
pub fn embed_identifier(exe: &Path, identifier: &str) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    let rt_rcdata = std::ptr::without_provenance::<u16>(10);
    let name: Vec<u16> = APP_ID_RESOURCE.encode_utf16().chain(Some(0)).collect();
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除已有的资源
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: `update` 是上面打开的；名字以 0 结尾，数据在 EndUpdateResourceW 之前一直活着，长度填对了；之后不再用 `update`
    let finished = unsafe {
        let written = UpdateResourceW(update, rt_rcdata, name.as_ptr(), 0, identifier.as_ptr().cast(), identifier.len() as u32) != 0;
        EndUpdateResourceW(update, if written { 0 } else { 1 }) != 0 && written
    };
    if !finished {
        return Err(format!("写 {} 的包标识符失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn embed_identifier(_exe: &std::path::Path, _identifier: &str) -> Result<(), String> {
    Ok(())
}

// ---------- 版本信息、去掉已有的资源 ----------

/// 版本信息里的文字：资源管理器的"详细信息"、任务管理器里进程的描述显示它们
pub struct VersionInfo<'a> {
    /// 应用名（文件说明、产品名称）
    pub name: &'a str,
    /// 只有数字和点的版本号（文件版本、产品版本的数字部分）
    pub version: &'a str,
    /// 写的什么就是什么的版本号（显示的版本文字）
    pub display_version: &'a str,
    /// 发布者（公司）
    pub company: &'a str,
    /// 版权
    pub copyright: &'a str,
    /// 可执行文件名（原始文件名）
    pub file_name: &'a str,
}

/// `1.2.3` → 文件版本的高、低两个 32 位数（每段 16 位，最多四段）
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
fn version_numbers(version: &str) -> (u32, u32) {
    let mut parts = version.split('.').map(|p| p.parse::<u16>().unwrap_or(0)).chain(std::iter::repeat(0));
    let mut next = || u32::from(parts.next().unwrap_or(0));
    let (a, b, c, d) = (next(), next(), next(), next());
    ((a << 16) | b, (c << 16) | d)
}

#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect()
}

#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// 版本信息里的一个节点：长度、值的长度、类型（0 二进制、1 文字）、键，对齐到 4 字节，值，每个子节点前对齐到 4 字节
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
fn version_node(key: &str, kind: u16, value: &[u8], value_length: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = vec![0u8; 6];
    buf.extend(utf16z(key));
    pad4(&mut buf);
    buf.extend_from_slice(value);
    for child in children {
        pad4(&mut buf);
        buf.extend_from_slice(child);
    }
    let length = u16::try_from(buf.len()).unwrap_or(u16::MAX);
    buf[0..2].copy_from_slice(&length.to_le_bytes());
    buf[2..4].copy_from_slice(&value_length.to_le_bytes());
    buf[4..6].copy_from_slice(&kind.to_le_bytes());
    buf
}

/// 版本信息资源（`VS_VERSIONINFO`）：固定部分（文件版本、产品版本、是应用程序），英文（美国）、Unicode 的一组文字，
/// 和对应的语言声明
#[cfg_attr(not(windows), allow(dead_code, reason = "只有打 Windows 的包时才用"))]
fn version_resource(info: &VersionInfo) -> Vec<u8> {
    let (high, low) = version_numbers(info.version);
    let fixed: Vec<u8> = [
        0xFEEF_04BD, // 签名
        0x0001_0000, // 结构的版本
        high,
        low,
        high,
        low,
        0x3F,      // 有效的标志位
        0,         // 标志：正式版
        0x0004_0004, // 系统：Windows NT
        1,         // 类型：应用程序
        0,
        0,
        0,
    ]
    .iter()
    .flat_map(|n: &u32| n.to_le_bytes())
    .collect();
    let text = |key: &str, value: &str| {
        let value_length = u16::try_from(value.encode_utf16().count() + 1).unwrap_or(u16::MAX);
        version_node(key, 1, &utf16z(value), value_length, &[])
    };
    let strings = [
        text("CompanyName", info.company),
        text("FileDescription", info.name),
        text("FileVersion", info.display_version),
        text("InternalName", info.name),
        text("LegalCopyright", info.copyright),
        text("OriginalFilename", info.file_name),
        text("ProductName", info.name),
        text("ProductVersion", info.display_version),
    ];
    let table = version_node("040904B0", 1, &[], 0, &strings);
    let string_info = version_node("StringFileInfo", 1, &[], 0, &[table]);
    let translation = version_node("Translation", 0, &[0x09, 0x04, 0xB0, 0x04], 4, &[]);
    let var_info = version_node("VarFileInfo", 1, &[], 0, &[translation]);
    version_node("VS_VERSION_INFO", 0, &fixed, 52, &[string_info, var_info])
}

/// 把版本信息写进 `exe`（资源类型 16，编号 1，不分语言）：先去掉已有的版本信息（各种语言的），不然两份并存，显示哪份说不准
#[cfg(windows)]
pub fn embed_version(exe: &Path, info: &VersionInfo) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    remove_resources(exe, &[RT_VERSION])?;
    let data = version_resource(info);
    let number = std::ptr::without_provenance::<u16>;
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除已有的资源
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: `update` 是上面打开的；数据在 EndUpdateResourceW 之前一直活着，长度填对了；之后不再用 `update`
    let finished = unsafe {
        let written = UpdateResourceW(update, number(usize::from(RT_VERSION)), number(1), 0, data.as_ptr().cast(), data.len() as u32) != 0;
        EndUpdateResourceW(update, if written { 0 } else { 1 }) != 0 && written
    };
    if !finished {
        return Err(format!("写 {} 的版本信息失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn embed_version(_exe: &std::path::Path, _info: &VersionInfo) -> Result<(), String> {
    Ok(())
}

/// 资源的类型：图标、图标组、版本信息、清单
pub const RT_ICON: u16 = 3;
pub const RT_GROUP_ICON: u16 = 14;
pub const RT_VERSION: u16 = 16;
pub const RT_MANIFEST: u16 = 24;

/// 资源的编号或名字
#[cfg(windows)]
enum ResourceName {
    Id(u16),
    Text(Vec<u16>),
}

/// 去掉 `exe` 里这几类资源（每个编号、每种语言的都去掉）：拿别人的程序（CEF 的 `bootstrap.exe`）改成应用自己的时，
/// 先去掉它的图标、版本信息、清单，再写应用的，不然新旧两份并存
#[cfg(windows)]
pub fn remove_resources(exe: &Path, types: &[u16]) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    let found = list_resources(exe, types)?;
    if found.is_empty() {
        return Ok(());
    }
    let number = std::ptr::without_provenance::<u16>;
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除别的资源
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: `update` 是上面打开的；数据传空指针、长度 0 是删除这一项；名字的字符串以 0 结尾、在调用期间活着
    let finished = unsafe {
        let removed = found.iter().all(|(kind, name, language)| {
            let name = match name {
                ResourceName::Id(id) => number(usize::from(*id)),
                ResourceName::Text(text) => text.as_ptr(),
            };
            UpdateResourceW(update, number(usize::from(*kind)), name, *language, std::ptr::null(), 0) != 0
        });
        EndUpdateResourceW(update, if removed { 0 } else { 1 }) != 0 && removed
    };
    if !finished {
        return Err(format!("去掉 {} 原来的资源失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// `exe` 里这几类资源：类型、编号或名字、语言
#[cfg(windows)]
fn list_resources(exe: &Path, types: &[u16]) -> Result<Vec<(u16, ResourceName, u16)>, String> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{
        EnumResourceLanguagesW, EnumResourceNamesW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW,
    };
    use windows_sys::core::{BOOL, PCWSTR};

    unsafe extern "system" fn name(_module: *mut core::ffi::c_void, _kind: PCWSTR, name: PCWSTR, found: isize) -> BOOL {
        // SAFETY: `found` 是下面那个 Vec 的地址，枚举期间有效；不是整数编号时名字是以 0 结尾的字符串，回调期间有效
        unsafe {
            let found = &mut *(found as *mut Vec<ResourceName>);
            let value = name as usize;
            if value >> 16 == 0 {
                found.push(ResourceName::Id(value as u16));
            } else {
                let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                found.push(ResourceName::Text(std::slice::from_raw_parts(name, len + 1).to_vec()));
            }
        }
        1
    }
    unsafe extern "system" fn language(_module: *mut core::ffi::c_void, _kind: PCWSTR, _name: PCWSTR, language: u16, found: isize) -> BOOL {
        // SAFETY: `found` 是下面那个 Vec 的地址，枚举期间有效
        unsafe { (*(found as *mut Vec<u16>)).push(language) };
        1
    }

    let number = std::ptr::without_provenance::<u16>;
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；按数据文件打开，不执行里面的代码
    let module = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE) };
    if module.is_null() {
        return Err(format!("打不开 {}：{}", exe.display(), std::io::Error::last_os_error()));
    }
    let mut out = Vec::new();
    for &kind in types {
        let mut names: Vec<ResourceName> = Vec::new();
        // SAFETY: `module` 是上面打开的；回调只在枚举期间调用，写的是 `names`；没有这一类时返回失败，不管它
        unsafe { EnumResourceNamesW(module, number(usize::from(kind)), Some(name), &mut names as *mut _ as isize) };
        for resource in names {
            let mut languages: Vec<u16> = Vec::new();
            let name_ptr = match &resource {
                ResourceName::Id(id) => number(usize::from(*id)),
                ResourceName::Text(text) => text.as_ptr(),
            };
            // SAFETY: 同上，写的是 `languages`
            unsafe { EnumResourceLanguagesW(module, number(usize::from(kind)), name_ptr, Some(language), &mut languages as *mut _ as isize) };
            for language in languages {
                let name = match &resource {
                    ResourceName::Id(id) => ResourceName::Id(*id),
                    ResourceName::Text(text) => ResourceName::Text(text.clone()),
                };
                out.push((kind, name, language));
            }
        }
    }
    // SAFETY: 上面打开的，之后不再用
    unsafe { FreeLibrary(module) };
    Ok(out)
}

#[cfg(not(windows))]
pub fn remove_resources(_exe: &std::path::Path, _types: &[u16]) -> Result<(), String> {
    Ok(())
}
