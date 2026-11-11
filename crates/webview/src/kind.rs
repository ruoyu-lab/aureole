//! 网页视图的元素种类：量大小、画外部纹理、大小和缩放变了告诉 CEF、有了大小时打开网页；挂上时网页回到前台，
//! 卸载时转到后台（不关，见 `lifecycle`）。

use std::rc::Rc;
use std::time::{Duration, Instant};

use aureole::Element;
use aureole_core::kinds::{
    AccessibilityProvider, AvailableSpace, ElementKind, InputTarget, KindHooks, MeasureContext, NextFrame, PaintContext, TextInputClient, TickContext, UnmountContext,
    UpdateContext,
};
use aureole_core::property::Dirty;

use crate::engine::Pacing;
use crate::state::{Geometry, Shared, WebController};

/// 没指定大小时的大小，逻辑像素
const DEFAULT_SIZE: [f32; 2] = [300.0, 150.0];

pub(crate) struct WebViewKind {
    pub(crate) shared: Rc<Shared>,
    /// 打开失败过：不再每次更新都重试（原因已经记进日志）
    failed: bool,
    /// 静止时下一次探页面要不要出帧的时刻（帧推进的时钟）
    next_poll: Duration,
    /// 上次交给框架时页面无障碍树的版本
    ax_seen: u64,
    /// 元素在哪个窗口里（问它所在屏幕的刷新率）；建元素时不在窗口里的，第一次更新时再找
    window: Option<aureole::WindowHandle>,
    /// 上次问屏幕刷新率的时刻
    rate_checked: Option<Instant>,
    /// 屏幕的每次刷新里到了几帧新画面的统计
    cadence: Cadence,
    /// 内核降了帧率以后（画好的帧没人取）：连着几次换上的帧在队里只等了一小会儿
    prompt: u32,
}

/// 换上的帧在队里等得不到这么久算框架跟得上；连着 [`PROMPT_FRAMES`] 次就把内核的帧率升回去
const PROMPT: Duration = Duration::from_millis(50);
const PROMPT_FRAMES: u32 = 2;

/// 多久问一次窗口所在屏幕的刷新率（窗口挪到别的屏幕、系统里改了刷新率时跟上）
const RATE_CHECK: Duration = Duration::from_secs(1);
/// macOS 开着低电量模式时内核最多每秒出几帧（系统只显示这么多）
const LOW_POWER_RATE: i32 = 60;
/// 多久记一次到帧的统计
const CADENCE_EVERY: Duration = Duration::from_secs(2);

/// 框架每出一帧（屏幕的一次刷新）时换上了几帧新画面：正好一帧是跟上了；没有是这次刷新还显示上一帧；
/// 两帧以上是有一帧没来得及显示就被换掉了（框架发信号时才会有）。`dropped` 是排着队来不及显示、被顶掉的
/// （内核自己出帧时）。调试级别的日志里每 [`CADENCE_EVERY`] 一行
#[derive(Default)]
struct Cadence {
    since: Option<Instant>,
    /// 没有、一帧、两帧以上
    counts: [u32; 3],
    dropped: u32,
}

impl Cadence {
    fn record(&mut self, arrived: u32, dropped: u32) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        self.counts[arrived.min(2) as usize] += 1;
        self.dropped += dropped;
        let span = now - since;
        if span >= CADENCE_EVERY {
            let [none, one, more] = self.counts;
            tracing::debug!(
                "CEF 视图：{:.1} 秒里框架出了 {} 帧，其中换上一帧新画面的 {one} 帧，没有新画面的 {none} 帧，两帧以上的 {more} 帧；来不及显示丢掉 {} 帧",
                span.as_secs_f32(),
                none + one + more,
                self.dropped
            );
            self.counts = [0; 3];
            self.dropped = 0;
            self.since = Some(now);
        }
    }
}

pub(crate) fn element(controller: &WebController) -> Element {
    // 输入目标只收得到拿焦点的左键：悬停、右键、中键、滚轮、按键松开由挂在元素上的处理器转给 CEF
    crate::input::attach(Element::from_kind(WebViewKind {
        shared: controller.shared.clone(),
        failed: false,
        next_poll: Duration::ZERO,
        ax_seen: 0,
        window: aureole::current_window(),
        rate_checked: None,
        cadence: Cadence::default(),
        prompt: 0,
    }), &controller.shared)
}

impl ElementKind for WebViewKind {
    fn name(&self) -> &'static str {
        "web_view"
    }

    fn hooks(&self) -> KindHooks {
        KindHooks::UPDATE | KindHooks::TICK
    }

    /// 框架发信号时（[`Pacing::Signal`]）跟着框架的帧时钟让 CEF 出帧：有动静（输入、加载、刚出过一帧）时每帧都要，
    /// 静止时隔一会儿探一次。内核自己出帧时（[`Pacing::Kernel`]）只对一下屏幕的刷新率
    fn tick(&mut self, cx: &TickContext) -> Dirty {
        match crate::engine::pacing() {
            Pacing::Signal => {
                let now = cx.now();
                if self.busy() || now >= self.next_poll {
                    crate::render::begin_frame(&self.shared);
                    self.next_poll = now + crate::render::IDLE_POLL;
                }
            }
            Pacing::Kernel => {
                // 回调在别的线程上拿不到窗口共用的设备：每帧交过去（同一个设备时只比一下编号）
                self.shared.page.set_gpu(aureole_runtime::shared_gpu());
                self.sync_frame_rate();
                // 排着队的新画面换上最早的一帧：屏幕的一次刷新换一帧
                let waited = crate::render::refresh(&self.shared);
                self.unthrottle(waited);
            }
        }
        let (arrived, dropped) = self.shared.page.take_arrivals();
        self.cadence.record(arrived, dropped);
        Dirty::empty()
    }

    /// 内核自己出帧时不用框架每帧来要：画好一帧时 `frame` 变了，元素重画，框架那时出一帧
    fn next_frame(&self, _now: Duration) -> NextFrame {
        if self.shared.browser.borrow().is_none() {
            return NextFrame::Idle;
        }
        match crate::engine::pacing() {
            Pacing::Signal if self.busy() => NextFrame::Continuous,
            Pacing::Signal => NextFrame::At(self.next_poll),
            Pacing::Kernel if continuous() => NextFrame::Continuous,
            Pacing::Kernel => NextFrame::Idle,
        }
    }

    fn measure(&mut self, _: &mut MeasureContext, known: [Option<f32>; 2], _: [AvailableSpace; 2]) -> [f32; 2] {
        [known[0].unwrap_or(DEFAULT_SIZE[0]), known[1].unwrap_or(DEFAULT_SIZE[1])]
    }

    /// 读 `frame`、`cursor`：CEF 画好一帧、换了光标时元素跟着重画（引用的纹理编号第一次有、或者换了）。
    /// 大小、缩放变了告诉 CEF；第一次有大小时打开网页
    fn update(&mut self, cx: &mut UpdateContext) {
        // 第一次更新就是刚挂上：网页回到前台（冻结的解冻，丢弃的变回活动、下面按地址打开）
        crate::lifecycle::shown(&self.shared);
        let _ = self.shared.frame.get();
        // 光标在重画的那帧重新读：CEF 不重画就换光标时也要跟上
        let _ = self.shared.cursor.get();
        // 页面的无障碍树变了：重建交给框架的虚拟子节点（辅助技术没打开时没有开销）
        let ax = self.shared.ax_version.get();
        if ax != self.ax_seen {
            self.ax_seen = ax;
            cx.request_accessibility();
        }
        // 页面自己关了（`window.close()`）：不按地址重新打开，应用 `load` 时才开
        let closed = self.shared.closed.get();
        let size = cx.size();
        let scale = cx.scale_factor();
        let page = &self.shared.page;
        let old = page.geometry();
        let geometry = Geometry { size, scale, ..old };
        if geometry != old {
            page.set_geometry(geometry);
            crate::render::wake(&self.shared);
            let host = self.shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
            if let Some(host) = host {
                if scale != old.scale {
                    cef::ImplBrowserHost::notify_screen_info_changed(&host);
                }
                cef::ImplBrowserHost::was_resized(&host);
            }
        }
        // 回调的线程上拿不到窗口共用的设备，打开之前先交过去
        page.set_gpu(aureole_runtime::shared_gpu());
        // 内核自己出帧时：打开之前先知道屏幕的刷新率
        self.sync_frame_rate();
        let opened = self.shared.browser.borrow().is_some();
        if !opened
            && !self.failed
            && !closed
            && size[0] >= 1.0
            && size[1] >= 1.0
            && let Err(error) = crate::engine::open(&self.shared)
        {
            tracing::warn!("CEF 视图打不开网页：{error}");
            self.failed = true;
        }
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        if let Some(texture) = self.shared.page.texture() {
            cx.external_texture(texture);
        }
    }

    fn input_target(&self) -> Option<&dyn InputTarget> {
        Some(self)
    }

    fn input_target_mut(&mut self) -> Option<&mut dyn InputTarget> {
        Some(self)
    }

    fn accessibility_provider(&self) -> Option<&dyn AccessibilityProvider> {
        Some(self)
    }

    fn accessibility_provider_mut(&mut self) -> Option<&mut dyn AccessibilityProvider> {
        Some(self)
    }

    fn text_input_client(&self) -> Option<&dyn TextInputClient> {
        Some(self)
    }

    fn text_input_client_mut(&mut self) -> Option<&mut dyn TextInputClient> {
        Some(self)
    }

    /// 网页不关，转到后台：控制器和元素都放掉时才关（`Shared` 的 `Drop`）
    fn unmount(&mut self, _cx: &mut UnmountContext) {
        crate::lifecycle::hidden(&self.shared);
    }
}

impl WebViewKind {
    /// 内核自己出帧时：把窗口所在屏幕的刷新率告诉它（变了才说；最多每 [`RATE_CHECK`] 问一次系统）。
    /// macOS 开着低电量模式时最多 [`LOW_POWER_RATE`]
    fn sync_frame_rate(&mut self) {
        if crate::engine::pacing() != Pacing::Kernel {
            return;
        }
        let now = Instant::now();
        if self.rate_checked.is_some_and(|at| now - at < RATE_CHECK) {
            return;
        }
        self.rate_checked = Some(now);
        if self.window.is_none() {
            self.window = aureole::current_window();
        }
        let Some(rate) = self.window.and_then(aureole::WindowHandle::refresh_rate) else { return };
        let mut rate = (rate.round() as i32).clamp(24, 480);
        // macOS 的低电量模式：系统只显示每秒 60 帧，框架的帧时钟也降到 60，内核多画的白画
        if cfg!(target_os = "macos") && aureole::is_low_power_mode() {
            rate = rate.min(LOW_POWER_RATE);
        }
        // 降着帧率的时候只记下，升回去时用
        if rate != self.shared.frame_rate.replace(rate) && !self.shared.throttled.get() {
            tracing::debug!("CEF 视图：内核按每秒 {rate} 帧出帧（屏幕的刷新率，低电量模式下最多 {LOW_POWER_RATE}）");
            let host = self.shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
            if let Some(host) = host {
                cef::ImplBrowserHost::set_windowless_frame_rate(&host, rate);
            }
        }
    }

    /// 内核降了帧率以后（画好的帧一直没人取，见 `render::throttle`）：框架又跟得上了（换上的帧在队里只等了一小会儿，
    /// 连着几次）就升回屏幕的刷新率。窗口被挡着时框架隔很久才出一帧，换上的帧等得久，不升
    fn unthrottle(&mut self, waited: Option<Duration>) {
        if !self.shared.throttled.get() {
            return;
        }
        match waited {
            Some(waited) if waited < PROMPT => self.prompt += 1,
            Some(_) => self.prompt = 0,
            None => {}
        }
        if self.prompt < PROMPT_FRAMES {
            return;
        }
        self.prompt = 0;
        self.shared.throttled.set(false);
        let rate = match self.shared.frame_rate.get() {
            0 => crate::engine::DEFAULT_FRAME_RATE,
            rate => rate,
        };
        let host = self.shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
        if let Some(host) = host {
            tracing::debug!("CEF 视图：框架又在出帧了，内核回到每秒 {rate} 帧");
            cef::ImplBrowserHost::set_windowless_frame_rate(&host, rate);
        }
    }

    /// 页面可能在动：有动静以后的一段时间里，或者正在加载
    fn busy(&self) -> bool {
        continuous() || crate::render::awake(&self.shared) || self.shared.loading.get_untracked()
    }
}

/// `AUREOLE_WEBVIEW_CONTINUOUS=1`：每帧都让 CEF 出帧，不按有没有动静省着（量性能、对比时用）
fn continuous() -> bool {
    static CONTINUOUS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CONTINUOUS.get_or_init(|| std::env::var_os("AUREOLE_WEBVIEW_CONTINUOUS").is_some_and(|v| v == "1"))
}
