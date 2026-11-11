# aureole-webview

Aureole 的网页视图（**试验性**）：内嵌 CEF 内核，网页离屏画进一张 GPU 纹理，框架把它当成普通元素合成。
圆角裁剪、透明度、变换、动画对它都有效，框架的菜单、弹窗能盖在网页上面。

```rust,no_run
use aureole::prelude::*;
use aureole_webview::{WebController, web_view};

fn main() {
    App::new(|| {
        let page = WebController::new("https://example.com");
        column().child(web_view(&page).width(pct(100)).grow(1))
    })
    .title("网页")
    .size(1024, 768)
    .run();
}
```

不用的时候不加载：第一个网页视图挂载时才加载内核、启动 CEF（几百毫秒），没用到网页视图的运行不占内存、不多开进程。

## 平台

同一份应用代码在 macOS 和 Windows 上用法一样，API 没有只在某个平台才有的部分；
唯一的例外是 Windows 上开沙箱用的入口（`bootstrap_entry!`、`run_bootstrapped`），见下面“沙箱”。

| 平台 | 状态 |
|---|---|
| macOS | 能用，能打包（`.app`） |
| Windows | 能用，能打包 |
| Linux | 能编译，还没在真机上跑过，不能打包 |

## 编译要准备的

- 第一次编译时自动下载 CEF 内核（约 500 MB）。设了环境变量 `CEF_PATH` 时下载到那里，换个目录、`cargo clean` 以后不用重新下；
  建议设成一个固定的目录。
- CEF 的 C++ 包装层要 cmake 和 Ninja；Windows 上还要 Visual Studio 的“使用 C++ 的桌面开发”。
- 这个 crate 不在框架的工作区里（编译它要下载内核），自己的应用直接依赖它就行：

```toml
[dependencies]
aureole = "0.0.1-beta.3"
aureole-webview = "0.0.1-beta.3"
```

## 开发时运行

CEF 的子进程跑的是另一个程序 `aureole-webview-helper`，要放在应用旁边；macOS 上内核还必须在应用包里。

- **Windows**：先编一次子进程程序，CEF 的运行文件编译时已经拷到目标目录里，之后 `cargo run` 照常用：

  ```bash
  cargo build -p aureole-webview --bin aureole-webview-helper
  cargo run
  ```

- **macOS**：用 `cargo bundle`（`cargo install aureole-bundle`）打成 `.app` 再打开，内核框架和 5 个辅助程序会装进 `Contents/Frameworks`：

  ```bash
  cargo bundle --release
  open target/release/bundle/<应用名>.app
  ```

仓库里的例子可以用脚本运行：macOS 上 `crates/webview/run-example.sh browser`，Windows 上 `.\crates\webview\run-example.ps1 browser`。

## 打包

应用直接依赖这个 crate 时，`cargo bundle` 自动编子进程程序、带上 CEF：Windows 上运行文件装在应用旁边，macOS 上装进 `.app`。
在应用的 `Cargo.toml` 里可以只带几种语言包（CEF 自己的界面文字），和开不开沙箱：

```toml
[package.metadata.bundle.webview]
locales = ["zh-CN"]   # 只带这几种语言包，英文的总带上
sandbox = true
```

## 沙箱

- macOS 上默认开着，应用不用改结构（`AUREOLE_WEBVIEW_NO_SANDBOX=1` 关掉）。
- Windows 上默认不开。要开时应用编成动态库（`[lib] crate-type = ["cdylib", "rlib"]`），主逻辑放进库里的一个函数，
  库里写 `aureole_webview::bootstrap_entry!(那个函数);`，`main.rs` 调同一个函数（开发时照常跑，不开沙箱），
  再在上面的表里写 `sandbox = true`，`cargo bundle` 会用 CEF 带的 `bootstrap.exe` 拼出程序。

只显示应用自带的页面时不开问题不大；要打开任意网站就该开。

## 能做什么

- 页面要应用出面的事（脚本的对话框、选文件、下载、权限申请、要去哪个地址、开新窗口、右键菜单、认证、证书错误）
  都是 `WebController` 上的钩子，不接时有默认的行为。
- 页面的状态（标题、地址、图标、加载进度、出错、崩溃、历史记录……）是信号，能直接绑到界面上。
- 页面用哪一套数据（Cookie、缓存、语言、代理、请求拦截、用户脚本）由 `WebProfile` 定。
- 页面和应用用 `window.aureole` 互发消息；应用自带的页面走 `app://`（`serve_app`）。
- 网页全屏、缩放、页内查找、开发者工具、打印；读屏软件能读到页面的内容。

查找框、下载列表、全屏时的样子这些都由应用自己画，这里只给原语。

## 出帧

macOS 上内核按窗口所在屏幕的刷新率自己出帧（120 Hz 的屏幕上是 120 帧），低电量模式下最多 60 帧，
窗口被挡住、最小化时降到每秒 10 帧；Windows 上跟着框架的帧时钟出帧。环境变量 `AUREOLE_WEBVIEW_FRAMES=kernel` 或 `signal`
可以换一种（对比、排查用）。

## 许可

Apache-2.0。CEF 是 BSD 许可，Chromium 自带的许可清单由 `cargo bundle` 装进安装包（`LICENSES.chromium.htm`）。
