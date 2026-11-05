//! 原始输入：窗口交给元素树、还没路由的输入（[`InputEvent`]），按键和修饰键、鼠标按键。
//! 真实窗口换算出来的、测试工具生成的、录制回放读回来的都是它们，走同一个入口（`WindowHost::input`）。
//! 元素上的交互（指针事件、焦点、快捷键）在 `interaction.rs`。

mod event;
mod keys;

pub use event::{
    Buttons, GestureInput, GesturePhase, Ime, ImePurpose, ImeRequest, InputEvent, InputKind, KeyInput, MouseButton, PointerId, PointerInput,
    PointerKind, WheelDelta, WheelInput,
};
pub use keys::{Key, Modifiers, NamedKey, PhysicalKey};
