//! 3D 变换和共享元素：绕 x、y 轴旋转、透视、变换中心、背面不可见，以及在两个元素之间"飞过去"的过渡。
//!
//! 这些属性定义在属性表（`tree/property_table.rs`）里，这里只有共享元素带曲线的写法。
//! 绕 x、y 轴转过的元素总是一个合成层：整层的内容在一个平面上，投到屏幕上是一个投影变换，
//! 由着色器直接按透视画出来。

use animation::Curve;

use crate::element::Element;
use crate::property::IntoProp;

impl Element {
    /// 共享元素，飞行用这条曲线（默认是响应 0.4 秒的临界阻尼弹簧），同 `.shared(tag).set(property::SHARED_CURVE, curve)`，
    /// 见 [`Element::shared`]
    pub fn shared_with<S: Into<String> + 'static>(self, tag: impl IntoProp<S>, curve: impl Into<Curve>) -> Self {
        self.shared(tag).set(crate::property::SHARED_CURVE, curve.into())
    }
}
