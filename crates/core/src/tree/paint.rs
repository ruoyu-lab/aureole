//! 绘制：节点的绘制属性和绘制标记，显示列表的生成状态（显示列表、合成层表、用过的着色器、要不要重新生成）。
//! 生成显示列表的遍历在 `tree/walk.rs`，定位和刷新屏幕变换在 `tree/place.rs`。
//!
//! 绘制属性 [`Paint`] 是节点上的数据记录，字段对整个 `tree` 模块可见；[`PaintState`] 的字段私有，
//! 别的模块通过方法标记重画、读显示列表和合成层表。

use std::sync::Arc;

use scene::DisplayList;

use super::patch::{ScenePatch, Span, Spans};
use super::walk::WalkBuffers;
use super::{DefaultColors, Node, NodeId, NodeStore};
use crate::property::{PropId, PropertyValue};
use crate::visual_effects::PaintEffects;
use crate::compositor::LayerInfo;
use crate::element::Shader;
use crate::geometry::Affine;
use crate::gradient::{Fill, Gradient};
use crate::shadow::Shadows;
use crate::shadow::Shadow;

/// 显示列表的生成状态
pub(super) struct PaintState {
    /// 显示列表：图元、裁剪、分组、特效
    list: Arc<DisplayList>,
    /// 显示列表里的合成层，和 `list` 一起生成
    layers: Arc<Vec<LayerInfo>>,
    /// 用到过的自定义着色器，元素用下标引用
    shaders: Vec<Shader>,
    /// 要重新生成显示列表
    dirty: bool,
    /// 合成层的动画值在主线程推进了、滚动了（没有重建图元），命中测试前要刷新屏幕变换
    worlds_dirty: bool,
    /// 静态地改了变换属性的元素：绘制之前重算它们子树的屏幕变换（见 `tree/place.rs`）
    moved: Vec<NodeId>,
    /// 定位遍历、刷新屏幕变换用的栈，留着下次用（布局一次、命中测试一次不为它们分配）
    places: Vec<(NodeId, [f32; 2], Affine)>,
    worlds: Vec<(NodeId, Affine)>,
    /// 绘制遍历用的栈和表，留着下次用（重画一次不为它们分配）
    buffers: WalkBuffers,
    /// 上一份显示列表和合成层表：渲染线程放手以后，下次整个重新生成时拿它们的容量来写（双缓冲，不再每次分配）
    spare: Option<(Arc<DisplayList>, Arc<Vec<LayerInfo>>)>,
    /// "节点 → 图元"的表和等着原地修补的节点（见 `tree/patch.rs`）
    spans: Spans,
    /// 自上次交给渲染线程以来整个重新生成过
    rebuilt: bool,
    /// 自上次交给渲染线程以来原地修补过的图元（整个重新生成过时不用看）
    touched: Vec<u32>,
    /// 元素没有指定时用的颜色
    colors: DefaultColors,
}

impl PaintState {
    pub(super) fn new() -> Self {
        Self {
            list: Arc::default(),
            layers: Arc::default(),
            shaders: Vec::new(),
            dirty: true,
            worlds_dirty: false,
            moved: Vec::new(),
            places: Vec::new(),
            worlds: Vec::new(),
            buffers: WalkBuffers::default(),
            spare: None,
            spans: Spans::default(),
            rebuilt: false,
            touched: Vec::new(),
            colors: DefaultColors::new(reactive::untracked(crate::defaults::palette)),
        }
    }

    /// 要重新生成显示列表
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 这一帧的显示列表生成完了
    pub(super) fn painted(&mut self) {
        self.dirty = false;
    }

    /// 合成层的值在主线程变了（没有重建图元）：命中测试前要刷新屏幕变换
    pub(super) fn mark_worlds(&mut self) {
        self.worlds_dirty = true;
    }

    pub(super) fn worlds_dirty(&self) -> bool {
        self.worlds_dirty
    }

    /// 静态地改了这个元素的变换属性：绘制之前重算它子树的屏幕变换
    pub(super) fn moved(&mut self, id: NodeId) {
        self.moved.push(id);
    }

    /// 取走静态地改了变换的元素
    pub(super) fn take_moved(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.moved)
    }

    /// 借出定位遍历用的栈（空的，容量留着）
    pub(super) fn take_places(&mut self) -> Vec<(NodeId, [f32; 2], Affine)> {
        std::mem::take(&mut self.places)
    }

    /// 还回定位遍历用的栈
    pub(super) fn put_places(&mut self, mut places: Vec<(NodeId, [f32; 2], Affine)>) {
        places.clear();
        self.places = places;
    }

    /// 借出刷新屏幕变换用的栈（空的，容量留着）
    pub(super) fn take_worlds(&mut self) -> Vec<(NodeId, Affine)> {
        std::mem::take(&mut self.worlds)
    }

    /// 还回刷新屏幕变换用的栈
    pub(super) fn put_worlds(&mut self, mut worlds: Vec<(NodeId, Affine)>) {
        worlds.clear();
        self.worlds = worlds;
    }

    /// 全部节点的屏幕变换都按此刻的值算过了（定位、命中测试前的刷新）
    pub(super) fn placed(&mut self) {
        self.worlds_dirty = false;
        self.moved.clear();
    }

    /// 借出绘制遍历用的栈和表
    pub(super) fn take_buffers(&mut self) -> WalkBuffers {
        std::mem::take(&mut self.buffers)
    }

    /// 还回绘制遍历用的栈和表
    pub(super) fn put_buffers(&mut self, buffers: WalkBuffers) {
        self.buffers = buffers;
    }

    /// 整个重新生成之前：拿回上一份显示列表和合成层表的容量（渲染线程和别处都放手了才拿得到，拿不到就新分配），
    /// "节点 → 图元"的表换一个编号。返回清空的列表、层表、表和编号
    pub(super) fn begin_list(&mut self, slots: usize) -> (DisplayList, Vec<LayerInfo>, Vec<Span>, u32) {
        let (mut list, mut layers) = match self.spare.take() {
            Some((list, layers)) => (Arc::try_unwrap(list).unwrap_or_default(), Arc::try_unwrap(layers).unwrap_or_default()),
            None => Default::default(),
        };
        list.clear();
        list.quads.reserve(self.list.quads.len());
        layers.clear();
        layers.reserve(self.layers.len());
        let (table, epoch) = self.spans.begin(slots);
        (list, layers, table, epoch)
    }

    /// 换上新生成的显示列表和合成层表，旧的留作下次的缓冲。`folded` 是生成之后又被改过的图元，
    /// `patchable` 为假时这一份不能原地修补
    pub(super) fn set_list(&mut self, list: DisplayList, layers: Vec<LayerInfo>, table: Vec<Span>, folded: &[u32], patchable: bool) {
        let list = std::mem::replace(&mut self.list, Arc::new(list));
        let layers = std::mem::replace(&mut self.layers, Arc::new(layers));
        self.spare = Some((list, layers));
        self.spans.finish(table, folded, patchable);
        self.rebuilt = true;
        self.touched.clear();
    }

    /// 这个节点的颜色、圆角、边框、一层外投影变了：等着原地修补（已经要整个重新生成时不用记）
    pub(super) fn patch(&mut self, id: NodeId) {
        if !self.dirty {
            self.spans.request(id);
        }
    }

    /// 半透明元素的透明度变了（还是半透明）：等着只改它的层的透明度（已经要整个重新生成时不用记）
    pub(super) fn patch_opacity(&mut self, id: NodeId) {
        if !self.dirty {
            self.spans.request_opacity(id);
        }
    }

    /// 有等着原地修补的节点
    pub(super) fn has_patches(&self) -> bool {
        self.spans.has_pending()
    }

    /// 原地修补等着的节点，返回改了几个图元；只改层的透明度的节点接在 `layers` 后面。
    /// 改不了时返回 `None`（调用方整个重新生成）
    pub(super) fn apply_patches(&mut self, nodes: &NodeStore, layers: &mut Vec<NodeId>) -> Option<usize> {
        self.spans.apply(nodes, &mut self.list, &mut self.touched, layers)
    }

    /// 取走自上次以来显示列表的变化：没变是 `None`，整个重新生成过是 `Some(None)`，只修补过是修补的几段
    pub(super) fn take_change(&mut self) -> Option<Option<ScenePatch>> {
        if std::mem::take(&mut self.rebuilt) {
            self.touched.clear();
            return Some(None);
        }
        if self.touched.is_empty() {
            return None;
        }
        Some(Some(ScenePatch::from_indices(&mut self.touched)))
    }

    /// 最近一次生成的显示列表
    pub(super) fn list(&self) -> &Arc<DisplayList> {
        &self.list
    }

    /// 最近一次生成的合成层表
    pub(super) fn layers(&self) -> &Arc<Vec<LayerInfo>> {
        &self.layers
    }

    /// 元素没有指定时用的颜色
    pub(super) fn colors(&self) -> &DefaultColors {
        &self.colors
    }

    /// 换成这一套调色板的默认颜色，返回变了没有
    pub(super) fn set_colors(&mut self, palette: &crate::defaults::Palette) -> bool {
        let colors = DefaultColors::new(palette);
        if colors == self.colors {
            return false;
        }
        self.colors = colors;
        self.dirty = true;
        true
    }

    /// 登记一个自定义着色器，返回给 `Paint::shader` 用的编号（同一份源码只登记一次）
    pub(super) fn register_shader(&mut self, shader: &Shader) -> u32 {
        let index = match self.shaders.iter().position(|s| Arc::ptr_eq(&s.source, &shader.source) || s.source == shader.source) {
            Some(index) => index,
            None => {
                self.shaders.push(shader.clone());
                self.shaders.len() - 1
            }
        };
        index as u32 + 1
    }

    /// 编号对应的着色器（0 是没有）
    pub(super) fn shader(&self, index: u16) -> Option<Shader> {
        self.shaders.get(usize::from(index).checked_sub(1)?).cloned()
    }

    /// 编号对应的着色器随时间变化（画面里有它时每帧重画）
    pub(super) fn shader_animated(&self, index: u16) -> bool {
        usize::from(index).checked_sub(1).and_then(|i| self.shaders.get(i)).is_some_and(|s| s.animated)
    }

    /// 全部着色器的源码（显示列表里按下标引用）
    pub(super) fn shader_sources(&self) -> Vec<Arc<str>> {
        self.shaders.iter().map(|shader| shader.source.clone()).collect()
    }
}

/// 节点上不常见的绘制特征，写属性时维护：绘制遍历先看 [`PaintFlags::is_plain`]，是就走快路径
/// （只算一次仿射变换、写一个形状图元），不用一样样去读节点、绘制属性、种类对象和 3D 设置。
/// 调试构建里遍历时核对它和按属性现算的一样（[`Node::compute_flags`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PaintFlags(u16);

impl PaintFlags {
    /// 带内容（种类对象）
    pub(super) const CONTENT: Self = Self(1);
    /// 合成层
    pub(super) const LAYER: Self = Self(1 << 1);
    /// 滚动视口，或者视口的内容（有滚动偏移）
    pub(super) const SCROLL: Self = Self(1 << 2);
    /// 3D 设置、变换中心、共享元素（`extra`）
    pub(super) const EXTRA: Self = Self(1 << 3);
    /// 透明度不是 1
    pub(super) const FADED: Self = Self(1 << 4);
    /// 模糊
    pub(super) const BLUR: Self = Self(1 << 5);
    /// 滤镜、混合模式、遮罩、缓存成纹理
    pub(super) const EFFECTS: Self = Self(1 << 6);
    /// 背景模糊
    pub(super) const BACKDROP: Self = Self(1 << 7);
    /// 自定义着色器
    pub(super) const SHADER: Self = Self(1 << 8);
    /// 裁剪子元素
    pub(super) const CLIP: Self = Self(1 << 9);
    /// 几层投影、内阴影、带扩展的投影、渐变边框
    pub(super) const LAYERED: Self = Self(1 << 10);
    /// 渐变背景
    pub(super) const GRADIENT: Self = Self(1 << 11);
    /// 以上任何一个都要走完整的路径
    const RARE: u16 = (1 << 12) - 1;
    /// 自己有平移、缩放、旋转（不算不常见：快路径多算一次仿射变换）
    pub(super) const TRANSFORM: Self = Self(1 << 12);

    /// 没有不常见的东西：走快路径
    #[inline(always)]
    pub(super) fn is_plain(self) -> bool {
        self.0 & Self::RARE == 0
    }

    /// 几组标记的并
    pub(super) const fn union(all: &[Self]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < all.len() {
            bits |= all[i].0;
            i += 1;
        }
        Self(bits)
    }

    /// 和 `other` 有共同的位
    #[inline(always)]
    pub(super) fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[inline(always)]
    pub(super) fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline(always)]
    fn set(&mut self, bit: Self, on: bool) {
        if on {
            self.0 |= bit.0;
        } else {
            self.0 &= !bit.0;
        }
    }
}

impl Node {
    /// 按节点和绘制属性现算的绘制标记
    pub(super) fn compute_flags(&self) -> PaintFlags {
        let p = &self.paint;
        let mut flags = PaintFlags::default();
        flags.set(PaintFlags::CONTENT, self.has_kind);
        flags.set(PaintFlags::LAYER, self.layer);
        flags.set(PaintFlags::SCROLL, self.scroll.is_some() || self.scrolled);
        flags.set(PaintFlags::EXTRA, self.extra.is_some());
        flags.set(PaintFlags::FADED, p.opacity != 1.0);
        flags.set(PaintFlags::BLUR, p.blur > 0.0);
        flags.set(PaintFlags::EFFECTS, p.effects.is_some());
        flags.set(PaintFlags::BACKDROP, p.backdrop_blur > 0.0);
        flags.set(PaintFlags::SHADER, p.shader > 0);
        flags.set(PaintFlags::CLIP, p.clip);
        flags.set(PaintFlags::LAYERED, matches!(p.shadow, ShadowSlot::Layered(_)));
        flags.set(PaintFlags::GRADIENT, matches!(p.background, Fill::Gradient(_)));
        flags.set(PaintFlags::TRANSFORM, p.has_transform());
        flags
    }

    /// 节点本身变了（成了合成层、有了内容、滚动、3D 设置……）或者一次改了好几样绘制属性：整个重算
    #[inline]
    pub(super) fn refresh_flags(&mut self) {
        self.flags = self.compute_flags();
    }

    /// 改了一个可动画属性之后：只重算这个属性影响的那一位（动画每帧都在写，不整个重算）
    #[inline(always)]
    pub(super) fn paint_changed(&mut self, property: PropId) {
        let p = &self.paint;
        let (bit, on) = match property {
            PropId::BACKGROUND => (PaintFlags::GRADIENT, matches!(p.background, Fill::Gradient(_))),
            PropId::OPACITY => (PaintFlags::FADED, p.opacity != 1.0),
            PropId::SCALE | PropId::ROTATION | PropId::TRANSLATION_X | PropId::TRANSLATION_Y => (PaintFlags::TRANSFORM, p.has_transform()),
            PropId::BLUR => (PaintFlags::BLUR, p.blur > 0.0),
            PropId::BACKDROP_BLUR => (PaintFlags::BACKDROP, p.backdrop_blur > 0.0),
            PropId::SHADOW | PropId::BORDER_COLOR => (PaintFlags::LAYERED, matches!(p.shadow, ShadowSlot::Layered(_))),
            PropId::FILTER => (PaintFlags::EFFECTS, p.effects.is_some()),
            _ => return,
        };
        self.flags.set(bit, on);
    }
}

/// 绘制属性
#[derive(Clone, PartialEq)]
pub(super) struct Paint {
    /// 纯色或渐变
    pub(super) background: Fill,
    pub(super) radius: f32,
    pub(super) border_width: f32,
    /// 边框颜色；渐变边框时是渐变的代表色（渐变在 `shadow` 的 [`ShadowSlot::Layered`] 里）
    pub(super) border_color: scene::Color,
    /// 投影：最常见的一层不带扩展的外投影直接存着（画在形状图元里）；几层、内阴影、带扩展的投影、
    /// 渐变边框很少用，单独分配。这样 `Paint` 和以前一样大（两个缓存行），绘制遍历只多一次判断
    pub(super) shadow: ShadowSlot,
    pub(super) opacity: f32,
    pub(super) scale: f32,
    pub(super) rotation: f32,
    pub(super) translate: [f32; 2],
    /// 文字颜色（也是路径的颜色），可以是渐变
    pub(super) color: Fill,
    /// 把子元素裁剪在自己的圆角矩形里
    pub(super) clip: bool,
    pub(super) blur: f32,
    pub(super) backdrop_blur: f32,
    pub(super) backdrop_saturation: f32,
    /// 自定义着色器：`Tree::shaders` 的下标 + 1，0 表示没有（`u16`：`Paint` 正好两个缓存行）
    pub(super) shader: u16,
    pub(super) shader_params: [f32; 4],
    /// 路径描边画哪一段
    pub(super) trim: [f32; 2],
    /// 滤镜、混合模式、遮罩、缓存成纹理（用到时才分配）
    pub(super) effects: Option<Box<PaintEffects>>,
}

/// 画在形状图元里的那一层投影（和以前一样 16 字节）
#[derive(Clone, Copy, PartialEq)]
pub(super) struct PlainShadow {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) blur: f32,
    pub(super) color: scene::Color,
}

impl PlainShadow {
    const NONE: Self = Self { x: 0.0, y: 0.0, blur: 0.0, color: scene::Color::TRANSPARENT };

    pub(super) fn new(s: &Shadow) -> Self {
        Self { x: s.x, y: s.y, blur: s.blur, color: s.color }
    }
}

#[derive(Clone, PartialEq)]
pub(super) enum ShadowSlot {
    /// 一层不带扩展的外投影（或者没有）、纯色边框
    Plain(PlainShadow),
    /// 几层投影、内阴影、带扩展的投影，或者渐变边框
    Layered(Box<Layered>),
}

/// 不能放进形状图元的投影和边框
#[derive(Clone, PartialEq)]
pub(super) struct Layered {
    pub(super) shadows: Shadows,
    pub(super) border: Option<Gradient>,
}

impl Default for Paint {
    fn default() -> Self {
        Self {
            background: Fill::Color(scene::Color::TRANSPARENT),
            radius: 0.0,
            border_width: 0.0,
            border_color: scene::Color::TRANSPARENT,
            shadow: ShadowSlot::Plain(PlainShadow::NONE),
            opacity: 1.0,
            scale: 1.0,
            rotation: 0.0,
            translate: [0.0, 0.0],
            // 继承文字样式时换成默认的文字颜色
            color: Fill::Color(scene::Color::TRANSPARENT),
            clip: false,
            blur: 0.0,
            backdrop_blur: 0.0,
            backdrop_saturation: 1.0,
            shader: 0,
            shader_params: [0.0; 4],
            trim: [0.0, 1.0],
            effects: None,
        }
    }
}

impl Paint {
    /// 有平移、缩放或旋转
    #[inline(always)]
    pub(super) fn has_transform(&self) -> bool {
        self.translate != [0.0, 0.0] || self.scale != 1.0 || self.rotation != 0.0
    }

    /// 完全透明的容器不产生图元
    #[inline]
    pub(super) fn visible(&self) -> bool {
        self.background.is_visible()
            || (self.border_width > 0.0 && !self.border_color.is_transparent())
            || match &self.shadow {
                ShadowSlot::Plain(s) => !s.color.is_transparent(),
                ShadowSlot::Layered(_) => true,
            }
    }

    /// 几层投影、内阴影、渐变边框（很少用）
    #[inline]
    pub(super) fn layered(&self) -> Option<&Layered> {
        match &self.shadow {
            ShadowSlot::Plain(_) => None,
            ShadowSlot::Layered(l) => Some(l),
        }
    }

    /// 全部的投影
    pub(super) fn shadows(&self) -> Shadows {
        match &self.shadow {
            ShadowSlot::Plain(s) => Shadows::from(Shadow::new(s.x, s.y, s.blur, s.color)),
            ShadowSlot::Layered(l) => l.shadows.clone(),
        }
    }

    /// 边框的颜色：纯色或渐变
    pub(super) fn border_fill(&self) -> Fill {
        match self.layered().and_then(|l| l.border.as_ref()) {
            Some(gradient) => Fill::Gradient(gradient.clone()),
            None => Fill::Color(self.border_color),
        }
    }

    /// 换成单独分配的样子（第一次用到几层投影、渐变边框时）
    pub(super) fn layered_mut(&mut self) -> &mut Layered {
        if let ShadowSlot::Plain(s) = self.shadow {
            let shadows = Shadows::from(Shadow::new(s.x, s.y, s.blur, s.color));
            self.shadow = ShadowSlot::Layered(Box::new(Layered { shadows, border: None }));
        }
        match &mut self.shadow {
            ShadowSlot::Layered(l) => l,
            ShadowSlot::Plain(_) => unreachable!(),
        }
    }

    /// 又只剩一层不带扩展的外投影、纯色边框了：回到直接存着的样子
    pub(super) fn tidy(&mut self) {
        if let ShadowSlot::Layered(l) = &self.shadow
            && l.border.is_none()
            && let Some(plain) = l.shadows.plain()
        {
            self.shadow = ShadowSlot::Plain(PlainShadow::new(plain));
        }
    }

    /// 一层投影：最常见的一层外投影换成另一层（投影动画的每一帧）直接改，不分配
    #[inline(never)]
    pub(super) fn set_shadow(&mut self, shadow: Shadow) -> bool {
        if let ShadowSlot::Plain(current) = &mut self.shadow
            && !shadow.inner
            && shadow.spread == 0.0
        {
            let plain = PlainShadow::new(&shadow);
            let changed = *current != plain;
            *current = plain;
            return changed;
        }
        self.set_shadows(Shadows::from(shadow))
    }

    /// 不内联：很少走到，内联进 [`Paint::set`] 会拖慢最常见的写入（颜色、变换）
    #[inline(never)]
    pub(super) fn set_shadows(&mut self, shadows: Shadows) -> bool {
        // 最常见：一层外投影换成另一层（动画的每一帧），不用比较整组
        if let (ShadowSlot::Plain(current), Some(plain)) = (&mut self.shadow, shadows.plain()) {
            let plain = PlainShadow::new(plain);
            let changed = *current != plain;
            *current = plain;
            return changed;
        }
        if self.shadows() == shadows {
            return false;
        }
        self.layered_mut().shadows = shadows;
        self.tidy();
        true
    }

    #[inline(never)]
    pub(super) fn set_border(&mut self, fill: Fill) -> bool {
        let current = self.layered().and_then(|l| l.border.clone());
        match fill {
            Fill::Color(c) => {
                if current.is_none() && self.border_color == c {
                    return false;
                }
                self.border_color = c;
                if current.is_some() {
                    self.layered_mut().border = None;
                    self.tidy();
                }
            }
            Fill::Gradient(g) => {
                if current.as_ref() == Some(&g) {
                    return false;
                }
                self.border_color = Fill::Gradient(g.clone()).color();
                self.layered_mut().border = Some(g);
            }
        }
        true
    }

    /// 写入一个可动画属性，返回值是否变了（只比较这一个字段，不用复制整个 `Paint`）
    pub(super) fn set(&mut self, property: PropId, value: PropertyValue) -> bool {
        fn assign<T: PartialEq>(field: &mut T, value: T) -> bool {
            let changed = *field != value;
            if changed {
                *field = value;
            }
            changed
        }
        match (property, value) {
            (PropId::BACKGROUND, PropertyValue::Color(c)) => assign(&mut self.background, Fill::Color(c)),
            (PropId::BACKGROUND, PropertyValue::Gradient(g)) => assign(&mut self.background, Fill::Gradient(g)),
            (PropId::BORDER_COLOR, PropertyValue::Color(c)) => self.set_border(Fill::Color(c)),
            (PropId::BORDER_COLOR, PropertyValue::Gradient(g)) => self.set_border(Fill::Gradient(g)),
            (PropId::BORDER_WIDTH, PropertyValue::Float(v)) => assign(&mut self.border_width, v),
            (PropId::RADIUS, PropertyValue::Float(v)) => assign(&mut self.radius, v),
            (PropId::SHADOW, PropertyValue::Shadow(s)) => self.set_shadow(s),
            (PropId::SHADOW, PropertyValue::Shadows(s)) => self.set_shadows(s),
            (PropId::OPACITY, PropertyValue::Float(v)) => assign(&mut self.opacity, v),
            (PropId::SCALE, PropertyValue::Float(v)) => assign(&mut self.scale, v),
            (PropId::ROTATION, PropertyValue::Float(v)) => assign(&mut self.rotation, v),
            (PropId::TRANSLATION_X, PropertyValue::Float(v)) => assign(&mut self.translate[0], v),
            (PropId::TRANSLATION_Y, PropertyValue::Float(v)) => assign(&mut self.translate[1], v),
            (PropId::COLOR, PropertyValue::Color(c)) => assign(&mut self.color, Fill::Color(c)),
            (PropId::COLOR, PropertyValue::Gradient(g)) => assign(&mut self.color, Fill::Gradient(g)),
            (PropId::BLUR, PropertyValue::Float(v)) => assign(&mut self.blur, v.max(0.0)),
            (PropId::BACKDROP_BLUR, PropertyValue::Float(v)) => assign(&mut self.backdrop_blur, v.max(0.0)),
            (PropId::BACKDROP_SATURATION, PropertyValue::Float(v)) => assign(&mut self.backdrop_saturation, v.max(0.0)),
            (PropId::STROKE_START, PropertyValue::Float(v)) => assign(&mut self.trim[0], v),
            (PropId::STROKE_END, PropertyValue::Float(v)) => assign(&mut self.trim[1], v),
            (PropId::FILTER, PropertyValue::Filter(f)) => self.set_filter(f),
            (property, value) => unreachable!("{property:?} 不接受 {value:?}"),
        }
    }
}
