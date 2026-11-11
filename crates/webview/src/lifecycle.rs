//! 网页的生命周期：前台、后台、冻结、丢弃、关掉。
//!
//! 视图卸载以后网页不关，转到后台：内核按"看不见"处理（不出帧，定时器放慢，页面里的 `document.visibilityState`
//! 是 `hidden`），纹理留着；再挂上时回到前台，接着显示（页面里的状态、历史记录都还在，切换时不用等重新加载）。
//! 后台的网页还可以：
//!
//! - **冻结**：脚本、定时器、网络回调都停下，内存留着（DevTools 协议的 `Page.setWebLifecycleState`）；
//! - **丢弃**：关掉、释放渲染进程和纹理，只记着地址和标题，再挂上或者变回活动时按地址重新打开（历史记录没了）。
//!
//! 再挂上时冻结的、丢弃的都自动变回活动。控制器和元素都放掉以后网页才真正关掉（`Shared` 的 `Drop`），
//! 或者应用调了 [`close`]（之后 `load`、`reload` 才重新打开）。
//!
//! 内核替页面开的新窗口（`popup`）在挂上之前也算后台。

use cef::{ImplBrowser, ImplBrowserHost, PaintElementType};

use crate::state::Shared;

/// 网页的生命周期（[`crate::WebController::lifecycle`]）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lifecycle {
    /// 活动：在前台显示，或者在后台照常跑（定时器放慢）
    #[default]
    Active,
    /// 冻结：脚本、定时器都停下，内存留着。只有后台的网页能冻结
    Frozen,
    /// 丢弃：网页关掉了，只记着地址和标题，挂上或者变回活动时重新打开。只有后台的网页能丢弃
    Discarded,
}

fn host(shared: &Shared) -> Option<cef::BrowserHost> {
    shared.browser.borrow().as_ref().and_then(ImplBrowser::host)
}

/// 元素挂上了：回到前台。冻结的解冻，丢弃的变回活动（元素接着按地址打开）
pub(crate) fn shown(shared: &Shared) {
    if shared.shown.replace(true) {
        return;
    }
    match shared.lifecycle.get_untracked() {
        Lifecycle::Frozen => set_web_state(shared, "active"),
        Lifecycle::Discarded => {}
        Lifecycle::Active => {}
    }
    shared.lifecycle.set_if_changed(Lifecycle::Active);
    if let Some(host) = host(shared) {
        host.was_hidden(0);
        host.was_resized();
        // 后台时没出帧：让内核整页重画一次
        host.invalidate(PaintElementType::VIEW);
    }
}

/// 元素卸载了：转到后台。网页不关，纹理留着（再挂上时先显示上次的画面）
pub(crate) fn hidden(shared: &Shared) {
    if !shared.shown.replace(false) {
        return;
    }
    if let Some(host) = host(shared) {
        host.set_focus(0);
        host.was_hidden(1);
    }
}

/// 网页建好了（`engine::created`）：还没挂到元素上的（内核替页面开的新窗口、后台重新打开的）按后台建
pub(crate) fn created(shared: &Shared, host: &cef::BrowserHost) {
    if !shared.shown.get() {
        host.was_hidden(1);
    }
}

/// 改生命周期。冻结、丢弃只对后台的网页（不在视图上）有效，在前台时不理；变回活动时冻结的解冻、丢弃的重新打开
pub(crate) fn set(shared: &std::rc::Rc<Shared>, state: Lifecycle) {
    let now = shared.lifecycle.get_untracked();
    if now == state {
        return;
    }
    if state != Lifecycle::Active && shared.shown.get() {
        tracing::debug!(?state, "网页在前台，不冻结、不丢弃");
        return;
    }
    match state {
        Lifecycle::Frozen => {
            if now == Lifecycle::Discarded || host(shared).is_none() {
                return;
            }
            set_web_state(shared, "frozen");
        }
        Lifecycle::Discarded => {
            if now == Lifecycle::Frozen {
                set_web_state(shared, "active");
            }
            release(shared);
        }
        Lifecycle::Active => match now {
            Lifecycle::Frozen => set_web_state(shared, "active"),
            Lifecycle::Discarded => {
                shared.lifecycle.set_if_changed(Lifecycle::Active);
                // 在前台时元素自己会打开（它读生命周期）；在后台时这里打开，按后台建
                if !shared.shown.get()
                    && !shared.closed.get_untracked()
                    && let Err(error) = crate::engine::open(shared)
                {
                    tracing::warn!("重新打开丢弃了的网页失败：{error}");
                }
                return;
            }
            Lifecycle::Active => {}
        },
    }
    shared.lifecycle.set_if_changed(state);
}

/// 关掉网页（应用的 `close`）：之后不按地址自动重新打开，`load`、`reload` 时才开
pub(crate) fn close(shared: &Shared) {
    shared.closed.set_if_changed(true);
    shared.lifecycle.set_if_changed(Lifecycle::Active);
    release(shared);
}

/// 关掉网页，放掉离屏绘制的纹理
fn release(shared: &Shared) {
    crate::engine::close(shared);
    shared.page.render().clear();
    shared.page.set_texture(None);
    // 元素重画一次：不再引用纹理
    shared.frame.update(|n| *n = n.wrapping_add(1));
}

/// 经 DevTools 协议改页面的生命周期（`active`、`frozen`）
fn set_web_state(shared: &Shared, state: &str) {
    if let Some(host) = host(shared) {
        crate::scripts::send(&host, "Page.setWebLifecycleState", &[("state", crate::scripts::Param::Text(state.to_owned()))]);
    }
}
