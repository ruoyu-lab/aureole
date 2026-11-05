//! 属性注册表的类型：属性的编号 [`PropId`]、类型化的键 [`Key`]、元数据 [`PropMetadata`]、不限个数的属性集合 [`PropSet`]、
//! 任意属性的值 [`AnyValue`]，以及定义属性用的 `props!` 宏。
//!
//! 属性只在一处定义：`tree/property_table.rs` 里的 `props!`。每条写名字、值的类型、默认值、存在哪、改了要重做什么、
//! 会不会继承、能不能动画、适用于哪些元素、绑定的顺序和读写函数；宏从这一条生成键、元数据、链式方法、
//! 样式对象的方法、按名字的解析和格式化。
//!
//! 建造者里的值是擦除了类型的 [`Slot`]：静态值放在 [`Stored`] 里（常用的类型直接放，不分配），
//! 绑定是一个闭包（只分配一次）。挂载时按注册表的顺序，每个属性调用一次它自己的、单态化的绑定函数。

use std::any::Any;
use std::fmt::{self, Debug, Write as _};
use std::marker::PhantomData;
use std::rc::Rc;

use scene::{Angle, Color};

use crate::gradient::Fill;
use crate::layout::{Align, Direction, Justify, Length};
use crate::shadow::Shadows;
use crate::visual_effects::Filter;
use crate::vector::VectorPath;

// ---- 编号和键 ----

/// 属性的编号：注册表里的下标。编号的先后就是挂载时绑定的先后（见 [`PropMetadata::order`]）
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PropId(pub(crate) u16);

impl PropId {
    /// 注册表里的下标
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// 这个属性的元数据
    pub fn metadata(self) -> &'static PropMetadata {
        &crate::tree::property_table::REGISTRY[self.index()]
    }

    /// 名字（和链式方法同名）
    pub fn name(self) -> &'static str {
        self.metadata().name
    }

    /// 按名字找：`PropId::from_name("background")`
    pub fn from_name(name: &str) -> Option<PropId> {
        crate::tree::property_table::by_name(name)
    }

    /// 全部属性，按绑定的顺序
    pub fn all() -> impl Iterator<Item = PropId> {
        (0..crate::tree::property_table::COUNT as u16).map(PropId)
    }
}

impl Debug for PropId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 类型化的属性键：`property::BACKGROUND` 是 `PropKey<Fill>`。[`crate::Element::set`] 按它设值，编译时就检查值的类型
pub struct PropKey<T> {
    id: PropId,
    _type: PhantomData<fn() -> T>,
}

impl<T> PropKey<T> {
    pub(crate) const fn new(id: PropId) -> Self {
        Self { id, _type: PhantomData }
    }

    /// 属性的编号
    pub const fn id(self) -> PropId {
        self.id
    }

    /// 属性的元数据
    pub fn metadata(self) -> &'static PropMetadata {
        self.id.metadata()
    }
}

impl<T> Clone for PropKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for PropKey<T> {}

impl<T> Debug for PropKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", self.id.name())
    }
}

impl<T> From<PropKey<T>> for PropId {
    fn from(key: PropKey<T>) -> PropId {
        key.id
    }
}

// ---- 元数据 ----

bitflags::bitflags! {
    /// 属性改了以后要重做什么
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Dirty: u8 {
        /// 重新布局
        const LAYOUT = 1;
        /// 重建显示列表
        const PAINT = 1 << 1;
        /// 重新排版文字（继承的文字样式往下传）
        const TEXT = 1 << 2;
        /// 只改合成层的变换、透明度（渲染线程上的值），不重建图元
        const COMPOSITOR = 1 << 3;
        /// 重建这个元素的无障碍节点
        const ACCESSIBILITY = 1 << 4;
        /// 只改行为（焦点、指针形状、键盘、拖放），不重画
        const BEHAVIOR = 1 << 5;
    }
}

bitflags::bitflags! {
    /// 元素的种类（属性适用于哪些元素）。设在不适用的元素上时，调试构建里提示一次
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct KindMask: u16 {
        /// 容器（没有内容的元素：`rect`、`row`、`column`、`grid`……）
        const CONTAINER = 1;
        /// 文字和富文本
        const TEXT = 1 << 1;
        /// 输入框（单行、多行）
        const EDITOR = 1 << 2;
        /// 矢量路径
        const PATH = 1 << 3;
        /// 图片
        const IMAGE = 1 << 4;
        /// 画布
        const CANVAS = 1 << 5;
        /// 字符网格
        const CHAR_GRID = 1 << 6;
        /// 滚动视口
        const SCROLL = 1 << 7;
        /// 绕排容器
        const FLOW = 1 << 8;
        /// 网格容器
        const GRID = 1 << 9;
        /// 应用自己定义的种类（[`crate::kinds::ElementKind`] 默认属于这一类）
        const CUSTOM = 1 << 10;
        /// 任何元素
        const ANY = Self::CONTAINER.bits() | Self::TEXT.bits() | Self::EDITOR.bits() | Self::PATH.bits() | Self::IMAGE.bits()
            | Self::CANVAS.bits() | Self::CHAR_GRID.bits() | Self::SCROLL.bits() | Self::FLOW.bits() | Self::GRID.bits()
            | Self::CUSTOM.bits();
    }
}

/// 属性的值存在节点的哪一块（说明用；读写由每条属性自己的函数做）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Storage {
    /// 节点上的名字、子元素这些冷数据
    Node,
    /// 布局样式
    Layout,
    /// 绘制属性（每帧都读的那两个缓存行）
    Paint,
    /// 3D 变换、变换中心、共享元素
    Depth,
    /// 滤镜、混合、遮罩、缓存成纹理
    VisualEffects,
    /// 元素自己设的文字样式（没设的继承）
    Text,
    /// 内容：文字、路径、图片、文字块的设置
    Content,
    /// 输入框的设置
    Editor,
    /// 交互：叠放次序、焦点、指针形状、禁用、拖放
    Interaction,
    /// 滚动视口
    Scroll,
    /// 绕排容器
    Flow,
    /// 无障碍
    Accessibility,
}

/// 能动画的属性怎么插值
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnimationKind {
    /// 一个数，按分量插值
    Number,
    /// 纯色或渐变：纯色按分量，有渐变时按进度混合两端
    Fill,
    /// 投影：一层外投影按分量，几层、内阴影按进度混合
    Shadow,
    /// 滤镜：按进度混合
    Filter,
    /// 路径的形状：按进度变形
    Path,
    /// 布局算出来的位置：滑过去（没有绑定值）
    Offset,
}

/// 能动画的属性的设置
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationSpec {
    /// 怎么插值
    pub kind: AnimationKind,
    /// 视为停稳的误差，按属性的单位
    pub tolerance: f32,
    /// 合成层的变换、透明度：动画交给渲染线程推进
    pub composited: bool,
    /// 绕 x、y 轴的旋转和透视：系统打开"减少动态效果"时不做动画，直接到位
    pub three_dimensional: bool,
}

impl AnimationSpec {
    pub(crate) const fn new(kind: AnimationKind, tolerance: f32) -> Self {
        Self { kind, tolerance, composited: false, three_dimensional: false }
    }

    pub(crate) const fn compositor(mut self) -> Self {
        self.composited = true;
        self
    }

    pub(crate) const fn depth(mut self) -> Self {
        self.three_dimensional = true;
        self.composited = true;
        self
    }
}

/// 属性的元数据：注册表里的一条（`property::BACKGROUND.metadata()`、`PropId::from_name("gap")`）
pub struct PropMetadata {
    /// 编号
    pub id: PropId,
    /// 名字：和链式方法同名
    pub name: &'static str,
    /// 说明（链式方法的文档）
    pub doc: &'static str,
    /// 值的类型（写在定义里的样子）
    pub value_type: &'static str,
    /// 存在节点的哪一块
    pub storage: Storage,
    /// 改了要重做什么
    pub dirty: Dirty,
    /// 子孙继承（文字样式）
    pub inherited: bool,
    /// 能不能动画、怎么插值、能不能交给渲染线程
    pub animation: Option<AnimationSpec>,
    /// 适用于哪些元素
    pub applies_to: KindMask,
    /// 绑定的顺序：越小越先绑定。整体和局部是两个属性时（`padding` 和 `padding_x`），局部的排在后面，盖过整体的
    pub order: u16,
    /// 有对应的链式方法（`false`：由工厂函数设，或者由别的方法一起设）
    pub has_method: bool,
    /// 样式对象能设它
    pub styleable: bool,
    /// 挂载子元素之后才绑定（绕排的间距、滚动条的颜色：要等子元素、视口建好）
    pub late: bool,
    pub(crate) bind: fn(&crate::tree::TreeRef, crate::tree::NodeId, Slot),
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) write: fn(&mut crate::tree::Tree, crate::tree::NodeId, &AnyValue) -> bool,
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) read: fn(&crate::tree::Tree, crate::tree::NodeId) -> Option<AnyValue>,
    /// 回到没设的样子：多数是写默认值，继承的属性是去掉自己设的值
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) reset: fn(&mut crate::tree::Tree, crate::tree::NodeId),
    pub(crate) default: fn() -> AnyValue,
    pub(crate) default_stored: fn() -> Stored,
    pub(crate) put: fn(&Stored, &mut dyn Any),
    pub(crate) parse: fn(&str) -> Result<AnyValue, ParseError>,
    pub(crate) format: fn(&AnyValue, &mut String) -> bool,
}

impl PropMetadata {
    /// 默认值（没设时元素上的值；继承的属性是根元素继承的值）
    pub fn default_value(&self) -> AnyValue {
        (self.default)()
    }

    /// 按文字解析成这个属性的值：`"12"`、`"50%"`、`"#138a84"`、`"90deg"`、`"center"`……
    pub fn parse(&self, text: &str) -> Result<AnyValue, ParseError> {
        (self.parse)(text)
    }

    /// 把值写成文字（[`PropMetadata::parse`] 能读回来的写法；复杂的值是调试格式）。值的类型不对时是 `None`
    pub fn format(&self, value: &AnyValue) -> Option<String> {
        let mut out = String::new();
        (self.format)(value, &mut out).then_some(out)
    }

    /// 值的类型和这个属性的一样
    pub fn accepts(&self, value: &AnyValue) -> bool {
        let mut out = String::new();
        (self.format)(value, &mut out)
    }
}

impl Debug for PropMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PropMetadata")
            .field("name", &self.name)
            .field("value_type", &self.value_type)
            .field("storage", &self.storage)
            .field("dirty", &self.dirty)
            .field("inherited", &self.inherited)
            .field("anim", &self.animation)
            .field("applies_to", &self.applies_to)
            .field("order", &self.order)
            .finish()
    }
}

// ---- 属性集合 ----

/// 一组属性（按编号的位集合），不受属性个数的限制：位数按注册表的大小定
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PropSet([u64; crate::tree::property_table::WORDS]);

impl PropSet {
    /// 空的
    pub const EMPTY: PropSet = PropSet([0; crate::tree::property_table::WORDS]);

    /// 加进来
    pub fn insert(&mut self, id: PropId) {
        self.0[id.index() / 64] |= 1 << (id.index() % 64);
    }

    /// 去掉
    pub fn remove(&mut self, id: PropId) {
        self.0[id.index() / 64] &= !(1 << (id.index() % 64));
    }

    /// 在不在里面
    pub fn contains(&self, id: PropId) -> bool {
        self.0[id.index() / 64] & (1 << (id.index() % 64)) != 0
    }

    /// 是不是空的
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// 里面的属性，按编号
    pub fn iter(&self) -> impl Iterator<Item = PropId> + '_ {
        PropId::all().filter(|id| self.contains(*id))
    }
}

impl Debug for PropSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl FromIterator<PropId> for PropSet {
    fn from_iter<I: IntoIterator<Item = PropId>>(iter: I) -> Self {
        let mut set = PropSet::EMPTY;
        for id in iter {
            set.insert(id);
        }
        set
    }
}

// ---- 值从哪来 ----

/// 节点上一个属性的值从哪来
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueSource {
    /// 没设：默认值
    Default,
    /// 没设，从祖先继承（文字样式）
    Inherited,
    /// 链式方法设的静态值
    Static,
    /// 绑定（闭包、信号、派生值）
    Bound,
    /// 样式对象设的（静态值或者按状态选值的绑定）
    Style,
    /// 正在动画（过渡、退场、时间线接管）：此刻的值不是绑定的值
    Animated,
    /// 覆盖层临时盖掉的值
    Override,
}

// ---- 任意属性的值 ----

/// 一个属性的值，擦除了类型（按名字解析和格式化、覆盖层用）。克隆只加引用计数
#[derive(Clone)]
pub struct AnyValue(Rc<dyn AnyProp>);

trait AnyProp: Debug {
    fn as_any(&self) -> &dyn Any;
    fn eq_any(&self, other: &dyn AnyProp) -> bool;
}

impl<T: PropType> AnyProp for T {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn eq_any(&self, other: &dyn AnyProp) -> bool {
        AnyProp::as_any(other).downcast_ref::<T>() == Some(self)
    }
}

impl AnyValue {
    /// 包一个值
    pub fn new<T: PropType>(value: T) -> Self {
        Self(Rc::new(value))
    }

    /// 是这个类型时借出来
    pub fn get<T: 'static>(&self) -> Option<&T> {
        // 写明是哪个特征的：`AnyClone` 对所有能克隆的类型都实现了，`self.0.as_any()` 会选中 `Rc` 自己
        AnyProp::as_any(&*self.0).downcast_ref()
    }
}

impl PartialEq for AnyValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_any(&*other.0)
    }
}

impl Debug for AnyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 解析属性的值失败
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// 这种值还不能按文字写（渐变、投影、路径这些结构化的值）
    Unsupported,
    /// 写法不对
    Invalid(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("这种值还不能按文字写"),
            Self::Invalid(why) => write!(f, "写法不对：{why}"),
        }
    }
}

impl std::error::Error for ParseError {}

fn invalid(text: &str, expected: &str) -> ParseError {
    ParseError::Invalid(format!("{text:?} 不是{expected}"))
}

// ---- 属性的值类型 ----

/// 能当属性值的类型：能克隆、比较、打印；可以按文字解析和格式化（结构化的值暂时只能格式化）
pub trait PropType: Clone + PartialEq + Debug + 'static {
    /// 按文字解析
    fn parse(text: &str) -> Result<Self, ParseError> {
        let _ = text;
        Err(ParseError::Unsupported)
    }

    /// 写成文字
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self:?}");
    }

    #[doc(hidden)]
    fn store(self) -> Stored {
        Stored::Boxed(Box::new(self))
    }

    #[doc(hidden)]
    fn unstore(stored: Stored) -> Option<Self> {
        match stored {
            Stored::Boxed(b) => b.into_any().downcast::<Self>().ok().map(|b| *b),
            _ => None,
        }
    }

    #[doc(hidden)]
    fn peek(stored: &Stored) -> Option<Self> {
        match stored {
            Stored::Boxed(b) => AnyClone::as_any(&**b).downcast_ref::<Self>().cloned(),
            _ => None,
        }
    }
}

/// 建造者里的一个静态值：常用的类型直接放（不分配），别的装箱
#[doc(hidden)]
pub enum Stored {
    F32(f32),
    Bool(bool),
    I32(i32),
    Length(Length),
    Angle(Angle),
    Color(Color),
    Fill(Fill),
    Shadows(Shadows),
    Filter(Filter),
    Path(VectorPath),
    Str(Box<str>),
    Direction(Direction),
    Align(Align),
    Justify(Justify),
    Boxed(Box<dyn AnyClone>),
}

#[doc(hidden)]
pub trait AnyClone: Any {
    fn clone_box(&self) -> Box<dyn AnyClone>;
    fn as_any(&self) -> &dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T: Clone + 'static> AnyClone for T {
    fn clone_box(&self) -> Box<dyn AnyClone> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Clone for Stored {
    fn clone(&self) -> Self {
        match self {
            Self::F32(v) => Self::F32(*v),
            Self::Bool(v) => Self::Bool(*v),
            Self::I32(v) => Self::I32(*v),
            Self::Length(v) => Self::Length(*v),
            Self::Angle(v) => Self::Angle(*v),
            Self::Color(v) => Self::Color(*v),
            Self::Fill(v) => Self::Fill(v.clone()),
            Self::Shadows(v) => Self::Shadows(v.clone()),
            Self::Filter(v) => Self::Filter(v.clone()),
            Self::Path(v) => Self::Path(v.clone()),
            Self::Str(v) => Self::Str(v.clone()),
            Self::Direction(v) => Self::Direction(*v),
            Self::Align(v) => Self::Align(*v),
            Self::Justify(v) => Self::Justify(*v),
            Self::Boxed(v) => Self::Boxed(AnyClone::clone_box(&**v)),
        }
    }
}

/// 直接放进 [`Stored`] 的类型
macro_rules! inline_types {
    ($($variant:ident: $ty:ty),* $(,)?) => {
        $(
            impl PropType for $ty {
                fn parse(text: &str) -> Result<Self, ParseError> {
                    Parse::parse(text)
                }

                fn format(&self, out: &mut String) {
                    Format::format(self, out)
                }

                fn store(self) -> Stored {
                    Stored::$variant(self)
                }

                fn unstore(stored: Stored) -> Option<Self> {
                    match stored {
                        Stored::$variant(v) => Some(v),
                        _ => None,
                    }
                }

                fn peek(stored: &Stored) -> Option<Self> {
                    match stored {
                        Stored::$variant(v) => Some(v.clone()),
                        _ => None,
                    }
                }
            }
        )*
    };
}

inline_types!(
    F32: f32,
    Bool: bool,
    I32: i32,
    Length: Length,
    Angle: Angle,
    Color: Color,
    Fill: Fill,
    Shadows: Shadows,
    Filter: Filter,
    Path: VectorPath,
    Direction: Direction,
    Align: Align,
    Justify: Justify,
);

/// 字符串存成 `Box<str>`（`"文字".to_owned()` 的容量正好，转过去不重新分配）
impl PropType for String {
    fn parse(text: &str) -> Result<Self, ParseError> {
        Ok(text.to_owned())
    }

    fn format(&self, out: &mut String) {
        out.push_str(self);
    }

    fn store(self) -> Stored {
        Stored::Str(self.into_boxed_str())
    }

    fn unstore(stored: Stored) -> Option<Self> {
        match stored {
            Stored::Str(s) => Some(String::from(s)),
            _ => None,
        }
    }

    fn peek(stored: &Stored) -> Option<Self> {
        match stored {
            Stored::Str(s) => Some(String::from(&**s)),
            _ => None,
        }
    }
}

/// 装箱放的类型：按文字解析、格式化的写法各自给（没写的用默认：不能解析，调试格式）
macro_rules! boxed_types {
    ($($ty:ty),* $(,)?) => {
        $(
            impl PropType for $ty {
                fn parse(text: &str) -> Result<Self, ParseError> {
                    Parse::parse(text)
                }

                fn format(&self, out: &mut String) {
                    Format::format(self, out)
                }
            }
        )*
    };
}

boxed_types!(
    usize,
    crate::layout::Tracks,
    crate::layout::Place,
    crate::text::TextAlign,
    crate::text::InlineAlign,
    crate::interaction::Cursor,
    crate::interaction::FocusRing,
    crate::image::ImageFit,
    crate::image::ImageSource,
    crate::visual_effects::BlendMode,
    crate::visual_effects::Mask,
    crate::vector::Stroke,
    crate::drag_drop::DragPreview,
    crate::editor::SubmitKey,
    crate::element::Shader,
    crate::accessibility::RangeValue,
    accesskit::Role,
    accesskit::Live,
    accesskit::Toggled,
    animation::Curve,
    [f32; 4],
    [f32; 2],
);

/// [`crate::Element::set`]、[`crate::Style::set`] 按键设属性时收的值，和同名的链式方法收的一样：
/// 填充、长度、投影、文字、图片来源、勾选状态、曲线收能转成它们的类型（`.set(property::BACKGROUND, 颜色)`），别的只收自己的类型。
///
/// 转换挂在属性的类型上，不写成 `V: Into<T>`：只收自己类型的属性（比如 `f32`）这样只有一个候选，
/// 闭包里的字面量（`.set(property::OPACITY, move || 0.5)`）推断得出来
pub trait FromPropValue<V>: PropType {
    /// 换成属性的值
    fn from_prop_value(value: V) -> Self;
}

/// 只收自己的类型
macro_rules! exact_values {
    ($($ty:ty),* $(,)?) => {
        $(
            impl FromPropValue<$ty> for $ty {
                #[inline]
                fn from_prop_value(value: $ty) -> Self {
                    value
                }
            }
        )*
    };
}

exact_values!(
    f32,
    bool,
    i32,
    usize,
    Angle,
    Color,
    Filter,
    VectorPath,
    Direction,
    Align,
    Justify,
    crate::layout::Tracks,
    crate::layout::Place,
    crate::text::TextAlign,
    crate::text::InlineAlign,
    crate::interaction::Cursor,
    crate::interaction::FocusRing,
    crate::image::ImageFit,
    crate::visual_effects::BlendMode,
    crate::visual_effects::Mask,
    crate::vector::Stroke,
    crate::drag_drop::DragPreview,
    crate::editor::SubmitKey,
    crate::element::Shader,
    crate::accessibility::RangeValue,
    accesskit::Role,
    accesskit::Live,
    [f32; 4],
    [f32; 2],
);

/// 收能转成它的类型
macro_rules! converted_values {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<V: Into<$ty>> FromPropValue<V> for $ty {
                #[inline]
                fn from_prop_value(value: V) -> Self {
                    value.into()
                }
            }
        )*
    };
}

converted_values!(Length, Fill, Shadows, String, crate::image::ImageSource, accesskit::Toggled, animation::Curve);

/// 按文字解析：注册表的解析函数用它（默认不支持）
trait Parse: Sized {
    fn parse(text: &str) -> Result<Self, ParseError> {
        let _ = text;
        Err(ParseError::Unsupported)
    }
}

/// 写成文字：默认是调试格式
trait Format: Debug {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self:?}");
    }
}

impl Parse for f32 {
    fn parse(text: &str) -> Result<Self, ParseError> {
        text.trim().parse().map_err(|_| invalid(text, "数"))
    }
}

impl Format for f32 {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}

impl Parse for usize {
    fn parse(text: &str) -> Result<Self, ParseError> {
        text.trim().parse().map_err(|_| invalid(text, "非负整数"))
    }
}

impl Format for usize {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}

impl Parse for i32 {
    fn parse(text: &str) -> Result<Self, ParseError> {
        text.trim().parse().map_err(|_| invalid(text, "整数"))
    }
}

impl Format for i32 {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}

impl Parse for bool {
    fn parse(text: &str) -> Result<Self, ParseError> {
        match text.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(invalid(text, "true 或 false")),
        }
    }
}

impl Format for bool {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{self}");
    }
}

/// 长度：`200`、`200px`、`50%`、`1fr`、`auto`、`min-content`、`max-content`
impl Parse for Length {
    fn parse(text: &str) -> Result<Self, ParseError> {
        let t = text.trim();
        let number = |s: &str| s.trim().parse::<f32>().map_err(|_| invalid(text, "长度"));
        Ok(match t {
            "auto" => Length::Auto,
            "min-content" => Length::MinContent,
            "max-content" => Length::MaxContent,
            _ if t.ends_with('%') => Length::Pct(number(&t[..t.len() - 1])?),
            _ if t.ends_with("fr") => Length::Fr(number(&t[..t.len() - 2])?),
            _ if t.ends_with("px") => Length::Px(number(&t[..t.len() - 2])?),
            _ => Length::Px(number(t)?),
        })
    }
}

impl Format for Length {
    fn format(&self, out: &mut String) {
        let _ = match self {
            Length::Px(v) => write!(out, "{v}"),
            Length::Pct(v) => write!(out, "{v}%"),
            Length::Fr(v) => write!(out, "{v}fr"),
            Length::Auto => write!(out, "auto"),
            Length::MinContent => write!(out, "min-content"),
            Length::MaxContent => write!(out, "max-content"),
        };
    }
}

/// 角度一定带单位：`90deg`、`1.5rad`、`0.25turn`
impl Parse for Angle {
    fn parse(text: &str) -> Result<Self, ParseError> {
        let t = text.trim();
        let number = |s: &str| s.trim().parse::<f32>().map_err(|_| invalid(text, "角度"));
        if let Some(v) = t.strip_suffix("deg") {
            Ok(scene::deg(number(v)?))
        } else if let Some(v) = t.strip_suffix("rad") {
            Ok(scene::rad(number(v)?))
        } else if let Some(v) = t.strip_suffix("turns").or_else(|| t.strip_suffix("turn")) {
            Ok(scene::turns(number(v)?))
        } else {
            Err(invalid(text, "带单位的角度（deg、rad、turn）"))
        }
    }
}

impl Format for Angle {
    fn format(&self, out: &mut String) {
        let _ = write!(out, "{}deg", self.degrees());
    }
}

/// 颜色：`#rrggbb`、`#rrggbbaa`
impl Parse for Color {
    fn parse(text: &str) -> Result<Self, ParseError> {
        let hex = text.trim().strip_prefix('#').ok_or_else(|| invalid(text, "颜色（#rrggbb）"))?;
        let value = u32::from_str_radix(hex, 16).map_err(|_| invalid(text, "颜色（#rrggbb）"))?;
        match hex.len() {
            6 => Ok(Color::hex(value)),
            8 => Ok(Color([(value >> 24) as u8, (value >> 16) as u8, (value >> 8) as u8, value as u8])),
            _ => Err(invalid(text, "颜色（#rrggbb）")),
        }
    }
}

impl Format for Color {
    fn format(&self, out: &mut String) {
        let [r, g, b, a] = self.0;
        let _ = if a == 255 { write!(out, "#{r:02x}{g:02x}{b:02x}") } else { write!(out, "#{r:02x}{g:02x}{b:02x}{a:02x}") };
    }
}

/// 填充：纯色按颜色的写法；渐变暂时不能按文字写
impl Parse for Fill {
    fn parse(text: &str) -> Result<Self, ParseError> {
        <Color as Parse>::parse(text).map(Fill::Color)
    }
}

impl Format for Fill {
    fn format(&self, out: &mut String) {
        match self {
            Fill::Color(c) => Format::format(c, out),
            Fill::Gradient(g) => {
                let _ = write!(out, "{g:?}");
            }
        }
    }
}

/// 按名字写的枚举：`center`、`space_between`……
macro_rules! named {
    ($ty:ty { $($variant:ident => $name:literal),* $(,)? }) => {
        impl Parse for $ty {
            fn parse(text: &str) -> Result<Self, ParseError> {
                match text.trim() {
                    $($name => Ok(<$ty>::$variant),)*
                    _ => Err(invalid(text, concat!($($name, " "),*))),
                }
            }
        }

        impl Format for $ty {
            fn format(&self, out: &mut String) {
                out.push_str(match self {
                    $(<$ty>::$variant => $name,)*
                });
            }
        }
    };
}

named!(Direction { Row => "row", Column => "column" });
named!(Align { Start => "start", Center => "center", End => "end", Stretch => "stretch", Baseline => "baseline" });
named!(Justify {
    Start => "start",
    Center => "center",
    End => "end",
    SpaceBetween => "space_between",
    SpaceAround => "space_around",
    SpaceEvenly => "space_evenly",
    Stretch => "stretch",
});
named!(crate::text::TextAlign { Start => "start", Center => "center", End => "end", Justify => "justify" });
named!(crate::text::InlineAlign { Baseline => "baseline", Center => "center", TextTop => "text_top", TextBottom => "text_bottom", Top => "top", Bottom => "bottom" });
named!(crate::image::ImageFit { Stretch => "stretch", Contain => "contain", Cover => "cover", None => "none", ScaleDown => "scale_down" });
named!(crate::editor::SubmitKey { Enter => "enter", PrimaryEnter => "primary_enter" });

impl Parse for Shadows {}
impl Format for Shadows {}
impl Parse for Filter {}
impl Format for Filter {}
impl Parse for VectorPath {}
impl Format for VectorPath {}
impl Parse for crate::layout::Tracks {}
impl Format for crate::layout::Tracks {}
impl Parse for crate::layout::Place {}
impl Format for crate::layout::Place {}
impl Parse for crate::interaction::Cursor {}
impl Format for crate::interaction::Cursor {}
impl Parse for crate::interaction::FocusRing {}
impl Format for crate::interaction::FocusRing {}
impl Parse for crate::image::ImageSource {}
impl Format for crate::image::ImageSource {}
impl Parse for crate::visual_effects::BlendMode {}
impl Format for crate::visual_effects::BlendMode {}
impl Parse for crate::visual_effects::Mask {}
impl Format for crate::visual_effects::Mask {}
impl Parse for crate::vector::Stroke {}
impl Format for crate::vector::Stroke {}
impl Parse for crate::drag_drop::DragPreview {}
impl Format for crate::drag_drop::DragPreview {}
impl Parse for crate::element::Shader {}
impl Format for crate::element::Shader {}
impl Parse for crate::accessibility::RangeValue {}
impl Format for crate::accessibility::RangeValue {}
impl Parse for accesskit::Role {}
impl Format for accesskit::Role {}
impl Parse for accesskit::Live {}
impl Format for accesskit::Live {}
impl Parse for accesskit::Toggled {}
impl Format for accesskit::Toggled {}
impl Parse for animation::Curve {}
impl Format for animation::Curve {}
impl Parse for [f32; 4] {}
impl Format for [f32; 4] {}
impl Parse for [f32; 2] {}
impl Format for [f32; 2] {}

// ---- 建造者里的值 ----

/// 绑定：求值时把结果写进 `out`（`Option<T>`）。一个属性一个闭包，只分配一次
#[doc(hidden)]
pub trait Eval {
    fn eval(&self, out: &mut dyn Any);
}

impl<T: 'static, F: Fn() -> T> Eval for F {
    fn eval(&self, out: &mut dyn Any) {
        if let Some(out) = out.downcast_mut::<Option<T>>() {
            *out = Some(self());
        }
    }
}

/// 建造者里一个属性的值：静态值、绑定，或者样式对象共用的绑定
#[doc(hidden)]
pub enum Slot {
    Static(Stored),
    Bound(Box<dyn Eval>),
    Shared(Rc<dyn Eval>),
}

impl Slot {
    /// 求值（绑定在副作用里调用，读到的信号就是依赖）
    pub(crate) fn get<T: PropType>(&self) -> Option<T> {
        match self {
            Slot::Static(s) => T::peek(s),
            Slot::Bound(f) => eval(&**f),
            Slot::Shared(f) => eval(&**f),
        }
    }

    /// 绑定的身份：样式对象建的绑定还在不在元素上
    pub(crate) fn identity(&self) -> Option<*const ()> {
        match self {
            Slot::Static(_) => None,
            Slot::Bound(f) => Some(&**f as *const dyn Eval as *const ()),
            Slot::Shared(f) => Some(Rc::as_ptr(f) as *const ()),
        }
    }
}

pub(crate) fn eval<T: 'static>(f: &dyn Eval) -> Option<T> {
    let mut out: Option<T> = None;
    f.eval(&mut out);
    out
}

/// 建造者里的一条：哪个属性、谁设的、值
#[doc(hidden)]
pub struct Entry {
    pub(crate) id: PropId,
    /// 样式对象设的（不是链式方法直接设的）
    pub(crate) span: bool,
    pub(crate) slot: Slot,
}

/// 元素上设了的属性：按编号排好（也就是按绑定的顺序），一个属性一条。大多数元素不超过 8 个属性，不用分配
#[derive(Default)]
#[doc(hidden)]
pub struct Props(smallvec::SmallVec<[Entry; 8]>);

impl Props {
    /// 设一个属性：已经有了就换掉（后调用的算数）
    #[inline]
    pub(crate) fn put(&mut self, id: PropId, slot: Slot, span: bool) {
        let entries = &mut self.0;
        // 多数链式调用按编号从小到大来：从后往前找插入的位置
        let mut at = entries.len();
        while at > 0 && entries[at - 1].id > id {
            at -= 1;
        }
        if at > 0 && entries[at - 1].id == id {
            entries[at - 1] = Entry { id, span, slot };
        } else {
            entries.insert(at, Entry { id, span, slot });
        }
    }

    /// 取走一个属性
    pub(crate) fn take(&mut self, id: PropId) -> Option<Entry> {
        let at = self.0.iter().position(|e| e.id == id)?;
        Some(self.0.remove(at))
    }

    /// 有没有设
    pub(crate) fn has(&self, id: PropId) -> bool {
        self.0.iter().any(|e| e.id == id)
    }

    /// 设了的值
    pub(crate) fn get(&self, id: PropId) -> Option<&Entry> {
        self.0.iter().find(|e| e.id == id)
    }

    pub(crate) fn into_entries(self) -> smallvec::IntoIter<[Entry; 8]> {
        self.0.into_iter()
    }

    pub(crate) fn entries(&self) -> &[Entry] {
        &self.0
    }
}

/// 一个值拆给几个属性（四边的外边距）：静态值复制，绑定共用同一个闭包
pub(crate) fn split<const N: usize>(slot: Slot) -> [Slot; N] {
    match slot {
        Slot::Static(stored) => [(); N].map(|_| Slot::Static(stored.clone())),
        Slot::Bound(f) => {
            let f: Rc<dyn Eval> = Rc::from(f);
            [(); N].map(|_| Slot::Shared(f.clone()))
        }
        Slot::Shared(f) => [(); N].map(|_| Slot::Shared(f.clone())),
    }
}

// ---- 挂载时的绑定 ----

/// 一个属性的值写进节点：静态值立即写；绑定建一个副作用，读到的信号变了重新求值、写进去
/// （这个属性被覆盖层盖着时不写，记成撤掉覆盖后要回到的值）。
/// 求值（用户代码）在借用树之前，副作用里可以安全地读任何信号。每种值的类型单态化一份
pub(crate) fn bind_typed<T: PropType>(
    tree: &crate::tree::TreeRef,
    id: crate::tree::NodeId,
    prop: PropId,
    slot: Slot,
    write: fn(&mut crate::tree::Tree, crate::tree::NodeId, T),
) {
    match slot {
        // 挂载时：刚建的节点不会被覆盖着
        Slot::Static(stored) => {
            if let Some(value) = T::unstore(stored) {
                write(&mut tree.borrow_mut(), id, value);
            }
        }
        Slot::Bound(f) => bind_effect(tree, id, prop, move || eval::<T>(&*f), write),
        Slot::Shared(f) => bind_effect(tree, id, prop, move || eval::<T>(&*f), write),
    }
}

fn bind_effect<T: PropType>(
    tree: &crate::tree::TreeRef,
    id: crate::tree::NodeId,
    prop: PropId,
    get: impl Fn() -> Option<T> + 'static,
    write: fn(&mut crate::tree::Tree, crate::tree::NodeId, T),
) {
    let tree = Rc::downgrade(tree);
    reactive::Effect::new(move || {
        let Some(value) = get() else { return };
        if let Some(tree) = tree.upgrade() {
            let mut tree = tree.borrow_mut();
            if let Some(value) = tree.hold_back(id, prop, value) {
                write(&mut tree, id, value);
            }
        }
    });
}

/// 不换类型
pub(crate) fn identity<T>(value: T) -> T {
    value
}

// ---- 定义属性的宏 ----

/// 属性的唯一定义处用的宏（见 `tree/property_table.rs`）。每条属性：
///
/// ```text
/// /// 文档（就是链式方法的文档）
/// 名字: 值的类型 = 默认值 => {
///     order: 绑定的顺序（整个表从小到大）,
///     storage: 存在哪（Storage 的变体）,
///     dirty: 改了要重做什么（Dirty 的常量，| 连起来）,
///     applies: 适用于哪些元素（KindMask 的常量，| 连起来）,
///     inherited: 子孙继承,
///     anim: [插值方式, 停稳的误差, compositor 或 depth] 或者 [],
///     method: exact（收这个类型）| into（收能转成它的）| editor、editor_into（多行输入框转给里面的编辑区）| manual（没有生成的链式方法）,
///     style: 样式对象能设它,
///     late: 挂载子元素之后才绑定,
///     bind: typed（静态值直接写，绑定建副作用）| custom(自己的绑定函数),
///     write: |树, 节点, 值| 写进节点,
///     read: |树, 节点| 读出节点上自己设的值,
///     reset: [] 写默认值 | [|树, 节点| 去掉自己设的值（继承的属性）],
/// }
/// ```
macro_rules! props {
    ($(
        $(#[doc = $doc:literal])*
        $name:ident: $ty:ty = $default:expr => {
            order: $order:literal,
            storage: $storage:ident,
            dirty: $($dirty:ident)|+,
            applies: $($kind:ident)|+,
            inherited: $inherited:literal,
            anim: [$($anim_kind:ident, $tolerance:literal $(, $anim_flag:ident)*)?],
            method: $method:ident,
            style: $style:tt,
            late: $late:literal,
            bind: $bind:ident $(($custom:path))?,
            write: $write:expr,
            read: $read:expr,
            reset: [$($reset:tt)*] $(,)?
        }
    )*) => {
        pastey::paste! {
            #[allow(non_camel_case_types, dead_code)]
            #[derive(Clone, Copy)]
            #[repr(u16)]
            enum Index { $($name,)* }

            /// 注册表里的属性个数
            pub(crate) const COUNT: usize = [$(stringify!($name)),*].len();
            /// 属性集合用几个 64 位的字
            pub(crate) const WORDS: usize = COUNT.div_ceil(64);

            // 表按绑定的顺序写：编号的先后就是绑定的先后
            const _: () = {
                let orders: [u16; COUNT] = [$($order),*];
                let mut i = 1;
                while i < COUNT {
                    assert!(orders[i - 1] < orders[i], "props! 里的属性要按 order 从小到大写");
                    i += 1;
                }
            };

            /// 类型化的属性键，一个属性一个：`property::BACKGROUND` 是 `PropKey<Fill>`
            pub mod keys {
                #[allow(unused_imports)]
                use super::*;
                $(
                    #[doc = concat!("属性 `", stringify!($name), "` 的键，值的类型是 `", stringify!($ty), "`")]
                    pub const [<$name:upper>]: PropKey<$ty> = PropKey::new(PropId(Index::$name as u16));
                )*
            }

            // 每个属性一个小宏给出 `Property` 变体的文档（文档和"能不能动画"在同一层重复里，不能直接放进变体里）
            $(
                #[allow(unused_macros)]
                macro_rules! [<property_doc_ $name>] {
                    () => {
                        property_doc!($method; $name; $ty; $($doc)*)
                    };
                }
            )*

            /// 能做动画的属性，配合 [`Element::transition`]、[`Element::exit`] 和时间线（`MotionHandle::animate`）使用。
            /// 由属性表生成：表里写了 `anim` 的属性一个变体，和链式方法同名（蛇形对驼峰：`rotation_x` 是 `RotationX`）。
            /// 值就是属性的编号（[`Property::id`]）
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
            #[repr(u16)]
            pub enum Property {
                $($(
                    #[doc = [<property_doc_ $name>]!()]
                    [<$name:camel>] = { let _ = stringify!($anim_kind); Index::$name as u16 },
                )?)*
            }

            impl Property {
                /// 属性的编号
                pub const fn id(self) -> PropId {
                    PropId(self as u16)
                }

                /// 编号对应的能动画的属性；不能动画的属性是 `None`
                pub fn from_id(id: PropId) -> Option<Property> {
                    match id.0 {
                        $($(
                            x if x == Index::$name as u16 && !stringify!($anim_kind).is_empty() => Some(Property::[<$name:camel>]),
                        )?)*
                        _ => None,
                    }
                }
            }

            impl From<Property> for PropId {
                fn from(property: Property) -> PropId {
                    property.id()
                }
            }

            impl PropId {
                $(
                    #[doc = concat!("属性 `", stringify!($name), "` 的编号")]
                    pub const [<$name:upper>]: PropId = PropId(Index::$name as u16);
                )*
            }

            /// 按名字找属性
            pub(crate) fn by_name(name: &str) -> Option<PropId> {
                match name {
                    $(stringify!($name) => Some(PropId::[<$name:upper>]),)*
                    _ => None,
                }
            }

            $(
                fn [<write_ $name>](t: &mut Tree, id: NodeId, value: $ty) {
                    let write: fn(&mut Tree, NodeId, $ty) = $write;
                    write(t, id, value)
                }

                fn [<read_ $name>](t: &Tree, id: NodeId) -> Option<$ty> {
                    let read: fn(&Tree, NodeId) -> Option<$ty> = $read;
                    read(t, id)
                }

                fn [<reset_ $name>](t: &mut Tree, id: NodeId) {
                    reset_fn!([$($reset)*], t, id, [<write_ $name>], $default)
                }
            )*

            /// 注册表：每个属性的元数据和读写函数，按编号
            pub(crate) static REGISTRY: [PropMetadata; COUNT] = [$(
                PropMetadata {
                    id: PropId(Index::$name as u16),
                    name: stringify!($name),
                    doc: concat!($($doc, "\n"),*),
                    value_type: stringify!($ty),
                    storage: Storage::$storage,
                    dirty: Dirty::from_bits_retain($(Dirty::$dirty.bits())|+),
                    inherited: $inherited,
                    animation: anim_spec!($($anim_kind, $tolerance $(, $anim_flag)*)?),
                    applies_to: KindMask::from_bits_retain($(KindMask::$kind.bits())|+),
                    order: $order,
                    has_method: has_method!($method),
                    styleable: $style,
                    late: $late,
                    bind: bind_fn!($bind $(($custom))?, $ty, PropId(Index::$name as u16), [<write_ $name>]),
                    write: |t, id, value| match value.get::<$ty>() {
                        Some(v) => {
                            [<write_ $name>](t, id, v.clone());
                            true
                        }
                        None => false,
                    },
                    read: |t, id| [<read_ $name>](t, id).map(AnyValue::new),
                    reset: [<reset_ $name>],
                    default: || AnyValue::new::<$ty>($default),
                    default_stored: || <$ty as PropType>::store($default),
                    put: |stored, out| {
                        if let Some(out) = out.downcast_mut::<Option<$ty>>() {
                            *out = <$ty as PropType>::peek(stored);
                        }
                    },
                    parse: |text| <$ty as PropType>::parse(text).map(AnyValue::new),
                    format: |value, out| match value.get::<$ty>() {
                        Some(v) => {
                            PropType::format(v, out);
                            true
                        }
                        None => false,
                    },
                },
            )*];

            impl Element {
                $( prop_method!($method; $name; $ty; [<$name:upper>]; $($doc)*); )*
            }

            impl Style {
                $( style_method!($style; $method; $name; $ty; [<$name:upper>]; $($doc)*); )*
            }

            /// 还不是元素、建好才是的东西（控件的构建器）上的属性方法：属性表里每个属性一个，
            /// 实现了 [`crate::ElementMethods`] 的类型都有。见 [`crate::ElementMethods`]
            pub trait ElementProps: crate::ElementMethods {
                $( forward_method!($method; $name; $ty; $($doc)*); )*
            }

            impl<T: crate::ElementMethods> ElementProps for T {}
        }
    };
}

macro_rules! anim_spec {
    () => {
        None
    };
    ($kind:ident, $tolerance:literal $(, $flag:ident)*) => {
        Some(AnimationSpec::new(AnimationKind::$kind, $tolerance)$(.$flag())*)
    };
}

/// `Property` 变体的文档：有链式方法的指过去；没有的（布局位置、路径的形状）用属性表里的整段说明
macro_rules! property_doc {
    (manual; $name:ident; $ty:ty; $($doc:literal)*) => {
        concat!($($doc, "\n"),*)
    };
    ($method:ident; $name:ident; $ty:ty; $($doc:literal)*) => {
        concat!("[`Element::", stringify!($name), "`] 设的属性（值是 `", stringify!($ty), "`）")
    };
}

macro_rules! has_method {
    (manual) => {
        false
    };
    ($other:ident) => {
        true
    };
}

macro_rules! bind_fn {
    (typed, $ty:ty, $prop:expr, $write:path) => {
        |tree, id, slot| bind_typed::<$ty>(tree, id, $prop, slot, $write)
    };
    (custom($custom:path), $ty:ty, $prop:expr, $write:path) => {
        $custom
    };
}

macro_rules! reset_fn {
    ([], $t:ident, $id:ident, $write:path, $default:expr) => {
        $write($t, $id, $default)
    };
    ([$($reset:tt)+], $t:ident, $id:ident, $write:path, $default:expr) => {{
        let reset: fn(&mut Tree, NodeId) = $($reset)+;
        reset($t, $id)
    }};
}

/// 链式方法：一行，调用 `set`
macro_rules! prop_method {
    (manual; $($rest:tt)*) => {};
    (exact; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        $(#[doc = $doc])*
        pub fn $name(self, value: impl IntoProp<$ty>) -> Self {
            self.set(keys::$key, value)
        }
    };
    (into; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        $(#[doc = $doc])*
        pub fn $name<V: Into<$ty> + 'static>(self, value: impl IntoProp<V>) -> Self {
            self.set(keys::$key, value)
        }
    };
    (editor; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        $(#[doc = $doc])*
        pub fn $name(mut self, value: impl IntoProp<$ty>) -> Self {
            self.put(keys::$key.id(), value.into_slot(identity));
            self
        }
    };
    (editor_into; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        $(#[doc = $doc])*
        pub fn $name<V: Into<$ty> + 'static>(mut self, value: impl IntoProp<V>) -> Self {
            self.put(keys::$key.id(), value.into_slot(V::into));
            self
        }
    };
}

/// 控件构建器上的同名方法：记下来，建好元素以后调用（见 `element/methods.rs`）
macro_rules! forward_method {
    (manual; $($rest:tt)*) => {};
    (exact; $name:ident; $ty:ty; $($doc:literal)*) => {
        $(#[doc = $doc])*
        fn $name(self, value: impl IntoProp<$ty> + 'static) -> Self {
            self.element(move |el| el.$name(value))
        }
    };
    (into; $name:ident; $ty:ty; $($doc:literal)*) => {
        $(#[doc = $doc])*
        fn $name<V: Into<$ty> + 'static>(self, value: impl IntoProp<V> + 'static) -> Self {
            self.element(move |el| el.$name(value))
        }
    };
    (editor; $($rest:tt)*) => { forward_method!(exact; $($rest)*); };
    (editor_into; $($rest:tt)*) => { forward_method!(into; $($rest)*); };
}

/// 样式对象上的同名方法：由同一条属性定义生成
macro_rules! style_method {
    (false; $($rest:tt)*) => {};
    (true; manual; $($rest:tt)*) => {};
    (true; exact; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        #[doc = concat!("同 [`Element::", stringify!($name), "`]：样式对象里设这个属性")]
        pub fn $name(self, value: impl IntoProp<$ty>) -> Self {
            self.set(keys::$key, value)
        }
    };
    (true; into; $name:ident; $ty:ty; $key:ident; $($doc:literal)*) => {
        #[doc = concat!("同 [`Element::", stringify!($name), "`]：样式对象里设这个属性")]
        pub fn $name<V: Into<$ty> + 'static>(self, value: impl IntoProp<V>) -> Self {
            self.set(keys::$key, value)
        }
    };
    (true; editor; $($rest:tt)*) => {};
    (true; editor_into; $($rest:tt)*) => {};
}

pub(crate) use {anim_spec, bind_fn, forward_method, has_method, prop_method, property_doc, props, reset_fn, style_method};
