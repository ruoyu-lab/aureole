//! 窗口的拖动区域（属性 `window_drag_area`）和应用自己画的最大化按钮（属性 `window_maximize_button`）。约定见 [`super`]。
//!
//! 按在拖动区域上时从指针下最内层的元素往外看，第一个说了算的算数：
//!
//! - 自己设了 `window_drag_area` 的元素：`true` 是拖动区域，`false` 不是（在拖动区域里挖一块）；
//! - 没设、能接这次按下的元素（有点击、按压、按下、拖动的处理器，能获得焦点，文字输入、可选中的文字，能拖放）：不是。
//!   所以拖动区域里的按钮、输入框照常响应，按在它们上面不拖窗口；禁用的元素不接按下，按在上面照样拖；
//! - 没设、不接按下的元素：接着往外看。走到浮层的宿主为止（浮层不算在拖动区域里）。
//!
//! 按下以后没有马上交给系统，越过拖动阈值才开始（和元素的拖动同一个阈值）：系统的移动循环收走松开，马上交的话
//! 区域上的 `on_click`、双击都收不到。开始时这次按下到此为止：按下处收到 `on_pointer_cancel`，按压反馈收回，松开不算点击。
//! 双击（第二次按下）按系统设置缩放或最小化窗口（[`crate::host::HostWindow::perform_title_bar_double_click`]）。
//! 右键松开时还在拖动区域上的，弹出系统的窗口菜单（[`crate::host::HostWindow::show_window_menu`]，Windows 的还原、移动、大小……），
//! 和在系统的标题栏上点右键一样。谁接右键另走一遍同样的路径：区域里有右键点击、按下、松开的处理器的元素，文字输入、可选中的文字
//! 自己接右键，不弹（只有 `on_click` 的按钮不接右键，在它上面点右键照样弹；拖动区域自己的 `on_secondary_click` 照样收到，弹不弹由它取消）。
//! `prevent_builtin(Builtin::WINDOW_DRAG)` 在按下（和第二次按下时的 `on_double_click`、右键的 `on_secondary_click`）里取消这三样。
//!
//! 按在拖动区域上不动焦点：和按在系统的标题栏上一样，输入框里打到一半的字还在。
//!
//! 主手指也一样，阈值是手指的（`touch.rs`）；按下时不知道是不是双击，第二下抬起时才算。
//! 要滚的视口、要拖动的元素先要这根手指，都不要才拖窗口。

use super::super::interaction::{distance, drag_threshold};
use super::super::{NodeId, Tree, TreeRef};
use super::pointer::{cancel_pointer, end_press};
use super::touch::TOUCH_SLOP;
use crate::event::Builtin;
use crate::geometry::Rect;
use crate::handlers::EventType;
use crate::input::MouseButton;

/// 拖动区域和最大化按钮的状态
#[derive(Default)]
pub(in crate::tree) struct WindowAreas {
    /// 声明过拖动区域：没有声明过的窗口，按下时不用走一遍路径
    declared: bool,
    /// 按在拖动区域上、还没越过阈值：按下的位置
    pending: Option<[f32; 2]>,
    /// 要窗口开始拖动
    drag: bool,
    /// 要窗口按系统设置处理双击
    double_click: bool,
    /// 右键按在了拖动区域上（没被取消），等松开
    menu_press: bool,
    /// 要窗口在这里（窗口坐标）弹出系统的窗口菜单
    menu: Option<[f32; 2]>,
    /// 声明成最大化按钮的元素
    maximize_button: Option<NodeId>,
    /// 上次报告给窗口的范围
    reported: Option<Rect>,
}

impl WindowAreas {
    /// 松开、取消：没越过阈值的按下作废（按着的右键也不再弹窗口菜单）
    pub(super) fn clear_pending(&mut self) {
        self.pending = None;
        self.menu_press = false;
    }
}

/// 这种事件的处理器说明元素要接这次按下
const TAKES_PRESS: [EventType; 7] =
    [EventType::Click, EventType::DoubleClick, EventType::MiddleClick, EventType::Press, EventType::PointerDown, EventType::PointerUp, EventType::Drag];
/// 这种事件的处理器说明元素要接右键（不弹窗口菜单）
const TAKES_SECONDARY: [EventType; 3] = [EventType::SecondaryClick, EventType::PointerDown, EventType::PointerUp];

impl Tree {
    /// 窗口大小和标题栏度量的句柄（`attach_metrics` 之前没有）
    pub(crate) fn window_metrics(&self) -> Option<crate::responsive::WindowMetrics> {
        self.layout.metrics()
    }

    /// 属性 `window_drag_area`
    pub(in crate::tree) fn set_window_drag_area(&mut self, id: NodeId, area: Option<bool>) {
        self.window_areas.declared |= area == Some(true);
        self.with_input(id, |i| i.window_drag_area = area);
    }

    /// 属性 `window_maximize_button`：一个窗口一个，后设的算数
    pub(in crate::tree) fn set_window_maximize_button(&mut self, id: NodeId, button: bool) {
        if button {
            self.window_areas.maximize_button = Some(id);
        } else if self.window_areas.maximize_button == Some(id) {
            self.window_areas.maximize_button = None;
        }
    }

    /// 属性 `window_maximize_button` 读回来
    pub(in crate::tree) fn is_window_maximize_button(&self, id: NodeId) -> bool {
        self.window_areas.maximize_button == Some(id)
    }

    /// 能接这次按下的元素，见模块说明
    fn takes_press(&self, id: NodeId) -> bool {
        if self.disabled(id) {
            return false;
        }
        self.is_text(id)
            || self.focusable(id)
            || self.node_input(id).and_then(|i| i.drag_drop()).is_some_and(|d| d.payload.is_some() || d.drag_out.is_some())
            || self.handlers(id).is_some_and(|h| TAKES_PRESS.iter().any(|&kind| h.listens(kind)))
    }

    /// 能接这次右键的元素（不弹窗口菜单），见模块说明
    fn takes_secondary(&self, id: NodeId) -> bool {
        !self.disabled(id) && (self.is_text(id) || self.handlers(id).is_some_and(|h| TAKES_SECONDARY.iter().any(|&kind| h.listens(kind))))
    }

    /// 指针下算不算拖动区域，见模块说明。`takes` 说一个没设属性的元素接不接这次按下
    fn pointer_in_window_drag_area(&self, takes: impl Fn(&Self, NodeId) -> bool) -> bool {
        let Some(target) = self.router.hovered().last().copied() else { return false };
        let path = self.route_path(target);
        for &id in &path[..self.within_overlay(&path)] {
            if let Some(area) = self.node_input(id).and_then(|i| i.window_drag_area) {
                return area;
            }
            if takes(self, id) {
                return false;
            }
        }
        false
    }

    /// 取走要交给窗口的：(双击, 开始拖动, 在哪弹出窗口菜单)
    pub(crate) fn take_window_requests(&mut self) -> (bool, bool, Option<[f32; 2]>) {
        let areas = &mut self.window_areas;
        (std::mem::take(&mut areas.double_click), std::mem::take(&mut areas.drag), areas.menu.take())
    }

    /// 最大化按钮的范围变了（布局变了、换了元素、元素卸载了）时是新的范围，外面一层的 `None` 是没变
    pub(crate) fn take_maximize_button_area(&mut self) -> Option<Option<Rect>> {
        let areas = &self.window_areas;
        if areas.maximize_button.is_none() && areas.reported.is_none() {
            return None;
        }
        let area = areas.maximize_button.filter(|&id| self.nodes.contains_key(id)).and_then(|id| self.layout_rect(id));
        if area == areas.reported {
            return None;
        }
        self.window_areas.reported = area;
        Some(area)
    }
}

/// 左键按在指针此刻的位置算不算按在拖动区域上（处理器没有取消）。要在拿焦点之前问：按在拖动区域上不动焦点
pub(super) fn is_press_in_area(tree: &TreeRef, prevented: Builtin) -> bool {
    let t = tree.borrow();
    t.window_areas.declared && !prevented.contains(Builtin::WINDOW_DRAG) && t.pointer_in_window_drag_area(Tree::takes_press)
}

/// 右键按下（处理器取消了哪些内置行为）：按在拖动区域上、没有元素接右键时记下，松开时弹窗口菜单
pub(super) fn secondary_pressed(tree: &TreeRef, prevented: Builtin) {
    let mut t = tree.borrow_mut();
    t.window_areas.menu_press =
        t.window_areas.declared && !prevented.contains(Builtin::WINDOW_DRAG) && t.pointer_in_window_drag_area(Tree::takes_secondary);
}

/// 右键松开：按下时在拖动区域上、松开时也还在的，请窗口在指针处弹出系统的窗口菜单
pub(super) fn secondary_released(tree: &TreeRef) {
    let mut t = tree.borrow_mut();
    if !std::mem::take(&mut t.window_areas.menu_press) || !t.pointer_in_window_drag_area(Tree::takes_secondary) {
        return;
    }
    t.window_areas.menu = t.router.pointer();
}

/// 左键按在了拖动区域上：记下位置，等越过阈值；第二次按下是双击（`on_double_click` 的处理器没有取消时）
pub(super) fn pressed(tree: &TreeRef, clicks: u32, prevented: Builtin) {
    let mut t = tree.borrow_mut();
    if prevented.contains(Builtin::WINDOW_DRAG) {
        return;
    }
    if clicks == 2 {
        t.window_areas.double_click = true;
    } else {
        t.window_areas.pending = t.router.pointer();
    }
}

/// 指针移动：按在拖动区域上越过了阈值时开始拖窗口，这次按下到此为止，返回 `true`（这次移动不再交给元素）
pub(super) fn pointer_moved(tree: &TreeRef, point: Option<[f32; 2]>) -> bool {
    let (start, finger) = {
        let t = tree.borrow();
        let Some(start) = t.window_areas.pending else { return false };
        (start, t.primary_is_finger())
    };
    let threshold = if finger { TOUCH_SLOP } else { drag_threshold() };
    let Some(point) = point.filter(|&point| distance(point, start) >= threshold) else { return false };
    // 系统的移动循环收走了松开，这边再也等不到
    cancel_pointer(tree);
    end_press(tree);
    let mut t = tree.borrow_mut();
    t.router.set_button(MouseButton::Left, false);
    // 手指被系统拿走以后没有指针位置；鼠标还在那里
    if !finger {
        t.router.set_pointer(Some(point));
    }
    t.window_areas.drag = true;
    true
}
