//! 插件和运行时服务在真实窗口里走一遍：插件挂的钩子（窗口创建后、平台事件、每帧开始和结束、退出前）、
//! 别的线程往主线程投递、换上执行器、定时器、`sleep`。打开一个窗口，一秒多以后自己退出，最后打印各个钩子被调用的次数。
//!
//! ```bash
//! cargo run -p aureole --example plugins
//! ```

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use aureole::prelude::*;
use aureole::{Plugin, PlatformEventKind};

/// 记下每个钩子被调用了几次，退出前打印
#[derive(Default)]
struct Counts {
    windows: Cell<u32>,
    events: Cell<u32>,
    eaten: Cell<u32>,
    frames: Cell<u32>,
    posted: Cell<u32>,
}

struct Probe(Rc<Counts>);

impl Plugin for Probe {
    fn build(&self, app: App) -> App {
        let (a, b, c, d) = (self.0.clone(), self.0.clone(), self.0.clone(), self.0.clone());
        app.spawner(aureole::PoolSpawner)
            .on_window_open(move |window| {
                a.windows.set(a.windows.get() + 1);
                println!("[插件] 窗口建好了：{window:?}");
            })
            .on_platform_event(move |event| {
                b.events.set(b.events.get() + 1);
                // 吃掉键盘事件：元素树收不到
                if event.kind() == PlatformEventKind::Keyboard {
                    b.eaten.set(b.eaten.get() + 1);
                    event.stop_propagation();
                }
            })
            .on_frame_end(move |frame| {
                c.frames.set(c.frames.get() + 1);
                if c.frames.get() == 1 {
                    println!("[插件] 第一帧：{:?}，时间 {:.3} 秒", frame.window, frame.time.as_secs_f64());
                }
            })
            .on_exit(move || {
                println!(
                    "[插件] 退出前：窗口 {} 个，平台事件 {} 个（吃掉 {} 个），帧 {} 个，投递 {} 次",
                    d.windows.get(),
                    d.events.get(),
                    d.eaten.get(),
                    d.frames.get(),
                    d.posted.get()
                );
            })
    }
}

fn main() {
    let counts = Rc::new(Counts::default());
    let c = counts.clone();
    App::new(move || {
        let status = signal(String::from("开始"));
        // 别的线程往主线程投递：闭包要 Send，在主线程上执行
        let sender = aureole::main_thread_sender();
        std::thread::spawn(move || {
            for _ in 0..3 {
                sender.send(|| POSTED.with(|p| p.set(p.get() + 1)));
            }
        });
        aureole::spawn_local(async move {
            aureole::sleep(Duration::from_millis(300)).await;
            let n = aureole::spawn(async { 6 * 7 }).await;
            status.set(format!("sleep 醒了，后台算出 {n}"));
        });
        let c = c.clone();
        aureole::after(Duration::from_millis(1200), move || {
            c.posted.set(POSTED.with(Cell::get));
            println!("[脚本] {}", status.get_untracked());
            aureole::quit();
        });
        column().padding(24).child(text(move || status.get()))
    })
    .title("插件")
    .size(360, 160)
    .plugin(Probe(counts))
    .run();
}

thread_local! {
    static POSTED: Cell<u32> = const { Cell::new(0) };
}
