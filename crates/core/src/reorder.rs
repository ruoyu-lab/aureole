//! 能拖动排序的列表：拖起一行，别的行平滑地让开、露出要放下的位置，松手后被拖的行从指针处落到新位置；
//! 有焦点的行按 ⌥↑ / ⌥↓ 移动（键盘替代）。全部用公开的拖放 API 写成，也是它的用法示例。
//!
//! 行都绝对定位在列表的左上角，位置是纵向平移（临界阻尼的弹簧，不回弹）：拖动、让开、放下、键盘移动都只动合成层，
//! 不重新布局、不重画。行高由 [`Element::report_bounds`] 量出来，可以不一样高。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::Hash;
use std::rc::Rc;

use reactive::{Signal, memo, on_cleanup, signal};

use crate::accessibility::Role;
use crate::property::Property;
use crate::drag_drop::{DragPreview, DropEvent};
use crate::element::{Element, View, column, rect};
use crate::event::{DragEvent, KeyEvent};
use crate::input::GesturePhase;
use crate::geometry::Rect;
use crate::layout::Direction;

/// 能拖动排序的列表，见 [`reorderable`]
pub struct Reorderable<T, K> {
    items: Signal<Vec<T>>,
    key: Rc<dyn Fn(&T) -> K>,
    view: Rc<dyn Fn(T) -> Element>,
    gap: f32,
    on_reorder: Option<Rc<dyn Fn(usize, usize)>>,
    dragged: Option<Signal<Option<K>>>,
    /// 元素的方法，建好元素时用上
    after: Vec<Box<dyn FnOnce(Element) -> Element>>,
}

impl<T: Clone + 'static, K: Clone + Eq + Hash + 'static> crate::ElementMethods for Reorderable<T, K> {
    fn element(mut self, f: impl FnOnce(Element) -> Element + 'static) -> Self {
        self.after.push(Box::new(f));
        self
    }
}

/// 能拖动排序的列表：`items` 里的每一项用 `view` 画成一行（按 `key` 增量更新，和 [`Element::each`] 一样），
/// 按住一行拖动就能换位置，放下时直接改 `items` 的顺序。
///
/// - 拖的时候被拖的行跟着指针（[`DragPreview::Lift`]，画在别的行上面），别的行在合成层上平滑让开，
///   松手后它从指针处落到新位置；按 Esc、拖出列表放下时回到原处。
/// - 列表在滚动视口里时，拖到视口边缘会自动滚动。
/// - 键盘：行能获得焦点（↑ ↓ 在行之间移动），有焦点的行按 ⌥↑ / ⌥↓（Alt+↑ / Alt+↓）上下移动一格。
/// - 拖的数据是这一行的 `key`：别的放置目标（比如回收站）用 `on_drop(|key: K, _| ..)` 也能接收。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::reorderable;
/// let tasks = signal(vec!["写周报", "改稿", "回邮件"]);
/// reorderable(tasks, |t: &&str| *t, |t: &str| text(t).padding(10))
///     .gap(6.0)
///     .on_reorder(|from, to| println!("{from} → {to}"));
/// ```
pub fn reorderable<T, K, V>(items: Signal<Vec<T>>, key: impl Fn(&T) -> K + 'static, view: impl Fn(T) -> V + 'static) -> Reorderable<T, K>
where
    T: Clone + 'static,
    K: Clone + Eq + Hash + 'static,
    V: View,
{
    Reorderable { items, key: Rc::new(key), view: Rc::new(move |item| view(item).into_element()), gap: 0.0, on_reorder: None, dragged: None, after: Vec::new() }
}

/// 每一行的纵向位置和列表的总高度
#[derive(PartialEq)]
struct Slots<K: Eq + Hash> {
    y: HashMap<K, f32>,
    total: f32,
}

impl<T: Clone + 'static, K: Clone + Eq + Hash + 'static> Reorderable<T, K> {
    /// 行之间的间距，逻辑像素
    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    /// 顺序变了之后调用（拖动放下、键盘移动），参数是这一项原来的下标和新的下标。`items` 已经改好了
    pub fn on_reorder<R>(mut self, f: impl Fn(usize, usize) -> R + 'static) -> Self {
        self.on_reorder = Some(Rc::new(move |from, to| {
            f(from, to);
        }));
        self
    }

    /// 正在被拖的那一行的 `key` 写进这个信号（松手时变回 `None`），行可以按它换成"浮起来"的样子
    pub fn report_dragging(mut self, dragged: Signal<Option<K>>) -> Self {
        self.dragged = Some(dragged);
        self
    }

    /// 生成列表（外面一层可以照常设背景、内边距、大小）
    pub fn build(mut self) -> Element {
        let after = std::mem::take(&mut self.after);
        after.into_iter().fold(self.construct(), |el, f| f(el))
    }

    fn construct(self) -> Element {
        let Reorderable { items, key, view, gap, on_reorder, dragged, after: _ } = self;
        // 每行量出来的大小；行挂载、卸载时版本号加一，位置跟着重算
        let sizes: Rc<RefCell<HashMap<K, Signal<Rect>>>> = Rc::default();
        let version = signal(0u64);
        // 被拖的行和它要插的位置
        let dragging: Signal<Option<(K, usize)>> = signal(None);
        // 按下处在被拖的行里的纵向位置
        let grab = Rc::new(Cell::new(0.0f32));

        let height = {
            let sizes = sizes.clone();
            Rc::new(move |k: &K, tracked: bool| {
                sizes.borrow().get(k).map_or(0.0, |s| if tracked { s.get().height } else { s.get_untracked().height })
            })
        };
        let keys = {
            let key = key.clone();
            Rc::new(move || items.with_untracked(|v| v.iter().map(|t| key(t)).collect::<Vec<K>>()))
        };
        let slots = {
            let (key, height) = (key.clone(), height.clone());
            memo(move || {
                version.get();
                let mut order: Vec<K> = items.with(|v| v.iter().map(|t| key(t)).collect());
                if let Some((k, to)) = dragging.get()
                    && let Some(from) = order.iter().position(|x| *x == k)
                {
                    let k = order.remove(from);
                    order.insert(to.min(order.len()), k);
                }
                let (mut y, mut map) = (0.0, HashMap::with_capacity(order.len()));
                for k in order {
                    let h = height(&k, true);
                    map.insert(k, y);
                    y += h + gap;
                }
                Slots { y: map, total: (y - gap).max(0.0) }
            })
        };
        // 被拖的行的中心落在哪个位置：插在第 j 行（不算被拖的行）前面，还是后面，以两种摆法下它中心的中点为界
        let insertion = {
            let (keys, height, grab) = (keys.clone(), height.clone(), grab.clone());
            move |k: &K, pointer: f32| {
                let own = height(k, false);
                let center = pointer - grab.get() + own * 0.5;
                let others: Vec<K> = keys().into_iter().filter(|x| x != k).collect();
                let mut top = 0.0;
                for (j, other) in others.iter().enumerate() {
                    let h = height(other, false);
                    if center < top + own * 0.5 + (h + gap) * 0.5 {
                        return j;
                    }
                    top += h + gap;
                }
                others.len()
            }
        };
        let commit = {
            let keys = keys.clone();
            Rc::new(move |k: &K, to: usize| {
                let Some(from) = keys().iter().position(|x| x == k) else { return };
                let to = to.min(items.with_untracked(Vec::len).saturating_sub(1));
                if from == to {
                    return;
                }
                items.update(|v| {
                    let item = v.remove(from);
                    v.insert(to, item);
                });
                if let Some(f) = &on_reorder {
                    f(from, to);
                }
            })
        };

        let slide = crate::defaults::slide();
        let row = {
            let (key, keys, commit, grab) = (key.clone(), keys.clone(), commit.clone(), grab.clone());
            move |item: T| {
                let k = key(&item);
                let size = signal(Rect::default());
                sizes.borrow_mut().insert(k.clone(), size);
                version.update(|v| *v += 1);
                {
                    let (sizes, k) = (sizes.clone(), k.clone());
                    on_cleanup(move || {
                        sizes.borrow_mut().remove(&k);
                        version.update(|v| *v += 1);
                    });
                }
                let y = {
                    let k = k.clone();
                    move || slots.with(|s| s.y.get(&k).copied().unwrap_or(0.0))
                };
                let (payload, start, moved) = (k.clone(), k.clone(), k);
                let (keys, commit, grab) = (keys.clone(), commit.clone(), grab.clone());
                let keys_for_start = keys.clone();
                column()
                    .position(0, 0)
                    .full_width(true)
                    .translation_y(y)
                    .transition(Property::TranslationY, slide)
                    .report_bounds(size)
                    .draggable(move || payload.clone())
                    .drag_preview(DragPreview::Lift)
                    .on_drag(move |e: &DragEvent| match e.phase {
                        GesturePhase::Start => {
                            grab.set(e.position[1] - e.translation[1]);
                            if let Some(from) = keys_for_start().iter().position(|x| *x == start) {
                                dragging.set(Some((start.clone(), from)));
                            }
                            if let Some(dragged) = dragged {
                                dragged.set(Some(start.clone()));
                            }
                        }
                        GesturePhase::End | GesturePhase::Cancel => {
                            if let Some(dragged) = dragged {
                                dragged.set(None);
                            }
                        }
                        GesturePhase::Move => {}
                    })
                    .focusable(true)
                    .accessibility_role(Role::ListItem)
                    // 键盘替代：⌥↑ / ⌥↓ 移动一格
                    .on_key_down(move |e: &KeyEvent| {
                        let step: isize = if e.is("Alt+ArrowUp") {
                            -1
                        } else if e.is("Alt+ArrowDown") {
                            1
                        } else {
                            return;
                        };
                        if let Some(from) = keys().iter().position(|x| *x == moved) {
                            commit(&moved, (from as isize + step).max(0) as usize);
                        }
                        e.stop_propagation();
                    })
                    .child(view(item))
            }
        };

        let list = rect()
            .height(move || slots.with(|s| s.total))
            .each(move || items.get(), move |t| key(t), row)
            .accepts_drop(move |k: &K| keys().contains(k))
            .on_drag_over(move |e: &DropEvent| {
                if let Some(k) = e.data::<K>() {
                    let to = insertion(k, e.position[1]);
                    if dragging.with_untracked(|d| d.as_ref().is_none_or(|(x, t)| x != k || *t != to)) {
                        dragging.set(Some((k.clone(), to)));
                    }
                }
            })
            .on_drop(move |k: K, _| {
                let to = dragging.with_untracked(|d| d.as_ref().filter(|(x, _)| *x == k).map(|(_, to)| *to));
                if let Some(to) = to {
                    commit(&k, to);
                }
            })
            // 放下、离开、取消：不再让开（放下时顺序已经改好，行停在原地）
            .on_drag_leave(move || dragging.set(None))
            .focus_arrows(Direction::Column)
            .accessibility_role(Role::List);
        column().child(list)
    }
}

impl<T: Clone + 'static, K: Clone + Eq + Hash + 'static> View for Reorderable<T, K> {
    fn into_element(self) -> Element {
        self.build()
    }
}
