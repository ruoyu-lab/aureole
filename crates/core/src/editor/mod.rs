//! 输入框：单行的 [`crate::text_field`] 和多行的 [`crate::text_area`]。
//!
//! 文字按段排版（见 [`document`]）；插入点移动、选区、编辑、输入法预编辑、撤销重做、剪贴板在这里，
//! 和信号双向同步、插入点闪烁、绘制也在这里。多行输入框是一个自己带滚动的元素（`tree/scrolling.rs` 的 `mount_own_viewport`）：
//! 滚动的手感、橡皮筋、滚动条和滚动视口是同一套，这里只画看得见的段（画在自己的内容层里，渲染线程按滚动偏移平移），
//! 并告诉元素树把插入点滚进来。

mod document;
mod flow;
mod history;
pub(crate) mod motion;

use std::borrow::Cow;
use std::ops::Range;

use parley::BoundingBox;
use reactive::Signal;
use scene::{Color, Quad};

use crate::geometry::Affine;
use crate::event::KeyEvent;
use crate::input::{Key, Modifiers, NamedKey};
use crate::geometry::Rect;
use crate::system::clipboard;
use crate::text::{Ink, TextAlign, TextBlock, TextStyle, TextSystem};
use document::{Caret, Document, Reach, Selection, Unit};
use history::{Edit, History, Kind};
use motion::Motion;

/// 插入点亮、灭各持续的时间，秒
const BLINK: f64 = 0.53;

/// 从 `epoch` 起每 `period` 秒换一次亮灭：`now` 之后下一次换的时刻
pub(crate) fn next_toggle(epoch: f64, now: f64, period: f64) -> f64 {
    epoch + (((now - epoch).max(0.0) / period).floor() + 1.0) * period
}
const CARET_WIDTH: f32 = 1.5;
/// 多行输入框在视口上下各多画这么多屏，小幅滚动不用重画
const OVERSCAN: f32 = 1.0;

/// 多行输入框里哪个键提交（调用 `on_submit`），见 [`crate::Element::submit_key`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitKey {
    /// 回车提交，Shift+回车换行（聊天输入框）
    Enter,
    /// Command+回车提交（Windows、Linux 上是 Ctrl+回车），回车换行
    PrimaryEnter,
}

/// 密码里每个字显示成的圆点
const MASK: char = '•';

/// 一个个圆点
fn masked(s: &str) -> String {
    s.chars().map(|_| MASK).collect()
}

/// 多行输入框在滚动视口里看得见的部分：从文档的 `top` 起 `height` 高
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Viewport {
    pub top: f32,
    pub height: f32,
}

pub(crate) struct EditorBlock {
    doc: Document,
    history: History,
    multiline: bool,
    submit: Option<SubmitKey>,
    accept_tab: bool,
    pub value: Signal<String>,
    placeholder: TextBlock,
    /// 文字排版原点（布局坐标，未滚动），绘制时更新，用于把指针位置换算到文字里
    origin: [f32; 2],
    /// 元素左上角（布局坐标），绘制时更新：插入点区域换算成元素坐标
    element: [f32; 2],
    /// 单行：横向滚动距离
    scroll: f32,
    /// 单行：手指拖着看别处：插入点动之前不把它滚回来
    free_scroll: bool,
    /// 多行：自己的滚动偏移（纵向，元素树滚动时同步）：指针位置、候选框的位置按它换算
    scroll_y: f32,
    /// 多行：交给它的滚动句柄（[`crate::Element::scroll_handle`]），挂载时交给元素树
    pub scroll_handle: Option<crate::scroll::ScrollHandle>,
    /// 最近一次编辑或移动插入点的时间：插入点从这里开始闪烁，打字时常亮
    blink_epoch: f64,
    caret_on: bool,
    focused: bool,
    /// 输入法的预编辑没给插入点位置：先不画插入点
    caret_hidden: bool,
    selection_color: Color,
    /// 没有焦点时选区的颜色（默认的淡灰）
    inactive_color: Color,
    /// 占位文字的颜色：文字是默认的正文色时用默认的占位色；`None` 时是文字颜色的 40%
    placeholder_color: Option<Color>,
    /// 文档高度变了：布局要重新量
    resized: bool,
    /// 插入点或选区动过：多行输入框要把插入点滚进视口
    reveal: bool,
    /// 多行：上次画了文档的哪一段（纵向），视口滚出这一段才需要重画；`None` 表示全画了
    painted: Option<[f32; 2]>,
    /// 视口的高度：翻页的距离
    page: f32,
    /// 密码：真正的内容（文档里放的是一样多的圆点）；不是密码时是 `None`
    secret: Option<String>,
    accept: Option<fn(char) -> bool>,
    /// 有 [`crate::Element::on_commit`]：获得焦点时记下内容，改完时和它比
    tracks_commit: bool,
    /// 上次改完时的内容（获得焦点时、提交之后记下）：失去焦点、回车时和它不一样才算改完了
    baseline: Option<String>,
    /// 正在打字（打字、删字、输入法）：很长的一段只排到看得见的地方，打字的耗时和段长无关；
    /// 插入点第一次换亮灭（停下来半秒）或者做别的事（移动插入点、点击、撤销）时排完
    typing: bool,
    /// 上次画到文档的哪里：打字时排到这里
    view_bottom: f32,
}

/// 输入框排出来的一行（无障碍的文字片段，见 [`EditorBlock::accessible_lines`]）
pub(crate) struct AccessibleLine<'a> {
    /// 这一行的文字（不含预编辑文字、不含换行符）
    pub text: Cow<'a, str>,
    /// 在内容（不含预编辑文字）里的起点，字节偏移
    pub start: usize,
    /// 后面有换行符（一段的最后一行，而且不是最后一段）
    pub line_break: bool,
    /// 在元素里的位置和大小：x、y、宽、高
    pub rect: [f32; 4],
    /// 在文档里的字节范围（含预编辑文字）
    pub range: Range<usize>,
}

/// 一次键盘或输入法事件的结果
#[derive(Default)]
pub(crate) struct EditOutcome {
    pub handled: bool,
    /// 文字变了（预编辑过程中的变化不算）
    pub changed: bool,
    pub submit: bool,
    pub blur: bool,
}

/// 输入框交给元素树的：内容改了没有、提交、改完了（元素树在释放借用之后写回信号、调用处理器）
#[derive(Default)]
pub(crate) struct EditReport {
    /// 内容变了（预编辑过程中的变化不算）
    pub changed: bool,
    /// 提交（`on_submit`）
    pub submit: bool,
    /// 变了或者提交时的内容
    pub text: Option<String>,
    /// 要写回的信号
    pub value: Option<Signal<String>>,
    /// 改完了（`on_commit`）时的内容
    pub commit: Option<String>,
    /// 要交给应用插入的文字（字符网格的输入法提交，`on_text_insert`）
    pub inserted: Option<String>,
}

pub(crate) enum PointerAction {
    /// 按下：第几次连击（1 放插入点，2 选词，3 选整段），是否按着 Shift（扩展选区）
    Press { clicks: u32, extend: bool },
    Drag,
}

impl EditorBlock {
    /// 绑定到 `value` 的输入框（单行或多行），内容先取信号此刻的值；挂载时绑定的值再同步一次
    pub fn new(value: Signal<String>, multiline: bool) -> Self {
        let (placeholder, submit, accept_tab, accept) = (String::new(), None, false, None);
        let style = TextStyle::default();
        let initial = value.get_untracked();
        let text = clean(&initial, multiline);
        Self {
            doc: Document::new(&text, style),
            history: History::default(),
            multiline,
            submit,
            accept_tab,
            value,
            placeholder: TextBlock::with_content(placeholder),
            origin: [0.0, 0.0],
            element: [0.0, 0.0],
            scroll: 0.0,
            free_scroll: false,
            scroll_y: 0.0,
            scroll_handle: None,
            blink_epoch: 0.0,
            caret_on: true,
            focused: false,
            caret_hidden: false,
            // 继承文字样式时换成默认的选区颜色
            selection_color: Color::TRANSPARENT,
            inactive_color: Color::TRANSPARENT,
            placeholder_color: None,
            resized: false,
            reveal: false,
            painted: None,
            page: 0.0,
            secret: None,
            accept,
            tracks_commit: false,
            baseline: None,
            typing: false,
            view_bottom: f32::INFINITY,
        }
    }

    /// 排版：打字时长段只排到看得见的地方，别的时候都排完
    fn sync(&mut self, text: &mut TextSystem) {
        if self.typing {
            self.doc.refresh_near(text, self.view_bottom);
        } else {
            self.doc.refresh(text);
        }
    }

    /// 不是在打字了：下次排版时把长段排完。还有没排完的返回 true（要重新量高度）
    pub fn stop_typing(&mut self) -> bool {
        self.typing = false;
        !self.doc.is_settled()
    }

    /// 现在就把长段排完（辅助技术要全部的行）。高度变了返回 true（要重新布局）
    pub fn settle(&mut self, text: &mut TextSystem) -> bool {
        if !self.stop_typing() {
            return false;
        }
        let height = self.doc.height();
        self.doc.refresh(text);
        self.doc.height() != height
    }

    /// 只收 `accept` 认的字（打字、粘贴、输入法提交）；别的字丢掉
    pub fn set_accept(&mut self, accept: fn(char) -> bool) {
        self.accept = Some(accept);
    }

    /// 密码模式开关：打开时文档换成圆点（插入点停在同一个字的位置），关上时换回真的内容
    pub fn set_password(&mut self, on: bool) -> bool {
        if on == self.secret.is_some() || self.multiline {
            return false;
        }
        let chars = self.doc.text()[..self.doc.selection.focus.offset].chars().count();
        if on {
            let real = self.doc.text();
            self.doc.set_text(&masked(&real));
            self.secret = Some(real);
            self.history.clear();
            self.doc.selection = Selection::caret(Caret::at(chars * MASK.len_utf8()));
        } else {
            let real = self.secret.take().unwrap_or_default();
            let offset = real.char_indices().nth(chars).map_or(real.len(), |(i, _)| i);
            self.doc.set_text(&real);
            self.doc.selection = Selection::caret(Caret::at(offset));
        }
        self.reveal = true;
        true
    }

    /// 是密码
    pub fn password(&self) -> bool {
        self.secret.is_some()
    }

    /// 密码：文档里的字节位置（都是圆点）换成真的内容里的字节位置
    fn real_offset(secret: &str, offset: usize) -> usize {
        let chars = offset / MASK.len_utf8();
        secret.char_indices().nth(chars).map_or(secret.len(), |(i, _)| i)
    }

    /// 有没有 `on_commit`（挂载时由元素树设置）
    pub fn track_commit(&mut self, on: bool) {
        self.tracks_commit = on;
    }

    /// 改完了：内容和上次改完时不一样就返回内容，并把它记成新的起点
    pub fn take_commit(&mut self) -> Option<String> {
        if !self.tracks_commit {
            return None;
        }
        let text = self.text();
        if self.baseline.as_deref() == Some(text.as_str()) {
            return None;
        }
        self.baseline = Some(text.clone());
        Some(text)
    }

    pub fn multiline(&self) -> bool {
        self.multiline
    }

    /// 字号等排版样式变了才返回 true
    pub fn update_style(&mut self, f: impl FnOnce(&mut String, &mut TextStyle)) -> bool {
        let mut ignored = String::new();
        let mut style = *self.doc.style();
        f(&mut ignored, &mut style);
        if style == *self.doc.style() {
            return false;
        }
        self.doc.set_style(style);
        self.placeholder.update(|_, s| *s = style);
        true
    }

    /// 选区颜色（有焦点、没焦点时）、占位文字的颜色变了返回 true（只重画）
    pub fn set_colors(&mut self, selection: Color, inactive: Color, placeholder: Option<Color>) -> bool {
        let before = (self.selection_color, self.inactive_color, self.placeholder_color);
        (self.selection_color, self.inactive_color, self.placeholder_color) = (selection, inactive, placeholder);
        before != (selection, inactive, placeholder)
    }

    /// 加载了新字体：重新排版
    pub fn invalidate(&mut self) {
        self.doc.invalidate();
        self.placeholder.invalidate();
    }

    pub fn line_height(&self) -> f32 {
        let style = self.doc.style();
        (style.size * style.line_height).ceil()
    }

    /// 多行：按内容区宽度断行后的大小。宽度不确定（`None`，量最小、最大内容时）时宽度报 0：
    /// 输入框的宽度和单行的一样由样式决定，不随内容变（不然打字时一行变长，旁边的元素会被挤动）；
    /// 高度按上次的断行宽度算
    pub fn measure(&mut self, text: &mut TextSystem, width: Option<f32>) -> [f32; 2] {
        if let Some(width) = width {
            self.doc.set_wrap(Some(width.max(1.0)));
        }
        self.sync(text);
        self.resized = false;
        [width.unwrap_or(0.0), self.doc.height().max(self.line_height())]
    }

    /// 内容（不含输入法的预编辑文字）；密码是真的内容
    pub fn text(&self) -> String {
        match &self.secret {
            Some(secret) => secret.clone(),
            None => self.doc.text(),
        }
    }

    /// 无障碍：内容和选区（全文的字节偏移：锚点、焦点）。输入法组字期间预编辑文字不算，
    /// 插入点报在组字开始的地方，辅助技术不会念出还没确定的字
    pub fn accessible(&self) -> (String, usize, usize) {
        let text = self.doc.text();
        match &self.doc.compose {
            Some(compose) => (text, compose.start, compose.start),
            None => {
                let selection = &self.doc.selection;
                (text, selection.anchor.offset, selection.focus.offset)
            }
        }
    }

    /// 无障碍按行的文字片段（要先排过版）：每一行的文字（不含预编辑文字）、在内容里的起点、后面有没有换行符、
    /// 在元素里的矩形（算上内边距和单行的横向滚动；多行的纵向滚动由元素树在外面套一层变换）
    pub fn accessible_lines(&self) -> Vec<AccessibleLine<'_>> {
        let lines = self.doc.lines();
        let last = lines.len().saturating_sub(1);
        let compose = self.doc.compose.clone();
        // 预编辑文字不算内容：之后的偏移往前挪
        let content = |offset: usize| match &compose {
            Some(c) if offset >= c.end => offset - c.len(),
            Some(c) if offset > c.start => c.start,
            _ => offset,
        };
        let inset = [self.origin[0] - self.element[0] - self.scroll, self.origin[1] - self.element[1]];
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let raw = self.doc.line_text(line.range.clone());
                let text = match &compose {
                    Some(c) if c.start < line.range.end && c.end > line.range.start => {
                        let a = c.start.max(line.range.start) - line.range.start;
                        let b = c.end.min(line.range.end) - line.range.start;
                        Cow::Owned(format!("{}{}", &raw[..a], &raw[b..]))
                    }
                    _ => Cow::Borrowed(raw),
                };
                let r = line.rect;
                let rect = [r.x0 as f32 + inset[0], r.y0 as f32 + inset[1], (r.x1 - r.x0) as f32, (r.y1 - r.y0) as f32];
                AccessibleLine { text, start: content(line.range.start), line_break: line.paragraph_end && i < last, rect, range: line.range }
            })
            .collect()
    }

    /// 一行里每个字素簇的起点和宽度（相对这一行的左边）：放大镜跟着插入点。组字时不给（预编辑文字在排版里占着位置）
    pub fn cluster_positions(&self, line: &AccessibleLine) -> Option<(Vec<f32>, Vec<f32>)> {
        if self.doc.compose.is_some() {
            return None;
        }
        let starts: Vec<usize> = crate::tree::clusters(&line.text).map(|(start, _)| line.range.start + start).collect();
        // 从左到右的行走一遍这一行；有从右到左的字时逐个找插入点的位置（每个都要从行首数起）
        let (mut xs, end) = self.doc.cluster_xs(line.range.clone(), &starts).unwrap_or_else(|| {
            (starts.iter().map(|&at| self.doc.x_at(at, false)).collect(), self.doc.x_at(line.range.end, true))
        });
        let x0 = xs.first().copied().unwrap_or(end);
        xs.iter_mut().for_each(|x| *x -= x0);
        let end = end - x0;
        let widths = (0..xs.len()).map(|i| (xs.get(i + 1).copied().unwrap_or(end) - xs[i]).abs()).collect::<Vec<f32>>();
        let mut widths = widths;
        if line.line_break {
            xs.push(end);
            widths.push(0.0);
        }
        Some((xs, widths))
    }

    /// 回放快照：内容（不含预编辑文字）、选区（全文的字节偏移：锚点、焦点；组字时按预编辑文字在文档里的位置）、预编辑文字
    pub fn snapshot(&self) -> (String, usize, usize, Option<String>) {
        let preedit = self.doc.compose.clone().map(|range| self.doc.slice(range));
        let selection = &self.doc.selection;
        (self.text(), selection.anchor.offset, selection.focus.offset, preedit)
    }

    /// 输入框为空时显示的提示文字
    pub fn placeholder_text(&self) -> &str {
        self.placeholder.content()
    }

    /// 换提示文字，变了返回 true（要重画；空着的输入框按提示文字量大小）
    pub fn set_placeholder(&mut self, text: String) -> bool {
        self.placeholder.update(|content, _| *content = text)
    }

    /// 多行输入框里提交的键
    pub fn submit_key(&self) -> Option<SubmitKey> {
        self.submit
    }

    /// 改提交的键（只改行为，不用重画）
    pub fn set_submit_key(&mut self, key: Option<SubmitKey>) -> bool {
        self.submit = key;
        false
    }

    /// 按 Tab 插入制表符
    pub fn accepts_tab(&self) -> bool {
        self.accept_tab
    }

    /// 改 Tab 是不是插入制表符（只改行为）
    pub fn set_accepts_tab(&mut self, accept: bool) -> bool {
        self.accept_tab = accept;
        false
    }

    /// 辅助技术设置选区（全文的字节偏移）；正在组字时不动
    pub fn select(&mut self, anchor: usize, focus: usize) {
        if self.doc.compose.is_some() {
            return;
        }
        let len = self.doc.len();
        let anchor = Caret::at(anchor.min(len));
        let focus = Caret::at(focus.min(len));
        self.move_to(anchor, false);
        self.move_to(focus, true);
    }

    /// 选中的范围（全文的字节偏移）
    pub fn selection_range(&self) -> Range<usize> {
        self.doc.selection.range()
    }

    /// 测试和基准用：累计整形过多少段
    pub fn shaped(&self) -> usize {
        self.doc.shaped
    }

    /// 绑定的值从外部变了：替换内容，插入点放到末尾，撤销记录清空；正在输入法预编辑时不打断
    pub fn set_value(&mut self, value: &str) -> bool {
        // 最常见的是自己写回信号的值又流回来：内容一样，先比（逐段比字节），不用先找回车符
        if self.secret.is_none() && self.doc.equals(value) {
            return false;
        }
        let value = clean(value, self.multiline);
        if let Some(secret) = &mut self.secret {
            if *secret == value {
                return false;
            }
            *secret = value.into_owned();
            self.doc.set_text(&masked(secret));
            self.resized = true;
            self.reveal = true;
            return true;
        }
        if self.doc.compose.is_some() || self.doc.equals(&value) {
            return false;
        }
        self.doc.set_text(&value);
        self.history.clear();
        self.resized = true;
        self.reveal = true;
        true
    }

    /// 失去焦点时保留选区（再次获得焦点时还在），画成不活跃的颜色
    pub fn set_focused(&mut self, focused: bool, now: f64) {
        self.focused = focused;
        self.typing = false;
        if focused && self.tracks_commit {
            self.baseline = Some(self.text());
        }
        self.history.close();
        self.wake(now);
    }

    /// 插入点重新常亮，过一会儿再开始闪烁
    fn wake(&mut self, now: f64) {
        self.blink_epoch = now;
        self.caret_on = true;
    }

    /// 插入点下一次换亮灭的时刻（统一时钟的秒，`now` 之后）：没有焦点、插入点藏着（输入法组字时）时不闪，是 `None`。
    /// 窗口睡到这一刻再出一帧（[`crate::kinds::NextFrame::At`]），不用每帧推进
    pub fn next_blink(&self, now: f64) -> Option<f64> {
        (self.focused && !self.caret_hidden).then(|| next_toggle(self.blink_epoch, now, BLINK))
    }

    /// 推进插入点闪烁，返回可见性是否变了
    pub fn blink(&mut self, now: f64) -> bool {
        let on = self.focused && (((now - self.blink_epoch).max(0.0) / BLINK) as u64).is_multiple_of(2);
        let changed = on != self.caret_on;
        self.caret_on = on;
        changed
    }

    /// 文档高度变了（布局要重新量）
    pub fn take_resized(&mut self) -> bool {
        std::mem::take(&mut self.resized)
    }

    /// 插入点动过：返回它的纵向范围（相对内容区顶部），让视口把它滚进来
    pub fn take_reveal(&mut self) -> Option<[f32; 2]> {
        if !std::mem::take(&mut self.reveal) {
            return None;
        }
        self.free_scroll = false;
        if !self.multiline {
            return None;
        }
        let rect = self.doc.caret_rect(self.doc.selection.focus, CARET_WIDTH);
        Some([rect.y0 as f32, rect.y1 as f32])
    }

    /// 视口滚到 `viewport` 时，上次画的范围是否已经不够
    pub fn needs_repaint(&self, viewport: Viewport) -> bool {
        self.painted.is_some_and(|[a, b]| viewport.top < a || viewport.top + viewport.height > b)
    }

    /// 多行：元素树滚动了（偏移按物理像素取整）
    pub fn set_scroll_y(&mut self, y: f32) {
        self.scroll_y = y;
    }

    /// 多行：排出来的内容有多高（滚动的范围）
    pub fn content_height(&self) -> f32 {
        self.doc.height().max(self.line_height())
    }

    /// 单行：内容区宽 `width` 时能横着滚多远
    fn max_scroll(&self, width: f32) -> f32 {
        (self.doc.width() - width + CARET_WIDTH).max(0.0)
    }

    /// 单行：手指往 `direction` 的符号那边拖时内容还能滚
    pub fn can_scroll_x(&self, direction: f32, width: f32) -> bool {
        !self.multiline && if direction > 0.0 { self.scroll < self.max_scroll(width) } else { direction < 0.0 && self.scroll > 0.0 }
    }

    /// 单行：手指拖着内容走了 `delta`（滚动量）。插入点再动时才滚回来
    pub fn scroll_x_by(&mut self, delta: f32, width: f32) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.max_scroll(width));
        self.free_scroll = true;
    }

    /// `point` 是布局坐标（已去掉元素变换）
    pub fn pointer(&mut self, text: &mut TextSystem, point: [f32; 2], action: PointerAction, now: f64) {
        self.typing = false;
        self.doc.refresh(text);
        let p = [point[0] - self.origin[0] + self.scroll, point[1] - self.origin[1] + self.scroll_y];
        match action {
            PointerAction::Press { clicks: 1, extend } => {
                let caret = self.doc.hit(p);
                if extend {
                    let sel = &mut self.doc.selection;
                    sel.focus = caret;
                    sel.h_pos = None;
                    sel.unit = None;
                } else {
                    self.doc.selection = Selection::caret(caret);
                }
            }
            PointerAction::Press { clicks, .. } => {
                let (unit, range) = match clicks {
                    2 => (Unit::Word, self.doc.word_at(p)),
                    _ => (Unit::Paragraph, self.doc.paragraph_range_at(p)),
                };
                self.doc.selection = Selection { unit: Some((unit, range.clone())), ..Selection::span(range.start, range.end) };
            }
            PointerAction::Drag => {
                let caret = self.doc.hit(p);
                match self.doc.selection.unit.clone() {
                    None => self.doc.selection.focus = caret,
                    // 双击、三击后拖动：最初选中的那一段一直选着，另一头按词、按段对齐
                    Some((unit, initial)) => {
                        let target = match unit {
                            Unit::Word => self.doc.word_at(p),
                            Unit::Paragraph => self.doc.paragraph_range_at(p),
                        };
                        let (anchor, focus) = if target.start < initial.start {
                            (initial.end, target.start)
                        } else {
                            (initial.start, target.end.max(initial.end))
                        };
                        self.doc.selection = Selection { unit: Some((unit, initial)), ..Selection::span(anchor, focus) };
                    }
                }
            }
        }
        self.history.close();
        self.reveal = true;
        self.wake(now);
    }

    /// 键盘：编辑键、移动键（Shift 扩展选区，按词的修饰键按词，主修饰键到行首行尾、文档首尾）、
    /// 主修饰键加 A/C/X/V/Z（重做是 Shift+Z，Windows 上 Y 也是），以及可打印字符（按着 Command、Ctrl 时不打字，留给快捷键；
    /// Windows 上 AltGr 报成 Ctrl+Alt，照样打字）。修饰键按平台惯例
    /// （macOS：按词是 Option，主修饰键加方向键到行首行尾；其他平台：按词是 Ctrl，行首行尾用 Home、End，Ctrl+Home、End 到文档首尾）。
    /// Tab（除非 `accepts_tab`）和 Esc 不算处理过，留给外层
    pub fn key(&mut self, text: &mut TextSystem, event: &KeyEvent, now: f64) -> EditOutcome {
        let conventions = crate::system::conventions();
        let (key, mods) = (&event.key, event.modifiers);
        // 主修饰键（AltGr 不算）；打不打字按按键管线的规则（`KeyEvent::typed_text`：组合键不打字，留给外层的处理器、快捷键）
        let (shift, alt, cmd) = (mods.has_shift(), mods.has_alt(), event.has_primary_modifier());
        // 打字、删字只动插入点附近；别的键（移动、撤销、粘贴）可能走到很远的地方，先把长段排完
        self.typing = matches!(key, Key::Named(NamedKey::Backspace | NamedKey::Delete | NamedKey::Enter | NamedKey::Tab)) || event.typed_text().is_some();
        self.sync(text);
        // 按词（macOS 是 Option，其他平台是 Ctrl）；主修饰键加退格删到行首（只有 macOS）
        let word = mods.intersects(conventions.word_modifier);
        let edge = cmd && conventions.uses_arrows_for_line_edges;
        let mut outcome = EditOutcome { handled: true, ..Default::default() };
        let range = self.doc.selection.range();
        if let Some(motion) = Motion::of_key(event) {
            self.apply_motion(motion, shift);
            self.wake(now);
            return outcome;
        }
        // Windows、Linux 的老习惯：Ctrl+Insert 复制、Shift+Insert 粘贴、Shift+Delete 剪切
        if conventions.accepts_insert_for_clipboard {
            let (insert, delete) = (key == &Key::Named(NamedKey::Insert), key == &Key::Named(NamedKey::Delete));
            if insert && mods == Modifiers::CONTROL {
                self.copy();
                self.wake(now);
                return outcome;
            }
            if (insert || delete) && mods == Modifiers::SHIFT {
                if insert {
                    match self.paste(text, now) {
                        Some(changed) => outcome.changed = changed,
                        None => outcome.handled = false,
                    }
                } else {
                    outcome.changed = self.cut(text, range, now);
                }
                self.wake(now);
                return outcome;
            }
        }
        match key {
            Key::Named(NamedKey::Backspace) => {
                let reach = if edge { Reach::Line } else if word { Reach::Word } else { Reach::Char };
                outcome.changed = self.delete(text, false, reach, now);
            }
            Key::Named(NamedKey::Delete) => {
                let reach = if word { Reach::Word } else { Reach::Char };
                outcome.changed = self.delete(text, true, reach, now);
            }
            Key::Named(NamedKey::PageUp | NamedKey::PageDown) => {
                let down = matches!(key, Key::Named(NamedKey::PageDown));
                if self.multiline {
                    // 翻一屏：视口能放下几行就走几行，至少一行
                    let lines = ((self.page / self.doc.line_height()).floor() as isize - 1).max(1);
                    self.vertical(if down { lines } else { -lines }, shift);
                } else {
                    self.move_to(if down { self.doc.end() } else { Caret::at(0) }, shift);
                }
            }
            Key::Named(NamedKey::Enter) => match (self.multiline, self.submit) {
                (false, _) => outcome.submit = true,
                (true, Some(SubmitKey::Enter)) if !shift && !alt && !cmd => outcome.submit = true,
                (true, Some(SubmitKey::PrimaryEnter)) if cmd => outcome.submit = true,
                // 没有设成提交的 Command+回车留给外层的快捷键
                (true, _) if cmd => outcome.handled = false,
                (true, _) => outcome.changed = self.insert(text, "\n", Kind::Typing, now),
            },
            Key::Named(NamedKey::Tab) => {
                if self.accept_tab && !shift && !cmd && !alt {
                    outcome.changed = self.insert(text, "\t", Kind::Typing, now);
                } else {
                    outcome.handled = false;
                }
            }
            // Esc：失去焦点，但不算处理过，外层（比如对话框）照样收到
            Key::Named(NamedKey::Escape) => {
                outcome.blur = true;
                outcome.handled = false;
            }
            Key::Character(c) if cmd => match c.to_lowercase().as_str() {
                "a" => self.select_all(),
                "c" => {
                    self.copy();
                }
                "x" => outcome.changed = self.cut(text, range, now),
                // 剪贴板里没有文字（比如只有图片）：不算处理了，按键接着冒泡，外面的 `on_key_down` 可以自己读图片
                "v" => match self.paste(text, now) {
                    Some(changed) => outcome.changed = changed,
                    None => outcome.handled = false,
                },
                "z" if shift => outcome.changed = self.redo(text),
                "y" if conventions.accepts_y_for_redo => outcome.changed = self.redo(text),
                "z" => outcome.changed = self.undo(text),
                _ => outcome.handled = false,
            },
            _ => match event.typed_text() {
                Some(typed) => outcome.changed = self.insert(text, typed, Kind::Typing, now),
                None => outcome.handled = false,
            },
        }
        self.wake(now);
        outcome
    }

    // ---- 输入法、听写、辅助技术（文字输入客户端，见 `tree/kinds/editor.rs`） ----

    /// 预编辑：文字先放进文档并加下划线（不写回信号）；空文字清掉预编辑。密码不用输入法，预编辑的字不放进来
    pub fn preedit(&mut self, text: &mut TextSystem, preedit: &str, cursor: Option<Range<usize>>, now: f64) -> EditOutcome {
        self.typing = true;
        self.sync(text);
        let changed = if self.secret.is_some() {
            false
        } else {
            // 预编辑里的换行去掉；插入点的字节位置按去掉之后的文字算（落在字中间时往前挪到字的开头）
            let cleaned: String = preedit.chars().filter(|c| !matches!(c, '\n' | '\r')).collect();
            if cleaned.is_empty() {
                self.clear_compose(text);
                false
            } else {
                let cursor = cursor.map(|r| (floor_char(&cleaned, r.start), floor_char(&cleaned, r.end)));
                self.set_compose(text, &cleaned, cursor, now)
            }
        };
        self.wake(now);
        EditOutcome { handled: true, changed, ..Default::default() }
    }

    /// 提交：先清掉预编辑（不依赖平台先送一个空的预编辑），再在组字开始的地方插入，算一步撤销
    pub fn commit_text(&mut self, text: &mut TextSystem, committed: &str, now: f64) -> EditOutcome {
        self.typing = true;
        self.sync(text);
        self.clear_compose(text);
        let committed = clean(committed, self.multiline);
        let changed = self.insert(text, &committed, Kind::Ime, now);
        self.wake(now);
        EditOutcome { handled: true, changed, ..Default::default() }
    }

    /// 放弃组字：预编辑文字去掉，插入点回到开始组字的地方。没在组字时什么都不做
    pub fn cancel_compose(&mut self, text: &mut TextSystem, now: f64) -> EditOutcome {
        let composing = self.doc.compose.is_some();
        if composing {
            self.typing = false;
            self.doc.refresh(text);
            self.clear_compose(text);
            self.wake(now);
        }
        EditOutcome { handled: composing, ..Default::default() }
    }

    /// 把全文里的 `range`（字节范围，落在字中间时往外扩到整字）换成 `insert`，记一步撤销：
    /// 辅助技术改值、替换选中的文字，听写改字。正在组字时先放弃组字
    pub fn replace(&mut self, text: &mut TextSystem, range: Range<usize>, insert: &str, now: f64) -> EditOutcome {
        self.typing = false;
        self.doc.refresh(text);
        self.clear_compose(text);
        let full = self.doc.text();
        let start = floor_char(&full, range.start.min(full.len()));
        let end = ceil_char(&full, range.end.clamp(start, full.len()));
        let insert = self.accepted(clean(insert, self.multiline));
        // 换成一样的文字：什么都没改（不记撤销、不算改了）
        let same = match &self.secret {
            Some(secret) => *secret == insert && start == 0 && end == full.len(),
            None => full[start..end] == *insert,
        };
        let changed = !same && self.edit(text, start..end, &insert, Kind::Other, now);
        self.wake(now);
        EditOutcome { handled: true, changed, ..Default::default() }
    }

    /// 删掉插入点（选区）前 `before`、后 `after` 个字节（落在字中间时往外扩到整字），记一步撤销。正在组字时不动
    pub fn delete_around(&mut self, text: &mut TextSystem, before: usize, after: usize, now: f64) -> EditOutcome {
        if self.doc.compose.is_some() {
            return EditOutcome::default();
        }
        self.typing = false;
        self.doc.refresh(text);
        let full = self.doc.text();
        let selected = self.doc.selection.range();
        let start = floor_char(&full, selected.start.saturating_sub(before));
        let end = ceil_char(&full, (selected.end + after).min(full.len()));
        let mut changed = false;
        // 选区本身不删：先删后面的，再删前面的（前面的偏移不受影响）
        if end > selected.end {
            changed |= self.edit(text, selected.end..end, "", Kind::Delete, now);
        }
        if start < selected.start {
            changed |= self.edit(text, start..selected.start, "", Kind::Backspace, now);
        }
        self.wake(now);
        EditOutcome { handled: true, changed, ..Default::default() }
    }

    /// 插入点所在的那一段和其中的选区（段里的字节范围）；密码不给
    pub fn surrounding(&self) -> Option<(String, Range<usize>)> {
        if self.secret.is_some() {
            return None;
        }
        let full = self.doc.text();
        let selected = self.doc.selection.range();
        let start = full[..selected.start].rfind('\n').map_or(0, |i| i + 1);
        let end = full[selected.end..].find('\n').map_or(full.len(), |i| selected.end + i);
        Some((full[start..end].to_owned(), selected.start - start..selected.end - start))
    }

    /// 输入法候选框应该避开的区域，元素坐标（见 [`EditorBlock::ime_area`]）
    pub fn caret_area(&self) -> Rect {
        let area = self.ime_area();
        Rect { x: area.x - self.element[0], y: area.y - self.element[1] - self.scroll_y, ..area }
    }

    /// 开始或更新预编辑。开始时如果有选区，先把它删掉（记一步撤销，提交时合进同一步）；返回内容是否变了
    fn set_compose(&mut self, text: &mut TextSystem, preedit: &str, cursor: Option<(usize, usize)>, now: f64) -> bool {
        let mut changed = false;
        let range = match self.doc.compose.take() {
            Some(range) => range,
            None => {
                let selected = self.doc.selection.range();
                if !selected.is_empty() {
                    changed = self.edit(text, selected.clone(), "", Kind::ImeStart, now);
                }
                selected.start..selected.start
            }
        };
        self.doc.replace(range.clone(), preedit);
        let start = range.start;
        self.doc.compose = Some(start..start + preedit.len());
        let (a, b) = cursor.unwrap_or((preedit.len(), preedit.len()));
        self.caret_hidden = cursor.is_none();
        self.doc.selection = Selection::span(start + a.min(preedit.len()), start + b.min(preedit.len()));
        self.after_edit(text);
        changed
    }

    fn clear_compose(&mut self, text: &mut TextSystem) {
        if let Some(range) = self.doc.compose.take() {
            self.doc.replace(range.clone(), "");
            self.doc.selection = Selection::caret(Caret::at(range.start));
            self.caret_hidden = false;
            self.after_edit(text);
        }
    }

    /// 输入法候选框应该避开的区域（布局坐标）：预编辑文字（没有时是插入点）左右各留出约三个字，
    /// 打字时候选框不会跟着一个字一个字地跳。平台的输入法把区域的左上角当成候选框的位置时（Windows，
    /// [`crate::Conventions::pads_ime_area`]）不留：候选框对齐组的字的开头
    pub fn ime_area(&self) -> Rect {
        let caret = self.doc.caret_rect(self.doc.selection.focus, 0.0);
        let mut area = caret;
        if let Some(compose) = &self.doc.compose {
            let mut first = true;
            self.doc.range_rects(compose.clone(), None, |rect| {
                // 只框住插入点所在的那一行
                if rect.y0 <= caret.y1 && rect.y1 >= caret.y0 {
                    area = if first { rect } else { area.union(rect) };
                    first = false;
                }
            });
        }
        let pad = if crate::system::conventions().pads_ime_area { 3.0 * 0.6 * self.doc.style().size } else { 0.0 };
        let inflate = f64::from(pad);
        let x0 = (area.x0 - inflate).max(0.0);
        let x1 = area.x1 + inflate;
        Rect {
            x: self.origin[0] - self.scroll + x0 as f32,
            y: self.origin[1] + area.y0 as f32,
            width: (x1 - x0) as f32,
            height: (area.y1 - area.y0) as f32,
        }
    }

    // ---- 编辑 ----

    /// 把 `range` 换成 `insert`，记一步撤销（同类的连续编辑合并），插入点放到插入的文字之后。返回内容是否变了
    fn edit(&mut self, text: &mut TextSystem, range: Range<usize>, insert: &str, kind: Kind, now: f64) -> bool {
        if range.is_empty() && insert.is_empty() {
            return false;
        }
        let before = (self.doc.selection.anchor, self.doc.selection.focus);
        // 密码：真的内容里换掉同样的几个字，文档里换成圆点；不记撤销（撤销记录里不留密码）
        if let Some(secret) = &mut self.secret {
            let (start, end) = (Self::real_offset(secret, range.start), Self::real_offset(secret, range.end));
            secret.replace_range(start..end, insert);
            let end = self.doc.replace(range, &masked(insert));
            self.doc.selection = Selection::caret(Caret::at(end));
            self.after_edit(text);
            return true;
        }
        let removed = self.doc.slice(range.clone());
        let end = self.doc.replace(range.clone(), insert);
        // 插入的文字之后：贴着刚插入的字（在折行处显示在上一行末尾）
        let caret = if insert.is_empty() || insert.ends_with('\n') { Caret::at(end) } else { Caret::upstream(end) };
        self.doc.selection = Selection::caret(caret);
        self.history.record(Edit { start: range.start, removed, inserted: insert, before, after: (caret, caret) }, kind, now);
        self.after_edit(text);
        true
    }

    /// 替换选区（或在插入点处插入）：只插入认的字（[`crate::Element::accepted_chars`]），一个都不认时什么都不做
    fn insert(&mut self, text: &mut TextSystem, insert: &str, kind: Kind, now: f64) -> bool {
        let range = self.doc.selection.range();
        let kept = self.accepted(Cow::Borrowed(insert));
        if kept.is_empty() && !insert.is_empty() {
            return false;
        }
        self.edit(text, range, &kept, kind, now)
    }

    /// 去掉不认的字（打字、粘贴、输入法提交、辅助技术改值都过这一遍）
    fn accepted<'a>(&self, text: Cow<'a, str>) -> Cow<'a, str> {
        match self.accept {
            Some(accept) if !text.chars().all(accept) => Cow::Owned(text.chars().filter(|&c| accept(c)).collect()),
            _ => text,
        }
    }

    /// 有选区时删掉选区，否则删掉插入点前（后）的一个字、一个词或到行首
    fn delete(&mut self, text: &mut TextSystem, forward: bool, reach: Reach, now: f64) -> bool {
        let selection = &self.doc.selection;
        let range =
            if selection.is_collapsed() { self.doc.delete_range(selection.focus, forward, reach) } else { selection.range() };
        self.edit(text, range, "", if forward { Kind::Delete } else { Kind::Backspace }, now)
    }

    /// 编辑之后：重新排改过的段，高度变了要重新布局，插入点要滚进视口
    fn after_edit(&mut self, text: &mut TextSystem) {
        let height = self.doc.height();
        self.sync(text);
        self.resized |= self.doc.height() != height;
        self.reveal = true;
    }

    fn undo(&mut self, text: &mut TextSystem) -> bool {
        if self.doc.compose.is_some() || self.secret.is_some() {
            return false;
        }
        let Some(change) = self.history.undo() else { return false };
        self.doc.replace(change.start..change.start + change.inserted.len(), &change.removed);
        self.set_selection(change.before);
        self.after_edit(text);
        true
    }

    fn redo(&mut self, text: &mut TextSystem) -> bool {
        if self.doc.compose.is_some() || self.secret.is_some() {
            return false;
        }
        let Some(change) = self.history.redo() else { return false };
        self.doc.replace(change.start..change.start + change.removed.len(), &change.inserted);
        self.set_selection(change.after);
        self.after_edit(text);
        true
    }

    fn set_selection(&mut self, (anchor, focus): (Caret, Caret)) {
        self.doc.selection = Selection { anchor, focus, h_pos: None, unit: None };
    }

    // ---- 移动 ----

    /// 移动键（哪个键往哪走见 [`Motion::of_key`]）。有选区时不带修饰键的左右收到选区的这一头；单行输入框里上下到开头、末尾
    fn apply_motion(&mut self, motion: Motion, extend: bool) {
        let selection = &self.doc.selection;
        let focus = selection.focus;
        let (collapsed, range) = (selection.is_collapsed(), selection.range());
        let to = match motion {
            Motion::Left { word: false } if !collapsed && !extend => Caret::at(range.start),
            Motion::Right { word: false } if !collapsed && !extend => Caret::upstream(range.end),
            Motion::Left { word } => self.doc.left(focus, word),
            Motion::Right { word } => self.doc.right(focus, word),
            Motion::RangeStart => Caret::at(0),
            Motion::RangeEnd => self.doc.end(),
            Motion::Up | Motion::ParagraphUp if !self.multiline => Caret::at(0),
            Motion::Down | Motion::ParagraphDown if !self.multiline => self.doc.end(),
            Motion::Up => return self.vertical(-1, extend),
            Motion::Down => return self.vertical(1, extend),
            Motion::ParagraphUp => {
                let start = self.doc.paragraph_start(focus);
                if start.offset == focus.offset && focus.offset > 0 { self.doc.paragraph_start(Caret::at(focus.offset - 1)) } else { start }
            }
            Motion::ParagraphDown => {
                let end = self.doc.paragraph_end(focus);
                if end.offset == focus.offset && focus.offset < self.doc.len() { self.doc.paragraph_end(Caret::at(focus.offset + 1)) } else { end }
            }
            Motion::LineStart => self.doc.line_start(focus),
            Motion::LineEnd => self.doc.line_end(focus),
        };
        self.move_to(to, extend);
    }

    /// 插入点移到 `to`；`extend` 时保留锚点（Shift 扩展选区）
    fn move_to(&mut self, to: Caret, extend: bool) {
        let selection = &mut self.doc.selection;
        selection.focus = to;
        if !extend {
            selection.anchor = to;
        }
        selection.h_pos = None;
        selection.unit = None;
        self.history.close();
        self.reveal = true;
    }

    /// 上下移动 `lines` 行，连续移动时保持最初的横坐标
    fn vertical(&mut self, lines: isize, extend: bool) {
        let selection = &self.doc.selection;
        let h_pos = selection.h_pos.unwrap_or_else(|| self.doc.x_of(selection.focus));
        let to = self.doc.vertical(selection.focus, lines, h_pos);
        self.move_to(to, extend);
        self.doc.selection.h_pos = Some(h_pos);
    }

    fn select_all(&mut self) {
        self.doc.selection = Selection::span(0, self.doc.len());
        self.history.close();
    }

    /// 选中的文字放进剪贴板；没有选中（或者正在组字）时返回 false。密码不能复制（所以也不能剪切）
    fn copy(&self) -> bool {
        let range = self.doc.selection.range();
        if range.is_empty() || self.doc.compose.is_some() || self.secret.is_some() {
            return false;
        }
        clipboard::write_text(self.doc.slice(range));
        true
    }

    /// 剪切：选中的文字放进剪贴板、删掉。返回内容变了没有（没有选中、密码时什么都不做）
    fn cut(&mut self, text: &mut TextSystem, range: Range<usize>, now: f64) -> bool {
        self.copy() && self.edit(text, range, "", Kind::Other, now)
    }

    /// 粘贴剪贴板里的文字（多余的换行按单行、多行清理），返回内容变了没有；剪贴板里没有文字时是 `None`
    fn paste(&mut self, text: &mut TextSystem, now: f64) -> Option<bool> {
        let pasted = clipboard::read_text()?;
        let pasted = clean(&pasted, self.multiline);
        Some(self.insert(text, &pasted, Kind::Other, now))
    }

    // ---- 绘制 ----

    /// 生成选区、文字（或占位符）、预编辑下划线、插入点的图元。
    /// 单行：横向滚动到插入点可见，横向裁剪到内容区，纵向裁剪到元素边框（行高可能比内容区高，不能切掉下伸部分和下划线）。
    /// 多行：按内容区宽度断行，只画 `viewport` 附近的段（裁剪由外面的滚动视口做）。
    /// `place` 是图元的层号和裁剪（多行的画在自己的内容层里）；`[0, 0]` 时由绘制遍历设
    pub fn paint(
        &mut self,
        text: &mut TextSystem,
        bounds: Rect,
        content: Rect,
        world: &Affine,
        ink: Ink,
        viewport: Option<Viewport>,
        place: [u32; 2],
        quads: &mut Vec<Quad>,
    ) {
        let Ink { color, opacity, .. } = ink;
        self.element = [bounds.x, bounds.y];
        if self.multiline {
            self.doc.set_wrap(Some(content.width.max(1.0)));
        }
        // 打字时长段只画看得见的这一屏（波及到后面的行都要重新生成）；停下来再多画上下各一屏
        let overscan = if self.typing && self.doc.has_flows() { 0.0 } else { OVERSCAN };
        let view = viewport.map(|v| {
            self.page = v.height;
            let margin = v.height * overscan;
            [v.top - margin, v.top + v.height + margin]
        });
        self.view_bottom = view.map_or(f32::INFINITY, |v| v[1]);
        let height = self.doc.height();
        self.sync(text);
        self.resized |= self.doc.height() != height;
        self.painted = view;
        if self.multiline {
            self.origin = [content.x, content.y];
            self.scroll = 0.0;
            if viewport.is_none() {
                self.page = content.height;
            }
        } else {
            // 单行文字在内容区里垂直居中（行高比内容区高时上下对称地溢出到内边距里）；放得下时按对齐方式摆
            let line_height = self.line_height();
            let free = (content.width - self.doc.width()).max(0.0);
            let shift = match self.doc.style().align {
                TextAlign::Center => free * 0.5,
                TextAlign::End => free,
                TextAlign::Start | TextAlign::Justify => 0.0,
            };
            self.origin = [content.x + shift, content.y + (content.height - line_height) * 0.5];
            // 让插入点留在可见范围内（手指拖着看别处时不跟）
            if !self.free_scroll {
                let caret = self.doc.caret_rect(self.doc.selection.focus, CARET_WIDTH).x0 as f32;
                if caret - self.scroll > content.width - CARET_WIDTH {
                    self.scroll = caret - content.width + CARET_WIDTH;
                }
                if caret < self.scroll {
                    self.scroll = caret;
                }
            }
            self.scroll = self.scroll.min(self.max_scroll(content.width)).max(0.0);
        }
        let origin = [self.origin[0] - self.scroll, self.origin[1]];

        let start = quads.len();
        // 失去焦点后选区还在，画成不活跃的颜色（再次获得焦点时还是这个选区）
        let fill = if self.focused { self.selection_color } else { self.inactive_color };
        self.doc.range_rects(self.doc.selection.range(), view, |rect| quads.push(rect_quad(origin, rect, world, fill, opacity)));
        // 段的字形拷贝时已经设好了层号和裁剪，别的图元（选区、占位文字、预编辑的下划线、插入点）最后设
        let glyphs = quads.len();
        if self.doc.is_empty() {
            let faint = self.placeholder_color.unwrap_or_else(|| color.with_alpha(color.0[3] as f32 / 255.0 * 0.4));
            let placeholder = self.placeholder.layout_at(text, content.width.max(1.0));
            text.push_glyphs(placeholder, self.origin, world, Ink { color: faint, opacity, gradient: 0 }, &[], quads);
        } else {
            self.doc.paint_glyphs(text, view, origin, world, ink, place, quads);
        }
        let placed = if self.doc.is_empty() { glyphs } else { quads.len() };
        if let Some(compose) = self.doc.compose.clone() {
            self.doc.range_rects(compose, view, |rect| {
                let underline = BoundingBox { y0: rect.y1 - 2.0, ..rect };
                quads.push(rect_quad(origin, underline, world, color, opacity));
            });
        }
        if self.focused && self.caret_on && !self.caret_hidden {
            let caret = self.doc.caret_rect(self.doc.selection.focus, CARET_WIDTH);
            quads.push(rect_quad(origin, caret, world, color, opacity));
        }
        if place != [0, 0] {
            let (before, after) = quads[start..].split_at_mut(placed - start);
            for q in before[..glyphs - start].iter_mut().chain(after) {
                q.layer = place[0];
                q.clip = place[1];
            }
        }

        // 单行：只有平移时能精确裁剪（轴对齐矩形求交，字形同时裁 UV）
        if !self.multiline && world.is_translation() {
            let clip = Rect {
                x: content.x + world.offset[0],
                y: bounds.y + world.offset[1],
                width: content.width,
                height: bounds.height,
            };
            let clipped: Vec<Quad> = quads.drain(start..).filter_map(|q| clip_quad(q, clip)).collect();
            quads.extend(clipped);
        }
    }
}

/// 往前挪到字的开头（`offset` 超出时是末尾）
fn floor_char(s: &str, mut offset: usize) -> usize {
    offset = offset.min(s.len());
    while !s.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// 往后挪到字的结尾（`offset` 超出时是末尾）
fn ceil_char(s: &str, mut offset: usize) -> usize {
    offset = offset.min(s.len());
    while !s.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// 单行输入框里换行符换成空格；多行输入框把 `\r\n`、`\r` 统一成 `\n`
fn clean(s: &str, multiline: bool) -> Cow<'_, str> {
    if multiline {
        if s.contains('\r') { Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n")) } else { Cow::Borrowed(s) }
    } else if s.contains(['\n', '\r']) {
        Cow::Owned(s.replace("\r\n", " ").replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(s)
    }
}

/// 排版坐标里的矩形（相对文字原点）变成图元
fn rect_quad(origin: [f32; 2], rect: BoundingBox, world: &Affine, color: Color, opacity: f32) -> Quad {
    let (w, h) = ((rect.x1 - rect.x0) as f32, (rect.y1 - rect.y0) as f32);
    let [cx, cy] = world.apply([origin[0] + rect.x0 as f32 + w * 0.5, origin[1] + rect.y0 as f32 + h * 0.5]);
    let mut quad = Quad::new(cx - w * 0.5, cy - h * 0.5, w, h).fill(color).opacity(opacity);
    quad.transform = world.linear;
    quad
}

/// 选中静态文字时的选区图元（和输入框同一种画法）
pub(crate) fn selection_quad(origin: [f32; 2], rect: BoundingBox, world: &Affine, color: Color, opacity: f32) -> Quad {
    rect_quad(origin, rect, world, color, opacity)
}

/// 行内代码的底（`rect` 相对文字原点，上下只到字的上伸、下伸部分）：左右各留出一点，圆角
pub(crate) fn code_quad(origin: [f32; 2], rect: crate::geometry::Rect, size: f32, world: &Affine, color: Color, opacity: f32) -> Quad {
    let (pad_x, pad_y) = ((size * 0.25).round().max(2.0), 1.0);
    let (w, h) = (rect.width + pad_x * 2.0, rect.height + pad_y * 2.0);
    let [cx, cy] = world.apply([origin[0] + rect.x - pad_x + w * 0.5, origin[1] + rect.y - pad_y + h * 0.5]);
    let mut quad = Quad::new(cx - w * 0.5, cy - h * 0.5, w, h).radius(4.0).fill(color).opacity(opacity);
    quad.transform = world.linear;
    quad
}

/// 轴对齐图元与裁剪矩形求交；字形按比例裁 UV。完全在外面时返回 `None`
fn clip_quad(mut quad: Quad, clip: Rect) -> Option<Quad> {
    let [x, y, w, h] = quad.rect;
    let x0 = x.max(clip.x);
    let y0 = y.max(clip.y);
    let x1 = (x + w).min(clip.x + clip.width);
    let y1 = (y + h).min(clip.y + clip.height);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    if quad.kind != Quad::SHAPE {
        let [u0, v0, u1, v1] = quad.uv;
        let (du, dv) = ((u1 - u0) / w, (v1 - v0) / h);
        quad.uv = [u0 + (x0 - x) * du, v0 + (y0 - y) * dv, u0 + (x1 - x) * du, v0 + (y1 - y) * dv];
    }
    quad.rect = [x0, y0, x1 - x0, y1 - y0];
    Some(quad)
}
