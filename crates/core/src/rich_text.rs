//! 富文本：一段文字里混排不同的字号、颜色、字重、斜体、下划线、删除线、行内代码和链接。
//!
//! 整段交给 parley 一次排版（各段的样式是排版里的区间样式），所以断行、对齐、截断、选择、复制都跨段照常工作；
//! 每段带一个刷子（段号），颜色、链接的悬停按它在绘制时查，改颜色、悬停链接只重画、不重新排版。
//! 元素树里的交互（悬停、点击、Tab、回车、无障碍）见 `tree/links.rs`。
//!
//! 一段也可以是一个元素（[`Span::inline`]）：它是元素树里真的元素，排版里只占一个和它一样大的盒子，见 `tree/inline.rs`。

use std::cell::Cell;
use std::rc::Rc;

use scene::Color;

use crate::element::{Element, View, rect};
use crate::handlers::{EventType, Handler, owned};
use crate::property::{IntoProp, Prop};
use crate::text::{Link, SpanStyle};

/// 富文本里的一段文字：自己的字号、字重、颜色、斜体、下划线、删除线，可以是行内代码或链接。
/// 没设的样式跟着整段（整段的样式设在 [`rich_text`] 元素上，也可以从父元素继承）。
///
/// 用 [`span`] 创建；`&str`、`String` 也能直接当成一段没有单独样式的文字。文字、字号、字重、颜色可以绑定信号
///
/// ```
/// # use aureole::prelude::*;
/// let count = signal(3);
/// rich_text()
///     .span("还有 ")
///     .span(span(move || count.get().to_string()).font_weight(600).color(Color::hex(0x2563eb)))
///     .span(" 项没完成");
/// ```
pub struct Span {
    pub(crate) content: Prop<String>,
    pub(crate) size: Option<Prop<f32>>,
    pub(crate) weight: Option<Prop<f32>>,
    pub(crate) color: Option<Prop<Color>>,
    /// 不绑定的样式（斜体、下划线、删除线、行内代码、字体族、字距）
    pub(crate) style: SpanStyle,
    pub(crate) link: Option<Link>,
    /// 这一段是一个元素（[`Span::inline`]）：文字只是占位字符
    pub(crate) inline: Option<Box<Element>>,
}

/// 一段富文本此刻的内容和样式：[`Span::info`] 读出来，也能再变回一段（`rich_text().span(info)`）。
/// 剪贴板里带格式的文字就是一串这个。文字、字号这些绑定了信号的，读到的是此刻的值
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanInfo {
    /// 文字
    pub text: String,
    /// 字号，逻辑像素；`None` 跟着整段
    pub font_size: Option<f32>,
    /// 字重，100～900；`None` 跟着整段
    pub font_weight: Option<f32>,
    /// 颜色；`None` 跟着整段
    pub color: Option<Color>,
    /// 斜体
    pub italic: bool,
    /// 下划线
    pub underline: bool,
    /// 删除线
    pub strikethrough: bool,
    /// 行内代码
    pub code: bool,
    /// 字体族名；`None` 跟着整段
    pub font_family: Option<String>,
    /// 字距，逻辑像素；`None` 跟着整段
    pub letter_spacing: Option<f32>,
    /// 链接的网址（[`Span::on_click`] 的处理器读不出来）
    pub link: Option<String>,
}

impl From<SpanInfo> for Span {
    fn from(info: SpanInfo) -> Self {
        let mut span = span(info.text).italic(info.italic).underline(info.underline).strikethrough(info.strikethrough).code(info.code);
        if let Some(size) = info.font_size {
            span = span.font_size(size);
        }
        if let Some(weight) = info.font_weight {
            span = span.font_weight(weight);
        }
        if let Some(color) = info.color {
            span = span.color(color);
        }
        if let Some(family) = info.font_family {
            span = span.font_family(family);
        }
        if let Some(spacing) = info.letter_spacing {
            span = span.letter_spacing(spacing);
        }
        if let Some(url) = info.link {
            span = span.link(url);
        }
        span
    }
}

/// 属性此刻的值（不算依赖：只是读一下）
fn current<T: Clone>(prop: &Prop<T>) -> T {
    match prop {
        Prop::Static(value) => value.clone(),
        Prop::Dynamic(f) => reactive::untracked(f),
    }
}

/// 带样式的一段文字，放进 [`rich_text`]：`rich_text().span(span("加粗").font_weight(600))`
pub fn span<S: Into<String> + 'static>(content: impl IntoProp<S>) -> Span {
    Span { content: content.into_prop().map(S::into), size: None, weight: None, color: None, style: SpanStyle::default(), link: None, inline: None }
}

/// 能当行内元素内容的东西（[`Span::inline`]）：一个视图，或者返回视图的闭包（读到的信号变了就换一个新的）。`Marker` 只用来区分这两种
pub trait IntoInline<Marker> {
    /// 转成放进排版的那个元素
    fn into_inline(self) -> Element;
}

/// 视图：挂载一次，会变的放在它的属性里
impl<V: View> IntoInline<fn() -> V> for V {
    fn into_inline(self) -> Element {
        self.into_element()
    }
}

/// 闭包：读到的信号变了就重新调用，新的元素替换旧的（旧的有退场动画时先退场）。外面包一层跟着内容大小的容器，
/// 占位盒子的大小跟着它变
impl<V: View, F: Fn() -> V + 'static> IntoInline<fn(()) -> V> for F {
    fn into_inline(self) -> Element {
        let next = Cell::new(0u64);
        rect().each(
            move || vec![self().into_element()],
            move |_: &Element| {
                next.set(next.get() + 1);
                next.get()
            },
            |element: Element| element,
        )
    }
}

impl From<&str> for Span {
    fn from(content: &str) -> Self {
        span(content)
    }
}

impl From<String> for Span {
    fn from(content: String) -> Self {
        span(content)
    }
}

impl From<&String> for Span {
    fn from(content: &String) -> Self {
        span(content)
    }
}

impl Span {
    /// 一段是一个元素：图片、按钮，或者任何元素。它是元素树里真的元素，自己的布局、绘制、动画、悬停、点击、Tab 焦点、
    /// 无障碍都照常；排版里占一个和它一样大的位置，大小变了（图片加载完、绑定变了）文字重新断行。
    /// 怎么在行里对齐看 [`Element::inline_align`]。
    ///
    /// 内容可以是一个视图，也可以是返回视图的闭包（读到的信号变了就换一个新的）。
    /// 选中时选区能跨过它，复制时换成它的 [`Element::accessibility_name`]，没有时是它里面的文字，都没有时什么都不留。
    /// 宽度不够时整个挪到下一行；[`Element::max_lines`] 截掉的元素不显示、不响应。
    /// [`Span`] 上设文字样式的方法、链接对这一段不起作用
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let done = signal(false);
    /// rich_text()
    ///     .span("点 ")
    ///     .span(Span::inline(rect().size(16, 16).radius(4).background(Color::hex(0x2563eb)).inline_align(InlineAlign::Center)))
    ///     .span(" 开始，状态：")
    ///     .inline(move || if done.get() { text("完成") } else { text("进行中").color(Color::hex(0xc98a12)) }); // 同 `.span(Span::inline(..))`
    /// ```
    pub fn inline<M>(content: impl IntoInline<M>) -> Self {
        let mut segment = span(String::new());
        segment.inline = Some(Box::new(content.into_inline()));
        segment
    }

    /// 这一段此刻的内容和样式；行内元素的这一段什么都没有
    pub fn info(&self) -> SpanInfo {
        if self.inline.is_some() {
            return SpanInfo::default();
        }
        SpanInfo {
            text: current(&self.content),
            font_size: self.size.as_ref().map(current).or(self.style.size),
            font_weight: self.weight.as_ref().map(current).or(self.style.weight),
            color: self.color.as_ref().map(current),
            italic: self.style.italic,
            underline: self.style.underline,
            strikethrough: self.style.strikethrough,
            code: self.style.code,
            font_family: self.style.family.and_then(crate::font::family_name).map(|name| name.to_string()),
            letter_spacing: self.style.letter_spacing,
            link: self.link.as_ref().and_then(|link| link.url.clone()),
        }
    }

    /// 字号，逻辑像素
    pub fn font_size(mut self, size: impl IntoProp<f32>) -> Self {
        self.size = Some(size.into_prop());
        self
    }

    /// 字重，100～900（500 是中等）
    pub fn font_weight(mut self, weight: impl IntoProp<f32>) -> Self {
        self.weight = Some(weight.into_prop());
        self
    }

    /// 颜色；改颜色只重画，不重新排版
    pub fn color(mut self, color: impl IntoProp<Color>) -> Self {
        self.color = Some(color.into_prop());
        self
    }

    /// 斜体（字体没有斜体字形时倾斜着画）
    pub fn italic(mut self, italic: bool) -> Self {
        self.style.italic = italic;
        self
    }

    /// 下划线
    pub fn underline(mut self, underline: bool) -> Self {
        self.style.underline = underline;
        self
    }

    /// 删除线
    pub fn strikethrough(mut self, strikethrough: bool) -> Self {
        self.style.strikethrough = strikethrough;
        self
    }

    /// 行内代码：等宽字体，字号是整段的 0.9 倍，底下一层淡淡的灰底
    pub fn code(mut self, code: bool) -> Self {
        self.style.code = code;
        self
    }

    /// 字体族名（系统里装的，或者用 [`crate::font::load`] 加载的）；这个字体里没有的字从系统字体回退
    pub fn font_family(mut self, family: impl Into<String>) -> Self {
        self.style.family = Some(crate::font::family_id(&family.into()));
        self
    }

    /// 字距，逻辑像素
    pub fn letter_spacing(mut self, spacing: f32) -> Self {
        self.style.letter_spacing = Some(spacing);
        self
    }

    /// 变成链接：点击（或者用 Tab 走到之后按回车、空格）时用系统默认的应用打开 `url`，
    /// 富文本元素上设了 [`Element::on_link_click`] 时交给它。链接是默认的强调色（蓝），指针放上去是手指形状、出现下划线
    pub fn link(mut self, url: impl Into<String>) -> Self {
        self.link.get_or_insert(Link { url: None, on_click: None }).url = Some(url.into());
        self
    }

    /// 变成链接：点击（或者用 Tab 走到之后按回车、空格）时调用。和 [`Span::link`] 一起用时只调用它，网址只给读屏软件
    pub fn on_click<R>(mut self, handler: impl Fn() -> R + 'static) -> Self {
        self.link.get_or_insert(Link { url: None, on_click: None }).on_click = Some(owned(move |()| handler()));
        self
    }
}

/// 富文本：一段文字里混排不同的字号、颜色、字重、斜体、下划线、删除线、行内代码和链接，整段一起排版——
/// 跨段自动换行、对齐、截断（[`Element::max_lines`]）、选中复制（[`Element::selectable`]）都和 [`crate::text()`] 一样。
/// 整段的样式（字号、颜色、行高……）设在这个元素上或者从父元素继承，每段只设和整段不同的。
/// 里面还可以放图片、按钮这样的元素（[`Element::inline`]）。
///
/// ```
/// # use aureole::prelude::*;
/// rich_text()
///     .span("用 ")
///     .code("cargo run")
///     .span(" 启动，详见")
///     .link("文档", "https://example.com/docs")
///     .span("。")
///     .span(span("注意：").font_weight(600).color(Color::hex(0xc98a12)))
///     .span(span("大字").font_size(20))
///     .font_size(15)
///     .selectable(true);
/// ```
pub fn rich_text() -> Element {
    let mut el = Element::from_kind(crate::text::TextBlock::default());
    el.rich = Some(Box::new(crate::element::RichProps { rich: true, ..Default::default() }));
    el
}

impl Element {
    /// 富文本里加一段：`&str`、`String`，或者 [`span`] 做的带样式的一段。
    /// 用在 [`crate::text()`] 上时，原来的文字成为第一段
    pub fn span(mut self, segment: impl Into<Span>) -> Self {
        if !self.rich.as_ref().is_some_and(|r| r.rich) {
            let content = self.props.take(crate::property::TEXT.id()).and_then(|e| e.slot.into_prop::<String>());
            let rich = self.rich_props();
            rich.rich = true;
            rich.spans.extend(content.map(span));
        }
        self.rich_props().spans.push(segment.into());
        self
    }

    /// 富文本的各段；用在还没有种类的元素上时，它成为文字元素
    fn rich_props(&mut self) -> &mut crate::element::RichProps {
        if self.kind.is_none() {
            self.kind = Some(Box::new(crate::text::TextBlock::default()));
        }
        self.rich.get_or_insert_with(Box::default)
    }

    /// 富文本里放一个元素（图片、按钮……），等于 `.span(Span::inline(content))`，见 [`Span::inline`]
    pub fn inline<M>(self, content: impl IntoInline<M>) -> Self {
        self.span(Span::inline(content))
    }

    /// 富文本里加一段行内代码（等宽字体、淡淡的底色），等于 `.span(span(content).code())`
    pub fn code<S: Into<String> + 'static>(self, content: impl IntoProp<S>) -> Self {
        self.span(span(content).code(true))
    }

    /// 富文本里加一个链接，等于 `.span(span(content).link(url))`，见 [`Span::link`]
    pub fn link<S: Into<String> + 'static>(self, content: impl IntoProp<S>, url: impl Into<String>) -> Self {
        self.span(span(content).link(url))
    }

    /// 点击富文本里带网址的链接时调用（参数是网址），代替用系统打开：应用里的跳转、先确认再打开
    pub fn on_link_click<R>(self, handler: impl Fn(String) -> R + 'static) -> Self {
        self.listen(EventType::LinkClick, Handler::Text(owned(handler)))
    }
}

/// 链接被点击时要调用的：自己的处理器，或者带着网址的 `on_link_click` 处理器，或者用系统打开网址
pub(crate) enum LinkAction {
    Call(Rc<dyn Fn(())>),
    Route(Vec<Rc<dyn Fn(String)>>, String),
    Open(String),
}

impl LinkAction {
    pub fn new(link: &Link, on_link: Vec<Rc<dyn Fn(String)>>) -> Option<Self> {
        if let Some(handler) = &link.on_click {
            return Some(Self::Call(handler.clone()));
        }
        let url = link.url.clone()?;
        Some(if on_link.is_empty() { Self::Open(url) } else { Self::Route(on_link, url) })
    }

    /// 调用（在批量更新里、已经释放树的借用）
    pub fn run(self) {
        match self {
            Self::Call(handler) => handler(()),
            Self::Route(handlers, url) => handlers.iter().for_each(|h| h(url.clone())),
            Self::Open(url) => {
                crate::system::open_url(&url);
            }
        }
    }
}
