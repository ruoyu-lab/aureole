//! 离屏绘制：CEF 画好的纹理（macOS 是 IOSurface，Windows 是 D3D11 的共享纹理）在回调里复制进框架登记的
//! 外部纹理（[`aureole_render::ExternalTexture`]），再到主线程上加一 `frame` 让元素重画。GPU 共享纹理不可用时，
//! CEF 给 CPU 的像素（`on_paint`），写进同一张纹理。
//!
//! 回调在 CEF 的界面线程上（macOS 上就是主线程，Windows 上是 CEF 自己的线程），复制在回调里做，
//! 状态放在 [`Page`] 里（加锁）；元素画的时候读的纹理编号是原子的，不等复制。
//!
//! 出帧的节奏有两种：
//! - 框架发信号（Windows 上默认）：元素每帧推进时，有动静（输入、加载、刚出过一帧）就给 CEF 发帧开始的信号，
//!   静止时每 [`IDLE_POLL`] 探一次。出帧跟屏幕的刷新对齐，但内核一次只让一帧在路上；
//! - 内核自己出帧（macOS 上默认）：内核按窗口所在屏幕的刷新率出帧，页面不动时不出。新画面到了先排队
//!   （最多 [`QUEUE`] 帧），框架出帧时换上最早的一帧（[`refresh`]）：屏幕的一次刷新换一帧。画好的帧一直没人取
//!   （窗口被挡住、最小化了）时让内核降到很低的帧率，框架又出帧了再升回去。
//!
//! 纹理建在窗口共用的设备上（[`aureole_runtime::shared_gpu`]，主线程每帧交给 [`Page`]）。CEF 的像素是预乘的
//! BGRA、sRGB 编码，复制进 sRGB 格式的纹理，框架的着色器采样时按 sRGB 解码，和图片一样。

use std::sync::Arc;
use std::time::{Duration, Instant};

use aureole_render::{ExternalTexture, Gpu};
use cef::{
    AcceleratedPaintInfo, AccessibilityHandler, Browser, DragData, DragOperationsMask, ImplRenderHandler, PaintElementType, Range, Rect, RenderHandler, ScreenInfo, WrapRenderHandler,
    rc::Rc as _, wrap_render_handler,
};

use crate::state::{Page, Shared};

#[cfg(target_os = "windows")]
mod d3d12;
#[cfg(target_os = "macos")]
mod iosurface;

/// 离屏绘制的状态（在 [`Page`] 里，加锁）
#[derive(Default)]
pub(crate) struct RenderState {
    /// 网页打开着：关掉以后晚到的帧不再建纹理
    open: bool,
    /// 登记给渲染器的外部纹理：元素画的时候引用它的编号
    external: Option<ExternalTexture>,
    /// 复制进去的纹理，和它建在哪个设备上
    target: Option<Target>,
    /// 出帧的统计：每隔 [`STATS_EVERY`] 记一行日志（调试级别）
    stats: Stats,
    /// macOS、内核自己出帧时：到了还没换上的新画面（最多 [`QUEUE`] 帧），框架出帧时换上最早的一帧（见 [`refresh`]）
    #[cfg(target_os = "macos")]
    queue: std::collections::VecDeque<(iosurface::Frame, Instant)>,
    /// macOS：连着顶掉了几帧没人取（框架出帧时清零）
    #[cfg(target_os = "macos")]
    starved: u32,
    /// Windows：打开过的 CEF 的共享纹理
    #[cfg(target_os = "windows")]
    opened: d3d12::Opened,
    /// Windows：在自己的队列上复制
    #[cfg(target_os = "windows")]
    copier: Option<d3d12::Copier>,
    /// Windows：打开过的弹出层的共享纹理（和页面的分开留：大小不一样）
    #[cfg(target_os = "windows")]
    opened_popup: d3d12::Opened,
    /// 页面里开着的弹出层（下拉框、日期选择这类）在页面里的位置和大小，逻辑像素；关着时为 `None`
    popup: Option<[i32; 4]>,
    /// CPU 的像素那条路上弹出层最近的一帧（BGRA、逐行紧密排列）和它的大小：页面每一帧都要叠回去
    popup_pixels: Option<(Vec<u8>, [u32; 2])>,
}

/// 排着队等换上的新画面最多几帧。内核出帧的节奏跟屏幕的刷新差不多齐，新画面常常正好在两次刷新的交界到：
/// 早一点晚一点，就成了这次刷新没有、下次两帧。留一帧的余量：到了两帧时这次换上早的那帧，晚的留给下次，
/// 之后每次刷新都有一帧可换（晚一帧显示）；再多的顶掉最早的
#[cfg(target_os = "macos")]
const QUEUE: usize = 2;
/// 连着顶掉这么多帧都没人取（120 帧时四分之一秒）：框架没在出帧（窗口被挡住、最小化了，屏幕关了），
/// 让内核降到 [`STARVED_RATE`]，不白画
#[cfg(target_os = "macos")]
const STARVED: u32 = 30;
/// 没人取的时候内核出帧的帧率：窗口再露出来时最多晚十分之一秒看到新画面，之后升回屏幕的刷新率
#[cfg(target_os = "macos")]
pub(crate) const STARVED_RATE: i32 = 10;

/// 多久记一次出帧的统计
const STATS_EVERY: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    /// 每帧在回调里花的时间（导入、复制，或者上传 CPU 的像素），毫秒
    costs: Vec<f32>,
    /// 其中等 GPU 复制完的时间，毫秒
    waits: Vec<f32>,
}

impl Stats {
    fn record(&mut self, cost: Duration, wait: Duration) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(now);
        self.costs.push(cost.as_secs_f32() * 1000.0);
        self.waits.push(wait.as_secs_f32() * 1000.0);
        let span = now - since;
        if span >= STATS_EVERY {
            self.costs.sort_by(f32::total_cmp);
            self.waits.sort_by(f32::total_cmp);
            let median = self.costs[self.costs.len() / 2];
            let worst = self.costs.last().copied().unwrap_or(0.0);
            let wait = self.waits[self.waits.len() / 2];
            tracing::debug!(
                "CEF 视图：{:.1} 秒到了 {} 帧（每秒 {:.0}），每帧在回调里中位 {median:.2} 毫秒（等 GPU {wait:.2}）、最长 {worst:.2} 毫秒",
                span.as_secs_f32(),
                self.costs.len(),
                self.costs.len() as f32 / span.as_secs_f32()
            );
            self.costs.clear();
            self.waits.clear();
            self.since = Some(now);
        }
    }
}

struct Target {
    texture: wgpu::Texture,
    gpu: Arc<Gpu>,
}

impl RenderState {
    /// 网页打开了：之后来的帧照常写
    pub(crate) fn opened(&mut self) {
        self.open = true;
    }

    /// 网页关掉（元素卸载）时：撤销外部纹理，放掉纹理
    pub(crate) fn clear(&mut self) {
        self.open = false;
        self.external = None;
        self.target = None;
        self.popup = None;
        self.popup_pixels = None;
        #[cfg(target_os = "macos")]
        self.queue.clear();
        #[cfg(target_os = "windows")]
        {
            self.opened.clear();
            self.opened_popup.clear();
            self.copier = None;
        }
    }

    /// 弹出层叠在目标纹理上的位置（物理像素）：逻辑像素的位置乘缩放
    fn popup_at(&self, scale: f32) -> Option<[u32; 2]> {
        let [x, y, _, _] = self.popup?;
        let scale = if scale > 0.0 { scale } else { 1.0 };
        Some([(x.max(0) as f32 * scale).round() as u32, (y.max(0) as f32 * scale).round() as u32])
    }

    /// 别处建好、写好的目标纹理（Windows 的复制队列自己建目标纹理）：新的换进来。返回元素引用的编号
    #[cfg(target_os = "windows")]
    fn install(&mut self, gpu: &Arc<Gpu>, texture: wgpu::Texture, fresh: bool) -> Option<u32> {
        if fresh || self.target.is_none() {
            self.target = Some(Target { texture, gpu: gpu.clone() });
        }
        self.finish(fresh)
    }

    /// 要写 `size`（物理像素）、`format` 的一帧：大小、格式、设备都对得上时用原来的纹理，
    /// 否则在 `gpu` 上建一张新的、换进登记表
    fn target(&mut self, gpu: &Arc<Gpu>, size: [u32; 2], format: wgpu::TextureFormat) -> (&wgpu::Texture, bool) {
        let format = format.add_srgb_suffix();
        let fits = self.target.as_ref().is_some_and(|t| {
            t.gpu.id() == gpu.id() && t.texture.width() == size[0] && t.texture.height() == size[1] && t.texture.format() == format
        });
        if !fits {
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("CEF 视图的一帧"),
                size: wgpu::Extent3d { width: size[0].max(1), height: size[1].max(1), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.target = Some(Target { texture, gpu: gpu.clone() });
        }
        let target = self.target.as_ref().expect("刚放进去");
        (&target.texture, !fits)
    }

    /// 写完一帧：新建的纹理换进登记表，原来的报告内容变了。返回元素引用的编号
    fn finish(&mut self, fresh: bool) -> Option<u32> {
        let target = self.target.as_ref()?;
        let external = self.external.get_or_insert_with(ExternalTexture::new);
        if fresh {
            external.set(&target.gpu, target.texture.clone());
        } else {
            external.changed();
        }
        Some(external.id())
    }
}

/// 有动静（输入、改大小、打开）、出了一帧以后再连续发多久帧开始的信号。按时间不按帧数：隔一会儿才画一次的页面
/// （每 100 毫秒刷新一次的提示、时钟、低帧率的小动画）一直续得上，不会掉到静止时探的节奏；
/// 页面不画以后很快不再发。静止的页面收到信号时几乎不花什么（Windows 上整个进程不到 1% 的 CPU）
const AWAKE: Duration = Duration::from_millis(500);
/// 静止时多久探一次：发一次信号，页面自己开始动了（定时器触发的动画、网络来的新内容）就出一帧，接着连续发。
/// 输入、加载引起的变化不等它（马上开始连续发）；页面自己开始的变化最多晚这么久画出来
pub(crate) const IDLE_POLL: Duration = Duration::from_millis(100);

/// 有动静：接下来一段时间每帧都让 CEF 出帧
pub(crate) fn wake(shared: &Shared) {
    let until = Instant::now() + AWAKE;
    shared.awake_until.set(Some(shared.awake_until.get().map_or(until, |t| t.max(until))));
}

/// 还在连续发帧开始的信号的那段时间里
pub(crate) fn awake(shared: &Shared) -> bool {
    shared.awake_until.get().is_some_and(|until| Instant::now() < until)
}

/// 框架的一帧：给 CEF 发帧开始的信号（它这一帧有要画的就画，画好了从离屏回调交来）。
/// 只在主线程上、元素的每帧推进里调用
pub(crate) fn begin_frame(shared: &Shared) {
    // 回调在别的线程上拿不到窗口共用的设备：每帧交过去（同一个设备时只比一下编号）
    shared.page.set_gpu(aureole_runtime::shared_gpu());
    let host = shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
    if let Some(host) = host {
        cef::ImplBrowserHost::send_external_begin_frame(&host);
    }
}

/// 一帧写好了（回调的线程上）：到主线程上让元素重画（引用的编号可能是第一次有）；页面在动，接着每帧要
fn painted(page: &Page) {
    page.on_main(|shared| {
        wake(shared);
        shared.frame.update(|n| *n = n.wrapping_add(1));
    });
}

/// CPU 的像素（BGRA、预乘、逐行紧密排列）写进纹理。`popup`：是弹出层的一帧（留着，叠在页面上）
fn upload(page: &Page, pixels: &[u8], size: [u32; 2], popup: bool) {
    let begin = Instant::now();
    let Some(gpu) = page.gpu() else { return };
    let scale = page.geometry().scale;
    let mut state = page.render();
    if !state.open {
        return;
    }
    if popup {
        state.popup_pixels = Some((pixels.to_vec(), size));
        let Some(at) = state.popup_at(scale) else { return };
        let Some(target) = state.target.as_ref().map(|t| t.texture.clone()) else { return };
        write_region(&gpu, &target, pixels, size, at);
        page.set_texture(state.finish(false));
        return;
    }
    let (texture, fresh) = state.target(&gpu, size, wgpu::TextureFormat::Bgra8Unorm);
    let texture = texture.clone();
    write_region(&gpu, &texture, pixels, size, [0, 0]);
    // 弹出层开着：叠回去，它不在页面这一帧里
    if let (Some(at), Some((popup, popup_size))) = (state.popup_at(scale), state.popup_pixels.as_ref()) {
        write_region(&gpu, &texture, popup, *popup_size, at);
    }
    page.set_texture(state.finish(fresh));
    page.note_arrival();
    state.stats.record(begin.elapsed(), Duration::ZERO);
}

/// 框架的一帧开始（主线程上，内核自己出帧时）：排着队的新画面换上最早的一帧
///
/// 返回换上的这一帧在队里等了多久（没有可换的是 `None`）：等得很短说明框架跟得上
pub(crate) fn refresh(shared: &Shared) -> Option<Duration> {
    #[cfg(target_os = "macos")]
    {
        let page = &shared.page;
        let gpu = page.gpu()?;
        let mut state = page.render();
        state.starved = 0;
        let (frame, arrived) = state.queue.pop_front()?;
        if state.open {
            copy(page, &gpu, &mut state, &frame.texture, Instant::now(), Some(Box::new(frame.surface)));
        }
        Some(arrived.elapsed())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = shared;
        None
    }
}

/// 画好的帧一直没人取：让内核降到很低的帧率（主线程上）。框架又跟得上时元素把它升回去
#[cfg(target_os = "macos")]
fn throttle(shared: &std::rc::Rc<Shared>) {
    if shared.throttled.replace(true) {
        return;
    }
    let host = shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
    if let Some(host) = host {
        tracing::debug!("CEF 视图：画好的帧没人取（窗口看不见？），内核降到每秒 {STARVED_RATE} 帧");
        cef::ImplBrowserHost::set_windowless_frame_rate(&host, STARVED_RATE);
    }
}

/// 把 `size` 的像素写到 `texture` 的 `at`（物理像素），超出纹理的部分不写
fn write_region(gpu: &Gpu, texture: &wgpu::Texture, pixels: &[u8], size: [u32; 2], at: [u32; 2]) {
    let width = size[0].min(texture.width().saturating_sub(at[0]));
    let height = size[1].min(texture.height().saturating_sub(at[1]));
    if width == 0 || height == 0 {
        return;
    }
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d { x: at[0], y: at[1], z: 0 }, aspect: wgpu::TextureAspect::All },
        pixels,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(size[0] * 4), rows_per_image: Some(size[1]) },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
}

/// 要等 GPU 复制完时（命令轮不过来、共享纹理不是轮着用的）最多等多久：GPU 卡住时不让回调一直等
#[cfg(target_os = "windows")]
const WAIT_LIMIT: Duration = Duration::from_millis(100);

/// 交来的纹理（macOS 包好的 IOSurface、通用导入复制出来的）在 wgpu 的队列上复制进登记的纹理，不等。
/// `begin` 是回调开始的时刻（导入的时间也算进统计）；`done` 在复制做完时（GPU 上）丢掉：交来的 IOSurface 在那之前记着在用
fn copy(page: &Page, gpu: &Arc<Gpu>, state: &mut RenderState, source: &wgpu::Texture, begin: Instant, done: Option<Box<dyn std::any::Any + Send>>) {
    let size = [source.width(), source.height()];
    let (texture, fresh) = state.target(gpu, size, source.format());
    let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("CEF 视图的一帧") });
    encoder.copy_texture_to_texture(source.as_image_copy(), texture.as_image_copy(), source.size());
    gpu.queue.submit([encoder.finish()]);
    if let Some(done) = done {
        gpu.queue.on_submitted_work_done(move || drop(done));
    }
    page.set_texture(state.finish(fresh));
    page.note_arrival();
    state.stats.record(begin.elapsed(), Duration::ZERO);
}

/// 弹出层的 GPU 共享纹理（Windows）：复制进弹出层自己的纹理，叠到页面上。别的平台还没有
fn accelerated_popup(page: &Page, info: &AcceleratedPaintInfo) {
    #[cfg(target_os = "windows")]
    {
        let begin = Instant::now();
        let Some(gpu) = page.gpu() else { return };
        let scale = page.geometry().scale;
        let mut state = page.render();
        let Some(at) = state.popup_at(scale).filter(|_| state.open) else { return };
        // SAFETY: 在 CEF 的离屏回调里，回调期间共享纹理的句柄有效
        let source = match unsafe { state.opened_popup.open(&gpu, info) } {
            Ok(source) => source,
            Err(reason) => return tracing::debug!("弹出层的共享纹理打不开（{reason}）"),
        };
        let copied = d3d12::copier(&mut state.copier, &gpu).and_then(|copier| copier.copy_popup(&gpu, &source, at, WAIT_LIMIT));
        match copied {
            Ok(copied) => {
                state.opened_popup.copied(source.index, copied.value);
                page.set_texture(state.install(&gpu, copied.texture, false));
                state.stats.record(begin.elapsed(), copied.waited);
            }
            Err(reason) => tracing::debug!("弹出层的这一帧没复制（{reason}）"),
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (page, info);
}

/// 弹出层开了、关了（回调的线程上）。关了时之后的页面帧不再叠它，让 CEF 重画一帧干净的页面
fn popup_shown(page: &Page, show: bool) {
    {
        let mut state = page.render();
        if !show {
            state.popup = None;
            state.popup_pixels = None;
            #[cfg(target_os = "windows")]
            if let Some(copier) = state.copier.as_mut() {
                copier.hide_popup();
            }
        }
    }
    page.on_main(move |shared| {
        wake(shared);
        let host = shared.browser.borrow().as_ref().and_then(cef::ImplBrowser::host);
        if let Some(host) = host.filter(|_| !show) {
            cef::ImplBrowserHost::invalidate(&host, PaintElementType::default());
        }
    });
}

/// GPU 共享纹理：按平台走快路径，不行时用 cef 自带的通用导入（复制成一张自己的纹理、等复制完）
fn accelerated(page: &Page, info: &AcceleratedPaintInfo) {
    use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;

    let begin = Instant::now();
    let Some(gpu) = page.gpu() else { return };
    let mut state = page.render();
    if !state.open {
        return;
    }
    // macOS：IOSurface 直接包成纹理，只复制一次，不等
    #[cfg(target_os = "macos")]
    {
        // SAFETY: 在 CEF 的离屏回调里，回调期间 IOSurface 有效
        match unsafe { iosurface::wrap(&gpu.device, info) } {
            // 内核自己出帧时不马上换：排队，框架出帧时换上最早的一帧（屏幕的一次刷新换一帧）
            Ok(frame) if crate::engine::pacing() == crate::engine::Pacing::Kernel => {
                let mut starving = false;
                if state.queue.len() >= QUEUE {
                    state.queue.pop_front();
                    page.note_dropped();
                    state.starved += 1;
                    starving = state.starved == STARVED;
                }
                state.queue.push_back((frame, begin));
                drop(state);
                if starving {
                    // 不在这个回调里调回内核：排到事件循环的下一轮
                    page.post_main(throttle);
                }
                return;
            }
            Ok(frame) => return copy(page, &gpu, &mut state, &frame.texture, begin, Some(Box::new(frame.surface))),
            Err(reason) => tracing::debug!("IOSurface 的快路径不能用（{reason}），走通用的导入"),
        }
    }
    // Windows：共享纹理在框架的 D3D12 设备上直接打开（打开过的留着），在自己的队列上复制一次
    #[cfg(target_os = "windows")]
    {
        // SAFETY: 在 CEF 的离屏回调里，回调期间共享纹理的句柄有效
        match unsafe { state.opened.open(&gpu, info) } {
            Ok(source) => {
                let copied = d3d12::copier(&mut state.copier, &gpu).and_then(|copier| copier.copy(&gpu, &source, WAIT_LIMIT));
                match copied {
                    Ok(copied) => {
                        state.opened.copied(source.index, copied.value);
                        page.set_texture(state.install(&gpu, copied.texture, copied.fresh));
                        page.note_arrival();
                        state.stats.record(begin.elapsed(), copied.waited);
                    }
                    Err(reason) => tracing::debug!("CEF 视图的这一帧没复制（{reason}）"),
                }
                return;
            }
            Err(reason) => tracing::debug!("D3D12 的快路径不能用（{reason}），走通用的导入"),
        }
    }
    let handle = SharedTextureHandle::new(info);
    if let SharedTextureHandle::Unsupported = handle {
        tracing::warn!("这个平台上 CEF 视图拿不到 GPU 共享纹理");
        return;
    }
    // SAFETY: CEF 保证回调期间共享纹理有效；导入时复制成一张自己的纹理、等复制完才返回，
    // 之后 CEF 可以接着用它的那张。共享纹理和框架的设备在同一块 GPU 上（离屏渲染按默认的适配器）
    match unsafe { handle.import_texture(&gpu.device, &gpu.queue) } {
        Ok(texture) => copy(page, &gpu, &mut state, &texture, begin, None),
        Err(error) => tracing::warn!("导入 CEF 视图的共享纹理失败：{error:?}"),
    }
}

wrap_render_handler! {
    struct Painter {
        page: Arc<Page>,
        accessibility: AccessibilityHandler,
    }

    impl RenderHandler {
        /// 读屏：内核的无障碍树的变化（辅助技术打开以后才有）
        fn accessibility_handler(&self) -> Option<AccessibilityHandler> {
            Some(self.accessibility.clone())
        }

        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let size = self.page.geometry().size;
            // CEF 要求大小不为 0
            rect.x = 0;
            rect.y = 0;
            rect.width = (size[0].round() as i32).max(1);
            rect.height = (size[1].round() as i32).max(1);
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            let Some(info) = info else { return 0 };
            let scale = self.page.geometry().scale;
            info.device_scale_factor = if scale > 0.0 { scale } else { 1.0 };
            1
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let size = [width as u32, height as u32];
            // SAFETY: CEF 保证回调期间 `buffer` 指向 width × height 个 BGRA 像素（每像素 4 字节、逐行紧密排列）
            let pixels = unsafe { std::slice::from_raw_parts(buffer, size[0] as usize * size[1] as usize * 4) };
            upload(&self.page, pixels, size, type_ != PaintElementType::default());
            painted(&self.page);
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            if type_ == PaintElementType::default() {
                accelerated(&self.page, info);
            } else {
                accelerated_popup(&self.page, info);
            }
            painted(&self.page);
        }

        /// 页面里的弹出层（下拉框、日期选择这类）开了、关了：CEF 另外画它（`PET_POPUP`），叠在页面上
        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            popup_shown(&self.page, show != 0);
        }

        /// 页面开始拖（页面里的元素、选中的文字）：在网页视图里接着拖（见 `input::page_drag_started`）
        fn start_dragging(
            &self,
            _browser: Option<&mut Browser>,
            drag_data: Option<&mut DragData>,
            allowed_ops: DragOperationsMask,
            x: ::std::os::raw::c_int,
            y: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(data) = drag_data.map(|data| <DragData as Clone>::clone(data)) else { return 0 };
            let allowed = *allowed_ops.as_ref();
            self.page.on_main(move |shared| crate::input::page_drag_started(shared, data, allowed, [x, y]));
            1
        }

        /// 页面说这会儿能放下什么操作
        fn update_drag_cursor(&self, _browser: Option<&mut Browser>, operation: DragOperationsMask) {
            let operation = *operation.as_ref();
            self.page.on_main(move |shared| crate::input::drag_cursor(shared, operation));
        }

        /// 弹出层在页面里的位置和大小（逻辑像素），在它的第一帧之前给
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            if let Some(rect) = rect {
                self.page.render().popup = Some([rect.x, rect.y, rect.width, rect.height]);
            }
        }

        fn on_ime_composition_range_changed(
            &self,
            _browser: Option<&mut Browser>,
            _selected_range: Option<&Range>,
            character_bounds: Option<&[Rect]>,
        ) {
            // 候选框要的是位置，范围只说明是文档里的哪几个字，不用传
            let bounds = character_bounds.unwrap_or(&[]).to_vec();
            self.page.on_main(move |shared| crate::input::ime_range_changed(shared, &bounds));
        }
    }
}

/// 离屏绘制的回调
pub(crate) fn handler(page: &Arc<Page>) -> Option<RenderHandler> {
    Some(Painter::new(page.clone(), crate::accessibility::handler(page)))
}
