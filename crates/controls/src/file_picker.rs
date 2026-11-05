//! 文件选择：一个按钮弹出系统的文件对话框，旁边显示选了什么，也能把文件拖进来。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use reactive::{Signal, signal};

use super::button::button;
use super::look::look;
use super::{Changes, Flag, Press, Stored, on_activate, part};
use aureole_core::{
    ChangeSource, Element, FieldValue, FileDialog, FileDialogKind, FileDropEvent, FileFilter, FocusHandle, IntoChangeHandler, IntoProp, Prop, Role, State, States, Stroke, Style, VectorPath, View, path,
    row, spawn_local, text,
};

/// 文件选择绑定的值：`Option<PathBuf>` 选一个，`Vec<PathBuf>` 选多个
pub trait FileValue: Clone + PartialEq + 'static {
    /// 能不能选多个
    const MULTIPLE: bool;
    /// 选了哪些
    fn paths(&self) -> Vec<PathBuf>;
    /// 选了这些之后绑定的信号里放什么；只能选一个的取第一个
    fn from_paths(paths: Vec<PathBuf>) -> Self;
}

impl FileValue for Option<PathBuf> {
    const MULTIPLE: bool = false;

    fn paths(&self) -> Vec<PathBuf> {
        self.iter().cloned().collect()
    }

    fn from_paths(paths: Vec<PathBuf>) -> Self {
        paths.into_iter().next()
    }
}

impl FileValue for Vec<PathBuf> {
    const MULTIPLE: bool = true;

    fn paths(&self) -> Vec<PathBuf> {
        self.clone()
    }

    fn from_paths(paths: Vec<PathBuf>) -> Self {
        paths
    }
}

/// 文件选择上的文字。[`FileTexts::chinese`] 是默认的，[`FileTexts::english`] 是英文的，也可以只改其中几项
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTexts {
    /// 按钮上的字：选文件
    pub choose_file: String,
    /// 按钮上的字：选文件夹
    pub choose_directory: String,
    /// 按钮上的字：选保存的位置
    pub choose_save: String,
    /// 没选时显示的字
    pub empty: String,
    /// 选了多个时显示的字，`{count}` 是个数
    pub many: String,
    /// 清除按钮的名字（辅助技术念的）
    pub clear: String,
}

impl FileTexts {
    /// 中文（默认）
    pub fn chinese() -> Self {
        Self {
            choose_file: "选择文件…".into(),
            choose_directory: "选择文件夹…".into(),
            choose_save: "保存到…".into(),
            empty: "没有选择".into(),
            many: "{count} 个文件".into(),
            clear: "清除".into(),
        }
    }

    /// 英文
    pub fn english() -> Self {
        Self {
            choose_file: "Choose file…".into(),
            choose_directory: "Choose folder…".into(),
            choose_save: "Save to…".into(),
            empty: "Nothing selected".into(),
            many: "{count} files".into(),
            clear: "Clear".into(),
        }
    }
}

impl Default for FileTexts {
    fn default() -> Self {
        Self::chinese()
    }
}

/// 文件选择的构建器；[`file_picker`] 是简写。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::{FileDialogKind};
/// # use std::path::PathBuf;
/// let file = signal(None::<PathBuf>);
/// file_picker(file);
/// // 只要图片，选多个：绑定 Vec
/// let photos = signal(Vec::<PathBuf>::new());
/// FilePicker::new(photos).file_filter("图片", &["png", "jpg", "webp"]).on_commit(|p: Vec<PathBuf>| println!("{p:?}"));
/// // 选文件夹、选保存的位置
/// FilePicker::new(file).kind(FileDialogKind::OpenDirectory);
/// FilePicker::new(file).kind(FileDialogKind::Save).file_name("导出.csv");
/// ```
///
/// 部位：外壳（`.style`）、按钮（[`FilePicker::button_style`]）、选了什么的文字（[`FilePicker::label_style`]）、清除按钮
/// （[`FilePicker::clear_style`]）。外壳的状态：打开（文件拖到上面了）、禁用。默认：按钮加一段文字，文字太长时只显示一行，悬停提示完整的路径；
/// 选了东西时右边多一个清除按钮；文件拖到上面时描边变强调色。没有动画。
///
/// 按钮是真正的系统对话框（[`FileDialog`]），开着时界面照常运行，再按没有反应。也能把文件拖进来（[`FilePicker::droppable`] 关掉）：
/// 只收符合 [`FilePicker::file_filter`] 的文件（只选一个时取第一个，选文件夹时只收文件夹，保存时不收）。取消对话框时值不变。
///
/// 每次选都是改完了：[`FilePicker::on_change`]、[`FilePicker::on_commit`] 同时调用；代码里改绑定的信号时都不调用。
pub struct FilePicker<V: 'static> {
    value: Signal<V>,
    kind: FileDialogKind,
    title: Option<String>,
    start_directory: Option<PathBuf>,
    file_name: Option<String>,
    filters: Vec<FileFilter>,
    texts: FileTexts,
    droppable: bool,
    clearable: bool,
    describe: Option<Describe>,
    disabled: Option<Prop<bool>>,
    focus: Option<FocusHandle>,
    changes: Changes<V>,
    view: Option<Box<dyn FnOnce(FileState<V>) -> Element>>,
    style: Style,
    button: Style,
    label: Style,
    clear: Style,
    /// 元素的方法，建好元素时用上
    after: crate::ElementCalls,
}

/// 文件选择的逻辑，不带界面：选了什么、弹对话框、哪些文件能收、选了之后通知谁。默认的文件选择也只用这里的方法。
/// 自己重做界面（[`FilePicker::view`]）时绑定的信号、`on_change`、`on_commit`、过滤、禁用都照常工作。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use std::path::PathBuf;
/// let photo = signal(None::<PathBuf>);
/// // 一块能点也能把文件拖进来的区域
/// FilePicker::new(photo).file_filter("图片", &["png", "jpg"]).view(|state: FileState<Option<PathBuf>>| {
///     column()
///         .size(240, 120)
///         .background(move || if state.is_hovering() { Color::hex(0xe7effd) } else { Color::hex(0xf0f2f5) })
///         .on_click(move |e: &ClickEvent| state.choose(e.source.into()))
///         .on_file_hover(move |over: bool| state.set_hovering(over))
///         .on_file_drop(move |e: &FileDropEvent| state.handle_drop(e))
///         .child(text(move || state.label()))
/// });
/// ```
pub struct FileState<V: 'static> {
    value: Signal<V>,
    waiting: Signal<bool>,
    hovering: Signal<bool>,
    disabled: Flag,
    focus: Option<FocusHandle>,
    shared: Stored<FileShared<V>>,
}

impl<V: 'static> Clone for FileState<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: 'static> Copy for FileState<V> {}

struct FileShared<V> {
    kind: FileDialogKind,
    title: Option<String>,
    start_directory: Option<PathBuf>,
    file_name: Option<String>,
    filters: Vec<FileFilter>,
    texts: FileTexts,
    describe: Option<Describe>,
    changes: Changes<V>,
}

impl<V: FileValue> FileState<V> {
    /// 选了哪些，响应式
    pub fn paths(self) -> Vec<PathBuf> {
        self.value.with(V::paths)
    }

    /// 有没有选，响应式
    pub fn has_value(self) -> bool {
        self.value.with(|v| !v.paths().is_empty())
    }

    /// 选了什么的文字，响应式：没选时是 [`FileTexts::empty`]，选了一个时是文件名（或者 [`FilePicker::describe`] 写的），选了多个时是个数
    pub fn label(self) -> String {
        let shared = self.shared.get();
        match self.paths().as_slice() {
            [] => shared.texts.empty.clone(),
            [only] => match &shared.describe {
                Some(describe) => describe(only),
                None => only.file_name().map_or_else(|| only.display().to_string(), |name| name.to_string_lossy().into_owned()),
            },
            all => shared.texts.many.replace("{count}", &all.len().to_string()),
        }
    }

    /// 选的完整路径，一行一个，响应式
    pub fn full_paths(self) -> String {
        self.paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
    }

    /// 选什么（[`FilePicker::kind`]）
    pub fn kind(self) -> FileDialogKind {
        self.shared.get().kind
    }

    /// 文字
    pub fn texts(self) -> FileTexts {
        self.shared.get().texts.clone()
    }

    /// 对话框是不是开着，响应式
    pub fn is_waiting(self) -> bool {
        self.waiting.get()
    }

    /// 文件是不是拖到上面了，响应式。自己的界面在 `on_file_hover` 里调用 [`FileState::set_hovering`]
    pub fn is_hovering(self) -> bool {
        self.hovering.get()
    }

    /// 记下文件是不是拖到上面了
    pub fn set_hovering(self, hovering: bool) {
        self.hovering.set(hovering);
    }

    /// 是不是禁用，响应式
    pub fn is_disabled(self) -> bool {
        self.disabled.get()
    }

    /// [`FilePicker::focus_handle`] 给的焦点句柄：放到自己的元素上，点标签才能作用到它
    pub fn focus_handle(self) -> Option<FocusHandle> {
        self.focus
    }

    /// 用户选了这些：变了才写、才调用 `on_change` 和 `on_commit`。只能选一个的取第一个。禁用时什么都不做
    pub fn set_paths(self, paths: Vec<PathBuf>, source: ChangeSource) {
        if self.disabled.get() {
            return;
        }
        let next = V::from_paths(paths);
        if self.value.with_untracked(|current| *current != next) {
            self.value.set(next.clone());
            self.shared.get().changes.both(next, source);
        }
    }

    /// 用户清除了
    pub fn clear(self, source: ChangeSource) {
        self.set_paths(Vec::new(), source);
    }

    /// 弹出系统的文件对话框，选好了就是 [`FileState::set_paths`]，取消时值不变。对话框开着时、禁用时什么都不做
    pub fn choose(self, source: ChangeSource) {
        if self.waiting.get_untracked() || self.disabled.get() {
            return;
        }
        self.waiting.set(true);
        let shared = self.shared.get();
        let mut dialog = FileDialog::new();
        if let Some(title) = &shared.title {
            dialog = dialog.title(title.clone());
        }
        if let Some(directory) = &shared.start_directory {
            dialog = dialog.directory(directory.clone());
        }
        if let Some(name) = &shared.file_name {
            dialog = dialog.file_name(name.clone());
        }
        for filter in &shared.filters {
            let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
            dialog = dialog.file_filter(&filter.name, &extensions);
        }
        let kind = shared.kind;
        spawn_local(async move {
            let paths = match kind {
                FileDialogKind::OpenFile if V::MULTIPLE => dialog.pick_files().await,
                FileDialogKind::OpenFile => dialog.pick_file().await.into_iter().collect(),
                FileDialogKind::OpenDirectory => dialog.pick_directory().await.into_iter().collect(),
                FileDialogKind::Save => dialog.save_file().await.into_iter().collect(),
            };
            self.waiting.set(false);
            // 取消了：值不变
            if !paths.is_empty() {
                self.set_paths(paths, source);
            }
        });
    }

    /// 这个路径能不能收：选文件夹时只收文件夹，选文件时按 [`FilePicker::file_filter`] 过滤，保存时不收
    pub fn accepts(self, path: &Path) -> bool {
        let shared = self.shared.get();
        match shared.kind {
            FileDialogKind::OpenDirectory => path.is_dir(),
            FileDialogKind::OpenFile => !path.is_dir() && matches_filters(path, &shared.filters),
            FileDialogKind::Save => false,
        }
    }

    /// 处理拖进来的文件：只收能收的，只能选一个时取第一个
    pub fn handle_drop(self, event: &FileDropEvent) {
        self.hovering.set(false);
        let usable = event.paths.iter().filter(|path| self.accepts(path));
        let paths: Vec<PathBuf> = if V::MULTIPLE { usable.cloned().collect() } else { usable.take(1).cloned().collect() };
        if !paths.is_empty() {
            self.set_paths(paths, ChangeSource::Pointer);
        }
    }
}

/// 选了一个时怎么显示
type Describe = Rc<dyn Fn(&Path) -> String>;

/// 清除的图标（24 的画布）
fn cross() -> VectorPath {
    VectorPath::svg("M7 7 L17 17 M17 7 L7 17").view_box(0.0, 0.0, 24.0, 24.0)
}

/// 路径的扩展名符合这些类型。没有限制时都符合；比较时不分大小写
fn matches_filters(path: &Path, filters: &[FileFilter]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else { return false };
    filters.iter().any(|filter| filter.extensions.iter().any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(extension)))
}

element_methods!([V: FileValue] FilePicker<V>);

impl<V: FileValue> FilePicker<V> {
    /// 绑定到 `value` 的文件选择。`value` 是 `Option<PathBuf>`（选一个）或 `Vec<PathBuf>`（选多个）
    pub fn new(value: Signal<V>) -> Self {
        Self {
            after: Default::default(),
            value,
            kind: FileDialogKind::OpenFile,
            title: None,
            start_directory: None,
            file_name: None,
            filters: Vec::new(),
            texts: FileTexts::chinese(),
            droppable: true,
            clearable: true,
            describe: None,
            disabled: None,
            focus: None,
            changes: Changes::default(),
            view: None,
            style: Style::new(),
            button: Style::new(),
            label: Style::new(),
            clear: Style::new(),
        }
    }

    /// 选什么：文件（默认）、文件夹、保存的位置
    pub fn kind(mut self, kind: FileDialogKind) -> Self {
        self.kind = kind;
        self
    }

    /// 对话框的标题
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// 对话框一开始显示的文件夹
    pub fn start_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.start_directory = Some(directory.into());
        self
    }

    /// 保存时默认的文件名
    pub fn file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }

    /// 只允许这些扩展名（不带点），可以多次调用。对话框和拖进来的文件都按它过滤
    pub fn file_filter(mut self, name: &str, extensions: &[&str]) -> Self {
        self.filters.push(FileFilter { name: name.to_owned(), extensions: extensions.iter().map(|e| (*e).to_owned()).collect() });
        self
    }

    /// 换掉文字（默认中文）
    pub fn texts(mut self, texts: FileTexts) -> Self {
        self.texts = texts;
        self
    }

    /// 能不能把文件拖进来（默认能）
    pub fn droppable(mut self, droppable: bool) -> Self {
        self.droppable = droppable;
        self
    }

    /// 选了东西时显示不显示清除按钮（默认显示）
    pub fn clearable(mut self, clearable: bool) -> Self {
        self.clearable = clearable;
        self
    }

    /// 选了一个时怎么显示（默认是文件名）
    pub fn describe(mut self, describe: impl Fn(&Path) -> String + 'static) -> Self {
        self.describe = Some(Rc::new(describe));
        self
    }

    /// 禁用：变淡，不能打开和拖进来，Tab 跳过
    pub fn disabled(mut self, disabled: impl IntoProp<bool>) -> Self {
        self.disabled = Some(disabled.into_prop());
        self
    }

    /// 用句柄控制按钮的焦点；[`super::Label::target`] 用同一个句柄时点标签就打开对话框
    pub fn focus_handle(mut self, handle: FocusHandle) -> Self {
        self.focus = Some(handle);
        self
    }

    /// 用户选了（对话框、拖进来、清除），参数是新的值。代码里改绑定的信号时不调用
    pub fn on_change<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_change(handler);
        self
    }

    /// 选完了：每次选都是完成的，和 [`FilePicker::on_change`] 同时调用
    pub fn on_commit<M>(mut self, handler: impl IntoChangeHandler<V, M>) -> Self {
        self.changes.on_commit(handler);
        self
    }

    /// 完全重做界面：不用默认的样子，自己搭，绑定的信号、`on_change`、`on_commit`、过滤、禁用都照常工作，逻辑在 [`FileState`] 上。
    /// 样式对象的那些方法这时不起作用，角色、键盘、拖放也要自己接
    pub fn view<C: View>(mut self, view: impl FnOnce(FileState<V>) -> C + 'static) -> Self {
        self.view = Some(Box::new(move |state| view(state).into_element()));
        self
    }

    /// 改外壳的样子：打开状态是文件拖到上面了
    pub fn style(mut self, style: &Style) -> Self {
        self.style = self.style.extend(style);
        self
    }

    /// 改按钮的样子
    pub fn button_style(mut self, style: &Style) -> Self {
        self.button = self.button.extend(style);
        self
    }

    /// 改选了什么的文字的样子
    pub fn label_style(mut self, style: &Style) -> Self {
        self.label = self.label.extend(style);
        self
    }

    /// 改清除按钮的样子
    pub fn clear_style(mut self, style: &Style) -> Self {
        self.clear = self.clear.extend(style);
        self
    }

    fn construct(self) -> Element {
        let FilePicker { value, kind, title, start_directory, file_name, filters, texts, droppable, clearable, describe, disabled, focus, mut changes, view, style, button: button_style, label, clear, after: _ } = self;
        let report = changes.report(|v: &V| FieldValue::Text(v.paths().iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")));
        let disabled = Flag::new(disabled);
        let hovering = signal(false);
        let caption = match kind {
            FileDialogKind::OpenFile => texts.choose_file.clone(),
            FileDialogKind::OpenDirectory => texts.choose_directory.clone(),
            FileDialogKind::Save => texts.choose_save.clone(),
        };
        let clear_name = texts.clear.clone();
        let state = FileState {
            value,
            waiting: signal(false),
            hovering,
            disabled,
            focus,
            shared: Stored::new(FileShared { kind, title, start_directory, file_name, filters, texts, describe, changes }),
        };
        if let Some(view) = view {
            return view(state).field_handle(report);
        }
        let l = look();
        let mut chooser = button(caption.clone()).name(caption);
        if let Some(handle) = focus {
            chooser = chooser.focus_handle(handle);
        }
        let chooser = part(disabled.apply(chooser), &l.file_button, &button_style, &States::new()).on_click(move |e: &aureole_core::ClickEvent| state.choose(e.source.into()));
        let has_value = move || state.has_value();
        let label_states = States::new().selected(has_value);
        let name_label = part(text(move || state.label()).max_lines(1).grow(1).min_width(0).name("file-label"), &l.file_label, &label, &label_states).tooltip(move || state.full_paths());
        // 每次出现时新建：选了又清除、再选，它会卸载又挂上
        let clear_button = {
            let l = l.clone();
            move || {
                let press = Press::new();
                let states = press.pointer_states();
                let el = on_activate(press.attach_pointer(row()).accessibility_role(Role::Button).accessibility_name(clear_name.clone()).name(clear_name.clone()), move |source| state.clear(source));
                part(disabled.apply(el), &l.file_clear, &clear, &states).child(path(cross()).size(12, 12).stroke(Stroke::new(2.0).round()))
            }
        };
        let states = disabled.state(States::new().open(hovering), State::Disabled);
        let mut el = part(disabled.apply(row()), &l.file, &style, &states).field_handle(report).child(chooser).child(name_label);
        if clearable {
            el = el.child_when(has_value, clear_button);
        }
        if droppable && kind != FileDialogKind::Save {
            el = el.on_file_hover(move |over: bool| state.set_hovering(over)).on_file_drop(move |e: &FileDropEvent| state.handle_drop(e));
        }
        el
    }
}

impl<V: FileValue> View for FilePicker<V> {
    fn into_element(self) -> Element {
        self.build()
    }
}

/// 文件选择，[`FilePicker::new`] 的简写
pub fn file_picker<V: FileValue>(value: Signal<V>) -> FilePicker<V> {
    FilePicker::new(value)
}
