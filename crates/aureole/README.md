# Aureole

用纯 Rust 写桌面应用的框架。窗口和事件循环、跟着显示器刷新的帧调度、输入法、渲染、响应式状态、布局、文字、动画都由框架负责，应用只写状态和界面。

重点是**性能和动画**：画面不变时不做工作；变换、透明度、滚动在渲染线程上推进，主线程忙的时候也不掉帧；所有视觉属性都能做可打断的过渡。

**现在是测试版（`0.0.1-beta.1`）**，API 还会改，欢迎试用和反馈。

## 安装

需要 Rust 1.90 或更新的版本。

```toml
[dependencies]
aureole = "0.0.1-beta.1"
```

## 例子

```rust
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
```

运行：`cargo run -p aureole --example hello`。从新建项目讲起见[教程](https://github.com/ruoyu-lab/aureole/blob/master/docs/guide/README.md)。

## 平台支持

| 平台 | 状态 |
|---|---|
| macOS | 主要支持，在 Apple Silicon 上开发和验证 |
| Windows | **实验性**：功能都写了，还没在真实桌面上验证过，可能有问题 |
| Linux | **实验性**：同上（X11、Wayland） |

## 文档

- [教程](https://github.com/ruoyu-lab/aureole/blob/master/docs/guide/README.md)：从第一个窗口讲到测试、性能和打包，还有自定义元素和插件
- [API 稳定性约定](https://github.com/ruoyu-lab/aureole/blob/master/docs/stability.md)：现在和正式版之后什么会变、什么不会变
- [更新记录](https://github.com/ruoyu-lab/aureole/blob/master/CHANGELOG.md)

## 打包

装上打包工具，就能把应用打包成 macOS 的 .app 和 .dmg、Windows 的安装程序、Linux 的 .deb 和 AppImage，设置写在应用的 `Cargo.toml` 里，见教程的[打包与发布](https://github.com/ruoyu-lab/aureole/blob/master/docs/guide/23-打包与发布.md)。

```bash
cargo install aureole-bundle --version 0.0.1-beta.1
cargo bundle --release
```

## 许可证

Apache-2.0，全文见 [LICENSE](https://github.com/ruoyu-lab/aureole/blob/master/LICENSE)。
