//! 打印：系统的打印对话框选好打印机和纸张，应用按纸张排出每一页（元素），这里把每一页离屏画成位图
//! （打印机的分辨率，最多 [`MAX_DPI`]），交给打印机。
//!
//! 每一页是一棵独立的元素树（不在窗口里）：按能印到的范围排版，按浅色外观画（纸是白的），等图片加载完
//! （最多 [`IMAGE_WAIT`]），动画直接到终点。画用的是窗口共用的 GPU 设备。

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use aureole_core::host::{self, Compositor, DragOutRequest, HostWindow, RenderFrame, TreeUpdate, WindowHost, WindowTree};
use aureole_core::{Appearance, Cursor, Element, ImeRequest, Scope};
use winit::raw_window_handle::HasWindowHandle;
use render::Gpu;

/// 页最多画成每英寸这么多点：再高纸上看不出来，位图却按平方变大
const MAX_DPI: f32 = 300.0;
/// 页上的图片最多等这么久（秒）：还没加载完的按没有画
const IMAGE_WAIT: f64 = 10.0;
/// 逻辑像素是 1/96 英寸，点是 1/72 英寸
const POINT: f32 = 96.0 / 72.0;

thread_local! {
    /// 窗口共用的 GPU 设备（第一个窗口打开时选好）。不拉住它：设备归事件循环，线程结束时不在这里释放
    static GPU: RefCell<Weak<Gpu>> = const { RefCell::new(Weak::new()) };
}

/// 窗口选好、换了 GPU 设备时记下来，打印时用它画
pub(crate) fn set_gpu(gpu: &Arc<Gpu>) {
    GPU.with(|slot| *slot.borrow_mut() = Arc::downgrade(gpu));
}

/// 窗口共用的 GPU 设备：第一个窗口打开以后才有；设备丢失、换了新设备以后是新的那个。只在主线程上有。
///
/// 给自己往 GPU 纹理里画、交给渲染器合成的代码用（[`render::ExternalTexture`]）：纹理要建在这个设备上。
/// 拿到的是这一刻的设备，存起来用时对一下 [`render::Gpu::id`]，变了就在新设备上重建
pub fn shared_gpu() -> Option<Arc<Gpu>> {
    GPU.with(|slot| slot.borrow().upgrade())
}

/// 一页纸，尺寸都是逻辑像素（1/96 英寸，和屏幕上的逻辑像素一样大）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSetup {
    /// 每页能印到的范围：每页的元素按这个大小排版，印的时候铺满它
    pub size: [f32; 2],
    /// 整张纸
    pub paper: [f32; 2],
    /// 能印到的范围在纸上的左上角（打印机印不到纸的边）
    pub origin: [f32; 2],
}

/// 打印的结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrintOutcome {
    /// 交给打印机了（进了系统的打印队列）
    Printed,
    /// 用户在打印对话框里（或者存成 PDF 时问文件名的地方）取消了，或者排出来一页都没有
    Cancelled,
    /// 印不了：平台不能打印（Linux）、没有窗口、打印机或者 GPU 出错
    Failed(String),
}

/// 打印：弹出系统的打印对话框，用户选好打印机和纸张以后，按纸张排出每一页、画好交给打印机。异步：对话框开着时界面照常运行。
///
/// 每一页是一个元素，按能印到的范围（[`PageSetup::size`]）排版；分页是应用的事（一页放什么由应用决定）。
/// 页按浅色外观画（纸是白的），图片等加载完再画，动画直接到终点。
///
/// ```no_run
/// # use aureole::prelude::*;
/// # use aureole::{PageSetup, PrintJob, PrintOutcome};
/// let lines: Vec<String> = (1..=120).map(|i| format!("第 {i} 行")).collect();
/// button("打印…").on_click(move || {
///     let lines = lines.clone();
///     aureole::spawn_local(async move {
///         // 每页 40 行
///         let job = PrintJob::new(move |page: &PageSetup| {
///             lines.chunks(40).map(|chunk| column().size(page.size[0], page.size[1]).children(chunk.iter().map(|line| text(line.clone())))).collect::<Vec<_>>()
///         });
///         if let PrintOutcome::Failed(error) = job.title("清单").print().await {
///             eprintln!("没印出来：{error}");
///         }
///     });
/// });
/// ```
pub struct PrintJob {
    title: String,
    pages: Pages,
}

/// 排出每一页
type Pages = Box<dyn FnOnce(&PageSetup) -> Vec<Element>>;

impl PrintJob {
    /// `pages` 在用户选好纸张以后调用一次，排出每一页
    pub fn new<P, I>(pages: P) -> Self
    where
        P: FnOnce(&PageSetup) -> I + 'static,
        I: IntoIterator,
        I::Item: Into<Element>,
    {
        Self { title: String::new(), pages: Box::new(move |page| pages(page).into_iter().map(Into::into).collect()) }
    }

    /// 打印队列里显示的名字（默认是发起打印的窗口的标题）
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// 弹出打印对话框、印。要在主线程上等（`aureole::spawn_local`）
    pub async fn print(self) -> PrintOutcome {
        if !crate::system::capabilities().supports_print {
            return PrintOutcome::Failed("这个平台还不能打印".into());
        }
        // 父窗口：发起的窗口，没有时最近有焦点的
        let parent = crate::window::current_window().or_else(crate::window::front_window).and_then(crate::window::native);
        let title = if self.title.is_empty() { parent.as_ref().map(|w| w.title()).unwrap_or_default() } else { self.title };
        let (done, chosen) = callback();
        crate::system::backend().dialog().print_dialog(parent.as_deref().map(|w| w as &dyn HasWindowHandle), done);
        let setup = match chosen.await {
            Ok(Some(setup)) => setup,
            Ok(None) => return PrintOutcome::Cancelled,
            Err(error) => return PrintOutcome::Failed(error.to_string()),
        };
        let page = page_setup(setup.paper, setup.printable);
        let pages = match render_pages(page, setup.dpi, self.pages).await {
            Ok(pages) => pages,
            Err(error) => {
                release(setup);
                return PrintOutcome::Failed(error);
            }
        };
        if pages.is_empty() {
            release(setup);
            return PrintOutcome::Cancelled;
        }
        let (done, printed) = callback();
        crate::system::backend().dialog().print(setup, &title, pages, done);
        match printed.await {
            Ok(true) => PrintOutcome::Printed,
            Ok(false) => PrintOutcome::Cancelled,
            Err(error) => PrintOutcome::Failed(error.to_string()),
        }
    }
}

/// 不印了：交空的页，平台释放对话框给的打印机
fn release(setup: platform::PrintSetup) {
    crate::system::backend().dialog().print(setup, "", Vec::new(), Box::new(|_| {}));
}

/// 对话框选好的纸张（点）换成逻辑像素
fn page_setup(paper: [f32; 2], printable: [f32; 4]) -> PageSetup {
    let [x, y, width, height] = printable.map(|v| v * POINT);
    PageSetup { size: [width, height], paper: paper.map(|v| v * POINT), origin: [x, y] }
}

/// 页画成的缩放：打印机的分辨率（最多 [`MAX_DPI`]）换成每逻辑像素几个像素，长边不超过纹理的上限
fn page_scale(size: [f32; 2], dpi: f32) -> f32 {
    let scale = dpi.clamp(72.0, MAX_DPI) / 96.0;
    let longest = size[0].max(size[1]).max(1.0);
    scale.min(render::max_texture_size() as f32 / longest)
}

/// 排出每一页、等图片、画成位图
async fn render_pages(page: PageSetup, dpi: f32, pages: Pages) -> Result<Vec<platform::RgbaImage>, String> {
    let gpu = GPU.with(|slot| slot.borrow().upgrade()).ok_or("还没有 GPU 设备（没有打开过窗口）")?;
    let gpu = if gpu.is_lost() { gpu.recover(None).map_err(|error| format!("GPU 不可用：{error}"))? } else { gpu };
    let scale = page_scale(page.size, dpi);
    let scope = Scope::new();
    let views = host::with_appearance(Appearance::Light, || scope.run(|| pages(&page)));
    let hosts: Vec<WindowHost<PageWindow>> = views
        .into_iter()
        .map(|view| {
            host::with_appearance(Appearance::Light, || {
                let tree = WindowTree::new(page.size);
                tree.set_scale(scale);
                scope.run(|| {
                    tree.attach_metrics(None);
                    tree.mount(view);
                });
                let window = PageWindow { compositor: RefCell::new(Compositor::new(Some(tree.handback()))), scale, time: Cell::new(0.0) };
                WindowHost::new(tree, window)
            })
        })
        .collect();
    // 图片在后台解码：事件循环照常转，解码好了下一帧画上；还在加载的按没有画
    let deadline = host::clock::now() + IMAGE_WAIT;
    loop {
        host::with_appearance(Appearance::Light, || {
            for h in &hosts {
                h.frame(host::clock::now());
            }
        });
        if aureole_core::image_cache_stats().loading_images == 0 || host::clock::now() > deadline {
            break;
        }
        aureole_core::sleep(Duration::from_millis(20)).await;
    }
    // 动画（图片淡入、过渡）直接到终点：最后一帧按很久以后的时刻推进
    let end = host::clock::now() + 3600.0;
    let mut images = Vec::with_capacity(hosts.len());
    for h in &hosts {
        let image = host::with_appearance(Appearance::Light, || {
            h.frame(end);
            h.window().render(&gpu, h.tree(), page.size)
        });
        images.push(image);
    }
    drop(hosts);
    scope.dispose();
    Ok(images)
}

/// 页这一半"窗口"：交来的帧推进合成层（和测试工具的一样），画的时候读它的变换
struct PageWindow {
    compositor: RefCell<Compositor>,
    scale: f32,
    /// 最后一帧的时刻（给自定义着色器）
    time: Cell<f64>,
}

impl PageWindow {
    /// 离屏画成白底的位图
    fn render(&self, gpu: &Gpu, tree: &WindowTree, size: [f32; 2]) -> platform::RgbaImage {
        let [width, height] = size.map(|v| ((v * self.scale).round() as u32).max(1));
        let viewport = render::Viewport { width, height, scale_factor: self.scale };
        let layers = self.compositor.borrow().transforms();
        let time = Duration::from_secs_f64(self.time.get().max(0.0));
        let pixels = render::render_to_image(gpu, &tree.display_list(), &layers, &tree.atlas_snapshot(), viewport, render::Color::WHITE, time);
        platform::RgbaImage { width, height, rgba: Arc::from(pixels) }
    }
}

impl HostWindow for PageWindow {
    fn scale_factor(&self) -> f32 {
        self.scale
    }

    fn submit(&self, now: f64, frame: Option<RenderFrame>) {
        self.time.set(now);
        let mut compositor = self.compositor.borrow_mut();
        match frame {
            Some(RenderFrame { scene, layers, atlas: _ }) => {
                compositor.advance_scrolls(layers.time);
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

    fn set_ime(&self, _: Option<ImeRequest>) {}

    fn set_cursor(&self, _: Cursor) {}

    fn accessibility(&self, _: TreeUpdate) {}

    fn begin_drag_out(&self, request: DragOutRequest) {
        request.result.set(Some(false));
    }

    fn focus_changed(&self, _: bool) {}
}

/// 平台的回调（可能在别的线程）换成主线程上等的未来值
fn callback<T: Send + 'static>() -> (Box<dyn FnOnce(T) + Send>, Callback<T>) {
    let state: Arc<Mutex<(Option<T>, Option<Waker>)>> = Arc::default();
    let sender = state.clone();
    let done = Box::new(move |value: T| {
        let waker = {
            let mut state = sender.lock().unwrap_or_else(|e| e.into_inner());
            state.0 = Some(value);
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    });
    (done, Callback(state))
}

/// 等平台的回调
struct Callback<T>(Arc<Mutex<(Option<T>, Option<Waker>)>>);

impl<T> Future for Callback<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match state.0.take() {
            Some(value) => Poll::Ready(value),
            None => {
                state.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
