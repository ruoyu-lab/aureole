//! 虚拟列表：十万行数据也只挂载看得见的几十行，滚动时按 key 增量挂载和卸载。
//!
//! 列表本身是一个竖向滚动视口，内容的高度是所有行的高度之和，看得见的行按位置绝对定位在内容里。
//! 滚动时只有偏移在变（合成层平移），可见范围跨过行的边界时才挂载新行、卸载离开的行。
//! 行高由内容决定的模式（[`measured`]）在 `virtual_list/measured.rs`。

mod measured;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Range;
use std::rc::Rc;

use reactive::{Effect, Memo, Scope, Signal, memo, signal, untracked};
use scene::Number;

use crate::accessibility::Role;
use crate::dynamic::Dynamic;
use crate::element::{Child, Element, View, column};

use crate::property::registry::{PropId, PropType, Slot};
use crate::scroll::{ScrollHandle, mounting, scroll_y};
use crate::tree::{self, NodeId, TreeRef};

pub(crate) use measured::HeightIndex;

/// 视口上下各多挂这么高（逻辑像素）的行：滚动时提前备好
const OVERSCAN: f32 = 120.0;

/// 视口上面、下面各要多挂多高：平时各 [`OVERSCAN`]；惯性、弹簧在走时，往运动的方向再多挂它还要走的距离
/// （最多一个视口）。滚动的物理在渲染线程上也推进一份，主线程卡住时它接着滑，滑进来的也是挂好的行
fn margins(lead: f32) -> (f32, f32) {
    (OVERSCAN + (-lead).max(0.0), OVERSCAN + lead.max(0.0))
}

/// 虚拟列表：数据可以很长（十万行），只挂载视口里看得见的行（上下各多挂一点），
/// 滚动时按 `key` 增量更新：还在范围里的行保留节点和状态，新进来的才构建，离开的连同绑定和信号一起释放。
///
/// - `items`：`Signal<Vec<T>>` / `Memo<Vec<T>>`（按引用读，滚动时不复制整个列表），
///   或者返回 `Vec<T>` 的闭包（结果缓存起来，依赖变了才重新调用）
/// - `height`：固定行高（`64.0`），或者按项给出行高（`|item: &Msg| if item.big { 160.0 } else { 64.0 }`，
///   数据变化时算一遍前缀和），或者由内容决定（[`measured()`]：挂载后量出来，聊天记录、动态这类行高事先算不出的）
/// - `view`：构建一行，行宽占满列表，高度由 `height` 决定（行元素没指定高度时填满这一行；`measured()` 时是行的自然高度）
///
/// 返回的元素是一个竖向滚动视口，可以接着设置大小、背景，用 [`Element::scroll_handle`] 读写偏移、
/// 滚到第几行（[`ScrollHandle::scroll_to_index`]）。行离开视口就会卸载，需要保留的状态请放进数据里（比如每项一个信号）。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let messages = signal((0..100_000).collect::<Vec<u32>>());
/// virtual_list(messages, |&id| id, 56.0, |id| row().padding(12).child(text(format!("第 {id} 条"))))
///     .grow(1);
/// ```
pub fn virtual_list<T, K, V>(
    items: impl IntoListSource<T>,
    key: impl Fn(&T) -> K + 'static,
    height: impl IntoRowHeights<T>,
    view: impl Fn(T) -> V + 'static,
) -> Element
where
    T: Clone + 'static,
    K: Eq + Hash + 'static,
    V: View,
{
    let items = items.into_list_source().0;
    let rows: Box<dyn Dynamic> = match height.into_row_heights().0 {
        Heights::Measured(estimate) => Box::new(measured::MeasuredRows { items, key, view, estimate }),
        height => Box::new(Rows { items, key, height, view }),
    };
    let content = Element { children: vec![Child::Dynamic(rows)], ..Default::default() };
    scroll_y(content).scroll_handle(ScrollHandle::new()).accessibility_role(Role::List)
}

/// 虚拟列表的数据：`Signal<Vec<T>>`、`Memo<Vec<T>>`，或者返回 `Vec<T>` 的闭包
pub trait IntoListSource<T> {
    /// 转成列表的数据源
    fn into_list_source(self) -> ListSource<T>;
}

/// 见 [`IntoListSource`]
pub struct ListSource<T>(Source<T>);

/// 读派生值的函数：`Memo::with` 要求 `T: PartialEq`，在知道这一点的地方装进函数指针，之后读的时候不再要求
type ReadMemo<T> = fn(&Memo<Vec<T>>, &mut dyn FnMut(&[T]));

enum Source<T> {
    Signal(Signal<Vec<T>>),
    Memo(Memo<Vec<T>>, ReadMemo<T>),
    Closure(Box<dyn Fn() -> Vec<T>>),
}

impl<T: 'static> IntoListSource<T> for Signal<Vec<T>> {
    fn into_list_source(self) -> ListSource<T> {
        ListSource(Source::Signal(self))
    }
}

impl<T: PartialEq + 'static> IntoListSource<T> for Memo<Vec<T>> {
    fn into_list_source(self) -> ListSource<T> {
        ListSource(Source::Memo(self, |memo, f| memo.with(|items| f(items))))
    }
}

impl<T: 'static, F: Fn() -> Vec<T> + 'static> IntoListSource<T> for F {
    fn into_list_source(self) -> ListSource<T> {
        ListSource(Source::Closure(Box::new(self)))
    }
}

/// 挂载后读数据的方式：按引用读，订阅
enum Reader<T: 'static> {
    Signal(Signal<Vec<T>>),
    Memo(Memo<Vec<T>>, ReadMemo<T>),
}

impl<T: 'static> Clone for Reader<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for Reader<T> {}

impl<T: 'static> Reader<T> {
    /// 闭包的结果缓存进一个信号，依赖变了才重新调用
    fn new(source: Source<T>) -> Self
    where
        T: Clone,
    {
        match source {
            Source::Signal(signal) => Self::Signal(signal),
            Source::Memo(memo, read) => Self::Memo(memo, read),
            Source::Closure(f) => {
                let cache = signal(Vec::new());
                Effect::new(move || cache.set(f()));
                Self::Signal(cache)
            }
        }
    }

    fn with<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        match self {
            Self::Signal(signal) => signal.with(|items| f(items)),
            Self::Memo(memo, read) => {
                let mut f = Some(f);
                let mut result = None;
                read(memo, &mut |items| result = f.take().map(|f| f(items)));
                result.expect("派生值没有读到")
            }
        }
    }
}

impl<T: Clone + 'static> ListSource<T> {
    /// 列表外面也要读这份数据时（控件按下标选中、键盘走到第几行）：换成一个读取器，它可以复制，
    /// 也能再交给 [`virtual_list`]。闭包的数据源在这里开始跟踪依赖，要在响应式作用域里调用
    pub fn reader(self) -> ListReader<T> {
        ListReader(Reader::new(self.0))
    }
}

/// 虚拟列表数据的读取器，见 [`ListSource::reader`]
pub struct ListReader<T: 'static>(Reader<T>);

impl<T: 'static> Clone for ListReader<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: 'static> Copy for ListReader<T> {}

impl<T: 'static> ListReader<T> {
    /// 按引用读现在的数据（响应式），不复制列表
    pub fn with<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        self.0.with(f)
    }
}

impl<T: 'static> IntoListSource<T> for ListReader<T> {
    fn into_list_source(self) -> ListSource<T> {
        ListSource(match self.0 {
            Reader::Signal(signal) => Source::Signal(signal),
            Reader::Memo(memo, read) => Source::Memo(memo, read),
        })
    }
}

/// 行高：固定值，按项给出（`Fn(&T) -> f32`），或者由内容决定（[`measured()`]）
pub trait IntoRowHeights<T> {
    /// 转成行高的来源
    fn into_row_heights(self) -> RowHeights<T>;
}

/// 见 [`IntoRowHeights`]
pub struct RowHeights<T>(Heights<T>);

enum Heights<T> {
    Fixed(f32),
    Each(Box<dyn Fn(&T) -> f32>),
    /// 量出来；没量过的行用给定的估计，没给时用量过的行的平均
    Measured(Option<f32>),
}

impl<T> IntoRowHeights<T> for f32 {
    fn into_row_heights(self) -> RowHeights<T> {
        RowHeights(Heights::Fixed(self))
    }
}

impl<T> IntoRowHeights<T> for i32 {
    fn into_row_heights(self) -> RowHeights<T> {
        RowHeights(Heights::Fixed(self as f32))
    }
}

impl<T, F: Fn(&T) -> f32 + 'static> IntoRowHeights<T> for F {
    fn into_row_heights(self) -> RowHeights<T> {
        RowHeights(Heights::Each(Box::new(self)))
    }
}

/// 行高由内容决定：每行按自然高度布局，挂载后量出实际高度（按 key 缓存，滚回来不用再估），
/// 还没挂载过的行按估计算。聊天记录、动态这类行高事先算不出的列表用它。
///
/// - 估计：[`Measured::estimate`] 给定；不给时用量过的行的平均高度（没量过时按 48）。
///   估计越准，滚动条的长度越稳（量得越多越准）。
/// - 锚点：视口顶上的那一行在屏幕上不动。量出来的行高和估计的不一样时（包括往上滚进没量过的行），
///   数据插入、删除时，偏移跟着挪，看到的内容不跳；触控板、惯性、弹簧照常接着走。
/// - 行高会变的行（换行随宽度变、展开收起）每次布局后重新量，下面的行跟着挪。
/// - [`ScrollHandle::scroll_to_index`] 滚到很远的、没量过的地方也停得准：目标行量出来后目标跟着修正。
///
/// 每一帧只布局新进来的行（和量出来之后挪位置的那一次），滚动时其余的帧只平移合成层。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let messages = signal((0..100_000).map(|i| format!("第 {i} 条消息")).collect::<Vec<_>>());
/// virtual_list(messages, |m| m.clone(), measured().estimate(64), |m| text(m).padding(12)).grow(1);
/// ```
pub fn measured() -> Measured {
    Measured { estimate: None }
}

/// 行高由内容决定，见 [`measured()`]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Measured {
    estimate: Option<f32>,
}

impl Measured {
    /// 没量过的行按多高估计（逻辑像素）。不给时用量过的行的平均高度
    pub fn estimate(mut self, height: impl Number) -> Self {
        self.estimate = Some(height.to_f32().max(0.0));
        self
    }
}

impl<T> IntoRowHeights<T> for Measured {
    fn into_row_heights(self) -> RowHeights<T> {
        RowHeights(Heights::Measured(self.estimate))
    }
}

/// 列表登记在元素树上的状态（不带泛型）：按下标找行的位置（[`ScrollHandle::scroll_to_index`]），
/// 量出来的行高（布局之后由元素树量，见 `tree/scrolling.rs` 的 `measure_lists`）
pub(crate) struct ListState {
    /// 列表的内容节点（行挂在它下面）
    pub content: NodeId,
    pub layout: RowLayout,
    /// 挂载着的行：(下标, 节点)，按下标排好
    pub mounted: Vec<(usize, NodeId)>,
    /// 行高量出来变了：要挂载的范围重算（释放树之后写）
    pub changed: Signal<u64>,
}

/// 行的位置：算出来的，或者量出来的
pub(crate) enum RowLayout {
    Known(Geometry),
    Measured(HeightIndex),
}

impl RowLayout {
    pub fn measured(&self) -> Option<&HeightIndex> {
        match self {
            Self::Measured(index) => Some(index),
            Self::Known(_) => None,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Known(g) => g.len(),
            Self::Measured(index) => index.len(),
        }
    }

    /// 第 i 行的顶（超出范围时是最后一行的）
    pub fn top(&self, i: usize) -> f32 {
        let i = i.min(self.len().saturating_sub(1));
        match self {
            Self::Known(g) if g.len() == 0 => 0.0,
            Self::Known(g) => g.top(i),
            Self::Measured(index) => index.top(i) as f32,
        }
    }
}

/// 要挂载的范围：上一次的范围盖住了需要的就不变；完全不相交时换成需要的；否则往需要的方向扩，
/// 离开的行先留着（不超过 `keep`，即两倍余量），等下一次有行进来时一起卸载，每跨过一行只重建一次
fn next_range(last: Range<usize>, need: Range<usize>, len: usize, keep: impl FnOnce() -> Range<usize>) -> Range<usize> {
    if last.start <= need.start && need.end <= last.end && last.end <= len {
        last
    } else if last.is_empty() || last.end < need.start || need.end < last.start {
        need
    } else {
        let keep = keep();
        last.start.max(keep.start).min(need.start)..last.end.min(keep.end).max(need.end)
    }
}

/// 行的位置
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Geometry {
    Fixed { height: f32, len: usize },
    /// 前缀和：第 i 行从 `p[i]` 到 `p[i + 1]`
    Prefix(Rc<[f32]>),
}

impl Geometry {
    fn measure<T>(heights: &Heights<T>, items: &[T]) -> Self {
        match heights {
            Heights::Fixed(height) => Self::Fixed { height: height.max(0.0), len: items.len() },
            Heights::Measured(_) => unreachable!("量出来的行高走 `measured::MeasuredRows`"),
            Heights::Each(f) => {
                let mut prefix = Vec::with_capacity(items.len() + 1);
                let mut total = 0.0;
                prefix.push(0.0);
                for item in items {
                    total += f(item).max(0.0);
                    prefix.push(total);
                }
                Self::Prefix(prefix.into())
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Fixed { len, .. } => *len,
            Self::Prefix(p) => p.len() - 1,
        }
    }

    fn total(&self) -> f32 {
        match self {
            Self::Fixed { height, len } => height * *len as f32,
            Self::Prefix(p) => p[p.len() - 1],
        }
    }

    fn top(&self, index: usize) -> f32 {
        match self {
            Self::Fixed { height, .. } => height * index as f32,
            Self::Prefix(p) => p[index],
        }
    }

    fn height(&self, index: usize) -> f32 {
        match self {
            Self::Fixed { height, .. } => *height,
            Self::Prefix(p) => p[index + 1] - p[index],
        }
    }

    /// 和 `[y0, y1)` 相交的行
    fn range(&self, y0: f32, y1: f32) -> Range<usize> {
        let len = self.len();
        let (y0, y1) = (y0.max(0.0), y1.max(0.0));
        let range = match self {
            Self::Fixed { height, .. } if *height > 0.0 => {
                (y0 / height).floor() as usize..(y1 / height).ceil() as usize
            }
            Self::Fixed { .. } => 0..len,
            // 第一行：最后一个 p[i] <= y0 的 i；结束：第一个 p[j] >= y1 的 j
            Self::Prefix(p) => p.partition_point(|&v| v <= y0).saturating_sub(1)..p.partition_point(|&v| v < y1),
        };
        let end = range.end.min(len);
        range.start.min(end)..end
    }
}

struct Rows<T, KF, VF> {
    items: Source<T>,
    key: KF,
    height: Heights<T>,
    view: VF,
}

impl<T, K, KF, VF, V> Dynamic for Rows<T, KF, VF>
where
    T: Clone + 'static,
    K: Eq + Hash + 'static,
    KF: Fn(&T) -> K + 'static,
    VF: Fn(T) -> V + 'static,
    V: View,
{
    fn mount(self: Box<Self>, tree: &TreeRef, parent: NodeId, slot: usize) {
        let Rows { items, key, height, view } = *self;
        let handle = mounting().expect("虚拟列表要放在滚动视口里");
        let items = Reader::new(items);

        // 行的位置：固定行高直接算，按项给的行高在数据变化时算一遍前缀和；
        // 也登记在元素树上（滚到第几行时按下标找位置）
        let geometry = signal(Geometry::Fixed { height: 0.0, len: 0 });
        let list = Rc::new(RefCell::new(ListState {
            content: parent,
            layout: RowLayout::Known(Geometry::Fixed { height: 0.0, len: 0 }),
            mounted: Vec::new(),
            changed: signal(0),
        }));
        tree.borrow_mut().register_list(list.clone());
        Effect::new(move || {
            let measured = items.with(|items| Geometry::measure(&height, items));
            list.borrow_mut().layout = RowLayout::Known(measured.clone());
            geometry.set(measured);
        });

        // 内容的高度是所有行的高度之和
        let weak = Rc::downgrade(tree);
        Effect::new(move || {
            let total = geometry.with(Geometry::total);
            if let Some(tree) = weak.upgrade() {
                tree.borrow_mut().set_content_height(parent, total);
            }
        });

        // 要挂载的行：滚动时每帧重算，结果变了下游的副作用才执行。只在有行要进来时才变：
        // 离开的行先留着（不超过两倍的余量），等下一次有行进来时一起卸载，每跨过一行只重建一次
        let mounted = Cell::new(0..0);
        let range = memo(move || {
            let y = handle.offset_y();
            let viewport = handle.viewport_size()[1];
            let (above, below) = margins(handle.lead_y());
            geometry.with(|g| {
                let need = g.range(y - above, y + viewport + below);
                let keep = || g.range(y - above - OVERSCAN, y + viewport + below + OVERSCAN);
                let next = next_range(mounted.take(), need, g.len(), keep);
                mounted.set(next.clone());
                next
            })
        });

        // 每一行的作用域归这个作用域所有（而不是归下面的副作用），副作用重新执行时保留下来的行不会被释放
        let owner = Scope::new();
        let weak = Rc::downgrade(tree);
        let mut current: Vec<(K, NodeId, Scope)> = Vec::new();
        Effect::new(move || {
            let range = range.get();
            let geometry = geometry.get();
            // 只复制要挂载的这一段（几十项），不碰整个列表
            let visible: Vec<(usize, T)> = items.with(|items| {
                let end = range.end.min(items.len());
                let start = range.start.min(end);
                items[start..end].iter().cloned().enumerate().map(|(i, item)| (start + i, item)).collect()
            });
            let Some(tree) = weak.upgrade() else { return };
            untracked(|| {
                let mut old: HashMap<K, (NodeId, Scope)> = HashMap::with_capacity(current.len());
                let mut removed = Vec::new();
                for (k, node, scope) in current.drain(..) {
                    // 上一轮有重复 key 时，多出来的那项也要清掉
                    if let Some(duplicate) = old.insert(k, (node, scope)) {
                        removed.push(duplicate);
                    }
                }
                let mut next = Vec::with_capacity(visible.len());
                for (index, item) in visible {
                    let k = key(&item);
                    let (node, scope) = old.remove(&k).unwrap_or_else(|| {
                        let scope = owner.run(Scope::new);
                        let node = scope.run(|| {
                            // 行元素没指定高度和 grow 时填满这一行（背景、悬停、点击都覆盖整行）
                            let mut element = view(item).into_element();
                            if !element.props.has(PropId::GROW) && !element.props.has(PropId::HEIGHT) {
                                element.put(PropId::GROW, Slot::Static(1.0f32.store()));
                            }
                            tree::mount(&tree, column().child(element).into_element())
                        });
                        (node, scope)
                    });
                    let mut t = tree.borrow_mut();
                    t.place_row(node, geometry.top(index), Some(geometry.height(index)));
                    t.accessibility_list_item(node, index, geometry.len());
                    drop(t);
                    next.push((k, node, scope));
                }
                removed.extend(old.into_values());
                for (node, scope) in removed {
                    scope.dispose();
                    tree.borrow_mut().unmount(node);
                }
                tree.borrow_mut().set_dynamic(parent, slot, next.iter().map(|(_, node, _)| *node).collect());
                current = next;
            });
        });
    }
}
