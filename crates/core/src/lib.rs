//! Aureole 的核心：元素树、属性和样式对象、布局、文字和编辑、事件路由、焦点、浮层、无障碍树、绘制遍历、帧内的阶段、
//! 动画接入、图片的解码和缓存、字体、异步任务的主线程执行器。
//!
//! 核心不认识窗口和 GPU：一帧画成的是 `aureole-scene` 里的数据（显示列表、层表、图集更新），交给宿主；
//! 输入从宿主进来（[`InputEvent`]）。宿主有两个：运行时（`aureole-runtime`，真实的窗口、渲染线程）和测试工具
//! （`aureole-testing`，没有窗口的虚拟时钟），用的都是公开的 [`host`] 模块，自己写的宿主也一样。系统那一侧的服务由宿主
//! 装进注入点：叫醒主线程（[`host::MainThreadWaker`]）、系统设置和剪贴板（[`host::SystemBackend`]）、当前窗口（[`host::WindowId`]）。
//!
//! 基础控件（`aureole-controls`）只用这里的公开 API 搭成。应用不直接依赖这个 crate，用门面 `aureole`：
//! 它从这里和其他几个 crate 重新导出全部公开的 API。

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

pub mod accessibility;
mod cancel;
mod canvas;
#[cfg(feature = "char-grid")]
mod char_grid;
mod clock;
mod compositor;
pub mod defaults;
mod date;
mod depth;
mod drag_drop;
mod dynamic;
mod editor;
mod element;
mod event;
mod field;
mod file_dialog;
mod flow;
pub mod font;
mod geometry;
mod gradient;
mod handlers;
pub mod host;
mod image;
mod input;
mod interaction;
pub mod kinds;
mod layout;
mod measure;
mod morph;
mod overlay;
pub mod performance;
pub mod property;
mod recording;
mod reorder;
mod resource;
mod responsive;
mod rich_text;
mod scroll;
mod segmentation;
mod shadow;
mod style;
mod system;
mod task;
pub mod testing;
mod text;
mod timeline;
mod timer;
mod tree;
mod vector;
mod virtual_list;
mod visual_effects;

pub use accessibility::{Live, RangeValue, Role, Toggled};
pub use date::{Date, ParseDateError, Time, Weekday};
pub use file_dialog::{FileDialog, FileDialogDone, FileDialogKind, FileDialogRequest, FileFilter};

pub use property::{Property, PropertyValue};
pub use animation::{Curve, Easing, Spring, Tween};
pub use style::{State, States, Style};
pub use editor::SubmitKey;
pub use visual_effects::{BlendMode, Filter, FilterChain, Mask};
pub use element::{Element, ElementMethods, Shader, View, column, path, rect, row, text, text_area, text_field};
pub use kinds::ElementKind;
pub use text::{InlineAlign, TextAlign};
pub use rich_text::{IntoInline, Span, SpanInfo, rich_text, span};
pub use flow::flow;
#[cfg(feature = "char-grid")]
pub use char_grid::{CaretShape, CharCell, CharCellContent, CharCellStyle, CharGrid, CharGridCaret, CharGridPosition, CharGridStats, char_grid};
pub use layout::{Length, Number, Place, Track, Tracks, auto, auto_fill, auto_fit, fr, grid, max_content, min_content, pct, px};
pub use image::{ImageCacheStats, ImageFit, PlaybackHandle, ImageSource, ImageState, RgbaImage, image, image_cache_stats, set_image_cache_budget};
pub use gradient::{Fill, Gradient, StopColor};
pub use scene::ColorSpace;
pub use vector::{LineCap, LineJoin, VectorPath, VectorPathBuilder, Stroke};
pub use resource::{Resource, debounced, resource};
pub use responsive::{SizeClass, TitleBarInfo, is_window_at_least, responsive, set_size_class_widths, title_bar_info, window_size_class, window_size};
pub use system::{
    Appearance, AppearanceMode, Conventions, DisplayInfo, appearance, appearance_mode, clipboard, conventions, displays, local_date_time, open_url, set_appearance_mode,
    today, is_motion_reduced, system_accent_color, system_appearance,
};
pub use task::{
    BlockingJob, LocalTask, MainThreadSender, PoolSpawner, SendFuture, Sleep, Spawner, Task, main_thread_sender,
    set_spawner, sleep, spawn, spawn_blocking, spawn_local, spawner,
};
#[cfg(feature = "tokio")]
pub use task::{TokioSpawner, set_tokio_runtime, spawn_tokio, tokio_handle};
#[cfg(feature = "tokio")]
pub use resource::resource_tokio;
pub use timer::{Timer, after, every};
pub use cancel::{Cancel, CancelGuard};
pub use timeline::{Finished, Keyframes, Motion, MotionHandle, Timeline, parallel, sequence};
pub use interaction::{Cursor, FocusHandle, FocusRing, IntoStackOrder, Shortcut, shortcut};
pub use event::{
    ClickSource, Builtin, ChangeEvent, ChangeSource, ClickEvent, DragEvent, ExternalDataKinds, ExternalDragEvent, ExternalDropData, ExternalDropEffect, ExternalDropEvent,
    FieldChangeEvent, FieldValue, FileDropEvent, IntoChangeHandler, IntoHandler,
    KeyEvent, PropagationPhase, PinchEvent, PointerEvent, PointerEventKind, RotateEvent, ScrollEvent, WheelEvent,
};
pub use field::FieldHandle;
pub use overlay::{Dismiss, Overlay, Placement, Side};
pub use drag_drop::{DragOut, DragPreview, DropEvent};
pub use reorder::{Reorderable, reorderable};
pub use interaction::{KeyUpListener, is_key_down, key_up_listener};
// 按键、修饰键，窗口交给元素树的输入事件（测试工具、录制回放也用它们）
pub use input::{
    Buttons, GestureInput, GesturePhase, Ime, ImePurpose, ImeRequest, InputEvent, InputKind, Key, KeyInput, Modifiers, MouseButton, NamedKey,
    PhysicalKey, PointerId, PointerInput, PointerKind, WheelDelta, WheelInput,
};
pub use property::{IntoProp, Prop};
pub use reactive::{Effect, Memo, Scope, Signal, StoredValue, batch, detached, effect, flush_effects, live_node_count, memo, on_cleanup, signal, stored_value, untracked};
pub use scene::{Angle, Color, WideColor, deg, ms, rad, secs, turns};
pub use scroll::{ScrollHandle, scroll, scroll_x, scroll_y, scroll_y_fit};
pub use virtual_list::{IntoListSource, ListSource, Measured, IntoRowHeights, RowHeights, measured, virtual_list};
pub use geometry::Rect;
pub use layout::{Align, Direction, Justify};
pub use shadow::{Shadow, Shadows};
pub use canvas::{Canvas, CanvasHits, CanvasTextMetrics, CanvasTextStyle, Drawing, canvas};
pub use clock::{AnimationClock, animation_clock};
pub use tree::{ElementProps, FrameStats, without_animation};

/// 框架自己的 crate 之间用的内部接口。**不稳定，不属于公开 API**，随时会改，不要依赖它。
/// 扩展要用的都在公开的模块里：自定义元素是 [`kinds`]，自定义窗口宿主是 [`host`]，自己做的控件用 [`Element`] 上的公开方法。
///
/// 这里只留真的只有框架自己用得上的，每一项写明谁用、为什么不公开
#[doc(hidden)]
pub mod internal {
    /// 测试工具读元素树的窄接口：`Harness` 的查询、回放的快照都经过它。读的是元素树的内部状态，写测试用门面的 `Harness`
    pub use crate::tree::TreeProbe;

    /// 测试工具把这个线程的时钟换成虚拟时间、从 0 重新开始。真实的宿主读真实的时钟，公开了只会被误用
    pub use crate::timer::{restart_virtual_time, set_virtual_time};

    /// 测试工具等这个线程发出的后台任务、后台解码全部结束（`aureole::testing::settle`、截图之前）。
    /// 真实的宿主靠事件循环一轮轮收结果，不该阻塞着等
    pub use crate::image::wait_idle as wait_image_idle;
    pub use crate::task::settle;

    /// 测试工具的 `Harness::animation_time`：一个宿主时刻（统一时钟的秒数）对应的动画时间。写应用读 [`crate::AnimationClock`]
    pub fn animation_time_at(host: f64) -> f64 {
        crate::clock::time_at(host)
    }

    /// 测试工具的回放读录制文件。写文件的 [`crate::host::Recorder`] 是公开的；读回来的事件只有回放用
    pub use crate::recording::Recording;

    /// 测试工具按组合键的写法（`"CmdOrCtrl+K"`）合成按键，和快捷键用同一份解析。写应用用 [`crate::shortcut`]
    pub use crate::interaction::Chord;

    /// 开发工具（`aureole-devtools`）在这个线程上装上图片文件的修订号从哪里来：文件变了图片重新解码。
    /// 写应用用门面的 `aureole::devtools::set_image_watching`
    pub use crate::image::set_image_revision_source;
}

/// 常用类型一次导入（门面 `aureole` 的前导模块在这上面加上应用、窗口这些运行时的类型）
pub mod prelude {
    pub use crate::{ElementMethods, ElementProps};
    pub use crate::{
        Date, Time, Weekday, today,
        Align, Color, ColorSpace, Direction, Easing, Effect, Element, Fill, Gradient, ImageFit, PlaybackHandle, ImageSource, ImageState, RgbaImage,
        IntoProp, Justify, LineCap, LineJoin, Memo, VectorPath, Property, Rect, Shadow, Shader, Signal, Spring, StoredValue, Stroke, WideColor,
        Tween, View, batch, column, flow, image, memo, path, rect, rich_text, row, signal, span, stored_value, text, text_area, text_field,
        Canvas, CanvasHits, CanvasTextMetrics, CanvasTextStyle, canvas, Shadows,
        untracked, InlineAlign, Span, SubmitKey, TextAlign, ScrollHandle, measured, scroll, scroll_x, scroll_y, scroll_y_fit, virtual_list,
    };
    // 滤镜、混合模式、遮罩
    pub use crate::{BlendMode, Filter, FilterChain, Mask};
    // 关键帧时间线
    pub use crate::{Keyframes, Motion, MotionHandle, Timeline, parallel, sequence};
    // 样式对象
    pub use crate::{State, States, Style};
    // 输入：指针形状、手势、焦点、按键、浮层
    pub use crate::{
        Builtin, ChangeEvent, ChangeSource, ClickEvent, Cursor, Dismiss, DragEvent, ExternalDataKinds, ExternalDragEvent, ExternalDropEvent, FieldChangeEvent, FileDropEvent, FocusHandle, FocusRing, GesturePhase, Key, KeyEvent, Modifiers,
        MouseButton, NamedKey, Overlay, PinchEvent, Placement, PointerEvent, PointerEventKind, PointerKind, RotateEvent, ScrollEvent, Side, WheelDelta, WheelEvent,
    };
    // 拖放
    pub use crate::{DragOut, DragPreview, DropEvent, Reorderable, reorderable};
    // 布局：长度、网格
    pub use crate::{Length, Place, Track, Tracks, auto, auto_fill, auto_fit, fr, grid, pct, px};
    // 角度
    pub use crate::{Angle, deg, rad, turns};
    // 时长
    pub use crate::{ms, secs};
    // 按窗口大小切换：尺寸档、断点
    pub use crate::{SizeClass, is_window_at_least, responsive, window_size_class};
    pub use crate::{Appearance, AppearanceMode, Resource, appearance, is_motion_reduced, resource, spawn, spawn_blocking, spawn_local, system_appearance};
    // 定时器，登记收成守卫（`every(..).guard()`）
    pub use crate::{Cancel, after, every};
    // 无障碍：角色、实时区域
    pub use crate::{Live, Role};
    // 字符网格
    #[cfg(feature = "char-grid")]
    pub use crate::{CaretShape, CharCell, CharCellContent, CharCellStyle, CharGrid, CharGridCaret, CharGridPosition, char_grid};
}
