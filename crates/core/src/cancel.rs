//! 能取消的登记（[`Cancel`]）和它的守卫（[`CancelGuard`]）。
//!
//! 定时器、任务、快捷键这些登记默认归创建时的作用域所有：作用域释放（元素卸载、窗口关闭、副作用重新执行）时取消，
//! 返回的句柄丢掉不影响它，界面代码里 `aureole::after(d, f);` 直接写就行。不在作用域里创建的（`main` 里、插件、
//! 主线程投递的工作、测试的函数体）没有主人，要手动取消；想让它跟着一个值走（放进库自己的结构体、插件的状态），
//! 用 [`Cancel::guard`] 收成守卫：守卫丢掉时取消。
//!
//! 为什么不让句柄本身丢掉就取消：界面代码里大量"定好就不管"的写法（`.on_click(|| aureole::after(..))`、
//! 定时器的闭包里再定一个），处理器的返回值会被直接丢掉，句柄一丢就取消会让这些写法悄悄失效。

use std::fmt;
use std::ops::Deref;

/// 能取消的东西：定时器（[`crate::Timer`]）、后台任务（[`crate::Task`]）、主线程上的任务（[`crate::LocalTask`]）、
/// 快捷键（[`crate::Shortcut`]）、窗口的按键松开监听（[`crate::KeyUpListener`]）、开发工具的文件监视……第三方库自己的登记也可以实现它，就能用 [`Cancel::guard`]。
///
/// 取消已经结束、已经取消的不做任何事
pub trait Cancel {
    /// 取消
    fn cancel(self);

    /// 收成守卫：守卫丢掉时取消（不管在不在作用域里）
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use aureole::{Cancel, CancelGuard, Timer};
    ///
    /// /// 库自己的结构体：它丢掉时定时器跟着取消
    /// struct Autosave {
    ///     _timer: CancelGuard<Timer>,
    /// }
    ///
    /// impl Autosave {
    ///     fn new() -> Self {
    ///         Self { _timer: aureole::every(Duration::from_secs(30), || println!("自动保存")).guard() }
    ///     }
    /// }
    /// ```
    fn guard(self) -> CancelGuard<Self>
    where
        Self: Sized,
    {
        CancelGuard(Some(self))
    }
}

/// 守卫：丢掉时取消里面的登记（[`Cancel::guard`] 得到）。可以借出里面的句柄（`Deref`）查状态
#[must_use = "守卫丢掉时就取消了：要留着它（放进变量、结构体），或者不用守卫"]
pub struct CancelGuard<T: Cancel>(Option<T>);

impl<T: Cancel> CancelGuard<T> {
    /// 不再当守卫，取回里面的登记：之后丢掉它不取消（在作用域里创建的仍然归作用域）
    pub fn into_inner(mut self) -> T {
        self.0.take().expect("守卫里的登记只取一次")
    }

    /// 马上取消
    pub fn cancel(mut self) {
        if let Some(inner) = self.0.take() {
            inner.cancel();
        }
    }
}

impl<T: Cancel> Deref for CancelGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.as_ref().expect("守卫里的登记只取一次")
    }
}

impl<T: Cancel> Drop for CancelGuard<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            inner.cancel();
        }
    }
}

impl<T: Cancel + fmt::Debug> fmt::Debug for CancelGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CancelGuard").field(&self.0).finish()
    }
}

impl Cancel for crate::Timer {
    fn cancel(self) {
        crate::Timer::cancel(self);
    }
}

impl Cancel for crate::LocalTask {
    fn cancel(self) {
        crate::LocalTask::cancel(self);
    }
}

impl<T: Send + 'static> Cancel for crate::Task<T> {
    fn cancel(self) {
        crate::Task::cancel(self);
    }
}

impl Cancel for crate::Shortcut {
    fn cancel(self) {
        crate::Shortcut::cancel(self);
    }
}

impl Cancel for crate::KeyUpListener {
    fn cancel(self) {
        crate::KeyUpListener::cancel(self);
    }
}
