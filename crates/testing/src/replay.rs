//! 回放录下的输入（[`super::Harness::replay`]）得到的快照：处理器的调用记录，加上关键状态（焦点、指针下的元素、
//! 按下的元素、指针形状、浮层、命名的输入框和滚动视口、测试点名的信号）每次变化的时刻。不记像素。
//!
//! 快照是文本，一行一条，按发生的顺序：
//!
//! ```text
//! # aureole replay snapshot v1
//! 0.0083 hover card > row
//! 0.0250 call row click
//! 0.0250 focus name
//! 0.0333 text name "ab" 2..2
//! ```

use std::cell::RefCell;
use std::fmt::{Debug, Display};
use std::path::Path;
use std::rc::Rc;

use reactive::untracked;

use super::Harness;
use aureole_core::{ClickEvent, DragEvent, Key, KeyEvent, Modifiers, PointerEvent};

/// 快照文件的第一行
const HEADER: &str = "# aureole replay snapshot v1";

/// 处理器的调用记录：测试视图的处理器往里写（哪个元素、什么事件），回放时快照按发生的顺序收进来。
/// 克隆的是同一份记录。帮手方法返回能直接交给链式方法的处理器：
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::testing::CallLog;
/// let log = CallLog::new();
/// rect().name("row").on_click(log.event("row", "click")).on_secondary_click(log.click("row", "secondary"));
/// ```
#[derive(Clone, Default)]
pub struct CallLog(Rc<RefCell<Vec<String>>>);

impl CallLog {
    /// 空的记录
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一行：`元素 事件`
    pub fn push(&self, element: &str, event: impl Display) {
        self.0.borrow_mut().push(format!("{element} {event}"));
    }

    /// 取走记下的
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.borrow_mut())
    }

    /// 没有参数的处理器（`on_click`、`on_drag_leave`……）：记 `元素 事件`
    pub fn event(&self, element: &str, event: &str) -> impl Fn() + 'static {
        let (log, line) = (self.clone(), format!("{element} {event}"));
        move || log.0.borrow_mut().push(line.clone())
    }

    /// 一个参数的处理器（`on_hover`、`on_press`、`on_focus`、`on_change`、`on_submit`……）：记 `元素 事件 值`
    pub fn value<T: Debug>(&self, element: &str, event: &str) -> impl Fn(T) + 'static {
        let (log, prefix) = (self.clone(), format!("{element} {event}"));
        move |value: T| log.0.borrow_mut().push(format!("{prefix} {value:?}"))
    }

    /// 指针事件（`on_pointer_move`、`on_pointer_down`……）：记元素自己的坐标、键、连击数、修饰键
    pub fn pointer(&self, element: &str, event: &str) -> impl Fn(&PointerEvent) + 'static {
        let (log, prefix) = (self.clone(), format!("{element} {event}"));
        move |e: &PointerEvent| {
            let [x, y] = e.position;
            let button = e.button.map(|b| format!(" {b:?}")).unwrap_or_default();
            log.0.borrow_mut().push(format!("{prefix} ({x:.1}, {y:.1}){button} clicks={}{}", e.clicks, mods(e.modifiers)));
        }
    }

    /// 点击（`on_secondary_click`、`on_double_click`、`on_middle_click`）：记元素自己的坐标、键、连击数、修饰键
    pub fn click(&self, element: &str, event: &str) -> impl Fn(&ClickEvent) + 'static {
        let (log, prefix) = (self.clone(), format!("{element} {event}"));
        move |e: &ClickEvent| {
            let [x, y] = e.position;
            log.0.borrow_mut().push(format!("{prefix} ({x:.1}, {y:.1}) {:?} clicks={}{}", e.button, e.clicks, mods(e.modifiers)));
        }
    }

    /// 拖动（`on_drag`）：记阶段、元素自己的坐标、总位移
    pub fn drag(&self, element: &str) -> impl Fn(&DragEvent) + 'static {
        let (log, prefix) = (self.clone(), format!("{element} drag"));
        move |e: &DragEvent| {
            let ([x, y], [dx, dy]) = (e.position, e.translation);
            log.0.borrow_mut().push(format!("{prefix} {:?} ({x:.1}, {y:.1}) moved ({dx:.1}, {dy:.1}){}", e.phase, mods(e.modifiers)));
        }
    }

    /// 按键（`on_key_down`、`on_key_up`）：记按键和修饰键；`handled` 时停止传播
    pub fn key(&self, element: &str, event: &str, handled: bool) -> impl Fn(&KeyEvent) + 'static {
        let (log, prefix) = (self.clone(), format!("{element} {event}"));
        move |e: &KeyEvent| {
            let repeat = if e.repeated { " repeat" } else { "" };
            log.0.borrow_mut().push(format!("{prefix} {}{}{repeat}", key_name(&e.key), mods(e.modifiers)));
            if handled {
                e.stop_propagation();
            }
        }
    }
}

fn key_name(key: &Key) -> String {
    match key {
        Key::Named(named) => named.name().to_owned(),
        Key::Character(c) => format!("{:?}", c.as_str()),
        Key::Dead(c) => format!("dead {c:?}"),
        Key::Unidentified => "unidentified".to_owned(),
    }
}

fn mods(modifiers: Modifiers) -> String {
    if modifiers.is_empty() { String::new() } else { format!(" {modifiers:?}") }
}

/// 回放时看什么：除了总会记的（焦点、焦点环、指针下的元素、按下的元素、指针形状、浮层），
/// 还有调用记录、按名字看的输入框（可选中的文字）和滚动视口、信号
#[derive(Default)]
pub struct Watch {
    log: Option<CallLog>,
    texts: Vec<String>,
    scrolls: Vec<String>,
    signals: Vec<(String, Box<dyn Fn() -> String>)>,
}

impl Watch {
    /// 只看总会记的那些
    pub fn new() -> Self {
        Self::default()
    }

    /// 收进这份调用记录
    pub fn log(mut self, log: &CallLog) -> Self {
        self.log = Some(log.clone());
        self
    }

    /// 命名的输入框：内容、选区、预编辑文字；可选中的文字：选中的部分
    pub fn text(mut self, name: &str) -> Self {
        self.texts.push(name.to_owned());
        self
    }

    /// 命名的滚动视口（或者它里面第一个视口）的偏移
    pub fn scroll(mut self, name: &str) -> Self {
        self.scrolls.push(name.to_owned());
        self
    }

    /// 一个值（通常读一个信号），按 `Debug` 记
    pub fn signal<T: Debug>(mut self, name: &str, read: impl Fn() -> T + 'static) -> Self {
        self.signals.push((name.to_owned(), Box::new(move || format!("{:?}", untracked(&read)))));
        self
    }
}

/// 回放的结果，见模块说明。和存着的快照比较用 [`Snapshot::assert_matches`]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    text: String,
    /// 不在这里回放的原因（录制时的系统和这里不同）
    skipped: Option<String>,
}

impl Snapshot {
    pub(super) fn skipped(reason: String) -> Self {
        Self { text: String::new(), skipped: Some(reason) }
    }

    /// 快照的文本
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 没有回放（录制时的系统和这里不同）时是原因
    pub fn skip_reason(&self) -> Option<&str> {
        self.skipped.as_deref()
    }

    /// 和存在 `path` 的快照比较，不一样时 panic，列出不一样的几行。设了环境变量 `AUREOLE_BLESS=1` 时改为写进 `path`
    /// （确认行为的变化是预期的之后重新生成）；跳过的回放什么都不做
    pub fn assert_matches(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if let Some(reason) = &self.skipped {
            eprintln!("跳过回放 {}：{reason}", path.display());
            return;
        }
        if std::env::var_os("AUREOLE_BLESS").is_some_and(|v| v == "1") {
            std::fs::write(path, &self.text).unwrap_or_else(|e| panic!("写不了快照 {}：{e}", path.display()));
            return;
        }
        let expected = std::fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("没有快照 {}：确认回放的行为是对的之后，用 AUREOLE_BLESS=1 生成", path.display()))
            .replace("\r\n", "\n");
        if expected == self.text {
            return;
        }
        let name = path.file_name().map_or_else(|| "snapshot".into(), |n| n.to_string_lossy().into_owned());
        let actual = std::env::temp_dir().join(format!("{name}.actual"));
        let _ = std::fs::write(&actual, &self.text);
        panic!(
            "回放快照和 {} 不一样：\n{}\n完整的新快照在 {}；确认是预期的变化之后用 AUREOLE_BLESS=1 重新生成",
            path.display(),
            diff(&expected, &self.text),
            actual.display()
        );
    }
}

/// 前后相同的行去掉，列出中间不一样的那一段（各最多 30 行）
fn diff(expected: &str, actual: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..].iter().rev().zip(b[head..].iter().rev()).take_while(|(x, y)| x == y).count();
    let mut out = format!("从第 {} 行起：\n", head + 1);
    for line in a[head..a.len() - tail].iter().take(30) {
        out.push_str(&format!("- {line}\n"));
    }
    for line in b[head..b.len() - tail].iter().take(30) {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

/// 回放时边走边记
pub(super) struct Builder<'a> {
    watch: &'a Watch,
    lines: Vec<String>,
    /// 每一项上次记下的值
    last: Vec<(String, String)>,
}

impl<'a> Builder<'a> {
    pub fn new(watch: &'a Watch) -> Self {
        Self { watch, lines: vec![HEADER.to_owned()], last: Vec::new() }
    }

    /// 一次输入、一帧之后：先记这期间的调用，再记变了的状态
    pub fn sample(&mut self, h: &Harness) {
        let time = aureole_core::host::clock::now();
        if let Some(log) = &self.watch.log {
            for call in log.take() {
                self.lines.push(format!("{time:.4} call {call}"));
            }
        }
        for (key, value) in self.state(h) {
            match self.last.iter_mut().find(|(k, _)| *k == key) {
                Some((_, last)) if *last == value => continue,
                Some((_, last)) => *last = value.clone(),
                None => self.last.push((key.clone(), value.clone())),
            }
            self.lines.push(format!("{time:.4} {key} {value}"));
        }
    }

    fn state(&self, h: &Harness) -> Vec<(String, String)> {
        let t = &h.probe;
        let list = |names: Vec<String>| if names.is_empty() { "-".to_owned() } else { names.join(" > ") };
        let mut state = vec![
            ("focus".to_owned(), t.focus_label().unwrap_or_else(|| "-".to_owned())),
            ("ring".to_owned(), t.is_focus_ring_visible().to_string()),
            ("hover".to_owned(), list(t.hovered_names())),
            ("pressed".to_owned(), t.pressed_name().unwrap_or_else(|| "-".to_owned())),
            ("cursor".to_owned(), format!("{:?}", t.cursor())),
            ("overlays".to_owned(), list(t.overlay_names())),
        ];
        for name in &self.watch.texts {
            state.push((format!("text {name}"), t.text_state(name).unwrap_or_else(|| "(none)".to_owned())));
        }
        for name in &self.watch.scrolls {
            // 取到 0.1 像素：滚动的物理在不同的机器上最后几位可能不同
            let offset = t.scroll_offset(name).map_or_else(|| "(none)".to_owned(), |[x, y]| format!("{x:.1} {y:.1}"));
            state.push((format!("scroll {name}"), offset));
        }
        for (name, read) in &self.watch.signals {
            state.push((format!("signal {name}"), read()));
        }
        state
    }

    pub fn finish(mut self) -> Snapshot {
        self.lines.push(String::new());
        Snapshot { text: self.lines.join("\n"), skipped: None }
    }
}
