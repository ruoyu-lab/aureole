//! 元素树里的 3D 变换：节点上的 3D 设置（用到时才分配）、元素自己的变换（绕 x、y 轴的旋转和透视、共享元素的飞行），
//! 按此刻的值沿父节点链算出屏幕变换。对外的链式方法在 [`crate::depth`]。

use super::shared::SharedNode;
use super::{Node, NodeId, NodeStore, Tree};
use crate::property::{PropId, PropertyValue};
use crate::compositor::{DepthValues, LayerDepth, fly};
use crate::geometry::{DEFAULT_PERSPECTIVE, Local, Transform, inverse_distance};
use crate::geometry::Rect;

/// 节点上不常用的变换设置和共享元素的状态，用到时才分配（节点每大一点，一万个元素的重画就慢一点）
pub(crate) struct NodeExtra {
    /// 绕 x 轴、绕 y 轴，弧度
    pub rotate: [f32; 2],
    /// 自己的观察距离，0 表示没设
    pub perspective: f32,
    /// 给子元素的观察距离，0 表示没有
    pub child_perspective: f32,
    /// 变换中心，占宽高的比例
    pub origin: [f32; 2],
    pub backface_hidden: bool,
    /// 声明了 3D 属性的动画：一直是带 3D 设置的合成层，渲染线程才能推进它们
    pub animated: bool,
    /// 共享元素（[`crate::Element::shared`]）
    pub shared: Option<SharedNode>,
    /// 到屏幕的变换有透视时的第三行（见 `Node::world`）；转过的元素的子孙也因此带着这一项
    pub world_row: Option<[f32; 3]>,
}

impl Default for NodeExtra {
    fn default() -> Self {
        Self {
            rotate: [0.0; 2],
            perspective: 0.0,
            child_perspective: 0.0,
            origin: [0.5, 0.5],
            backface_hidden: false,
            animated: false,
            shared: None,
            world_row: None,
        }
    }
}

impl NodeExtra {
    #[inline]
    pub fn has_rotation(&self) -> bool {
        self.rotate != [0.0; 2]
    }

    /// 同一个共享标签有别的元素在显示：不画、不接收指针
    #[inline]
    pub fn hidden(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.hidden)
    }

    /// 正在飞（或者淡入淡出）：此刻在屏幕上额外的平移、缩放、透明度
    #[inline]
    pub fn flight(&self) -> Option<[f32; 5]> {
        self.shared.as_ref()?.flight.as_ref().map(|f| f.value())
    }

    /// 飞着：画在最上面、不受祖先的裁剪
    #[inline]
    pub fn lifted(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.lift && s.flight.is_some())
    }

    /// 要成为合成层，并在层表里带上 3D 设置
    #[inline]
    pub fn needs_layer(&self) -> bool {
        self.has_rotation() || self.backface_hidden || self.animated || self.shared.as_ref().is_some_and(|s| s.flight.is_some())
    }

    /// 变换中心（布局坐标）
    #[inline]
    pub fn origin_point(&self, b: Rect) -> [f32; 2] {
        [b.x + b.width * self.origin[0], b.y + b.height * self.origin[1]]
    }

    /// 视点和观察距离的倒数：自己的透视，没有时用父元素给子元素的（`given`），都没有时用默认的距离
    fn eye(&self, origin: [f32; 2], given: Option<Eye>) -> Eye {
        match inverse_distance(self.perspective) {
            Some(k) => (origin, k),
            None => given.unwrap_or((origin, 1.0 / DEFAULT_PERSPECTIVE)),
        }
    }
}

/// 视点（布局坐标）和观察距离的倒数
pub(super) type Eye = ([f32; 2], f32);

/// 父元素给子元素的透视（[`crate::Element::child_perspective`]）：视点在它的中心。
/// 只有自己有 3D 设置时才去找父元素，平的元素不多查一次
#[inline]
pub(super) fn parent_eye(nodes: &NodeStore, node: &Node) -> Option<Eye> {
    node.extra.as_ref()?;
    let parent = nodes.get(node.parent?)?;
    let k = inverse_distance(parent.extra.as_deref()?.child_perspective)?;
    Some((parent.bounds.center(), k))
}

impl Node {
    /// 到屏幕的完整变换（命中测试用）
    #[inline]
    pub(super) fn world(&self) -> Transform {
        Transform::with_row(self.world, self.extra.as_deref().and_then(|e| e.world_row))
    }

    /// 记下到屏幕的变换。平的只存前两行（和以前一样）；有透视时第三行放进按需分配的 `extra`
    #[inline(always)]
    pub(super) fn set_world(&mut self, world: Transform) {
        self.world = world.affine;
        if self.extra.is_some() || !world.is_affine() {
            self.set_world_row(world);
        }
    }

    #[cold]
    fn set_world_row(&mut self, world: Transform) {
        let row = (!world.is_affine()).then_some(world.perspective);
        if row.is_some() || self.extra.is_some() {
            self.extra.get_or_insert_with(Default::default).world_row = row;
            self.refresh_flags();
        }
    }

    /// 写入一个可动画属性，返回值是否变了：3D 的放在 `extra` 里（用到时才分配），其他的在 `paint` 里
    #[inline]
    pub(super) fn set_value(&mut self, property: PropId, value: PropertyValue) -> bool {
        let (PropId::ROTATION_X | PropId::ROTATION_Y | PropId::PERSPECTIVE, PropertyValue::Float(v)) = (property, &value) else {
            let changed = self.paint.set(property, value);
            if changed {
                self.paint_changed(property);
            }
            return changed;
        };
        if self.extra.is_none() {
            self.extra = Some(Box::default());
            self.refresh_flags();
        }
        let extra = self.extra.get_or_insert_with(Default::default);
        let field = match property {
            PropId::ROTATION_X => &mut extra.rotate[0],
            PropId::ROTATION_Y => &mut extra.rotate[1],
            _ => &mut extra.perspective,
        };
        let changed = *field != *v;
        *field = *v;
        changed
    }

    /// 3D 属性此刻的值
    pub(super) fn depth_value(&self, property: PropId) -> f32 {
        let Some(extra) = self.extra.as_deref() else { return 0.0 };
        match property {
            PropId::ROTATION_X => extra.rotate[0],
            PropId::ROTATION_Y => extra.rotate[1],
            _ => extra.perspective,
        }
    }

    /// 套上自己的变换：`outer` 是父元素到屏幕的变换，`bounds` 是自己的布局位置，`eye` 是父元素给的透视（[`parent_eye`]）。
    /// `flight`：连同共享元素此刻的飞行
    #[inline(always)]
    pub(super) fn place(&self, outer: &Transform, bounds: Rect, eye: Option<Eye>, flight: bool) -> Transform {
        let p = &self.paint;
        let shift = self.offset();
        let translate = [p.translate[0] + shift[0], p.translate[1] + shift[1]];
        match self.extra.as_deref() {
            None => outer.then_local(bounds.center(), translate, p.scale, p.rotation),
            Some(extra) => self.place_extra(extra, outer, bounds, translate, eye, flight),
        }
    }

    /// [`Self::place`] 有 3D 设置、变换中心或者在飞的元素
    #[cold]
    fn place_extra(&self, extra: &NodeExtra, outer: &Transform, bounds: Rect, translate: [f32; 2], eye: Option<Eye>, flight: bool) -> Transform {
        let p = &self.paint;
        let origin = extra.origin_point(bounds);
        let world = if extra.has_rotation() {
            let (eye, k) = extra.eye(origin, eye);
            let local = Local { origin, translate, scale: p.scale, rotation: p.rotation, rotate: extra.rotate, eye, k };
            outer.compose(&local.transform())
        } else {
            outer.then_local(origin, translate, p.scale, p.rotation)
        };
        match extra.flight().filter(|_| flight) {
            Some(f) => fly(world, bounds.center(), f),
            None => world,
        }
    }

    /// 背朝屏幕、设了背面不可见：不画、不接收指针
    #[inline]
    pub(super) fn backface_turned(&self) -> bool {
        self.extra.as_deref().is_some_and(|e| e.backface_hidden && self.world().faces_back(self.bounds.center()))
    }

    /// 命中测试里有 3D 设置的元素：藏起来的、背朝屏幕的点不到；按投影的逆变换看点在不在自己的范围里
    #[cold]
    pub(super) fn hit_extra(&self, extra: &NodeExtra, point: [f32; 2], holder: bool) -> bool {
        !extra.hidden() && !self.backface_turned() && (holder || self.world_invert(point).is_some_and(|local| self.bounds.contains(local)))
    }

    /// 屏幕上的点对应自己布局坐标里的哪一点（命中测试）；平的时候和以前一样只求仿射变换的逆
    #[inline(always)]
    pub(super) fn world_invert(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        match self.extra.as_deref().and_then(|e| e.world_row) {
            None => self.world.invert(p),
            Some(row) => Transform::with_row(self.world, Some(row)).invert(p),
        }
    }

    /// 层表里的 3D 设置（不需要时为 `None`）
    pub(super) fn layer_depth(&self, bounds: Rect, eye: Option<Eye>, lifted: bool) -> Option<Box<LayerDepth>> {
        let extra = self.extra.as_deref().filter(|e| e.needs_layer())?;
        Some(Box::new(LayerDepth {
            values: DepthValues {
                rotate: extra.rotate,
                perspective: extra.perspective,
                flight: extra.flight().unwrap_or(crate::compositor::FLIGHT_IDENTITY),
            },
            eye,
            middle: bounds.center(),
            backface_hidden: extra.backface_hidden,
            lifted,
        }))
    }
}

impl Tree {
    /// 改节点的 3D 设置（用到时才分配），重画
    pub(crate) fn update_extra(&mut self, id: NodeId, f: impl FnOnce(&mut NodeExtra)) {
        let Some(node) = self.nodes.get_mut(id) else { return };
        f(node.extra.get_or_insert_with(Default::default));
        node.refresh_flags();
        self.paint.invalidate();
        // 变换中心、背面、给子元素的透视变了：这棵子树到屏幕的变换在绘制之前重算
        self.paint.moved(id);
        self.accessibility.touch(id);
    }

    /// 元素此刻到屏幕的变换：沿父节点链按此刻的变换、滚动、合成层的动画值、飞行算，O(深度)
    pub(super) fn screen_transform(&self, id: NodeId) -> Option<Transform> {
        let mut chain = vec![id];
        let mut current = self.nodes.get(id)?.parent;
        while let Some(parent) = current {
            chain.push(parent);
            current = self.nodes.get(parent)?.parent;
        }
        let mut world = Transform::IDENTITY;
        for &n in chain.iter().rev() {
            let node = &self.nodes[n];
            world = node.place(&world, node.bounds, parent_eye(&self.nodes, node), true);
        }
        Some(world)
    }

    /// 命名元素此刻在屏幕上的四个角（左上、右上、右下、左下），算上 3D 旋转、透视和飞行
    pub(crate) fn window_corners(&self, name: &str) -> Option<[[f32; 2]; 4]> {
        let id = self.named(name)?;
        let b = self.nodes[id].bounds;
        self.screen_transform(id)?.corners(b.x, b.y, b.width, b.height)
    }
}
