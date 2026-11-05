//! 带点击回调和动作按钮的系统通知。
//!
//! 平台的回调可能在系统的线程上（macOS 的代理、Windows 的激活、Linux 的信号线程），这里只把"哪条通知、点了什么"投递回主线程
//! （[`aureole_core::MainThreadSender`]），处理器（不要求 `Send`）在事件循环里、在创建通知时的作用域里、在批量更新里执行。
//! 创建通知的作用域释放以后（比如发通知的元素卸载了），再点通知什么都不发生。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use platform::{NotificationRequest, NotificationResponse, PlatformError};
use reactive::Scope;

/// 处理器最多留多少条：系统不一定告诉框架通知被划掉了，太老的忘掉
const KEPT: usize = 256;

type Handler = Rc<dyn Fn()>;

struct Shown {
    id: u64,
    on_click: Option<Handler>,
    actions: Vec<Handler>,
    scope: Option<Scope>,
}

#[derive(Default)]
struct Registry {
    next: u64,
    shown: VecDeque<Shown>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::default();
}

fn forget(id: u64) {
    REGISTRY.with(|registry| registry.borrow_mut().shown.retain(|shown| shown.id != id));
}

/// 通知被点了（在主线程、事件循环里）
fn dispatch(id: u64, response: NotificationResponse) {
    let handler = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let index = registry.shown.iter().position(|shown| shown.id == id)?;
        // 一条通知点一次就没了，处理器也不留
        let shown = registry.shown.remove(index)?;
        let handler = match response {
            NotificationResponse::Click => shown.on_click,
            NotificationResponse::Action(action) => shown.actions.get(action).cloned(),
            _ => None,
        };
        Some((handler?, shown.scope))
    });
    let Some((handler, scope)) = handler else { return };
    match scope {
        Some(scope) => scope.run(move || reactive::batch(|| handler())),
        None => reactive::batch(|| handler()),
    }
}

/// 一条系统通知：标题、正文、动作按钮、点击回调。只要内容时用 [`crate::notify`] 就够了。
///
/// ```no_run
/// # use aureole::prelude::*;
/// use aureole::Notification;
///
/// let opened = signal(false);
/// let result = Notification::new("导出完成")
///     .body("已保存到下载文件夹")
///     .action("打开", move || opened.set(true))
///     .action("稍后", || {})
///     .on_click(move || opened.set(true))
///     .show();
/// if let Err(error) = result {
///     // 没打包（macOS）、没有应用标识（Windows）、没有通知服务（Linux）：把提示放进窗口里
///     println!("通知发不了：{error}");
/// }
/// ```
///
/// 处理器在主线程、在创建通知时的作用域里执行，可以直接写信号；点一次就用掉了。
/// 通知还在系统的通知中心里、应用已经退出了时再点，处理器已经不在了（系统会重新启动应用，这里没有接这条路）。
///
/// 各平台的前提和 [`crate::notify`] 一样；动作按钮怎么显示由系统决定（macOS 要用户把通知样式设成提醒才一直显示按钮，
/// Linux 看桌面的通知服务支不支持）。测试工具里不真的发，`Harness::notifications` 读回来，`Harness::click_notification`、
/// `Harness::press_notification_action` 模拟用户点
#[derive(Default)]
#[must_use = "调用 show 才会发出去"]
pub struct Notification {
    title: String,
    body: String,
    on_click: Option<Handler>,
    actions: Vec<(String, Handler)>,
}

impl Notification {
    /// 标题是 `title` 的通知
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), ..Self::default() }
    }

    /// 正文
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// 加一个动作按钮，点了调用 `handler`
    pub fn action<R>(mut self, title: impl Into<String>, handler: impl Fn() -> R + 'static) -> Self {
        self.actions.push((title.into(), Rc::new(move || {
            handler();
        })));
        self
    }

    /// 点了通知本身时调用（不是按钮）
    pub fn on_click<R>(mut self, handler: impl Fn() -> R + 'static) -> Self {
        self.on_click = Some(Rc::new(move || {
            handler();
        }));
        self
    }

    /// 发出去。交给系统（或者排进发送的队列）了是 `Ok`。没打包成 .app（macOS）、没有应用标识（Windows）时是
    /// [`PlatformError::NotPackaged`]，没有通知服务时是 [`PlatformError::Unsupported`]；这时处理器都不会被调用
    pub fn show(self) -> Result<(), PlatformError> {
        let Self { title, body, on_click, actions } = self;
        let (titles, handlers): (Vec<String>, Vec<Handler>) = actions.into_iter().unzip();
        let scope = Scope::current();
        let id = REGISTRY.with(|registry| {
            let mut registry = registry.borrow_mut();
            registry.next += 1;
            let id = registry.next;
            if registry.shown.len() >= KEPT {
                registry.shown.pop_front();
            }
            registry.shown.push_back(Shown { id, on_click, actions: handlers, scope });
            id
        });
        let main = aureole_core::main_thread_sender();
        let on_response = Arc::new(move |response: NotificationResponse| {
            main.send(move || dispatch(id, response));
        });
        let request = NotificationRequest { title, body, actions: titles };
        let shown = platform::with_backend(|backend| backend.notification().show(request, on_response));
        match shown {
            Ok(()) => {
                // 作用域没了，处理器读的信号也没了：这时再点不调用它
                if scope.is_some() {
                    reactive::on_cleanup(move || forget(id));
                }
                Ok(())
            }
            Err(error) => {
                forget(id);
                Err(error)
            }
        }
    }
}
