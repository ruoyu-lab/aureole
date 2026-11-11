//! 读屏：内核的无障碍树（页面里的标题、段落、链接、按钮、输入框……）交给框架的无障碍树。
//!
//! 辅助技术打开时（框架第一次问网页视图的无障碍节点）才让内核打开它的无障碍（`set_accessibility_state`），
//! 没开读屏的运行没有开销。内核把树的变化交给离屏的回调（[`PageAccessibility`]），这里转成自己的结构
//! （[`Ax`]，CEF 的值不出它的线程）投递到主线程，并进主线程上的一棵树（[`AxTree`]）；网页视图的元素按文档顺序
//! 把有意义的节点当成虚拟子节点交给框架，读屏软件按顺序读：有语义的（标题、链接、按钮、输入框、图片……）整个算一项，
//! 不再往里展开（不然同一段字念两遍），别处的文字一段一项。读屏软件点了哪一项，在它的中心给页面点一下。
//!
//! 内核给的格式：`{"updates": [{"nodes": [{"id", "role", "attributes": {"name", "value", ...}, "location": {"x", "y",
//! "width", "height"}, "offset_container_id", "child_ids", "state"}], "node_id_to_clear", "root_id"}]}`；位置是物理像素、
//! 相对偏移容器，容器带着自己的滚动偏移（`scrollX`、`scrollY`）。位置单独变时是
//! `[{"id", "new_location": {"bounds": {...}, "offset_container_id"}}]`。

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use accesskit::{Action, Node, Role};
use aureole_core::kinds::{AccessibilityAction, AccessibilityContext, AccessibilityProvider, Handled, TargetContext};
use cef::rc::Rc as _;
use cef::sys::cef_value_type_t;
use cef::{
    AccessibilityHandler, CefString, CefStringList, ImplAccessibilityHandler, ImplDictionaryValue, ImplListValue, ImplValue, Value, ValueType,
    WrapAccessibilityHandler, wrap_accessibility_handler,
};

use crate::kind::WebViewKind;
use crate::state::{Page, Shared};

/// CEF 的值转成的自己的结构（能跨线程）
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ax {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    List(Vec<Ax>),
    Dict(Vec<(String, Ax)>),
}

impl Ax {
    fn get(&self, key: &str) -> Option<&Ax> {
        match self {
            Ax::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            Ax::Int(v) => Some(*v),
            Ax::Double(v) => Some(*v as i64),
            _ => None,
        }
    }

    fn float(&self) -> Option<f32> {
        match self {
            Ax::Int(v) => Some(*v as f32),
            Ax::Double(v) => Some(*v as f32),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Ax::Str(s) => Some(s),
            _ => None,
        }
    }

    fn list(&self) -> &[Ax] {
        match self {
            Ax::List(items) => items,
            _ => &[],
        }
    }
}

/// CEF 的值转成 [`Ax`]
fn convert(value: &Value) -> Ax {
    let kind = value.get_type();
    if kind == ValueType::from(cef_value_type_t::VTYPE_BOOL) {
        Ax::Bool(value.bool() != 0)
    } else if kind == ValueType::from(cef_value_type_t::VTYPE_INT) {
        Ax::Int(i64::from(value.int()))
    } else if kind == ValueType::from(cef_value_type_t::VTYPE_DOUBLE) {
        Ax::Double(value.double())
    } else if kind == ValueType::from(cef_value_type_t::VTYPE_STRING) {
        Ax::Str(CefString::from(&value.string()).to_string())
    } else if kind == ValueType::from(cef_value_type_t::VTYPE_LIST) {
        let Some(list) = value.list() else { return Ax::Null };
        Ax::List((0..list.size()).filter_map(|i| list.value(i)).map(|v| convert(&v)).collect())
    } else if kind == ValueType::from(cef_value_type_t::VTYPE_DICTIONARY) {
        let Some(dict) = value.dictionary() else { return Ax::Null };
        let mut keys = CefStringList::new();
        dict.keys(Some(&mut keys));
        Ax::Dict(
            keys.into_iter()
                .filter_map(|key| {
                    let value = dict.value(Some(&CefString::from(key.as_str())))?;
                    Some((key, convert(&value)))
                })
                .collect(),
        )
    } else {
        Ax::Null
    }
}

/// 页面的一个无障碍节点
#[derive(Clone, Debug, Default, PartialEq)]
struct AxNode {
    role: String,
    name: String,
    value: String,
    description: String,
    placeholder: String,
    /// 标题的级别
    level: Option<usize>,
    /// 被忽略的、看不见的（不读）
    hidden: bool,
    /// 相对偏移容器的位置和大小，物理像素：x、y、宽、高
    location: [f32; 4],
    /// 偏移容器（0 是没有）
    container: i64,
    /// 自己的滚动偏移（是容器时减掉）
    scroll: [f32; 2],
    children: Vec<i64>,
}

impl AxNode {
    fn parse(node: &Ax) -> Option<(i64, Self)> {
        let id = node.get("id")?.int()?;
        let attributes = node.get("attributes");
        let attribute = |key: &str| attributes.and_then(|a| a.get(key));
        let text = |key: &str| attribute(key).and_then(Ax::str).unwrap_or_default().to_owned();
        let location = node.get("location").map(bounds).unwrap_or_default();
        let states: Vec<&str> = node.get("state").map(Ax::list).unwrap_or_default().iter().filter_map(Ax::str).collect();
        Some((
            id,
            Self {
                role: node.get("role").and_then(Ax::str).unwrap_or_default().to_owned(),
                name: text("name"),
                value: text("value"),
                description: text("description"),
                placeholder: text("placeholder"),
                level: attribute("hierarchicalLevel").and_then(Ax::int).and_then(|l| usize::try_from(l).ok()),
                hidden: states.iter().any(|s| matches!(*s, "ignored" | "invisible")),
                location,
                container: node.get("offset_container_id").and_then(Ax::int).unwrap_or(0),
                scroll: [attribute("scrollX").and_then(Ax::float).unwrap_or(0.0), attribute("scrollY").and_then(Ax::float).unwrap_or(0.0)],
                children: node.get("child_ids").map(Ax::list).unwrap_or_default().iter().filter_map(Ax::int).collect(),
            },
        ))
    }
}

/// `{"x", "y", "width", "height"}`
fn bounds(value: &Ax) -> [f32; 4] {
    let get = |key: &str| value.get(key).and_then(Ax::float).unwrap_or(0.0);
    [get("x"), get("y"), get("width"), get("height")]
}

/// 主线程上的页面无障碍树
#[derive(Default)]
pub(crate) struct AxTree {
    nodes: HashMap<i64, AxNode>,
    root: Option<i64>,
}

impl AxTree {
    /// 并进内核交来的树的变化
    fn apply(&mut self, change: &Ax) {
        for update in change.get("updates").map(Ax::list).unwrap_or_default() {
            if let Some(clear) = update.get("node_id_to_clear").and_then(Ax::int).filter(|&id| id != 0) {
                let children = self.nodes.get(&clear).map(|n| n.children.clone()).unwrap_or_default();
                for child in children {
                    self.remove(child);
                }
            }
            if let Some(root) = update.get("root_id").and_then(Ax::int).filter(|&id| id != 0) {
                self.root = Some(root);
            }
            for node in update.get("nodes").map(Ax::list).unwrap_or_default() {
                if let Some((id, node)) = AxNode::parse(node) {
                    if node.role == "rootWebArea" && self.root.is_none() {
                        self.root = Some(id);
                    }
                    self.nodes.insert(id, node);
                }
            }
        }
        self.prune();
    }

    /// 只是位置变了
    fn move_nodes(&mut self, change: &Ax) {
        for item in change.list() {
            let (Some(id), Some(location)) = (item.get("id").and_then(Ax::int), item.get("new_location")) else { continue };
            if let Some(node) = self.nodes.get_mut(&id) {
                node.location = location.get("bounds").map(bounds).unwrap_or(node.location);
                node.container = location.get("offset_container_id").and_then(Ax::int).unwrap_or(node.container);
            }
        }
    }

    fn remove(&mut self, id: i64) {
        if let Some(node) = self.nodes.remove(&id) {
            for child in node.children {
                self.remove(child);
            }
        }
    }

    /// 从根走不到的节点（父节点不再列出它们）去掉
    fn prune(&mut self) {
        let Some(root) = self.root else { return };
        let mut reachable = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if reachable.insert(id)
                && let Some(node) = self.nodes.get(&id)
            {
                stack.extend(node.children.iter().copied());
            }
        }
        self.nodes.retain(|id, _| reachable.contains(id));
    }

    /// 节点在页面里的位置（物理像素：左、上、宽、高）：沿偏移容器加上去，减掉容器的滚动
    fn absolute(&self, id: i64) -> [f32; 4] {
        let Some(node) = self.nodes.get(&id) else { return [0.0; 4] };
        let [mut x, mut y, width, height] = node.location;
        let mut container = node.container;
        // 防环：最多走这么多层
        for _ in 0..64 {
            let Some(parent) = self.nodes.get(&container).filter(|_| container != 0) else { break };
            x += parent.location[0] - parent.scroll[0];
            y += parent.location[1] - parent.scroll[1];
            container = parent.container;
        }
        [x, y, width, height]
    }

    /// 按文档顺序列出要交给框架的节点（见模块说明）
    fn items(&self) -> Vec<i64> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            let children = self.nodes.get(&root).map(|n| n.children.clone()).unwrap_or_default();
            for child in children {
                self.collect(child, &mut out, 0);
            }
        }
        out
    }

    fn collect(&self, id: i64, out: &mut Vec<i64>, depth: usize) {
        let Some(node) = self.nodes.get(&id) else { return };
        if depth > 256 {
            return;
        }
        let whole = role_of(&node.role).is_some();
        if whole && !node.hidden {
            out.push(id);
            return;
        }
        if node.role == "staticText" && !node.hidden && !node.name.trim().is_empty() {
            out.push(id);
            return;
        }
        for &child in &node.children {
            self.collect(child, out, depth + 1);
        }
    }
}

/// 内核的角色里整个算一项的那些，对应框架的角色；别的（容器、段落……）往里展开
fn role_of(role: &str) -> Option<Role> {
    Some(match role {
        "heading" => Role::Heading,
        "link" => Role::Link,
        "button" => Role::Button,
        "textField" => Role::TextInput,
        "textFieldWithComboBox" => Role::EditableComboBox,
        "searchBox" => Role::SearchInput,
        "checkBox" => Role::CheckBox,
        "radioButton" => Role::RadioButton,
        "switch" => Role::Switch,
        "comboBoxSelect" | "popUpButton" | "comboBoxMenuButton" => Role::ComboBox,
        "image" => Role::Image,
        "slider" => Role::Slider,
        "listBoxOption" => Role::ListBoxOption,
        "menuItem" => Role::MenuItem,
        "tab" => Role::Tab,
        "progressIndicator" => Role::ProgressIndicator,
        _ => return None,
    })
}

wrap_accessibility_handler! {
    struct PageAccessibility {
        page: Arc<Page>,
    }

    impl AccessibilityHandler {
        fn on_accessibility_tree_change(&self, value: Option<&mut Value>) {
            let Some(value) = value else { return };
            let change = convert(value);
            self.page.on_main(move |shared| {
                shared.ax_tree.borrow_mut().apply(&change);
                shared.ax_version.update(|v| *v = v.wrapping_add(1));
            });
        }

        fn on_accessibility_location_change(&self, value: Option<&mut Value>) {
            let Some(value) = value else { return };
            let change = convert(value);
            self.page.on_main(move |shared| {
                shared.ax_tree.borrow_mut().move_nodes(&change);
                shared.ax_version.update(|v| *v = v.wrapping_add(1));
            });
        }
    }
}

/// 给离屏绘制的回调：内核的无障碍树的变化
pub(crate) fn handler(page: &Arc<Page>) -> AccessibilityHandler {
    PageAccessibility::new(page.clone())
}

/// 辅助技术来问了：让内核打开它的无障碍（每个网页一次）
fn enable(shared: &Shared) {
    if shared.accessibility.replace(true) {
        return;
    }
    // 网页还没打开时，建好时再开（`created`）
    let host = shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
    if let Some(host) = host {
        turn_on(&host);
    }
}

/// 网页建好了：辅助技术来问过的话，让新的网页也打开无障碍
pub(crate) fn created(shared: &Shared, host: &cef::BrowserHost) {
    if shared.accessibility.get() {
        turn_on(host);
    }
}

fn turn_on(host: &cef::BrowserHost) {
    tracing::debug!("辅助技术打开了：让内核打开网页的无障碍");
    cef::ImplBrowserHost::set_accessibility_state(host, cef::State::from(cef::sys::cef_state_t::STATE_ENABLED));
}

impl AccessibilityProvider for WebViewKind {
    fn role(&self) -> Option<Role> {
        Some(Role::Document)
    }

    fn write_node(&self, node: &mut Node, _cx: &AccessibilityContext) {
        enable(&self.shared);
        let title = self.shared.title.get_untracked();
        if !title.is_empty() {
            node.set_label(title.as_str());
        }
    }

    fn virtual_children(&self, cx: &mut AccessibilityContext) {
        let tree = self.shared.ax_tree.borrow();
        let scale = self.shared.page.geometry().scale;
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let origin = cx.content_rect();
        for id in tree.items() {
            let Some(node) = tree.nodes.get(&id) else { continue };
            let [x, y, width, height] = tree.absolute(id);
            let rect = [origin.x + x / scale, origin.y + y / scale, origin.x + (x + width) / scale, origin.y + (y + height) / scale];
            let mut hasher = DefaultHasher::new();
            (&node.role, &node.name, &node.value, &node.placeholder, node.level, rect.map(f32::to_bits)).hash(&mut hasher);
            let Ok(key) = u32::try_from(id) else { continue };
            cx.push_child(key & 0x7FFF_FFFF, hasher.finish(), || {
                let role = role_of(&node.role).unwrap_or(Role::Label);
                let mut out = Node::new(role);
                if !node.name.is_empty() {
                    out.set_label(node.name.as_str());
                    // 文字节点念的是值
                    if role == Role::Label {
                        out.set_value(node.name.as_str());
                    }
                }
                if !node.value.is_empty() {
                    out.set_value(node.value.as_str());
                }
                if !node.description.is_empty() {
                    out.set_description(node.description.as_str());
                }
                if !node.placeholder.is_empty() {
                    out.set_placeholder(node.placeholder.as_str());
                }
                if let Some(level) = node.level {
                    out.set_level(level);
                }
                out.set_bounds(accesskit::Rect::new(rect[0] as f64, rect[1] as f64, rect[2] as f64, rect[3] as f64));
                if role != Role::Label {
                    out.add_action(Action::Click);
                    out.add_action(Action::Focus);
                }
                out
            });
        }
    }

    /// 读屏软件点了一项：在它的中心给页面点一下。要聚焦一项时不处理，框架把焦点给网页视图
    /// （内核没有按无障碍节点聚焦的接口；点一下会误触复选框之类的）
    fn action(&mut self, action: &AccessibilityAction, _cx: &mut TargetContext) -> Handled {
        let Some(key) = action.child else { return Handled::No };
        if action.action != Action::Click {
            return Handled::No;
        }
        let center = {
            let tree = self.shared.ax_tree.borrow();
            if !tree.nodes.contains_key(&i64::from(key)) {
                return Handled::No;
            }
            let [x, y, width, height] = tree.absolute(i64::from(key));
            let scale = self.shared.page.geometry().scale;
            let scale = if scale > 0.0 { scale } else { 1.0 };
            [(x + width / 2.0) / scale, (y + height / 2.0) / scale]
        };
        crate::input::click_at(&self.shared, center);
        Handled::Yes
    }
}
