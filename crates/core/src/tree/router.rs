//! 指针和键盘的路由状态：指针在哪、指针下和按下的元素、拖选、链接的悬停和按下、连击、拖动和手势、
//! 修饰键、焦点和焦点环、按着的键、等着调用的焦点处理器、正在输入法组字的元素。
//!
//! 只记状态，不碰节点和别的子结构；事件怎么分发在 `tree/interaction.rs`、`tree/route/`。
//! 节点开始退场、浮层关闭、节点卸载时，指向它们的状态只在 [`Router::forget`] 里清理。

use super::interaction::{Drag, FocusCache, Gesture, Held};
use super::route::{Finger, Touches};
use super::{FocusCallbacks, NodeId, NodeStore};
use crate::event::{Builtin, ExternalDataKinds, ExternalDropEffect};
use crate::handlers::{EventType, Handler};
use crate::input::{Buttons, Modifiers, MouseButton, PointerId, PointerKind};
use crate::interaction::FocusHandle;

/// 手指的两次点按算连击的距离：手指落不到同一个像素上。鼠标的间隔和距离、手指的间隔按平台惯例
/// （[`crate::Conventions::double_click_interval`]、[`crate::Conventions::double_click_distance`]，Windows 跟着系统设置）
const DOUBLE_TAP_DISTANCE: f32 = 24.0;

/// 哪些节点要从路由状态里去掉
#[derive(Clone, Copy)]
pub(super) enum Leave {
    /// 这个节点开始退场：它和子孙不再参与交互（处理器和信号已随作用域释放）
    Exit(NodeId),
    /// 这个浮层开始关闭：焦点由浮层自己还给打开前的元素，拖着文件经过的元素等文件离开时照常通知
    Close(NodeId),
    /// 卸载：已经不在树里的节点
    Unmount,
}

impl Leave {
    /// `id` 是不是要去掉的节点
    pub(super) fn gone(self, nodes: &NodeStore, id: NodeId) -> bool {
        match self {
            Self::Exit(root) | Self::Close(root) => nodes.is_within(id, root),
            Self::Unmount => !nodes.contains_key(id),
        }
    }
}

/// 一次按下（左键或中键）：松开时在"按下和松开都在其下"的最内层元素上合成点击
pub(super) struct Press {
    /// 按下时指针下从外到内的元素
    pub(super) path: Vec<NodeId>,
    /// 按下的处理器取消了的内置行为（取消了 `ACTIVATE` 就不合成点击）
    pub(super) prevented: Builtin,
}

/// 两次按下算不算连击
fn repeated(last: Option<(f64, [f32; 2])>, now: f64, point: [f32; 2], within: f32) -> bool {
    let interval = crate::system::conventions().double_click_interval.as_secs_f64();
    last.is_some_and(|(time, at)| now - time < interval && (at[0] - point[0]).hypot(at[1] - point[1]) < within)
}

/// 别的应用拖来的内容这一次问到哪了
#[derive(Default)]
pub(super) struct ExternalDragState {
    /// 这次收到过经过事件的元素：离开时通知它们
    pub asked: Vec<NodeId>,
    /// 最后一次问的内容、位置（通知离开时用）
    pub kinds: ExternalDataKinds,
    pub point: [f32; 2],
    /// 接受的元素和它说的操作
    pub target: Option<(NodeId, ExternalDropEffect)>,
    /// 指针下有元素关心这种拖放
    pub claimed: bool,
}

#[derive(Default)]
pub(super) struct Router {
    /// 指针在窗口里的位置
    pointer: Option<[f32; 2]>,
    /// 主指针是什么设备、哪一个（手指按着时是第一根手指）：指针事件带上它
    device: PointerKind,
    pointer_id: PointerId,
    /// 屏幕上的手指（见 `route/touch.rs`）：没有手指时是空的，鼠标的路径不碰它
    touches: Option<Box<Touches>>,
    /// 指针下从外到内的元素
    hovered: Vec<NodeId>,
    /// 拖着的文件下面的元素，从外到内
    file_hovered: Vec<NodeId>,
    /// 别的应用拖来的内容
    external_drag: ExternalDragState,
    /// 左键按下（还没松开、没开始拖动）
    press: Option<Press>,
    /// 按下时收到 on_press(true) 的元素，松开时通知它们 false
    press_feedback: Vec<NodeId>,
    /// 处理器捕获了指针：之后的移动、松开交给它，直到松开
    captured: Option<NodeId>,
    /// 按着的指针键
    buttons: Buttons,
    /// 正在拖动选择文字的输入框或可选中文字
    selecting: Option<NodeId>,
    /// 指针下的链接、按下时的链接：(文字元素, 第几个链接)，见 `links.rs`
    link_hover: Option<(NodeId, usize)>,
    link_press: Option<(NodeId, usize)>,
    /// 上一次左键按下：(时间, 位置, 连击数)。只有这一套：双击处理器、点击和指针事件里的连击数、
    /// 文字的双击选词三击选段都用它，时间是定时器的时钟（真实时间，测试工具里是虚拟时间）
    clicks: Option<(f64, [f32; 2], u32)>,
    /// 中键按下
    middle: Option<Press>,
    /// macOS 上按住 Control 的左键按成了右键，松开时不再当左键
    control_click: bool,
    /// 进行中的拖动（按下了还没越过阈值时也在）
    drag: Option<Drag>,
    pinch: Option<Gesture>,
    rotate: Option<Gesture>,
    /// 指针形状的绑定变了，窗口要重新设置
    cursor_dirty: bool,
    /// 当前按着的修饰键（Shift+点击扩展选区）
    modifiers: Modifiers,
    /// 有键盘焦点的元素（或者选中了文字的可选中文字）
    focused: Option<NodeId>,
    /// 焦点是用键盘（或最近一次输入是键盘时用程序）移过来的：画焦点环
    focus_visible: bool,
    /// 最近一次输入是键盘
    keyboard: bool,
    /// 按着的键，按下的顺序
    held: Vec<Held>,
    /// 焦点句柄交给了哪个元素（标签找它对应的控件）
    handles: Vec<(FocusHandle, NodeId)>,
    /// Tab 的候选（树的结构没变时不重新遍历）
    pub(super) focus_cache: FocusCache,
    /// 在借用树时产生（自动聚焦）、要在释放后调用的焦点处理器
    callbacks: FocusCallbacks,
    /// 正在输入法组字的元素（收到了没清掉的预编辑）
    composing: Option<NodeId>,
    /// 放弃了组字：平台的输入法也要重置（丢掉它那边没确定的字），窗口处理完这次输入（或者这一帧）时做
    ime_reset: bool,
}

impl Router {
    // ---- 指针 ----

    pub(super) fn pointer(&self) -> Option<[f32; 2]> {
        self.pointer
    }

    pub(super) fn set_pointer(&mut self, point: Option<[f32; 2]>) {
        self.pointer = point;
    }

    pub(super) fn device(&self) -> PointerKind {
        self.device
    }

    pub(super) fn pointer_id(&self) -> PointerId {
        self.pointer_id
    }

    /// 主指针换成这个设备上的这一个
    pub(super) fn set_source(&mut self, device: PointerKind, pointer_id: PointerId) {
        self.device = device;
        self.pointer_id = pointer_id;
    }

    pub(super) fn touches(&self) -> Option<&Touches> {
        self.touches.as_deref()
    }

    pub(super) fn touches_mut(&mut self) -> Option<&mut Touches> {
        self.touches.as_deref_mut()
    }

    pub(super) fn touches_or_default(&mut self) -> &mut Touches {
        self.touches.get_or_insert_default()
    }

    /// 手指离开屏幕：拿掉它，返回它和它是不是主手指。最后一根手指离开时状态也没了
    pub(super) fn take_finger(&mut self, id: PointerId) -> Option<(Finger, bool)> {
        let touches = self.touches.as_deref_mut()?;
        let finger = touches.remove(id)?;
        if touches.is_empty() {
            self.touches = None;
        }
        Some(finger)
    }

    pub(super) fn hovered(&self) -> &[NodeId] {
        &self.hovered
    }

    /// 换上新的悬停路径：抄进原有的缓冲（够大时不分配）
    pub(super) fn set_hovered_from(&mut self, path: &[NodeId]) {
        self.hovered.clear();
        self.hovered.extend_from_slice(path);
    }

    pub(super) fn file_hovered(&self) -> &[NodeId] {
        &self.file_hovered
    }

    pub(super) fn set_file_hovered(&mut self, path: Vec<NodeId>) {
        self.file_hovered = path;
    }

    pub(super) fn external_drag(&self) -> &ExternalDragState {
        &self.external_drag
    }

    /// 记下这次问的结果，取走上一次的
    pub(super) fn replace_external_drag(&mut self, state: ExternalDragState) -> ExternalDragState {
        std::mem::replace(&mut self.external_drag, state)
    }

    /// 左键按下（松开时合成点击）
    pub(super) fn press(&self) -> Option<&Press> {
        self.press.as_ref()
    }

    pub(super) fn set_press(&mut self, press: Option<Press>) {
        self.press = press;
    }

    pub(super) fn take_press(&mut self) -> Option<Press> {
        self.press.take()
    }

    pub(super) fn set_press_feedback(&mut self, feedback: Vec<NodeId>) {
        self.press_feedback = feedback;
    }

    /// 收到按压反馈的元素（拖动交给了外面的元素时，里面的元素从这里拿走）
    pub(super) fn press_feedback_mut(&mut self) -> &mut Vec<NodeId> {
        &mut self.press_feedback
    }

    /// 取走收到按压反馈的元素（松开、交给系统时通知 false）
    pub(super) fn take_press_feedback(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.press_feedback)
    }

    pub(super) fn captured(&self) -> Option<NodeId> {
        self.captured
    }

    pub(super) fn set_captured(&mut self, id: Option<NodeId>) {
        self.captured = id;
    }

    pub(super) fn buttons(&self) -> Buttons {
        self.buttons
    }

    pub(super) fn set_button(&mut self, button: MouseButton, down: bool) {
        self.buttons.set(Buttons::from_button(button), down);
    }

    pub(super) fn selecting(&self) -> Option<NodeId> {
        self.selecting
    }

    pub(super) fn set_selecting(&mut self, id: Option<NodeId>) {
        self.selecting = id;
    }

    pub(super) fn link_hover(&self) -> Option<(NodeId, usize)> {
        self.link_hover
    }

    pub(super) fn set_link_hover(&mut self, link: Option<(NodeId, usize)>) {
        self.link_hover = link;
    }

    pub(super) fn set_link_press(&mut self, link: Option<(NodeId, usize)>) {
        self.link_press = link;
    }

    pub(super) fn take_link_press(&mut self) -> Option<(NodeId, usize)> {
        self.link_press.take()
    }

    /// 左键按下：数连击，记下这一次。`now` 是定时器的时钟：显示链接停着、元素树的帧时间不走的时候也照样走
    pub(super) fn count_click(&mut self, now: f64, point: [f32; 2]) -> u32 {
        self.count(now, point, crate::system::conventions().double_click_distance)
    }

    /// 主手指放上：和 [`Self::count_click`] 同一套，距离放宽一些
    pub(super) fn count_tap(&mut self, now: f64, point: [f32; 2]) -> u32 {
        self.count(now, point, DOUBLE_TAP_DISTANCE)
    }

    fn count(&mut self, now: f64, point: [f32; 2], within: f32) -> u32 {
        let clicks = match self.clicks {
            Some((time, at, count)) if repeated(Some((time, at)), now, point, within) => count + 1,
            _ => 1,
        };
        self.clicks = Some((now, point, clicks));
        clicks
    }

    /// 这次按下不算点按了（手指被拿去滚动、做手势）：紧接着的下一次按下不算连击
    pub(super) fn forget_clicks(&mut self) {
        self.clicks = None;
    }

    /// 上一次左键按下的连击数（松开时的点击带上它）
    pub(super) fn last_clicks(&self) -> u32 {
        self.clicks.map_or(1, |(.., clicks)| clicks)
    }

    pub(super) fn set_middle(&mut self, press: Option<Press>) {
        self.middle = press;
    }

    pub(super) fn take_middle(&mut self) -> Option<Press> {
        self.middle.take()
    }

    pub(super) fn set_control_click(&mut self) {
        self.control_click = true;
    }

    pub(super) fn take_control_click(&mut self) -> bool {
        std::mem::take(&mut self.control_click)
    }

    pub(super) fn drag(&self) -> Option<&Drag> {
        self.drag.as_ref()
    }

    pub(super) fn set_drag(&mut self, drag: Option<Drag>) {
        self.drag = drag;
    }

    pub(super) fn take_drag(&mut self) -> Option<Drag> {
        self.drag.take()
    }

    /// 进行中的捏合手势（取出来改、再放回去）
    pub(super) fn pinch_mut(&mut self) -> &mut Option<Gesture> {
        &mut self.pinch
    }

    /// 进行中的旋转手势（取出来改、再放回去）
    pub(super) fn rotate_mut(&mut self) -> &mut Option<Gesture> {
        &mut self.rotate
    }

    /// 拖动交给了系统、窗口失去焦点：按下的状态清掉，返回收到按压反馈的元素（要通知 false）
    pub(super) fn cancel_press(&mut self) -> Vec<NodeId> {
        self.drag = None;
        self.press = None;
        self.captured = None;
        self.selecting = None;
        self.take_press_feedback()
    }

    pub(super) fn mark_cursor(&mut self) {
        self.cursor_dirty = true;
    }

    pub(super) fn take_cursor_dirty(&mut self) -> bool {
        std::mem::take(&mut self.cursor_dirty)
    }

    // ---- 键盘 ----

    pub(super) fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub(super) fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    pub(super) fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    pub(super) fn set_focused(&mut self, id: Option<NodeId>) {
        self.focused = id;
    }

    pub(super) fn focus_visible(&self) -> bool {
        self.focus_visible
    }

    /// 改焦点环显不显示，返回变了没有
    pub(super) fn set_focus_visible(&mut self, visible: bool) -> bool {
        std::mem::replace(&mut self.focus_visible, visible) != visible
    }

    pub(super) fn keyboard(&self) -> bool {
        self.keyboard
    }

    pub(super) fn set_keyboard(&mut self, keyboard: bool) {
        self.keyboard = keyboard;
    }

    /// 用键盘得到焦点的元素（画焦点环的时候）
    pub(super) fn keyboard_focus(&self) -> Option<NodeId> {
        self.focused.filter(|_| self.focus_visible)
    }

    /// 用键盘得到焦点的元素从 `before` 换了：通知这两个元素（[`crate::Element::on_keyboard_focus`]）
    pub(super) fn keyboard_focus_changed(&self, nodes: &NodeStore, before: Option<NodeId>, callbacks: &mut FocusCallbacks) {
        let after = self.keyboard_focus();
        if before == after {
            return;
        }
        for (id, focused) in [(before, false), (after, true)] {
            let Some(cold) = id.and_then(|id| nodes.cold(id)) else { continue };
            callbacks.extend(cold.handlers.of(EventType::KeyboardFocus, Handler::flag).into_iter().map(|h| (h, focused)));
        }
    }

    /// 焦点句柄交给了哪个元素
    pub(super) fn handle_target(&self, handle: FocusHandle) -> Option<NodeId> {
        self.handles.iter().find(|(h, _)| *h == handle).map(|&(_, id)| id)
    }

    /// 元素的焦点句柄
    pub(super) fn handle_of(&self, id: NodeId) -> Option<FocusHandle> {
        self.handles.iter().find(|(_, node)| *node == id).map(|&(handle, _)| handle)
    }

    /// 元素自己的焦点句柄；没有时是它里面的元素的（数字输入的句柄在中间的输入框上）
    pub(super) fn handle_within(&self, nodes: &NodeStore, id: NodeId) -> Option<FocusHandle> {
        self.handle_of(id).or_else(|| self.handles.iter().find(|(_, node)| nodes.is_within(*node, id)).map(|&(handle, _)| handle))
    }

    pub(super) fn add_handle(&mut self, handle: FocusHandle, id: NodeId) {
        self.handles.push((handle, id));
    }

    /// 按着的键
    pub(super) fn held(&self) -> &[Held] {
        &self.held
    }

    pub(super) fn held_mut(&mut self) -> &mut Vec<Held> {
        &mut self.held
    }

    /// 留到释放树的借用之后调用的焦点处理器
    pub(super) fn defer_callbacks(&mut self, callbacks: FocusCallbacks) {
        self.callbacks.extend(callbacks);
    }

    pub(super) fn take_callbacks(&mut self) -> FocusCallbacks {
        std::mem::take(&mut self.callbacks)
    }

    // ---- 输入法 ----

    /// 正在输入法组字的元素
    pub(super) fn composing(&self) -> Option<NodeId> {
        self.composing
    }

    pub(super) fn set_composing(&mut self, id: Option<NodeId>) {
        self.composing = id;
    }

    pub(super) fn take_composing(&mut self) -> Option<NodeId> {
        self.composing.take()
    }

    /// 平台的输入法要重置
    pub(super) fn request_ime_reset(&mut self) {
        self.ime_reset = true;
    }

    pub(super) fn take_ime_reset(&mut self) -> bool {
        std::mem::take(&mut self.ime_reset)
    }

    // ---- 节点离开 ----

    /// 节点开始退场、浮层开始关闭、节点被卸载：指向这些节点的状态都清掉（它们的处理器已随作用域释放）。
    /// 返回焦点是不是因此清掉了。
    ///
    /// 三种情况清的不完全一样：浮层关闭时焦点由浮层自己还给打开前的元素，拖着文件经过的元素等文件离开时照常通知；
    /// 链接的悬停、按下只在卸载时清（退场、关闭中的文字还画着，悬停的样子等指针移开时照常收回）
    pub(super) fn forget(&mut self, nodes: &NodeStore, leave: Leave) -> bool {
        let gone = |id: NodeId| leave.gone(nodes, id);
        let mut lost_focus = false;
        if !matches!(leave, Leave::Close(_)) && self.focused.is_some_and(gone) {
            self.focused = None;
            lost_focus = true;
        }
        self.hovered.retain(|&id| !gone(id));
        if !matches!(leave, Leave::Close(_)) {
            self.file_hovered.retain(|&id| !gone(id));
            self.external_drag.asked.retain(|&id| !gone(id));
            if self.external_drag.target.is_some_and(|(id, _)| gone(id)) {
                self.external_drag.target = None;
            }
        }
        // 按下的元素没了（按下的元素所在的浮层被关掉也算）：这次按下不再算点击
        if self.press.as_ref().is_some_and(|p| p.path.iter().any(|&id| gone(id))) {
            self.press = None;
        }
        if self.captured.is_some_and(gone) {
            self.captured = None;
        }
        if self.selecting.is_some_and(gone) {
            self.selecting = None;
        }
        self.press_feedback.retain(|&id| !gone(id));
        if let Some(touches) = self.touches.as_deref_mut() {
            touches.forget(nodes, leave);
        }
        if matches!(leave, Leave::Unmount) {
            if self.link_hover.is_some_and(|(id, _)| gone(id)) {
                self.link_hover = None;
            }
            if self.link_press.is_some_and(|(id, _)| gone(id)) {
                self.link_press = None;
            }
        }
        // 拖动、手势、中键：拿到它的元素没了就结束（还没开始的拖动，按下处的路径上没了的元素在仲裁时跳过）
        if self.drag.as_ref().is_some_and(|d| d.node.is_some_and(gone)) {
            self.drag = None;
        }
        if self.pinch.as_ref().is_some_and(|g| g.node.is_some_and(gone)) {
            self.pinch = None;
        }
        if self.rotate.as_ref().is_some_and(|g| g.node.is_some_and(gone)) {
            self.rotate = None;
        }
        if self.middle.as_ref().is_some_and(|p| p.path.iter().any(|&id| gone(id))) {
            self.middle = None;
        }
        self.handles.retain(|(_, id)| !gone(*id));
        // 正在组字的元素没了：平台那边没确定的字也不要了
        if self.composing.is_some_and(gone) {
            self.composing = None;
            self.ime_reset = true;
        }
        lost_focus
    }
}
