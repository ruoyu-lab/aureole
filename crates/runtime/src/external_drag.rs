//! 别的应用拖来的内容：平台层的回调（系统的拖放流程里、主线程上直接调进来）换成窗口宿主的输入，当场回答接不接受。
//!
//! 文件有两条路，都汇到窗口库的文件事件（换算见 `translate.rs`，所以文件拖放的行为各平台一样）：
//! macOS 上窗口库自己发，这里只回答；Windows 上拖放目标是平台层接的，它把文件交过来，这里发一样的窗口事件，走事件循环的同一条路。
//! 别的内容（文字、链接、图片）直接交给窗口宿主。系统每隔一会儿问一次，位置没变时不再交（和文件的悬停一样去重）。

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

fn kinds_from_platform(kinds: platform::ExternalDataKinds) -> ExternalDataKinds {
    let mut out = ExternalDataKinds::empty();
    out.set(ExternalDataKinds::TEXT, kinds.text);
    out.set(ExternalDataKinds::LINK, kinds.link);
    out.set(ExternalDataKinds::IMAGE, kinds.image);
    out.set(ExternalDataKinds::FILES, kinds.files);
    out
}

fn data_from_platform(data: platform::ExternalDropData) -> ExternalDropData {
    ExternalDropData {
        text: data.text,
        link: data.link,
        image: data.image.map(|image| RgbaImage { width: image.width, height: image.height, pixels: image.rgba }),
        files: Vec::new(),
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

/// 窗口库不管文件的平台上，把平台层交来的文件换成窗口库一样的事件，排进事件循环
struct OwnFiles {
    proxy: EventLoopProxy<UserEvent>,
    window: WindowId,
    hovering: Cell<bool>,
}

impl OwnFiles {
    fn send(&self, event: WindowEvent) {
        let _ = self.proxy.send_event(UserEvent::Window(self.window, event));
    }

    fn entered(&self, files: Vec<PathBuf>) {
        self.hovering.set(true);
        files.into_iter().for_each(|file| self.send(WindowEvent::HoveredFile(file)));
    }

    fn dropped(&self, files: Vec<PathBuf>) {
        self.hovering.set(false);
        files.into_iter().for_each(|file| self.send(WindowEvent::DroppedFile(file)));
    }

    /// 离开了：是这里发过悬停的才要发取消（窗口库自己发文件事件的平台上它自己发）
    fn left(&self) {
        if self.hovering.replace(false) {
            self.send(WindowEvent::HoveredFileCancelled);
        }
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
    let own_files = OwnFiles { proxy: proxy.clone(), window: window.id(), hovering: Cell::new(false) };
    let input = move |host: &WindowHost<W>, event: InputEvent| {
        host.input(event);
        // 帧时钟可能停着：元素的处理器改了界面要出帧
        let _ = proxy.send_event(UserEvent::Wake);
    };
    let on_drag: OnExternalDrag = Rc::new(move |drag| {
        window::with_current(handle, || match drag {
            ExternalDrag::Over { kinds, entered_files, .. } if kinds.files => {
                own_files.entered(entered_files);
                let tree = host.tree();
                file_answer(tree.external_drop_effect(), tree.is_external_drag_claimed()).map(effect_to_platform)
            }
            ExternalDrag::Over { kinds, position, .. } => {
                let event = {
                    let mut state = translate.borrow_mut();
                    state.time = host::clock::now_duration();
                    state.external_over(position, kinds_from_platform(kinds))
                };
                if let Some(event) = event {
                    input(&host, event);
                }
                host.tree().external_drop_effect().map(effect_to_platform)
            }
            ExternalDrag::Leave => {
                own_files.left();
                let event = {
                    let mut state = translate.borrow_mut();
                    state.time = host::clock::now_duration();
                    state.external_left()
                };
                if let Some(event) = event {
                    input(&host, event);
                }
                None
            }
            ExternalDrag::Drop { mut data, position } => {
                own_files.dropped(std::mem::take(&mut data.files));
                let data = data_from_platform(data);
                if data.kinds().is_empty() {
                    return None;
                }
                let time = {
                    let mut state = translate.borrow_mut();
                    state.external_ended();
                    host::clock::now_duration()
                };
                input(&host, InputEvent::new(time, InputKind::ExternalDrop { position, data }));
                None
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
