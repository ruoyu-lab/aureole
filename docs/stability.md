# API 稳定性约定

用 Aureole 写应用的人能指望什么：升级时什么不会变，变了会怎么告诉你。

**现在是测试版。** 版本是 `0.0.1-beta.3`：第一个正式版之前，任何一个版本都可能有破坏性改动，改了写在[更新记录](../CHANGELOG.md)里。下面的约定从第一个正式版开始生效。

## 版本号

按语义化版本，和 Cargo 的规则一样：

| 版本 | 可以有破坏性改动 | 只能加东西、修问题 |
|---|---|---|
| `0.0.z-beta.n`（测试版） | 每个版本都可以 | 不保证 |
| `0.y.z`（1.0 之前） | `y` 加一 | `z` 加一 |
| `x.y.z`（1.0 之后） | `x` 加一 | `y`、`z` 加一 |

所以写 `aureole = "0.3"` 时，`cargo update` 只会升到 `0.3.x`，不会破坏你的代码。各个 crate 的版本一起走。

测试版要写全版本号（`aureole = "0.0.1-beta.3"`），`cargo add` 也要写上（`cargo add aureole@0.0.1-beta.3`）：Cargo 默认不选测试版。

## 约定管什么

| 内容 | 范围 |
|---|---|
| `aureole` 这个 crate 公开的名字 | 顶层的、`prelude`、`kinds`（自定义元素）、`host`（自己的窗口宿主）、`controls`、`property`、`accessibility`、`font`、`defaults`、`performance` |
| 打开特性才有的名字 | `testing`、`devtools`、`tokio`、`svg`、`woff2`、`menus`、`char-grid`、`cjk-words` |
| 特性的名字和默认开哪些 | 默认开的特性变少算破坏性改动 |
| 打包 | `[package.metadata.bundle]` 的键、`cargo bundle` 的参数 |
| 录制文件 | 新版本读得了旧版本录的文件；格式变了版本号加一（回放） |
| 给用户用的环境变量 | `AUREOLE_GPU`、`AUREOLE_FRAME_CLOCK` |

## 约定不管什么

- **试验性的 crate**：`aureole-webview`（CEF 视图）每个版本都可能改，正式版之前也不保证跟着下面的规矩走。
- **直接依赖子 crate**：`aureole-core`、`aureole-runtime` 这些是实现的一部分，只通过 `aureole` 用才受约定保护。`aureole_core::internal` 和标了 `#[doc(hidden)]` 的随时会改。
- **文字**：日志、错误信息、`Debug` 的输出，不要拿来做判断。
- **像素**：基础控件默认的样子、抗锯齿的结果可能在小版本里调整，写进更新记录。要固定的外观用样式对象自己写。
- **性能数字**：会尽量不退步，但不是接口的一部分。
- **测试用的环境变量**：`AUREOLE_BLESS`、`AUREOLE_RECORD`、`AUREOLE_RESTORE`、`AUREOLE_SIMULATE_*`。
- **平台支持程度**：约定管接口，不代表每个平台都验证过，以 [平台状态](../README.md#平台支持) 为准。

## 什么算破坏性改动

| 算 | 不算 |
|---|---|
| 删除、改名、改签名 | 加新的名字（包括 `prelude` 里；万一和你的名字撞了，显式导入优先） |
| 没标 `#[non_exhaustive]` 的枚举加种类 | 标了 `#[non_exhaustive]` 的枚举加种类 |
| 公开字段的结构体加字段 | 不公开字段的结构体加字段 |
| trait 加没有默认实现的方法 | trait 加有默认实现的方法 |
| 默认开的特性变少、特性改名 | 加新的可选特性 |
| 属性的默认值变了，导致已有的布局变了 | 修问题：文档写的是 A、实际是 B，改成 A |

修问题时如果很多应用依赖了错的行为，按破坏性改动处理。

## 枚举怎么定

枚举默认不标 `#[non_exhaustive]`，符合下面一条才标：

- **列的是外部世界的东西，或者以后可能加种类**：标。比如 `Cursor`、`PointerKind`、`ClipFormat`、`WindowMaterial`、`PlatformError`、`GpuError`、`BlendMode`、`Easing`。`match` 时要写 `_` 分支。
- **种类本来就是封闭的，用户通常会完整地 `match`**：不标。比如 `ImageState`（加载中、好了、出错）、`OpenSource`、`Direction`、`Side`、`Handled`。这时标上只会逼每个用户多写一条永远走不到的分支。

加新的公开枚举时要想这一条：发布之后再标，本身就是破坏性改动。

## 废弃

1. 先标 `#[deprecated(since = "版本", note = "用 …")]`，新写法同时能用，更新记录的"改变"里写升级方式。
2. 至少留一个版本（`0.y` 时到下一个 `0.y`，1.0 之后到下一个主版本），才能删。

例外：内存安全、安全问题可以直接改，更新记录里说明。

## 最低 Rust 版本

写在 `Cargo.toml` 的 `rust-version`（现在是 1.90），CI 用这个版本编译检查。提高它要加 `y`（`0.y` 和 1.0 之后都是），更新记录里写明，不算破坏性改动。

## 第一次发布之前还要做的

- 决定第一个版本号和发布时机。
- CI 接上 `cargo-semver-checks`：它要和 crates.io 上的版本比，第一次发布之后才接得上。
- 发布到 crates.io 要账号。
