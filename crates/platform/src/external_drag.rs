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
}

impl ExternalDataKinds {
    /// 一种都没有
    pub fn is_empty(self) -> bool {
        !(self.text || self.link || self.image || self.files)
    }
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
    /// 文件（窗口库自己发文件事件的平台上是空的，见模块说明）
    pub files: Vec<PathBuf>,
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
        /// 刚进来时的文件，只有窗口库不管文件的平台（Windows）才有
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

/// 拖放的回调：在主线程上，从系统的拖放流程里直接调进来。返回接不接受（`None` 不接受，系统显示不能放下的光标）；
/// [`ExternalDrag::Leave`]、[`ExternalDrag::Drop`] 的返回值不用
pub type OnExternalDrag = Rc<dyn Fn(ExternalDrag) -> Option<ExternalDropEffect>>;

/// 回调里 panic 不能穿过系统的拖放流程：接住，记一条错误，当成不接受
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn guarded<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!("拖放的处理器 panic 了");
        fallback
    })
}
