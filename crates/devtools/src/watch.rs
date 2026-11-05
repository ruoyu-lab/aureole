//! 监视文件：文件变了（保存、整个替换）在后台线程重新读取、解析，结果交回主线程写进信号。
//!
//! 用 `notify` crate 监视文件所在的文件夹（macOS 上是 FSEvents）：编辑器"先写临时文件再改名"的保存方式也能收到。
//! 一个后台线程等事件，没有事件时一直睡，**不轮询**；同一次保存的一串事件攒 30 毫秒一起处理，
//! 内容和上次一样时不交回。结果投递回订阅它的线程（[`MainThreadSender`]，信号不能跨线程），
//! 和任务、解码好的图片走同一个收件箱：投递时叫醒事件循环，那个线程在这一轮或者下一帧开始时写进信号。
//!
//! 解析出错不拖垮监视：`parse` panic 时这一次不交回（信号保持上一次的值），记一条错误，等文件下次改了再读；
//! 监视线程自己出错时记一条错误接着监视，别的文件不受影响。

use std::any::Any;
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use reactive::{Scope, Signal, batch, on_cleanup, signal};

use aureole_core::{Cancel, CancelGuard, MainThreadSender, main_thread_sender};

/// 同一次保存的一串事件攒这么久一起处理
const SETTLE: Duration = Duration::from_millis(30);

/// 交给后台线程的消息
enum Message {
    /// 系统报告这个路径（文件或文件夹）变了
    Changed(PathBuf),
    /// 监视的文件夹变了（系统的监视重新开始时，中间的事件可能丢掉）：所有文件都比较一遍
    Rescan,
}

type Parse = Box<dyn Fn(&[u8]) -> Box<dyn Any + Send> + Send + Sync>;
type Deliver = Box<dyn Fn(Box<dyn Any + Send>)>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一个被监视的文件
struct Subscriber {
    /// 文件夹按真实路径（解开符号链接）+ 文件名：和系统报告的路径一致
    file: PathBuf,
    parse: Parse,
    /// 只比较大小和修改时间（大文件，比如可执行文件），不读内容
    metadata: bool,
    /// 上次读到的内容（或大小和修改时间）；读不到时为 `None`
    last: Mutex<Option<Vec<u8>>>,
    /// 投递回订阅它的线程
    sender: MainThreadSender,
}

/// 后台的监视服务，所有窗口和线程共用
struct Service {
    watcher: Mutex<Option<RecommendedWatcher>>,
    /// 监视着的文件夹和其中被监视的文件数
    dirs: Mutex<HashMap<PathBuf, usize>>,
    subscribers: Mutex<HashMap<u64, Arc<Subscriber>>>,
    next: AtomicU64,
    worker: Mutex<Sender<Message>>,
}

fn service() -> &'static Arc<Service> {
    static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Message>();
        let service = Arc::new(Service {
            watcher: Mutex::new(None),
            dirs: Mutex::default(),
            subscribers: Mutex::default(),
            next: AtomicU64::new(1),
            worker: Mutex::new(sender.clone()),
        });
        let worker = service.clone();
        std::thread::Builder::new()
            .name("文件监视".into())
            .spawn(move || worker.run(receiver))
            .expect("启动文件监视线程失败");
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                for path in event.paths {
                    let _ = sender.send(Message::Changed(path));
                }
            }
        });
        match watcher {
            Ok(watcher) => *lock(&service.watcher) = Some(watcher),
            Err(e) => tracing::error!("监视文件不可用：{e}"),
        }
        service
    })
}

impl Service {
    /// 后台线程：等事件，攒一会儿，把变了的文件重新读一遍，直到发送端关闭。出错（panic）时记一条错误接着监视
    fn run(&self, receiver: Receiver<Message>) {
        loop {
            match catch_unwind(AssertUnwindSafe(|| self.watch(&receiver))) {
                Ok(()) => return,
                Err(panic) => tracing::error!("文件监视线程出错，接着监视：{}", panic_message(&*panic)),
            }
        }
    }

    fn watch(&self, receiver: &Receiver<Message>) {
        while let Ok(first) = receiver.recv() {
            let mut changed = HashSet::new();
            let mut rescan = false;
            let mut message = Some(first);
            while let Some(m) = message.take() {
                match m {
                    Message::Changed(path) => _ = changed.insert(path),
                    Message::Rescan => rescan = true,
                }
                message = match receiver.recv_timeout(SETTLE) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
            }
            let subscribers: Vec<(u64, Arc<Subscriber>)> = lock(&self.subscribers).iter().map(|(&id, s)| (id, s.clone())).collect();
            for (id, subscriber) in subscribers {
                let hit = rescan || changed.iter().any(|p| *p == subscriber.file || subscriber.file.parent() == Some(p.as_path()));
                if !hit {
                    continue;
                }
                let now = read(&subscriber.file, subscriber.metadata);
                let mut last = lock(&subscriber.last);
                if *last == now {
                    continue;
                }
                let parsed = catch_unwind(AssertUnwindSafe(|| (subscriber.parse)(now.as_deref().unwrap_or_default())));
                *last = now;
                match parsed {
                    Ok(value) => _ = subscriber.sender.send(move || deliver(id, value)),
                    // 这一次不交回，等文件下次改了再读
                    Err(panic) => tracing::error!(file = %subscriber.file.display(), "解析监视的文件时出错：{}", panic_message(&*panic)),
                }
            }
        }
    }

    fn subscribe(&self, path: &Path, metadata: bool, parse: Parse, sender: MainThreadSender) -> u64 {
        let file = real_path(path);
        let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let last = Mutex::new(read(&file, metadata));
        lock(&self.subscribers).insert(id, Arc::new(Subscriber { file, parse, metadata, last, sender }));
        let mut dirs = lock(&self.dirs);
        let count = dirs.entry(dir.clone()).or_insert(0);
        *count += 1;
        if *count == 1
            && let Some(watcher) = lock(&self.watcher).as_mut()
        {
            match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                // 系统的监视重新开始了一遍，这期间别的文件的事件可能丢掉：比较一遍
                Ok(()) => _ = lock(&self.worker).send(Message::Rescan),
                Err(e) => tracing::error!(dir = %dir.display(), "监视文件夹失败：{e}"),
            }
        }
        id
    }

    fn unsubscribe(&self, id: u64) {
        let Some(subscriber) = lock(&self.subscribers).remove(&id) else { return };
        let dir = subscriber.file.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut dirs = lock(&self.dirs);
        if let Some(count) = dirs.get_mut(&dir) {
            *count -= 1;
            if *count == 0 {
                dirs.remove(&dir);
                if let Some(watcher) = lock(&self.watcher).as_mut()
                    && watcher.unwatch(&dir).is_ok()
                {
                    let _ = lock(&self.worker).send(Message::Rescan);
                }
            }
        }
    }
}

/// panic 的信息（`panic!` 的字符串）
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("（不是字符串）")
}

/// 文件现在的内容；`metadata` 时只取大小、修改时间（和 inode）
fn read(file: &Path, metadata: bool) -> Option<Vec<u8>> {
    if !metadata {
        return std::fs::read(file).ok();
    }
    let m = std::fs::metadata(file).ok()?;
    let modified = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let inode = 0u64;
    Some([m.len().to_le_bytes().as_slice(), &modified.to_le_bytes(), &inode.to_le_bytes()].concat())
}

/// 文件夹换成真实路径（解开符号链接，和系统报告的路径一致），文件本身可以还不存在
fn real_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let (Some(dir), Some(name)) = (absolute.parent(), absolute.file_name()) else { return absolute };
    std::fs::canonicalize(dir).map(|dir| dir.join(name)).unwrap_or(absolute)
}

thread_local! {
    /// 这个线程上的订阅收到结果时做什么（写信号）
    static HANDLERS: RefCell<HashMap<u64, Deliver>> = RefCell::default();
}

/// 后台读好的一个文件投递回来了：交给订阅的处理器，写进信号（在主线程的批量更新里，不在借用元素树时）
fn deliver(id: u64, value: Box<dyn Any + Send>) {
    // 先拿出来再调用（处理器里可能又订阅、取消订阅），调用完还在订阅着就放回去
    let Some(handler) = HANDLERS.with(|handlers| handlers.borrow_mut().remove(&id)) else { return };
    handler(value);
    if lock(&service().subscribers).contains_key(&id) {
        HANDLERS.with(|handlers| {
            handlers.borrow_mut().entry(id).or_insert(handler);
        });
    }
}

/// 订阅一个文件：`deliver` 在主线程收到解析好的新内容。返回订阅编号
fn subscribe(path: &Path, metadata: bool, parse: Parse, deliver: Deliver) -> u64 {
    let id = service().subscribe(path, metadata, parse, main_thread_sender());
    HANDLERS.with(|handlers| handlers.borrow_mut().insert(id, deliver));
    id
}

fn unsubscribe(id: u64) {
    service().unsubscribe(id);
    let _ = HANDLERS.try_with(|handlers| handlers.borrow_mut().remove(&id));
}

/// 监视一个文本文件：返回的信号是文件的内容（按 UTF-8 读，坏的字节换成 �；文件不存在时是空字符串），
/// 文件保存后在下一帧更新。用来热加载配色、文案、要调的参数：
///
/// ```no_run
/// # use aureole::prelude::*;
/// let palette = aureole::devtools::watch_file("assets/palette.txt");
/// text(move || palette.get()).font_size(14);
/// ```
///
/// 归调用时的作用域所有：作用域释放（元素卸载、窗口关闭）时停止监视。
/// 读文件和比较在后台线程上做，内容没变（只是"保存"了一下）时信号不更新
pub fn watch_file(path: impl AsRef<Path>) -> Signal<String> {
    watch_file_with(path, |bytes| String::from_utf8_lossy(bytes).into_owned())
}

/// 监视一个文件，用 `parse` 把内容解析成值（在后台线程上解析，文件不存在时传入空的内容）。
/// 第一次在调用的线程上同步读取，所以界面一开始就有值：
///
/// ```no_run
/// # use aureole::prelude::*;
/// // 每行一个 "名字 = 数值"，改了保存马上看到效果（调弹簧、间距、圆角）
/// let tweaks = aureole::devtools::watch_file_with("tweaks.txt", |bytes| {
///     String::from_utf8_lossy(bytes)
///         .lines()
///         .filter_map(|line| line.split_once('='))
///         .filter_map(|(k, v)| Some((k.trim().to_string(), v.trim().parse::<f32>().ok()?)))
///         .collect::<std::collections::HashMap<_, _>>()
/// });
/// let radius = move || tweaks.with(|t| t.get("radius").copied().unwrap_or(12.0));
/// rect().size(120, 80).radius(radius).background(Color::hex(0x0a84ff));
/// ```
pub fn watch_file_with<T: Send + 'static>(path: impl AsRef<Path>, parse: impl Fn(&[u8]) -> T + Send + Sync + 'static) -> Signal<T> {
    let path = path.as_ref();
    let initial = std::fs::read(path).ok();
    if initial.is_none() {
        tracing::warn!(path = %path.display(), "监视的文件现在不存在，出现后再读");
    }
    let value = signal(parse(initial.as_deref().unwrap_or_default()));
    let parse: Parse = Box::new(move |bytes| Box::new(parse(bytes)));
    let deliver: Deliver = Box::new(move |any| {
        if let Ok(v) = any.downcast::<T>() {
            value.set(*v);
        }
    });
    let id = subscribe(path, false, parse, deliver);
    if Scope::current().is_some() {
        on_cleanup(move || unsubscribe(id));
    }
    value
}

/// 一个文件监视（[`watch_metadata`] 返回）：取消后不再监视
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileWatch(u64);

impl Cancel for FileWatch {
    fn cancel(self) {
        unsubscribe(self.0);
    }
}

/// 监视一个大文件的大小和修改时间（不读内容），变了在主线程调用 `changed`。不归任何作用域：
/// 返回的守卫丢掉时停止监视（运行时监视自己的可执行文件，守卫留到应用退出）
pub fn watch_metadata(path: &Path, changed: impl Fn() + 'static) -> CancelGuard<FileWatch> {
    let parse: Parse = Box::new(|_| Box::new(()));
    FileWatch(subscribe(path, true, parse, Box::new(move |_| changed()))).guard()
}

// ---------- 图片 ----------

/// 图片文件的监视：每个用到过的路径一个修订号，文件变了加一（图片缓存的键带上它，于是重新解码）
#[derive(Default)]
struct Images {
    enabled: bool,
    /// 任何一张图片变了都加一：读过它的图片绑定重新执行
    revision: Option<Signal<u64>>,
    paths: HashMap<PathBuf, (u64, u64)>,
}

thread_local! {
    static IMAGES: RefCell<Images> = RefCell::default();
}

/// 让 [`aureole_core::image()`] 用到的图片文件改了以后自动重新解码（解码好之前继续显示旧图）。
/// 开发时打开，换图标、调图片时不用重启；关掉后不再监视。默认关闭，关闭时没有任何开销
pub fn set_image_watching(enabled: bool) {
    aureole_core::internal::set_image_revision_source(image_revision);
    let ids: Vec<u64> = IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        images.enabled = enabled;
        if enabled {
            if images.revision.is_none() {
                // 全应用共用，不归任何作用域
                images.revision = Some(reactive::detached(|| signal(0)));
            }
            Vec::new()
        } else {
            images.paths.drain().map(|(_, (_, id))| id).collect()
        }
    });
    ids.into_iter().for_each(unsubscribe);
}

/// 图片文件现在的修订号（图片绑定里调用，会订阅变化）；没打开图片监视时是 0
fn image_revision(path: &Path) -> u64 {
    let watched = IMAGES.with(|images| {
        let images = images.borrow();
        if !images.enabled {
            return None;
        }
        let revision = images.revision?;
        Some((revision, images.paths.get(path).map(|&(rev, _)| rev)))
    });
    let Some((revision, known)) = watched else { return 0 };
    revision.get();
    if let Some(rev) = known {
        return rev;
    }
    let owned = path.to_path_buf();
    let deliver: Deliver = Box::new(move |_| {
        let bumped = IMAGES.with(|images| {
            let mut images = images.borrow_mut();
            let entry = images.paths.get_mut(&owned)?;
            entry.0 += 1;
            images.revision
        });
        if let Some(revision) = bumped {
            batch(|| revision.update(|r| *r += 1));
        }
    });
    let parse: Parse = Box::new(|_| Box::new(()));
    let id = subscribe(path, false, parse, deliver);
    IMAGES.with(|images| images.borrow_mut().paths.insert(path.to_path_buf(), (0, id)));
    0
}
