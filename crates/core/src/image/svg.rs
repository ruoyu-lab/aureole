//! SVG 文件（`svg` 特性）：[`crate::image()`] 认得的一种来源。
//!
//! 在后台线程上用 usvg 解析：它把样式、`use`、基本形状、单位、文字都解开，剩下只有路径、位图和分组的一棵树。
//! 这里把那棵树转成框架自己的绘制数据 [`SvgDocument`]：每条路径带着到文档坐标的变换、填充规则或描边样式、颜色或渐变，
//! 不再依赖 usvg 的类型。文档不可变，同一个来源的元素、窗口共用一份，和解码好的图片留在同一个缓存里。
//! 怎么放进显示列表见 `svg_paint.rs`。
//!
//! 能用现成的东西表示的都用现成的：
//! - 路径栅格化进路径图集（zeno），渐变是显示列表的渐变（在 sRGB 编码上插值，和 SVG 的默认一样）；
//! - 一个矩形的裁剪路径是显示列表的裁剪区域，不离屏；别的裁剪路径和遮罩是分组的遮罩；
//! - 分组透明度是分组，只有一个图元时直接乘到图元上；
//! - 文字由 usvg 转成路径（字体来自系统和 [`crate::font::load`] 加载过的），位图解码成和图片一样的数据。
//!
//! 不支持的（滤镜、混合模式、图案填充、渐变的重复和焦点）记一条警告并跳过。

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use scene::{Color, ColorSpace, GradientShape, ImageData};
use usvg::fontdb;
use usvg::tiny_skia_path::{self, PathSegment};
use zeno::{Command, Point};

use super::pixels::{self, Decoded};
use crate::geometry::{Affine, Rect};
use crate::vector::next_id;

/// 代表"当前颜色"的颜色。SVG 的填充、描边可以写一个关键字，表示跟着 `color` 属性走（单色图标常这样写）。usvg 解析时就把它换成了
/// `color` 属性的值（没写时是黑色），而文字颜色要到画的时候才知道，所以给根元素一个样式 `color: 这个颜色`，转换时再认回来。
/// 真的写了这个颜色的文件也会跟着文字颜色走
const CURRENT_COLOR: usvg::Color = usvg::Color { red: 0xc7, green: 0x01, blue: 0xf6 };

/// 解析好的一个 SVG 文件
pub(crate) struct SvgDocument {
    /// 固有大小（`width`、`height`，没写时按 `viewBox`），逻辑像素
    pub size: [f32; 2],
    pub nodes: Vec<SvgNode>,
    /// 里面的位图：[`SvgBitmap::index`] 是这里的下标
    pub images: Vec<SvgImage>,
    /// 所有路径的外接矩形的面积之和（文档坐标）：决定栅格化时要不要降低分辨率（路径图集放得下）
    pub area: f32,
    /// 最大的路径的外接矩形最长的一边（文档坐标）
    pub extent: f32,
    /// 有东西画到画布外面：要裁剪（SVG 的根元素默认裁掉画布外面的）
    pub overflows: bool,
    /// 路径的条数（快照里显示）
    pub paths: usize,
    /// 解析结果大约占多少字节（图片缓存的预算）
    pub bytes: usize,
}

/// 文档里的一个节点
pub(crate) enum SvgNode {
    Path(SvgPath),
    Image(SvgBitmap),
    /// 要先画进离屏纹理的一组
    Group(SvgGroup),
    /// 只在一个矩形里可见的一组，不离屏
    Clip(SvgClip),
}

/// 一条路径（填充和描边算两条）
pub(crate) struct SvgPath {
    /// 栅格化缓存的键
    pub id: u64,
    pub commands: Arc<[Command]>,
    pub style: PathStyle,
    /// 路径坐标 -> 文档坐标
    pub transform: Affine,
    pub paint: SvgPaint,
    pub opacity: f32,
}

/// 路径怎么变成覆盖率
pub(crate) enum PathStyle {
    Fill { even_odd: bool },
    Stroke(SvgStroke),
}

/// 描边：线宽在路径坐标里，先描边再经过变换（不等比的变换下线宽也跟着变形）
pub(crate) struct SvgStroke {
    pub width: f32,
    pub miter_limit: f32,
    pub cap: zeno::Cap,
    pub join: zeno::Join,
    /// 虚线的图案（画多长、空多长交替），可以不止一对
    pub dash: Option<Box<[f32]>>,
    pub dash_offset: f32,
}

/// 路径用什么颜色
pub(crate) enum SvgPaint {
    Color(Color),
    /// 当前颜色（见 [`CURRENT_COLOR`]）：元素继承的文字颜色
    Current,
    Gradient(Arc<SvgGradient>),
}

/// 一个渐变：画的时候接上到层坐标的变换，登记进显示列表
pub(crate) struct SvgGradient {
    pub shape: GradientShape,
    /// 路径坐标 -> 渐变坐标（线性渐变的位置是 x，径向是到原点的距离）
    pub to_gradient: Affine,
    /// 位置和 sRGB 编码的预乘颜色
    pub stops: Box<[(f32, [f32; 4])]>,
}

/// 一张位图：放在（`transform` 之前的坐标里）从原点起、`size` 大小的矩形里
pub(crate) struct SvgBitmap {
    pub index: usize,
    pub size: [f32; 2],
    /// 位图坐标 -> 文档坐标
    pub transform: Affine,
    pub opacity: f32,
}

/// 文档里的一张位图的像素
pub(crate) struct SvgImage {
    /// 在每棵元素树的图片资源里的键
    pub id: u64,
    pub data: Arc<ImageData>,
}

/// 先画进离屏纹理，再按透明度、遮罩合成的一组
pub(crate) struct SvgGroup {
    /// 文档里的第几组：和元素一起组成分组缓存的键
    pub index: u32,
    pub opacity: f32,
    pub children: Vec<SvgNode>,
    pub matte: Option<SvgMatte>,
}

/// 分组的遮罩：裁剪路径（白色的形状，按透明度）或者 `<mask>` 的内容（按明度或透明度）
pub(crate) struct SvgMatte {
    pub nodes: Vec<SvgNode>,
    pub luminance: bool,
}

/// 只在一个矩形（`transform` 之前的坐标）里可见的一组
pub(crate) struct SvgClip {
    pub rect: Rect,
    /// 矩形所在的坐标 -> 文档坐标
    pub transform: Affine,
    pub children: Vec<SvgNode>,
}

impl SvgPath {
    /// 栅格化成覆盖率：路径坐标经过 `transform`，再按 `scale` 缩放到物理像素
    pub fn rasterize(&self, scale: [f32; 2]) -> (Vec<u8>, zeno::Placement) {
        let t = Affine { linear: [scale[0], 0.0, 0.0, scale[1]], offset: [0.0, 0.0] }.compose(&self.transform);
        let [xx, xy, yx, yy] = t.linear;
        let mut mask = zeno::Mask::new(&self.commands[..]);
        mask.transform(Some(zeno::Transform::new(xx, xy, yx, yy, t.offset[0], t.offset[1])));
        match &self.style {
            PathStyle::Fill { even_odd } => {
                mask.style(if *even_odd { zeno::Fill::EvenOdd } else { zeno::Fill::NonZero });
            }
            PathStyle::Stroke(s) => {
                let mut stroke = zeno::Stroke::new(s.width);
                stroke.cap(s.cap).join(s.join).miter_limit(s.miter_limit);
                if let Some(dash) = &s.dash {
                    stroke.dash(dash, s.dash_offset);
                }
                mask.style(stroke);
            }
        }
        mask.render()
    }
}

/// 解析一个 SVG 文件（`bytes` 可以是 gzip 压缩的）。`directory` 是文件所在的目录：`<image>` 按它找相对路径，
/// 内存里的数据没有目录，只认内嵌的位图。`cap` 是内嵌的动图解码时的内存上限（只用第一帧），`source` 是记警告时的来源
pub(crate) fn parse(bytes: &[u8], directory: Option<&Path>, cap: usize, source: &dyn fmt::Debug) -> Result<SvgDocument, String> {
    let image_href_resolver = match directory {
        Some(_) => usvg::ImageHrefResolver::default(),
        None => usvg::ImageHrefResolver { resolve_string: Box::new(|_, _| None), ..Default::default() },
    };
    let usvg::Color { red, green, blue } = CURRENT_COLOR;
    let options = usvg::Options {
        resources_dir: directory.map(Path::to_path_buf),
        style_sheet: Some(format!("svg {{ color: #{red:02x}{green:02x}{blue:02x} }}")),
        font_resolver: font_resolver(),
        image_href_resolver,
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|e| format!("SVG 解析失败：{e}"))?;
    let size = [tree.size().width(), tree.size().height()];
    let mut convert = Convert { size, cap, images: Vec::new(), groups: 0, area: 0.0, extent: 0.0, overflows: false, paths: 0, bytes: 0, skipped: Vec::new() };
    let mut nodes = Vec::new();
    convert.children(tree.root(), Affine::IDENTITY, Mode::Paint, &mut nodes);
    for what in &convert.skipped {
        tracing::warn!(source = ?source, "SVG 里的{what}不支持，跳过");
    }
    let Convert { images, area, extent, overflows, paths, bytes, .. } = convert;
    Ok(SvgDocument { size, nodes, images, area, extent, overflows, paths, bytes })
}

/// 文字用的字体：系统字体（解析第一个有文字的文件时才扫描）和 [`crate::font::load`] 加载过的字体，所有文件共用一份
fn fonts() -> Arc<fontdb::Database> {
    static FONTS: Mutex<Option<(usize, Arc<fontdb::Database>)>> = Mutex::new(None);
    let mut fonts = FONTS.lock().unwrap_or_else(PoisonError::into_inner);
    let (seen, database) = fonts.get_or_insert_with(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        (0, Arc::new(database))
    });
    let count = crate::font::count();
    if *seen < count {
        let database = Arc::make_mut(database);
        for font in crate::font::loaded_since(*seen) {
            database.load_font_source(fontdb::Source::Binary(Arc::new(font)));
        }
        *seen = count;
    }
    database.clone()
}

/// 找字体：第一次要字体时才把字体库换成 [`fonts`]（没有文字的文件不用扫描系统字体）；文件要的字体都没有时用字体库里的第一个，
/// 文字照样画出来，缺的字再按字符回退
fn font_resolver() -> usvg::FontResolver<'static> {
    let (select_font, select_fallback) = (usvg::FontResolver::default_font_selector(), usvg::FontResolver::default_fallback_selector());
    usvg::FontResolver {
        select_font: Box::new(move |font, database| {
            if database.is_empty() {
                *database = fonts();
            }
            select_font(font, database).or_else(|| database.faces().next().map(|face| face.id))
        }),
        select_fallback: Box::new(move |c, used, database| {
            if database.is_empty() {
                *database = fonts();
            }
            select_fallback(c, used, database)
        }),
    }
}

/// 转换一棵子树是在画内容，还是在画裁剪路径的形状
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Paint,
    /// 裁剪路径的形状：只看填充的几何，都是白色，不描边、不画位图
    ClipShape,
}

/// 转换时的状态
struct Convert {
    size: [f32; 2],
    cap: usize,
    images: Vec<SvgImage>,
    groups: u32,
    area: f32,
    extent: f32,
    overflows: bool,
    paths: usize,
    bytes: usize,
    /// 遇到过的不支持的东西：每种只警告一次
    skipped: Vec<&'static str>,
}

fn affine(t: usvg::Transform) -> Affine {
    Affine { linear: [t.sx, t.ky, t.kx, t.sy], offset: [t.tx, t.ty] }
}

fn commands(path: &tiny_skia_path::Path) -> Arc<[Command]> {
    let point = |p: tiny_skia_path::Point| Point::new(p.x, p.y);
    path.segments()
        .map(|segment| match segment {
            PathSegment::MoveTo(p) => Command::MoveTo(point(p)),
            PathSegment::LineTo(p) => Command::LineTo(point(p)),
            PathSegment::QuadTo(c, p) => Command::QuadTo(point(c), point(p)),
            PathSegment::CubicTo(a, b, p) => Command::CurveTo(point(a), point(b), point(p)),
            PathSegment::Close => Command::Close,
        })
        .collect()
}

/// 路径是一个和坐标轴对齐的矩形（四个角，相邻两个只有一个坐标不同）：`<rect>` 和 `M0 0 H24 V24 H0 Z` 都是
fn axis_aligned_rect(path: &tiny_skia_path::Path) -> Option<Rect> {
    let mut corners: Vec<tiny_skia_path::Point> = Vec::with_capacity(5);
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(p) if corners.is_empty() => corners.push(p),
            PathSegment::LineTo(p) if corners.len() < 5 => corners.push(p),
            PathSegment::Close => {}
            _ => return None,
        }
    }
    if corners.len() == 5 && corners[0] == corners[4] {
        corners.pop();
    }
    if corners.len() != 4 {
        return None;
    }
    let axis_aligned = (0..4).all(|i| {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        (a.x == b.x) != (a.y == b.y)
    });
    let bounds = path.bounds();
    axis_aligned.then_some(Rect { x: bounds.x(), y: bounds.y(), width: bounds.width(), height: bounds.height() })
}

/// 裁剪路径只有一个矩形（可能包在只有变换的分组里）：矩形和它到文档坐标的变换
fn clip_rect(root: &usvg::Group, transform: Affine) -> Option<(Rect, Affine)> {
    let [child] = root.children() else { return None };
    match child {
        usvg::Node::Path(path) if path.is_visible() && path.fill().is_some() => axis_aligned_rect(path.data()).map(|rect| (rect, transform)),
        usvg::Node::Group(group) if group.clip_path().is_none() && group.mask().is_none() => clip_rect(group, transform.compose(&affine(group.transform()))),
        _ => None,
    }
}

/// 把透明度直接乘到唯一的节点上（分组直接乘它的透明度）。不止一个节点时返回 `false`：重叠的地方要先合起来，得离屏
fn fold_opacity(nodes: &mut [SvgNode], opacity: f32) -> bool {
    let [node] = nodes else { return nodes.is_empty() };
    match node {
        SvgNode::Path(path) => path.opacity *= opacity,
        SvgNode::Image(image) => image.opacity *= opacity,
        SvgNode::Group(group) => group.opacity *= opacity,
        SvgNode::Clip(clip) => return fold_opacity(&mut clip.children, opacity),
    }
    true
}

/// 色标的颜色。色标写成当前颜色时按黑色算（色标在转换时就编码好了，那时还不知道文字颜色）
fn stop_color(stop: &usvg::Stop) -> Color {
    let color = stop.color();
    let usvg::Color { red, green, blue } = if color == CURRENT_COLOR { usvg::Color::black() } else { color };
    Color::rgb(red, green, blue).with_alpha(stop.opacity().get())
}

fn stroke_style(stroke: &usvg::Stroke) -> SvgStroke {
    SvgStroke {
        width: stroke.width().get(),
        miter_limit: stroke.miterlimit().get(),
        cap: match stroke.linecap() {
            usvg::LineCap::Butt => zeno::Cap::Butt,
            usvg::LineCap::Round => zeno::Cap::Round,
            usvg::LineCap::Square => zeno::Cap::Square,
        },
        join: match stroke.linejoin() {
            usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => zeno::Join::Miter,
            usvg::LineJoin::Round => zeno::Join::Round,
            usvg::LineJoin::Bevel => zeno::Join::Bevel,
        },
        dash: stroke.dasharray().map(Box::from),
        dash_offset: stroke.dashoffset(),
    }
}

/// 解码内嵌的位图：动图只用第一帧
fn bitmap(bytes: &[u8], cap: usize) -> Option<Arc<ImageData>> {
    Some(match pixels::decode_any(bytes, None, cap).ok()? {
        Decoded::Still(rgba, width, height) => Arc::new(pixels::process(rgba, width, height, None)),
        Decoded::Wide(rgba, width, height) => Arc::new(pixels::process_wide(rgba, width, height, None)),
        Decoded::Animated { frames, .. } => frames.frames.first()?.clone(),
        Decoded::Vector(_) => return None,
    })
}

impl Convert {
    fn skip(&mut self, what: &'static str) {
        if !self.skipped.contains(&what) {
            self.skipped.push(what);
        }
    }

    fn children(&mut self, parent: &usvg::Group, base: Affine, mode: Mode, out: &mut Vec<SvgNode>) {
        for child in parent.children() {
            match child {
                usvg::Node::Group(group) => self.group(group, base, mode, out),
                usvg::Node::Path(path) => self.path(path, base, mode, out),
                usvg::Node::Image(image) if mode == Mode::Paint => self.image(image, base, out),
                usvg::Node::Image(_) => {}
                // 文字已经由 usvg 转成了路径（彩色字形是位图）
                usvg::Node::Text(text) => self.group(text.flattened(), base, mode, out),
            }
        }
    }

    fn group(&mut self, group: &usvg::Group, base: Affine, mode: Mode, out: &mut Vec<SvgNode>) {
        let transform = base.compose(&affine(group.transform()));
        if !group.filters().is_empty() {
            self.skip("滤镜");
        }
        if group.blend_mode() != usvg::BlendMode::Normal {
            self.skip("混合模式");
        }
        let mut children = Vec::new();
        self.children(group, transform, mode, &mut children);
        if let Some(clip) = group.clip_path() {
            children = self.clipped(clip, transform, children);
        }
        if let Some(mask) = group.mask() {
            children = self.masked(mask, transform, children);
        }
        let opacity = group.opacity().get();
        if opacity < 1.0 && mode == Mode::Paint && !fold_opacity(&mut children, opacity) {
            children = vec![self.new_group(opacity, children, None)];
        }
        out.append(&mut children);
    }

    fn new_group(&mut self, opacity: f32, children: Vec<SvgNode>, matte: Option<SvgMatte>) -> SvgNode {
        self.groups += 1;
        SvgNode::Group(SvgGroup { index: self.groups, opacity, children, matte })
    }

    /// 只在裁剪路径里可见：一个矩形时用裁剪区域，别的形状用分组的遮罩（透明度）
    fn clipped(&mut self, clip: &usvg::ClipPath, base: Affine, children: Vec<SvgNode>) -> Vec<SvgNode> {
        if children.is_empty() {
            return children;
        }
        let transform = base.compose(&affine(clip.transform()));
        let node = match clip_rect(clip.root(), transform) {
            Some((rect, transform)) => SvgNode::Clip(SvgClip { rect, transform, children }),
            None => {
                let mut nodes = Vec::new();
                self.children(clip.root(), transform, Mode::ClipShape, &mut nodes);
                // 裁剪路径里什么都没有：什么都看不见
                if nodes.is_empty() {
                    return Vec::new();
                }
                self.new_group(1.0, children, Some(SvgMatte { nodes, luminance: false }))
            }
        };
        match clip.clip_path() {
            Some(outer) => self.clipped(outer, base, vec![node]),
            None => vec![node],
        }
    }

    /// 乘上遮罩：遮罩的内容只在它的矩形里有效，没有内容时什么都看不见
    fn masked(&mut self, mask: &usvg::Mask, base: Affine, children: Vec<SvgNode>) -> Vec<SvgNode> {
        let mut content = Vec::new();
        self.children(mask.root(), base, Mode::Paint, &mut content);
        if children.is_empty() || content.is_empty() {
            return Vec::new();
        }
        let r = mask.rect();
        let rect = Rect { x: r.x(), y: r.y(), width: r.width(), height: r.height() };
        let nodes = vec![SvgNode::Clip(SvgClip { rect, transform: base, children: content })];
        let node = self.new_group(1.0, children, Some(SvgMatte { nodes, luminance: mask.kind() == usvg::MaskType::Luminance }));
        match mask.mask() {
            Some(outer) => self.masked(outer, base, vec![node]),
            None => vec![node],
        }
    }

    /// 记下一个外接矩形（`transform` 之前的坐标）：决定栅格化的分辨率、要不要裁到画布里
    fn measure(&mut self, rect: usvg::Rect, transform: &Affine, raster: bool) {
        let (x0, y0, x1, y1) = (rect.left(), rect.top(), rect.right(), rect.bottom());
        let corners = [[x0, y0], [x1, y0], [x0, y1], [x1, y1]].map(|p| transform.apply(p));
        let [left, top, right, bottom] = crate::geometry::bounding(&corners);
        self.overflows |= left < -0.01 || top < -0.01 || right > self.size[0] + 0.01 || bottom > self.size[1] + 0.01;
        if raster {
            self.area += (right - left) * (bottom - top);
            self.extent = self.extent.max(right - left).max(bottom - top);
        }
    }

    fn path(&mut self, path: &usvg::Path, transform: Affine, mode: Mode, out: &mut Vec<SvgNode>) {
        if !path.is_visible() {
            return;
        }
        let commands = commands(path.data());
        let node = |style, paint, opacity| SvgPath { id: next_id(), commands: commands.clone(), style, transform, paint, opacity };
        let fill_style = |fill: &usvg::Fill| PathStyle::Fill { even_odd: fill.rule() == usvg::FillRule::EvenOdd };
        let (first, second) = if mode == Mode::ClipShape {
            let Some(fill) = path.fill() else { return };
            (Some(node(fill_style(fill), SvgPaint::Color(Color::WHITE), 1.0)), None)
        } else {
            let fill = path.fill().and_then(|fill| Some(node(fill_style(fill), self.paint(fill.paint())?, fill.opacity().get())));
            let stroke = path.stroke().and_then(|s| Some(node(PathStyle::Stroke(stroke_style(s)), self.paint(s.paint())?, s.opacity().get())));
            match path.paint_order() {
                usvg::PaintOrder::FillAndStroke => (fill, stroke),
                usvg::PaintOrder::StrokeAndFill => (stroke, fill),
            }
        };
        if first.is_none() && second.is_none() {
            return;
        }
        self.measure(path.stroke_bounding_box(), &transform, true);
        for path in [first, second].into_iter().flatten() {
            self.paths += 1;
            self.bytes += commands.len() * size_of::<Command>() + size_of::<SvgNode>();
            out.push(SvgNode::Path(path));
        }
    }

    fn paint(&mut self, paint: &usvg::Paint) -> Option<SvgPaint> {
        match paint {
            usvg::Paint::Color(color) if *color == CURRENT_COLOR => Some(SvgPaint::Current),
            usvg::Paint::Color(color) => Some(SvgPaint::Color(Color::rgb(color.red, color.green, color.blue))),
            usvg::Paint::LinearGradient(gradient) => {
                let [x, y] = [gradient.x2() - gradient.x1(), gradient.y2() - gradient.y1()];
                let length = x * x + y * y;
                // 两端重合：整个是最后一个色标的颜色（SVG 的规定）
                if length <= 0.0 {
                    return gradient.stops().last().map(|stop| SvgPaint::Color(stop_color(stop)));
                }
                let offset = -(gradient.x1() * x + gradient.y1() * y) / length;
                let line = Affine { linear: [x / length, 0.0, y / length, 0.0], offset: [offset, 0.0] };
                self.gradient(GradientShape::Linear, line, gradient)
            }
            usvg::Paint::RadialGradient(gradient) => {
                if gradient.fx() != gradient.cx() || gradient.fy() != gradient.cy() || gradient.fr().get() > 0.0 {
                    self.skip("径向渐变的焦点");
                }
                let r = gradient.r().get().max(1e-6);
                let circle = Affine { linear: [1.0 / r, 0.0, 0.0, 1.0 / r], offset: [-gradient.cx() / r, -gradient.cy() / r] };
                self.gradient(GradientShape::Radial, circle, gradient)
            }
            usvg::Paint::Pattern(_) => {
                self.skip("图案填充");
                None
            }
        }
    }

    /// `geometry` 是渐变所在的坐标 -> 渐变坐标；路径坐标先经过 `gradientTransform` 的逆变换到那里
    fn gradient(&mut self, shape: GradientShape, geometry: Affine, gradient: &usvg::BaseGradient) -> Option<SvgPaint> {
        if gradient.spread_method() != usvg::SpreadMethod::Pad {
            self.skip("渐变的重复");
        }
        let to_gradient = geometry.compose(&affine(gradient.transform()).inverse()?);
        let stops = gradient.stops().iter().map(|stop| (stop.offset().get(), ColorSpace::Srgb.encode(stop_color(stop)))).collect();
        Some(SvgPaint::Gradient(Arc::new(SvgGradient { shape, to_gradient, stops })))
    }

    fn image(&mut self, image: &usvg::Image, transform: Affine, out: &mut Vec<SvgNode>) {
        if !image.is_visible() {
            return;
        }
        let bytes = match image.kind() {
            usvg::ImageKind::SVG(tree) => return self.children(tree.root(), transform, Mode::Paint, out),
            usvg::ImageKind::JPEG(bytes) | usvg::ImageKind::PNG(bytes) | usvg::ImageKind::GIF(bytes) | usvg::ImageKind::WEBP(bytes) => bytes,
        };
        let Some(data) = bitmap(bytes, self.cap) else {
            self.skip("解码失败的位图");
            return;
        };
        let size = [image.size().width(), image.size().height()];
        self.measure(image.bounding_box(), &transform, false);
        self.bytes += data.bytes();
        self.images.push(SvgImage { id: next_id(), data });
        out.push(SvgNode::Image(SvgBitmap { index: self.images.len() - 1, size, transform, opacity: 1.0 }));
    }
}
