//! 图片：用到的图片（引用计数、图集、给渲染线程的纹理更新）和动图的播放。
//!
//! 显示动图的元素登记在 [`Images`] 里，它记着最早到点的时间：没到点的帧只比较一次时间。
//! 每帧在布局和重画之后推进（刚挂载的元素已经有了位置）。
//! 到点的元素换下一帧时只把新的一帧交给渲染线程，传到这个元素在图集（或单独纹理）里的位置上，
//! 显示列表不变，不重新布局、不重建图元。元素不在屏幕上（滚到视口外、被裁掉、完全透明、大小为 0）、
//! 窗口被挡住时不换帧，重新露出来时从停下的那一帧接着播。

use reactive::{Signal, batch};

use super::{NodeId, Tree, TreeRef};
use crate::geometry::Affine;
use crate::image::{ImageBlock, ImageSystem};
use crate::geometry::Rect;

/// 跟着系统暂停（减少动态效果）的动图多久看一次设置变了没有，秒
const RECHECK: f64 = 0.25;
/// 不在屏幕上的动图最多多久看一次露出来没有，秒（一般按它这一帧的延迟看）
const HIDDEN_RECHECK: f64 = 0.05;

/// 用到的图片和在播的动图
#[derive(Default)]
pub(super) struct Images {
    /// 用到的图片：引用计数、图片图集、给渲染线程的纹理更新
    system: ImageSystem,
    /// 显示动图的元素
    players: Vec<NodeId>,
    /// 最早要换帧的时间
    due: f64,
    /// 有动图因为不在屏幕上没换帧
    hidden: bool,
    /// 窗口被挡住了
    occluded: bool,
    /// 播完了要写回的信号
    writes: Vec<(Signal<bool>, bool)>,
}

impl Images {
    pub(super) fn system(&self) -> &ImageSystem {
        &self.system
    }

    pub(super) fn system_mut(&mut self) -> &mut ImageSystem {
        &mut self.system
    }

    /// 有动图在屏幕上播着（要每帧推进）
    pub(super) fn playing(&self) -> bool {
        !self.players.is_empty() && !self.hidden
    }

    /// 有东西动了（布局、重画、滚动、合成层动画）：不在屏幕上的动图马上再看一次露出来没有
    pub(super) fn moved(&mut self, now: f64) {
        if self.hidden {
            self.due = self.due.min(now);
        }
    }

    /// 释放树的借用之后要写回的信号（句柄上的"播完了"）
    pub(super) fn write(&mut self, signal: Signal<bool>, value: bool) {
        self.writes.push((signal, value));
    }
}

impl Tree {
    /// 元素显示的图片变了：显示的是动图就登记，下一帧开始计时
    pub(super) fn track_player(&mut self, id: NodeId) {
        if !self.images.players.contains(&id) {
            self.images.players.push(id);
        }
        self.images.due = 0.0;
    }

    /// 绑定的播放意图变了（`None` 是跟着系统）
    pub(crate) fn set_playing(&mut self, id: NodeId, intent: Option<bool>) {
        let Some(block) = self.nodes.kind_mut::<ImageBlock>(id) else { return };
        block.intent = intent;
        if let Some(player) = &mut block.player {
            // 重新开始播时从这一刻计时（暂停期间的时间不算）
            player.due = None;
        }
        self.images.due = 0.0;
    }

    /// 回到第一帧重新播
    pub(crate) fn restart_image(&mut self, id: NodeId) {
        let Some(block) = self.nodes.kind_mut::<ImageBlock>(id) else { return };
        if let Some(player) = &mut block.player
            && player.rewind()
        {
            self.images.system.show_frame(player);
        }
        self.images.due = 0.0;
    }

    /// 窗口被完全挡住（最小化、在别的窗口后面）/ 重新露出来
    pub(crate) fn set_occluded(&mut self, occluded: bool) {
        self.images.occluded = occluded;
        self.images.due = 0.0;
    }

    /// 命名的图片元素此刻显示动图的第几帧（测试用）；不是动图时为 `None`
    pub(crate) fn image_frame(&self, name: &str) -> Option<usize> {
        let id = self.named(name)?;
        self.nodes.kind::<ImageBlock>(id)?.player.as_ref().map(|p| p.frame)
    }

    /// 推进动图：没到点时只比较一次时间
    pub(super) fn step_images(&mut self, now: f64) {
        if self.images.players.is_empty() || now < self.images.due {
            return;
        }
        self.refresh_worlds();
        let reduce = reactive::untracked(crate::system::is_motion_reduced);
        let mut next = f64::INFINITY;
        self.images.hidden = false;
        let players = std::mem::take(&mut self.images.players);
        let mut kept = Vec::with_capacity(players.len());
        for id in players {
            if !self.nodes.contains_key(id) {
                continue;
            }
            let visible = !self.images.occluded && self.image_visible(id);
            let Some(block) = self.nodes.kind_mut::<ImageBlock>(id) else { continue };
            let (intent, handle) = (block.intent, block.handle);
            let Some(player) = &mut block.player else { continue };
            kept.push(id);
            if !intent.unwrap_or(!reduce) {
                player.due = None;
                // 跟着系统停下的：回到第一帧，过一会儿再看设置变了没有
                if intent.is_none() {
                    if player.rewind() {
                        self.images.system.show_frame(player);
                    }
                    next = next.min(now + RECHECK);
                }
                continue;
            }
            if player.finished {
                continue;
            }
            if !visible {
                // 不在屏幕上：时间不算，露出来以后这一帧重新计时
                player.due = None;
                self.images.hidden = true;
                next = next.min(now + player.delay().max(HIDDEN_RECHECK));
                continue;
            }
            let stepped = player.step(now);
            if stepped.changed {
                self.images.system.show_frame(player);
            }
            if stepped.finished
                && let Some(handle) = handle
            {
                self.images.writes.push((handle.finished, true));
            }
            if let (false, Some(due)) = (player.finished, player.due) {
                next = next.min(due);
            }
        }
        kept.append(&mut self.images.players);
        self.images.players = kept;
        self.images.due = next;
    }

    /// 元素此刻在屏幕上看得见：外接矩形和所有裁剪的祖先（`clip`、滚动视口）、窗口都有交集，且不是完全透明
    fn image_visible(&self, id: NodeId) -> bool {
        let Some(node) = self.nodes.get(id) else { return false };
        let mut visible = screen_rect(&node.world, node.bounds);
        let mut opacity = node.paint.opacity;
        let mut current = node.parent;
        while let Some(parent) = current.and_then(|p| self.nodes.get(p)) {
            opacity *= parent.paint.opacity;
            if parent.paint.clip || parent.scroll.is_some() {
                visible = intersect(visible, screen_rect(&parent.world, parent.bounds));
            }
            current = parent.parent;
        }
        let window = Rect { x: 0.0, y: 0.0, width: self.layout.viewport()[0], height: self.layout.viewport()[1] };
        let visible = intersect(visible, window);
        opacity > 0.0 && visible.width > 0.0 && visible.height > 0.0
    }
}

/// 布局矩形经过屏幕变换后的外接矩形
fn screen_rect(world: &Affine, b: Rect) -> Rect {
    let corners = [[b.x, b.y], [b.x + b.width, b.y], [b.x, b.y + b.height], [b.x + b.width, b.y + b.height]].map(|c| world.apply(c));
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for [x, y] in corners {
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.width).min(b.x + b.width), (a.y + a.height).min(b.y + b.height));
    Rect { x: x0, y: y0, width: (x1 - x0).max(0.0), height: (y1 - y0).max(0.0) }
}

/// 播完了的动图写回句柄的信号（树已经释放借用）。返回有没有写
pub(super) fn flush(tree: &TreeRef) -> bool {
    let writes: Vec<(Signal<bool>, bool)> = std::mem::take(&mut tree.borrow_mut().images.writes);
    if writes.is_empty() {
        return false;
    }
    batch(|| {
        for (signal, value) in writes {
            signal.set_if_changed(value);
        }
    });
    true
}
