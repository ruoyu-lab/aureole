//! 布局状态：布局树（每个元素一个布局节点）、内部根节点和窗口大小、布局和文字样式的脏标记、窗口大小的信号。
//!
//! 内容（文字、输入框……）存在元素树的节点上，布局节点只记着它属于哪个元素节点（带内容的才记），
//! 量叶子时按它找到内容。
//!
//! 字段私有：别的模块通过方法读布局结果、改样式、标记要重新布局。真正量文字、排版的 [`LayoutState::compute`]
//! 也在这里，它要文字系统和节点（内容在节点上）。

use taffy::{AvailableSpace, Baselines, Dimension, Display, FlexDirection, Position, ResolveOrZero, Size, Style, TaffyTree};

use super::node::NodeStore;
use super::NodeId;
use crate::kinds::{AvailableSpace as Space, ElementKind, MeasureContext};
use crate::responsive::WindowMetrics;
use crate::text::TextSystem;

pub(super) struct LayoutState {
    /// 带内容的元素的布局节点上记着元素节点（量叶子时按它找到内容）
    taffy: TaffyTree<NodeId>,
    /// 内部根节点，大小等于窗口；用户的根元素挂在它下面
    root: taffy::NodeId,
    viewport: [f32; 2],
    /// 窗口大小的信号（[`crate::window_size_class`] 等读它），每帧开始时把新大小写进去
    metrics: Option<WindowMetrics>,
    /// 窗口大小变了，还没写进信号
    viewport_write: bool,
    /// 窗口大小变了，要重新布局（和 `dirty` 分开：只是窗口大小变了时布局位置的过渡不滑）
    viewport_dirty: bool,
    /// 要重新布局
    dirty: bool,
    /// 文字样式需要重新沿树继承
    styles_dirty: bool,
    /// 布局过几次（文字换了内容、重新断过行都要经过布局）：记着它就知道中间有没有布局过
    passes: u64,
}

impl LayoutState {
    pub(super) fn new(width: f32, height: f32) -> Self {
        let mut taffy = TaffyTree::new();
        let root = taffy
            .new_leaf(Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                size: Size { width: Dimension::length(width), height: Dimension::length(height) },
                ..Default::default()
            })
            .expect("创建布局根节点失败");
        Self {
            taffy,
            root,
            viewport: [width, height],
            metrics: None,
            viewport_write: false,
            viewport_dirty: false,
            dirty: true,
            styles_dirty: true,
            passes: 0,
        }
    }

    // ---- 窗口 ----

    /// 窗口内容区的大小，逻辑像素
    pub(super) fn viewport(&self) -> [f32; 2] {
        self.viewport
    }

    /// 窗口大小变了：内部根节点跟着变，下一帧重新布局、把新大小写进信号
    pub(super) fn set_viewport(&mut self, width: f32, height: f32) {
        if self.viewport == [width, height] {
            return;
        }
        self.viewport = [width, height];
        let mut style = self.taffy.style(self.root).expect("布局根节点丢失").clone();
        style.size = Size { width: Dimension::length(width), height: Dimension::length(height) };
        self.taffy.set_style(self.root, style).expect("更新布局根节点失败");
        self.viewport_dirty = true;
        self.viewport_write = true;
    }

    pub(super) fn set_metrics(&mut self, metrics: WindowMetrics) {
        self.metrics = Some(metrics);
    }

    /// 窗口大小变了还没写进信号：返回信号和新大小
    pub(super) fn take_viewport_write(&mut self) -> Option<(WindowMetrics, [f32; 2])> {
        if !std::mem::take(&mut self.viewport_write) {
            return None;
        }
        self.metrics.clone().map(|m| (m, self.viewport))
    }

    /// 窗口大小的句柄（[`LayoutState::set_metrics`] 之前没有）
    pub(super) fn metrics(&self) -> Option<WindowMetrics> {
        self.metrics.clone()
    }

    // ---- 布局节点 ----

    /// 建一个布局节点
    pub(super) fn new_node(&mut self) -> taffy::NodeId {
        // 每个元素都是子元素绝对定位的参照（taffy 默认的 Static 不是，绝对定位的元素会一路找到根节点）
        let style = Style { display: Display::Flex, position: Position::Relative, ..Default::default() };
        self.taffy.new_leaf(style).expect("创建布局节点失败")
    }

    /// 带内容的元素：布局节点记着它的节点，量叶子时按它找到内容
    pub(super) fn set_measured(&mut self, node: taffy::NodeId, id: NodeId) {
        self.taffy.set_node_context(node, Some(id)).expect("布局节点丢失");
    }

    pub(super) fn remove(&mut self, node: taffy::NodeId) {
        let _ = self.taffy.remove(node);
    }

    pub(super) fn set_children(&mut self, node: taffy::NodeId, children: &[taffy::NodeId]) {
        self.taffy.set_children(node, children).expect("更新子元素失败");
    }

    /// 用户的根元素、浮层挂到内部根节点下面
    pub(super) fn add_root(&mut self, node: taffy::NodeId) -> taffy::TaffyResult<()> {
        self.taffy.add_child(self.root, node)
    }

    pub(super) fn parent(&self, node: taffy::NodeId) -> Option<taffy::NodeId> {
        self.taffy.parent(node)
    }

    /// 最近一次布局的结果（相对父元素的位置、大小、内边距、边框）
    #[inline]
    pub(super) fn get(&self, node: taffy::NodeId) -> &taffy::Layout {
        self.taffy.layout(node).expect("布局节点丢失")
    }

    /// 同 [`Self::get`]，布局节点不在时是 `None`
    #[inline]
    pub(super) fn try_get(&self, node: taffy::NodeId) -> Option<&taffy::Layout> {
        self.taffy.layout(node).ok()
    }

    pub(super) fn style(&self, node: taffy::NodeId) -> &Style {
        self.taffy.style(node).expect("布局节点丢失")
    }

    /// 同 [`Self::style`]，布局节点不在时是 `None`
    pub(super) fn try_style(&self, node: taffy::NodeId) -> Option<&Style> {
        self.taffy.style(node).ok()
    }

    /// 改样式，真的变了才要重新布局
    pub(super) fn update_style(&mut self, node: taffy::NodeId, f: impl FnOnce(&mut Style)) {
        let current = self.style(node);
        let mut style = current.clone();
        f(&mut style);
        if &style != current {
            self.taffy.set_style(node, style).expect("更新布局失败");
            self.dirty = true;
        }
    }

    /// 内容的大小变了（文字改了、输入框的行数变了……）：这个节点重新量，要重新布局
    pub(super) fn mark_dirty(&mut self, node: taffy::NodeId) {
        self.taffy.mark_dirty(node).expect("布局节点丢失");
        self.dirty = true;
    }

    // ---- 脏标记 ----

    /// 要重新布局
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// 文字样式要重新沿树继承
    pub(super) fn restyle(&mut self) {
        self.styles_dirty = true;
    }

    /// 取走"文字样式要重新继承"的标记
    pub(super) fn take_restyle(&mut self) -> bool {
        std::mem::take(&mut self.styles_dirty)
    }

    /// 有没有要做的布局：有的话，`Some(true)` 是样式变了（布局位置的过渡要滑），`Some(false)` 是只有窗口大小变了
    pub(super) fn pending(&self) -> Option<bool> {
        (self.dirty || self.viewport_dirty).then_some(self.dirty)
    }

    /// 这一帧的布局做完了
    pub(super) fn laid_out(&mut self) {
        self.dirty = false;
        self.viewport_dirty = false;
    }

    /// 布局过几次
    pub(super) fn passes(&self) -> u64 {
        self.passes
    }

    /// 还有没做的布局或样式继承
    pub(super) fn is_dirty(&self) -> bool {
        self.dirty || self.viewport_dirty || self.styles_dirty
    }

    // ---- 计算 ----

    /// 布局整棵树（只重算标脏的部分）
    pub(super) fn compute(&mut self, text: &mut TextSystem, nodes: &mut NodeStore) {
        self.passes += 1;
        let [width, height] = self.viewport;
        let space = Size { width: AvailableSpace::Definite(width), height: AvailableSpace::Definite(height) };
        compute_from(&mut self.taffy, self.root, space, text, nodes);
    }

    /// 布局不挂在内部根节点下面的一棵（富文本里的行内元素）：`width` 是百分比、换行参照的宽度，`None` 时不限
    pub(super) fn compute_detached(&mut self, node: taffy::NodeId, width: Option<f32>, text: &mut TextSystem, nodes: &mut NodeStore) {
        let space = Size { width: width.map_or(AvailableSpace::MaxContent, AvailableSpace::Definite), height: AvailableSpace::MaxContent };
        compute_from(&mut self.taffy, node, space, text, nodes);
    }

    /// 这个节点的布局结果过时了（自己或子孙标了脏）
    pub(super) fn is_stale(&self, node: taffy::NodeId) -> bool {
        self.taffy.dirty(node).unwrap_or(true)
    }
}

/// 布局 `root` 这一棵（只重算标脏的部分）
fn compute_from(taffy: &mut TaffyTree<NodeId>, root: taffy::NodeId, space: Size<AvailableSpace>, text: &mut TextSystem, nodes: &mut NodeStore) {
    taffy
        .compute_layout_with_measure(root, space, |inputs, _, id, style| match id.and_then(|&mut id| nodes.element_kind_mut(id)) {
            Some(kind) => measure_kind(&mut **kind, inputs, style, text),
            None => {
                let mut output = taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, _| Size {
                    width: known.width.unwrap_or(0.0),
                    height: known.height.unwrap_or(0.0),
                });
                // 容器刚失去最后一个子元素（比如绝对定位的子元素退场后卸载）时按叶子布局：taffy 只在有
                // 定位区域时清理它记着的绝对定位子孙，叶子没有，会留下已删除的节点，下次取整时越界。
                // 给一个空的定位区域，让它清掉
                output.oof_positioning_area = Some(taffy::OofPositioningArea { size: output.size, offset: taffy::Point::ZERO });
                output
            }
        })
        .expect("布局失败");
}

/// 种类量叶子：大小是内容区的，基线相对内容区顶部（这里加上上内边距和上边框报给布局）
fn measure_kind(kind: &mut dyn ElementKind, inputs: taffy::LayoutInput, style: &Style, text: &mut TextSystem) -> taffy::LayoutOutput {
    let mut output = taffy::compute_leaf_layout(inputs, style, |_, _| 0.0, |known, available| {
        let mut cx = MeasureContext { text };
        let [width, height] = kind.measure(&mut cx, [known.width, known.height], [Space::from_layout(available.width), Space::from_layout(available.height)]);
        Size { width, height }
    });
    if let Some((first, last)) = kind.baselines() {
        let parent_width = inputs.parent_size.width;
        let top = style.padding.top.resolve_or_zero(parent_width, |_, _| 0.0) + style.border.top.resolve_or_zero(parent_width, |_, _| 0.0);
        output.baselines = Baselines { first: Some(top + first), last: Some(top + last) };
    }
    output
}
