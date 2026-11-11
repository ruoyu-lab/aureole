//! 别的应用拖来的内容：平台层的回调（系统的拖放流程里、主线程上直接调进来）换成窗口宿主的输入，当场回答接不接受。
//!
//! 文件有两条路，都汇到窗口库的文件事件（换算见 `translate.rs`，所以文件拖放的行为各平台一样）：
//! macOS 上窗口库自己发，这里只回答；Windows 上拖放目标是平台层接的，它把文件交过来，这里发一样的窗口事件，走事件循环的同一条路。
//! 别的内容（文字、链接、图片）直接交给窗口宿主。系统每隔一会儿问一次，位置没变时不再交（和文件的悬停一样去重）。
//!
//! 应用自己的窗口交给系统的元素之间拖放（拖着元素到了应用的另一个窗口，见 `HostWindow::app_window_at`）：拖着的数据留在进程里
//! （[`carry`]），经过应用的窗口时先按元素之间的拖放问那里的元素（`WindowTree::element_drag_over`），有元素接收就是它，
//! 没有时照旧走上面两条路（拖出去的内容里的文件、文字）。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use platform::{ExternalDrag, ExternalDropEffect, OnExternalDrag, PlatformError};
use winit::event::WindowEvent;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use aureole_core::host::{self, HostWindow, WindowHost};
use aureole_core::{ExternalDataKinds, ExternalDropData, InputEvent, InputKind, RgbaImage};

use crate::translate::TranslateState;
use crate::window::{self, UserEvent, WindowHandle};

thread_local! {
    /// 这个进程拖出去的会话带着的元素之间拖放的数据（会话还没结束时有）
    static CARRIED: RefCell<Option<Rc<dyn Any>>> = const { RefCell::new(None) };
}

/// 拖出去的会话开始时记下元素之间拖放的数据，结束时清掉（`None`）
pub(crate) fn carry(data: Option<Rc<dyn Any>>) {
    CARRIED.with(|carried| *carried.borrow_mut() = data);
}

/// 自己拖出去的会话带着的元素之间拖放的数据
fn carried() -> Option<Rc<dyn Any>> {
    CARRIED.with(|carried| carried.borrow().clone())
}

fn kinds_from_platform(kinds: platform::ExternalDataKinds) -> ExternalDataKinds {
    let mut out = ExternalDataKinds::empty();
    out.set(ExternalDataKinds::TEXT, kinds.text);
    out.set(ExternalDataKinds::LINK, kinds.link);
    out.set(ExternalDataKinds::IMAGE, kinds.image);
    out.set(ExternalDataKinds::FILES, kinds.files);
    out.set(ExternalDataKinds::RICH_TEXT, kinds.rich_text);
    out.set(ExternalDataKinds::CUSTOM, kinds.custom);
    out
}

/// 别的程序（或同一个应用的别的窗口、别的实例）拖进来时也读应用自己定的格式 `format`（剪贴板、[`aureole_core::DragOut`] 的
/// `custom` 用的那种名字），放下时元素用 [`aureole_core::ExternalDropEvent::custom`] 取。重复登记不算。
/// macOS 上窗口打开时告诉系统接受哪些拖放类型，所以要在打开窗口之前（`App::run` 之前）调用
pub fn accept_drop_format(format: &str) {
    platform::accept_custom_format(format);
}

fn data_from_platform(data: platform::ExternalDropData) -> ExternalDropData {
    ExternalDropData {
        text: data.text,
        link: data.link,
        image: data.image.map(|image| RgbaImage { width: image.width, height: image.height, pixels: image.rgba }),
        files: Vec::new(),
        rich_text: data.rich_text.map(|bytes| crate::clipboard::spans_from_rtf(&bytes)).unwrap_or_default(),
        custom: data.custom,
    }
}

fn effect_to_platform(effect: aureole_core::ExternalDropEffect) -> ExternalDropEffect {
    match effect {
        aureole_core::ExternalDropEffect::Move => ExternalDropEffect::Move,
        aureole_core::ExternalDropEffect::Link => ExternalDropEffect::Link,
        // 拷贝，和系统不认识的效果都按拷贝：源文件不会丢
        _ => ExternalDropEffect::Copy,
    }
}

/// 拖文件时给系统的回答：元素说了算；没有元素关心时照旧显示能放下的光标（放下了没人收，和以前一样）
fn file_answer(effect: Option<aureole_core::ExternalDropEffect>, claimed: bool) -> Option<aureole_core::ExternalDropEffect> {
    effect.or((!claimed).then_some(aureole_core::ExternalDropEffect::Copy))
}

/// 窗口库不管文件的时候（Windows，macOS 上自己拖出去的），把平台层交来的文件换成窗口库一样的事件，排进事件循环
struct OwnFiles {
    proxy: EventLoopProxy<UserEvent>,
    window: WindowId,
    hovering: Cell<bool>,
    /// 这次拖放刚进来时的文件：元素之间的拖放接了一阵又不接时，悬停再发一次
    entered: RefCell<Vec<PathBuf>>,
}

impl OwnFiles {
    fn send(&self, event: WindowEvent) {
        let _ = self.proxy.send_event(UserEvent::Window(self.window, event));
    }

    /// 刚进来时平台交来的文件（之后的经过是空的）
    fn remember(&self, files: Vec<PathBuf>) {
        if !files.is_empty() {
            *self.entered.borrow_mut() = files;
        }
    }

    /// 文件悬停着：刚开始悬停时发这次的文件
    fn over(&self) {
        if !self.hovering.replace(true) {
            self.entered.borrow().iter().for_each(|file| self.send(WindowEvent::HoveredFile(file.clone())));
        }
    }

    fn dropped(&self, files: Vec<PathBuf>) {
        self.hovering.set(false);
        self.entered.borrow_mut().clear();
        files.into_iter().for_each(|file| self.send(WindowEvent::DroppedFile(file)));
    }

    /// 不悬停了：是这里发过悬停的才要发取消（窗口库自己发文件事件的时候它自己发）
    fn left(&self) {
        if self.hovering.replace(false) {
            self.send(WindowEvent::HoveredFileCancelled);
        }
    }

    /// 这次拖放离开了窗口或者结束了
    fn ended(&self) {
        self.left();
        self.entered.borrow_mut().clear();
    }
}

/// 窗口开始接收拖来的内容（平台不支持时只记一条调试日志：文件照样能拖）
pub(crate) fn listen<W: HostWindow + 'static>(
    window: &Arc<winit::window::Window>,
    handle: WindowHandle,
    host: &Rc<WindowHost<W>>,
    translate: &Rc<RefCell<TranslateState>>,
    proxy: &EventLoopProxy<UserEvent>,
) {
    let (host, translate, proxy) = (host.clone(), translate.clone(), proxy.clone());
    let own_files = OwnFiles { proxy: proxy.clone(), window: window.id(), hovering: Cell::new(false), entered: RefCell::default() };
    // 帧时钟可能停着：元素的处理器改了界面要出帧
    let wake = move || {
        let _ = proxy.send_event(UserEvent::Wake);
    };
    let input = {
        let wake = wake.clone();
        move |host: &WindowHost<W>, event: InputEvent| {
            host.input(event);
            wake();
        }
    };
    let on_drag: OnExternalDrag = Rc::new(move |drag| {
        // 别的内容那条路上的悬停收回（交过经过才交离开）
        let leave_external = || {
            let event = {
                let mut state = translate.borrow_mut();
                state.time = host::clock::now_duration();
                state.external_left()
            };
            if let Some(event) = event {
                input(&host, event);
            }
        };
        window::with_current(handle, || match drag {
            ExternalDrag::Over { kinds, position, entered_files } => {
                own_files.remember(entered_files);
                // 应用自己的窗口拖来的元素：有元素按元素之间的拖放接收时是它，别的内容那条路上的悬停收回
                if let Some(data) = kinds.own.then(carried).flatten() {
                    let accepted = host.tree().element_drag_over(position, &data);
                    wake();
                    if accepted {
                        own_files.left();
                        leave_external();
                        return Some(ExternalDropEffect::Copy);
                    }
                }
                if kinds.files {
                    own_files.over();
                    let tree = host.tree();
                    return file_answer(tree.external_drop_effect(), tree.is_external_drag_claimed()).map(effect_to_platform);
                }
                let kinds = kinds_from_platform(kinds);
                // 只带着元素之间拖放的数据（拖出去的内容是空的）：别的内容那条路上什么都没有
                if kinds.is_empty() {
                    leave_external();
                    return None;
                }
                let event = {
                    let mut state = translate.borrow_mut();
                    state.time = host::clock::now_duration();
                    state.external_over(position, kinds)
                };
                if let Some(event) = event {
                    input(&host, event);
                }
                host.tree().external_drop_effect().map(effect_to_platform)
            }
            ExternalDrag::Leave => {
                host.tree().element_drag_leave();
                wake();
                own_files.ended();
                leave_external();
                None
            }
            // 返回真正做了的操作（Windows 告诉拖动的来源）：放下时指针下的元素答应的那一种
            ExternalDrag::Drop { mut data, position } => {
                // 应用自己的窗口拖来的元素：有元素接住就是它（来源只许拷贝：说拷贝）
                if let Some(element) = carried() {
                    let accepted = host.tree().element_drop(position, &element);
                    wake();
                    if accepted {
                        own_files.ended();
                        leave_external();
                        translate.borrow_mut().external_ended();
                        return Some(ExternalDropEffect::Copy);
                    }
                }
                let files = std::mem::take(&mut data.files);
                let had_files = !files.is_empty();
                // 文件交给窗口库一样的事件：只交出了路径，不会是应用把它们挪走了，最多算拷贝
                let file_effect = had_files.then(|| {
                    let tree = host.tree();
                    file_answer(tree.external_drop_effect(), tree.is_external_drag_claimed()).map(|effect| match effect {
                        aureole_core::ExternalDropEffect::Link => ExternalDropEffect::Link,
                        _ => ExternalDropEffect::Copy,
                    })
                });
                own_files.dropped(files);
                let data = data_from_platform(data);
                if data.kinds().is_empty() {
                    return file_effect.flatten();
                }
                let effect = host.tree().external_drop_effect().map(effect_to_platform);
                let time = {
                    let mut state = translate.borrow_mut();
                    state.external_ended();
                    host::clock::now_duration()
                };
                input(&host, InputEvent::new(time, InputKind::ExternalDrop { position, data }));
                effect
            }
        })
    });
    match crate::system::backend().window().listen_external_drag(&**window, on_drag) {
        Ok(()) => {}
        // Linux：窗口库只提供文件
        Err(error @ PlatformError::Unsupported(_)) => tracing::debug!("接不了别的应用拖来的内容（文件照样能拖）：{error}"),
        // Windows 上文件也是从这里接的：这时连文件也拖不进来了
        Err(error) => tracing::error!("拖放不可用：{error}"),
    }
}
