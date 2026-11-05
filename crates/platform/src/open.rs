//! 应用被要求打开的网址和文件：命令行参数怎么解析（各平台一样的纯逻辑），macOS 上系统交来的打开事件。
//!
//! - macOS：打包成 .app 后，自定义协议的网址、在访达里双击的文档不在命令行参数里，而是由系统发打开事件
//!   （Apple Event：打开网址 `GURL`、打开文档 `odoc`）。这里在 `NSAppleEventManager` 上登记处理函数，
//!   不改 winit 装的应用委托。没打包时（`cargo run`）和其他平台一样从命令行参数来。
//! - Windows：注册表里的命令行写成 `"程序.exe" "%1"`，网址和文件都是命令行参数。
//! - Linux：`.desktop` 文件写 `Exec=程序 %U`，网址和文件（`file://` 网址）都是命令行参数。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::PlatformBackend;
use crate::system::portable::uri_to_path;

/// 这个平台的实现，由 [`crate::NativeBackend`] 调用
#[cfg(target_os = "macos")]
pub(crate) use apple as imp;
#[cfg(not(target_os = "macos"))]
pub(crate) use fallback as imp;

/// 系统会不会用打开事件交来网址和文件：macOS 打包成 .app 时会；其他平台的网址和文件都在命令行参数里
pub fn delivers_events() -> bool {
    crate::native().single_instance().delivers_open_events()
}

/// 登记打开事件（打开网址、打开文档）的回调，在主线程调用；再调用一次换掉回调。
/// 只有 macOS 有打开事件，其他平台什么都不做
pub fn listen(on_open: OnOpen) {
    crate::native().single_instance().listen_open_events(on_open);
}

/// 单实例里后启动的那个（macOS 打包成 .app，被系统启动来打开网址、文档）：收下系统交来的打开事件，
/// 最多等 `timeout`，然后转给已经在运行的实例。其他平台马上返回空的
pub fn collect_launch(timeout: std::time::Duration) -> Opened {
    crate::native().single_instance().collect_launch_events(timeout)
}

/// 要打开的网址和文件
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opened {
    /// 网址（`协议:…`），原样
    pub urls: Vec<String>,
    /// 文件和文件夹，绝对路径
    pub files: Vec<PathBuf>,
}

impl Opened {
    /// 网址和文件都没有
    pub fn is_empty(&self) -> bool {
        self.urls.is_empty() && self.files.is_empty()
    }

    /// 把 `other` 的网址和文件接在后面
    pub fn extend(&mut self, other: Opened) {
        self.urls.extend(other.urls);
        self.files.extend(other.files);
    }
}

/// 打开事件的回调（macOS 上在主线程调用）
pub type OnOpen = Box<dyn Fn(Opened)>;

/// 像网址：协议名以字母开头、由字母数字和 `+ . -` 组成、至少两个字符（排除 Windows 的盘符 `C:`），冒号后面还有内容
pub fn is_url(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else { return false };
    scheme.len() >= 2
        && !rest.is_empty()
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

impl Opened {
    /// 这个进程的命令行参数（不含程序名）里的网址和文件，见 [`Opened::parse_args`]
    pub fn from_command_line() -> Self {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        let cwd = std::env::current_dir().unwrap_or_default();
        Self::parse_args(&args, &cwd, |path| path.exists(), cfg!(windows))
    }

    /// 从命令行参数（不含程序名）里找出网址和文件：
    ///
    /// - 以 `-` 开头的是选项，跳过（包括 macOS 老版本带的 `-psn_…`）；
    /// - `file://` 网址换成路径；
    /// - 存在的文件、文件夹算文件（相对路径相对于 `cwd`），其余像网址的算网址，别的不要（比如选项的值）；
    /// - `windows` 时按 Windows 的习惯处理引号：注册表写成 `"%1"` 时，以反斜杠结尾的路径会多出一个引号
    ///   （`"C:\目录\"` 解析成 `C:\目录"`），去掉；写成不带引号的 `%1` 时，带空格的路径被拆成几个参数，依次拼回去试。
    pub fn parse_args(args: &[OsString], cwd: &Path, exists: impl Fn(&Path) -> bool, windows: bool) -> Self {
        let mut opened = Opened::default();
        let absolute = |p: &Path| if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            if arg.to_string_lossy().starts_with('-') {
                continue;
            }
            let Some(text) = arg.to_str() else {
                // 不是 UTF-8 的只可能是路径
                let path = absolute(Path::new(arg));
                if exists(&path) {
                    opened.files.push(path);
                }
                continue;
            };
            let text = if windows { text.trim_matches('"') } else { text };
            if text.is_empty() {
                continue;
            }
            if text.starts_with("file:") {
                opened.files.extend(uri_to_path(text));
                continue;
            }
            let path = absolute(Path::new(text));
            if exists(&path) {
                opened.files.push(path);
            } else if is_url(text) {
                opened.urls.push(text.to_owned());
            } else if windows {
                // 不带引号的 %1：把后面的参数用空格接上，直到拼出一个存在的路径
                let mut joined = text.to_owned();
                for (j, next) in args.iter().enumerate().skip(i) {
                    let Some(next) = next.to_str() else { break };
                    joined.push(' ');
                    joined.push_str(next.trim_matches('"'));
                    let path = absolute(Path::new(&joined));
                    if exists(&path) {
                        opened.files.push(path);
                        i = j + 1;
                        break;
                    }
                }
            }
        }
        opened
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod apple {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationWillFinishLaunchingNotification, NSEventMask};
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSBundle, NSDate, NSDefaultRunLoopMode, NSNotification, NSNotificationCenter};

    use super::{OnOpen, Opened};

    /// 四字符代码（`AEEventClass`、`AEEventID`、`AEKeyword` 都是 32 位整数）
    const fn code(c: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*c)
    }
    /// kInternetEventClass、kAEGetURL
    const GET_URL: u32 = code(b"GURL");
    /// kCoreEventClass、kAEOpenDocuments
    const CORE_EVENT: u32 = code(b"aevt");
    const OPEN_DOCUMENTS: u32 = code(b"odoc");
    /// keyDirectObject：事件的主参数
    const DIRECT_OBJECT: u32 = code(b"----");

    struct Ivars {
        on_open: OnOpen,
    }

    define_class!(
        // SAFETY: NSObject 没有子类化要求；OpenHandler 没有实现 Drop
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "AureoleOpenHandler"]
        #[ivars = Ivars]
        struct OpenHandler;

        impl OpenHandler {
            #[unsafe(method(handleGetURL:withReplyEvent:))]
            fn get_url(&self, event: &NSAppleEventDescriptor, _reply: Option<&NSAppleEventDescriptor>) {
                let url = direct_object(event).and_then(|d| d.stringValue()).map(|s| s.to_string()).filter(|u| !u.is_empty());
                if let Some(url) = url {
                    (self.ivars().on_open)(Opened { urls: vec![url], files: Vec::new() });
                }
            }

            #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
            fn open_documents(&self, event: &NSAppleEventDescriptor, _reply: Option<&NSAppleEventDescriptor>) {
                let Some(list) = direct_object(event) else { return };
                // 主参数一般是文件的列表（序号从 1 开始），也可能只是一个文件
                let count = list.numberOfItems();
                let items: Vec<Retained<NSAppleEventDescriptor>> =
                    if count > 0 { (1..=count).filter_map(|i| list.descriptorAtIndex(i)).collect() } else { vec![list] };
                let files: Vec<PathBuf> = items.iter().filter_map(|d| d.fileURLValue()).filter_map(|url| url.to_file_path()).collect();
                if !files.is_empty() {
                    (self.ivars().on_open)(Opened { urls: Vec::new(), files });
                }
            }

            #[unsafe(method(willFinishLaunching:))]
            fn will_finish_launching(&self, _notification: &NSNotification) {
                self.install();
            }
        }
    );

    impl OpenHandler {
        fn new(mtm: MainThreadMarker, on_open: OnOpen) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { on_open });
            // SAFETY: init 是父类 NSObject 的初始化方法，不带参数，返回初始化好的同一个对象，和返回类型 Retained<Self> 一致；
            // 实例变量在上一行已经设好
            unsafe { msg_send![super(this), init] }
        }

        /// 在 Apple Event 管理器上登记两个处理函数（同一种事件再登记一次就换掉原来的）
        fn install(&self) {
            let manager = NSAppleEventManager::sharedAppleEventManager();
            // SAFETY: 处理函数的签名是 (事件, 回复)，四字符代码是 UInt32；处理对象存在 HANDLER 里，一直活着
            unsafe {
                let _: () = msg_send![&*manager, setEventHandler: self, andSelector: sel!(handleGetURL:withReplyEvent:),
                    forEventClass: GET_URL, andEventID: GET_URL];
                let _: () = msg_send![&*manager, setEventHandler: self, andSelector: sel!(handleOpenDocuments:withReplyEvent:),
                    forEventClass: CORE_EVENT, andEventID: OPEN_DOCUMENTS];
            }
        }
    }

    fn direct_object(event: &NSAppleEventDescriptor) -> Option<Retained<NSAppleEventDescriptor>> {
        // SAFETY: paramDescriptorForKeyword: 的参数是四字符代码，返回描述符或 nil
        unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] }
    }

    thread_local! {
        static HANDLER: RefCell<Option<Retained<OpenHandler>>> = const { RefCell::new(None) };
    }

    /// 打包成 .app 时系统用打开事件交来网址和文件（没打包时不会有）
    pub fn delivers_events() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    /// 登记打开事件（打开网址、打开文档）的回调，在主线程调用；再调用一次换掉回调。不在主线程时什么都不做。
    ///
    /// 应用启动时（`finishLaunching`）系统装上自己的处理函数、换掉这里的，所以在"即将启动完成"的通知里再装一次：
    /// 系统在这之后才交出启动时的打开事件（为了打开文档或网址而启动应用时）。之前到的事件由系统留着，不会丢
    pub fn listen(on_open: OnOpen) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let handler = OpenHandler::new(mtm, on_open);
        handler.install();
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: 观察者实现了 willFinishLaunching:；旧的观察者先摘掉，新的一直活着（存在 HANDLER 里）
        unsafe {
            if let Some(old) = HANDLER.with(|h| h.borrow_mut().take()) {
                center.removeObserver(&old);
            }
            center.addObserver_selector_name_object(&handler, sel!(willFinishLaunching:), Some(NSApplicationWillFinishLaunchingNotification), None);
        }
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
    }

    /// 单实例里后启动的那个（打包成 .app，被系统启动来打开网址、文档）：不进程序坞、不抢焦点地完成启动，
    /// 收下系统交来的打开事件（最多等 `timeout`，收到后再等一小会儿没有新的就返回），然后转给已经在运行的实例。
    /// 之后这个进程不能再跑 winit 的事件循环（应用已经启动完了），应该退出
    pub fn collect_launch(timeout: Duration) -> Opened {
        let Some(mtm) = MainThreadMarker::new() else { return Opened::default() };
        let collected = Rc::new(RefCell::new(Opened::default()));
        let sink = collected.clone();
        listen(Box::new(move |opened| sink.borrow_mut().extend(opened)));
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        app.finishLaunching();
        let mut deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.02);
            // SAFETY: NSDefaultRunLoopMode 是 Foundation 导出的不可变常量，只读不写
            let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), unsafe { NSDefaultRunLoopMode }, true);
            if let Some(event) = event {
                app.sendEvent(&event);
            }
            if !collected.borrow().is_empty() {
                deadline = deadline.min(Instant::now() + Duration::from_millis(80));
            }
        }
        if let Some(handler) = HANDLER.with(|h| h.borrow_mut().take()) {
            // SAFETY: handler 是 listen 里用 addObserver 登记的那个观察者对象，类型对得上
            unsafe { NSNotificationCenter::defaultCenter().removeObserver(&handler) };
        }
        collected.take()
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) mod fallback {
    use std::time::Duration;

    use super::{OnOpen, Opened};

    /// 其他平台的网址和文件都在命令行参数里
    pub fn delivers_events() -> bool {
        false
    }

    /// 其他平台没有打开事件，什么都不做
    pub fn listen(_on_open: OnOpen) {}

    /// 其他平台没有打开事件，马上返回空的
    pub fn collect_launch(_timeout: Duration) -> Opened {
        Opened::default()
    }
}
