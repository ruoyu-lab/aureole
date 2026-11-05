//! 文字：parley 排版（字体回退、整形、断行、双向文本），swash 栅格化，
//! 字形放进图集，生成字形图元。
//!
//! 排版用逻辑像素；栅格化用物理像素（字号 × 窗口缩放）。没有缩放旋转时，
//! 字形对齐到物理像素，水平方向保留 4 档亚像素位置，既清晰又不会字距忽大忽小。

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, DefaultHasher, Hash, Hasher};
use std::mem;
use std::ops::Range;
use std::rc::Rc;

use parley::setting::Tag;
use parley::{
    Affinity, Alignment, AlignmentOptions, BoundingBox, Brush, Cluster, Cursor, FontContext, FontData, FontFamily, FontFamilyName, FontFeature,
    FontFeatures, FontStyle, FontVariation, FontVariations, FontWeight, GenericFamily, Layout, LayoutContext, LineHeight,
    OverflowWrap, PositionedLayoutItem, RangedBuilder, Selection, StyleProperty, WordBreak, YieldData,
};
use scene::{AtlasKind, AtlasUpdate, Color, GlyphAtlas, Quad};
use swash::FontRef;
use swash::scale::image::{Content, Image};
use swash::scale::outline::Outline;
use swash::scale::{Render, ScaleContext, Scaler, Source, StrikeWith};
use swash::zeno::{Angle, Format, Mask, Origin, Point, Transform, Vector, Verb};

use crate::font;
use crate::geometry::Affine;
use crate::geometry::Rect;
use crate::vector::{PathCache, Raster};

mod inline;

pub use inline::InlineAlign;
pub(crate) use inline::{InlineMetrics, InlineSlot, PLACEHOLDER};

/// 截断时加在末尾的省略号
const ELLIPSIS: &str = "…";
/// 等宽数字
static TABULAR: [FontFeature; 1] = [FontFeature::new(Tag::new(b"tnum"), 1)];

/// 水平亚像素档数
const SUBPIXEL_BINS: f32 = 4.0;

/// 字形的颜色和透明度（透明度对彩色字形也生效）
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Ink {
    pub color: Color,
    pub opacity: f32,
    /// 渐变填充（显示列表里的下标，0 表示用 `color`），彩色字形不用
    pub gradient: u32,
}

/// 文字在行内的水平对齐。对齐的范围是文字元素的内容区：要居中一行字，元素本身要比字宽
/// （设宽度、`full_width`，或者在纵向容器里默认拉伸）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextAlign {
    /// 行首（从左到右的文字是左边）
    #[default]
    Start,
    /// 居中
    Center,
    /// 行尾
    End,
    /// 两端对齐：拉开词间（中文是字间）的距离让每行排满，段落最后一行靠行首
    Justify,
}

impl TextAlign {
    fn parley(self) -> Alignment {
        match self {
            Self::Start => Alignment::Start,
            Self::Center => Alignment::Center,
            Self::End => Alignment::End,
            Self::Justify => Alignment::Justify,
        }
    }
}

/// 影响排版的样式。颜色不在这里：改颜色不需要重新排版
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextStyle {
    pub size: f32,
    pub weight: f32,
    /// 字号的倍数
    pub line_height: f32,
    pub underline: bool,
    pub strikethrough: bool,
    /// 字体族的编号（见 [`font::family_id`]），0 是系统界面字体
    pub family: u32,
    pub italic: bool,
    /// 字距，逻辑像素
    pub letter_spacing: f32,
    pub align: TextAlign,
    /// 等宽数字（OpenType 的 tnum 特性）
    pub tabular: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            size: 14.0,
            weight: 400.0,
            line_height: 1.4,
            underline: false,
            strikethrough: false,
            family: 0,
            italic: false,
            letter_spacing: 0.0,
            align: TextAlign::Start,
            tabular: false,
        }
    }
}

impl TextStyle {
    pub fn alignment(&self) -> Alignment {
        self.align.parley()
    }
}

/// 把样式交给排版：指定的字体族后面接上系统界面字体，指定的字体里没有的字（比如中文）从系统字体回退。
/// 自己加载的可变字体按字号取光学尺寸（`opsz` 轴）：不取时用的是字体的默认字形，常常是给大标题的那一档，
/// 笔画细到正文字号下连字符、横画几乎看不见
pub(crate) fn push_style(builder: &mut RangedBuilder<'_, SpanId>, style: &TextStyle) {
    let system = FontFamilyName::Generic(GenericFamily::SystemUi);
    let family = match font::family_name(style.family) {
        Some(name) => {
            let optical = FontVariation::new(Tag::new(b"opsz"), style.size);
            builder.push_default(StyleProperty::FontVariations(FontVariations::List(Cow::Owned(vec![optical]))));
            FontFamily::List(Cow::Owned(vec![FontFamilyName::Named(Cow::Owned(name.to_string())), system]))
        }
        None => FontFamily::Single(system),
    };
    builder.push_default(StyleProperty::FontFamily(family));
    builder.push_default(StyleProperty::FontSize(style.size));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(style.weight)));
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(style.line_height)));
    if style.italic {
        builder.push_default(StyleProperty::FontStyle(FontStyle::Italic));
    }
    if style.letter_spacing != 0.0 {
        builder.push_default(StyleProperty::LetterSpacing(style.letter_spacing));
    }
    if style.tabular {
        builder.push_default(StyleProperty::FontFeatures(FontFeatures::List(Cow::Borrowed(&TABULAR))));
    }
    if style.underline {
        builder.push_default(StyleProperty::Underline(true));
    }
    if style.strikethrough {
        builder.push_default(StyleProperty::Strikethrough(true));
    }
}

/// 排版里每段文字带的刷子：富文本里的第几段（从 1 起），0 是没有单独样式的部分（普通文字全是 0）。
/// 颜色、链接的悬停这些只影响绘制的东西按它查，改了不用重新排版
pub(crate) type SpanId = u32;

/// 行内代码的字体：等宽字体，按顺序找，系统里没有的跳过；最后是系统界面字体（中文从这里回退）
static MONOSPACE: [FontFamilyName<'static>; 6] = [
    FontFamilyName::Named(Cow::Borrowed("SF Mono")),
    FontFamilyName::Named(Cow::Borrowed("Menlo")),
    FontFamilyName::Named(Cow::Borrowed("Consolas")),
    FontFamilyName::Named(Cow::Borrowed("DejaVu Sans Mono")),
    FontFamilyName::Generic(GenericFamily::Monospace),
    FontFamilyName::Generic(GenericFamily::SystemUi),
];

/// 字符网格里明说要 emoji 样子的簇（带 U+FE0F）用的字体族
#[cfg(feature = "char-grid")]
static EMOJI: [FontFamilyName<'static>; 1] = [FontFamilyName::Generic(GenericFamily::Emoji)];

/// 行内代码的字号是整段的这么多倍：等宽字体看起来比正文大一点
const CODE_SCALE: f32 = 0.9;

/// 富文本里一段文字自己的排版样式，没设的跟着整段
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SpanStyle {
    pub size: Option<f32>,
    pub weight: Option<f32>,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    /// 行内代码：等宽字体、小一点的字号、淡淡的底色
    pub code: bool,
    /// 字体族的编号（见 [`font::family_id`]）
    pub family: Option<u32>,
    pub letter_spacing: Option<f32>,
}

/// 链接被点击时做什么：有处理器调用处理器，否则打开网址
#[derive(Clone)]
pub(crate) struct Link {
    pub url: Option<String>,
    pub on_click: Option<Rc<dyn Fn(())>>,
}

/// 富文本的一段
#[derive(Clone)]
pub(crate) struct RichSpan {
    pub text: String,
    pub style: SpanStyle,
    /// 自己的颜色，改了只重画；`None` 跟着整段（链接用默认的强调色）
    pub color: Option<Color>,
    pub link: Option<Link>,
}

/// 富文本：各段的文字、样式、颜色和链接。普通文字没有这一份，不多占内存、不多花时间
pub(crate) struct Rich {
    pub spans: Vec<RichSpan>,
    /// 每段在全文里的字节范围
    ranges: Vec<Range<usize>>,
    /// 链接是第几段（按出现的顺序）
    links: Vec<usize>,
    /// 指针下、按着、有键盘焦点的链接（链接的序号）
    pub hovered: Option<usize>,
    pub pressed: Option<usize>,
    pub focused: Option<usize>,
    /// 行内元素（见 `text/inline.rs`）；没有时什么都不多做
    inlines: inline::Inlines,
}

/// 链接的颜色（默认的，见 `defaults.rs`）：平时、悬停、按下
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LinkColors {
    pub normal: Color,
    pub hover: Color,
    pub pressed: Color,
}

/// 绘制时一段文字的颜色和要不要加下划线（悬停的链接）
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct SpanInk {
    pub color: Option<Color>,
    pub underline: bool,
}

/// 富文本的样式按段交给排版：每段带上自己的刷子（段号），设了的样式只作用在这一段
fn push_spans(builder: &mut RangedBuilder<'_, SpanId>, style: &TextStyle, rich: &Rich, limit: usize) {
    for (i, (span, range)) in rich.spans.iter().zip(&rich.ranges).enumerate() {
        let range = range.start.min(limit)..range.end.min(limit);
        if range.is_empty() {
            continue;
        }
        builder.push(StyleProperty::Brush(i as SpanId + 1), range.clone());
        let s = &span.style;
        let size = s.size.unwrap_or(if s.code { style.size * CODE_SCALE } else { style.size });
        if size != style.size {
            builder.push(StyleProperty::FontSize(size), range.clone());
        }
        // 指定的字体（自己的或者整段的；行内代码用系统的等宽字体）字号不同时，光学尺寸跟着这一段的字号
        let family = if s.code && s.family.is_none() { None } else { Some(s.family.unwrap_or(style.family)) };
        if let Some(family) = family
            && (size != style.size || s.family.is_some())
            && font::family_name(family).is_some()
        {
            let optical = FontVariation::new(Tag::new(b"opsz"), size);
            builder.push(StyleProperty::FontVariations(FontVariations::List(Cow::Owned(vec![optical]))), range.clone());
        }
        if let Some(name) = s.family.and_then(font::family_name) {
            let system = FontFamilyName::Generic(GenericFamily::SystemUi);
            let list = vec![FontFamilyName::Named(Cow::Owned(name.to_string())), system];
            builder.push(StyleProperty::FontFamily(FontFamily::List(Cow::Owned(list))), range.clone());
        } else if s.code {
            builder.push(StyleProperty::FontFamily(FontFamily::List(Cow::Borrowed(&MONOSPACE))), range.clone());
        }
        if let Some(weight) = s.weight {
            builder.push(StyleProperty::FontWeight(FontWeight::new(weight)), range.clone());
        }
        if s.italic {
            builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone());
        }
        if let Some(spacing) = s.letter_spacing {
            builder.push(StyleProperty::LetterSpacing(spacing), range.clone());
        }
        if s.underline {
            builder.push(StyleProperty::Underline(true), range.clone());
        }
        if s.strikethrough {
            builder.push(StyleProperty::Strikethrough(true), range.clone());
        }
        // 链接和行内代码尽量不在中间断行（中文也整个挪到下一行），一行放不下时才断开
        if span.link.is_some() || s.code {
            builder.push(StyleProperty::WordBreak(WordBreak::KeepAll), range.clone());
            builder.push(StyleProperty::OverflowWrap(OverflowWrap::BreakWord), range);
        }
    }
}

impl Rich {
    pub fn new(spans: Vec<RichSpan>) -> Self {
        let links = spans.iter().enumerate().filter(|(_, s)| s.link.is_some()).map(|(i, _)| i).collect();
        let mut rich = Self { spans, ranges: Vec::new(), links, hovered: None, pressed: None, focused: None, inlines: Default::default() };
        rich.restart();
        rich
    }

    /// 重新累加每段的范围，返回全文
    fn restart(&mut self) -> String {
        let mut content = String::with_capacity(self.spans.iter().map(|s| s.text.len()).sum());
        self.ranges.clear();
        for span in &self.spans {
            let start = content.len();
            content.push_str(&span.text);
            self.ranges.push(start..content.len());
        }
        self.refresh_plain(&content);
        content
    }

    /// 链接有几个
    pub fn links(&self) -> usize {
        self.links.len()
    }

    /// 第 `k` 个链接是第几段
    pub fn link_span(&self, k: usize) -> Option<usize> {
        self.links.get(k).copied()
    }

    /// 第 `k` 个链接的文字和点击时做什么
    pub fn link(&self, k: usize) -> Option<(&str, &Link)> {
        let span = &self.spans[self.link_span(k)?];
        Some((&span.text, span.link.as_ref()?))
    }

    /// 每段画成什么颜色：链接用默认的强调色，悬停时换成悬停色并加下划线，按着时是按下色
    pub fn inks(&self, links: LinkColors) -> Vec<SpanInk> {
        self.spans
            .iter()
            .enumerate()
            .map(|(i, span)| match span.link {
                Some(_) => {
                    let k = self.links.iter().position(|&s| s == i);
                    let (hovered, pressed) = (k.is_some() && k == self.hovered, k.is_some() && k == self.pressed);
                    let state = if pressed && hovered {
                        links.pressed
                    } else if hovered {
                        links.hover
                    } else {
                        links.normal
                    };
                    SpanInk { color: Some(span.color.unwrap_or(state)), underline: hovered }
                }
                None => SpanInk { color: span.color, underline: false },
            })
            .collect()
    }
}

/// 一段文字的排版：富文本按段带上各自的样式，只排前 `limit` 个字节对应的段（截断时）
fn build_block(text: &mut TextSystem, content: &str, style: &TextStyle, rich: Option<&Rich>) -> Layout<SpanId> {
    match rich {
        None => text.build(content, style),
        Some(rich) => {
            let mut builder = text.layouts.ranged_builder(&mut text.fonts, content, 1.0, false);
            push_style(&mut builder, style);
            push_spans(&mut builder, style, rich, content.len());
            rich.push_inline_boxes(&mut builder, content.len());
            builder.build(content)
        }
    }
}

/// 某一段（刷子是 `id`）在每一行上占的矩形，相对文字原点。`tight` 时上下只到字的上伸、下伸部分
/// （行内代码的底、链接的焦点环），否则是整行的高度（判断指针在不在链接上）
pub(crate) fn span_boxes(layout: &Layout<SpanId>, id: SpanId, tight: bool) -> Vec<Rect> {
    let mut boxes: Vec<Rect> = Vec::new();
    for line in layout.lines() {
        let metrics = line.metrics();
        let first = boxes.len();
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(run) = item else { continue };
            if run.style().brush != id {
                continue;
            }
            let (y0, y1) = if tight {
                let m = run.run().font_metrics();
                (run.baseline() - m.ascent, run.baseline() + m.descent)
            } else {
                (metrics.block_min_coord, metrics.block_max_coord)
            };
            let x = run.offset();
            // 同一行里接着的几段（中英文回退成不同的字体时会分开）合成一个
            match boxes[first..].last_mut() {
                Some(last) if (last.x + last.width - x).abs() < 0.5 => {
                    let bottom = (last.y + last.height).max(y1);
                    last.y = last.y.min(y0);
                    last.width = x + run.advance() - last.x;
                    last.height = bottom - last.y;
                }
                _ => boxes.push(Rect { x, y: y0, width: run.advance(), height: y1 - y0 }),
            }
        }
    }
    boxes
}

/// 文字绕排时旁边一个元素占掉的地方（相对文字原点）：上边从 `top` 到 `bottom` 之间的行，左边让出 `left`、右边让出 `right`
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Around {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

/// 这一行（从 `y0` 到 `y1`）旁边让出的左右宽度
fn band(around: &[Around], y0: f32, y1: f32) -> (f32, f32) {
    around
        .iter()
        .filter(|a| a.top < y1 && a.bottom > y0)
        .fold((0.0, 0.0), |(left, right), a| (left.max(a.left), right.max(a.right)))
}

/// 断行时宽度多给这么多：布局按量出来的宽度回传时会带一点浮点误差（旁边的元素宽度在动时，正好整数宽的两个汉字
/// 拿到 27.999998），差这么一点不能多断一行。肉眼看不出超出的这一点
const BREAK_SLACK: f32 = 0.02;

/// 断行。有绕排的地方时一行一行地断（parley 的逐行断行可以给每行不同的起点和宽度）：和旁边的元素在同一高度的行
/// 从元素旁边开始、变短；剩下的宽度放不下两个字时这一行挪到元素下面。行高事先不知道，先按 `line` 估，
/// 断完发现实际行高碰到了另一组元素就退回去按实际行高重断一次（混排字号时）
fn break_lines(layout: &mut Layout<SpanId>, width: Option<f32>, around: &[Around], line: f32, size: f32) {
    let width = width.map(|w| w + BREAK_SLACK);
    if around.is_empty() {
        layout.break_all_lines(width);
        return;
    }
    let total = width.unwrap_or(f32::MAX);
    let narrow = (size * 2.0).min(total * 0.5);
    let mut breaker = layout.break_lines();
    let mut guess = line;
    let mut retried = false;
    loop {
        let y = breaker.committed_y() as f32;
        let (left, right) = band(around, y, y + guess);
        let room = (total - left - right).max(0.0);
        if room < narrow && (left > 0.0 || right > 0.0) {
            // 太窄：跳到挡着的元素下面（最早结束的那个），再看一次
            let below = around.iter().filter(|a| a.top < y + guess && a.bottom > y).map(|a| a.bottom).fold(f32::MAX, f32::min);
            breaker.state_mut().set_line_y(below as f64);
            continue;
        }
        let state = breaker.state_mut();
        state.set_layout_max_advance(total);
        state.set_line_x(left);
        state.set_line_max_advance(room);
        match breaker.break_next() {
            None => break,
            Some(YieldData::LineBreak(data)) => {
                let height = (data.line_y_end - data.line_y_start) as f32;
                if !retried && height > guess + 0.5 && band(around, y, y + height) != (left, right) {
                    breaker.revert();
                    guess = height;
                    retried = true;
                    continue;
                }
                (guess, retried) = (line, false);
            }
            Some(YieldData::MaxHeightExceeded(_)) => {}
            // 文字里没有行内的盒子；万一有，按零大小放下，保证往前走
            Some(YieldData::InlineBoxBreak(_)) => {
                let state = breaker.state_mut();
                let x = state.line_x();
                state.append_inline_box_to_line(x, 0.0, 0.0);
            }
        }
    }
    breaker.finish();
}

/// 量过一次的结果：按哪个最大宽度量的、量出来的大小、第一行和最后一行的基线
#[derive(Clone, Copy)]
struct Measured {
    width: Option<f32>,
    size: [f32; 2],
    baselines: Option<(f32, f32)>,
}

/// 按 `max_lines` 截断后显示的文字和它的排版
struct Truncated {
    text: String,
    layout: Layout<SpanId>,
}

/// 算好的选区矩形和算的时候选区的两端。绘制每帧都要这些矩形：一个选择根选中一万段时，每帧问一万次排版
struct SelectionBoxes {
    anchor: Cursor,
    focus: Cursor,
    boxes: Vec<BoundingBox>,
}

/// 文字元素的内容
#[derive(Default)]
pub(crate) struct TextBlock {
    content: String,
    style: TextStyle,
    /// 最多显示几行，0 表示不限。超出时最后一行截断，末尾加省略号
    max_lines: usize,
    layout: Option<Layout<SpanId>>,
    /// 上次断行用的最大宽度；`None` 表示还没断过
    broken_at: Option<Option<f32>>,
    /// 断行后行数超过 `max_lines` 时截断的结果
    truncated: Option<Box<Truncated>>,
    /// 断行之后已经按 `style.align` 对齐过
    aligned: bool,
    /// 能用指针选中、复制（[`crate::Element::selectable`]）
    pub selectable: bool,
    /// 选区，在显示出来的排版上（截断时是截断后的文字）
    selection: Option<Selection>,
    /// 上次算的选区矩形：选区的两端没变、排版没变时直接用。放在堆上，没选中的文字只占一个指针
    selection_boxes: Option<Box<SelectionBoxes>>,
    /// 用 Command+A 全选：复制原文（截断时也是完整的）
    selected_all: bool,
    /// 双击选中的词：之后拖动时按词扩展
    word: Option<Range<usize>>,
    /// 富文本的各段（[`crate::rich_text`]）；普通文字是 `None`
    rich: Option<Box<Rich>>,
    pub selection_color: Color,
    /// 绕排：旁边的元素占掉的地方（[`crate::flow`]）
    around: Vec<Around>,
    /// 绕排容器里最后一段文字：高度至少到旁边的元素底下，把容器撑高
    clear: f32,
    /// 在选择根（[`crate::Element::selection_root`]）里：选择根的节点键。由元素树在选择时写，绘制时判断选区是不是有焦点
    pub selection_root: Option<u64>,
    /// 上次生成的字形图元：排版变了（重新断行、对齐）时清掉，别的元素变了、这段文字没动时照样用
    glyphs: GlyphQuads,
    /// 最近几次量的结果（轮着放）。布局求一行、一列的大小时会按最大内容、最小内容、确定的宽度把同一段文字
    /// 轮着量好几遍（改一个标签的内容要量九次），每换一次宽度都要重新断行；内容、样式、绕排、行数变了时清掉
    measured: [Option<Measured>; 3],
    /// 下一个量的结果放在哪一格
    measured_next: u8,
    /// 最近一次量的结果的基线（布局量完马上读）
    measured_baselines: Option<Option<(f32, f32)>>,
}

impl TextBlock {
    pub fn with_content(content: String) -> Self {
        Self { content, ..Default::default() }
    }

    /// 继承之后的样式
    pub fn style(&self) -> &TextStyle {
        &self.style
    }

    /// 内容或样式变了才返回 true（需要重新排版）。内容变了时选区清空
    pub fn update(&mut self, f: impl FnOnce(&mut String, &mut TextStyle)) -> bool {
        let (content, style) = (self.content.clone(), self.style);
        f(&mut self.content, &mut self.style);
        let changed = self.content != content || self.style != style;
        if changed {
            if self.content != content {
                self.clear_selection();
            }
            self.invalidate();
        }
        changed
    }

    /// 内容（没截断的原文）；行内元素不算（它在全文里的占位字符不出现在这里）
    pub fn content(&self) -> &str {
        self.rich.as_deref().and_then(Rich::plain).unwrap_or(&self.content)
    }

    /// 丢掉排版（加载了新字体），下次用到时重新排
    pub fn invalidate(&mut self) {
        self.layout = None;
        self.broken_at = None;
        self.truncated = None;
        self.aligned = false;
        self.forget_measures();
    }

    /// 断行的条件变了：量过的结果不能再用
    fn forget_measures(&mut self) {
        self.measured = [None; 3];
        self.measured_baselines = None;
    }

    /// 绕排的区域和最低高度，变了返回 true（需要重新布局）
    pub fn set_around(&mut self, around: Vec<Around>, clear: f32) -> bool {
        if self.around == around && self.clear == clear {
            return false;
        }
        self.around = around;
        self.clear = clear;
        self.broken_at = None;
        self.truncated = None;
        self.aligned = false;
        self.forget_measures();
        true
    }

    /// 一行大约多高（逐行断行时先按它估）
    fn line_guess(&self) -> f32 {
        self.style.size * self.style.line_height
    }

    /// 最多显示几行，0 是不限
    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// 改最多显示的行数，变了返回 true（需要重新布局）
    pub fn set_max_lines(&mut self, lines: usize) -> bool {
        if self.max_lines == lines {
            return false;
        }
        self.max_lines = lines;
        self.broken_at = None;
        self.truncated = None;
        self.selection = None;
        self.forget_measures();
        true
    }

    fn layout(&mut self, text: &mut TextSystem) -> &mut Layout<SpanId> {
        let rich = self.rich.as_deref();
        self.layout.get_or_insert_with(|| build_block(text, &self.content, &self.style, rich))
    }

    // ---- 富文本 ----

    /// 换成富文本（挂载时）：全文是各段连起来
    pub fn set_rich(&mut self, rich: Rich) {
        let mut rich = Box::new(rich);
        self.content = rich.restart();
        self.rich = Some(rich);
        self.clear_selection();
        self.invalidate();
    }

    pub fn rich(&self) -> Option<&Rich> {
        self.rich.as_deref()
    }

    pub fn rich_mut(&mut self) -> Option<&mut Rich> {
        self.rich.as_deref_mut()
    }

    /// 改富文本的第 `i` 段，返回 (要重新排版, 要重画)。文字变了时选区清空
    pub fn update_span(&mut self, i: usize, f: impl FnOnce(&mut RichSpan)) -> (bool, bool) {
        let Some(rich) = self.rich.as_deref_mut() else { return (false, false) };
        let Some(span) = rich.spans.get_mut(i) else { return (false, false) };
        let (text, style, color) = (span.text.clone(), span.style.clone(), span.color);
        f(span);
        let (text_changed, style_changed) = (span.text != text, span.style != style);
        let repaint = span.color != color;
        if text_changed {
            self.content = rich.restart();
            self.clear_selection();
        }
        if text_changed || style_changed {
            self.invalidate();
        }
        (text_changed || style_changed, repaint)
    }

    /// 指针在哪个链接上（`point` 相对文字原点）
    pub fn link_at(&self, point: [f32; 2]) -> Option<usize> {
        let (rich, layout) = (self.rich.as_deref()?, self.shown()?);
        (0..rich.links()).find(|&k| {
            let id = rich.link_span(k).map_or(0, |s| s as SpanId + 1);
            span_boxes(layout, id, false).iter().any(|r| r.contains(point))
        })
    }

    /// 第 `k` 个链接在每一行上的矩形（相对文字原点）；`tight` 见 [`span_boxes`]
    pub fn link_boxes(&self, k: usize, tight: bool) -> Vec<Rect> {
        match (self.rich.as_deref().and_then(|r| r.link_span(k)), self.shown()) {
            (Some(span), Some(layout)) => span_boxes(layout, span as SpanId + 1, tight),
            _ => Vec::new(),
        }
    }

    /// 行内代码的底：(矩形, 字号)，相对文字原点
    pub fn code_boxes(&self) -> Vec<(Rect, f32)> {
        let (Some(rich), Some(layout)) = (self.rich.as_deref(), self.shown()) else { return Vec::new() };
        let mut boxes = Vec::new();
        for (i, span) in rich.spans.iter().enumerate().filter(|(_, s)| s.style.code) {
            let size = span.style.size.unwrap_or(self.style.size * CODE_SCALE);
            boxes.extend(span_boxes(layout, i as SpanId + 1, true).into_iter().map(|r| (r, size)));
        }
        boxes
    }

    /// 绘制时每段的颜色（普通文字是空的）
    pub fn inks(&self, links: LinkColors) -> Vec<SpanInk> {
        self.rich.as_deref().map(|rich| rich.inks(links)).unwrap_or_default()
    }

    /// 按宽度断行；行数超过 `max_lines` 时截断
    fn break_at(&mut self, text: &mut TextSystem, max_width: Option<f32>) {
        if self.broken_at == Some(max_width) {
            return;
        }
        let around = mem::take(&mut self.around);
        let (line, size) = (self.line_guess(), self.style.size);
        break_lines(self.layout(text), max_width, &around, line, size);
        self.around = around;
        self.broken_at = Some(max_width);
        self.aligned = false;
        let truncated = self.truncate(text, max_width);
        if truncated.is_some() || self.truncated.is_some() {
            self.selection = None;
        }
        self.truncated = truncated;
    }

    /// 显示出来的排版：截断过的或者完整的
    fn shown(&self) -> Option<&Layout<SpanId>> {
        match &self.truncated {
            Some(t) => Some(&t.layout),
            None => self.layout.as_ref(),
        }
    }

    /// 显示出来的文字
    fn shown_text(&self) -> &str {
        match &self.truncated {
            Some(t) => &t.text,
            None => &self.content,
        }
    }

    /// 截断：第 `max_lines` 行按字（簇）累加宽度，放不下"这些字 + 省略号"时停下，
    /// 然后把截下来的文字加上省略号重新排一次（前面各行的断行不变）。中文按字截断，英文也按字母截断
    fn truncate(&self, text: &mut TextSystem, width: Option<f32>) -> Option<Box<Truncated>> {
        let lines = self.max_lines;
        let full = self.layout.as_ref()?;
        if lines == 0 || full.len() - self.inline_blank_lines(full) <= lines {
            return None;
        }
        let last = full.get(lines - 1)?;
        let range = last.text_range();
        let mut ellipsis = text.build(ELLIPSIS, &self.style);
        ellipsis.break_all_lines(None);
        // 绕排时这一行可能比整段窄
        let m = last.metrics();
        let room = if self.around.is_empty() { width } else { width.map(|_| m.inline_max_coord - m.inline_min_coord) };
        let limit = room.map_or(f32::MAX, |w| w - ellipsis.full_width());
        let (mut end, mut advance) = (range.start, 0.0);
        let mut cluster = Cluster::from_byte_index(full, range.start);
        while let Some(c) = cluster {
            let r = c.text_range();
            if r.start >= range.end || c.is_hard_line_break() {
                break;
            }
            advance += c.advance() + self.inline_width_after(&r);
            if advance > limit {
                break;
            }
            end = r.end;
            cluster = c.next_logical();
        }
        let mut kept = self.content[..end].trim_end().to_owned();
        loop {
            let shown = format!("{kept}{ELLIPSIS}");
            // 富文本：截下来的部分保留各段的样式，省略号用整段的样式
            let mut layout = match self.rich.as_deref() {
                Some(rich) => {
                    let mut builder = text.layouts.ranged_builder(&mut text.fonts, &shown, 1.0, false);
                    push_style(&mut builder, &self.style);
                    push_spans(&mut builder, &self.style, rich, kept.len());
                    rich.push_inline_boxes(&mut builder, kept.len());
                    builder.build(&shown)
                }
                None => text.build(&shown, &self.style),
            };
            break_lines(&mut layout, width, &self.around, self.line_guess(), self.style.size);
            // 极少数情况下省略号被挤到下一行：再去掉一个字
            if layout.len() <= lines || kept.is_empty() {
                return Some(Box::new(Truncated { text: shown, layout }));
            }
            kept.pop();
            kept.truncate(kept.trim_end().len());
        }
    }

    /// 给定最大宽度时的尺寸（`None` 表示不限宽，不断行）。最近量过的宽度直接用量过的结果，不重新断行
    pub fn measure(&mut self, text: &mut TextSystem, max_width: Option<f32>) -> [f32; 2] {
        if let Some(m) = self.measured.iter().flatten().find(|m| m.width == max_width) {
            self.measured_baselines = Some(m.baselines);
            return m.size;
        }
        self.break_at(text, max_width);
        let empty = self.content.is_empty();
        let layout = self.shown().expect("刚断过行");
        // 空文字不占宽度，但保留一行的高度；向上取整，避免布局把同样的宽度传回来时
        // 因为浮点误差多断一行
        let width = if empty { 0.0 } else { layout.width().ceil() };
        let height = layout.height() - self.inline_blank_height(layout);
        let size = [width, height.max(self.clear).ceil()];
        let baselines = self.shown_baselines();
        self.measured[self.measured_next as usize] = Some(Measured { width: max_width, size, baselines });
        self.measured_next = (self.measured_next + 1) % 3;
        self.measured_baselines = Some(baselines);
        size
    }

    /// 第一行和最后一行的基线，相对文字顶部；最近一次测量的结果
    pub fn baselines(&self) -> Option<(f32, f32)> {
        self.measured_baselines.unwrap_or_else(|| self.shown_baselines())
    }

    /// 显示出来的排版的第一行和最后一行的基线
    fn shown_baselines(&self) -> Option<(f32, f32)> {
        let layout = self.shown()?;
        let first = layout.lines().next()?.metrics().baseline;
        let last = layout.lines().last()?.metrics().baseline;
        Some((first, last))
    }

    /// 按最终宽度断行、对齐，用于绘制和选择
    pub fn layout_at(&mut self, text: &mut TextSystem, width: f32) -> &Layout<SpanId> {
        self.break_at(text, Some(width));
        if !self.aligned {
            let alignment = self.style.alignment();
            let layout = match &mut self.truncated {
                Some(t) => &mut t.layout,
                None => self.layout.as_mut().expect("刚断过行"),
            };
            layout.align(alignment, AlignmentOptions::default());
            self.aligned = true;
            // 排版变了都要经过这里（重新整形、断行之后都要重新对齐）：上次生成的字形图元、算的选区矩形不能再用
            self.glyphs.clear();
            self.selection_boxes = None;
        }
        self.shown().expect("刚断过行")
    }

    /// 按最终宽度排好，生成字形图元（排版和生成的条件都没变时拷贝上次的）
    pub fn paint_glyphs(&mut self, text: &mut TextSystem, width: f32, origin: [f32; 2], world: &Affine, ink: Ink, spans: &[SpanInk], quads: &mut Vec<Quad>) {
        self.layout_at(text, width);
        let layout = match &self.truncated {
            Some(t) => &t.layout,
            None => self.layout.as_ref().expect("刚断过行"),
        };
        text.push_glyphs_cached(&mut self.glyphs, layout, 0..usize::MAX, origin, world, ink, spans, 0.0, [0, 0], quads);
    }

    // ---- 选择 ----

    /// 按下指针（`point` 相对文字原点）：单击放下起点（按住 Shift 时扩展已有的选区），双击选词，三击选整段
    pub fn press(&mut self, point: [f32; 2], clicks: u32, extend: bool) {
        if self.press_inline(point, clicks, extend) {
            return;
        }
        let Some(layout) = self.shown() else { return };
        let [x, y] = point;
        let selection = match clicks {
            1 => match self.selection {
                Some(selection) if extend => selection.shift_click_extension(layout, x, y),
                _ => Selection::from_point(layout, x, y),
            },
            2 => {
                let word = word_range(layout, self.shown_text(), x, y);
                let selection = span_selection(layout, word.start, word.end);
                self.word = Some(word);
                self.selection = Some(selection);
                self.selected_all = false;
                return;
            }
            _ => Selection::hard_line_from_point(layout, x, y),
        };
        self.selection = Some(selection);
        self.word = None;
        self.selected_all = false;
    }

    /// 按住拖动：扩展选区（双击、三击之后按词、按段扩展）
    pub fn drag(&mut self, point: [f32; 2]) {
        if self.drag_inline(point) {
            return;
        }
        let (Some(layout), Some(selection)) = (self.shown(), self.selection) else { return };
        let [x, y] = point;
        self.selection = Some(match &self.word {
            // 双击之后：最初选中的词一直选着，另一头按词对齐（中日文按词典分的词）
            Some(initial) => {
                let target = word_range(layout, self.shown_text(), x, y);
                let (anchor, focus) = if target.start < initial.start {
                    (initial.end, target.start)
                } else {
                    (initial.start, target.end.max(initial.end))
                };
                span_selection(layout, anchor, focus)
            }
            None => selection.extend_to_point(layout, x, y),
        });
        self.selected_all = false;
    }

    pub fn select_all(&mut self) {
        let Some(layout) = self.shown() else { return };
        let end = self.shown_text().len();
        self.selection = Some(span_selection(layout, 0, end));
        self.word = None;
        self.selected_all = true;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_boxes = None;
        self.word = None;
        self.selected_all = false;
    }

    /// 有选中的文字（不只是一个插入点）
    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|s| !s.is_collapsed())
    }

    /// 选中的文字；全选时是完整的原文（截断了也是），否则是选中的那一段（不含省略号）
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.filter(|s| !s.is_collapsed())?;
        if self.selected_all {
            return Some(self.copy_text(&self.content, 0..self.content.len()));
        }
        let shown = self.shown_text();
        let range = selection.text_range();
        shown.get(range.clone())?;
        let mut selected = self.copy_text(shown, range);
        if self.truncated.is_some() && selected.ends_with(ELLIPSIS) {
            selected.truncate(selected.len() - ELLIPSIS.len());
        }
        Some(selected)
    }

    // ---- 跨元素的选择（见 `tree/selection.rs`）：位置都是显示出来的文字里的字节偏移 ----

    /// 这一点（相对文字原点）离哪个字的边界最近，贴着哪一边。点在行内元素上时是离得近的那一头
    pub fn caret_at(&self, [x, y]: [f32; 2]) -> Option<Cursor> {
        let layout = self.shown()?;
        if let Some(offset) = self.inline_offset([x, y]) {
            return Some(Cursor::from_byte_index(layout, offset, Affinity::Downstream));
        }
        let caret = Cursor::from_point(layout, x, y);
        Some(match self.inline_left_behind(caret.index(), y) {
            Some(before) => Cursor::from_byte_index(layout, before, Affinity::Upstream),
            None => caret,
        })
    }

    /// 这一点所在的词（中日文按词典）
    pub fn word_at(&self, [x, y]: [f32; 2]) -> Option<Range<usize>> {
        if let Some(range) = self.inline_range([x, y]) {
            return Some(range);
        }
        Some(word_range(self.shown()?, self.shown_text(), x, y))
    }

    /// 这一点所在的整段（到换行符为止，连同换行符）
    pub fn line_at(&self, [x, y]: [f32; 2]) -> Option<Range<usize>> {
        Some(Selection::hard_line_from_point(self.shown()?, x, y).text_range())
    }

    /// 显示出来的文字有多长（字节）
    pub fn shown_len(&self) -> usize {
        self.shown_text().len()
    }

    /// 选中从 `anchor` 到 `focus`；`whole` 是整段都在选区里（复制完整的原文，截断了也是）
    pub fn select_range(&mut self, anchor: usize, focus: usize, whole: bool, selection_root: u64) {
        let Some(layout) = self.shown() else { return };
        let len = self.shown_text().len();
        self.selection = Some(span_selection(layout, anchor.min(len), focus.min(len)));
        self.word = None;
        self.selected_all = whole;
        self.selection_root = Some(selection_root);
    }

    /// 显示出来的排版和文字（键盘移动选区时用）
    pub fn shown_parts(&self) -> Option<(&Layout<SpanId>, &str)> {
        Some((self.shown()?, self.shown_text()))
    }

    /// 显示出来有几行
    pub fn shown_line_count(&self) -> usize {
        self.shown().map_or(0, |layout| layout.len() - self.inline_blank_lines(layout))
    }

    /// 选区一端的矩形（相对文字原点，宽 1）：上下移动的横坐标、算它在第几行、滚到看得见都用它
    pub fn caret_box(&self, at: Cursor) -> Option<Rect> {
        if let Some(caret) = self.inline_caret_box(at) {
            return Some(caret);
        }
        let layout = self.shown()?;
        let r = at.geometry(layout, 0.0);
        Some(Rect { x: crate::editor::motion::x_of(layout, at), y: r.y0 as f32, width: 1.0, height: (r.y1 - r.y0) as f32 })
    }

    /// 第 `line` 行上横坐标 `x` 处的位置（上下移动的落点）
    pub fn caret_on_line(&self, line: usize, x: f32) -> Option<Cursor> {
        let layout = self.shown()?;
        let target = layout.get(line)?;
        let metrics = target.metrics();
        let y = (metrics.block_min_coord + metrics.block_max_coord) * 0.5;
        // 这一行只有行内元素，没有字：parley 找不到字，落在元素离得近的一头
        if target.runs().next().is_none()
            && let Some(offset) = self.inline_nearest(x, y)
        {
            return Some(Cursor::from_byte_index(layout, offset, Affinity::Downstream));
        }
        self.caret_at([x, y])
    }

    /// 选区的活动端（这段文字自己的选区，不在选择根里时用）
    pub fn selection_focus(&self) -> Option<Cursor> {
        Some(self.selection?.focus())
    }

    /// 活动端挪到 `focus`，起点不动
    pub fn set_selection_focus(&mut self, focus: Cursor) {
        let Some(selection) = self.selection else { return };
        self.selection = Some(Selection::new(selection.anchor(), focus));
        self.word = None;
        self.selected_all = false;
    }

    /// 选区的矩形（相对文字原点）。选区的两端没变、排版没变时不再问排版。
    /// 有行内元素的文字每次都算：要去掉元素占的那几列，元素的位置在排版之外
    pub fn selection_rects(&mut self) -> &[BoundingBox] {
        let Some(selection) = self.selection else { return &[] };
        let (anchor, focus) = (selection.anchor(), selection.focus());
        let reusable = self.with_inlines().is_none() && self.selection_boxes.as_ref().is_some_and(|b| b.anchor == anchor && b.focus == focus);
        if !reusable {
            let Some(layout) = self.shown() else { return &[] };
            let boxes = self.around_inlines(selection.geometry(layout).into_iter().map(|(rect, _)| rect).collect());
            self.selection_boxes = Some(Box::new(SelectionBoxes { anchor, focus, boxes }));
        }
        self.selection_boxes.as_ref().map_or(&[], |b| &b.boxes)
    }
}

/// 从 `anchor` 到 `focus`（字节偏移）的选区
pub(crate) fn span_selection<B: Brush>(layout: &Layout<B>, anchor: usize, focus: usize) -> Selection {
    let affinity = |at: usize, other: usize| if at <= other { Affinity::Downstream } else { Affinity::Upstream };
    Selection::new(
        Cursor::from_byte_index(layout, anchor, affinity(anchor, focus)),
        Cursor::from_byte_index(layout, focus, affinity(focus, anchor)),
    )
}

/// 这一点所在的词（双击）：中日文按词典分词（见 [`crate::words`]），其他文字交给 parley
pub(crate) fn word_range<B: Brush>(layout: &Layout<B>, text: &str, x: f32, y: f32) -> Range<usize> {
    if let Some((cluster, _)) = Cluster::from_point(layout, x, y)
        && let Some(word) = crate::segmentation::word_at(text, cluster.text_range().start)
    {
        return word;
    }
    Selection::word_from_point(layout, x, y).text_range()
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct GlyphKey {
    font: u64,
    index: u32,
    glyph: u32,
    /// 物理像素字号的位模式
    size: u32,
    subpixel: u8,
    coords: u64,
    embolden: bool,
    /// 合成斜体的倾斜角度（度），0 表示不倾斜
    skew: i8,
}

/// 拼成四个整数再哈希（每个字段单独写一次的话，绘制时每个字形都多几次哈希调用）
impl Hash for GlyphKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.font);
        state.write_u64((self.index as u64) << 32 | self.glyph as u64);
        let synthesis = (self.embolden as u64) << 8 | self.skew as u8 as u64;
        state.write_u64((self.size as u64) << 32 | (self.subpixel as u64) << 16 | synthesis);
        state.write_u64(self.coords);
    }
}

/// 字形缓存的哈希：键是几个整数，乘法混合就够了。绘制时每个字形查一次缓存，
/// 默认的 SipHash 在这里占了绘制文字的三分之一
#[derive(Default)]
pub(crate) struct GlyphHasher(u64);

impl Hasher for GlyphHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(byte as u64);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    /// 字符网格按字查字形时的键
    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }
}

/// 字符网格画字用的字面：哪个字体、可变字体的坐标、合成的粗体和斜体
#[derive(Clone)]
#[cfg(feature = "char-grid")]
pub(crate) struct CellFace {
    font: FontData,
    coords: Vec<i16>,
    coords_hash: u64,
    embolden: bool,
    skew: i8,
}

#[cfg(feature = "char-grid")]
impl CellFace {
    fn of(run: &parley::Run<'_, SpanId>) -> Self {
        let coords: Vec<i16> = run.normalized_coords().iter().map(|c| c.to_bits()).collect();
        let mut hasher = DefaultHasher::new();
        coords.hash(&mut hasher);
        let synthesis = run.synthesis();
        Self {
            font: run.font().clone(),
            coords_hash: hasher.finish(),
            coords,
            embolden: synthesis.embolden(),
            skew: synthesis.skew().map_or(0, |degrees| degrees.round() as i8),
        }
    }
}

/// 字符网格里一个字的字形（不用整形的字，字形号直接查字符表）
#[cfg(feature = "char-grid")]
pub(crate) struct CellGlyph {
    pub face: CellFace,
    pub glyph: u32,
    /// 字形的宽（逻辑像素）：回退来的字和格子不一样宽时放在格子正中
    pub advance: f32,
}

/// 字符网格里整形好的一段字（一个字素簇；连字开着时是同样式连续的一串格子）。位置都是逻辑像素，相对这段字（或者字形组）的起点，
/// 和格子无关：缓存之后放到哪一列都能用
#[derive(Default)]
#[cfg(feature = "char-grid")]
pub(crate) struct CellShape {
    faces: Vec<CellFace>,
    /// 按字形组排：一组是一个字素簇，或者连成一个连字的几个字素簇
    pub units: Vec<CellShapeUnit>,
    glyphs: Vec<CellShapeGlyph>,
    /// 整形时用：正在往哪一组放字形、每组从哪开始（和 `units` 对应，只有整形时才有）
    walk: CellShapeWalk,
}

/// 整形时往组里分字形的进度
#[derive(Default)]
#[cfg(feature = "char-grid")]
struct CellShapeWalk {
    /// 这一段字形（一个字体）的组从第几个开始，现在到第几个
    first: usize,
    current: usize,
    /// 每组的起点在这一段字形里的位置
    starts: Vec<f32>,
    /// 这一段的笔走到哪了（相对这一段的开头）
    pen: f32,
    rtl: bool,
    /// 这一段文字的起点：同一段文字（同一个字体）拆成几个字形段时不用重新分组
    text_start: Option<usize>,
    /// 这个字形段的笔的起点和基线（把 parley 放好的位置换回偏移）
    run_pen: f32,
    baseline: f32,
}

/// 整形结果里的一组字形
#[cfg(feature = "char-grid")]
pub(crate) struct CellShapeUnit {
    /// 这组字形对应的文字（字节范围）
    pub text: Range<u32>,
    /// 这组字形的总宽
    pub advance: f32,
    face: u32,
    glyphs: Range<u32>,
}

/// 整形结果里的一个字形
#[derive(Clone, Copy)]
#[cfg(feature = "char-grid")]
pub(crate) struct CellShapeGlyph {
    pub id: u32,
    /// 相对字形组起点的横向位置
    pub x: f32,
    /// 相对基线的纵向偏移（往下是正的）
    pub y: f32,
}

#[cfg(feature = "char-grid")]
impl CellShape {
    pub fn face(&self, unit: &CellShapeUnit) -> &CellFace {
        &self.faces[unit.face as usize]
    }

    pub fn glyphs(&self, unit: &CellShapeUnit) -> &[CellShapeGlyph] {
        &self.glyphs[unit.glyphs.start as usize..unit.glyphs.end as usize]
    }

    /// 一共几个字形（缓存按它算占了多少）
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }

    /// 开始一段字形（一个字体的 `source`，笔的起点和基线）：换了一段文字（换了字体）时按字素簇分好组，连字的几个簇合成一组
    fn begin_glyph_run(&mut self, source: &parley::Run<'_, SpanId>, offset: f32, baseline: f32) {
        let walk = &mut self.walk;
        walk.run_pen = offset;
        walk.baseline = baseline;
        let text_start = source.text_range().start;
        if walk.text_start == Some(text_start) {
            return;
        }
        walk.text_start = Some(text_start);
        walk.first = self.units.len();
        walk.current = walk.first;
        walk.starts.clear();
        walk.pen = 0.0;
        walk.rtl = source.is_rtl();
        let face = self.faces.len() as u32;
        self.faces.push(CellFace::of(source));
        // 从左到右排：从右到左的文字里连字的第一个簇在最右边
        let mut start = 0.0;
        let mut joins_next = false;
        for cluster in source.visual_clusters() {
            let range = cluster.text_range();
            let (range, advance) = ((range.start as u32, range.end as u32), cluster.advance());
            let joined = if walk.rtl { joins_next } else { cluster.is_ligature_continuation() };
            joins_next = cluster.is_ligature_continuation();
            match self.units.last_mut().filter(|_| joined) {
                Some(unit) => {
                    unit.text.start = unit.text.start.min(range.0);
                    unit.text.end = unit.text.end.max(range.1);
                    unit.advance += advance;
                }
                None => {
                    let at = self.glyphs.len() as u32;
                    self.units.push(CellShapeUnit { text: range.0..range.1, advance, face, glyphs: at..at });
                    walk.starts.push(start);
                }
            }
            start += advance;
        }
    }

    /// 一个放好位置的字形（`positioned_glyphs` 的）：笔走到哪一组的范围里就算哪一组。字宽是 0 的字形（变音符号）跟着前面的字形；
    /// 从右到左的文字里它排在基字前面，跟着后面的
    fn add_glyph(&mut self, glyph: parley::Glyph) {
        const NEAR: f32 = 1e-3;
        let walk = &mut self.walk;
        if self.units.len() == walk.first {
            return;
        }
        let zero_width = glyph.advance.abs() < NEAR;
        loop {
            let i = walk.current - walk.first;
            let Some(&next) = walk.starts.get(i + 1) else { break };
            let past = walk.pen > next - NEAR;
            let stays = zero_width && !walk.rtl && walk.pen <= next + NEAR;
            if !past || stays {
                break;
            }
            walk.current += 1;
            let at = self.glyphs.len() as u32;
            self.units[walk.current].glyphs = at..at;
        }
        let start = walk.starts[walk.current - walk.first];
        self.glyphs.push(CellShapeGlyph { id: glyph.id, x: walk.pen - start + (glyph.x - walk.run_pen), y: glyph.y - walk.baseline });
        self.units[walk.current].glyphs.end = self.glyphs.len() as u32;
        walk.pen += glyph.advance;
        walk.run_pen += glyph.advance;
    }
}

/// 字符网格的格子：宽（等宽字体的字宽）、高（字号 × 行高）、基线、下划线、删除线，都对齐到物理像素（逻辑像素）
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(feature = "char-grid")]
pub(crate) struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
    /// 下划线的顶在基线下面多少、多粗
    pub underline_offset: f32,
    pub underline_size: f32,
    /// 删除线的顶在基线上面多少、多粗
    pub strikethrough_offset: f32,
    pub strikethrough_size: f32,
}

#[derive(Clone, Copy)]
struct GlyphEntry {
    kind: AtlasKind,
    uv: [f32; 4],
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

/// 一段排版上次生成的字形图元（每个文字元素一份，输入框每段一份）。排版没变、生成的条件（原点、变换、颜色、
/// 缩放、图集）也没变时原样拷贝，不再逐个字形查缓存、算位置。排版变了由持有它的一方清掉（[`GlyphQuads::clear`]）
#[derive(Default)]
pub(crate) struct GlyphQuads {
    /// 生成时的条件；`None` 表示没有能用的图元
    stamp: Option<GlyphStamp>,
    /// 生成时富文本各段的颜色（普通文字是空的）
    spans: Vec<SpanInk>,
    quads: Vec<Quad>,
}

/// 生成字形图元时的条件：一样时生成的图元也一样
#[derive(Clone, Copy, PartialEq)]
struct GlyphStamp {
    origin: [f32; 2],
    world: Affine,
    ink: Ink,
    scale: f32,
    /// 图集扩大过几次（扩大后字形在图集里的位置都变了）
    atlas: u64,
    /// 测试丢掉过几次留着的图元（[`TextSystem::forget_glyphs`]）
    forgotten: u64,
}

impl GlyphQuads {
    /// 排版变了：下次重新生成（留着内存，重新生成时不用再分配）
    pub fn clear(&mut self) {
        self.stamp = None;
    }

    /// 不再画了（滚出去的段）：连内存一起还掉
    pub fn release(&mut self) {
        *self = Self::default();
    }
}

/// 字体库、排版缓冲、栅格化器、字形图集和字形缓存，每棵元素树一份
pub(crate) struct TextSystem {
    fonts: FontContext,
    layouts: LayoutContext<SpanId>,
    scaler: ScaleContext,
    atlas: GlyphAtlas,
    /// `None` 表示空字形（例如空格）
    glyphs: HashMap<GlyphKey, Option<GlyphEntry>, BuildHasherDefault<GlyphHasher>>,
    /// 路径的栅格化缓存（路径和字形共用图集，各占一页）
    paths: PathCache,
    /// 本次绘制中图集放不下的种类
    full: Option<AtlasKind>,
    /// 窗口缩放（逻辑像素 -> 物理像素），决定栅格化的分辨率
    scale: f32,
    /// 已经登记进字体库的自定义字体份数（见 [`fonts`]）
    fonts_loaded: usize,
    /// 字形图集扩大过几次（扩大后字形的位置都变了，字符网格缓存的图元要重新生成）
    atlas_generation: u64,
    /// 测试丢掉过几次文字、输入框留着的字形图元（[`Self::forget_glyphs`]）
    forgotten: u64,
    /// 文字、输入框累计重新生成过多少个图元（不算拷贝留着的；测试和基准用）
    pub generated: usize,
}

impl TextSystem {
    pub fn new() -> Self {
        let mut system = Self {
            fonts: FontContext::new(),
            layouts: LayoutContext::new(),
            scaler: ScaleContext::new(),
            atlas: GlyphAtlas::new(),
            glyphs: HashMap::default(),
            paths: PathCache::default(),
            full: None,
            scale: 1.0,
            fonts_loaded: 0,
            atlas_generation: 0,
            forgotten: 0,
            generated: 0,
        };
        system.sync_fonts();
        system
    }

    /// 把之后加载的自定义字体登记进自己的字体库，有新字体时返回 true（已经排好的文字要重新排）
    pub fn sync_fonts(&mut self) -> bool {
        if font::count() == self.fonts_loaded {
            return false;
        }
        let added = font::loaded_since(self.fonts_loaded);
        self.fonts_loaded += added.len();
        for blob in added {
            self.fonts.collection.register_fonts(blob, None);
        }
        true
    }

    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// 返回是否变了；字形缓存按物理字号区分，不用清
    pub fn set_scale(&mut self, scale: f32) -> bool {
        let changed = self.scale != scale;
        self.scale = scale;
        changed
    }

    pub(crate) fn build(&mut self, content: &str, style: &TextStyle) -> Layout<SpanId> {
        let mut builder = self.layouts.ranged_builder(&mut self.fonts, content, 1.0, false);
        push_style(&mut builder, style);
        builder.build(content)
    }

    /// 编辑器的一段：排进已有的排版里（复用它的内存），行高、行顶取整到像素
    pub(crate) fn build_into(&mut self, layout: &mut Layout<SpanId>, content: &str, style: &TextStyle) {
        let mut builder = self.layouts.ranged_builder(&mut self.fonts, content, 1.0, true);
        push_style(&mut builder, style);
        builder.build_into(layout, content);
    }

    /// 图集某一页的边长和用量
    pub fn atlas_usage(&self, kind: scene::AtlasKind) -> (u32, f32) {
        self.atlas.usage(kind)
    }

    pub fn take_atlas_updates(&mut self) -> Vec<AtlasUpdate> {
        self.atlas.take_updates()
    }

    pub fn atlas_snapshot(&self) -> Vec<AtlasUpdate> {
        self.atlas.snapshot()
    }

    /// 本次绘制中图集满了：扩大图集（最大时清空），丢掉这一种的缓存。没满时返回 `None`；满了要重画，
    /// 返回 `Some(这一页还能不能再扩大)`：已经是最大的一页时重画一遍就够了，再满重来也放不下
    pub fn grow_if_full(&mut self) -> Option<bool> {
        let kind = self.full.take()?;
        self.atlas.grow(kind);
        self.atlas_generation += 1;
        match kind {
            AtlasKind::Path => self.paths.clear(),
            _ => self.glyphs.retain(|_, entry| entry.is_none_or(|e| e.kind != kind)),
        }
        Some(self.atlas.usage(kind).0 < scene::max_atlas_size())
    }

    /// 路径变形的配对（缓存）
    pub fn morph_pair(&mut self, from: &crate::vector::VectorPath, to: &crate::vector::VectorPath) -> std::sync::Arc<crate::morph::MorphPair> {
        self.paths.pair(from, to)
    }

    /// 变形结束，图集里预留的那块还回去
    pub fn release_slot(&mut self, slot: Option<crate::vector::Slot>) {
        self.paths.release(slot);
    }

    /// 字形图集扩大过几次
    #[cfg(feature = "char-grid")]
    pub fn atlas_generation(&self) -> u64 {
        self.atlas_generation
    }

    /// 文字、输入框留着的字形图元都不再用，下次绘制整段重新生成（测试拿它对照复用的结果）
    pub fn forget_glyphs(&mut self) {
        self.forgotten += 1;
    }

    // ---- 字符网格（见 `char_grid.rs`）：不排版，按字查字形 ----

    /// 字符网格的排版：没指定字体族时用系统的等宽字体（和行内代码一样的列表）。带 U+FE0F 的簇明说了要 emoji 的样子，
    /// 先找 emoji 字体：不然等宽字体里正好有这个字（❤ ☀ ↔）时会画成单色的
    #[cfg(feature = "char-grid")]
    fn build_cell(&mut self, content: &str, style: &TextStyle) -> Layout<SpanId> {
        let mut builder = self.layouts.ranged_builder(&mut self.fonts, content, 1.0, false);
        push_style(&mut builder, style);
        if style.family == 0 {
            builder.push_default(StyleProperty::FontFamily(FontFamily::List(Cow::Borrowed(&MONOSPACE))));
        }
        if content.contains('\u{FE0F}') {
            let mut start = 0;
            for end in crate::segmentation::graphemes(content).skip(1) {
                if content[start..end].contains('\u{FE0F}') {
                    builder.push(StyleProperty::FontFamily(FontFamily::List(Cow::Borrowed(&EMOJI))), start..end);
                }
                start = end;
            }
        }
        builder.build(content)
    }

    /// 格子的尺寸：等宽字体里 M 的宽、字号 × 行高，基线让字在格子里上下居中；都取整到物理像素
    #[cfg(feature = "char-grid")]
    pub(crate) fn cell_metrics(&mut self, style: &TextStyle) -> CellMetrics {
        let mut layout = self.build_cell("M", style);
        layout.break_all_lines(None);
        let scale = self.scale;
        let snap = |v: f32| (v * scale).round().max(1.0) / scale;
        let (mut ascent, mut descent, mut offset, mut size) = (style.size * 0.8, style.size * 0.2, style.size * 0.1, style.size * 0.06);
        if let Some(run) = layout.lines().next().and_then(|l| {
            l.items().find_map(|i| match i {
                PositionedLayoutItem::GlyphRun(run) => Some(run),
                _ => None,
            })
        }) {
            let m = run.run().font_metrics();
            (ascent, descent) = (m.ascent, m.descent);
            (offset, size) = (-m.underline_offset, m.underline_size);
        }
        let height = snap(style.size * style.line_height);
        // 删除线和普通文字同一个规矩（见 `push_glyphs`）：按字号定，不用字体自带的位置
        let strike = (style.size * 0.06).max(1.0);
        CellMetrics {
            width: snap(layout.width()),
            height,
            baseline: snap((height - (ascent + descent)) * 0.5 + ascent),
            underline_offset: snap(offset.max(1.0 / scale)),
            underline_size: snap(size),
            strikethrough_offset: snap(style.size * 0.35 + strike * 0.5),
            strikethrough_size: snap(strike),
        }
    }

    /// 一个字用哪个字体的哪个字形：用 parley 排这一个字（字体回退、粗体斜体的字面或合成都在里面），
    /// 取出第一个字形。哪个字体里都没有时是 `None`
    #[cfg(feature = "char-grid")]
    pub(crate) fn cell_glyph(&mut self, ch: char, style: &TextStyle) -> Option<CellGlyph> {
        let mut buf = [0u8; 4];
        let mut layout = self.build_cell(ch.encode_utf8(&mut buf), style);
        layout.break_all_lines(None);
        let line = layout.lines().next()?;
        let run = line.items().find_map(|i| match i {
            PositionedLayoutItem::GlyphRun(run) => Some(run),
            _ => None,
        })?;
        let face = CellFace::of(run.run());
        // 字形号按排版选中的字体的字符表查（一个字不用整形）；不遍历 parley 的字形，免得它的迭代器在绘制文字的
        // 热循环里不再被内联
        let glyph = FontRef::from_index(face.font.data.data(), face.font.index as usize)?.charmap().map(ch);
        Some(CellGlyph { face, glyph: glyph as u32, advance: run.advance() })
    }

    /// 整形一段字（字素簇，或者连字开着时同样式连续的一串格子）：字体回退、连字、变音符号的位置、合成都来自这次排版。
    /// 字形走 [`Self::walk_glyphs`]（见那里为什么），分组靠字素簇的宽度（[`CellShape::add_glyph`]）
    #[cfg(feature = "char-grid")]
    #[cold]
    #[inline(never)]
    pub(crate) fn cell_shape(&mut self, content: &str, style: &TextStyle) -> CellShape {
        let mut layout = self.build_cell(content, style);
        layout.break_all_lines(None);
        let mut shape = CellShape::default();
        let ink = Ink { color: Color::TRANSPARENT, opacity: 1.0, gradient: 0 };
        self.walk_glyphs(&layout, 0..usize::MAX, [0.0, 0.0], &Affine::IDENTITY, ink, &[], &mut Vec::new(), Some(&mut |event| match event {
            GlyphEvent::Run { source, offset, baseline } => shape.begin_glyph_run(&source, offset, baseline),
            GlyphEvent::Glyph(glyph) => shape.add_glyph(glyph),
        }));
        // 结果要缓存，分组的进度不留
        shape.walk = CellShapeWalk::default();
        shape
    }

    /// 字形放进图集（有缓存），返回图元。`pen` 是笔的位置（物理像素，整数：横坐标是格子左边，纵坐标是基线），
    /// 图元的位置换回逻辑像素
    #[cfg(feature = "char-grid")]
    pub(crate) fn cell_quad(&mut self, face: &CellFace, glyph: u32, size: f32, pen: [f32; 2], color: Color) -> Option<Quad> {
        let key = GlyphKey {
            font: face.font.data.id(),
            index: face.font.index,
            glyph,
            size: size.to_bits(),
            subpixel: 0,
            coords: face.coords_hash,
            embolden: face.embolden,
            skew: face.skew,
        };
        let entry = self.entry(key, &face.font, size, &face.coords, 0.0)?;
        let scale = self.scale;
        let (w, h) = (entry.width as f32 / scale, entry.height as f32 / scale);
        let (x, y) = ((pen[0] + entry.left as f32) / scale, (pen[1] - entry.top as f32) / scale);
        Some(Quad::glyph(x, y, w, h, entry.uv, color, entry.kind == AtlasKind::Color))
    }

    /// 借出栅格化路径要用的东西
    pub fn raster(&mut self) -> Raster<'_> {
        Raster { scale: self.scale, atlas: &mut self.atlas, cache: &mut self.paths, full: &mut self.full }
    }

    /// 为一段已排版的文字生成字形图元。`origin` 是内容区左上角（布局坐标），
    /// `world` 是元素的累积变换。`spans` 是富文本各段的颜色（按刷子查，普通文字是空的）
    pub fn push_glyphs(
        &mut self,
        layout: &Layout<SpanId>,
        origin: [f32; 2],
        world: &Affine,
        ink: Ink,
        spans: &[SpanInk],
        quads: &mut Vec<Quad>,
    ) {
        self.walk_glyphs(layout, 0..usize::MAX, origin, world, ink, spans, quads, None);
    }

    /// 这里是整个 crate 唯一遍历 parley 字形的地方：它的字形迭代器只有一处调用时才内联进这个循环，多一处
    /// （哪怕是很少走的路）绘制文字就慢两到五成（实测，`绕排`、`改全部富文本里绑定的颜色` 那几个场景）。所以要一个个字形的
    /// 别的地方（字符网格整形）也从这里拿：给 `visit` 时不生成图元，每段字形收到一次 [`GlyphEvent`]。
    /// 交给 `visit` 的都是拷贝的值，不是 `run` 的引用：`run` 的地址一旦传出去，编译器就不能把它拆进寄存器，绘制文字慢 3% 到 6%（实测）。
    /// `lines` 是走哪几行（输入框的长段分块排版时，一份排版里有几行不归它画）
    fn walk_glyphs(
        &mut self,
        layout: &Layout<SpanId>,
        lines: Range<usize>,
        origin: [f32; 2],
        world: &Affine,
        ink: Ink,
        spans: &[SpanInk],
        quads: &mut Vec<Quad>,
        mut visit: Option<GlyphVisitor<'_>>,
    ) {
        let opacity = ink.opacity;
        let scale = self.scale;
        // 给了 `visit` 时当成不对齐来走：判断放在不对齐的那个分支里，绘制对齐的文字时循环里一条指令也不多
        let snap = world.is_translation() && visit.is_none();
        for line in layout.lines().take(lines.end).skip(lines.start) {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else { continue };
                // 富文本里自己有颜色的段用纯色（不铺整段的渐变），悬停的链接加下划线
                let (color, gradient, hover_line) = match spans.get((run.style().brush as usize).wrapping_sub(1)) {
                    Some(&SpanInk { color: Some(color), underline }) => (color, 0, underline),
                    Some(&SpanInk { color: None, underline }) => (ink.color, ink.gradient, underline),
                    None => (ink.color, ink.gradient, false),
                };
                let font = run.run().font();
                let size = run.run().font_size() * scale;
                let coords: Vec<i16> = run.run().normalized_coords().iter().map(|c| c.to_bits()).collect();
                let synthesis = run.run().synthesis();
                let embolden = synthesis.embolden();
                // 字体没有斜体时合成：栅格化时按这个角度倾斜轮廓
                let skew = synthesis.skew().map_or(0, |degrees| degrees.round() as i8);
                let mut hasher = DefaultHasher::new();
                coords.hash(&mut hasher);
                let coords_hash = hasher.finish();

                if visit.is_some() {
                    visit_event(&mut visit, GlyphEvent::Run { source: *run.run(), offset: run.offset(), baseline: run.baseline() });
                }
                for glyph in run.positioned_glyphs() {
                    let pen = [origin[0] + glyph.x, origin[1] + glyph.y];
                    // 对齐到物理像素：x 保留亚像素档位，y 取整
                    let (physical, subpixel) = if snap {
                        let [x, y] = world.apply(pen);
                        let (x, y) = (x * scale, y * scale);
                        let mut whole = x.floor();
                        let mut bin = ((x - whole) * SUBPIXEL_BINS).round();
                        if bin >= SUBPIXEL_BINS {
                            whole += 1.0;
                            bin = 0.0;
                        }
                        ([whole, y.round()], bin as u8)
                    } else {
                        if visit.is_some() {
                            visit_event(&mut visit, GlyphEvent::Glyph(glyph));
                            continue;
                        }
                        ([0.0, 0.0], 0)
                    };
                    let key = GlyphKey {
                        font: font.data.id(),
                        index: font.index,
                        glyph: glyph.id,
                        size: size.to_bits(),
                        subpixel,
                        coords: coords_hash,
                        embolden,
                        skew,
                    };
                    let Some(entry) = self.entry(key, font, size, &coords, subpixel as f32 / SUBPIXEL_BINS) else {
                        continue;
                    };

                    let (w, h) = (entry.width as f32 / scale, entry.height as f32 / scale);
                    let colored = entry.kind == AtlasKind::Color;
                    let mut quad = if snap {
                        let x = (physical[0] + entry.left as f32) / scale;
                        let y = (physical[1] - entry.top as f32) / scale;
                        Quad::glyph(x, y, w, h, entry.uv, color, colored)
                    } else {
                        // 有缩放旋转：在布局坐标里放好，再整体变换（中心变换，矩阵绕中心）
                        let x = pen[0] + entry.left as f32 / scale;
                        let y = pen[1] - entry.top as f32 / scale;
                        let [cx, cy] = world.apply([x + w * 0.5, y + h * 0.5]);
                        let mut quad = Quad::glyph(cx - w * 0.5, cy - h * 0.5, w, h, entry.uv, color, colored);
                        quad.transform = world.linear;
                        quad
                    };
                    quad.opacity = opacity;
                    if !colored {
                        quad.gradient = gradient;
                    }
                    quads.push(quad);
                }

                if visit.is_some() {
                    continue;
                }

                // 下划线：位置和粗细取字体自带的度量。删除线按字号定：中心在基线上方 0.35 倍字号（汉字字身、大写字母的正中），
                // 粗细是字号的 6%、至少 1 逻辑像素。字体自带的位置按拉丁小写字母定，回退的中文字体里偏低、只有半个像素粗，
                // 穿在汉字下部的横画里看不出来；按字号定，中英混排时各段（不同的字体）也是同一条直线
                let style = run.style();
                let metrics = run.run().font_metrics();
                let font_size = size / scale;
                let strike = (font_size * 0.06).max(1.0);
                let underline = match &style.underline {
                    Some(d) => Some((d.offset.unwrap_or(metrics.underline_offset), d.size.unwrap_or(metrics.underline_size))),
                    None => hover_line.then_some((metrics.underline_offset, metrics.underline_size)),
                };
                let strikethrough = style.strikethrough.as_ref().map(|d| {
                    (d.offset.unwrap_or(font_size * 0.35 + strike * 0.5), d.size.unwrap_or(strike))
                });
                for (offset, size) in [underline, strikethrough].into_iter().flatten() {
                    let size = size.max(1.0 / scale);
                    let mut rect = [origin[0] + run.offset(), origin[1] + run.baseline() - offset, run.advance(), size];
                    if snap {
                        // 对齐物理像素，线条才清晰
                        let [x, y] = world.apply([rect[0], rect[1]]);
                        let height = (size * scale).round().max(1.0) / scale;
                        rect = [x, (y * scale).round() / scale, rect[2], height];
                    } else {
                        let [cx, cy] = world.apply([rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.5]);
                        rect = [cx - rect[2] * 0.5, cy - rect[3] * 0.5, rect[2], rect[3]];
                    }
                    let mut quad = Quad::new(rect[0], rect[1], rect[2], rect[3]).fill(color).opacity(opacity);
                    quad.gradient = gradient;
                    if !snap {
                        quad.transform = world.linear;
                    }
                    quads.push(quad);
                }
            }
        }
    }

    /// 和 [`Self::push_glyphs`] 一样，但先看 `cache`：排版没变（变了时持有的一方已经清掉）、生成的条件也一样时
    /// 直接拷贝上次的图元。`shift` 是拷贝时整体往下挪的距离：输入框的段按段顶为 0 生成，拷贝时挪到段顶，
    /// 前面的段长高、变矮时后面的段不用重新生成（只在只有平移、对齐物理像素时用，见 `editor/document.rs`）。
    /// `place` 是拷贝出去的图元的层号和裁剪（画在自己的内容层里的多行输入框）；`[0, 0]` 时由绘制遍历设。
    /// `lines` 是生成排版的哪几行（整个排版时是 `0..usize::MAX`）
    pub fn push_glyphs_cached(
        &mut self,
        cache: &mut GlyphQuads,
        layout: &Layout<SpanId>,
        lines: Range<usize>,
        origin: [f32; 2],
        world: &Affine,
        ink: Ink,
        spans: &[SpanInk],
        shift: f32,
        place: [u32; 2],
        quads: &mut Vec<Quad>,
    ) {
        let stamp = GlyphStamp { origin, world: *world, ink, scale: self.scale, atlas: self.atlas_generation, forgotten: self.forgotten };
        if cache.stamp != Some(stamp) || cache.spans != spans {
            cache.quads.clear();
            self.walk_glyphs(layout, lines, origin, world, ink, spans, &mut cache.quads, None);
            self.generated += cache.quads.len();
            cache.spans.clear();
            cache.spans.extend_from_slice(spans);
            // 图集这一帧放不下了：生成的不完整（扩大图集后整帧重画），不留
            cache.stamp = self.full.is_none().then_some(stamp);
        }
        // 整块拷贝，再一遍改纵坐标、层号和裁剪（逐个构造图元再放进去慢得多）
        let start = quads.len();
        quads.extend_from_slice(&cache.quads);
        if shift != 0.0 || place != [0, 0] {
            for q in &mut quads[start..] {
                q.rect[1] += shift;
                q.layer = place[0];
                q.clip = place[1];
            }
        }
    }

    /// 查缓存，没有就栅格化并放进图集。查缓存是绘制文字最热的一步，一定内联进调用的地方
    /// （文字和字符网格两处调用时编译器不再自己内联，绘制一万字慢一倍）
    #[inline(always)]
    fn entry(&mut self, key: GlyphKey, font: &FontData, size: f32, coords: &[i16], offset: f32) -> Option<GlyphEntry> {
        match self.glyphs.get(&key) {
            Some(entry) => *entry,
            None => self.rasterize(key, font, size, coords, offset),
        }
    }

    /// 栅格化一个字形放进图集（缓存里没有时）
    #[cold]
    #[inline(never)]
    fn rasterize(&mut self, key: GlyphKey, font: &FontData, size: f32, coords: &[i16], offset: f32) -> Option<GlyphEntry> {
        if self.full.is_some() {
            return None;
        }
        let font_ref = FontRef::from_index(font.data.data(), font.index as usize)?;
        let mut scaler = self.scaler.builder(font_ref).size(size).hint(false).normalized_coords(coords).build();
        // 轮廓坐标 y 朝上：x 加上 y·tan(角度)，上端往右倒
        let skew = (key.skew != 0).then(|| Transform::skew(Angle::from_degrees(key.skew as f32), Angle::from_degrees(0.0)));
        // 合成粗体只对普通的轮廓有意义（彩色字形、位图不加粗）
        let image = if key.embolden && scaler.has_outlines() && !scaler.has_color_outlines() && !scaler.has_color_bitmaps() {
            emboldened(&mut scaler, key.glyph as u16, size / 40.0, skew, offset)
        } else {
            let mut render = Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline]);
            render.format(Format::Alpha).offset(Vector::new(offset, 0.0)).transform(skew);
            render.render(&mut scaler, key.glyph as u16)
        };
        let entry = match image {
            Some(image) if image.placement.width > 0 && image.placement.height > 0 => {
                let kind = match image.content {
                    Content::Color => AtlasKind::Color,
                    Content::Mask | Content::SubpixelMask => AtlasKind::Mask,
                };
                let p = image.placement;
                let Some(uv) = self.atlas.insert(kind, p.width, p.height, &image.data) else {
                    self.full = Some(kind);
                    return None;
                };
                Some(GlyphEntry { kind, uv, left: p.left, top: p.top, width: p.width, height: p.height })
            }
            _ => None,
        };
        self.glyphs.insert(key, entry);
        entry
    }
}

/// 遍历字形时交给观察者的（[`TextSystem::walk_glyphs`]）：一段字形开始（用的字体，笔的起点和基线），和放好位置的每个字形。
/// 现在只有字符网格看这些字段
#[cfg_attr(not(feature = "char-grid"), allow(dead_code))]
enum GlyphEvent<'a> {
    Run { source: parley::Run<'a, SpanId>, offset: f32, baseline: f32 },
    Glyph(parley::Glyph),
}

/// 遍历字形的观察者
type GlyphVisitor<'a> = &'a mut dyn for<'b> FnMut(GlyphEvent<'b>);

/// 交给观察者。放在循环外面：绘制文字时这条路不走，别占循环里的地方
#[cold]
#[inline(never)]
fn visit_event(visit: &mut Option<GlyphVisitor<'_>>, event: GlyphEvent<'_>) {
    if let Some(visit) = visit {
        visit(event);
    }
}

/// 合成粗体的字形：轮廓加粗（[`embolden`]）、倾斜，再画成遮罩
fn emboldened(scaler: &mut Scaler, glyph: u16, strength: f32, skew: Option<Transform>, offset: f32) -> Option<Image> {
    let mut outline = scaler.scale_outline(glyph)?;
    embolden(&mut outline, strength);
    if let Some(skew) = &skew {
        outline.transform(skew);
    }
    let offset = Vector::new(offset, 0.0);
    // inspect 先算好大小：placement 的 top 按算好的高度换算
    let (data, placement) = Mask::new(outline.path()).format(Format::Alpha).origin(Origin::BottomLeft).offset(offset).render_offset(offset).inspect(|_, _, _| {}).render();
    Some(Image { source: Source::Outline, content: Content::Mask, placement, data })
}

/// 轮廓往外扩 `strength`。swash 判断轮廓方向时把所有轮廓的点连成一个多边形（[`winding_misjudged`]），
/// 几条分开的轮廓（i、j、冒号这些）可能判断反，轮廓往里缩，字反而变细变淡。判断反的时候给相反的强度：
/// 往哪边扩又反回来，和判断对时一样；只有整体的平移（它把字往右上挪一个强度）也反了，补回来
fn embolden(outline: &mut Outline, strength: f32) {
    if winding_misjudged(outline.points(), outline.verbs()) {
        outline.embolden(-strength, -strength);
        for point in outline.points_mut() {
            point.x += 2.0 * strength;
            point.y += 2.0 * strength;
        }
    } else {
        outline.embolden(strength, strength);
    }
}

/// 把所有轮廓的点连成一个多边形算出的方向（swash 的算法），和每条轮廓各算各的面积加起来的方向不一样
fn winding_misjudged(points: &[Point], verbs: &[Verb]) -> bool {
    let mut area = 0.0;
    let (mut start, mut end) = (0, 0);
    for verb in verbs {
        match verb {
            Verb::MoveTo | Verb::Close => {
                area += signed_area(&points[start..end]);
                start = end;
                end += usize::from(*verb == Verb::MoveTo);
            }
            Verb::LineTo => end += 1,
            Verb::QuadTo => end += 2,
            Verb::CurveTo => end += 3,
        }
    }
    area += signed_area(&points[start..end]);
    (signed_area(points) > 0.0) != (area > 0.0)
}

/// 多边形面积的两倍，逆时针（y 朝上）是正的
fn signed_area(points: &[Point]) -> f32 {
    let Some(&last) = points.last() else { return 0.0 };
    let mut previous = last;
    let mut area = 0.0;
    for &point in points {
        area += (point.y - previous.y) * (point.x + previous.x);
        previous = point;
    }
    area
}
