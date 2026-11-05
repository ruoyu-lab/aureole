//! 布局的值：长度（像素、百分比、auto、fr）、网格轨道和网格位置，方向和对齐；外边距、定位的简写。
//!
//! 布局属性本身（尺寸、内外边距、网格、换行、定位……）定义在属性表（`tree/property_table.rs`）。

use std::ops::{Range, RangeFrom, RangeFull};

use taffy::{
    Dimension, GridPlacement, GridTemplateComponent, LengthPercentageAuto, Line, MaxTrackSizingFunction, MinTrackSizingFunction,
    RepetitionCount, Style, TrackSizingFunction,
};

use crate::element::Element;
use crate::property::registry::{PropId, split};
use crate::property::{IntoProp, Prop};

/// 长度：宽高、最小最大尺寸、外边距、定位、网格轨道都用它。
///
/// 数字直接当像素：`.width(200)`；其他用 [`px`]、[`pct`]、[`auto`]、[`fr`] 构造
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Length {
    /// 逻辑像素
    Px(f32),
    /// 父元素的百分比，0～100（宽、左右边距和定位按父元素宽度，高、上下按高度）
    Pct(f32),
    /// 由内容和布局决定；外边距是 auto 时吃掉剩余空间，用来把元素推到一边
    Auto,
    /// 网格里剩余空间的份数，只对网格轨道有效，用在别处等同于 auto
    Fr(f32),
    /// 内容能缩到的最小宽度（文字是最长的一个词）
    MinContent,
    /// 内容不换行时的宽度
    MaxContent,
}

pub use scene::Number;

macro_rules! numbers {
    ($($t:ty),*) => {
        $(
            impl From<$t> for Length {
                fn from(v: $t) -> Self {
                    Length::Px(v as f32)
                }
            }
        )*
    };
}

numbers!(f32, f64, i32, u32);

/// 逻辑像素
pub fn px(value: impl Number) -> Length {
    Length::Px(value.to_f32())
}

/// 父元素的百分比，`pct(50)` 是一半
pub fn pct(percent: impl Number) -> Length {
    Length::Pct(percent.to_f32())
}

/// 由内容和布局决定，见 [`Length::Auto`]
pub fn auto() -> Length {
    Length::Auto
}

/// 网格里剩余空间的份数：`[px(200), fr(1), fr(2)]`
pub fn fr(fraction: impl Number) -> Length {
    Length::Fr(fraction.to_f32())
}

/// 内容能缩到的最小尺寸，见 [`Length::MinContent`]
pub fn min_content() -> Length {
    Length::MinContent
}

/// 内容不换行时的尺寸，见 [`Length::MaxContent`]
pub fn max_content() -> Length {
    Length::MaxContent
}

impl IntoProp<Length> for Length {
    fn into_prop(self) -> Prop<Length> {
        Prop::Static(self)
    }
}

impl Length {
    pub(crate) fn dimension(self) -> Dimension {
        match self {
            Length::Px(v) => Dimension::length(v),
            Length::Pct(v) => Dimension::percent(v / 100.0),
            Length::Auto | Length::Fr(_) => Dimension::auto(),
            Length::MinContent => Dimension::min_content(),
            Length::MaxContent => Dimension::max_content(),
        }
    }

    pub(crate) fn lpa(self) -> LengthPercentageAuto {
        match self {
            Length::Px(v) => LengthPercentageAuto::length(v),
            Length::Pct(v) => LengthPercentageAuto::percent(v / 100.0),
            _ => LengthPercentageAuto::auto(),
        }
    }

    fn min_track(self) -> MinTrackSizingFunction {
        match self {
            Length::Px(v) => MinTrackSizingFunction::length(v),
            Length::Pct(v) => MinTrackSizingFunction::percent(v / 100.0),
            // 份数不能当下限
            Length::Auto | Length::Fr(_) => MinTrackSizingFunction::auto(),
            Length::MinContent => MinTrackSizingFunction::min_content(),
            Length::MaxContent => MinTrackSizingFunction::max_content(),
        }
    }

    fn max_track(self) -> MaxTrackSizingFunction {
        match self {
            Length::Px(v) => MaxTrackSizingFunction::length(v),
            Length::Pct(v) => MaxTrackSizingFunction::percent(v / 100.0),
            Length::Auto => MaxTrackSizingFunction::auto(),
            Length::Fr(v) => MaxTrackSizingFunction::fr(v),
            Length::MinContent => MaxTrackSizingFunction::min_content(),
            Length::MaxContent => MaxTrackSizingFunction::max_content(),
        }
    }
}

/// 网格轨道（一列或一行）的尺寸：一个 [`Length`]，或者 [`Track::min_max`]，可以 [`Tracks::repeat`]、[`auto_fill`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    min: Length,
    max: Length,
    repeat: Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Repeat {
    Once,
    Count(u16),
    AutoFill,
    AutoFit,
}

/// 单个长度当轨道：`fr(1)` 等于 `Track::min_max(auto(), fr(1))`，不会比内容的最小宽度还窄；
/// 要严格等分（内容再长也不撑开）用 `Track::min_max(0, fr(1))`
impl From<Length> for Track {
    fn from(length: Length) -> Self {
        let min = if let Length::Fr(_) = length { Length::Auto } else { length };
        Track { min, max: length, repeat: Repeat::Once }
    }
}

/// 按容器宽度放下尽可能多的轨道，常用来做随窗口重排的卡片网格：
/// `.columns(auto_fill(Track::min_max(240, fr(1))))`。轨道需要有固定的下限或上限
pub fn auto_fill(track: impl Into<Track>) -> Track {
    Track { repeat: Repeat::AutoFill, ..track.into() }
}

/// 同 [`auto_fill`]，但空着的轨道收成 0，已有的项目把整行撑满
pub fn auto_fit(track: impl Into<Track>) -> Track {
    Track { repeat: Repeat::AutoFit, ..track.into() }
}

impl Track {
    /// 不小于 `min`、不大于 `max` 的轨道
    pub fn min_max(min: impl Into<Length>, max: impl Into<Length>) -> Track {
        Track { min: min.into(), max: max.into(), repeat: Repeat::Once }
    }

    fn sizing(self) -> TrackSizingFunction {
        TrackSizingFunction { min: self.min.min_track(), max: self.max.max_track() }
    }

    fn template(self) -> GridTemplateComponent<String> {
        let count = match self.repeat {
            Repeat::Once => return GridTemplateComponent::Single(self.sizing()),
            Repeat::Count(n) => RepetitionCount::Count(n),
            Repeat::AutoFill => RepetitionCount::AutoFill,
            Repeat::AutoFit => RepetitionCount::AutoFit,
        };
        taffy::style_helpers::repeat(count, vec![self.sizing()])
    }
}

/// 一组网格轨道，`columns` / `rows` 的参数。数组、`Vec`、单个轨道、元组（混用 [`Length`] 和 [`Track`]）都能直接传：
///
/// ```
/// # use aureole::prelude::*;
/// grid().columns([px(220), fr(1), fr(1)]);                     // 220px 1fr 1fr
/// grid().columns([fr(1); 3]);                                  // Tracks::repeat(3, 1fr)
/// grid().columns(auto_fill(Track::min_max(240, fr(1))));               // 随宽度重排
/// grid().columns((px(220), Track::min_max(0, fr(1))));                  // 混用长度和轨道
/// let wide = signal(true);
/// grid().columns(move || if wide.get() { Tracks::from([fr(1); 3]) } else { Tracks::from([fr(1)]) });
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tracks(Vec<Track>);

impl Tracks {
    /// 同一个轨道重复 `count` 次，`Tracks::repeat(3, fr(1))`；也可以写成数组 `[fr(1); 3]`
    pub fn repeat(count: u16, track: impl Into<Track>) -> Track {
        Track { repeat: Repeat::Count(count), ..track.into() }
    }

    pub(crate) fn template(&self) -> Vec<GridTemplateComponent<String>> {
        self.0.iter().map(|track| track.template()).collect()
    }

    /// 隐式轨道（`auto_rows`）不能自动重复：固定次数的展开，auto-fill / auto-fit 当作一个
    pub(crate) fn implicit(&self) -> Vec<TrackSizingFunction> {
        let count = |track: &Track| if let Repeat::Count(n) = track.repeat { n as usize } else { 1 };
        self.0.iter().flat_map(|track| std::iter::repeat_n(track.sizing(), count(track))).collect()
    }
}

impl<T: Into<Track>, const N: usize> From<[T; N]> for Tracks {
    fn from(tracks: [T; N]) -> Self {
        Tracks(tracks.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Track>> From<Vec<T>> for Tracks {
    fn from(tracks: Vec<T>) -> Self {
        Tracks(tracks.into_iter().map(Into::into).collect())
    }
}

impl From<Track> for Tracks {
    fn from(track: Track) -> Self {
        Tracks(vec![track])
    }
}

impl IntoProp<Tracks> for Tracks {
    fn into_prop(self) -> Prop<Tracks> {
        Prop::Static(self)
    }
}

impl IntoProp<Tracks> for Track {
    fn into_prop(self) -> Prop<Tracks> {
        Prop::Static(self.into())
    }
}

impl<T: Into<Track>, const N: usize> IntoProp<Tracks> for [T; N] {
    fn into_prop(self) -> Prop<Tracks> {
        Prop::Static(self.into())
    }
}

impl<T: Into<Track>> IntoProp<Tracks> for Vec<T> {
    fn into_prop(self) -> Prop<Tracks> {
        Prop::Static(self.into())
    }
}

macro_rules! track_tuples {
    ($($name:ident),+) => {
        impl<$($name: Into<Track>),+> IntoProp<Tracks> for ($($name,)+) {
            #[allow(non_snake_case)]
            fn into_prop(self) -> Prop<Tracks> {
                let ($($name,)+) = self;
                Prop::Static(Tracks(vec![$($name.into()),+]))
            }
        }
    };
}

track_tuples!(A, B);
track_tuples!(A, B, C);
track_tuples!(A, B, C, D);
track_tuples!(A, B, C, D, E);
track_tuples!(A, B, C, D, E, F);

/// 网格项目在一个方向上的位置，按轨道之间的分隔线算，第一条线是 1：
/// `1..3` 占第 1、2 条轨道，`2..` 从第 2 条线一直到最后，`..` 占满整行，[`Place::span`]`(2)` 自动找位置、跨两条轨道
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Place {
    start: Edge,
    end: Edge,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Edge {
    Auto,
    Line(i16),
    Span(u16),
}

impl Place {
    /// 跨 `n` 条轨道，位置自动放
    pub fn span(n: u16) -> Place {
        Place { start: Edge::Auto, end: Edge::Span(n) }
    }
}

fn line(v: i32) -> Edge {
    Edge::Line(v.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
}

impl From<Range<i32>> for Place {
    fn from(range: Range<i32>) -> Self {
        Place { start: line(range.start), end: line(range.end) }
    }
}

/// 到最后一条线（-1 号线是显式网格的最后一条）
impl From<RangeFrom<i32>> for Place {
    fn from(range: RangeFrom<i32>) -> Self {
        Place { start: line(range.start), end: Edge::Line(-1) }
    }
}

impl From<RangeFull> for Place {
    fn from(_: RangeFull) -> Self {
        Place { start: Edge::Line(1), end: Edge::Line(-1) }
    }
}

impl IntoProp<Place> for Place {
    fn into_prop(self) -> Prop<Place> {
        Prop::Static(self)
    }
}

macro_rules! place_props {
    ($($t:ty),*) => {
        $(
            impl IntoProp<Place> for $t {
                fn into_prop(self) -> Prop<Place> {
                    Prop::Static(self.into())
                }
            }
        )*
    };
}

place_props!(Range<i32>, RangeFrom<i32>, RangeFull);

impl Place {
    /// 自动放（默认）
    pub(crate) const AUTO: Place = Place { start: Edge::Auto, end: Edge::Auto };

    pub(crate) fn new(start: Edge, end: Edge) -> Self {
        Self { start, end }
    }

    pub(crate) fn line(self) -> Line<GridPlacement<String>> {
        let edge = |edge| match edge {
            // 0 号线不存在，当作自动
            Edge::Auto | Edge::Line(0) => GridPlacement::Auto,
            Edge::Line(n) => GridPlacement::Line(n.into()),
            Edge::Span(n) => GridPlacement::Span(n.max(1)),
        };
        Line { start: edge(self.start), end: edge(self.end) }
    }
}

/// 网格容器：用 [`Element::columns`] / [`Element::rows`] 定义轨道，子元素按行自动排进格子，
/// 或者用 [`Element::grid_column`] / [`Element::grid_row`] 指定位置
pub fn grid() -> Element {
    Element { grid_layout: true, ..Default::default() }
}

impl Element {
    // ---- 外边距、定位的简写 ----
    // 简写设的是各边的属性本身，所以后调用的算数：`.margin(8).margin_top(0)` 上边是 0

    /// 四边相同的外边距，同 `.margin_left(m).margin_right(m).margin_top(m).margin_bottom(m)`。
    /// [`auto`] 吃掉剩余空间：`.margin_x(auto())` 水平居中
    pub fn margin<L: Into<Length> + 'static>(mut self, margin: impl IntoProp<L>) -> Self {
        let [left, right, top, bottom] = split(margin.into_slot(L::into));
        self.put(PropId::MARGIN_LEFT, left);
        self.put(PropId::MARGIN_RIGHT, right);
        self.put(PropId::MARGIN_TOP, top);
        self.put(PropId::MARGIN_BOTTOM, bottom);
        self
    }

    /// 左右外边距，同 `.margin_left(m).margin_right(m)`；`.margin_x(auto())` 水平居中
    pub fn margin_x<L: Into<Length> + 'static>(mut self, margin: impl IntoProp<L>) -> Self {
        let [left, right] = split(margin.into_slot(L::into));
        self.put(PropId::MARGIN_LEFT, left);
        self.put(PropId::MARGIN_RIGHT, right);
        self
    }

    /// 上下外边距，同 `.margin_top(m).margin_bottom(m)`
    pub fn margin_y<L: Into<Length> + 'static>(mut self, margin: impl IntoProp<L>) -> Self {
        let [top, bottom] = split(margin.into_slot(L::into));
        self.put(PropId::MARGIN_TOP, top);
        self.put(PropId::MARGIN_BOTTOM, bottom);
        self
    }

    /// 左上角放在父元素内边距框里的 (x, y)，同 `.inset_left(x).inset_top(y)`。用来叠放元素，后面的兄弟画在上面
    pub fn position<X: Into<Length> + 'static, Y: Into<Length> + 'static>(self, x: impl IntoProp<X>, y: impl IntoProp<Y>) -> Self {
        self.inset_left(x).inset_top(y)
    }

    /// 四边到父元素内边距框的距离，同四个 `inset_*`；`.inset(0)` 盖满父元素（遮罩、浮层）
    pub fn inset<L: Into<Length> + 'static>(mut self, inset: impl IntoProp<L>) -> Self {
        let [left, right, top, bottom] = split(inset.into_slot(L::into));
        self.put(PropId::INSET_LEFT, left);
        self.put(PropId::INSET_RIGHT, right);
        self.put(PropId::INSET_TOP, top);
        self.put(PropId::INSET_BOTTOM, bottom);
        self
    }
}

/// 宽高写进样式（像素值以外的，或者没有声明动画的像素值）
pub(crate) fn set_dimension(style: &mut Style, height: bool, length: Length) {
    let dimension = length.dimension();
    if height {
        style.size.height = dimension;
    } else {
        style.size.width = dimension;
    }
}

// ---- 方向和对齐 ----

/// 子元素的排列方向
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// 横排，从左到右
    Row,
    /// 竖排，从上到下
    Column,
}

/// 子元素在交叉轴上的对齐（横排时是竖直方向）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Align {
    /// 靠起点（上或左）
    Start,
    /// 居中
    Center,
    /// 靠终点（下或右）
    End,
    /// 拉伸填满（没有固定尺寸的子元素）
    Stretch,
    /// 按第一行文字的基线对齐：大标题和旁边的小字看起来坐在同一条线上
    Baseline,
}

/// 子元素在主轴上的分布（横排时是水平方向）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Justify {
    /// 挤在起点
    Start,
    /// 挤在中间
    Center,
    /// 挤在终点
    End,
    /// 首尾贴边，剩余空间平分到子元素之间
    SpaceBetween,
    /// 每个子元素两侧留相同空间（首尾的边距是中间间隔的一半）
    SpaceAround,
    /// 所有间隔（包括首尾）都相等
    SpaceEvenly,
    /// 拉伸：网格的 auto 轨道、换行后的各行分掉剩余空间（用于 `align_content` 和网格；弹性布局的主轴上同 `Start`）
    Stretch,
}

pub(crate) fn align_items(align: Align) -> taffy::AlignItems {
    match align {
        Align::Start => taffy::AlignItems::START,
        Align::Center => taffy::AlignItems::CENTER,
        Align::End => taffy::AlignItems::END,
        Align::Stretch => taffy::AlignItems::STRETCH,
        Align::Baseline => taffy::AlignItems::BASELINE,
    }
}

pub(crate) fn justify_content(justify: Justify) -> taffy::JustifyContent {
    match justify {
        Justify::Start => taffy::JustifyContent::START,
        Justify::Center => taffy::JustifyContent::CENTER,
        Justify::End => taffy::JustifyContent::END,
        Justify::SpaceBetween => taffy::JustifyContent::SPACE_BETWEEN,
        Justify::SpaceAround => taffy::JustifyContent::SPACE_AROUND,
        Justify::SpaceEvenly => taffy::JustifyContent::SPACE_EVENLY,
        Justify::Stretch => taffy::JustifyContent::STRETCH,
    }
}

pub(crate) fn flex_direction(direction: Direction) -> taffy::FlexDirection {
    match direction {
        Direction::Row => taffy::FlexDirection::Row,
        Direction::Column => taffy::FlexDirection::Column,
    }
}
