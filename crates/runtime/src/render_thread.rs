//! 渲染线程：上传图元、推进合成层动画和滚动、绘制和呈现，UI 逻辑全在主线程。
//!
//! 平时跟着主线程的节奏：主线程每帧发一条消息，显示列表变了、合成层在动或者在滚动才重画。
//! 合成层有动画在动（或者有视口在滑、在弹）、而主线程超过约 1.5 帧没来消息（卡在同步 IO、大量计算上）时，
//! 渲染线程不再等它，按垂直同步自己出帧，每帧只上传层的变换，这些动画和滚动照样流畅；
//! 滚动推进到的状态交回主线程（恢复后命中测试、虚拟列表用它）。主线程恢复后回到它的节奏。
//! 窗口被完全挡住时系统会给主线程降频，这时不自己出帧。表面给不出纹理（窗口最小化、被挡住，显示器睡着、锁着屏）时
//! 这一帧跳过，不按刷新间隔重试：等下一条消息（主线程的帧、遮挡变化、改大小）来了再画，画面静止时渲染线程照样睡着。
//!
//! 改窗口大小时不用旧布局抢着画（旧布局画在新尺寸上位置是错的，下一帧又弹回来）：[`RenderMsg::Resize`] 之后等
//! [`RenderMsg::ResizeFrameDone`]，主线程在这之间按新大小出一帧。macOS 上这一帧画好后交回主线程呈现（[`DrawnFrame`]），
//! 进系统改窗口大小的事务，画面和窗口的新大小同时生效；Windows 上渲染线程画完自己呈现，呈现了告诉在改大小的消息里等着的主线程
//! （[`ResizeSync::Presented`]），新画面赶上窗口新大小的那一次合成；别的平台画完直接呈现。积压的几次改大小只按最后一次配置表面。
//!
//! 表面平时是 8 位 sRGB；主线程决定要扩展范围的输出（画面里有广色域、HDR 的内容，显示器又支持，
//! 见 `display`）时发 [`RenderMsg::Output`]，这里换成半精度浮点、线性扩展 sRGB 的表面，并记下显示器的余量。
//!
//! GPU 设备丢失（驱动重置、显卡被拔掉）时，第一个发现的渲染线程重新创建设备（整个进程只试一次，几个窗口共用新设备），
//! 用它重建渲染器、重新配置表面，再请主线程把图集和图片的完整内容、此刻的显示列表交过来（[`RenderMsg::Refill`]），之后照常出帧。
//! 再丢失或者重新创建失败时交给主线程说明原因后退出。
//!
//! 渲染线程里出错（panic）时不让窗口停住：接住它，记一条错误，把渲染线程的状态（表面、设备、合成层）
//! 连同收消息的一端交回主线程（[`take_crashed`]），线程结束。主线程为这个窗口重建渲染线程（[`Crashed::restart`]）：
//! 新线程重新配置表面、重建渲染器，主线程下一帧交来图集和图片的完整内容、此刻的显示列表，和设备丢失后的重建同一条路。
//!
//! 显示列表上传完就放手，不留着整份：主线程只修补了几个图元时直接改它那一份（没人拿着就不用复制），整个重新生成时
//! 拿上一份的容量来写（双缓冲）。积压的几帧里的显示列表只上传最后一份；都只修补过时合起来，只上传改过的几段。
//! 同一个窗口 10 秒内第三次出错时不再重建，按没有可用的 GPU 处理（说明原因后退出）。

use aureole_core::MainThreadSender;
use std::any::Any;
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use platform::PresentFeedback;
use render::{Gpu, Renderer};
use scene::{AtlasUpdate, Color, DisplayList, Viewport};

use aureole_core::host::{Compositor, Handback, LayerUpdates, Scene};

use crate::window::WindowHandle;

/// 分组缓存的统计（[`group_cache_stats`]）
pub use render::CacheStats as GroupCacheStats;

/// 所有窗口的分组缓存的统计：累计用缓存合成了多少次、画进离屏纹理多少次，此刻缓存了多少个分组、占多少显存
pub fn group_cache_stats() -> GroupCacheStats {
    render::cache_stats()
}

/// 每个窗口的分组缓存最多占多少字节的显存（默认 64 MB），超出时最久没用的先赶出去；0 表示不缓存
pub fn set_group_cache_budget(bytes: u64) {
    render::set_cache_budget(bytes);
}

/// 改大小时把画好的一帧交回主线程呈现。没有缓冲：主线程等不及先走了的话，渲染线程当场知道，自己呈现
pub(crate) type PresentHandoff = SyncSender<DrawnFrame>;

/// 改大小之后的这一帧怎么和窗口的新大小对上
pub(crate) enum ResizeSync {
    /// 画好的纹理交回主线程呈现，进系统改窗口大小的事务（macOS）
    Handoff(PresentHandoff),
    /// 渲染线程自己呈现，呈现了发一条（Windows）：主线程在改大小的消息里等着，等到了才让系统接着改，
    /// 新画面赶上窗口新大小的那一次合成。没画就放掉，主线程马上不等
    Presented(SyncSender<()>),
}

/// 渲染线程画好、等主线程呈现的一帧。呈现完渲染线程才接着走：一个表面同时只能取出一张纹理
pub(crate) struct DrawnFrame {
    texture: wgpu::SurfaceTexture,
    queue: wgpu::Queue,
    /// 放掉就是告诉渲染线程呈现完了
    _presented: SyncSender<()>,
}

impl DrawnFrame {
    /// 呈现：进调用线程的系统动画事务（渲染线程取纹理前设好了，见 [`render::set_surface_present_in_transaction`]）
    pub(crate) fn present(self) {
        self.queue.present(self.texture);
    }
}

pub(crate) enum RenderMsg {
    /// 主线程推进了一帧。`scene` 为 `None` 表示显示列表没变；
    /// `layers` 是合成层的变化（动画的新状态、卸载的层、改过的滚动状态、这一帧的时间；放在堆上，消息本身小）；
    /// `atlas` 是这一帧新放进字形图集的内容，必须在绘制前上传
    Frame { scene: Option<Scene>, layers: Box<LayerUpdates>, atlas: Vec<AtlasUpdate> },
    Resize(Viewport),
    /// 改大小之后这一帧的消息发完了（[`RenderMsg::Resize`] 之后到这条之前不画：新布局还没到，旧布局画在新尺寸上位置是错的）。
    /// 带着 [`ResizeSync`] 时，这一帧按它和窗口的新大小对上
    ResizeFrameDone(Option<ResizeSync>),
    /// 窗口被完全挡住 / 重新露出来
    Occluded(bool),
    /// 表面的颜色输出：扩展范围（广色域、HDR）还是 8 位 sRGB，显示器此刻的余量（8 位时是 1）
    Output { extended: bool, headroom: f32 },
    /// 窗口的材质在透明和不透明之间换了：表面的透明度模式、清屏的颜色
    Backdrop { transparent: bool, background: Color },
    /// 设备丢失、渲染线程出错重建后：图集和图片的完整内容（之前的增量更新作废），和此刻完整的显示列表
    /// （渲染线程上传完就放手，不留着整份列表）
    Refill { atlas: Vec<AtlasUpdate>, list: Arc<DisplayList> },
    Quit,
}

/// 窗口的渲染器：建的时候不编译管线（第一帧在渲染线程上编它用到的），第一帧画完以后在后台把常用的管线先编好
fn window_renderer(gpu: &Gpu, format: wgpu::TextureFormat, transparent: bool) -> Renderer {
    let mut renderer = Renderer::from_gpu(gpu, format);
    renderer.set_precompile(true);
    renderer.set_transparent_output(transparent);
    renderer
}

/// 表面的透明度模式：透明的窗口（有窗口材质）用预乘透明度，别的窗口不透明（有的话；系统合成的对象上的表面默认的第一种是"自动"，
/// 交换链按未指定建），没有时用表面默认的第一种
fn alpha_mode(caps: &wgpu::SurfaceCapabilities, transparent: bool) -> wgpu::CompositeAlphaMode {
    let transparent = transparent.then(|| render::transparent_alpha_mode(caps)).flatten();
    let opaque = caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque).then_some(wgpu::CompositeAlphaMode::Opaque);
    transparent.or(opaque).unwrap_or(caps.alpha_modes[0])
}

/// 积压的消息里要上传的显示列表：整份新的，或者只修补过（改过的几段，几帧的合在一起）
enum Upload {
    Full(Arc<DisplayList>),
    Patch(Arc<DisplayList>, Vec<std::ops::Range<u32>>),
}

impl Upload {
    /// 积压的又一份：整份新的就换成它；只修补过的接在前面的上（前面有整份新的时，还是按整份上传最新的这份）
    fn merge(before: Option<Self>, list: Arc<DisplayList>, patch: Option<Vec<std::ops::Range<u32>>>) -> Self {
        match (before, patch) {
            (_, None) | (Some(Self::Full(_)), Some(_)) => Self::Full(list),
            (None, Some(quads)) => Self::Patch(list, quads),
            (Some(Self::Patch(_, mut earlier)), Some(quads)) => {
                earlier.extend(quads);
                earlier.sort_by_key(|r| r.start);
                Self::Patch(list, earlier)
            }
        }
    }
}

/// 所有窗口的渲染线程共用一个设备。配置表面时 wgpu 要先等设备空闲，等的时候别的线程提交了就算配置失败（`GpuWaitTimeout`），
/// 表面留在旧大小、渲染器却按新大小画：一个窗口改大小时另一个窗口在动画就会碰上。配置拿写锁，提交一帧拿读锁：
/// 窗口之间照常一起画，配置的那一下没人提交
static DEVICE_GATE: RwLock<()> = RwLock::new(());

/// 配置表面（见 [`DEVICE_GATE`]）
fn configure(surface: &wgpu::Surface<'_>, device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) {
    let _exclusive = DEVICE_GATE.write().unwrap_or_else(|e| e.into_inner());
    surface.configure(device, config);
}

/// 改大小的那一帧呈现以后最多等 GPU 这么久（一般不到 1 毫秒）
const RESIZE_GPU_WAIT: Duration = Duration::from_millis(20);

/// 主线程两帧之间的间隔，开始时按 60Hz 估计，之后按实际测量
const INITIAL_INTERVAL: Duration = Duration::from_micros(16_667);

/// 同一个窗口的渲染线程在这段时间里出错到 [`MAX_CRASHES`] 次就不再重建
const CRASH_WINDOW: Duration = Duration::from_secs(10);
const MAX_CRASHES: usize = 3;

pub(crate) struct RenderThread {
    surface: wgpu::Surface<'static>,
    /// 表面支持的格式和色彩空间：换输出方式时从里面选
    caps: wgpu::SurfaceCapabilities,
    gpu: Arc<Gpu>,
    config: wgpu::SurfaceConfiguration,
    scale: f32,
    renderer: Renderer,
    background: Color,
    /// 窗口透明（有窗口材质）：表面带透明度，最后一步换算成系统合成要的样子
    transparent: bool,
    /// 显示列表里有随时间变化的自定义着色器：每帧都要重画（时间是动画时间，和合成层的动画同一个时钟）
    animated_shaders: bool,
    compositor: Compositor,
    /// 主线程每帧消息的平均间隔（约等于刷新间隔）
    tick_interval: Duration,
    /// 窗口被完全挡住：只跟着主线程（系统已经把它降频），不自己出帧
    occluded: bool,
    needs_redraw: bool,
    /// 只有合成层的滚动偏移（或者纹理的内容：动图换帧）变了：重画一次，不用重新上传图元
    needs_composite: bool,
    /// 上次画的时候外部纹理的登记表的版本（[`render::external_revision`]）：变了要重画，显示列表没变也一样
    external: u64,
    /// 表面这会儿给不出纹理（超时、窗口看不见）：要画的留着，等下一条消息来了再试，不空转
    surface_blocked: bool,
    /// 最近一帧主线程的时间，和收到它的时刻：主线程卡住时滚动、合成层的动画按这个对应关系往后推
    anchor: Option<(f64, Instant)>,
    /// 合成层的动画推进到的宿主时刻：跟着主线程时是它最近一帧的时刻，主线程卡住时往后推，不往回走
    host: f64,
    /// 窗口标题：提示里分得清是哪个窗口
    label: String,
    /// 显示器此刻的余量：换了新设备后交给新的渲染器
    headroom: f32,
    /// 换了新设备、在等主线程交来图集的完整内容：这期间不画，帧消息里的增量图集更新丢掉
    awaiting_refill: bool,
    /// 改了大小、新布局还没到：这期间不画（旧布局画在新尺寸上，位置是错的）
    awaiting_resize_frame: bool,
    /// 下一帧画好后交给主线程呈现，或者呈现了告诉它
    handoff: Option<ResizeSync>,
    /// 表面现在设成了呈现时进系统动画事务
    present_in_transaction: bool,
    /// 和主线程共用：置位后主线程下一帧发 [`RenderMsg::Refill`]
    refill: Arc<AtomicBool>,
    /// 测试设备丢失的恢复：到这个时刻主动销毁设备（环境变量 `AUREOLE_SIMULATE_DEVICE_LOSS`，秒）
    simulate_loss: Option<Instant>,
    /// 测试出错后的重建：到时主动 panic（环境变量 `AUREOLE_SIMULATE_RENDER_PANIC`）
    simulate_panic: Option<SimulatedPanic>,
    /// 这个窗口的渲染线程最近出错的时刻：主线程据此决定重建还是放弃
    crashes: CrashHistory,
    /// 呈现返回后回报时刻：没有呈现回调的帧时钟（计时器）把之后的帧对齐到它
    feedback: Option<PresentFeedback>,
    /// 投递到主线程：换了新设备时叫醒它，换不了时让它说明原因后退出
    main: MainThreadSender,
}

impl RenderThread {
    pub(crate) fn new(
        surface: wgpu::Surface<'static>,
        caps: wgpu::SurfaceCapabilities,
        gpu: Arc<Gpu>,
        viewport: Viewport,
        background: Color,
        transparent: bool,
        label: String,
        handback: Arc<Handback>,
        refill: Arc<AtomicBool>,
        feedback: Option<PresentFeedback>,
        main: MainThreadSender,
    ) -> Self {
        // 先用 8 位 sRGB；要扩展范围的输出时主线程会发 `RenderMsg::Output`
        let (format, color_space, _) = render::surface_format(&caps, false).expect("表面没有可用的格式");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space,
            width: viewport.width.clamp(1, scene::max_texture_size()),
            height: viewport.height.clamp(1, scene::max_texture_size()),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: alpha_mode(&caps, transparent),
            view_formats: vec![],
        };
        configure(&surface, &gpu.device, &config);
        render::keep_surface_contents_unscaled(&surface);
        // 测试恢复用的开关：只在调试构建里读，发布的应用没有这条路
        let simulate_loss = cfg!(debug_assertions).then(|| std::env::var("AUREOLE_SIMULATE_DEVICE_LOSS").ok().and_then(|s| s.parse::<f64>().ok())).flatten();
        let simulate_panic = cfg!(debug_assertions).then(|| std::env::var("AUREOLE_SIMULATE_RENDER_PANIC").ok().and_then(|s| SimulatedPanic::parse(&s))).flatten();
        Self {
            renderer: window_renderer(&gpu, format, transparent),
            surface,
            caps,
            gpu,
            config,
            scale: viewport.scale_factor,
            background,
            transparent,
            animated_shaders: false,
            compositor: Compositor::new(Some(handback)),
            tick_interval: INITIAL_INTERVAL,
            occluded: false,
            needs_redraw: true,
            needs_composite: false,
            external: render::external_revision(),
            surface_blocked: false,
            anchor: None,
            host: f64::NEG_INFINITY,
            label,
            headroom: 1.0,
            awaiting_refill: false,
            awaiting_resize_frame: false,
            handoff: None,
            present_in_transaction: false,
            refill,
            simulate_loss: simulate_loss.map(|s| Instant::now() + Duration::from_secs_f64(s.max(0.0))),
            simulate_panic,
            crashes: CrashHistory::default(),
            feedback,
            main,
        }
    }

    /// 在新的线程上运行。`rebuild`：出错后重建的，先重建 GPU 那一侧（[`Self::rebuild`]）。
    /// 线程里出错（panic）时接住：记一条错误，把渲染线程连同收消息的一端交回主线程（[`take_crashed`]），线程结束
    pub(crate) fn spawn(mut self, window: WindowHandle, receiver: Receiver<RenderMsg>, rebuild: bool) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name(format!("render: {}", self.label))
            .spawn(move || {
                if let Some(simulate) = &mut self.simulate_panic {
                    simulate.arm(Instant::now());
                }
                let outcome = guarded(|| {
                    if rebuild {
                        self.rebuild();
                    }
                    self.run(&receiver);
                });
                if let Err(message) = outcome {
                    tracing::error!(window = %self.label, "渲染线程出错（panic），交给主线程处理：{message}");
                    let main = self.main.clone();
                    let crashed = Crashed { window, render: self, receiver, message };
                    // 主线程已经不在（应用在退出）时没人接，直接丢掉
                    main.send(move || CRASHED.with(|c| c.borrow_mut().push(crashed)));
                }
            })
            .expect("启动渲染线程失败")
    }

    fn run(&mut self, receiver: &Receiver<RenderMsg>) {
        let mut last_message = Instant::now();
        // 主线程在按时出帧；为假时说明它卡住了，渲染线程自己出帧
        let mut main_alive = true;
        loop {
            let first = if self.occluded || self.surface_blocked || (!self.animating() && !self.needs_composite) {
                // 没有合成层动画，或者窗口看不见、表面给不出纹理：一切跟着主线程。
                // 渲染器还留着离屏纹理时，等到它们空闲满了醒来归还显存（测试开关模拟出错时，到时也醒来）
                let simulate = self.simulate_panic.as_ref().and_then(|s| s.wait(Instant::now()));
                let wait = match self.renderer.next_release().into_iter().chain(simulate).min() {
                    Some(wait) => receiver.recv_timeout(wait),
                    None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                match wait {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => {
                        self.renderer.release_idle();
                        self.maybe_simulate_panic();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else if !self.animating() {
                // 动画刚停下、上一帧近似地用了缓存的分组：主线程可能已经没事做了（帧时钟会暂停），
                // 最多等它一帧，然后自己按此刻的变换再画一次
                match receiver.recv_timeout(self.tick_interval) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else if main_alive {
                // 合成层在动：照常等主线程这一帧，超过 1.5 帧没来就当它卡住了
                match receiver.recv_timeout(self.tick_interval.mul_f32(1.5)) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => {
                        main_alive = false;
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                // 主线程卡着：不等，有消息（它恢复了）就顺手处理
                match receiver.try_recv() {
                    Ok(msg) => Some(msg),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            };
            // 这一轮的工作从这里开始（之前是在等主线程）
            let work = Instant::now();
            if first.is_some() {
                let now = Instant::now();
                let interval = now - last_message;
                last_message = now;
                if main_alive && interval < Duration::from_millis(50) {
                    self.tick_interval = self.tick_interval.mul_f32(0.9) + interval.mul_f32(0.1);
                }
                main_alive = true;
                // 有新消息了：表面给不出纹理的话再试一次
                self.surface_blocked = false;
            }

            // 一次取完积压的消息，只画最新的一帧：积压的几份显示列表只上传最后一份（只修补过的合起来上传改过的几段）
            let mut upload: Option<Upload> = None;
            let mut resized: Option<Viewport> = None;
            for msg in first.into_iter().chain(receiver.try_iter()) {
                match msg {
                    RenderMsg::Frame { scene, layers, atlas } => {
                        // 合并帧时也要按顺序应用每一帧的图集更新。只有纹理变了（动图换帧）时显示列表不变，重画一次。
                        // 换了新设备、还没收到完整内容时，增量更新作废（完整内容里都有）
                        if !self.awaiting_refill {
                            self.renderer.apply_atlas(&self.gpu.device, &self.gpu.queue, &atlas);
                        }
                        self.needs_composite |= !atlas.is_empty();
                        // 外部纹理（内嵌的 CEF 视图）出了新的一帧：显示列表可能没变，也要重画
                        let external = render::external_revision();
                        self.needs_composite |= external != self.external;
                        self.external = external;
                        // 滚动先推进到主线程这一帧的时间，再收下主线程改过的状态（已经在这个时间上）
                        self.needs_composite |= self.compositor.advance_scrolls(layers.time);
                        self.anchor = Some((layers.time, Instant::now()));
                        if let Some(scene) = scene {
                            // 合成层表按顺序换（只修补过图元的帧，层表没变）；显示列表等积压的都看完再上传。
                            // 换了新设备、在等完整内容时不用管：完整内容里带着此刻的列表
                            if scene.patch.is_none() {
                                self.compositor.set_layers(scene.layers.clone());
                            }
                            if !self.awaiting_refill {
                                upload = Some(Upload::merge(upload, scene.list, scene.patch.map(|p| p.quads)));
                            }
                        }
                        self.needs_composite |= self.compositor.apply(*layers);
                    }
                    RenderMsg::Resize(viewport) => {
                        resized = Some(viewport);
                        self.awaiting_resize_frame = true;
                    }
                    RenderMsg::ResizeFrameDone(handoff) => {
                        self.awaiting_resize_frame = false;
                        self.handoff = handoff;
                        self.needs_redraw = true;
                    }
                    RenderMsg::Occluded(occluded) => self.occluded = occluded,
                    RenderMsg::Output { extended, headroom } => self.set_output(extended, headroom),
                    RenderMsg::Backdrop { transparent, background } => self.set_backdrop(transparent, background),
                    RenderMsg::Refill { atlas, list } => {
                        self.renderer.apply_atlas(&self.gpu.device, &self.gpu.queue, &atlas);
                        upload = Some(Upload::Full(list));
                        self.awaiting_refill = false;
                        self.needs_redraw = true;
                    }
                    RenderMsg::Quit => return,
                }
            }

            // 积压的几次改大小只按最后一次配置表面：重建交换链一次要十几毫秒，拖动窗口边缘时每秒几百次改大小，
            // 一次次重建的话一帧都画不出来，松手后还要把排着的重建做完画面才跟上
            if let Some(viewport) = resized {
                self.resize(viewport);
            }
            if let Some(upload) = upload {
                self.upload(upload);
            }
            // 宿主时刻：跟着主线程时是它最近一帧的时刻（和主线程对同一个时刻采样，值一样）；主线程卡住时按收到那一帧之后
            // 过去的时间往后推。卡完以后主线程的帧可能比推出来的早一点：不往回走
            if let Some((time, at)) = self.anchor {
                let host = if main_alive { time } else { time + at.elapsed().as_secs_f64() };
                self.host = self.host.max(host);
            }
            // 主线程卡住：滚动推进到这个时刻，推进到的状态交回主线程
            if !main_alive && self.anchor.is_some() {
                self.needs_composite |= self.compositor.advance_scrolls(self.host);
                self.compositor.publish();
            }
            if self.simulate_loss.is_some_and(|at| Instant::now() >= at) {
                self.simulate_loss = None;
                tracing::debug!(window = %self.label, "测试：主动销毁 GPU 设备");
                self.gpu.device.destroy();
                // 销毁要等队列里的工作做完才算丢失（丢失的回调在这之后调用）
                let _ = self.gpu.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(1)) });
            }
            self.maybe_simulate_panic();
            // 设备丢失：换新设备；换不了时主线程说明原因后退出，这个线程不再画
            if self.gpu.is_lost() && !self.recover() {
                return;
            }
            // 合成层动画推进到这个宿主时刻，按主线程交来的动画时钟换算成动画时间。不截断：渲染线程被耽误后，
            // 进行中的动画按真实时间追上
            let start = Instant::now();
            let animating = self.compositor.has_moving_layers() || self.shaders_animating();
            if animating {
                self.compositor.advance(self.host);
            }
            // 刚停下的动画也要把终值画出来，之后回到等主线程的节奏。滚动变了的帧由 `needs_composite` 标出来
            if (animating || self.needs_redraw || self.needs_composite) && !self.awaiting_refill && !self.awaiting_resize_frame {
                self.render(work);
            }
            // 这一轮没有画（在等完整内容）：放开等着的主线程
            self.handoff = None;
            // 自己出帧时不超过刷新率：呈现通常已按垂直同步阻塞，窗口被遮住时不一定，防止空转
            if !main_alive {
                let min_frame = self.tick_interval.mul_f32(0.9);
                if let Some(rest) = min_frame.checked_sub(start.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        }
    }

    /// 合成层在动、有视口在滚动，或者有随时间变化的自定义着色器：主线程卡住时自己出帧
    fn animating(&self) -> bool {
        self.compositor.is_animating() || self.shaders_animating()
    }

    /// 有随时间变化的自定义着色器，动画时钟也在走（暂停时它们的时间也停着，不用重画）
    fn shaders_animating(&self) -> bool {
        self.animated_shaders && self.compositor.is_clock_running()
    }

    fn resize(&mut self, viewport: Viewport) {
        if viewport.width == 0 || viewport.height == 0 {
            return;
        }
        // 比设备的纹理边长上限还大的窗口：表面按上限，而不是配置失败（多出来的地方 macOS 上没有内容，别的平台由系统拉伸）
        let max = scene::max_texture_size();
        self.config.width = viewport.width.min(max);
        self.config.height = viewport.height.min(max);
        self.scale = viewport.scale_factor;
        configure(&self.surface, &self.gpu.device, &self.config);
        self.needs_redraw = true;
    }

    /// 换表面的格式和色彩空间（扩展范围 / 8 位 sRGB），记下显示器的余量。sRGB 的内容在两种表面上一样
    fn set_output(&mut self, extended: bool, headroom: f32) {
        self.headroom = headroom;
        self.renderer.set_headroom(headroom);
        if let Some((format, color_space, _)) = render::surface_format(&self.caps, extended)
            && (format, color_space) != (self.config.format, self.config.color_space)
        {
            self.config.format = format;
            self.config.color_space = color_space;
            configure(&self.surface, &self.gpu.device, &self.config);
        }
        self.needs_redraw = true;
    }

    /// 窗口的材质在透明和不透明之间换了：换表面的透明度模式、清屏的颜色，透明时最后一步要换算，重画
    fn set_backdrop(&mut self, transparent: bool, background: Color) {
        self.transparent = transparent;
        self.background = background;
        self.renderer.set_transparent_output(transparent);
        let mode = alpha_mode(&self.caps, transparent);
        if mode != self.config.alpha_mode {
            self.config.alpha_mode = mode;
            configure(&self.surface, &self.gpu.device, &self.config);
        }
        self.needs_redraw = true;
    }

    /// 测试开关 `AUREOLE_SIMULATE_RENDER_PANIC` 到时了：主动 panic
    fn maybe_simulate_panic(&mut self) {
        if self.simulate_panic.as_mut().is_some_and(|s| s.due(Instant::now())) {
            panic!("测试：模拟渲染线程出错（AUREOLE_SIMULATE_RENDER_PANIC）");
        }
    }

    /// 出错后在新的线程上重建 GPU 那一侧（见模块说明）：重新配置表面、重建渲染器；
    /// 图集、图片和显示列表等主线程交来完整内容（主线程重建时已经置位了 `refill`），在那之前不画。合成层的状态沿用出错前的
    fn rebuild(&mut self) {
        tracing::info!(window = %self.label, "重建渲染线程");
        self.awaiting_resize_frame = false;
        self.rebuild_renderer();
        self.needs_redraw = true;
        self.surface_blocked = false;
    }

    /// 重新配置表面、重建渲染器，之后在等主线程交来图集和显示列表的完整内容（设备丢失、出错后的重建共用）
    fn rebuild_renderer(&mut self) {
        // 换了设备时，第一次配置还想在旧设备的交换链上改大小（DX12），失败后丢掉它；再配置一次才在新设备上建交换链
        configure(&self.surface, &self.gpu.device, &self.config);
        configure(&self.surface, &self.gpu.device, &self.config);
        let format = render::surface_format(&self.caps, false).map_or(self.config.format, |(format, ..)| format);
        self.renderer = window_renderer(&self.gpu, format, self.transparent);
        self.renderer.set_headroom(self.headroom);
        self.awaiting_refill = true;
    }

    /// 上传积压的消息里最后一份显示列表：整份的全部上传，只修补过的只传改过的几段。上传完就放手
    fn upload(&mut self, upload: Upload) {
        let (list, patch) = match upload {
            Upload::Full(list) => (list, None),
            Upload::Patch(list, quads) => (list, Some(quads)),
        };
        match patch {
            Some(quads) => self.renderer.patch_display_list(&self.gpu.device, &self.gpu.queue, &list, &quads),
            None => self.renderer.set_display_list(&self.gpu.device, &self.gpu.queue, &list),
        }
        self.animated_shaders = list.animated;
        self.needs_redraw = true;
    }

    /// 设备丢失后换新设备（见模块说明）：重建渲染器和计时器、重新配置表面，请主线程交来图集和显示列表的完整内容。
    /// 换不了时把原因交给主线程（说明后退出），返回 `false`
    fn recover(&mut self) -> bool {
        let gpu = match self.gpu.recover(Some(&self.surface)) {
            Ok(gpu) => gpu,
            Err(error) => {
                tracing::error!(window = %self.label, "{error}");
                if !self.main.send(move || crate::app::gpu_failed(&error)) {
                    std::process::exit(1);
                }
                return false;
            }
        };
        tracing::info!(window = %self.label, gpu = %gpu.info, "换用新的 GPU 设备");
        self.gpu = gpu;
        self.rebuild_renderer();
        self.refill.store(true, Ordering::Release);
        // 主线程可能停着（画面静止时不出帧）：叫醒它，所有窗口恢复帧时钟，下一帧交来图集
        self.main.send(|| {});
        true
    }

    /// `work`：这一轮的工作开始的时刻（记录每帧耗时用）
    fn render(&mut self, work: Instant) {
        let handoff = self.handoff.take();
        let in_transaction = matches!(handoff, Some(ResizeSync::Handoff(_)));
        if in_transaction != self.present_in_transaction {
            self.present_in_transaction = in_transaction;
            render::set_surface_present_in_transaction(&self.surface, self.present_in_transaction);
        }
        let acquire = Instant::now();
        let mut current = self.surface.get_current_texture();
        // 交换链过时了（Vulkan 上窗口刚改了大小）：重新配置后马上再取一次，不然这一帧丢了、要画的留到下一条消息才画
        if matches!(current, wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost) {
            configure(&self.surface, &self.gpu.device, &self.config);
            current = self.surface.get_current_texture();
        }
        let surface_texture = match current {
            wgpu::CurrentSurfaceTexture::Success(texture) | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface_blocked = true;
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                // 设备失效了（丢失、被销毁）：轮询一次，让 wgpu 调用丢失的回调，下一轮换新设备
                let _ = self.gpu.device.poll(wgpu::PollType::Poll);
                return;
            }
            // 超时、窗口看不见：这一帧跳过，等下一条消息再试（不然要画的标记一直在，会按刷新间隔空转）
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.surface_blocked = true;
                return;
            }
        };
        // 等交换链给出纹理的时间不算这一帧的工作
        let waited = acquire.elapsed();
        let viewport = Viewport { width: self.config.width, height: self.config.height, scale_factor: self.scale };
        let layers = self.compositor.transforms();
        let time = Duration::from_secs_f64(self.compositor.animation_time().max(0.0));
        {
            let _submitting = DEVICE_GATE.read().unwrap_or_else(|e| e.into_inner());
            self.renderer.render(&self.gpu, &surface_texture.texture, viewport, &layers, time, self.background, None);
        }
        match handoff {
            Some(ResizeSync::Handoff(main)) => hand_over(&main, surface_texture, self.gpu.queue.clone()),
            Some(ResizeSync::Presented(done)) => {
                self.gpu.queue.present(surface_texture);
                // 等 GPU 画完再告诉主线程：主线程一返回系统就把新的窗口大小交给合成器，这一帧那时要能上屏
                let _ = self.gpu.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(RESIZE_GPU_WAIT) });
                let _ = done.try_send(());
            }
            None => self.gpu.queue.present(surface_texture),
        }
        if let Some(feedback) = &self.feedback {
            feedback.record_present(Instant::now());
        }
        crate::performance::presented();
        aureole_core::host::record_render(work.elapsed().saturating_sub(waited));
        self.needs_redraw = false;
        // 有分组在动、近似地用了缓存的纹理：停下以后再画一帧，按此刻的变换重画它们
        self.needs_composite = self.renderer.needs_refine();
    }
}

/// 画好的一帧交给主线程呈现，等它呈现完。主线程等不及先走了：这里呈现，自己提交系统动画事务
fn hand_over(main: &PresentHandoff, texture: wgpu::SurfaceTexture, queue: wgpu::Queue) {
    let (presented, wait) = sync_channel(0);
    match main.send(DrawnFrame { texture, queue, _presented: presented }) {
        Ok(()) => {
            let _ = wait.recv();
        }
        Err(unwanted) => {
            unwanted.0.present();
            render::flush_surface_transaction();
        }
    }
}

/// 运行 `f`，接住 panic，返回 panic 的信息（状态留在调用方手里，交给主线程重建）
fn guarded(f: impl FnOnce()) -> Result<(), String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|panic| panic_message(&*panic))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(message), _) => (*message).to_owned(),
        (_, Some(message)) => message.clone(),
        _ => "（panic 的值不是文字）".to_owned(),
    }
}

thread_local! {
    /// 主线程：渲染线程出错后交回来、还没处理的
    static CRASHED: RefCell<Vec<Crashed>> = const { RefCell::new(Vec::new()) };
}

/// 主线程：取走渲染线程出错后交回来的（投递过来的工作执行之后调用）
pub(crate) fn take_crashed() -> Vec<Crashed> {
    CRASHED.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// 出错（panic）的渲染线程交回主线程的东西：表面、设备、合成层都还在，渲染器作废；
/// 收消息的一端也在，窗口那一半照常往里发消息
pub(crate) struct Crashed {
    window: WindowHandle,
    render: RenderThread,
    receiver: Receiver<RenderMsg>,
    message: String,
}

impl Crashed {
    /// 出错的是哪个窗口的渲染线程
    pub(crate) fn window(&self) -> WindowHandle {
        self.window
    }

    /// 在新的线程上重建这个窗口的渲染线程：置位补交图集的标志，新线程先重建 GPU 那一侧再照常运行；
    /// 主线程要恢复这个窗口的帧时钟，下一帧把图集和图片的完整内容交过去。
    /// 同一个窗口 [`CRASH_WINDOW`] 之内第 [`MAX_CRASHES`] 次出错时不再重建，返回原因（按没有可用的 GPU 处理）
    pub(crate) fn restart(self) -> Result<JoinHandle<()>, String> {
        let Self { window, mut render, receiver, message } = self;
        if !render.crashes.record(Instant::now()) {
            return Err(format!(
                "窗口「{}」的渲染线程 {} 秒内出错 {} 次，不再重建（最后一次：{message}）",
                render.label,
                CRASH_WINDOW.as_secs(),
                MAX_CRASHES
            ));
        }
        render.refill.store(true, Ordering::Release);
        Ok(render.spawn(window, receiver, true))
    }
}

/// 一个窗口的渲染线程出错的时刻
#[derive(Default)]
struct CrashHistory(Vec<Instant>);

impl CrashHistory {
    /// 记下 `at` 这一次，返回还该不该重建：[`CRASH_WINDOW`] 之内这是第 [`MAX_CRASHES`] 次时不该
    fn record(&mut self, at: Instant) -> bool {
        self.0.retain(|&t| at.saturating_duration_since(t) < CRASH_WINDOW);
        self.0.push(at);
        self.0.len() < MAX_CRASHES
    }
}

/// 测试开关 `AUREOLE_SIMULATE_RENDER_PANIC=秒[,次数]`：渲染线程每次（重新）开始后过这么多秒主动 panic，
/// 一共这么多次（默认 1 次）。`1` 测一次出错后的重建，`1,3` 测 10 秒内第三次出错时放弃
#[derive(Debug, PartialEq)]
struct SimulatedPanic {
    after: Duration,
    remaining: u32,
    /// 这一次到时的时刻；还没开始计时、次数用完时是 `None`
    at: Option<Instant>,
}

impl SimulatedPanic {
    fn parse(value: &str) -> Option<Self> {
        let (seconds, times) = match value.split_once(',') {
            Some((seconds, times)) => (seconds, times.trim().parse().ok()?),
            None => (value, 1),
        };
        let seconds: f64 = seconds.trim().parse().ok()?;
        (seconds.is_finite() && seconds >= 0.0 && times > 0).then(|| Self { after: Duration::from_secs_f64(seconds), remaining: times, at: None })
    }

    /// 渲染线程（重新）开始了：还有次数时从 `now` 起计时
    fn arm(&mut self, now: Instant) {
        self.at = (self.remaining > 0).then(|| now + self.after);
    }

    /// 离到时还有多久（渲染线程空闲时按它醒来）
    fn wait(&self, now: Instant) -> Option<Duration> {
        self.at.map(|at| at.saturating_duration_since(now))
    }

    /// 到时了：用掉一次，返回 `true`
    fn due(&mut self, now: Instant) -> bool {
        if self.at.is_none_or(|at| now < at) {
            return false;
        }
        self.at = None;
        self.remaining -= 1;
        true
    }
}
