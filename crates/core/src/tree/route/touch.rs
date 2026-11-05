//! 触摸屏：手指怎么变成指针事件。约定见 [`super`]，传播见 `dispatch.rs`，拖动见 `pointer.rs`。
//!
//! **第一根手指是主指针**（屏幕上没有别的手指、鼠标没按着时放上的）：和鼠标走同一套状态（悬停、按下、捕获、拖动、
//! 点击合成），设备和编号记在路由器上，事件带着它们。和鼠标不一样的地方：
//!
//! - 没有悬停：手指放上时才发进入（不发移动），抬起后发离开，抬起后没有指针位置；
//! - 移动超过 [`TOUCH_SLOP`] 才开始拖动（手指按下时会抖，鼠标是 4 逻辑像素）；要滚的视口也参加这次仲裁（见下面）；
//! - 拿焦点、放插入点等到点按完成（松开）时才做：按下时还不知道这根手指是不是要滚动。所以输入框里点一下放插入点，
//!   拖动是滚内容（多行输入框是视口，单行输入框自己滚；滚不动时交给外面的视口），不选字；选字用长按（选词）；
//! - 不拖滚动条（滚动条只是指示），指针靠近边缘也不显示滚动条；
//! - 按住不动到平台惯例的时长（[`crate::Conventions::long_press_duration`]）算长按：路径上有 `on_secondary_click` 时合成右键点击
//!   （`on_pointer_down` 里取消 `CONTEXT_MENU` 就不弹），按在文字上时选词；做了其中一样这根手指就被拿走了（按下处收到取消，抬起不算点击）。
//!
//! **别的手指**各有各的按下目标、捕获、按压反馈（[`Finger`]），互不干扰：照样捕获 → 目标 → 冒泡、合成点击；
//! 不动悬停和焦点，不开始拖动。
//!
//! **滚动**：主手指越过阈值时，按下处最内层能往这个方向滚的视口（选法和滚轮一样）也参加拖动的仲裁，排在它里面有 `on_drag` 的元素之后。
//! 轮到它时手指拿去滚动（`tree/scrolling.rs` 的 `Pan`）：按下处的整条路径收到取消、按压反馈收回、悬停离开，之后的移动只改偏移，
//! 抬起时带着速度滑出去。物理和触控板是同一份，也交给渲染线程。处理器在 `on_pointer_down` 里取消 `SCROLL`、捕获指针时不滚。
//! 越过阈值时没有能滚的（手指先往不能滚的方向走了），之后的移动接着找。手指放上时它下面滑着的视口停住，这一下不算点按。
//!
//! **两根手指**（[`Pair`]）：屏幕上正好有两根没被拿走的手指、它们按下处有公共的元素、路径上有人听捏合或旋转时开始识别。
//! 距离变了 [`PAIR_SLOP`] 是捏合，转过的弧长到了 [`PAIR_SLOP`] 是旋转，各自独立开始，`Start` 走和触控板一样的仲裁（`gesture.rs`），
//! 目标是公共的最内层元素，位置是两根手指的中点。有元素拿到时两根手指都被拿走：进行中的拖动以 `Cancel` 结束，按下处的路径收到取消。
//! 没有元素拿到时手指该干什么还干什么。手指抬起手势 `End`，系统收走 `Cancel`；留在屏幕上的那根手指等它自己抬起。
//! 已经在滚动的手指不参加：第二根手指放上时只是一根别的手指。
//!
//! 一根手指被别的东西拿走（滚动、手势、系统取消）以后不再发事件，等它抬起。
//! 鼠标的路径不碰这里的状态：主指针是手指时窗口宿主不把鼠标事件交进来。

use std::f32::consts::{PI, TAU};
use std::rc::Rc;

use reactive::batch;
use smallvec::SmallVec;

use super::super::interaction::{Drag, distance, sub};
use super::super::scrolling::PanTarget;
use super::super::router::{Leave, Press};
use super::super::{NodeId, NodeStore, Tree, TreeRef};
use super::HitPath;
use super::dispatch::{RoutePath, dispatch};
use super::gesture::{Pad, anyone_listens, pad_start, pad_step};
use super::window_drag;
use super::pointer::{
    POINTER, before_press, cancel_press_state, click, context_menu, dismiss_outside, dispatch_down, focus_and_select, notify_press, press_targets, press_feedback, hover,
    pointer_move, release, send_click, start_drag,
};
use crate::event::{Builtin, ClickSource, PinchEvent, PointerEvent, PointerEventKind, PropagationPhase, RotateEvent};
use crate::handlers::EventType;
use crate::input::{Buttons, GesturePhase, MouseButton, PointerId, PointerKind};
use crate::timer::{self, Timer};

/// 手指按下后移动超过这么远（逻辑像素）才算拖动：手指按下时会抖
pub(super) const TOUCH_SLOP: f32 = 8.0;
/// 两根手指的距离变了这么多才算捏合，转过的弧长（两根手指各走的距离）到了这么多才算旋转，逻辑像素
const PAIR_SLOP: f32 = 8.0;

/// 屏幕上的一根手指
pub(in crate::tree) struct Finger {
    id: PointerId,
    /// 最新的位置，窗口坐标
    point: [f32; 2],
    /// 上一次的位置：开始滚动时内容从这里开始跟手
    previous: [f32; 2],
    /// 按下的位置
    start: [f32; 2],
    /// 按下的时间（定时器的时钟）：拖动的速度从这里算起
    time: f64,
    /// 按下时手指下从外到内的元素：松开时合成点击；元素没了时清空（不再算点击）
    path: Vec<NodeId>,
    /// 按下的处理器取消了的内置行为
    prevented: Builtin,
    /// 处理器捕获了这根手指（主手指的记在路由器上）
    captured: Option<NodeId>,
    /// 收到按压反馈的元素（主手指的记在路由器上）
    feedback: Vec<NodeId>,
    /// 移动超过过阈值
    crossed: bool,
    /// 越过阈值时要滚的视口参加了仲裁，没轮到它（有元素停止了传播、视口自己让出）：这次按下不再滚
    pan_refused: bool,
    /// 被拿走了：之后的移动不发，抬起不算点击
    taken: bool,
}

/// 屏幕上的手指。只有手指在屏幕上时才有，路由器上是一个空的指针
#[derive(Default)]
pub(in crate::tree) struct Touches {
    /// 按放上的先后
    fingers: SmallVec<[Finger; 4]>,
    /// 主手指还在屏幕上时是它
    primary: Option<PointerId>,
    /// 两根手指的识别器
    pair: Option<Pair>,
    /// 主手指的长按定时器：手指动了、抬起、被拿走时取消
    hold: Option<Timer>,
}

/// 两根手指：捏合和旋转各一个识别器。距离和角度在识别器开始之前是两根手指凑成一对时的，开始之后是上一次的
struct Pair {
    fingers: [PointerId; 2],
    /// 从两根手指按下处公共的最内层元素往外
    route: RoutePath,
    pinch: Recognizer,
    rotate: Recognizer,
    distance: f32,
    /// 弧度，窗口坐标（y 向下：变大是顺时针）
    angle: f32,
    /// 手指已经被手势拿走了
    took: bool,
    /// 上次看过之后哪根手指动过（见 [`Pair::ready`]）
    moved: [bool; 2],
}

impl Pair {
    /// 第 `index` 根手指动了：现在该看一眼距离和角度吗。系统把同一时刻的几根手指一根一根报过来，两根手指一起平移时，
    /// 只动了一根的那一瞬间距离变了，不是真的捏合。所以两根都动过了才看；另一根一直不动时（一根手指当支点）
    /// 这根动第二下就看。代价是开始时晚一个事件
    fn ready(&mut self, index: usize) -> bool {
        if self.moved[1 - index] {
            self.moved = [false; 2];
            return true;
        }
        std::mem::replace(&mut self.moved[index], true)
    }
}

/// 一个识别器走到哪一步
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recognizer {
    /// 等手指动到阈值
    Waiting,
    /// 开始了，有元素拿到
    Running,
    /// 路径上没人听，或者开始时没有元素拿到
    Off,
}

/// 两根手指的距离和方向（弧度）
fn span(a: [f32; 2], b: [f32; 2]) -> (f32, f32) {
    (distance(a, b), (b[1] - a[1]).atan2(b[0] - a[0]))
}

/// 角度差换到 -π..π：转过 ±π 时 `atan2` 会跳一圈
fn wrapped(angle: f32) -> f32 {
    (angle + PI).rem_euclid(TAU) - PI
}

impl Touches {
    fn finger(&self, id: PointerId) -> Option<&Finger> {
        self.fingers.iter().find(|f| f.id == id)
    }

    fn finger_mut(&mut self, id: PointerId) -> Option<&mut Finger> {
        self.fingers.iter_mut().find(|f| f.id == id)
    }

    /// 主手指被拿走了
    fn mark_primary_taken(&mut self) {
        if let Some(id) = self.primary
            && let Some(finger) = self.finger_mut(id)
        {
            finger.taken = true;
        }
    }

    /// 主手指不再等长按
    fn cancel_hold(&mut self) {
        if let Some(hold) = self.hold.take() {
            hold.cancel();
        }
    }

    /// 拿掉一根手指，返回它和它是不是主手指
    pub(in crate::tree) fn remove(&mut self, id: PointerId) -> Option<(Finger, bool)> {
        let index = self.fingers.iter().position(|f| f.id == id)?;
        let primary = self.primary == Some(id);
        if primary {
            self.primary = None;
            self.cancel_hold();
        }
        Some((self.fingers.remove(index), primary))
    }

    pub(in crate::tree) fn is_empty(&self) -> bool {
        self.fingers.is_empty()
    }

    /// 节点开始退场、浮层开始关闭、节点被卸载：指向它们的状态清掉（见 [`crate::tree::router::Router::forget`]）
    pub(in crate::tree) fn forget(&mut self, nodes: &NodeStore, leave: Leave) {
        for finger in &mut self.fingers {
            // 按下的元素没了：这次按下不再算点击
            if finger.path.iter().any(|&id| leave.gone(nodes, id)) {
                finger.path.clear();
            }
            if finger.captured.is_some_and(|id| leave.gone(nodes, id)) {
                finger.captured = None;
            }
            finger.feedback.retain(|&id| !leave.gone(nodes, id));
        }
    }
}

impl Tree {
    /// 主指针是手指：窗口宿主这时不把鼠标事件交进来
    pub(crate) fn primary_is_finger(&self) -> bool {
        self.router.device() == PointerKind::Touch
    }

    /// 不是主指针的一根手指的事件
    fn finger_event(&self, id: PointerId, kind: PointerEventKind, point: [f32; 2], button: Option<MouseButton>, clicks: u32) -> PointerEvent {
        let mut event = PointerEvent::new(kind, point, button, self.router.modifiers(), clicks);
        event.buttons = Buttons::LEFT;
        event.device = PointerKind::Touch;
        event.pointer_id = id;
        event
    }
}

/// 手指放上屏幕。屏幕上没有别的手指、鼠标没按着时是主指针
pub(crate) fn touch_down(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    // 上一次的抬起系统没报告：同一个编号又放上来了，先当作取消
    if tree.borrow().router.touches().is_some_and(|t| t.finger(id).is_some()) {
        touch_cancel(tree, id);
    }
    let primary = {
        let t = tree.borrow();
        t.router.touches().is_none() && t.router.buttons().is_empty()
    };
    if primary {
        primary_down(tree, id, point);
    } else {
        finger_down(tree, id, point);
        form_pair(tree);
    }
}

/// 第二根手指放上之后：屏幕上正好两根没被拿走的手指、都没被捕获、主手指没在滚动、按下处有公共的元素、路径上有人听捏合或旋转时开始识别
fn form_pair(tree: &TreeRef) {
    let mut t = tree.borrow_mut();
    let t = &mut *t;
    if t.panning() || t.router.captured().is_some() {
        return;
    }
    let Some(touches) = t.router.touches() else { return };
    if touches.pair.is_some() {
        return;
    }
    let mut active = touches.fingers.iter().filter(|f| !f.taken);
    let (Some(a), Some(b), None) = (active.next(), active.next(), active.next()) else { return };
    if a.captured.is_some() || b.captured.is_some() {
        return;
    }
    let Some(target) = a.path.iter().zip(&b.path).take_while(|(x, y)| x == y).last().map(|(&id, _)| id) else { return };
    let route = t.route_path(target);
    let recognizer = |kind| if anyone_listens(t, &route, kind) { Recognizer::Waiting } else { Recognizer::Off };
    let (pinch, rotate) = (recognizer(EventType::Pinch), recognizer(EventType::Rotate));
    if pinch == Recognizer::Off && rotate == Recognizer::Off {
        return;
    }
    let (distance, angle) = span(a.point, b.point);
    let pair = Pair { fingers: [a.id, b.id], route, pinch, rotate, distance, angle, took: false, moved: [false; 2] };
    if let Some(touches) = t.router.touches_mut() {
        touches.pair = Some(pair);
        // 两根手指可能是手势：不再等长按
        touches.cancel_hold();
    }
}

/// 这根手指是不是一对手指里的、那对手指被手势拿走了没有：移动了就走一步识别器。返回这次移动有没有被手势吃掉
fn pair_move(tree: &TreeRef, id: PointerId) -> bool {
    let (fingers, route, pinch, rotate, last_distance, last_angle, took, a, b) = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return false };
        let points = touches.pair.as_ref().and_then(|pair| Some((touches.finger(pair.fingers[0])?.point, touches.finger(pair.fingers[1])?.point)));
        let Some(pair) = touches.pair.as_mut() else { return false };
        let Some(index) = pair.fingers.iter().position(|&finger| finger == id) else { return false };
        let Some((a, b)) = points else { return false };
        if !pair.ready(index) {
            return pair.took;
        }
        (pair.fingers, pair.route.clone(), pair.pinch, pair.rotate, pair.distance, pair.angle, pair.took, a, b)
    };
    let (distance, angle) = span(a, b);
    let middle = [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5];
    let turned = wrapped(angle - last_angle);
    // 捏合：这一次放大的比例
    let pinch = advance::<PinchEvent>(tree, pinch, (distance - last_distance).abs() >= PAIR_SLOP, distance / last_distance.max(1.0) - 1.0, &route, middle);
    // 旋转：和触控板报告的方向一样（正值逆时针），窗口坐标里角度变大是顺时针
    let rotate = advance::<RotateEvent>(tree, rotate, turned.abs() * distance * 0.5 >= PAIR_SLOP, -turned, &route, middle);
    let running = pinch == Recognizer::Running || rotate == Recognizer::Running;
    {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return running };
        if pinch == Recognizer::Off && rotate == Recognizer::Off {
            touches.pair = None;
            return false;
        }
        let Some(pair) = touches.pair.as_mut() else { return running };
        pair.pinch = pinch;
        pair.rotate = rotate;
        if pinch == Recognizer::Running {
            pair.distance = distance;
        }
        if rotate == Recognizer::Running {
            pair.angle = angle;
        }
        pair.took |= running;
    }
    // 第一次有元素拿到手势：两根手指都归手势了
    if running && !took {
        for id in fingers {
            take_finger(tree, id);
        }
    }
    running
}

/// 一个识别器走一步：等着的动到阈值（`crossed`）时开始，没有元素拿到就关掉；开始了的把这一次的变化交给拿到的元素
fn advance<E: Pad>(tree: &TreeRef, state: Recognizer, crossed: bool, raw: f32, route: &[NodeId], point: [f32; 2]) -> Recognizer {
    match state {
        Recognizer::Waiting if crossed => {
            if pad_start::<E>(tree, route, 0.0, GesturePhase::Start, point) {
                Recognizer::Running
            } else {
                *E::slot(&mut tree.borrow_mut()) = None;
                Recognizer::Off
            }
        }
        Recognizer::Running => {
            pad_step::<E>(tree, raw, GesturePhase::Move, point);
            Recognizer::Running
        }
        state => state,
    }
}

/// 一对手指里的这根离开了屏幕：开始了的手势以 `End` 或 `Cancel` 结束（在这根手指离开之前算位置）
fn pair_end(tree: &TreeRef, id: PointerId, phase: GesturePhase) {
    let (pair, middle) = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return };
        if !touches.pair.as_ref().is_some_and(|p| p.fingers.contains(&id)) {
            return;
        }
        let Some(pair) = touches.pair.take() else { return };
        let middle = match (touches.finger(pair.fingers[0]), touches.finger(pair.fingers[1])) {
            (Some(a), Some(b)) => [(a.point[0] + b.point[0]) * 0.5, (a.point[1] + b.point[1]) * 0.5],
            _ => return,
        };
        (pair, middle)
    };
    if pair.pinch == Recognizer::Running {
        pad_step::<PinchEvent>(tree, 0.0, phase, middle);
    }
    if pair.rotate == Recognizer::Running {
        pad_step::<RotateEvent>(tree, 0.0, phase, middle);
    }
}

/// 这根手指归手势了：主手指和别的手指各走各的取消
fn take_finger(tree: &TreeRef, id: PointerId) {
    let (primary, lost) = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return };
        let primary = touches.primary == Some(id);
        let Some(finger) = touches.finger_mut(id) else { return };
        if finger.taken {
            return;
        }
        if !primary {
            finger.taken = true;
        }
        // 别的手指：取消要用到的几样抄出来，按压反馈从手指上拿走
        (primary, (!primary).then(|| (finger.id, finger.point, finger.captured, finger.path.last().copied(), std::mem::take(&mut finger.feedback))))
    };
    if primary {
        take_primary(tree);
    } else if let Some((id, point, captured, target, feedback)) = lost {
        cancel_finger(tree, id, point, captured.or(target), &feedback);
    }
}

/// 主手指放上：进入、按下冒泡、按压反馈、关外面的浮层，记下这次按下。拿焦点、放插入点等到点按完成
fn primary_down(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    tree.borrow_mut().router.set_source(PointerKind::Touch, id);
    // 手指按下之前没有悬停：进入在这里发，不发移动
    hover(tree, Some(point));
    before_press(tree, MouseButton::Left);
    // 滑着的视口停住：这一下只是停住它，不算点按
    let caught = tree.borrow_mut().catch_under_finger();
    let clicks = tree.borrow_mut().router.count_tap(timer::now(), point);
    let (path, mut prevented, reached) = dispatch_down(tree, MouseButton::Left, clicks);
    if caught {
        prevented |= Builtin::ACTIVATE | Builtin::FOCUS | Builtin::TEXT_SELECT;
    }
    press_feedback(tree, &path, reached);
    if !prevented.contains(Builtin::DISMISS) {
        dismiss_outside(tree);
    }
    // 按在窗口的拖动区域上：越过阈值时拖窗口。第二下也一样，抬起时才知道是双击
    if window_drag::is_press_in_area(tree, prevented) {
        window_drag::pressed(tree, 1, prevented);
    }
    let mut t = tree.borrow_mut();
    let hit = t.router.hovered().to_vec();
    t.router.set_press(Some(Press { path: hit.clone(), prevented }));
    let touches = t.router.touches_or_default();
    touches.primary = Some(id);
    touches.fingers.push(Finger {
        id,
        point,
        previous: point,
        start: point,
        time: timer::now(),
        path: hit,
        prevented,
        captured: None,
        feedback: Vec::new(),
        crossed: false,
        pan_refused: false,
        taken: false,
    });
    touches.hold = Some(hold_timer(tree, id));
}

/// 等长按的定时器：到时手指还按着、这次按下还算数就是长按。定时器不归谁的作用域，手指抬起、动了时取消
fn hold_timer(tree: &TreeRef, id: PointerId) -> Timer {
    let tree = Rc::downgrade(tree);
    let window = crate::host::current_window_id();
    timer::after(crate::system::conventions().long_press_duration, move || {
        let Some(tree) = tree.upgrade() else { return };
        match window {
            Some(window) => crate::host::run_in_window(window, || long_press(&tree, id)),
            None => long_press(&tree, id),
        }
    })
}

/// 长按：按在文字上时拿焦点、选词；路径上有 `on_secondary_click` 时合成右键点击。做了其中一样这根手指就被拿走了；
/// 都没有时什么都不算，这次按下照常（抬起算点击）
fn long_press(tree: &TreeRef, id: PointerId) {
    let prevented = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return };
        touches.hold = None;
        if touches.primary != Some(id) {
            return;
        }
        match t.router.press() {
            Some(press) => press.prevented,
            None => return,
        }
    };
    let selected = select_word(tree, prevented);
    let menu = !prevented.contains(Builtin::CONTEXT_MENU) && {
        let t = tree.borrow();
        t.innermost_listening(t.router.hovered(), EventType::SecondaryClick).is_some()
    };
    if menu {
        context_menu(tree, prevented);
    }
    if selected || menu {
        take_primary(tree);
    }
}

/// 长按在文字上（输入框、可选中的文字）：拿焦点、选词（和双击一样）。返回选了没有
fn select_word(tree: &TreeRef, prevented: Builtin) -> bool {
    let on_text = {
        let t = tree.borrow();
        t.focus_target_under_pointer().is_some_and(|id| t.is_text(id))
    };
    if !on_text {
        return false;
    }
    focus_and_select(tree, prevented, 2);
    let mut t = tree.borrow_mut();
    // 按在链接上也是选词，不算按下了这个链接
    let _ = t.release_link();
    let Some(id) = t.router.selecting() else { return false };
    if let Some(point) = t.router.pointer() {
        t.target_pointer(id, PointerEventKind::Up, point, 2);
    }
    t.router.set_selecting(None);
    true
}

/// 别的手指放上：自己命中、按下冒泡、捕获、按压反馈、关外面的浮层
fn finger_down(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    tree.borrow_mut().cancel_composition(true);
    let (hit, path, mut event) = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        let hit = t.hit_path(point);
        let path = hit.last().map_or_else(Default::default, |&target| t.route_path(target));
        let event = t.finger_event(id, PointerEventKind::Down, point, Some(MouseButton::Left), 1);
        (hit, path, event)
    };
    let outcome = dispatch(tree, &path, EventType::PointerDown, &mut event, POINTER, |_, _, _, _| {});
    let prevented = event.flow().prevented();
    let feedback = press_targets(tree, &path, outcome.reached);
    notify_press(tree, &feedback, true);
    if !prevented.contains(Builtin::DISMISS) {
        let dismiss = tree.borrow().outside_dismissals(&hit);
        if !dismiss.is_empty() {
            batch(|| dismiss.iter().for_each(|d| d(crate::overlay::Dismiss::Outside)));
        }
    }
    tree.borrow_mut().router.touches_or_default().fingers.push(Finger {
        id,
        point,
        previous: point,
        start: point,
        time: timer::now(),
        path: hit,
        prevented,
        captured: outcome.captured,
        feedback,
        crossed: false,
        pan_refused: false,
        taken: false,
    });
}

/// 手指移动
pub(crate) fn touch_move(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    let primary = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return };
        let primary = touches.primary == Some(id);
        let Some(finger) = touches.finger_mut(id) else { return };
        if finger.point == point {
            return;
        }
        finger.previous = std::mem::replace(&mut finger.point, point);
        if primary && t.pan_to(point) {
            return;
        }
        primary
    };
    if pair_move(tree, id) {
        return;
    }
    if tree.borrow().router.touches().and_then(|touches| touches.finger(id)).is_none_or(|finger| finger.taken) {
        return;
    }
    if primary {
        primary_move(tree, id, point);
    } else {
        finger_move(tree, id, point);
    }
}

/// 主手指移动：越过阈值时问谁要这个拖动（要滚的视口也算）；越过以后还没人要时接着找能滚的（手指先往不能滚的方向走了）；
/// 没人要、还没越过时和鼠标移动一样（悬停跟着走，移动事件冒泡）
fn primary_move(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    let (crossing, crossed) = {
        let mut t = tree.borrow_mut();
        let Some(touches) = t.router.touches_mut() else { return };
        let Some(finger) = touches.finger_mut(id) else { return };
        let crossing = !finger.crossed && distance(point, finger.start) >= TOUCH_SLOP;
        finger.crossed |= crossing;
        let crossed = finger.crossed;
        // 动了：不算长按
        if crossing {
            touches.cancel_hold();
        }
        (crossing, crossed)
    };
    if crossed && begin_gesture(tree, id, point, crossing) {
        return;
    }
    pointer_move(tree, Some(point));
}

/// 主手指越过了阈值、按下还算数：`ask_handlers`（刚越过）时按下处的路径上有要拖动的元素就走仲裁，要滚的东西也参加；
/// 不然有能滚的就滚。返回这次移动有没有被吃掉
fn begin_gesture(tree: &TreeRef, id: PointerId, point: [f32; 2], ask_handlers: bool) -> bool {
    let (drag, pan, from) = {
        let t = tree.borrow();
        let Some(press) = t.router.press() else { return false };
        let Some(finger) = t.router.touches().and_then(|touches| touches.finger(id)) else { return false };
        let captured = t.router.captured();
        // 捕获了指针的元素自己处理移动，不滚
        let scrolls = captured.is_none() && !press.prevented.contains(Builtin::SCROLL) && !finger.pan_refused;
        let pan = if scrolls { t.pan_target(&press.path, sub(point, finger.start)) } else { None };
        let drag = match captured.or_else(|| press.path.last().copied()) {
            Some(target) if ask_handlers && !press.prevented.contains(Builtin::DRAG) => {
                let path = t.route_path(target);
                let wanted = path.iter().any(|&id| t.drag_candidate(id) || t.handlers(id).is_some_and(|h| h.listens_in(EventType::Drag, PropagationPhase::Target)));
                wanted.then(|| Drag {
                    node: None,
                    phase: PropagationPhase::Target,
                    path,
                    button: MouseButton::Left,
                    start: finger.start,
                    last: finger.start,
                    started: false,
                    samples: vec![(finger.time, finger.start)],
                    drag_drop: false,
                    out: false,
                })
            }
            _ => None,
        };
        (drag, pan, finger.previous)
    };
    if let Some(drag) = drag {
        tree.borrow_mut().router.set_pointer(Some(point));
        let eaten = start_drag(tree, drag, point, pan.map(|target| (target, from)));
        if !eaten && pan.is_some() {
            let mut t = tree.borrow_mut();
            if let Some(finger) = t.router.touches_mut().and_then(|touches| touches.finger_mut(id)) {
                finger.pan_refused = true;
            }
        }
        return eaten;
    }
    match pan {
        Some(target) => {
            begin_pan(tree, target, from, point);
            true
        }
        None => false,
    }
}

/// 这根手指（主手指）拿去滚动：按下处的整条路径收到取消，悬停离开；内容从 `from` 开始跟手
pub(super) fn begin_pan(tree: &TreeRef, target: PanTarget, from: [f32; 2], point: [f32; 2]) {
    take_primary(tree);
    let mut t = tree.borrow_mut();
    if let Some(touches) = t.router.touches_mut() {
        touches.pair = None;
    }
    t.begin_pan(target, from, point);
}

/// 别的手指移动：交给捕获了它的元素，否则是它下面最内层的元素，冒泡。不动悬停
fn finger_move(tree: &TreeRef, id: PointerId, point: [f32; 2]) {
    let routed = {
        let mut t = tree.borrow_mut();
        let captured = t.router.touches().and_then(|touches| touches.finger(id)).and_then(|f| f.captured);
        t.finger_target(captured, point).map(|target| t.route_path(target))
    };
    let Some(path) = routed else { return };
    if !path.iter().any(|&id| tree.borrow().handlers(id).is_some_and(|h| h.listens_in(EventType::PointerMove, PropagationPhase::Target))) {
        return;
    }
    let mut event = tree.borrow().finger_event(id, PointerEventKind::Move, point, None, 0);
    dispatch(tree, &path, EventType::PointerMove, &mut event, POINTER, |_, _, _, _| {});
}

impl Tree {
    /// 一根手指的事件交给谁：捕获了它的元素（还在时），否则是它下面最内层的元素
    fn finger_target(&mut self, captured: Option<NodeId>, point: [f32; 2]) -> Option<NodeId> {
        if let Some(id) = captured.filter(|&id| self.nodes.contains_key(id)) {
            return Some(id);
        }
        self.refresh_worlds();
        let mut hit = HitPath::new();
        self.hit_into(point, &mut hit);
        hit.last().copied()
    }
}

/// 手指抬起。`point` 是抬起时的位置：和最后一次移动不一样时先当一次移动
pub(crate) fn touch_up(tree: &TreeRef, id: PointerId, point: Option<[f32; 2]>) {
    if let Some(point) = point {
        touch_move(tree, id, point);
    }
    pair_end(tree, id, GesturePhase::End);
    let Some((finger, primary)) = tree.borrow_mut().router.take_finger(id) else { return };
    if primary {
        // 拖着滚动的手指抬起：内容带着速度滑出去
        if !tree.borrow_mut().end_pan(true) && !finger.taken {
            primary_up(tree);
        }
        tree.borrow_mut().router.set_source(PointerKind::Mouse, PointerId::MOUSE);
    } else if !finger.taken {
        finger_up(tree, finger);
    }
}

/// 主手指抬起：点按的话现在拿焦点、放插入点；然后和鼠标松开一样（结束拖动、松开冒泡、合成点击）；最后悬停离开
fn primary_up(tree: &TreeRef) {
    let press = tree.borrow().router.press().map(|p| p.prevented);
    if let Some(prevented) = press {
        let clicks = tree.borrow().router.last_clicks();
        // 按在窗口的拖动区域上：不动焦点（和鼠标一样）；双击（处理器没有取消时）按系统设置缩放窗口
        let window_area = window_drag::is_press_in_area(tree, prevented);
        focus_and_select(tree, if window_area { prevented | Builtin::FOCUS | Builtin::TEXT_SELECT } else { prevented }, clicks);
        if clicks == 2 {
            let target = tree.borrow().pointer_target();
            let double = target.map_or(prevented, |target| prevented | click(tree, target, EventType::DoubleClick, MouseButton::Left, clicks));
            if window_area {
                window_drag::pressed(tree, 2, double);
            }
        }
    }
    release(tree, MouseButton::Left);
    // 手指抬起后没有指针位置
    pointer_move(tree, None);
}

/// 别的手指抬起：松开冒泡，按压反馈收回，抬起处和按下处都在其下的最内层元素上合成点击
fn finger_up(tree: &TreeRef, finger: Finger) {
    let Finger { id, point, path: pressed, prevented, captured, feedback, .. } = finger;
    let (hit, path) = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        let hit = t.hit_path(point);
        let path = t.finger_target(captured, point).map_or_else(Default::default, |target| t.route_path(target));
        (hit, path)
    };
    let mut event = tree.borrow().finger_event(id, PointerEventKind::Up, point, Some(MouseButton::Left), 1);
    dispatch(tree, &path, EventType::PointerUp, &mut event, POINTER, |_, _, _, _| {});
    notify_press(tree, &feedback, false);
    if (prevented | event.flow().prevented()).contains(Builtin::ACTIVATE) {
        return;
    }
    let target = pressed.iter().zip(&hit).take_while(|(a, b)| a == b).last().map(|(&id, _)| id);
    if let Some(target) = target {
        let mut event = tree.borrow().click_event(target, ClickSource::Pointer, MouseButton::Left, 1);
        event.window_position = point;
        send_click(tree, target, EventType::Click, event);
    }
}

/// 系统收走了这根手指：捕获了它的或者按下处最内层的元素收到取消（冒泡），按压反馈收回，不算点击
pub(crate) fn touch_cancel(tree: &TreeRef, id: PointerId) {
    pair_end(tree, id, GesturePhase::Cancel);
    let Some((finger, primary)) = tree.borrow_mut().router.take_finger(id) else { return };
    if primary {
        // 拖着滚动时被收走：不滑，拉过边缘的弹回去
        if !tree.borrow_mut().end_pan(false) && !finger.taken {
            take_primary(tree);
        }
        tree.borrow_mut().router.set_source(PointerKind::Mouse, PointerId::MOUSE);
    } else if !finger.taken {
        cancel_finger(tree, finger.id, finger.point, finger.captured.or_else(|| finger.path.last().copied()), &finger.feedback);
    }
}

/// 窗口失去焦点：所有手指都算系统收走了
pub(super) fn cancel_fingers(tree: &TreeRef) {
    let ids: SmallVec<[PointerId; 4]> = match tree.borrow().router.touches() {
        Some(touches) => touches.fingers.iter().map(|f| f.id).collect(),
        None => return,
    };
    for id in ids {
        touch_cancel(tree, id);
    }
}

/// 主手指被拿走：按压反馈收回，进行中的拖动以 `Cancel` 结束，按下处的整条路径收到取消，悬停离开；这次按下到此为止
fn take_primary(tree: &TreeRef) {
    let feedback = tree.borrow_mut().router.take_press_feedback();
    notify_press(tree, &feedback, false);
    cancel_press_state(tree);
    {
        let mut t = tree.borrow_mut();
        t.router.set_button(MouseButton::Left, false);
        t.router.forget_clicks();
        if let Some(touches) = t.router.touches_mut() {
            touches.mark_primary_taken();
            touches.cancel_hold();
        }
    }
    pointer_move(tree, None);
}

/// 别的手指被拿走：按压反馈收回，`target`（捕获了它的或者按下处最内层的元素）收到取消（冒泡）
fn cancel_finger(tree: &TreeRef, id: PointerId, point: [f32; 2], target: Option<NodeId>, feedback: &[NodeId]) {
    notify_press(tree, feedback, false);
    let routed = {
        let t = tree.borrow();
        target.filter(|&id| t.nodes.contains_key(id)).map(|target| (t.route_path(target), t.finger_event(id, PointerEventKind::Cancel, point, None, 0)))
    };
    if let Some((path, mut event)) = routed {
        dispatch(tree, &path, EventType::PointerCancel, &mut event, POINTER, |_, _, _, _| {});
    }
}
