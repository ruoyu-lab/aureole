//! 每帧的管线：一帧做哪些事、按什么顺序、各做几遍。
//!
//! 窗口的显示链接到点、测试工具推进时间时，按这个顺序（[`advance`] 和 [`Tree::frame`]）：
//!
//! 0. 推进之前（不在这里）：事件循环执行到点的定时器和任务、处理积压的输入（`app.rs`）；
//!    窗口收下后台解码好的图片、重新读好的文件（`host.rs`）
//! 1. **窗口大小**：变了就写进信号，按尺寸档切换的绑定在这一帧的布局之前执行（[`write_viewport`]）
//! 2. **滚动**：收回渲染线程推进到的滚动、拖放靠近边缘时自动滚动、推进滚动的物理，偏移写回句柄（[`scroll`]）
//! 3. **树外更新**：读到的信号变了的元素种类（画布……）在树外更新（[`update_kinds`]）
//! 4. **一遍帧**（[`Tree::frame`]，借着树）：
//!    - 动画：属性动画、共享元素的飞行、时间线、结束退场和关闭的浮层、拖放的影子（[`Tree::animate`]）
//!    - 内容的时钟：新加载的字体、输入框插入点闪烁、字符网格（[`Tree::tick_content`]）
//!    - 样式继承：文字样式沿树往下传（[`Tree::inherit_styles`]）
//!    - 布局：先量富文本里的行内元素（它们在布局树里自成一棵），再排版；量出行高、绕排让开的区域、行内元素的大小
//!      写回后再排，最多 [`EXTRA_LAYOUTS`] 遍；然后布局位置的过渡、无障碍、滚动范围（[`Tree::lay_out`]）
//!    - 布局之后：插入点滚进视口、共享元素决定显示谁（[`Tree::after_layout`]）
//!    - 定位：布局了就把绝对位置、内容区、到屏幕的变换写到全部节点上；没布局时只重算静态地改了变换的子树
//!      （[`Tree::place`]，见 `tree/place.rs`）
//!    - 绘制：生成显示列表；有画布等着重画时推迟（[`Tree::paint_phase`]）
//!    - 绘制之后：动图换帧、拖放的影子落向哪里、浮层跟着锚点（[`Tree::after_paint`]）
//! 5. **写回**：滚动偏移、浮层的位置、焦点处理器、动图播完、测量结果、时间线、画布写回信号（[`write_back`]）；
//!    写了之后界面又脏了就回到 4，一次推进最多 [`MAX_PASSES`] 遍
//! 6. **悬停**：重新布局或者滚动过，按新位置刷新指针下的元素
//! 7. 之后（不在这里）：窗口取走要交给渲染线程的东西（显示列表、图集、[`Tree::take_layer_updates`]），
//!    然后是无障碍、输入法、指针形状（`host.rs`）
//!
//! 节点的绝对位置和到屏幕的变换（`bounds`、`world`）在定位阶段写好：绘制只读它们，命中测试读到的是这一次布局的位置，
//! 不用等这一帧画完（画布推迟了重画时也一样）。

use std::cell::Cell;
use std::time::Instant;

use super::{FrameStats, Tree, TreeRef, animated, interaction, kinds, measure, overlays, refresh_hover, scrolling, timeline};

/// 一遍帧里，第一次布局之后最多再布局几遍（量出来的行高、绕排让开的区域写回后又要布局；
/// 行高不随位置变，一般一次就够，绕排一般第二遍就稳定）
pub(super) const EXTRA_LAYOUTS: usize = 4;

/// 一次推进最多做几遍帧（写回信号之后界面又变了就再做一遍：虚拟列表第一次知道视口多高、量出行高后
/// 要多挂几行，浮层限制高度后重新布局……）
pub(super) const MAX_PASSES: usize = 4;

thread_local! {
    /// 已经提示过"布局用满了遍数"、"推进用满了遍数"（每个线程各提示一次，免得每帧刷屏）
    static WARNED: Cell<[bool; 2]> = const { Cell::new([false; 2]) };
}

/// 用满了遍数、最后一遍还在变：记一条警告，说明是哪些阶段反复把界面弄脏（每种只提示一次）
fn warn_once(which: usize, message: impl FnOnce() -> String) {
    let first = WARNED.with(|w| {
        let mut warned = w.get();
        let first = !warned[which];
        warned[which] = true;
        w.set(warned);
        first
    });
    if first {
        tracing::warn!("{}", message());
    }
}

/// 推进一帧（窗口的显示链接和测试工具都走这里）。顺序见模块说明
pub(crate) fn advance(tree: &TreeRef, now: f64) -> FrameStats {
    write_viewport(tree);
    let scrolled = scroll(tree, now);
    update_kinds(tree);
    let mut stats = tree.borrow_mut().frame(now);
    let mut last = stats.laid_out;
    for pass in 1..MAX_PASSES {
        let writers = write_back(tree, last);
        if !writers.any() || !tree.borrow().is_dirty() {
            break;
        }
        if pass == MAX_PASSES - 1 {
            warn_once(1, || {
                format!("一次推进做满了 {MAX_PASSES} 遍帧，最后一遍写回之后界面还在变：{}。剩下的留到下一帧", writers.names())
            });
        }
        let again = tree.borrow_mut().frame(now);
        last = again.laid_out;
        // 有一遍整个重新生成了显示列表：交出去的是整份新的，不算修补
        let rebuilt = |s: &FrameStats| s.repainted && s.patched_quads == 0;
        let patched = if rebuilt(&stats) || rebuilt(&again) { 0 } else { stats.patched_quads + again.patched_quads };
        stats = FrameStats {
            laid_out: stats.laid_out || again.laid_out,
            repainted: stats.repainted || again.repainted,
            patched_quads: patched,
            layout_time: stats.layout_time + again.layout_time,
            paint_time: stats.paint_time + again.paint_time,
            canvas_draws: stats.canvas_draws + again.canvas_draws,
            ..again
        };
    }
    // 视口的偏移变了：一帧发一次 on_scroll
    scrolling::emit_scroll_events(tree);
    // 重新布局或滚动之后按新位置刷新悬停：只发进入和离开，不发移动（指针没动）
    if stats.laid_out || scrolled {
        refresh_hover(tree);
    }
    stats
}

/// 第 1 步，窗口大小变了：先写进信号（不借着树），按尺寸档、断点切换的绑定在这一帧的布局之前执行
fn write_viewport(tree: &TreeRef) {
    let resized = tree.borrow_mut().take_viewport_write();
    if let Some((metrics, size)) = resized {
        metrics.set_size(size);
    }
}

/// 第 2 步，滚动：主线程刚卡过就先收回渲染线程推进到的滚动状态，拖放中指针靠近视口边缘时先自动滚动，
/// 再推进滚动的物理；偏移写回句柄（滚动联动的绑定、虚拟列表在同一帧跟上）。返回有没有视口的偏移变了
fn scroll(tree: &TreeRef, now: f64) -> bool {
    let scrolled = {
        let mut t = tree.borrow_mut();
        t.adopt_scrolls();
        // 主线程按指针推着走，每帧把状态交给渲染线程，和手指拖着一样
        if t.drag_drop.dragging() {
            t.auto_scroll(now);
        }
        t.step_scroll(now)
    };
    scrolling::flush(tree);
    scrolled
}

/// 第 3 步，读到的信号变了的画布先重画（大小变了的在布局之后发现，写回时再画）
fn update_kinds(tree: &TreeRef) {
    tree.borrow_mut().kinds.reset_deferred();
    kinds::flush_updates(tree);
}

/// 一遍写回里写了东西的阶段
#[derive(Clone, Copy)]
struct Writers([bool; 7]);

impl Writers {
    const NAMES: [&'static str; 7] = ["滚动偏移", "浮层的位置", "焦点回调和拖放", "动图播完", "测量结果", "时间线", "画布"];

    fn any(self) -> bool {
        self.0.iter().any(|&w| w)
    }

    fn names(self) -> String {
        let names: Vec<&str> = Self::NAMES.iter().zip(self.0).filter(|(_, w)| *w).map(|(name, _)| *name).collect();
        names.join("、")
    }
}

/// 第 5 步，这一遍的结果写回信号（已释放树的借用，信号触发的副作用可以改树）：滚动偏移、浮层实际的位置、
/// 自动聚焦的处理器、动图播完、测量结果（只在重新布局之后才会变）、时间线、大小变了的画布。
/// 每一样都要写（不因为前面的写了就跳过后面的）
fn write_back(tree: &TreeRef, laid_out: bool) -> Writers {
    let scrolls = scrolling::flush(tree);
    let overlays = overlays::flush(tree);
    let input = interaction::flush(tree);
    let images = animated::flush(tree);
    let measures = laid_out && measure::flush(tree);
    let timelines = timeline::flush(tree);
    let canvases = kinds::flush_updates(tree);
    Writers([scrolls, overlays, input, images, measures, timelines, canvases])
}

impl Tree {
    /// 第 4 步，一遍帧：推进到时间 `now`（秒），只做被标记的工作。顺序见模块说明
    pub(crate) fn frame(&mut self, now: f64) -> FrameStats {
        // 这一帧的动画时间：宿主时刻经动画时钟换算。轨道按它采样，卡顿之后进行中的动画按真实时间追上（和渲染线程一致）
        let time = self.anim.tick(now);
        let timelines = self.animate(time);
        self.tick_content(now);
        self.inherit_styles();
        let mut stats = FrameStats::default();
        self.lay_out(now, &mut stats);
        self.after_layout(now);
        self.place(&stats);
        self.paint_phase(&mut stats);
        self.after_paint(now, time, &stats);
        [stats.quads, stats.layers] = [self.paint.list().quads.len(), self.paint.layers().len()];
        stats.animations = self.anim.active().len() + usize::from(self.drag_drop.animating()) + timelines + self.shared.flying();
        self.anim.set_moving(stats.animations);
        stats.canvas_draws = self.kinds.take_updates();
        stats
    }

    /// 还有没做的布局、样式继承或绘制（包括等着原地修补的）
    pub(super) fn is_dirty(&self) -> bool {
        self.layout.is_dirty() || self.paint.is_dirty() || self.paint.has_patches()
    }

    /// 动画：属性动画、共享元素的飞行、时间线在这一帧的动画时间 `time` 采样，动画停下的退场节点和关闭的浮层卸载，
    /// 拖放的影子落到位、淡出。返回还在动的时间线数
    fn animate(&mut self, time: f64) -> usize {
        if self.anim.take_seek() {
            self.requeue_animations();
            self.resample_timelines();
        }
        self.step_animations(time);
        self.step_flights(time);
        let timelines = self.step_timelines(time);
        self.finish_exits();
        self.finish_overlays();
        // 结束了就去掉，这一帧重画
        self.step_drag_preview(time);
        timelines
    }

    /// 内容自己的时钟：加载了新字体时已经排好的文字都重新排（缺字回退的结果可能变了）；应用的外观换了时换一套默认的颜色，
    /// 继承的文字颜色、选区重新算，文字重新排（链接、行内代码的颜色在排版里）；元素种类的时钟（有焦点的输入框、字符网格的插入点闪烁）
    fn tick_content(&mut self, now: f64) {
        if self.text.sync_fonts() {
            self.invalidate_text();
        }
        if self.paint.set_colors(reactive::untracked(crate::defaults::palette)) {
            self.layout.restyle();
            self.invalidate_text();
        }
        self.tick_kinds(now);
    }

    /// 样式继承：文字样式沿树往下传（有元素改了文字样式、增删了子元素时）
    fn inherit_styles(&mut self) {
        if self.layout.take_restyle() {
            self.resolve_styles();
        }
    }

    /// 布局（脏了才做）：排版；行高量出来的虚拟列表量挂载着的行、绕排放好让开的区域，变了再排，
    /// 最多再排 [`EXTRA_LAYOUTS`] 遍。之后布局位置的过渡、无障碍、滚动范围跟上，要重画
    fn lay_out(&mut self, now: f64, stats: &mut FrameStats) {
        let Some(restyled) = self.layout.pending() else { return };
        // 只是窗口大小变了（`restyled` 为假）：布局位置直接跟上，不滑
        let start = Instant::now();
        self.measure_inlines();
        self.layout.compute(&mut self.text, &mut self.nodes);
        for extra in 0..EXTRA_LAYOUTS {
            let (lists, flows, inlines) = (self.measure_lists(), self.place_flows(), self.settle_inlines());
            if !(lists | flows | inlines) {
                break;
            }
            if extra == EXTRA_LAYOUTS - 1 {
                let culprits = match (lists, flows) {
                    (true, true) => "虚拟列表量出的行高、绕排让开的区域",
                    (true, false) => "虚拟列表量出的行高",
                    (false, true) => "绕排让开的区域",
                    (false, false) => "行内元素的大小",
                };
                warn_once(0, || format!("一遍帧里布局之后又排了 {EXTRA_LAYOUTS} 遍，最后一遍{culprits}还在变，可能不会稳定"));
            }
            self.layout.compute(&mut self.text, &mut self.nodes);
        }
        self.finish_inlines();
        self.layout.laid_out();
        if self.anim.sliding() {
            self.detect_slides(restyled);
        }
        self.accessibility.relayout();
        self.update_scroll_metrics(now);
        self.paint.invalidate();
        stats.laid_out = true;
        stats.layout_time = start.elapsed();
    }

    /// 布局之后、绘制之前：多行输入框的插入点滚进视口（用新的内容高度），共享元素有变化的标签重新决定显示谁
    /// （新显示的在这一次画的时候开始飞）
    fn after_layout(&mut self, now: f64) {
        self.reveal_caret(now);
        self.resolve_shared();
    }

    /// 定位：布局了就把位置、内容区、到屏幕的变换写到全部节点上（命中测试、无障碍、浮层读的就是它们）；
    /// 没布局时只重算静态地改了变换属性的子树
    fn place(&mut self, stats: &FrameStats) {
        if stats.laid_out {
            self.place_nodes();
        } else {
            self.refresh_moved();
        }
    }

    /// 绘制（脏了才做）：生成显示列表和合成层表；字形图集满了扩大后再生成一次（最多到 4096）。
    /// 有画布要重画（大小变了）时这一遍先不生成，等画完（写回时，已经释放了树的借用）再生成，
    /// 免得同一帧生成两次；每次推进最多推迟一次
    fn paint_phase(&mut self, stats: &mut FrameStats) {
        let defer = self.kinds.can_defer() && self.updates_pending();
        if defer {
            self.kinds.defer();
        }
        if defer || !(self.paint.is_dirty() || self.paint.has_patches()) {
            return;
        }
        let start = Instant::now();
        // 只改了颜色、圆角、边框、一层外投影：原地修补那几个图元（改不了时下面整个重新生成）
        let mut faded = Vec::new();
        if !self.paint.is_dirty()
            && let Some(patched) = self.paint.apply_patches(&self.nodes, &mut faded)
        {
            // 半透明的元素改了透明度：只改它的层的透明度（交给渲染线程，层表、图元都不动）
            for id in faded {
                self.commit.opacity(super::layer_key(id), self.nodes[id].paint.opacity);
            }
            stats.repainted |= patched > 0;
            stats.patched_quads += patched;
            stats.paint_time = start.elapsed();
            return;
        }
        self.build_list();
        // 图集满了：扩大再画一遍。一次用到很多路径时（一屏不同的 SVG 图标）要连着扩大几次，到最大的一页为止
        while let Some(can_grow) = self.text.grow_if_full() {
            self.build_list();
            if !can_grow {
                break;
            }
        }
        self.paint.painted();
        stats.repainted = true;
        stats.paint_time = start.elapsed();
    }

    /// 绘制之后：动图换帧（刚挂载的元素已经有了位置；有东西动了时不在屏幕上的动图马上再看一次露出来没有），
    /// 拖放的影子决定落向哪里、抬起的元素跟着指针或者落到位，浮层跟着锚点重新放（布局、重画或者有东西动了之后）
    fn after_paint(&mut self, now: f64, time: f64, stats: &FrameStats) {
        let moved = stats.laid_out || stats.repainted || self.paint.worlds_dirty();
        if moved {
            self.images.moved(now);
        }
        self.step_images(now);
        if self.drag_drop.animating() {
            self.place_drag_preview(time);
        }
        if !self.overlays.is_empty() && (stats.laid_out || stats.repainted || self.paint.worlds_dirty()) {
            self.sync_timeline_layers();
            self.place_overlays();
        }
    }
}
