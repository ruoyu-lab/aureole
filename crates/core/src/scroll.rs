//! 滚动容器：触控板跟手（接系统的惯性）、滚轮平滑、边缘橡皮筋回弹、叠加式滚动条、程序化滚动。
//!
//! ```no_run
//! # use aureole::prelude::*;
//! let feed = ScrollHandle::new();
//! column()
//!     .child(button("回到顶部").on_click(move || feed.scroll_to(0.0, 0.0)))
//!     .child(text(move || format!("滚了 {:.0}px", feed.offset_y())))
//!     .child(scroll_y(column().gap(8).children((0..100).map(|i| text(format!("第 {i} 行"))))).grow(1).scroll_handle(feed));
//! ```
//!
//! 视口（`scroll_y` 等返回的元素）把内容裁剪在自己的圆角矩形里；内容是一个合成层，
//! 滚动偏移是这一层的额外平移：滚动时不重新布局、不重建图元。
//! 滚动的物理（[`Physics`]：惯性、弹簧、橡皮筋、滚动条的显隐）主线程和渲染线程各推进一份：
//! 渲染线程那份上屏，主线程卡住时惯性和弹簧照样走；主线程那份用来挂载虚拟列表的行、命中测试。
//! 这里只有纯数据和数学，和元素树打交道的部分在 `tree/scrolling.rs`，渲染线程那一侧在 `compositor.rs`。

use std::cell::RefCell;
use std::f32::consts::{E, TAU};

use animation::{TrackState, Track, Tween, TweenTrack};
use reactive::{Signal, signal};
use scene::Color;

use crate::compositor::DerivedLayer;
use crate::element::{Element, View};
use crate::geometry::Rect;
use crate::layout::Direction;
use crate::tree::NodeId;

/// 一"行"的距离，逻辑像素：方向键滚一下、翻页留下的重叠（[`crate::Conventions::wheel_line_height`]）。macOS 的滚轮事件自带加速
/// （转得快时一格有好几行），一行短一些；其他平台一格就是一行，按常见的三行文字算
pub(crate) fn line() -> f32 {
    crate::system::conventions().wheel_line_height
}

/// 翻一页滚多远：视口的长度留一行重叠，至少一行（翻页键、系统设置成"一次滚动一个屏幕"的滚轮）
pub(crate) fn page(viewport: f32) -> f32 {
    (viewport - line()).max(line())
}

/// 橡皮筋系数：拉出去的距离按 `x·d·c / (d + c·x)` 显示，越拉越沉，最多接近一个视口
const RUBBER: f32 = 0.55;
/// 回弹、滚轮平滑、程序化滚动的弹簧响应时间（秒），都是临界阻尼，不来回晃
const BOUNCE: f32 = 0.32;
const WHEEL: f32 = 0.13;
const SCROLL_TO: f32 = 0.45;
/// 程序化滚动的距离超过这么多屏时，从离目标这么多屏的地方开始滑过去
const FAR: f32 = 2.0;
/// 手指抬起后这么久之内开始的触控板事件是系统的惯性，不是新的一次触摸
pub(crate) const MOMENTUM_WINDOW: f64 = 0.1;
/// 滚轮（和没有阶段的精确位移）在这段时间内锁定同一个视口，不会滚到一半换成外层
pub(crate) const WHEEL_LATCH: f64 = 0.3;
/// 用最近几帧的输入位移估计速度
const SAMPLES: usize = 4;

/// 滚动条：粗细、离边缘的距离、最短长度
pub(crate) const BAR_THICKNESS: f32 = 6.0;
pub(crate) const BAR_INSET: f32 = 2.0;
const BAR_MIN: f32 = 28.0;
/// 停下多久之后滚动条淡出
pub(crate) const BAR_HIDE_DELAY: f64 = 0.8;
/// 指针离边缘这么近时显示滚动条（可以拖）
pub(crate) const BAR_HOVER: f32 = 14.0;
const BAR_FADE: Tween = Tween::new(std::time::Duration::from_millis(180)).ease_out();

/// 只在 x 方向滚动的视口（横向轮播）。高度跟着内容；宽度不跟着内容，同 [`scroll_y`] 的高度
/// （竖排里默认拉伸到竖排的宽度，一般不用管）
pub fn scroll_x(content: impl View) -> Element {
    viewport(content, [true, false])
}

/// 只在 y 方向滚动的视口（列表、页面）。
///
/// **视口的高度不跟着内容**：用 `.height(..)`、`.grow(1)`、`.full_height(true)` 给它高度，或者放在会拉伸它的地方
/// （横排默认把子元素拉伸到横排的高度）。什么都没给时高度是 0、看不到内容，调试构建会记一条警告。
/// 这是故意的：很长的内容（比如十万行的虚拟列表）不会把外面的容器一路撑高，也不会因此挂载所有的行。
/// 要"跟着内容长高，长到头再滚动"（下拉菜单、建议列表）用 [`scroll_y_fit`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// column()
///     .height(480)
///     .child(text("标题"))
///     .child(scroll_y(column().gap(8).children((0..100).map(|i| text(format!("第 {i} 行"))))).grow(1)); // 占满剩下的高度
/// ```
pub fn scroll_y(content: impl View) -> Element {
    viewport(content, [false, true])
}

/// 跟着内容长高的竖向滚动视口：内容少时和内容一样高，长到 `max_height` 为止（父元素的高度固定时，
/// 也不会超出父元素；和兄弟一起按比例收缩，不想被挤的兄弟设 `shrink(0)`），再多就在里面滚动。
/// 下拉菜单的选项列表、多行输入框都是这样。
///
/// 和 [`scroll_y`] 不同，内容的高度会算进视口和外面容器的大小，所以要给一个上限：`.max_height(..)`，
/// 或者放在高度固定的容器里。很长的列表用 `scroll_y` 加 `grow` / `height`，或者 [`crate::virtual_list`]。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let results = (1..=30).map(|i| text(format!("结果 {i}")));
/// scroll_y_fit(column().gap(4).children(results)).max_height(240); // 不到 240 时和内容一样高
/// ```
pub fn scroll_y_fit(content: impl View) -> Element {
    let holder = Element::default().direction(Direction::Column);
    let mut element = Element::default().direction(Direction::Column).clip(true);
    element.scroll = Some(Box::new(ScrollOptions::new([false, true], holder, content.into_element())));
    element
}

/// 两个方向都能滚的视口（画布、表格）。内容的宽度不跟视口走，通常要给内容指定大小
pub fn scroll(content: impl View) -> Element {
    viewport(content, [true, true])
}

/// 视口：裁剪内容，内容在滚动方向上不被压缩。视口在滚动方向上的大小由 `height` / `grow` 等决定，
/// 不会被内容撑大（也不会把外面会长高的容器撑大）；不滚的方向跟着内容（横向轮播的高度就是卡片的高度）。
/// 内容的内边距和间距写在内容上（跟着滚），写在视口上就是不滚的边框。
///
/// 内容外面包一层"托架"：它在滚动方向上的大小是 0，内容从它里面溢出去，
/// 这样布局时内容的长度不会算进视口和外层容器的最小尺寸
fn viewport(content: impl View, axes: [bool; 2]) -> Element {
    let direction = if axes == [true, false] { Direction::Row } else { Direction::Column };
    let mut holder = Element::default().direction(direction);
    if axes[0] {
        holder = holder.width(0);
    }
    if axes[1] {
        holder = holder.height(0);
    }
    let mut element = Element::default().direction(direction).clip(true);
    element.scroll = Some(Box::new(ScrollOptions::new(axes, holder, content.into_element())));
    element
}

/// 挂载前的滚动设置。视口"视口 → 托架 → 内容"三层：托架和内容不放进视口的子元素，由这里直接拿着，
/// 挂载时作为视口的第一个子元素挂上，视口记住它们是哪两个节点（不靠位置找）
pub(crate) struct ScrollOptions {
    pub axes: [bool; 2],
    pub handle: Option<ScrollHandle>,
    /// 浮层的表面（见 `overlay::scrollable`）：内容放得下时不接滚轮；放得下时也裁剪吗（元素自己要求裁剪）
    pub surface: Option<bool>,
    /// 托架：滚动方向上大小是 0，内容从它里面溢出去（还没有子元素，挂载时把内容挂上）
    pub holder: Box<Element>,
    /// 跟着滚的内容
    pub content: Box<Element>,
}

impl ScrollOptions {
    pub(crate) fn new(axes: [bool; 2], holder: Element, content: Element) -> Self {
        Self { axes, handle: None, surface: None, holder: Box::new(holder), content: Box::new(content) }
    }
}

impl Element {
    /// 用句柄读取和控制这个滚动视口（只对 [`scroll_x`]、[`scroll_y`]、[`scroll_y_fit`]、[`scroll`]、[`crate::virtual_list`]
    /// 和自己带滚动的 [`crate::text_area`] 有效）
    pub fn scroll_handle(mut self, handle: ScrollHandle) -> Self {
        if let Some(config) = &mut self.scroll {
            config.handle = Some(handle);
        } else if let Some(editor) = self.kind_mut::<crate::editor::EditorBlock>().filter(|e| e.multiline()) {
            editor.scroll_handle = Some(handle);
        }
        self
    }
}

/// 滚动视口的句柄：读当前偏移（响应式，滚动时每帧更新）、滚到某处。和信号一样是 `Copy` 的，
/// 先创建，再用 [`Element::scroll_handle`] 交给视口；视口挂载前发出的滚动请求在第一次布局后执行。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let list = ScrollHandle::new();
/// // 滚动联动：标题栏随滚动渐隐
/// text("标题").opacity(move || 1.0 - (list.offset_y() / 120.0).clamp(0.0, 1.0));
/// list.scroll_to(0.0, 0.0); // 平滑回到顶部
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollHandle {
    offset: Signal<[f32; 2]>,
    metrics: Signal<Metrics>,
    requests: Signal<RefCell<Vec<Request>>>,
    /// 惯性、弹簧还要走多远（带方向，最多一个视口）：虚拟列表往运动方向多挂这么多行，
    /// 主线程卡住时渲染线程接着滑，也滑在挂好的行上
    lead: Signal<[f32; 2]>,
}

/// 视口和内容的大小，布局之后更新
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Metrics {
    /// 视口去掉内边距和边框后的大小
    pub viewport: [f32; 2],
    pub content: [f32; 2],
}

impl Metrics {
    fn max(&self) -> [f32; 2] {
        [(self.content[0] - self.viewport[0]).max(0.0), (self.content[1] - self.viewport[1]).max(0.0)]
    }
}

/// 程序化滚动
#[derive(Clone, Copy, Debug)]
pub(crate) enum Request {
    To { target: [f32; 2], animated: bool },
    By([f32; 2]),
    /// 竖向：第几行滚到顶上
    ToIndex { index: usize, animated: bool },
}

/// 元素的句柄：在别处（快捷键、按钮、数据变了之后）把这个元素滚进视口。
/// 和信号一样是 `Copy` 的；先创建，再用 [`Element::element_handle`] 交给元素。请求在下一次布局之后执行：
/// 刚加进来、刚改过的元素也按新的位置滚；元素还没挂载时，等它挂载后的那次布局。
/// 虚拟列表里还没挂载的行用 [`ScrollHandle::scroll_to_index`]
///
/// ```no_run
/// # use aureole::prelude::*;
/// let latest = ElementHandle::new();
/// let log = column().child(text("最新的一条").element_handle(latest));
/// scroll_y(log).height(300);
/// latest.scroll_into_view();
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementHandle {
    pub(crate) request: Signal<RevealRequest>,
    /// 已经执行过的请求编号：元素重新挂载时不再执行旧请求
    pub(crate) handled: Signal<u64>,
}

/// 句柄发出的请求：编号递增，只执行比 `handled` 新的请求（挂载前发出的在挂载后执行）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RevealRequest {
    pub serial: u64,
    pub animated: bool,
}

impl ElementHandle {
    /// 新句柄，归当前作用域所有
    pub fn new() -> Self {
        Self { request: signal(RevealRequest::default()), handled: signal(0) }
    }

    /// 由内到外，每一层滚动视口平滑地滚到正好露出这个元素（四周留 8 逻辑像素）；已经露着的方向不动。
    /// 和键盘把焦点移到看不见的元素上时一样，只是不改焦点
    pub fn scroll_into_view(&self) {
        self.request.update(|r| *r = RevealRequest { serial: r.serial + 1, animated: true });
    }

    /// 立即跳到露出这个元素，见 [`Self::scroll_into_view`]
    pub fn jump_into_view(&self) {
        self.request.update(|r| *r = RevealRequest { serial: r.serial + 1, animated: false });
    }
}

impl Default for ElementHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Element {
    /// 用句柄在别处操作这个元素（[`ElementHandle::scroll_into_view`]）。任何元素都能挂，不改变它能不能获得焦点
    pub fn element_handle(mut self, handle: ElementHandle) -> Self {
        self.interaction().element_handle = Some(handle);
        self
    }
}

impl Default for ScrollHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollHandle {
    /// 句柄里的信号归当前作用域所有
    pub fn new() -> Self {
        Self {
            offset: signal([0.0; 2]),
            metrics: signal(Metrics::default()),
            requests: signal(RefCell::new(Vec::new())),
            lead: signal([0.0; 2]),
        }
    }

    /// 当前偏移 (x, y)，逻辑像素，按物理像素取整（和屏幕上内容的位置一致）。在绑定里读会订阅，
    /// 滚动时每帧更新。拉过边缘（橡皮筋）时会是负数或大于 [`Self::max_offset`]，可以用来做下拉刷新
    pub fn offset(&self) -> [f32; 2] {
        self.offset.get()
    }

    /// 当前横向偏移，同 [`Self::offset`] 的 x
    pub fn offset_x(&self) -> f32 {
        self.offset.with(|o| o[0])
    }

    /// 当前纵向偏移，同 [`Self::offset`] 的 y
    pub fn offset_y(&self) -> f32 {
        self.offset.with(|o| o[1])
    }

    /// 最大偏移：内容比视口长出来的部分
    pub fn max_offset(&self) -> [f32; 2] {
        self.metrics.with(Metrics::max)
    }

    /// 视口（去掉内边距和边框）的大小，布局后更新
    pub fn viewport_size(&self) -> [f32; 2] {
        self.metrics.with(|m| m.viewport)
    }

    /// 内容的大小，布局后更新
    pub fn content_size(&self) -> [f32; 2] {
        self.metrics.with(|m| m.content)
    }

    /// 平滑地滚到 (x, y)（弹簧，可被手势打断）；超出范围的部分夹到边缘，不能滚的方向忽略
    pub fn scroll_to(&self, x: f32, y: f32) {
        self.push(Request::To { target: [x, y], animated: true });
    }

    /// 立即跳到 (x, y)
    pub fn jump_to(&self, x: f32, y: f32) {
        self.push(Request::To { target: [x, y], animated: false });
    }

    /// 平滑地相对滚动（和滚轮一样，连续调用会累加）
    pub fn scroll_by(&self, dx: f32, dy: f32) {
        self.push(Request::By([dx, dy]));
    }

    /// 平滑地把第 `index` 行（虚拟列表的第几项；普通视口是内容的第几个子元素）滚到视口顶上，
    /// 超出范围时滚到末尾。行高量出来的虚拟列表里，目标行和中间的行挂载、量过之后目标会跟着修正，
    /// 停下时正好在那一行；之后行高再变（比如最后一行变长）也保持在那里，直到用户自己滚动。
    /// 所以 `scroll_to_index(最后一项)` 会一直贴着末尾（聊天记录）
    pub fn scroll_to_index(&self, index: usize) {
        self.push(Request::ToIndex { index, animated: true });
    }

    /// 立即跳到第 `index` 行，见 [`Self::scroll_to_index`]
    pub fn jump_to_index(&self, index: usize) {
        self.push(Request::ToIndex { index, animated: false });
    }

    fn push(&self, request: Request) {
        self.requests.update(|queue| queue.get_mut().push(request));
    }

    /// 取走排队的请求（订阅：有新请求时视口的副作用重新执行）
    pub(crate) fn take_requests(&self) -> Vec<Request> {
        self.requests.with(|queue| std::mem::take(&mut *queue.borrow_mut()))
    }

    pub(crate) fn offset_signal(&self) -> Signal<[f32; 2]> {
        self.offset
    }

    pub(crate) fn metrics_signal(&self) -> Signal<Metrics> {
        self.metrics
    }

    pub(crate) fn lead_signal(&self) -> Signal<[f32; 2]> {
        self.lead
    }

    /// 竖向的运动还要走多远（订阅），见 `lead`
    pub(crate) fn lead_y(&self) -> f32 {
        self.lead.with(|l| l[1])
    }
}

thread_local! {
    /// 正在挂载的滚动视口的句柄，从外到内。视口里的虚拟列表挂载时用它找到自己的视口
    static MOUNTING: RefCell<Vec<Option<ScrollHandle>>> = const { RefCell::new(Vec::new()) };
}

/// 挂载视口的子元素期间存在，离开时出栈
pub(crate) struct Mounting;

impl Mounting {
    pub fn enter(handle: Option<ScrollHandle>) -> Self {
        MOUNTING.with(|stack| stack.borrow_mut().push(handle));
        Self
    }
}

impl Drop for Mounting {
    fn drop(&mut self) {
        MOUNTING.with(|stack| stack.borrow_mut().pop());
    }
}

/// 最近一层正在挂载的视口的句柄
pub(crate) fn mounting() -> Option<ScrollHandle> {
    MOUNTING.with(|stack| stack.borrow().last().copied().flatten())
}

pub(crate) use crate::input::WheelDelta;

/// 滚动条的淡入淡出：补间轨道和最近一次采样的值
#[derive(Clone, Debug)]
struct Fade {
    track: TweenTrack,
    value: f32,
    moving: bool,
}

impl Default for Fade {
    fn default() -> Self {
        Self { track: TweenTrack::new(BAR_FADE, &[0.0]), value: 0.0, moving: false }
    }
}

impl Fade {
    fn target(&self) -> f32 {
        self.track.target()[0]
    }

    /// 从时刻 `now` 起改去 `target`（同一个目标不重新计时）
    fn retarget(&mut self, now: f64, target: f32) {
        if self.target() != target {
            self.track.retarget(now, &[target]);
            self.moving = true;
        }
    }

    /// 在时刻 `now` 采样
    fn sample(&mut self, now: f64) {
        let mut out = [0.0];
        self.moving = self.track.sample(now, &mut out) == TrackState::Moving;
        self.value = out[0];
    }
}

/// 一个方向上的运动状态
#[derive(Clone, Copy, Debug, PartialEq)]
enum ScrollPhase {
    Idle,
    /// 手指在触控板上：`raw` 是不加阻力的位置，超出边缘的部分按橡皮筋显示
    Touch { raw: f32 },
    /// 临界阻尼弹簧：滚轮平滑、回弹、程序化滚动。`x` 是相对目标的位移，单独保存：
    /// 偏移到几百万像素时 f32 只能精确到零点几像素，每帧从偏移倒推位移会卡在离目标一步的地方
    Spring { target: f32, omega: f32, x: f32 },
    /// 惯性：速度按指数衰减，撞到边缘时把当时的速度交给回弹弹簧
    Fling,
}

/// 惯性的摩擦（每秒的衰减率）：速度每毫秒乘 0.998，和系统的惯性一样。位移的解析解是 `v/k·(1 − e^(−kt))`，任意步长精确
const FRICTION: f32 = 2.002_003;
/// 惯性慢到这个速度（逻辑像素每秒）就停下
const FLING_STOP: f32 = 20.0;

/// 一个视口的滚动物理：偏移、运动（跟手、弹簧、惯性、橡皮筋）、滚动条的显隐。
///
/// 主线程和渲染线程各有一份，用同一段代码、在同一条时间线（主线程的帧时间）上推进，每帧的结果相同：
/// 主线程的那份用于布局、虚拟列表挂载、命中测试、写回句柄，渲染线程的那份上屏。主线程在帧与帧之间改了它
/// （输入、程序化滚动、布局后的范围）时把整份交给渲染线程；主线程卡住时渲染线程接着推进，
/// 恢复后主线程收回渲染线程推进到的状态（见 [`crate::compositor::Handback`]）
#[derive(Clone, Debug)]
pub(crate) struct Physics {
    pub axes: [bool; 2],
    /// 当前偏移，拉过边缘时超出 `[0, max]`
    pub offset: [f32; 2],
    pub max: [f32; 2],
    /// 视口的长度（去掉内边距和边框）：橡皮筋、冲出去的上限、很远的程序化滚动按它算
    pub view: [f32; 2],
    motion: [ScrollPhase; 2],
    /// 偏移变大为正，逻辑像素每秒：弹簧和惯性的速度
    velocity: [f32; 2],
    /// 窗口缩放：偏移按物理像素取整
    pub scale: f32,
    /// 滚动条在轨道上能走多远（轨道减去滚动条的长度）；这个方向没有滚动条时为 `None`
    pub travel: [Option<f32>; 2],
    /// 滚动条的透明度：按滚动的时间线（宿主时间，不跟动画时钟走）采样的补间
    fade: Fade,
    /// 滚动条看到的偏移（取整后），变了就显示滚动条
    seen: [f32; 2],
    /// 最近一次滚动（或指针靠近滚动条）的时间，过一会儿滚动条淡出
    pub active_at: f64,
    /// 正在拖滚动条：一直显示
    pub hold: bool,
}

impl Physics {
    pub fn new(axes: [bool; 2]) -> Self {
        Self {
            axes,
            offset: [0.0; 2],
            max: [0.0; 2],
            view: [0.0; 2],
            motion: [ScrollPhase::Idle; 2],
            velocity: [0.0; 2],
            scale: 1.0,
            travel: [None; 2],
            fade: Fade::default(),
            seen: [0.0; 2],
            active_at: 0.0,
            hold: false,
        }
    }

    fn clamp(&self, axis: usize, value: f32) -> f32 {
        value.clamp(0.0, self.max[axis])
    }

    /// 拉过边缘多少（带方向），没有拉过时为 0
    pub fn overscroll(&self, axis: usize) -> f32 {
        self.offset[axis] - self.clamp(axis, self.offset[axis])
    }

    /// 这个方向还能往 `direction` 的符号那边滚
    pub fn can_scroll(&self, axis: usize, direction: f32) -> bool {
        self.axes[axis]
            && if direction > 0.0 { self.offset[axis] < self.max[axis] - 0.5 } else { direction < 0.0 && self.offset[axis] > 0.5 }
    }

    /// 自己在动：弹簧、惯性，或者拉过了边缘还没弹回去
    pub fn moving(&self) -> bool {
        (0..2).any(|axis| matches!(self.motion[axis], ScrollPhase::Spring { .. } | ScrollPhase::Fling) || self.overscroll(axis) != 0.0)
    }

    /// 在动，或者被手指拉着
    pub fn busy(&self) -> bool {
        self.moving() || self.motion.iter().any(|m| matches!(m, ScrollPhase::Touch { .. }))
    }

    /// 还要每帧推进：在动，滚动条在淡入淡出、或者还显示着（等着淡出）
    pub fn needs_frames(&self) -> bool {
        self.moving() || self.fade.moving || self.fade.target() > 0.0
    }

    /// 按物理像素取整的偏移：内容层只做整像素的平移，文字保持清晰
    pub fn snapped(&self) -> [f32; 2] {
        [snap(self.offset[0], self.scale), snap(self.offset[1], self.scale)]
    }

    /// 滚动条沿轨道的位置（未取整）；这个方向没有滚动条时为 `None`
    pub fn thumb_position(&self, axis: usize) -> Option<f32> {
        let travel = self.travel[axis]?;
        let progress = if self.max[axis] > 0.0 { (self.offset[axis] / self.max[axis]).clamp(0.0, 1.0) } else { 0.0 };
        Some(travel * progress)
    }

    /// 这一段运动还要走多远（带方向）：惯性是剩下的滑行距离，弹簧是离目标还有多远；跟手、停着时为 0
    pub fn remaining(&self, axis: usize) -> f32 {
        match self.motion[axis] {
            ScrollPhase::Fling => self.velocity[axis] / FRICTION,
            ScrollPhase::Spring { target, .. } => target - self.offset[axis],
            ScrollPhase::Idle | ScrollPhase::Touch { .. } => 0.0,
        }
    }

    /// 新的滚动范围和视口长度：最大偏移变小时把停着的偏移夹回范围内（跟手的、在弹的、在滑的不动，交给它们自己收尾）
    pub fn set_bounds(&mut self, max: [f32; 2], view: [f32; 2]) {
        self.view = view;
        for (axis, max) in max.into_iter().enumerate() {
            self.max[axis] = if self.axes[axis] { max } else { 0.0 };
            match self.motion[axis] {
                ScrollPhase::Idle => self.offset[axis] = self.clamp(axis, self.offset[axis]),
                ScrollPhase::Spring { target, omega, x } => {
                    let clamped = self.clamp(axis, target);
                    self.motion[axis] = ScrollPhase::Spring { target: clamped, omega, x: x + target - clamped };
                }
                ScrollPhase::Touch { .. } | ScrollPhase::Fling => {}
            }
        }
    }

    fn spring(&mut self, axis: usize, target: f32, response: f32, velocity: f32) {
        self.motion[axis] = ScrollPhase::Spring { target, omega: TAU / response, x: self.offset[axis] - target };
        self.velocity[axis] = velocity;
    }

    /// 弹簧、惯性的速度，用来接着下一段运动；没在动时为 0
    fn carried_velocity(&self, axis: usize) -> f32 {
        if matches!(self.motion[axis], ScrollPhase::Spring { .. } | ScrollPhase::Fling) { self.velocity[axis] } else { 0.0 }
    }

    /// 手指拖动：跟手；超出边缘的部分越拉越沉（打断正在进行的弹簧、惯性，从当前位置接着拖）
    pub fn touch(&mut self, axis: usize, delta: f32) {
        let dimension = self.view[axis].max(1.0);
        let raw = match self.motion[axis] {
            ScrollPhase::Touch { raw } => raw,
            _ => to_raw(self.offset[axis], self.max[axis], dimension),
        } + delta;
        self.motion[axis] = ScrollPhase::Touch { raw };
        self.velocity[axis] = 0.0;
        self.offset[axis] = from_raw(raw, self.max[axis], dimension);
    }

    /// 手指抬起：拉过边缘的带着松手时的速度（`velocity`）弹回去；没拉过的停在原地，等系统的惯性开始
    pub fn release(&mut self, axis: usize, velocity: f32) {
        if !matches!(self.motion[axis], ScrollPhase::Touch { .. }) {
            return;
        }
        self.motion[axis] = ScrollPhase::Idle;
        if self.overscroll(axis) != 0.0 {
            let edge = self.clamp(axis, self.offset[axis]);
            self.spring(axis, edge, BOUNCE, velocity);
        }
    }

    /// 惯性：从当前位置带着 `velocity` 滑出去，越来越慢；在弹的（松手时拉过了边缘）不管
    pub fn fling(&mut self, axis: usize, velocity: f32) {
        if matches!(self.motion[axis], ScrollPhase::Spring { .. }) || self.overscroll(axis) != 0.0 {
            return;
        }
        if velocity.abs() < FLING_STOP {
            self.motion[axis] = ScrollPhase::Idle;
            return;
        }
        self.motion[axis] = ScrollPhase::Fling;
        self.velocity[axis] = velocity;
    }

    /// 还在滑（惯性）
    pub fn flinging(&self) -> bool {
        self.motion.contains(&ScrollPhase::Fling)
    }

    /// 手指放上触控板、触摸屏：滑着的惯性停在原地
    pub fn catch(&mut self, axis: usize) {
        if self.motion[axis] == ScrollPhase::Fling {
            self.motion[axis] = ScrollPhase::Idle;
            self.velocity[axis] = 0.0;
        }
    }

    /// 没有阶段的精确位移（有些鼠标）：直接跟着走，不超出边缘
    pub fn nudge(&mut self, axis: usize, delta: f32) {
        self.motion[axis] = ScrollPhase::Idle;
        self.velocity[axis] = 0.0;
        self.offset[axis] = self.clamp(axis, self.offset[axis] + delta);
    }

    /// 滚轮一格（或 `scroll_by`）：目标累加，弹簧平滑过去，不超出边缘
    pub fn wheel(&mut self, axis: usize, delta: f32) {
        let target = self.clamp(axis, self.wheel_target(axis) + delta);
        self.spring(axis, target, WHEEL, self.carried_velocity(axis));
    }

    /// 滚轮报来的不足一格的位移（精确式触控板、高分辨率滚轮）：直接跟着走，不超出边缘。
    /// 前面整格的弹簧还没走完时从它的目标接着走，剩下的那段一下到位，不丢
    pub fn precise_wheel(&mut self, axis: usize, delta: f32) {
        let from = self.wheel_target(axis);
        self.motion[axis] = ScrollPhase::Idle;
        self.velocity[axis] = 0.0;
        self.offset[axis] = self.clamp(axis, from + delta);
    }

    /// 滚轮接着往哪儿累加：滚轮的弹簧在走时是它的目标，否则是现在的位置（拉过边缘的从边缘算）
    fn wheel_target(&self, axis: usize) -> f32 {
        match self.motion[axis] {
            ScrollPhase::Spring { target, omega, .. } if omega == TAU / WHEEL => target,
            _ => self.clamp(axis, self.offset[axis]),
        }
    }

    /// 程序化滚动到某处
    pub fn scroll_to(&mut self, axis: usize, target: f32, animated: bool) {
        let target = self.clamp(axis, target);
        let far = FAR * self.view[axis].max(1.0);
        let distance = target - self.offset[axis];
        if animated && distance.abs() > far * 1.5 {
            // 很远：先跳到离目标两屏的地方，带着正好"滑到"目标的速度落地（不然一路上每帧都是整屏的新内容，
            // 虚拟列表每帧都要换掉所有行，弹簧的尾巴也很长）
            self.offset[axis] = target - far * distance.signum();
            self.spring(axis, target, SCROLL_TO, TAU / SCROLL_TO * far * distance.signum());
        } else if animated {
            self.spring(axis, target, SCROLL_TO, self.carried_velocity(axis));
        } else {
            self.motion[axis] = ScrollPhase::Idle;
            self.velocity[axis] = 0.0;
            self.offset[axis] = target;
        }
    }

    /// 内容在偏移底下挪了 `delta`（虚拟列表上面的行量出来和估计的不一样、前面插了一项）：
    /// 偏移和进行中的运动一起挪，屏幕上的内容不动
    pub fn shift(&mut self, axis: usize, delta: f32) {
        self.offset[axis] += delta;
        self.motion[axis] = match self.motion[axis] {
            ScrollPhase::Spring { target, omega, x } => ScrollPhase::Spring { target: target + delta, omega, x },
            ScrollPhase::Touch { raw } => ScrollPhase::Touch { raw: raw + delta },
            motion @ (ScrollPhase::Idle | ScrollPhase::Fling) => motion,
        };
    }

    /// 程序化滚动的目标变了（滚到第几行时，目标行的位置量出来了）：在弹的从当前位置和速度接着滑向新目标，
    /// 停着的直接跳过去；手指在触控板上、惯性在滑时不管
    pub fn retarget(&mut self, axis: usize, target: f32) {
        let target = self.clamp(axis, target);
        match self.motion[axis] {
            ScrollPhase::Spring { omega, .. } => self.motion[axis] = ScrollPhase::Spring { target, omega, x: self.offset[axis] - target },
            ScrollPhase::Idle => self.offset[axis] = target,
            ScrollPhase::Touch { .. } | ScrollPhase::Fling => {}
        }
    }

    /// 偏移（取整后）变了就显示滚动条，返回是否变了
    pub fn sync(&mut self, now: f64) -> bool {
        self.sync_from(now, now)
    }

    /// 同 [`Self::sync`]，滚动条从时刻 `start` 起淡入（推进一步时是这一步的开头）
    fn sync_from(&mut self, now: f64, start: f64) -> bool {
        let snapped = self.snapped();
        if snapped == self.seen {
            return false;
        }
        self.seen = snapped;
        self.active_at = now;
        self.fade.retarget(start, 1.0);
        true
    }

    /// 显示滚动条（滚动了、指针靠近边缘），停下一会儿之后再淡出
    pub fn reveal(&mut self, now: f64) {
        self.active_at = now;
        self.fade.retarget(now, 1.0);
    }

    /// 滚动条此刻的透明度
    pub fn bar_opacity(&self) -> f32 {
        self.fade.value
    }

    /// 滚动条正在淡出或者已经藏起来了
    pub fn bar_hiding(&self) -> bool {
        self.fade.target() == 0.0
    }

    /// 推进 `dt` 秒到时间 `now`：弹簧和惯性走一步，没人拉着却超出了边缘就弹回去；滚动条按偏移显示、停下一会儿后淡出
    pub fn step(&mut self, dt: f32, now: f64) {
        for axis in 0..2 {
            if self.axes[axis] {
                self.advance(axis, dt);
            }
        }
        // 这一步里滚动了：滚动条从这一步的开头起淡入（和原来逐帧推进时一样，这一步就走了 `dt`）
        self.sync_from(now, now - f64::from(dt));
        let busy = self.busy();
        if busy || self.hold {
            self.active_at = now;
        }
        if !busy && !self.hold && self.fade.target() > 0.0 && now - self.active_at > BAR_HIDE_DELAY {
            // 停下满了一会儿是在这一步里的某一刻：从这一步开头起淡出（和原来逐帧推进时一样，这一步就走了 `dt`）
            self.fade.retarget(now - f64::from(dt), 0.0);
        }
        self.fade.sample(now);
    }

    fn advance(&mut self, axis: usize, dt: f32) {
        match self.motion[axis] {
            ScrollPhase::Spring { .. } => self.step_spring(axis, dt),
            ScrollPhase::Fling if self.overscroll(axis) != 0.0 => {
                // 范围在滑的时候变小了：从这里弹回去
                let edge = self.clamp(axis, self.offset[axis]);
                let velocity = self.velocity[axis].clamp(-self.bounce_limit(axis), self.bounce_limit(axis));
                self.spring(axis, edge, BOUNCE, velocity);
                self.step_spring(axis, dt);
            }
            ScrollPhase::Fling => {
                let (x0, v0) = (self.offset[axis], self.velocity[axis]);
                let decay = (-FRICTION * dt).exp();
                let x = x0 + v0 * (1.0 - decay) / FRICTION;
                let v = v0 * decay;
                let edge = self.clamp(axis, x);
                if x != edge {
                    // 撞到边缘：按解析解算出撞上时的速度 v0 − k·d 和时刻，剩下的时间交给回弹弹簧（冲出去约 v/(ωe)，
                    // 最多视口的四分之一再弹回来）
                    let contact = v0 - FRICTION * (edge - x0);
                    let at = if contact * v0 > 0.0 { ((v0 / contact).ln() / FRICTION).clamp(0.0, dt) } else { 0.0 };
                    let limit = self.bounce_limit(axis);
                    self.offset[axis] = edge;
                    self.spring(axis, edge, BOUNCE, contact.clamp(-limit, limit));
                    self.step_spring(axis, dt - at);
                } else if v.abs() < FLING_STOP {
                    self.offset[axis] = x;
                    self.velocity[axis] = 0.0;
                    self.motion[axis] = ScrollPhase::Idle;
                } else {
                    self.offset[axis] = x;
                    self.velocity[axis] = v;
                }
            }
            ScrollPhase::Idle if self.overscroll(axis) != 0.0 => {
                let edge = self.clamp(axis, self.offset[axis]);
                self.spring(axis, edge, BOUNCE, 0.0);
            }
            ScrollPhase::Idle | ScrollPhase::Touch { .. } => {}
        }
    }

    fn step_spring(&mut self, axis: usize, dt: f32) {
        let ScrollPhase::Spring { target, omega, x } = self.motion[axis] else { return };
        let (x, v) = spring_step(x, self.velocity[axis], omega, dt);
        if x.abs() < 0.05 && v.abs() < 2.0 {
            self.offset[axis] = target;
            self.velocity[axis] = 0.0;
            self.motion[axis] = ScrollPhase::Idle;
        } else {
            self.offset[axis] = target + x;
            self.velocity[axis] = v;
            self.motion[axis] = ScrollPhase::Spring { target, omega, x };
        }
    }

    /// 撞到边缘时交给回弹弹簧的速度上限：冲出去约 v/(ωe)，最多视口的四分之一
    fn bounce_limit(&self, axis: usize) -> f32 {
        TAU / BOUNCE * E * self.view[axis].max(1.0) * 0.25
    }
}

/// 自己滚的元素（多行输入框）画内容的那一层的标识（见 [`DerivedLayer`]）。
/// 渲染线程按这个元素的滚动状态平移这一层，和滚动视口的内容层一样
pub(crate) fn own_content_key(id: NodeId) -> u64 {
    use slotmap::Key as _;
    DerivedLayer::OwnContent.key(id.data().as_ffi())
}

/// 一个视口的滚动状态：物理（[`Physics`]，渲染线程有同样的一份）加上只在主线程用的部分：
/// 输入的速度估计、布局结果、句柄、程序化滚动的排队和目标行
pub(crate) struct Scroller {
    pub phys: Physics,
    pub metrics: Metrics,
    /// 视口的边框盒大小，滚动条沿着它的边
    pub size: [f32; 2],
    /// 用户输入时按最近几帧的位移估计的速度，逻辑像素每秒：松手回弹时用
    estimate: [f32; 2],
    /// 手指抬起那一刻的速度估计：系统的惯性随后开始时（隔一两帧）按它滑出去
    flick: [f32; 2],
    /// 最近几帧的 (时长, x 输入位移, y 输入位移)
    samples: [[f32; 3]; SAMPLES],
    sample: usize,
    /// 这一帧还没记入 `samples` 的输入位移
    input: [f32; 2],
    /// 最近一次交给内容节点（命中测试、无障碍）、写回句柄的偏移（按物理像素取整）
    pub shown: [f32; 2],
    /// 虚拟列表往运动方向多挂多远（带方向）：跟着运动还要走的距离，最多一个视口；
    /// 变长时每帧最多长四分之一个视口，甩出去的那一帧不用一下子挂上一整屏的行
    lead: [f32; 2],
    /// 跟着滚的内容节点
    pub content: NodeId,
    /// 托架节点：视口的第一个子元素，内容在它里面（滚动方向上大小是 0）
    pub holder: NodeId,
    /// 内容是元素自己画的（多行输入框）：没有托架和内容节点（`holder`、`content` 都是元素自己），
    /// 滚动的是它画在自己内容层里的图元（[`own_content_key`]）
    pub own: bool,
    pub handle: Option<ScrollHandle>,
    /// 滚动条的颜色；`None` 时用默认的（见 `Tree::scrollbar_color`）
    pub bar: Option<Color>,
    /// 已经在 `Scrolls::awake` 里
    pub awake: bool,
    /// 布局过一次之后才知道能滚多远；之前的程序化滚动先存着
    pub laid_out: bool,
    pub pending: Vec<Request>,
    /// 调试构建：已经提示过"视口在滚动方向上是 0"
    #[cfg(debug_assertions)]
    pub warned: bool,
    /// 程序化地滚到了第几行（`scroll_to_index`）：行高变了时目标跟着修正，用户自己滚动时清掉
    pub seek: Option<usize>,
    /// 浮层的表面：内容放得下时不接滚轮；放得下时也裁剪吗（元素自己要求裁剪），`false` 时只在滚动时裁剪
    pub surface: Option<bool>,
    /// 物理状态在帧与帧之间改过（输入、程序化滚动、布局后的范围）：这一帧把整份交给渲染线程
    pub dirty: bool,
    /// 物理状态停在的时间（从渲染线程收回来的状态比上一帧新）：下一次推进从这里算起
    pub clock: Option<f64>,
}

impl std::ops::Deref for Scroller {
    type Target = Physics;
    fn deref(&self) -> &Physics {
        &self.phys
    }
}

impl std::ops::DerefMut for Scroller {
    fn deref_mut(&mut self) -> &mut Physics {
        &mut self.phys
    }
}

impl Scroller {
    pub fn new(axes: [bool; 2], content: NodeId, handle: Option<ScrollHandle>, bar: Option<Color>) -> Self {
        Self {
            phys: Physics::new(axes),
            metrics: Metrics::default(),
            size: [0.0; 2],
            estimate: [0.0; 2],
            flick: [0.0; 2],
            samples: [[0.0; 3]; SAMPLES],
            sample: 0,
            input: [0.0; 2],
            shown: [0.0; 2],
            lead: [0.0; 2],
            content,
            holder: NodeId::default(),
            own: false,
            handle,
            bar,
            awake: false,
            laid_out: false,
            pending: Vec::new(),
            #[cfg(debug_assertions)]
            warned: false,
            seek: None,
            surface: None,
            dirty: true,
            clock: None,
        }
    }

    /// 还要每帧推进：在动、被拉着，或者刚有输入（速度估计还没归零）
    pub fn busy(&self) -> bool {
        self.phys.busy() || self.estimate != [0.0; 2]
    }

    /// 新的布局结果：滚动范围、视口长度、滚动条能走的距离
    pub fn set_metrics(&mut self, metrics: Metrics, size: [f32; 2]) {
        self.metrics = metrics;
        self.size = size;
        self.phys.set_bounds(metrics.max(), metrics.viewport);
        self.phys.travel = [0, 1].map(|axis| thumb_length(&self.phys, &metrics, size[axis], axis).map(|(_, travel)| travel));
    }

    /// 手指拖动，见 [`Physics::touch`]
    pub fn touch(&mut self, axis: usize, delta: f32) {
        self.seek = None;
        let before = self.phys.offset[axis];
        self.phys.touch(axis, delta);
        self.input[axis] += self.phys.offset[axis] - before;
    }

    /// 手指抬起：拉过边缘的带着松手时的速度弹回去
    pub fn release(&mut self, axis: usize) {
        self.flick[axis] = self.estimate[axis];
        self.phys.release(axis, self.estimate[axis]);
    }

    /// 手指离开触摸屏：拉过边缘的弹回去；`fling` 时带着松手时的速度自己滑出去（触摸屏没有系统的惯性事件），
    /// 系统收走手指时不滑。之后和触控板的惯性是同一段物理，渲染线程照样推进
    pub fn lift(&mut self, axis: usize, fling: bool) {
        let velocity = self.estimate[axis];
        self.release(axis);
        if fling {
            self.seek = None;
            self.phys.fling(axis, velocity);
        }
    }

    /// 系统的惯性开始了（第一个事件带着位移 `delta`）：带着松手时的速度自己滑出去，之后系统的惯性事件不再管。
    /// 速度是松手那一刻按最近几帧估计的；和这个事件的方向不一致（没估出来）时按这一个事件算
    pub fn momentum(&mut self, axis: usize, delta: f32) {
        self.seek = None;
        let flick = self.flick[axis];
        let velocity = if flick * delta > 0.0 { flick } else { delta * 120.0 };
        self.phys.fling(axis, velocity);
    }

    /// 没有阶段的精确位移，见 [`Physics::nudge`]
    pub fn nudge(&mut self, axis: usize, delta: f32) {
        self.seek = None;
        let before = self.phys.offset[axis];
        self.phys.nudge(axis, delta);
        self.input[axis] += self.phys.offset[axis] - before;
    }

    /// 滚轮一格，见 [`Physics::wheel`]
    pub fn wheel(&mut self, axis: usize, delta: f32) {
        self.seek = None;
        self.phys.wheel(axis, delta);
    }

    /// 滚轮的精确位移，见 [`Physics::precise_wheel`]
    pub fn precise_wheel(&mut self, axis: usize, delta: f32) {
        self.seek = None;
        let before = self.phys.offset[axis];
        self.phys.precise_wheel(axis, delta);
        self.input[axis] += self.phys.offset[axis] - before;
    }

    /// 程序化滚动到某处（滚到第几行的请求之后再记下 `seek`）
    pub fn scroll_to(&mut self, axis: usize, target: f32, animated: bool) {
        self.seek = None;
        self.phys.scroll_to(axis, target, animated);
    }

    /// 这一帧虚拟列表往运动方向多挂多远（见 `lead`），变了才返回
    pub fn lead(&mut self) -> Option<[f32; 2]> {
        let before = self.lead;
        for axis in 0..2 {
            let view = self.phys.view[axis];
            let want = self.phys.remaining(axis).clamp(-view, view);
            // 换了方向从 0 重新长
            let last = if want * self.lead[axis] < 0.0 { 0.0 } else { self.lead[axis] };
            self.lead[axis] = if want.abs() <= last.abs() { want } else { last + (want - last).clamp(-view * 0.25, view * 0.25) };
        }
        (self.lead != before).then_some(self.lead)
    }

    /// 每帧推进到 `now`：物理走一步；用户输入时按最近几帧的位移估计速度
    pub fn step(&mut self, dt: f32, now: f64) {
        let input = std::mem::take(&mut self.input);
        self.samples[self.sample] = [dt, input[0], input[1]];
        self.sample = (self.sample + 1) % SAMPLES;
        let span: f32 = self.samples.iter().map(|s| s[0]).sum();
        for axis in 0..2 {
            // 最近几帧的位移之和 / 时长之和：事件到达的节奏不均匀，只看一帧会忽大忽小
            let moved: f32 = self.samples.iter().map(|s| s[1 + axis]).sum();
            self.estimate[axis] = if span > 0.0 && moved != 0.0 { moved / span } else { 0.0 };
        }
        self.phys.step(dt, now);
    }
}

/// 橡皮筋：拉出去 `x`（≥ 0）显示成多少
fn rubber(x: f32, dimension: f32) -> f32 {
    x * dimension * RUBBER / (dimension + RUBBER * x)
}

/// [`rubber`] 的反函数
fn unrubber(y: f32, dimension: f32) -> f32 {
    let y = y.min(dimension * 0.99);
    y * dimension / (RUBBER * (dimension - y))
}

/// 不加阻力的位置 -> 显示的偏移
fn from_raw(raw: f32, max: f32, dimension: f32) -> f32 {
    if raw < 0.0 {
        -rubber(-raw, dimension)
    } else if raw > max {
        max + rubber(raw - max, dimension)
    } else {
        raw
    }
}

fn to_raw(offset: f32, max: f32, dimension: f32) -> f32 {
    if offset < 0.0 {
        -unrubber(-offset, dimension)
    } else if offset > max {
        max + unrubber(offset - max, dimension)
    } else {
        offset
    }
}

/// 临界阻尼弹簧的解析解：相对目标的位移和速度推进 `dt` 秒（任意步长都精确，不会发散）
fn spring_step(x0: f32, v0: f32, omega: f32, dt: f32) -> (f32, f32) {
    let decay = (-omega * dt).exp();
    let c = v0 + omega * x0;
    ((x0 + c * dt) * decay, (v0 - omega * c * dt) * decay)
}

/// 滚动条的长度和能走的距离（轨道减去长度）：`length` 是视口在这个方向上的边框盒长度
fn thumb_length(phys: &Physics, metrics: &Metrics, length: f32, axis: usize) -> Option<(f32, f32)> {
    if !phys.axes[axis] || phys.max[axis] < 0.5 {
        return None;
    }
    // 两个方向都有滚动条时，各自让出角上的位置
    let other = 1 - axis;
    let corner = if phys.axes[other] && phys.max[other] >= 0.5 { BAR_THICKNESS + BAR_INSET } else { 0.0 };
    let track = (length - 2.0 * BAR_INSET - corner).max(0.0);
    let content = metrics.content[axis].max(1.0);
    let thumb = (track * metrics.viewport[axis] / content).clamp(BAR_MIN.min(track), track);
    Some((thumb, track - thumb))
}

/// 视口滚动条的几何：在视口的布局坐标里，滚动条位于轨道起点时的矩形，和沿轨道的当前位置（未取整）
pub(crate) fn thumb(scroller: &Scroller, bounds: Rect, axis: usize) -> Option<(Rect, f32)> {
    let length = if axis == 0 { bounds.width } else { bounds.height };
    let (thumb, _) = thumb_length(&scroller.phys, &scroller.metrics, length, axis)?;
    let position = scroller.thumb_position(axis)?;
    let rect = if axis == 0 {
        Rect {
            x: bounds.x + BAR_INSET,
            y: bounds.y + bounds.height - BAR_INSET - BAR_THICKNESS,
            width: thumb,
            height: BAR_THICKNESS,
        }
    } else {
        Rect { x: bounds.x + bounds.width - BAR_INSET - BAR_THICKNESS, y: bounds.y + BAR_INSET, width: BAR_THICKNESS, height: thumb }
    };
    Some((rect, position))
}

/// 拖动滚动条时：滚动条在轨道上移动一像素，内容滚多少
pub(crate) fn thumb_ratio(scroller: &Scroller, axis: usize) -> f32 {
    match scroller.travel[axis] {
        Some(free) if free > 0.5 => scroller.max[axis] / free,
        _ => 0.0,
    }
}

/// 偏移按物理像素取整：内容层只做整像素的平移，文字保持清晰
pub(crate) fn snap(value: f32, scale: f32) -> f32 {
    (value * scale).round() / scale
}
