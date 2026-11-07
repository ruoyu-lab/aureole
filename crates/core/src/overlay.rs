//! 浮层：浮在窗口里所有内容之上的东西（下拉菜单、弹出面板、右键菜单、对话框、提示）。
//!
//! 浮层声明在一个元素上（[`Element::overlay`]），条件为真时打开。它不在元素的子树里：
//! 画在所有内容之上，不受祖先的裁剪和滚动视口限制；默认锚定在声明它的元素旁边，
//! 放不下时翻到另一边、沿边缘挪进窗口，元素移动、滚动时跟着走。比窗口还高时限制在放得下的高度里，
//! 在里面滚动（见 [`scrollable`]）。元素树里的实现见 `tree/overlays.rs`。
//!
//! 浮层里的按键、滚轮、指针、改值汇总沿声明它的元素往外冒泡（模态浮层是边界）；点击只在浮层里传播，到浮层的根为止，
//! 不传回声明它的元素：菜单项点完不用停止传播，打开菜单的按钮不会把菜单又打开。

use std::cell::Cell;
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use reactive::{Signal, signal};
use scene::{Color, Number};

use animation::Curve;

use crate::element::{Element, View, text};
use crate::handlers::owned;
use crate::property::registry::{PropId, PropType, Slot};
use crate::property::{IntoProp, Prop};
use crate::scroll::ScrollOptions;
use crate::layout::Direction;
use crate::timer::{Timer, after};

/// 浮层放在锚点的哪一边
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// 上方
    Top,
    /// 下方
    Bottom,
    /// 左侧
    Left,
    /// 右侧
    Right,
}

impl Side {
    /// 对面那一边
    pub fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// 沿着那一边怎么对齐
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Edge {
    Start,
    Center,
    End,
}

/// 浮层相对锚点的位置：哪一边 + 沿边怎么对齐。`Placement::bottom().start()` 是下拉菜单
/// （浮层左边和锚点左边对齐），`Placement::top()` 是居中的提示，`Placement::right().start()` 是子菜单
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    pub(crate) side: Side,
    pub(crate) edge: Edge,
}

impl Placement {
    /// 锚点上方，水平居中
    pub const fn top() -> Self {
        Self { side: Side::Top, edge: Edge::Center }
    }

    /// 锚点下方，水平居中（默认）
    pub const fn bottom() -> Self {
        Self { side: Side::Bottom, edge: Edge::Center }
    }

    /// 锚点左侧，竖直居中
    pub const fn left() -> Self {
        Self { side: Side::Left, edge: Edge::Center }
    }

    /// 锚点右侧，竖直居中
    pub const fn right() -> Self {
        Self { side: Side::Right, edge: Edge::Center }
    }

    /// 和锚点的起始边对齐（上下两边时是左边，左右两边时是上边）
    pub const fn start(mut self) -> Self {
        self.edge = Edge::Start;
        self
    }

    /// 沿边居中
    pub const fn center(mut self) -> Self {
        self.edge = Edge::Center;
        self
    }

    /// 和锚点的末尾边对齐（上下两边时是右边，左右两边时是下边）
    pub const fn end(mut self) -> Self {
        self.edge = Edge::End;
        self
    }

    /// 放在哪一边
    pub fn side(&self) -> Side {
        self.side
    }
}

impl Default for Placement {
    fn default() -> Self {
        Self::bottom()
    }
}

/// 浮层为什么要关：交给 [`Overlay::on_dismiss`]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Dismiss {
    /// 按了 Esc
    Escape,
    /// 在浮层外面按下了指针（点锚点元素自己不算，它通常自己切换开关）
    Outside,
}

/// 浮层锚定在哪
#[derive(Clone)]
pub(crate) enum AnchorSpec {
    /// 声明浮层的元素
    Owner,
    /// 窗口里的一点（右键菜单）
    Point(Rc<dyn Fn() -> [f32; 2]>),
    /// 窗口正中（对话框）
    Window,
}

/// 浮层的描述，用 [`Element::overlay`] 挂到元素上。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # fn menu() -> Element { column() }
/// let open = signal(false);
/// button("选项")
///     .on_click(move || open.update(|o| *o = !*o))
///     .overlay(open, Overlay::new(menu)
///         .placement(Placement::bottom().start())
///         .offset(6)
///         .on_dismiss(move |_| open.set(false)));
/// ```
///
/// 入场动画在内容的 `on_mount` 里触发，退场动画用内容根元素上的 `.exit(..)`：关闭时先播完退场动画再卸载，
/// 期间浮层不再响应指针。
pub struct Overlay {
    pub(crate) view: Rc<dyn Fn() -> Element>,
    pub(crate) anchor: AnchorSpec,
    pub(crate) placement: Placement,
    pub(crate) offset: f32,
    pub(crate) margin: f32,
    pub(crate) modal: bool,
    pub(crate) auto_focus: Option<bool>,
    pub(crate) passthrough: bool,
    pub(crate) backdrop: Option<Rc<dyn Fn() -> Color>>,
    pub(crate) backdrop_blur: f32,
    pub(crate) backdrop_transition: Option<Curve>,
    pub(crate) on_dismiss: Option<Rc<dyn Fn(Dismiss)>>,
    pub(crate) placed: Option<Signal<Placement>>,
}

impl Overlay {
    /// `view` 在每次打开时调用，构建浮层的内容（里面的状态随浮层关闭释放）
    pub fn new<V: View>(view: impl Fn() -> V + 'static) -> Self {
        Self {
            view: Rc::new(move || view().into_element()),
            anchor: AnchorSpec::Owner,
            placement: Placement::bottom(),
            offset: 0.0,
            margin: 8.0,
            modal: false,
            auto_focus: None,
            passthrough: false,
            backdrop: None,
            backdrop_blur: 0.0,
            backdrop_transition: None,
            on_dismiss: None,
            placed: None,
        }
    }

    /// 放在锚点的哪一边、怎么对齐（默认下方居中）。那一边放不下而另一边放得下时翻过去
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// 和锚点之间的距离，逻辑像素
    pub fn offset(mut self, offset: impl Number) -> Self {
        self.offset = offset.to_f32();
        self
    }

    /// 离窗口边缘至少留多少（默认 8）：放不下时沿边缘挪进来
    pub fn margin(mut self, margin: impl Number) -> Self {
        self.margin = margin.to_f32();
        self
    }

    /// 锚定在窗口里的一点（窗口坐标，可以绑定信号），右键菜单用：`.at(point).placement(Placement::bottom().start())`
    pub fn at(mut self, point: impl IntoProp<[f32; 2]>) -> Self {
        self.anchor = AnchorSpec::Point(match point.into_prop() {
            Prop::Static(p) => Rc::new(move || p),
            Prop::Dynamic(f) => Rc::from(f),
        });
        self
    }

    /// 放在窗口正中（对话框）；`false` 回到锚定在声明它的元素上
    pub fn centered(mut self, centered: bool) -> Self {
        if centered {
            self.anchor = AnchorSpec::Window;
        } else if matches!(self.anchor, AnchorSpec::Window) {
            self.anchor = AnchorSpec::Owner;
        }
        self
    }

    /// 模态：挡住下面所有内容的指针输入，Tab 只在浮层里转，打开时让里面第一个能获得焦点的元素获得焦点，
    /// 关闭时焦点还给打开前的元素
    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    /// 打开时让里面第一个能获得焦点的元素获得焦点（模态浮层默认是），关闭时还回去
    pub fn auto_focus(mut self, focus: bool) -> Self {
        self.auto_focus = Some(focus);
        self
    }

    /// 背景遮罩的颜色（铺满窗口，挡住指针），可以绑定信号。打开、关闭时立即出现、消失；
    /// 要淡入淡出用 [`Overlay::backdrop_transition`]
    pub fn backdrop(mut self, color: impl IntoProp<Color>) -> Self {
        self.backdrop = Some(match color.into_prop() {
            Prop::Static(c) => Rc::new(move || c),
            Prop::Dynamic(f) => Rc::from(f),
        });
        self
    }

    /// 遮罩下面的内容模糊（标准差，逻辑像素），和遮罩一起出现；没设遮罩颜色时用透明的遮罩
    pub fn backdrop_blur(mut self, sigma: impl Number) -> Self {
        self.backdrop_blur = sigma.to_f32();
        self.backdrop.get_or_insert_with(|| Rc::new(|| Color::TRANSPARENT));
        self
    }

    /// 遮罩（颜色和背景模糊）打开时按这条曲线淡入，关闭时按它淡出，浮层等它淡完才卸载。默认不做动画
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # fn dialog() -> Element { column() }
    /// let open = signal(false);
    /// rect().overlay(open, Overlay::new(dialog).centered(true).modal(true)
    ///     .backdrop(Color::BLACK.with_alpha(0.3))
    ///     .backdrop_transition(Tween::new(ms(150)).ease_out()));
    /// ```
    pub fn backdrop_transition(mut self, curve: impl Into<Curve>) -> Self {
        self.backdrop_transition = Some(curve.into());
        self
    }

    /// 不接收指针（提示）：指针直接落到下面的内容上
    pub fn passthrough(mut self, passthrough: bool) -> Self {
        self.passthrough = passthrough;
        self
    }

    /// 用户想关掉浮层（按 Esc、在外面按下指针）。浮层不自己关，由处理器改掉控制它的信号
    pub fn on_dismiss<R>(mut self, handler: impl Fn(Dismiss) -> R + 'static) -> Self {
        self.on_dismiss = Some(owned(handler));
        self
    }

    /// 实际放的位置（翻到另一边之后的）写进这个信号，内容可以按它决定箭头朝向、从哪边滑入
    pub fn report_placement(mut self, placement: Signal<Placement>) -> Self {
        self.placed = Some(placement);
        self
    }
}

/// 浮层内容的根元素变成跟着内容长高的竖向视口（和 [`crate::scroll_y_fit`] 一样）：外观（背景、边框、圆角、投影、
/// 透明度、变换）、大小、事件、动画、无障碍属性留在根元素上；子元素连同排列方式（方向、间距、对齐、内边距、网格）
/// 挪进里面跟着滚的一层。放得下时和原来一样（不裁剪、不接滚轮）；浮层放不下时宿主限制它的最大高度，
/// 里面就滚动起来，表面的圆角、描边、投影照常画在外面。
///
/// 已经是滚动视口的、没有子元素的（文字这类）不改：它们自己处理（或者不需要）滚动
pub(crate) fn scrollable(mut root: Element) -> Element {
    let leaf = root.kind.is_some();
    if root.scroll.is_some() || leaf || root.children.is_empty() {
        return root;
    }
    let mut inner = Element { children: mem::take(&mut root.children), grid_layout: mem::take(&mut root.grid_layout), ..Default::default() };
    // 根元素没设方向时是横排，里面那一层也一样
    if let Some(direction) = root.props.take(PropId::DIRECTION) {
        inner.props.put(PropId::DIRECTION, direction.slot, direction.span);
    }
    root.put(PropId::DIRECTION, Slot::Static(Direction::Column.store()));
    // 怎么排列子元素的属性挪进去：间距、对齐、内边距、网格、换行
    for id in ARRANGEMENT {
        if let Some(entry) = root.props.take(id) {
            inner.props.put(id, entry.slot, entry.span);
        }
    }
    // 内边距、间距的动画跟着属性挪进去
    let (moved, kept) = mem::take(&mut root.animations).into_iter().partition(|(property, _)| matches!(*property, PropId::PADDING | PropId::GAP));
    (inner.animations, root.animations) = (moved, kept);
    // 元素自己要求裁剪时一直裁剪；否则只在内容放不下（滚动）时裁剪
    let clips = root.props.take(PropId::CLIP).and_then(|e| e.slot.get::<bool>()).unwrap_or(false);
    root.put(PropId::CLIP, Slot::Static(true.store()));
    let holder = Element::default().direction(Direction::Column);
    let mut config = ScrollOptions::new([false, true], holder, inner);
    config.surface = Some(clips);
    root.scroll = Some(Box::new(config));
    root
}

/// 浮层的表面挪进里面那一层的属性：怎么排列子元素（间距、对齐、内边距、网格、换行）
const ARRANGEMENT: [PropId; 15] = [
    PropId::GAP,
    PropId::ALIGN,
    PropId::JUSTIFY,
    PropId::PADDING,
    PropId::PADDING_X,
    PropId::PADDING_Y,
    PropId::WRAP,
    PropId::COLUMNS,
    PropId::ROWS,
    PropId::AUTO_ROWS,
    PropId::AUTO_COLUMNS,
    PropId::JUSTIFY_ITEMS,
    PropId::ALIGN_CONTENT,
    PropId::GAP_X,
    PropId::GAP_Y,
];

/// 元素上声明的一个浮层
pub(crate) struct OverlaySpec {
    pub show: Prop<bool>,
    pub overlay: Overlay,
}

impl Element {
    /// 浮层：`show` 为真时打开，变假时关闭（先播退场动画）。默认锚定在这个元素上；
    /// 元素卸载时浮层一起卸载。内容的文字样式从这个元素继承
    pub fn overlay(mut self, show: impl IntoProp<bool>, overlay: Overlay) -> Self {
        self.interaction().overlays.push(OverlaySpec { show: show.into_prop(), overlay });
        self
    }

    /// 提示：指针停在元素上 0.5 秒后在上方显示一小段文字（深灰底白字、圆角 4、字号 12），移开或按下时消失，
    /// 出现、消失都是立即的。不挡指针
    pub fn tooltip<S: Into<String> + 'static>(self, label: impl IntoProp<S>) -> Self {
        let label: Rc<dyn Fn() -> String> = match label.into_prop().map(S::into) {
            Prop::Static(s) => Rc::new(move || s.clone()),
            Prop::Dynamic(f) => Rc::from(f),
        };
        let show = signal(false);
        let timer: Rc<Cell<Option<Timer>>> = Rc::default();
        let pending = timer.clone();
        // 提示的文字也交给辅助技术（当描述）
        self.accessibility_tooltip(label.clone()).on_hover(move |inside| {
            if let Some(t) = pending.take() {
                t.cancel();
            }
            if inside {
                pending.set(Some(after(TOOLTIP_DELAY, move || show.set(true))));
            } else {
                show.set(false);
            }
        })
        .on_press(move |_| {
            if let Some(t) = timer.take() {
                t.cancel();
            }
            show.set(false);
        })
        .overlay(
            show,
            Overlay::new(move || {
                let label = label.clone();
                text(move || label())
                    .font_size(12)
                    .padding_x(8)
                    .padding_y(4)
                    .radius(4)
                    .background(|| crate::defaults::palette().tooltip)
                    .color(|| crate::defaults::palette().on_tooltip)
            })
            .placement(Placement::top())
            .offset(6.0)
            .passthrough(true),
        )
    }
}

/// 指针停多久显示提示
const TOOLTIP_DELAY: Duration = Duration::from_millis(500);

/// 点（浮层的锚点）也能直接传
impl IntoProp<[f32; 2]> for [f32; 2] {
    fn into_prop(self) -> Prop<[f32; 2]> {
        Prop::Static(self)
    }
}
