//! 从别的应用拖进来的内容：文字、链接、图片、文件。
//!
//! 接口是窗口服务上的 [`crate::WindowService::listen_external_drag`]：系统每次问"放在这里行不行"都调用回调，
//! 回调当场回答（系统按回答显示光标）。文件拖放窗口库本来就做：
//!
//! - macOS：窗口库把拖放登记在窗口上，只认文件。这里给窗口的内容视图加上拖放方法、登记更多类型，
//!   视图比窗口先收到拖放；有文件时原样转给窗口库，它照旧发文件的窗口事件，所以文件拖放的行为不变。
//! - Windows：拖放目标（`IDropTarget`）一个窗口只能登记一个，所以运行时关掉窗口库的文件拖放，全部由这里接；
//!   文件交给回调，运行时换成和窗口库一样的窗口事件。
//! - Linux：窗口库只提供文件（X11 的 XDND、Wayland 的数据设备都不开放别的类型），不支持别的内容，照样能拖文件。
//!
//! 这个进程自己拖出去的（[`crate::begin_drag_out`] 开始、还没结束的会话）经过自己的窗口时，种类里标着 [`ExternalDataKinds::own`]：
//! 内容是空的也交给回调（应用自己的窗口之间拖元素，真正的数据留在进程里）。这时 macOS 上的文件也不转给窗口库，
//! 和 Windows 一样交给回调。

use std::path::PathBuf;
use std::rc::Rc;

use crate::clipboard::RgbaImage;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

/// 拖来的内容有哪几种。有文件时只算文件（访达拖文件时也带着文件名的文字）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExternalDataKinds {
    /// 文字
    pub text: bool,
    /// 链接
    pub link: bool,
    /// 图片
    pub image: bool,
    /// 文件
    pub files: bool,
    /// 带格式的文字（RTF）
    pub rich_text: bool,
    /// 应用登记过的自己定的格式（[`accept_custom_format`]）里至少有一种
    pub custom: bool,
    /// 是这个进程自己拖出去的（[`crate::begin_drag_out`] 开始、还没结束的会话；同一时刻只有一次拖放，所以就是它）
    pub own: bool,
}

impl ExternalDataKinds {
    /// 一种都没有，也不是自己拖出去的
    pub fn is_empty(self) -> bool {
        !(self.text || self.link || self.image || self.files || self.rich_text || self.custom || self.own)
    }
}

/// 拖进来时要读的应用自己定的格式，整个进程一份（[`accept_custom_format`]）
static CUSTOM_FORMATS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 别的程序（或同一个应用的别的窗口、别的实例）拖进来时也读应用自己定的格式 `name`（剪贴板、`DragOut` 的 `custom` 用的那种名字）。
/// 重复登记不算。macOS 上窗口打开时按登记过的格式告诉系统接受哪些拖放类型：要在打开窗口之前调用
pub fn accept_custom_format(name: &str) {
    if name.is_empty() {
        return;
    }
    let mut formats = CUSTOM_FORMATS.lock().unwrap_or_else(|e| e.into_inner());
    if !formats.iter().any(|known| known == name) {
        formats.push(name.to_owned());
    }
}

/// 登记过的自己定的格式
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn custom_formats() -> Vec<String> {
    CUSTOM_FORMATS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 接受的话算什么操作：系统按它显示光标，也告诉拖动的来源
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalDropEffect {
    /// 拷贝
    Copy,
    /// 移动（来源可能删掉原来的）
    Move,
    /// 建链接
    Link,
}

/// 放下的内容
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalDropData {
    /// 文字
    pub text: Option<String>,
    /// 链接
    pub link: Option<String>,
    /// 图片
    pub image: Option<RgbaImage>,
    /// 文件（窗口库自己发文件事件的平台上、不是自己拖出去的时候是空的，见模块说明）
    pub files: Vec<PathBuf>,
    /// 带格式的文字（RTF，见 [`crate::clipboard::rtf`]）
    pub rich_text: Option<Vec<u8>>,
    /// 登记过的自己定的格式里拖来了的：格式名和原样的字节
    pub custom: Vec<(String, Vec<u8>)>,
}

/// 拖放的进展。位置是窗口内容区里的逻辑像素，左上角为原点
#[derive(Clone, Debug, PartialEq)]
pub enum ExternalDrag {
    /// 拖着经过窗口：刚进来时和之后每隔一会儿都有，位置不变也有
    Over {
        /// 内容的种类
        kinds: ExternalDataKinds,
        /// 位置
        position: [f32; 2],
        /// 刚进来时的文件，只有窗口库不管文件的时候（Windows，macOS 上自己拖出去的）才有
        entered_files: Vec<PathBuf>,
    },
    /// 离开窗口或取消
    Leave,
    /// 放下
    Drop {
        /// 内容
        data: ExternalDropData,
        /// 位置
        position: [f32; 2],
    },
}

/// 拖放的回调：在主线程上，从系统的拖放流程里直接调进来。[`ExternalDrag::Over`] 返回接不接受（`None` 不接受，
/// 系统显示不能放下的光标）；[`ExternalDrag::Drop`] 返回真正做了的操作（`None`：没人收下），Windows 把它告诉拖动的来源：
/// 说"移动"时来源会删掉原来的（文件只交出了路径，应用没自己挪走时要说拷贝）。[`ExternalDrag::Leave`] 的返回值不用
pub type OnExternalDrag = Rc<dyn Fn(ExternalDrag) -> Option<ExternalDropEffect>>;

/// 回调里 panic 不能穿过系统的拖放流程：接住，记一条错误，当成不接受
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn guarded<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!("拖放的处理器 panic 了");
        fallback
    })
}
