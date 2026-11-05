//! 行高由内容决定的虚拟列表：行按自然高度布局，挂载后量出高度（按 key 缓存），没挂载过的行用估计值。
//!
//! - **行高索引** [`HeightIndex`]：两个树状数组，分别记量过的行的高度之和与行数。第 i 行的顶 =
//!   前 i 行里量过的高度之和 + 没量过的行数 × 估计值，估计值（给定的，或者量过的行的平均）随时可以变，
//!   不用改数组；量出一行是 O(log n)，按位置找行是一次二分下降（O(log n)）。
//! - **量**：每次布局之后，元素树读挂载着的行的实际高度（`tree/scrolling.rs` 的 `measure_lists`），
//!   和索引里的不一样时更新索引、重新摆放挂载着的行、更新内容高度，再布局一次（同一帧里）。
//! - **锚点**：视口顶上那一行在屏幕上不动。它上面的行量出来和估计的不一样时，偏移跟着挪同样的距离
//!   （进行中的弹簧、手指的位置一起挪），往上滚进没量过的行也不跳。数据变了（插入、删除）时，
//!   锚点是挂载着的、还在数据里的第一行，按 key 找到它的新位置。
//! - **缓存**：离开视口卸载的行把高度按 key 存起来；数据变化时按 key 找回来，删掉的 key 跟着丢掉。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use reactive::{Effect, Scope, memo, signal, untracked};

use super::{ListState, OVERSCAN, Reader, RowLayout, Source, margins, next_range};
use crate::dynamic::Dynamic;
use crate::element::{View, column};
use crate::scroll::mounting;
use crate::tree::{self, NodeId, TreeRef};

/// 没给估计、也还没量过任何一行时，按这个高度估计
const DEFAULT_ESTIMATE: f32 = 48.0;

/// 只留下最低的那个 1 位（树状数组里节点管的区间长度）；0 还是 0。
/// 标准库的 `isolate_lowest_one` 要 1.97，比支持的最低版本新
fn lowest_one(i: usize) -> usize {
    i & i.wrapping_neg()
}

/// 行高索引：每行量出来的高度（没量过的用估计值），按位置求前缀和、按偏移找行
#[derive(Clone, Debug, Default)]
pub(crate) struct HeightIndex {
    /// 每行量出来的高度，没量过是 NaN
    heights: Vec<f32>,
    /// 树状数组（下标从 1 开始）：量过的行的高度之和、行数
    sums: Vec<f64>,
    counts: Vec<u32>,
    /// 给定的估计；`None` 时用量过的行的平均高度
    given: Option<f32>,
    measured_sum: f64,
    measured: u32,
}

impl HeightIndex {
    /// `heights` 里没量过的是 NaN。O(n) 建树
    pub fn new(heights: Vec<f32>, given: Option<f32>) -> Self {
        let n = heights.len();
        let mut sums = vec![0.0f64; n + 1];
        let mut counts = vec![0u32; n + 1];
        let (mut measured_sum, mut measured) = (0.0, 0);
        for i in 1..=n {
            let h = heights[i - 1];
            if !h.is_nan() {
                sums[i] += h as f64;
                counts[i] += 1;
                measured_sum += h as f64;
                measured += 1;
            }
            let parent = i + lowest_one(i);
            if parent <= n {
                sums[parent] += sums[i];
                counts[parent] += counts[i];
            }
        }
        Self { heights, sums, counts, given, measured_sum, measured }
    }

    pub fn len(&self) -> usize {
        self.heights.len()
    }

    /// 没量过的行按多高算
    fn estimate(&self) -> f64 {
        match self.given {
            Some(h) => h as f64,
            None if self.measured > 0 => self.measured_sum / self.measured as f64,
            None => DEFAULT_ESTIMATE as f64,
        }
    }

    /// 第 i 行量过的高度
    pub fn measured(&self, i: usize) -> Option<f32> {
        self.heights.get(i).copied().filter(|h| !h.is_nan())
    }

    /// 记下第 i 行量出来的高度，返回变没变
    pub fn set(&mut self, i: usize, height: f32) -> bool {
        let old = self.heights[i];
        if old == height {
            return false;
        }
        let (sum, count) = if old.is_nan() { (height as f64, 1) } else { (height as f64 - old as f64, 0) };
        self.heights[i] = height;
        self.measured_sum += sum;
        self.measured += count;
        let mut k = i + 1;
        while k < self.sums.len() {
            self.sums[k] += sum;
            self.counts[k] += count;
            k += lowest_one(k);
        }
        true
    }

    /// 前 i 行的高度之和（第 i 行的顶）
    pub fn top(&self, i: usize) -> f64 {
        let (mut sum, mut count, mut k) = (0.0, 0u32, i.min(self.len()));
        let rows = k;
        while k > 0 {
            sum += self.sums[k];
            count += self.counts[k];
            k &= k - 1;
        }
        sum + (rows - count as usize) as f64 * self.estimate()
    }

    pub fn total(&self) -> f64 {
        self.measured_sum + (self.len() - self.measured as usize) as f64 * self.estimate()
    }

    /// 最大的 p（0..=n）使第 p 行的顶 `<= y`（`strict` 时 `< y`）；没有时为 `None`
    fn search(&self, y: f64, strict: bool) -> Option<usize> {
        let below = |top: f64| if strict { top < y } else { top <= y };
        if !below(0.0) {
            return None;
        }
        let n = self.len();
        let estimate = self.estimate();
        let (mut p, mut sum, mut count) = (0usize, 0.0f64, 0u32);
        let mut step = if n == 0 { 0 } else { 1 << (usize::BITS - 1 - n.leading_zeros()) };
        while step > 0 {
            let q = p + step;
            if q <= n {
                let (s, c) = (sum + self.sums[q], count + self.counts[q]);
                if below(s + (q - c as usize) as f64 * estimate) {
                    (p, sum, count) = (q, s, c);
                }
            }
            step >>= 1;
        }
        Some(p)
    }

    /// 偏移 `y` 处是第几行（拉过顶部时是第 0 行，过了末尾是最后一行）；列表是空的时为 0
    pub fn row_at(&self, y: f32) -> usize {
        self.search(y.max(0.0) as f64, false).unwrap_or(0).min(self.len().saturating_sub(1))
    }

    /// 和 `[y0, y1)` 相交的行（正好在边界上的行不算）
    pub fn range(&self, y0: f32, y1: f32) -> Range<usize> {
        let n = self.len();
        let start = self.search(y0.max(0.0) as f64, false).unwrap_or(0);
        let end = self.search(y1.max(0.0) as f64, true).map_or(0, |p| p + 1).min(n);
        start.min(end)..end
    }
}

/// key 的哈希：乘法混合就够了（数据变化时整个列表的 key 各查一次缓存，默认的哈希太慢）
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            self.write_u64(u64::from_le_bytes(chunk));
        }
        for &byte in rest {
            self.write_u64(byte as u64);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(n as u64);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

type KeyMap<K, V> = HashMap<K, V, BuildHasherDefault<KeyHasher>>;

/// 按 key 的状态（`ListState` 不带泛型，交给元素树；这里是只有列表自己用的部分）
struct Keyed<K> {
    /// 挂载着的行：key、节点、作用域，和 `ListState::mounted` 一一对应（按下标排好）
    current: Vec<(K, NodeId, Scope)>,
    /// 量过、现在没挂载的行的高度
    cache: KeyMap<K, f32>,
}

pub(super) struct MeasuredRows<T, KF, VF> {
    pub items: Source<T>,
    pub key: KF,
    pub view: VF,
    pub estimate: Option<f32>,
}

impl<T, K, KF, VF, V> Dynamic for MeasuredRows<T, KF, VF>
where
    T: Clone + 'static,
    K: Eq + Hash + 'static,
    KF: Fn(&T) -> K + 'static,
    VF: Fn(T) -> V + 'static,
    V: View,
{
    fn mount(self: Box<Self>, tree: &TreeRef, parent: NodeId, slot: usize) {
        let MeasuredRows { items, key, view, estimate } = *self;
        let handle = mounting().expect("虚拟列表要放在滚动视口里");
        let items = Reader::new(items);
        let key = Rc::new(key);
        // 数据变了（索引重建过）、行高量出来变了：要挂载的范围重算
        let (data, heights) = (signal(0u64), signal(0u64));
        let list = Rc::new(RefCell::new(ListState {
            content: parent,
            layout: RowLayout::Measured(HeightIndex::new(Vec::new(), estimate)),
            mounted: Vec::new(),
            changed: heights,
        }));
        tree.borrow_mut().register_list(list.clone());
        let keyed: Rc<RefCell<Keyed<K>>> = Rc::new(RefCell::new(Keyed { current: Vec::new(), cache: KeyMap::default() }));

        // 数据变了：重建行高索引（量过的按 key 找回来），锚点留在屏幕上原来的位置
        {
            let (list, keyed, key) = (list.clone(), keyed.clone(), key.clone());
            let weak = Rc::downgrade(tree);
            Effect::new(move || {
                let delta = items.with(|items| untracked(|| rebuild(&list, &keyed, &*key, items, estimate, &weak)));
                if let Some(tree) = weak.upgrade() {
                    tree::list_changed(&tree, parent, delta);
                }
                data.update(|v| *v += 1);
            });
        }

        // 要挂载的行：同固定行高的列表，离开的行先留着（不超过两倍余量），每跨过一行只重建一次
        // 数据变了时下标整体挪了，上一次的范围不再对应同样的行：重新按需要的算
        let (mounted, seen) = (Cell::new(0..0), Cell::new(0u64));
        let range = {
            let list = list.clone();
            memo(move || {
                let y = handle.offset_y();
                let viewport = handle.viewport_size()[1];
                let (above, below) = margins(handle.lead_y());
                let version = data.get();
                heights.get();
                let list = list.borrow();
                let index = list.layout.measured().expect("量出来的行高");
                let need = index.range(y - above, y + viewport + below);
                let keep = || index.range(y - above - OVERSCAN, y + viewport + below + OVERSCAN);
                let last = if seen.replace(version) == version { mounted.take() } else { 0..0 };
                let next = next_range(last, need, index.len(), keep);
                mounted.set(next.clone());
                next
            })
        };

        // 按 key 增量挂载和卸载；行按自然高度布局，放在索引算出的位置上（布局之后元素树量出实际高度再修正）
        let owner = Scope::new();
        let weak = Rc::downgrade(tree);
        Effect::new(move || {
            let range = range.get();
            data.get();
            let Some(tree) = weak.upgrade() else { return };
            untracked(|| {
                // 只复制要挂载的这一段（几十项），不碰整个列表
                let visible: Vec<(usize, T)> = items.with(|items| {
                    let end = range.end.min(items.len());
                    let start = range.start.min(end);
                    items[start..end].iter().cloned().enumerate().map(|(i, item)| (start + i, item)).collect()
                });
                let mut keyed = keyed.borrow_mut();
                let Keyed { current, cache } = &mut *keyed;
                let mut old: KeyMap<K, (NodeId, Scope)> = KeyMap::with_capacity_and_hasher(current.len(), Default::default());
                let mut removed = Vec::new();
                for (k, node, scope) in current.drain(..) {
                    if let Some(duplicate) = old.insert(k, (node, scope)) {
                        removed.push(duplicate);
                    }
                }
                let mut next = Vec::with_capacity(visible.len());
                for (index, item) in visible {
                    let k = key(&item);
                    let (node, scope) = old.remove(&k).unwrap_or_else(|| {
                        let scope = owner.run(Scope::new);
                        let node = scope.run(|| tree::mount(&tree, column().child(view(item)).into_element()));
                        (node, scope)
                    });
                    next.push((index, k, node, scope));
                }
                let mut list = list.borrow_mut();
                let index = list.layout.measured().expect("量出来的行高");
                // 离开的行：量过的高度按 key 存起来（数据变化时用）
                let positions: KeyMap<NodeId, usize> = list.mounted.iter().map(|&(i, node)| (node, i)).collect();
                for (k, (node, scope)) in old {
                    if let Some(h) = positions.get(&node).and_then(|&i| index.measured(i)) {
                        cache.insert(k, h);
                    }
                    removed.push((node, scope));
                }
                {
                    let mut t = tree.borrow_mut();
                    let len = index.len();
                    for &(i, _, node, _) in &next {
                        t.place_row(node, index.top(i) as f32, None);
                        t.accessibility_list_item(node, i, len);
                    }
                    t.set_content_height(parent, index.total() as f32);
                }
                list.mounted = next.iter().map(|&(i, _, node, _)| (i, node)).collect();
                drop(list);
                for (node, scope) in removed {
                    scope.dispose();
                    tree.borrow_mut().unmount(node);
                }
                let nodes = next.iter().map(|&(_, _, node, _)| node).collect();
                tree.borrow_mut().set_dynamic(parent, slot, nodes);
                *current = next.into_iter().map(|(_, k, node, scope)| (k, node, scope)).collect();
            });
        });
    }
}

/// 数据变了：重建行高索引。挂载着的行用索引里量过的高度，其余的按 key 查缓存（只留下还在数据里的 key）。
/// 返回锚点要挪的距离：挂载着的、还在数据里的第一行（从视口顶上那一行往下找）新的顶减去旧的顶
fn rebuild<T, K: Eq + Hash>(
    list: &RefCell<ListState>,
    keyed: &RefCell<Keyed<K>>,
    key: &dyn Fn(&T) -> K,
    items: &[T],
    estimate: Option<f32>,
    tree: &std::rc::Weak<RefCell<tree::Tree>>,
) -> f32 {
    let mut list = list.borrow_mut();
    let mut keyed = keyed.borrow_mut();
    let Keyed { current, cache } = &mut *keyed;
    let RowLayout::Measured(old) = &list.layout else { return 0.0 };
    // 视口顶上：从这一行往下找锚点
    let offset = tree.upgrade().and_then(|t| t.borrow().list_offset(list.content));
    let first = offset.map_or(usize::MAX, |y| old.row_at(y));
    let mounted: KeyMap<&K, usize> = current.iter().enumerate().map(|(m, (k, ..))| (k, m)).collect();
    let mut moved = vec![None; current.len()];
    let mut heights = Vec::with_capacity(items.len());
    let mut kept = KeyMap::with_capacity_and_hasher(cache.len().min(items.len()), Default::default());
    for (i, item) in items.iter().enumerate() {
        let k = key(item);
        let height = match mounted.get(&k) {
            Some(&m) => {
                moved[m].get_or_insert(i);
                old.measured(list.mounted[m].0)
            }
            None if !cache.is_empty() => cache.remove(&k).inspect(|&h| {
                kept.insert(k, h);
            }),
            None => None,
        };
        heights.push(height.unwrap_or(f32::NAN));
    }
    let index = HeightIndex::new(heights, estimate);
    let anchor = (0..current.len())
        .filter(|&m| list.mounted[m].0 >= first)
        .find_map(|m| moved[m].map(|new| (list.mounted[m].0, new)));
    let delta = anchor.map_or(0.0, |(before, after)| (index.top(after) - old.top(before)) as f32);
    drop(mounted);
    *cache = kept;
    // 挂载着的行换成新的下标（数据里没有了的记成 MAX），卸载时按它存高度
    for (row, new) in list.mounted.iter_mut().zip(moved) {
        row.0 = new.unwrap_or(usize::MAX);
    }
    list.layout = RowLayout::Measured(index);
    delta
}
