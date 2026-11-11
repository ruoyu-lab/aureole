
//! 指针：指针移动和离开、按下和松开（点击、输入目标的插入点与选择、链接）、右键和中键、
//! 拖动和指针捕获（含元素之间的拖放）。约定见 [`super`]，传播见 `dispatch.rs`，拖动开始时谁拿到见 `gesture.rs`，
//! 触控板的捏合和旋转也在那里。
//!
//! 按下、松开、移动、点击都冒泡：交给指针下最内层的元素（指针被捕获时是捕获它的元素），沿逻辑父元素往外传，
//! 禁用的元素跳过。处理器之后执行没被取消的内置行为：按下时关外面的浮层、拿焦点、选字、准备拖动、右键点击；
//! 松开时在"按下和松开都在其下"的最内层元素上合成点击。进入、离开不传播。

use reactive::batch;

use super::super::interaction::{Drag, VELOCITY_WINDOW, distance, drag_threshold, sub};
use super::super::router::Press;
use super::super::{NodeId, Tree, TreeRef, drag_drop};
use super::HitPath;
use super::dispatch::{Options, deliver, dispatch};
use super::gesture::{Arbitration, arbitrate, cancel_pads, cancel_seen};
use super::{run_focus_callbacks, window_drag};
use super::super::scrolling::PanTarget;
use super::touch::{begin_pan, cancel_fingers};
use crate::event::{ClickSource, Builtin, ClickEvent, PropagationPhase, PointerEvent, PointerEventKind};
use crate::handlers::{EventType, Handler};
use crate::input::{GesturePhase, MouseButton, PointerKind};
use crate::overlay::Dismiss;
use crate::timer;

/// 指针事件、点击传播时跳过禁用的元素
pub(super) const POINTER: Options = Options { skip_disabled: true, captured: false };

/// 指针移动或离开窗口（`None`）：更新悬停状态，触发进入和离开（不传播），再把移动交给指针下最内层的元素冒泡；
/// 正在输入框或可选中的文字上拖动时扩展选区
pub(crate) fn pointer_move(tree: &TreeRef, point: Option<[f32; 2]>) {
    if hover(tree, point) {
        move_event(tree);
    }
}

/// 指针没动、它下面的东西变了（重新布局、视口滚过、拖动结束、拖放取消）：按此刻的指针刷新悬停状态，
/// 发进入和离开；不发移动（指针没有移动，处理器不该收到一次假的移动）
pub(crate) fn refresh_hover(tree: &TreeRef) {
    let pointer = tree.borrow().router.pointer();
    hover(tree, pointer);
}

/// 指针在 `point`（`None` 是离开了窗口）：更新悬停状态，触发进入和离开。拖动中返回 `false`（位置交给拿到拖动的元素，
/// 悬停状态不变；指针没动、内容滚过去时重新找放置目标）
pub(super) fn hover(tree: &TreeRef, point: Option<[f32; 2]>) -> bool {
    // 按在窗口的拖动区域上越过了阈值：交给系统拖动窗口
    if window_drag::pointer_moved(tree, point) {
        return false;
    }
    // 拖动中（指针被捕获）：位置交给被拖的元素，悬停状态不变
    if drag_move(tree, point) {
        return false;
    }
    let (left, entered) = {
        let mut t = tree.borrow_mut();
        t.router.set_pointer(point);
        if let (Some(id), Some(p)) = (t.router.selecting(), point) {
            t.target_pointer(id, PointerEventKind::Move, p, 0);
        }
        t.refresh_worlds();
        t.drag_thumb();
        // 指针下的元素先放在栈上，再抄进悬停路径原有的缓冲（指针移动不分配）
        let mut path = HitPath::new();
        if let Some(p) = point {
            t.hit_into(p, &mut path);
        }
        let left = t.crossing(t.router.hovered().iter().filter(|id| !path.contains(id)).copied(), false);
        let entered = t.crossing(path.iter().filter(|id| !t.router.hovered().contains(id)).copied(), true);
        t.router.set_hovered_from(&path);
        t.hover_link();
        // 手指的滚动条只是指示：靠近边缘不显示
        if t.router.device() != PointerKind::Touch {
            t.reveal_bars();
        }
        (left, entered)
    };
    // 处理器在释放树的借用之后执行，里面可以随意写信号
    if !left.is_empty() || !entered.is_empty() {
        batch(|| {
            for crossing in left.iter().chain(&entered) {
                crossing.run();
            }
        });
    }
    true
}

/// 一个元素的进入或离开：`on_pointer_enter` / `on_pointer_leave` 和 `on_hover` 的简写
pub(in crate::tree) struct Crossing {
    pointer: Vec<crate::handlers::On<PointerEvent>>,
    hover: Vec<std::rc::Rc<dyn Fn(bool)>>,
    event: PointerEvent,
    inside: bool,
}

impl Crossing {
    fn run(&self) {
        self.pointer.iter().for_each(|h| h(&self.event));
        self.hover.iter().for_each(|h| h(self.inside));
    }
}

impl Tree {
    /// 这些元素进入（`inside`）或离开：有处理器的才记下
    fn crossing(&self, ids: impl Iterator<Item = NodeId>, inside: bool) -> Vec<Crossing> {
        let kind = if inside { EventType::PointerEnter } else { EventType::PointerLeave };
        let mut out = Vec::new();
        for id in ids {
            let Some(handlers) = self.handlers(id) else { continue };
            if !handlers.listens(kind) && !handlers.listens(EventType::Hover) {
                continue;
            }
            let point = self.router.pointer().unwrap_or_default();
            let event_kind = if inside { PointerEventKind::Enter } else { PointerEventKind::Leave };
            let event = self.pointer_event(id, event_kind, point, None, 0);
            out.push(Crossing { pointer: handlers.of(kind, Handler::pointer), hover: handlers.of(EventType::Hover, Handler::flag), event, inside });
        }
        out
    }

    /// 指针事件的目标：捕获了指针的元素，否则是指针下最内层的元素
    pub(super) fn pointer_target(&self) -> Option<NodeId> {
        self.router.captured().filter(|&id| self.nodes.contains_key(id)).or_else(|| self.router.hovered().last().copied())
    }

    /// 从 `target` 出发的一个指针事件（位置是此刻的指针）
    fn routed_pointer(&self, kind: PointerEventKind, button: Option<MouseButton>, clicks: u32) -> Option<(super::dispatch::RoutePath, PointerEvent)> {
        let target = self.pointer_target()?;
        let point = self.router.pointer()?;
        Some((self.route_path(target), self.primary_event(kind, point, button, clicks)))
    }

    /// 按下处和松开处都在其下的最内层元素（两条从外到内的路径的最长公共前缀的最后一个）
    fn common_target(&self, down: &[NodeId]) -> Option<NodeId> {
        down.iter().zip(self.router.hovered()).take_while(|(a, b)| a == b).last().map(|(&id, _)| id)
    }
}

/// 指针移动之后（悬停已经更新）：交给捕获了指针的元素或者指针下最内层的元素，冒泡
fn move_event(tree: &TreeRef) {
    let (path, mut event) = {
        let t = tree.borrow();
        let (Some(target), Some(point)) = (t.pointer_target(), t.router.pointer()) else { return };
        let path = t.route_path(target);
        // 路径上没人听（指针移动最常见的情况）：只读一遍位掩码，不造事件（造事件要读时钟）
        if !path.iter().any(|&id| t.handlers(id).is_some_and(|h| h.listens_in(EventType::PointerMove, PropagationPhase::Target))) {
            return;
        }
        let event = t.primary_event(PointerEventKind::Move, point, None, 0);
        (path, event)
    };
    dispatch(tree, &path, EventType::PointerMove, &mut event, POINTER, |_, _, _, _| {});
}

/// 按下之前（在哪都算）：放弃正在组的字，平台的输入法也重置；不再画焦点环；记下按着的键
pub(super) fn before_press(tree: &TreeRef, button: MouseButton) {
    tree.borrow_mut().cancel_composition(true);
    hide_focus_ring(tree);
    tree.borrow_mut().router.set_button(button, true);
}

/// 分发按下：捕获了指针的元素记下来。返回路径、处理器取消了的内置行为、传到了路径上的第几个
pub(super) fn dispatch_down(tree: &TreeRef, button: MouseButton, clicks: u32) -> (super::dispatch::RoutePath, Builtin, usize) {
    let routed = tree.borrow().routed_pointer(PointerEventKind::Down, Some(button), clicks);
    match routed {
        Some((path, mut event)) => {
            let outcome = dispatch(tree, &path, EventType::PointerDown, &mut event, POINTER, |_, _, _, _| {});
            if let Some(id) = outcome.captured {
                tree.borrow_mut().router.set_captured(Some(id));
            }
            (path, event.flow().prevented(), outcome.reached)
        }
        None => (Default::default(), Builtin::empty(), 0),
    }
}

/// 收按压反馈的元素：按下传到的元素（停止传播之前）里有 on_press 的；和点击一样到浮层为止（按菜单项时打开菜单的按钮不显示按下）
pub(super) fn press_targets(tree: &TreeRef, path: &[NodeId], reached: usize) -> Vec<NodeId> {
    let t = tree.borrow();
    let reached = reached.min(t.within_overlay(path));
    path[..reached].iter().copied().filter(|&id| t.listens(id, EventType::Press) && !t.disabled(id)).collect()
}

/// 主指针按下的按压反馈：记下收到反馈的元素，通知它们 true
pub(super) fn press_feedback(tree: &TreeRef, path: &[NodeId], reached: usize) {
    let feedback = press_targets(tree, path, reached);
    let handlers: Vec<_> = feedback.iter().flat_map(|&id| tree.borrow().handlers_of(id, EventType::Press, Handler::flag)).collect();
    tree.borrow_mut().router.set_press_feedback(feedback);
    if !handlers.is_empty() {
        batch(|| handlers.iter().for_each(|h| h(true)));
    }
}

/// 通知这些元素按下、松开（`on_press`）
pub(super) fn notify_press(tree: &TreeRef, ids: &[NodeId], down: bool) {
    let handlers: Vec<_> = ids.iter().flat_map(|&id| tree.borrow().handlers_of(id, EventType::Press, Handler::flag)).collect();
    if !handlers.is_empty() {
        batch(|| handlers.iter().for_each(|h| h(down)));
    }
}

/// 按下一个键（左、右、中）：分发按下，然后执行没被取消的内置行为
fn press(tree: &TreeRef, button: MouseButton) {
    before_press(tree, button);
    // 按在滚动条上：拖滚动条，不算点击（滚动条不是元素，事件不往下传；外面的浮层照常请求关闭）
    if button == MouseButton::Left && tree.borrow_mut().begin_thumb_drag() {
        dismiss_outside(tree);
        return;
    }
    // 左键数连击（双击处理器、文字选词）
    let clicks = if button == MouseButton::Left {
        let mut t = tree.borrow_mut();
        match t.router.pointer() {
            Some(point) => t.router.count_click(timer::now(), point),
            None => 1,
        }
    } else {
        1
    };
    // macOS（平台惯例）：按住 Control 点左键是右键点击（指针下有元素接右键时），这次左键松开不再当左键
    let control = button == MouseButton::Left
        && crate::system::conventions().treats_control_click_as_secondary
        && tree.borrow().router.modifiers().has_control()
        && tree.borrow().innermost_listening(tree.borrow().router.hovered(), EventType::SecondaryClick).is_some();
    if control {
        tree.borrow_mut().router.set_control_click();
    }
    let (path, prevented, reached) = dispatch_down(tree, button, clicks);
    let hovered = tree.borrow().router.hovered().to_vec();
    if button == MouseButton::Left && !control {
        press_feedback(tree, &path, reached);
    }
    // 内置行为：关外面的浮层（在处理器之后，处理器可以取消）
    if !prevented.contains(Builtin::DISMISS) {
        dismiss_outside(tree);
    }
    match button {
        MouseButton::Left if control => {
            context_menu(tree, prevented);
        }
        MouseButton::Left => {
            tree.borrow_mut().router.set_press(Some(Press { path: hovered.clone(), prevented }));
            // 按在窗口的拖动区域上：不动焦点，和按在系统的标题栏上一样
            let window_area = window_drag::is_press_in_area(tree, prevented);
            focus_and_select(tree, if window_area { prevented | Builtin::FOCUS | Builtin::TEXT_SELECT } else { prevented }, clicks);
            // 准备拖动、双击
            let double = after_press(tree, clicks, prevented);
            if window_area {
                window_drag::pressed(tree, clicks, double);
            }
        }
        MouseButton::Right => {
            let clicked = context_menu(tree, prevented);
            // 按在窗口的拖动区域上：松开时弹系统的窗口菜单（按下、右键点击的处理器都能取消）
            window_drag::secondary_pressed(tree, prevented | clicked);
        }
        MouseButton::Middle => tree.borrow_mut().router.set_middle(Some(Press { path: hovered, prevented })),
        MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => {}
    }
}

/// 拿焦点、选字（左键按下的内置行为）：焦点给指针下最内层能获得焦点的元素（输入框、可选中的文字、多行输入框的视口里的输入框，
/// 或者 focusable），按在别处时失去焦点；是文字的话由它放置插入点、选择。按在链接上时焦点给链接所在的文字、落在这个链接上
pub(super) fn focus_and_select(tree: &TreeRef, prevented: Builtin, clicks: u32) {
    let callbacks = {
        let mut t = tree.borrow_mut();
        let (target, callbacks) = if prevented.contains(Builtin::FOCUS) { (t.router.focused(), Vec::new()) } else { t.focus_on_press() };
        t.press_link();
        let target = target.filter(|&id| t.is_text(id) && t.router.focused() == Some(id) && !prevented.contains(Builtin::TEXT_SELECT));
        if let (Some(id), Some(point)) = (target, t.router.pointer()) {
            // 输入目标放插入点、选字；之后的拖动、松开都交给它
            t.target_pointer(id, PointerEventKind::Down, point, clicks);
            t.router.set_selecting(Some(id));
        }
        callbacks
    };
    run_focus_callbacks(callbacks);
}

/// 右键（和 macOS 上的 Control+左键、手指长按）按下的内置行为：右键点击，交给指针下最内层的元素冒泡。
/// 返回右键点击的处理器取消了哪些内置行为（没有合成时是空的）
pub(super) fn context_menu(tree: &TreeRef, prevented: Builtin) -> Builtin {
    if prevented.contains(Builtin::CONTEXT_MENU) {
        return Builtin::empty();
    }
    let target = {
        let t = tree.borrow();
        t.pointer_target().filter(|_| t.router.pointer().is_some())
    };
    match target {
        Some(target) => click(tree, target, EventType::SecondaryClick, MouseButton::Right, 1),
        None => Builtin::empty(),
    }
}

/// 窗口的拖动区域按着没拖：松开、取消时作废
fn clear_window_drag(tree: &TreeRef) {
    tree.borrow_mut().window_areas.clear_pending();
}

/// 在 `target` 上合成一次点击，冒泡。输入框处理自己的点击（放插入点、选字），不再往外传。
/// 返回处理器取消了哪些内置行为
pub(super) fn click(tree: &TreeRef, target: NodeId, kind: EventType, button: MouseButton, clicks: u32) -> Builtin {
    let event = tree.borrow().click_event(target, ClickSource::Pointer, button, clicks);
    send_click(tree, target, kind, event)
}

/// 把造好的点击从 `target` 冒泡出去，见 [`click`]
pub(super) fn send_click(tree: &TreeRef, target: NodeId, kind: EventType, mut event: ClickEvent) -> Builtin {
    let path = tree.borrow().route_path(target);
    dispatch(tree, &path, kind, &mut event, POINTER, |tree, id, phase, event: &ClickEvent| {
        if phase != PropagationPhase::Capture && tree.borrow().wants_text_input(id) {
            event.stop_propagation();
        }
    });
    event.flow().prevented()
}

/// 松开一个键：结束拖动，分发松开，合成点击（左键、中键）
pub(super) fn release(tree: &TreeRef, button: MouseButton) {
    let control = button == MouseButton::Left && tree.borrow_mut().router.take_control_click();
    // 结束拖动（拖动开始时已经取消了这次点击）
    if button == MouseButton::Left {
        clear_window_drag(tree);
        before_release(tree);
    }
    if button == MouseButton::Right {
        window_drag::secondary_released(tree);
    }
    let routed = tree.borrow().routed_pointer(PointerEventKind::Up, Some(button), tree.borrow().router.last_clicks());
    let prevented = match routed {
        Some((path, mut event)) => {
            dispatch(tree, &path, EventType::PointerUp, &mut event, POINTER, |_, _, _, _| {});
            event.flow().prevented()
        }
        None => Builtin::empty(),
    };
    let press = {
        let mut t = tree.borrow_mut();
        t.router.set_button(button, false);
        t.router.set_captured(None);
        match button {
            MouseButton::Left => {
                // 拖着选字的输入目标：松开
                if let (Some(id), Some(point)) = (t.router.selecting(), t.router.pointer()) {
                    let clicks = t.router.last_clicks();
                    t.target_pointer(id, PointerEventKind::Up, point, clicks);
                }
                t.router.set_selecting(None);
                t.end_thumb_drag();
                t.router.take_press()
            }
            MouseButton::Middle => t.router.take_middle(),
            _ => None,
        }
    };
    if control {
        return;
    }
    let (feedback, link) = if button == MouseButton::Left {
        let mut t = tree.borrow_mut();
        // 松开时还在按下的链接上（没拖出选区）：打开它
        let link = t.release_link();
        let feedback: Vec<_> = t.router.take_press_feedback().into_iter().flat_map(|id| t.handlers_of(id, EventType::Press, Handler::flag)).collect();
        (feedback, link)
    } else {
        (Vec::new(), None)
    };
    if !feedback.is_empty() {
        batch(|| feedback.iter().for_each(|h| h(false)));
    }
    // 点击：按下和松开都在其下的最内层元素
    let target = press
        .filter(|p| !(p.prevented | prevented).contains(Builtin::ACTIVATE))
        .and_then(|p| tree.borrow().common_target(&p.path));
    if let Some(target) = target {
        let (kind, clicks) = match button {
            MouseButton::Middle => (EventType::MiddleClick, 1),
            _ => (EventType::Click, tree.borrow().router.last_clicks()),
        };
        click(tree, target, kind, button, clicks);
    }
    if let Some(link) = link {
        batch(|| link.run());
    }
}

/// 窗口的鼠标按键：按下、松开都冒泡；侧键和其他键也分发，没有内置行为
pub(crate) fn mouse_button(tree: &TreeRef, button: MouseButton, pressed: bool) {
    if pressed {
        press(tree, button);
    } else {
        release(tree, button);
    }
}

/// 不再画焦点环（任何键按下之前）
fn hide_focus_ring(tree: &TreeRef) {
    let callbacks = {
        let mut t = tree.borrow_mut();
        t.router.set_keyboard(false);
        let mut callbacks = Vec::new();
        if t.router.focus_visible() {
            let ring = t.router.keyboard_focus();
            t.router.set_focus_visible(false);
            t.paint.invalidate();
            t.router.keyboard_focus_changed(&t.nodes, ring, &mut callbacks);
        }
        callbacks
    };
    run_focus_callbacks(callbacks);
}

/// 在浮层外面按下：让这些浮层关闭（交给它们的 on_dismiss）
pub(super) fn dismiss_outside(tree: &TreeRef) {
    let dismiss = {
        let t = tree.borrow();
        t.outside_dismissals(t.router.hovered())
    };
    if !dismiss.is_empty() {
        batch(|| dismiss.iter().for_each(|d| d(Dismiss::Outside)));
    }
}

/// 左键按下之后（拿焦点、选字已经做过）：双击处理器、准备拖动。返回按下和双击的处理器取消了哪些内置行为
fn after_press(tree: &TreeRef, clicks: u32, prevented: Builtin) -> Builtin {
    let double = {
        let mut t = tree.borrow_mut();
        let Some(point) = t.router.pointer() else { return prevented };
        let now = timer::now();
        // 拖动：按在输入框里（选文字）、滚动条上时不拖
        t.router.set_drag(None);
        // 路径上（从按下的元素往外）有拖动的处理器，或者能拖放、能拖到应用外面的元素时，记下这次按下，越过阈值时仲裁
        let path = match t.pointer_target() {
            Some(target) if t.router.selecting().is_none() && !t.dragging_thumb() && !prevented.contains(Builtin::DRAG) => t.route_path(target),
            _ => Default::default(),
        };
        if path.iter().any(|&id| t.drag_candidate(id) || t.handlers(id).is_some_and(|h| h.listens_in(EventType::Drag, PropagationPhase::Target))) {
            t.router.set_drag(Some(Drag {
                node: None,
                phase: PropagationPhase::Target,
                path,
                button: MouseButton::Left,
                start: point,
                last: point,
                started: false,
                samples: vec![(now, point)],
                drag_drop: false,
                out: false,
            }));
        }
        (clicks == 2).then(|| t.pointer_target()).flatten()
    };
    match double {
        Some(target) => prevented | click(tree, target, EventType::DoubleClick, MouseButton::Left, clicks),
        None => prevented,
    }
}

impl Tree {
    /// 不表态时能默认拿到拖动的元素：有冒泡阶段的 `on_drag`，或者能拖放、能拖到应用外面
    pub(super) fn drag_candidate(&self, id: NodeId) -> bool {
        self.listens(id, EventType::Drag) || self.node_input(id).and_then(|i| i.drag_drop()).is_some_and(|d| d.payload.is_some() || d.drag_out.is_some())
    }
}

/// 拖动中指针移动（指针被捕获）：位置交给拿到拖动的元素，悬停状态不变；返回这次移动是不是被拖动吃掉了。
/// 还没越过阈值时照常处理移动；越过阈值时仲裁谁拿到拖动（没有元素拿到时这次按下照常，松开算点击）；
/// 拖动中移出窗口（`None`）时等后面窗口外的位置
fn drag_move(tree: &TreeRef, point: Option<[f32; 2]>) -> bool {
    let call = {
        let mut t = tree.borrow_mut();
        let Some(mut drag) = t.router.take_drag() else { return false };
        let Some(point) = point else {
            let started = drag.started;
            t.router.set_drag(Some(drag));
            return started;
        };
        if !drag.started {
            if distance(point, drag.start) < drag_threshold() {
                t.router.set_drag(Some(drag));
                return false;
            }
            t.router.set_pointer(Some(point));
            drop(t);
            return start_drag(tree, drag, point, None);
        }
        t.router.set_pointer(Some(point));
        if point == drag.last {
            // 指针没动、内容滚过去了（自动滚动）：放置目标要重新找
            let drag_drop = t.drag_drop.dragging();
            t.router.set_drag(Some(drag));
            drop(t);
            if drag_drop && drag_drop::update(tree) {
                hand_off(tree);
            }
            return true;
        }
        let now = timer::now();
        drag.samples.retain(|&(time, _)| now - time <= VELOCITY_WINDOW);
        drag.samples.push((now, point));
        let delta = sub(point, drag.last);
        drag.last = point;
        let event = t.drag_event(&drag, GesturePhase::Move, delta, now);
        let (owner, drag_drop) = (drag.node.map(|node| (node, drag.phase)), drag.drag_drop);
        t.router.set_drag(Some(drag));
        t.router.mark_cursor();
        (owner, event, drag_drop)
    };
    let (owner, mut event, drag_drop) = call;
    if let Some((owner, at)) = owner {
        deliver(tree, owner, EventType::Drag, at, &mut event);
    }
    if drag_drop && drag_drop::update(tree) {
        hand_off(tree);
    }
    true
}

/// 越过阈值：`Start` 沿按下处的路径走捕获 → 目标 → 冒泡，仲裁谁拿到拖动（见 `gesture.rs`）。
/// 拿到的元素独占之后的移动、松开；它里面的元素收到指针取消、按压反馈收回；收到过 `Start` 没拿到的元素收到 `Cancel`。
/// 返回这次移动是不是被拖动吃掉了（没有元素拿到时不是）。
///
/// `pan` 是手指拖动时要滚的东西和内容从哪开始跟手：它也参加仲裁，轮到它（目标和冒泡阶段里面的元素都没要，它自己也没有拖动的处理器、没人明说要）时
/// 这根手指拿去滚动
pub(super) fn start_drag(tree: &TreeRef, mut drag: Drag, point: [f32; 2], pan: Option<(PanTarget, [f32; 2])>) -> bool {
    let now = timer::now();
    drag.samples.retain(|&(time, _)| now - time <= VELOCITY_WINDOW);
    drag.samples.push((now, point));
    // `Start` 的位移从按下处算起（包括越过阈值的那一小段）
    let delta = sub(point, drag.start);
    drag.last = point;
    let mut event = tree.borrow().drag_event(&drag, GesturePhase::Start, delta, now);
    let path = std::mem::take(&mut drag.path);
    let pans = |id: NodeId| pan.is_some_and(|(target, _)| target.node() == id);
    let Arbitration { owner, claimed, seen } = arbitrate(tree, &path, EventType::Drag, &mut event, |t, id| t.drag_candidate(id) || pans(id));
    let cancel = || tree.borrow().drag_event(&drag, GesturePhase::Cancel, [0.0; 2], now);
    let Some((owner, at)) = owner.filter(|&(id, _)| tree.borrow().routable(id)) else {
        // 没有元素要：这次按下照常（松开时算点击）
        cancel_seen(tree, &seen, EventType::Drag, cancel);
        return false;
    };
    cancel_seen(tree, &seen, EventType::Drag, cancel);
    if let Some((target, from)) = pan
        && pans(owner)
        && !claimed
        && !tree.borrow().drag_candidate(owner)
    {
        begin_pan(tree, target, from, point);
        return true;
    }
    let (drag_drop, out) = {
        let t = tree.borrow();
        t.node_input(owner).and_then(|i| i.drag_drop()).map_or((false, false), |d| (d.payload.is_some(), d.drag_out.is_some()))
    };
    drag.node = Some(owner);
    drag.phase = at;
    drag.started = true;
    drag.drag_drop = drag_drop;
    drag.out = out;
    {
        let mut t = tree.borrow_mut();
        // 这次按下不再算点击
        t.router.set_press(None);
        t.router.mark_cursor();
    }
    cancel_inside(tree, &path, owner);
    // 只能拖到应用外面：越过阈值就交给系统，这次按下到此为止（系统的拖动会话收走了松开），收到过 `Start` 的它自己收到 `Cancel`
    if out && !drag_drop {
        let mut cancel = tree.borrow().drag_event(&drag, GesturePhase::Cancel, [0.0; 2], now);
        deliver(tree, owner, EventType::Drag, at, &mut cancel);
        end_press(tree);
        drag_drop::begin_out(tree, owner, point, None);
        return true;
    }
    let start = drag.start;
    tree.borrow_mut().router.set_drag(Some(drag));
    if drag_drop {
        drag_drop::start(tree, owner, start);
        if drag_drop::update(tree) {
            hand_off(tree);
        }
    }
    true
}

/// 拖动交给了 `owner`：按下处的路径上它里面的元素失去这次按下（`on_press(false)`、`on_pointer_cancel` 冒泡到它为止）。
/// 它自己和外面的元素不变（滑块松手时照常收到 `on_press(false)`）
fn cancel_inside(tree: &TreeRef, path: &[NodeId], owner: NodeId) {
    let Some(inside) = path.iter().position(|&id| id == owner).filter(|&i| i > 0) else { return };
    let (feedback, routed) = {
        let mut t = tree.borrow_mut();
        let lost: Vec<NodeId> = {
            let t = &mut *t;
            let nodes = &t.nodes;
            t.router.press_feedback_mut().extract_if(.., |id| *id != owner && nodes.is_within(*id, owner)).collect()
        };
        let feedback: Vec<_> = lost.into_iter().flat_map(|id| t.handlers_of(id, EventType::Press, Handler::flag)).collect();
        let routed = t.router.pointer().map(|point| t.primary_event(PointerEventKind::Cancel, point, None, 0));
        (feedback, routed)
    };
    if !feedback.is_empty() {
        batch(|| feedback.iter().for_each(|h| h(false)));
    }
    if let Some(mut event) = routed {
        dispatch(tree, &path[..inside], EventType::PointerCancel, &mut event, POINTER, |_, _, _, _| {});
    }
}

/// 拖动交给了系统（拖到应用外面）：按下的状态清掉，按压反馈收回（系统的会话收走了松开）
pub(super) fn end_press(tree: &TreeRef) {
    let press = {
        let mut t = tree.borrow_mut();
        t.router.cancel_press().into_iter().flat_map(|id| t.handlers_of(id, EventType::Press, Handler::flag)).collect::<Vec<_>>()
    };
    if !press.is_empty() {
        batch(|| press.iter().for_each(|h| h(false)));
    }
}

/// 在应用里拖着元素出了窗口（能拖到应用外面，或者指针下面是同一个应用的另一个窗口）：应用里的拖放取消（影子直接去掉），
/// 交给系统接着拖，带着拖放的数据
pub(crate) fn hand_off(tree: &TreeRef) {
    let Some((node, point, data)) = ({
        let t = tree.borrow();
        t.router.drag().and_then(Drag::owner).zip(t.router.pointer()).map(|(node, point)| (node, point, t.drag_drop.data()))
    }) else {
        return;
    };
    finish_drag_drop(tree, Finish::HandOff);
    end_press(tree);
    drag_drop::begin_out(tree, node, point, data);
}

/// 元素之间的拖放怎么结束
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    /// 松开
    Release,
    /// Esc、窗口失去焦点
    Cancel,
    /// 拖出窗口，交给系统
    HandOff,
}

/// 结束元素之间的拖放：松开时交给目标（接住了是 `End`，没接住是 `Cancel`），拿到拖动的元素的 on_drag 收到结束
fn finish_drag_drop(tree: &TreeRef, how: Finish) {
    let accepted = match how {
        Finish::Release => drag_drop::drop_on_target(tree),
        Finish::Cancel => {
            drag_drop::cancel(tree, false);
            false
        }
        Finish::HandOff => {
            drag_drop::cancel(tree, true);
            false
        }
    };
    let phase = if accepted { GesturePhase::End } else { GesturePhase::Cancel };
    end_drag(tree, phase);
}

/// 结束拿到了的拖动：拿到它的元素收到 `End` 或 `Cancel`（目标阶段）
fn end_drag(tree: &TreeRef, phase: GesturePhase) {
    let call = {
        let mut t = tree.borrow_mut();
        let Some(drag) = t.router.take_drag() else { return };
        let Some(owner) = drag.owner() else { return };
        t.router.mark_cursor();
        (owner, drag.phase, t.drag_event(&drag, phase, [0.0; 2], timer::now()))
    };
    let (owner, at, mut event) = call;
    deliver(tree, owner, EventType::Drag, at, &mut event);
}

/// 元素之间拖放时按了 Esc：取消，影子飞回去（指针仍被按着，之后的移动不再算拖动，松开也不算点击）
pub(super) fn cancel_drag_drop(tree: &TreeRef) {
    finish_drag_drop(tree, Finish::Cancel);
    refresh_hover(tree);
}

/// 左键松开之前：结束拖动（之后按松开时的位置刷新悬停，不发移动）
fn before_release(tree: &TreeRef) {
    if tree.borrow().drag_drop.dragging() {
        finish_drag_drop(tree, Finish::Release);
        refresh_hover(tree);
        return;
    }
    if !tree.borrow().router.drag().is_some_and(|d| d.started) {
        tree.borrow_mut().router.set_drag(None);
        return;
    }
    end_drag(tree, GesturePhase::End);
    refresh_hover(tree);
}

/// 窗口失去焦点等：手指都算系统收走了；按着的键收到取消（交给捕获了指针的元素或者按下处最内层的元素，冒泡），
/// 进行中的拖动和手势以 `Cancel` 结束，按下的状态清掉
pub(crate) fn cancel_pointer(tree: &TreeRef) {
    cancel_fingers(tree);
    cancel_press_state(tree);
    cancel_pads(tree);
}

/// 主指针的这次按下到此为止：进行中的拖放、拖动以 `Cancel` 结束，捕获了指针的元素或者按下处最内层的元素收到取消（冒泡），状态清掉
pub(super) fn cancel_press_state(tree: &TreeRef) {
    clear_window_drag(tree);
    if tree.borrow().drag_drop.dragging() {
        finish_drag_drop(tree, Finish::Cancel);
    }
    let routed = {
        let t = tree.borrow();
        let target = t.router.captured().or_else(|| t.router.press().and_then(|p| p.path.last().copied())).filter(|&id| t.nodes.contains_key(id));
        target.zip(t.router.pointer()).map(|(target, point)| (t.route_path(target), t.primary_event(PointerEventKind::Cancel, point, None, 0)))
    };
    if let Some((path, mut event)) = routed {
        dispatch(tree, &path, EventType::PointerCancel, &mut event, POINTER, |_, _, _, _| {});
    }
    end_drag(tree, GesturePhase::Cancel);
    {
        let mut t = tree.borrow_mut();
        t.router.set_drag(None);
        t.router.set_press(None);
        t.router.set_captured(None);
        t.router.set_selecting(None);
        t.router.set_middle(None);
        // 拖着的滚动条松开（否则它一直显示着）
        t.end_thumb_drag();
    }
}
