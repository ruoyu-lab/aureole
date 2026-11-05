//! 控件构建器上的元素方法。
//!
//! 控件的构建器（比如 `checkbox(v)`）不是元素，建好才是。为了让每种控件和普通元素一样能直接点、拖、悬停、设外边距，
//! 构建器实现 [`ElementMethods`]：链式方法先记下来，建好元素时按调用的顺序用上。
//!
//! 只要实现一个方法 [`ElementMethods::element`]，别的都有了：事件和不由属性表生成的方法写在这里，属性的方法
//! （[`crate::ElementProps`]）由属性表（`tree/property_table.rs`）生成，加属性不用改这里。
//! 构建器自己有同名方法时（`.style`、`.label`、`.focus_handle`）用它自己的。

use animation::Curve;
use reactive::Signal;
use crate::geometry::Rect;

use super::Element;
use crate::drag_drop::{DragOut, DropEvent};
use crate::event::{FieldChangeEvent, IntoHandler, KeyEvent};
use crate::field::FieldHandle;
use crate::gradient::Fill;
use crate::interaction::{FocusHandle, IntoStackOrder};
use crate::layout::Length;
use crate::overlay::Overlay;
use crate::property::{IntoProp, Property, PropertyValue};

/// 收事件对象的处理器，签名同元素上的同名方法
macro_rules! handlers {
    ($($event:ty: $($name:ident)+;)*) => { $($(
        #[doc = concat!("同 [`Element::", stringify!($name), "`]")]
        fn $name<M>(self, handler: impl IntoHandler<$event, M> + 'static) -> Self {
            self.element(move |el| el.$name(handler))
        }
    )+)* };
}

/// 收 `true`、`false` 的处理器
macro_rules! flags {
    ($($name:ident)+) => { $(
        #[doc = concat!("同 [`Element::", stringify!($name), "`]")]
        fn $name<R>(self, handler: impl Fn(bool) -> R + 'static) -> Self {
            self.element(move |el| el.$name(handler))
        }
    )+ };
}

/// 收按键事件的处理器
macro_rules! keys {
    ($($name:ident)+) => { $(
        #[doc = concat!("同 [`Element::", stringify!($name), "`]")]
        fn $name(self, handler: impl Fn(&KeyEvent) + 'static) -> Self {
            self.element(move |el| el.$name(handler))
        }
    )+ };
}

/// 收一个值的方法
macro_rules! values {
    ($($name:ident($value:ident: $ty:ty);)+) => { $(
        #[doc = concat!("同 [`Element::", stringify!($name), "`]")]
        fn $name(self, $value: $ty) -> Self {
            self.element(move |el| el.$name($value))
        }
    )+ };
}

/// 收一个能转成长度的值的方法
macro_rules! lengths {
    ($($name:ident)+) => { $(
        #[doc = concat!("同 [`Element::", stringify!($name), "`]")]
        fn $name<L: Into<Length> + 'static>(self, value: impl IntoProp<L> + 'static) -> Self {
            self.element(move |el| el.$name(value))
        }
    )+ };
}

/// 还不是元素、建好才是的东西（控件的构建器）上的元素方法：记下来，建好元素时按调用的顺序用上。
///
/// 只要实现 [`Self::element`]。属性的方法在 [`crate::ElementProps`] 上，实现了这个 trait 就有；
/// 类型自己有同名方法时用它自己的。
pub trait ElementMethods: Sized {
    /// 建好元素以后对它做的事，别的方法都通过它。这里没有的元素方法也可以这样用：
    /// `checkbox(v).element(|el| el.draggable(|| 1))`
    fn element(self, f: impl FnOnce(Element) -> Element + 'static) -> Self;

    // ---- 事件 ----

    handlers! {
        crate::ClickEvent: on_click on_click_capture on_double_click on_secondary_click on_middle_click;
        crate::PointerEvent: on_pointer_down on_pointer_down_capture on_pointer_up on_pointer_up_capture
            on_pointer_move on_pointer_move_capture on_pointer_cancel on_pointer_enter on_pointer_leave;
        crate::WheelEvent: on_wheel on_wheel_capture;
        crate::ScrollEvent: on_scroll;
        crate::DragEvent: on_drag on_drag_capture;
        crate::PinchEvent: on_pinch on_pinch_capture;
        crate::RotateEvent: on_rotate on_rotate_capture;
        crate::FileDropEvent: on_file_drop;
        crate::ExternalDragEvent: on_external_drag_over on_external_drag_leave;
        crate::ExternalDropEvent: on_external_drop;
        DropEvent: on_drag_enter on_drag_over on_drag_leave;
        FieldChangeEvent: on_field_change on_field_change_capture;
    }

    flags!(on_hover on_press on_focus on_keyboard_focus on_file_hover on_drag_out_end);
    keys!(on_key_down on_key_up on_key_down_capture on_key_up_capture);

    /// 同 [`Element::on_mount`]
    fn on_mount<R>(self, f: impl FnOnce() -> R + 'static) -> Self {
        self.element(move |el| el.on_mount(f))
    }

    // ---- 拖放 ----

    /// 同 [`Element::draggable`]
    fn draggable<T: 'static>(self, payload: impl Fn() -> T + 'static) -> Self {
        self.element(move |el| el.draggable(payload))
    }

    /// 同 [`Element::on_drop`]
    fn on_drop<T: Clone + 'static, R>(self, handler: impl Fn(T, &DropEvent) -> R + 'static) -> Self {
        self.element(move |el| el.on_drop(handler))
    }

    /// 同 [`Element::accepts_drop`]
    fn accepts_drop<T: 'static>(self, accept: impl Fn(&T) -> bool + 'static) -> Self {
        self.element(move |el| el.accepts_drop(accept))
    }

    /// 同 [`Element::drag_out`]
    fn drag_out(self, items: impl Fn() -> DragOut + 'static) -> Self {
        self.element(move |el| el.drag_out(items))
    }

    // ---- 布局和绘制的简写 ----

    lengths!(margin margin_x margin_y inset);

    values! {
        full_width(full: bool);
        full_height(full: bool);
        full_size(full: bool);
        focus_handle(handle: FocusHandle);
        field_handle(handle: FieldHandle);
        motion_handle(handle: crate::MotionHandle);
        report_bounds(rect: Signal<Rect>);
    }

    /// 同 [`Element::size`]
    fn size<W: Into<Length> + 'static, H: Into<Length> + 'static>(self, width: impl IntoProp<W> + 'static, height: impl IntoProp<H> + 'static) -> Self {
        self.element(move |el| el.size(width, height))
    }

    /// 同 [`Element::position`]
    fn position<X: Into<Length> + 'static, Y: Into<Length> + 'static>(self, x: impl IntoProp<X> + 'static, y: impl IntoProp<Y> + 'static) -> Self {
        self.element(move |el| el.position(x, y))
    }

    /// 同 [`Element::border`]
    fn border<T: Into<Fill> + 'static>(self, width: impl IntoProp<f32> + 'static, color: impl IntoProp<T> + 'static) -> Self {
        self.element(move |el| el.border(width, color))
    }

    /// 同 [`Element::translation`]
    fn translation(self, x: impl IntoProp<f32> + 'static, y: impl IntoProp<f32> + 'static) -> Self {
        self.element(move |el| el.translation(x, y))
    }

    /// 同 [`Element::transform_origin`]
    fn transform_origin(self, x: impl IntoProp<f32> + 'static, y: impl IntoProp<f32> + 'static) -> Self {
        self.element(move |el| el.transform_origin(x, y))
    }

    /// 同 [`Element::stack_order`]
    fn stack_order(self, z: impl IntoStackOrder + 'static) -> Self {
        self.element(move |el| el.stack_order(z))
    }

    // ---- 过渡和退场 ----

    /// 同 [`Element::transition`]
    fn transition(self, property: Property, curve: impl Into<Curve>) -> Self {
        let curve = curve.into();
        self.element(move |el| el.transition(property, curve))
    }

    /// 同 [`Element::exit`]
    fn exit(self, property: Property, value: impl Into<PropertyValue>) -> Self {
        let value = value.into();
        self.element(move |el| el.exit(property, value))
    }

    /// 同 [`Element::exit_with`]
    fn exit_with(self, property: Property, value: impl Into<PropertyValue>, curve: impl Into<Curve>) -> Self {
        let (value, curve) = (value.into(), curve.into());
        self.element(move |el| el.exit_with(property, value, curve))
    }

    /// 同 [`Element::shared_with`]
    fn shared_with<S: Into<String> + 'static>(self, tag: impl IntoProp<S> + 'static, curve: impl Into<Curve>) -> Self {
        let curve = curve.into();
        self.element(move |el| el.shared_with(tag, curve))
    }

    // ---- 浮层 ----

    /// 同 [`Element::tooltip`]
    fn tooltip<S: Into<String> + 'static>(self, label: impl IntoProp<S> + 'static) -> Self {
        self.element(move |el| el.tooltip(label))
    }

    /// 同 [`Element::overlay`]
    fn overlay(self, show: impl IntoProp<bool> + 'static, overlay: Overlay) -> Self {
        self.element(move |el| el.overlay(show, overlay))
    }
}
