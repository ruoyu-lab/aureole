//! 撤销和重做。每一步是"在某处删掉一段、插入一段"，加上前后的选区；
//! 连续打字、连续退格合成一步，输入法提交的一个词是一步。

use super::document::Caret;

/// 记撤销时交进来的一次编辑：插入的文字借着用，接着上一步合并时不用先复制一份
pub(crate) struct Edit<'a> {
    pub start: usize,
    pub removed: String,
    pub inserted: &'a str,
    pub before: (Caret, Caret),
    pub after: (Caret, Caret),
}

/// 一次编辑：从 `start` 起删掉 `removed`、插入 `inserted`
#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub start: usize,
    pub removed: String,
    pub inserted: String,
    /// 编辑前后的选区（锚点、焦点）
    pub before: (Caret, Caret),
    pub after: (Caret, Caret),
}

/// 编辑的种类：同类的连续编辑才合并
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Typing,
    Backspace,
    Delete,
    /// 输入法开始组字时替换掉了选中的文字：等提交的文字来了合成一步
    ImeStart,
    Ime,
    /// 粘贴、剪切、其他：各自一步
    Other,
}

struct Entry {
    change: Change,
    kind: Kind,
    time: f64,
}

/// 停顿超过这么久，下一次打字另起一步
const PAUSE: f64 = 1.5;
/// 最多记多少步
const LIMIT: usize = 500;

#[derive(Default)]
pub(crate) struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// 最后一步还能接着合并（移动插入点、点击之后就不能了）
    open: bool,
}

impl History {
    pub fn record(&mut self, edit: Edit, kind: Kind, now: f64) {
        self.redo.clear();
        if self.open
            && let Some(last) = self.undo.last_mut()
            && now - last.time < PAUSE
            && merge(last, &edit, kind)
        {
            last.time = now;
            // 换行结束这一组：撤销时一行一行地撤
            self.open = !(kind == Kind::Typing && edit.inserted.ends_with('\n'));
            return;
        }
        self.open = match kind {
            Kind::Typing => !edit.inserted.ends_with('\n'),
            Kind::Backspace | Kind::Delete | Kind::ImeStart => true,
            Kind::Ime | Kind::Other => false,
        };
        let Edit { start, removed, inserted, before, after } = edit;
        let change = Change { start, removed, inserted: inserted.to_owned(), before, after };
        self.undo.push(Entry { change, kind, time: now });
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
    }

    /// 插入点被移走了：之后的编辑另起一步
    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn undo(&mut self) -> Option<Change> {
        let entry = self.undo.pop()?;
        self.open = false;
        let change = entry.change.clone();
        self.redo.push(entry);
        Some(change)
    }

    pub fn redo(&mut self) -> Option<Change> {
        let entry = self.redo.pop()?;
        self.open = false;
        let change = entry.change.clone();
        self.undo.push(entry);
        Some(change)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// 能接上就合进上一步
fn merge(last: &mut Entry, new: &Edit, kind: Kind) -> bool {
    let old = &mut last.change;
    let merged = match (last.kind, kind) {
        // 接着上一次插入的末尾继续打字
        (Kind::Typing, Kind::Typing) if new.removed.is_empty() && new.start == old.start + old.inserted.len() => {
            old.inserted.push_str(new.inserted);
            true
        }
        // 连续退格：删掉的字接在前面
        (Kind::Backspace, Kind::Backspace)
            if new.inserted.is_empty() && old.inserted.is_empty() && new.start + new.removed.len() == old.start =>
        {
            old.removed.insert_str(0, &new.removed);
            old.start = new.start;
            true
        }
        // 连续向前删除：删掉的字接在后面
        (Kind::Delete, Kind::Delete) if new.inserted.is_empty() && old.inserted.is_empty() && new.start == old.start => {
            old.removed.push_str(&new.removed);
            true
        }
        // 组字前删掉的选区 + 提交的文字 = 一步
        (Kind::ImeStart, Kind::Ime) if new.removed.is_empty() && new.start == old.start && old.inserted.is_empty() => {
            new.inserted.clone_into(&mut old.inserted);
            last.kind = Kind::Ime;
            true
        }
        _ => false,
    };
    if merged {
        last.change.after = new.after;
    }
    merged
}
