//! 打印：系统的打印对话框（选打印机、纸张、份数），把画好的每一页交给打印机。
//!
//! 每一页怎么排、怎么画是运行时的事（按对话框选好的纸张排版、画成位图）；这里只管对话框和交给系统：
//!
//! - macOS：`NSPrintPanel`（有父窗口时是它的表单），选好的纸张记在 `NSPrintInfo` 里；印的时候一个只在打印时用的视图
//!   一页一页画这些位图，交给 `NSPrintOperation`（不再弹面板，显示进度）。
//! - Windows：`PrintDlgEx`（在自己的线程里，和文件对话框一样不挡事件循环）给打印机的设备上下文；
//!   印的时候也在自己的线程里 `StartDoc`、每页 `StretchDIBits` 铺满能印到的范围、`EndDoc`。
//! - Linux：还没有（[`crate::Capabilities::supports_print`] 是假）。
//!
//! 对话框给的设备留在平台层，[`PrintSetup`] 只带编号：之后不管印不印都要交一次 [`crate::DialogService::print`]
//! （不印时交空的页），设备才释放。

use crate::PlatformError;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

/// 用户在打印对话框里选好的打印机和纸张。尺寸都是点（1/72 英寸），已经按纸张的方向转过
#[derive(Debug, PartialEq)]
pub struct PrintSetup {
    /// 整张纸的宽、高
    pub paper: [f32; 2],
    /// 能印到的范围：左、上、宽、高（纸张坐标，左上角为原点）
    pub printable: [f32; 4],
    /// 打印机的分辨率（每英寸多少点）；系统不按点阵交给打印机时（macOS）是建议画成的分辨率
    pub dpi: f32,
    /// 这次打印在平台层的编号：对话框给的设备留在平台层
    pub(crate) id: u64,
}

/// 打印对话框关掉了：选好时是 `Ok(Some(..))`，取消是 `Ok(None)`。可能在别的线程调用
pub type PrintDialogDone = Box<dyn FnOnce(Result<Option<PrintSetup>, PlatformError>) + Send>;

/// 交给打印机了（进了系统的打印队列）是 `Ok(true)`；用户在系统接着弹的界面里取消了（存成 PDF 时问文件名、进度面板）是 `Ok(false)`；
/// 或者出错了。可能在别的线程调用
pub type PrintDone = Box<dyn FnOnce(Result<bool, PlatformError>) + Send>;

/// 下一次打印的编号
pub(crate) fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 页的像素数不对时的错误
pub(crate) fn check_pages(pages: &[crate::RgbaImage]) -> Result<(), PlatformError> {
    match pages.iter().position(|page| !page.is_valid() || page.width == 0 || page.height == 0) {
        Some(index) => Err(PlatformError::System(format!("第 {} 页的像素数不对", index + 1).into())),
        None => Ok(()),
    }
}
