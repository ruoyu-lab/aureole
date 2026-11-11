//! 拖放：元素之间（带着任意类型的数据，放置目标按类型和条件接收）、拖到应用外面（文件、文字交给访达和其他应用）。
//!
//! 拖动源用 [`Element::draggable`]，放置目标用 [`Element::on_drop`]（参数的类型决定接收什么）、
//! [`Element::accepts_drop`]（再加条件）、[`Element::on_drag_enter`] / [`Element::on_drag_over`] /
//! [`Element::on_drag_leave`]。拖到应用外面用 [`Element::drag_out`]。元素树里怎么分发见 `tree/drag_drop.rs`。

use std::any::{Any, TypeId};
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use reactive::Scope;

use crate::element::Element;
use crate::image::RgbaImage;
use crate::rich_text::{Span, SpanInfo};
use crate::event::IntoHandler;
use crate::handlers::{EventType, Handler, owned, owned_ref};
use crate::input::Modifiers;

/// 拖动时跟着指针走的样子，见 [`Element::drag_preview`]。[`Element::drag_preview_axis`] 让它只沿一个方向走，
/// [`Element::drag_preview_contained`] 让它不出父元素
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DragPreview {
    /// 元素的一份半透明拷贝浮在最上面跟着指针走（单独的合成层，不受祖先裁剪，移动时不重新布局、不重画），
    /// 原来的元素留在原处、变淡。放下后元素还在的话拷贝落到它的新位置，没被接住时飞回去
    #[default]
    Copy,
    /// 元素自己跟着指针走（在它的合成层上），画在兄弟上面；松手后从指针处落到它的新位置。
    /// 仍在原来的父元素里，会被祖先裁剪：适合在同一个列表里排序（父元素裁剪时加上 [`Element::drag_preview_contained`]）
    Lift,
    /// 不显示，应用自己画（用 [`Element::on_drag`] 的位置）
    None,
}

/// 拖着的数据经过放置目标、放下时的事件
#[derive(Clone)]
pub struct DropEvent {
    /// 目标元素自己的坐标（去掉了变换，滚动视口里的元素已经算上了滚动）
    pub position: [f32; 2],
    /// 窗口坐标，逻辑像素
    pub window_position: [f32; 2],
    /// 此刻按住的修饰键
    pub modifiers: Modifiers,
    pub(crate) data: Rc<dyn Any>,
}

impl DropEvent {
    /// 拖着的数据，类型不对时是 `None`
    pub fn data<T: 'static>(&self) -> Option<&T> {
        self.data.downcast_ref()
    }
}

impl fmt::Debug for DropEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DropEvent").field("position", &self.position).field("window_position", &self.window_position).finish_non_exhaustive()
    }
}

/// 拖到应用外面的内容，见 [`Element::drag_out`]。和剪贴板一样，同一份内容可以有几种表示，接收的程序挑自己认的：
/// `DragOut::image(图).with_text("说明")`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DragOut {
    files: Vec<PathBuf>,
    text: Option<String>,
    rich_text: Vec<SpanInfo>,
    image: Option<RgbaImage>,
    custom: Vec<(String, Vec<u8>)>,
}

impl DragOut {
    /// 一个或几个文件（要已经存在）：拖到访达里是拷贝过去，拖到其他应用里是打开或插入
    pub fn files(paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self::default().with_files(paths)
    }

    /// 一个文件
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::files([path])
    }

    /// 纯文本：拖到文本编辑器里插入，拖到访达里存成文本剪辑
    pub fn text(text: impl Into<String>) -> Self {
        Self::default().with_text(text)
    }

    /// 带格式的文字（一串 [`Span`]）：认带格式文字的程序插入带样式的文字，别的程序拿到它的纯文字（没给纯文字时是这些段连起来）
    pub fn rich_text(spans: impl IntoIterator<Item = impl Into<Span>>) -> Self {
        Self::default().with_rich_text(spans)
    }

    /// 图片：拖到能放图片的程序里插入，拖到访达、资源管理器里存成图片文件（看系统）
    pub fn image(image: impl Into<RgbaImage>) -> Self {
        Self::default().with_image(image)
    }

    /// 应用自己定的格式（和剪贴板的 `custom` 一样：反向域名的格式名，字节原样交出去）。拖到同一个应用的别的窗口、
    /// 同一个应用的别的实例里时，接收方按名字读（[`Element::on_external_drop`] 收到的数据里）
    pub fn custom(format: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self::default().with_custom(format, bytes)
    }

    /// 换成这些文件
    pub fn with_files(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.files = paths.into_iter().map(Into::into).collect();
        self
    }

    /// 换成这段纯文字
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// 换成这段带格式的文字
    pub fn with_rich_text(mut self, spans: impl IntoIterator<Item = impl Into<Span>>) -> Self {
        self.rich_text = spans.into_iter().map(|span| span.into().info()).collect();
        self
    }

    /// 换成这张图片
    pub fn with_image(mut self, image: impl Into<RgbaImage>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// 加一种应用自己定的格式；同一个格式名放两次时后放的算数
    pub fn with_custom(mut self, format: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        let format = format.into();
        self.custom.retain(|(name, _)| *name != format);
        self.custom.push((format, bytes.into()));
        self
    }

    /// 要拖的文件
    pub fn paths(&self) -> &[PathBuf] {
        &self.files
    }

    /// 要拖的文字
    pub fn text_content(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// 要拖的带格式的文字（空的是没有）
    pub fn rich_text_content(&self) -> &[SpanInfo] {
        &self.rich_text
    }

    /// 要拖的图片
    pub fn image_content(&self) -> Option<&RgbaImage> {
        self.image.as_ref()
    }

    /// 要拖的应用自己定的格式
    pub fn custom_content(&self) -> &[(String, Vec<u8>)] {
        &self.custom
    }

    /// 没有东西可拖（文件都不存在、文字为空，也没有别的）
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.text.as_deref().is_none_or(str::is_empty) && self.rich_text.is_empty() && self.image.is_none() && self.custom.is_empty()
    }

    /// 去掉不存在的文件（每个记一条警告）
    pub(crate) fn existing(mut self) -> Self {
        self.files.retain(|path| {
            let exists = path.exists();
            if !exists {
                tracing::warn!(path = %path.display(), "拖到应用外面：文件不存在，跳过");
            }
            exists
        });
        self
    }
}

/// 数据的接收条件（类型已经对上）
pub(crate) type Check = Rc<dyn Fn(&dyn Any) -> bool>;
/// 一个类型的接收条件
pub(crate) struct Accept {
    pub ty: TypeId,
    pub check: Check,
}

/// 元素上的拖放设置，用到时才分配
#[derive(Default)]
pub(crate) struct DragDropSpec {
    /// 拖动开始时取数据（`draggable`）
    pub payload: Option<Rc<dyn Fn() -> Rc<dyn Any>>>,
    /// 接收的数据类型（有这个类型的 `on_drop`）
    pub targets: Vec<TypeId>,
    /// 额外的接收条件
    pub accept: Vec<Accept>,
    /// 拖到应用外面
    pub drag_out: Option<Rc<dyn Fn() -> DragOut>>,
}

impl DragDropSpec {
    /// 是不是放置目标
    pub fn is_target(&self) -> bool {
        !self.targets.is_empty()
    }

    /// 接不接这份数据：有这个类型的 `on_drop`，并且这个类型的条件都满足（条件是用户代码，在借用树之外调用）
    pub fn accepts(&self, data: &dyn Any) -> bool {
        let ty = data.type_id();
        self.targets.contains(&ty) && self.accept.iter().filter(|a| a.ty == ty).all(|a| (a.check)(data))
    }
}

impl Element {
    fn drag_drop(&mut self) -> &mut DragDropSpec {
        self.interaction().drag_drop.get_or_insert_default()
    }

    /// 能拖到放置目标上：按下后移动超过 4 像素开始拖，`payload` 在这时调用一次，数据交给目标的
    /// [`Element::on_drop`]（参数的类型和数据一样的目标才接收）。拖的时候默认有一份半透明的拷贝跟着指针走
    /// （[`Element::drag_preview`] 换样式），按 Esc、放在不接收的地方时取消、拷贝飞回去。
    ///
    /// 元素自己的 [`Element::on_drag`] 照常收到 `Start` / `Move`，被目标接住时是 `End`，取消时是 `Cancel`。
    /// 指针靠近滚动视口的边缘时自动滚动
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let done = signal(Vec::<u32>::new());
    /// row()
    ///     .child(text("任务 7").padding(8).draggable(|| 7u32))
    ///     .child(rect().size(200, 120).on_drop(move |id: u32, _| done.update(|d| d.push(id))));
    /// ```
    pub fn draggable<T: 'static>(mut self, payload: impl Fn() -> T + 'static) -> Self {
        let payload = in_scope(move || Rc::new(payload()) as Rc<dyn Any>);
        self.drag_drop().payload = Some(payload);
        self
    }

    /// 放置目标：接收类型是 `T` 的数据（[`Element::draggable`] 给的），放下时调用，参数是数据和位置。
    /// 可以有几个不同类型的；放在子元素上时交给最内层接收它的元素。放下之后接着调用 [`Element::on_drag_leave`]
    pub fn on_drop<T: Clone + 'static, R>(mut self, handler: impl Fn(T, &DropEvent) -> R + 'static) -> Self {
        let handler = owned_ref(move |e: &DropEvent| {
            if let Some(value) = e.data::<T>().cloned() {
                handler(value, e);
            }
        });
        let ty = TypeId::of::<T>();
        if !self.drag_drop().targets.contains(&ty) {
            self.drag_drop().targets.push(ty);
        }
        self.listen(EventType::Drop, Handler::TypedDrop(ty, handler))
    }

    /// 放置目标再加一个条件：类型是 `T` 的数据只有条件为真时才接收（比如不接收已经在这一列里的卡片）。
    /// 不接收时不触发进入、经过、放下，放在这里算取消
    pub fn accepts_drop<T: 'static>(mut self, accept: impl Fn(&T) -> bool + 'static) -> Self {
        let scope = Scope::current();
        let check: Check = Rc::new(move |data: &dyn Any| {
            let Some(value) = data.downcast_ref::<T>() else { return false };
            match scope {
                Some(scope) => scope.run(|| accept(value)),
                None => accept(value),
            }
        });
        self.drag_drop().accept.push(Accept { ty: TypeId::of::<T>(), check });
        self
    }

    /// 拖着它接收的数据进入这个元素（含子元素）：用来高亮放置区域。紧接着有一次 [`Element::on_drag_over`]
    pub fn on_drag_enter<M>(self, handler: impl IntoHandler<DropEvent, M>) -> Self {
        self.listen(EventType::DragEnter, Handler::DropEvent(handler.into_handler()))
    }

    /// 拖着它接收的数据在元素上移动（滚动视口自动滚动、内容在指针下移过去时也算）：
    /// 位置是元素自己的坐标，列表用它算插在第几项
    pub fn on_drag_over<M>(self, handler: impl IntoHandler<DropEvent, M>) -> Self {
        self.listen(EventType::DragOver, Handler::DropEvent(handler.into_handler()))
    }

    /// 拖着的数据离开、放下、取消（Esc、窗口失去焦点）：去掉高亮
    pub fn on_drag_leave<M>(self, handler: impl IntoHandler<DropEvent, M>) -> Self {
        self.listen(EventType::DragLeave, Handler::DropEvent(handler.into_handler()))
    }

    /// 能拖到应用外面（访达、资源管理器、桌面、其他应用）：按下后移动超过 4 像素时调用 `items`，交给系统开始拖
    /// （系统的拖动会话；拖动的图像是文件的图标，只拖图片时是这张图，只拖文字时是纯文本的图标）。
    /// 同时是 [`Element::draggable`] 时先在应用里拖，拖出窗口时才交给系统。结束时调用 [`Element::on_drag_out_end`]。
    ///
    /// macOS、Windows 有；Linux 上什么都不做，`on_drag_out_end` 收到 `false`
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # use aureole::DragOut;
    /// let path = std::env::temp_dir().join("报告.txt");
    /// text("报告.txt").drag_out(move || DragOut::file(path.clone()));
    /// ```
    pub fn drag_out(mut self, items: impl Fn() -> DragOut + 'static) -> Self {
        self.drag_drop().drag_out = Some(in_scope(items));
        self
    }

    /// 拖到应用外面结束：`true` 是被别的应用接住了（比如访达拷贝了文件），`false` 是取消或者这个平台不支持
    pub fn on_drag_out_end<R>(self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.listen(EventType::DragOutEnd, Handler::Flag(owned(handler)))
    }
}

/// 在创建元素时的作用域里调用
fn in_scope<T: 'static>(f: impl Fn() -> T + 'static) -> Rc<dyn Fn() -> T> {
    match Scope::current() {
        Some(scope) => Rc::new(move || scope.run(&f)),
        None => Rc::new(f),
    }
}
