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
const AS_INVOKER_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
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

/// 把"不用提权"的清单嵌进安装程序（资源类型 24，编号 1）
#[cfg(windows)]
pub fn embed_manifest(exe: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};

    let number = std::ptr::without_provenance::<u16>;
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾；不删除已有的资源
    let update = unsafe { BeginUpdateResourceW(path.as_ptr(), 0) };
    if update.is_null() {
        return Err(format!("改不了 {} 的资源：{}", exe.display(), std::io::Error::last_os_error()));
    }
    // SAFETY: `update` 是上面打开的；数据是常量，长度填对了；之后不再用 `update`
    let finished = unsafe {
        let written = UpdateResourceW(update, number(24), number(1), 0, AS_INVOKER_MANIFEST.as_ptr().cast(), AS_INVOKER_MANIFEST.len() as u32) != 0;
        EndUpdateResourceW(update, if written { 0 } else { 1 }) != 0 && written
    };
    if !finished {
        return Err(format!("写 {} 的清单失败：{}", exe.display(), std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn embed_manifest(_exe: &std::path::Path) -> Result<(), String> {
    Ok(())
}
