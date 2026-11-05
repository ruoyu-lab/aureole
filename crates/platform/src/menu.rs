//! 本机的菜单栏和托盘，实现后端的 [`MenuService`](crate::MenuService)、[`TrayService`](crate::TrayService)：
//!
//! - macOS、Windows：muda 和 tray-icon（`menu/muda_menu.rs`）。macOS 的菜单栏在屏幕顶部，Windows 的挂在每个有边框的窗口上。
//! - Linux：托盘走 D-Bus 上的 StatusNotifierItem 和 `com.canonical.dbusmenu`（ksni，纯 Rust，不链接原生界面工具包，
//!   `menu/sni.rs`）；没有菜单栏（窗口库的窗口挂不上那种菜单栏），[`crate::PlatformBackend::menu`] 是 `None`。
//!
//! 菜单对象只能在主线程上建和改，放在主线程的线程局部里。点击由系统的回调送来（可能在系统菜单的处理流程里，
//! Linux 上在托盘服务的线程里），只把菜单项编号交给调用方给的回调，调用方自己转回事件循环。
//!
//! 托盘图标由调用方画成位图：画成什么样（几种边长、什么颜色、是不是模板图标）问 [`icon_format`]，外观变了再问一次。
//!
//! 这些库都在 `menus` 特性后面：关掉时后端的 [`crate::PlatformBackend::menu`]、[`crate::PlatformBackend::tray`] 都是 `None`。

use crate::backend::TrayIconFormat;

#[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
mod muda_menu;
#[cfg(all(feature = "menus", target_os = "linux"))]
pub(crate) mod sni;

#[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
pub(crate) use muda_menu::{bar as menu_bar, tray};
#[cfg(all(feature = "menus", target_os = "windows"))]
pub(crate) use muda_menu::translate_accelerator;
#[cfg(all(feature = "menus", target_os = "linux"))]
pub(crate) use sni as tray;

/// 托盘图标此刻该画成什么样：macOS 是 36 像素见方的黑色模板图标（菜单栏图标 18pt，按 2 倍画；系统按菜单栏的深浅着色）；
/// Windows 是 32 像素（通知区图标 16 像素的 2 倍），Linux 是 32、64 像素两张（桌面按面板的高度挑），
/// 都没有模板图标，按任务栏、面板的深浅画成白色或接近黑色
pub(crate) fn icon_format() -> TrayIconFormat {
    if cfg!(target_os = "macos") {
        return TrayIconFormat { sizes: vec![36], rgb: [0, 0, 0], template: true };
    }
    let rgb = if crate::system::is_panel_dark().unwrap_or(true) { [255, 255, 255] } else { [0x1c, 0x1c, 0x1e] };
    let sizes = if cfg!(target_os = "linux") { vec![32, 64] } else { vec![32] };
    TrayIconFormat { sizes, rgb, template: false }
}

/// 系统自带的菜单项（关于、隐藏、退出……）按用户的语言写
#[cfg(all(feature = "menus", any(target_os = "macos", target_os = "windows")))]
fn chinese() -> bool {
    crate::preferred_language().is_some_and(|l| l.starts_with("zh"))
}
