//! 保留模式的元素树：挂载、属性绑定、动画、布局、生成绘制数据、命中测试。
//!
//! 树在主线程，和响应式运行时在一起。每个绑定属性对应一个副作用，
//! 信号变化时只改这一个属性，并按属性种类标记"需要重新布局"或"需要重建绘制数据"。
//! 声明了动画的属性，新值只改动画目标，由每帧推进动画写入。
//! 每帧 [`Tree::frame`] 只做被标记的工作；什么都没变、也没有动画时什么都不做。

use std::cell::RefCell;
use std::mem;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use animation::{CurveTrack, MAX_COMPONENTS};
use scene::{AtlasUpdate, DisplayList};
use taffy::{Dimension, LengthPercentage, Size, Style};

use crate::property::{PropId, PropertyValue};
use crate::compositor::{FLIGHT_IDENTITY, Handback, LayerInfo, LayerUpdates, LayerValues, compose, is_compositor};
use crate::editor::EditorBlock;
use crate::element::{Element, Shader};
use crate::kinds::NextFrame;
use crate::handlers::{EventType, Handler};
use crate::geometry::Affine;
use crate::image::{ImageBlock, ImageRef};
use crate::input::Modifiers;
use crate::property::Mix;
use crate::gradient::Fill;
use crate::geometry::Rect;
use crate::text::{LinkColors, TextAlign, TextBlock, TextStyle, TextSystem};
use crate::vector::PathBlock;
use crate::kinds::{KindInfo, StyleContext, UnmountContext};

mod accessibility;
mod anim;
mod animated;
mod canvas;
mod depth;
mod drag_drop;
mod flow;
mod visual_effects;
mod frame;
#[cfg(feature = "char-grid")]
mod char_grid;
mod commit;
mod inline;
mod interaction;
mod kinds;
mod layout;
mod links;
mod measure;
mod mount;
mod node;
mod overlays;
mod paint;
mod patch;
mod place;
mod walk;
mod probe;
pub use probe::TreeProbe;
pub(crate) mod property_table;
pub use property_table::ElementProps;
mod route;
mod router;
mod selection;
mod scrolling;
mod shared;
mod sources;
mod timeline;
pub(crate) use accessibility::action as accessibility_action;
pub(crate) use accessibility::VIRTUAL;
pub(crate) use kinds::clusters;
pub(crate) use accessibility::accessibility_id;
pub use drag_drop::DragOutRequest;
pub(crate) use drag_drop::finish_out;
pub(crate) use route::{
    cancel_pointer, end_composition, external_drag_leave, external_drag_over, external_drop, field_change, file_drop, file_hover, ime, key_down, key_up, mouse_button, pinch, pointer_move,
    refresh_hover, release_keys, rotate,
    set_modifiers,
    touch_cancel, touch_down, touch_move, touch_up,
};
pub(crate) use mount::{attach_metrics, mount_root};
pub use mount::take_misapplied;
pub(crate) use mount::{bind, mount};
use route::run_focus_callbacks;
pub(crate) use frame::advance;
pub(crate) use walk::{ContentArea, may_project};
pub use patch::ScenePatch;
pub(crate) use scrolling::adopt_scrolls;
pub(crate) use scrolling::{list_changed, wheel};
use depth::NodeExtra;
pub(crate) use node::NodeId;
use node::{ChildSlot, Cold, Node, NodeStore, layer_key, layer_node};
use anim::{AnimSlot, AnimState, animations_disabled, is_motion_reduced};
pub use anim::without_animation;
use commit::Commit;
use layout::LayoutState;
use paint::{Paint, PaintState};
use walk::{Frame, Step, Walk};
use router::{Leave, Router};
use scrolling::Scrolls;
use shared::Shared;

/// 一个窗口（或测试工具）的元素树，主线程上共享
pub(crate) type TreeRef = Rc<RefCell<Tree>>;

/// 焦点变化后要调用的 on_focus 处理器和参数
type FocusCallbacks = Vec<(Rc<dyn Fn(bool)>, bool)>;

/// 输入目标处理一次按键、输入法事件的结果：处理了没有、要不要交出焦点，输入框改了什么（要写回的信号和文字、提交、改完）
#[derive(Default)]
struct Edited {
    node: NodeId,
    handled: bool,
    release_focus: bool,
    edit: crate::editor::EditReport,
}

/// 元素自己设置的文字样式；没设的从父元素继承（下划线和删除线不继承）
#[derive(Clone, Default, PartialEq)]
struct TextOverrides {
    size: Option<f32>,
    weight: Option<f32>,
    line_height: Option<f32>,
    color: Option<Fill>,
    underline: bool,
    strikethrough: bool,
    /// 字体族编号（见 `font::family_id`）
    family: Option<u32>,
    italic: bool,
    letter_spacing: Option<f32>,
    align: Option<TextAlign>,
    tabular: bool,
    selection: Option<scene::Color>,
}

/// 继承下来的文字样式
#[derive(Clone)]
struct Inherited {
    size: f32,
    weight: f32,
    line_height: f32,
    color: Fill,
    family: u32,
    italic: bool,
    letter_spacing: f32,
    align: TextAlign,
    tabular: bool,
    selection: scene::Color,
}

impl Inherited {
    /// 根元素继承的：默认的文字样式，文字和选区的颜色是默认的（见 `defaults.rs`）
    fn root(color: scene::Color, selection: scene::Color) -> Self {
        let style = TextStyle::default();
        Self {
            size: style.size,
            weight: style.weight,
            line_height: style.line_height,
            color: Fill::Color(color),
            family: style.family,
            italic: style.italic,
            letter_spacing: style.letter_spacing,
            align: style.align,
            tabular: style.tabular,
            selection,
        }
    }
}

/// 元素没有指定时用的颜色（见 `defaults.rs`）：文字、选区、滚动条、占位文字、富文本
#[derive(Clone, Copy, PartialEq)]
struct DefaultColors {
    text: scene::Color,
    selection: scene::Color,
    scrollbar: scene::Color,
    placeholder: scene::Color,
    rich: TextTheme,
}

/// 文字里不沿树继承的颜色：没焦点时的选区、链接、行内代码的底、窗口底色（字符网格反色时没有底色可用的话）
#[derive(Clone, Copy, PartialEq)]
struct TextTheme {
    inactive: scene::Color,
    links: LinkColors,
    code: scene::Color,
    background: scene::Color,
}

impl DefaultColors {
    fn new(p: &crate::defaults::Palette) -> Self {
        let [normal, hover, pressed] = p.link();
        let links = LinkColors { normal, hover, pressed };
        let rich = TextTheme { inactive: p.selection_inactive, links, code: p.code, background: p.background };
        Self { text: p.text, selection: p.selection, scrollbar: p.scrollbar, placeholder: p.placeholder, rich }
    }
}

/// 一帧做了哪些工作
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// 这一帧重新布局了
    pub laid_out: bool,
    /// 这一帧显示列表变了：整个重新生成了，或者原地修补了一些图元（见 `patched_quads`）
    pub repainted: bool,
    /// 布局耗时
    pub layout_time: Duration,
    /// 生成显示列表耗时
    pub paint_time: Duration,
    /// 显示列表里的图元数
    pub quads: usize,
    /// 这一帧没有重新生成显示列表、只原地修补了几个图元时，修补的个数（颜色、圆角、边框、一层外投影这类不影响结构的改动）；
    /// 重新生成了时是 0
    pub patched_quads: usize,
    /// 合成层数
    pub layers: usize,
    /// 本帧结束后仍在运动的动画数（含交给渲染线程的合成层动画）
    pub animations: usize,
    /// 这一帧重画了几个画布（[`crate::canvas()`]，读到的信号变了或者大小变了才重画），也算上别的元素种类在树外的更新
    /// （[`crate::kinds::ElementKind::update`]）
    pub canvas_draws: usize,
}

/// 一个窗口的元素树：节点、布局、绘制、动画、输入路由的状态（见模块说明）
pub(crate) struct Tree {
    /// 布局树、窗口大小、布局和样式的脏标记
    layout: LayoutState,
    text: TextSystem,
    /// 用到的图片、在播的动图
    images: animated::Images,
    /// 节点（冷热分开存，内容另存）、用户的根元素、结构版本
    nodes: NodeStore,
    /// 绕排容器（见 `tree/flow.rs`）
    flows: flow::Flows,
    /// 带行内元素的富文本（见 `tree/inline.rs`）
    inlines: inline::Inlines,
    /// 选择根（见 `tree/selection.rs`）
    selection_roots: selection::SelectionRoots,
    /// 要每帧推进、要在树外更新、和缩放有关的元素种类（见 `tree/kinds.rs`）
    kinds: kinds::Kinds,
    /// 显示列表的生成状态、合成层表、用过的着色器、默认颜色
    paint: PaintState,
    /// 要交给渲染线程的合成层变化、帧号、收回的滚动状态
    commit: Commit,
    /// 这一帧的时钟、在动的动画、退场、布局位置的过渡
    anim: AnimState,
    /// 指针和键盘：悬停、按下、拖选、焦点、连击、拖动和手势、修饰键、按着的键
    router: Router,
    /// 滚动视口、进行中的滚动手势
    scrolls: Scrolls,
    /// 元素之间的拖放、影子、抬起的元素、拖到应用外面
    drag_drop: drag_drop::DragDropState,
    /// 窗口的拖动区域、应用自己画的最大化按钮
    window_areas: route::WindowAreas,
    /// 布局之后要把位置和大小写回信号的元素（[`crate::Element::report_bounds`]）
    measures: measure::Measures,
    /// 浮层，从下到上
    overlays: overlays::Overlays,
    /// 无障碍树（辅助技术在用时才工作）
    accessibility: accessibility::Accessibility,
    /// 关键帧时间线：句柄的成员、接管的属性
    motion: timeline::Host,
    /// 共享元素的标签和飞行
    shared: Shared,
    /// 覆盖层：被覆盖着的属性和撤掉时回到的值（见 `tree/sources.rs`）
    overrides: sources::Overrides,
}

impl Tree {
    /// 空的元素树，视口 `width` x `height`（逻辑像素）
    pub(crate) fn new(width: f32, height: f32) -> Self {
        Self {
            layout: LayoutState::new(width, height),
            text: TextSystem::new(),
            images: animated::Images::default(),
            nodes: NodeStore::new(),
            flows: flow::Flows::default(),
            inlines: inline::Inlines::default(),
            selection_roots: selection::SelectionRoots::default(),
            kinds: kinds::Kinds::default(),
            paint: PaintState::new(),
            commit: Commit::default(),
            anim: AnimState::new(),
            router: Router::default(),
            scrolls: Scrolls::default(),
            drag_drop: drag_drop::DragDropState::default(),
            window_areas: route::WindowAreas::default(),
            measures: measure::Measures::default(),
            overlays: overlays::Overlays::default(),
            accessibility: accessibility::Accessibility::default(),
            motion: timeline::Host::default(),
            shared: Shared::default(),
            overrides: sources::Overrides::default(),
        }
    }

    pub(crate) fn set_viewport(&mut self, width: f32, height: f32) {
        self.layout.set_viewport(width, height);
    }

    /// 窗口大小变了还没写进信号：返回信号和新大小（每帧开始时由 `advance` 在不借用树时写）
    pub(crate) fn take_viewport_write(&mut self) -> Option<(crate::responsive::WindowMetrics, [f32; 2])> {
        self.layout.take_viewport_write()
    }

    /// 窗口内容区的大小，逻辑像素
    pub(crate) fn viewport(&self) -> [f32; 2] {
        self.layout.viewport()
    }

    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub(crate) fn scale(&self) -> f32 {
        self.text.scale()
    }

    /// 窗口缩放（逻辑像素 -> 物理像素），决定字形栅格化的分辨率
    pub(crate) fn set_scale(&mut self, scale: f32) {
        if self.text.set_scale(scale) {
            self.paint.invalidate();
            self.accessibility.touch_root();
            // 声明了要知道缩放的种类（字符网格的格子按物理像素取整），缩放变了大小也变
            for id in self.kinds.scaled() {
                self.remeasure_kind(id);
            }
            // 行内元素的位置对齐物理像素
            if !self.inlines.is_empty() {
                self.layout.invalidate();
            }
            self.set_scroll_scale(scale);
        }
    }

    /// 带内容的元素的大小要重新量
    fn remeasure_kind(&mut self, id: NodeId) {
        let Some(layout) = self.nodes.get(id).map(|n| n.layout) else { return };
        self.layout.mark_dirty(layout);
    }

    /// 自上次以来新放进字形图集的内容，交给渲染线程上传
    pub(crate) fn take_atlas_updates(&mut self) -> Vec<AtlasUpdate> {
        let mut updates = self.text.take_atlas_updates();
        updates.extend(self.images.system_mut().take_updates());
        updates
    }

    /// 字形、路径图集某一页的边长和用量
    pub(crate) fn text_atlas_usage(&self, kind: scene::AtlasKind) -> (u32, f32) {
        self.text.atlas_usage(kind)
    }

    /// 字形图集和图片纹理的完整内容，用于离屏渲染
    pub(crate) fn atlas_snapshot(&self) -> Vec<AtlasUpdate> {
        let mut updates = self.text.atlas_snapshot();
        updates.extend(self.images.system().snapshot());
        updates
    }

    /// 最近一次生成的显示列表
    pub(crate) fn display_list(&self) -> Arc<DisplayList> {
        self.paint.list().clone()
    }

    /// 自上次交给渲染线程以来显示列表的变化：没变是 `None`；整个重新生成过是新的列表和层表；
    /// 只原地修补过是同一结构的列表（合成层表没变）加上改了的几段
    pub(crate) fn take_scene(&mut self) -> Option<crate::host::Scene> {
        let patch = self.paint.take_change()?;
        Some(crate::host::Scene { list: self.paint.list().clone(), layers: self.paint.layers().clone(), patch })
    }

    /// 登记一个自定义着色器，返回给 `Paint::shader` 用的编号（同一份源码只登记一次）
    pub(crate) fn register_shader(&mut self, shader: &Shader) -> u32 {
        self.paint.register_shader(shader)
    }

    /// 自上次以来合成层的变化（动画的新状态、卸载的层、改过的滚动状态），连同这一帧的时间交给渲染线程
    pub(crate) fn take_layer_updates(&mut self) -> LayerUpdates {
        let mut updates = self.commit.take(self.anim.host());
        updates.scrolls = self.take_scroll_states();
        updates
    }

    /// 这一帧没有要交给渲染线程的东西：合成层没有变化，也没有在动的动画、视口（它们要主线程的帧时间对齐）。
    /// 这时窗口不给渲染线程发消息，画面静止时渲染线程一直睡着
    pub(crate) fn render_idle(&self) -> bool {
        self.commit.idle()
            && !self.anim.busy()
            && self.scrolls_idle()
    }

    /// 还要每帧推进：有动画（含时间线、共享元素的飞行）、退场、焦点环在动、视口在滚动、拖放没完、动图在播、
    /// 元素种类每帧要帧（画布读到的信号变了，种类的 `next_frame` 是 `Continuous`、或者要的时刻已经到了），或者有没做完的布局和绘制。
    /// 都没有时窗口暂停显示链接（输入、定时器、任务、后台线程把事件循环叫醒时恢复），画面静止时进程不被唤醒；
    /// 种类只要某个时刻的一帧（插入点闪烁）时也是假，那一刻见 [`Self::next_frame`]
    pub(crate) fn wants_frames(&self) -> bool {
        self.next_frame() == NextFrame::Continuous
    }

    /// 接下来要什么时候的帧：[`NextFrame::Continuous`] 是每帧都要（[`Self::wants_frames`] 的那些情况）；
    /// [`NextFrame::At`] 是只有元素种类要在某个时刻（统一时钟的读数，见 [`crate::kinds::TickContext::now`]）出一帧，
    /// 比如插入点下一次换亮灭：窗口在那之前睡着，到点出一帧；[`NextFrame::Idle`] 是都不要
    pub(crate) fn next_frame(&self) -> NextFrame {
        let busy = self.commit.clock_changed()
            || self.anim.busy()
            || self.drag_drop.busy()
            || (self.shared.flying() > 0 && crate::clock::running())
            || !self.scrolls_idle()
            || self.layout.is_dirty()
            || self.paint.is_dirty()
            || self.paint.has_patches()
            || self.images.playing();
        if busy { NextFrame::Continuous } else { self.kinds_next_frame() }
    }

    /// 渲染线程交回滚动状态的地方（创建渲染线程时交给它）
    pub(crate) fn handback(&self) -> Arc<Handback> {
        self.commit.handback()
    }

    /// 按主线程当前的动画值算出每层的变换（测试和离屏截图用；窗口里由渲染线程算）
    pub(crate) fn layer_transforms(&mut self) -> Vec<scene::LayerTransform> {
        self.sync_timeline_layers();
        let values = |layer: &LayerInfo| {
            // 拖放的影子
            if let Some(values) = self.drag_drop.layer_values(layer.key) {
                return values;
            }
            match self.nodes.get(layer_node(layer.key)) {
                Some(node) => LayerValues {
                    translate: node.paint.translate,
                    scale: node.paint.scale,
                    rotation: node.paint.rotation,
                    opacity: node.paint.opacity,
                    offset: node.offset(),
                },
                None => layer.values,
            }
        };
        let layers = self.paint.layers();
        if !crate::compositor::has_depth(layers) {
            return crate::compositor::compose_flat(layers, |_, layer| values(layer));
        }
        compose(layers, |_, layer| values(layer), |_, layer, depth| match self.nodes.get(layer_node(layer.key)).and_then(|node| node.extra.as_deref()) {
            Some(extra) => crate::compositor::DepthValues {
                rotate: extra.rotate,
                perspective: extra.perspective,
                flight: extra.flight().unwrap_or(FLIGHT_IDENTITY),
            },
            None => depth.values,
        })
    }

    /// 指针此刻的位置（窗口坐标）；在窗口外时是 `None`
    pub(crate) fn pointer(&self) -> Option<[f32; 2]> {
        self.router.pointer()
    }

    /// 起了这个名字的元素
    pub(crate) fn named(&self, name: &str) -> Option<NodeId> {
        self.nodes.named(name)
    }

    /// 命名元素在最近一次布局后的位置（父元素的坐标）
    pub(crate) fn bounds(&self, name: &str) -> Option<Rect> {
        self.nodes.named(name).map(|id| self.nodes[id].bounds)
    }

    /// 命名元素现在显示的文字：自己是文字或输入框时是它的内容（输入法正在组的字不算），
    /// 否则是子孙里按树的顺序第一个文字或输入框的内容（比如按钮的标签）
    pub(crate) fn text_content(&self, name: &str) -> Option<String> {
        let mut stack = vec![self.named(name)?];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            match self.nodes.element_kind(id) {
                Some(kind) => {
                    let mut info = KindInfo::default();
                    kind.describe(&mut info);
                    match info.text {
                        Some(text) => return Some(text),
                        None => stack.extend(node.children.iter().rev()),
                    }
                }
                _ => stack.extend(node.children.iter().rev()),
            }
        }
        None
    }

    /// 建节点，还没挂到父节点上
    fn insert(&mut self, element: &mut Element) -> NodeId {
        let kind = element.kind.take();
        let layout = self.layout.new_node();
        let animations: Vec<AnimSlot> = element
            .animations
            .drain(..)
            .map(|(property, curve)| AnimSlot::new(property, curve))
            .collect();
        // 变换、透明度会动（包括退场动画）、布局位置要滑动的元素做成合成层
        let animated = || animations.iter().map(|slot| slot.property).chain(element.exit.iter().map(|(p, ..)| *p));
        let sliding = animations.iter().any(|slot| slot.property == PropId::LAYOUT_POSITION);
        let layer = sliding || animated().any(is_compositor);
        // 绕 x、y 轴的旋转、透视会动：一直带着 3D 设置，渲染线程才能推进它们
        let extra = animated().any(PropId::is_depth).then(|| Box::new(NodeExtra { animated: true, ..Default::default() }));
        let handles = mem::take(&mut element.motion);
        let mut node = Node {
            layout,
            paint: Paint::default(),
            animations,
            children: Vec::new(),
            text: TextOverrides::default(),
            bounds: Rect::default(),
            world: Affine::IDENTITY,
            parent: None,
            layer,
            motion: 0,
            exiting: false,
            scrolled: false,
            shift: [0.0; 2],
            scroll: None,
            input: None,
            extra,
            has_kind: false,
            flags: Default::default(),
        };
        node.refresh_flags();
        let cold = Cold {
            sources: sources::Sources::of(&element.props),
            name: None,
            slots: Vec::new(),
            exit: mem::take(&mut element.exit),
            handlers: mem::take(&mut element.events.handlers),
        };
        let id = self.nodes.insert(node, cold);
        if let Some(kind) = kind {
            self.nodes.set_kind(id, kind);
            self.layout.set_measured(layout, id);
        }
        if sliding {
            self.anim.track_slide(id);
        }
        if !handles.is_empty() {
            self.join_motion(id, &handles);
        }
        // 种类声明的每帧步骤（时钟、树外更新、和缩放有关）
        if let Some(kind) = self.nodes.element_kind(id) {
            let hooks = kind.hooks();
            self.kinds.add(id, hooks);
        }
        self.layout.invalidate();
        self.layout.restyle();
        id
    }

    /// 设置节点的全部子元素位置
    fn set_slots(&mut self, id: NodeId, slots: Vec<ChildSlot>) {
        let Some(cold) = self.nodes.cold_mut(id) else { return };
        cold.slots = slots;
        self.rebuild_children(id);
    }

    /// 动态位置的子元素变了（增删或重排）。还在退场的旧子元素留在原来的位置附近
    pub(crate) fn set_dynamic(&mut self, id: NodeId, slot: usize, mut children: Vec<NodeId>) {
        let Some(cold) = self.nodes.cold(id) else { return };
        if let ChildSlot::Dynamic(old) = &cold.slots[slot] {
            // 共享元素在飞的时候新挂上的：画在飞的元素后面时盖住它（没有在飞的时什么都不做）
            if self.shared.flying() > 0 {
                let fresh: Vec<NodeId> = children.iter().copied().filter(|c| !old.contains(c)).collect();
                self.shared.mounted(fresh);
            }
            for (index, &child) in old.iter().enumerate() {
                if self.nodes.get(child).is_some_and(|c| c.exiting) && !children.contains(&child) {
                    children.insert(index.min(children.len()), child);
                }
            }
        }
        if let Some(cold) = self.nodes.cold_mut(id) {
            cold.slots[slot] = ChildSlot::Dynamic(children);
        }
        self.rebuild_children(id);
    }

    /// 展开所有位置，同步到布局树
    fn rebuild_children(&mut self, id: NodeId) {
        let mut children = mem::take(&mut self.nodes[id].children);
        children.clear();
        for slot in self.nodes.cold(id).map_or(&[][..], |cold| &cold.slots[..]) {
            match slot {
                ChildSlot::Static(child) => children.push(*child),
                ChildSlot::Dynamic(dynamic) => children.extend(dynamic),
            }
        }
        let mut layouts: Vec<taffy::NodeId> = children.iter().map(|&c| self.nodes[c].layout).collect();
        for &child in &children {
            self.nodes[child].parent = Some(id);
        }
        // 遮罩元素也要布局（绝对定位，铺满这个元素）
        if let Some(mask) = self.nodes[id].mask() {
            layouts.push(self.nodes[mask].layout);
        }
        let layout = self.nodes[id].layout;
        // 富文本里的行内元素不在布局树里（文字是布局树的叶子），截掉的也不算子元素
        if !self.inlines.is_empty() {
            self.filter_inline_children(id, &mut children, &mut layouts);
        }
        self.nodes[id].children = children;
        self.layout.set_children(layout, &layouts);
        self.sort_children(id);
        self.nodes.restructured();
        self.layout.invalidate();
        self.layout.restyle();
        self.accessibility.touch(id);
    }

    /// 卸载一棵子树：删除节点和布局节点，清掉指向它们的焦点、悬停、按下、拖选状态。
    /// 绑定和元素自己的信号由调用方释放对应的作用域
    pub(crate) fn unmount(&mut self, id: NodeId) {
        // 共享元素：先按完整的父节点链量下它们离开时在屏幕上的位置
        self.depart_subtree(id);
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let content = self.nodes.take_kind(id);
            let Some(node) = self.nodes.remove(id) else { continue };
            stack.extend(node.mask());
            stack.extend(node.children);
            // 富文本里被截掉的行内元素不在子元素里
            if !self.inlines.is_empty() {
                self.inlines.forget(id);
                stack.extend(inline::hidden_children(content.as_deref()));
            }
            self.accessibility.forget(id);
            if node.motion != 0 {
                self.forget_motion(id);
            }
            // 声明在它上面的浮层一起卸载
            if let Some(input) = &node.input {
                stack.extend(input.overlays.iter().copied());
            }
            // 种类自己还回占着的资源（路径图集的块、图片的引用）
            if let Some(mut kind) = content {
                kind.unmount(&mut UnmountContext { text: &mut self.text, images: self.images.system_mut() });
            }
            self.layout.remove(node.layout);
            // 带 3D 设置、飞过的元素也可能在渲染线程那边留着动画
            if node.layer || node.extra.is_some() {
                self.commit.remove(layer_key(id));
            }
            if let Some(s) = node.scroll.as_deref() {
                self.commit.remove_all([0, 1].map(|axis| scrolling::thumb_key(id, axis)));
                if s.own {
                    self.commit.remove(crate::scroll::own_content_key(id));
                }
            }
        }
        self.overlays.retain(|id| self.nodes.contains_key(id));
        if !self.inlines.is_empty() {
            self.inlines.retain(|id| self.nodes.contains_key(id));
        }
        self.forget_overrides();
        // 焦点、悬停、按下、拖选、链接、拖动、拖放里指向卸载了的节点的都清掉
        self.forget_nodes(Leave::Unmount);
        self.nodes.restructured();
        self.kinds.retain(|id| self.nodes.contains_key(id));
        self.forget_selection_roots();
        self.layout.invalidate();
    }

    /// 沿树向下计算继承的文字样式，写进文字和输入框；只有样式真的变了才重新排版
    fn resolve_styles(&mut self) {
        let colors = *self.paint.colors();
        let root = Inherited::root(colors.text, colors.selection);
        let mut stack: Vec<(NodeId, Inherited)> = self.nodes.roots().iter().copied().map(|id| (id, root.clone())).collect();
        while let Some((id, parent)) = stack.pop() {
            let node = &mut self.nodes[id];
            let own = &node.text;
            let inherited = Inherited {
                size: own.size.unwrap_or(parent.size),
                weight: own.weight.unwrap_or(parent.weight),
                line_height: own.line_height.unwrap_or(parent.line_height),
                color: own.color.clone().unwrap_or(parent.color),
                family: own.family.unwrap_or(parent.family),
                italic: own.italic || parent.italic,
                letter_spacing: own.letter_spacing.unwrap_or(parent.letter_spacing),
                align: own.align.unwrap_or(parent.align),
                tabular: own.tabular || parent.tabular,
                selection: own.selection.unwrap_or(parent.selection),
            };
            let (underline, strikethrough) = (own.underline, own.strikethrough);
            if node.paint.color != inherited.color {
                node.paint.color = inherited.color.clone();
                self.paint.invalidate();
            }
            let style = TextStyle {
                size: inherited.size,
                weight: inherited.weight,
                line_height: inherited.line_height,
                underline,
                strikethrough,
                family: inherited.family,
                italic: inherited.italic,
                letter_spacing: inherited.letter_spacing,
                align: inherited.align,
                tabular: inherited.tabular,
            };
            let selection = inherited.selection;
            // 文字是默认的正文色时，输入框的占位文字用默认的占位色（自己定了文字颜色时按它变淡）
            let placeholder = (inherited.color == Fill::Color(colors.text)).then_some(colors.placeholder);
            stack.extend(node.children.iter().chain(node.mask().as_ref()).map(|&child| (child, inherited.clone())));
            // 浮层的内容从声明它的元素继承
            if let Some(input) = &node.input {
                stack.extend(input.overlays.iter().map(|&host| (host, inherited.clone())));
            }
            if let Some(kind) = self.nodes.element_kind_mut(id) {
                let cx = StyleContext { style, selection, inactive: colors.rich.inactive, placeholder };
                let dirty = kind.restyle(&cx);
                if !dirty.is_empty() {
                    self.apply_dirty(id, dirty);
                }
            }
        }
    }

    fn set_text_override(&mut self, id: NodeId, f: impl FnOnce(&mut TextOverrides)) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        let before = node.text.clone();
        f(&mut node.text);
        if node.text != before {
            self.layout.restyle();
        }
    }

    fn write(&mut self, id: NodeId, property: PropId, value: PropertyValue) {
        match (property, value) {
            (PropId::WIDTH, PropertyValue::Float(v)) => self.update_style(id, |s| s.size.width = Dimension::length(v)),
            (PropId::HEIGHT, PropertyValue::Float(v)) => self.update_style(id, |s| s.size.height = Dimension::length(v)),
            (PropId::COLOR, value @ (PropertyValue::Color(_) | PropertyValue::Gradient(_))) => {
                let Some(node) = self.nodes.get_mut(id) else { return };
                node.text.color = value.clone().into_fill();
                if node.paint.set(PropId::COLOR, value) {
                    self.paint.invalidate();
                }
                // 有子元素时它们继承这个颜色
                if !node.children.is_empty() {
                    self.layout.restyle();
                }
            }
            // 路径变形的进度：停稳时正好是 1，变形结束，换成目标路径本身（用缓存的栅格化结果）
            (PropId::PATH, PropertyValue::Float(t)) => {
                let mut released = None;
                self.update_path(id, |block| {
                    if t == 1.0 {
                        released = block.morph.take().map(|m| m.slot);
                    } else if let Some(morph) = block.morph.as_deref_mut() {
                        morph.progress = t;
                    }
                });
                if let Some(slot) = released {
                    self.text.release_slot(slot);
                }
            }
            (PropId::PADDING, PropertyValue::Float(v)) => self.update_style(id, |s| {
                let v = LengthPercentage::length(v);
                s.padding = taffy::Rect { left: v, right: v, top: v, bottom: v };
            }),
            (PropId::GAP, PropertyValue::Float(v)) => {
                self.update_style(id, |s| s.gap = Size { width: LengthPercentage::length(v), height: LengthPercentage::length(v) })
            }
            (property, value) => {
                let Some(node) = self.nodes.get_mut(id) else { return };
                // 半透明的元素还是半透明：它是一个层（分组的候选），只改层的透明度
                let translucent = |o: f32| o > 0.0 && o < 1.0;
                let faded = property == PropId::OPACITY && translucent(node.paint.opacity);
                if node.set_value(property, value) {
                    let faded = faded && translucent(node.paint.opacity);
                    // 颜色、圆角、边框、一层外投影：不影响结构和图元个数，等着原地修补（改不了时绘制阶段整个重新生成）。
                    // 裁剪子元素的元素的圆角也是裁剪的形状，不修补
                    if faded {
                        self.paint.patch_opacity(id);
                    } else if patch::patches(property) && !(property == PropId::RADIUS && node.flags.contains(paint::PaintFlags::CLIP)) {
                        self.paint.patch(id);
                    } else {
                        self.paint.invalidate();
                    }
                    // 变换变了：无障碍树里的位置跟着变；这棵子树到屏幕的变换在绘制之前重算
                    if is_compositor(property) {
                        self.accessibility.touch(id);
                        if property != PropId::OPACITY {
                            self.paint.moved(id);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn update_style(&mut self, id: NodeId, f: impl FnOnce(&mut Style)) {
        let Some(node) = self.nodes.get(id) else { return };
        self.layout.update_style(node.layout, f);
    }

    fn update_content(&mut self, id: NodeId, content: String) {
        let changed = self.nodes.kind_mut::<TextBlock>(id).is_some_and(|block| block.update(|text, _| *text = content));
        if changed {
            let layout = self.nodes[id].layout;
            self.layout.mark_dirty(layout);
            self.accessibility.touch(id);
        }
    }

    /// 改文字块的设置（最多几行、能否选中）：返回 true 时要重新布局
    fn update_text_block(&mut self, id: NodeId, f: impl FnOnce(&mut TextBlock) -> bool) {
        let Some(block) = self.nodes.kind_mut::<TextBlock>(id) else { return };
        if f(block) {
            let layout = self.nodes[id].layout;
            self.layout.mark_dirty(layout);
        }
    }

    /// 加载了新字体：所有文字和输入框重新排版
    fn invalidate_text(&mut self) {
        let ids: Vec<NodeId> = self.nodes.keys().collect();
        for id in ids {
            if let Some(kind) = self.nodes.element_kind_mut(id) {
                let dirty = kind.refresh_fonts();
                if !dirty.is_empty() {
                    self.apply_dirty(id, dirty);
                }
            }
        }
        self.layout.invalidate();
    }

    /// 改路径元素的路径或描边：画布大小变了要重新布局
    fn update_path(&mut self, id: NodeId, f: impl FnOnce(&mut PathBlock)) {
        let Some(block) = self.nodes.kind_mut::<PathBlock>(id) else { return };
        let before = block.intrinsic();
        f(block);
        let resized = block.intrinsic() != before;
        if resized {
            let layout = self.nodes[id].layout;
            self.layout.mark_dirty(layout);
        }
        self.paint.invalidate();
    }

    /// 路径元素绑定的路径换了：声明了 [`Property::Path`] 动画时从此刻的形状变过去
    /// （第一次的值、`without_animation` 里的不变形）；中途再换时从此刻的样子接着变，换回原来的路径时沿原路返回
    fn set_path(&mut self, id: NodeId, path: crate::vector::VectorPath) {
        let Some(node) = self.nodes.get(id) else { return };
        let index = node.animations.iter().position(|slot| slot.property == PropId::PATH);
        // 进行中的变形此刻的速度（中途换路径时接着这个速度变）
        let now = self.anim.now();
        let speed = index.and_then(|i| node.animations[i].track.as_ref()).map_or(0.0, |track| {
            let mut velocity = [0.0; MAX_COMPONENTS];
            track.velocity(now, &mut velocity);
            velocity[0]
        });
        let Some(block) = self.nodes.kind_mut::<PathBlock>(id) else { return };
        let old = block.path.clone();
        if old.as_ref() == Some(&path) {
            return;
        }
        let animate = index.is_some() && old.is_some() && !animations_disabled();
        if !animate {
            let slot = block.morph.take().map(|m| m.slot);
            self.update_path(id, |b| b.path = Some(path));
            if let Some(slot) = slot {
                self.text.release_slot(slot);
            }
            if let Some(index) = index {
                // 停下进行中的变形
                let slot = &mut self.nodes[id].animations[index];
                slot.track = Some(CurveTrack::new(slot.curve, &[1.0], PropId::PATH.tolerance()));
                if mem::take(&mut slot.queued) {
                    self.anim.dequeue(id, index);
                }
            }
            return;
        }
        let index = index.expect("上面判断过");
        let old = old.expect("上面判断过");
        let even_odd = path.is_even_odd();
        let (pair, velocity, source) = match block.morph.take() {
            None => (self.text.morph_pair(&old, &path), 0.0, Some(old.clone())),
            Some(morph) => {
                let slot = morph.slot;
                self.text.release_slot(slot);
                if morph.source.as_ref() == Some(&path) {
                    // 改回原来的路径：沿原路返回
                    let (pair, v) = crate::morph::reverse(&morph.pair, morph.progress, speed);
                    (Arc::new(pair), v, Some(old.clone()))
                } else {
                    let (pair, v) = crate::morph::retarget(&morph.pair, morph.progress, speed, path.shape(), path.view_box_rect(), even_odd);
                    (Arc::new(pair), v, None)
                }
            }
        };
        let Some(block) = self.nodes.kind_mut::<PathBlock>(id) else { return };
        block.morph = Some(Box::new(crate::morph::Morph { pair, progress: 0.0, source, slot: None, even_odd }));
        self.update_path(id, |b| b.path = Some(path));
        let slot = &mut self.nodes[id].animations[index];
        // 进度从 0 带着换算过来的速度奔向 1
        let mut track = CurveTrack::new(slot.curve, &[1.0], PropId::PATH.tolerance());
        track.restart(now, &[0.0], &[velocity]);
        slot.track = Some(track);
        if !slot.queued {
            slot.queued = true;
            self.anim.queue(id, index);
        }
    }

    /// 图片元素绑定的图片变了或者解码好了，返回现在有没有图片在显示
    pub(crate) fn set_image(&mut self, id: NodeId, image: ImageRef) -> bool {
        let Some(layout) = self.nodes.get(id).map(|node| node.layout) else { return false };
        let Some(block) = self.nodes.kind_mut::<ImageBlock>(id) else { return false };
        let before = block.intrinsic();
        if self.images.system_mut().update(block, image) {
            self.paint.invalidate();
            // 换了图：动图从第一帧开始播，句柄上的"播完了"清掉
            if block.player.is_some() {
                if let Some(handle) = block.handle {
                    self.images.write(handle.finished, false);
                }
                self.track_player(id);
            }
        }
        let Some(block) = self.nodes.kind::<ImageBlock>(id) else { return false };
        let (resized, showing) = (block.intrinsic() != before, block.showing());
        if resized {
            self.layout.mark_dirty(layout);
        }
        showing
    }

    /// 改图片的摆放方式（fit、对齐），只重画
    pub(crate) fn update_image(&mut self, id: NodeId, f: impl FnOnce(&mut ImageBlock)) {
        let Some(block) = self.nodes.kind_mut::<ImageBlock>(id) else { return };
        f(block);
        self.paint.invalidate();
    }

    /// 输入框绑定的值从外部变了
    /// 密码模式开关（`Element::password` 的绑定）
    fn set_password(&mut self, id: NodeId, on: bool) {
        if let Some(editor) = self.nodes.kind_mut::<EditorBlock>(id)
            && editor.set_password(on)
        {
            self.paint.invalidate();
            self.accessibility.touch(id);
        }
    }

    fn update_editor_value(&mut self, id: NodeId, value: &str) {
        let Some(layout) = self.nodes.get(id).map(|node| node.layout) else { return };
        if let Some(editor) = self.nodes.kind_mut::<EditorBlock>(id)
            && editor.set_value(value)
        {
            self.paint.invalidate();
            self.accessibility.touch(id);
            if editor.take_resized() {
                self.layout.mark_dirty(layout);
            }
        }
    }

    /// 切换焦点，返回需要调用的 on_focus 处理器。输入目标得到、失去焦点（[`crate::kinds::InputTarget::on_focus`]）；
    /// 输入框失去焦点时内容和获得焦点时不一样：改完了（`on_commit`）
    fn set_focus(&mut self, target: Option<NodeId>) -> FocusCallbacks {
        if self.router.focused() == target {
            return Vec::new();
        }
        // 焦点离开正在组字的元素：放弃组字，平台的输入法也重置（之后的提交不会落到新焦点上）
        if self.router.composing().is_some_and(|c| Some(c) != target) {
            self.cancel_composition(true);
        }
        let mut callbacks = Vec::new();
        for (id, focused) in [(self.router.focused(), false), (target, true)].into_iter().filter_map(|(id, f)| Some((id?, f))) {
            if let Some(text) = self.target_focus_changed(id, focused) {
                let handlers = self.nodes.cold(id).map(|c| c.handlers.of(EventType::Commit, Handler::text_change)).unwrap_or_default();
                let event = crate::event::ChangeEvent { value: text, source: self.change_source() };
                callbacks.push((Rc::new(move |_| handlers.iter().for_each(|h| h(&event))) as Rc<dyn Fn(bool)>, false));
            }
            callbacks.extend(self.handlers_of(id, EventType::Focus, Handler::flag).into_iter().map(|handler| (handler, focused)));
        }
        self.router.set_focused(target);
        self.paint.invalidate();
        callbacks
    }

    /// 多行输入框的插入点动过：让外面的视口把它滚进来（布局之后、绘制之前，用新的内容高度）
    fn reveal_caret(&mut self, now: f64) {
        let Some(id) = self.router.focused() else { return };
        let Some(editor) = self.nodes.kind_mut::<EditorBlock>(id) else { return };
        let Some(caret) = editor.take_reveal() else { return };
        if let Some(viewport) = self.viewport_of(id) {
            self.reveal(viewport, caret, now);
        }
    }

    /// 文字、输入框留着的字形图元都不再用，重画一次（测试用：对照复用的和整段重新生成的图元）
    pub(crate) fn forget_glyphs(&mut self) {
        self.text.forget_glyphs();
        self.paint.invalidate();
    }

    /// 文字、输入框累计重新生成过多少个字形图元（测试和基准用：确认编辑时只重新生成改动的那一段）
    pub(crate) fn generated_glyph_count(&self) -> usize {
        self.text.generated
    }

    /// 所有输入框累计整形过多少段（测试和基准用：确认编辑时只重排了改动的那一段）
    pub(crate) fn shaped_paragraph_count(&self) -> usize {
        self.nodes
            .keys()
            .filter_map(|id| self.nodes.kind::<EditorBlock>(id).map(EditorBlock::shaped))
            .sum()
    }

    /// 修饰键变了（窗口转来的；Shift+点击扩展选区要用）
    pub(crate) fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.router.set_modifiers(modifiers);
    }

    /// 改输入框的设置（属性表的写入）：`f` 返回 true 时重画、重新布局
    fn update_editor(&mut self, id: NodeId, f: impl FnOnce(&mut EditorBlock) -> bool) {
        let Some(layout) = self.nodes.get(id).map(|node| node.layout) else { return };
        if let Some(editor) = self.nodes.kind_mut::<EditorBlock>(id)
            && f(editor)
        {
            self.paint.invalidate();
            self.layout.mark_dirty(layout);
        }
    }

    /// 元素用的自定义着色器
    fn shader_of(&self, id: NodeId) -> Option<Shader> {
        let index = self.nodes.get(id)?.paint.shader;
        self.paint.shader(index)
    }

    /// 去掉元素自己设的文字颜色（回到继承的）
    fn clear_color(&mut self, id: NodeId) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        if node.text.color.take().is_some() {
            self.layout.restyle();
        }
    }

    /// 换图（按名字写 `source` 时）：开始解码，解码好之前继续显示旧图
    fn request_image(&mut self, id: NodeId, source: &crate::image::ImageSource) {
        let image = reactive::untracked(|| crate::image::acquire(source));
        self.set_image(id, image);
    }

    fn update_paint(&mut self, id: NodeId, f: impl FnOnce(&mut Paint)) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        let before = node.paint.clone();
        f(&mut node.paint);
        if node.paint != before {
            node.refresh_flags();
            self.paint.invalidate();
        }
    }
}
