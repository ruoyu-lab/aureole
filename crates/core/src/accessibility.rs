//! 无障碍：读屏软件（VoiceOver、讲述人、Orca）和只用键盘的用户。
//!
//! 每个窗口一个 AccessKit 适配器。辅助技术第一次来要树时（运行时收到 `accesskit_winit::WindowEvent::InitialTreeRequested`）
//! 才建整棵无障碍树，之前元素树里只多一次布尔判断；之后每帧只把变了的节点交给适配器。
//! 元素树到无障碍树的映射、增量更新、辅助技术发来的操作在 `tree/accessibility.rs`。
//!
//! 角色默认按元素推断：文字是标签，输入框是文本框，有 `on_click` 的是按钮，
//! 滚动视口是滚动区域，模态浮层是对话框，提示是提示，有替代文字的图片是图片……
//! 推断不对时用 [`Element::accessibility_role`]、[`Element::accessibility_name`] 等显式声明。

use std::rc::Rc;

/// AccessKit 本身：测试里读写无障碍树、请求操作时用它的类型
pub use accesskit;
pub use accesskit::{Live, Role, Toggled};

use crate::element::Element;
use crate::handlers::{EventType, Handler, owned};
use crate::property::{IntoProp, Prop};

/// 数值和范围：滑块、进度条、步进器。见 [`Element::accessibility_value`]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeValue {
    /// 当前值
    pub value: f64,
    /// 最小值
    pub min: f64,
    /// 最大值
    pub max: f64,
    /// 增减一次的步长；`None` 时按范围的十分之一
    pub step: Option<f64>,
}

impl RangeValue {
    /// `min..=max` 里的 `value`，步长按范围的十分之一（用 [`Self::step`] 改）
    pub fn new(value: f64, min: f64, max: f64) -> Self {
        Self { value, min, max, step: None }
    }

    /// 增减一次的步长（辅助技术的"增大""减小"）
    pub fn step(mut self, step: f64) -> Self {
        self.step = Some(step);
        self
    }

    /// 增减一次的步长
    pub(crate) fn stride(&self) -> f64 {
        self.step.filter(|s| *s > 0.0).unwrap_or((self.max - self.min).abs() / 10.0)
    }
}

impl IntoProp<RangeValue> for RangeValue {
    fn into_prop(self) -> Prop<RangeValue> {
        Prop::Static(self)
    }
}

impl IntoProp<Toggled> for Toggled {
    fn into_prop(self) -> Prop<Toggled> {
        Prop::Static(self)
    }
}

/// 元素上和无障碍有关、又不是属性的设置（无障碍的属性定义在属性表 `tree/property_table.rs` 里），用到时才分配
#[derive(Default)]
pub(crate) struct AccessibilityProps {
    /// 提示的文字（[`Element::tooltip`]）：没有描述时当描述
    pub tooltip: Option<Prop<String>>,
    /// 标签：给这个句柄的元素命名（`controls::Label::target`）
    pub label_for: Option<crate::interaction::FocusHandle>,
}

impl Element {
    /// 无障碍属性（没设过时建一份）
    fn accessibility(&mut self) -> &mut AccessibilityProps {
        self.accessibility.get_or_insert_default()
    }

    /// 这个元素是焦点句柄 `handle` 那个元素的标签：对辅助技术，那个元素"由这个元素命名"（它自己没有名字时，
    /// 这个元素里的文字就是它的名字）。只管辅助技术；点了作用到那个元素上用 [`crate::FocusHandle::activate`]
    /// （基础控件的 `controls::Label::target` 两样都做了）
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let (volume, focus) = (signal(0.5f32), FocusHandle::new());
    /// row().gap(8)
    ///     .child(text("音量").accessibility_label_target(focus).on_click(move || focus.activate()))
    ///     .child(slider(volume).focus_handle(focus));
    /// ```
    pub fn accessibility_label_target(mut self, handle: crate::interaction::FocusHandle) -> Self {
        self.accessibility().label_for = Some(handle);
        self
    }

    /// 辅助技术要求改数值（拖动滑块、"增大""减小"）：参数是新值，已经夹在范围里。
    /// 增大、减小按 [`RangeValue::step`] 算
    pub fn on_value_request<R>(self, handler: impl Fn(f64) -> R + 'static) -> Self {
        self.listen(EventType::ValueRequest, Handler::Number(owned(handler)))
    }

    /// 提示的文字也交给辅助技术（当描述）
    pub(crate) fn accessibility_tooltip(mut self, label: Rc<dyn Fn() -> String>) -> Self {
        self.accessibility().tooltip = Some(Prop::Dynamic(Box::new(move || label())));
        self
    }
}
