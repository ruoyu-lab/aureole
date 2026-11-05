//! 画布：在一个元素里自由地画大量图形和图表（[`canvas`]）。
//!
//! 画的闭包只在它读到的信号变了、或者画布大小变了时重新执行，画出来的东西记下来；界面别处变了要重新生成
//! 显示列表时，直接把记下的图元拷过去，不再执行闭包。矩形、圆、线段、圆角连接的折线直接变成距离场图元
//! （和元素的形状一样，每个一个实例，和界面的其他部分在同一次 draw call 里），几万个点也只是几万个实例；
//! 任意路径栅格化进路径图集里画布自己预留的一块，内容不变时不重新栅格化。
//! 裁剪是显示列表的圆角矩形裁剪，跟着元素的变换、合成层动画走。
//!
//! 画布是一种元素种类（[`crate::kinds::ElementKind`]），只用公开的接口：闭包在树外更新时执行（`tree/kinds.rs`），
//! 画好的东西记在 [`Drawing`] 里，放进显示列表见 `tree/canvas.rs`。

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use reactive::Signal;
use scene::{Angle, Color, Quad};

use crate::element::Element;
use crate::kinds::{
    AccessibilityContext, AccessibilityProvider, ElementKind, InputTarget, KindHooks, PaintContext, StyleContext, SubTarget, UnmountContext,
    UpdateContext,
};
use crate::property::registry::{Dirty, KindMask};
use crate::geometry::Affine;
use crate::gradient::Fill;
use crate::geometry::Rect;
use crate::layout::Align;
use crate::text::{SpanId, TextStyle, TextSystem};
use crate::vector::{CanvasPath, CanvasRaster, LineCap, LineJoin, VectorPath, Stroke};

/// 画布：`draw` 在里面自由地画图形、路径、文字，适合图表、大量的点、自己画的控件。
///
/// 坐标是元素自己的坐标（左上角是原点，逻辑像素），[`Canvas::size`] 是元素的大小。`draw` 读到的信号变了
/// 才重新画（读的是 [`Signal::get`] 这样会订阅的读法），画布大小变了也重新画；别的时候用上次画的结果。
/// 画布没有默认大小：用 `size`、`full_size`、`grow` 给它大小。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let points = signal(vec![[0.0f32, 0.3], [0.5, 0.8], [1.0, 0.5]]);
/// canvas(move |cx| {
///     let [w, h] = cx.size();
///     cx.fill_round_rect(0.0, 0.0, w, h, 10.0, Color::hex(0xf6f8f9));
///     let line: Vec<[f32; 2]> = points.get().iter().map(|p| [p[0] * w, (1.0 - p[1]) * h]).collect();
///     cx.polyline(&line, Stroke::new(2.0).round(), Color::hex(0x138a84));
///     for p in &line {
///         cx.fill_circle(p[0], p[1], 3.5, Color::hex(0x138a84));
///     }
///     cx.text(8.0, 8.0, "温度", &CanvasTextStyle::new(12.0).align(Align::Start, Align::Start));
/// })
/// .size(320, 160);
/// ```
///
/// - 形状：[`Canvas::fill_rect`]、[`Canvas::fill_round_rect`]、[`Canvas::stroke_rect`]、[`Canvas::fill_circle`]、
///   [`Canvas::stroke_circle`]、[`Canvas::line`]、[`Canvas::polyline`]、[`Canvas::fill_path`]、[`Canvas::stroke_path`]、
///   [`Canvas::text`]；颜色都可以是 [`crate::Gradient`]（铺满这个图形的外接矩形）
/// - 状态：[`Canvas::save`] / [`Canvas::restore`] 成对使用，中间的平移、缩放、旋转、裁剪、透明度只对后面画的东西有效
/// - 命中：[`Canvas::hit_circle`]、[`Canvas::hit_rect`] 登记能点中的区域，[`CanvasHits`] 查指针下是哪一个
///
/// `draw` 里不要改信号，也不要读 [`CanvasHits`]（每次画完都会更新它，读了会一直重画）
pub fn canvas(draw: impl Fn(&mut Canvas) + 'static) -> Element {
    Element::from_kind(CanvasKind { draw: Box::new(draw), hits: None, drawing: Drawing::new(), described: Vec::new() })
}

/// 画布这种元素。它只用元素种类的公开接口（[`crate::kinds`]）：画的闭包在树外更新时执行（[`ElementKind::update`]，
/// 读到的信号变了、大小变了才重新画），画好的 [`Drawing`] 每次生成显示列表时拷进去。自己定义元素时照着它写
struct CanvasKind {
    draw: Box<dyn Fn(&mut Canvas)>,
    hits: Option<CanvasHits>,
    drawing: Drawing,
    /// 起了名字的区域（[`Canvas::hit_accessibility_name`]）：辅助技术看到的子节点
    described: Vec<DescribedRegion>,
}

impl ElementKind for CanvasKind {
    fn name(&self) -> &'static str {
        "canvas"
    }

    fn kind_mask(&self) -> KindMask {
        KindMask::CANVAS
    }

    /// 画的闭包读信号：在树外执行，读到的信号变了、大小变了再执行
    fn hooks(&self) -> KindHooks {
        KindHooks::UPDATE
    }

    // 没有自己的大小：用默认的 `measure`（样式给多大就多大）

    /// 文字在放进显示列表时才排：继承的样式变了只要重画
    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        if self.drawing.restyle(cx) { Dirty::PAINT } else { Dirty::empty() }
    }

    fn update(&mut self, cx: &mut UpdateContext) {
        self.drawing.record(cx.size(), cx.scale_factor(), |canvas| (self.draw)(canvas));
        self.described = self.drawing.described_regions();
        if let Some(hits) = self.hits {
            self.drawing.report_hits(hits);
        }
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        self.drawing.paint(cx);
    }

    fn unmount(&mut self, cx: &mut UnmountContext) {
        self.drawing.release(cx);
    }

    fn input_target(&self) -> Option<&dyn InputTarget> {
        Some(self)
    }

    fn input_target_mut(&mut self) -> Option<&mut dyn InputTarget> {
        Some(self)
    }

    fn accessibility_provider(&self) -> Option<&dyn AccessibilityProvider> {
        Some(self)
    }
}

/// 画布给辅助技术的：起了名字的区域（[`Canvas::hit_accessibility_name`]）是它的子节点，边界是区域的外接矩形。
/// 角色按元素推断（声明了名字的是图形，否则是装饰）
impl AccessibilityProvider for CanvasKind {
    fn write_node(&self, _: &mut accesskit::Node, _: &AccessibilityContext) {}

    fn virtual_children(&self, cx: &mut AccessibilityContext) {
        for (i, region) in self.described.iter().enumerate() {
            let mut hasher = DefaultHasher::new();
            (region.id, &region.name, region.bounds.map(f32::to_bits)).hash(&mut hasher);
            cx.push_child(i as u32, hasher.finish(), || {
                let mut node = accesskit::Node::new(accesskit::Role::Image);
                node.set_label(region.name.as_str());
                let [x0, y0, x1, y1] = region.bounds;
                node.set_bounds(accesskit::Rect::new(x0 as f64, y0 as f64, x1 as f64, y1 as f64));
                node
            });
        }
    }
}

/// 起了名字的命中区域（[`Canvas::hit_accessibility_name`]，[`Drawing::described_regions`] 读）：
/// 自己定义的元素把它们当成辅助技术看到的子节点
#[derive(Clone, Debug, PartialEq)]
pub struct DescribedRegion {
    /// 登记命中区域时的编号
    pub id: usize,
    /// 读屏软件念的名字
    pub name: String,
    /// 这个编号的所有区域的外接矩形（画布坐标）：左、上、右、下
    pub bounds: [f32; 4],
}

/// 画布是输入目标：指针下是哪个登记过的区域（[`Canvas::hit_circle`]、[`Canvas::hit_rect`]，交给了 [`Element::hits`] 时）。
/// 它不拿焦点，事件照常交给元素上的处理器（`.focusable(true)` 之后 Tab 走得到）
impl InputTarget for CanvasKind {
    fn hit(&self, position: [f32; 2], _: Rect) -> Option<SubTarget> {
        match self.hits.and_then(|hits| hits.at_untracked(position)) {
            Some(region) => Some(SubTarget::Part(region)),
            None => Some(SubTarget::Whole),
        }
    }
}

impl Element {
    /// 画布（[`canvas`]）每次画完，把 [`Canvas::hit_circle`]、[`Canvas::hit_rect`] 登记的区域交给 `hits`，
    /// 事件里用 [`CanvasHits::at`] 查指针下是哪一个。不是画布的元素上什么都不做
    pub fn canvas_hits(mut self, hits: CanvasHits) -> Self {
        if let Some(canvas) = self.kind_mut::<CanvasKind>() {
            canvas.hits = Some(hits);
        }
        self
    }
}

/// 画布上的文字样式（[`Canvas::text`]）。没设的字号、字重、颜色用画布元素继承的文字样式
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CanvasTextStyle {
    size: Option<f32>,
    weight: Option<f32>,
    color: Option<Fill>,
    align: [Option<Align>; 2],
    tabular: bool,
}

impl CanvasTextStyle {
    /// 字号
    pub fn new(size: f32) -> Self {
        Self { size: Some(size), ..Default::default() }
    }

    /// 字重（400 常规，500 中等）
    pub fn font_weight(mut self, weight: f32) -> Self {
        self.weight = Some(weight);
        self
    }

    /// 颜色：纯色或渐变
    pub fn color(mut self, color: impl Into<Fill>) -> Self {
        self.color = Some(color.into());
        self
    }

    /// 文字的哪一点放在给的坐标上。水平：`Start` 左边、`Center` 中间、`End` 右边；
    /// 竖直：`Start` 上边、`Center` 中间、`End` 下边、`Baseline` 第一行的基线（默认）
    pub fn align(mut self, horizontal: Align, vertical: Align) -> Self {
        self.align = [Some(horizontal), Some(vertical)];
        self
    }

    /// 等宽数字：数字一样宽，刻度、读数对得齐
    pub fn tabular_numbers(mut self, tabular: bool) -> Self {
        self.tabular = tabular;
        self
    }

    pub(crate) fn size(&self) -> Option<f32> {
        self.size
    }

    pub(crate) fn weight_value(&self) -> Option<f32> {
        self.weight
    }

    pub(crate) fn fill(&self) -> Option<&Fill> {
        self.color.as_ref()
    }

    pub(crate) fn anchor(&self) -> [Align; 2] {
        [self.align[0].unwrap_or(Align::Start), self.align[1].unwrap_or(Align::Baseline)]
    }

    pub(crate) fn is_tabular(&self) -> bool {
        self.tabular
    }
}

/// 画的状态：变换、裁剪、透明度。[`Canvas::save`] 存一份，[`Canvas::restore`] 取回来
#[derive(Clone, Copy)]
struct State {
    transform: Affine,
    /// 裁剪：[`Recording::clips`] 的下标 + 1，0 表示没有（只受元素外面的裁剪）
    clip: u32,
    opacity: f32,
}

/// 画布的画笔：[`canvas`] 的闭包拿到它。坐标是元素自己的坐标（左上角是原点），先经过当前的变换
pub struct Canvas {
    state: State,
    stack: Vec<State>,
    rec: Recording,
    /// 逻辑像素 -> 物理像素：折线按物理像素列抽稀
    scale: f32,
}

/// 画好的一幅：[`Drawing::record`] 用画笔 [`Canvas`] 画，画出来的东西记下来；生成显示列表时 [`Drawing::paint`]
/// 把记下的图元拷进去（经过元素的变换），不再执行画的闭包。自己定义的元素种类（[`crate::kinds::ElementKind`]）用它画内容：
/// 种类对象里放一个，在 `update`（读信号）或 `paint` 里记，在 `paint` 里放进显示列表，在 `unmount` 里 [`Drawing::release`]。
///
/// 路径栅格化进路径图集里为这一幅预留的块，内容不变时不重新栅格化；文字按内容和样式缓存排版，
/// 没设字号、颜色的文字用元素继承的文字样式（[`Drawing::restyle`] 记下）
#[derive(Default)]
pub struct Drawing {
    pub(crate) recording: Option<Recording>,
    /// 继承的文字样式：文字的字体、默认的字号和字重
    pub(crate) style: TextStyle,
    /// 每条路径在路径图集里预留的一块，按画的顺序
    pub(crate) rasters: Vec<CanvasRaster>,
    /// 排好的文字。每次生成显示列表时只留下这一次用到的
    pub(crate) texts: HashMap<TextKey, parley::Layout<SpanId>>,
}

/// 文字排版的缓存键：内容、字号、字重、字体、等宽数字、斜体、字距
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct TextKey {
    pub text: String,
    pub numbers: [u32; 3],
    pub family: u32,
    pub flags: u8,
}

impl Drawing {
    /// 空的一幅
    pub fn new() -> Self {
        Self::default()
    }

    /// 重新画：`size` 是画布大小（一般是元素的大小，[`crate::kinds::PaintContext::size`]），`scale` 是窗口缩放。
    /// 原来记下的换掉；路径图集里预留的块、排好的文字留着，下次放进显示列表时还能用上
    pub fn record(&mut self, size: [f32; 2], scale: f32, draw: impl FnOnce(&mut Canvas)) {
        let mut canvas = Canvas::new(size, scale);
        draw(&mut canvas);
        self.recording = Some(canvas.finish());
    }

    /// 画过没有
    pub fn is_empty(&self) -> bool {
        self.recording.is_none()
    }

    /// 上次画时起了名字的区域（[`Canvas::hit_accessibility_name`]）：编号、名字、外接矩形，同一个编号的几块合成一个
    pub fn described_regions(&self) -> Vec<DescribedRegion> {
        let Some(recording) = &self.recording else { return Vec::new() };
        recording
            .labels
            .iter()
            .filter_map(|(id, label)| {
                let bounds = recording.hits.iter().filter(|r| r.id == *id).map(Region::bounds).reduce(|a, b| {
                    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
                })?;
                Some(DescribedRegion { id: *id, name: label.clone(), bounds })
            })
            .collect()
    }

    /// 上次画时登记的命中区域（[`Canvas::hit_circle`]、[`Canvas::hit_rect`]）交给 `hits`，之后事件里用 [`CanvasHits::at`] 查
    pub fn report_hits(&mut self, hits: CanvasHits) {
        if let Some(recording) = &mut self.recording {
            hits.set(std::mem::take(&mut recording.hits));
        }
    }

    /// 记下元素继承的文字样式（[`crate::kinds::ElementKind::restyle`] 里调用），返回变了没有（变了要重画）
    pub fn restyle(&mut self, cx: &crate::kinds::StyleContext) -> bool {
        if self.style == cx.style {
            return false;
        }
        self.style = cx.style;
        true
    }

    /// 还回路径图集里预留的块（元素卸载时，[`crate::kinds::ElementKind::unmount`] 里调用）
    pub fn release(&mut self, cx: &mut crate::kinds::UnmountContext) {
        self.release_slots(cx.text);
    }

    pub(crate) fn release_slots(&mut self, text: &mut TextSystem) {
        for raster in self.rasters.drain(..) {
            text.release_slot(raster.slot);
        }
    }
}

/// 画出来的东西：坐标在画布里（还没有放到元素的位置上），生成显示列表时再经过元素的变换
#[derive(Default)]
pub(crate) struct Recording {
    pub size: [f32; 2],
    /// 距离场图元
    pub quads: Vec<Quad>,
    /// 按画的顺序：一段现成的图元，或者生成显示列表时才栅格化、排版的路径和文字
    pub items: Vec<Item>,
    pub clips: Vec<ClipShape>,
    /// 渐变填充和渐变描边：生成显示列表时登记进去
    pub gradients: Vec<GradientUse>,
    pub hits: Vec<Region>,
    /// 区域的名字（[`Canvas::hit_accessibility_name`]）：编号 → 名字
    pub labels: Vec<(usize, String)>,
}

pub(crate) enum Item {
    /// `quads[start..end]`
    Quads(u32, u32),
    Path(Box<PathItem>),
    Text(Box<TextItem>),
}

/// 一条要栅格化的路径
pub(crate) struct PathItem {
    pub path: CanvasPath,
    /// 路径坐标 -> 画布
    pub transform: Affine,
    pub fill: Fill,
    pub opacity: f32,
    pub clip: u32,
    /// 路径坐标里的外接矩形（渐变铺满它）
    pub bounds: Rect,
}

/// 一段文字：生成显示列表时排版（按内容和样式缓存）
pub(crate) struct TextItem {
    pub text: String,
    pub text_style: CanvasTextStyle,
    pub at: [f32; 2],
    pub transform: Affine,
    pub opacity: f32,
    pub clip: u32,
}

/// 一个裁剪区域：圆角矩形，在 `transform` 之前的坐标里
pub(crate) struct ClipShape {
    pub rect: Rect,
    pub radius: f32,
    pub transform: Affine,
    pub parent: u32,
}

/// 第 `quad` 个图元的填充（或描边）是渐变
pub(crate) struct GradientUse {
    pub quad: u32,
    pub gradient: crate::Gradient,
    /// 填充：渐变铺满的框（`transform` 之前的坐标）；描边：图元自己的大小
    pub bounds: Rect,
    pub transform: Affine,
    pub border: bool,
}

/// 一个能点中的区域，画布坐标
#[derive(Clone, Copy, Debug)]
pub(crate) struct Region {
    id: usize,
    shape: HitShape,
}

#[derive(Clone, Copy, Debug)]
enum HitShape {
    Circle([f32; 2], f32),
    Rect([f32; 4]),
}

impl Canvas {
    pub(crate) fn new(size: [f32; 2], scale: f32) -> Self {
        Self {
            state: State { transform: Affine::IDENTITY, clip: 0, opacity: 1.0 },
            stack: Vec::new(),
            rec: Recording { size, ..Default::default() },
            scale: scale.max(0.1),
        }
    }

    pub(crate) fn finish(self) -> Recording {
        self.rec
    }

    /// 画布（元素）的大小，逻辑像素
    pub fn size(&self) -> [f32; 2] {
        self.rec.size
    }

    /// 一个物理像素是多少逻辑像素：画清晰的细线用
    pub fn pixel(&self) -> f32 {
        1.0 / self.scale
    }

    // ---- 状态 ----

    /// 存下当前的变换、裁剪、透明度
    pub fn save(&mut self) {
        self.stack.push(self.state);
    }

    /// 回到上一次 [`Canvas::save`] 时的状态；没有存过时什么都不做
    pub fn restore(&mut self) {
        if let Some(state) = self.stack.pop() {
            self.state = state;
        }
    }

    /// 平移之后画的东西
    pub fn translate(&mut self, x: f32, y: f32) {
        self.concat(Affine { linear: Affine::IDENTITY.linear, offset: [x, y] });
    }

    /// 缩放之后画的东西（绕当前原点），线宽也跟着缩放
    pub fn scale(&mut self, x: f32, y: f32) {
        self.concat(Affine { linear: [x, 0.0, 0.0, y], offset: [0.0, 0.0] });
    }

    /// 旋转之后画的东西（绕当前原点），正值顺时针
    pub fn rotate(&mut self, angle: Angle) {
        let (sin, cos) = angle.radians().sin_cos();
        self.concat(Affine { linear: [cos, sin, -sin, cos], offset: [0.0, 0.0] });
    }

    fn concat(&mut self, other: Affine) {
        self.state.transform = self.state.transform.compose(&other);
    }

    /// 之后画的东西再乘上这个透明度（0..1）
    pub fn opacity(&mut self, opacity: f32) {
        self.state.opacity *= opacity.clamp(0.0, 1.0);
    }

    /// 之后画的东西只在这个矩形里可见（和已有的裁剪取交集），边缘抗锯齿
    pub fn clip_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.clip_round_rect(x, y, width, height, 0.0);
    }

    /// 之后画的东西只在这个圆角矩形里可见（和已有的裁剪取交集）
    pub fn clip_round_rect(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32) {
        let rect = Rect { x, y, width: width.max(0.0), height: height.max(0.0) };
        self.rec.clips.push(ClipShape { rect, radius: radius.max(0.0), transform: self.state.transform, parent: self.state.clip });
        self.state.clip = self.rec.clips.len() as u32;
    }

    // ---- 形状 ----

    /// 填充矩形
    pub fn fill_rect(&mut self, x: f32, y: f32, width: f32, height: f32, fill: impl Into<Fill>) {
        self.fill_round_rect(x, y, width, height, 0.0, fill);
    }

    /// 填充圆角矩形
    pub fn fill_round_rect(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, fill: impl Into<Fill>) {
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let rect = Rect { x, y, width, height };
        self.shape(rect.center(), [width, height], None, [radius.max(0.0); 4], &fill.into(), None, rect);
    }

    /// 描矩形的边：线以边为中心，宽 `width`，转角是尖的
    pub fn stroke_rect(&mut self, x: f32, y: f32, width: f32, height: f32, line: f32, color: impl Into<Fill>) {
        self.stroke_round_rect(x, y, width, height, 0.0, line, color);
    }

    /// 描圆角矩形的边：线以边为中心，宽 `line`
    pub fn stroke_round_rect(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, line: f32, color: impl Into<Fill>) {
        if line <= 0.0 {
            return;
        }
        let half = line * 0.5;
        let outer = Rect { x: x - half, y: y - half, width: width + line, height: height + line };
        let radius = if radius > 0.0 { radius + half } else { 0.0 };
        self.shape(outer.center(), [outer.width, outer.height], None, [radius; 4], &Fill::Color(Color::TRANSPARENT), Some((line, &color.into())), outer);
    }

    /// 填充圆，圆心 (x, y)
    pub fn fill_circle(&mut self, x: f32, y: f32, radius: f32, fill: impl Into<Fill>) {
        if radius <= 0.0 {
            return;
        }
        let d = radius * 2.0;
        let rect = Rect { x: x - radius, y: y - radius, width: d, height: d };
        self.shape([x, y], [d, d], None, [radius; 4], &fill.into(), None, rect);
    }

    /// 描圆：线以圆周为中心，宽 `line`
    pub fn stroke_circle(&mut self, x: f32, y: f32, radius: f32, line: f32, color: impl Into<Fill>) {
        if line <= 0.0 {
            return;
        }
        let r = radius + line * 0.5;
        let rect = Rect { x: x - r, y: y - r, width: r * 2.0, height: r * 2.0 };
        self.shape([x, y], [r * 2.0, r * 2.0], None, [r; 4], &Fill::Color(Color::TRANSPARENT), Some((line, &color.into())), rect);
    }

    /// 线段：端点样式、虚线按 `stroke`（转角样式用不到）
    pub fn line(&mut self, from: [f32; 2], to: [f32; 2], stroke: Stroke, color: impl Into<Fill>) {
        let fill = color.into();
        let bounds = segment_bounds(&[from, to], stroke.width);
        if stroke.is_dashed() {
            self.dashed(from, to, stroke, &fill, bounds);
        } else {
            self.segment(from, to, stroke.width, [stroke.cap, stroke.cap], &fill, bounds);
        }
    }

    /// 折线：依次连起 `points`。圆角连接（[`Stroke::round`]）、不透明的纯色、不是虚线时每段一个距离场图元
    /// （连接处是两段的圆头叠在一起，多少个点都便宜）；x 单调、比物理像素还密的点先按像素列抽稀
    /// （每列留第一个、最高、最低、最后一个点，看起来一样）。其余情况（尖角、斜切角、半透明、渐变、虚线）
    /// 栅格化成路径
    pub fn polyline(&mut self, points: &[[f32; 2]], stroke: Stroke, color: impl Into<Fill>) {
        let fill = color.into();
        if points.len() < 2 || stroke.width <= 0.0 {
            return;
        }
        let points = self.thin(points);
        let opaque = matches!(fill, Fill::Color(c) if c.to_linear()[3] >= 1.0) && self.state.opacity >= 1.0;
        if stroke.join == LineJoin::Round && !stroke.is_dashed() && opaque {
            let bounds = segment_bounds(&points, stroke.width);
            let last = points.len() - 2;
            for (i, pair) in points.windows(2).enumerate() {
                let caps = [if i == 0 { stroke.cap } else { LineCap::Round }, if i == last { stroke.cap } else { LineCap::Round }];
                self.segment(pair[0], pair[1], stroke.width, caps, &fill, bounds);
            }
            return;
        }
        self.stroke_path(&VectorPath::polyline(points.iter().copied()), stroke, fill);
    }

    /// 填充路径（按路径的填充规则）。路径坐标就是画布坐标，再经过当前的变换
    pub fn fill_path(&mut self, path: &VectorPath, fill: impl Into<Fill>) {
        self.path(path, None, fill.into());
    }

    /// 描路径
    pub fn stroke_path(&mut self, path: &VectorPath, stroke: Stroke, color: impl Into<Fill>) {
        if stroke.width > 0.0 {
            self.path(path, Some(stroke), color.into());
        }
    }

    /// 文字：`label` 决定字号、颜色和文字的哪一点放在 (x, y) 上。单行，不换行
    pub fn text(&mut self, x: f32, y: f32, text: impl Into<String>, style: &CanvasTextStyle) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        let State { transform, clip, opacity } = self.state;
        self.rec.items.push(Item::Text(Box::new(TextItem { text, text_style: style.clone(), at: [x, y], transform, opacity, clip })));
    }

    // ---- 命中 ----

    /// 登记一个能点中的圆（经过当前的变换）：[`CanvasHits::at`] 在它里面时返回 `id`
    pub fn hit_circle(&mut self, id: usize, x: f32, y: f32, radius: f32) {
        let t = &self.state.transform;
        let center = t.apply([x, y]);
        let [a, b, c, d] = t.linear;
        let scale = (a * d - b * c).abs().sqrt();
        self.rec.hits.push(Region { id, shape: HitShape::Circle(center, radius * scale) });
    }

    /// 登记一个能点中的矩形（经过当前的变换后的外接矩形）
    pub fn hit_rect(&mut self, id: usize, x: f32, y: f32, width: f32, height: f32) {
        let t = &self.state.transform;
        let corners = [[x, y], [x + width, y], [x, y + height], [x + width, y + height]].map(|p| t.apply(p));
        let [x0, y0, x1, y1] = crate::geometry::bounding(&corners);
        self.rec.hits.push(Region { id, shape: HitShape::Rect([x0, y0, x1, y1]) });
    }

    /// 给编号是 `id` 的区域（[`Canvas::hit_circle`]、[`Canvas::hit_rect`]）起个名字：辅助技术把它当成画布里的一个子节点
    /// （图形，名字就是 `name`，边界是区域的外接矩形），读屏软件能一个个读到图表上的点、地图上的块
    pub fn hit_accessibility_name(&mut self, id: usize, name: impl Into<String>) {
        self.rec.labels.push((id, name.into()));
    }

    // ---- 内部 ----

    /// 一个距离场图元：中心在 `center`（变换之前的坐标），大小 `size`，可以再绕自己的中心转 `rotation`（cos, sin）
    #[inline]
    fn shape(
        &mut self,
        center: [f32; 2],
        size: [f32; 2],
        rotation: Option<[f32; 2]>,
        radii: [f32; 4],
        fill: &Fill,
        border: Option<(f32, &Fill)>,
        bounds: Rect,
    ) {
        let State { transform, clip, opacity } = self.state;
        let [cx, cy] = transform.apply(center);
        let mut quad = Quad::new(cx - size[0] * 0.5, cy - size[1] * 0.5, size[0], size[1]);
        quad.radii = radii;
        quad.transform = match rotation {
            None => transform.linear,
            Some([cos, sin]) => transform.compose(&Affine { linear: [cos, sin, -sin, cos], offset: [0.0, 0.0] }).linear,
        };
        quad.fill = match fill {
            Fill::Color(c) => *c,
            Fill::Gradient(_) => fill.color(),
        };
        quad.opacity = opacity;
        quad.clip = clip;
        let index = self.rec.quads.len() as u32;
        if let Fill::Gradient(gradient) = fill {
            self.rec.gradients.push(GradientUse { quad: index, gradient: gradient.clone(), bounds, transform, border: false });
        }
        if let Some((width, color)) = border {
            quad.border_width = width;
            quad.border_color = color.color();
            if let Fill::Gradient(gradient) = color {
                let own = Rect { x: 0.0, y: 0.0, width: size[0], height: size[1] };
                self.rec.gradients.push(GradientUse { quad: index, gradient: gradient.clone(), bounds: own, transform, border: true });
            }
        }
        self.rec.quads.push(quad);
        match self.rec.items.last_mut() {
            Some(Item::Quads(_, end)) if *end == index => *end += 1,
            _ => self.rec.items.push(Item::Quads(index, index + 1)),
        }
    }

    /// 一段线：两端各自的端点样式
    fn segment(&mut self, a: [f32; 2], b: [f32; 2], width: f32, caps: [LineCap; 2], fill: &Fill, bounds: Rect) {
        if width <= 0.0 {
            return;
        }
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = dx.hypot(dy);
        let half = width * 0.5;
        let extend = |cap: LineCap| if cap == LineCap::Butt { 0.0 } else { half };
        let (start, end) = (extend(caps[0]), extend(caps[1]));
        if length < 1e-6 {
            // 长度为 0：圆头画成一个点，方头画成一个方块，平头不画
            if caps[0] != LineCap::Butt {
                let r = if caps[0] == LineCap::Round { half } else { 0.0 };
                self.shape(a, [width, width], None, [r; 4], fill, None, bounds);
            }
            return;
        }
        let (cos, sin) = (dx / length, dy / length);
        let along = (length + end - start) * 0.5;
        let center = [a[0] + cos * along, a[1] + sin * along];
        let round = |cap: LineCap| if cap == LineCap::Round { half } else { 0.0 };
        let (r0, r1) = (round(caps[0]), round(caps[1]));
        // 沿线的方向是图元的 x：起点那头是左上、左下两个角
        self.shape(center, [length + start + end, width], Some([cos, sin]), [r0, r1, r1, r0], fill, None, bounds);
    }

    /// 虚线：按图案切成一段段，每段按端点样式收尾
    fn dashed(&mut self, a: [f32; 2], b: [f32; 2], stroke: Stroke, fill: &Fill, bounds: Rect) {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = dx.hypot(dy);
        let [on, off] = stroke.dash;
        let period = on + off;
        if length <= 0.0 || period <= 0.0 {
            return;
        }
        // 太密时（一条线超过一万段）当实线画
        if length / period > 10_000.0 {
            self.segment(a, b, stroke.width, [stroke.cap; 2], fill, bounds);
            return;
        }
        let (ux, uy) = (dx / length, dy / length);
        let mut s = -stroke.dash_offset.rem_euclid(period);
        while s < length {
            let (from, to) = (s.max(0.0), (s + on).min(length));
            if to > from {
                let p = |t: f32| [a[0] + ux * t, a[1] + uy * t];
                self.segment(p(from), p(to), stroke.width, [stroke.cap; 2], fill, bounds);
            }
            s += period;
        }
    }

    fn path(&mut self, path: &VectorPath, stroke: Option<Stroke>, fill: Fill) {
        let commands = path.shared_commands();
        if commands.is_empty() {
            return;
        }
        let mut hasher = DefaultHasher::new();
        for command in commands.iter() {
            hash_command(command, &mut hasher);
        }
        let [x0, y0, x1, y1] = path.bounds();
        let grow = stroke.map_or(0.0, |s| s.width * 0.5);
        let bounds = Rect { x: x0 - grow, y: y0 - grow, width: x1 - x0 + grow * 2.0, height: y1 - y0 + grow * 2.0 };
        let State { transform, clip, opacity } = self.state;
        let path = CanvasPath { commands: commands.clone(), hash: hasher.finish(), stroke, even_odd: path.is_even_odd() };
        self.rec.items.push(Item::Path(Box::new(PathItem { path, transform, fill, opacity, clip, bounds })));
    }

    /// x 单调、比物理像素列还密的折线：每列留第一个、最低、最高、最后一个点（按原来的顺序）
    fn thin<'a>(&self, points: &'a [[f32; 2]]) -> Cow<'a, [[f32; 2]]> {
        let [a, b, c, d] = self.state.transform.linear;
        // 有旋转、斜切时不抽稀
        if points.len() < 256 || b != 0.0 || c != 0.0 || a == 0.0 || d == 0.0 {
            return Cow::Borrowed(points);
        }
        let column = 1.0 / (self.scale * a.abs());
        let (first, last) = (points[0][0], points[points.len() - 1][0]);
        let columns = ((last - first).abs() / column).ceil() + 1.0;
        if (points.len() as f32) < columns * 6.0 || !points.windows(2).all(|w| w[1][0] >= w[0][0]) {
            return Cow::Borrowed(points);
        }
        let mut out = Vec::with_capacity(columns as usize * 4 + 4);
        let mut i = 0;
        while i < points.len() {
            let key = (points[i][0] / column).floor();
            let start = i;
            let (mut low, mut high) = (i, i);
            while i < points.len() && (points[i][0] / column).floor() == key {
                if points[i][1] < points[low][1] {
                    low = i;
                }
                if points[i][1] > points[high][1] {
                    high = i;
                }
                i += 1;
            }
            let mut keep = [start, low, high, i - 1];
            keep.sort_unstable();
            let mut previous = usize::MAX;
            for k in keep {
                if k != previous {
                    out.push(points[k]);
                    previous = k;
                }
            }
        }
        Cow::Owned(out)
    }
}

/// 线（宽 `width`）的外接矩形：渐变铺满它
fn segment_bounds(points: &[[f32; 2]], width: f32) -> Rect {
    let [x0, y0, x1, y1] = crate::geometry::bounding(points);
    let half = width * 0.5;
    Rect { x: x0 - half, y: y0 - half, width: x1 - x0 + width, height: y1 - y0 + width }
}

fn hash_command(command: &zeno::Command, hasher: &mut DefaultHasher) {
    use zeno::Command::*;
    std::mem::discriminant(command).hash(hasher);
    let points: &[zeno::Point] = match command {
        MoveTo(p) | LineTo(p) => std::slice::from_ref(p),
        QuadTo(c, p) => &[*c, *p],
        CurveTo(a, b, p) => &[*a, *b, *p],
        Close => &[],
    };
    for p in points {
        (p.x.to_bits(), p.y.to_bits()).hash(hasher);
    }
}

/// 画布上能点中的区域（[`Canvas::hit_circle`]、[`Canvas::hit_rect`] 登记的），每次画完更新。
/// 和信号一样是 `Copy` 的，可以放进好几个闭包；在绑定里读时，画布重画后绑定也跟着更新
///
/// ```no_run
/// # use aureole::prelude::*;
/// let points: Vec<[f32; 2]> = (0..2000).map(|i| [i as f32 * 0.3, (i as f32 * 0.05).sin() * 60.0 + 80.0]).collect();
/// let (hits, hovered) = (CanvasHits::new(), signal(None::<usize>));
/// canvas(move |cx| {
///     for (i, p) in points.iter().enumerate() {
///         let r = if hovered.get() == Some(i) { 5.0 } else { 2.5 };
///         cx.fill_circle(p[0], p[1], r, Color::hex(0x138a84));
///         cx.hit_circle(i, p[0], p[1], 6.0); // 比点大一些，好指
///     }
/// })
/// .size(600, 160)
/// .canvas_hits(hits)
/// .on_pointer_move(move |e: &PointerEvent| hovered.set(hits.at(e.position)));
/// ```
#[derive(Clone, Copy)]
pub struct CanvasHits(Signal<HitIndex>);

impl CanvasHits {
    /// 新建（属于当前作用域）
    pub fn new() -> Self {
        Self(reactive::signal(HitIndex::default()))
    }

    /// 点 `point`（元素自己的坐标）在哪个区域里；在几个区域里时取中心离它最近的（一样近时取后登记的）
    pub fn at(&self, point: [f32; 2]) -> Option<usize> {
        self.0.with(|index| index.at(point))
    }

    /// 同 [`CanvasHits::at`]，不跟踪：元素树问 [`crate::kinds::InputTarget::hit`] 时用这个，路由不该订阅它
    pub fn at_untracked(&self, point: [f32; 2]) -> Option<usize> {
        self.0.with_untracked(|index| index.at(point))
    }

    /// 登记了几个区域
    pub fn len(&self) -> usize {
        self.0.with(|index| index.regions.len())
    }

    /// 没有登记区域
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn set(&self, regions: Vec<Region>) {
        self.0.set(HitIndex::new(regions));
    }
}

impl Default for CanvasHits {
    fn default() -> Self {
        Self::new()
    }
}

/// 均匀网格：每个格子记下和它相交的区域，查一个点只看它所在的格子
#[derive(Default)]
pub(crate) struct HitIndex {
    regions: Vec<Region>,
    origin: [f32; 2],
    cell: f32,
    columns: usize,
    rows: usize,
    /// 第 k 个格子的区域是 `items[starts[k]..starts[k + 1]]`
    starts: Vec<u32>,
    items: Vec<u32>,
}

impl Region {
    fn bounds(&self) -> [f32; 4] {
        match self.shape {
            HitShape::Circle([x, y], r) => [x - r, y - r, x + r, y + r],
            HitShape::Rect(b) => b,
        }
    }

    /// 点在不在里面，在的话离中心多远
    fn distance(&self, [px, py]: [f32; 2]) -> Option<f32> {
        match self.shape {
            HitShape::Circle([x, y], r) => {
                let d = (px - x).hypot(py - y);
                (d <= r).then_some(d)
            }
            HitShape::Rect([x0, y0, x1, y1]) => {
                (px >= x0 && px <= x1 && py >= y0 && py <= y1).then(|| (px - (x0 + x1) * 0.5).hypot(py - (y0 + y1) * 0.5))
            }
        }
    }
}

impl HitIndex {
    fn new(regions: Vec<Region>) -> Self {
        if regions.is_empty() {
            return Self::default();
        }
        let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        let mut area = 0.0;
        for r in &regions {
            let [x0, y0, x1, y1] = r.bounds();
            b = [b[0].min(x0), b[1].min(y0), b[2].max(x1), b[3].max(y1)];
            area += (x1 - x0) * (y1 - y0);
        }
        let (width, height) = ((b[2] - b[0]).max(1.0), (b[3] - b[1]).max(1.0));
        // 格子边长：和区域的平均大小差不多（一个区域只落进几个格子），但格子总数不超过 256 x 256
        let average = (area / regions.len() as f32).sqrt().max(1.0);
        let cell = average.max(width / 256.0).max(height / 256.0) * 1.5;
        let (columns, rows) = ((width / cell).ceil() as usize + 1, (height / cell).ceil() as usize + 1);
        let range = |r: &Region| {
            let [x0, y0, x1, y1] = r.bounds();
            let c0 = ((x0 - b[0]) / cell).floor().max(0.0) as usize;
            let c1 = (((x1 - b[0]) / cell).floor().max(0.0) as usize).min(columns - 1);
            let r0 = ((y0 - b[1]) / cell).floor().max(0.0) as usize;
            let r1 = (((y1 - b[1]) / cell).floor().max(0.0) as usize).min(rows - 1);
            (c0..=c1.max(c0), r0..=r1.max(r0))
        };
        let mut counts = vec![0u32; columns * rows + 1];
        for r in &regions {
            let (cs, rs) = range(r);
            for row in rs {
                for col in cs.clone() {
                    counts[row * columns + col + 1] += 1;
                }
            }
        }
        for k in 1..counts.len() {
            counts[k] += counts[k - 1];
        }
        let mut fill = counts.clone();
        let mut items = vec![0u32; counts[columns * rows] as usize];
        for (i, r) in regions.iter().enumerate() {
            let (cs, rs) = range(r);
            for row in rs {
                for col in cs.clone() {
                    let slot = &mut fill[row * columns + col];
                    items[*slot as usize] = i as u32;
                    *slot += 1;
                }
            }
        }
        Self { regions, origin: [b[0], b[1]], cell, columns, rows, starts: counts, items }
    }

    fn at(&self, point: [f32; 2]) -> Option<usize> {
        if self.regions.is_empty() {
            return None;
        }
        let col = ((point[0] - self.origin[0]) / self.cell).floor();
        let row = ((point[1] - self.origin[1]) / self.cell).floor();
        if col < 0.0 || row < 0.0 || col as usize >= self.columns || row as usize >= self.rows {
            return None;
        }
        let k = row as usize * self.columns + col as usize;
        let mut best: Option<(f32, u32)> = None;
        for &i in &self.items[self.starts[k] as usize..self.starts[k + 1] as usize] {
            if let Some(d) = self.regions[i as usize].distance(point)
                && best.is_none_or(|(bd, bi)| d < bd || (d == bd && i > bi))
            {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| self.regions[i as usize].id)
    }
}
