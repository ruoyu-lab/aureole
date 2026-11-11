//! 网页视图的例子：窗口里先只有一行字，过一会儿（默认 2 秒）挂上一个网页视图，这时才启动 CEF。
//! 引擎的日志（什么时候启动、用了多久、网页加载完没有）打在标准错误，每行前面是进程里过了多久。
//!
//! macOS 上内核框架和辅助程序要在应用包里，用打包脚本编译、打包、运行：
//!
//! ```bash
//! CEF_PATH=…/target/cef crates/webview/run-example.sh browser
//! CEF_PATH=…/target/cef crates/webview/run-example.sh browser -- https://example.org --mount-after 1 --quit-after 10
//! AUREOLE_WEBVIEW_LOG=info CEF_PATH=…/target/cef crates/webview/run-example.sh browser    # 连 CEF 自己的日志一起看
//! ```
//!
//! 参数：第一个不以 `--` 开头的是网址；`--mount-after 秒` 过多久挂上网页视图；`--unmount-after 秒` 过多久卸载它
//! （网页转到后台，不关）；`--quit-after 秒` 过多久自己退出（不给时关窗口退出）；`--terminate-after 秒` 过多久
//! 像菜单里的"退出"那样由系统结束（macOS 的 `terminate:`：窗口还开着就走退出钩子，然后结束进程）。
//! `--resize-test 秒` 挂上以后连续改这么多秒的窗口大小（和拖窗口边一样，每次重新配置表面）。
//! `--minimize-after 秒`、`--restore-after 秒` 过多久最小化窗口、再还原（看窗口看不见时页面还画不画）。
//! `RUST_LOG` 可以换日志的过滤（`目标=级别`，逗号隔开）。

use std::time::Duration;

use aureole::prelude::*;
use aureole_webview::{WebController, web_view};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// 默认打印的日志：警告，加上例子和网页视图自己的
const LOG: &str = "warn,browser=info,aureole_webview=debug";

struct Options {
    url: String,
    mount_after: Duration,
    unmount_after: Option<Duration>,
    quit_after: Option<Duration>,
    terminate_after: Option<Duration>,
    resize_test: Option<Duration>,
    minimize_after: Option<Duration>,
    restore_after: Option<Duration>,
}

fn options() -> Options {
    let mut options = Options { url: "https://example.com".into(), mount_after: Duration::from_secs(2), unmount_after: None, quit_after: None, terminate_after: None, resize_test: None, minimize_after: None, restore_after: None };
    let mut args = std::env::args().skip(1);
    let seconds = |value: Option<String>| value.and_then(|v| v.parse::<f64>().ok()).filter(|s| *s >= 0.0).map(Duration::from_secs_f64);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mount-after" => options.mount_after = seconds(args.next()).unwrap_or(options.mount_after),
            "--unmount-after" => options.unmount_after = seconds(args.next()),
            "--quit-after" => options.quit_after = seconds(args.next()),
            "--terminate-after" => options.terminate_after = seconds(args.next()),
            "--resize-test" => options.resize_test = seconds(args.next()),
            "--minimize-after" => options.minimize_after = seconds(args.next()),
            "--restore-after" => options.restore_after = seconds(args.next()),
            _ if !arg.starts_with("--") => options.url = arg,
            _ => eprintln!("不认识的参数 {arg}"),
        }
    }
    options
}

fn main() {
    let filter: Targets = std::env::var("RUST_LOG").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| LOG.parse().expect("默认的日志过滤"));
    let log = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_timer(tracing_subscriber::fmt::time::uptime()).with_target(false);
    tracing_subscriber::registry().with(log).with(filter).init();
    let Options { url, mount_after, unmount_after, quit_after, terminate_after, resize_test, minimize_after, restore_after } = options();
    tracing::info!("例子启动了，{} 秒后挂上网页视图（{url}）", mount_after.as_secs_f64());

    App::new(move || {
        let page = WebController::new(url.clone());
        let mounted = signal(false);
        after(mount_after, move || {
            tracing::info!("挂上网页视图");
            mounted.set(true);
        });
        if let Some(unmount_after) = unmount_after {
            after(unmount_after, move || {
                tracing::info!("卸载网页视图");
                mounted.set(false);
            });
        }
        if let Some(quit_after) = quit_after {
            after(quit_after, || {
                tracing::info!("时间到了，退出");
                aureole::quit();
            });
        }
        if let Some(length) = resize_test {
            // 挂上 1 秒以后，每 16 毫秒改一次窗口大小，宽高来回变：和拖着窗口边改大小一样，每次都重新配置表面
            let window = aureole::current_window();
            after(mount_after + Duration::from_secs(1), move || {
                tracing::info!("开始连续改窗口大小，{} 秒", length.as_secs_f64());
                let start = std::time::Instant::now();
                let timer = every(Duration::from_millis(16), move || {
                    let t = start.elapsed().as_secs_f32();
                    if let Some(window) = window {
                        window.set_size(832.0 + 200.0 * (t * 3.0).sin(), 640.0 + 120.0 * (t * 2.0).cos());
                    }
                });
                after(length, move || {
                    timer.cancel();
                    tracing::info!("改窗口大小结束");
                });
            });
        }
        // 最小化、再还原：窗口看不见时页面不该白画（内核自己出帧时降到很低的帧率），还原以后升回去
        let window = aureole::current_window();
        if let (Some(window), Some(at)) = (window, minimize_after) {
            after(at, move || {
                tracing::info!("最小化窗口");
                window.set_minimized(true);
            });
        }
        if let (Some(window), Some(at)) = (window, restore_after) {
            after(at, move || {
                tracing::info!("还原窗口");
                window.set_minimized(false);
            });
        }
        if let Some(terminate_after) = terminate_after {
            after(terminate_after, || {
                tracing::info!("时间到了，让系统结束应用");
                terminate();
            });
        }
        let status = {
            let page = page.clone();
            move || {
                if !mounted.get() {
                    return "还没挂上网页视图".to_owned();
                }
                let title = page.title().get();
                let state = if page.loading().get() { "加载中" } else { "" };
                format!("{} {state} {title}", page.url().get())
            }
        };
        column()
            .width(pct(100))
            .height(pct(100))
            .padding(16)
            .gap(12)
            .child(text(status).max_lines(1))
            // 网页视图占满下面剩下的地方，跟着窗口改大小
            .child_when(move || mounted.get(), move || web_view(&page).width(pct(100)).grow(1.0).min_height(0).radius(12))
    })
    .title("网页视图")
    .size(832, 640)
    .run();
}

/// 像应用菜单里的"退出"那样让系统结束应用：从运行循环里发 `terminate:`（菜单也是这样），
/// 不在窗口库正处理着的事件里直接调
#[cfg(target_os = "macos")]
fn terminate() {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    let Some(mtm) = objc2::MainThreadMarker::new() else { return };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    // SAFETY: `performSelector:withObject:afterDelay:` 的参数是选择器、对象（可以是空）、秒数，不返回值
    unsafe {
        let _: () = msg_send![&*app, performSelector: sel!(terminate:), withObject: std::ptr::null::<AnyObject>(), afterDelay: 0.0f64];
    }
}

#[cfg(not(target_os = "macos"))]
fn terminate() {
    aureole::quit();
}
