//! Linux 的托盘：D-Bus 上的 StatusNotifierItem 和 `com.canonical.dbusmenu`（ksni，纯 Rust，不链接原生界面工具包）。
//! KDE、Xfce、Cinnamon、装了 AppIndicator 扩展的 GNOME 会显示。
//!
//! 菜单的数据放在托盘服务的线程里，调用方改它时交给那个线程（`Handle::update`），服务随后通知桌面；
//! 点击在托盘服务的线程里调用调用方给的回调（调用方自己转回事件循环）。托盘的名字用通知用的应用名
//! （[`crate::NotificationService::set_app_name`]）。在 Flatpak 里不占用 StatusNotifierItem 的总线名字。

use std::sync::{Mutex, MutexGuard};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};

use crate::backend::{MenuEntry, MenuItemId, MenuItemUpdate, OnMenuItem, OnTrayClick, PlatformError, TrayImage, TrayModel};

/// 托盘服务线程里的数据
struct Model {
    id: String,
    title: String,
    tooltip: String,
    icon: Vec<ksni::Icon>,
    items: Vec<Node>,
    on_item: OnMenuItem,
    on_click: Option<OnTrayClick>,
}

enum Node {
    Item(Item),
    Submenu(String, Vec<Node>),
    Separator,
}

struct Item {
    id: MenuItemId,
    label: String,
    enabled: bool,
    checked: Option<bool>,
}

impl Model {
    fn item(&mut self, id: MenuItemId) -> Option<&mut Item> {
        fn find(nodes: &mut [Node], id: MenuItemId) -> Option<&mut Item> {
            nodes.iter_mut().find_map(|node| match node {
                Node::Item(item) if item.id == id => Some(item),
                Node::Submenu(_, children) => find(children, id),
                _ => None,
            })
        }
        find(&mut self.items, id)
    }
}

/// `MENU_ON_LEFT`：只有菜单时左键弹菜单；同时有点击回调时左键调用回调、右键弹菜单
struct SniTray<const MENU_ON_LEFT: bool>(Model);

impl<const MENU_ON_LEFT: bool> ksni::Tray for SniTray<MENU_ON_LEFT> {
    const MENU_ON_ACTIVATE: bool = MENU_ON_LEFT;

    fn id(&self) -> String {
        self.0.id.clone()
    }

    fn title(&self) -> String {
        self.0.title.clone()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.0.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.0.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Some(on_click) = &self.0.on_click {
            on_click();
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        convert(&self.0.items)
    }
}

/// 菜单数据 → ksni 的菜单。dbusmenu 里下划线是助记符的前缀，字面的下划线要写两个
fn convert<const M: bool>(nodes: &[Node]) -> Vec<ksni::MenuItem<SniTray<M>>> {
    let label = |text: &str| text.replace('_', "__");
    nodes
        .iter()
        .map(|node| match node {
            Node::Separator => ksni::MenuItem::Separator,
            Node::Submenu(title, children) => SubMenu { label: label(title), submenu: convert(children), ..Default::default() }.into(),
            Node::Item(item) => {
                let id = item.id;
                let activate = Box::new(move |tray: &mut SniTray<M>| (tray.0.on_item)(id));
                match item.checked {
                    Some(checked) => {
                        CheckmarkItem { label: label(&item.label), enabled: item.enabled, checked, activate, ..Default::default() }.into()
                    }
                    None => StandardItem { label: label(&item.label), enabled: item.enabled, activate, ..Default::default() }.into(),
                }
            }
        })
        .collect()
}

fn nodes(entries: Vec<MenuEntry>) -> Vec<Node> {
    entries
        .into_iter()
        .map(|entry| match entry {
            MenuEntry::Separator => Node::Separator,
            MenuEntry::Submenu(submenu) => Node::Submenu(submenu.title, nodes(submenu.entries)),
            MenuEntry::Item(item) => Node::Item(Item { id: item.id, label: item.text, enabled: !item.disabled, checked: item.checked }),
        })
        .collect()
}

/// 几种边长的位图 → ksni 的图标（ARGB）
fn pixmaps(icons: &[TrayImage]) -> Vec<ksni::Icon> {
    icons
        .iter()
        .map(|image| ksni::Icon {
            width: image.size as i32,
            height: image.size as i32,
            data: crate::system::portable::argb_from_rgba(&image.rgba),
        })
        .collect()
}

/// 两种托盘的句柄
#[derive(Clone)]
enum Handle {
    MenuOnLeft(ksni::blocking::Handle<SniTray<true>>),
    ClickOnLeft(ksni::blocking::Handle<SniTray<false>>),
}

impl Handle {
    /// 改托盘服务里的数据，服务随后通知桌面
    fn update(&self, f: impl FnOnce(&mut Model)) {
        match self {
            Handle::MenuOnLeft(handle) => handle.update(|tray| f(&mut tray.0)),
            Handle::ClickOnLeft(handle) => handle.update(|tray| f(&mut tray.0)),
        };
    }

    fn shutdown(&self) {
        match self {
            Handle::MenuOnLeft(handle) => drop(handle.shutdown()),
            Handle::ClickOnLeft(handle) => drop(handle.shutdown()),
        }
    }
}

static TRAY: Mutex<Option<Handle>> = Mutex::new(None);

fn tray() -> MutexGuard<'static, Option<Handle>> {
    TRAY.lock().unwrap_or_else(|e| e.into_inner())
}

fn with_tray(f: impl FnOnce(&Handle)) -> Result<(), PlatformError> {
    let handle = tray().clone();
    match handle {
        Some(handle) => {
            f(&handle);
            Ok(())
        }
        None => Err(PlatformError::System("还没有托盘图标".into())),
    }
}

/// 放上托盘图标，换掉之前的
pub(crate) fn set(model: TrayModel, on_item: OnMenuItem, on_click: Option<OnTrayClick>) -> Result<(), PlatformError> {
    let name = crate::system::imp::app_name();
    let TrayModel { icons, tooltip, title: _, menu } = model;
    let menu_on_left = on_click.is_none();
    let model = Model {
        id: name.clone(),
        title: name.clone(),
        tooltip: tooltip.unwrap_or(name),
        icon: pixmaps(&icons),
        items: menu.map(|menu| nodes(menu.entries)).unwrap_or_default(),
        on_item,
        on_click,
    };
    // 沙盒（Flatpak）里不能占用 StatusNotifierItem 的总线名字
    let sandboxed = std::path::Path::new("/.flatpak-info").exists();
    let spawned = if menu_on_left {
        SniTray::<true>(model).disable_dbus_name(sandboxed).spawn().map(Handle::MenuOnLeft)
    } else {
        SniTray::<false>(model).disable_dbus_name(sandboxed).spawn().map(Handle::ClickOnLeft)
    };
    let handle = spawned.map_err(|error| {
        PlatformError::Unsupported(format!("{error}（桌面要支持 StatusNotifierItem；GNOME 要装 AppIndicator 扩展）").into())
    })?;
    if let Some(old) = tray().replace(handle) {
        old.shutdown();
    }
    Ok(())
}

/// 改托盘菜单里的一项
pub(crate) fn update(id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
    with_tray(|handle| {
        handle.update(|model| {
            if let Some(item) = model.item(id) {
                if let Some(text) = update.text {
                    item.label = text;
                }
                if let Some(disabled) = update.disabled {
                    item.enabled = !disabled;
                }
                if update.checked.is_some() {
                    item.checked = update.checked;
                }
            }
        })
    })
}

/// 图标旁边的文字只有 macOS 显示：这里什么都不做
pub(crate) fn set_title(_title: Option<&str>) -> Result<(), PlatformError> {
    Ok(())
}

/// 换图标（面板的深浅变了，按新的颜色重画的）
pub(crate) fn set_icon(icons: Vec<TrayImage>) -> Result<(), PlatformError> {
    let icon = pixmaps(&icons);
    with_tray(|handle| handle.update(move |model| model.icon = icon))
}

/// 拿掉托盘图标
pub(crate) fn remove() {
    if let Some(handle) = tray().take() {
        handle.shutdown();
    }
}
