//! 原生菜单（macOS 屏幕顶部的菜单栏、Windows 窗口的菜单）和托盘图标（macOS 菜单栏右侧、Windows 任务栏通知区、
//! Linux 桌面的状态栏）。
//!
//! 菜单项的文字、能否点击、是否打勾都可以绑定信号，状态变了菜单跟着变。点击的处理器在主线程、
//! 在创建菜单时的作用域里执行，和处理器一样。
//!
//! 这里是描述（[`Menu`]、[`Submenu`]、[`MenuItem`]、[`Tray`]）和响应式的那一层：把描述换成平台层中立的菜单模型
//! （每个菜单项一个编号），交给平台后端的菜单栏、托盘服务，绑定的信号变了按编号改原生菜单；
//! 原生的实现在平台层（macOS、Windows：muda 和 tray-icon，Windows 的菜单栏挂在每个有边框的窗口上；
//! Linux：托盘走 D-Bus 上的 StatusNotifierItem，没有菜单栏）。没有原生菜单栏的平台上菜单项的快捷键登记成全应用的快捷键，照样起作用。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use platform::{
    MenuEntry, MenuItemId, MenuItemModel, MenuItemUpdate, MenuModel, OnMenuItem, OnTrayClick, PlatformError, SubmenuModel, TrayIconFormat,
    TrayImage, TrayModel,
};
use reactive::{Effect, Scope, Signal};

use aureole_core::{IntoProp, MainThreadSender, VectorPath, Prop, Stroke};

use crate::system::backend;

/// 一个菜单：作为菜单栏时，每一项是一个 [`Submenu`]；作为托盘菜单时，放 [`MenuItem`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::{Menu, MenuItem, Submenu};
/// let on_top = signal(false);
/// let menu = Menu::new()
///     .submenu(Submenu::app("我的应用"))
///     .submenu(
///         Submenu::new("窗口")
///             .item(MenuItem::new("新建窗口").shortcut("CmdOrCtrl+N").on_click(|| { /* ... */ }))
///             .separator()
///             .item(MenuItem::new("置顶").checked(on_top)),
///     );
/// ```
#[derive(Default)]
pub struct Menu {
    pub(crate) entries: Vec<Entry>,
}

pub(crate) enum Entry {
    Item(MenuItem),
    Submenu(Submenu),
    Separator,
}

impl Menu {
    /// 空菜单
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加子菜单（菜单栏上的一项）
    pub fn submenu(mut self, submenu: Submenu) -> Self {
        self.entries.push(Entry::Submenu(submenu));
        self
    }

    /// 添加菜单项
    pub fn item(mut self, item: MenuItem) -> Self {
        self.entries.push(Entry::Item(item));
        self
    }

    /// 添加分隔线
    pub fn separator(mut self) -> Self {
        self.entries.push(Entry::Separator);
        self
    }
}

/// 子菜单
pub struct Submenu {
    pub(crate) title: String,
    pub(crate) entries: Vec<Entry>,
    /// macOS 的应用菜单：关于、服务、隐藏、退出
    pub(crate) app: bool,
}

impl Submenu {
    /// 指定标题的空子菜单
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), entries: Vec::new(), app: false }
    }

    /// macOS 的应用菜单（菜单栏最左边、以应用名命名的那个）：关于、服务、隐藏、显示全部、退出。
    /// 菜单栏的第一项不是它时自动补上；可以再往里加自己的项，加在"退出"前面
    pub fn app(name: impl Into<String>) -> Self {
        Self { title: name.into(), entries: Vec::new(), app: true }
    }

    /// 添加菜单项
    pub fn item(mut self, item: MenuItem) -> Self {
        self.entries.push(Entry::Item(item));
        self
    }

    /// 添加下一级子菜单
    pub fn submenu(mut self, submenu: Submenu) -> Self {
        self.entries.push(Entry::Submenu(submenu));
        self
    }

    /// 添加分隔线
    pub fn separator(mut self) -> Self {
        self.entries.push(Entry::Separator);
        self
    }
}

/// 菜单项
pub struct MenuItem {
    pub(crate) text: Prop<String>,
    pub(crate) shortcut: Option<String>,
    pub(crate) disabled: Option<Prop<bool>>,
    pub(crate) checked: Option<Signal<bool>>,
    pub(crate) on_click: Option<Rc<dyn Fn()>>,
}

impl MenuItem {
    /// 文字可以绑定信号
    pub fn new<S: Into<String> + 'static>(text: impl IntoProp<S>) -> Self {
        Self { text: text.into_prop().map(S::into), shortcut: None, disabled: None, checked: None, on_click: None }
    }

    /// 快捷键，例如 `"CmdOrCtrl+N"`、`"Shift+CmdOrCtrl+S"`、`"Alt+F4"`。
    /// 菜单的快捷键先于窗口收到按键：不要占用输入框要用的 `CmdOrCtrl+C/V/X/A/Z`
    pub fn shortcut(mut self, shortcut: impl Into<String>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// 禁用（不能点击），可以绑定信号
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 带勾选标记的菜单项：点击时切换这个信号，信号变了勾选跟着变
    pub fn checked(mut self, checked: Signal<bool>) -> Self {
        self.checked = Some(checked);
        self
    }

    /// 点击时调用，在主线程、创建菜单时的作用域里执行。处理器的返回值不用
    pub fn on_click<R>(mut self, handler: impl Fn() -> R + 'static) -> Self {
        self.on_click = Some(Rc::new(move || {
            handler();
        }));
        self
    }
}

/// 托盘图标。图标用矢量路径画（macOS 上是模板图标，跟着菜单栏的深浅色变）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::{Menu, MenuItem, Tray};
/// let tray = Tray::new()
///     .icon(VectorPath::circle(12.0, 12.0, 9.0).view_box(0.0, 0.0, 24.0, 24.0))
///     .tooltip("我的应用")
///     .menu(Menu::new().item(MenuItem::new("退出").on_click(aureole::quit)));
/// ```
#[derive(Default)]
pub struct Tray {
    pub(crate) icon: Option<(VectorPath, Option<Stroke>)>,
    pub(crate) tooltip: Option<String>,
    pub(crate) title: Option<Prop<String>>,
    pub(crate) menu: Option<Menu>,
    pub(crate) on_click: Option<Rc<dyn Fn()>>,
}

impl Tray {
    /// 空托盘图标，设好图标、菜单后交给 [`crate::App::tray`] 安装
    pub fn new() -> Self {
        Self::default()
    }

    /// 图标：填充的路径
    pub fn icon(mut self, path: VectorPath) -> Self {
        self.icon = Some((path, None));
        self
    }

    /// 图标：描边的路径
    pub fn icon_stroke(mut self, path: VectorPath, stroke: Stroke) -> Self {
        self.icon = Some((path, Some(stroke)));
        self
    }

    /// 指针停在图标上时的提示
    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// 图标旁边的文字（只有 macOS 显示），可以绑定信号
    pub fn title<S: Into<String> + 'static>(mut self, title: impl IntoProp<S>) -> Self {
        self.title = Some(title.into_prop().map(S::into));
        self
    }

    /// 点图标弹出的菜单。同时设了 [`Tray::on_click`] 时，左键点击调用处理器，右键弹出菜单
    pub fn menu(mut self, menu: Menu) -> Self {
        self.menu = Some(menu);
        self
    }

    /// 点击托盘图标时调用；同时设了菜单时只响应左键。处理器的返回值不用
    pub fn on_click<R>(mut self, handler: impl Fn() -> R + 'static) -> Self {
        self.on_click = Some(Rc::new(move || {
            handler();
        }));
        self
    }
}

/// 装好的菜单栏和托盘，活到应用退出
struct Installed {
    /// 装上了原生菜单栏（新窗口要挂上它）
    menu_bar: bool,
    /// 托盘图标的来源：任务栏、面板的深浅变了时按新的颜色重画
    tray_icon: Option<TrayIcon>,
    _scope: Scope,
}

struct TrayIcon {
    path: VectorPath,
    stroke: Option<Stroke>,
    rgb: Cell<[u8; 3]>,
}

thread_local! {
    /// 装好的菜单栏和托盘（主线程上，事件循环开始后装）
    static INSTALLED: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

fn with_installed(f: impl FnOnce(&Installed)) {
    INSTALLED.with(|i| {
        if let Some(installed) = &*i.borrow() {
            f(installed);
        }
    });
}

/// 新窗口创建好了：菜单栏长在窗口里的平台（Windows）挂上去
pub(crate) fn attach_window(window: &winit::window::Window) {
    with_installed(|installed| installed.attach(window));
}

/// 窗口要关了：从窗口上摘下菜单栏
pub(crate) fn detach_window(window: &winit::window::Window) {
    with_installed(|installed| installed.detach(window));
}

/// 系统外观变了：托盘图标按新的深浅重画
pub(crate) fn refresh_appearance() {
    with_installed(Installed::refresh_appearance);
}

impl Installed {
    /// 新窗口创建好了：菜单栏长在窗口里的平台（Windows）挂上去
    fn attach(&self, window: &winit::window::Window) {
        if let (true, Some(menu)) = (self.menu_bar, backend().menu())
            && let Err(error) = menu.attach_window(window)
        {
            tracing::error!("菜单栏挂到窗口上失败：{error}");
        }
    }

    /// 窗口要关了：从窗口上摘下菜单栏
    fn detach(&self, window: &winit::window::Window) {
        if let (true, Some(menu)) = (self.menu_bar, backend().menu()) {
            menu.detach_window(window);
        }
    }

    /// 系统外观变了：托盘图标按任务栏、面板的新深浅重画（macOS 的模板图标由系统着色，颜色不变，不用重画）
    fn refresh_appearance(&self) {
        let (Some(icon), Some(tray)) = (&self.tray_icon, backend().tray()) else { return };
        let format = tray.icon_format();
        if icon.rgb.replace(format.rgb) != format.rgb {
            report("换托盘图标", tray.set_icon(render(&icon.path, icon.stroke, &format)));
        }
    }
}

/// 装上菜单栏和托盘（事件循环开始后，在主线程）。`app_name` 用于自动补上的 macOS 应用菜单
pub(crate) fn install(menu: Option<Menu>, tray: Option<Tray>, app_name: &str) {
    let main = aureole_core::main_thread_sender();
    let scope = Scope::new();
    let (menu_bar, tray_icon) = scope.run(|| {
        let menu_bar = menu.is_some_and(|menu| install_menu_bar(menu, app_name, &main));
        let tray_icon = tray.and_then(|tray| install_tray(tray, &main));
        (menu_bar, tray_icon)
    });
    INSTALLED.with(|i| *i.borrow_mut() = Some(Installed { menu_bar, tray_icon, _scope: scope }));
}

/// 菜单项被点了：编号投递回主线程再交给处理器（原生回调可能在系统菜单的处理流程里、在托盘服务的线程里）
fn forward(main: &MainThreadSender) -> OnMenuItem {
    let main = main.clone();
    Arc::new(move |id| {
        main.send(move || dispatch_menu(id));
    })
}

/// 装上菜单栏，返回装上了没有。没有原生菜单栏的平台（Linux）上菜单项的快捷键登记成全应用的快捷键
fn install_menu_bar(menu: Menu, app_name: &str, main: &MainThreadSender) -> bool {
    let Some(service) = backend().menu() else {
        tracing::warn!("这个平台不显示菜单栏，菜单项的快捷键照样起作用");
        bind_shortcuts(menu.entries);
        return false;
    };
    let mut bindings = Vec::new();
    let entries = model(menu.entries, &mut bindings);
    // Windows 的菜单栏长在窗口里，系统只翻译落在挂着它的窗口里的快捷键：没挂菜单栏的窗口（隐藏了标题栏、无边框）里
    // 照样要起作用，另外登记成全应用的快捷键（挂着菜单栏的窗口里按键先被系统翻译掉，不会触发两次）
    let shortcuts = if cfg!(target_os = "windows") { shortcuts_of(&entries) } else { Vec::new() };
    match service.set_menu_bar(MenuModel { entries }, app_name, forward(main)) {
        Ok(()) => {
            bind(bindings, move |id, update| service.update_item(id, update));
            for (chord, id) in shortcuts {
                aureole_core::shortcut(&chord, move || {
                    if !is_disabled(id) {
                        dispatch_menu(id);
                    }
                });
            }
            true
        }
        Err(error) => {
            tracing::error!("装不上菜单栏：{error}");
            false
        }
    }
}

/// 放上托盘图标，返回图标的来源（外观变了重画用）
fn install_tray(tray: Tray, main: &MainThreadSender) -> Option<TrayIcon> {
    let Some(service) = backend().tray() else {
        tracing::warn!("这个平台没有托盘，已忽略");
        return None;
    };
    let format = service.icon_format();
    let icons = tray.icon.as_ref().map(|(path, stroke)| render(path, *stroke, &format)).unwrap_or_default();
    let mut bindings = Vec::new();
    let menu = tray.menu.map(|menu| MenuModel { entries: model(menu.entries, &mut bindings) });
    let title = tray.title.as_ref().map(|title| match title {
        Prop::Static(title) => title.clone(),
        Prop::Dynamic(f) => reactive::untracked(f),
    });
    let on_click = tray.on_click.map(|handler| {
        register_tray(handler, Scope::current());
        let main = main.clone();
        Arc::new(move || {
            main.send(dispatch_tray_click);
        }) as OnTrayClick
    });
    let model = TrayModel { icons, tooltip: tray.tooltip, title, menu };
    if let Err(error) = service.set_tray(model, forward(main), on_click) {
        tracing::error!("托盘图标不可用：{error}");
        return None;
    }
    bind(bindings, move |id, update| service.update_item(id, update));
    if let Some(Prop::Dynamic(f)) = tray.title {
        Effect::new(move || report("改托盘的文字", service.set_title(Some(&f()))));
    }
    tray.icon.map(|(path, stroke)| TrayIcon { path, stroke, rgb: Cell::new(format.rgb) })
}

/// 托盘图标按平台要的样子画成位图：每种边长一张
fn render(path: &VectorPath, stroke: Option<Stroke>, format: &TrayIconFormat) -> Vec<TrayImage> {
    format
        .sizes
        .iter()
        .map(|&size| TrayImage {
            rgba: path.to_rgba(stroke, size, format.rgb),
            size,
            template: format.template,
        })
        .collect()
}

/// 改原生菜单失败时记一条
fn report(what: &str, result: Result<(), PlatformError>) {
    if let Err(error) = result {
        tracing::warn!("{what}失败：{error}");
    }
}

/// 菜单项的绑定：装好菜单以后接上，状态变了按编号改原生菜单
enum Binding {
    Text(MenuItemId, Box<dyn Fn() -> String>),
    Disabled(MenuItemId, Box<dyn Fn() -> bool>),
    Checked(MenuItemId, Signal<bool>),
}

thread_local! {
    /// 此刻禁用着的菜单项（快捷键另外登记成全应用的快捷键时，按下前看一眼）
    static DISABLED: RefCell<HashSet<MenuItemId>> = RefCell::default();
}

fn set_disabled(id: MenuItemId, disabled: bool) {
    DISABLED.with(|d| {
        let mut d = d.borrow_mut();
        if disabled {
            d.insert(id);
        } else {
            d.remove(&id);
        }
    });
}

fn is_disabled(id: MenuItemId) -> bool {
    DISABLED.with(|d| d.borrow().contains(&id))
}

/// 菜单模型里带快捷键的菜单项：快捷键的写法和编号
fn shortcuts_of(entries: &[MenuEntry]) -> Vec<(String, MenuItemId)> {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            MenuEntry::Separator => {}
            MenuEntry::Submenu(submenu) => out.extend(shortcuts_of(&submenu.entries)),
            MenuEntry::Item(item) => out.extend(item.shortcut.clone().map(|chord| (chord, item.id))),
        }
    }
    out
}

/// 每个菜单项一个编号（菜单栏和托盘共用，不重）
fn next_id() -> MenuItemId {
    thread_local! {
        static NEXT: Cell<u64> = const { Cell::new(1) };
    }
    MenuItemId(NEXT.with(|n| n.replace(n.get() + 1)))
}

/// 描述 → 中立的菜单模型：给每个菜单项一个编号，登记点击的处理器（在当前作用域里执行），记下要接的绑定
fn model(entries: Vec<Entry>, bindings: &mut Vec<Binding>) -> Vec<MenuEntry> {
    entries
        .into_iter()
        .map(|entry| match entry {
            Entry::Separator => MenuEntry::Separator,
            Entry::Submenu(submenu) => MenuEntry::Submenu(SubmenuModel {
                title: submenu.title,
                entries: model(submenu.entries, bindings),
                is_application_menu: submenu.app,
            }),
            Entry::Item(item) => {
                let id = next_id();
                let text = match item.text {
                    Prop::Static(text) => text,
                    Prop::Dynamic(f) => {
                        let text = reactive::untracked(&f);
                        bindings.push(Binding::Text(id, f));
                        text
                    }
                };
                let disabled = match item.disabled {
                    None => false,
                    Some(Prop::Static(disabled)) => disabled,
                    Some(Prop::Dynamic(f)) => {
                        let disabled = reactive::untracked(&f);
                        bindings.push(Binding::Disabled(id, f));
                        disabled
                    }
                };
                set_disabled(id, disabled);
                let checked = item.checked.map(|signal| {
                    bindings.push(Binding::Checked(id, signal));
                    reactive::untracked(|| signal.get())
                });
                let (signal, on_click) = (item.checked, item.on_click);
                register(id, Scope::current(), move || click(signal, on_click.as_deref()));
                MenuEntry::Item(MenuItemModel { id, text, shortcut: item.shortcut, disabled, checked })
            }
        })
        .collect()
}

/// 接上绑定：每个绑定一个副作用（在装菜单的作用域里），状态变了用 `update` 改原生菜单的那一项
fn bind(bindings: Vec<Binding>, update: impl Fn(MenuItemId, MenuItemUpdate) -> Result<(), PlatformError> + Clone + 'static) {
    for binding in bindings {
        let update = update.clone();
        let change = move |id, change: MenuItemUpdate| report("改菜单项", update(id, change));
        match binding {
            Binding::Text(id, f) => Effect::new(move || change(id, MenuItemUpdate { text: Some(f()), ..Default::default() })),
            Binding::Disabled(id, f) => Effect::new(move || {
                let disabled = f();
                set_disabled(id, disabled);
                change(id, MenuItemUpdate { disabled: Some(disabled), ..Default::default() })
            }),
            Binding::Checked(id, signal) => {
                Effect::new(move || change(id, MenuItemUpdate { checked: Some(signal.get()), ..Default::default() }))
            }
        };
    }
}

/// 没有原生菜单栏时：有快捷键的菜单项登记成全应用的快捷键，能点击时按下等于点了它（勾选项切换信号）
fn bind_shortcuts(entries: Vec<Entry>) {
    for entry in entries {
        match entry {
            Entry::Separator => {}
            Entry::Submenu(submenu) => bind_shortcuts(submenu.entries),
            Entry::Item(item) => {
                let Some(chord) = item.shortcut else { continue };
                let disabled: Rc<dyn Fn() -> bool> = match item.disabled {
                    None => Rc::new(|| false),
                    Some(Prop::Static(disabled)) => Rc::new(move || disabled),
                    Some(Prop::Dynamic(f)) => Rc::from(f),
                };
                let (checked, on_click) = (item.checked, item.on_click);
                aureole_core::shortcut(&chord, move || {
                    if !reactive::untracked(|| disabled()) {
                        click(checked, on_click.as_deref());
                    }
                });
            }
        }
    }
}

/// 点了菜单项：勾选项先切换信号，再调用处理器（所以窗口里的开关和菜单里的勾选永远一致）
fn click(checked: Option<Signal<bool>>, on_click: Option<&dyn Fn()>) {
    if let Some(checked) = checked {
        checked.update(|c| *c = !*c);
    }
    if let Some(f) = on_click {
        f();
    }
}

/// 原生菜单和托盘的回调都只把事件转给事件循环，真正的处理在主线程里、作用域里执行
fn run_in(scope: Option<Scope>, handler: &Rc<dyn Fn()>) {
    let handler = handler.clone();
    match scope {
        Some(scope) => scope.run(move || reactive::batch(|| handler())),
        None => reactive::batch(|| handler()),
    }
}

/// 点击的处理器和它所在的作用域
type Handler = (Rc<dyn Fn()>, Option<Scope>);

#[derive(Default)]
struct Handlers {
    menu: HashMap<MenuItemId, Handler>,
    tray: Option<Handler>,
}

thread_local! {
    static HANDLERS: RefCell<Handlers> = RefCell::default();
}

/// 登记菜单项被点击时的处理器（在当前作用域里执行）
fn register(id: MenuItemId, scope: Option<Scope>, handler: impl Fn() + 'static) {
    HANDLERS.with(|h| h.borrow_mut().menu.insert(id, (Rc::new(handler), scope)));
}

/// 登记托盘图标被点击时的处理器
fn register_tray(handler: Rc<dyn Fn()>, scope: Option<Scope>) {
    HANDLERS.with(|h| h.borrow_mut().tray = Some((handler, scope)));
}

/// 菜单项被点了（在主线程、事件循环里）。平台在应用菜单里补上的"退出"（Windows）走框架的 [`crate::quit`]
pub(crate) fn dispatch_menu(id: MenuItemId) {
    if id == MenuItemId::QUIT {
        crate::window::quit();
        return;
    }
    let handler = HANDLERS.with(|h| h.borrow().menu.get(&id).cloned());
    if let Some((handler, scope)) = handler {
        run_in(scope, &handler);
    }
}

pub(crate) fn dispatch_tray_click() {
    let handler = HANDLERS.with(|h| h.borrow().tray.clone());
    if let Some((handler, scope)) = handler {
        run_in(scope, &handler);
    }
}
