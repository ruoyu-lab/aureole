//! 节点和树的结构：每个元素一个节点，冷热分开存。
//!
//! 绘制遍历、命中测试、布局每帧按节点读的字段（布局节点、绘制属性、子元素、位置、变换……）留在 [`Node`] 里，
//! 和节点一起存在 `SlotMap`；只在挂载、卸载、事件分发、按名字找元素时才读的（名字、处理器、退场值、子元素的位置）
//! 放在按同一个键存的冷存储 [`Cold`] 里。节点每大一点，一万个元素的重画就慢一点。
//! 带内容的元素（文字、输入框……）的种类对象（[`crate::kinds::ElementKind`]）也按同一个键另存，节点上只有一个标记：
//! 绘制遍历看到容器时不用去查它。
//!
//! 节点本身是数据记录，字段对整个 `tree` 模块可见；存储（增删、按名字找、根元素、结构版本）只通过 [`NodeStore`] 的方法。

use std::ops::{Index, IndexMut};

use animation::Curve;
use slotmap::{Key as _, KeyData, SecondaryMap, SlotMap, new_key_type};

use super::depth::NodeExtra;
use super::interaction::NodeInput;
use super::{AnimSlot, Paint, TextOverrides};
use crate::kinds::ElementKind;
use crate::property::{PropId, PropertyValue};
use crate::handlers::{EventType, Handler, HandlerList};
use crate::geometry::Affine;
use crate::scroll::Scroller;
use crate::geometry::Rect;

new_key_type! {
    pub struct NodeId;
}

/// 显示列表里层的标识（元素节点的键）
pub(super) fn layer_key(id: NodeId) -> u64 {
    id.data().as_ffi()
}

/// 节点在存储里的下标（同一时刻活着的节点各不相同，删掉后会被新节点重用）：按节点存的密集的表用它
#[inline(always)]
pub(super) fn slot_index(id: NodeId) -> usize {
    id.data().as_ffi() as u32 as usize
}

pub(super) fn layer_node(key: u64) -> NodeId {
    KeyData::from_ffi(key).into()
}

/// 子元素的一个位置：静态子元素，或者一组由 `each` / `child_when` 管理的动态子元素。
/// 节点的 `children` 是所有位置按顺序展开的结果
pub(super) enum ChildSlot {
    Static(NodeId),
    Dynamic(Vec<NodeId>),
}

/// 节点上每帧要读的部分（绘制遍历、命中测试、布局、动画推进）
pub(super) struct Node {
    pub(super) layout: taffy::NodeId,
    pub(super) paint: Paint,
    pub(super) animations: Vec<AnimSlot>,
    pub(super) children: Vec<NodeId>,
    pub(super) text: TextOverrides,
    /// 最近一次布局后的位置，逻辑像素，不含变换
    pub(super) bounds: Rect,
    /// 布局坐标到屏幕坐标的累积变换，命中测试用。有透视时（自己或祖先绕 x、y 轴转过）第三行在 `extra` 里，
    /// 用 [`Node::world`] 取完整的
    pub(super) world: Affine,
    pub(super) parent: Option<NodeId>,
    /// 正在退场：不响应指针，动画结束后卸载
    pub(super) exiting: bool,
    /// 声明了变换或透明度动画：是一个合成层，这些动画由渲染线程推进
    pub(super) layer: bool,
    /// 滚动视口的内容：`shift` 是滚动偏移，渲染线程按自己推进的滚动加上，交给它的额外平移不含这一部分
    pub(super) scrolled: bool,
    /// 关键帧时间线的标记：是某个句柄的成员、有属性被接管着（接管了哪些属性记在时间线那边，见 `tree/timeline.rs`）
    pub(super) motion: u8,
    /// 滚动视口的内容：滚动带来的平移，是合成层的额外平移（不在 `paint` 里，改它不重建图元）
    pub(super) shift: [f32; 2],
    /// 滚动视口的状态
    pub(super) scroll: Option<Box<Scroller>>,
    /// 叠放次序、焦点、指针手势、按键、声明的浮层（用到时才分配）
    pub(super) input: Option<Box<NodeInput>>,
    /// 绕 x、y 轴的旋转、透视、变换中心、背面、共享元素（用到时才分配）
    pub(super) extra: Option<Box<NodeExtra>>,
    /// 有种类对象（带内容的元素：文字、输入框、路径、图片、字符网格、画布、自己定义的，种类对象存在 [`NodeStore`] 里）：
    /// 绘制遍历先看它，容器不用去查（放在几个布尔的空隙里，节点不变大）
    pub(super) has_kind: bool,
    /// 不常见的绘制特征（写属性、改节点时维护）：绘制遍历先看它决定走不走快路径
    pub(super) flags: super::paint::PaintFlags,
}

/// 节点上只在挂载、卸载、事件分发、按名字找元素时才读的部分
#[derive(Default)]
pub(super) struct Cold {
    /// 元素设了哪些属性、哪些是绑定、哪些是样式对象设的（属性的来源，见 `tree/sources.rs`）
    pub(super) sources: super::sources::Sources,
    pub(super) name: Option<Box<str>>,
    /// 子元素的位置（静态的和动态的），展开后是节点的 `children`
    pub(super) slots: Vec<ChildSlot>,
    /// 被移除时要过渡到的属性值
    pub(super) exit: Vec<(PropId, PropertyValue, Option<Curve>)>,
    /// 元素上登记的处理器（见 `handlers.rs`）
    pub(super) handlers: HandlerList,
}

impl Node {
    /// 遮罩元素
    #[inline]
    pub(super) fn mask(&self) -> Option<NodeId> {
        self.paint.effects.as_ref().and_then(|e| e.mask)
    }

    /// 合成层的额外平移：滚动偏移，加上布局位置的过渡（[`Property::LayoutPosition`] 动画的当前值：离新的布局位置还差多少）。
    /// 过渡的值放在动画里，节点不用多一个字段（节点每大一点，一万个元素的重画就慢一点）
    #[inline]
    pub(super) fn offset(&self) -> [f32; 2] {
        match self.slide() {
            Some(a) => [self.shift[0] + a[0], self.shift[1] + a[1]],
            None => self.shift,
        }
    }

    /// 布局位置的过渡此刻的值
    #[inline]
    fn slide(&self) -> Option<&[f32]> {
        match self.layer {
            true => self.animations.iter().find(|s| s.property == PropId::LAYOUT_POSITION && s.track.is_some()).map(|s| &s.slide[..]),
            false => None,
        }
    }

    /// 交给渲染线程的额外平移：滚动视口的内容层不含滚动偏移（渲染线程按自己推进的滚动加上）
    #[inline]
    pub(super) fn layer_offset(&self) -> [f32; 2] {
        match (self.scrolled, self.slide()) {
            (true, Some(a)) => [a[0], a[1]],
            (true, None) => [0.0; 2],
            (false, _) => self.offset(),
        }
    }
}

/// 所有节点：热的部分、冷的部分、种类对象按同一个键存；还有用户的根元素和树的结构版本
pub(super) struct NodeStore {
    hot: SlotMap<NodeId, Node>,
    cold: SecondaryMap<NodeId, Cold>,
    /// 带内容的节点的种类对象（节点上的 `has_kind` 为真的才有）
    kinds: SecondaryMap<NodeId, Box<dyn ElementKind>>,
    /// 带内容的节点的内容区离四边多远（内边距加边框：左、上、右、下），定位时按布局写（见 `tree/place.rs`）
    insets: SecondaryMap<NodeId, [f32; 4]>,
    /// 用户的根元素（挂在布局的内部根节点下面）
    roots: Vec<NodeId>,
    /// 树的结构版本：增删、重排子元素时加一（选择根按它判断成员表要不要重算）
    structure: u64,
    /// 有过按叠放次序排序的子元素：没有时画和命中测试直接用 `children`，不用去读节点的输入状态
    sorted: bool,
    /// 登记过的处理器的事件种类（各节点处理器表位掩码的并，只增不减）：整棵树都没有某种事件的处理器时，
    /// 分发连命中测试都不用做（滚轮）
    listening: u64,
}

impl NodeStore {
    pub(super) fn new() -> Self {
        Self {
            hot: SlotMap::with_key(),
            cold: SecondaryMap::new(),
            kinds: SecondaryMap::new(),
            insets: SecondaryMap::new(),
            roots: Vec::new(),
            structure: 0,
            sorted: false,
            listening: 0,
        }
    }

    /// 存一个新节点
    pub(super) fn insert(&mut self, node: Node, cold: Cold) -> NodeId {
        self.listening |= cold.handlers.mask();
        let id = self.hot.insert(node);
        self.cold.insert(id, cold);
        id
    }

    /// 挂载以后给节点加一个处理器
    pub(super) fn add_handler(&mut self, id: NodeId, kind: EventType, handler: Handler) {
        if let Some(cold) = self.cold.get_mut(id) {
            cold.handlers.push(kind, handler);
            self.listening |= kind.bit();
        }
    }

    /// 树上有没有登记过这些位（[`HandlerList::mask`] 的位）的处理器；卸载了的也算，宁可多做一次分发
    #[inline]
    pub(super) fn ever_listens(&self, mask: u64) -> bool {
        self.listening & mask != 0
    }

    /// 删掉一个节点（冷的部分、内容一起），返回热的部分
    pub(super) fn remove(&mut self, id: NodeId) -> Option<Node> {
        self.cold.remove(id);
        self.kinds.remove(id);
        self.insets.remove(id);
        self.hot.remove(id)
    }

    /// 定位时记下带内容的节点的内容区离四边多远（左、上、右、下）
    #[inline]
    pub(super) fn set_insets(&mut self, id: NodeId, insets: [f32; 4]) {
        self.insets.insert(id, insets);
    }

    /// 内容区离四边多远（左、上、右、下）；还没定位过、不带内容的是 0
    #[inline]
    pub(super) fn insets(&self, id: NodeId) -> [f32; 4] {
        self.insets.get(id).copied().unwrap_or_default()
    }

    #[inline]
    pub(super) fn get(&self, id: NodeId) -> Option<&Node> {
        self.hot.get(id)
    }

    #[inline]
    pub(super) fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.hot.get_mut(id)
    }

    /// 节点的种类对象（容器没有）
    #[inline]
    pub(super) fn element_kind(&self, id: NodeId) -> Option<&dyn ElementKind> {
        self.kinds.get(id).map(|kind| &**kind)
    }

    #[inline]
    pub(super) fn element_kind_mut(&mut self, id: NodeId) -> Option<&mut Box<dyn ElementKind>> {
        self.kinds.get_mut(id)
    }

    /// 节点的种类对象，换成具体的类型
    #[inline]
    pub(super) fn kind<K: ElementKind>(&self, id: NodeId) -> Option<&K> {
        self.kinds.get(id)?.downcast_ref()
    }

    #[inline]
    pub(super) fn kind_mut<K: ElementKind>(&mut self, id: NodeId) -> Option<&mut K> {
        self.kinds.get_mut(id)?.downcast_mut()
    }

    /// 给刚建好的节点挂上种类对象
    pub(super) fn set_kind(&mut self, id: NodeId, kind: Box<dyn ElementKind>) {
        if let Some(node) = self.hot.get_mut(id) {
            node.has_kind = true;
            node.refresh_flags();
            self.kinds.insert(id, kind);
        }
    }

    /// 暂时取下种类对象（画的时候、在树外更新的时候要同时用元素树），用完用 [`Self::put_kind`] 放回去
    #[inline]
    pub(super) fn take_kind(&mut self, id: NodeId) -> Option<Box<dyn ElementKind>> {
        self.kinds.remove(id)
    }

    #[inline]
    pub(super) fn put_kind(&mut self, id: NodeId, kind: Box<dyn ElementKind>) {
        if self.hot.contains_key(id) {
            self.kinds.insert(id, kind);
        }
    }

    #[inline]
    pub(super) fn contains_key(&self, id: NodeId) -> bool {
        self.hot.contains_key(id)
    }

    /// 节点数
    pub(super) fn len(&self) -> usize {
        self.hot.len()
    }

    /// 节点的下标（[`slot_index`]）都小于它（存储里第 0 个位置是占位，`capacity` 不算它）
    pub(super) fn slots(&self) -> usize {
        self.hot.capacity() + 1
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.hot.keys()
    }

    /// 节点的冷数据（名字、处理器、退场值、子元素的位置）
    #[inline]
    pub(super) fn cold(&self, id: NodeId) -> Option<&Cold> {
        self.cold.get(id)
    }

    pub(super) fn cold_mut(&mut self, id: NodeId) -> Option<&mut Cold> {
        self.cold.get_mut(id)
    }

    /// 元素的名字
    pub(super) fn name(&self, id: NodeId) -> Option<&str> {
        self.cold.get(id)?.name.as_deref()
    }

    /// 起了这个名字的元素（有几个时是键最小的那个）
    pub(super) fn named(&self, name: &str) -> Option<NodeId> {
        self.cold.iter().find(|(_, cold)| cold.name.as_deref() == Some(name)).map(|(id, _)| id)
    }

    /// `id` 是否是 `ancestor` 本身或它的子孙
    pub(super) fn is_within(&self, mut id: NodeId, ancestor: NodeId) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            match self.hot.get(id).and_then(|node| node.parent) {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }

    /// 用户的根元素
    pub(super) fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    pub(super) fn add_root(&mut self, id: NodeId) {
        self.roots.push(id);
    }

    /// 树的结构版本
    pub(super) fn structure(&self) -> u64 {
        self.structure
    }

    /// 增删、重排了子元素
    pub(super) fn restructured(&mut self) {
        self.structure += 1;
    }

    /// 有没有过按叠放次序排序的子元素
    #[inline]
    pub(super) fn sorted(&self) -> bool {
        self.sorted
    }

    pub(super) fn set_sorted(&mut self) {
        self.sorted = true;
    }

    /// 子元素画的顺序（后画的在上面，命中测试倒着找）
    pub(super) fn paint_order(&self, id: NodeId) -> &[NodeId] {
        let node = &self.hot[id];
        if !self.sorted {
            return &node.children;
        }
        node.input.as_ref().and_then(|i| i.order.as_deref()).unwrap_or(&node.children)
    }
}

impl Index<NodeId> for NodeStore {
    type Output = Node;

    #[inline]
    fn index(&self, id: NodeId) -> &Node {
        &self.hot[id]
    }
}

impl IndexMut<NodeId> for NodeStore {
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.hot[id]
    }
}
