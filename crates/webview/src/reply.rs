//! 等内核答复的 future：内核在它的线程上做完（读 Cookie、清缓存……）时回调，这里把结果交给等着的人。
//! 答复的一方（[`Responder`]）能跨线程；没答复就被放掉时按"没有结果"（`T::default()`）答复，等的一方不会一直等。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// 等内核做完一件事的结果（读 Cookie、写 Cookie、清缓存……）。在主线程上 `.await`（`spawn_local` 里）
#[must_use = "不等它的话结果就丢了"]
pub struct Reply<T> {
    state: Arc<Mutex<State<T>>>,
}

/// 答复 [`Reply`] 的一方
pub(crate) struct Responder<T: Default> {
    state: Option<Arc<Mutex<State<T>>>>,
}

fn lock<T>(state: &Mutex<State<T>>) -> std::sync::MutexGuard<'_, State<T>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一对：等的一方、答复的一方
pub(crate) fn reply<T: Default>() -> (Reply<T>, Responder<T>) {
    let state = Arc::new(Mutex::new(State { value: None, waker: None }));
    (Reply { state: state.clone() }, Responder { state: Some(state) })
}

impl<T: Default> Responder<T> {
    /// 答复（只答一次：答复以后再放掉不会再答）
    pub(crate) fn respond(mut self, value: T) {
        self.send(value);
    }

    fn send(&mut self, value: T) {
        let Some(state) = self.state.take() else { return };
        let waker = {
            let mut state = lock(&state);
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T: Default> Drop for Responder<T> {
    fn drop(&mut self) {
        self.send(T::default());
    }
}

impl<T> Future for Reply<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = lock(&self.state);
        match state.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
