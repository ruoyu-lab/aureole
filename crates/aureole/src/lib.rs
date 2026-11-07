//! 自研的 Rust 应用框架：纯 Rust 链式 API + 细粒度信号。
//!
//! 现在是测试版，API 还会改。主要支持 macOS；Windows、Linux 是实验性的，还没在真实桌面上验证过。
//!
//! ```no_run
//! use aureole::prelude::*;
//!
//! fn app() -> impl View {
//!     let on = signal(false);
//!     column().padding(24).gap(16).child(
//!         rect()
//!             .size(56, 32)
//!             .radius(16)
//!             .background(move || if on.get() { Color::hex(0x2563eb) } else { Color::hex(0xd5dae0) })
//!             .on_click(move || on.update(|v| *v = !*v)),
//!     )
//! }
//!
//! App::new(app).title("示例").run();
//! ```
//!
//! # 概览
//!
//! - **元素和链式 API**：[`Element`] 是唯一的元素类型，[`rect`]、[`row`]、[`column()`]、[`text()`]、[`path`]、
//!   [`image()`] 创建它，再链式设置样式和事件。自己的部件就是返回 `impl View` 的普通函数
//! - **基础控件**（[`controls`]）：日期、时间、颜色、文件选择，按钮、输入框（单行、多行、密码）、复选框、单选组、开关、下拉选择、滑块、进度条、
//!   数字输入、标签。行为完整（键盘、焦点、无障碍、拖动、弹出列表），默认的样子朴素、只有浅色，没有内置动画
//! - **样式对象**：[`Style`] 是一组可以共用、合起来、带状态（悬停、按下、焦点、禁用，控件部位的勾选、选中、打开）的属性，
//!   [`Element::style`] 用到元素上，控件的每个部位也收样式对象
//! - **信号**：属性可以是静态值，也可以是读取信号的闭包（[`IntoProp`]），信号变了只更新这一个属性。
//!   随数据增删的子元素用 [`Element::each`]、[`Element::child_when`]
//! - **动画**：[`Element::transition`] 给属性声明弹簧或补间（[`Property`]、[`Spring`]、[`Tween`]），
//!   [`Element::exit`] 声明退场动画；变换和透明度动画在渲染线程推进，主线程卡住也流畅。
//!   多步、多个元素编排在一起的动画用关键帧时间线（[`Timeline`]、[`Keyframes`]、[`MotionHandle`]）；
//!   路径之间的变形是 [`Property::Path`]
//! - **布局**：横排、竖排和网格（[`grid`]），长度见 [`Length`]，还有换行、外边距、绝对定位；
//!   按窗口大小切换样式见 [`window_size_class`]、[`is_window_at_least`]、[`responsive`]
//! - **文字**：[`text()`] 自动换行，[`rich_text`] 一段里混排样式、行内代码、链接和元素，[`Element::selectable`] 可选中、
//!   [`Element::selection_root`] 跨段落选择，[`flow`] 绕着图片排；[`text_field`]、[`text_area`] 是输入框；[`font`] 加载自定义字体
//!   （WOFF2、WOFF 要打开 `woff2` 特性）；`char_grid` 是终端、代码编辑器用的等宽字符网格（`char-grid` 特性，默认开）
//! - **自定义元素**：实现 [`ElementKind`]（量大小、画内容），[`Element::from_kind`] 做成元素；内容用 [`Drawing`] 画。
//!   内置的画布就是这样做的，见 [`kinds`]
//! - **图片和矢量**：[`image()`] 在后台解码并缓存，GIF 和动态 WebP 会动（[`PlaybackHandle`] 控制播放），`svg` 特性打开时也认得 SVG 文件；[`VectorPath`] 画图标和图表；[`Gradient`] 渐变，[`Shader`] 自定义着色器
//! - **效果**：[`Element::filter`] 滤镜（[`Filter`]）、[`Element::blend`] 混合模式（[`BlendMode`]）、[`Element::mask`] 遮罩（[`Mask`]）；
//!   内容没变的效果分组缓存成纹理（[`Element::group_cache`]、[`group_cache_stats`]）
//! - **滚动**：[`scroll_y`]、[`scroll`] 滚动容器，[`scroll_y_fit`] 跟着内容长高的滚动容器，[`ScrollHandle`] 程序化滚动，
//!   [`virtual_list`] 只构建可见的行（行高可以由内容决定：[`measured()`]）
//! - **浮层和输入**：[`Element::overlay`] 下拉菜单、弹出面板、提示；指针手势、键盘焦点（[`FocusHandle`]）、
//!   快捷键（[`shortcut`]）、指针形状（[`Cursor`]）；元素之间的拖放（[`Element::draggable`]、[`Element::on_drop`]）、
//!   拖到应用外面（[`Element::drag_out`]）、按键松开（[`Element::on_key_up`]）
//! - **窗口和任务**：[`App`]、[`Window`] 多窗口，[`App::gpu`] 选 GPU（[`GpuPreference`]），`Menu`、`Tray` 菜单栏和托盘（`menus` 特性，默认开），
//!   [`FileDialog`]；打开网址和文件（[`App::on_open`]、[`OpenRequest`]）、单实例（[`App::single_instance`]）；
//!   [`spawn`]、[`spawn_blocking`]、[`spawn_local`] 异步任务，[`after`]、[`every`] 定时器，[`resource`] 数据加载；
//!   `tokio` 特性的 `spawn_tokio`、`resource_tokio` 在 tokio 运行时上跑（reqwest、sqlx 这些库要它）
//! - **测试**（`testing` 特性）：`testing::Harness` 不开窗口、不用 GPU 地挂载视图，手动推进帧、模拟指针和键盘、读文字；
//!   `testing::Harness::screenshot` 离屏截图
//! - **开发工具**（`devtools` 特性）：`devtools::watch_file` 文件保存后界面跟着变，`App::restarts_on_rebuild` 重新编译后原地重启
//! - **自己的宿主**：把界面接到自己的窗口、事件循环、渲染器上，见 [`host`]；真实窗口和测试工具也是这样接的
//!
//! # 特性
//!
//! | 特性 | 默认 | 管什么 |
//! |---|---|---|
//! | `cjk-words` | 开 | 双击选词、Option+方向键在中文、日文里按词走（二进制大约多 2 MB 的词典）；关掉时按字 |
//! | `char-grid` | 开 | 字符网格：`char_grid`、`CharGrid` 和它们的类型 |
//! | `menus` | 开 | 菜单栏和托盘：`Menu`、`Tray`、`App::menu`、`App::tray`（多三个库，二进制大约多 0.5 MB） |
//! | `woff2` | 关 | `font::load` 认得 WOFF2、WOFF |
//! | `svg` | 关 | `image()` 认得 SVG 文件（后台解析，画成矢量路径） |
//! | `tokio` | 关 | `spawn_tokio`、`resource_tokio`、`TokioSpawner`、`TokioPlugin` |
//! | `testing` | 关 | 测试工具 `aureole::testing`。只在开发依赖里打开 |
//! | `devtools` | 关 | 开发工具 `aureole::devtools`、`App::restarts_on_rebuild`。发布的应用不开 |
//!
//! 特性关着时对应的 API 不存在。测试里用测试工具：
//!
//! ```toml
//! [dev-dependencies]
//! aureole = { version = "0.0.1-beta.1", features = ["testing"] }
//! ```
//!
//! 这个 crate 是门面：只重新导出，加上特性开关。元素树、布局、文字、输入、动画在 `aureole-core`，应用、窗口、
//! 事件循环、渲染线程、系统集成在 `aureole-runtime`；响应式来自 `aureole-reactive`，动画曲线来自 `aureole-animation`，
//! 绘制由 `aureole-render` 完成。常用类型都从这里重新导出，一般 `use aureole::prelude::*` 就够了。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

#[doc(inline)]
pub use aureole_core::{accessibility, defaults, font, host, kinds, property};
#[doc(inline)]
pub use controls;

pub use aureole_core::{
    // 无障碍、属性、动画曲线
    Live, RangeValue, Role, Toggled, Property, PropertyValue, Curve, Easing, Spring, Tween,
    // 日期和时间、文件对话框的类型
    Date, ParseDateError, Time, Weekday, local_date_time, today, FileDialogKind, FileDialogRequest, FileFilter,
    // 样式对象、效果
    State, States, Style, SubmitKey, BlendMode, Filter, FilterChain, Mask,
    // 元素
    Element, Shader, View, column, path, rect, row, text, text_area, text_field, TextAlign, InlineAlign, IntoInline, Span, SpanInfo, rich_text, span, flow,
    // 布局
    Length, Number, Place, Track, Tracks, auto, auto_fill, auto_fit, fr, grid, max_content, min_content, pct, px,
    // 角度
    Angle, deg, rad, turns,
    // 时长
    ms, secs,
    // 图片、填充、矢量
    ImageCacheStats, ImageFit, PlaybackHandle, ImageSource, ImageState, RgbaImage, image, image_cache_stats, set_image_cache_budget,
    Fill, Gradient, StopColor, ColorSpace, LineCap, LineJoin, VectorPath, VectorPathBuilder, Stroke,
    // 数据加载、按窗口大小切换、系统外观
    Resource, debounced, resource, SizeClass, TitleBarInfo, is_window_at_least, responsive, set_size_class_widths, title_bar_info, window_size_class, window_size, Appearance, Conventions, DisplayInfo,
    // 任务和定时器、时间线
    LocalTask, Sleep, Task, sleep, spawn, spawn_blocking, spawn_local, Timer, after, every,
    // 后台执行器：内置的线程池，或者应用换上的
    BlockingJob, PoolSpawner, SendFuture, Spawner, set_spawner, spawner,
    // 从任何线程往主线程投递工作
    MainThreadSender, main_thread_sender,
    // 能取消的登记和它的守卫
    Cancel, CancelGuard,
    Finished, Keyframes, Motion, MotionHandle, Timeline, parallel, sequence,
    // 输入：事件对象和传播、指针、手势、焦点、快捷键、浮层、拖放
    ClickSource, Builtin, ChangeEvent, ChangeSource, ClickEvent, ExternalDataKinds, ExternalDragEvent, ExternalDropData, ExternalDropEffect, ExternalDropEvent,
    FieldChangeEvent, FieldHandle, FieldValue, FileDropEvent, IntoChangeHandler,
    IntoHandler, PropagationPhase, PointerEventKind, ScrollEvent, WheelEvent,
    Cursor, DragEvent, FocusHandle, FocusRing, GesturePhase, IntoStackOrder, KeyEvent, MouseButton, PinchEvent, PointerEvent,
    RotateEvent, Shortcut, shortcut, Dismiss, Overlay, Placement, Side, DragOut, DragPreview, DropEvent, Reorderable, reorderable,
    KeyUpListener, is_key_down, key_up_listener,
    // 按键、修饰键，窗口交给元素树的输入事件（测试工具、录制回放也用它们）
    Buttons, GestureInput, Ime, ImePurpose, ImeRequest, InputEvent, InputKind, Key, KeyInput, Modifiers, NamedKey, PhysicalKey, PointerId, PointerInput, PointerKind,
    WheelDelta, WheelInput,
    // 属性的值、响应式
    IntoProp, Prop, Effect, Memo, Scope, Signal, StoredValue, batch, detached, effect, flush_effects, live_node_count, memo, on_cleanup, signal, stored_value, untracked,
    // 颜色、滚动、虚拟列表、样式的值、画布
    Color, WideColor, ScrollHandle, scroll, scroll_x, scroll_y, scroll_y_fit,
    IntoListSource, ListSource, Measured, IntoRowHeights, RowHeights, measured, virtual_list,
    Align, Direction, Justify, Rect, Shadow, Shadows, Canvas, CanvasHits, CanvasTextMetrics, CanvasTextStyle, Drawing, canvas, FrameStats, without_animation,
    // 元素种类：自己定义带内容的元素（见 `kinds` 模块）
    ElementKind,
    // 动画的时钟（暂停、慢放、拖动时间）
    AnimationClock, animation_clock,
};
// 字符网格
#[cfg(feature = "char-grid")]
pub use aureole_core::{CaretShape, CharCell, CharCellContent, CharCellStyle, CharGrid, CharGridCaret, CharGridPosition, CharGridStats, char_grid};
#[cfg(feature = "tokio")]
pub use aureole_core::{TokioSpawner, resource_tokio, set_tokio_runtime, spawn_tokio, tokio_handle};
#[cfg(feature = "tokio")]
pub use runtime::TokioPlugin;

// 开发工具的热加载
#[cfg(feature = "devtools")]
pub use devtools::{watch_file, watch_file_with, set_image_watching};

// 基础控件
pub use controls::{
    Checkbox, Label, NumberField, ProgressBar, RadioGroup, Select, Slider, Switch, button, checkbox, label, number_field, progress_bar,
    radio_group, select, slider, switch,
};

pub use runtime::{
    App, Capabilities, CloseRequest, ColorOutput, DisplayColor, FileDialog, FrameClockKind, GpuError, GpuPreference, GroupCacheStats, Notification, OpenRequest, OpenSource,
    PlatformError, SingleInstance, TitleBar, Window, WindowHandle, WindowMaterial, beep, capabilities, clipboard, current_window, display_color, displays, group_cache_stats, notify,
    open_path, open_url, quit, is_motion_reduced, reveal_path, set_group_cache_budget, system_accent_color, system_appearance,
};
// 菜单栏和托盘
#[cfg(feature = "menus")]
pub use runtime::{Menu, MenuItem, Submenu, Tray};
// 插件：钩子、平台事件、每帧的信息
pub use runtime::{FrameInfo, PlatformEvent, PlatformEventKind, Plugin};

#[cfg(feature = "devtools")]
pub mod devtools {
    //! 开发工具（`devtools` 特性）：热加载和重新编译后接着跑。
    //!
    //! **热加载**：[`watch_file`] 把文件内容变成信号，文件保存后界面跟着变（配色、文案、调参数）；
    //! [`set_image_watching`] 让图片文件改了以后自动重新解码。
    //!
    //! **重新编译后接着跑**：[`crate::App::restarts_on_rebuild`] 监视自己的可执行文件，`cargo build` 换掉它之后
    //! 保存主窗口的位置大小和 [`persist`] 登记的信号，换成新程序，新程序读回这些状态。
    //!
    //! 两样一起装：插件 [`HotReloadPlugin`]（`App::new(app).plugin(HotReloadPlugin::new())`）。
    //!
    //! **动画的时钟**：[`crate::animation_clock`] 暂停、慢放、拖动所有窗口的动画（两条线程一起），看清楚一段动画的每一刻。

    pub use devtools::{watch_file, watch_file_with, set_image_watching};
    pub use runtime::restart::{HotReloadPlugin, is_restored, persist, restart};
}

pub mod performance {
    //! 性能和内存的测量：本进程的资源用量、渲染器分配的显存、选中的 GPU、从启动到第一帧上屏的时间、每帧的耗时。
    //!
    //! ```no_run
    //! use aureole::performance;
    //! let before = performance::usage();
    //! performance::set_frame_recording(true);
    //! // …… 跑一段 ……
    //! let frames = performance::take_frame_times();
    //! let after = performance::usage();
    //! println!("主线程每帧 p99 {:?}，常驻内存 {:?} 字节", performance::percentile(&frames.main, 0.99), after.resident);
    //! # let _ = before;
    //! ```
    //!
    //! 资源用量只用不需要权限的系统接口（见 `platform::process`），这个平台拿不到的项是 `None`。

    pub use aureole_core::performance::{FrameTimes, percentile, set_frame_recording, take_frame_times};
    pub use runtime::performance::{
        GpuInfo, GpuKind, GpuMemory, Usage, event_loop_wakeup_count, first_present, gpu_info, gpu_memory, scheduled_wakeup_count, since_start, usage,
    };
}

#[cfg(feature = "testing")]
pub mod testing {
    //! 测试工具（`testing` 特性，只在开发依赖里打开）：不开窗口、不用 GPU 地挂载视图，手动推进帧、模拟输入、读状态、离屏截图；
    //! 录下的输入回放。

    pub use runtime::testing::*;
    pub use testing::*;
}

pub mod prelude {
    //! 常用类型一次导入

    pub use aureole_core::prelude::*;
    // 基础控件
    pub use controls::prelude::*;
    // 应用和窗口
    pub use runtime::{App, Window, WindowHandle, current_window};
    // 系统设置：先装上平台的实现再读（同门面上的同名函数）
    pub use runtime::{is_motion_reduced, system_appearance};
    // 选 GPU（`App::gpu`）
    pub use runtime::GpuPreference;
}
