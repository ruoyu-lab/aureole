//! 元素上不由属性表生成的链式方法：一次设几个属性的简写（`size`、`full_width`、`border`、`stroke_range`、
//! `translation`、`transform_origin`、`shader`），过渡、退场和时间线的句柄，输入框内部用的设置。
//!
//! 属性本身（`width`、`background`、`font_size`……）的链式方法由属性表（`tree/property_table.rs`）生成，
//! 简写只是依次调用它们，没有自己的行为。这个模块不碰处理器（`element/events.rs`）。

use animation::Curve;

use super::{Element, Shader};
use crate::gradient::Fill;
use crate::layout::Length;
use crate::property::{IntoProp, Property, PropertyValue};

/// 富文本（[`crate::rich_text`]）的各段和链接的处理器
#[derive(Default)]
pub(crate) struct RichProps {
    /// 是富文本（`rich_text()`，或者 `text(..)` 上加了 `span`）
    pub rich: bool,
    pub spans: Vec<crate::rich_text::Span>,
}

impl Element {
    // ---- 布局的简写 ----

    /// 同时设宽和高，同 `.width(w).height(h)`
    pub fn size<W: Into<Length> + 'static, H: Into<Length> + 'static>(self, width: impl IntoProp<W>, height: impl IntoProp<H>) -> Self {
        self.width(width).height(height)
    }

    /// 宽度撑满父元素：`true` 同 `.width(pct(100))`，`false` 同 `.width(auto())`。替换前面设的宽度（包括绑定）
    pub fn full_width(self, full: bool) -> Self {
        self.width(if full { Length::Pct(100.0) } else { Length::Auto })
    }

    /// 高度撑满父元素：`true` 同 `.height(pct(100))`，`false` 同 `.height(auto())`。替换前面设的高度（包括绑定）
    pub fn full_height(self, full: bool) -> Self {
        self.height(if full { Length::Pct(100.0) } else { Length::Auto })
    }

    /// 宽高都撑满父元素，同 `.full_width(full).full_height(full)`
    pub fn full_size(self, full: bool) -> Self {
        self.full_width(full).full_height(full)
    }

    // ---- 绘制的简写 ----

    /// 向内绘制的边框，同 `.border_width(width).border_color(color)`
    pub fn border<T: Into<Fill> + 'static>(self, width: impl IntoProp<f32>, color: impl IntoProp<T>) -> Self {
        self.border_width(width).border_color(color)
    }

    /// 只画描边从 `start` 到 `end` 的一段，同 `.stroke_start(start).stroke_end(end)`
    pub fn stroke_range(self, start: impl IntoProp<f32>, end: impl IntoProp<f32>) -> Self {
        self.stroke_start(start).stroke_end(end)
    }

    /// 平移，逻辑像素，连同子元素；不影响布局。同 `.translation_x(x).translation_y(y)`
    pub fn translation(self, x: impl IntoProp<f32>, y: impl IntoProp<f32>) -> Self {
        self.translation_x(x).translation_y(y)
    }

    /// 缩放和各个方向的旋转绕哪一点：占元素宽高的比例，默认 `(0.5, 0.5)` 是中心，`(0.0, 0.0)` 是左上角。
    /// 同 `.transform_origin_x(x).transform_origin_y(y)`；改它只重画，不重新布局（不做动画）
    pub fn transform_origin(self, x: impl IntoProp<f32>, y: impl IntoProp<f32>) -> Self {
        self.transform_origin_x(x).transform_origin_y(y)
    }

    /// 用自定义着色器填充背景，`params` 是传给着色器的 4 个数（可以绑定信号），
    /// 同 `.set(property::SHADER, shader.clone()).shader_params(params)`。圆角、边框、阴影、透明度和裁剪照常生效
    pub fn shader(self, shader: &Shader, params: impl IntoProp<[f32; 4]>) -> Self {
        self.set(crate::property::SHADER, shader.clone()).shader_params(params)
    }

    // ---- 动画 ----

    /// 属性变化时按曲线过渡，而不是立即跳到新值。
    /// 挂载时的初始值不做动画。同一属性多次声明以最后一次为准。
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let hovered = signal(false);
    /// rect()
    ///     .scale(move || if hovered.get() { 1.06 } else { 1.0 })
    ///     .transition(Property::Scale, Spring::snappy())
    ///     .background(move || if hovered.get() { Color::WHITE } else { Color::hex(0x2c2c2e) })
    ///     .transition(Property::Background, Tween::new(ms(150)).ease_out());
    /// ```
    pub fn transition(mut self, property: Property, curve: impl Into<Curve>) -> Self {
        let (property, curve) = (property.id(), curve.into());
        match self.animations.iter_mut().find(|(p, _)| *p == property) {
            Some(existing) => existing.1 = curve,
            None => self.animations.push((property, curve)),
        }
        self
    }

    /// 退场动画：元素被 `each` / `child_when` 移除时，先把属性过渡到这个值，
    /// 动画结束后才真正卸载。可以声明多个，例如淡出加高度收为 0，下面的行会平滑地补上来。
    ///
    /// 曲线用 [`Element::transition`] 为该属性声明的，没有声明时用 200ms 的 ease-out 补间
    /// （退场要用另一条曲线时见 [`Element::exit_with`]）。
    /// 退场期间元素不响应指针，它的绑定已经释放；随父元素一起卸载时不做退场动画。
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// rect().exit(Property::Opacity, 0.0).exit(Property::Height, 0.0);
    /// ```
    pub fn exit(self, property: Property, value: impl Into<PropertyValue>) -> Self {
        self.push_exit(property, value.into(), None)
    }

    /// 退场动画，用这条曲线（不管为这个属性声明的是哪条）：出现用 150 毫秒、消失用 100 毫秒这类进出不对称的过渡。
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let shown = signal(false);
    /// rect()
    ///     .opacity(move || if shown.get() { 1.0 } else { 0.0 })
    ///     .transition(Property::Opacity, Tween::new(ms(150)).ease_out())
    ///     .exit_with(Property::Opacity, 0.0, Tween::new(ms(100)).ease_out())
    ///     .on_mount(move || shown.set(true));
    /// ```
    pub fn exit_with(self, property: Property, value: impl Into<PropertyValue>, curve: impl Into<Curve>) -> Self {
        self.push_exit(property, value.into(), Some(curve.into()))
    }

    /// 交给关键帧时间线的句柄：[`crate::Timeline`] 里 `handle.animate(..)` 的那些段会动这个元素。
    /// 同一个句柄可以交给多个元素（比如列表的每一行），它们一起动，[`crate::Motion::stagger`] 按界面里的先后错开；
    /// 一个元素也可以加入几个句柄
    ///
    /// ```
    /// # use aureole::prelude::*;
    /// let dot = MotionHandle::new();
    /// let pulse = Timeline::new(dot.animate(Property::Scale, Keyframes::new(1.0).at(ms(300), 1.2).at(ms(600), 1.0)).forever());
    /// rect().size(12, 12).radius(6).background(Color::hex(0x138a84)).motion_handle(dot).on_mount(move || pulse.play());
    /// ```
    pub fn motion_handle(mut self, handle: crate::MotionHandle) -> Self {
        if !self.motion.contains(&handle.0) {
            self.motion.push(handle.0);
        }
        self
    }

    fn push_exit(mut self, property: Property, value: PropertyValue, curve: Option<Curve>) -> Self {
        assert!(property.accepts(&value), "{property:?} 的退场值类型不对：{value:?}");
        let property = property.id();
        match self.exit.iter_mut().find(|(p, ..)| *p == property) {
            Some(existing) => *existing = (property, value, curve),
            None => self.exit.push((property, value, curve)),
        }
        self
    }

    // ---- 输入框 ----

    /// 输入框（[`crate::text_field`]、[`crate::text_area`]）只收 `accept` 认的字，别的字丢掉：打字、粘贴、输入法和听写的提交、
    /// 辅助技术改值，所有进到输入框的文字都过一遍（输入法组字时的预编辑照常显示，提交时才过滤）。
    /// 代码里改绑定的信号不过滤。别的元素上什么都不做
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// let digits = signal(String::new());
    /// text_field(digits).accepted_chars(|c| c.is_ascii_digit());
    /// ```
    pub fn accepted_chars(mut self, accept: fn(char) -> bool) -> Self {
        if let Some(editor) = self.kind_mut::<crate::editor::EditorBlock>() {
            editor.set_accept(accept);
        }
        self
    }
}
