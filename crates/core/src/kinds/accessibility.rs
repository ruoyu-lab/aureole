//! 无障碍提供者（[`AccessibilityProvider`]）和它的上下文。

use accesskit::{Action, ActionData, Node, NodeId, Role};

use crate::geometry::Rect;
use crate::kinds::{Handled, TargetContext};

/// 向辅助技术描述内容的能力：输入框、文字、字符网格、画布实现它，自己定义的元素实现它就能被读屏软件读到。
///
/// 元素树负责元素自己的那一部分：编号、边界和变换、名字（[`crate::Element::accessibility_name`]）、描述、
/// 禁用、能获得焦点、能点击、滚动；元素上声明的无障碍属性（角色、状态、数值）优先。提供者补上内容：
///
/// - [`AccessibilityProvider::role`]：元素上没声明角色时的角色；
/// - [`AccessibilityProvider::write_node`]：值、选区、占位文字、能做的编辑动作（`SetValue`、`SetTextSelection`……）；
/// - [`AccessibilityProvider::virtual_children`]：不是元素的子节点——按行的文字片段（`TextRun`）、富文本的链接、
///   画布里登记的区域。每个给一个编号和一个"版本"（内容变了才变），版本没变的不重建，只重发变了的那几个；
/// - [`AccessibilityProvider::action`]：辅助技术发来的动作（选字、改值、替换选中的文字），改文字的按编辑记进撤销。
///
/// 只有辅助技术打开时才调用；元素的内容变了，在 [`TargetContext`] 里 `request_accessibility`（内置的种类改了内容都会），
/// 值来自信号的在 [`crate::kinds::UpdateContext`] 里同样要求。坐标是元素自己的
pub trait AccessibilityProvider {
    /// 角色：元素上没用 [`crate::Element::accessibility_role`] 声明时用它。`None` 时按元素推断
    /// （声明了名字的是图形，否则是装饰）
    fn role(&self) -> Option<Role> {
        None
    }

    /// 填这个元素的节点：值、选区（文字片段的编号用 [`AccessibilityContext::child_id`]）、占位文字、能做的动作
    fn write_node(&self, node: &mut Node, cx: &AccessibilityContext);

    /// 虚拟子节点：按顺序 [`AccessibilityContext::push_child`]，排在元素的子元素后面
    fn virtual_children(&self, cx: &mut AccessibilityContext) {
        let _ = cx;
    }

    /// 辅助技术发来的动作（元素没被禁用时）。返回 [`Handled::No`] 时元素树照通用的办（点击、聚焦、滚动、
    /// 有文字输入客户端的元素整段改值）
    fn action(&mut self, action: &AccessibilityAction, cx: &mut TargetContext) -> Handled {
        let _ = (action, cx);
        Handled::No
    }
}

/// 填节点、列虚拟子节点时能用的（[`AccessibilityProvider`]）
pub struct AccessibilityContext<'a> {
    /// 虚拟子节点编号的前缀（元素第一次有虚拟子节点时分配）
    pub(crate) base: u64,
    pub(crate) size: [f32; 2],
    pub(crate) content: Rect,
    pub(crate) disabled: bool,
    pub(crate) focused: bool,
    /// 上次发出去的虚拟子节点：(编号, 版本)
    pub(crate) sent: &'a [(u32, u64)],
    /// 这次的虚拟子节点：(编号, 版本)
    pub(crate) children: Vec<(u32, u64)>,
    /// 要发出去的节点（变了的）
    pub(crate) out: &'a mut Vec<(NodeId, Node)>,
    /// 富文本里的行内元素排在第几个虚拟子节点前面（读屏时和文字按顺序）：(元素, 虚拟子节点的个数)
    pub(crate) inline_order: Vec<(crate::tree::NodeId, usize)>,
}

impl AccessibilityContext<'_> {
    /// 元素的大小（含内边距），逻辑像素
    pub fn size(&self) -> [f32; 2] {
        self.size
    }

    /// 内容区（去掉内边距和边框）：在元素自己的坐标里
    pub fn content_rect(&self) -> Rect {
        self.content
    }

    /// 元素（或祖先）被禁用了：不要加编辑的动作
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// 元素有键盘焦点
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// 编号是 `key` 的虚拟子节点在无障碍树里的编号（选区的位置要用）。`key` 要小于 2^31
    pub fn child_id(&self, key: u32) -> NodeId {
        child_id(self.base, key)
    }

    /// 加一个虚拟子节点：`key` 是它在这个元素里的编号（下次还是同一个东西就用同一个编号，小于 2^31），
    /// `stamp` 是它的版本（内容、位置变了就变）。和上次发出去的版本一样时不调用 `build`，节点不重发
    pub fn push_child(&mut self, key: u32, stamp: u64, build: impl FnOnce() -> Node) {
        let index = self.children.len();
        // 一般还在原来的位置上；不在时再找
        let same = match self.sent.get(index) {
            Some(&(k, s)) if k == key => s == stamp,
            _ => self.sent.iter().any(|&(k, s)| k == key && s == stamp),
        };
        if !same {
            self.out.push((self.child_id(key), build()));
        }
        self.children.push((key, stamp));
    }
}

/// 虚拟子节点在无障碍树里的编号：最高位是 1，然后是元素的前缀，低 32 位是编号
pub(crate) fn child_id(base: u64, key: u32) -> NodeId {
    NodeId(crate::tree::VIRTUAL | (base << 32) | key as u64)
}

/// 辅助技术发来的一个动作（[`AccessibilityProvider::action`]）
pub struct AccessibilityAction<'a> {
    /// 什么动作
    pub action: Action,
    /// 落在哪个虚拟子节点上（它的编号）；落在元素自己身上时是 `None`
    pub child: Option<u32>,
    /// 动作带的数据（改值的文字、选区）
    pub data: Option<&'a ActionData>,
    pub(crate) base: Option<u64>,
}

impl AccessibilityAction<'_> {
    /// 辅助技术给的节点编号（选区里的位置）是这个元素的哪个虚拟子节点；不是时 `None`
    pub fn child_key(&self, id: NodeId) -> Option<u32> {
        let base = self.base?;
        (id.0 & crate::tree::VIRTUAL != 0 && (id.0 & !crate::tree::VIRTUAL) >> 32 == base).then_some(id.0 as u32)
    }
}
