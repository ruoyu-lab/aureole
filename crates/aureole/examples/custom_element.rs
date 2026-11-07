//! 自己定义一种元素：仪表盘。
//!
//! 实现 `ElementKind`（量大小、继承文字样式、在树外读信号更新、画、卸载、描述自己），用 `Element::from_kind`
//! 做成元素，之后和别的元素一样设大小、背景、事件。内置的画布也是这样做的。拖下面的滑块，仪表盘跟着变。
//!
//! ```bash
//! cargo run -p aureole --example custom_element
//! ```

use aureole::kinds::{AvailableSpace, Drawing, ElementKind, KindHooks, KindInfo, KindSpec, MeasureContext, PaintContext, StyleContext,
    UnmountContext, UpdateContext, register_kind};
use aureole::prelude::*;
use aureole::property::{Dirty, KindMask};

/// 仪表盘：一段 270° 的弧，按比例画亮一截，中间写百分比
pub struct Gauge {
    /// 读数，0..1
    value: Signal<f32>,
    /// 上次更新时读到的（快照里显示）
    shown: f32,
    /// 画好的一幅：在 `update` 里画，在 `paint` 里放进显示列表
    drawing: Drawing,
}

/// 弧从左下（135°）开始，顺时针转 270°
const START: f32 = std::f32::consts::PI * 0.75;
const SWEEP: f32 = std::f32::consts::PI * 1.5;
const TRACK: Color = Color::hex(0xe3e8eb);
const ACCENT: Color = Color::hex(0x138a84);

impl ElementKind for Gauge {
    fn name(&self) -> &'static str {
        "gauge"
    }

    /// 读数是信号：在树外的 `update` 里读，信号变了自动再更新
    fn hooks(&self) -> KindHooks {
        KindHooks::UPDATE
    }

    /// 没给大小时 160 x 120；只给了宽时高是宽的 3/4
    fn measure(&mut self, _: &mut MeasureContext, known: [Option<f32>; 2], _: [AvailableSpace; 2]) -> [f32; 2] {
        match known {
            [Some(w), Some(h)] => [w, h],
            [Some(w), None] => [w, w * 0.75],
            [None, Some(h)] => [h / 0.75, h],
            [None, None] => [160.0, 120.0],
        }
    }

    /// 中间的数字用继承的字体：样式变了重画
    fn restyle(&mut self, cx: &StyleContext) -> Dirty {
        if self.drawing.restyle(cx) { Dirty::PAINT } else { Dirty::empty() }
    }

    fn update(&mut self, cx: &mut UpdateContext) {
        self.shown = self.value.get().clamp(0.0, 1.0);
        let value = self.shown;
        let [w, h] = cx.size();
        self.drawing.record_in(cx, |c| {
            let (center, r) = ([w * 0.5, h * 0.56], (w * 0.5).min(h * 0.62) - 10.0);
            let arc = |to: f32| -> Vec<[f32; 2]> {
                let steps = 48;
                (0..=steps)
                    .map(|i| {
                        let a = START + to * SWEEP * i as f32 / steps as f32;
                        [center[0] + r * a.cos(), center[1] + r * a.sin()]
                    })
                    .collect()
            };
            c.polyline(&arc(1.0), Stroke::new(10.0).round(), TRACK);
            if value > 0.0 {
                c.polyline(&arc(value), Stroke::new(10.0).round(), ACCENT);
            }
            let tip = START + value * SWEEP;
            c.line(center, [center[0] + (r - 18.0) * tip.cos(), center[1] + (r - 18.0) * tip.sin()], Stroke::new(3.0).round(), ACCENT);
            c.fill_circle(center[0], center[1], 5.0, ACCENT);
            let label = CanvasTextStyle::new(20.0).font_weight(600.0).align(Align::Center, Align::Center).tabular_numbers(true);
            c.text(center[0], center[1] + r * 0.55, format!("{:.0}%", value * 100.0), &label);
        });
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        self.drawing.paint(cx);
    }

    fn unmount(&mut self, cx: &mut UnmountContext) {
        self.drawing.release(cx);
    }

    fn describe(&self, info: &mut KindInfo) {
        info.summary = Some(format!("{:.0}%", self.shown * 100.0));
    }
}

/// 仪表盘元素：读数是 0..1 的信号
pub fn gauge(value: Signal<f32>) -> Element {
    Element::from_kind(Gauge { value, shown: 0.0, drawing: Drawing::new() })
}

/// 登记到种类表里：之后能按名字 `gauge` 找到并做出一个（读数是新建的信号）
pub fn register() {
    register_kind(KindSpec { name: "gauge", kind_mask: KindMask::CUSTOM, create: || gauge(signal(0.5)) });
}

/// 例子的界面：仪表盘和控制它的滑块
pub fn app() -> impl View {
    let value = signal(0.72f32);
    column()
        .padding(32)
        .gap(20)
        .align(Align::Center)
        .child(gauge(value).width(220).radius(16).background(Color::hex(0xf6f8f9)).name("gauge"))
        .child(slider(value).width(220))
        .child(text("自己定义的元素：量大小、在树外读信号、用画笔画").font_size(13).color(Color::hex(0x5b6b73)))
}

#[allow(dead_code, reason = "测试里也引用这个文件，那里不用 main")]
fn main() {
    register();
    App::new(app).title("自定义元素").size(360, 360).run();
}
