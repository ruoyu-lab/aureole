//! 表格：固定的表头、虚拟滚动的行、选中、键盘、点表头排序、拖动调列宽。
//!
//! 行由 [`aureole_core::virtual_list`] 挂载，十万行也只挂载看得见的部分。列宽是算出来的数（不靠每行各自弹性布局），
//! 表头和每一行的列总是对齐；内容比表格宽时表头和行放在同一个横向滚动视口里，一起滚。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::rc::Rc;

use reactive::{Effect, Memo, Signal, memo, on_cleanup, signal, untracked};

use super::look::look;
use super::{Changes, Flag, Press, on_activate, on_key, part, reader};
use aureole_core::accessibility::Role;
use aureole_core::{
    Align, Builtin, ChangeSource, ClickEvent, Cursor, DragEvent, Element, FocusHandle, GesturePhase, IntoChangeHandler, IntoListSource, IntoProp, Justify, Number,
    Key, KeyEvent, ListReader, ListSource, Modifiers, NamedKey, Prop, Rect, ScrollHandle, State, States, Stroke, Style, TextAlign, VectorPath, View, column,
    path, pct, rect, row, scroll_x, text, virtual_list, without_animation,
};

/// 一行的 key
type KeyOf<T, K> = Rc<dyn Fn(&T) -> K>;
/// 收一行数据的处理器
type OnRow<T> = Rc<dyn Fn(&T)>;
/// 调了列宽的处理器：第几列、新的宽度
type OnResize = Rc<dyn Fn(usize, f32)>;
/// 给一行的元素再加东西
type RowElement<T> = Rc<dyn Fn(Element, Signal<T>) -> Element>;
/// 挂载着的行：key 对应这一行的数据
type LiveRows<T, K> = Rc<RefCell<HashMap<K, Signal<T>>>>;

/// 列的默认最小宽度
const MIN_COLUMN: f32 = 48.0;
/// 默认的行高
const ROW_HEIGHT: f32 = 32.0;
/// ← → 横着滚多远
const HORIZONTAL_STEP: f32 = 48.0;

/// 表格选中绑定的值：`Option<K>` 选一行，`Vec<K>` 选多行
pub trait TableSelection<K>: Clone + PartialEq + 'static {
    /// 能不能选多行
    const MULTIPLE: bool;
    /// 选了哪些行
    fn keys(&self) -> Vec<K>;
    /// 选了这些行之后绑定的信号里放什么；只能选一行的取第一个
    fn from_keys(keys: Vec<K>) -> Self;
}

impl<K: Clone + PartialEq + 'static> TableSelection<K> for Option<K> {
    const MULTIPLE: bool = false;

    fn keys(&self) -> Vec<K> {
        self.iter().cloned().collect()
    }

    fn from_keys(keys: Vec<K>) -> Self {
        keys.into_iter().next()
    }
}

impl<K: Clone + PartialEq + 'static> TableSelection<K> for Vec<K> {
    const MULTIPLE: bool = true;

    fn keys(&self) -> Vec<K> {
        self.clone()
    }

    fn from_keys(keys: Vec<K>) -> Self {
        keys
    }
}

/// 排序的方向
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SortOrder {
    /// 从小到大
    Ascending,
    /// 从大到小
    Descending,
}

/// 按哪一列、什么方向排序。`key` 是 [`TableColumn::sort_key`] 给的
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableSort {
    /// 列的排序键
    pub key: &'static str,
    /// 方向
    pub order: SortOrder,
}

/// 单元格里放什么
enum Content<T: 'static> {
    Text(Rc<dyn Fn(&T) -> String>),
    View(Rc<dyn Fn(Signal<T>) -> Element>),
}

impl<T: 'static> Clone for Content<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Text(f) => Self::Text(f.clone()),
            Self::View(f) => Self::View(f.clone()),
        }
    }
}

/// 表格的一列：标题、单元格的内容、宽度、对齐、排序键。默认平分剩余的宽度
///
/// ```no_run
/// # use aureole::prelude::*;
/// #[derive(Clone, PartialEq)]
/// struct Person { id: u32, name: String, age: u32 }
/// TableColumn::new("姓名", |p: &Person| p.name.clone()).sort_key("name");
/// TableColumn::new("年龄", |p: &Person| p.age.to_string()).width(80).align(TextAlign::End);
/// TableColumn::view("操作", |p: Signal<Person>| button("删除").on_click(move || println!("{}", p.get().id))).width(96);
/// ```
pub struct TableColumn<T: 'static> {
    title: Prop<String>,
    content: Content<T>,
    width: Option<f32>,
    grow: f32,
    min: f32,
    align: TextAlign,
    sort: Option<&'static str>,
    resizable: bool,
}

impl<T: 'static> TableColumn<T> {
    fn with(title: impl IntoProp<String>, content: Content<T>) -> Self {
        Self { title: title.into_prop(), content, width: None, grow: 1.0, min: MIN_COLUMN, align: TextAlign::Start, sort: None, resizable: true }
    }

    /// 文字的一列：`value` 从这一行的数据算出要显示的文字，放不下时末尾省略
    pub fn new<S: Into<String>>(title: impl IntoProp<String>, value: impl Fn(&T) -> S + 'static) -> Self {
        Self::with(title, Content::Text(Rc::new(move |row| value(row).into())))
    }

    /// 单元格自己搭的一列（按钮、进度条、输入框……）：`view` 只在这一行挂载时调用一次，`row` 是这一行现在的数据，
    /// 数据变了要跟着变的地方读这个信号
    pub fn view<V: View>(title: impl IntoProp<String>, view: impl Fn(Signal<T>) -> V + 'static) -> Self {
        Self::with(title, Content::View(Rc::new(move |row| view(row).into_element())))
    }

    /// 固定的宽度，逻辑像素
    pub fn width(mut self, width: impl Number) -> Self {
        self.width = Some(width.to_f32().max(0.0));
        self
    }

    /// 按比例分剩余的宽度（默认 1）
    pub fn grow(mut self, grow: impl Number) -> Self {
        self.width = None;
        self.grow = grow.to_f32().max(0.0);
        self
    }

    /// 最小宽度（默认 48）：分宽度、拖动调列宽都不会比它窄
    pub fn min_width(mut self, width: impl Number) -> Self {
        self.min = width.to_f32().max(0.0);
        self
    }

    /// 标题和内容的对齐（默认靠行首）
    pub fn align(mut self, align: TextAlign) -> Self {
        self.align = align;
        self
    }

    /// 这一列能排序：点表头在升序、降序、不排序之间轮流，新的状态写进 [`Table::sort`] 绑定的信号，`key` 用来认出是哪一列
    pub fn sort_key(mut self, key: &'static str) -> Self {
        self.sort = Some(key);
        self
    }

    /// 能不能拖动表头右边调宽（默认能）
    pub fn resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }
}

/// 分宽度时一列的情况
#[derive(Clone, Copy, Debug, PartialEq)]
struct ColumnSize {
    width: Option<f32>,
    grow: f32,
    min: f32,
}

/// 按可用的宽度算出每列的宽度：固定的先拿走，剩下的按比例分，分到的比最小宽度窄时按最小宽度，
/// 重新分给别的列。每列的边落在整数上，总宽不变（文字不会因为小数的位置发虚）
fn column_widths(sizes: &[ColumnSize], available: f32) -> Vec<f32> {
    let mut widths: Vec<f32> = sizes.iter().map(|s| s.width.map_or(s.min, |w| w.max(s.min))).collect();
    let mut flexible: Vec<usize> = (0..sizes.len()).filter(|&i| sizes[i].width.is_none() && sizes[i].grow > 0.0).collect();
    let fixed: f32 = (0..sizes.len()).filter(|i| !flexible.contains(i)).map(|i| widths[i]).sum();
    let mut left = (available - fixed).max(0.0);
    while !flexible.is_empty() {
        let total: f32 = flexible.iter().map(|&i| sizes[i].grow).sum();
        let narrow = flexible.iter().position(|&i| left * sizes[i].grow / total < sizes[i].min);
        match narrow {
            Some(at) => {
                let i = flexible.remove(at);
                left = (left - sizes[i].min).max(0.0);
            }
            None => {
                for &i in &flexible {
                    widths[i] = left * sizes[i].grow / total;
                }
                break;
            }
        }
    }
    let mut edge = 0.0f32;
    for width in &mut widths {
        let next = edge + *width;
        *width = next.round() - edge.round();
        edge = next;
    }
    widths
}

/// 挂载以后的一列
struct LiveColumn<T: 'static> {
    title: Rc<dyn Fn() -> String>,
    content: Content<T>,
    align: TextAlign,
    sort: Option<&'static str>,
    resizable: bool,
    min: f32,
    /// 拖动调过的宽度
    dragged: Signal<Option<f32>>,
    /// 算出来的宽度
    width: Memo<f32>,
}

/// 选中绑定的信号，已经不带值的类型
struct Binding<K> {
    read: Rc<dyn Fn() -> Vec<K>>,
    write: Box<dyn Fn(Vec<K>)>,
    multiple: bool,
}

/// 表格的逻辑：行、选中、键盘走到哪一行。行和键盘的处理器共用
struct Shared<T: 'static, K: 'static> {
    rows: ListReader<T>,
    key: KeyOf<T, K>,
    selection: Option<Binding<K>>,
    /// 选中的行（查起来快）
    selected: Signal<Rc<HashSet<K>>>,
    /// 键盘走到的一行
    cursor: Signal<Option<K>>,
    /// Shift 选一段时固定的那一头
    anchor: RefCell<Option<K>>,
    /// 上次在第几行找到光标（一般没变，不用从头找）
    hint: Cell<usize>,
    row_height: f32,
    list: ScrollHandle,
    horizontal: ScrollHandle,
    focus: FocusHandle,
    /// 这次是用键盘操作的（走到的那一行要画出来）
    keyboard: Signal<bool>,
    changes: Changes<Vec<K>>,
    activate: Vec<OnRow<T>>,
}

impl<T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static> Shared<T, K> {
    fn len(&self) -> usize {
        untracked(|| self.rows.with(<[T]>::len))
    }

    fn key_at(&self, index: usize) -> Option<K> {
        untracked(|| self.rows.with(|rows| rows.get(index).map(|row| (self.key)(row))))
    }

    /// 这一行现在是第几行
    fn index_of(&self, key: &K) -> Option<usize> {
        untracked(|| {
            self.rows.with(|rows| {
                let hint = self.hint.get();
                if rows.get(hint).is_some_and(|row| (self.key)(row) == *key) {
                    return Some(hint);
                }
                let found = rows.iter().position(|row| (self.key)(row) == *key);
                self.hint.set(found.unwrap_or(0));
                found
            })
        })
    }

    fn cursor_index(&self) -> Option<usize> {
        self.cursor.with_untracked(|cursor| cursor.as_ref().and_then(|key| self.index_of(key)))
    }

    fn multiple(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| s.multiple)
    }

    fn select(&self, keys: Vec<K>, source: ChangeSource) {
        let Some(binding) = &self.selection else { return };
        let same = keys.len() == self.selected.with_untracked(|s| s.len()) && self.selected.with_untracked(|s| keys.iter().all(|k| s.contains(k)));
        if same {
            return;
        }
        (binding.write)(keys.clone());
        self.changes.both(keys, source);
    }

    /// `from..=to` 这一段的行
    fn span(&self, from: usize, to: usize) -> Vec<K> {
        let (start, end) = (from.min(to), from.max(to));
        untracked(|| self.rows.with(|rows| rows[start..=end.min(rows.len() - 1)].iter().map(|row| (self.key)(row)).collect()))
    }

    /// 选到第 `to` 行：按着 Shift 时选从固定的一头到这里的一段，没有时只选这一行
    fn extend_or_replace(&self, to: usize, extend: bool, source: ChangeSource) {
        let Some(key) = self.key_at(to) else { return };
        let anchor = self.anchor.borrow().clone().and_then(|a| self.index_of(&a));
        match anchor.filter(|_| extend && self.multiple()) {
            Some(anchor) => self.select(self.span(anchor, to), source),
            None => {
                *self.anchor.borrow_mut() = Some(key.clone());
                self.select(vec![key], source);
            }
        }
    }

    /// 点了一行：单击只选它，主修饰键加选或取消，Shift 选一段
    fn click(&self, key: K, modifiers: Modifiers, source: ChangeSource) {
        self.cursor.set(Some(key.clone()));
        let Some(index) = self.index_of(&key) else { return };
        // 按平台惯例（和键盘的 has_primary_modifier 一样），不按编译目标：测试工具在哪都用 macOS 的一套
        let primary = modifiers.intersects(aureole_core::conventions().primary_modifier);
        if primary && self.multiple() {
            let mut keys: Vec<K> = self.selected.with_untracked(|s| s.iter().cloned().collect());
            match keys.iter().position(|k| *k == key) {
                Some(at) => {
                    keys.remove(at);
                }
                None => keys.push(key.clone()),
            }
            *self.anchor.borrow_mut() = Some(key);
            self.select(keys, source);
        } else {
            self.extend_or_replace(index, modifiers.has_shift(), source);
        }
    }

    /// 让第 `index` 行露出来
    fn reveal(&self, index: usize) {
        let (top, viewport) = (index as f32 * self.row_height, self.list.viewport_size()[1]);
        let offset = self.list.offset_y();
        if top < offset {
            self.list.jump_to(0.0, top);
        } else if top + self.row_height > offset + viewport {
            self.list.jump_to(0.0, top + self.row_height - viewport);
        }
    }

    fn activate(&self, index: usize) {
        let row = untracked(|| self.rows.with(|rows| rows.get(index).cloned()));
        if let Some(row) = row {
            self.activate.iter().for_each(|handler| handler(&row));
        }
    }

    /// 键盘走到第 `to` 行
    fn move_to(&self, to: usize, e: &KeyEvent) {
        let Some(key) = self.key_at(to) else { return };
        self.cursor.set(Some(key));
        self.keyboard.set(true);
        // 主修饰键：只走，不改选中（多选时用来走到别的行再按空格）
        if !(e.has_primary_modifier() && self.multiple()) {
            self.extend_or_replace(to, e.modifiers.has_shift(), ChangeSource::Keyboard);
        }
        self.reveal(to);
    }

    /// 按键：处理了返回 `true`
    fn handle_key(&self, e: &KeyEvent) -> bool {
        if !self.focus.is_focused() || e.modifiers.has_alt() {
            return false;
        }
        let len = self.len();
        let Key::Named(name) = &e.key else {
            let select_all = e.has_primary_modifier() && matches!(&e.key, Key::Character(c) if c.eq_ignore_ascii_case("a"));
            if select_all && self.multiple() && len > 0 {
                self.select(self.span(0, len - 1), ChangeSource::Keyboard);
                return true;
            }
            if matches!(&e.key, Key::Character(c) if c == " ") {
                return self.toggle_cursor();
            }
            return false;
        };
        let page = ((self.list.viewport_size()[1] / self.row_height).floor() as usize).saturating_sub(1).max(1);
        let from = self.cursor_index();
        let last = len.saturating_sub(1);
        let target = match name {
            NamedKey::ArrowDown => Some(from.map_or(0, |i| (i + 1).min(last))),
            NamedKey::ArrowUp => Some(from.map_or(0, |i| i.saturating_sub(1))),
            NamedKey::PageDown => Some(from.map_or(0, |i| (i + page).min(last))),
            NamedKey::PageUp => Some(from.map_or(0, |i| i.saturating_sub(page))),
            NamedKey::Home => Some(0),
            NamedKey::End => Some(last),
            NamedKey::ArrowLeft => {
                self.horizontal.scroll_by(-HORIZONTAL_STEP, 0.0);
                return true;
            }
            NamedKey::ArrowRight => {
                self.horizontal.scroll_by(HORIZONTAL_STEP, 0.0);
                return true;
            }
            NamedKey::Space => return self.toggle_cursor(),
            NamedKey::Enter => {
                if let Some(index) = from {
                    self.activate(index);
                }
                return from.is_some();
            }
            _ => None,
        };
        match target {
            Some(to) if len > 0 => {
                self.move_to(to, e);
                true
            }
            _ => false,
        }
    }

    /// 空格：多选时切换走到的这一行，单选时选中它
    fn toggle_cursor(&self) -> bool {
        let Some(key) = self.cursor.get_untracked() else { return false };
        self.keyboard.set(true);
        if self.multiple() {
            let mut keys: Vec<K> = self.selected.with_untracked(|s| s.iter().cloned().collect());
            match keys.iter().position(|k| *k == key) {
                Some(at) => {
                    keys.remove(at);
                }
                None => keys.push(key.clone()),
            }
            *self.anchor.borrow_mut() = Some(key);
            self.select(keys, ChangeSource::Keyboard);
        } else {
            self.select(vec![key], ChangeSource::Keyboard);
        }
        true
    }
}

/// 表格的构建器；[`table`] 是简写。给数据和每行的 key，再加几列。默认铺满父元素的宽度、高 320，
/// 比这多的行在里面滚动，用 `.height(..)`、`.grow(1)` 改。
///
/// ```no_run
/// # use aureole::prelude::*;
/// #[derive(Clone, PartialEq)]
/// struct Person { id: u32, name: String, age: u32 }
/// let people = signal(vec![Person { id: 1, name: "张三".into(), age: 32 }]);
/// let picked = signal(Vec::<u32>::new()); // `Option<u32>` 只能选一行
/// let sort = signal(None::<TableSort>);
/// // 数据由应用自己排：把排好的交给表格
/// let shown = move || {
///     let mut rows = people.get();
///     if let Some(s) = sort.get() {
///         rows.sort_by_key(|p| p.age);
///         if s.order == SortOrder::Descending {
///             rows.reverse();
///         }
///     }
///     rows
/// };
/// table(shown, |p: &Person| p.id)
///     .column(TableColumn::new("姓名", |p: &Person| p.name.clone()))
///     .column(TableColumn::new("年龄", |p: &Person| p.age.to_string()).width(80).sort_key("age"))
///     .selection(picked)
///     .sort(sort)
///     .on_row_activate(|p: &Person| println!("打开 {}", p.name))
///     .height(480);
/// ```
///
/// 部位：外框（`.style`）、表头（[`Table::header_style`]）、表头的一格（[`Table::header_cell_style`]）、一行（[`Table::row_style`]）、
/// 一个单元格（[`Table::cell_style`]）、调列宽的把手（[`Table::resizer_style`]）。状态：行有悬停、按下、选中、焦点（键盘走到的那一行）；
/// 表头的一格有悬停、按下、选中（正按这一列排序）。
///
/// 键盘（整个表格是一个 Tab 停靠点）：↑ ↓ 走一行，Page Up / Page Down 一屏，Home / End 到两头，走到哪选到哪；
/// Shift 加这些键选一段；主修饰键（Command、Ctrl）加这些键只走不选，空格切换这一行；主修饰键 + A 全选；← → 横着滚；回车和双击调用 [`Table::on_row_activate`]。
///
/// 对辅助技术：能选中时是网格，不能时是表格；行带选中状态、第几行和总行数，键盘走到哪一行读屏软件就念哪一行。
///
/// 行高是固定的；行离开视口就卸载，要保留的状态放进数据里。数据里同一个 key 的内容变了，这一行跟着变，选中不变。
pub struct Table<T: 'static, K: 'static> {
    source: ListSource<T>,
    key: KeyOf<T, K>,
    columns: Vec<TableColumn<T>>,
    selection: Option<Binding<K>>,
    sort: Option<Signal<Option<TableSort>>>,
    sort_changes: Changes<Option<TableSort>>,
    changes: Changes<Vec<K>>,
    activate: Vec<OnRow<T>>,
    resized: Vec<OnResize>,
    row_height: f32,
    row_element: Option<RowElement<T>>,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    handle: Option<ScrollHandle>,
    style: Style,
    header: Style,
    header_cell: Style,
    row: Style,
    cell: Style,
    resizer: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

element_methods!([T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static] Table<T, K>);

impl<T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static> Table<T, K> {
    /// `rows` 是数据（`Signal<Vec<T>>`、`Memo<Vec<T>>` 或者返回 `Vec<T>` 的闭包），`key` 认出每一行：选中、排序之后行还是那一行
    pub fn new(rows: impl IntoListSource<T>, key: impl Fn(&T) -> K + 'static) -> Self {
        Self {
            source: rows.into_list_source(),
            key: Rc::new(key),
            columns: Vec::new(),
            selection: None,
            sort: None,
            sort_changes: Changes::default(),
            changes: Changes::default(),
            activate: Vec::new(),
            resized: Vec::new(),
            row_height: ROW_HEIGHT,
            row_element: None,
            disabled: None,
            focus: None,
            handle: None,
            style: Style::new(),
            header: Style::new(),
            header_cell: Style::new(),
            row: Style::new(),
            cell: Style::new(),
            resizer: Style::new(),
            after: Default::default(),
        }
    }

    /// 加一列
    pub fn column(mut self, column: TableColumn<T>) -> Self {
        self.columns.push(column);
        self
    }

    /// 加几列
    pub fn columns(mut self, columns: impl IntoIterator<Item = TableColumn<T>>) -> Self {
        self.columns.extend(columns);
        self
    }

    /// 行能选中，选了哪些行放在 `value` 里：`Signal<Option<K>>` 选一行，`Signal<Vec<K>>` 选多行。不调用时行不能选中
    pub fn selection<S: TableSelection<K>>(mut self, value: Signal<S>) -> Self {
        self.selection = Some(Binding {
            read: Rc::new(move || value.with(S::keys)),
            write: Box::new(move |keys| value.set(S::from_keys(keys))),
            multiple: S::MULTIPLE,
        });
        self
    }

    /// 排序的状态放在 `value` 里，`None` 是不排序。表格只显示箭头、在点表头时改它，数据由应用自己按它排
    pub fn sort(mut self, value: Signal<Option<TableSort>>) -> Self {
        self.sort = Some(value);
        self
    }

    /// 行高（默认 32），逻辑像素
    pub fn row_height(mut self, height: impl Number) -> Self {
        self.row_height = height.to_f32().max(1.0);
        self
    }

    /// 用户改了选中（点击、键盘），参数是选中的行的 key（只能选一行时最多一个）。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<Vec<K>, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 改完了：选中每次改都是改完了，和 [`Table::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<Vec<K>, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 用户点表头改了排序
    pub fn on_sort<M>(mut self, handler: impl IntoChangeHandler<Option<TableSort>, M>) -> Self {
        self.sort_changes.on_change(handler);
        self
    }

    /// 打开一行：双击，或者键盘走到这一行时按回车
    pub fn on_row_activate(mut self, handler: impl Fn(&T) + 'static) -> Self {
        self.activate.push(Rc::new(handler));
        self
    }

    /// 用户拖动调了列宽（松手时），参数是第几列（从 0 起）和新的宽度
    pub fn on_column_resize(mut self, handler: impl Fn(usize, f32) + 'static) -> Self {
        self.resized.push(Rc::new(handler));
        self
    }

    /// 给每一行的元素再加东西（右键菜单、拖动、按数据换样子）：`row` 是这一行现在的数据
    pub fn row_element(mut self, f: impl Fn(Element, Signal<T>) -> Element + 'static) -> Self {
        self.row_element = Some(Rc::new(f));
        self
    }

    /// 禁用：变淡，不响应指针和键盘，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制焦点；[`super::Label::target`] 用同一个句柄时点标签就让它获得焦点
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用句柄读取和控制行的滚动（滚到第几行用 [`ScrollHandle::scroll_to_index`]）
    pub fn scroll_handle(mut self, handle: ScrollHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    /// 改外框的样子（大小、边框、圆角）
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改表头这一行的样子
    pub fn header_style(mut self, style: &Style) -> Self {
        self.header = self.header.extend(style);
        self
    }

    /// 改表头每一格的样子
    pub fn header_cell_style(mut self, style: &Style) -> Self {
        self.header_cell = self.header_cell.extend(style);
        self
    }

    /// 改每一行的样子
    pub fn row_style(mut self, style: &Style) -> Self {
        self.row = self.row.extend(style);
        self
    }

    /// 改每个单元格的样子
    pub fn cell_style(mut self, style: &Style) -> Self {
        self.cell = self.cell.extend(style);
        self
    }

    /// 改调列宽的把手的样子
    pub fn resizer_style(mut self, style: &Style) -> Self {
        self.resizer = self.resizer.extend(style);
        self
    }

    fn construct(self) -> Element {
        let Table { source, key, columns, selection, sort, sort_changes, changes, activate, resized, row_height, row_element, disabled, focus, handle, style, header, header_cell, row: row_look, cell, resizer, after: _ } = self;
        let l = look();
        let rows = source.reader();
        let focus = focus.unwrap_or_default();
        let (list, horizontal) = (handle.unwrap_or_default(), ScrollHandle::new());
        let disabled = Flag::new(disabled);
        let sort = sort.unwrap_or_else(|| signal(None));
        let sort_changes = Rc::new(sort_changes);
        let resized: Rc<[_]> = resized.into();

        // 列宽：可用的宽度变了、拖动调了都重新算。视口的大小在同一帧的布局之后写回来，改窗口大小时不差一帧
        let viewport = signal(Rect::default());
        let dragged: Vec<Signal<Option<f32>>> = columns.iter().map(|_| signal(None)).collect();
        let specs: Rc<[ColumnSize]> = columns.iter().map(|c| ColumnSize { width: c.width, grow: c.grow, min: c.min }).collect();
        let widths = {
            let dragged = dragged.clone();
            memo(move || {
                let sizes: Vec<ColumnSize> = specs.iter().zip(&dragged).map(|(s, d)| ColumnSize { width: d.get().or(s.width), ..*s }).collect();
                column_widths(&sizes, viewport.get().width)
            })
        };
        let columns: Rc<[LiveColumn<T>]> = columns
            .into_iter()
            .zip(dragged)
            .enumerate()
            .map(|(i, (c, dragged))| LiveColumn {
                title: reader(c.title),
                content: c.content,
                align: c.align,
                sort: c.sort,
                resizable: c.resizable,
                min: c.min,
                dragged,
                width: memo(move || widths.with(|w| w[i])),
            })
            .collect();

        let selected = signal(Rc::new(HashSet::new()));
        if let Some(binding) = &selection {
            // 选中的 key 放进集合，每行只查一下
            let read = binding.read.clone();
            Effect::new(move || selected.set(Rc::new(read().into_iter().collect())));
        }
        let selectable = selection.is_some();
        let keyboard = signal(false);
        let shared = Rc::new(Shared {
            rows,
            key: key.clone(),
            selection,
            selected,
            cursor: signal(None),
            anchor: RefCell::new(None),
            hint: Cell::new(0),
            row_height,
            list,
            horizontal,
            focus,
            keyboard,
            changes,
            activate,
        });

        // 同一个 key 的内容变了：挂载着的行不重建（滚动位置、悬停、单元格里的状态都留着），只更新这一行的信号
        let live: LiveRows<T, K> = Rc::new(RefCell::new(HashMap::new()));
        {
            let (live, key) = (live.clone(), key.clone());
            Effect::new(move || {
                let changed: Vec<(Signal<T>, T)> = rows.with(|items| {
                    let live = live.borrow();
                    let mut left = live.len();
                    let mut changed = Vec::new();
                    for item in items {
                        if left == 0 {
                            break;
                        }
                        if let Some(cell) = live.get(&key(item)) {
                            left -= 1;
                            if cell.with_untracked(|current| current != item) {
                                changed.push((*cell, item.clone()));
                            }
                        }
                    }
                    changed
                });
                for (cell, item) in changed {
                    cell.set(item);
                }
            });
        }

        let row_parts = Rc::new(RowParts { row: l.table_row.clone().extend(&row_look), cell: l.table_cell.clone().extend(&cell) });
        let list_view = {
            let (shared, columns, key) = (shared.clone(), columns.clone(), key.clone());
            move |item: T| table_row(&shared, &columns, &row_parts, &row_element, &live, key(&item), item, selectable)
        };
        let body = virtual_list(rows, move |item: &T| key(item), row_height, list_view).scroll_handle(list).grow(1).min_height(0.0);

        let header_parts = HeaderParts { cell: l.table_header_cell.clone().extend(&header_cell), resizer: l.table_resizer.clone().extend(&resizer) };
        let header_row = part(row(), &l.table_header, &header, &States::new())
            .shrink(0)
            .accessibility_role(Role::Row)
            .children(columns.iter().enumerate().map(|(index, column)| header_cell_element(column, index, &header_parts, sort, &sort_changes, &resized)).collect::<Vec<_>>());

        let total = memo(move || widths.with(|w| w.iter().sum::<f32>()));
        let content = column().width(move || total.get().max(viewport.get().width)).height(pct(100)).child(header_row).child(rect().style(&l.table_divider)).child(body);
        let scroller = scroll_x(content).scroll_handle(horizontal).report_bounds(viewport).grow(1).min_height(0.0);

        let states = disabled.state(States::new(), State::Disabled);
        let root = disabled.apply(column());
        let root = part(root, &l.table, &style, &states)
            .focus_handle(focus)
            .focusable(true)
            .accessibility_role(if selectable { Role::Grid } else { Role::Table })
            .on_keyboard_focus(move |focused| keyboard.set(focused))
            .on_focus(move |focused| {
                if !focused {
                    keyboard.set(false);
                }
            })
            .child(scroller);
        on_key(root, move |e| {
            let handled = shared.handle_key(e);
            if handled {
                e.prevent_builtin(Builtin::SCROLL);
            }
            handled
        })
    }
}

impl<T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static> View for Table<T, K> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 表格的简写，见 [`Table`]
pub fn table<T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static>(rows: impl IntoListSource<T>, key: impl Fn(&T) -> K + 'static) -> Table<T, K> {
    Table::new(rows, key)
}

/// 一行的部位（默认的加上使用的人给的，已经合好）
struct RowParts {
    row: Style,
    cell: Style,
}

struct HeaderParts {
    cell: Style,
    resizer: Style,
}

/// 文字的对齐对应的内容对齐
fn justify_of(align: TextAlign) -> Justify {
    match align {
        TextAlign::Center => Justify::Center,
        TextAlign::End => Justify::End,
        _ => Justify::Start,
    }
}

/// 一行
#[allow(clippy::too_many_arguments, reason = "只在这个文件里用，都是挂载一行要的东西")]
fn table_row<T: Clone + PartialEq + 'static, K: Clone + Eq + Hash + 'static>(
    shared: &Rc<Shared<T, K>>,
    columns: &Rc<[LiveColumn<T>]>,
    parts: &Rc<RowParts>,
    row_element: &Option<RowElement<T>>,
    live: &LiveRows<T, K>,
    key: K,
    item: T,
    selectable: bool,
) -> Element {
    let data = signal(item);
    live.borrow_mut().insert(key.clone(), data);
    {
        let (live, key) = (live.clone(), key.clone());
        on_cleanup(move || {
            live.borrow_mut().remove(&key);
        });
    }
    let press = Press::new();
    let at_cursor = {
        let (cursor, key) = (shared.cursor, key.clone());
        memo(move || cursor.with(|c| c.as_ref() == Some(&key)))
    };
    let keyboard = shared.keyboard;
    let mut states = press.pointer_states().focused(move || at_cursor.get() && keyboard.get());
    let mut el = press.attach_pointer(row()).accessibility_role(Role::Row).accessibility_active(at_cursor);
    let is_selected = selectable.then(|| {
        let (selected, key) = (shared.selected, key.clone());
        memo(move || selected.with(|s| s.contains(&key)))
    });
    if let Some(is_selected) = is_selected {
        states = states.selected(is_selected);
        el = el.accessibility_selected(is_selected);
    }
    let cells: Vec<Element> = columns.iter().map(|column| table_cell(column, data, &parts.cell, is_selected)).collect();
    let click = {
        let shared = shared.clone();
        move |e: &ClickEvent| {
            if !e.is_builtin_prevented(Builtin::ACTIVATE) {
                shared.focus.focus();
                shared.keyboard.set(false);
                shared.click(key.clone(), e.modifiers, e.source.into());
                if e.clicks == 2
                    && let Some(index) = shared.index_of(&key)
                {
                    shared.activate(index);
                }
            }
            e.stop_propagation();
        }
    };
    let el = part(el, &parts.row, &Style::new(), &states).align(Align::Center).builtin_handlers(|el| el.on_click(click)).children(cells);
    match row_element {
        Some(f) => f(el, data),
        None => el,
    }
}

/// 一个单元格：放得下字的元素本身就是格子（少一层元素），自己搭的内容外面包一层。
/// 能选中的表格（网格）里是网格的格子，带着这一行的选中状态（`selected`）：Windows 的无障碍接口只从格子上报选中，
/// 行上的不报（AccessKit 还没做表格的行），读屏软件靠它知道哪些行选中了
fn table_cell<T: Clone + PartialEq + 'static>(column: &LiveColumn<T>, data: Signal<T>, style: &Style, selected: Option<Memo<bool>>) -> Element {
    let width = column.width;
    let cell = match &column.content {
        Content::Text(value) => {
            let value = value.clone();
            text(move || data.with(|row| value(row))).max_lines(1).text_align(column.align)
        }
        Content::View(view) => row().align(Align::Center).justify(justify_of(column.align)).child(view(data)),
    };
    let cell = cell.style(style).width(move || width.get()).shrink(0);
    match selected {
        Some(selected) => cell.accessibility_role(Role::GridCell).accessibility_selected(selected),
        None => cell.accessibility_role(Role::Cell),
    }
}

/// 表头的一格
fn header_cell_element<T: 'static>(column: &LiveColumn<T>, index: usize, parts: &HeaderParts, sort: Signal<Option<TableSort>>, changes: &Rc<Changes<Option<TableSort>>>, resized: &Rc<[OnResize]>) -> Element {
    let width = column.width;
    let press = Press::new();
    let key = column.sort;
    let order = memo(move || sort.get().filter(|s| Some(s.key) == key).map(|s| s.order));
    let mut states = States::new().selected(move || order.get().is_some());
    if key.is_some() {
        states = press.pointer_states().selected(move || order.get().is_some());
    }
    let name = column.title.clone();
    let title = text(move || name()).max_lines(1).shrink(1).min_width(0.0).text_align(column.align);
    let mut el = part(row().width(move || width.get()).shrink(0).align(Align::Center).justify(justify_of(column.align)), &parts.cell, &Style::new(), &states).accessibility_role(Role::ColumnHeader).child(title);
    if let Some(key) = key {
        let changes = changes.clone();
        el = on_activate(press.attach_pointer(el), move |source| {
            let next = match sort.get_untracked() {
                Some(TableSort { key: current, order: SortOrder::Ascending }) if current == key => Some(TableSort { key, order: SortOrder::Descending }),
                Some(TableSort { key: current, order: SortOrder::Descending }) if current == key => None,
                _ => Some(TableSort { key, order: SortOrder::Ascending }),
            };
            sort.set(next);
            changes.changed(next, source);
        })
        .child_when(move || order.get() == Some(SortOrder::Ascending), || sort_mark("M6 15 L12 9 L18 15"))
        .child_when(move || order.get() == Some(SortOrder::Descending), || sort_mark("M6 9 L12 15 L18 9"));
    }
    if column.resizable {
        el = el.child(resize_grip(column, index, parts, resized));
    }
    el
}

/// 表头里表示排序方向的小箭头
fn sort_mark(svg: &str) -> Element {
    path(VectorPath::svg(svg).view_box(0.0, 0.0, 24.0, 24.0)).size(12, 12).shrink(0).margin_left(4).stroke(Stroke::new(2.4).round())
}

/// 调列宽的把手：贴着表头这一格的右边
fn resize_grip<T: 'static>(column: &LiveColumn<T>, index: usize, parts: &HeaderParts, resized: &Rc<[OnResize]>) -> Element {
    let (width, dragged, min) = (column.width, column.dragged, column.min);
    let resized = resized.clone();
    let press = Press::new();
    let started = Cell::new(0.0f32);
    let grip = press.attach_pointer(rect()).inset_top(0).inset_right(0).height(pct(100)).cursor(Cursor::ColumnResize).accessibility_hidden(true);
    part(grip, &parts.resizer, &Style::new(), &press.pointer_states())
        .on_drag(move |e: &DragEvent| match e.phase {
            GesturePhase::Start => started.set(width.get_untracked()),
            GesturePhase::Move => without_animation(|| dragged.set(Some((started.get() + e.translation[0]).max(min)))),
            GesturePhase::End => {
                let last = width.get_untracked();
                resized.iter().for_each(|handler| handler(index, last));
            }
            _ => {}
        })
        .builtin_handlers(|el| el.on_click(|e: &ClickEvent| e.stop_propagation()))
}
