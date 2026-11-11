//! 事件的入口：窗口（和测试工具、录制回放）把输入交给元素树的函数。
//!
//! 入口函数拿的是 [`TreeRef`]，自己决定什么时候借用树，约定是**先求值、再借用树**：
//!
//! - 借着树的时候不调用任何用户代码，也不写信号：树上的方法（`impl Tree`）只改树，
//!   把要调用的处理器、要写的信号收集起来返回；
//! - 处理器在释放树的借用之后调用（一般放在 `batch` 里）：里面可以随意读写信号，
//!   信号触发的副作用会再借用树；
//! - 要求值的用户代码（拖放的数据和接收条件之类）在借用树之前或者释放之后调用；
//! - 一个入口里可以借用树好几次，每次都在调用用户代码之前释放。
//!
//! 指针（移动、按下、松开、其他键、拖动）在 `pointer.rs`，触摸屏的手指在 `touch.rs`，手势开始时谁拿到（拖动、捏合、旋转）在 `gesture.rs`，键盘和输入法在 `keys.rs`，
//! 从系统拖进来的文件在 `files.rs`，命中测试在 `hit.rs`，输入目标（输入框、可选中的文字、字符网格……的焦点、指针、按键）
//! 在 `target.rs`，输入法（交给文字输入客户端）在 `compose.rs`。挂载和属性绑定守同样的约定，在 `tree/mount.rs`；
//! 推进一帧在 `tree/frame.rs`。

mod compose;
mod dispatch;
mod fields;
mod files;
mod gesture;
mod hit;
mod keys;
mod pointer;
mod target;
mod touch;
mod window_drag;

pub(crate) use compose::end_composition;
pub(crate) use fields::field_change;
pub(crate) use files::{external_drag_leave, external_drag_over, external_drop, file_drop, file_hover};
pub(crate) use gesture::{pinch, rotate};
pub(crate) use keys::{ime, key_down, key_up, release_keys, set_modifiers};
pub(in crate::tree) use keys::finish_edit;
pub(crate) use pointer::{cancel_pointer, hand_off, mouse_button, pointer_move, refresh_hover};
pub(in crate::tree) use dispatch::{Options, RoutePath, deliver, dispatch};
pub(in crate::tree) use hit::HitPath;
pub(crate) use touch::{touch_cancel, touch_down, touch_move, touch_up};
pub(in crate::tree) use touch::{Finger, Touches};
pub(in crate::tree) use window_drag::WindowAreas;

use reactive::batch;

use super::FocusCallbacks;

pub(in crate::tree) fn run_focus_callbacks(callbacks: FocusCallbacks) {
    if !callbacks.is_empty() {
        batch(|| callbacks.iter().for_each(|(handler, focused)| handler(*focused)));
    }
}
