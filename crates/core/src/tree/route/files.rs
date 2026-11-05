//! 从系统拖进来的东西：文件的经过（进入、离开）和放下，别的应用拖来的内容（文字、链接、图片，文件也算）的经过、离开和放下。
//! 约定见 [`super`]。
//!
//! 拖文件时两种处理器都有事：`on_file_hover`、`on_file_drop` 照旧，`on_external_*` 按"内容是文件"走同一条路。
//! 没有元素关心时什么都不多做。

use std::path::PathBuf;
use std::rc::Rc;

use reactive::batch;

use super::super::router::ExternalDragState;
use super::super::{NodeId, TreeRef};
use crate::event::{ExternalDataKinds, ExternalDragEvent, ExternalDropData, ExternalDropEffect, ExternalDropEvent, FileDropEvent};
use crate::handlers::{EventType, Handler};

/// 拖着文件经过窗口（`None`：离开窗口或取消拖动）：更新文件下面的元素，触发进入和离开处理器
pub(crate) fn file_hover(tree: &TreeRef, point: Option<[f32; 2]>) {
    let (left, entered, path) = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        let path = point.map_or_else(Vec::new, |p| t.hit_path(p));
        let handlers = |id: &NodeId| t.handlers_of(*id, EventType::FileHover, Handler::flag);
        let left: Vec<_> = t.router.file_hovered().iter().filter(|id| !path.contains(id)).flat_map(handlers).collect();
        let entered: Vec<_> = path.iter().filter(|id| !t.router.file_hovered().contains(id)).flat_map(handlers).collect();
        t.router.set_file_hovered(path.clone());
        (left, entered, path)
    };
    if !left.is_empty() || !entered.is_empty() {
        batch(|| {
            left.iter().for_each(|h| h(false));
            entered.iter().for_each(|h| h(true));
        });
    }
    match point {
        Some(point) => ask(tree, &path, point, ExternalDataKinds::FILES),
        None => external_drag_leave(tree),
    }
}

/// 文件放下：交给指针下最内层有 `on_file_drop` 的元素，然后结束悬停。
/// 接受这次拖放的元素同时收到 `on_external_drop`
pub(crate) fn file_drop(tree: &TreeRef, point: Option<[f32; 2]>, files: Vec<PathBuf>) {
    let path = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        point.map_or_else(Vec::new, |p| t.hit_path(p))
    };
    let external = point.and_then(|point| {
        ask(tree, &path, point, ExternalDataKinds::FILES);
        external_drop_call(tree, point, || ExternalDropData { files: files.clone(), ..ExternalDropData::default() })
    });
    let call = {
        let t = tree.borrow();
        path.iter().rev().find(|&&id| t.listens(id, EventType::FileDrop)).map(|&id| {
            let window = point.unwrap_or_default();
            let event = FileDropEvent { paths: files, position: t.local_point(id, window), window_position: window };
            (t.handlers_of(id, EventType::FileDrop, Handler::file_drop), event)
        })
    };
    file_hover(tree, None);
    if let Some((handlers, event)) = call {
        batch(|| handlers.iter().for_each(|h| h(&event)));
    }
    if let Some((handlers, event)) = external {
        batch(|| handlers.iter().for_each(|h| h(&event)));
    }
}

/// 别的应用拖来的内容经过 `point`：从最内层往外问，问到接受的为止，结果记在路由状态里（系统的拖放流程按它显示光标）
pub(crate) fn external_drag_over(tree: &TreeRef, point: [f32; 2], kinds: ExternalDataKinds) {
    let path = {
        let mut t = tree.borrow_mut();
        t.refresh_worlds();
        t.hit_path(point)
    };
    ask(tree, &path, point, kinds);
}

/// 问指针下的元素（`path` 从外到内）接不接受：只有放下处理器的什么都接受，有经过处理器的要它调用 `accept`；
/// 拖的是文件时有 `on_file_drop` 的也算接受。之前问过、这次没问到的收到离开
fn ask(tree: &TreeRef, path: &[NodeId], point: [f32; 2], kinds: ExternalDataKinds) {
    let files = kinds.contains(ExternalDataKinds::FILES);
    let candidates: Vec<NodeId> = {
        let t = tree.borrow();
        path.iter()
            .rev()
            .copied()
            .filter(|&id| t.listens(id, EventType::ExternalDragOver) || t.listens(id, EventType::ExternalDrop) || (files && t.listens(id, EventType::FileDrop)))
            .collect()
    };
    if candidates.is_empty() && tree.borrow().router.external_drag().asked.is_empty() {
        tree.borrow_mut().router.replace_external_drag(ExternalDragState { kinds, point, ..ExternalDragState::default() });
        return;
    }
    let claimed = !candidates.is_empty();
    let (asked, target) = batch(|| {
        let mut asked = Vec::new();
        for &id in &candidates {
            let (handlers, position, takes_files) = {
                let t = tree.borrow();
                (t.handlers_of(id, EventType::ExternalDragOver, Handler::external_drag), t.local_point(id, point), files && t.listens(id, EventType::FileDrop))
            };
            if handlers.is_empty() {
                return (asked, Some((id, ExternalDropEffect::Copy)));
            }
            let event = ExternalDragEvent::new(kinds, position, point);
            handlers.iter().for_each(|h| h(&event));
            asked.push(id);
            if let Some(effect) = event.effect.get() {
                return (asked, Some((id, effect)));
            }
            if takes_files {
                return (asked, Some((id, ExternalDropEffect::Copy)));
            }
        }
        (asked, None)
    });
    let previous = tree.borrow_mut().router.replace_external_drag(ExternalDragState { asked: asked.clone(), kinds, point, target, claimed });
    notify_left(tree, &previous, &asked);
}

type LeaveCall = (Vec<Rc<dyn Fn(&ExternalDragEvent)>>, ExternalDragEvent);

/// 之前问过、这次不在 `still` 里的元素收到离开
fn notify_left(tree: &TreeRef, previous: &ExternalDragState, still: &[NodeId]) {
    let left: Vec<LeaveCall> = {
        let t = tree.borrow();
        previous
            .asked
            .iter()
            .filter(|id| !still.contains(id))
            .map(|&id| {
                let event = ExternalDragEvent::new(previous.kinds, t.local_point(id, previous.point), previous.point);
                (t.handlers_of(id, EventType::ExternalDragLeave, Handler::external_drag), event)
            })
            .filter(|(handlers, _)| !handlers.is_empty())
            .collect()
    };
    if !left.is_empty() {
        batch(|| left.iter().for_each(|(handlers, event)| handlers.iter().for_each(|h| h(event))));
    }
}

/// 别的应用拖来的内容离开窗口或取消（放下也经过这里）
pub(crate) fn external_drag_leave(tree: &TreeRef) {
    let previous = tree.borrow_mut().router.replace_external_drag(ExternalDragState::default());
    notify_left(tree, &previous, &[]);
}

/// 别的应用拖来的内容放下：按放下的位置再问一次，接受的元素收到内容，问过的都算离开
pub(crate) fn external_drop(tree: &TreeRef, point: [f32; 2], data: ExternalDropData) {
    external_drag_over(tree, point, data.kinds());
    let call = external_drop_call(tree, point, move || data);
    external_drag_leave(tree);
    if let Some((handlers, event)) = call {
        batch(|| handlers.iter().for_each(|h| h(&event)));
    }
}

type DropCall = (Vec<Rc<dyn Fn(&ExternalDropEvent)>>, ExternalDropEvent);

/// 接受的元素有放下的处理器时：处理器和事件（内容只在要交出去时才拿）
fn external_drop_call(tree: &TreeRef, point: [f32; 2], data: impl FnOnce() -> ExternalDropData) -> Option<DropCall> {
    let t = tree.borrow();
    let (id, _) = t.router.external_drag().target?;
    let handlers = t.handlers_of(id, EventType::ExternalDrop, Handler::external_drop);
    if handlers.is_empty() {
        return None;
    }
    let ExternalDropData { text, link, image, files } = data();
    Some((handlers, ExternalDropEvent { text, link, image, files, position: t.local_point(id, point), window_position: point }))
}
