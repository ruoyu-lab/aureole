//! 无头后端：给测试和测试工具用。所有服务都有进程内的替身，不碰系统（不改用户的剪贴板、不打开网址、不弹对话框、
//! 不发通知）；替身记下发生了什么，测试读回来核对，也能模拟用户的操作（选好对话框、点菜单项、改系统外观）。
//!
//! 帧时钟由调用方手动推进：[`HeadlessBackend::frame_clock`](crate::PlatformBackend::frame_clock) 给的是
//! [`FrameClockKind::Manual`] 的计时器，帧的时间就是传给 [`FrameClock::dispatch`] 的时间（虚拟时间也行），
//! 按帧率提示的间隔出帧，暂停时截止时间是 `None`。
//!
//! 能力可以改（[`HeadlessBackend::with_capabilities`]）：关掉的服务返回 [`PlatformError::Unsupported`]，
//! 用来测调用方在没有这项能力的平台上的退路。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use raw_window_handle::{DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle};

use crate::backend::{
    AppearanceService, Capabilities, ClipboardService, Conventions, DialogService, EnvironmentService,
    SingleInstanceService, MenuItemId, MenuItemUpdate, MenuModel, MenuService, NotificationRequest, NotificationResponse, NotificationService, OnMenuItem,
    OnNotificationResponse, OnTrayClick,
    PlatformBackend, PlatformError, PlatformWindow, ShellService, TrayIconFormat, TrayImage, TrayModel, TrayService, WindowService,
};
use crate::clipboard::{ClipFormat, ClipboardContent, RgbaImage};
use crate::external_drag::{ExternalDrag, ExternalDropEffect, OnExternalDrag};
use crate::frame::{FrameClock, FrameClockKind, FrameClockOptions, TimerClock};
use crate::single_instance::{InProcessInstances, InstanceRole};
use crate::open::{OnOpen, Opened};
use crate::system::{DialogDone, DialogOptions, SettingsChanged};
use crate::title_bar::{OnTitleBarChange, TitleBarAction, TitleBarMetrics, TitleBarOptions, WindowMaterial};
use crate::{DisplayColor, MenuEntry};

thread_local! {
    /// 这个线程上登记的拖放回调：它只在主线程上用，不要求能跨线程，不放进后端的状态
    static EXTERNAL_DRAG: std::cell::RefCell<Option<OnExternalDrag>> = const { std::cell::RefCell::new(None) };
}

/// 没有原生窗口的窗口：句柄都报"拿不到"。给无头后端的帧时钟、窗口服务当参数用
#[derive(Clone, Copy, Debug, Default)]
pub struct HeadlessWindow;

impl HasWindowHandle for HeadlessWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

impl HasDisplayHandle for HeadlessWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::Unavailable)
    }
}

/// 一次打印：印了什么
#[derive(Clone, Debug, PartialEq)]
pub struct HeadlessPrint {
    /// 打印队列里的名字
    pub title: String,
    /// 每一页
    pub pages: Vec<RgbaImage>,
}

/// 一次拖到应用外面
#[derive(Clone, Debug, PartialEq)]
pub struct HeadlessDrag {
    /// 拖的内容
    pub content: ClipboardContent,
    /// 起点，逻辑像素
    pub at: [f32; 2],
}

struct PendingDialog {
    options: DialogOptions,
    done: DialogDone,
}

struct Menu {
    model: MenuModel,
    on_item: OnMenuItem,
}

struct ShownNotification {
    request: NotificationRequest,
    on_response: Option<OnNotificationResponse>,
}

struct Tray {
    model: TrayModel,
    on_item: OnMenuItem,
    on_click: Option<OnTrayClick>,
}

struct State {
    clipboard: ClipboardContent,
    opened_urls: Vec<String>,
    opened_paths: Vec<PathBuf>,
    revealed_paths: Vec<PathBuf>,
    notifications: Vec<ShownNotification>,
    app_name: Option<String>,
    /// 放过几次提示音
    beeps: usize,
    dialogs: Vec<PendingDialog>,
    alerts: Vec<(String, String)>,
    dark_mode: Option<bool>,
    panel_dark: Option<bool>,
    accent_color: Option<[u8; 3]>,
    motion_reduced: bool,
    watchers: Vec<Arc<dyn Fn() + Send + Sync>>,
    language: Option<String>,
    low_power: bool,
    bundle_name: Option<String>,
    display_color: Option<DisplayColor>,
    pointer: Option<[f32; 2]>,
    secure_input: bool,
    drags: Vec<HeadlessDrag>,
    prints: Vec<HeadlessPrint>,
    menu_bar: Option<Menu>,
    tray: Option<Tray>,
    title_bar: Option<(TitleBarOptions, OnTitleBarChange)>,
    title_bar_metrics: TitleBarMetrics,
    title_bar_action: TitleBarAction,
    maximize_button: Option<[f32; 4]>,
    backdrop: WindowMaterial,
}

/// 无头后端，见[模块说明](self)
pub struct HeadlessBackend {
    capabilities: Capabilities,
    conventions: Conventions,
    state: Mutex<State>,
    instances: InProcessInstances,
}

impl Default for HeadlessBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// 打开、选中时路径不存在：和本机后端一样报错，不记下来
fn existing(path: &Path) -> Result<PathBuf, PlatformError> {
    let absolute = std::path::absolute(path).map_err(PlatformError::from)?;
    if absolute.exists() { Ok(absolute) } else { Err(PlatformError::System(format!("不存在：{}", absolute.display()).into())) }
}

fn unsupported(what: &'static str) -> PlatformError {
    PlatformError::Unsupported(format!("无头后端关掉了{what}").into())
}

/// 菜单里有没有这一项；有的话按 `update` 改
fn update_entries(entries: &mut [MenuEntry], id: MenuItemId, update: &MenuItemUpdate) -> bool {
    for entry in entries {
        match entry {
            MenuEntry::Item(item) if item.id == id => {
                if let Some(text) = &update.text {
                    item.text = text.clone();
                }
                if let Some(disabled) = update.disabled {
                    item.disabled = disabled;
                }
                if update.checked.is_some() {
                    item.checked = update.checked;
                }
                return true;
            }
            MenuEntry::Submenu(submenu) => {
                if update_entries(&mut submenu.entries, id, update) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// 菜单里这一项能不能点（存在、没禁用）
fn clickable(entries: &[MenuEntry], id: MenuItemId) -> bool {
    entries.iter().any(|entry| match entry {
        MenuEntry::Item(item) => item.id == id && !item.disabled,
        MenuEntry::Submenu(submenu) => clickable(&submenu.entries, id),
        MenuEntry::Separator => false,
    })
}

impl HeadlessBackend {
    /// 所有能力都打开，帧时钟手动推进；外观浅色，没有强调色，语言 `en-US`
    pub fn new() -> Self {
        let capabilities = Capabilities {
            frame_clock: FrameClockKind::Manual,
            supports_menu_bar: true,
            supports_tray: true,
            supports_clipboard: true,
            supports_clipboard_image: true,
            supports_clipboard_files: true,
            supports_clipboard_rich_text: true,
            supports_clipboard_multiple_formats: true,
            supports_clipboard_custom: true,
            supports_file_dialog: true,
            supports_notification: true,
            supports_notification_actions: true,
            supports_drag_out: true,
            supports_external_drop: true,
            supports_pointer_position: true,
            supports_display_color: true,
            supports_open_events: false,
            supports_settings_watch: true,
            supports_low_power_detection: true,
            supports_single_instance: true,
            supports_hidden_title_bar: true,
            supports_title_bar_controls: true,
            supports_snap_layouts: true,
            supports_window_backdrop: true,
            supports_print: true,
        };
        Self {
            capabilities,
            conventions: *Conventions::current(),
            state: Mutex::new(State {
                clipboard: ClipboardContent::default(),
                opened_urls: Vec::new(),
                opened_paths: Vec::new(),
                revealed_paths: Vec::new(),
                notifications: Vec::new(),
                app_name: None,
                beeps: 0,
                dialogs: Vec::new(),
                alerts: Vec::new(),
                dark_mode: Some(false),
                panel_dark: None,
                accent_color: None,
                motion_reduced: false,
                watchers: Vec::new(),
                language: Some("en-US".into()),
                low_power: false,
                bundle_name: None,
                display_color: None,
                pointer: None,
                secure_input: false,
                drags: Vec::new(),
                prints: Vec::new(),
                menu_bar: None,
                tray: None,
                title_bar: None,
                title_bar_metrics: TitleBarMetrics::default(),
                title_bar_action: TitleBarAction::Zoom,
                maximize_button: None,
                backdrop: WindowMaterial::Opaque,
            }),
            instances: InProcessInstances::default(),
        }
    }

    /// 换掉能力：关掉的服务返回"不支持"，菜单栏、托盘关掉时 [`menu`](PlatformBackend::menu)、[`tray`](PlatformBackend::tray) 是 `None`
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 换掉平台惯例（测别的平台的快捷键、滚动步长）
    pub fn with_conventions(mut self, conventions: Conventions) -> Self {
        self.conventions = conventions;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn caps(&self) -> Capabilities {
        self.capabilities
    }

    /// 改了外观设置：在调用方的线程上通知 [`watch_settings`](AppearanceService::watch_settings) 的回调
    fn settings_changed(&self) {
        let watchers = self.lock().watchers.clone();
        for watcher in watchers {
            watcher();
        }
    }

    // ---------- 读回发生了什么 ----------

    /// 打开过的网址
    pub fn opened_urls(&self) -> Vec<String> {
        self.lock().opened_urls.clone()
    }

    /// 打开过的文件和文件夹（绝对路径）
    pub fn opened_paths(&self) -> Vec<PathBuf> {
        self.lock().opened_paths.clone()
    }

    /// 在文件管理器里显示过的路径（绝对路径）
    pub fn revealed_paths(&self) -> Vec<PathBuf> {
        self.lock().revealed_paths.clone()
    }

    /// 发过的通知（标题、正文）
    pub fn notifications(&self) -> Vec<(String, String)> {
        self.lock().notifications.iter().map(|shown| (shown.request.title.clone(), shown.request.body.clone())).collect()
    }

    /// 放过几次提示音
    pub fn beep_count(&self) -> usize {
        self.lock().beeps
    }

    /// 发过的通知，带着动作按钮
    pub fn notification_requests(&self) -> Vec<NotificationRequest> {
        self.lock().notifications.iter().map(|shown| shown.request.clone()).collect()
    }

    /// 模拟用户对第 `index` 条通知做了什么（在调用方的线程上调用回调）；没有这一条、没有回调、没有这个按钮时返回 `false`
    pub fn respond_to_notification(&self, index: usize, response: NotificationResponse) -> bool {
        let handler = {
            let state = self.lock();
            let Some(shown) = state.notifications.get(index) else { return false };
            if let NotificationResponse::Action(action) = response
                && action >= shown.request.actions.len()
            {
                return false;
            }
            shown.on_response.clone()
        };
        handler.map(|on_response| on_response(response)).is_some()
    }

    /// 通知用的应用名
    pub fn app_name(&self) -> Option<String> {
        self.lock().app_name.clone()
    }

    /// 弹过的出错提示框（标题、内容）
    pub fn alerts(&self) -> Vec<(String, String)> {
        self.lock().alerts.clone()
    }

    /// 开着、等用户选的文件对话框，按弹出的顺序
    pub fn pending_dialogs(&self) -> Vec<DialogOptions> {
        self.lock().dialogs.iter().map(|d| d.options.clone()).collect()
    }

    /// 像用户选好一样完成最早弹出的那个文件对话框；没有开着的对话框时返回 `false`
    pub fn answer_dialog(&self, paths: Vec<PathBuf>) -> bool {
        let pending = {
            let mut state = self.lock();
            if state.dialogs.is_empty() { None } else { Some(state.dialogs.remove(0)) }
        };
        match pending {
            Some(dialog) => {
                (dialog.done)(paths);
                true
            }
            None => false,
        }
    }

    /// 拖到应用外面过的东西
    pub fn drags(&self) -> Vec<HeadlessDrag> {
        self.lock().drags.clone()
    }

    /// 印过的（[`DialogService::print`] 交来的）
    pub fn prints(&self) -> Vec<HeadlessPrint> {
        self.lock().prints.clone()
    }

    /// 装着的菜单栏
    pub fn menu_bar(&self) -> Option<MenuModel> {
        self.lock().menu_bar.as_ref().map(|m| m.model.clone())
    }

    /// 模拟系统的拖放流程调一次拖放的回调（这个线程上登记的），返回回答；没有登记时是 `None`
    pub fn simulate_external_drag(&self, drag: ExternalDrag) -> Option<ExternalDropEffect> {
        let handler = EXTERNAL_DRAG.with(|slot| slot.borrow().clone())?;
        handler(drag)
    }

    /// 像用户点击一样点菜单栏里的一项；没有这一项、禁用着时返回 `false`
    pub fn click_menu_item(&self, id: MenuItemId) -> bool {
        let handler = {
            let state = self.lock();
            state.menu_bar.as_ref().filter(|m| clickable(&m.model.entries, id)).map(|m| m.on_item.clone())
        };
        handler.map(|on_item| on_item(id)).is_some()
    }

    /// 放着的托盘图标
    pub fn tray_model(&self) -> Option<TrayModel> {
        self.lock().tray.as_ref().map(|t| t.model.clone())
    }

    /// 像用户左键点一下托盘图标；没有托盘、没有点击回调时返回 `false`
    pub fn click_tray(&self) -> bool {
        let handler = self.lock().tray.as_ref().and_then(|t| t.on_click.clone());
        handler.map(|on_click| on_click()).is_some()
    }

    /// 像用户点击一样点托盘菜单里的一项
    pub fn click_tray_item(&self, id: MenuItemId) -> bool {
        let handler = {
            let state = self.lock();
            state
                .tray
                .as_ref()
                .filter(|t| t.model.menu.as_ref().is_some_and(|m| clickable(&m.entries, id)))
                .map(|t| t.on_item.clone())
        };
        handler.map(|on_item| on_item(id)).is_some()
    }

    /// 这个标识有没有实例拿着
    pub fn is_instance_held(&self, id: &str) -> bool {
        self.instances.is_held(id)
    }

    // ---------- 模拟系统的状态 ----------

    /// 模拟用户切换深浅色（`None` 表示读不到），通知监视设置的回调
    pub fn set_dark_mode(&self, dark: Option<bool>) {
        self.lock().dark_mode = dark;
        self.settings_changed();
    }

    /// 模拟任务栏、面板的深浅
    pub fn set_panel_dark(&self, dark: Option<bool>) {
        self.lock().panel_dark = dark;
    }

    /// 模拟换强调色，通知监视设置的回调
    pub fn set_accent_color(&self, color: Option<[u8; 3]>) {
        self.lock().accent_color = color;
        self.settings_changed();
    }

    /// 模拟打开、关掉"减少动态效果"，通知监视设置的回调
    pub fn set_motion_reduced(&self, reduced: bool) {
        self.lock().motion_reduced = reduced;
        self.settings_changed();
    }

    /// 模拟首选语言
    pub fn set_preferred_language(&self, language: Option<String>) {
        self.lock().language = language;
    }

    /// 模拟低电量模式
    pub fn set_low_power_mode(&self, on: bool) {
        self.lock().low_power = on;
    }

    /// 模拟打包后的应用名
    pub fn set_bundle_name(&self, name: Option<String>) {
        self.lock().bundle_name = name;
    }

    /// 模拟显示器的颜色能力
    pub fn set_display_color(&self, color: Option<DisplayColor>) {
        self.lock().display_color = color;
    }

    /// 模拟指针位置
    pub fn set_pointer_position(&self, position: Option<[f32; 2]>) {
        self.lock().pointer = position;
    }

    /// 隐藏了的标题栏的设置；没隐藏时是 `None`
    pub fn title_bar_options(&self) -> Option<TitleBarOptions> {
        self.lock().title_bar.as_ref().map(|(options, _)| *options)
    }

    /// 模拟系统改了标题栏的度量（窗口缩放、进出全屏）：记下来，通知 [`attach_title_bar`](WindowService::attach_title_bar) 给的回调
    pub fn set_title_bar_metrics(&self, metrics: TitleBarMetrics) {
        let on_change = {
            let mut state = self.lock();
            state.title_bar_metrics = metrics;
            state.title_bar.as_ref().map(|(_, on_change)| on_change.clone())
        };
        if let Some(on_change) = on_change {
            on_change();
        }
    }

    /// 模拟系统设置里双击标题栏该做什么
    pub fn set_title_bar_action(&self, action: TitleBarAction) {
        self.lock().title_bar_action = action;
    }

    /// 报告给系统的最大化按钮的范围
    pub fn maximize_button_area(&self) -> Option<[f32; 4]> {
        self.lock().maximize_button
    }

    /// 窗口此刻的材质
    pub fn window_backdrop(&self) -> WindowMaterial {
        self.lock().backdrop
    }

    /// 模拟别的程序往剪贴板里放了文字（`None` 是清空）
    pub fn set_clipboard_text(&self, text: Option<String>) {
        self.lock().clipboard = ClipboardContent { text, ..ClipboardContent::default() };
    }

    /// 模拟别的程序换掉了剪贴板的内容
    pub fn set_clipboard(&self, content: ClipboardContent) {
        self.lock().clipboard = content;
    }

    /// 剪贴板此刻的内容
    pub fn clipboard_content(&self) -> ClipboardContent {
        self.lock().clipboard.clone()
    }
}

impl PlatformBackend for HeadlessBackend {
    fn name(&self) -> &'static str {
        "headless"
    }

    fn capabilities(&self) -> Capabilities {
        self.caps()
    }

    fn conventions(&self) -> &Conventions {
        &self.conventions
    }

    fn frame_clock(&self, _window: &dyn PlatformWindow, options: FrameClockOptions) -> Box<dyn FrameClock> {
        Box::new(TimerClock::manual(options.rate))
    }

    fn window(&self) -> &dyn WindowService {
        self
    }

    fn appearance(&self) -> &dyn AppearanceService {
        self
    }

    fn environment(&self) -> &dyn EnvironmentService {
        self
    }

    fn clipboard(&self) -> &dyn ClipboardService {
        self
    }

    fn dialog(&self) -> &dyn DialogService {
        self
    }

    fn shell(&self) -> &dyn ShellService {
        self
    }

    fn notification(&self) -> &dyn NotificationService {
        self
    }

    fn menu(&self) -> Option<&dyn MenuService> {
        if self.caps().supports_menu_bar { Some(self) } else { None }
    }

    fn tray(&self) -> Option<&dyn TrayService> {
        if self.caps().supports_tray { Some(self) } else { None }
    }

    fn single_instance(&self) -> &dyn SingleInstanceService {
        self
    }

}

impl WindowService for HeadlessBackend {
    fn display_color(&self, _window: &dyn HasWindowHandle) -> Option<DisplayColor> {
        self.lock().display_color
    }

    fn pointer_position(&self, _window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
        self.lock().pointer
    }

    fn bring_to_front(&self, _window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
        Ok(())
    }

    fn activate_if_unpackaged(&self) -> bool {
        false
    }

    fn handling_wheel_event(&self) -> bool {
        false
    }

    /// 记下拖了什么（空的也记：应用自己的窗口之间拖元素），马上以"没被接住"结束（在调用方的线程上调用 `on_end(false)`）
    fn begin_drag_out(
        &self,
        _window: &dyn HasWindowHandle,
        content: &ClipboardContent,
        at: [f32; 2],
        on_end: Box<dyn FnOnce(bool)>,
    ) -> Result<(), PlatformError> {
        if !self.caps().supports_drag_out {
            return Err(unsupported("拖到应用外面"));
        }
        self.lock().drags.push(HeadlessDrag { content: content.clone(), at });
        on_end(false);
        Ok(())
    }

    /// 记下回调，[`HeadlessBackend::simulate_external_drag`] 调用它
    fn listen_external_drag(&self, _window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
        if !self.caps().supports_external_drop {
            return Err(unsupported("接收别的应用拖来的内容"));
        }
        EXTERNAL_DRAG.with(|slot| *slot.borrow_mut() = Some(on_drag));
        Ok(())
    }

    fn stop_external_drag(&self, _window: &dyn HasWindowHandle) {
        EXTERNAL_DRAG.with(|slot| *slot.borrow_mut() = None);
    }

    /// 记下开关（[`WindowService::is_secure_input_enabled`] 读回）
    fn set_secure_input(&self, on: bool) -> Result<(), PlatformError> {
        self.lock().secure_input = on;
        Ok(())
    }

    fn is_secure_input_enabled(&self) -> bool {
        self.lock().secure_input
    }

    /// 记下设置和回调（[`HeadlessBackend::set_title_bar_metrics`] 调用它）
    fn attach_title_bar(&self, _window: &dyn HasWindowHandle, options: TitleBarOptions, on_change: OnTitleBarChange) -> Result<(), PlatformError> {
        if !self.caps().supports_hidden_title_bar {
            return Err(unsupported("隐藏标题栏"));
        }
        self.lock().title_bar = Some((options, on_change));
        Ok(())
    }

    fn detach_title_bar(&self, _window: &dyn HasWindowHandle) {
        let mut state = self.lock();
        state.title_bar = None;
        state.maximize_button = None;
    }

    fn set_title_bar_options(&self, _window: &dyn HasWindowHandle, options: TitleBarOptions) -> Result<(), PlatformError> {
        match &mut self.lock().title_bar {
            Some(attached) => attached.0 = options,
            None => return Err(PlatformError::System("这个窗口的标题栏没有隐藏".into())),
        }
        Ok(())
    }

    fn title_bar_metrics(&self, _window: &dyn HasWindowHandle) -> Option<TitleBarMetrics> {
        self.caps().supports_hidden_title_bar.then(|| self.lock().title_bar_metrics)
    }

    fn set_maximize_button_area(&self, _window: &dyn HasWindowHandle, area: Option<[f32; 4]>) {
        if self.caps().supports_snap_layouts {
            self.lock().maximize_button = area;
        }
    }

    fn title_bar_action(&self) -> TitleBarAction {
        self.lock().title_bar_action
    }

    fn set_window_backdrop(&self, _window: &dyn HasWindowHandle, backdrop: WindowMaterial) -> Result<(), PlatformError> {
        if !self.caps().supports_window_backdrop {
            return Err(unsupported("窗口材质"));
        }
        self.lock().backdrop = backdrop;
        Ok(())
    }
}

impl AppearanceService for HeadlessBackend {
    fn is_dark_mode(&self) -> Option<bool> {
        self.lock().dark_mode
    }

    fn is_panel_dark(&self) -> Option<bool> {
        self.lock().panel_dark
    }

    fn accent_color(&self) -> Option<[u8; 3]> {
        self.lock().accent_color
    }

    fn is_motion_reduced(&self) -> bool {
        self.lock().motion_reduced
    }

    fn watch_settings(&self, on_change: SettingsChanged) {
        if self.caps().supports_settings_watch {
            self.lock().watchers.push(Arc::from(on_change));
        }
    }
}

impl EnvironmentService for HeadlessBackend {
    fn preferred_language(&self) -> Option<String> {
        self.lock().language.clone()
    }

    fn is_low_power_mode(&self) -> bool {
        self.lock().low_power
    }

    fn bundle_name(&self) -> Option<String> {
        self.lock().bundle_name.clone()
    }
}

impl HeadlessBackend {
    /// 读写一种格式前看它开着没有。一次只能放一种的本机后端（Linux）放什么，无头后端按同一个优先顺序放
    fn clipboard_allows(&self, enabled: bool) -> Result<(), PlatformError> {
        if self.capabilities.supports_clipboard && enabled { Ok(()) } else { Err(unsupported("剪贴板的这种格式")) }
    }
}

impl ClipboardService for HeadlessBackend {
    fn read_text(&self) -> Result<Option<String>, PlatformError> {
        let state = self.lock();
        if !self.capabilities.supports_clipboard {
            return Err(unsupported("剪贴板"));
        }
        Ok(state.clipboard.text.clone())
    }

    fn write_text(&self, text: &str) -> Result<(), PlatformError> {
        let mut state = self.lock();
        if !self.capabilities.supports_clipboard {
            return Err(unsupported("剪贴板"));
        }
        state.clipboard = ClipboardContent { text: Some(text.to_owned()), ..ClipboardContent::default() };
        Ok(())
    }

    fn formats(&self) -> Result<Vec<ClipFormat>, PlatformError> {
        self.clipboard_allows(true)?;
        Ok(self.lock().clipboard.formats())
    }

    fn read_rich_text(&self) -> Result<Option<Vec<u8>>, PlatformError> {
        self.clipboard_allows(self.capabilities.supports_clipboard_rich_text)?;
        Ok(self.lock().clipboard.rich_text.clone())
    }

    fn read_image(&self) -> Result<Option<RgbaImage>, PlatformError> {
        self.clipboard_allows(self.capabilities.supports_clipboard_image)?;
        Ok(self.lock().clipboard.image.clone())
    }

    fn read_files(&self) -> Result<Vec<PathBuf>, PlatformError> {
        self.clipboard_allows(self.capabilities.supports_clipboard_files)?;
        Ok(self.lock().clipboard.files.clone())
    }

    fn read_custom(&self, format: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        self.clipboard_allows(self.capabilities.supports_clipboard_custom)?;
        Ok(self.lock().clipboard.custom.iter().find(|(name, _)| name == format).map(|(_, bytes)| bytes.clone()))
    }

    /// 能力关掉的格式放不进去：一次只能放一种时只留最优先的（文件、图片、文字），和本机后端一样
    fn write(&self, content: &ClipboardContent) -> Result<(), PlatformError> {
        let caps = self.capabilities;
        self.clipboard_allows(true)?;
        if let Some(image) = content.image.as_ref().filter(|image| !image.is_valid()) {
            return Err(PlatformError::System(format!("图片的像素数不对：{}×{}，{} 字节", image.width, image.height, image.rgba.len()).into()));
        }
        let mut kept = content.clone();
        if !caps.supports_clipboard_files {
            kept.files.clear();
        }
        if !caps.supports_clipboard_image {
            kept.image = None;
        }
        if !caps.supports_clipboard_rich_text {
            kept.rich_text = None;
        }
        if !caps.supports_clipboard_custom {
            kept.custom.clear();
        }
        if !caps.supports_clipboard_multiple_formats {
            if !kept.files.is_empty() {
                kept = ClipboardContent { files: kept.files, ..ClipboardContent::default() };
            } else if kept.image.is_some() {
                kept = ClipboardContent { image: kept.image, ..ClipboardContent::default() };
            } else {
                kept = ClipboardContent { text: kept.text, ..ClipboardContent::default() };
            }
        }
        self.lock().clipboard = kept;
        Ok(())
    }
}

impl DialogService for HeadlessBackend {
    /// 记下来，等测试用 [`HeadlessBackend::answer_dialog`] 选好或 [`cancel_file_dialogs`](Self::cancel_file_dialogs) 取消；
    /// 关掉了文件对话框的能力时马上以空列表完成
    fn file_dialog(&self, options: DialogOptions, _parent: Option<&dyn HasWindowHandle>, done: DialogDone) {
        let mut state = self.lock();
        if !self.capabilities.supports_file_dialog {
            drop(state);
            return done(Vec::new());
        }
        state.dialogs.push(PendingDialog { options, done });
    }

    fn cancel_file_dialogs(&self) -> usize {
        let pending = std::mem::take(&mut self.lock().dialogs);
        let count = pending.len();
        for dialog in pending {
            (dialog.done)(Vec::new());
        }
        count
    }

    fn alert(&self, title: &str, message: &str) {
        self.lock().alerts.push((title.to_owned(), message.to_owned()));
    }

    /// 马上选好：A4 纸，四边各 18 点印不到，300 点每英寸
    fn print_dialog(&self, _parent: Option<&dyn HasWindowHandle>, done: crate::PrintDialogDone) {
        if !self.capabilities.supports_print {
            return done(Err(unsupported("打印")));
        }
        let paper = [595.28, 841.89];
        done(Ok(Some(crate::PrintSetup { paper, printable: [18.0, 18.0, paper[0] - 36.0, paper[1] - 36.0], dpi: 300.0, id: crate::print::next_id() })));
    }

    /// 记下印了什么（没有页时不记），马上完成
    fn print(&self, _setup: crate::PrintSetup, title: &str, pages: Vec<RgbaImage>, done: crate::PrintDone) {
        if let Err(error) = crate::print::check_pages(&pages) {
            return done(Err(error));
        }
        if !pages.is_empty() {
            self.lock().prints.push(HeadlessPrint { title: title.to_owned(), pages });
        }
        done(Ok(true));
    }
}

impl ShellService for HeadlessBackend {
    fn open_url(&self, url: &str) -> Result<(), PlatformError> {
        if !crate::open::is_url(url) {
            return Err(PlatformError::System(format!("不是网址：{url}").into()));
        }
        self.lock().opened_urls.push(url.to_owned());
        Ok(())
    }

    fn open_path(&self, path: &Path) -> Result<(), PlatformError> {
        let path = existing(path)?;
        self.lock().opened_paths.push(path);
        Ok(())
    }

    fn reveal_path(&self, path: &Path) -> Result<(), PlatformError> {
        let path = existing(path)?;
        self.lock().revealed_paths.push(path);
        Ok(())
    }
}

impl NotificationService for HeadlessBackend {
    fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError> {
        let mut state = self.lock();
        if !self.capabilities.supports_notification {
            return Err(unsupported("系统通知"));
        }
        state.notifications.push(ShownNotification {
            request: NotificationRequest { title: title.to_owned(), body: body.to_owned(), actions: Vec::new() },
            on_response: None,
        });
        Ok(())
    }

    fn show(&self, request: NotificationRequest, on_response: OnNotificationResponse) -> Result<(), PlatformError> {
        let mut state = self.lock();
        if !self.capabilities.supports_notification_actions {
            return Err(unsupported("带动作的系统通知"));
        }
        state.notifications.push(ShownNotification { request, on_response: Some(on_response) });
        Ok(())
    }

    fn set_app_name(&self, name: &str) {
        self.lock().app_name = Some(name.to_owned());
    }

    fn beep(&self) -> Result<(), PlatformError> {
        self.lock().beeps += 1;
        Ok(())
    }
}

/// 位图的像素数和边长对得上
fn check_icon(icon: &TrayImage) -> Result<(), PlatformError> {
    if icon.rgba.len() == (icon.size as usize).pow(2) * 4 {
        Ok(())
    } else {
        Err(PlatformError::System(format!("图标的像素数不对：{} 字节，边长 {}", icon.rgba.len(), icon.size).into()))
    }
}

impl MenuService for HeadlessBackend {
    /// 照给的样子记下来，不补应用菜单（那是 macOS 的本机后端做的）
    fn set_menu_bar(&self, menu: MenuModel, _app_name: &str, on_item: OnMenuItem) -> Result<(), PlatformError> {
        let mut state = self.lock();
        if !self.capabilities.supports_menu_bar {
            return Err(unsupported("菜单栏"));
        }
        state.menu_bar = Some(Menu { model: menu, on_item });
        Ok(())
    }

    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        let mut state = self.lock();
        let found = state.menu_bar.as_mut().is_some_and(|menu| update_entries(&mut menu.model.entries, id, &update));
        if found { Ok(()) } else { Err(PlatformError::System(format!("菜单栏里没有第 {} 项", id.0).into())) }
    }

    fn attach_window(&self, _window: &dyn HasWindowHandle) -> Result<(), PlatformError> {
        Ok(())
    }

    fn detach_window(&self, _window: &dyn HasWindowHandle) {}
}

impl TrayService for HeadlessBackend {
    /// 像 Windows 一样：32 像素一张，按面板的深浅（[`HeadlessBackend::set_panel_dark`]）是白色或接近黑色，不是模板图标
    fn icon_format(&self) -> TrayIconFormat {
        let rgb = if self.lock().panel_dark.unwrap_or(true) { [255, 255, 255] } else { [0x1c, 0x1c, 0x1e] };
        TrayIconFormat { sizes: vec![32], rgb, template: false }
    }

    fn set_tray(&self, tray: TrayModel, on_item: OnMenuItem, on_click: Option<OnTrayClick>) -> Result<(), PlatformError> {
        tray.icons.iter().try_for_each(check_icon)?;
        let mut state = self.lock();
        if !self.capabilities.supports_tray {
            return Err(unsupported("托盘"));
        }
        state.tray = Some(Tray { model: tray, on_item, on_click });
        Ok(())
    }

    fn update_item(&self, id: MenuItemId, update: MenuItemUpdate) -> Result<(), PlatformError> {
        let mut state = self.lock();
        let found = state.tray.as_mut().and_then(|t| t.model.menu.as_mut()).is_some_and(|menu| update_entries(&mut menu.entries, id, &update));
        if found { Ok(()) } else { Err(PlatformError::System(format!("托盘菜单里没有第 {} 项", id.0).into())) }
    }

    fn set_title(&self, title: Option<&str>) -> Result<(), PlatformError> {
        let mut state = self.lock();
        let tray = state.tray.as_mut().ok_or_else(|| PlatformError::System("还没有托盘图标".into()))?;
        tray.model.title = title.map(str::to_owned);
        Ok(())
    }

    fn set_icon(&self, icons: Vec<TrayImage>) -> Result<(), PlatformError> {
        icons.iter().try_for_each(check_icon)?;
        let mut state = self.lock();
        let tray = state.tray.as_mut().ok_or_else(|| PlatformError::System("还没有托盘图标".into()))?;
        tray.model.icons = icons;
        Ok(())
    }

    fn remove_tray(&self) {
        self.lock().tray = None;
    }
}

impl SingleInstanceService for HeadlessBackend {
    fn acquire(&self, id: &str, wait: bool) -> Result<InstanceRole, PlatformError> {
        if !self.caps().supports_single_instance {
            return Err(unsupported("单实例"));
        }
        self.instances.acquire(id, wait).map_err(PlatformError::from)
    }

    fn delivers_open_events(&self) -> bool {
        false
    }

    fn listen_open_events(&self, _on_open: OnOpen) {}

    fn collect_launch_events(&self, _timeout: Duration) -> Opened {
        Opened::default()
    }
}
