//! 窗口宿主：把元素树接到一个窗口上。真实窗口（运行时 `aureole-runtime`）和没有窗口的测试工具（`aureole-testing`）
//! 都只用这个模块；要把界面放进自己的窗口、别的事件循环、自己的渲染器时，写第三个宿主。
//!
//! 一个宿主要做这几件事：
//!
//! 1. **建树、挂载**：[`WindowTree`] 是元素树的句柄。在一个响应式作用域里 `attach_metrics`、`mount`，
//!    再和窗口的另一半（实现 [`HostWindow`]）一起建 [`WindowHost`]。
//! 2. **交输入**：窗口的事件换算成 [`crate::InputEvent`] 交给 [`WindowHost::input`]，这是输入的唯一入口。
//! 3. **出帧**：帧时钟到了调用 [`WindowHost::frame`]，它推进滚动和动画、只做被标记的布局和绘制，
//!    把结果（[`RenderFrame`]）、无障碍更新、输入法、指针形状交给 [`HostWindow`] 的方法。
//!    返回的 [`FrameReport`] 说明这一帧做了什么、下一帧要在什么时候：连续 [`IDLE_FRAMES`] 帧没事就可以停下帧时钟。
//! 4. **转事件循环**：每一轮调用 [`run_ready_tasks`]（后台任务交回的结果、就绪的主线程任务）、[`run_due_timers`]（到期的定时器）。
//!    有后台结果到时靠 [`set_main_thread_waker`] 装上的句柄叫醒事件循环。
//! 5. **画出来**：[`RenderFrame`] 里的显示列表、图集更新是 `aureole-scene` 的纯数据，交给 `aureole-render` 或者自己的渲染器；
//!    合成层的动画、滚动的物理在 [`Compositor`] 里，可以放在另一条线程上独立推进（把 [`LayerUpdates`] 发给它）。
//! 6. **系统服务**：剪贴板、打开网址、平台的惯例用 [`set_system_backend`] 装上；
//!    系统外观变了调用 [`set_system_appearance`] 这几个函数。
//!
//! ```
//! use aureole::host::{self, DragOutRequest, HostWindow, RenderFrame, TreeUpdate, WindowHost, WindowTree};
//! use aureole::prelude::*;
//! use aureole::{Cursor, ImeRequest, Scope};
//!
//! /// 窗口的另一半：这里只记有没有收到画面
//! struct MyWindow;
//!
//! impl HostWindow for MyWindow {
//!     fn scale_factor(&self) -> f32 {
//!         2.0
//!     }
//!     fn submit(&self, _now: f64, frame: Option<RenderFrame>) {
//!         if let Some(frame) = frame {
//!             let _quads = frame.scene.map_or(0, |scene| scene.list.quads.len());
//!         }
//!     }
//!     fn set_ime(&self, _: Option<ImeRequest>) {}
//!     fn set_cursor(&self, _: Cursor) {}
//!     fn accessibility(&self, _: TreeUpdate) {}
//!     fn begin_drag_out(&self, request: DragOutRequest) {
//!         // 这个窗口拖不到外面：直接以"没被接住"结束
//!         request.result.set(Some(false));
//!     }
//!     fn focus_changed(&self, _: bool) {}
//! }
//!
//! let scope = Scope::new();
//! let tree = WindowTree::new([800.0, 600.0]);
//! scope.run(|| {
//!     tree.attach_metrics(None);
//!     tree.mount(column().padding(24).child(text("你好")).into_element());
//! });
//! let host = WindowHost::new(tree, MyWindow);
//! host::run_ready_tasks();
//! host::run_due_timers();
//! let report = host.frame(host::clock::now());
//! assert!(report.stats.laid_out);
//! # scope.dispose();
//! ```
//!
//! 宿主用的时刻（[`WindowHost::frame`] 的 `now`、[`clock::now`]）是统一时钟的秒数（`f64`），不是 [`std::time::Duration`]：
//! 帧时刻要能精确累加，和每帧求值的动画接口同一种写法。

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Instant;

use scene::{AtlasUpdate, DisplayList};

use crate::element::Element;
use crate::geometry::Rect;
use crate::input::{ImeRequest, InputEvent, InputKind, PointerInput, PointerKind};
use crate::interaction::Cursor;
use crate::kinds::NextFrame;
use crate::tree::{self, FrameStats, Tree, TreeRef};

pub use accesskit::{ActionRequest, TreeUpdate};

pub use crate::compositor::{Compositor, Handback, LayerInfo, LayerUpdates};
pub use crate::performance::record_render;
pub use crate::recording::Recorder;
pub use crate::system::{SystemBackend, set_motion_reduced, set_system_accent_color, set_system_appearance, set_system_backend, set_thread_system_backend};
pub use crate::task::{MainThreadWaker, main_thread_waker, run_ready_tasks, set_main_thread_waker};
pub use crate::timer::run_due_timers;
pub use crate::tree::{DragOutRequest, ScenePatch};

/// 统一的时钟：定时器、`sleep`、帧时间、输入事件的时间戳都读它，所以几样东西之间的先后、间隔对得上。
/// 从进程里第一次读它的时刻算起；测试工具里是虚拟时间
pub mod clock {
    pub use crate::timer::{now, now_duration, origin, seconds, time_at};
}

/// 窗口在核心里的身份：运行时的窗口句柄里包着它（测试工具没有窗口）。键盘监听、快捷键、按着的键、
/// 窗口大小的信号、主线程上的任务按它分窗口
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

thread_local! {
    /// 正在构建视图、处理事件或推进帧的窗口
    static CURRENT_WINDOW: Cell<Option<WindowId>> = const { Cell::new(None) };
}

/// 正在构建视图、处理事件或推进帧的窗口；不在窗口里（测试工具、窗口之外）时是 `None`
pub fn current_window_id() -> Option<WindowId> {
    CURRENT_WINDOW.with(Cell::get)
}

/// 在 `f` 期间把 `window` 当作当前窗口：宿主处理一个窗口的事件、推进它的帧时，主线程上的任务在创建时的窗口里继续时。
/// `f` panic 了也恢复原来的当前窗口（守卫在展开时恢复）
pub fn run_in_window<R>(window: WindowId, f: impl FnOnce() -> R) -> R {
    /// 离开时（正常返回或者 panic 展开）恢复原来的当前窗口
    struct Restore(Option<WindowId>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = CURRENT_WINDOW.try_with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(CURRENT_WINDOW.with(|c| c.replace(Some(window))));
    f()
}

/// 连续这么多帧什么都没做就暂停显示链接（测试工具的 `idle` 按同样的条件停下）
pub const IDLE_FRAMES: u32 = 8;

/// 窗口的另一半：真实的窗口（`app.rs`）或者测试工具里没有窗口的那一个（`testing.rs`）。
/// 方法都在主线程上、元素树没被借用时调用：系统可能在调用里同步送回事件（比如关输入法时马上送来
/// 输入法关闭），事件又要借元素树。所以先把要交的值读出来、放开借用，再调用窗口
pub trait HostWindow {
    /// 窗口此刻的缩放（逻辑像素 -> 物理像素）
    fn scale_factor(&self) -> f32;
    /// 这一帧推进完了：`frame` 是要交给渲染线程的（显示列表、合成层的变化、图集）；画面静止时是 `None`，
    /// 不打扰渲染线程。`now` 是这一帧的时间
    fn submit(&self, now: f64, frame: Option<RenderFrame>);
    /// 文字输入：焦点上的元素要文字输入时是插入点的位置（候选框跟着它）和用途，没有时为 `None`。
    /// 用途是密码时不开输入法，窗口在前面时请求系统的安全输入。每帧调用，自己去重
    fn set_ime(&self, ime: Option<ImeRequest>);
    /// 放弃了组字（焦点换走、在别处按下、禁用、窗口失焦）：平台的输入法重置，丢掉它那边没确定的字
    /// （关一次再开一次）。在处理完一次输入、推进完一帧之后调用，平台在调用里同步送回的输入法事件会排队
    fn reset_ime(&self) {}
    /// 指针形状。每次输入之后、重新布局或重画之后调用，自己去重
    fn set_cursor(&self, cursor: Cursor);
    /// 无障碍树的增量更新（只在辅助技术打开时有）
    fn accessibility(&self, update: TreeUpdate);
    /// 元素要拖到应用外面：交给系统的拖动会话，结束时把结果写进请求带的格子
    fn begin_drag_out(&self, request: DragOutRequest);
    /// 窗口得到、失去键盘焦点（元素树处理之前）
    fn focus_changed(&self, focused: bool);
    /// 指针按在窗口的拖动区域（`window_drag_area`）上拖动了：交给系统拖动窗口。这次按下在元素树这边已经结束了
    /// （系统的移动循环收走松开）。不会拖窗口的宿主不用实现
    fn begin_window_drag(&self) {}
    /// 指针双击了窗口的拖动区域：按系统设置缩放或最小化窗口。不会缩放窗口的宿主不用实现
    fn perform_title_bar_double_click(&self) {}
    /// 应用自己画的最大化按钮（`window_maximize_button`）的范围变了，窗口坐标；元素卸载了时是 `None`。
    /// 系统没有这回事的宿主不用实现
    fn set_maximize_button_area(&self, area: Option<Rect>) {
        let _ = area;
    }
}

/// 显示列表和其中的合成层
pub struct Scene {
    /// 显示列表
    pub list: Arc<DisplayList>,
    /// 显示列表里的合成层（层号从 1 开始）
    pub layers: Arc<Vec<LayerInfo>>,
    /// 只原地修补了几段图元时是改了的几段：结构、图元个数、合成层表都和上一份一样（渲染线程只上传这几段，不换层表）。
    /// 整份新的时是 `None`
    pub patch: Option<crate::tree::ScenePatch>,
}

/// 一帧要交给渲染线程的
pub struct RenderFrame {
    /// 新的显示列表和合成层；显示列表没变时为 `None`
    pub scene: Option<Scene>,
    /// 合成层的变化（动画的新状态、卸载的层、改过的滚动状态、这一帧的时间）
    pub layers: Box<LayerUpdates>,
    /// 这一帧新放进图集的内容，绘制前上传
    pub atlas: Vec<AtlasUpdate>,
}

/// 一帧推进的结果
#[derive(Clone, Copy, Debug)]
pub struct FrameReport {
    /// 这一帧做了什么（布局、绘制、耗时）
    pub stats: FrameStats,
    /// 这一帧做了事，或者还要每帧推进（连续几帧没事时窗口暂停显示链接）
    pub busy: bool,
    /// 接下来要什么时候的帧（元素树的 `next_frame`）：`At(时刻)` 时窗口暂停显示链接睡到那个时刻（统一时钟的读数），到点出一帧
    pub next: NextFrame,
}

/// 一个窗口的元素树的句柄，复制的是句柄。元素树在主线程上：句柄不能发到别的线程。
///
/// 窗口的另一半（[`HostWindow`]）往往要先拿到 [`WindowTree::handback`] 才建得出来，所以先建树、挂载，再建 [`WindowHost`]。
/// 建好以后输入和帧都走 [`WindowHost`]，这里剩下窗口事件里用得到的几样
#[derive(Clone)]
pub struct WindowTree(pub(crate) TreeRef);

impl WindowTree {
    /// 新建一棵空的元素树，窗口内容区的大小是 `size`（逻辑像素）
    pub fn new(size: [f32; 2]) -> Self {
        Self(Rc::new(RefCell::new(Tree::new(size[0], size[1]))))
    }

    /// 窗口的缩放（逻辑像素 -> 物理像素）。[`WindowHost::new`] 也按窗口设一次；挂载之前就知道时先设，文字不用量两遍
    pub fn set_scale(&self, scale: f32) {
        self.0.borrow_mut().set_scale(scale);
    }

    /// 无障碍树根节点的名字（读屏软件念的窗口标题）
    pub fn set_accessibility_title(&self, title: impl Into<String>) {
        self.0.borrow_mut().accessibility_set_title(title.into());
    }

    /// 在当前作用域（窗口自己的）里建好窗口大小的信号，提供给里面的绑定（[`crate::window_size_class`] 等）。挂载之前调用。
    /// `window` 是窗口在核心里的身份：键盘监听、快捷键、按着的键按它分窗口；只有一个窗口、不区分时可以是 `None`
    pub fn attach_metrics(&self, window: Option<WindowId>) {
        tree::attach_metrics(&self.0, window);
    }

    /// 隐藏了的标题栏的度量（[`crate::title_bar_info`]）：窗口缩放、进出全屏时宿主告诉界面。[`Self::attach_metrics`] 之后才有效
    pub fn set_title_bar_info(&self, info: crate::responsive::TitleBarInfo) {
        let metrics = self.0.borrow().window_metrics();
        if let Some(metrics) = metrics {
            metrics.set_title_bar(info);
        }
    }

    /// 挂载根元素，绑定建立在当前作用域里（释放作用域时一起释放）。根元素没指定高度和 `grow` 时默认占满窗口
    pub fn mount(&self, root: Element) {
        tree::mount_root(&self.0, root);
    }

    /// 合成器（另一条线程上的 [`Compositor`]）交回滚动状态的地方，建合成器时给它
    pub fn handback(&self) -> Arc<Handback> {
        self.0.borrow().handback()
    }

    /// 指针最后已知的位置（窗口坐标）；指针不在窗口里时是 `None`
    pub fn pointer(&self) -> Option<[f32; 2]> {
        self.0.borrow().pointer()
    }

    /// 最后一次问拖来的内容时的回答：有元素接受了是它说的操作，没有是 `None`。系统的拖放流程按它显示光标
    pub fn external_drop_effect(&self) -> Option<crate::event::ExternalDropEffect> {
        self.0.borrow().external_drop_effect()
    }

    /// 最后一次问拖来的内容时，指针下有元素关心它（有拖放的处理器，接不接受另说）。
    /// 拖文件时没有元素关心就照旧显示能放下的光标（放下了也没人收）
    pub fn is_external_drag_claimed(&self) -> bool {
        self.0.borrow().is_external_drag_claimed()
    }

    /// 窗口被完全遮住、最小化：动图停下、不解后面的帧（画面看不见）；露出来时接着放
    pub fn set_occluded(&self, occluded: bool) {
        self.0.borrow_mut().set_occluded(occluded);
    }

    /// 取走还没交出去的图集更新（渲染器换了设备时丢掉它们，改交 [`WindowTree::atlas_snapshot`]）
    pub fn take_atlas_updates(&self) -> Vec<AtlasUpdate> {
        self.0.borrow_mut().take_atlas_updates()
    }

    /// 图集和图片的完整内容：渲染器换了 GPU 设备、纹理都没了时重新上传
    pub fn atlas_snapshot(&self) -> Vec<AtlasUpdate> {
        self.0.borrow().atlas_snapshot()
    }

    /// 现在的显示列表
    pub fn display_list(&self) -> Arc<DisplayList> {
        self.0.borrow().display_list()
    }

    /// 主线程卡住之后、处理积压的输入之前：收回合成器推进到的滚动状态，命中测试按屏幕上的偏移。
    /// [`WindowHost::input`] 每次都做，一般不用自己调用
    pub fn adopt_scrolls(&self) {
        tree::adopt_scrolls(&self.0);
    }

    /// 辅助技术第一次来问：开始维护无障碍树，返回完整的一份。之后的增量由 [`HostWindow::accessibility`] 交出
    pub fn activate_accessibility(&self) -> TreeUpdate {
        self.0.borrow_mut().accessibility_activate()
    }

    /// 辅助技术走了：停止维护无障碍树
    pub fn deactivate_accessibility(&self) {
        self.0.borrow_mut().accessibility_deactivate();
    }

    /// 辅助技术请求一个动作（按下、设焦点、滚动、改值……）
    pub fn perform_accessibility_action(&self, request: ActionRequest) {
        tree::accessibility_action(&self.0, request);
    }

    /// 拖到应用外面结束了：`accepted` 是有没有被接住，立刻交给源元素的 `on_drag_out_end`。
    /// 系统的回调里不方便碰元素树时，写进 [`DragOutRequest::result`]，下一帧交给源元素
    pub fn finish_drag_out(&self, accepted: bool) {
        tree::finish_out(&self.0, accepted);
    }

    /// 不保活的句柄：放进这棵树自己的绑定里时用，不让它们互相拉住
    pub fn downgrade(&self) -> WeakWindowTree {
        WeakWindowTree(Rc::downgrade(&self.0))
    }
}

/// [`WindowTree::downgrade`] 得到的不保活的句柄
#[derive(Clone)]
pub struct WeakWindowTree(Weak<RefCell<Tree>>);

impl WeakWindowTree {
    /// 树还在时取回句柄
    pub fn upgrade(&self) -> Option<WindowTree> {
        self.0.upgrade().map(WindowTree)
    }
}

/// 一个窗口：元素树 + 窗口的另一半，见模块说明
pub struct WindowHost<W> {
    tree: WindowTree,
    window: W,
    /// 录下经过的每一次输入（真实窗口的 `AUREOLE_RECORD`、测试工具的 `record`）
    recorder: RefCell<Option<Recorder>>,
    /// 正在处理一次输入：这期间同步进来的输入排队（见 [`WindowHost::input`]）
    dispatching: Cell<bool>,
    pending: RefCell<std::collections::VecDeque<InputEvent>>,
    /// 主指针是一根手指：这期间不理会鼠标的移动和按键（系统把触摸转成的鼠标、真的鼠标都不该打断这根手指）
    primary_finger: Cell<bool>,
}

/// 处理输入期间立着的标记：处理完（或者处理器 panic 了）都放倒，之后的输入不会一直排队
struct Dispatching<'a>(&'a Cell<bool>);

impl Drop for Dispatching<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl<W: HostWindow> WindowHost<W> {
    /// 元素树按窗口的缩放栅格化文字
    pub fn new(tree: WindowTree, window: W) -> Self {
        tree.set_scale(window.scale_factor());
        Self { tree, window, recorder: RefCell::new(None), dispatching: Cell::new(false), pending: RefCell::default(), primary_finger: Cell::new(false) }
    }

    /// 这个窗口的元素树
    pub fn tree(&self) -> &WindowTree {
        &self.tree
    }

    /// 窗口的另一半
    pub fn window(&self) -> &W {
        &self.window
    }

    /// 之后的输入都录下来（换掉之前的录制，不写结束标记）
    pub fn record(&self, recorder: Recorder) {
        self.recorder.replace(Some(recorder));
    }

    /// 结束录制，写上结束的时间；返回录制文件和一共录了几次输入，没在录时为 `None`
    pub fn finish_recording(&self, time: f64) -> Option<(std::path::PathBuf, usize)> {
        let recorder = self.recorder.borrow_mut().take()?;
        let path = recorder.path().to_owned();
        Some((path, recorder.finish(time)))
    }

    /// 一次输入。真实窗口的事件（换算之后）、测试工具、回放都从这里进；返回按键有没有被处理。
    ///
    /// 重入：处理这次输入的时候又同步进来了输入（系统在调用里同步派发事件——模态对话框的嵌套事件循环、输入法），
    /// 后来的排队，等这一次处理完再按到达的顺序处理，返回 `false`。
    /// 所以一个处理器总是看到一次完整的分发，不会在半路被另一次输入打断
    pub fn input(&self, event: InputEvent) -> bool {
        if let Some(recorder) = self.recorder.borrow_mut().as_mut() {
            recorder.write(&event);
        }
        if self.dispatching.replace(true) {
            self.pending.borrow_mut().push_back(event);
            return false;
        }
        let _guard = Dispatching(&self.dispatching);
        let handled = self.handle(event);
        loop {
            let next = self.pending.borrow_mut().pop_front();
            let Some(next) = next else { break };
            self.handle(next);
        }
        handled
    }

    /// 处理一次输入（不重入）
    fn handle(&self, event: InputEvent) -> bool {
        let tree = &self.tree.0;
        // 主线程刚卡过：积压的输入按屏幕上（渲染线程推进到）的滚动偏移命中
        tree::adopt_scrolls(tree);
        let handled = match event.kind {
            // 手指（位置总是有的，没有时没法处理）
            InputKind::PointerMove(PointerInput { device: PointerKind::Touch, pointer_id, position, .. }) => {
                if let Some(point) = position {
                    tree::touch_move(tree, pointer_id, point);
                    // 手指可能被系统拿走了（拖窗口）：之后等不到它抬起
                    self.note_primary_finger();
                }
                false
            }
            InputKind::PointerDown(PointerInput { device: PointerKind::Touch, pointer_id, position, .. }) => {
                if let Some(point) = position {
                    tree::touch_down(tree, pointer_id, point);
                    self.note_primary_finger();
                }
                false
            }
            InputKind::PointerUp(PointerInput { device: PointerKind::Touch, pointer_id, position, .. }) => {
                tree::touch_up(tree, pointer_id, position);
                self.note_primary_finger();
                false
            }
            InputKind::PointerCancel(PointerInput { device: PointerKind::Touch, pointer_id, .. }) => {
                tree::touch_cancel(tree, pointer_id);
                self.note_primary_finger();
                false
            }
            InputKind::PointerMove(p) => {
                if !self.primary_finger.get() {
                    tree::pointer_move(tree, p.position);
                }
                false
            }
            InputKind::PointerLeave(_) => {
                if !self.primary_finger.get() {
                    tree::pointer_move(tree, None);
                }
                false
            }
            // 进入之后紧跟着一次移动
            InputKind::PointerEnter(_) => false,
            InputKind::PointerDown(PointerInput { button: Some(button), .. }) => {
                if !self.primary_finger.get() {
                    tree::mouse_button(tree, button, true);
                }
                false
            }
            InputKind::PointerUp(PointerInput { button: Some(button), .. }) => {
                if !self.primary_finger.get() {
                    tree::mouse_button(tree, button, false);
                }
                false
            }
            InputKind::PointerDown(_) | InputKind::PointerUp(_) => false,
            InputKind::PointerCancel(_) => {
                tree::cancel_pointer(tree);
                self.note_primary_finger();
                false
            }
            InputKind::Wheel(w) => {
                tree::wheel(tree, w.delta, w.phase, w.modifiers);
                false
            }
            InputKind::Pinch(g) => {
                tree::pinch(tree, g.delta, g.phase);
                false
            }
            InputKind::Rotate(g) => {
                tree::rotate(tree, g.delta, g.phase);
                false
            }
            InputKind::Modifiers(modifiers) => {
                tree::set_modifiers(tree, modifiers);
                false
            }
            InputKind::KeyDown(key) => tree::key_down(tree, &key),
            InputKind::KeyUp(key) => tree::key_up(tree, &key),
            InputKind::Ime(ime) => {
                tree::ime(tree, &ime);
                false
            }
            InputKind::WindowFocus(focused) => {
                self.window.focus_changed(focused);
                // 切到别的应用时可能收不到松开：进行中的拖动和手势取消，按着的键松开，正在组的字放弃
                if !focused {
                    tree::cancel_pointer(tree);
                    self.note_primary_finger();
                    tree::release_keys(tree);
                    tree::end_composition(tree);
                }
                false
            }
            InputKind::FileHover(point) => {
                tree::file_hover(tree, point);
                false
            }
            InputKind::FileDrop { position, paths } => {
                tree::file_drop(tree, position, paths);
                false
            }
            InputKind::FileCancel => {
                tree::file_hover(tree, None);
                false
            }
            InputKind::ExternalDragOver { position, kinds } => {
                tree::external_drag_over(tree, position, kinds);
                false
            }
            InputKind::ExternalDragLeave => {
                tree::external_drag_leave(tree);
                false
            }
            InputKind::ExternalDrop { position, data } => {
                tree::external_drop(tree, position, data);
                false
            }
            InputKind::Resize { size, scale_factor: scale } => {
                let mut t = tree.borrow_mut();
                t.set_viewport(size[0], size[1]);
                t.set_scale(scale);
                false
            }
        };
        // 拖到应用外面、拖动窗口：还在处理这次指针事件时交给系统
        self.drag_out();
        self.window_requests();
        self.reset_ime();
        let cursor = self.tree.0.borrow().cursor();
        self.window.set_cursor(cursor);
        handled
    }

    /// 手指放上、抬起之后：主指针是不是手指
    fn note_primary_finger(&self) {
        self.primary_finger.set(self.tree.0.borrow().primary_is_finger());
    }

    /// 放弃了组字：平台的输入法重置（先取走标记、放开借用，平台可能在调用里同步送回输入法事件）
    fn reset_ime(&self) {
        let reset = self.tree.0.borrow_mut().take_ime_reset();
        if reset {
            self.window.reset_ime();
        }
    }

    /// 推进到时间 `now`（秒，显示链接给的预计上屏时间，测试工具里是虚拟时间）：执行后台线程投递过来的工作
    /// （解码好的图片、重新读好的文件），推进滚动和动画，只做被标记的布局和绘制工作，交给窗口；然后是无障碍、输入法、指针形状
    pub fn frame(&self, now: f64) -> FrameReport {
        let start = Instant::now();
        // 后台线程交回的结果：在借用元素树之前收下，绑定它们的元素随即更新
        crate::task::run_posted();
        let stats = tree::advance(&self.tree.0, now);
        let mut t = self.tree.0.borrow_mut();
        let atlas = t.take_atlas_updates();
        // 画面静止（没有新的图元、图集、合成层变化，也没有在动的东西）：不打扰渲染线程，让它一直睡着
        let send = stats.repainted || !atlas.is_empty() || !t.render_idle();
        let frame = send.then(|| RenderFrame {
            scene: t.take_scene(),
            layers: Box::new(t.take_layer_updates()),
            atlas,
        });
        drop(t);
        self.window.submit(now, frame);
        // 无障碍：只把变了的节点交给窗口（没有辅助技术时 `accessibility_update` 直接返回）
        let update = self.tree.0.borrow_mut().accessibility_update();
        if let Some(update) = update {
            self.window.accessibility(update);
        }
        self.reset_ime();
        let ime = self.tree.0.borrow().ime_request();
        self.window.set_ime(ime);
        // 重新布局、重画后指针下的元素可能变了；指针形状的绑定变了
        let cursor_dirty = self.tree.0.borrow_mut().take_cursor_dirty();
        if stats.laid_out || stats.repainted || cursor_dirty {
            let cursor = self.tree.0.borrow().cursor();
            self.window.set_cursor(cursor);
        }
        self.drag_out();
        // 应用自己画的最大化按钮挪了地方（没有这个元素时只多一次判断）
        let area = self.tree.0.borrow_mut().take_maximize_button_area();
        if let Some(area) = area {
            self.window.set_maximize_button_area(area);
        }
        crate::performance::record_main(start);
        let next = self.tree.0.borrow().next_frame();
        let busy = send || stats.laid_out || next == NextFrame::Continuous;
        FrameReport { stats, busy, next }
    }

    /// 元素要拖到应用外面：交给窗口
    fn drag_out(&self) {
        let request = self.tree.0.borrow_mut().take_drag_out();
        if let Some(request) = request {
            self.window.begin_drag_out(request);
        }
    }

    /// 窗口的拖动区域：双击、开始拖动交给窗口（系统的移动循环可能不马上返回，先放开元素树的借用）
    fn window_requests(&self) {
        let (double_click, drag) = self.tree.0.borrow_mut().take_window_requests();
        if double_click {
            self.window.perform_title_bar_double_click();
        }
        if drag {
            self.window.begin_window_drag();
        }
    }
}
