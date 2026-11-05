//! 元素：建造者 [`Element`] 本身、子元素和结构（名字、子元素、`each`）、各种元素的构造函数。
//!
//! 属性的链式方法由属性表（`tree/property_table.rs`）生成，都只是调用 [`Element::set`]；一次设几个属性的简写、
//! 过渡和退场在 `element/property_methods.rs`，处理器的登记（`on_click` 之类）在 `element/events.rs`。
//! 它们都只通过 [`Element`] 上的字段 `props`、`events` 碰元素。

mod events;
mod methods;
mod property_methods;

pub use methods::ElementMethods;

use std::hash::Hash;
use std::sync::Arc;

use animation::Curve;
use reactive::Signal;

use crate::property::PropertyValue;
use crate::dynamic::{Dynamic, Each};
use crate::editor::EditorBlock;
use crate::property::IntoProp;
use crate::property::registry::{FromPropValue, PropKey, PropId, Props, Slot};
use crate::scroll::ScrollOptions;
use crate::kinds::ElementKind;
use crate::layout::Direction;
use crate::vector::VectorPath;

pub(crate) use events::Events;
pub(crate) use property_methods::RichProps;

/// 能放进元素树的东西。组件就是返回 `impl View` 的普通函数。
pub trait View {
    /// 转成元素
    fn into_element(self) -> Element;
}

/// 横向排列子元素的容器
pub fn row() -> Element {
    Element::default().direction(Direction::Row)
}

/// 纵向排列子元素的容器
pub fn column() -> Element {
    Element::default().direction(Direction::Column)
}

/// 一个矩形，也可以有子元素（默认横向排列）
pub fn rect() -> Element {
    Element::default()
}

/// 单行输入框（基础控件之一），内容和 `value` 双向同步：输入时写回信号（输入法预编辑期间不写，
/// 提交后才写），外部改信号时更新输入框。支持中文输入法、选区、撤销重做（Command+Z / Shift+Command+Z）、
/// Command+A/C/X/V、Option 按词移动、Command 到行首行尾；[`Element::password`] 是密码模式。
///
/// 默认的样子：宽 200、高 30、左右内边距 8、圆角 4，白底加 1 像素灰色描边，悬停时描边深一点，
/// 有焦点时描边变强调色（不另画焦点环），禁用时变淡。都是一个样式对象先用上的，后面的链式方法、
/// `.style(&s)` 照常盖过它（见 [`Element::style`]）；状态有悬停、焦点（有焦点就算）、禁用。
/// 输入框的宽度不随内容变化。用户改了内容时调用 [`Element::on_change`]，改完时调用 [`Element::on_commit`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let name = signal(String::new());
/// text_field(name).placeholder("名字").width(240).on_commit(|name| println!("改成了 {name}"));
/// // 没有框的样子：去掉描边和底色
/// text_field(name).style(&Style::new().border(0, Color::TRANSPARENT).background(Color::TRANSPARENT).padding_x(0));
/// ```
pub fn text_field(value: Signal<String>) -> Element {
    let el = Element::from_kind(EditorBlock::new(value, false));
    el.focus_ring(crate::interaction::FocusRing::NONE).style(&crate::defaults::text_field_look(false))
}

/// 多行输入框（基础控件之一），内容和 `value` 双向同步。按元素宽度自动换行，回车换行（[`Element::submit_key`] 可以改成提交）。
/// 高度跟着内容长（用 `min_height` / `max_height` 限制），超过时在里面滚动，插入点始终滚在看得见的地方；
/// 也可以直接给固定高度或 `grow`。默认的样子和 [`text_field`] 一样（宽 320、最小高 64、四边内边距 8）。
///
/// 插入点移动：方向键（Option 按词、Command 到行首行尾和文档首尾、Option+↑↓ 到段首段尾）、Home / End、
/// Page Up / Down；Shift 扩展选区。指针：点击、拖选（可以跨行）、双击选词、三击选整段。
/// 中文输入法的候选框跟着任意一行上的插入点走。撤销重做、剪贴板和 [`text_field`] 一样。
///
/// 是一个元素，内部自己带滚动（滚动的手感、橡皮筋、滚动条和滚动视口一样，滚动也在渲染线程上推进）：
/// 背景、边框、圆角、内边距、大小、文字样式都直接设在它上面。Tab 默认留给焦点切换，[`Element::accepts_tab`] 让它插入制表符。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let notes = signal(String::new());
/// text_area(notes)
///     .placeholder("写点什么…")
///     .width(420).min_height(120).max_height(360)
///     .padding(12).radius(10).background(Color::hex(0xfbfbfc))
///     .submit_key(SubmitKey::PrimaryEnter)
///     .on_submit(|text| println!("{text}"));
/// ```
pub fn text_area(value: Signal<String>) -> Element {
    let el = Element::from_kind(EditorBlock::new(value, true)).clip(true);
    el.focus_ring(crate::interaction::FocusRing::NONE).style(&crate::defaults::text_field_look(true))
}

/// 文字。内容可以是固定的字符串（字面量、`String`，或者任何 `&str` / `&String`，复制一份），
/// 也可以绑定：`text(move || format!("{} 项", n.get()))`。
/// 宽度不够时自动换行；中英文混排、emoji 由系统字体自动回退
pub fn text<S: Into<String> + 'static>(content: impl IntoProp<S>) -> Element {
    Element::from_kind(crate::text::TextBlock::default()).set(crate::property::TEXT, content)
}

/// 矢量路径：图标、自定义形状、折线图。默认用文字颜色（[`Element::color`]，可继承）填充，
/// 所以图标会跟着周围文字的颜色走；[`Element::stroke`] 改成描边。路径可以绑定信号（比如图表数据）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let check = VectorPath::svg("M4 12 L9 17 L20 6").view_box(0.0, 0.0, 24.0, 24.0);
/// path(check).size(16, 16).stroke(Stroke::new(2.0).round()).color(Color::hex(0x30d158));
/// ```
pub fn path(path: impl IntoProp<VectorPath>) -> Element {
    Element::from_kind(crate::vector::PathBlock::default()).set(crate::property::PATH, path)
}

/// 元素的描述，挂载时才变成元素树里的节点。
///
/// 每个属性方法都接受 [`IntoProp`]：静态值、闭包、`Signal` 或 `Memo`。
/// 布局属性变化会触发重新布局；绘制属性（颜色、圆角、阴影、透明度、
/// 平移、缩放、旋转）变化只重建绘制数据，不碰布局。
/// 用 [`Element::transition`] 声明的属性，变化时按曲线过渡。
#[derive(Default)]
pub struct Element {
    /// 设了的属性，按注册表的编号排好（也就是挂载时绑定的顺序），一个属性一条
    pub(crate) props: Props,
    /// 网格容器（[`crate::grid`]）
    pub(crate) grid_layout: bool,
    pub(crate) children: Vec<Child>,
    /// 处理器（见 `element/events.rs`）
    pub(crate) events: Events,
    /// 声明了过渡的属性和曲线
    pub(crate) animations: Vec<(PropId, Curve)>,
    /// 退场时过渡到的值
    pub(crate) exit: Vec<(PropId, PropertyValue, Option<Curve>)>,
    /// 加入的关键帧时间线句柄（[`Element::motion_handle`]）
    pub(crate) motion: Vec<u64>,
    /// 滚动视口（`scroll_x` / `scroll_y` / `scroll`）
    pub(crate) scroll: Option<Box<ScrollOptions>>,
    /// 叠放次序、指针形状、焦点、指针手势、按键、浮层（见 `interaction.rs`、`overlay.rs`）
    pub(crate) input: Option<Box<crate::interaction::Interaction>>,
    /// 角色、名字、状态等无障碍属性（见 `accessibility.rs`）
    pub(crate) accessibility: Option<Box<crate::accessibility::AccessibilityProps>>,
    /// 绕排容器：哪些子元素绕排（见 `flow.rs`）
    pub(crate) flow: Option<Box<crate::flow::FlowProps>>,
    /// 富文本的各段、链接的处理器（见 `rich_text.rs`）
    pub(crate) rich: Option<Box<RichProps>>,
    /// 图片的淡入、加载状态、播放句柄（见 `image.rs`）
    pub(crate) image: Option<Box<crate::image::ImageExtras>>,
    /// 元素的种类（[`Element::from_kind`]）；容器没有
    pub(crate) kind: Option<Box<dyn ElementKind>>,
}

/// 自定义着色器：用 WGSL 写一个函数
///
/// ```wgsl
/// fn shade(in: ShaderInput) -> vec4<f32> {
///     // in.uv：元素里的位置（0..1）；in.size：元素大小；in.time：秒；
///     // in.params：元素上传入的 4 个数；in.color：元素的背景色
///     return vec4(in.uv, 0.5 + 0.5 * sin(in.time), 1.0);
/// }
/// ```
///
/// 返回 sRGB、未预乘的颜色（和 `Color::hex` 写的颜色一样）。着色器在渲染线程上第一次用到时编译，
/// 出错时记一条错误（`tracing`）、不画。用到 `in.time` 的着色器要标记 [`Shader::animated`]，
/// 渲染线程才会在它可见时每帧重画（不经过主线程）。
#[derive(Clone)]
pub struct Shader {
    pub(crate) source: Arc<str>,
    pub(crate) animated: bool,
}

/// 同一份源码（同一个 `Arc`）、同样的设置才算一样：着色器按源码登记
impl PartialEq for Shader {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source) && self.animated == other.animated
    }
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shader").field("source", &self.source.len()).field("animated", &self.animated).finish()
    }
}

impl Shader {
    /// 用 WGSL 源码创建；不会立即编译
    pub fn new(source: impl Into<Arc<str>>) -> Self {
        Self { source: source.into(), animated: false }
    }

    /// 画面随时间变化（用到了 `in.time`）
    pub fn animated(mut self, animated: bool) -> Self {
        self.animated = animated;
        self
    }
}

/// 子元素：挂载时确定的静态元素，或者由 `each` / `child_when` 维护的一组
pub(crate) enum Child {
    Static(Box<Element>),
    Dynamic(Box<dyn Dynamic>),
}

impl View for Element {
    fn into_element(self) -> Element {
        self
    }
}

/// 一段固定的文字：`.child("说明")`、`button("保存")`，同 [`text`]
impl View for &str {
    fn into_element(self) -> Element {
        text(self)
    }
}

/// 一段固定的文字，同 [`text`]
impl View for String {
    fn into_element(self) -> Element {
        text(self)
    }
}

impl Element {
    /// 一种带内容的元素：内容由 `kind` 量大小、画出来（见 [`crate::kinds`]）。返回的元素和别的元素一样设属性、挂事件、
    /// 放子元素。内置的画布就是这样做的：
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// # use aureole::kinds::{Drawing, ElementKind, PaintContext};
    /// /// 一个画着对角线的方块
    /// struct Cross(Drawing);
    ///
    /// impl ElementKind for Cross {
    ///     fn name(&self) -> &'static str {
    ///         "cross"
    ///     }
    ///
    ///     fn paint(&mut self, cx: &mut PaintContext) {
    ///         let [w, h] = cx.size();
    ///         self.0.record([w, h], cx.scale_factor(), |c| c.line([0.0, 0.0], [w, h], Stroke::new(2.0), Color::hex(0x138a84)));
    ///         self.0.paint(cx);
    ///     }
    /// }
    ///
    /// let cross = Element::from_kind(Cross(Drawing::new())).size(40, 40);
    /// ```
    pub fn from_kind(kind: impl ElementKind) -> Element {
        Element { kind: Some(Box::new(kind)), ..Default::default() }
    }

    /// 元素的种类对象（[`Element::from_kind`] 给的），换成具体的类型：自己的种类在工厂函数之后还要改设置时用
    pub fn kind<K: ElementKind>(&self) -> Option<&K> {
        self.kind.as_deref()?.downcast_ref()
    }

    /// 同 [`Element::kind`]，可以改
    pub fn kind_mut<K: ElementKind>(&mut self) -> Option<&mut K> {
        self.kind.as_deref_mut()?.downcast_mut()
    }

    /// 按键设一个属性：`rect().set(property::OPACITY, 0.5)`，和 `.opacity(0.5)` 一样（属性的链式方法就是调用它）。
    /// 值可以是静态值、闭包、信号、派生值，能转换的和链式方法一样转换（`Color` 转成 `Fill`、数字转成 `Length`，
    /// 见 [`FromPropValue`]）；同一个属性设几次时后设的算数
    pub fn set<T: FromPropValue<V>, V: 'static>(mut self, key: PropKey<T>, value: impl IntoProp<V>) -> Self {
        self.props.put(key.id(), value.into_slot(T::from_prop_value), false);
        self
    }

    /// 设一个属性（已经换成建造者里的值）
    pub(crate) fn put(&mut self, id: PropId, slot: Slot) {
        self.props.put(id, slot, false);
    }

    /// 添加一个子元素
    pub fn child(mut self, child: impl View) -> Self {
        self.children.push(Child::Static(Box::new(child.into_element())));
        self
    }

    /// 添加一组子元素（固定的，不随数据变化；会变的列表用 [`Element::each`]）
    pub fn children<V: View>(mut self, children: impl IntoIterator<Item = V>) -> Self {
        self.children.extend(children.into_iter().map(|child| Child::Static(Box::new(child.into_element()))));
        self
    }

    /// 随数据增删的子元素列表，按 `key` 增量更新：key 不变的项保留节点和状态，
    /// 新 key 才创建，消失的 key 连同它的绑定和信号一起释放，顺序变化只是重排。
    ///
    /// 每一项的视图只在它第一次出现时构建一次；项里会变的字段请放进信号。
    /// 可以和 `child` 混用，按声明顺序排列。
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// #[derive(Clone)]
    /// struct Todo { id: u32, title: String, done: Signal<bool> }
    ///
    /// let todos = signal(Vec::<Todo>::new());
    /// column().each(move || todos.get(), |todo| todo.id, |todo| {
    ///     text(todo.title).color(move || if todo.done.get() { Color::hex(0x8e8e93) } else { Color::WHITE })
    /// });
    /// ```
    pub fn each<T, Key, V>(
        mut self,
        items: impl Fn() -> Vec<T> + 'static,
        key: impl Fn(&T) -> Key + 'static,
        view: impl Fn(T) -> V + 'static,
    ) -> Self
    where
        T: 'static,
        Key: Eq + Hash + 'static,
        V: View,
    {
        self.children.push(Child::Dynamic(Box::new(Each { items, key, view })));
        self
    }

    /// 条件为真时显示子元素；条件变假时连同它的绑定和信号一起释放
    pub fn child_when<V: View>(self, when: impl Fn() -> bool + 'static, view: impl Fn() -> V + 'static) -> Self {
        self.each(move || if when() { vec![()] } else { Vec::new() }, |_| (), move |_| view())
    }

}
