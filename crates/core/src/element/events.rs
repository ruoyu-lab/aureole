//! 元素上登记的处理器：点击、悬停、按下、焦点、提交、拖入文件、挂载，输入框的改动和改完。
//!
//! 处理器都放进元素的处理器表（[`HandlerList`]，见 `handlers.rs`），挂载时整张表交给节点；
//! 别处的链式方法（按键、指针、拖放、无障碍、富文本的链接）也登记在同一张表里。
//! 这个模块不碰属性（`element/property_methods.rs`）。

use super::Element;
use crate::event::{ClickEvent, ExternalDragEvent, ExternalDropEvent, FileDropEvent, IntoChangeHandler, IntoHandler};
use crate::handlers::{EventType, Handler, HandlerList, owned};

/// 元素上登记的处理器，挂载时交给节点
#[derive(Default)]
pub(crate) struct Events {
    /// 按事件种类登记的处理器
    pub handlers: HandlerList,
    pub on_mount: Option<Box<dyn FnOnce()>>,
}

impl Element {
    /// 在 `build` 里挂的处理器是这个元素的**内置处理器**，给自己做的控件用：控件自己的行为写在这里，
    /// 使用控件的人直接挂在元素上的 `on_*` 排在它们前面。
    ///
    /// 同一个元素、同一个阶段（捕获或者目标和冒泡）上，内置处理器在别的处理器之后调用，不管链式调用时谁写在前面。
    /// 这样使用控件的人总能先看到事件，决定控件要不要照常动作：
    ///
    /// - 取消：使用的人调用 `e.prevent_builtin(Builtin::ACTIVATE)`，内置处理器用 `e.is_builtin_prevented(..)`
    ///   查到以后不动作。查哪一种由控件决定（按下去激活的查 [`crate::Builtin::ACTIVATE`]，方向键的查 [`crate::Builtin::ARROWS`]），
    ///   框架不替它判断；
    /// - 接管：使用的人调用 `e.stop_propagation()`。同一个元素上剩下的处理器照样调用，所以内置处理器要自己用
    ///   `e.is_propagation_stopped()` 查（按键常这样写）；
    /// - 内置处理器处理完通常也调用 `e.stop_propagation()`，外面的可点击元素不再收到。基础控件都是这样做的。
    ///
    /// 只有会传播的事件才分先后（指针按下、松开、移动，点击，按键，滚轮，手势，改值汇总）；只发给元素自己的事件
    /// （`on_hover`、`on_focus`、`on_change`……）在 `build` 里挂也按登记的顺序调用。`build` 里只挂处理器：
    /// 里面新建的子元素，它们的处理器也算内置处理器。
    ///
    /// ```
    /// use aureole::prelude::*;
    ///
    /// /// 一个开关：点击切换
    /// fn toggle(on: Signal<bool>) -> Element {
    ///     rect().size(40, 20).builtin_handlers(|el| {
    ///         el.on_click(move |e: &ClickEvent| {
    ///             if !e.is_builtin_prevented(Builtin::ACTIVATE) {
    ///                 on.update(|v| *v = !*v);
    ///             }
    ///             e.stop_propagation();
    ///         })
    ///     })
    /// }
    ///
    /// // 使用的人：写在后面也先收到点击，可以不让它切换
    /// # let on = signal(false);
    /// # let locked = signal(true);
    /// toggle(on).on_click(move |e: &ClickEvent| {
    ///     if locked.get_untracked() {
    ///         e.prevent_builtin(Builtin::ACTIVATE);
    ///     }
    /// });
    /// ```
    pub fn builtin_handlers(self, build: impl FnOnce(Self) -> Self) -> Self {
        crate::handlers::builtin_handlers(|| build(self))
    }

    /// 登记一个处理器（冒泡和目标阶段）
    pub(crate) fn listen(mut self, kind: EventType, handler: Handler) -> Self {
        self.events.handlers.push(kind, handler);
        self
    }

    /// 登记一个捕获阶段的处理器
    pub(crate) fn listen_capture(mut self, kind: EventType, handler: Handler) -> Self {
        self.events.handlers.push_capture(kind, handler);
        self
    }

    /// 元素挂载、属性都绑定好之后调用一次。可以在这里改信号来触发入场动画：
    /// 初始值不做动画，挂载之后的变化才做。处理器的返回值不用。
    /// 调用几次就挂几个，按调用的顺序执行（组件自己挂的不会被使用的人挂的顶掉）
    pub fn on_mount<R>(mut self, f: impl FnOnce() -> R + 'static) -> Self {
        let before = self.events.on_mount.take();
        self.events.on_mount = Some(Box::new(move || {
            if let Some(before) = before {
                before();
            }
            f();
        }));
        self
    }

    /// 点击：在元素上按下并松开指针左键；有焦点时按回车、空格；辅助技术请求点击；点了它的标签（[`ClickEvent::source`]）。
    /// 点击交给"按下和松开都在其下"的最内层元素，往外冒泡（禁用的元素跳过）：嵌套的可点击元素都收到，
    /// 里面的处理完不想让外面再收到就调用 [`ClickEvent::stop_propagation`]。基础控件处理完自己的点击都停止传播，
    /// 输入框也不让点击往外传。浮层里的点击只在浮层里传播，到浮层的根为止，不传回声明浮层的元素（菜单项点完不用停止传播）。
    ///
    /// 处理器可以不带参数，也可以收事件对象（写出参数的类型，见 [`IntoHandler`]）。
    /// 这个和其他处理器都可以有返回值，返回值不用：`.on_click(|| aureole::open_url(".."))`、
    /// `.on_click(move || aureole::spawn_local(save()))` 可以直接写，不用加花括号和分号
    pub fn on_click<M>(self, handler: impl IntoHandler<ClickEvent, M>) -> Self {
        self.listen(EventType::Click, Handler::Click(handler.into_handler()))
    }

    /// 点击的捕获阶段：从外往里，在里面的元素之前收到，见 [`Element::on_click`]
    pub fn on_click_capture<M>(self, handler: impl IntoHandler<ClickEvent, M>) -> Self {
        self.listen_capture(EventType::Click, Handler::Click(handler.into_handler()))
    }

    /// 输入框：用户改了内容（打字、删除、粘贴、剪切、撤销、输入法提交、辅助技术设置）时调用，参数是新的内容
    /// （或者整个 [`crate::ChangeEvent`]，带着是怎么改的）。代码里改绑定的信号时不调用；输入法组字期间不调用，提交时才调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<String, M>) -> Self {
        if self.kind::<crate::editor::EditorBlock>().is_some() {
            self.events.handlers.push(EventType::Change, Handler::TextChange(handler.into_change_handler()));
        }
        self
    }

    /// 输入框：改完了（回车，或者失去焦点）时调用，参数是内容；内容和获得焦点时（上次改完时）一样就不调用。
    /// 多行输入框只在失去焦点和 [`Element::submit_key`] 的键时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<String, M>) -> Self {
        if self.kind::<crate::editor::EditorBlock>().is_some() {
            self.events.handlers.push(EventType::Commit, Handler::TextChange(handler.into_change_handler()));
        }
        self
    }

    /// 获得（`true`）或失去（`false`）焦点：用指针、键盘、焦点句柄、辅助技术都算；所有能获得焦点的元素都有（输入框、可选中的文字、`focusable` 的元素、基础控件）
    pub fn on_focus<R>(mut self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.events.handlers.push(EventType::Focus, Handler::Flag(owned(handler)));
        self
    }

    /// 在输入框里提交（单行输入框按回车；多行输入框见 [`Element::submit_key`]），参数是当前内容
    pub fn on_submit<R>(mut self, handler: impl Fn(String) -> R + 'static) -> Self {
        self.events.handlers.push(EventType::Submit, Handler::Text(owned(handler)));
        self
    }

    /// 字符网格（`char_grid`）：输入法、听写提交了文字，参数是要插入的文字（应用把它写进终端程序）。
    /// 设了它，网格有焦点时打开系统的输入法（用途是终端），组字时的预编辑画在插入点处；没设时不开输入法，
    /// 打字只走 [`Element::on_key_down`]。别的元素上不调用
    pub fn on_text_insert<R>(mut self, handler: impl Fn(String) -> R + 'static) -> Self {
        #[cfg(feature = "char-grid")]
        if let Some(grid) = self.kind_mut::<crate::char_grid::CharGridBlock>() {
            grid.text_input = true;
        }
        self.events.handlers.push(EventType::TextInsert, Handler::Text(owned(handler)));
        self
    }

    /// 指针在元素上按下（`true`）和松开（`false`），用于按压反馈
    pub fn on_press<R>(self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.listen(EventType::Press, Handler::Flag(owned(handler)))
    }

    /// 指针进入（`true`）或离开（`false`）元素。子元素上方也算在元素里
    pub fn on_hover<R>(self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.listen(EventType::Hover, Handler::Flag(owned(handler)))
    }

    /// 拖着文件进入（`true`）或离开（`false`）这个元素，含子元素；用来高亮放置区域
    pub fn on_file_hover<R>(self, handler: impl Fn(bool) -> R + 'static) -> Self {
        self.listen(EventType::FileHover, Handler::Flag(owned(handler)))
    }

    /// 文件放到这个元素上（从系统的访达、桌面拖进来），事件里有文件和放下的位置。
    /// 放在子元素上时，交给最内层有这个处理器的元素
    pub fn on_file_drop<M>(self, handler: impl IntoHandler<FileDropEvent, M>) -> Self {
        self.listen(EventType::FileDrop, Handler::FileDrop(handler.into_handler()))
    }

    /// 别的应用拖来的内容（文字、链接、图片、文件）经过这个元素：进来时和之后位置变了时调用。
    /// 调用 [`ExternalDragEvent::accept`] 才算接受，系统显示能放下的光标；不调用就接着问外面一层的元素，问到接受的为止。
    /// 这时只知道内容有哪几种（[`ExternalDragEvent::kinds`]），放下时才读得到内容。
    ///
    /// 不支持的平台（`aureole::capabilities().supports_external_drop` 是假）上只有文件会来
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let hovering = signal(false);
    /// rect()
    ///     .on_external_drag_over(move |e: &ExternalDragEvent| {
    ///         if e.kinds.intersects(ExternalDataKinds::TEXT | ExternalDataKinds::LINK) {
    ///             e.accept();
    ///             hovering.set(true);
    ///         }
    ///     })
    ///     .on_external_drag_leave(move || hovering.set(false))
    ///     .on_external_drop(|e: &ExternalDropEvent| println!("{:?} {:?}", e.text, e.link));
    /// ```
    pub fn on_external_drag_over<M>(self, handler: impl IntoHandler<ExternalDragEvent, M>) -> Self {
        self.listen(EventType::ExternalDragOver, Handler::ExternalDrag(handler.into_handler()))
    }

    /// 别的应用拖来的内容离开这个元素（放下、取消也算）。这个元素之前收到过 [`Element::on_external_drag_over`] 才有
    pub fn on_external_drag_leave<M>(self, handler: impl IntoHandler<ExternalDragEvent, M>) -> Self {
        self.listen(EventType::ExternalDragLeave, Handler::ExternalDrag(handler.into_handler()))
    }

    /// 别的应用拖来的内容放到这个元素上，事件里有文字、链接、图片、文件和位置。
    /// 只有它、没有 [`Element::on_external_drag_over`] 的元素什么内容都接受；
    /// 有的话要在经过时接受了才收得到（放下时按放下的位置再问一次）。
    /// 拖文件时和 [`Element::on_file_drop`] 互不影响，各自按自己的处理器找元素
    pub fn on_external_drop<M>(self, handler: impl IntoHandler<ExternalDropEvent, M>) -> Self {
        self.listen(EventType::ExternalDrop, Handler::ExternalDrop(handler.into_handler()))
    }
}
