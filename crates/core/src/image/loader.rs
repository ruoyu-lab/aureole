//! 图片缓存和后台解码。
//!
//! 缓存在主线程（和响应式运行时一起，所有窗口共用）：同一个来源只解码一次，
//! 用到它的元素各持有一个 [`ImageRef`]，最后一个释放时条目删除；解码好的像素按最近使用
//! 留在一个有预算的缓存里（默认 64MB），很快又用到时（列表滚回来、切回上一张）不用重新解码。
//!
//! 解码在后台线程池里做（最多 4 个线程，按需开、空闲久了退出）。做完把结果投递回主线程（[`MainThreadSender`]，
//! 和任务共用一个收件箱，投递时唤醒事件循环）；主线程在事件循环这一轮、或者下一帧开始时把状态写进条目，
//! 通知还在等它的元素。
//!
//! 解码器出错（坏文件让解码库 panic）不拖垮应用：这一张按失败交回（图片进入错误状态），记一条错误，
//! 线程接着解码别的；线程万一还是退出了，线程池补上一个。
//!
//! SVG 文件（`svg` 特性）走同一条路：后台解析，缓存、预算、出错都一样，只是结果是文档，不是像素。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use reactive::Signal;
use scene::ImageData;

use super::pixels::{self, Decoded, Frames};
use super::{ImageCacheStats, ImageSource, ImageState, SourceKey, SourceKind};
use crate::task::{MainThreadSender, Pool, main_thread_sender, panic_message};

/// 默认留着多少字节没人用的解码结果
const DEFAULT_BUDGET: usize = 64 << 20;

#[derive(Clone)]
pub(crate) enum Status {
    Loading,
    /// `natural` 是原图大小（像素），`data` 可能缩小过；动图的 `data` 是第一帧，`frames` 是全部帧
    Ready { natural: [u32; 2], data: Arc<ImageData>, frames: Option<Arc<Frames>> },
    /// SVG 文件（`svg` 特性）：解析好的文档
    #[cfg(feature = "svg")]
    Vector { document: Arc<super::svg::SvgDocument> },
    Error(Arc<str>),
}

impl Status {
    /// 解码好的像素一共占多少字节（动图是所有帧；SVG 是解析结果）
    fn bytes(&self) -> usize {
        match self {
            Status::Ready { frames: Some(frames), .. } => frames.bytes(),
            Status::Ready { data, .. } => data.bytes(),
            #[cfg(feature = "svg")]
            Status::Vector { document } => document.bytes,
            _ => 0,
        }
    }

    /// 解码好了（图片或者 SVG 文档）
    fn is_ready(&self) -> bool {
        match self {
            Status::Ready { .. } => true,
            #[cfg(feature = "svg")]
            Status::Vector { .. } => true,
            Status::Loading | Status::Error(_) => false,
        }
    }
}

pub(crate) struct Entry {
    id: u64,
    key: SourceKey,
    /// 留着来源：内存里的数据按地址区分，来源活着地址就不会被别的数据复用
    source: ImageSource,
    status: RefCell<Status>,
    /// 还在等解码结果的元素（每个元素一个计数信号，加一就让它的绑定重新执行）
    listeners: RefCell<Vec<Signal<u64>>>,
    /// 没人用了：还没开始解码就不做了
    cancel: Arc<AtomicBool>,
}

/// 对一张图片的引用。克隆便宜；最后一个释放时缓存条目删除（像素进入最近使用的缓存或直接释放）
#[derive(Clone)]
pub(crate) struct ImageRef(Rc<Entry>);

impl ImageRef {
    pub fn id(&self) -> u64 {
        self.0.id
    }

    pub fn same(&self, other: &ImageRef) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn status(&self) -> Status {
        self.0.status.borrow().clone()
    }

    pub fn source(&self) -> &ImageSource {
        &self.0.source
    }

    pub fn state(&self) -> ImageState {
        match &*self.0.status.borrow() {
            Status::Loading => ImageState::Loading,
            Status::Ready { natural, .. } => ImageState::Ready { width: natural[0], height: natural[1] },
            // 固有大小可以是分数：向上取整
            #[cfg(feature = "svg")]
            Status::Vector { document } => ImageState::Ready { width: document.size[0].ceil() as u32, height: document.size[1].ceil() as u32 },
            Status::Error(message) => ImageState::Error(message.clone()),
        }
    }

    /// 解码好之后让 `tick` 加一
    pub fn listen(&self, tick: Signal<u64>) {
        if matches!(*self.0.status.borrow(), Status::Loading) {
            let mut listeners = self.0.listeners.borrow_mut();
            if !listeners.contains(&tick) {
                listeners.push(tick);
            }
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let status = std::mem::replace(self.status.get_mut(), Status::Loading);
        // 线程退出时缓存可能已经析构；正被借用时（不应该发生）只是少做清理，弱引用会在下次查找时被覆盖
        let _ = CACHE.try_with(|cache| {
            let Ok(mut cache) = cache.try_borrow_mut() else { return };
            if cache.entries.get(&self.key).is_some_and(|w| w.strong_count() == 0) {
                cache.entries.remove(&self.key);
            }
            cache.by_id.remove(&self.id);
            if status.is_ready() {
                cache.retain(Retained { key: self.key.clone(), _source: self.source.clone(), status });
            }
        });
    }
}

/// 没人用、但还留着的解码结果
struct Retained {
    key: SourceKey,
    _source: ImageSource,
    status: Status,
}

impl Retained {
    fn bytes(&self) -> usize {
        self.status.bytes()
    }
}

struct Cache {
    entries: HashMap<SourceKey, Weak<Entry>>,
    by_id: HashMap<u64, Weak<Entry>>,
    next: u64,
    /// 最近使用的在后面
    retained: VecDeque<Retained>,
    retained_bytes: usize,
    budget: usize,
    loader: Option<Loader>,
    decoded: usize,
    decode_time: Duration,
}

impl Cache {
    fn retain(&mut self, retained: Retained) {
        let bytes = retained.bytes();
        if bytes > self.budget {
            return;
        }
        self.retained_bytes += bytes;
        self.retained.push_back(retained);
        while self.retained_bytes > self.budget {
            let Some(old) = self.retained.pop_front() else { break };
            self.retained_bytes -= old.bytes();
        }
    }

    fn take_retained(&mut self, key: &SourceKey) -> Option<Retained> {
        let index = self.retained.iter().position(|r| &r.key == key)?;
        let retained = self.retained.remove(index)?;
        self.retained_bytes -= retained.bytes();
        Some(retained)
    }

    /// 一张动图的所有帧最多占多少字节：缓存预算的一半，至少 [`MIN_ANIMATION_CAP`]
    fn animation_cap(&self) -> usize {
        (self.budget / 2).max(MIN_ANIMATION_CAP)
    }
}

/// 动图所有帧的内存上限的下限（预算设成 0 时也能放下一张小动图）
const MIN_ANIMATION_CAP: usize = 4 << 20;

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        entries: HashMap::new(),
        by_id: HashMap::new(),
        next: 0,
        retained: VecDeque::new(),
        retained_bytes: 0,
        budget: DEFAULT_BUDGET,
        loader: None,
        decoded: 0,
        decode_time: Duration::ZERO,
    });
}

/// 取得一个来源的图片：已经有了（正在用或刚用过）就共享，否则交给后台解码
pub(crate) fn acquire(source: &ImageSource) -> ImageRef {
    let key = source.key();
    let existing = CACHE.with(|c| c.borrow().entries.get(&key).and_then(Weak::upgrade));
    if let Some(entry) = existing {
        return ImageRef(entry);
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let retained = cache.take_retained(&key);
        cache.next += 1;
        let id = cache.next;
        let status = retained.as_ref().map_or(Status::Loading, |r| r.status.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let entry = Rc::new(Entry {
            id,
            key: key.clone(),
            source: source.clone(),
            status: RefCell::new(status),
            listeners: RefCell::new(Vec::new()),
            cancel: cancel.clone(),
        });
        cache.entries.insert(key, Rc::downgrade(&entry));
        cache.by_id.insert(id, Rc::downgrade(&entry));
        if retained.is_none() {
            let cap = cache.animation_cap();
            cache.loader.get_or_insert_with(Loader::new).submit(Job { id, source: source.clone(), cancel, cap });
        }
        ImageRef(entry)
    })
}

/// 收下一张后台解码好的图片（解码线程投递过来，在主线程的批量更新里执行，不在借用元素树时）：
/// 写进缓存条目，让在等的元素的绑定重新执行
fn apply(done: Done) {
    let entry = CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let entry = cache.by_id.get(&done.id).and_then(Weak::upgrade)?;
        if done.status.is_ready() {
            cache.decoded += 1;
            cache.decode_time += done.took;
        }
        Some(entry)
    });
    // 条目可能已经没人用了；有人用时，最后一个引用在缓存的借用之外释放（释放时要借缓存）
    let Some(entry) = entry else { return };
    *entry.status.borrow_mut() = done.status;
    for tick in entry.listeners.take() {
        tick.update(|t| *t += 1);
    }
}

/// 等这个线程提交的解码全部做完、结果都投递回来（测试用；结果在下一帧写进条目）。
///
/// 按进展等：解码线程池连续 `stall` 没有交回任何一张才算卡住，这时返回还在等的图片（来源的调试表示）。
/// 别的线程（同一个进程里别的测试）的图片也算进展：线程池是共用的，排在它们后面时自己的一张也还没轮到，
/// 机器忙的时候总共等多久说不准，只要池子还在动就接着等
pub(crate) fn wait_idle(stall: Duration) -> Result<(), Vec<String>> {
    let progress = CACHE.with(|c| c.borrow().loader.as_ref().map(|l| (l.results.clone(), l.submitted)));
    let Some((results, submitted)) = progress else { return Ok(()) };
    let mut seen = DELIVERED.load(Ordering::Acquire);
    loop {
        if crate::task::wait_until(stall, || results.finished.load(Ordering::Acquire) >= submitted) {
            return Ok(());
        }
        let delivered = DELIVERED.load(Ordering::Acquire);
        if delivered == seen {
            let pending = results.pending.lock().unwrap_or_else(PoisonError::into_inner);
            return Err(pending.iter().map(|(_, source)| format!("{source:?}")).collect());
        }
        seen = delivered;
    }
}

pub(crate) fn stats() -> ImageCacheStats {
    CACHE.with(|c| {
        let cache = c.borrow();
        let live: Vec<Rc<Entry>> = cache.by_id.values().filter_map(Weak::upgrade).collect();
        let loading = live.iter().filter(|e| matches!(*e.status.borrow(), Status::Loading)).count();
        let live_bytes: usize = live.iter().map(|e| e.status.borrow().bytes()).sum();
        let stats = ImageCacheStats {
            images: live.len(),
            loading_images: loading,
            bytes: live_bytes as u64,
            retained_images: cache.retained.len(),
            retained_bytes: cache.retained_bytes as u64,
            decoded_images: cache.decoded,
            decode_time: cache.decode_time,
        };
        drop(cache);
        drop(live);
        stats
    })
}

pub(crate) fn set_budget(bytes: usize) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        cache.budget = bytes;
        while cache.retained_bytes > cache.budget {
            let Some(old) = cache.retained.pop_front() else { break };
            cache.retained_bytes -= old.bytes();
        }
    });
}

struct Job {
    id: u64,
    source: ImageSource,
    cancel: Arc<AtomicBool>,
    /// 动图所有帧最多占多少字节
    cap: usize,
}

struct Done {
    id: u64,
    status: Status,
    took: Duration,
}

/// 解码线程池（所有线程提交的）一共交回了多少张：等图片的测试按它看池子还有没有进展（[`wait_idle`]）
static DELIVERED: AtomicUsize = AtomicUsize::new(0);

/// 解码线程交回结果：投递回提交它的线程（主线程），记下做完了几个（测试工具等它们）
struct Results {
    sender: MainThreadSender,
    /// 已经投递回去的结果数
    finished: AtomicUsize,
    /// 提交了、还没交回的：编号和来源。等图片超时的时候说明在等哪几张
    pending: Mutex<Vec<(u64, ImageSource)>>,
}

impl Results {
    fn deliver(&self, done: Done) {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).retain(|(id, _)| *id != done.id);
        // 先记数再投递：测试工具看到数目够了的时候，结果一定已经在收件箱里
        self.finished.fetch_add(1, Ordering::Release);
        DELIVERED.fetch_add(1, Ordering::Release);
        self.sender.send(move || apply(done));
    }
}

struct Loader {
    results: Arc<Results>,
    /// 一共提交了多少张
    submitted: usize,
}

impl Loader {
    fn new() -> Self {
        let results = Results { sender: main_thread_sender(), finished: AtomicUsize::new(0), pending: Mutex::new(Vec::new()) };
        Self { results: Arc::new(results), submitted: 0 }
    }

    fn submit(&mut self, job: Job) {
        self.submitted += 1;
        self.results.pending.lock().unwrap_or_else(PoisonError::into_inner).push((job.id, job.source.clone()));
        let results = self.results.clone();
        decode_pool().execute(Box::new(move || decode(job, &results)));
    }
}

/// 解码的线程池，所有线程的图片缓存共用：留出主线程和渲染线程，最多 4 个；空闲 10 秒的线程退出
fn decode_pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(2)).clamp(1, 4);
        Pool::new("image-decode", threads, Duration::from_secs(10))
    })
}

/// 在解码线程上解码一张，结果投递回提交它的线程。解码器 panic 时这一张按失败交回
fn decode(job: Job, results: &Results) {
    let start = Instant::now();
    let status = if job.cancel.load(Ordering::Relaxed) {
        Status::Error("已取消".into())
    } else {
        match catch_unwind(AssertUnwindSafe(|| load(&job.source, job.cap))) {
            Ok(status) => status,
            Err(panic) => {
                let message = panic_message(&*panic);
                tracing::error!(source = ?job.source.key(), "解码图片时出错（解码器 panic）：{message}");
                Status::Error(format!("解码时出错：{message}").into())
            }
        }
    };
    results.deliver(Done { id: job.id, status, took: start.elapsed() });
}

/// 数据是 SVG：第一个不是空白的字符是 `<`、里面有 `<svg`（位图的文件头都不是这样），或者是 gzip 压缩的（只有 SVG 这样压，
/// 没打开 `svg` 特性时看不出来是不是，照不认识的格式报错）。按内容认，内存里的数据没有扩展名
fn is_svg(bytes: &[u8]) -> bool {
    let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let markup = text.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<') && text.windows(4).any(|w| w == b"<svg");
    markup || (cfg!(feature = "svg") && bytes.starts_with(&[0x1f, 0x8b]))
}

#[cfg(feature = "svg")]
fn decode_svg(bytes: &[u8], directory: Option<&std::path::Path>, cap: usize, source: &ImageSource) -> Result<Decoded, String> {
    super::svg::parse(bytes, directory, cap, source).map(Decoded::Vector)
}

#[cfg(not(feature = "svg"))]
fn decode_svg(_: &[u8], _: Option<&std::path::Path>, _: usize, _: &ImageSource) -> Result<Decoded, String> {
    Err("这是 SVG 文件，要打开 `svg` 特性才能加载".to_string())
}

fn load(source: &ImageSource, cap: usize) -> Status {
    let directory = match &source.kind {
        SourceKind::File(path) => path.parent(),
        _ => None,
    };
    let decode = |bytes: &[u8]| {
        if is_svg(bytes) {
            return decode_svg(bytes, directory, cap, source);
        }
        pixels::decode_any(bytes, source.max_size, cap)
    };
    let decoded = match &source.kind {
        SourceKind::File(path) => std::fs::read(path).map_err(|e| format!("无法读取 {}：{e}", path.display())).and_then(|bytes| decode(&bytes)),
        SourceKind::Bytes(bytes) => decode(bytes),
        SourceKind::Static(bytes) => decode(bytes),
        SourceKind::Rgba { width, height, pixels } => {
            if pixels.len() == (*width as usize) * (*height as usize) * 4 && *width > 0 && *height > 0 {
                Ok(Decoded::Still(pixels.to_vec(), *width, *height))
            } else {
                Err(format!("RGBA 数据长度 {} 和大小 {width}x{height} 不符", pixels.len()))
            }
        }
        SourceKind::RgbaF32 { width, height, pixels } => {
            if pixels.len() == (*width as usize) * (*height as usize) * 4 && *width > 0 && *height > 0 {
                Ok(Decoded::Wide(pixels.to_vec(), *width, *height))
            } else {
                Err(format!("RGBA 数据长度 {} 和大小 {width}x{height} 不符", pixels.len()))
            }
        }
    };
    match decoded {
        Ok(Decoded::Still(rgba, width, height)) => Status::Ready {
            natural: [width, height],
            data: Arc::new(pixels::process(rgba, width, height, source.max_size)),
            frames: None,
        },
        Ok(Decoded::Wide(rgba, width, height)) => Status::Ready {
            natural: [width, height],
            data: Arc::new(pixels::process_wide(rgba, width, height, source.max_size)),
            frames: None,
        },
        Ok(Decoded::Animated { natural, frames }) => match frames.frames.len() {
            0 => Status::Error("动图里没有能解码的帧".into()),
            1 => Status::Ready { natural, data: frames.frames[0].clone(), frames: None },
            _ => Status::Ready { natural, data: frames.frames[0].clone(), frames: Some(Arc::new(frames)) },
        },
        #[cfg(feature = "svg")]
        Ok(Decoded::Vector(document)) => Status::Vector { document: Arc::new(document) },
        Err(message) => Status::Error(message.into()),
    }
}
