//! 元素种类：带内容的元素（文字、输入框、路径、图片、字符网格、画布，以及应用自己定义的元素）共用的接口。
//!
//! 元素树里的节点分两类：只有布局和绘制属性的容器（`rect`、`row`、`column`、`grid`……），和带内容的元素。
//! 带内容的元素在节点上挂一个实现了 [`ElementKind`] 的对象，元素树在这几个时候调用它：
//!
//! | 时候 | 方法 | 做什么 |
//! |---|---|---|
//! | 布局量叶子 | [`ElementKind::measure`]、[`ElementKind::baselines`] | 已知的大小、可用的空间 → 内容区的大小；基线 |
//! | 继承的文字样式变了 | [`ElementKind::restyle`] | 记下新样式，返回要重做什么 |
//! | 加载了新字体 | [`ElementKind::refresh_fonts`] | 丢掉排版缓存，返回要重做什么 |
//! | 每帧（声明了 [`KindHooks::TICK`]，或者有焦点） | [`ElementKind::tick`]、[`ElementKind::next_frame`] | 推进自己的时钟（插入点闪烁）；要不要下一帧 |
//! | 有了大小、读到的信号变了（声明了 [`KindHooks::UPDATE`]） | [`ElementKind::update`] | 在元素树外面执行用户代码 |
//! | 生成显示列表 | [`ElementKind::paint`] | 往显示列表里写图元 |
//! | 卸载 | [`ElementKind::unmount`] | 还回占着的资源 |
//! | 元素树的快照 | [`ElementKind::name`]、[`ElementKind::describe`] | 认出这个元素、读它的状态 |
//! | 指针、按键、焦点 | [`ElementKind::input_target`] → [`InputTarget`] | 拿焦点、选字、编辑键（输入框、可选中的文字、字符网格都是这样接进来的） |
//! | 输入法、听写 | [`ElementKind::text_input_client`] → [`TextInputClient`] | 预编辑、提交、取消组字、替换文字 |
//! | 辅助技术 | [`ElementKind::accessibility_provider`] → [`AccessibilityProvider`] | 角色、值、按行的文字片段、链接、动作 |
//!
//! 自己定义一种元素：实现 [`ElementKind`]，用 [`crate::Element::from_kind`] 做成元素，再像别的元素一样设属性、挂事件。
//! 内容用 [`Drawing`] 画（和 [`crate::canvas()`] 的画笔一样）；要读信号的放在 [`ElementKind::update`] 里，信号变了
//! 自动再调用。内置的画布就是这样做的。要能按名字创建它，用 [`register_kind`] 登记。
//!
//! 只有带内容的元素才有这个对象，绘制时只有它们才多一次虚调用；容器走原来的快路径。

mod accessibility;
mod input;

use std::any::Any;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use scene::Color;

pub use crate::canvas::{Canvas, DescribedRegion, Drawing};
pub use accessibility::{AccessibilityAction, AccessibilityContext, AccessibilityProvider};
pub(crate) use accessibility::child_id;
pub use input::{Focusable, Handled, InputTarget, SubTarget, TargetContext, TextInputClient};
use crate::element::Element;
use crate::geometry::Rect;
use crate::property::registry::{Dirty, KindMask, PropId};
use crate::text::{TextStyle, TextSystem};

/// 一种带内容的元素。见[模块说明](self)。
///
/// 除了 [`ElementKind::name`] 和 [`ElementKind::paint`]，方法都有什么都不做的默认实现。
/// 种类对象由工厂函数建好（[`crate::Element::from_kind`]），挂载时挪进节点，卸载时丢掉；同一个对象只属于一个元素。
pub trait ElementKind: Any {
    /// 种类的名字（蛇形）：`"text"`、`"canvas"`、`"gauge"`。元素树的快照、种类的登记表（[`kind_spec`]）按它认出元素，
    /// 内置种类的名字和它们的工厂函数同名
    fn name(&self) -> &'static str;

    /// 属于哪几类元素：属性适用于哪些元素（[`crate::property::PropMetadata::applies_to`]）按它判断，设在不适用的元素上的属性不绑定。
    /// 自己定义的种类默认是 [`KindMask::CUSTOM`]：只收任何元素都适用的属性（布局、背景、变换、事件……）
    fn kind_mask(&self) -> KindMask {
        KindMask::CUSTOM
    }

    /// 要参与哪些每帧的步骤（挂载时读一次）
    fn hooks(&self) -> KindHooks {
        KindHooks::empty()
    }

    /// 量内容区（去掉内边距和边框）的大小。`known` 是已经确定的宽、高（样式定了大小、或者被拉伸），
    /// `available` 是父元素给的空间。只在布局要量这个元素、而且上次的结果不能用时才调用。
    /// 默认：已知的边用已知的，未知的是 0（没有自己的大小，靠样式给）
    fn measure(&mut self, cx: &mut MeasureContext, known: [Option<f32>; 2], available: [AvailableSpace; 2]) -> [f32; 2] {
        let _ = (cx, available);
        [known[0].unwrap_or(0.0), known[1].unwrap_or(0.0)]
    }

    /// 内容的第一行、最后一行的基线在哪（相对内容区顶部，逻辑像素）：父元素按基线对齐时用。
    /// 布局量完这个元素以后读，默认没有基线
    fn baselines(&self) -> Option<(f32, f32)> {
        None
    }

    /// 从父元素继承的文字样式算好了（有元素改了文字样式、增删了子元素时，沿树往下传一遍）：记下要用的，
    /// 返回要重做什么（[`Dirty::LAYOUT`] 重新量这个元素，[`Dirty::PAINT`] 重画）
    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        let _ = cx;
        Dirty::empty()
    }

    /// 加载了新字体（缺字回退的结果可能变了）：丢掉排版的缓存，返回要重做什么
    fn refresh_fonts(&mut self) -> Dirty {
        Dirty::empty()
    }

    /// 在元素树外面更新（声明了 [`KindHooks::UPDATE`] 时）：这里可以执行用户的代码、读信号。
    /// 元素有了大小（不是 0）以后调用第一次；之后这次读过的信号变了、或者元素的大小变了，下一帧再调用。
    /// 调用完重画这个元素
    fn update(&mut self, cx: &mut UpdateContext) {
        let _ = cx;
    }

    /// 推进自己的时钟（声明了 [`KindHooks::TICK`]，或者元素有焦点时，每帧在布局之前调用）：插入点闪烁这类。
    /// 返回要重做什么
    fn tick(&mut self, cx: &TickContext) -> Dirty {
        let _ = cx;
        Dirty::empty()
    }

    /// 下一帧什么时候要（和 [`ElementKind::tick`] 一样，声明了 [`KindHooks::TICK`] 或者有焦点时才问）。
    /// 都是 [`NextFrame::Idle`] 而且别的也没在动时，窗口不出帧
    fn next_frame(&self, now: Duration) -> NextFrame {
        let _ = now;
        NextFrame::Idle
    }

    /// 把内容写进显示列表。坐标、变换、透明度、裁剪由 [`PaintContext`] 管；用 [`Drawing::paint`] 把画好的东西放进去
    fn paint(&mut self, cx: &mut PaintContext);

    /// 元素卸载了：还回占着的资源（[`Drawing::release`]）
    fn unmount(&mut self, cx: &mut UnmountContext) {
        let _ = cx;
    }

    /// 描述自己：元素树的快照里一行的说明、显示的文字、别的状态
    fn describe(&self, info: &mut KindInfo) {
        let _ = info;
    }

    /// 接收指针、按键的能力（见 [`InputTarget`]）。默认没有：事件只交给元素上登记的处理器。
    /// 种类自己实现了的，这里和 [`ElementKind::input_target_mut`] 都返回 `Some(self)`
    fn input_target(&self) -> Option<&dyn InputTarget> {
        None
    }

    /// 同 [`ElementKind::input_target`]，可以改
    fn input_target_mut(&mut self) -> Option<&mut dyn InputTarget> {
        None
    }

    /// 接收输入法、听写、辅助技术改文字的能力（见 [`TextInputClient`]）。默认没有：焦点在它上面时不开输入法
    fn text_input_client(&self) -> Option<&dyn TextInputClient> {
        None
    }

    /// 同 [`ElementKind::text_input_client`]，可以改
    fn text_input_client_mut(&mut self) -> Option<&mut dyn TextInputClient> {
        None
    }

    /// 向辅助技术描述内容的能力（见 [`AccessibilityProvider`]）。默认没有：按元素上声明的无障碍属性
    fn accessibility_provider(&self) -> Option<&dyn AccessibilityProvider> {
        None
    }

    /// 同 [`ElementKind::accessibility_provider`]，可以改（处理辅助技术的动作）
    fn accessibility_provider_mut(&mut self) -> Option<&mut dyn AccessibilityProvider> {
        None
    }
}

impl dyn ElementKind {
    /// 是不是 `K` 这种
    pub fn is<K: ElementKind>(&self) -> bool {
        (self as &dyn Any).is::<K>()
    }

    /// 换成具体的类型
    pub fn downcast_ref<K: ElementKind>(&self) -> Option<&K> {
        (self as &dyn Any).downcast_ref()
    }

    /// 换成具体的类型
    pub fn downcast_mut<K: ElementKind>(&mut self) -> Option<&mut K> {
        (self as &mut dyn Any).downcast_mut()
    }
}

bitflags::bitflags! {
    /// 种类要参与的每帧步骤（[`ElementKind::hooks`]，挂载时读一次）
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct KindHooks: u8 {
        /// 每帧调用 [`ElementKind::tick`]、问 [`ElementKind::next_frame`]（没有焦点时也是）
        const TICK = 1;
        /// 在元素树外面更新（[`ElementKind::update`]）：读到的信号变了、大小变了再调用
        const UPDATE = 1 << 1;
        /// 量出来的大小和窗口缩放有关（按物理像素取整）：缩放变了重新量
        const SCALE_FACTOR = 1 << 2;
    }
}

/// 下一帧什么时候要（[`ElementKind::next_frame`]）
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NextFrame {
    /// 不要：画面静止时窗口不出帧
    Idle,
    /// 到这个时刻（和 [`TickContext::now`] 同一个时钟）要一帧
    At(Duration),
    /// 每帧都要
    Continuous,
}

/// 父元素给的空间（[`ElementKind::measure`]）
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AvailableSpace {
    /// 最多这么宽（高），逻辑像素
    Definite(f32),
    /// 量最小的大小（能断行的地方都断开）
    MinContent,
    /// 量不受限制时的大小（不断行）
    MaxContent,
}

impl AvailableSpace {
    /// 布局库给的空间换成这里的（不让布局库的类型出现在公开的接口里）
    pub(crate) fn from_layout(space: taffy::AvailableSpace) -> Self {
        match space {
            taffy::AvailableSpace::Definite(v) => Self::Definite(v),
            taffy::AvailableSpace::MinContent => Self::MinContent,
            taffy::AvailableSpace::MaxContent => Self::MaxContent,
        }
    }
}

/// 量大小时能用的（[`ElementKind::measure`]）
pub struct MeasureContext<'a> {
    pub(crate) text: &'a mut TextSystem,
}

impl MeasureContext<'_> {
    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub fn scale_factor(&self) -> f32 {
        self.text.scale()
    }
}

/// 继承下来的文字样式（[`ElementKind::restyle`]）
pub struct StyleContext {
    pub(crate) style: TextStyle,
    /// 选区的颜色（有焦点时）
    pub(crate) selection: Color,
    /// 没有焦点时选区的颜色
    pub(crate) inactive: Color,
    /// 输入框占位文字的颜色：文字是默认的正文色时用默认的占位色，自己定了文字颜色时是 `None`（按文字颜色变淡）
    pub(crate) placeholder: Option<Color>,
}

impl StyleContext {
    /// 字号，逻辑像素
    pub fn font_size(&self) -> f32 {
        self.style.size
    }

    /// 字重（400 常规，700 粗）
    pub fn font_weight(&self) -> f32 {
        self.style.weight
    }

    /// 行高，字号的倍数
    pub fn line_height(&self) -> f32 {
        self.style.line_height
    }

    /// 选区的颜色
    pub fn selection_color(&self) -> Color {
        self.selection
    }
}

/// 在元素树外面更新时能用的（[`ElementKind::update`]）
pub struct UpdateContext {
    pub(crate) size: [f32; 2],
    pub(crate) scale: f32,
    /// 要重做什么：至少重画
    pub(crate) dirty: Dirty,
    /// 量文字（[`crate::Drawing::record_in`] 交给画笔）
    pub(crate) text: Option<crate::canvas::TextMeasurer>,
}

impl UpdateContext {
    /// 元素的大小（布局之后的，含内边距），逻辑像素
    pub fn size(&self) -> [f32; 2] {
        self.size
    }

    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub fn scale_factor(&self) -> f32 {
        self.scale
    }

    /// 内容的大小变了：重新量这个元素（[`ElementKind::measure`]）
    pub fn request_layout(&mut self) {
        self.dirty |= Dirty::LAYOUT;
    }

    /// 给辅助技术的描述变了（值来自信号，代码里改了信号）：重建这个元素的无障碍节点。
    /// 辅助技术没打开时没有开销
    pub fn request_accessibility(&mut self) {
        self.dirty |= Dirty::ACCESSIBILITY;
    }
}

/// 推进时钟时能用的（[`ElementKind::tick`]）
pub struct TickContext {
    /// 这一帧的时间，秒
    pub(crate) seconds: f64,
    pub(crate) focused: bool,
}

impl TickContext {
    /// 这一帧的时间（从统一的时钟起算；测试工具里是虚拟时间）
    pub fn now(&self) -> Duration {
        Duration::from_secs_f64(self.seconds.max(0.0))
    }

    /// 元素有键盘焦点
    pub fn is_focused(&self) -> bool {
        self.focused
    }
}

/// 卸载时能用的（[`ElementKind::unmount`]）
pub struct UnmountContext<'a> {
    pub(crate) text: &'a mut TextSystem,
    pub(crate) images: &'a mut crate::image::ImageSystem,
}

/// 生成显示列表时能用的（[`ElementKind::paint`]）。坐标是元素自己的：左上角是原点，逻辑像素，
/// 元素的位置、变换、所在的合成层、外面的裁剪由元素树加上
pub struct PaintContext<'a> {
    pub(crate) tree: &'a mut crate::tree::Tree,
    pub(crate) id: crate::tree::NodeId,
    pub(crate) area: &'a crate::tree::ContentArea,
    pub(crate) layers: &'a mut Vec<crate::compositor::LayerInfo>,
    pub(crate) list: &'a mut scene::DisplayList,
    /// 继承的文字颜色（渐变已经登记好）
    pub(crate) ink: crate::text::Ink,
    /// 从这个下标起的图元自己设好了层和裁剪（[`Drawing::paint`] 放进去的）
    pub(crate) placed: Option<usize>,
}

impl PaintContext<'_> {
    /// 元素的大小（含内边距），逻辑像素
    pub fn size(&self) -> [f32; 2] {
        [self.area.bounds.width, self.area.bounds.height]
    }

    /// 内容区（去掉内边距和边框）：在元素自己的坐标里
    pub fn content_rect(&self) -> Rect {
        let (b, c) = (self.area.bounds, self.area.content);
        Rect { x: c.x - b.x, y: c.y - b.y, width: c.width, height: c.height }
    }

    /// 透明度：自己和祖先的累积（合成层的透明度由渲染线程另乘）
    pub fn opacity(&self) -> f32 {
        self.area.opacity
    }

    /// 窗口缩放（逻辑像素 -> 物理像素）
    pub fn scale_factor(&self) -> f32 {
        self.tree.scale()
    }
}

/// 种类对自己的描述（[`ElementKind::describe`]）
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KindInfo {
    /// 元素树快照里一行的简短说明：文字的内容、图片的来源
    pub summary: Option<String>,
    /// 元素显示的文字（文字、输入框的内容）：测试工具的 `text_content` 读它
    pub text: Option<String>,
    /// 别的状态：名字和值
    pub fields: Vec<(&'static str, String)>,
}

/// 一种元素的登记：名字、属于哪几类（适用的属性按它从属性表里筛）、做一个这种元素的函数。
/// 按名字找它（[`kind_spec`]）
#[derive(Clone, Copy, Debug)]
pub struct KindSpec {
    /// 名字：和 [`ElementKind::name`]、工厂函数同名
    pub name: &'static str,
    /// 属于哪几类元素
    pub kind_mask: KindMask,
    /// 做一个这种元素（默认的样子、空的内容）
    pub create: fn() -> Element,
}

impl KindSpec {
    /// 适用于这种元素的属性，按绑定的顺序
    pub fn properties(&self) -> impl Iterator<Item = PropId> + '_ {
        PropId::all().filter(|id| id.metadata().applies_to.intersects(self.kind_mask))
    }
}

/// 内置的种类：容器和带内容的元素
const BUILTIN: &[KindSpec] = &[
    KindSpec { name: "rect", kind_mask: KindMask::CONTAINER, create: crate::element::rect },
    KindSpec { name: "row", kind_mask: KindMask::CONTAINER, create: crate::element::row },
    KindSpec { name: "column", kind_mask: KindMask::CONTAINER, create: crate::element::column },
    KindSpec { name: "grid", kind_mask: KindMask::CONTAINER.union(KindMask::GRID), create: crate::layout::grid },
    KindSpec { name: "flow", kind_mask: KindMask::CONTAINER.union(KindMask::FLOW), create: crate::flow::flow },
    KindSpec { name: "scroll", kind_mask: KindMask::CONTAINER.union(KindMask::SCROLL), create: || crate::scroll::scroll_y(crate::element::column()) },
    KindSpec { name: "text", kind_mask: KindMask::TEXT, create: || crate::element::text("") },
    KindSpec { name: "rich_text", kind_mask: KindMask::TEXT, create: crate::rich_text::rich_text },
    KindSpec { name: "text_field", kind_mask: KindMask::EDITOR, create: || crate::element::text_field(reactive::signal(String::new())) },
    KindSpec {
        name: "text_area",
        kind_mask: KindMask::EDITOR.union(KindMask::SCROLL),
        create: || crate::element::text_area(reactive::signal(String::new())),
    },
    KindSpec { name: "path", kind_mask: KindMask::PATH, create: || crate::element::path(crate::vector::VectorPath::svg("")) },
    KindSpec { name: "image", kind_mask: KindMask::IMAGE, create: || crate::image::image(crate::image::ImageSource::rgba(0, 0, Vec::new())) },
    KindSpec { name: "canvas", kind_mask: KindMask::CANVAS, create: || crate::canvas::canvas(|_| {}) },
    #[cfg(feature = "char-grid")]
    KindSpec { name: "char_grid", kind_mask: KindMask::CHAR_GRID, create: || crate::char_grid::char_grid(&crate::char_grid::CharGrid::new(80, 24)) },
];

/// 应用登记的种类
static REGISTERED: RwLock<Vec<KindSpec>> = RwLock::new(Vec::new());

/// 登记一种自己定义的元素，之后能按名字找到并创建它（[`kind_spec`]）。和已经登记的（包括内置的）同名时替换掉
pub fn register_kind(spec: KindSpec) {
    let mut registered = REGISTERED.write().unwrap_or_else(PoisonError::into_inner);
    match registered.iter_mut().find(|s| s.name == spec.name) {
        Some(existing) => *existing = spec,
        None => registered.push(spec),
    }
}

/// 按名字找一种元素：先找应用登记的，再找内置的
pub fn kind_spec(name: &str) -> Option<KindSpec> {
    let registered = REGISTERED.read().unwrap_or_else(PoisonError::into_inner);
    registered.iter().chain(BUILTIN).find(|s| s.name == name).copied()
}

/// 全部种类：内置的在前（被应用登记的同名种类替换掉的不算），然后是应用登记的
pub fn kind_specs() -> Vec<KindSpec> {
    let registered = REGISTERED.read().unwrap_or_else(PoisonError::into_inner);
    let builtin = BUILTIN.iter().filter(|b| !registered.iter().any(|r| r.name == b.name));
    builtin.chain(registered.iter()).copied().collect()
}
