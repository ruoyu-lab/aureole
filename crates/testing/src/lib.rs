//! 测试工具：无窗口、无 GPU 地驱动元素树，用于测试和基准。
//!
//! 测试工具就是一个没有窗口的窗口宿主（核心的 `host.rs`）：输入和真实窗口走同一个 `WindowHost::input`，
//! 每帧走同一份 `WindowHost::frame`；画面交给进程里模拟的渲染线程（合成器），输入法、指针形状直接读元素树。
//! 系统那一侧换成平台层的无头后端（`aureole_platform::headless`：剪贴板是进程内的，打开网址只记下来），截图用渲染器离屏画。
//!
//! 元素树的快照、动图编码这些离不开核心的部分在核心的 `testing` 模块里，这里一并转出；
//! 门面的 `aureole::testing` 再加上运行时读回平台对象的几个函数。

use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use platform::{NotificationResponse, PlatformBackend};
use platform::headless::HeadlessBackend;
use reactive::Scope;
use render::Gpu;
use scene::{Angle, AtlasUpdate, Color, DisplayList, LayerTransform, Quad, Viewport};

use aureole_core::host::{self, Compositor, DragOutRequest, HostWindow, IDLE_FRAMES, Recorder, RenderFrame, WindowHost, WindowTree};
use aureole_core::internal::{Chord, Recording, TreeProbe, animation_time_at, restart_virtual_time, set_virtual_time, settle as settle_tasks, wait_image_idle};
use aureole_core::kinds::NextFrame;
use aureole_core::testing::{Source, typed};
use aureole_core::{
    Appearance, Cursor, ExternalDataKinds, ExternalDropData, ExternalDropEffect, FrameStats, GesturePhase, Ime, InputEvent, InputKind, Key, KeyInput, Modifiers, MouseButton, NamedKey, PointerId, Rect,
    View, WheelDelta, defaults,
};

mod replay;

pub use aureole_core::testing::{
    AnimationFormat, AnimationInfo, Edges, ElementFlags, ElementInfo, ElementNode, ElementTree, PaintInfo,
    encode_animation,
};
pub use platform::NotificationRequest;
pub use replay::{CallLog, Snapshot, Watch};

/// 每帧推进的时间：1/120 秒（取整到纳秒）
pub const FRAME: Duration = Duration::from_nanos(8_333_333);

/// 虚拟时间每帧累加的秒数，不用取整到纳秒的 [`FRAME`]：第 60 帧正好是 0.5 秒，定时器和动画的结尾落在帧上
const FRAME_SECONDS: f64 = 1.0 / 120.0;

/// 挂载一个视图，手动推进帧、模拟鼠标。时间是虚拟的，每帧前进 [`FRAME`]。
/// 也模拟渲染线程那一侧的合成层：和窗口里一样，合成层动画由它独立推进
pub struct Harness {
    /// 没有窗口的窗口宿主：输入、帧管线和真实窗口是同一份
    host: WindowHost<Headless>,
    /// 读元素树的状态
    probe: TreeProbe,
    scope: Scope,
    time: Cell<f64>,
    /// 生成输入事件时记着的指针位置、按着的键、修饰键
    source: RefCell<Source>,
}

/// 把后来的无障碍更新合进先来的（两次读取之间推进了几帧）：同一个节点留最新的
fn merge_accessibility(older: Option<accesskit::TreeUpdate>, newer: accesskit::TreeUpdate) -> accesskit::TreeUpdate {
    let Some(mut merged) = older else { return newer };
    let mut index: std::collections::HashMap<accesskit::NodeId, usize> = merged.nodes.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    for (id, node) in newer.nodes {
        match index.get(&id) {
            Some(&i) => merged.nodes[i].1 = node,
            None => {
                index.insert(id, merged.nodes.len());
                merged.nodes.push((id, node));
            }
        }
    }
    merged.tree = newer.tree.or(merged.tree);
    merged.tree_id = newer.tree_id;
    merged.focus = newer.focus;
    merged
}

/// 测试工具这一半窗口：画面交给进程里模拟的渲染线程；图集更新、无障碍更新、拖到应用外面的请求先记下，
/// 测试读的时候再取（[`Harness::take_atlas_updates`]、[`Harness::accessibility_update`]、[`Harness::take_drag_out`]）
pub(crate) struct Headless {
    /// 每次调用都检查元素树没被借用（真实窗口的系统可能在调用里同步送回事件，见 [`HostWindow`]）
    probe: TreeProbe,
    compositor: RefCell<Compositor>,
    atlas: RefCell<Vec<AtlasUpdate>>,
    accessibility: RefCell<Option<accesskit::TreeUpdate>>,
    drag_out: RefCell<Option<DragOutRequest>>,
    /// 请平台的输入法重置了几次（放弃组字时，[`Harness::ime_reset_count`]）
    ime_reset_count: Cell<usize>,
    /// 请系统拖动窗口了几次（[`Harness::window_drag_count`]）
    window_drag_count: Cell<usize>,
    /// 请系统按设置处理双击了几次（[`Harness::title_bar_double_click_count`]）
    title_bar_double_click_count: Cell<usize>,
    /// 报告给窗口的最大化按钮的范围（[`Harness::maximize_button_area`]）
    maximize_button_area: Cell<Option<aureole_core::Rect>>,
}

impl Headless {
    fn released(&self) {
        assert!(self.probe.is_released(), "调用窗口时元素树还借着：系统同步送回的事件会借不到它");
    }
}

impl HostWindow for Headless {
    fn scale_factor(&self) -> f32 {
        self.released();
        1.0
    }

    /// 渲染线程：滚动推进到这一帧的时间，收下新的显示列表、动画和滚动状态；合成层动画推进到这一帧的宿主时刻
    /// （按主线程交来的动画时钟换算，和主线程同一个动画时间）。
    /// 主线程这一帧没东西交过来时，渲染线程上还在动的滚动（比如滚动条淡出）照样走完
    fn submit(&self, now: f64, frame: Option<RenderFrame>) {
        self.released();
        let mut compositor = self.compositor.borrow_mut();
        match frame {
            Some(RenderFrame { scene, layers, atlas }) => {
                self.atlas.borrow_mut().extend(atlas);
                compositor.advance_scrolls(layers.time);
                // 只原地修补了图元的帧：合成层表没变（换上的话主线程给过的额外平移会被当成过时的丢掉）
                if let Some(scene) = scene.filter(|s| s.patch.is_none()) {
                    compositor.set_layers(scene.layers);
                }
                compositor.apply(*layers);
            }
            None => {
                compositor.advance_scrolls(now);
            }
        }
        compositor.advance(now);
    }

    // 输入法的位置、指针形状：测试直接读元素树（[`Harness::ime_area`]、[`Harness::cursor`]）
    fn set_ime(&self, _: Option<aureole_core::ImeRequest>) {
        self.released();
    }

    fn reset_ime(&self) {
        self.released();
        self.ime_reset_count.set(self.ime_reset_count.get() + 1);
    }

    fn set_cursor(&self, _: Cursor) {
        self.released();
    }

    fn accessibility(&self, update: accesskit::TreeUpdate) {
        self.released();
        let older = self.accessibility.take();
        self.accessibility.replace(Some(merge_accessibility(older, update)));
    }

    fn begin_drag_out(&self, request: DragOutRequest) {
        self.released();
        self.drag_out.replace(Some(request));
    }

    fn focus_changed(&self, _: bool) {
        self.released();
    }

    fn begin_window_drag(&self) {
        self.released();
        self.window_drag_count.set(self.window_drag_count.get() + 1);
    }

    fn perform_title_bar_double_click(&self) {
        self.released();
        self.title_bar_double_click_count.set(self.title_bar_double_click_count.get() + 1);
    }

    fn set_maximize_button_area(&self, area: Option<aureole_core::Rect>) {
        self.released();
        self.maximize_button_area.set(area);
    }
}

impl Harness {
    /// 在 `width` x `height`（逻辑像素）的虚拟窗口里挂载视图。系统外观固定为浅色、不减少动态效果，剪贴板换成进程内的，
    /// 打开网址只记下来（[`Self::opened_urls`]）；
    /// 挂载后还没有布局，先调用 [`Self::frame`]。
    ///
    /// 虚拟时钟从 0 开始：之后（包括第一次 [`Self::frame`] 之前）创建的定时器、`sleep` 都按这个测试工具的时间算，
    /// 不受同一个线程里之前的测试工具影响。`view` 在调用之前就构建好了，构建时建的定时器也一样：
    /// 还没到期的定时器跟着时钟一起拨回去，剩下的等待时间不变，和先拨好钟再构建视图一样
    pub fn new(width: f32, height: f32, view: impl View) -> Self {
        // 虚拟时间归零：定时器表和时钟是线程局部的，先建的测试工具会把时钟留在它推进到的地方。
        // 参数里构建视图时建的定时器是按旧的时钟定的到期时间，一起挪过来
        restart_virtual_time();
        // 系统那一侧换成平台层的无头后端：剪贴板是进程内的，测试复制粘贴时不碰用户的剪贴板（`aureole::clipboard` 读写的也是它）；
        // 点链接、`open_url` 不真的打开网址，记下来（`opened_urls`）
        HeadlessSystem::install();
        // 系统外观固定为浅色、不减少动态效果、没有强调色，结果不随运行测试的机器变
        aureole_core::host::set_system_appearance(Appearance::Light);
        aureole_core::host::set_motion_reduced(false);
        aureole_core::host::set_system_accent_color(None);
        let tree = WindowTree::new([width, height]);
        let scope = Scope::new();
        scope.run(|| {
            tree.attach_metrics(None);
            tree.mount(view.into_element());
        });
        let probe = TreeProbe::new(&tree);
        let headless = Headless {
            probe: probe.clone(),
            compositor: RefCell::new(Compositor::new(Some(tree.handback()))),
            atlas: RefCell::default(),
            accessibility: RefCell::default(),
            drag_out: RefCell::default(),
            ime_reset_count: Cell::new(0),
            window_drag_count: Cell::new(0),
            title_bar_double_click_count: Cell::new(0),
            maximize_button_area: Cell::new(None),
        };
        Self {
            host: WindowHost::new(tree, headless),
            probe,
            scope,
            time: Cell::new(0.0),
            source: RefCell::default(),
        }
    }

    /// 推进一帧：先执行投递过来的工作、就绪的异步任务，再推进虚拟时间、执行到期的定时器、唤醒到期的 `sleep`，
    /// 再走和窗口一样的帧管线
    /// （重新布局或滚动后按新位置刷新悬停状态）
    pub fn frame(&self) -> FrameStats {
        host::run_ready_tasks();
        self.time.set(self.time.get() + FRAME_SECONDS);
        set_virtual_time(self.time.get());
        let _ = host::run_due_timers();
        self.host.frame(self.time.get()).stats
    }

    /// 一次输入，和真实窗口里（换算之后的）事件走同一条路；返回按键有没有被处理。
    /// 时间戳不用：事件按测试工具此刻的虚拟时间处理
    pub fn input(&self, event: InputEvent) -> bool {
        self.host.input(event)
    }

    /// 此刻发生的一次输入
    fn send(&self, kind: InputKind) -> bool {
        self.host.input(InputEvent::new(Duration::from_secs_f64(self.time.get()), kind))
    }

    // ---- 录制和回放 ----

    /// 之后的输入（点击、键入……都和真实窗口一样经过窗口宿主）录进 `path`，时间戳是虚拟时间。
    /// 文件头记下此刻的窗口大小和缩放；[`Self::finish_recording`]（或者测试工具释放时）写上结束的时间。
    /// 回放用 [`Self::replay`]
    pub fn record(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        let (size, scale) = (self.probe.viewport(), self.probe.scale());
        let recorder = Recorder::create(path, size, scale, None).unwrap_or_else(|e| panic!("建不了录制文件 {}：{e}", path.display()));
        self.host.record(recorder);
    }

    /// 结束录制，写上此刻的时间
    pub fn finish_recording(&self) {
        self.host.finish_recording(self.time.get());
    }

    /// 回放录下的输入（[`Self::record`] 录的，或者真实窗口里设了 `AUREOLE_RECORD` 录的），得到快照。
    /// 先按文件头改窗口大小和缩放；之后每次输入之前一帧一帧推进到它的时间（虚拟时间，和录制时一样每帧 [`FRAME`]），
    /// 虚拟时钟再走到它的时间戳，然后和真实窗口一样交给窗口宿主；最后推进到结束的时间再加 1 秒（动画、滚动停下来）。
    /// 每次输入、每帧之后记下调用和变了的状态（见 [`Watch`]）。
    ///
    /// 真实窗口里录的带着系统的输入习惯（Command / Ctrl、按 Control 点击是右键……），只在同一种系统上回放，
    /// 其他系统上返回跳过的快照
    pub fn replay(&self, recording: impl AsRef<Path>, watch: &Watch) -> Snapshot {
        /// 时间戳取到微秒，和帧的时间比较时留出余量
        const EPSILON: f64 = 1e-5;
        let recording = Recording::load(recording.as_ref()).unwrap_or_else(|e| panic!("{e}"));
        if let Some(platform) = recording.platform.as_deref().filter(|&p| p != std::env::consts::OS) {
            return Snapshot::skipped(format!("在 {platform} 上录的"));
        }
        let (size, scale) = (self.probe.viewport(), self.probe.scale());
        if (size, scale) != (recording.size, recording.scale) {
            self.send(InputKind::Resize { size: recording.size, scale_factor: recording.scale });
        }
        let mut snapshot = replay::Builder::new(watch);
        snapshot.sample(self);
        for event in recording.events {
            let time = host::clock::seconds(event.time);
            while self.time.get() + FRAME_SECONDS <= time + EPSILON {
                self.frame();
                snapshot.sample(self);
            }
            if time > host::clock::now() {
                set_virtual_time(time);
            }
            self.host.input(event);
            snapshot.sample(self);
        }
        while self.time.get() + FRAME_SECONDS <= recording.end + 1.0 + EPSILON {
            self.frame();
            snapshot.sample(self);
        }
        snapshot.finish()
    }

    /// 元素树此刻的快照：每个元素的种类、名字、文字、布局位置和标记，按树的顺序，根元素之后是开着的浮层。
    /// `to_string()` 一行一个元素：`text #title "标题" 60x20 @16,14`
    pub fn element_tree(&self) -> ElementTree {
        self.probe.element_tree()
    }

    /// 命名元素此刻的详细信息：外接矩形、外边距、内边距、绘制属性、字体、滚动偏移、叠放次序、属性动画
    pub fn element_info(&self, name: &str) -> Option<ElementInfo> {
        self.probe.element_info(name)
    }

    /// 等异步任务做完：见 [`settle`]。不推进帧（界面的布局和绘制在下一次 [`Self::frame`]）
    pub fn settle(&self) {
        settle();
    }

    /// 模拟用户在系统设置里切换外观（[`aureole_core::system_appearance`]）
    pub fn set_system_appearance(&self, appearance: Appearance) {
        aureole_core::host::set_system_appearance(appearance);
    }

    /// 模拟用户在系统的辅助功能设置里打开、关掉"减少动态效果"（[`aureole_core::is_motion_reduced`]）
    pub fn set_motion_reduced(&self, reduced: bool) {
        aureole_core::host::set_motion_reduced(reduced);
    }

    /// 模拟窗口隐藏了标题栏、度量变了（[`aureole_core::title_bar_info`]）
    pub fn set_title_bar_info(&self, info: aureole_core::TitleBarInfo) {
        self.host.tree().set_title_bar_info(info);
    }

    /// 按在窗口的拖动区域上拖动、交给系统拖动窗口的次数（属性 `window_drag_area`）
    pub fn window_drag_count(&self) -> usize {
        self.host.window().window_drag_count.get()
    }

    /// 双击窗口的拖动区域、交给系统按设置缩放或最小化窗口的次数
    pub fn title_bar_double_click_count(&self) -> usize {
        self.host.window().title_bar_double_click_count.get()
    }

    /// 报告给窗口的最大化按钮的范围（属性 `window_maximize_button`），推进一帧之后更新
    pub fn maximize_button_area(&self) -> Option<aureole_core::Rect> {
        self.host.window().maximize_button_area.get()
    }

    /// 换一套平台惯例（主修饰键、按词移动的修饰键、行首行尾的键、重做的键、滚轮一行滚多远）：测别的平台上的快捷键和编辑键。
    /// 测试工具默认是 macOS 的一套（[`aureole_core::Conventions::MACOS`]），在哪个系统上跑都一样
    pub fn set_conventions(&self, conventions: aureole_core::Conventions) {
        HeadlessSystem::with(|s| *s.conventions.lock().unwrap_or_else(|e| e.into_inner()) = conventions);
    }

    /// 等后台线程把图片全部解码完，再推进一帧把它们画出来。
    /// 没有事件循环，测试里用它代替"解码好了唤醒窗口"。
    ///
    /// 按进展等：解码线程池是全进程共用的，同时跑的测试多、机器忙时排得久，总共等多久说不准；
    /// 池子连续 10 秒没有解完任何一张（谁的都算）才算超时，panic 的信息里列出还在等的图片
    pub fn wait_images(&self) -> FrameStats {
        if let Err(waiting) = wait_image_idle(Duration::from_secs(10)) {
            panic!("等图片解码超时：解码线程池连续 10 秒没有解完任何一张，还在等 {} 张：{}", waiting.len(), waiting.join("、"));
        }
        self.frame()
    }

    /// 模拟主线程卡住 `duration` 这么久：这段时间只有渲染线程在推进合成层动画和滚动（惯性、弹簧、回弹、滚动条），
    /// 每帧把推进到的滚动状态交回主线程。结束时主线程恢复：先收回这些状态（之后的输入按屏幕上的偏移命中），
    /// 下一次 [`Self::frame`] 从那里接着推进
    pub fn stall(&self, duration: Duration) {
        self.stall_without_resuming(duration);
        self.resume();
    }

    /// 和 [`Self::stall`] 一样只推进渲染线程，但主线程还没恢复（不收回滚动状态）：检查卡住期间屏幕上的样子
    pub fn stall_without_resuming(&self, duration: Duration) {
        let frames = (duration.as_secs_f64() / FRAME_SECONDS).round() as usize;
        let window = self.host.window();
        for _ in 0..frames {
            self.time.set(self.time.get() + FRAME_SECONDS);
            // 统一时钟照走（主线程卡住时真实时间也在走）：卡完以后改目标、发命令从此刻出发，不回到卡住之前
            set_virtual_time(self.time.get());
            let mut compositor = window.compositor.borrow_mut();
            compositor.advance(self.time.get());
            compositor.advance_scrolls(self.time.get());
            compositor.publish();
        }
    }

    /// 主线程恢复：收回渲染线程推进到的滚动状态（[`Self::stall`] 结束时已经做了）
    pub fn resume(&self) {
        self.host.tree().adopt_scrolls();
    }

    /// 像窗口一样过 `duration` 这么久：还有事要做时一帧一帧推进，连续几帧什么都没做（和窗口暂停显示链接的条件一样）
    /// 就停下不出帧，时钟照走（元素树记着的时间停在最后一帧），到期的定时器、`sleep`，就绪的任务照样执行，执行后接着出帧。
    /// 元素种类只要某个时刻的一帧时（[`Self::next_frame`] 是 `At`，比如插入点闪烁），停着时钟直接走到那一刻、出一帧，
    /// 之后还是只要某个时刻的帧就接着停（和窗口按时刻唤醒一样）。
    /// 测"显示链接停着的时候来了输入"这类只在窗口里出现的时序。
    ///
    /// 顺便检查暂停的条件：说着不要帧的那几帧里，屏幕上的合成层不能还在动（不然窗口停下以后它就停住了），
    /// 动了就 panic，说明 [`Self::wants_frames`] 漏了它
    pub fn idle(&self, duration: Duration) {
        let end = self.time.get() + duration.as_secs_f64();
        let mut quiet = 0;
        let mut still: Option<Vec<LayerTransform>> = None;
        while self.time.get() + FRAME_SECONDS <= end + 1e-9 {
            if quiet < IDLE_FRAMES {
                let stats = self.frame();
                if stats.laid_out || stats.repainted || self.wants_frames() {
                    quiet = 0;
                    still = None;
                    continue;
                }
                let layers = self.layers();
                if let Some(before) = still.replace(layers.clone()) {
                    assert_eq!(before, layers, "不要帧了，屏幕上的合成层却还在动：窗口停下显示链接以后它会停住");
                }
                quiet += 1;
                continue;
            }
            // 显示链接停着：到期的定时器、sleep，就绪的任务把窗口叫醒；种类要的时刻到了出一帧（这一帧以后还是只要
            // 某个时刻的帧就接着停）；都没有就让时钟直接走到下一个到期的（或者结束）
            let (fired, next) = host::run_due_timers();
            if host::run_ready_tasks() | fired {
                quiet = 0;
                continue;
            }
            let at = match self.next_frame() {
                NextFrame::At(at) => Some(at.as_secs_f64()),
                NextFrame::Idle | NextFrame::Continuous => None,
            };
            if let Some(at) = at
                && self.time.get() + FRAME_SECONDS >= at - 1e-9
            {
                self.frame();
                if self.wants_frames() {
                    quiet = 0;
                }
                continue;
            }
            if self.time.get() >= end {
                return;
            }
            let mut due = next.map_or(end, |d| (self.time.get() + d.as_secs_f64() + 1e-9).min(end));
            // 下一次 `frame` 正好落在种类要的那一刻
            if let Some(at) = at {
                due = due.min(at - FRAME_SECONDS);
            }
            self.time.set(due);
            set_virtual_time(due);
        }
    }

    /// 连续推进 `duration` 这么久，返回最后一帧的统计
    pub fn advance(&self, duration: Duration) -> FrameStats {
        let frames = (duration.as_secs_f64() / FRAME_SECONDS).round().max(1.0) as usize;
        (0..frames).map(|_| self.frame()).last().unwrap_or_default()
    }

    /// 窗口缩放，决定字形栅格化的分辨率；离屏渲染用 2 倍时设成 2
    pub fn set_scale_factor(&self, scale_factor: f32) {
        let size = self.probe.viewport();
        self.send(InputKind::Resize { size, scale_factor });
    }

    /// 取走自上次以来的图集更新（窗口里每帧交给渲染线程的那些），用来检查缓存
    pub fn take_atlas_updates(&self) -> Vec<AtlasUpdate> {
        let mut updates = self.host.window().atlas.take();
        updates.extend(self.host.tree().take_atlas_updates());
        updates
    }

    /// 路径图集那一页的边长和已用的比例（0 到 1），检查路径的栅格化结果有没有一直占新地方
    pub fn path_atlas(&self) -> (u32, f32) {
        self.probe.text_atlas_usage(scene::AtlasKind::Path)
    }

    /// 字形图集的完整内容，自己调用渲染器时用（一般用 [`Self::screenshot`] 就够了）
    pub fn atlas(&self) -> Vec<AtlasUpdate> {
        self.host.tree().atlas_snapshot()
    }

    /// 显示列表里的图元（层内坐标，没有乘上合成层的变换，见 [`Self::composited_quads`]）
    pub fn quads(&self) -> Vec<Quad> {
        self.host.tree().display_list().quads.clone()
    }

    /// 完整的显示列表（图元、裁剪、分组、特效），自己调用渲染器时用（一般用 [`Self::screenshot`] 就够了）
    pub fn display_list(&self) -> Arc<DisplayList> {
        self.host.tree().display_list()
    }

    /// 动画时间（给自定义着色器）：虚拟时间经动画时钟换算，平时就是虚拟时间；动画时钟暂停、慢放、拖动时跟着它
    pub fn time(&self) -> Duration {
        Duration::from_secs_f64(animation_time_at(self.time.get()).max(0.0))
    }

    /// 渲染线程此刻每个合成层的变换，自己调用渲染器时用（一般用 [`Self::screenshot`] 就够了）
    pub fn layers(&self) -> Vec<LayerTransform> {
        self.host.window().compositor.borrow().transforms()
    }

    /// 把合成层的变换和透明度乘进图元（和着色器做的一样），得到屏幕上的样子。
    /// 只对平的层准确：绕 x、y 轴转过的层投到屏幕上不是矩形，用 [`Self::layers`] 的 [`LayerTransform::apply`] 投各个角
    pub fn composited_quads(&self) -> Vec<Quad> {
        let layers = self.layers();
        self.quads()
            .iter()
            .map(|quad| {
                let layer = layers[quad.layer as usize];
                let [a, b, c, d] = layer.linear;
                let apply = |[x, y]: [f32; 2]| [a * x + c * y, b * x + d * y];
                let [w, h] = [quad.rect[2], quad.rect[3]];
                let center = apply([quad.rect[0] + w * 0.5, quad.rect[1] + h * 0.5]);
                let [x0, y0] = [center[0] + layer.offset[0], center[1] + layer.offset[1]];
                let t = quad.transform;
                let [c0, c1] = [apply([t[0], t[1]]), apply([t[2], t[3]])];
                Quad {
                    rect: [x0 - w * 0.5, y0 - h * 0.5, w, h],
                    transform: [c0[0], c0[1], c1[0], c1[1]],
                    opacity: quad.opacity * layer.opacity,
                    layer: 0,
                    ..*quad
                }
            })
            .collect()
    }

    /// 渲染线程是否还有合成层动画在动、视口在滚（在动就每帧自己出帧）
    pub fn is_compositing(&self) -> bool {
        self.host.window().compositor.borrow().is_animating()
    }

    /// 界面还要每帧推进吗：有动画、在滚动、输入框的插入点在闪……都没有时窗口暂停显示链接，
    /// 有输入、定时器、任务时再恢复（画面静止时进程不被唤醒）
    pub fn wants_frames(&self) -> bool {
        self.probe.wants_frames()
    }

    /// 接下来要什么时候的帧：`Continuous` 每帧要（[`Self::wants_frames`]）；`At(时刻)` 只要那一刻的一帧
    /// （虚拟时间，比如插入点下一次换亮灭），窗口在那之前睡着；`Idle` 都不要
    pub fn next_frame(&self) -> NextFrame {
        self.probe.next_frame()
    }

    /// 主线程按自己那份动画状态算出的层变换（命中测试用的就是它），应和 [`Self::layers`] 一致
    pub fn main_thread_layers(&self) -> Vec<LayerTransform> {
        self.probe.layer_transforms()
    }

    /// 用 [`aureole_core::Element::name`] 命名的元素在最近一次布局后的位置
    pub fn bounds(&self, name: &str) -> Option<Rect> {
        self.probe.bounds(name)
    }

    /// 命名元素现在显示的文字（完整的，不截断）：自己是 [`aureole_core::text()`] 或输入框时是它的内容，
    /// 否则是子孙里按树的顺序第一段文字（比如按钮的标签）。没有这个元素、里面也没有文字时是 `None`。
    /// 读的是元素树里现在的内容：绑定的文字在信号变了之后马上就是新的，不用等下一帧
    pub fn text(&self, name: &str) -> Option<String> {
        self.probe.text_content(name)
    }

    /// 命名的富文本（[`aureole_core::rich_text`]）里第 `k` 个链接（从 0 数）第一行的位置，窗口坐标；点它的中间就是点这个链接
    pub fn link_bounds(&self, name: &str, k: usize) -> Option<Rect> {
        self.probe.link_bounds(name, k)
    }

    /// 开着的文件对话框的请求（[`aureole_core::FileDialog`]、文件选择控件弹出的），最早的在前。
    /// 测试工具里不弹出，用 [`Harness::respond_file_dialog`] 给结果
    pub fn file_dialogs(&self) -> Vec<aureole_core::FileDialogRequest> {
        HeadlessSystem::with(|s| s.dialogs.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(request, _)| request.clone()).collect())
    }

    /// 像用户选好一样完成最早开着的文件对话框（空列表是取消），然后推进一帧让等它的任务继续。
    /// 没有开着的对话框时返回 `false`
    pub fn respond_file_dialog(&self, paths: Vec<std::path::PathBuf>) -> bool {
        let oldest = HeadlessSystem::with(|s| {
            let mut dialogs = s.dialogs.lock().unwrap_or_else(|e| e.into_inner());
            if dialogs.is_empty() { None } else { Some(dialogs.remove(0)) }
        });
        let Some((_, done)) = oldest else { return false };
        done(paths);
        self.frame();
        true
    }

    /// 换掉接着的显示器（默认是一个 1440 × 900、缩放比例 2 的主显示器）：测试恢复窗口位置这类按显示器决定的逻辑
    pub fn set_displays(&self, displays: Vec<aureole_core::DisplayInfo>) {
        HeadlessSystem::with(|s| *s.displays.lock().unwrap_or_else(|e| e.into_inner()) = displays);
    }

    /// 换掉现在的本地日期和时间（默认固定为 2026-01-15 09:30，测试不随真实的时钟变）
    pub fn set_local_time(&self, date: aureole_core::Date, time: aureole_core::Time) {
        HeadlessSystem::with(|s| *s.local_time.lock().unwrap_or_else(|e| e.into_inner()) = (date, time));
    }

    /// 这个测试工具创建以来打开过的网址（点链接、[`aureole_core::open_url`]）：测试里不真的交给系统
    pub fn opened_urls(&self) -> Vec<String> {
        HeadlessSystem::with(|s| s.backend.opened_urls())
    }

    /// 这个测试工具创建以来发过的通知（[`aureole_core`] 外面的 `aureole::notify`、`aureole::Notification`）：测试里不真的发
    pub fn notifications(&self) -> Vec<NotificationRequest> {
        HeadlessSystem::with(|s| s.backend.notification_requests())
    }

    /// 这个测试工具创建以来放过几次提示音（`aureole::beep`）：测试里不出声
    pub fn beep_count(&self) -> usize {
        HeadlessSystem::with(|s| s.backend.beep_count())
    }

    /// 模拟用户点了第 `index` 条通知本身（从 0 数）。回调投递回主线程，下一次 [`Self::frame`] 时调用处理器。
    /// 没有这一条、它没有回调时返回 `false`
    pub fn click_notification(&self, index: usize) -> bool {
        HeadlessSystem::with(|s| s.backend.respond_to_notification(index, NotificationResponse::Click))
    }

    /// 模拟用户点了第 `index` 条通知的第 `action` 个按钮（都从 0 数），同 [`Self::click_notification`]
    pub fn press_notification_action(&self, index: usize, action: usize) -> bool {
        HeadlessSystem::with(|s| s.backend.respond_to_notification(index, NotificationResponse::Action(action)))
    }

    /// 离屏截图：把此刻的显示列表和合成层画成像素，底色和窗口一样是调色板的窗口底色（跟着应用的外观）。
    /// 大小是虚拟窗口乘上 [`Self::set_scale_factor`] 的缩放（物理像素）。要 GPU；第一次调用时创建，之后共用。
    ///
    /// 先 [`Self::frame`]（有图片时 [`Self::wait_images`]）；合成层按渲染线程此刻的状态画，动画中途也能截。
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # use aureole::testing::Harness;
    /// let h = Harness::new(200.0, 100.0, rect().size(50, 50).background(Color::rgb(255, 0, 0)));
    /// h.frame();
    /// let shot = h.screenshot();
    /// assert_eq!(shot.pixel(25, 25), Color::rgb(255, 0, 0));
    /// shot.save_png(std::env::temp_dir().join("red.png")).unwrap();
    /// ```
    pub fn screenshot(&self) -> Screenshot {
        self.screenshot_with(defaults::palette().background)
    }

    /// 离屏截图，底色用 `background`（比如纯黑，方便检查像素），见 [`Self::screenshot`]
    pub fn screenshot_with(&self, background: Color) -> Screenshot {
        let (size, scale) = (self.probe.viewport(), self.probe.scale());
        let [width, height] = size.map(|v| ((v * scale).round() as u32).max(1));
        let viewport = Viewport { width, height, scale_factor: scale };
        let pixels = render::render_to_image(gpu(), &self.display_list(), &self.layers(), &self.atlas(), viewport, background, self.time());
        Screenshot { width, height, pixels }
    }

    /// 离屏截图到扩展范围的目标（和窗口在广色域、HDR 显示器上用的表面一样，见窗口的 `ColorOutput`），
    /// 显示器此刻的余量当作 `headroom`（SDR 白的倍数）：比它亮的颜色、HDR 图片按它压缩。
    /// 像素是线性的（sRGB 原色，分量可以小于 0、大于 1），其余同 [`Self::screenshot`]
    pub fn extended_screenshot(&self, headroom: f32) -> ExtendedScreenshot {
        let background = defaults::palette().background;
        let (size, scale) = (self.probe.viewport(), self.probe.scale());
        let [width, height] = size.map(|v| ((v * scale).round() as u32).max(1));
        let viewport = Viewport { width, height, scale_factor: scale };
        let pixels = render::render_to_extended_image(
            gpu(),
            &self.display_list(),
            &self.layers(),
            &self.atlas(),
            viewport,
            background,
            self.time(),
            headroom,
        );
        ExtendedScreenshot { width, height, pixels }
    }

    /// 命名的图片元素此刻显示动图（GIF、动态 WebP）的第几帧，从 0 开始；不是动图（或者还没解码好）时为 `None`
    pub fn image_frame(&self, name: &str) -> Option<usize> {
        self.probe.image_frame(name)
    }

    /// 模拟窗口被完全挡住（最小化、在别的窗口后面）/ 重新露出来：挡住时动图不换帧
    pub fn set_occluded(&self, occluded: bool) {
        self.host.tree().set_occluded(occluded);
    }

    /// 改虚拟窗口的大小，逻辑像素；下一帧重新布局
    pub fn resize(&self, width: f32, height: f32) {
        let scale = self.probe.scale();
        self.send(InputKind::Resize { size: [width, height], scale_factor: scale });
    }

    /// 鼠标移到窗口坐标 (x, y)，逻辑像素
    pub fn mouse_move(&self, x: f32, y: f32) {
        let kind = self.source.borrow_mut().moved(Some([x, y]));
        self.send(kind);
    }

    /// 鼠标移出窗口
    pub fn mouse_leave(&self) {
        let kind = self.source.borrow_mut().moved(None);
        self.send(kind);
    }

    /// 拖着文件经过 (x, y)；`None` 表示拖出窗口或取消
    pub fn hover_files(&self, point: Option<[f32; 2]>) {
        self.send(InputKind::FileHover(point));
    }

    /// 在 (x, y) 放下文件
    pub fn drop_files(&self, x: f32, y: f32, files: Vec<std::path::PathBuf>) {
        self.send(InputKind::FileDrop { position: Some([x, y]), paths: files });
    }

    /// 别的应用拖来的内容经过 (x, y)，返回元素的回答：有元素接受了是它说的操作，没有是 `None`
    pub fn external_drag_over(&self, x: f32, y: f32, kinds: ExternalDataKinds) -> Option<ExternalDropEffect> {
        self.send(InputKind::ExternalDragOver { position: [x, y], kinds });
        self.host.tree().external_drop_effect()
    }

    /// 最后一次问拖来的内容时的回答（文件的悬停也算）：有元素接受了是它说的操作，没有是 `None`
    pub fn external_drop_effect(&self) -> Option<ExternalDropEffect> {
        self.host.tree().external_drop_effect()
    }

    /// 最后一次问拖来的内容时，指针下有元素关心它
    pub fn is_external_drag_claimed(&self) -> bool {
        self.host.tree().is_external_drag_claimed()
    }

    /// 别的应用拖来的内容离开窗口或取消
    pub fn external_drag_leave(&self) {
        self.send(InputKind::ExternalDragLeave);
    }

    /// 别的应用拖来的内容放在 (x, y)
    pub fn external_drop(&self, x: f32, y: f32, data: ExternalDropData) {
        self.send(InputKind::ExternalDrop { position: [x, y], data });
    }

    /// 在鼠标此刻的位置按下、松开某个键
    fn button(&self, button: MouseButton, pressed: bool) {
        let kind = self.source.borrow_mut().button(button, pressed);
        self.send(kind);
    }

    /// 左键点击（带上 [`Self::set_modifiers`] 设的修饰键）
    pub fn click(&self, x: f32, y: f32) {
        self.mouse_move(x, y);
        self.button(MouseButton::Left, true);
        self.button(MouseButton::Left, false);
    }

    /// 按着修饰键点击（例如 Shift+点击扩展选区）
    pub fn click_with(&self, x: f32, y: f32, modifiers: Modifiers) {
        self.set_modifiers(modifiers);
        self.click(x, y);
        self.set_modifiers(Modifiers::empty());
    }

    /// 逐字符键入（不经过输入法）：每个字符按下再松开
    pub fn type_text(&self, text: &str) {
        for kind in text.chars().flat_map(typed) {
            self.send(kind);
        }
    }

    /// 不带修饰键按一个功能键；返回是否被处理
    pub fn key_down(&self, key: NamedKey) -> bool {
        self.key_down_with(Key::Named(key), Modifiers::empty())
    }

    /// 带修饰键按键，例如 `Key::Character("a".into())` + `Modifiers::SUPER`；返回是否被处理
    pub fn key_down_with(&self, key: Key, modifiers: Modifiers) -> bool {
        self.send(InputKind::KeyDown(KeyInput::new(key, modifiers)))
    }

    /// 松开一个功能键（之前用 [`Self::key_down`] 按下的）；返回是否被处理。没按下过的键松开时什么都不做
    pub fn key_up(&self, key: NamedKey) -> bool {
        self.key_up_with(Key::Named(key), Modifiers::empty())
    }

    /// 松开一个键，`modifiers` 是松开时按着的修饰键（先松开了 Shift 再松开字母时是空的）；返回是否被处理。
    /// 按键事件和按下时一样，交给按下时的那条链（见 [`aureole_core::Element::on_key_up`]）
    pub fn key_up_with(&self, key: Key, modifiers: Modifiers) -> bool {
        self.send(InputKind::KeyUp(KeyInput::new(key, modifiers)))
    }

    /// 窗口失去焦点：进行中的拖动、拖放和手势取消，按着的键全部松开（窗口收不到松开了）
    pub fn unfocus_window(&self) {
        self.send(InputKind::WindowFocus(false));
    }

    /// 取走拖到应用外面的请求（窗口里这时交给系统开始拖），没有时是 `None`。
    /// 之后用 [`Self::finish_drag_out`] 模拟系统的会话结束
    pub fn take_drag_out(&self) -> Option<aureole_core::DragOut> {
        let request = self.host.window().drag_out.take().or_else(|| self.probe.take_drag_out());
        request.map(|request| request.out)
    }

    /// 模拟系统的拖动会话结束：`accepted` 是有没有被别的应用接住，交给源元素的 `on_drag_out_end`
    pub fn finish_drag_out(&self, accepted: bool) {
        self.host.tree().finish_drag_out(accepted);
    }

    /// 输入法预编辑（例如拼音 "ni"），`cursor` 是预编辑文字里的字节范围
    pub fn ime_preedit(&self, text: &str, cursor: Option<(usize, usize)>) {
        self.send(InputKind::Ime(Ime::Preedit(text.to_owned(), cursor)));
    }

    /// 输入法提交：和真实窗口一样，先清空预编辑再提交
    pub fn ime_commit(&self, text: &str) {
        self.send(InputKind::Ime(Ime::Preedit(String::new(), None)));
        self.send(InputKind::Ime(Ime::Commit(text.to_owned())));
    }

    /// 所有输入框累计整形过多少段（输入框按段排版，编辑时只重排改动的那一段）
    pub fn shaped_paragraph_count(&self) -> usize {
        self.probe.shaped_paragraph_count()
    }

    /// 文字、输入框累计重新生成过多少个字形图元（字形、下划线、删除线；不算拷贝上次留着的）：
    /// 编辑时只重新生成改动的那一段，没动的、只是上下挪了的段拷贝上次的
    pub fn generated_glyph_count(&self) -> usize {
        self.probe.generated_glyph_count()
    }

    /// 文字、输入框留着的字形图元都不再用，下一帧整段重新生成。文字、输入框的每一段平时只在排版变了时重新生成图元，
    /// 别的都拷贝上次的（挪了位置的段整体挪过去）：测试拿它对照两种结果逐个图元相等
    pub fn forget_glyphs(&self) {
        self.probe.forget_glyphs();
    }

    /// 输入法候选框的位置；没有要文字输入的元素有焦点（或者是密码，不用输入法）时为 `None`
    pub fn ime_area(&self) -> Option<Rect> {
        self.probe.ime_area()
    }

    /// 放弃组字时请平台的输入法重置了几次（真实窗口关一次再开一次输入法，丢掉平台那边没确定的字）
    pub fn ime_reset_count(&self) -> usize {
        self.host.window().ime_reset_count.get()
    }

    /// 窗口要怎么设系统的文字输入（插入点区域和用途，密码也有）；没有要文字输入的元素有焦点时为 `None`
    pub fn ime_request(&self) -> Option<aureole_core::ImeRequest> {
        self.probe.ime_request()
    }

    /// 命名元素（或者它里面第一个输入框、第一段文字）的状态，和回放快照里 `text` 一行一样：
    /// 输入框是 `"内容" 锚点..焦点`（全文的字节偏移）加上 ` preedit "预编辑文字"`，可选中的文字是内容加上选中的部分
    pub fn text_state(&self, name: &str) -> Option<String> {
        self.probe.text_state(name)
    }

    /// 点击命名元素的中心
    pub fn click_named(&self, name: &str) {
        let [x, y] = self.bounds(name).unwrap_or_else(|| panic!("没有名为 {name} 的元素")).center();
        self.click(x, y);
    }

    /// 按住左键从 `from` 拖到 `to`（分 8 步移动，每步之后推进一帧），再松开。
    /// `to` 可以在元素外、窗口外（负数或超过窗口大小）：拖动开始后指针被捕获
    pub fn drag(&self, from: [f32; 2], to: [f32; 2]) {
        self.mouse_move(from[0], from[1]);
        self.button(MouseButton::Left, true);
        for i in 1..=8 {
            let t = i as f32 / 8.0;
            self.mouse_move(from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t);
            self.frame();
        }
        self.button(MouseButton::Left, false);
    }

    // ---- 鼠标按键和手势 ----

    /// 在 (x, y) 按下某个键（先把鼠标移过去），不松开
    pub fn mouse_down(&self, x: f32, y: f32, button: MouseButton) {
        self.mouse_move(x, y);
        self.button(button, true);
    }

    /// 松开某个键（在鼠标当前的位置）
    pub fn mouse_up(&self, button: MouseButton) {
        self.button(button, false);
    }

    /// 右键点击
    pub fn right_click(&self, x: f32, y: f32) {
        self.mouse_down(x, y, MouseButton::Right);
        self.mouse_up(MouseButton::Right);
    }

    /// 中键点击
    pub fn middle_click(&self, x: f32, y: f32) {
        self.mouse_down(x, y, MouseButton::Middle);
        self.mouse_up(MouseButton::Middle);
    }

    /// 双击：两次点击之间推进一帧（在双击的时间间隔内）
    pub fn double_click(&self, x: f32, y: f32) {
        self.click(x, y);
        self.frame();
        self.click(x, y);
    }

    /// 按住左键沿着 `points` 拖（第一个点按下，之后每个点移动一次、推进一帧），最后松开
    pub fn drag_path(&self, points: &[[f32; 2]]) {
        let Some(&[x, y]) = points.first() else { return };
        self.mouse_down(x, y, MouseButton::Left);
        for &[x, y] in &points[1..] {
            self.mouse_move(x, y);
            self.frame();
        }
        self.mouse_up(MouseButton::Left);
    }

    /// 修饰键（之后的指针事件带上它；按键用 [`Self::key_down_with`] 自己带）
    pub fn set_modifiers(&self, modifiers: Modifiers) {
        let kind = self.source.borrow_mut().modifiers(modifiers);
        self.send(kind);
    }

    /// 窗口失去焦点：进行中的拖动和手势取消
    pub fn cancel_pointer(&self) {
        let kind = InputKind::PointerCancel(self.source.borrow().pointer_input(None));
        self.send(kind);
    }

    fn pinch_event(&self, delta: f32, phase: GesturePhase) {
        let gesture = self.source.borrow().gesture(delta, phase);
        self.send(InputKind::Pinch(gesture));
    }

    fn rotate_event(&self, degrees: f32, phase: GesturePhase) {
        let gesture = self.source.borrow().gesture(degrees.to_radians(), phase);
        self.send(InputKind::Rotate(gesture));
    }

    /// 指针在 (x, y)，触控板捏合：开始，每个比例（0.1 是放大 10%）一次移动并推进一帧，结束
    pub fn pinch(&self, x: f32, y: f32, deltas: &[f32]) {
        self.mouse_move(x, y);
        self.pinch_event(0.0, GesturePhase::Start);
        for &delta in deltas {
            self.pinch_event(delta, GesturePhase::Move);
            self.frame();
        }
        self.pinch_event(0.0, GesturePhase::End);
    }

    /// 指针在 (x, y)，触控板旋转（按触控板报告的方向，正值逆时针）：每一步转多少
    pub fn rotate(&self, x: f32, y: f32, steps: &[Angle]) {
        self.mouse_move(x, y);
        self.rotate_event(0.0, GesturePhase::Start);
        for &delta in steps {
            self.rotate_event(delta.degrees(), GesturePhase::Move);
            self.frame();
        }
        self.rotate_event(0.0, GesturePhase::End);
    }

    /// 此刻的指针形状
    pub fn cursor(&self) -> Cursor {
        self.probe.cursor()
    }

    // ---- 触摸屏 ----

    /// 一根手指放上 (x, y)。`finger` 是它的编号，别的手指用别的编号；第一根手指是主指针
    pub fn touch_down(&self, finger: u64, x: f32, y: f32) {
        let kind = self.source.borrow_mut().touch_down(PointerId(finger), [x, y]);
        self.send(kind);
    }

    /// 这根手指移到 (x, y)
    pub fn touch_move(&self, finger: u64, x: f32, y: f32) {
        let kind = self.source.borrow_mut().touch_move(PointerId(finger), [x, y]);
        self.send(kind);
    }

    /// 这根手指抬起（在它最后的位置）
    pub fn touch_up(&self, finger: u64) {
        let kind = self.source.borrow_mut().touch_up(PointerId(finger));
        self.send(kind);
    }

    /// 系统收走了这根手指（比如系统手势开始了）
    pub fn touch_cancel(&self, finger: u64) {
        let kind = self.source.borrow_mut().touch_cancel(PointerId(finger));
        self.send(kind);
    }

    /// 一根手指在 (x, y) 点一下
    pub fn touch_tap(&self, x: f32, y: f32) {
        self.touch_down(1, x, y);
        self.touch_up(1);
    }

    /// 一根手指从 `from` 划到 `to`：放上，分 `frames` 帧匀速移动（每次之后推进一帧），抬起。
    /// 帧数少、距离长就是快速划动（松手有惯性），最后停一会儿再抬起用 [`Self::touch_down`] 这几个方法自己写
    pub fn touch_swipe(&self, from: [f32; 2], to: [f32; 2], frames: usize) {
        let frames = frames.max(1);
        self.touch_down(1, from[0], from[1]);
        for i in 1..=frames {
            let t = i as f32 / frames as f32;
            self.touch_move(1, from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t);
            self.frame();
        }
        self.touch_up(1);
    }

    // ---- 焦点和按键 ----

    /// 按一个组合键，写法同 [`aureole_core::shortcut`]（`"CmdOrCtrl+K"`、`"Shift+Tab"`、`"Escape"`）；返回是否被处理
    pub fn chord(&self, chord: &str) -> bool {
        let (key, modifiers) = Chord::parse(chord).unwrap_or_else(|| panic!("组合键写法不对：{chord}")).key();
        // 单个可打印字符（没有 Cmd / Ctrl）也带上输入的文字，和真实键盘一样
        let typed = match &key {
            Key::Character(c) if !modifiers.has_super() && !modifiers.has_control() => Some(c.to_string()),
            Key::Named(NamedKey::Space) => Some(" ".to_owned()),
            _ => None,
        };
        self.send(InputKind::KeyDown(KeyInput { text: typed.map(Into::into), ..KeyInput::new(key, modifiers) }))
    }

    /// 按 Tab，焦点移到下一个元素；返回是否被处理
    pub fn tab(&self) -> bool {
        self.key_down(NamedKey::Tab)
    }

    /// 按 Shift+Tab，焦点移到上一个元素；返回是否被处理
    pub fn shift_tab(&self) -> bool {
        self.key_down_with(Key::Named(NamedKey::Tab), Modifiers::SHIFT)
    }

    /// 有焦点的元素的名字（[`aureole_core::Element::name`]，没起名字的是空字符串）；没有焦点时为 `None`
    pub fn focused_name(&self) -> Option<String> {
        self.probe.focused_name()
    }

    /// 焦点环此刻显示着（焦点是用键盘移过来的）
    pub fn is_focus_ring_visible(&self) -> bool {
        self.probe.is_focus_ring_visible()
    }

    /// 命名元素此刻在窗口里的外接矩形：算上自己和祖先的变换、滚动、浮层的位置、3D 旋转和透视、共享元素的飞行
    /// （[`Self::bounds`] 是布局位置）。按主线程那份动画值算
    pub fn window_bounds(&self, name: &str) -> Option<Rect> {
        self.probe.window_bounds(name)
    }

    /// 命名元素此刻在窗口里的四个角（左上、右上、右下、左下）：绕 x、y 轴转过的元素投到屏幕上是一般的四边形。
    /// 有角在视点后面时为 `None`
    pub fn window_corners(&self, name: &str) -> Option<[[f32; 2]; 4]> {
        self.probe.window_corners(name)
    }

    // ---- 无障碍 ----

    /// 像辅助技术第一次来要树那样打开无障碍，返回整棵树（AccessKit 的格式）。之后元素树的变化
    /// 用 [`Self::accessibility_update`] 取
    pub fn accessibility(&self) -> accesskit::TreeUpdate {
        self.host.window().accessibility.take();
        self.host.tree().activate_accessibility()
    }

    /// 上次以来变了的节点（先 [`Self::frame`]：和窗口一样每帧生成一次，没读的合在一起）；没打开或者什么都没变时是 `None`
    pub fn accessibility_update(&self) -> Option<accesskit::TreeUpdate> {
        let pending = self.host.window().accessibility.take();
        match self.probe.accessibility_update() {
            Some(update) => Some(merge_accessibility(pending, update)),
            None => pending,
        }
    }

    /// 辅助技术请求一个操作（点击、聚焦、设置值……），和窗口里一样执行
    pub fn accessibility_action(&self, action: accesskit::Action, target: accesskit::NodeId, data: Option<accesskit::ActionData>) {
        let request = accesskit::ActionRequest { action, target_tree: accesskit::TreeId::ROOT, target_node: target, data };
        self.host.tree().perform_accessibility_action(request);
    }

    /// 命名元素在无障碍树里的编号（[`aureole_core::Element::name`] 也是节点的 `author_id`）
    pub fn accessibility_id(&self, name: &str) -> Option<accesskit::NodeId> {
        self.probe.accessibility_id(name)
    }

    // ---- 滚动 ----
    // 位移都是"滚动量"：正数向下、向右滚（偏移变大，内容往上、往左走），逻辑像素

    /// 鼠标在 (x, y) 处，滚轮滚 `lines` 格（每格约 40 逻辑像素），弹簧平滑过去
    pub fn wheel(&self, x: f32, y: f32, lines_x: f32, lines_y: f32) {
        self.mouse_move(x, y);
        let kind = self.source.borrow().wheel(WheelDelta::Lines([lines_x, lines_y]), GesturePhase::Move);
        self.send(kind);
    }

    /// 指针在 (x, y) 处的一个触控板事件。手指放上、移动、抬起分别是 `Started`、`Moved`、`Ended`；
    /// 系统的惯性紧跟在抬起之后，也是一组 `Started`、`Moved`、`Ended`
    pub fn trackpad(&self, x: f32, y: f32, phase: GesturePhase, dx: f32, dy: f32) {
        self.mouse_move(x, y);
        let kind = self.source.borrow().wheel(WheelDelta::Pixels([dx, dy]), phase);
        self.send(kind);
    }

    /// 手指在触控板上划动 (dx, dy)：放上，分 `frames` 帧匀速移动（每次之后推进一帧），抬起，不带惯性
    pub fn swipe(&self, x: f32, y: f32, dx: f32, dy: f32, frames: usize) {
        let frames = frames.max(1);
        self.trackpad(x, y, GesturePhase::Start, 0.0, 0.0);
        for _ in 0..frames {
            self.trackpad(x, y, GesturePhase::Move, dx / frames as f32, dy / frames as f32);
            self.frame();
        }
        self.trackpad(x, y, GesturePhase::End, 0.0, 0.0);
    }

    /// 快速划动后松手：手指以 (vx, vy) 逻辑像素每秒划 4 帧，抬起后是系统的惯性事件，
    /// 和 macOS 一样每毫秒衰减到 0.998 倍，直到慢下来（每次之后推进一帧）。返回系统的惯性事件一共给了多少位移。
    /// 视口在惯性开始时带着松手的速度自己滑出去（减速一样），之后的惯性事件不再管
    pub fn fling(&self, x: f32, y: f32, vx: f32, vy: f32) -> [f32; 2] {
        let step = FRAME_SECONDS as f32;
        self.trackpad(x, y, GesturePhase::Start, 0.0, 0.0);
        for _ in 0..4 {
            self.trackpad(x, y, GesturePhase::Move, vx * step, vy * step);
            self.frame();
        }
        self.trackpad(x, y, GesturePhase::End, 0.0, 0.0);
        self.frame();
        let decay = 0.998f32.powf(step * 1000.0);
        // 系统的惯性：第一个事件就带着位移
        let (mut v, mut total) = ([vx, vy], [0.0f32; 2]);
        let mut phase = GesturePhase::Start;
        while v[0].hypot(v[1]) > 20.0 {
            self.trackpad(x, y, std::mem::replace(&mut phase, GesturePhase::Move), v[0] * step, v[1] * step);
            total = [total[0] + v[0] * step, total[1] + v[1] * step];
            self.frame();
            v = [v[0] * decay, v[1] * decay];
        }
        self.trackpad(x, y, GesturePhase::End, 0.0, 0.0);
        total
    }
}

/// 截图共用的 GPU 设备
fn gpu() -> &'static Gpu {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    GPU.get_or_init(|| Gpu::new(&render::instance(), None))
}

/// 离屏截图（[`Harness::screenshot`]）：RGBA8、sRGB 编码、未预乘，逐行紧密排列
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screenshot {
    /// 宽，物理像素
    pub width: u32,
    /// 高，物理像素
    pub height: u32,
    /// 每个像素 4 个字节（R、G、B、A），从左上角开始一行一行排
    pub pixels: Vec<u8>,
}

impl Screenshot {
    /// (x, y) 处的颜色，物理像素坐标（2 倍缩放时是逻辑坐标的 2 倍）。超出范围时 panic
    pub fn pixel(&self, x: u32, y: u32) -> Color {
        assert!(x < self.width && y < self.height, "({x}, {y}) 在截图（{} x {}）外面", self.width, self.height);
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Color(self.pixels[i..i + 4].try_into().expect("每个像素 4 个字节"))
    }

    /// 存成 PNG
    pub fn save_png(&self, path: impl AsRef<Path>) -> io::Result<()> {
        image::save_buffer_with_format(path, &self.pixels, self.width, self.height, image::ExtendedColorType::Rgba8, image::ImageFormat::Png)
            .map_err(io::Error::other)
    }
}

/// 扩展范围的离屏截图（[`Harness::extended_screenshot`]）：线性、预乘的 RGBA（sRGB 原色），
/// 分量可以小于 0（sRGB 色域外）、大于 1（比 SDR 白亮）
#[derive(Clone, Debug, PartialEq)]
pub struct ExtendedScreenshot {
    /// 宽，物理像素
    pub width: u32,
    /// 高，物理像素
    pub height: u32,
    /// 每个像素 4 个 `f32`（R、G、B、A），从左上角开始一行一行排
    pub pixels: Vec<f32>,
}

impl ExtendedScreenshot {
    /// (x, y) 处的线性 RGBA（预乘）。超出范围时 panic
    pub fn pixel(&self, x: u32, y: u32) -> [f32; 4] {
        assert!(x < self.width && y < self.height, "({x}, {y}) 在截图（{} x {}）外面", self.width, self.height);
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.pixels[i..i + 4].try_into().expect("每个像素 4 个分量")
    }

    /// 截断到 sRGB 再编码成 8 位（色域外的分量截掉、比白亮的截到 1），和 [`Screenshot`] 比较用
    pub fn to_srgb(&self) -> Screenshot {
        let pixels = self
            .pixels
            .iter()
            .enumerate()
            .map(|(i, &v)| if i % 4 == 3 { (v.clamp(0.0, 1.0) * 255.0).round() as u8 } else { (scene::encode_srgb(v.clamp(0.0, 1.0)) * 255.0).round() as u8 })
            .collect();
        Screenshot { width: self.width, height: self.height, pixels }
    }

    /// 存成 16 位的 PNG，嵌上 Display P3 的 ICC 配置：在广色域显示器上看得出 P3 的颜色。
    /// P3 色域外的分量截断，比 SDR 白亮的部分截到白（PNG 是 SDR 的）
    pub fn save_p3_png(&self, path: impl AsRef<Path>) -> io::Result<()> {
        use image::ImageEncoder;
        let data: Vec<u8> = self
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|&[r, g, b, a]| {
                let a = a.clamp(0.0, 1.0);
                let rgb = if a > 0.0 { [r / a, g / a, b / a] } else { [0.0; 3] };
                let [r, g, b] = scene::multiply_matrix3(&scene::SRGB_TO_P3, rgb).map(|c| scene::encode_srgb(c.clamp(0.0, 1.0)));
                [r, g, b, a].map(|c| ((c * 65535.0).round() as u16).to_ne_bytes()).into_iter().flatten()
            })
            .collect();
        let file = io::BufWriter::new(std::fs::File::create(path)?);
        let mut encoder = image::codecs::png::PngEncoder::new(file);
        encoder.set_icc_profile(profile_bytes(ColorProfile::DisplayP3)).map_err(io::Error::other)?;
        encoder.write_image(&data, self.width, self.height, image::ExtendedColorType::Rgba16).map_err(io::Error::other)
    }
}

/// 色彩空间：测试和演示里生成带色彩配置的图片用，见 [`encode_with_profile`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorProfile {
    /// sRGB（嵌上 sRGB 的配置，和不嵌一样）
    Srgb,
    /// Display P3：和 sRGB 一样的曲线，红、绿更饱和
    DisplayP3,
    /// Adobe RGB (1998)：绿、青更饱和，曲线是 2.2 次幂
    AdobeRgb,
}

/// ICC 配置的字节
fn profile_bytes(profile: ColorProfile) -> Vec<u8> {
    let mut profile = match profile {
        ColorProfile::Srgb => moxcms::ColorProfile::new_srgb(),
        ColorProfile::DisplayP3 => moxcms::ColorProfile::new_display_p3(),
        ColorProfile::AdobeRgb => moxcms::ColorProfile::new_adobe_rgb(),
    };
    // 按 ICC 4 版的写法：媒体白点写 PCS 的 D50，色适应（chad）写 D65 -> D50 的 Bradford 矩阵，不写 cicp 标签
    profile.cicp = None;
    profile.media_white_point = Some(moxcms::Xyzd { x: 0.964_2, y: 1.0, z: 0.824_9 });
    profile.chromatic_adaptation = Some(moxcms::Matrix3d {
        v: [[1.047_811_2, 0.022_886_6, -0.050_127], [0.029_542_4, 0.990_484_4, -0.017_049_1], [-0.009_234_5, 0.015_043_6, 0.752_131_6]],
    });
    let mut bytes = profile.encode().expect("编码 ICC 配置失败");
    // 整个配置补齐到 4 字节的倍数（规范要求），头里的大小跟着改：不补齐时系统的图片查看器不认
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    let size = (bytes.len() as u32).to_be_bytes();
    bytes[..4].copy_from_slice(&size);
    bytes
}

/// 带色彩配置的图片的格式，见 [`encode_with_profile`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaggedFormat {
    /// PNG（ICC 在 iCCP 块里）
    Png,
    /// JPEG（质量 95，ICC 在 APP2 段里；alpha 丢掉）
    Jpeg,
    /// 无损 WebP（ICC 在 ICCP 块里）
    WebP,
}

/// 测试和演示用：把 RGBA8 像素（`profile` 色彩空间里的编码值，直通 alpha）编码成图片，嵌上 `profile` 的 ICC 配置。
/// 解码时按配置换到工作空间：同样的数值，Display P3 的颜色比 sRGB 的更饱和
///
/// ```
/// use aureole::testing::{ColorProfile, TaggedFormat, encode_with_profile};
/// let red = [255, 0, 0, 255].repeat(8 * 8);
/// let png = encode_with_profile(TaggedFormat::Png, 8, 8, &red, ColorProfile::DisplayP3);
/// assert_eq!(&png[1..4], b"PNG");
/// ```
pub fn encode_with_profile(format: TaggedFormat, width: u32, height: u32, pixels: &[u8], profile: ColorProfile) -> Vec<u8> {
    use image::ImageEncoder;
    let icc = profile_bytes(profile);
    let mut out = Vec::new();
    match format {
        TaggedFormat::Png => {
            let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
            encoder.set_icc_profile(icc).expect("PNG 能嵌 ICC");
            encoder.write_image(pixels, width, height, image::ExtendedColorType::Rgba8).expect("写 PNG 失败");
        }
        TaggedFormat::Jpeg => {
            let rgb: Vec<u8> = pixels.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95);
            encoder.set_icc_profile(icc).expect("JPEG 能嵌 ICC");
            encoder.write_image(&rgb, width, height, image::ExtendedColorType::Rgb8).expect("写 JPEG 失败");
        }
        TaggedFormat::WebP => {
            let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            encoder.set_icc_profile(icc).expect("WebP 能嵌 ICC");
            encoder.write_image(pixels, width, height, image::ExtendedColorType::Rgba8).expect("写 WebP 失败");
        }
    }
    out
}

/// 测试和演示用：编码成 HDR 的 PNG（16 位 RGB，cICP 写 BT.2020 原色、PQ 曲线）。`linear` 是每个像素线性的 RGB
/// （sRGB 原色，1.0 是 SDR 白，按参考白 203 尼特换成绝对亮度，最亮 10000 尼特约 49 倍）。
/// 解码后是半精度浮点的图，显示时按显示器的余量做色调映射
pub fn encode_hdr_png(width: u32, height: u32, linear: &[[f32; 3]]) -> Vec<u8> {
    assert_eq!(linear.len(), (width * height) as usize, "像素数和大小不符");
    let to_bt2020 = invert3(&scene::BT2020_TO_SRGB);
    let data: Vec<u8> = linear
        .iter()
        .flat_map(|&rgb| {
            scene::multiply_matrix3(&to_bt2020, rgb).map(|c| ((pq_inverse_eotf(c.max(0.0) * 203.0) * 65535.0).round() as u16).to_be_bytes())
        })
        .flatten()
        .collect();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().expect("写 PNG 失败");
    // cICP：BT.2020 原色（9）、PQ（16）、RGB（矩阵 0）、全范围
    writer.write_chunk(png::chunk::cICP, &[9, 16, 0, 1]).expect("写 PNG 失败");
    writer.write_image_data(&data).expect("写 PNG 失败");
    drop(writer);
    out
}

/// PQ 的 EOTF 的逆：绝对亮度（尼特）-> 编码值 0..=1
fn pq_inverse_eotf(nits: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

fn invert3(m: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let [[a, b, c], [d, e, f], [g, h, i]] = m.map(|row| row.map(|v| v as f64));
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    [
        [(e * i - f * h) / det, (c * h - b * i) / det, (b * f - c * e) / det],
        [(f * g - d * i) / det, (a * i - c * g) / det, (c * d - a * f) / det],
        [(d * h - e * g) / det, (b * g - a * h) / det, (a * e - b * d) / det],
    ]
    .map(|row| row.map(|v| v as f32))
}

/// 执行就绪的主线程任务（[`aureole_core::spawn_local`]、[`aureole_core::Task::then`]），等这个线程发出的后台任务
/// （[`aureole_core::spawn`]、[`aureole_core::spawn_blocking`]、[`aureole_core::sleep`]）全部结束、结果交回主线程，直到没有事可做。
/// 防抖、定时器、测试工具里的 `sleep` 按虚拟时间走，不在这里推进（用 [`Harness::advance`]）：后台任务在等虚拟时间的
/// `sleep` 时先推进时间再等它。最多等 10 秒，超时 panic
pub fn settle() {
    assert!(
        settle_tasks(Duration::from_secs(10)),
        "等了 10 秒，后台任务还没做完（在等测试工具里的 sleep？它跟虚拟时间走，先用 Harness::advance 推进）"
    );
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.finish_recording();
        self.scope.dispose();
        HeadlessSystem::uninstall();
    }
}

/// 测试工具在自己的线程上换上的系统那一侧：平台层的无头后端（[`HeadlessBackend`]）接到核心的注入点上。
/// 剪贴板是进程内的，打开网址只记下来，都由无头后端做（运行时那一侧接的是本机后端，两边走同一套服务接口）；
/// 外观浅色、不减少动态效果、没有强调色；平台惯例是 macOS 的一套（测试按 Command 写快捷键）。
/// 线程上最近创建的测试工具换上一个新的（剪贴板、记下的网址都从空的开始），任何一个测试工具释放时换回系统的
struct HeadlessSystem {
    backend: Arc<HeadlessBackend>,
    /// 开着的文件对话框：等测试给结果（[`Harness::respond_file_dialog`]）
    dialogs: std::sync::Mutex<Vec<(aureole_core::FileDialogRequest, aureole_core::FileDialogDone)>>,
    /// 现在的本地日期和时间（[`Harness::set_local_time`]）
    local_time: std::sync::Mutex<(aureole_core::Date, aureole_core::Time)>,
    /// 核心的惯例：测试可以换（[`Harness::set_conventions`]），无头后端的那一份建好以后不能改
    conventions: std::sync::Mutex<aureole_core::Conventions>,
    /// 接着的显示器（[`Harness::set_displays`]）
    displays: std::sync::Mutex<Vec<aureole_core::DisplayInfo>>,
}

thread_local! {
    static HEADLESS_SYSTEM: RefCell<Option<Rc<HeadlessSystem>>> = const { RefCell::new(None) };
}

impl HeadlessSystem {
    fn install() {
        let system = Rc::new(HeadlessSystem {
            backend: Arc::new(HeadlessBackend::new()),
            conventions: std::sync::Mutex::new(aureole_core::Conventions::MACOS),
            dialogs: std::sync::Mutex::new(Vec::new()),
            local_time: std::sync::Mutex::new((aureole_core::Date::new(2026, 1, 15), aureole_core::Time::new(9, 30))),
            displays: std::sync::Mutex::new(vec![aureole_core::DisplayInfo {
                name: "测试显示器".into(),
                position: [0.0, 0.0],
                size: [1440.0, 900.0],
                scale_factor: 2.0,
                primary: true,
            }]),
        });
        // 应用直接用的剪贴板、通知走平台层的后端：同一个无头后端
        platform::set_thread_backend(Some(system.backend.clone()));
        HEADLESS_SYSTEM.with(|s| *s.borrow_mut() = Some(system.clone()));
        aureole_core::host::set_thread_system_backend(Some(system));
    }

    fn uninstall() {
        HEADLESS_SYSTEM.with(|s| *s.borrow_mut() = None);
        aureole_core::host::set_thread_system_backend(None);
        platform::set_thread_backend(None);
    }

    fn with<R: Default>(f: impl FnOnce(&HeadlessSystem) -> R) -> R {
        HEADLESS_SYSTEM.with(|s| s.borrow().as_deref().map(f).unwrap_or_default())
    }
}

impl aureole_core::host::SystemBackend for HeadlessSystem {
    fn appearance(&self) -> Appearance {
        if self.backend.appearance().is_dark_mode() == Some(true) { Appearance::Dark } else { Appearance::Light }
    }

    fn is_motion_reduced(&self) -> bool {
        self.backend.appearance().is_motion_reduced()
    }

    fn accent_color(&self) -> Option<Color> {
        self.backend.appearance().accent_color().map(|[r, g, b]| Color::rgb(r, g, b))
    }

    fn read_clipboard(&self) -> Option<String> {
        self.backend.clipboard().read_text().ok().flatten()
    }

    fn write_clipboard(&self, text: String) -> bool {
        self.backend.clipboard().write_text(&text).is_ok()
    }

    fn open_url(&self, url: &str) -> bool {
        self.backend.shell().open_url(url).is_ok()
    }

    fn conventions(&self) -> aureole_core::Conventions {
        *self.conventions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn file_dialog(&self, request: aureole_core::FileDialogRequest, done: aureole_core::FileDialogDone) {
        self.dialogs.lock().unwrap_or_else(|e| e.into_inner()).push((request, done));
    }

    fn local_time(&self) -> Option<(aureole_core::Date, aureole_core::Time)> {
        Some(*self.local_time.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn displays(&self) -> Vec<aureole_core::DisplayInfo> {
        self.displays.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
