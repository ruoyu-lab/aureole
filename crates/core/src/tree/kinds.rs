//! 元素种类（[`crate::kinds::ElementKind`]）在元素树里的部分：登记要每帧推进、要在树外更新、和缩放有关的种类，
//! 推进它们的时钟，在树外更新，量大小，画。
//!
//! - **时钟**：声明了 `TICK` 的种类和有焦点的元素，每帧在布局之前调用 `tick`，返回的脏标记照办；
//!   它们的 `next_frame` 决定画面静止时要不要继续出帧
//! - **树外更新**：声明了 `UPDATE` 的种类，元素有了大小以后、这次读到的信号变了或者大小变了时，在树外面调用
//!   `update`（借着树时执行用户代码不安全）：先把种类对象从节点上取下来，放进一个副作用里执行（读到的信号就是它的依赖），
//!   再放回去。信号变了时副作用只做标记，下一帧开始时（已经释放了树的借用）统一再更新，一帧里改多少次信号都只更新一次。
//!   大小变了的在布局之后发现：这一遍先不生成显示列表，写回时更新完再生成，免得同一帧生成两次

#[cfg(feature = "char-grid")]
mod char_grid;
mod editor;
mod image;
pub(crate) use image::image_area;
mod path;
mod text;
mod text_runs;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use reactive::{Effect, Scope};

pub(crate) use text::LINK_KEY;
pub(crate) use text_runs::clusters;

use super::{NodeId, Tree, TreeRef};
use crate::canvas::{CanvasTextMetrics, TextMeasurer};
use crate::gradient::Fill;
use crate::text::TextStyle;
use crate::kinds::{ElementKind, KindHooks, NextFrame, PaintContext, TickContext, UpdateContext};
use crate::property::registry::Dirty;

/// 只指定了一边时按 `[宽, 高]` 的比例算另一边，都没指定时就用它（路径的画布大小、图片的原始大小）
fn keep_aspect(known: [Option<f32>; 2], [w, h]: [f32; 2]) -> [f32; 2] {
    let ratio = if w > 0.0 && h > 0.0 { h / w } else { 1.0 };
    match known {
        [Some(width), Some(height)] => [width, height],
        [Some(width), None] => [width, width * ratio],
        [None, Some(height)] => [height / ratio, height],
        [None, None] => [w, h],
    }
}

/// 声明了 `UPDATE` 的一个元素：它的副作用和上次更新时的大小
struct Tracked {
    id: NodeId,
    /// 上次更新读到的信号变了（副作用里设置，不借用树）
    invalid: Rc<Cell<bool>>,
    /// 跟踪上次更新读到的信号
    effect: Option<Effect>,
    /// 元素所在的作用域：副作用归它，元素卸载时一起释放
    scope: Option<Scope>,
    /// 上次更新时元素的大小；还没更新过是 `None`
    size: Option<[f32; 2]>,
}

/// 要参与每帧步骤的种类
#[derive(Default)]
pub(super) struct Kinds {
    /// 声明了 `TICK` 的
    ticking: Vec<NodeId>,
    /// 声明了 `SCALE_FACTOR` 的
    scaled: Vec<NodeId>,
    /// 声明了 `UPDATE` 的
    tracked: Vec<Tracked>,
    /// 上次报告以来更新了几次
    updates: usize,
    /// 这一次推进里已经为了等树外更新推迟过一次生成显示列表
    deferred: bool,
}

impl Kinds {
    /// 新挂载的元素按它的种类声明的步骤登记
    pub(super) fn add(&mut self, id: NodeId, hooks: KindHooks) {
        if hooks.contains(KindHooks::TICK) {
            self.ticking.push(id);
        }
        if hooks.contains(KindHooks::SCALE_FACTOR) {
            self.scaled.push(id);
        }
        if hooks.contains(KindHooks::UPDATE) {
            let invalid = Rc::new(Cell::new(true));
            self.tracked.push(Tracked { id, invalid, effect: None, scope: Scope::current(), size: None });
        }
    }

    /// 卸载了的元素不再登记着；它们跟踪信号的副作用停下
    pub(super) fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        self.ticking.retain(|&id| keep(id));
        self.scaled.retain(|&id| keep(id));
        self.tracked.retain_mut(|t| {
            let kept = keep(t.id);
            if !kept && let Some(effect) = t.effect.take() {
                effect.dispose();
            }
            kept
        });
    }

    /// 量出来的大小和窗口缩放有关的元素
    pub(super) fn scaled(&self) -> Vec<NodeId> {
        self.scaled.clone()
    }

    /// 新的一次推进开始：可以再推迟一次生成显示列表
    pub(super) fn reset_deferred(&mut self) {
        self.deferred = false;
    }

    /// 这一次推进还没推迟过
    pub(super) fn can_defer(&self) -> bool {
        !self.deferred
    }

    pub(super) fn defer(&mut self) {
        self.deferred = true;
    }

    /// 取走上次报告以来更新的次数
    pub(super) fn take_updates(&mut self) -> usize {
        std::mem::take(&mut self.updates)
    }
}

/// 要更新的一个元素（从树里取出来，释放借用之后再更新）
struct Job {
    id: NodeId,
    size: [f32; 2],
    scale: f32,
    scope: Option<Scope>,
    invalid: Rc<Cell<bool>>,
    old: Option<Effect>,
}

impl Tree {
    /// 改了种类的状态以后，按它返回的脏标记办：重新量、重画、重建无障碍节点
    pub(super) fn apply_dirty(&mut self, id: NodeId, dirty: Dirty) {
        if dirty.contains(Dirty::LAYOUT)
            && let Some(node) = self.nodes.get(id)
        {
            self.layout.mark_dirty(node.layout);
        }
        if dirty.intersects(Dirty::PAINT | Dirty::LAYOUT) {
            self.paint.invalidate();
        }
        if dirty.contains(Dirty::ACCESSIBILITY) {
            self.accessibility.touch(id);
        }
    }

    /// 推进声明了 `TICK` 的种类和有焦点的种类的时钟
    pub(super) fn tick_kinds(&mut self, now: f64) {
        let focused = self.router.focused();
        if let Some(id) = focused {
            self.tick_kind(id, now, true);
        }
        for i in 0..self.kinds.ticking.len() {
            let id = self.kinds.ticking[i];
            if Some(id) != focused {
                self.tick_kind(id, now, false);
            }
        }
    }

    fn tick_kind(&mut self, id: NodeId, now: f64, focused: bool) {
        let Some(kind) = self.nodes.element_kind_mut(id) else { return };
        let dirty = kind.tick(&TickContext { seconds: now, focused });
        if !dirty.is_empty() {
            self.apply_dirty(id, dirty);
        }
    }

    /// 种类要的下一帧：有焦点的、声明了 `TICK` 的问 `next_frame`（按这一帧的宿主时刻，和 [`TickContext::now`] 同一个时钟），
    /// 声明了 `UPDATE` 的读到的信号变了还没更新时要下一帧。几个种类要的时刻取最早的；时刻已经到了算作要下一帧
    pub(super) fn kinds_next_frame(&self) -> NextFrame {
        if self.kinds.tracked.iter().any(|t| t.invalid.get() && self.nodes.contains_key(t.id)) {
            return NextFrame::Continuous;
        }
        let now = std::time::Duration::from_secs_f64(self.anim.host().max(0.0));
        let mut next = NextFrame::Idle;
        for id in self.router.focused().into_iter().chain(self.kinds.ticking.iter().copied()) {
            let Some(kind) = self.nodes.element_kind(id) else { continue };
            match kind.next_frame(now) {
                NextFrame::Idle => {}
                NextFrame::At(at) if at > now => {
                    next = match next {
                        NextFrame::At(earlier) if earlier <= at => next,
                        _ => NextFrame::At(at),
                    }
                }
                NextFrame::At(_) | NextFrame::Continuous => return NextFrame::Continuous,
            }
        }
        next
    }

    /// 元素现在的大小（布局之后的）；还没布局过或者大小是 0 时是 `None`
    fn laid_out_size(&self, id: NodeId) -> Option<[f32; 2]> {
        let l = self.layout.try_get(self.nodes.get(id)?.layout)?;
        let size = [l.size.width, l.size.height];
        (size[0] > 0.0 && size[1] > 0.0).then_some(size)
    }

    /// 有种类要在树外更新：读到的信号变了，或者大小变了（布局之后检查：这一遍先不生成显示列表）
    pub(super) fn updates_pending(&self) -> bool {
        !self.kinds.tracked.is_empty()
            && self.kinds.tracked.iter().any(|t| self.laid_out_size(t.id).is_some_and(|size| t.invalid.get() || t.size != Some(size)))
    }

    /// 要在树外更新的种类（种类对象更新时才一个个取下来）
    fn update_jobs(&mut self) -> Vec<Job> {
        let scale = self.text.scale();
        let mut jobs = Vec::new();
        for i in 0..self.kinds.tracked.len() {
            let id = self.kinds.tracked[i].id;
            let Some(size) = self.laid_out_size(id) else { continue };
            let t = &self.kinds.tracked[i];
            if !t.invalid.get() && t.size == Some(size) {
                continue;
            }
            if self.nodes.element_kind(id).is_none() {
                continue;
            }
            let t = &mut self.kinds.tracked[i];
            t.invalid.set(false);
            t.size = Some(size);
            jobs.push(Job { id, size, scale, scope: t.scope, invalid: t.invalid.clone(), old: t.effect.take() });
        }
        jobs
    }

    /// 更新完了：种类对象放回去，记下跟踪信号的副作用，重画（要的话重新量）
    fn kind_updated(&mut self, id: NodeId, mut kind: Box<dyn ElementKind>, effect: Effect, dirty: Dirty) {
        if !self.nodes.contains_key(id) {
            // 更新的时候元素被卸载了：种类对象自己还回资源
            effect.dispose();
            kind.unmount(&mut crate::kinds::UnmountContext { text: &mut self.text, images: self.images.system_mut() });
            return;
        }
        self.nodes.put_kind(id, kind);
        match self.kinds.tracked.iter_mut().find(|t| t.id == id) {
            Some(t) => t.effect = Some(effect),
            None => effect.dispose(),
        }
        self.kinds.updates += 1;
        self.apply_dirty(id, dirty);
    }

    /// 继承的文字颜色（[`PaintContext::color`]）
    fn node_color(&self, id: NodeId) -> &Fill {
        &self.nodes[id].paint.color
    }
}

/// 在树外更新读到的信号变了、或者大小变了的种类（树已经释放；`update` 里可以读任何信号）。返回有没有更新
pub(crate) fn flush_updates(tree: &TreeRef) -> bool {
    let jobs = {
        let mut t = tree.borrow_mut();
        if t.kinds.tracked.is_empty() {
            return false;
        }
        t.update_jobs()
    };
    if jobs.is_empty() {
        return false;
    }
    // 画布在树外画，量文字时才短暂借一下树里的文字系统（这时没有别的借用）
    let measure: TextMeasurer = {
        let tree = tree.clone();
        Rc::new(move |content: &str, style: &TextStyle| {
            let mut layout = tree.borrow_mut().text.build(content, style);
            layout.break_all_lines(None);
            let baseline = layout.lines().next().map_or(0.0, |line| line.metrics().baseline);
            CanvasTextMetrics { width: layout.width(), height: layout.height(), baseline }
        })
    };
    for job in jobs {
        let Job { id, size, scale, scope, invalid, old } = job;
        if let Some(old) = old {
            old.dispose();
        }
        // 取下种类对象（更新时要在树外面执行用户代码）
        let Some(kind) = tree.borrow_mut().nodes.take_kind(id) else { continue };
        // 种类对象放进副作用里更新一次（读到的信号成为依赖），再取出来放回节点。
        // 在批量更新里做：`update` 里写的信号（命中区域）等放回去、释放树以后才通知
        let slot: Rc<RefCell<Option<Box<dyn ElementKind>>>> = Rc::new(RefCell::new(Some(kind)));
        let dirty = Rc::new(Cell::new(Dirty::PAINT));
        reactive::batch(|| {
            let (inside, flag, first, measure) = (slot.clone(), dirty.clone(), Cell::new(true), measure.clone());
            let make = move || {
                Effect::new(move || {
                    if first.replace(false) {
                        if let Some(kind) = inside.borrow_mut().as_mut() {
                            let mut cx = UpdateContext { size, scale, dirty: Dirty::PAINT, text: Some(measure.clone()) };
                            kind.update(&mut cx);
                            flag.set(cx.dirty);
                        }
                    } else {
                        // 读到的信号变了：只标记，下一帧再更新（这个副作用之后不再读信号，等着下次更新时被换掉）
                        invalid.set(true);
                    }
                })
            };
            let effect = match scope {
                Some(scope) => scope.run(make),
                None => make(),
            };
            let kind = slot.borrow_mut().take();
            if let Some(kind) = kind {
                tree.borrow_mut().kind_updated(id, kind, effect, dirty.get());
            } else {
                effect.dispose();
            }
        });
    }
    true
}

impl PaintContext<'_> {
    /// 继承的文字颜色（[`crate::Element::color`]）
    pub fn color(&self) -> &Fill {
        self.tree.node_color(self.id)
    }

    /// 元素有键盘焦点
    pub fn is_focused(&self) -> bool {
        self.tree.router.focused() == Some(self.id)
    }
}
