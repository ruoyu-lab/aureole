//! 输入转给 CEF：指针（移动、按下、松开、离开）、滚轮、按键（按下、松开、字符）、输入法、焦点；CEF 要的光标形状。
//!
//! 两条路进来：
//!
//! - 输入目标（[`InputTarget`]、[`TextInputClient`]，元素种类上）：左键拿焦点的那次按下和之后的拖动、松开（拖出元素也照样收到），
//!   按键按下，焦点，手指拖着滚动，输入法。元素树先给焦点再交按下，网页先得到焦点再收到点击，和在系统窗口里一样；
//! - 元素上的处理器（[`attach`] 挂上）：输入目标收不到的——悬停时的移动、离开、右键和中键、侧键、滚轮、按键松开、被打断的按下。
//!
//! 坐标的换算见 `convert`，按键编码见 `keys`。网页还没打开（`shared.browser` 是 `None`）时都安静地忽略。
//!
//! 每个网页的输入状态（[`InputState`]）放在按 [`Shared`] 找的表里：共享状态里还没有它的字段，元素种类、处理器、
//! 只拿到 `&Shared` 的回调（[`ime_range_changed`]）找到的是同一份。

mod convert;
mod keys;

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};

use aureole::Element;
use aureole_core::kinds::{Focusable, Handled, InputTarget, SubTarget, TargetContext, TextInputClient};
use aureole_core::{
    Builtin, Buttons, Cursor, ExternalDragEvent, ExternalDropEvent, GesturePhase, KeyEvent, MouseButton, PointerEvent, PointerEventKind, Rect, WheelDelta, WheelEvent,
};
use cef::{ImplBrowser, ImplBrowserHost, ImplDragData, ImplFrame};

use self::convert::Platform;
use self::keys::{Edit, Press, Stroke};
use crate::kind::WebViewKind;
use crate::state::Shared;

/// 一个网页的输入状态
#[derive(Default)]
pub(crate) struct InputState {
    /// 内容区在元素里的偏移（内边距和边框），逻辑像素：元素坐标减掉它是网页里的坐标。
    /// 输入目标的每次调用都带着内容区，记下来给处理器和 [`TextInputClient::caret_area`] 用
    inset: Cell<[f32; 2]>,
    /// 按下交给了 CEF、还没松开的键（左键是输入目标交的，别的是处理器交的）。按着的按键的标记只算这些
    pressed: Cell<Buttons>,
    /// 指针在元素上面（进入、离开的处理器记）：松开时已经在外面的话告诉网页指针离开了
    inside: Cell<bool>,
    /// 指针最后在网页里的位置：手指拖着滚动时滚轮事件要一个位置
    last: Cell<[f32; 2]>,
    /// 滚轮换成整数 DIP 留下的零头
    wheel_rest: Cell<[f32; 2]>,
    /// 按下交给了 CEF 的键（虚拟键码）：松开时只给这些发 KEYUP，网页收不到没按下过的松开
    held: RefCell<Vec<u16>>,
    /// 网页里正在组字
    composing: Cell<bool>,
    /// 组的字的区域，网页里的 DIP（[`ime_range_changed`] 记）。组完以后留着：下次组字开始时候选框先放在这附近
    composition: Cell<Option<Rect>>,
    /// 页面自己开始的拖放（拖页面里的元素、选中的文字）在进行：页面许的操作。这期间左键的移动、
    /// 松开改成拖放的经过、放下（[`page_drag_started`]）
    page_drag: Cell<Option<DragMask>>,
    /// 页面这会儿接受的拖放操作（[`drag_cursor`] 记），结束时告诉页面；没有时是不能放
    drag_operation: Cell<Option<DragMask>>,
}

impl InputState {
    fn set_inset(&self, content: Rect) {
        self.inset.set([content.x, content.y]);
    }

    /// 元素坐标换成网页里的坐标，记下来
    fn web(&self, position: [f32; 2]) -> [f32; 2] {
        let web = convert::web_position(position, self.inset.get());
        self.last.set(web);
        web
    }
}

thread_local! {
    /// 每个网页的输入状态：都在主线程上。网页的共享状态释放了的在下次查找时清掉
    static STATES: RefCell<Vec<(Weak<Shared>, Rc<InputState>)>> = const { RefCell::new(Vec::new()) };
}

/// `shared` 的输入状态，还没有时建一份
fn state_of(shared: &Rc<Shared>) -> Rc<InputState> {
    STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|(owner, _)| owner.strong_count() > 0);
        if let Some((_, state)) = states.iter().find(|(owner, _)| owner.as_ptr() == Rc::as_ptr(shared)) {
            return state.clone();
        }
        let state = Rc::new(InputState::default());
        states.push((Rc::downgrade(shared), state.clone()));
        state
    })
}

/// 只拿到 `&Shared` 时找它的输入状态；元素还没建过（没有输入状态）时是 `None`
fn find_state(shared: &Shared) -> Option<Rc<InputState>> {
    STATES.with(|states| {
        let states = states.borrow();
        states.iter().find(|(owner, _)| owner.strong_count() > 0 && std::ptr::eq(owner.as_ptr(), shared)).map(|(_, state)| state.clone())
    })
}

/// 网页的宿主（发输入事件的对象）；网页还没打开时是 `None`。先取出来再调，调用时不借着 `shared.browser`
///
/// 有输入要交给网页：接下来一段时间每帧都让 CEF 出帧（悬停、按下的反馈马上画出来）
fn host(shared: &Shared) -> Option<cef::BrowserHost> {
    crate::render::wake(shared);
    shared.browser.borrow().as_ref().and_then(|browser| browser.host())
}

/// 按键事件里换算要用的几项
fn press(event: &KeyEvent) -> Press<'_> {
    Press {
        key: &event.key,
        base: &event.base,
        physical: event.physical.as_ref().map(|p| p.name()),
        text: event.text.as_deref(),
        typed: event.typed_text(),
        chord: event.is_chord(),
        modifiers: event.modifiers,
        repeated: event.repeated,
    }
}

impl WebViewKind {
    fn input(&self) -> Rc<InputState> {
        state_of(&self.shared)
    }
}

/// 网页视图的输入目标：整个内容区，按下和 Tab 都能拿焦点；拿焦点之后左键的按下、拖动、松开，按键按下交给网页
impl InputTarget for WebViewKind {
    /// 内容区里都算（内边距、边框不算）
    fn hit(&self, position: [f32; 2], content: Rect) -> Option<SubTarget> {
        self.input().set_inset(content);
        content.contains(position).then_some(SubTarget::Whole)
    }

    fn focusable(&self) -> Focusable {
        Focusable::Keyboard
    }

    /// 左键：拿焦点的那次按下（带着连击数）、之后的拖动（拖出元素也照样收到）、松开
    fn on_pointer(&mut self, event: &PointerEvent, cx: &mut TargetContext) -> Handled {
        let state = self.input();
        state.set_inset(cx.content_rect());
        let Some(host) = host(&self.shared) else { return Handled::No };
        let web = state.web(event.position);
        let clicks = event.clicks.max(1) as i32;
        match event.kind {
            PointerEventKind::Down => {
                state.pressed.set(state.pressed.get() | Buttons::LEFT);
                state.inside.set(true);
                let flags = convert::modifier_flags(event.modifiers) | convert::button_flags(event.buttons & state.pressed.get());
                host.send_mouse_click_event(Some(&convert::mouse_event(web, flags)), cef::MouseButtonType::LEFT, 0, clicks);
            }
            PointerEventKind::Move => {
                if drag_over(&state, &host, web) {
                    return Handled::Yes;
                }
                let flags = convert::modifier_flags(event.modifiers) | convert::button_flags(event.buttons & state.pressed.get());
                host.send_mouse_move_event(Some(&convert::mouse_event(web, flags)), 0);
            }
            PointerEventKind::Up => {
                state.pressed.set(state.pressed.get() - Buttons::LEFT);
                if drag_end(&state, &host, web, true) {
                    return Handled::Yes;
                }
                let flags = convert::modifier_flags(event.modifiers) | convert::button_flags(event.buttons & state.pressed.get());
                host.send_mouse_click_event(Some(&convert::mouse_event(web, flags)), cef::MouseButtonType::LEFT, 1, clicks);
                leave_if_outside(&state, &host);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// 按下交给网页（按下事件，打出字时再来字符事件）。组合键（按着 Command 或 Ctrl）也交给网页，但返回没处理，
    /// 应用的快捷键照样收得到；macOS 上的编辑命令（⌘C 这些）让网页执行，算处理了
    fn on_key_down(&mut self, event: &KeyEvent, cx: &mut TargetContext) -> Handled {
        let state = self.input();
        state.set_inset(cx.content_rect());
        let Some(host) = host(&self.shared) else { return Handled::No };
        let press = press(event);
        let platform = Platform::CURRENT;
        let code = keys::windows_key_code(&press);
        {
            let mut held = state.held.borrow_mut();
            if !(press.repeated && held.contains(&code)) {
                held.push(code);
            }
        }
        host.send_key_event(Some(&keys::key_event(&press, Stroke::Down, platform)));
        for event in keys::char_events(&press, platform) {
            host.send_key_event(Some(&event));
        }
        if let Some(edit) = keys::edit_command(&press, platform) {
            let frame = self.shared.browser.borrow().as_ref().and_then(|browser| browser.focused_frame());
            if let Some(frame) = frame {
                match edit {
                    Edit::Undo => frame.undo(),
                    Edit::Redo => frame.redo(),
                    Edit::Cut => frame.cut(),
                    Edit::Copy => frame.copy(),
                    Edit::Paste => frame.paste(),
                    Edit::SelectAll => frame.select_all(),
                }
            }
            return Handled::Yes;
        }
        Handled::from(!press.chord)
    }

    /// 告诉网页得到、失去焦点（网页还没打开时只记下，打开时按它给焦点）
    fn on_focus(&mut self, focused: bool, cx: &mut TargetContext) {
        self.input().set_inset(cx.content_rect());
        self.shared.focused.set(focused);
        if let Some(host) = host(&self.shared) {
            host.set_focus(i32::from(focused));
        }
    }

    /// 内容区里是网页要的形状（[`cursor_changed`] 记下的）
    fn cursor(&self, position: [f32; 2], content: Rect) -> Option<Cursor> {
        self.input().set_inset(content);
        content.contains(position).then(|| self.shared.cursor.get_untracked())
    }

    /// 手指拖着滚：网页自己决定滚不滚、滚到哪，打开着就都交给它（外面的视口不滚）
    fn can_scroll_content(&self, _: [f32; 2], content: Rect) -> bool {
        self.input().set_inset(content);
        self.shared.browser.borrow().is_some()
    }

    /// 手指拖着滚：当成触控板的滚动交给网页，位置是手指最后的位置
    fn scroll_content(&mut self, delta: [f32; 2], cx: &mut TargetContext) {
        let state = self.input();
        state.set_inset(cx.content_rect());
        if let Some(host) = host(&self.shared) {
            send_wheel(&state, &host, WheelDelta::Pixels(delta), state.last.get(), 0);
        }
    }

    fn wants_text_input(&self) -> bool {
        true
    }
}

/// 网页里的输入法：预编辑、提交、放弃交给 CEF 的组字；候选框避开网页报来的组字区域
impl TextInputClient for WebViewKind {
    /// 组的字的区域（网页报来的，换成元素坐标）；还没组过字时是内容区左上角
    fn caret_area(&self) -> Rect {
        let state = self.input();
        let [x, y] = state.inset.get();
        let area = state.composition.get().unwrap_or_default();
        Rect { x: area.x + x, y: area.y + y, ..area }
    }

    /// 预编辑：整段加一条下划线（颜色是 0，网页按文字的颜色画），插入点换成 UTF-16 的范围；空文字放弃正在组的字
    fn set_preedit(&mut self, text: &str, cursor: Option<Range<usize>>, cx: &mut TargetContext) {
        let state = self.input();
        state.set_inset(cx.content_rect());
        let Some(host) = host(&self.shared) else { return };
        if text.is_empty() {
            if state.composing.replace(false) {
                host.ime_cancel_composition();
            }
            return;
        }
        state.composing.set(true);
        let length = convert::utf16_offset(text, text.len());
        let selection = match cursor {
            Some(c) => cef::Range { from: convert::utf16_offset(text, c.start), to: convert::utf16_offset(text, c.end) },
            None => cef::Range { from: length, to: length },
        };
        let underline = cef::CompositionUnderline {
            range: cef::Range { from: 0, to: length },
            style: cef::CompositionUnderlineStyle::SOLID,
            ..Default::default()
        };
        host.ime_set_composition(Some(&cef::CefString::from(text)), Some(&[underline]), Some(&convert::NO_RANGE), Some(&selection));
    }

    /// 提交：替换正在组的字（没有时插在插入点）
    fn commit(&mut self, text: &str, cx: &mut TargetContext) {
        let state = self.input();
        state.set_inset(cx.content_rect());
        state.composing.set(false);
        if let Some(host) = host(&self.shared) {
            host.ime_commit_text(Some(&cef::CefString::from(text)), Some(&convert::NO_RANGE), 0);
        }
    }

    fn cancel_composition(&mut self, cx: &mut TargetContext) {
        let state = self.input();
        state.set_inset(cx.content_rect());
        if state.composing.replace(false)
            && let Some(host) = host(&self.shared)
        {
            host.ime_cancel_composition();
        }
    }
}

/// 松开以后（没有按着的键了）指针已经不在元素上：告诉网页指针离开了（拖出去的时候没告诉它）
fn leave_if_outside(state: &InputState, host: &cef::BrowserHost) {
    if state.pressed.get().is_empty() && !state.inside.get() {
        host.send_mouse_move_event(Some(&convert::mouse_event(state.last.get(), 0)), 1);
    }
}

/// 一次滚动交给网页：位置是网页里的坐标，`flags` 是修饰键的标记
fn send_wheel(state: &InputState, host: &cef::BrowserHost, delta: WheelDelta, web: [f32; 2], flags: u32) {
    let wheel = convert::wheel(delta, aureole_core::conventions().wheel_notch, state.wheel_rest.get());
    state.wheel_rest.set(wheel.rest);
    if wheel.delta == [0; 2] {
        return;
    }
    let flags = flags | convert::button_flags(state.pressed.get()) | if wheel.precise { convert::PRECISE } else { 0 };
    host.send_mouse_wheel_event(Some(&convert::mouse_event(web, flags)), wheel.delta[0], wheel.delta[1]);
}

/// 给网页视图的元素挂上输入目标收不到的事件：悬停时的移动、进入和离开、右键和中键的按下松开、侧键、滚轮、按键松开、
/// 被打断的按下。元素种类建元素时调用
pub(crate) fn attach(element: Element, shared: &Rc<Shared>) -> Element {
    let handlers = Rc::new(Handlers { shared: shared.clone(), state: state_of(shared) });
    let pointer = |f: fn(&Handlers, &PointerEvent)| {
        let handlers = handlers.clone();
        move |e: &PointerEvent| f(&handlers, e)
    };
    let element = element
        .on_pointer_move(pointer(Handlers::moved))
        .on_pointer_down(pointer(Handlers::pressed))
        .on_pointer_up(pointer(Handlers::released))
        .on_pointer_enter(pointer(Handlers::entered))
        .on_pointer_leave(pointer(Handlers::left))
        .on_pointer_cancel(pointer(Handlers::cancelled));
    let wheel = handlers.clone();
    let over = handlers.clone();
    let drop = handlers.clone();
    element
        .on_wheel(move |e: &WheelEvent| wheel.wheel(e))
        .on_external_drag_over(move |e: &ExternalDragEvent| over.external_drag_over(e))
        .on_external_drop(move |e: &ExternalDropEvent| drop.external_drop(e))
        .on_key_up(move |e: &KeyEvent| handlers.key_up(e))
}

/// 在网页里的 `web`（逻辑像素）点一下左键（读屏软件点了页面里的一项）
pub(crate) fn click_at(shared: &Shared, web: [f32; 2]) {
    let Some(host) = host(shared) else { return };
    // 和真的点一样：网页先得到焦点（没有焦点的网页不理点击）
    host.set_focus(1);
    host.send_mouse_move_event(Some(&convert::mouse_event(web, 0)), 0);
    host.send_mouse_click_event(Some(&convert::mouse_event(web, convert::LEFT_BUTTON)), cef::MouseButtonType::LEFT, 0, 1);
    host.send_mouse_click_event(Some(&convert::mouse_event(web, 0)), cef::MouseButtonType::LEFT, 1, 1);
}

/// CEF 的拖放操作的位：原样存着，不拆成整数（各平台上的整数类型不一样）
pub(crate) type DragMask = cef::sys::cef_drag_operations_mask_t;

fn operations(mask: DragMask) -> cef::DragOperationsMask {
    cef::DragOperationsMask::from(mask)
}

/// 页面开始拖了（`render` 的回调转到主线程上）：在网页视图里接着拖——之后左键的移动是经过、松开是放下，
/// 都交回给页面（拖到页面里别的地方：排序、拖进输入框）。不交给系统，拖不出窗口
pub(crate) fn page_drag_started(shared: &Shared, data: cef::DragData, allowed: DragMask, at: [i32; 2]) {
    let (Some(state), Some(host)) = (find_state(shared), host(shared)) else { return };
    state.page_drag.set(Some(allowed));
    state.drag_operation.set(None);
    let mut data = data;
    let web = [at[0] as f32, at[1] as f32];
    host.drag_target_drag_enter(Some(&mut data), Some(&convert::mouse_event(web, convert::LEFT_BUTTON)), operations(allowed));
}

/// 页面说这会儿能放下什么操作（`render` 的回调转到主线程上）：不能放时光标是禁止的样子
pub(crate) fn drag_cursor(shared: &Shared, operation: DragMask) {
    let Some(state) = find_state(shared) else { return };
    if state.page_drag.get().is_none() {
        return;
    }
    let operation = (operation != DragMask::DRAG_OPERATION_NONE).then_some(operation);
    state.drag_operation.set(operation);
    shared.cursor.set_if_changed(if operation.is_none() { Cursor::NotAllowed } else { Cursor::Grabbing });
}

/// 页面自己的拖放在进行：这次移动交成拖放的经过。返回是不是
fn drag_over(state: &InputState, host: &cef::BrowserHost, web: [f32; 2]) -> bool {
    let Some(allowed) = state.page_drag.get() else { return false };
    host.drag_target_drag_over(Some(&convert::mouse_event(web, convert::LEFT_BUTTON)), operations(allowed));
    true
}

/// 页面自己的拖放结束：`dropped` 时在这里放下，否则（被打断）离开。告诉页面拖的那边结束了。返回是不是在拖
fn drag_end(state: &InputState, host: &cef::BrowserHost, web: [f32; 2], dropped: bool) -> bool {
    if state.page_drag.take().is_none() {
        return false;
    }
    let operation = state.drag_operation.take().filter(|_| dropped);
    if operation.is_some() {
        host.drag_target_drop(Some(&convert::mouse_event(web, 0)));
    } else {
        host.drag_target_drag_leave();
    }
    host.drag_source_ended_at(web[0] as i32, web[1] as i32, operations(operation.unwrap_or(DragMask::DRAG_OPERATION_NONE)));
    host.drag_source_system_drag_ended();
    true
}

/// 元素上的处理器共用的
struct Handlers {
    shared: Rc<Shared>,
    state: Rc<InputState>,
}

impl Handlers {
    /// 按着的键里交给了 CEF 的那些的标记，加上修饰键
    fn flags(&self, event: &PointerEvent) -> u32 {
        convert::modifier_flags(event.modifiers) | convert::button_flags(event.buttons & self.state.pressed.get())
    }

    /// 悬停时的移动，按着右键、中键拖动（捕获着指针，拖出元素也收到）。左键按着时输入目标在交，这里不重复；
    /// 记着左键按着、其实已经松开了（松开没交到输入目标）时不再算按着
    fn moved(&self, event: &PointerEvent) {
        let pressed = self.state.pressed.get();
        if pressed.contains(Buttons::LEFT) {
            if event.buttons.contains(Buttons::LEFT) {
                return;
            }
            self.state.pressed.set(pressed - Buttons::LEFT);
        }
        let Some(host) = host(&self.shared) else { return };
        let web = self.state.web(event.position);
        if drag_over(&self.state, &host, web) {
            return;
        }
        host.send_mouse_move_event(Some(&convert::mouse_event(web, self.flags(event))), 0);
    }

    /// 右键、中键按下：捕获指针（松开时在元素外面也收得到）。左键由输入目标交（先拿焦点）
    fn pressed(&self, event: &PointerEvent) {
        let Some(button) = event.button.filter(|&b| b != MouseButton::Left) else { return };
        let Some(kind) = convert::mouse_button(button) else { return };
        let Some(host) = host(&self.shared) else { return };
        event.capture_pointer();
        self.state.pressed.set(self.state.pressed.get() | Buttons::from_button(button));
        self.state.inside.set(true);
        let web = self.state.web(event.position);
        host.send_mouse_click_event(Some(&convert::mouse_event(web, self.flags(event))), kind, 0, 1);
    }

    /// 右键、中键松开（按下交给过 CEF 的）；侧键松开是后退、前进（网页收不到侧键）
    fn released(&self, event: &PointerEvent) {
        let Some(button) = event.button.filter(|&b| b != MouseButton::Left) else { return };
        if button == MouseButton::Back {
            crate::client::back(&self.shared);
            return;
        }
        if button == MouseButton::Forward {
            crate::client::forward(&self.shared);
            return;
        }
        let bit = Buttons::from_button(button);
        let Some(kind) = convert::mouse_button(button).filter(|_| self.state.pressed.get().contains(bit)) else { return };
        self.state.pressed.set(self.state.pressed.get() - bit);
        let Some(host) = host(&self.shared) else { return };
        let web = self.state.web(event.position);
        host.send_mouse_click_event(Some(&convert::mouse_event(web, self.flags(event))), kind, 1, 1);
        leave_if_outside(&self.state, &host);
    }

    /// 指针进入元素
    fn entered(&self, _: &PointerEvent) {
        self.state.inside.set(true);
    }

    /// 指针离开元素：没有按着的键时告诉网页（按着时等松开）
    fn left(&self, _: &PointerEvent) {
        self.state.inside.set(false);
        if !self.state.pressed.get().is_empty() {
            return;
        }
        if let Some(host) = host(&self.shared) {
            host.send_mouse_move_event(Some(&convert::mouse_event(self.state.last.get(), 0)), 1);
        }
    }

    /// 进行中的按下被打断（窗口失去焦点、系统收走了指针）：网页丢掉指针的捕获，按着的键都算松开了
    fn cancelled(&self, _: &PointerEvent) {
        if self.state.pressed.replace(Buttons::empty()).is_empty() {
            return;
        }
        if let Some(host) = host(&self.shared)
            && !drag_end(&self.state, &host, self.state.last.get(), false)
        {
            host.send_capture_lost_event();
        }
    }

    /// 别的应用拖来的内容经过网页视图：都接受（放下时才知道内容，交给页面以后由页面决定要不要）
    fn external_drag_over(&self, event: &ExternalDragEvent) {
        event.accept();
    }

    /// 别的应用拖来的内容放下了：做成内核的拖放数据，在放下的位置依次交给页面进入、经过、放下
    /// （经过时还读不到内容，页面在放下的那一刻才收到 `dragenter`、`dragover`、`drop`）
    fn external_drop(&self, event: &ExternalDropEvent) {
        let Some(host) = host(&self.shared) else { return };
        let Some(mut data) = cef::drag_data_create() else { return };
        for path in &event.files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            data.add_file(Some(&cef::CefString::from(path.to_string_lossy().as_ref())), Some(&cef::CefString::from(name.as_str())));
        }
        if let Some(text) = &event.text {
            data.set_fragment_text(Some(&cef::CefString::from(text.as_str())));
        }
        if let Some(link) = &event.link {
            data.set_link_url(Some(&cef::CefString::from(link.as_str())));
        }
        let web = self.state.web(event.position);
        let mouse = convert::mouse_event(web, 0);
        let every = operations(DragMask::DRAG_OPERATION_EVERY);
        host.drag_target_drag_enter(Some(&mut data), Some(&mouse), every);
        host.drag_target_drag_over(Some(&mouse), every);
        host.drag_target_drop(Some(&mouse));
    }

    /// 滚轮、触控板滚动交给网页；外面的视口不滚、不再往外传
    fn wheel(&self, event: &WheelEvent) {
        let Some(host) = host(&self.shared) else { return };
        let web = self.state.web(event.position);
        send_wheel(&self.state, &host, event.delta, web, convert::modifier_flags(event.modifiers));
        if matches!(event.phase, GesturePhase::End | GesturePhase::Cancel) {
            self.state.wheel_rest.set([0.0; 2]);
        }
        event.prevent_builtin(Builtin::SCROLL);
        event.stop_propagation();
    }

    /// 按键松开：按下交给过网页的才交（框架保证松开交给按下时的那条链，焦点换走了也收得到）
    fn key_up(&self, event: &KeyEvent) {
        let press = press(event);
        let code = keys::windows_key_code(&press);
        let was_held = {
            let mut held = self.state.held.borrow_mut();
            held.iter().position(|&k| k == code).map(|i| held.remove(i)).is_some()
        };
        if !was_held {
            return;
        }
        if let Some(host) = host(&self.shared) {
            host.send_key_event(Some(&keys::key_event(&press, Stroke::Up, Platform::CURRENT)));
        }
        event.stop_propagation();
    }
}

/// CEF 要换光标（`client` 的显示回调转过来）：换成框架的形状写进 `shared.cursor`
pub(crate) fn cursor_changed(shared: &Shared, cursor: cef::CursorType) {
    shared.cursor.set_if_changed(convert::cursor(cursor));
}

/// 网页里组字的每个字的位置变了（`render` 的离屏回调转到主线程上）：输入法的候选框跟着它放。
/// 位置是网页内的逻辑像素
pub(crate) fn ime_range_changed(shared: &Shared, bounds: &[cef::Rect]) {
    let Some(state) = find_state(shared) else { return };
    if let Some(area) = convert::composition_area(bounds) {
        state.composition.set(Some(area));
    }
}
