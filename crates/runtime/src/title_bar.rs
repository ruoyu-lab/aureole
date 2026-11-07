//! 隐藏的标题栏和窗口材质在运行时这一侧的部分：建窗口时的窗口库属性、平台给的度量换成界面读的 [`TitleBarInfo`]、
//! Linux 上窗口边缘的调整大小。平台那一侧见 `aureole_platform::title_bar`。

use aureole_core::{Rect, TitleBarInfo};
use platform::{TitleBar, TitleBarMetrics, WindowMaterial};
use winit::window::WindowAttributes;

/// 建窗口时的属性：内容铺满整个窗口、标题栏透明（macOS），去掉标题栏但留着边框和阴影（Windows），去装饰（Linux）。
/// 有材质的窗口的内容画在系统合成的对象上，不要窗口自己的位图（Windows）
pub(crate) fn window_attributes(attributes: WindowAttributes, title_bar: TitleBar, backdrop: WindowMaterial) -> WindowAttributes {
    let hidden = title_bar == TitleBar::Hidden;
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::WindowAttributesExtMacOS;
        let _ = backdrop;
        if hidden { attributes.with_titlebar_transparent(true).with_title_hidden(true).with_fullsize_content_view(true) } else { attributes }
    }
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        let attributes = if hidden { attributes.with_decorations(false).with_undecorated_shadow(true) } else { attributes };
        if backdrop.is_opaque() { attributes } else { attributes.with_no_redirection_bitmap(true) }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = backdrop;
        if hidden { attributes.with_decorations(false) } else { attributes }
    }
}

/// 平台给的度量换成界面读的值：内容从窗口按钮的另一边开始，和窗口边之间留多宽，和按钮之间也留多宽。
/// 按钮在窗口右半边时（从右到左的系统语言）让开的是右边
pub(crate) fn info(metrics: TitleBarMetrics, window_width: f32) -> TitleBarInfo {
    let (mut inset_left, mut inset_right) = (0.0, 0.0);
    if let Some([x, _, width, _]) = metrics.controls {
        if x + width / 2.0 <= window_width / 2.0 {
            inset_left = x + width + x;
        } else {
            let margin = window_width - (x + width);
            inset_right = margin + width + margin;
        }
    }
    let controls = metrics.controls.map(|[x, y, width, height]| Rect { x, y, width, height });
    TitleBarInfo { height: metrics.height, controls, inset_left, inset_right }
}

/// 窗口的清屏颜色：有材质时没设背景色就是透明的（设了就是盖在材质上的一层，可以带透明度）；不透明时没设是调色板的窗口底色
/// （跟着应用的外观）
pub(crate) fn clear_color(background: Option<scene::Color>, backdrop: WindowMaterial) -> scene::Color {
    match background {
        Some(color) => color,
        None if backdrop.is_opaque() => aureole_core::defaults::palette().background,
        None => scene::Color::TRANSPARENT,
    }
}

/// Linux：内容铺满的窗口没有系统的边框，窗口边缘这么宽（逻辑像素）算调整大小
#[cfg(target_os = "linux")]
const RESIZE_BORDER: f32 = 6.0;

/// Linux：指针在窗口的哪条边上（最大化、全屏时没有边框，不能改大小的窗口也没有）
#[cfg(target_os = "linux")]
pub(crate) fn resize_edge_at(window: &winit::window::Window, point: Option<[f32; 2]>) -> Option<platform::ResizeEdge> {
    if !window.is_resizable() || window.is_maximized() || window.fullscreen().is_some() {
        return None;
    }
    let size = window.inner_size().to_logical::<f32>(window.scale_factor());
    platform::title_bar::resize_edge(point?, [size.width, size.height], RESIZE_BORDER)
}

/// Linux：窗口的这条边对应的调整大小的方向
#[cfg(target_os = "linux")]
pub(crate) fn resize_direction(edge: platform::ResizeEdge) -> winit::window::ResizeDirection {
    use platform::ResizeEdge as E;
    use winit::window::ResizeDirection as D;
    match edge {
        E::Top => D::North,
        E::Bottom => D::South,
        E::Left => D::West,
        E::Right => D::East,
        E::TopLeft => D::NorthWest,
        E::TopRight => D::NorthEast,
        E::BottomLeft => D::SouthWest,
        E::BottomRight => D::SouthEast,
    }
}

/// Linux：窗口的这条边上的指针形状
#[cfg(target_os = "linux")]
pub(crate) fn resize_cursor(edge: platform::ResizeEdge) -> winit::window::CursorIcon {
    use platform::ResizeEdge as E;
    use winit::window::CursorIcon as C;
    match edge {
        E::Top | E::Bottom => C::NsResize,
        E::Left | E::Right => C::EwResize,
        E::TopLeft | E::BottomRight => C::NwseResize,
        E::TopRight | E::BottomLeft => C::NeswResize,
    }
}
