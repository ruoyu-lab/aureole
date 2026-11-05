//! 手势的仲裁：拖动、捏合、旋转开始时谁拿到这个手势。约定见 [`super`]，传播见 `dispatch.rs`。
//!
//! 手势开始（拖动越过阈值、触控板的捏合和旋转开始、两根手指的距离和角度变了，见 `touch.rs`）时，
//! `Start` 沿路径（拖动是按下处、捏合旋转是指针处，两根手指是它们按下处公共的最内层元素，从目标往外）
//! 走捕获 → 目标 → 冒泡，只交给有这种手势的处理器的元素（禁用的跳过）：
//!
//! - 处理器调用 `claim_gesture()`：这个元素拿到手势，传播停止。父元素在捕获阶段调用就是抢在里面的元素之前；
//! - 没人调用时，目标和冒泡阶段第一个有冒泡阶段处理器（`on_drag` 而不只是 `on_drag_capture`）的元素拿到，
//!   和以前"交给最内层"一样；拖动时能拖放、能拖到应用外面的元素也算；
//! - 处理器调用 `decline_gesture()`：这个元素不要，接着往外传；
//! - 收到过 `Start` 没拿到的元素收到 `Cancel`（按当时的那一段）；之后的 `Move`、`End` 只交给拿到的元素，
//!   按它拿到时的那一段（在捕获阶段抢到的交给 `on_*_capture`，在目标阶段拿到的两种都交）；
//! - 停止了传播又没有元素拿到时，这个手势不开始。

use reactive::batch;
use smallvec::SmallVec;

use super::super::interaction::Gesture;
use super::super::{NodeId, Tree, TreeRef};
use super::dispatch::{Options, Routed, deliver, dispatch};
use crate::event::{Claim, PropagationPhase, PinchEvent, RotateEvent};
use crate::handlers::EventType;
use crate::input::GesturePhase;

/// 手势的处理器传播时跳过禁用的元素
const SKIP_DISABLED: Options = Options { skip_disabled: true, captured: false };

/// 一次仲裁的结果
pub(super) struct Arbitration {
    /// 拿到手势的元素，和它在哪一段拿到的
    pub owner: Option<(NodeId, PropagationPhase)>,
    /// 处理器明说要它的（`claim_gesture`），不是默认轮到的
    pub claimed: bool,
    /// 收到过 `Start`、没拿到手势的元素和当时的那一段（要收到 `Cancel`）
    pub seen: SmallVec<[(NodeId, PropagationPhase); 4]>,
}

/// 手势开始：`Start` 沿 `path`（从目标往外）走捕获 → 目标 → 冒泡，决定谁拿到手势。
/// `candidate` 说一个元素不表态时能不能默认拿到（目标和冒泡阶段才看）
pub(super) fn arbitrate<E: Routed>(tree: &TreeRef, path: &[NodeId], kind: EventType, event: &mut E, candidate: impl Fn(&Tree, NodeId) -> bool) -> Arbitration {
    let mut owner = None;
    let mut claimed = false;
    let mut seen = SmallVec::new();
    dispatch(tree, path, kind, event, SKIP_DISABLED, |tree, id, phase, event| {
        let claim = event.flow().take_claim();
        if owner.is_some() {
            return;
        }
        let t = tree.borrow();
        if !t.routable(id) {
            return;
        }
        let heard = t.handlers(id).is_some_and(|h| h.listens_in(kind, phase));
        let takes = match claim {
            Claim::Claimed => true,
            Claim::Declined => false,
            Claim::Undecided => phase != PropagationPhase::Capture && candidate(&t, id),
        };
        if takes {
            owner = Some((id, phase));
            claimed = claim == Claim::Claimed;
            event.flow().stop();
        } else if heard {
            seen.push((id, phase));
        }
    });
    Arbitration { owner, claimed, seen }
}

/// 收到过 `Start` 没拿到手势的元素：各自收到一次 `Cancel`（`cancel` 造这个事件），不传播
pub(super) fn cancel_seen<E: Routed>(tree: &TreeRef, seen: &[(NodeId, PropagationPhase)], kind: EventType, cancel: impl Fn() -> E) {
    if seen.is_empty() {
        return;
    }
    batch(|| {
        for &(id, phase) in seen {
            deliver(tree, id, kind, phase, &mut cancel());
        }
    });
}

/// 有没有元素在 `path` 上听这种手势（捕获或冒泡阶段都算）：没有时不用仲裁
pub(super) fn anyone_listens(tree: &Tree, path: &[NodeId], kind: EventType) -> bool {
    path.iter().any(|&id| tree.handlers(id).is_some_and(|h| h.listens_in(kind, PropagationPhase::Target)))
}

/// 一种手势（捏合或旋转，触控板或者两根手指）：开始时仲裁、锁定拿到的元素，之后交给它
pub(super) trait Pad: Routed {
    const KIND: EventType;
    /// 开始时的累计值（捏合是 1 倍，旋转是 0）
    const IDENTITY: f32;
    fn slot(tree: &mut Tree) -> &mut Option<Gesture>;
    /// 这一次的变化（不合法的、开始时的是不变）和累计值的合成
    fn step(total: f32, raw: f32, phase: GesturePhase) -> (f32, f32);
    fn event(phase: GesturePhase, total: f32, delta: f32, window_position: [f32; 2]) -> Self;
}

impl Pad for PinchEvent {
    const KIND: EventType = EventType::Pinch;
    const IDENTITY: f32 = 1.0;

    fn slot(tree: &mut Tree) -> &mut Option<Gesture> {
        tree.router.pinch_mut()
    }

    /// `raw` 是这一次放大的比例（0.02 是放大 2%）
    fn step(total: f32, raw: f32, phase: GesturePhase) -> (f32, f32) {
        let factor = if raw.is_finite() && phase != GesturePhase::Start { (1.0 + raw).max(0.01) } else { 1.0 };
        (factor, total * factor)
    }

    fn event(phase: GesturePhase, total: f32, delta: f32, window_position: [f32; 2]) -> Self {
        PinchEvent::new(phase, total, delta, window_position)
    }
}

impl Pad for RotateEvent {
    const KIND: EventType = EventType::Rotate;
    const IDENTITY: f32 = 0.0;

    fn slot(tree: &mut Tree) -> &mut Option<Gesture> {
        tree.router.rotate_mut()
    }

    /// `raw` 是这一次转过的弧度（触控板报告的方向：正值逆时针）；换成和元素旋转一样的约定：弧度，正值顺时针
    fn step(total: f32, raw: f32, phase: GesturePhase) -> (f32, f32) {
        let delta = if raw.is_finite() && phase != GesturePhase::Start { -raw } else { 0.0 };
        (delta, total + delta)
    }

    fn event(phase: GesturePhase, total: f32, delta: f32, window_position: [f32; 2]) -> Self {
        RotateEvent::new(phase, scene::rad(total), scene::rad(delta), window_position)
    }
}

/// 触控板手势的一步。一次手势锁定开始时拿到它的元素；没收到开始就先开始；没开始过的手势收到结束不理会。
/// 开始时没有元素拿到的手势，之后的移动不再找别的元素，等到结束
fn pad<E: Pad>(tree: &TreeRef, raw: f32, phase: GesturePhase) {
    let Some(point) = tree.borrow().router.pointer() else { return };
    let mut phase = phase;
    {
        let mut t = tree.borrow_mut();
        let slot = E::slot(&mut t);
        if slot.is_none() && matches!(phase, GesturePhase::End | GesturePhase::Cancel) {
            return;
        }
        if slot.is_some() && phase != GesturePhase::Start {
            drop(t);
            pad_step::<E>(tree, raw, phase, point);
            return;
        }
        // 开始（收到开始，或者没收到开始就来了移动）
        if phase == GesturePhase::Move {
            phase = GesturePhase::Start;
        }
        *slot = Some(Gesture { node: None, phase: PropagationPhase::Target, total: E::IDENTITY });
    }
    let path = {
        let t = tree.borrow();
        let Some(target) = t.router.hovered().last().copied() else { return };
        t.route_path(target)
    };
    pad_start::<E>(tree, &path, raw, phase, point);
}

/// 手势开始：`Start` 沿 `path`（从目标往外）仲裁，记下拿到的元素。返回有没有元素拿到；
/// 没有时手势的记录还留着（触控板等这次手势结束，手指的识别器自己清掉）
pub(super) fn pad_start<E: Pad>(tree: &TreeRef, path: &[NodeId], raw: f32, phase: GesturePhase, point: [f32; 2]) -> bool {
    *E::slot(&mut tree.borrow_mut()) = Some(Gesture { node: None, phase: PropagationPhase::Target, total: E::IDENTITY });
    if !anyone_listens(&tree.borrow(), path, E::KIND) {
        return false;
    }
    let mut event = E::event(phase, E::IDENTITY, E::step(E::IDENTITY, raw, phase).0, point);
    let Arbitration { owner, seen, .. } = arbitrate(tree, path, E::KIND, &mut event, |t, id| t.listens(id, E::KIND));
    let got = owner.is_some();
    if let (Some(gesture), Some((node, at))) = (E::slot(&mut tree.borrow_mut()).as_mut(), owner) {
        gesture.node = Some(node);
        gesture.phase = at;
    }
    cancel_seen(tree, &seen, E::KIND, || E::event(GesturePhase::Cancel, E::IDENTITY, E::step(E::IDENTITY, f32::NAN, GesturePhase::Cancel).0, point));
    got
}

/// 进行中的手势的一步：交给拿到的元素；`End`、`Cancel` 之后手势就没了
pub(super) fn pad_step<E: Pad>(tree: &TreeRef, raw: f32, phase: GesturePhase, point: [f32; 2]) {
    let owner = {
        let mut t = tree.borrow_mut();
        let slot = E::slot(&mut t);
        let Some(gesture) = slot.as_mut() else { return };
        let (delta, total) = E::step(gesture.total, raw, phase);
        gesture.total = total;
        let owner = gesture.node.map(|node| (node, gesture.phase));
        if matches!(phase, GesturePhase::End | GesturePhase::Cancel) {
            *slot = None;
        }
        owner.map(|owner| (owner, delta, total))
    };
    if let Some(((owner, at), delta, total)) = owner {
        deliver(tree, owner, E::KIND, at, &mut E::event(phase, total, delta, point));
    }
}

/// 触控板捏合：`delta` 是这一次放大的比例（0.02 是放大 2%）
pub(crate) fn pinch(tree: &TreeRef, delta: f32, phase: GesturePhase) {
    pad::<PinchEvent>(tree, delta, phase);
}

/// 触控板旋转：`radians` 是这一次转过的弧度（触控板报告的方向：正值逆时针）
pub(crate) fn rotate(tree: &TreeRef, radians: f32, phase: GesturePhase) {
    pad::<RotateEvent>(tree, radians, phase);
}

/// 窗口失去焦点：进行中的捏合、旋转以 `Cancel` 结束（交给拿到的元素）
pub(super) fn cancel_pads(tree: &TreeRef) {
    let point = tree.borrow().router.pointer().unwrap_or_default();
    let (pinch, rotate) = {
        let mut t = tree.borrow_mut();
        (t.router.pinch_mut().take(), t.router.rotate_mut().take())
    };
    batch(|| {
        if let Some(Gesture { node: Some(owner), phase, total }) = pinch {
            deliver(tree, owner, EventType::Pinch, phase, &mut PinchEvent::new(GesturePhase::Cancel, total, 1.0, point));
        }
        if let Some(Gesture { node: Some(owner), phase, total }) = rotate {
            deliver(tree, owner, EventType::Rotate, phase, &mut RotateEvent::new(GesturePhase::Cancel, scene::rad(total), scene::Angle::ZERO, point));
        }
    });
}
