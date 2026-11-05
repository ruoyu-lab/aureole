//! 挂载和属性绑定：把元素建成节点、子元素挂上去，属性写进节点或者绑定成副作用。
//!
//! 属性按注册表的顺序绑定（`tree/property_table.rs` 里的 `order`，也就是编号的先后）：建造者里的属性已经按编号排好，
//! 挂载时逐个调用每个属性自己的绑定函数（静态值直接写，绑定建一个副作用）。少数要等子元素挂好的属性
//! （`late`：绕排的间距、滚动条的颜色）在子元素之后绑定。
//!
//! 和事件的入口一样**先求值、再借用树**（见 `tree/route.rs`）：属性的值（用户代码）在副作用里先求出来，
//! 再借用树写进去；`on_mount` 之类的处理器在挂好之后、不借着树时调用。

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use reactive::Effect;
use smallvec::SmallVec;

use super::{ChildSlot, NodeId, Tree, TreeRef, accessibility, flow, inline, interaction, links, overlays, scrolling};
use crate::editor::EditorBlock;
use crate::element::{Child, Element};
use crate::property::Prop;
use crate::property::registry::{Entry, KindMask, PropId, PropType, Props, Slot, Stored};
use crate::scroll::{Mounting, ScrollOptions};

/// 在当前作用域（窗口或测试工具的）里建好这棵树的窗口大小信号，提供给里面的绑定（[`crate::window_size_class`] 等）
pub fn attach_metrics(tree: &TreeRef, window: Option<crate::host::WindowId>) {
    let size = tree.borrow().layout.viewport();
    let metrics = crate::responsive::WindowMetrics::attach(size, window);
    tree.borrow_mut().layout.set_metrics(metrics);
}

/// 挂载根元素。根元素没指定高度和 grow 时默认占满窗口。
pub fn mount_root(tree: &TreeRef, mut element: Element) {
    if !element.props.has(PropId::GROW) && !element.props.has(PropId::HEIGHT) {
        element.put(PropId::GROW, Slot::Static(1.0f32.store()));
    }
    let id = mount(tree, element);
    let mut t = tree.borrow_mut();
    let layout = t.nodes[id].layout;
    t.layout.add_root(layout).expect("挂载根元素失败");
    t.nodes.add_root(id);
}

/// 挂载一个元素及其子树，返回还没挂到父节点上的节点。
/// 绑定建立在当前作用域里；动态子元素各自有作用域，卸载时一起释放
pub(crate) fn mount(tree: &TreeRef, mut element: Element) -> NodeId {
    let value = element.kind::<EditorBlock>().map(|editor| editor.value);
    // 多行输入框自己带滚动：滚动句柄挂载时交给元素树
    let own_scroll = element.kind_mut::<EditorBlock>().filter(|e| e.multiline()).map(|e| e.scroll_handle.take());
    let scroll = element.scroll.take();
    let mut interaction = element.input.take();
    // 样式对象、控件记的状态：禁用先写进去，下面的属性绑定第一次读到的就是对的
    if let Some(i) = interaction.as_deref_mut()
        && let Some(states) = i.states
    {
        crate::style::wire_disabled(states, &mut element.props);
    }
    let accessibility = element.accessibility.take();
    let flow = element.flow.take();
    // 字符网格：数据改了下一帧重画（行数列数变了重新布局）
    #[cfg(feature = "char-grid")]
    let grid = element.kind::<crate::char_grid::CharGridBlock>().map(|block| block.grid.version_signal());
    let id = tree.borrow_mut().insert(&mut element);
    // 建节点时按它们定内容的种类，建好以后再取走
    let rich = element.rich.take();
    let image = element.image.take();
    if element.grid_layout {
        super::property_table::mark_grid(tree, id);
    }
    if let Some(props) = accessibility {
        accessibility::attach(tree, id, *props);
    }
    // 输入框：有 `on_commit` 时跟踪"改完了"；绑定的值从外部变化时同步进编辑器
    if let Some(value) = value {
        {
            let mut t = tree.borrow_mut();
            let commit = t.listens(id, crate::handlers::EventType::Commit);
            if let Some(editor) = t.nodes.kind_mut::<EditorBlock>(id) {
                editor.track_commit(commit);
            }
        }
        // 借着读，不克隆整段文字（输入框自己写回去的值流回来时，比一遍就放过）
        let weak = Rc::downgrade(tree);
        Effect::new(move || {
            value.with(|v| {
                if let Some(tree) = weak.upgrade() {
                    tree.borrow_mut().update_editor_value(id, v);
                }
            });
        });
    }
    // 富文本的各段；放在里面的元素和子元素一起挂载
    let inlines = rich.filter(|r| r.rich).map_or_else(Vec::new, |rich| links::mount_spans(tree, id, rich.spans));
    // 图片：来源和淡入、加载状态、播放句柄一起绑定；淡入时透明度乘上"有图片在显示了"
    let source = element.props.take(PropId::SOURCE);
    if source.is_some() || image.is_some() {
        let playing = element.props.has(PropId::PLAYING);
        if let Some(loaded) = crate::image::bind(tree, id, source.map(|e| e.slot), image, playing) {
            fade_in_opacity(&mut element.props, loaded);
        }
    }
    let arrangement = Arrangement { scroll: scroll.is_some(), flow: flow.is_some(), grid: element.grid_layout };
    let late = bind_props(tree, id, element.props, arrangement);
    #[cfg(feature = "char-grid")]
    if let Some(version) = grid {
        bind(tree, Some(Prop::Dynamic(Box::new(move || version.get()))), move |tree, _: u64| tree.char_grid_changed(id));
    }
    let overlays = interaction.map(|i| interaction::attach(tree, id, *i)).unwrap_or_default();
    // 滚动视口：里面的虚拟列表挂载时要找到它的句柄
    let viewport = scroll.as_ref().map(|config| Mounting::enter(config.handle));

    let mut slots = Vec::with_capacity(element.children.len() + usize::from(scroll.is_some()));
    // 视口：托架是第一个子元素，内容挂在托架里（先托架后内容，和写成子元素时建节点的顺序一样）
    let scroll = scroll.map(|config| {
        let ScrollOptions { axes, handle, surface, holder, content } = *config;
        let holder = mount(tree, *holder);
        let content = mount(tree, *content);
        tree.borrow_mut().set_slots(holder, vec![ChildSlot::Static(content)]);
        slots.push(ChildSlot::Static(holder));
        scrolling::Parts { axes, handle, surface, holder, content }
    });
    let mut dynamic = Vec::new();
    for child in element.children {
        match child {
            Child::Static(child) => slots.push(ChildSlot::Static(mount(tree, *child))),
            Child::Dynamic(children) => {
                dynamic.push((slots.len(), children));
                slots.push(ChildSlot::Dynamic(Vec::new()));
            }
        }
    }
    if !inlines.is_empty() {
        let mounted: Vec<(usize, NodeId)> = inlines.into_iter().map(|(span, element)| (span, mount(tree, element))).collect();
        slots.extend(mounted.iter().map(|&(_, node)| ChildSlot::Static(node)));
        inline::attach(tree, id, &mounted);
    }
    // 绕排的子元素是哪几个节点（按声明的位置）
    let statics: Vec<Option<NodeId>> = match flow {
        Some(_) => slots.iter().map(|s| if let ChildSlot::Static(n) = s { Some(*n) } else { None }).collect(),
        None => Vec::new(),
    };
    tree.borrow_mut().set_slots(id, slots);
    if let Some(flow) = flow {
        flow::attach(tree, id, *flow, &statics);
    }
    for (slot, children) in dynamic {
        children.mount(tree, id, slot);
    }
    if let Some(parts) = scroll {
        scrolling::mount_viewport(tree, id, parts);
    }
    // 多行输入框：自己带滚动（内容是它自己画的）
    if let Some(handle) = own_scroll {
        scrolling::mount_own_viewport(tree, id, handle);
    }
    drop(viewport);
    // 要等子元素、视口建好的属性
    for entry in late {
        (entry.id.metadata().bind)(tree, id, entry.slot);
    }
    if !overlays.is_empty() {
        overlays::declare(tree, id, overlays);
    }
    if let Some(on_mount) = element.events.on_mount {
        on_mount();
    }
    id
}

/// 容器的几样：是不是滚动视口、绕排容器、网格（种类对象在节点上，这几样在建造者上）
#[derive(Clone, Copy)]
struct Arrangement {
    scroll: bool,
    flow: bool,
    grid: bool,
}

/// 按注册表的顺序绑定元素上设了的属性，返回要等子元素挂好才绑定的那几个。
/// 只适用于某几种元素的属性设在别的元素上时不绑定（本来也不起作用），调试构建里提示一次
fn bind_props(tree: &TreeRef, id: NodeId, props: Props, arrangement: Arrangement) -> SmallVec<[Entry; 2]> {
    let mut late = SmallVec::new();
    let mut mask = None;
    for entry in props.into_entries() {
        let meta = entry.id.metadata();
        if meta.applies_to != KindMask::ANY {
            let mask = *mask.get_or_insert_with(|| kind_of(&tree.borrow(), id, arrangement));
            if !meta.applies_to.intersects(mask) {
                misapplied(entry.id, mask);
                tree.borrow_mut().forget_source(id, entry.id);
                continue;
            }
        }
        if meta.late {
            late.push(entry);
        } else {
            (meta.bind)(tree, id, entry.slot);
        }
    }
    late
}

/// 节点是哪种元素：带内容的由种类自己说（[`crate::kinds::ElementKind::kind_mask`]），容器是 `CONTAINER`；
/// 再加上滚动视口、绕排、网格的位
fn kind_of(tree: &Tree, id: NodeId, arrangement: Arrangement) -> KindMask {
    let mut mask = tree.nodes.element_kind(id).map_or(KindMask::CONTAINER, |kind| kind.kind_mask());
    if arrangement.scroll {
        mask |= KindMask::SCROLL;
    }
    if arrangement.flow {
        mask |= KindMask::FLOW;
    }
    if arrangement.grid {
        mask |= KindMask::GRID;
    }
    mask
}

thread_local! {
    /// 设在不适用的元素上的属性（只在调试构建里记）：(属性, 元素的种类)
    static MISAPPLIED: std::cell::RefCell<Vec<(PropId, KindMask)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 属性设在了不适用的元素上（比如 `placeholder` 设在不是输入框的元素上）：调试构建里每个属性提示一次
fn misapplied(prop: PropId, kind: KindMask) {
    if cfg!(debug_assertions) {
        MISAPPLIED.with(|m| {
            let mut m = m.borrow_mut();
            if !m.iter().any(|(p, _)| *p == prop) {
                tracing::warn!(property = prop.name(), element = ?kind, "属性设在了不适用的元素上，不起作用（适用于 {:?}）", prop.metadata().applies_to);
            }
            m.push((prop, kind));
        });
    }
}

/// 取走调试构建里记下的"属性设在了不适用的元素上"：(属性, 元素的种类)。测试用；发布构建里总是空的
pub fn take_misapplied() -> Vec<(PropId, KindMask)> {
    MISAPPLIED.with(|m| std::mem::take(&mut *m.borrow_mut()))
}

/// 图片淡入：解码好之前透明度是 0，之后是设的透明度（没设时是 1）
fn fade_in_opacity(props: &mut Props, loaded: reactive::Signal<bool>) {
    let opacity = props.take(PropId::OPACITY).map(|e| e.slot);
    let slot = match opacity {
        None => Slot::Bound(Box::new(move || if loaded.get() { 1.0f32 } else { 0.0 })),
        Some(Slot::Static(Stored::F32(v))) => Slot::Bound(Box::new(move || if loaded.get() { v } else { 0.0 })),
        Some(slot) => Slot::Bound(Box::new(move || if loaded.get() { slot.get::<f32>().unwrap_or(1.0) } else { 0.0 })),
    };
    props.put(PropId::OPACITY, slot, false);
}

/// 静态值直接写入；绑定值建一个副作用，信号变化时重新求值并写入。
/// 求值（用户代码）在借用树之前完成，副作用里可以安全地读任何信号。
/// 属性表以外的东西（输入框的值、富文本的段、浮层、无障碍的提示……）用它
pub(crate) fn bind<T: 'static>(tree: &TreeRef, prop: Option<Prop<T>>, apply: impl Fn(&mut Tree, T) + 'static) {
    match prop {
        None => {}
        Some(Prop::Static(value)) => apply(&mut tree.borrow_mut(), value),
        Some(Prop::Dynamic(f)) => {
            let tree: Weak<RefCell<Tree>> = Rc::downgrade(tree);
            Effect::new(move || {
                let value = f();
                if let Some(tree) = tree.upgrade() {
                    apply(&mut tree.borrow_mut(), value);
                }
            });
        }
    }
}
