//! 矢量路径：图标、自定义形状、折线图。
//!
//! 路径在主线程生成显示列表时用 zeno 栅格化成覆盖率，放进路径图集（和字形分开的一页），
//! 画的时候和其他图元一样是一个四边形，仍在同一次 draw call 里。栅格化按物理像素大小做，
//! 并对齐到物理像素，所以图标清晰；结果按路径、大小、样式缓存，静止的路径只栅格化一次。
//! 两条路径之间变形（`morph.rs`）时每帧重新栅格化，写进图集里预留的同一块（[`Slot`]），不每帧占新地方。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use scene::{AtlasKind, GlyphAtlas, Quad, max_atlas_size};
use zeno::{Command, Fill, Mask, PathData, Point, Transform};

use crate::geometry::Affine;
use crate::morph::{Morph, MorphPair, Shape};
use crate::geometry::Rect;
use crate::text::Ink;

/// 一条矢量路径，不可变，克隆很便宜。
///
/// ```
/// # use aureole::VectorPath;
/// // SVG 的路径语法，坐标在 24x24 的画布里（同 SVG 的 viewBox）
/// let check = VectorPath::svg("M4 12 L9 17 L20 6").view_box(0.0, 0.0, 24.0, 24.0);
/// let triangle = VectorPath::builder().move_to(0.0, 20.0).line_to(10.0, 0.0).line_to(20.0, 20.0).close().build();
/// ```
///
/// 没有指定画布时，路径坐标就是元素内容区里的逻辑像素
#[derive(Clone)]
pub struct VectorPath(Arc<PathInner>);

struct PathInner {
    /// 缓存栅格化结果用的标识，每条路径（包括改了画布、填充规则的副本）都不同
    id: u64,
    commands: Arc<[Command]>,
    view_box: Option<[f32; 4]>,
    even_odd: bool,
    /// 填充的外接矩形 `[x0, y0, x1, y1]`
    bounds: [f32; 4],
    /// 规整成三次贝塞尔的样子（变形时才算，算一次）
    shape: OnceLock<Shape>,
}

pub(crate) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl PartialEq for VectorPath {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for VectorPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorPath").field("commands", &self.0.commands.len()).field("view_box", &self.0.view_box).finish()
    }
}

impl VectorPath {
    fn from_commands(commands: impl Into<Arc<[Command]>>, view_box: Option<[f32; 4]>, even_odd: bool) -> Self {
        let commands: Arc<[Command]> = commands.into();
        let b = zeno::bounds(&commands[..], Fill::NonZero, None);
        let bounds = if commands.is_empty() { [0.0; 4] } else { [b.min.x, b.min.y, b.max.x, b.max.y] };
        Self(Arc::new(PathInner { id: next_id(), commands, view_box, even_odd, bounds, shape: OnceLock::new() }))
    }

    /// 路径的命令（共享，不复制）
    pub(crate) fn shared_commands(&self) -> Arc<[Command]> {
        self.0.commands.clone()
    }

    /// 规整成三次贝塞尔的样子，变形用（第一次用到时算，之后缓存在路径上）
    pub(crate) fn shape(&self) -> &Shape {
        self.0.shape.get_or_init(|| Shape::from_commands(&self.0.commands))
    }

    pub(crate) fn id(&self) -> u64 {
        self.0.id
    }

    pub(crate) fn view_box_rect(&self) -> Option<[f32; 4]> {
        self.0.view_box
    }

    pub(crate) fn is_even_odd(&self) -> bool {
        self.0.even_odd
    }

    /// 解析 SVG 的路径数据（`<path d="...">`），支持全部命令，包括圆弧。出错时保留出错之前的部分
    pub fn svg(data: &str) -> Self {
        Self::from_commands(data.commands().collect::<Vec<_>>(), None, false)
    }

    /// 逐段构造路径，见 [`VectorPathBuilder`]
    pub fn builder() -> VectorPathBuilder {
        VectorPathBuilder { commands: Vec::new() }
    }

    /// 依次连接各点的折线（不闭合），适合折线图
    pub fn polyline(points: impl IntoIterator<Item = [f32; 2]>) -> Self {
        let mut commands = Vec::new();
        for [x, y] in points {
            let point = Point::new(x, y);
            commands.push(if commands.is_empty() { Command::MoveTo(point) } else { Command::LineTo(point) });
        }
        Self::from_commands(commands, None, false)
    }

    /// 圆（四段三次贝塞尔）
    pub fn circle(cx: f32, cy: f32, r: f32) -> Self {
        // 四分之一圆弧的控制点距离
        let k = r * 0.552_284_8;
        Self::builder()
            .move_to(cx + r, cy)
            .curve_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r)
            .curve_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy)
            .curve_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r)
            .curve_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy)
            .close()
            .build()
    }

    /// 指定画布（同 SVG 的 viewBox）：画到元素上时，画布等比缩放、居中放进元素的内容区；
    /// 元素没指定大小时，画布大小就是元素大小
    pub fn view_box(&self, x: f32, y: f32, width: f32, height: f32) -> Self {
        Self::from_commands(self.0.commands.clone(), Some([x, y, width, height]), self.0.even_odd)
    }

    /// 奇偶填充规则（同 SVG 的 `fill-rule: evenodd`），默认是非零规则
    pub fn even_odd(&self, even_odd: bool) -> Self {
        Self::from_commands(self.0.commands.clone(), self.0.view_box, even_odd)
    }

    /// 填充的外接矩形 `[x0, y0, x1, y1]`，路径坐标
    pub fn bounds(&self) -> [f32; 4] {
        self.0.bounds
    }

    /// 总长度，路径坐标（曲线按万分之一大小的精度展平后测量，和 [`crate::Element::stroke_range`] 一致）
    pub fn length(&self) -> f32 {
        let [x0, y0, x1, y1] = self.0.bounds;
        let tolerance = ((x1 - x0).hypot(y1 - y0) * 1e-4).max(1e-4);
        flatten(&self.0.commands, tolerance).iter().flat_map(|points| points.windows(2).map(|w| distance(w[0], w[1]))).sum()
    }

    /// 画布：指定了就用它，否则是从原点到路径右下角
    fn canvas(&self) -> [f32; 4] {
        self.0.view_box.unwrap_or([0.0, 0.0, self.0.bounds[2].max(0.0), self.0.bounds[3].max(0.0)])
    }

    /// 把路径画成 `size` x `size` 的 RGBA 位图：纯色 `rgb`，覆盖率放在透明度里，画布等比缩放、居中。
    /// 托盘图标这类要交给系统的小图用它；`stroke` 是 `None` 时填充。
    ///
    /// ```
    /// use aureole::VectorPath;
    /// let pixels = VectorPath::circle(5.0, 5.0, 5.0).to_rgba(None, 16, [255, 0, 0]);
    /// assert_eq!(pixels.len(), 16 * 16 * 4);
    /// assert_eq!(pixels[(8 * 16 + 8) * 4..][..4], [255, 0, 0, 255]);
    /// ```
    pub fn to_rgba(&self, stroke: Option<Stroke>, size: u32, rgb: [u8; 3]) -> Vec<u8> {
        let [vx, vy, vw, vh] = self.canvas();
        let fit = if vw > 0.0 && vh > 0.0 { size as f32 / vw.max(vh) } else { 1.0 };
        let offset = [(size as f32 - vw * fit) * 0.5 - vx * fit, (size as f32 - vh * fit) * 0.5 - vy * fit];
        let mut mask = Mask::new(&self.0.commands[..]);
        mask.size(size, size)
            .transform(Some(Transform::scale(fit, fit).then_translate(offset[0], offset[1])));
        let mut dashes = [0.0; 2];
        match stroke {
            Some(s) => {
                mask.style(s.zeno(&mut dashes));
            }
            None => {
                mask.style(if self.0.even_odd { Fill::EvenOdd } else { Fill::NonZero });
            }
        }
        let (coverage, _) = mask.render();
        coverage.iter().flat_map(|&a| [rgb[0], rgb[1], rgb[2], a]).collect()
    }
}

/// 逐段构造路径，坐标在路径自己的画布里
pub struct VectorPathBuilder {
    commands: Vec<Command>,
}

impl VectorPathBuilder {
    /// 开始一段新的子路径，起点 (x, y)
    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.commands.push(Command::MoveTo(Point::new(x, y)));
        self
    }

    /// 直线连到 (x, y)
    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.commands.push(Command::LineTo(Point::new(x, y)));
        self
    }

    /// 二次贝塞尔：控制点 (cx, cy)，终点 (x, y)
    pub fn quadratic_to(mut self, cx: f32, cy: f32, x: f32, y: f32) -> Self {
        self.commands.push(Command::QuadTo(Point::new(cx, cy), Point::new(x, y)));
        self
    }

    /// 三次贝塞尔：两个控制点，终点 (x, y)
    pub fn curve_to(mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> Self {
        self.commands.push(Command::CurveTo(Point::new(c1x, c1y), Point::new(c2x, c2y), Point::new(x, y)));
        self
    }

    /// 闭合当前子路径
    pub fn close(mut self) -> Self {
        self.commands.push(Command::Close);
        self
    }

    /// 构造完成，得到路径
    pub fn build(self) -> VectorPath {
        VectorPath::from_commands(self.commands, None, false)
    }
}

/// 线条端点的样式
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineCap {
    /// 平头，正好停在端点（默认）
    Butt,
    /// 圆头，超出端点半个线宽
    Round,
    /// 方头，超出端点半个线宽
    Square,
}

/// 线条转角的样式
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineJoin {
    /// 尖角（默认）
    Miter,
    /// 圆角
    Round,
    /// 斜切角
    Bevel,
}

/// 描边：画路径的轮廓线而不是填充。宽度是路径坐标里的（和 SVG 的 `stroke-width` 一样随画布缩放）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    /// 线宽，路径坐标
    pub width: f32,
    /// 端点样式
    pub cap: LineCap,
    /// 转角样式
    pub join: LineJoin,
    /// 虚线：每一段画多长、空多长（路径坐标），有一个不大于 0 就是实线。每一段都按端点样式收尾
    pub dash: [f32; 2],
    /// 虚线从图案的哪里开始（路径坐标），改它可以让虚线沿路径流动
    pub dash_offset: f32,
}

impl Stroke {
    /// 指定线宽，平头、尖角
    pub const fn new(width: f32) -> Self {
        Self { width, cap: LineCap::Butt, join: LineJoin::Miter, dash: [0.0; 2], dash_offset: 0.0 }
    }

    /// 虚线：画 `on`、空 `off`（路径坐标）
    pub const fn dash(self, on: f32, off: f32) -> Self {
        Self { dash: [on, off], ..self }
    }

    /// 虚线从图案的哪里开始（路径坐标）
    pub const fn dash_offset(self, offset: f32) -> Self {
        Self { dash_offset: offset, ..self }
    }

    /// 是虚线
    #[inline]
    pub(crate) fn is_dashed(&self) -> bool {
        self.dash[0] > 0.0 && self.dash[1] > 0.0
    }

    /// 缓存键里的样式（位模式）
    fn key(&self) -> StrokeKey {
        let dash = if self.is_dashed() { [self.dash[0], self.dash[1], self.dash_offset].map(f32::to_bits) } else { [0; 3] };
        (self.width.to_bits(), self.cap, self.join, dash)
    }

    /// 交给 zeno 的描边样式；`dashes` 是虚线图案的存放处（zeno 借用它）
    fn zeno<'a>(&self, dashes: &'a mut [f32; 2]) -> zeno::Stroke<'a> {
        let mut zs = zeno::Stroke::new(self.width);
        zs.cap(match self.cap {
            LineCap::Butt => zeno::Cap::Butt,
            LineCap::Round => zeno::Cap::Round,
            LineCap::Square => zeno::Cap::Square,
        });
        zs.join(match self.join {
            LineJoin::Miter => zeno::Join::Miter,
            LineJoin::Round => zeno::Join::Round,
            LineJoin::Bevel => zeno::Join::Bevel,
        });
        if self.is_dashed() {
            *dashes = self.dash;
            zs.dash(&dashes[..], self.dash_offset);
        }
        zs
    }

    /// 圆头、圆角（线性图标常用）
    pub const fn round(self) -> Self {
        Self { cap: LineCap::Round, join: LineJoin::Round, ..self }
    }

    /// 端点样式
    pub const fn cap(self, cap: LineCap) -> Self {
        Self { cap, ..self }
    }

    /// 转角样式
    pub const fn join(self, join: LineJoin) -> Self {
        Self { join, ..self }
    }
}

/// 元素上的路径内容
#[derive(Default)]
pub(crate) struct PathBlock {
    pub path: Option<VectorPath>,
    pub stroke: Option<Stroke>,
    /// 正在从上一条路径变成 `path`（[`crate::Property::Path`] 的动画）
    pub morph: Option<Box<Morph>>,
}

impl PathBlock {
    /// 没指定大小时的大小：画布大小（描边时加上线宽）
    pub fn intrinsic(&self) -> [f32; 2] {
        let Some(path) = &self.path else { return [0.0; 2] };
        let [_, _, w, h] = path.canvas();
        let extra = match (self.stroke, path.0.view_box) {
            (Some(stroke), None) => stroke.width * 0.5,
            _ => 0.0,
        };
        [w + extra, h + extra]
    }
}

/// 描边样式的缓存键：线宽、端点、转角、虚线（位模式）
type StrokeKey = (u32, LineCap, LineJoin, [u32; 3]);

/// 栅格化结果的缓存键
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PathKey {
    id: u64,
    /// 路径坐标 -> 物理像素的缩放（位模式）
    scale: u32,
    stroke: Option<StrokeKey>,
    trim: [u32; 2],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PathEntry {
    pub uv: [f32; 4],
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 图集里为变形中的路径预留的一块：每帧把新的样子写进同一块，变形结束后还回去给下一次用
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// 预留时路径页是第几代：路径页扩大或清空后作废
    epoch: u32,
}

/// 路径栅格化结果的缓存；路径图集清空时一起清
#[derive(Default)]
pub(crate) struct PathCache {
    /// `None` 表示画出来是空的（或者太大放不进图集）
    entries: HashMap<PathKey, Option<PathEntry>>,
    /// 变形结束后还回来的预留块
    free: Vec<Slot>,
    /// 路径页清空的次数：之前预留的块都作废
    epoch: u32,
    /// 变形配对的缓存：（从哪条路径，到哪条路径）
    pairs: HashMap<(u64, u64), Arc<MorphPair>>,
    /// SVG 文件里的路径的栅格化结果：（路径的标识，文档到物理像素的两个轴的缩放，位模式）
    #[cfg(feature = "svg")]
    svg: HashMap<(u64, [u32; 2]), Option<PathEntry>>,
}

impl PathCache {
    pub fn clear(&mut self) {
        self.entries.clear();
        #[cfg(feature = "svg")]
        self.svg.clear();
        self.free.clear();
        self.epoch += 1;
    }

    /// 从 `from` 变到 `to` 的配对（缓存，同一对路径只配一次）
    pub fn pair(&mut self, from: &VectorPath, to: &VectorPath) -> Arc<MorphPair> {
        if let Some(pair) = self.pairs.get(&(from.id(), to.id())) {
            return pair.clone();
        }
        if self.pairs.len() >= crate::morph::PAIR_CACHE {
            self.pairs.clear();
        }
        let pair = Arc::new(crate::morph::pair(from.shape(), from.view_box_rect(), to.shape(), to.view_box_rect(), to.is_even_odd()));
        self.pairs.insert((from.id(), to.id()), pair.clone());
        pair
    }

    /// 变形结束：预留块还回来
    pub fn release(&mut self, slot: Option<Slot>) {
        if let Some(slot) = slot.filter(|s| s.epoch == self.epoch) {
            self.free.push(slot);
        }
    }
}

/// 画布上的一条路径：展平前的命令、内容的哈希（缓存键）、描边或填充规则
pub(crate) struct CanvasPath {
    pub commands: Arc<[Command]>,
    pub hash: u64,
    pub stroke: Option<Stroke>,
    pub even_odd: bool,
}

/// 画布为一条路径预留的一块路径图集，和上次写进去的内容
#[derive(Default)]
pub(crate) struct CanvasRaster {
    key: u64,
    pub slot: Option<Slot>,
    entry: Option<PathEntry>,
}

/// 栅格化时需要的东西，由 [`crate::text::TextSystem`] 借出
pub(crate) struct Raster<'a> {
    pub scale: f32,
    pub atlas: &'a mut GlyphAtlas,
    pub cache: &'a mut PathCache,
    /// 图集放不下时记下，绘制完扩大或清空图集再重画
    pub full: &'a mut Option<AtlasKind>,
}

impl Raster<'_> {
    /// 为元素的路径生成图元：画布等比缩放、居中放进内容区。`world` 是元素的累积变换，
    /// `trim` 是只画描边的哪一段（按每条子路径的长度比例）
    pub fn push(&mut self, block: &mut PathBlock, content: Rect, world: &Affine, ink: Ink, trim: [f32; 2], quads: &mut Vec<Quad>) {
        if let Some(morph) = block.morph.as_deref_mut() {
            self.push_morph(morph, block.stroke, content, world, ink, trim, quads);
            return;
        }
        let Some(path) = &block.path else { return };
        let [vx, vy, vw, vh] = path.canvas();
        if vw <= 0.0 || vh <= 0.0 || content.width <= 0.0 || content.height <= 0.0 {
            return;
        }
        // 画布 -> 布局坐标：有画布时等比缩放、居中；没有时路径坐标就是内容区里的逻辑像素
        let (fit, origin) = match path.0.view_box {
            Some(_) => {
                let fit = (content.width / vw).min(content.height / vh);
                (fit, [content.x + (content.width - vw * fit) * 0.5 - vx * fit, content.y + (content.height - vh * fit) * 0.5 - vy * fit])
            }
            None => (1.0, [content.x, content.y]),
        };
        let trim = match block.stroke {
            Some(_) => [trim[0].clamp(0.0, 1.0), trim[1].clamp(0.0, 1.0)],
            None => [0.0, 1.0],
        };
        if trim[1] <= trim[0] {
            return;
        }
        let scale = fit * self.scale;
        let key = PathKey {
            id: path.0.id,
            scale: scale.to_bits(),
            stroke: block.stroke.map(|s| s.key()),
            trim: trim.map(f32::to_bits),
        };
        let Some(entry) = self.entry(key, path, block.stroke, scale, trim) else { return };
        quads.push(self.quad(entry, origin, world, ink));
    }

    /// 栅格化的结果放到布局坐标的 `origin`（路径坐标原点所在的位置），再经过元素的变换
    fn quad(&self, entry: PathEntry, origin: [f32; 2], world: &Affine, ink: Ink) -> Quad {
        let (w, h) = (entry.width as f32 / self.scale, entry.height as f32 / self.scale);
        let mut quad = if world.is_translation() {
            // 路径原点对齐到物理像素，栅格化的结果原样贴上去才清晰
            let [x, y] = world.apply(origin);
            let (x, y) = ((x * self.scale).round(), (y * self.scale).round());
            Quad::path((x + entry.left as f32) / self.scale, (y + entry.top as f32) / self.scale, w, h, entry.uv, ink.color)
        } else {
            // 有缩放旋转：在布局坐标里放好，再整体变换（中心变换，矩阵绕中心）
            let x = origin[0] + entry.left as f32 / self.scale;
            let y = origin[1] + entry.top as f32 / self.scale;
            let [cx, cy] = world.apply([x + w * 0.5, y + h * 0.5]);
            let mut quad = Quad::path(cx - w * 0.5, cy - h * 0.5, w, h, entry.uv, ink.color);
            quad.transform = world.linear;
            quad
        };
        quad.opacity = ink.opacity;
        quad.gradient = ink.gradient;
        quad
    }

    /// 变形中的路径：按此刻的进度插值出形状，栅格化后写进为它预留的一块（每帧覆盖，不占新地方）
    fn push_morph(
        &mut self,
        morph: &mut Morph,
        stroke: Option<Stroke>,
        content: Rect,
        world: &Affine,
        ink: Ink,
        trim: [f32; 2],
        quads: &mut Vec<Quad>,
    ) {
        if content.width <= 0.0 || content.height <= 0.0 || self.full.is_some() {
            return;
        }
        let t = morph.progress;
        let (fit, origin) = match morph.pair.canvas(t) {
            Some([vx, vy, vw, vh]) if vw > 0.0 && vh > 0.0 => {
                let fit = (content.width / vw).min(content.height / vh);
                (fit, [content.x + (content.width - vw * fit) * 0.5 - vx * fit, content.y + (content.height - vh * fit) * 0.5 - vy * fit])
            }
            Some(_) => return,
            None => (1.0, [content.x, content.y]),
        };
        let trim = match stroke {
            Some(_) => [trim[0].clamp(0.0, 1.0), trim[1].clamp(0.0, 1.0)],
            None => [0.0, 1.0],
        };
        if trim[1] <= trim[0] {
            return;
        }
        let scale = fit * self.scale;
        let mut commands = morph.pair.commands(t);
        if trim != [0.0, 1.0] {
            commands = trim_commands(&commands, trim, 0.25 / scale);
        }
        let (pixels, p) = rasterize(&commands, stroke, morph_even_odd(morph), scale);
        if p.width == 0 || p.height == 0 {
            return;
        }
        let Some(entry) = self.write_slot(&mut morph.slot, &pixels, p) else { return };
        quads.push(self.quad(entry, origin, world, ink));
    }

    /// 把栅格化的结果写进预留的一块：作废了（路径页清空过）或者放不下时换一块，留些余量，
    /// 反复重画（变形、画布）时不用每次换。太大或者路径页满了返回 `None`
    fn write_slot(&mut self, slot: &mut Option<Slot>, pixels: &[u8], p: zeno::Placement) -> Option<PathEntry> {
        let (width, height) = (p.width, p.height);
        let epoch = self.cache.epoch;
        let fits = move |s: &Slot| s.epoch == epoch && s.width >= width && s.height >= height;
        if !slot.as_ref().is_some_and(fits) {
            self.cache.release(slot.take());
            let reuse = self.cache.free.iter().position(|s| fits(s) && s.width * s.height <= 4 * (width + 8) * (height + 8));
            *slot = match reuse {
                Some(i) => Some(self.cache.free.swap_remove(i)),
                None => {
                    let (w, h) = (width + width / 4 + 4, height + height / 4 + 4);
                    if w >= max_atlas_size() || h >= max_atlas_size() {
                        return None;
                    }
                    let Some([x, y]) = self.atlas.reserve(AtlasKind::Path, w, h) else {
                        *self.full = Some(AtlasKind::Path);
                        return None;
                    };
                    Some(Slot { x, y, width: w, height: h, epoch: self.cache.epoch })
                }
            };
        }
        let slot = slot.expect("刚预留好");
        // 连同右边和下边一圈空白一起写：上一次更大时留下的像素不会被双线性采样带进来
        let (w, h) = ((width + 1).min(slot.width), (height + 1).min(slot.height));
        let mut padded = vec![0u8; (w * h) as usize];
        for row in 0..height as usize {
            let src = &pixels[row * width as usize..(row + 1) * width as usize];
            padded[row * w as usize..row * w as usize + width as usize].copy_from_slice(src);
        }
        self.atlas.write(AtlasKind::Path, slot.x, slot.y, w, h, &padded);
        let size = self.atlas.usage(AtlasKind::Path).0 as f32;
        let uv = [slot.x as f32 / size, slot.y as f32 / size, (slot.x + width) as f32 / size, (slot.y + height) as f32 / size];
        Some(PathEntry { uv, left: p.left, top: p.top, width, height })
    }

    /// 画布上的一条路径（[`crate::Canvas`]）：栅格化后写进画布为它预留的一块，内容、大小、在物理像素里的
    /// 小数位置都没变时直接用上次的结果，所以不占全局的路径缓存，反复重画也不会把路径页填满。
    ///
    /// `scale` 是路径坐标 -> 逻辑像素的缩放（画布变换的缩放部分），`world` 把缩放后的路径坐标放进层坐标
    /// （画布变换去掉缩放后的旋转、平移，再接上元素的变换）。只有平移时按物理像素对齐，小数部分栅格化进去
    pub fn push_canvas(
        &mut self,
        cached: &mut CanvasRaster,
        path: &CanvasPath,
        scale: f32,
        world: &Affine,
        ink: Ink,
        quads: &mut Vec<Quad>,
    ) {
        let physical = scale * self.scale;
        // 只有平移：原点对齐到物理像素，小数部分（按 1/8 像素取整）交给栅格化
        let (snap, fraction) = if world.is_translation() {
            let [x, y] = world.apply([0.0, 0.0]);
            let (x, y) = (x * self.scale, y * self.scale);
            let fraction = [((x - x.round()) * 8.0).round() / 8.0, ((y - y.round()) * 8.0).round() / 8.0];
            (Some([x.round(), y.round()]), fraction)
        } else {
            (None, [0.0; 2])
        };
        let mut hasher = DefaultHasher::new();
        (path.hash, physical.to_bits(), path.stroke.map(|s| s.key()), path.even_odd, fraction.map(f32::to_bits)).hash(&mut hasher);
        let key = hasher.finish();
        let epoch = self.cache.epoch;
        let valid = cached.key == key && cached.slot.is_some_and(|s| s.epoch == epoch);
        if !valid {
            if self.full.is_some() {
                return;
            }
            let transform = Transform::scale(physical, physical).then_translate(fraction[0], fraction[1]);
            let (pixels, p) = match path.stroke.and_then(|s| rasterize_chunked(&path.commands, s, transform)) {
                Some(result) => result,
                None => {
                    let mut mask = Mask::new(&path.commands[..]);
                    mask.transform(Some(transform));
                    let mut dashes = [0.0; 2];
                    match path.stroke {
                        Some(s) => mask.style(s.zeno(&mut dashes)),
                        None => mask.style(if path.even_odd { Fill::EvenOdd } else { Fill::NonZero }),
                    };
                    mask.render()
                }
            };
            cached.entry = if p.width == 0 || p.height == 0 { None } else { self.write_slot(&mut cached.slot, &pixels, p) };
            if cached.entry.is_none() && self.full.is_some() {
                return;
            }
            cached.key = key;
        }
        let Some(entry) = cached.entry else { return };
        let (w, h) = (entry.width as f32 / self.scale, entry.height as f32 / self.scale);
        let mut quad = match snap {
            Some([x, y]) => Quad::path((x + entry.left as f32) / self.scale, (y + entry.top as f32) / self.scale, w, h, entry.uv, ink.color),
            None => {
                let (x, y) = (entry.left as f32 / self.scale, entry.top as f32 / self.scale);
                let [cx, cy] = world.apply([x + w * 0.5, y + h * 0.5]);
                let mut quad = Quad::path(cx - w * 0.5, cy - h * 0.5, w, h, entry.uv, ink.color);
                quad.transform = world.linear;
                quad
            }
        };
        quad.opacity = ink.opacity;
        quad.gradient = ink.gradient;
        quads.push(quad);
    }

    /// SVG 文件里的一条路径：和元素的路径同一个图集，按路径和缩放缓存（同一个文件的很多个元素只栅格化一次）。
    /// `render` 只在第一次用到这个缩放时调用。画出来是空的、太大放不下、图集满了时是 `None`（图集满了的这一遍重画时再放）
    #[cfg(feature = "svg")]
    pub fn svg_path(&mut self, id: u64, scale: [f32; 2], render: impl FnOnce() -> (Vec<u8>, zeno::Placement)) -> Option<PathEntry> {
        let key = (id, scale.map(f32::to_bits));
        if let Some(entry) = self.cache.svg.get(&key) {
            return *entry;
        }
        if self.full.is_some() {
            return None;
        }
        let (pixels, p) = render();
        let entry = if p.width == 0 || p.height == 0 || p.width >= max_atlas_size() || p.height >= max_atlas_size() {
            None
        } else {
            let Some(uv) = self.atlas.insert(AtlasKind::Path, p.width, p.height, &pixels) else {
                *self.full = Some(AtlasKind::Path);
                return None;
            };
            Some(PathEntry { uv, left: p.left, top: p.top, width: p.width, height: p.height })
        };
        self.cache.svg.insert(key, entry);
        entry
    }

    fn entry(&mut self, key: PathKey, path: &VectorPath, stroke: Option<Stroke>, scale: f32, trim: [f32; 2]) -> Option<PathEntry> {
        if let Some(entry) = self.cache.entries.get(&key) {
            return *entry;
        }
        if self.full.is_some() {
            return None;
        }
        let trimmed;
        let commands = if trim == [0.0, 1.0] {
            &path.0.commands[..]
        } else {
            // 截取时曲线按物理像素的精度展平成折线
            trimmed = trim_commands(&path.0.commands, trim, 0.25 / scale);
            &trimmed
        };
        let (pixels, p) = rasterize(commands, stroke, path.0.even_odd, scale);
        let entry = if p.width == 0 || p.height == 0 || p.width >= max_atlas_size() || p.height >= max_atlas_size() {
            None
        } else {
            let Some(uv) = self.atlas.insert(AtlasKind::Path, p.width, p.height, &pixels) else {
                *self.full = Some(AtlasKind::Path);
                return None;
            };
            Some(PathEntry { uv, left: p.left, top: p.top, width: p.width, height: p.height })
        };
        self.cache.entries.insert(key, entry);
        entry
    }
}

/// 栅格化成覆盖率：按 `scale` 缩放，描边或按填充规则填充
fn rasterize(commands: &[Command], stroke: Option<Stroke>, even_odd: bool, scale: f32) -> (Vec<u8>, zeno::Placement) {
    let mut mask = Mask::new(commands);
    mask.transform(Some(Transform::scale(scale, scale)));
    let mut dashes = [0.0; 2];
    match stroke {
        Some(s) => {
            mask.style(s.zeno(&mut dashes));
        }
        None => {
            mask.style(if even_odd { Fill::EvenOdd } else { Fill::NonZero });
        }
    }
    mask.render()
}

/// 长折线的描边一次栅格化的点数
const CHUNK: usize = 64;

/// 很长的折线描边（只有直线段、不闭合）：zeno 栅格化起伏很大、自己叠来叠去的描边很慢（六千个点要将近一秒），
/// 分成每段 `CHUNK` 个点分别栅格化（快几十倍），再按覆盖率取大合成一张。相邻两段多画一条共用的线段，
/// 段与段之间的转角照样是转角样式；中间断开处用平头，整条线的两端用原来的端点样式。共用的那一段两边的覆盖率一样，
/// 取大之后和整条一起栅格化的结果一样，半透明的线也不会叠深。虚线按走过的长度接上图案。不合适时返回 `None`
fn rasterize_chunked(commands: &[Command], stroke: Stroke, transform: Transform) -> Option<(Vec<u8>, zeno::Placement)> {
    if commands.len() <= CHUNK * 2 || commands.iter().any(|c| !matches!(c, Command::MoveTo(_) | Command::LineTo(_))) {
        return None;
    }
    // 拆成子路径
    let mut subpaths: Vec<Vec<Point>> = Vec::new();
    for command in commands {
        match *command {
            Command::MoveTo(p) => subpaths.push(vec![p]),
            Command::LineTo(p) => match subpaths.last_mut() {
                Some(points) => points.push(p),
                None => subpaths.push(vec![p]),
            },
            _ => unreachable!(),
        }
    }
    let cap = |c: LineCap| match c {
        LineCap::Butt => zeno::Cap::Butt,
        LineCap::Round => zeno::Cap::Round,
        LineCap::Square => zeno::Cap::Square,
    };
    let mut pieces: Vec<(Vec<u8>, zeno::Placement)> = Vec::new();
    for points in &subpaths {
        if points.len() < 2 {
            continue;
        }
        // 走到每个点的长度：虚线从这里接上图案
        let mut lengths = Vec::with_capacity(points.len());
        let mut walked = 0.0;
        for (i, &p) in points.iter().enumerate() {
            if i > 0 {
                walked += distance(points[i - 1], p);
            }
            lengths.push(walked);
        }
        let last_point = points.len() - 1;
        let mut start = 0;
        while start < last_point {
            // 自己的线段是 start..to；第一段之外往回多带一条线段，转角才完整
            let from = start.saturating_sub(1);
            let to = (start + CHUNK).min(last_point);
            let chunk: Vec<Command> =
                points[from..=to].iter().enumerate().map(|(i, &p)| if i == 0 { Command::MoveTo(p) } else { Command::LineTo(p) }).collect();
            let mut style = stroke;
            style.dash_offset = stroke.dash_offset + lengths[from];
            let mut dashes = [0.0; 2];
            let mut zs = style.zeno(&mut dashes);
            zs.caps(if from == 0 { cap(stroke.cap) } else { zeno::Cap::Butt }, if to == last_point { cap(stroke.cap) } else { zeno::Cap::Butt });
            let mut mask = Mask::new(&chunk[..]);
            mask.transform(Some(transform)).style(zs);
            let (pixels, p) = mask.render();
            if p.width > 0 && p.height > 0 {
                pieces.push((pixels, p));
            }
            start = to;
        }
    }
    if pieces.is_empty() {
        return Some((Vec::new(), zeno::Placement { left: 0, top: 0, width: 0, height: 0 }));
    }
    let left = pieces.iter().map(|(_, p)| p.left).min().unwrap_or(0);
    let top = pieces.iter().map(|(_, p)| p.top).min().unwrap_or(0);
    let right = pieces.iter().map(|(_, p)| p.left + p.width as i32).max().unwrap_or(0);
    let bottom = pieces.iter().map(|(_, p)| p.top + p.height as i32).max().unwrap_or(0);
    let (width, height) = ((right - left) as u32, (bottom - top) as u32);
    let mut out = vec![0u8; (width * height) as usize];
    for (pixels, p) in &pieces {
        let (dx, dy) = ((p.left - left) as usize, (p.top - top) as usize);
        for row in 0..p.height as usize {
            let src = &pixels[row * p.width as usize..(row + 1) * p.width as usize];
            let dst = &mut out[(dy + row) * width as usize + dx..][..p.width as usize];
            for (d, &s) in dst.iter_mut().zip(src) {
                *d = (*d).max(s);
            }
        }
    }
    Some((out, zeno::Placement { left, top, width, height }))
}

/// 变形用的填充规则：跟目标路径
fn morph_even_odd(morph: &Morph) -> bool {
    morph.even_odd
}

/// 只保留每条子路径从 `trim[0]` 到 `trim[1]`（长度比例）的部分。曲线先展平成折线，
/// `tolerance` 是展平的精度（路径坐标）
fn trim_commands(commands: &[Command], trim: [f32; 2], tolerance: f32) -> Vec<Command> {
    let mut out = Vec::new();
    for points in flatten(commands, tolerance) {
        let lengths: Vec<f32> = points.windows(2).map(|w| distance(w[0], w[1])).collect();
        let total: f32 = lengths.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let (start, end) = (trim[0] * total, trim[1] * total);
        let mut walked = 0.0;
        let mut started = false;
        for (i, &len) in lengths.iter().enumerate() {
            let (a, b) = (points[i], points[i + 1]);
            let (seg_start, seg_end) = (walked, walked + len);
            walked = seg_end;
            if seg_end < start || len <= 0.0 {
                continue;
            }
            if seg_start > end {
                break;
            }
            let from = lerp(a, b, ((start - seg_start) / len).clamp(0.0, 1.0));
            let to = lerp(a, b, ((end - seg_start) / len).clamp(0.0, 1.0));
            if !started {
                out.push(Command::MoveTo(from));
                started = true;
            }
            out.push(Command::LineTo(to));
        }
    }
    out
}

/// 展平成折线，每条子路径一组点；闭合的子路径最后回到起点
fn flatten(commands: &[Command], tolerance: f32) -> Vec<Vec<Point>> {
    let mut paths: Vec<Vec<Point>> = Vec::new();
    let mut current = Point::new(0.0, 0.0);
    let mut start = current;
    for &command in commands {
        match command {
            Command::MoveTo(p) => {
                paths.push(vec![p]);
                current = p;
                start = p;
            }
            Command::LineTo(p) => {
                push_point(&mut paths, current, p);
                current = p;
            }
            Command::QuadTo(c, p) => {
                let n = segments([current, c, p].windows(2).map(|w| distance(w[0], w[1])).sum(), tolerance);
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let q = lerp(lerp(current, c, t), lerp(c, p, t), t);
                    push_point(&mut paths, current, q);
                }
                current = p;
            }
            Command::CurveTo(c1, c2, p) => {
                let n = segments([current, c1, c2, p].windows(2).map(|w| distance(w[0], w[1])).sum(), tolerance);
                let from = current;
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let (a, b, c) = (lerp(from, c1, t), lerp(c1, c2, t), lerp(c2, p, t));
                    let q = lerp(lerp(a, b, t), lerp(b, c, t), t);
                    push_point(&mut paths, from, q);
                }
                current = p;
            }
            Command::Close => {
                push_point(&mut paths, current, start);
                current = start;
            }
        }
    }
    paths
}

fn push_point(paths: &mut Vec<Vec<Point>>, current: Point, p: Point) {
    match paths.last_mut() {
        Some(points) => points.push(p),
        None => paths.push(vec![current, p]),
    }
}

/// 曲线展平的段数：按控制多边形的长度估计，保证每段的弦高误差在精度以内
fn segments(length: f32, tolerance: f32) -> usize {
    ((length / tolerance.max(1e-3)).sqrt().ceil() as usize).clamp(1, 256)
}

fn distance(a: Point, b: Point) -> f32 {
    ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt()
}

fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}
