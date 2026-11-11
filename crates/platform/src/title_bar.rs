//! 标题栏和窗口材质：应用自己画标题栏（内容铺满整个窗口）、窗口背后透出系统的材质。
//!
//! 这里是不碰系统的那一半：选项和度量的类型、各平台材质的对应关系、双击标题栏的系统设置怎么解释、
//! 窗口边缘和最大化按钮的命中规则、Windows 上隐藏了标题栏的窗口的内容区怎么算，在任何平台上都能单元测试。碰系统的在 `title_bar/macos.rs`、`title_bar/windows.rs`，
//! 由 [`crate::NativeBackend`] 的 [`crate::WindowService`] 调用。Linux 上的窗口去装饰、拖动、改大小都是窗口库的，
//! 平台层只给度量。

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

use std::sync::Arc;

/// 窗口的标题栏
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TitleBar {
    /// 系统的标题栏（默认）
    #[default]
    System,
    /// 内容铺满整个窗口，标题栏透明、不显示标题文字，标题栏由应用自己画。窗口的边框、阴影、调整大小照旧。
    /// macOS 上系统的窗口按钮（红绿灯）还在，浮在内容上面；Windows、Linux 上按钮也要应用自己画
    Hidden,
}

/// 窗口背后透出来的系统材质。设了以后窗口是透明的，没有被元素盖住的地方透出后面的东西；
/// 不支持的平台（见 [`crate::Capabilities::supports_window_backdrop`]）照常画不透明的背景。
/// 窗口失去焦点时的样子跟系统
///
/// | | macOS（`NSVisualEffectView`） | Windows 11（`DWMWA_SYSTEMBACKDROP_TYPE`） |
/// |---|---|---|
/// | [`Self::Window`] | `underWindowBackground` | Mica |
/// | [`Self::Sidebar`] | `sidebar` | Mica |
/// | [`Self::Header`] | `headerView` | Mica Alt |
/// | [`Self::Menu`] | `menu` | Acrylic |
/// | [`Self::Popover`] | `popover` | Acrylic |
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowMaterial {
    /// 不透明（默认）：渲染路径和没有这个功能时一样
    #[default]
    Opaque,
    /// 主窗口的底
    Window,
    /// 侧栏
    Sidebar,
    /// 标题栏、标签栏这样的顶部一条
    Header,
    /// 菜单
    Menu,
    /// 弹出的小窗口
    Popover,
}

impl WindowMaterial {
    /// 不透明：没有材质，窗口照常画背景
    pub const fn is_opaque(self) -> bool {
        matches!(self, Self::Opaque)
    }

    /// `DWMWA_SYSTEMBACKDROP_TYPE` 的取值（`DWM_SYSTEMBACKDROP_TYPE`）
    pub const fn windows_backdrop(self) -> i32 {
        match self {
            Self::Opaque => 1,                 // DWMSBT_NONE
            Self::Window | Self::Sidebar => 2, // DWMSBT_MAINWINDOW：Mica
            Self::Menu | Self::Popover => 3,   // DWMSBT_TRANSIENTWINDOW：Acrylic
            Self::Header => 4,                 // DWMSBT_TABBEDWINDOW：Mica Alt
        }
    }
}

/// 隐藏了的标题栏的设置
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleBarOptions {
    /// 系统窗口按钮的位置（macOS 的红绿灯）：第一个按钮的左边、上边离窗口左上角多远，逻辑像素。
    /// 标题栏的高度跟着变成"按钮的高度加两倍的上边距"，按钮在这一条里竖直居中。`None` 时是系统的位置
    pub controls_position: Option<[f32; 2]>,
}

/// 隐藏了的标题栏此刻占了多少。都是窗口内容区里的逻辑像素，左上角为原点
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleBarMetrics {
    /// 标题栏这一条有多高。全屏时是 0
    pub height: f32,
    /// 系统窗口按钮（macOS 的红绿灯）的范围：`[x, y, 宽, 高]`。这个平台要应用自己画按钮、全屏时是 `None`
    pub controls: Option<[f32; 4]>,
}

/// 隐藏了的标题栏的度量变了（窗口缩放、进出全屏、系统重新摆了按钮）。可能不在调用方期望的时机调用，调用方转回主线程再读
pub type OnTitleBarChange = Arc<dyn Fn() + Send + Sync>;

/// 双击标题栏该做什么：系统设置里的选择
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TitleBarAction {
    /// 缩放：最大化或还原（默认）
    #[default]
    Zoom,
    /// 最小化
    Minimize,
    /// 什么都不做
    None,
}

impl TitleBarAction {
    /// macOS 用户默认设置里的 `AppleActionOnDoubleClick`：`Maximize`（缩放）、`Fill`（填充，当缩放）、`Minimize`、`None`。
    /// 没设过是缩放；老系统上是布尔的 `AppleMiniaturizeOnDoubleClick`
    pub fn from_macos_defaults(action: Option<&str>, miniaturize: bool) -> Self {
        match action {
            Some("Minimize") => Self::Minimize,
            Some("None") => Self::None,
            Some(_) => Self::Zoom,
            None if miniaturize => Self::Minimize,
            None => Self::Zoom,
        }
    }

    /// GNOME 的 `org.gnome.desktop.wm.preferences` 里的 `action-double-click-titlebar`：
    /// `toggle-maximize`（和两个方向上的）是缩放，`minimize` 是最小化，别的（`none`、`lower`、`menu`）都当什么都不做。
    /// 读不到是缩放
    pub fn from_gnome_setting(action: Option<&str>) -> Self {
        match action {
            None => Self::Zoom,
            Some(action) if action.starts_with("toggle-maximize") => Self::Zoom,
            Some("minimize") => Self::Minimize,
            Some(_) => Self::None,
        }
    }
}

/// 窗口边缘的哪一条边、哪一个角
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResizeEdge {
    /// 上
    Top,
    /// 下
    Bottom,
    /// 左
    Left,
    /// 右
    Right,
    /// 左上
    TopLeft,
    /// 右上
    TopRight,
    /// 左下
    BottomLeft,
    /// 右下
    BottomRight,
}

/// 指针按在内容铺满整个窗口的窗口的哪一条边上（Linux）：系统不再替这样的窗口留边框，边缘 `border` 以内算调整大小。
/// 角的范围是两倍的 `border`，比边好抓。`point`、`size`、`border` 同一种单位
pub fn resize_edge(point: [f32; 2], size: [f32; 2], border: f32) -> Option<ResizeEdge> {
    let ([x, y], [w, h]) = (point, size);
    if x < 0.0 || y < 0.0 || x >= w || y >= h {
        return None;
    }
    let (top, bottom) = (y < border, y >= h - border);
    let (left, right) = (x < border, x >= w - border);
    let (near_top, near_bottom) = (y < border * 2.0, y >= h - border * 2.0);
    let (near_left, near_right) = (x < border * 2.0, x >= w - border * 2.0);
    match () {
        () if (top && near_left) || (left && near_top) => Some(ResizeEdge::TopLeft),
        () if (top && near_right) || (right && near_top) => Some(ResizeEdge::TopRight),
        () if (bottom && near_left) || (left && near_bottom) => Some(ResizeEdge::BottomLeft),
        () if (bottom && near_right) || (right && near_bottom) => Some(ResizeEdge::BottomRight),
        () if top => Some(ResizeEdge::Top),
        () if bottom => Some(ResizeEdge::Bottom),
        () if left => Some(ResizeEdge::Left),
        () if right => Some(ResizeEdge::Right),
        () => None,
    }
}

/// 指针在隐藏了标题栏的窗口的内容区里算哪一块（Windows 的 `WM_NCHITTEST` 在内容区里按它回答）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowHit {
    /// 内容
    Client,
    /// 边框：调整大小
    Edge(ResizeEdge),
    /// 应用自己画的最大化按钮：系统按最大化按钮对待（悬停时弹贴靠布局）
    MaximizeButton,
}

/// 内容铺到窗口上边的窗口（Windows 隐藏了标题栏）：指针在内容区上边 `border` 以内是改大小，离左右两边两倍的 `border`
/// 以内是角。左、右、下用不着：那三边是系统的边框，在内容区外面，系统自己回答。`point`、`width`、`border` 同一种单位
pub fn top_edge(point: [f32; 2], width: f32, border: f32) -> Option<ResizeEdge> {
    let [x, y] = point;
    if x < 0.0 || y < 0.0 || x >= width || y >= border {
        return None;
    }
    Some(match () {
        () if x < border * 2.0 => ResizeEdge::TopLeft,
        () if x >= width - border * 2.0 => ResizeEdge::TopRight,
        () => ResizeEdge::Top,
    })
}

/// [`WindowHit`] 的规则（Windows 的内容区里，`point` 相对内容区的左上角）：能改大小而且没有最大化、全屏时（`framed`），
/// 上边一条算调整大小（[`top_edge`]，和系统的标题栏一样，最上面几个像素拖着改大小）；最大化按钮的区域优先于内容，
/// 但让位给这一条。都是物理像素。
pub fn hit_test(point: [f32; 2], width: f32, border: f32, framed: bool, maximize_button: Option<[f32; 4]>) -> WindowHit {
    if framed && let Some(edge) = top_edge(point, width, border) {
        return WindowHit::Edge(edge);
    }
    if let Some([x, y, w, h]) = maximize_button
        && point[0] >= x
        && point[0] < x + w
        && point[1] >= y
        && point[1] < y + h
    {
        return WindowHit::MaximizeButton;
    }
    WindowHit::Client
}

/// 窗口矩形、内容区、边框的左、上、右、下（Windows 的屏幕坐标，物理像素；边框是四边各多宽）
pub type Edges = [i32; 4];

/// 自动隐藏的任务栏贴着的那条边，最大化的窗口的内容区让出这么宽（物理像素）。内容区盖满整个显示器时系统把窗口当成全屏的，
/// 指针碰到屏幕边任务栏也不弹出来
pub const AUTOHIDE_TASKBAR_GAP: i32 = 2;

/// Windows：隐藏了标题栏、没有最大化的窗口的内容区（`WM_NCCALCSIZE`）。`frame` 是系统给这种样式的窗口的边框
/// （`AdjustWindowRectExForDpi`，上边含标题栏）。左、右、下留着系统的边框：Windows 10 起它看不见，是窗口外面那一圈拖着改大小的地方，
/// 阴影也跟着它，和别的窗口一样；上边不留，内容铺到窗口的上边（标题栏是应用画的，上边改大小的那一条在内容里，见 [`top_edge`]）
pub fn framed_client(window: Edges, frame: Edges) -> Edges {
    let [left, top, right, bottom] = window;
    [left + frame[0], top, right - frame[2], bottom - frame[3]]
}

/// Windows：隐藏了标题栏的最大化的窗口的内容区。最大化的窗口四边都比显示器的工作区大一圈边框（系统这样把改大小的边框藏到屏幕外面），
/// 四边各收进一圈边框（上边也只收边框，不含标题栏）、再限在工作区（`work`）里，正好是工作区。
/// `autohide` 是自动隐藏的任务栏在这个显示器的哪几条边上（左、上、右、下），那几条边让出 [`AUTOHIDE_TASKBAR_GAP`]
pub fn maximized_client(window: Edges, frame: Edges, work: Option<Edges>, autohide: [bool; 4]) -> Edges {
    let [left, top, right, bottom] = window;
    let inset = [left + frame[0], top + frame[3], right - frame[2], bottom - frame[3]];
    let clamped = work.map(|[l, t, r, b]| [inset[0].max(l), inset[1].max(t), inset[2].min(r), inset[3].min(b)]);
    let mut client = clamped.filter(|[l, t, r, b]| l < r && t < b).unwrap_or(inset);
    let gap = |on: bool| if on { AUTOHIDE_TASKBAR_GAP } else { 0 };
    client[0] += gap(autohide[0]);
    client[1] += gap(autohide[1]);
    client[2] -= gap(autohide[2]);
    client[3] -= gap(autohide[3]);
    client
}
