//! 绘制遍历：前序走一遍元素树，生成显示列表和合成层表。
//!
//! 每个元素先看节点上的 [`PaintFlags`]：没有不常见的东西（内容、合成层、滚动、3D、透明度、模糊、效果、背景模糊、
//! 着色器、裁剪、几层投影、渐变背景）时走快路径——只算一次仿射变换、写一个形状图元、把子元素压栈，层号和裁剪
//! 直接作为参数传给生成图元的函数；有不常见的东西才走完整的路径（[`Tree::enter`]，不内联）。
//!
//! 位置和到屏幕的变换在定位时已经写在节点上（见 `tree/place.rs`），这里只读、不回写。
//! 遍历的上下文（层内的累积变换、透明度、所在的层、裁剪）存在一张表里（[`Frame`]），同一个父元素的子元素共用一项，
//! 变换、透明度、层、裁剪都不变的子元素直接用父元素那一项；栈里的一步只记节点和上下文的下标（[`Step`]，24 字节）。

use scene::{Clip, DisplayList, Group, Quad};

use super::depth::{self, NodeExtra};
use super::node::slot_index;
use super::paint::{Paint, PaintFlags, ShadowSlot};
use super::patch::{self, Span};
use super::visual_effects::{EffectsExit, EffectsStep};
use super::{NodeId, Tree, layer_key, timeline};
use crate::compositor::{LayerInfo, LayerValues};
use crate::editor::Viewport;
use crate::geometry::{Affine, Rect, Transform};
use crate::gradient::Fill;
use crate::image::ImageBlock;
use crate::kinds::PaintContext;
use crate::property::PropId;
use crate::text::Ink;

/// 绘制遍历的上下文：父元素到当前合成层坐标的累积变换（烘焙进图元的部分）、当前合成层之内的累积透明度、
/// 所在的合成层、祖先的裁剪区域（显示列表里的下标 + 1，0 表示不裁剪）。同一个父元素的子元素共用一项
#[derive(Clone, Copy)]
pub(super) struct Frame {
    pub(super) local: Affine,
    pub(super) opacity: f32,
    pub(super) layer: u32,
    pub(super) clip: u32,
}

impl Frame {
    /// 根元素、浮层：单位变换、不透明、根层、不裁剪
    const ROOT: Self = Self { local: Affine::IDENTITY, opacity: 1.0, layer: 0, clip: 0 };
}

/// 绘制遍历的一步。进入元素只记节点和上下文的下标（上下文在 [`Walk::frames`] 里），离开时要做的事各一种
#[derive(Clone, Copy)]
pub(super) enum Step {
    /// 进入一个元素（节点，上下文的下标）
    Enter(NodeId, u32),
    /// 进入一个正在飞的共享元素：所有元素画完以后才画它（在最上面），不受祖先的裁剪
    Lift(NodeId, u32),
    /// 离开一个可能成为分组的元素（这时才知道它的子树有多少图元）
    Exit { layer: u32, first: u32, blur: f32, clip: u32 },
    /// 有滤镜、混合模式、遮罩的元素：离开它，或者开始画它的遮罩（见 `tree/visual_effects.rs`；平的界面不走这里）
    Effects(EffectsStep),
    /// 离开滚动视口（在子元素上面画滚动条，用自己的上下文 `frame`）或者有键盘焦点的元素
    /// （在它和子孙上面画焦点环，用父元素的裁剪 `outer`）
    After { id: NodeId, frame: u32, outer: u32 },
    /// 根元素和浮层画完了：拖放的影子（源元素在最上面的合成层里再画一遍）
    Ghost(NodeId),
}

/// 绘制遍历的状态：生成中的显示列表和层表、步骤栈、上下文表、有效果的元素离开时要用的、推迟画的飞行元素，
/// 顺手记下的"节点 → 图元"的表（原地修补用，见 `tree/patch.rs`）
pub(super) struct Walk {
    pub(super) list: DisplayList,
    pub(super) layers: Vec<LayerInfo>,
    /// 按节点在存储里的下标：自己的形状是哪个图元（或者没画）
    spans: Vec<Span>,
    /// 这一份显示列表的编号
    epoch: u32,
    /// 生成之后又被改过的图元（祖先的滤镜直接改了它的颜色）：不能按表修补
    pub(super) folded: Vec<u32>,
    pub(super) stack: Vec<Step>,
    pub(super) frames: Vec<Frame>,
    /// 有效果的元素离开时要用的（步骤里只记下标，步骤不因它变大）
    pub(super) exits: Vec<EffectsExit>,
    /// 飞着的共享元素（节点，上下文）：所有元素画完以后（拖放的影子之前）再画
    pub(super) lifted: Vec<(NodeId, u32)>,
}

/// 遍历里的栈和表，留着下次用（重画一次不为它们分配）
#[derive(Default)]
pub(super) struct WalkBuffers {
    stack: Vec<Step>,
    frames: Vec<Frame>,
    exits: Vec<EffectsExit>,
    lifted: Vec<(NodeId, u32)>,
    folded: Vec<u32>,
}

impl Walk {
    /// 新的一次遍历：栈和表借自上一次（清空，容量留着）
    fn new(list: DisplayList, layers: Vec<LayerInfo>, spans: Vec<Span>, epoch: u32, buffers: WalkBuffers) -> Self {
        let WalkBuffers { mut stack, mut frames, mut exits, mut lifted, mut folded } = buffers;
        stack.clear();
        frames.clear();
        exits.clear();
        lifted.clear();
        folded.clear();
        frames.push(Frame::ROOT);
        Self { list, layers, spans, epoch, folded, stack, frames, exits, lifted }
    }

    /// 记下节点自己的形状画成了哪个图元（[`Span::HIDDEN`]、[`Span::INVISIBLE`]：没画）
    #[inline(always)]
    fn span(&mut self, id: NodeId, quad: u32) {
        self.spans[slot_index(id)] = Span::new(self.epoch, quad);
    }

    /// 放进一个新的上下文，返回下标
    #[inline]
    pub(super) fn frame(&mut self, frame: Frame) -> u32 {
        self.frames.push(frame);
        self.frames.len() as u32 - 1
    }
}

/// 遍历里每个元素都要看的、循环外先取出来的：要画焦点环的元素、键盘焦点所在的链接、有焦点的选择范围、
/// 有没有按叠放次序排过的子元素
#[derive(Clone, Copy)]
struct Marks {
    ring: Option<NodeId>,
    link_ring: Option<(NodeId, usize)>,
    focused_selection_root: Option<u64>,
    sorted: bool,
}

impl Marks {
    /// 这个元素有焦点环要画（键盘焦点在它或者它的链接上）：走完整的路径
    #[inline]
    fn marked(&self, id: NodeId) -> bool {
        self.ring == Some(id) || self.link_ring.is_some_and(|(f, _)| f == id)
    }
}

/// 绘制遍历里画元素内容要用的：位置、内容区、层内变换、透明度、所在的层和裁剪，链接的焦点环、有焦点的选择范围
#[derive(Clone, Copy)]
pub(crate) struct ContentArea {
    pub(crate) bounds: Rect,
    pub(crate) content: Rect,
    pub(crate) local: Affine,
    pub(crate) opacity: f32,
    pub(crate) layer: u32,
    /// 元素自己的裁剪（内容在它里面）
    pub(crate) inner: u32,
    pub(crate) link_ring: Option<(NodeId, usize)>,
    pub(crate) focused_selection_root: Option<u64>,
}

/// 子树里不止一个图元（或者有模糊）的分组候选记成分组（不撑大绘制遍历的循环）
#[cold]
#[inline(never)]
fn push_group(list: &mut DisplayList, layers: &mut [LayerInfo], quads: std::ops::Range<u32>, layer: u32, blur: f32, clip: u32) {
    let key = layers[layer as usize - 1].key;
    list.groups.push(Group { quads, layers: layer..layers.len() as u32 + 1, blur, clip, key, ..Default::default() });
    layers[layer as usize - 1].group = true;
}

/// 分组在屏幕上的范围由渲染线程按层变换算，这里给出每层图元在层坐标里的外接矩形（`layer_bounds` 已经是层数 + 1 项）
pub(super) fn layer_bounds(list: &mut DisplayList) {
    list.layer_bounds.fill(scene::EMPTY_BOUNDS);
    for quad in &list.quads {
        let b = quad.bounds();
        let l = &mut list.layer_bounds[quad.layer as usize];
        *l = [l[0].min(b[0]), l[1].min(b[1]), l[2].max(b[2]), l[3].max(b[3])];
    }
}

/// 层 `layer`（层号，0 是根层）自己或祖先层带着 3D 设置：变换可能有透视
pub(crate) fn may_project(layers: &[LayerInfo], mut layer: u32) -> bool {
    while layer != 0 {
        let desc = &layers[layer as usize - 1];
        if desc.depth.is_some() {
            return true;
        }
        layer = desc.parent;
    }
    false
}

/// 元素自己的形状图元。`center` 是变换后的中心，`linear` 是累积的缩放旋转（绕图元自身中心），层号和裁剪直接写进去
#[inline(always)]
fn make_quad(bounds: Rect, paint: &Paint, center: [f32; 2], linear: [f32; 4], opacity: f32, layer: u32, clip: u32) -> Quad {
    let (w, h) = (bounds.width, bounds.height);
    let mut quad = Quad::new(center[0] - w * 0.5, center[1] - h * 0.5, w, h);
    quad.transform = linear;
    quad.opacity = opacity;
    quad.layer = layer;
    quad.clip = clip;
    style_quad(&mut quad, paint);
    quad
}

/// 形状图元里只由绘制属性决定、不影响图元个数和结构的部分：圆角、填充色、边框、一层外投影。
/// 生成图元和原地修补（`tree/patch.rs`）都用它，两边写出来的逐位相同
#[inline(always)]
pub(super) fn style_quad(quad: &mut Quad, paint: &Paint) {
    quad.radii = [paint.radius; 4];
    quad.fill = paint.background.color();
    (quad.border_width, quad.border_color) = if paint.border_width > 0.0 { (paint.border_width, paint.border_color) } else { (0.0, scene::Color::TRANSPARENT) };
    match paint.shadow {
        ShadowSlot::Plain(s) if !s.color.is_transparent() => {
            quad.shadow_offset = [s.x, s.y];
            quad.shadow_blur = s.blur;
            quad.shadow_color = s.color;
        }
        _ => {
            quad.shadow_offset = [0.0; 2];
            quad.shadow_blur = 0.0;
            quad.shadow_color = scene::Color::TRANSPARENT;
        }
    }
}

impl Tree {
    /// 自己滚的元素（多行输入框）看得见的部分（内容坐标）；不是自己滚的时是 `None`（全部画）
    pub(super) fn editor_viewport(&self, id: NodeId) -> Option<Viewport> {
        let s = self.nodes.get(id)?.scroll.as_deref().filter(|s| s.own)?;
        Some(Viewport { top: s.shown[1], height: s.metrics.viewport[1] })
    }

    /// 前序遍历生成显示列表和合成层：父元素在子元素下面，先出现的兄弟在下面。
    /// 普通元素的平移、缩放、旋转、透明度沿树累积，烘焙进图元；合成层自己的这些属性
    /// 留给渲染线程每帧计算，层内的图元用层坐标。
    ///
    /// 半透明（或透明度在动）、模糊的元素是分组的候选：它也成为一个层，离开它的子树时，
    /// 如果子树里不止一个图元（或者有模糊），就记成分组，渲染时先画到离屏纹理再整体合成
    pub(super) fn build_list(&mut self) {
        // 时间线在合成层上的值：层表里的值要用
        self.sync_timeline_layers();
        // 拖放的影子画在源元素此刻在屏幕上的位置
        let ghost = self.drag_drop.ghost_source();
        if ghost.is_some() {
            self.refresh_worlds();
        }
        // 解码好的图片先分配 GPU 位置（图集重新装箱会改变所有图片的位置）
        self.images.system_mut().prepare();
        // 上一份列表和层表的容量拿回来接着用（双缓冲），"节点 → 图元"的表换个编号
        let (mut list, layers, spans, epoch) = self.paint.begin_list(self.nodes.slots());
        list.shaders = self.paint.shader_sources();
        let mut w = Walk::new(list, layers, spans, epoch, self.paint.take_buffers());
        let marks = Marks {
            // 要画焦点环的元素、有没有按叠放次序排过的子元素：循环外先取出来，每个元素只比较一次
            ring: self.router.focused().filter(|&id| self.router.focus_visible() && self.ring_on(id)),
            // 键盘焦点在富文本的链接上：焦点环画在链接上
            link_ring: self.focused_link().filter(|_| self.router.focus_visible()),
            // 焦点在选择根里的文字上：这个选择根里的选区都画成有焦点的颜色
            focused_selection_root: self.focused_selection_root(),
            sorted: self.nodes.sorted(),
        };
        // 浮层画在所有根元素之后（栈底），不受任何祖先的裁剪；拖放的影子在最底下（最后画）
        w.stack.extend(ghost.map(Step::Ghost));
        w.stack.extend(self.overlays.hosts().rev().chain(self.nodes.roots().iter().rev().copied()).map(|id| Step::Enter(id, 0)));
        // 正在飞的共享元素：别的都画完以后（拖放的影子之前）再画；它起飞之后才挂上的子树盖住它，在那之前画
        let covers = self.flight_covers();

        loop {
            let step = match w.stack.pop() {
                Some(Step::Enter(id, f)) if !covers.is_empty() && w.lifted.iter().any(|&(l, _)| covers.contains(&(id, l))) => {
                    let (under, rest) = w.lifted.drain(..).partition::<Vec<_>, _>(|&(l, _)| covers.contains(&(id, l)));
                    w.lifted = rest;
                    w.stack.push(Step::Enter(id, f));
                    w.stack.extend(under.into_iter().rev().map(|(l, f)| Step::Lift(l, f)));
                    continue;
                }
                // 拖放的影子之前
                Some(step @ Step::Ghost(_)) if !w.lifted.is_empty() => {
                    w.stack.push(step);
                    w.stack.extend(w.lifted.drain(..).rev().map(|(l, f)| Step::Lift(l, f)));
                    continue;
                }
                Some(step) => step,
                None if !w.lifted.is_empty() => {
                    w.stack.extend(w.lifted.drain(..).rev().map(|(l, f)| Step::Lift(l, f)));
                    continue;
                }
                None => break,
            };
            match step {
                Step::Enter(id, f) => {
                    let node = &self.nodes[id];
                    debug_assert_eq!(node.flags, node.compute_flags(), "节点的绘制标记没跟上属性的变化");
                    // 快路径：没有不常见的东西。算一次仿射变换、写一个形状图元，子元素压栈
                    if node.flags.is_plain() && !marks.marked(id) {
                        let frame = w.frames[f as usize];
                        let p = &node.paint;
                        let bounds = node.bounds;
                        let center = bounds.center();
                        let moved = node.flags.contains(PaintFlags::TRANSFORM);
                        let local = if moved { frame.local.then_local(center, p.translate, p.scale, p.rotation) } else { frame.local };
                        let quad = if frame.opacity <= 0.0 {
                            Span::INVISIBLE
                        } else if p.visible() {
                            w.list.quads.push(make_quad(bounds, p, local.apply(center), local.linear, frame.opacity, frame.layer, frame.clip));
                            w.list.quads.len() as u32 - 1
                        } else {
                            Span::HIDDEN
                        };
                        w.span(id, quad);
                        // 子元素按叠放次序画（`stack_order` 大的在后面、在上面）
                        let order = if marks.sorted { node.input.as_ref().and_then(|i| i.order.as_deref()).unwrap_or(&node.children) } else { &node.children };
                        if !order.is_empty() {
                            // 自己没有变换：子元素用父元素那一项上下文
                            let child = if moved { w.frame(Frame { local, ..frame }) } else { f };
                            w.stack.extend(order.iter().rev().map(|&c| Step::Enter(c, child)));
                        }
                        continue;
                    }
                    self.enter(id, f, false, &mut w, &marks);
                }
                Step::Lift(id, f) => self.enter(id, f, true, &mut w, &marks),
                Step::Exit { layer, first, blur, clip } => {
                    let end = w.list.quads.len() as u32;
                    if blur > 0.0 || end - first >= 2 {
                        push_group(&mut w.list, &mut w.layers, first..end, layer, blur, clip);
                    }
                }
                Step::Effects(step) => self.effects_step(step, &mut w),
                Step::After { id, frame, outer } => {
                    let Frame { local, opacity, layer, clip } = w.frames[frame as usize];
                    if self.nodes[id].scroll.is_some() {
                        self.paint_bars(id, layer, clip, local, opacity, &mut w.list, &mut w.layers);
                    }
                    if marks.ring == Some(id) {
                        self.paint_ring(id, layer, outer, local, opacity, &mut w.list);
                    }
                    self.paint_inline_selection(id, layer, clip, &local, opacity, &mut w.list);
                }
                Step::Ghost(id) => {
                    let first = w.list.quads.len() as u32;
                    if let Some(frame) = self.begin_ghost(id, &mut w.list, &mut w.layers) {
                        let f = w.frame(frame);
                        w.stack.push(Step::Exit { layer: frame.layer, first, blur: 0.0, clip: 0 });
                        w.stack.push(Step::Enter(id, f));
                    }
                }
            }
        }

        let Walk { mut list, mut layers, spans, folded, stack, frames, exits, lifted, .. } = w;
        // 用影子拖动时源元素变淡：它的层（第一遍画的那个，影子里的是第二个）透明度乘上去
        if let Some(source) = ghost {
            self.dim_source(source, &mut layers);
        }
        // 分组按前序排列（离开子树时才登记的，外层的排在后面）
        list.groups.sort_by_key(|g| (g.quads.start, std::cmp::Reverse(g.quads.end), g.layers.start));
        if !list.groups.is_empty() {
            list.layer_bounds.resize(layers.len() + 1, scene::EMPTY_BOUNDS);
            layer_bounds(&mut list);
        }
        // 拖放的影子把源元素的子树又画了一遍（同一个节点两个图元）：这一份不能原地修补
        self.paint.set_list(list, layers, spans, &folded, ghost.is_none());
        self.paint.put_buffers(WalkBuffers { stack, frames, exits, lifted, folded });
        // 这一帧之前给的滚动偏移都已经写进新的层表
        self.commit.clear_offsets();
    }

    /// 完整的路径：有不常见的东西（内容、合成层、滚动、3D、透明度、模糊、效果、背景模糊、着色器、裁剪、几层投影、
    /// 渐变背景）或者要画焦点环的元素。`lifting`：飞着的共享元素推迟到最后画的那一次
    #[inline(never)]
    fn enter(&mut self, id: NodeId, f: u32, lifting: bool, w: &mut Walk, marks: &Marks) {
        let Frame { local, opacity, layer, clip } = w.frames[f as usize];
        let node = &self.nodes[id];
        // 位置在定位时已经写好（布局之后），这里只读
        let bounds = node.bounds;
        // 3D 设置、变换中心、共享元素很少见，单独处理（可能不画、推迟到最后画）
        let (node, eye) = if node.extra.is_none() {
            (node, None)
        } else {
            let Some(eye) = self.enter_extra(id, f, bounds, lifting, w) else { return };
            (&self.nodes[id], eye)
        };
        let extra = node.extra.as_deref();
        let paint = &node.paint;
        let center = bounds.center();
        let origin_point = extra.map_or(center, |e| e.origin_point(bounds));
        let shift = node.offset();
        let translate = [paint.translate[0] + shift[0], paint.translate[1] + shift[1]];
        // 分组候选：透明度不是 1（或者在动）、有模糊、滤镜、遮罩、混合模式。静态全透明的不用
        let fading = node.layer
            && ((node.motion & timeline::TAKEN != 0 && self.motion.is_taken(id, PropId::OPACITY))
                || node.animations.iter().any(|slot| slot.property == PropId::OPACITY));
        let group = paint.blur > 0.0 || ((paint.opacity < 1.0 || fading) && (paint.opacity > 0.0 || fading)) || paint.effects.is_some();
        // 绕 x、y 轴转过的、背面不可见的、飞着的元素也是合成层（它们的变换不能烘焙进图元）
        let (local, opacity, layer) = if node.layer || group || extra.is_some_and(NodeExtra::needs_layer) {
            w.layers.push(LayerInfo {
                key: layer_key(id),
                parent: layer,
                base: local,
                base_opacity: opacity,
                center: origin_point,
                values: LayerValues {
                    translate: paint.translate,
                    scale: paint.scale,
                    rotation: paint.rotation,
                    opacity: paint.opacity,
                    offset: node.layer_offset(),
                },
                group: false,
                depth: if extra.is_some() { node.layer_depth(bounds, eye, lifting) } else { None },
            });
            // 层内从布局坐标重新开始；层自身的变换和透明度由渲染线程加上
            (Affine::IDENTITY, 1.0, w.layers.len() as u32)
        } else {
            (local.then_local(origin_point, translate, paint.scale, paint.rotation), opacity * paint.opacity, layer)
        };

        // 自己的形状：用祖先的裁剪
        let first = w.list.quads.len();
        if opacity > 0.0 && (paint.visible() || paint.backdrop_blur > 0.0 || paint.shader > 0) {
            let list = &mut w.list;
            let mut base = make_quad(bounds, paint, local.apply(center), local.linear, opacity, layer, clip);
            // 图片的边框画在图片上面（见下面的内容）
            if paint.border_width > 0.0 && node.has_kind && self.nodes.kind::<ImageBlock>(id).is_some() {
                base.border_width = 0.0;
            }
            if paint.backdrop_blur > 0.0 {
                // 背景模糊画在最下面，背景色、边框、阴影照常画在它上面
                let mut backdrop = base;
                backdrop.kind = Quad::BACKDROP;
                backdrop.border_width = 0.0;
                backdrop.shadow_color = scene::Color::TRANSPARENT;
                list.quad_effects.push(scene::QuadEffect::Backdrop { quad: list.quads.len() as u32, blur: paint.backdrop_blur, saturation: paint.backdrop_saturation });
                list.quads.push(backdrop);
            }
            if paint.shader > 0 {
                // 自定义着色器代替背景色：阴影在下，内阴影和边框在上
                let carrier = Quad { fill: scene::Color::TRANSPARENT, border_width: 0.0, ..base };
                let layered = paint.layered();
                if let Some(l) = layered {
                    crate::shadow::push_outer(list, &carrier, &l.shadows);
                }
                if !carrier.shadow_color.is_transparent() {
                    list.quads.push(carrier);
                }
                list.animated |= self.paint.shader_animated(paint.shader);
                list.quad_effects.push(scene::QuadEffect::Shader { quad: list.quads.len() as u32, shader: u32::from(paint.shader) - 1 });
                list.quads.push(Quad { kind: Quad::CUSTOM, uv: paint.shader_params, border_width: 0.0, shadow_color: scene::Color::TRANSPARENT, ..base });
                if let Some(l) = layered {
                    crate::shadow::push_inner(list, &base, &l.shadows, paint.border_width);
                }
                if paint.border_width > 0.0 && !paint.border_color.is_transparent() {
                    let mut border = Quad { fill: scene::Color::TRANSPARENT, shadow_color: scene::Color::TRANSPARENT, ..base };
                    if let Some(gradient) = layered.and_then(|l| l.border.as_ref()) {
                        border = border.with_border_gradient(gradient.push_local(list, [bounds.width, bounds.height]));
                    }
                    list.quads.push(border);
                }
            } else if paint.visible() {
                let mut quad = base;
                if let Fill::Gradient(gradient) = &paint.background {
                    quad.gradient = gradient.push(list, bounds, &local);
                }
                match &paint.shadow {
                    ShadowSlot::Plain(_) => list.quads.push(quad),
                    ShadowSlot::Layered(l) => crate::shadow::push_layered(list, quad, &l.shadows, l.border.as_ref(), [bounds.width, bounds.height]),
                }
            }
            // 投影、内阴影、边框这些额外的图元是从 `base` 复制的，层号和裁剪已经对了
        }
        // 自己的形状是一个普通形状图元（或者没画）：记进"节点 → 图元"的表，颜色这类属性变了时原地修补
        if patch::regular(node.flags) {
            let quad = if opacity <= 0.0 {
                Span::INVISIBLE
            } else if paint.visible() {
                first as u32
            } else {
                Span::HIDDEN
            };
            w.span(id, quad);
        }

        // 自己的裁剪区域：作用于文字和子元素
        let inner = if paint.clip {
            let element = local.inverse().unwrap_or(Affine::IDENTITY);
            w.list.clips.push(Clip {
                inverse_linear: element.linear,
                inverse_offset: element.offset,
                center,
                half_size: [bounds.width * 0.5, bounds.height * 0.5],
                // 自己或祖先层带着 3D 设置（可能有透视）：着色器按投影算这个裁剪
                layer: if may_project(&w.layers, layer) { layer | Clip::PROJECTIVE } else { layer },
                parent: clip,
                radii: [paint.radius; 4],
            });
            w.list.clips.len() as u32
        } else {
            clip
        };
        let first_content = w.list.quads.len();
        // 文字、输入框、路径、字符网格、图片、链接的焦点环
        let has_content = node.has_kind || marks.link_ring.is_some_and(|(f, _)| f == id);
        let (node, placed) = if opacity > 0.0 && has_content {
            // 内容区：去掉内边距和边框（定位时记下的）
            let [left, top, right, bottom] = self.nodes.insets(id);
            let content = Rect { x: bounds.x + left, y: bounds.y + top, width: bounds.width - left - right, height: bounds.height - top - bottom };
            let area = ContentArea { bounds, content, local, opacity, layer, inner, link_ring: marks.link_ring, focused_selection_root: marks.focused_selection_root };
            let placed = self.paint_content(id, area, &mut w.layers, &mut w.list);
            (&self.nodes[id], placed)
        } else {
            (node, w.list.quads.len())
        };
        let paint = &node.paint;
        // 画布的图元自己设好了层和裁剪（画布里有自己的裁剪）
        for quad in &mut w.list.quads[first_content..placed] {
            quad.layer = layer;
            quad.clip = inner;
        }

        // 子元素的上下文：和自己的一样时用同一项
        let inner_frame = Frame { local, opacity, layer, clip: inner };
        let same = local == w.frames[f as usize].local && opacity == w.frames[f as usize].opacity && layer == w.frames[f as usize].layer && inner == clip;
        let child = if same { f } else { w.frame(inner_frame) };
        if group {
            match paint.effects {
                None => w.stack.push(Step::Exit { layer, first: first as u32, blur: paint.blur, clip }),
                // 有效果的元素（还可能有遮罩，在子元素之后画）
                Some(_) => self.enter_effects(child, EffectsExit { id, layer, first: first as u32, clip, mask: None }, w),
            }
        }
        if node.scroll.is_some() || marks.ring == Some(id) || (node.has_kind && self.has_selected_inlines(id)) {
            w.stack.push(Step::After { id, frame: child, outer: clip });
        }
        // 子元素按叠放次序画（`stack_order` 大的在后面、在上面）
        let order = if marks.sorted { node.input.as_ref().and_then(|i| i.order.as_deref()).unwrap_or(&node.children) } else { &node.children };
        w.stack.extend(order.iter().rev().map(|&c| Step::Enter(c, child)));
    }

    /// 元素自己的内容（种类对象的 `paint`：文字、输入框、路径、字符网格、图片、画布、自己定义的），
    /// 键盘焦点在链接上时的焦点环（绘制遍历里有种类对象或者链接焦点环的元素才调用）。
    /// 返回从哪里开始的图元已经自己设好了层和裁剪（用 `Drawing` 画的），其余的由调用方设
    #[inline(never)]
    pub(super) fn paint_content(&mut self, id: NodeId, area: ContentArea, layers: &mut Vec<LayerInfo>, list: &mut DisplayList) -> usize {
        let ContentArea { bounds, local, opacity, layer, inner, link_ring, .. } = area;
        // 从这里开始的图元自己设好了层和裁剪
        let mut placed = None;
        // 种类对象先取下来（画的时候要读别的节点），画完放回去
        if let Some(mut kind) = self.nodes.take_kind(id) {
            // 继承的文字颜色；渐变铺满元素自己的框
            let color = &self.nodes[id].paint.color;
            let mut ink = Ink { color: color.color(), opacity, gradient: 0 };
            if let Fill::Gradient(gradient) = color {
                ink.gradient = gradient.push(list, bounds, &local);
            }
            let mut cx = PaintContext { tree: self, id, area: &area, layers, list, ink, placed: None };
            kind.paint(&mut cx);
            placed = cx.placed;
            self.nodes.put_kind(id, kind);
        }
        if let Some((focused, k)) = link_ring
            && focused == id
        {
            self.paint_link_ring(id, k, layer, inner, &local, opacity, list);
        }
        placed.unwrap_or(list.quads.len())
    }

    /// 绘制遍历进入一个有 3D 设置、变换中心或者共享标签的元素：返回父元素给的透视。
    /// 同一个标签有别的元素在显示时不画（`None`）；刚接替旧元素的开始飞；飞着的推迟到所有元素画完以后再画（`None`）
    #[cold]
    #[inline(never)]
    fn enter_extra(&mut self, id: NodeId, f: u32, bounds: Rect, lifting: bool, w: &mut Walk) -> Option<Option<depth::Eye>> {
        let node = &self.nodes[id];
        if node.extra.as_deref().is_some_and(NodeExtra::hidden) {
            return None;
        }
        let eye = depth::parent_eye(&self.nodes, node);
        // 刚接替旧元素：这时才知道它的位置（不含飞行的屏幕变换），开始飞；之后命中测试按飞到的位置算
        if self.flight_pending(id) {
            if let Some(own) = self.own_screen_transform(id) {
                self.start_flight(id, &own, bounds);
            }
            self.paint.mark_worlds();
        }
        // 飞着：在最上面画，不受祖先的裁剪
        if !lifting && self.nodes[id].extra.as_deref().is_some_and(NodeExtra::lifted) {
            let frame = Frame { clip: 0, ..w.frames[f as usize] };
            let f = w.frame(frame);
            w.lifted.push((id, f));
            return None;
        }
        Some(eye)
    }

    /// 元素自己（不算共享元素的飞行）到屏幕的变换：祖先按此刻的值，O(深度)
    fn own_screen_transform(&self, id: NodeId) -> Option<Transform> {
        let node = self.nodes.get(id)?;
        let outer = match node.parent {
            Some(parent) => self.screen_transform(parent)?,
            None => Transform::IDENTITY,
        };
        Some(node.place(&outer, node.bounds, depth::parent_eye(&self.nodes, node), false))
    }
}
