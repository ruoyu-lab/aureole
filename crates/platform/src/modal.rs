//! Windows：系统自己的消息循环（拖动窗口、改窗口大小，菜单栏的菜单、右键菜单、系统菜单打开着）。
//!
//! 这些操作由 `DefWindowProc`、`TrackPopupMenu` 在里面转一个模态循环，松手、菜单关上才返回。窗口库的事件循环这期间不转：
//! 窗口事件和投递的事件照常交出来，但不再报告"这一轮处理完了"（winit 的 `about_to_wait`），也不按截止时间醒来。
//! 运行时在收到的事件里自己推进一轮（[`in_modal_loop`] 为真时），截止时间交给这里的系统计时器（[`wake_after`]）：
//! 模态循环照样分发计时器的消息，到点调用交来的唤醒器。

use std::cell::RefCell;
use std::task::Waker;
use std::time::Duration;

use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GUI_INMENUMODE, GUI_INMOVESIZE, GUI_POPUPMENUMODE, GUI_SYSTEMMENUMODE, GUITHREADINFO, GetGUIThreadInfo, KillTimer, SetTimer,
};

thread_local! {
    /// 定好的计时器（系统给的编号）和到点要叫醒的
    static TIMER: RefCell<Option<(usize, Waker)>> = const { RefCell::new(None) };
    /// 平台层自己调用的、会转模态循环的系统函数（拖到应用外面的 `DoDragDrop`）套了几层
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// 调用会转模态循环的系统函数（`DoDragDrop`）期间拿着：这期间 [`in_modal_loop`] 也是真，运行时照样自己推进一轮
pub(crate) struct ModalGuard(());

impl ModalGuard {
    /// 进入：这个线程的层数加一，释放时减一
    pub(crate) fn enter() -> Self {
        DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self(())
    }
}

impl Drop for ModalGuard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// 调用线程此刻在系统的模态循环里：窗口在移动、改大小，或者有菜单打开着，或者在平台层自己调用的模态函数里（[`ModalGuard`]）
pub(crate) fn in_modal_loop() -> bool {
    if DEPTH.with(std::cell::Cell::get) > 0 {
        return true;
    }
    let mut info = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    // SAFETY: `info` 是调用方的结构体，`cbSize` 填好了；线程编号是调用线程自己的
    let ok = unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &mut info) } != 0;
    ok && info.flags & (GUI_INMOVESIZE | GUI_INMENUMODE | GUI_POPUPMENUMODE | GUI_SYSTEMMENUMODE) != 0
}

/// 过 `after` 调用一次 `waker`（之前定的作废）；`None` 只取消。只在主线程上调用：计时器的消息由这个线程的消息循环分发，
/// 模态循环里也一样
pub(crate) fn wake_after(after: Option<Duration>, waker: &Waker) {
    TIMER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let previous = slot.take().map(|(id, _)| id);
        let Some(after) = after else {
            if let Some(id) = previous {
                // SAFETY: 编号是这个线程上 `SetTimer` 给的、没有窗口的计时器
                unsafe { KillTimer(std::ptr::null_mut(), id) };
            }
            return;
        };
        // 不到 10 毫秒的系统按 10 毫秒算；向上取整，不早于截止时间醒来
        let ms = after.as_micros().div_ceil(1000).clamp(1, u32::MAX as u128) as u32;
        // 没有窗口的计时器：传上一次的编号就是改它，不会多出一个
        // SAFETY: 回调是下面的 `extern "system"` 函数，签名和 `TIMERPROC` 一致
        let id = unsafe { SetTimer(std::ptr::null_mut(), previous.unwrap_or(0), ms, Some(fired)) };
        if id != 0 {
            *slot = Some((id, waker.clone()));
        }
    });
}

/// 计时器到点（主线程，消息循环分发的）：只叫醒一次
unsafe extern "system" fn fired(_window: windows_sys::Win32::Foundation::HWND, _message: u32, id: usize, _time: u32) {
    // SAFETY: 编号是系统交来的、这个线程上的计时器
    unsafe { KillTimer(std::ptr::null_mut(), id) };
    let waker = TIMER.with(|slot| slot.borrow_mut().take_if(|(current, _)| *current == id).map(|(_, waker)| waker));
    if let Some(waker) = waker {
        waker.wake();
    }
}
