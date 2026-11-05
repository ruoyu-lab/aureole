//! 最小的应用：一个窗口、一个按钮、一个带过渡的方块。
//!
//! ```bash
//! cargo run -p aureole --example hello
//! ```

use aureole::prelude::*;

fn main() {
    App::new(|| {
        let count = signal(0);
        let odd = move || count.get() % 2 == 1;
        column()
            .padding(40)
            .gap(20)
            .align(Align::Center)
            .child(text(move || format!("点了 {} 次", count.get())).font_size(20))
            .child(button("点我").on_click(move || count.update(|n| *n += 1)))
            .child(
                rect()
                    .size(64, 64)
                    .radius(16)
                    .background(move || if odd() { Color::hex(0x2563eb) } else { Color::hex(0x94a3b8) })
                    .transition(Property::Background, Tween::new(ms(200)).ease_out())
                    .scale(move || if odd() { 1.4 } else { 1.0 })
                    .transition(Property::Scale, Spring::bouncy()),
            )
    })
    .title("你好")
    .size(480, 360)
    .run();
}
