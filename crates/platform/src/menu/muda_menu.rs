//! macOS、Windows 的菜单栏和托盘：muda 和 tray-icon（见 [`super`]）。
//!
//! - 菜单项在 muda 里的编号是"属于谁"的前缀加调用方的编号（菜单栏 `b`、托盘菜单 `t`），点击时 muda 的全局回调按前缀
//!   交给菜单栏或托盘的回调；系统补上的项（关于、服务、隐藏……）用 muda 自己的编号，点了由系统处理。
//! - macOS 的应用菜单：菜单栏的第一项不是应用菜单时补上一个；系统项按用户的语言写，带上应用名。
//! - Windows 的应用菜单只有关于、应用自己的项、退出；退出送回 [`MenuItemId::QUIT`]，调用方按自己的方式退出。
//!   菜单栏挂在每个有边框的窗口上，快捷键要在分发消息前翻译（[`translate_accelerator`]）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, Once};

use muda::accelerator::Accelerator;
use muda::{CheckMenuItem, IsMenuItem, MenuId, PredefinedMenuItem};

use crate::backend::{
    MenuEntry, MenuItemId, MenuItemModel, MenuItemUpdate, OnMenuItem, OnTrayClick, PlatformError, SubmenuModel,
};

/// 菜单项属于菜单栏还是托盘菜单（两边的编号可以重）
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Bar,
    Tray,
}

/// Windows 应用菜单里补上的"退出"在 muda 里的编号
const QUIT: &str = "quit";

fn muda_id(owner: Owner, id: MenuItemId) -> MenuId {
    let prefix = match owner {
        Owner::Bar => 'b',
        Owner::Tray => 't',
    };
    MenuId::new(format!("{prefix}{}", id.0))
}

/// muda 的编号 → 属于谁、调用方的编号；系统补上的项是 `None`
fn parse(id: &str) -> Option<(Owner, MenuItemId)> {
    if id == QUIT {
        return Some((Owner::Bar, MenuItemId::QUIT));
    }
    let owner = match id.chars().next()? {
        'b' => Owner::Bar,
        't' => Owner::Tray,
        _ => return None,
    };
    id[1..].parse().ok().map(|n| (owner, MenuItemId(n)))
}

fn error(error: muda::Error) -> PlatformError {
    PlatformError::System(error.to_string().into())
}

/// 能改的菜单项
enum Item {
    Plain(muda::MenuItem),
    Check(CheckMenuItem),
}

impl Item {
    fn update(&self, update: &MenuItemUpdate) {
        match self {
            Item::Plain(item) => {
                if let Some(text) = &update.text {
                    item.set_text(text);
                }
                if let Some(disabled) = update.disabled {
                    item.set_enabled(!disabled);
                }
            }
            Item::Check(item) => {
                if let Some(text) = &update.text {
                    item.set_text(text);
                }
                if let Some(disabled) = update.disabled {
                    item.set_enabled(!disabled);
                }
                if let Some(checked) = update.checked {
                    item.set_checked(checked);
                }
            }
        }
    }
}

/// 主线程上的菜单对象
#[derive(Default)]
struct State {
    bar: Option<muda::Menu>,
    bar_items: HashMap<MenuItemId, Item>,
    /// 挂着菜单栏的窗口（Windows）
    #[cfg(target_os = "windows")]
    windows: Vec<isize>,
    /// 菜单栏按什么深浅画（窗口 → 是不是深色，Windows）：设过的才有，没变时不再设
    #[cfg(target_os = "windows")]
    themes: HashMap<isize, bool>,
    tray: Option<tray_icon::TrayIcon>,
    tray_items: HashMap<MenuItemId, Item>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// 点击回调：muda、tray-icon 的全局回调（可能不在主线程）转给它们
struct Handlers {
    bar: Option<OnMenuItem>,
    tray: Option<OnMenuItem>,
    tray_click: Option<OnTrayClick>,
}

static HANDLERS: Mutex<Handlers> = Mutex::new(Handlers { bar: None, tray: None, tray_click: None });

fn handlers() -> MutexGuard<'static, Handlers> {
    HANDLERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 第一次装菜单或托盘时登记 muda、tray-icon 的全局回调（整个进程一次）
fn listen() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        muda::MenuEvent::set_event_handler(Some(|event: muda::MenuEvent| {
            let Some((owner, id)) = parse(&event.id.0) else { return };
            let handler = {
                let handlers = handlers();
                if owner == Owner::Bar { handlers.bar.clone() } else { handlers.tray.clone() }
            };
            if let Some(handler) = handler {
                handler(id);
            }
        }));
        tray_icon::TrayIconEvent::set_event_handler(Some(|event: tray_icon::TrayIconEvent| {
            if let tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            } = event
            {
                let handler = handlers().tray_click.clone();
                if let Some(handler) = handler {
                    handler();
                }
            }
        }));
    });
}

/// 菜单只能在主线程上建（macOS 的 AppKit 要求）
fn main_thread() -> Result<(), PlatformError> {
    #[cfg(target_os = "macos")]
    if objc2::MainThreadMarker::new().is_none() {
        return Err(PlatformError::System("菜单只能在主线程上建".into()));
    }
    Ok(())
}

fn build(entry: MenuEntry, owner: Owner, items: &mut HashMap<MenuItemId, Item>) -> Box<dyn IsMenuItem> {
    match entry {
        MenuEntry::Separator => Box::new(PredefinedMenuItem::separator()),
        MenuEntry::Submenu(submenu) => Box::new(build_submenu(submenu, owner, items)),
        MenuEntry::Item(MenuItemModel { id, text, shortcut, disabled, checked }) => {
            let accelerator = shortcut.as_deref().and_then(|s| match s.parse::<Accelerator>() {
                Ok(accelerator) => Some(accelerator),
                Err(error) => {
                    tracing::warn!("菜单快捷键 {s:?} 无法识别：{error}");
                    None
                }
            });
            match checked {
                Some(checked) => {
                    let item = CheckMenuItem::with_id(muda_id(owner, id), text, !disabled, checked, accelerator);
                    items.insert(id, Item::Check(item.clone()));
                    Box::new(item)
                }
                None => {
                    let item = muda::MenuItem::with_id(muda_id(owner, id), text, !disabled, accelerator);
                    items.insert(id, Item::Plain(item.clone()));
                    Box::new(item)
                }
            }
        }
    }
}

fn build_submenu(submenu: SubmenuModel, owner: Owner, items: &mut HashMap<MenuItemId, Item>) -> muda::Submenu {
    let native = muda::Submenu::new(&submenu.title, true);
    let custom: Vec<Box<dyn IsMenuItem>> = submenu.entries.into_iter().map(|entry| build(entry, owner, items)).collect();
    if !submenu.is_application_menu {
        for item in &custom {
            let _ = native.append(item.as_ref());
        }
        return native;
    }
    // 系统自带的几项按用户的语言写，带上应用名
    let name = &submenu.title;
    let chinese = super::chinese();
    let label = |zh: String, en: String| if chinese { zh } else { en };
    let about = muda::AboutMetadata { name: Some(name.clone()), ..Default::default() };
    let about_text = label(format!("关于 {name}"), format!("About {name}"));
    let _ = native.append(&PredefinedMenuItem::about(Some(&about_text), Some(about)));
    let _ = native.append(&PredefinedMenuItem::separator());
    for item in &custom {
        let _ = native.append(item.as_ref());
    }
    if !custom.is_empty() {
        let _ = native.append(&PredefinedMenuItem::separator());
    }
    if cfg!(target_os = "windows") {
        // Windows 没有服务、隐藏其他这些项。退出送回调用方（关窗口、结束事件循环），不用 muda 的（它直接发 WM_QUIT）
        let _ = native.append(&muda::MenuItem::with_id(QUIT, label("退出".into(), "Exit".into()), true, None));
    } else {
        let _ = native.append(&PredefinedMenuItem::services(Some(&label("服务".into(), "Services".into()))));
        let _ = native.append(&PredefinedMenuItem::separator());
        let _ = native.append(&PredefinedMenuItem::hide(Some(&label(format!("隐藏 {name}"), format!("Hide {name}")))));
        let _ = native.append(&PredefinedMenuItem::hide_others(Some(&label("隐藏其他".into(), "Hide Others".into()))));
        let _ = native.append(&PredefinedMenuItem::show_all(Some(&label("全部显示".into(), "Show All".into()))));
        let _ = native.append(&PredefinedMenuItem::separator());
        let _ = native.append(&PredefinedMenuItem::quit(Some(&label(format!("退出 {name}"), format!("Quit {name}")))));
    }
    native
}

/// 窗口的 HWND
#[cfg(target_os = "windows")]
fn hwnd(window: &dyn raw_window_handle::HasWindowHandle) -> Option<isize> {
    match window.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// 菜单栏
pub(crate) mod bar {
    use std::collections::HashMap;

    use raw_window_handle::HasWindowHandle;

    use super::{Owner, STATE, build, error, handlers, listen, main_thread};
    use crate::backend::{MenuEntry, MenuItemId, MenuItemUpdate, MenuModel, OnMenuItem, PlatformError, SubmenuModel};

    /// 装上菜单栏，换掉之前的。macOS：第一项不是应用菜单时补上一个（名字是 `app_name`），装在屏幕顶部；
    /// Windows：已经挂着菜单栏的窗口换上新的，之后的窗口由 [`attach`] 挂上
    pub(crate) fn set(menu: MenuModel, app_name: &str, on_item: OnMenuItem) -> Result<(), PlatformError> {
        main_thread()?;
        listen();
        let mut entries = menu.entries;
        if cfg!(target_os = "macos") && !matches!(entries.first(), Some(MenuEntry::Submenu(s)) if s.is_application_menu) {
            entries.insert(0, MenuEntry::Submenu(SubmenuModel { title: app_name.to_owned(), entries: Vec::new(), is_application_menu: true }));
        }
        let bar = muda::Menu::new();
        let mut items = HashMap::new();
        for entry in entries {
            bar.append(build(entry, Owner::Bar, &mut items).as_ref()).map_err(error)?;
        }
        #[cfg(target_os = "macos")]
        bar.init_for_nsapp();
        handlers().bar = Some(on_item);
        #[cfg(target_os = "windows")]
        let windows = STATE.with(|s| s.borrow().windows.clone());
        let old = STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.bar_items = items;
            s.bar.replace(bar.clone())
        });
        #[cfg(target_os = "windows")]
        for hwnd in windows {
            if let Some(old) = &old {
                // SAFETY: 挂着菜单栏的窗口还没销毁（关窗口前 detach 会把它从表里去掉）
                let _ = unsafe { old.remove_for_hwnd(hwnd) };
            }
            // SAFETY: 同上，窗口还在
            if let Err(error) = unsafe { bar.init_for_hwnd(hwnd) } {
                tracing::error!("菜单栏挂到窗口上失败：{error}");
            }
            // 新的菜单栏按原来设过的深浅画
            if let Some(dark) = STATE.with(|s| s.borrow().themes.get(&hwnd).copied()) {
                let theme = if dark { muda::MenuTheme::Dark } else { muda::MenuTheme::Light };
                // SAFETY: 同上，窗口还在，刚挂上这个菜单栏
                let _ = unsafe { bar.set_theme_for_hwnd(hwnd, theme) };
            }
        }
        drop(old);
        Ok(())
    }

    /// 改菜单栏里的一项
    pub(crate) fn update(id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        let found = STATE.with(|s| s.borrow().bar_items.get(&id).map(|item| item.update(&update)).is_some());
        if found { Ok(()) } else { Err(PlatformError::System(format!("菜单栏里没有第 {} 项", id.0).into())) }
    }

    /// 新窗口建好了：Windows 上把菜单栏挂上去（macOS 的菜单栏在屏幕顶部，什么都不用做）
    pub(crate) fn attach(window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
        #[cfg(target_os = "windows")]
        {
            let Some(hwnd) = super::hwnd(window) else { return Err(PlatformError::System("不是 Win32 窗口".into())) };
            let Some(bar) = STATE.with(|s| s.borrow().bar.clone()) else { return Ok(()) };
            // SAFETY: 窗口刚创建，句柄有效
            unsafe { bar.init_for_hwnd(hwnd) }.map_err(error)?;
            STATE.with(|s| s.borrow_mut().windows.push(hwnd));
        }
        #[cfg(not(target_os = "windows"))]
        let _ = window;
        Ok(())
    }

    /// 窗口要关了：从窗口上摘下菜单栏（Windows）
    pub(crate) fn detach(window: &dyn HasWindowHandle) {
        #[cfg(target_os = "windows")]
        if let Some(hwnd) = super::hwnd(window) {
            let bar = STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.windows.retain(|h| *h != hwnd);
                s.themes.remove(&hwnd);
                s.bar.clone()
            });
            if let Some(bar) = bar {
                // SAFETY: 窗口还没销毁
                let _ = unsafe { bar.remove_for_hwnd(hwnd) };
            }
        }
        #[cfg(not(target_os = "windows"))]
        let _ = window;
    }

    /// 窗口外观的深浅变了（应用固定了深浅、或者跟着系统变了）：挂在它上面的菜单栏按同样的深浅画（Windows）。
    /// 没挂菜单栏的窗口、深浅没变时什么都不做。还没显示的窗口也不换：muda 换深浅时模拟一次失去、重新获得激活来重画菜单栏，
    /// 窗口还没显示时最后留下的是"不激活"，标题栏会画成灰的；窗口显示以后运行时按外观再设一次
    #[cfg(target_os = "windows")]
    pub(crate) fn set_theme(window: &dyn HasWindowHandle, dark: bool) {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
        let Some(hwnd) = super::hwnd(window) else { return };
        // SAFETY: 只读窗口的状态；句柄来自活着的窗口
        if unsafe { IsWindowVisible(hwnd as _) } == 0 {
            return;
        }
        let bar = STATE.with(|s| {
            let mut s = s.borrow_mut();
            if !s.windows.contains(&hwnd) || s.themes.get(&hwnd) == Some(&dark) {
                return None;
            }
            s.themes.insert(hwnd, dark);
            s.bar.clone()
        });
        if let Some(bar) = bar {
            let theme = if dark { muda::MenuTheme::Dark } else { muda::MenuTheme::Light };
            // SAFETY: 窗口还挂着菜单栏（关窗前 detach 会把它从表里去掉），没销毁
            let _ = unsafe { bar.set_theme_for_hwnd(hwnd, theme) };
        }
    }
}

/// Windows 的消息钩子：分发消息之前把菜单快捷键翻译成菜单命令，否则快捷键不起作用。翻译了返回 `true`，
/// 这条消息就不再分发。
///
/// 只翻译落在挂着菜单栏的窗口里的按键（消息的目标是它，或者是它的子窗口）。没挂菜单栏的窗口（隐藏了标题栏、无边框）里
/// 不翻译：按键照常交给窗口，运行时把菜单项的快捷键另外登记成了全应用的快捷键，在那里起作用
#[cfg(target_os = "windows")]
pub(crate) fn translate_accelerator(msg: *const std::ffi::c_void) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, MSG, TranslateAcceleratorW};
    // 钩子在消息循环里调用，不会碰上正借着的状态；万一碰上了，这条消息不翻译
    let found = STATE.with(|s| {
        let s = s.try_borrow().ok()?;
        let haccel = s.bar.as_ref()?.haccel();
        if haccel == 0 {
            return None;
        }
        let msg = msg.cast::<MSG>();
        // SAFETY: winit 传进来的是消息循环里正要分发的 MSG；窗口句柄可以是空的（线程消息），那时 GetAncestor 返回空
        let target = unsafe { GetAncestor((*msg).hwnd, GA_ROOT) } as isize;
        s.windows.contains(&target).then_some((haccel, target, msg))
    });
    let Some((haccel, target, msg)) = found else { return false };
    // SAFETY: msg 同上，是有效的 MSG；target 是还挂着菜单栏的窗口（关窗前 detach 会把它从表里去掉），
    // haccel 是菜单栏自己的快捷键表，菜单栏存在状态里一直活着
    unsafe { TranslateAcceleratorW(target as _, haccel as _, msg) != 0 }
}

/// 托盘
pub(crate) mod tray {
    use std::collections::HashMap;

    use super::{Owner, STATE, build, error, handlers, listen, main_thread};
    use crate::backend::{MenuItemId, MenuItemUpdate, OnMenuItem, OnTrayClick, PlatformError, TrayImage, TrayModel};

    fn icon(image: &TrayImage) -> Result<tray_icon::Icon, PlatformError> {
        tray_icon::Icon::from_rgba(image.rgba.clone(), image.size, image.size)
            .map_err(|error| PlatformError::System(format!("托盘图标无效：{error}").into()))
    }

    /// 放上托盘图标，换掉之前的
    pub(crate) fn set(model: TrayModel, on_item: OnMenuItem, on_click: Option<OnTrayClick>) -> Result<(), PlatformError> {
        main_thread()?;
        listen();
        let TrayModel { icons, tooltip, title, menu } = model;
        let mut builder = tray_icon::TrayIconBuilder::new();
        if let Some(image) = icons.first() {
            // macOS：模板图标，系统按菜单栏的深浅着色
            #[cfg(target_os = "macos")]
            if image.template {
                builder = builder.with_icon_templated(icon(image)?);
            } else {
                builder = builder.with_icon(icon(image)?);
            }
            #[cfg(not(target_os = "macos"))]
            {
                builder = builder.with_icon(icon(image)?);
            }
        }
        if let Some(tooltip) = &tooltip {
            builder = builder.with_tooltip(tooltip);
        }
        if let Some(title) = &title {
            builder = builder.with_title(title);
        }
        let mut items = HashMap::new();
        if let Some(menu) = menu {
            let native = muda::Menu::new();
            for entry in menu.entries {
                native.append(build(entry, Owner::Tray, &mut items).as_ref()).map_err(error)?;
            }
            builder = builder.with_menu(Box::new(native)).with_menu_on_left_click(on_click.is_none());
        }
        let tray = builder.build().map_err(|error| PlatformError::System(format!("建不起托盘图标：{error}").into()))?;
        {
            let mut handlers = handlers();
            handlers.tray = Some(on_item);
            handlers.tray_click = on_click;
        }
        let old = STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.tray_items = items;
            s.tray.replace(tray)
        });
        drop(old);
        Ok(())
    }

    /// 改托盘菜单里的一项
    pub(crate) fn update(id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        let found = STATE.with(|s| s.borrow().tray_items.get(&id).map(|item| item.update(&update)).is_some());
        if found { Ok(()) } else { Err(PlatformError::System(format!("托盘菜单里没有第 {} 项", id.0).into())) }
    }

    fn with_tray(f: impl FnOnce(&tray_icon::TrayIcon) -> Result<(), PlatformError>) -> Result<(), PlatformError> {
        let tray = STATE.with(|s| s.borrow().tray.clone());
        match tray {
            Some(tray) => f(&tray),
            None => Err(PlatformError::System("还没有托盘图标".into())),
        }
    }

    /// 图标旁边的文字（只有 macOS 显示）
    pub(crate) fn set_title(title: Option<&str>) -> Result<(), PlatformError> {
        with_tray(|tray| {
            tray.set_title(title);
            Ok(())
        })
    }

    /// 换图标（外观变了，按新的颜色重画的）
    pub(crate) fn set_icon(icons: Vec<TrayImage>) -> Result<(), PlatformError> {
        let Some(image) = icons.first() else { return Ok(()) };
        let native = icon(image)?;
        with_tray(|tray| {
            #[cfg(target_os = "macos")]
            let result = if image.template { tray.set_icon_templated(Some(native)) } else { tray.set_icon(Some(native)) };
            #[cfg(not(target_os = "macos"))]
            let result = tray.set_icon(Some(native));
            result.map_err(|error| PlatformError::System(format!("换不了托盘图标：{error}").into()))
        })
    }

    /// 拿掉托盘图标
    pub(crate) fn remove() {
        let old = STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.tray_items.clear();
            s.tray.take()
        });
        drop(old);
        let mut handlers = handlers();
        handlers.tray = None;
        handlers.tray_click = None;
    }
}
