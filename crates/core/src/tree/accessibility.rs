//! 元素树 → 无障碍树（AccessKit）：角色推断、名字和状态、边界、焦点、增量更新，以及辅助技术发来的操作。
//! 对外的链式方法在 [`crate::accessibility`]。
//!
//! 元素和无障碍节点一一对应（节点编号就是元素的键），推断不出角色的容器是 `GenericContainer`，
//! 平台适配器会把它们拍平。对辅助技术隐藏的、正在退场的、透明度为 0 的元素连同子孙不出现；
//! 复选框、开关、滑块、图片这类角色的子孙是外观的一部分，也不出现。带内容的元素（输入框、文字、字符网格、画布、
//! 自己定义的）的角色、值、虚拟子节点（输入框按行、按字素簇的文字片段，富文本的链接，画布起了名字的区域）
//! 和编辑动作交给它的无障碍提供者（[`crate::kinds::AccessibilityProvider`]），这里只按接口调用。
//!
//! 坐标都相对父节点：节点的边界是 (0, 0, 宽, 高)，变换把它放进父节点（布局位置，加上平移、缩放、旋转、
//! 滚动偏移、浮层的位置），根节点再乘上窗口缩放（AccessKit 要物理像素）。所以列表里插进一行时只有后面的行
//! 要更新（行里的元素不用），滚动、拖动、浮层跟着锚点走时只更新一个节点。
//!
//! 没有辅助技术时只多一次布尔判断。打开后：元素树在改动的地方（内容、子元素、焦点、编辑、滚动、浮层、
//! 透明度、变换……）把节点记成脏的，重新布局之后比较一遍边界；每帧只重建脏节点，新出现的子树整棵发出去，
//! 消失的子树从记录里删掉。合成层动画在停下时才更新变换，不每帧发。

use std::collections::HashMap;

use crate::kinds::{AccessibilityAction, AccessibilityContext, child_id};
use crate::text::TextBlock;
use std::mem;

use accesskit::{
    Action, ActionData, ActionRequest, Live, Node as AxNode, NodeId as AxId, Role, Toggled,
    TreeId, TreeInfo, TreeUpdate,
};
use reactive::batch;
use slotmap::{Key as _, KeyData, SecondaryMap};

use super::interaction::KeyScroll;
use super::route::finish_edit;
use super::{Node, NodeId, NodeStore, Tree, TreeRef, bind, run_focus_callbacks};
use crate::accessibility::{AccessibilityProps, RangeValue};
use crate::property::PropId;
use crate::handlers::{EventType, Handler};
use crate::geometry::Affine;
use crate::event::ClickSource;
use crate::input::MouseButton;
use crate::geometry::Rect;

/// 窗口（无障碍树的根）的编号；元素的编号是它的键，不会是 0
const ROOT: AxId = AxId(0);
/// 虚拟子节点（输入框的文字片段、富文本的链接、画布的区域……）：最高位是 1，然后是元素的前缀（<< 32），
/// 低 32 位是提供者给的编号（见 [`child_id`]）
pub(crate) const VIRTUAL: u64 = 1 << 63;
/// 富文本里的链接：虚拟子节点的编号带着这一位（见 `tree/kinds/text.rs`）
const LINK: u32 = super::kinds::LINK_KEY;
/// 自己滚的元素里跟着滚动平移的那层容器（虚拟子节点在它里面）
const SCROLLED: u32 = u32::MAX;

/// 元素上声明的无障碍属性的当前值（绑定的已经求过值）
#[derive(Default)]
pub(crate) struct NodeProps {
    pub(super) role: Option<Role>,
    pub(super) label: Option<String>,
    pub(super) description: Option<String>,
    tooltip: Option<String>,
    pub(super) hidden: bool,
    pub(super) live: Option<Live>,
    pub(super) toggled: Option<Toggled>,
    pub(super) selected: Option<bool>,
    pub(super) expanded: Option<bool>,
    pub(super) value: Option<RangeValue>,
    pub(super) value_text: Option<String>,
    /// 虚拟列表里的一行：(第几行，从 0 起，总行数)
    item: Option<(usize, usize)>,
}

/// 已经交给辅助技术的节点
struct Sent {
    /// 这一轮更新处理过（去重）
    stamp: u32,
    role: Role,
    /// 在父节点里的位置和大小
    place: Rect,
    /// 子节点（元素）；虚拟子节点不在这里
    children: Vec<NodeId>,
    /// 无障碍提供者给的虚拟子节点：(编号, 版本)，只发版本变了的
    virtuals: Vec<(u32, u64)>,
    /// 自己滚的元素：上次发的跟着滚动平移的容器（偏移和子节点的哈希）
    scrolled: Option<u64>,
}

impl Sent {
    fn new() -> Self {
        Self { stamp: 0, role: Role::Unknown, place: Rect::default(), children: Vec::new(), virtuals: Vec::new(), scrolled: None }
    }
}

/// 树上的无障碍状态
#[derive(Default)]
pub(crate) struct Accessibility {
    /// 有辅助技术在用：只有这时才记脏节点、生成更新
    active: bool,
    /// 只有声明了无障碍属性的元素才有（装箱：表按元素的下标排，空位只占一个指针）
    pub(super) props: SecondaryMap<NodeId, Box<NodeProps>>,
    /// 声明了 `accessibility_live` 的元素个数：没有时不用往上找实时区域
    live: usize,
    dirty: Vec<NodeId>,
    /// 根节点的子节点（根元素、浮层）或窗口缩放、标题变了
    root_dirty: bool,
    /// 重新布局过：更新前比较一遍边界
    relayout: bool,
    sent: SecondaryMap<NodeId, Sent>,
    /// 根节点的子节点
    root: Vec<NodeId>,
    stamp: u32,
    /// 上次发出的焦点
    focus: Option<AxId>,
    title: String,
    /// 虚拟子节点的前缀，和反查
    runs: SecondaryMap<NodeId, u64>,
    owners: HashMap<u64, NodeId>,
    next_run: u64,
    /// 上一次更新发了多少个节点（测试和基准用）
    last_nodes: usize,
    /// 标签和它命名的控件的焦点句柄（`controls::Label::target`）
    labels: Vec<(NodeId, crate::interaction::FocusHandle)>,
}

impl Accessibility {
    /// 元素的无障碍信息变了；没有辅助技术时什么都不做
    #[inline]
    pub fn touch(&mut self, id: NodeId) {
        if self.active {
            self.dirty.push(id);
        }
    }

    /// 根节点要重新列子节点（浮层开关）
    #[inline]
    pub fn touch_root(&mut self) {
        if self.active {
            self.root_dirty = true;
        }
    }

    /// 重新布局了：下次更新前比较边界
    #[inline]
    pub fn relayout(&mut self) {
        self.relayout |= self.active;
    }

    /// 元素卸载了
    pub fn forget(&mut self, id: NodeId) {
        if !self.labels.is_empty() {
            self.labels.retain(|(label, _)| *label != id);
        }
        if let Some(props) = self.props.remove(id)
            && props.live.is_some()
        {
            self.live -= 1;
        }
        if self.active {
            self.sent.remove(id);
        }
        if let Some(base) = self.runs.remove(id) {
            self.owners.remove(&base);
        }
    }

    pub fn set_title(&mut self, title: String) {
        if self.title != title {
            self.title = title;
            self.touch_root();
        }
    }
}

/// 节点在无障碍树里的编号
pub fn accessibility_id(id: NodeId) -> AxId {
    AxId(id.data().as_ffi())
}

/// 元素在父元素里的位置和大小（根元素、浮层相对窗口）
fn place(nodes: &NodeStore, node: &Node) -> Rect {
    let origin = node.parent.and_then(|p| nodes.get(p)).map_or([0.0, 0.0], |p| [p.bounds.x, p.bounds.y]);
    let b = node.bounds;
    Rect { x: b.x - origin[0], y: b.y - origin[1], width: b.width, height: b.height }
}

/// 把节点（坐标原点在自己左上角）放进父节点的变换：布局位置，加上平移、缩放、旋转（绕中心）、
/// 滚动视口内容的偏移、浮层的位置
fn placement(nodes: &NodeStore, node: &Node, place: Rect) -> accesskit::Affine {
    let p = &node.paint;
    if node.extra.is_some() {
        return projected(nodes, node);
    }
    let shift = node.offset();
    let t = [place.x + p.translate[0] + shift[0], place.y + p.translate[1] + shift[1]];
    if p.scale == 1.0 && p.rotation == 0.0 {
        return accesskit::Affine::translate((t[0] as f64, t[1] as f64));
    }
    // 绕中心：q = A (q - 半边长) + 半边长 + 位置
    let (sin, cos) = p.rotation.sin_cos();
    let a = Affine { linear: [cos * p.scale, sin * p.scale, -sin * p.scale, cos * p.scale], offset: [0.0, 0.0] };
    let half = [place.width * 0.5, place.height * 0.5];
    let turned = a.apply(half);
    let offset = [t[0] + half[0] - turned[0], t[1] + half[1] - turned[1]];
    let [m0, m1, m2, m3] = a.linear.map(f64::from);
    accesskit::Affine::new([m0, m1, m2, m3, offset[0] as f64, offset[1] as f64])
}

/// 有 3D 旋转、变换中心、飞行的节点：按自己的变换把左上、右上、左下三个角投过去，
/// 拟合成平面变换（无障碍树只认平面变换，焦点框是投影后的平行四边形近似）
fn projected(nodes: &NodeStore, node: &Node) -> accesskit::Affine {
    let parent = node.parent.and_then(|p| nodes.get(p));
    let world = node.place(&crate::geometry::Transform::IDENTITY, node.bounds, super::depth::parent_eye(nodes, node), true);
    let b = node.bounds;
    let origin = parent.map_or([0.0, 0.0], |p| [p.bounds.x, p.bounds.y]);
    let at = |x: f32, y: f32| {
        let [px, py] = world.apply([b.x + x, b.y + y]);
        [(px - origin[0]) as f64, (py - origin[1]) as f64]
    };
    let (o, right, down) = (at(0.0, 0.0), at(b.width, 0.0), at(0.0, b.height));
    let (w, h) = (b.width.max(1e-3) as f64, b.height.max(1e-3) as f64);
    accesskit::Affine::new([(right[0] - o[0]) / w, (right[1] - o[1]) / w, (down[0] - o[0]) / h, (down[1] - o[1]) / h, o[0], o[1]])
}

/// 这些角色的子孙是外观的一部分（复选框里的对勾、开关的滑块），不单独出现
fn presentational(role: Role) -> bool {
    matches!(
        role,
        Role::CheckBox
            | Role::Switch
            | Role::RadioButton
            | Role::Slider
            | Role::ProgressIndicator
            | Role::Meter
            | Role::Image
            | Role::ScrollBar
            | Role::SpinButton
            | Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::PasswordInput
    )
}

impl Tree {
    // ---- 改动的地方调用 ----

    /// 元素出现或消失（透明度、退场、隐藏）：它的父节点（根元素是窗口）要重新列子节点
    pub(super) fn accessibility_touch_parent(&mut self, id: NodeId) {
        if !self.accessibility.active {
            return;
        }
        match self.nodes.get(id).and_then(|n| n.parent) {
            Some(parent) => self.accessibility.dirty.push(parent),
            None => self.accessibility.root_dirty = true,
        }
    }

    /// 元素和子孙都要重建（禁用状态沿树继承）
    fn accessibility_touch_subtree(&mut self, id: NodeId) {
        if !self.accessibility.active {
            return;
        }
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.nodes.get(id) {
                self.accessibility.dirty.push(id);
                stack.extend(&node.children);
            }
        }
    }

    /// 改元素的无障碍属性（绑定的新值）
    pub(super) fn accessibility_set(&mut self, id: NodeId, f: impl FnOnce(&mut NodeProps)) {
        if !self.nodes.contains_key(id) {
            return;
        }
        let a = &mut self.accessibility;
        let Some(props) = a.props.entry(id).map(|e| e.or_default()) else { return };
        let (hidden, live) = (props.hidden, props.live.is_some());
        f(props);
        if props.live.is_some() != live {
            if live {
                a.live -= 1;
            } else {
                a.live += 1;
            }
        }
        let shown = props.hidden != hidden;
        self.accessibility.touch(id);
        if shown {
            self.accessibility_touch_parent(id);
        }
    }

    /// 虚拟列表的一行在第几行、一共几行
    pub(crate) fn accessibility_list_item(&mut self, id: NodeId, index: usize, len: usize) {
        let item = Some((index, len));
        if self.accessibility.props.get(id).is_none_or(|p| p.item != item) {
            self.accessibility_set(id, |p| p.item = item);
        }
    }

    /// 窗口标题：无障碍树根节点的名字
    pub(crate) fn accessibility_set_title(&mut self, title: String) {
        self.accessibility.set_title(title);
    }

    // ---- 打开、关闭、生成更新 ----

    /// 辅助技术来要树了：打开，返回整棵树
    pub(crate) fn accessibility_activate(&mut self) -> TreeUpdate {
        let a = &mut self.accessibility;
        a.active = true;
        a.sent.clear();
        a.root.clear();
        a.dirty.clear();
        a.root_dirty = true;
        a.relayout = false;
        a.focus = None;
        self.accessibility_build(true).expect("整棵树至少有根节点")
    }

    /// 辅助技术不用了：不再记脏节点
    pub(crate) fn accessibility_deactivate(&mut self) {
        let a = &mut self.accessibility;
        a.active = false;
        a.sent.clear();
        a.root.clear();
        a.dirty.clear();
        a.root_dirty = false;
        a.relayout = false;
    }

    /// 上次以来变了的节点；没有辅助技术、什么都没变时是 `None`
    pub(crate) fn accessibility_update(&mut self) -> Option<TreeUpdate> {
        if !self.accessibility.active {
            return None;
        }
        self.accessibility_build(false)
    }

    fn accessibility_build(&mut self, full: bool) -> Option<TreeUpdate> {
        let mut queue = mem::take(&mut self.accessibility.dirty);
        // 输入框打字时很长的一段可能只排到了看得见的地方：辅助技术要全部的行
        for &id in &queue {
            if let Some(editor) = self.nodes.kind_mut::<crate::editor::EditorBlock>(id)
                && editor.settle(&mut self.text)
                && let Some(layout) = self.nodes.get(id).map(|node| node.layout)
            {
                self.layout.mark_dirty(layout);
            }
        }
        // 实时区域的名字是里面的文字：里面变了（内容、子元素）它也要重建。
        // 声明了 `Live::Off` 的子树不算在外面的实时区域里（变了不念）：往上找到它就停下
        if self.accessibility.live > 0 {
            let mut extra = Vec::new();
            for &id in &queue {
                let mut current = Some(id);
                while let Some(node) = current {
                    match self.accessibility.props.get(node).and_then(|p| p.live) {
                        Some(Live::Off) => break,
                        Some(_) if node != id => extra.push(node),
                        _ => {}
                    }
                    current = self.nodes.get(node).and_then(|n| n.parent);
                }
            }
            queue.extend(extra);
        }
        // 重新布局过：在父节点里的位置或大小变了的节点。布局属性在动（宽高、内边距、间距的动画）时
        // 不每帧比较，停下之后再比较
        if self.accessibility.relayout && !self.layout_animating() {
            self.accessibility.relayout = false;
            let Self { accessibility, nodes, .. } = self;
            for (id, sent) in accessibility.sent.iter() {
                if let Some(node) = nodes.get(id)
                    && place(nodes, node) != sent.place
                {
                    queue.push(id);
                }
            }
        }
        let root_dirty = mem::take(&mut self.accessibility.root_dirty);
        let focus_before = self.accessibility.focus;
        if queue.is_empty() && !root_dirty && focus_before == Some(self.focus_target().unwrap_or(ROOT)) {
            return None;
        }

        self.accessibility.stamp = self.accessibility.stamp.wrapping_add(1).max(1);
        let stamp = self.accessibility.stamp;
        // 第一遍：结构。重新列脏节点的子节点，消失的子树从记录里删掉，新出现的子节点排进队列（整棵发出去）
        let mut emit: Vec<NodeId> = Vec::with_capacity(queue.len());
        if root_dirty {
            let children = self.accessibility_root_children();
            let old = mem::replace(&mut self.accessibility.root, children.clone());
            self.accessibility_restructure(&old, &children, &mut queue);
        }
        while let Some(id) = queue.pop() {
            match self.accessibility.sent.get(id) {
                Some(sent) if sent.stamp != stamp && self.nodes.contains_key(id) => {}
                _ => continue,
            }
            let role = self.accessibility_role(id);
            let children = self.accessibility_children(id, role);
            let sent = self.accessibility.sent.get_mut(id).expect("上面检查过");
            sent.stamp = stamp;
            sent.role = role;
            let old = mem::take(&mut sent.children);
            if old != children {
                self.accessibility_restructure(&old, &children, &mut queue);
            }
            self.accessibility.sent[id].children = children;
            emit.push(id);
        }

        // 第二遍：生成节点（被删掉的不发）
        let mut nodes = Vec::with_capacity(emit.len() + 1);
        if root_dirty {
            nodes.push((ROOT, self.accessibility_root_node()));
        }
        for id in emit {
            if self.accessibility.sent.get(id).is_some_and(|s| s.stamp == stamp) {
                let node = self.accessibility_node(id, &mut nodes);
                nodes.push((accessibility_id(id), node));
            }
        }
        let focus = self.focus_target().unwrap_or(ROOT);
        self.accessibility.focus = Some(focus);
        if nodes.is_empty() && focus_before == Some(focus) {
            return None;
        }
        self.accessibility.last_nodes = nodes.len();
        let tree = full.then(|| TreeInfo {
            root: ROOT,
            toolkit_name: Some("Aureole".into()),
            toolkit_version: Some(env!("CARGO_PKG_VERSION").into()),
        });
        Some(TreeUpdate { nodes, tree, tree_id: TreeId::ROOT, focus })
    }

    /// 子节点从 `old` 变成 `new`：不在了的连同子孙从记录里删掉，新来的排进队列
    fn accessibility_restructure(&mut self, old: &[NodeId], new: &[NodeId], queue: &mut Vec<NodeId>) {
        let a = &mut self.accessibility;
        // 长列表用哈希表判断"还在不在"
        let kept: Option<std::collections::HashSet<NodeId>> = (old.len() > 16).then(|| new.iter().copied().collect());
        let still = |id: &NodeId| kept.as_ref().map_or_else(|| new.contains(id), |k| k.contains(id));
        let mut gone: Vec<NodeId> = old.iter().copied().filter(|id| !still(id)).collect();
        while let Some(id) = gone.pop() {
            if let Some(sent) = a.sent.remove(id) {
                gone.extend(sent.children);
            }
        }
        for &child in new {
            if !a.sent.contains_key(child) {
                a.sent.insert(child, Sent::new());
                queue.push(child);
            }
        }
    }

    /// 有布局属性的动画在动
    fn layout_animating(&self) -> bool {
        self.anim.active().iter().any(|&(id, index)| {
            self.nodes
                .get(id)
                .and_then(|node| node.animations.get(index))
                .is_some_and(|slot| matches!(slot.property, PropId::WIDTH | PropId::HEIGHT | PropId::PADDING | PropId::GAP))
        })
    }

    /// 有焦点的元素（要已经在无障碍树里）；焦点在富文本的链接上时是链接节点
    fn focus_target(&self) -> Option<AxId> {
        let focused = self.router.focused().filter(|&f| self.accessibility.sent.contains_key(f))?;
        if let Some((_, k)) = self.focused_link()
            && let Some(&base) = self.accessibility.runs.get(focused)
        {
            return Some(child_id(base, LINK | k as u32));
        }
        Some(accessibility_id(focused))
    }

    // ---- 结构 ----

    /// 显示在无障碍树里：没被隐藏、没在退场、没被同标签的共享元素接替、透明度（在动时按目标）不为 0
    fn accessibility_included(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else { return false };
        if node.exiting || self.accessibility.props.get(id).is_some_and(|p| p.hidden) || node.extra.as_deref().is_some_and(|e| e.hidden()) {
            return false;
        }
        let opacity = node
            .animations
            .iter()
            .find(|slot| slot.property == PropId::OPACITY)
            .and_then(|slot| slot.track.as_ref())
            .map_or(node.paint.opacity, |a| a.target()[0]);
        opacity > 0.0
    }

    fn accessibility_children(&self, id: NodeId, role: Role) -> Vec<NodeId> {
        if presentational(role) {
            return Vec::new();
        }
        self.nodes[id].children.iter().copied().filter(|&c| self.accessibility_included(c)).collect()
    }

    /// 窗口下面：根元素，然后是开着的浮层（从下到上）
    fn accessibility_root_children(&self) -> Vec<NodeId> {
        self.nodes.roots().iter().copied().chain(self.open_hosts()).filter(|&id| self.accessibility_included(id)).collect()
    }

    /// 角色：声明的优先，否则按元素推断
    fn accessibility_role(&self, id: NodeId) -> Role {
        let props = self.accessibility.props.get(id);
        if let Some(role) = props.and_then(|p| p.role) {
            return role;
        }
        let node = &self.nodes[id];
        let labelled = props.is_some_and(|p| p.label.is_some());
        // 带内容的元素：无障碍提供者说的角色；没说的（图片、图标、画布、自己画的）是图形，没有替代文字的是装饰
        if let Some(kind) = self.nodes.element_kind(id) {
            return match kind.accessibility_provider().and_then(|p| p.role()) {
                Some(role) => role,
                None if labelled => Role::Image,
                None => Role::GenericContainer,
            };
        }
        if let Some(p) = props {
            if p.item.is_some() {
                return Role::ListItem;
            }
            match p.live {
                Some(Live::Assertive) => return Role::Alert,
                Some(Live::Polite) => return Role::Status,
                _ => {}
            }
        }
        if let Some((modal, passthrough)) = self.overlay_content(id) {
            if modal {
                return Role::Dialog;
            }
            if passthrough {
                return Role::Tooltip;
            }
            if self.node_input(id).is_some_and(|i| i.arrows.is_some()) {
                return Role::Menu;
            }
        }
        if let Some(scroll) = &node.scroll
            && scroll.surface.is_none()
        {
            // 多行输入框外面的视口只是外观
            return Role::ScrollView;
        }
        if self.listens(id, EventType::Click) {
            return Role::Button;
        }
        if let Some(p) = props {
            if p.toggled.is_some() {
                return Role::CheckBox;
            }
            if p.value.is_some() {
                return if self.listens(id, EventType::ValueRequest) { Role::Slider } else { Role::ProgressIndicator };
            }
            if labelled {
                return Role::Group;
            }
        }
        Role::GenericContainer
    }

    // ---- 节点 ----

    fn accessibility_root_node(&self) -> AxNode {
        let mut node = AxNode::new(Role::Window);
        node.set_children(self.accessibility.root.iter().map(|&id| accessibility_id(id)).collect::<Vec<_>>());
        node.set_bounds(accesskit::Rect::new(0.0, 0.0, self.layout.viewport()[0] as f64, self.layout.viewport()[1] as f64));
        // AccessKit 的坐标是物理像素
        let scale = self.text.scale() as f64;
        if scale != 1.0 {
            node.set_transform(accesskit::Affine::scale(scale));
        }
        if !self.accessibility.title.is_empty() {
            node.set_label(self.accessibility.title.as_str());
        }
        node
    }

    /// 一个元素的节点；输入框的文字片段（变了的）放进 `out`
    fn accessibility_node(&mut self, id: NodeId, out: &mut Vec<(AxId, AxNode)>) -> AxNode {
        let sent = &self.accessibility.sent[id];
        let role = sent.role;
        let mut ax = AxNode::new(role);
        ax.set_children(sent.children.iter().map(|&c| accessibility_id(c)).collect::<Vec<_>>());
        let node = &self.nodes[id];
        let place = place(&self.nodes, node);
        ax.set_bounds(accesskit::Rect::new(0.0, 0.0, place.width as f64, place.height as f64));
        ax.set_transform(placement(&self.nodes, node, place));
        if node.paint.clip {
            ax.set_clips_children();
        }
        if let Some(name) = self.nodes.name(id) {
            ax.set_author_id(name);
        }
        let disabled = self.disabled(id);
        if disabled {
            ax.set_disabled();
        }
        let props = self.accessibility.props.get(id);
        // 标签命名的控件：由它们命名；控件自己没有名字时标签的文字就是名字
        let labels = self.labels_of(id);
        if !labels.is_empty() {
            ax.set_labelled_by(labels.iter().map(|&l| accessibility_id(l)).collect::<Vec<_>>());
        }
        // 里面没有文字的元素（只有图标的按钮）没声明名字时，提示的文字当名字
        let mut named_by_tooltip = false;
        if let Some(label) = props.and_then(|p| p.label.as_deref()) {
            ax.set_label(label);
        } else if !labels.is_empty() {
            let names: Vec<String> = labels.iter().map(|&l| self.subtree_text(l, false)).filter(|s| !s.is_empty()).collect();
            ax.set_label(names.join(" "));
        } else if props.is_some_and(|p| p.live.is_some_and(|l| l != Live::Off)) && role != Role::Label {
            // 实时区域：名字是里面所有文字（声明了 `Live::Off` 的子树除外），变了才念得出来
            let text = self.subtree_text(id, true);
            if !text.is_empty() {
                ax.set_label(text);
            }
        } else if let Some(tooltip) = props.and_then(|p| p.tooltip.as_deref())
            && self.subtree_text(id, false).is_empty()
        {
            ax.set_label(tooltip);
            named_by_tooltip = true;
        }
        if let Some(p) = props {
            // 提示的文字当描述，除非它已经是名字（当了名字，或者和声明的名字一样）
            let tooltip = p.tooltip.as_deref().filter(|&t| !named_by_tooltip && p.label.as_deref() != Some(t));
            if let Some(description) = p.description.as_deref().or(tooltip) {
                ax.set_description(description);
            }
            if let Some(tooltip) = p.tooltip.as_deref() {
                ax.set_tooltip(tooltip);
            }
            if let Some(live) = p.live {
                ax.set_live(live);
            }
            if let Some(toggled) = p.toggled {
                ax.set_toggled(toggled);
            }
            if let Some(selected) = p.selected {
                ax.set_selected(selected);
            }
            if let Some(expanded) = p.expanded {
                ax.set_expanded(expanded);
            }
            if let Some(v) = p.value {
                ax.set_numeric_value(v.value);
                ax.set_min_numeric_value(v.min);
                ax.set_max_numeric_value(v.max);
                ax.set_numeric_value_step(v.stride());
            }
            if let Some(text) = p.value_text.as_deref() {
                ax.set_value(text);
            }
            if let Some((index, len)) = p.item {
                ax.set_position_in_set(index + 1);
                ax.set_size_of_set(len);
            }
        }
        if let Some((true, _)) = self.overlay_content(id) {
            ax.set_modal();
        }

        // 操作
        if !disabled {
            if self.listens(id, EventType::Click) {
                ax.add_action(Action::Click);
            }
            if self.focusable(id) || self.is_text(id) {
                ax.add_action(Action::Focus);
            }
            if self.listens(id, EventType::ValueRequest) {
                ax.add_action(Action::SetValue);
                if props.is_some_and(|p| p.value.is_some()) {
                    ax.add_action(Action::Increment);
                    ax.add_action(Action::Decrement);
                }
            }
            if self.listens(id, EventType::SecondaryClick) {
                ax.add_action(Action::ShowContextMenu);
            }
        }
        if role != Role::GenericContainer {
            ax.add_action(Action::ScrollIntoView);
        }
        if let Some(s) = node.scroll.as_deref() {
            if s.axes[0] {
                ax.set_scroll_x(s.shown[0] as f64);
                ax.set_scroll_x_min(0.0);
                ax.set_scroll_x_max(s.max[0] as f64);
                ax.add_action(Action::ScrollLeft);
                ax.add_action(Action::ScrollRight);
            }
            if s.axes[1] {
                ax.set_scroll_y(s.shown[1] as f64);
                ax.set_scroll_y_min(0.0);
                ax.set_scroll_y_max(s.max[1] as f64);
                ax.add_action(Action::ScrollUp);
                ax.add_action(Action::ScrollDown);
            }
        }

        // 带内容的元素：值、选区、编辑动作、虚拟子节点交给它的无障碍提供者
        if node.has_kind {
            self.provide(id, disabled, &mut ax, out);
        }
        self.accessibility.sent[id].place = place;
        ax
    }

    /// 无障碍提供者填节点、列虚拟子节点（版本没变的不重建、不重发），虚拟子节点接在子元素后面
    fn provide(&mut self, id: NodeId, disabled: bool, ax: &mut AxNode, out: &mut Vec<(AxId, AxNode)>) {
        if !self.nodes.element_kind(id).is_some_and(|k| k.accessibility_provider().is_some()) {
            return;
        }
        let base = self.run_base(id);
        let Some((_, size, content)) = self.target_geometry(id) else { return };
        let focused = self.router.focused() == Some(id);
        let sent = mem::take(&mut self.accessibility.sent[id].virtuals);
        let Some(provider) = self.nodes.element_kind(id).and_then(|k| k.accessibility_provider()) else { return };
        let mut cx = AccessibilityContext { base, size, content, disabled, focused, sent: &sent, children: Vec::new(), out, inline_order: Vec::new() };
        provider.virtual_children(&mut cx);
        provider.write_node(ax, &cx);
        let (virtuals, inline_order) = (cx.children, cx.inline_order);
        if !virtuals.is_empty() {
            let mut children: Vec<AxId> = self.accessibility.sent[id].children.iter().map(|&c| accessibility_id(c)).collect();
            let ids = virtuals.iter().map(|&(key, _)| child_id(base, key));
            // 自己滚的元素（多行输入框）：虚拟子节点在内容坐标里，外面套一层跟着滚动平移的容器，
            // 滚动时只重发这一个节点，不重发每一行
            match self.nodes[id].scroll.as_deref().filter(|s| s.own) {
                Some(s) => {
                    let ids: Vec<AxId> = ids.collect();
                    // 偏移、子节点都没变时不重发
                    let mut hasher = std::hash::DefaultHasher::new();
                    std::hash::Hash::hash(&(s.shown.map(f32::to_bits), ids.iter().map(|i| i.0).collect::<Vec<_>>()), &mut hasher);
                    let stamp = std::hash::Hasher::finish(&hasher);
                    let key = child_id(base, SCROLLED);
                    if self.accessibility.sent[id].scrolled != Some(stamp) {
                        let mut content = AxNode::new(Role::GenericContainer);
                        content.set_children(ids);
                        content.set_transform(accesskit::Affine::translate((-s.shown[0] as f64, -s.shown[1] as f64)));
                        out.push((key, content));
                        self.accessibility.sent[id].scrolled = Some(stamp);
                    }
                    children.push(key);
                }
                None if inline_order.is_empty() => children.extend(ids),
                // 富文本里的行内元素和文字按在文字里的先后
                None => {
                    let ids: Vec<AxId> = ids.collect();
                    let is_inline = |c: &AxId| inline_order.iter().any(|&(node, _)| accessibility_id(node) == *c);
                    let shown: Vec<(AxId, usize)> =
                        inline_order.iter().map(|&(node, at)| (accessibility_id(node), at)).filter(|(c, _)| children.contains(c)).collect();
                    children.retain(|c| !is_inline(c));
                    for at in 0..=ids.len() {
                        children.extend(shown.iter().filter(|&&(_, before)| before == at).map(|&(c, _)| c));
                        children.extend(ids.get(at));
                    }
                }
            }
            ax.set_children(children);
        }
        self.accessibility.sent[id].virtuals = virtuals;
    }

    /// 给这个元素命名的标签（它的焦点句柄和标签的一样）
    fn labels_of(&self, id: NodeId) -> Vec<NodeId> {
        if self.accessibility.labels.is_empty() {
            return Vec::new();
        }
        let Some(handle) = self.router.handle_of(id) else { return Vec::new() };
        self.accessibility.labels.iter().filter(|(_, h)| *h == handle).map(|(label, _)| *label).collect()
    }

    /// 虚拟子节点的前缀（第一次用到时分配）
    fn run_base(&mut self, id: NodeId) -> u64 {
        let a = &mut self.accessibility;
        match a.runs.get(id) {
            Some(&base) => base,
            None => {
                a.next_run += 1;
                let base = a.next_run;
                a.runs.insert(id, base);
                a.owners.insert(base, id);
                base
            }
        }
    }

    /// 富文本里的链接节点 → (文字元素, 第几个链接)
    fn accessibility_link(&self, id: AxId) -> Option<(NodeId, usize)> {
        let key = id.0 as u32;
        if id.0 & VIRTUAL == 0 || key & LINK == 0 {
            return None;
        }
        let owner = *self.accessibility.owners.get(&((id.0 & !VIRTUAL) >> 32))?;
        (self.links(owner) > 0).then_some((owner, (key & !LINK) as usize))
    }

    /// 子树里所有（显示在无障碍树里的）文字，按顺序用空格连起来。
    /// `live_region`：给实时区域取名字，跳过里面声明了 `Live::Off` 的子树（那部分变了不念，也不该念出来）
    fn subtree_text(&self, id: NodeId, live_region: bool) -> String {
        let mut text = String::new();
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            if let Some(block) = self.nodes.kind::<TextBlock>(id) {
                let content = block.content().trim();
                if !content.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(content);
                }
            }
            let quiet = |c: NodeId| live_region && self.accessibility.props.get(c).is_some_and(|p| p.live == Some(Live::Off));
            stack.extend(node.children.iter().rev().copied().filter(|&c| self.accessibility_included(c) && !quiet(c)));
        }
        text
    }

    /// 选中复制时代替行内元素的文字：它的无障碍名字，没有时是它里面的文字
    pub(super) fn inline_copy_text(&self, id: NodeId) -> String {
        match self.accessibility.props.get(id).and_then(|props| props.label.clone()) {
            Some(name) => name,
            None => self.subtree_text(id, false),
        }
    }

    /// 辅助技术给的编号 → 元素和虚拟子节点的编号（虚拟子节点算它所在的元素）
    fn accessibility_target(&self, id: AxId) -> Option<(NodeId, Option<u32>)> {
        let (element, child) = if id.0 & VIRTUAL != 0 {
            (*self.accessibility.owners.get(&((id.0 & !VIRTUAL) >> 32))?, Some(id.0 as u32))
        } else {
            (KeyData::from_ffi(id.0).into(), None)
        };
        self.nodes.contains_key(element).then_some((element, child))
    }

}

// ---- 挂载 ----

/// 挂载时把元素上声明的无障碍属性交给树，绑定会变的
pub(super) fn attach(tree: &TreeRef, id: NodeId, props: AccessibilityProps) {
    let AccessibilityProps { tooltip, label_for } = props;
    if let Some(handle) = label_for {
        let mut t = tree.borrow_mut();
        t.accessibility.labels.push((id, handle));
        // 控件已经挂好了：它的名字要重发
        if let Some(target) = t.router.handle_target(handle) {
            t.accessibility.touch(target);
        }
    }
    bind(tree, tooltip, move |t, v| t.accessibility_set(id, |p| p.tooltip = Some(v)));
}

/// 禁用状态变了：自己和子孙都要重建
pub(super) fn disabled_changed(tree: &mut Tree, id: NodeId) {
    tree.accessibility_touch_subtree(id);
}

// ---- 辅助技术发来的操作 ----

/// 辅助技术请求一个操作（点击、聚焦、设置值、增减、滚动、选择文字、右键菜单）。在主线程的事件循环里调用，
/// 处理器和指针、键盘的一样在批量更新里执行
pub fn action(tree: &TreeRef, request: ActionRequest) {
    // 富文本里的链接：点击打开，聚焦时焦点落在这个链接上
    let link = tree.borrow().accessibility_link(request.target_node);
    if let Some((id, k)) = link {
        match request.action {
            Action::Click => {
                let action = {
                    let t = tree.borrow();
                    if t.disabled(id) { None } else { t.link_action(id, k) }
                };
                if let Some(action) = action {
                    batch(|| action.run());
                }
                return;
            }
            Action::Focus => {
                let callbacks = {
                    let mut t = tree.borrow_mut();
                    if t.disabled(id) {
                        return;
                    }
                    let callbacks = t.focus_to(Some(id), true);
                    t.enter_links(id, true, Some(k));
                    t.scroll_into_view(id, true);
                    callbacks
                };
                run_focus_callbacks(callbacks);
                return;
            }
            _ => {}
        }
    }
    let Some((id, child)) = tree.borrow().accessibility_target(request.target_node) else { return };
    // 带内容的元素：先交给它的无障碍提供者（选字、改值、替换选中的文字），它不处理的照通用的办
    if provided(tree, id, child, &request) {
        return;
    }
    match request.action {
        Action::Click => click(tree, id),
        Action::Focus => {
            let callbacks = {
                let mut t = tree.borrow_mut();
                let id = t.press_target(id, None).unwrap_or(id);
                if t.disabled(id) || !(t.focusable(id) || t.is_text(id)) {
                    return;
                }
                let callbacks = t.focus_to(Some(id), true);
                t.scroll_into_view(id, true);
                callbacks
            };
            run_focus_callbacks(callbacks);
        }
        Action::Blur => {
            let callbacks = {
                let mut t = tree.borrow_mut();
                if t.router.focused() != Some(id) {
                    return;
                }
                t.focus_to(None, false)
            };
            run_focus_callbacks(callbacks);
        }
        Action::SetValue => match request.data {
            Some(ActionData::Value(text)) => set_text(tree, id, &text),
            Some(ActionData::NumericValue(v)) => set_number(tree, id, |_| v),
            _ => {}
        },
        Action::Increment => set_number(tree, id, |r| r.value + r.stride()),
        Action::Decrement => set_number(tree, id, |r| r.value - r.stride()),
        Action::ScrollIntoView => tree.borrow_mut().scroll_into_view(id, true),
        Action::ScrollUp | Action::ScrollDown | Action::ScrollLeft | Action::ScrollRight => {
            let (axis, direction) = match request.action {
                Action::ScrollUp => (1, -1.0),
                Action::ScrollDown => (1, 1.0),
                Action::ScrollLeft => (0, -1.0),
                _ => (0, 1.0),
            };
            tree.borrow_mut().key_scroll(&[id], axis, KeyScroll::Page(direction));
        }
        Action::ShowContextMenu => context_menu(tree, id),
        _ => {}
    }
}

/// 交给元素的无障碍提供者（元素没被禁用时）；改了文字的写回信号、调用改值的处理器（来源是辅助技术）。返回处理了没有
fn provided(tree: &TreeRef, id: NodeId, child: Option<u32>, request: &ActionRequest) -> bool {
    let edited = {
        let mut t = tree.borrow_mut();
        if t.disabled(id) || !t.nodes.element_kind(id).is_some_and(|k| k.accessibility_provider().is_some()) {
            return false;
        }
        let base = t.accessibility.runs.get(id).copied();
        let action = AccessibilityAction { action: request.action, child, data: request.data.as_ref(), base };
        let Some((handled, outcome)) = t.with_kind(id, |kind, cx| Some(kind.accessibility_provider_mut()?.action(&action, cx))) else {
            return false;
        };
        if !handled.is_handled() {
            return false;
        }
        super::Edited { node: id, handled: true, release_focus: outcome.release_focus, edit: outcome.edit }
    };
    finish_edit(tree, Some(edited), crate::event::ChangeSource::Accessibility);
    true
}

/// 点击：输入框、可选中的文字获得焦点；其他的调用自己（或者最近的祖先）的 `on_click`
fn click(tree: &TreeRef, id: NodeId) {
    let handlers = {
        let t = tree.borrow();
        if let Some(target) = t.press_target(id, None) {
            drop(t);
            let callbacks = {
                let mut t = tree.borrow_mut();
                if t.disabled(target) {
                    return;
                }
                t.focus_to(Some(target), true)
            };
            run_focus_callbacks(callbacks);
            return;
        }
        let mut current = Some(id);
        let mut found = None;
        while let Some(n) = current {
            let Some(node) = t.nodes.get(n) else { break };
            if t.listens(n, EventType::Click) {
                found = (!t.disabled(n)).then(|| (t.handlers_of(n, EventType::Click, Handler::click), t.click_event(n, ClickSource::Accessibility, MouseButton::Left, 1)));
                break;
            }
            current = node.parent;
        }
        found
    };
    if let Some((handlers, event)) = handlers {
        batch(|| handlers.iter().for_each(|h| h(&event)));
    }
}

/// 设置文字：有文字输入客户端的（输入框）整段换掉，按编辑记进撤销（[`crate::kinds::TextInputClient::replace_range`]），
/// 改值事件的来源是辅助技术；别的元素按数值处理（数字也可以用文字给）
fn set_text(tree: &TreeRef, id: NodeId, text: &str) {
    let edited = {
        let mut t = tree.borrow_mut();
        let client = t.nodes.element_kind(id).is_some_and(|k| k.text_input_client().is_some());
        match client {
            true if t.disabled(id) => return,
            true => t.with_client(id, |client, cx| client.replace_range(0..usize::MAX, text, cx)),
            false => None,
        }
    };
    match edited {
        Some(edited) => {
            finish_edit(tree, Some(edited), crate::event::ChangeSource::Accessibility);
        }
        None => {
            if let Ok(v) = text.trim().parse::<f64>() {
                set_number(tree, id, |_| v);
            }
        }
    }
}

/// 设置数值：按当前值算出新值，夹在范围里，交给 `on_value_request`
fn set_number(tree: &TreeRef, id: NodeId, f: impl FnOnce(&RangeValue) -> f64) {
    let call = {
        let t = tree.borrow();
        if t.disabled(id) {
            return;
        }
        if !t.listens(id, EventType::ValueRequest) {
            return;
        }
        let range = t.accessibility.props.get(id).and_then(|p| p.value).unwrap_or(RangeValue::new(0.0, f64::MIN, f64::MAX));
        let (lo, hi) = (range.min.min(range.max), range.min.max(range.max));
        let value = f(&range).clamp(lo, hi);
        (t.handlers_of(id, EventType::ValueRequest, Handler::number), value)
    };
    let (handlers, value) = call;
    batch(|| handlers.iter().for_each(|h| h(value)));
}

/// 右键菜单：像在元素正中点了右键
fn context_menu(tree: &TreeRef, id: NodeId) {
    let call = {
        let t = tree.borrow();
        if t.disabled(id) {
            return;
        }
        if !t.listens(id, EventType::SecondaryClick) || t.screen_rect(id).is_none() {
            return;
        }
        let event = t.click_event(id, ClickSource::Accessibility, MouseButton::Right, 1);
        (t.handlers_of(id, EventType::SecondaryClick, Handler::click), event)
    };
    let (handlers, event) = call;
    batch(|| handlers.iter().for_each(|h| h(&event)));
}
