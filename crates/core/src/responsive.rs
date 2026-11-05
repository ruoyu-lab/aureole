//! 按窗口大小切换样式：尺寸档（[`window_size_class`]）、断点（[`is_window_at_least`]）、按档取值（[`responsive`]）。
//!
//! 每个窗口（和每个测试工具）有一份自己的窗口大小信号，挂在窗口的作用域上（[`reactive::provide_context`]）；
//! 窗口里的绑定在哪个窗口里执行，就读哪个窗口的大小，多窗口互不影响。尺寸档、断点都是派生值：
//! 拖动改变窗口大小时它们每帧重算一次（比较一个数），只有跨过分档宽度时值才变，读它的绑定才重新执行。

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use reactive::{Memo, Scope, Signal, memo, signal};

use crate::geometry::Rect;
use crate::host::{WindowId, current_window_id};

/// 窗口宽度的档位，见 [`window_size_class`]。可以比较大小：`Compact < Regular < Wide`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SizeClass {
    /// 窄：默认宽度小于 640
    Compact,
    /// 中等：默认 640 到 1024
    #[default]
    Regular,
    /// 宽：默认 1024 及以上
    Wide,
}

impl SizeClass {
    /// 按分档宽度（[`set_size_class_widths`]）给出 `width` 属于哪一档。在绑定里调用时，改分档宽度也会让它重新执行；
    /// 量到的容器宽度（[`crate::Element::report_bounds`]）也可以这样分档
    pub fn from_width(width: f32) -> Self {
        let [regular, wide] = thresholds().get();
        if width >= wide {
            Self::Wide
        } else if width >= regular {
            Self::Regular
        } else {
            Self::Compact
        }
    }

    /// 是否是窄档
    pub fn is_compact(self) -> bool {
        self == Self::Compact
    }

    /// 是否是宽档
    pub fn is_wide(self) -> bool {
        self == Self::Wide
    }

    /// 按档位取一个值：`class.pick(12.0, 16.0, 24.0)`
    pub fn pick<T>(self, compact: T, regular: T, wide: T) -> T {
        match self {
            Self::Compact => compact,
            Self::Regular => regular,
            Self::Wide => wide,
        }
    }
}

thread_local! {
    /// 分档宽度 [中等, 宽]，全应用一份
    static THRESHOLDS: Cell<Option<Signal<[f32; 2]>>> = const { Cell::new(None) };
    /// 所有窗口（和测试工具）的大小，找不到所在窗口时按当前窗口或最后一个查
    static REGISTRY: RefCell<Vec<(Option<WindowId>, Weak<Inner>)>> = const { RefCell::new(Vec::new()) };
}

fn thresholds() -> Signal<[f32; 2]> {
    THRESHOLDS.with(|cell| {
        cell.get().unwrap_or_else(|| {
            let s = reactive::detached(|| signal([640.0, 1024.0]));
            cell.set(Some(s));
            s
        })
    })
}

/// 改尺寸档的分界：宽度不小于 `regular` 是中等档，不小于 `wide` 是宽档（默认 640、1024）。
/// 全应用一份；所有窗口里读 [`window_size_class`] 的绑定跟着更新
pub fn set_size_class_widths(regular: f32, wide: f32) {
    thresholds().set_if_changed([regular, wide.max(regular)]);
}

/// 当前窗口宽度的档位（响应式）。窗口大小每帧都可能变，但只有跨过分档宽度时值才变，
/// 读它的绑定只在这时重新执行：
///
/// ```no_run
/// # use aureole::prelude::*;
/// use aureole::{SizeClass, window_size_class};
/// column()
///     .direction(move || if window_size_class() >= SizeClass::Regular { Direction::Row } else { Direction::Column })
///     .gap(move || window_size_class().pick(12.0, 16.0, 24.0));
/// ```
///
/// "当前窗口"是调用它的代码所在的窗口：元素的绑定属于挂载它的窗口（测试工具里是 `Harness`
/// 的虚拟窗口），在哪个窗口里重新执行都一样。要在绑定里调用；在构建视图的函数体里直接调用只读一次，不会跟着变
pub fn window_size_class() -> SizeClass {
    metrics().map_or_else(SizeClass::default, |m| m.class.get())
}

/// 当前窗口内容区的大小，逻辑像素（响应式，每次变化都通知；只关心档位时用 [`window_size_class`] 或 [`is_window_at_least`]）
pub fn window_size() -> [f32; 2] {
    metrics().map_or([0.0; 2], |m| m.size.get())
}

/// 当前窗口宽度是否不小于 `width`（响应式）：只在跨过这个宽度时通知。同一个窗口里同一个宽度共用一个派生值
///
/// ```no_run
/// # use aureole::prelude::*;
/// use aureole::is_window_at_least;
/// row().child_when(move || is_window_at_least(900.0), || text("宽窗口才显示的侧栏"));
/// ```
pub fn is_window_at_least(width: f32) -> bool {
    let Some(m) = metrics() else { return false };
    let bits = width.to_bits();
    let existing = m.breakpoints.borrow().iter().find(|(w, _)| *w == bits).map(|(_, memo)| *memo);
    let memo = existing.unwrap_or_else(|| {
        let size = m.size;
        // 建在窗口的作用域里：不随调用它的绑定重新执行而释放
        let memo = m.scope.run(|| memo(move || size.get()[0] >= width));
        m.breakpoints.borrow_mut().push((bits, memo));
        memo
    });
    memo.get()
}

/// 按当前窗口的档位取值，直接传给属性：`.gap(responsive(12.0, 16.0, 24.0))`、
/// `.direction(responsive(Direction::Column, Direction::Row, Direction::Row))`。只在跨过分档宽度时重新取值
pub fn responsive<T: Clone + 'static>(compact: T, regular: T, wide: T) -> impl Fn() -> T + 'static {
    move || window_size_class().pick(compact.clone(), regular.clone(), wide.clone())
}

/// 隐藏了的标题栏占了多少（[`title_bar_info`]）。都是窗口内容区里的逻辑像素
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleBarInfo {
    /// 标题栏这一条有多高：自己画标题栏时用它当高度。系统的标题栏（内容不在标题栏下面）、全屏时是 0
    pub height: f32,
    /// 系统窗口按钮（macOS 的红绿灯）的范围。要应用自己画按钮的平台（Windows、Linux）、全屏时是 `None`
    pub controls: Option<Rect>,
    /// 标题栏的内容要从左边让开多少，才不和系统窗口按钮叠在一起
    pub inset_left: f32,
    /// 标题栏的内容要从右边让开多少，才不和系统窗口按钮叠在一起
    pub inset_right: f32,
}

/// 当前窗口隐藏了的标题栏（`TitleBar::Hidden`）占了多少：高度、系统窗口按钮在哪、标题栏的内容要让开多少（响应式）。
/// 窗口缩放、进出全屏、挪了按钮时变，绑定跟着更新，所以可以直接绑到布局上：
///
/// ```no_run
/// # use aureole::prelude::*;
/// use aureole::title_bar_info;
/// row()
///     .window_drag_area(true)
///     .height(move || px(title_bar_info().height))
///     .child(text("我的应用").margin_left(move || px(title_bar_info().inset_left)));
/// ```
///
/// "当前窗口"的意思同 [`window_size`]。测试工具里是 [`TitleBarInfo::default`]，用 `Harness::set_title_bar_info` 模拟
pub fn title_bar_info() -> TitleBarInfo {
    metrics().map_or_else(TitleBarInfo::default, |m| m.title_bar.get())
}

/// 一个窗口（或测试工具）的大小和由它派生的档位、隐藏了的标题栏的度量
pub(crate) struct Inner {
    size: Signal<[f32; 2]>,
    class: Memo<SizeClass>,
    title_bar: Signal<TitleBarInfo>,
    breakpoints: RefCell<Vec<(u32, Memo<bool>)>>,
    scope: Scope,
    /// 作用域释放后不能再读里面的信号
    alive: Cell<bool>,
}

/// 窗口大小的句柄，元素树里留一份（每帧把新大小写进去），作用域上提供一份（绑定里查）
#[derive(Clone)]
pub(crate) struct WindowMetrics(Rc<Inner>);

impl WindowMetrics {
    /// 在当前作用域（窗口或测试工具的）里建好，提供给里面的代码，并登记（找不到所在窗口时用）
    pub fn attach(size: [f32; 2], window: Option<WindowId>) -> Self {
        let scope = Scope::current().unwrap_or_default();
        let size = signal(size);
        let class = memo(move || SizeClass::from_width(size.get()[0]));
        let title_bar = signal(TitleBarInfo::default());
        let inner = Rc::new(Inner { size, class, title_bar, breakpoints: RefCell::new(Vec::new()), scope, alive: Cell::new(true) });
        let metrics = Self(inner);
        reactive::provide_context(metrics.clone());
        let weak = Rc::downgrade(&metrics.0);
        REGISTRY.with(|r| {
            let mut r = r.borrow_mut();
            r.retain(|(_, w)| w.upgrade().is_some_and(|i| i.alive.get()));
            r.push((window, weak.clone()));
        });
        reactive::on_cleanup(move || {
            if let Some(inner) = weak.upgrade() {
                inner.alive.set(false);
            }
        });
        metrics
    }

    /// 窗口大小变了（每帧开始时，不借用元素树时调用）
    pub fn set_size(&self, size: [f32; 2]) {
        if self.0.alive.get() {
            self.0.size.set_if_changed(size);
        }
    }

    /// 隐藏了的标题栏的度量变了（不借用元素树时调用）
    pub fn set_title_bar(&self, info: TitleBarInfo) {
        if self.0.alive.get() {
            self.0.title_bar.set_if_changed(info);
        }
    }
}

/// 调用方所在窗口的大小：先沿作用域找，找不到（不在任何窗口的作用域里）时用当前窗口，再不行用最后建的那个
fn metrics() -> Option<Rc<Inner>> {
    if let Some(m) = reactive::context::<WindowMetrics>() {
        return m.0.alive.get().then_some(m.0);
    }
    let current = current_window_id();
    REGISTRY.with(|r| {
        let r = r.borrow();
        let live = |(_, w): &(Option<WindowId>, Weak<Inner>)| w.upgrade().filter(|i| i.alive.get());
        r.iter().rev().find(|(h, _)| current.is_some() && *h == current).and_then(live).or_else(|| r.iter().rev().find_map(live))
    })
}
