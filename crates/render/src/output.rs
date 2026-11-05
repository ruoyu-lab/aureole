//! 窗口表面的输出方式：8 位 sRGB，或者半精度浮点的线性扩展 sRGB（广色域、HDR）。
//!
//! 工作空间一直是线性的 sRGB 原色。扩展范围的表面就用同样的原色，只是分量可以小于 0（sRGB 色域外，
//! 比如 Display P3 的饱和色）、大于 1（比 SDR 白亮），由系统合成时换到显示器的色彩空间。
//! 所以 sRGB 的内容在两种表面上算出来的值一样，不用换算，也不会过饱和。

/// 扩展范围的表面、离屏目标用的格式：半精度浮点，线性
pub const EXTENDED_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// 窗口表面的输出方式
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SurfaceOutput {
    /// 8 位 sRGB（默认）：色域外的分量截掉，比 SDR 白亮的颜色压到 1
    #[default]
    Srgb,
    /// 半精度浮点、线性的扩展 sRGB（[`EXTENDED_FORMAT`]）：广色域和比 SDR 白亮的颜色都原样交给系统合成。
    /// 每个像素 8 字节，比 8 位的表面多一倍带宽，只在有扩展范围的内容时用
    Extended,
}

/// 按表面的能力选格式和色彩空间：要扩展范围（`extended`）、表面又支持 [`EXTENDED_FORMAT`] 上的
/// 线性扩展 sRGB 时用它；否则用 sRGB 编码的 8 位格式（没有就用第一个格式）。表面一个格式都没有时返回 `None`。
///
/// ```
/// use aureole_render::{SurfaceOutput, surface_format};
/// let mut caps = wgpu::SurfaceCapabilities::default();
/// caps.formats = vec![wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Bgra8UnormSrgb];
/// let (format, _, output) = surface_format(&caps, true).unwrap();
/// assert_eq!((format, output), (wgpu::TextureFormat::Bgra8UnormSrgb, SurfaceOutput::Srgb));
/// ```
pub fn surface_format(caps: &wgpu::SurfaceCapabilities, extended: bool) -> Option<(wgpu::TextureFormat, wgpu::SurfaceColorSpace, SurfaceOutput)> {
    if extended && supports_extended(caps) {
        return Some((EXTENDED_FORMAT, wgpu::SurfaceColorSpace::ExtendedSrgbLinear, SurfaceOutput::Extended));
    }
    let format = caps.formats.iter().copied().find(|f| f.is_srgb()).or_else(|| caps.formats.first().copied())?;
    Some((format, wgpu::SurfaceColorSpace::Auto, SurfaceOutput::Srgb))
}

/// 表面能不能用扩展范围的输出（[`EXTENDED_FORMAT`] 上的线性扩展 sRGB）
pub fn supports_extended(caps: &wgpu::SurfaceCapabilities) -> bool {
    caps.color_spaces(EXTENDED_FORMAT).contains(wgpu::SurfaceColorSpaces::EXTENDED_SRGB_LINEAR)
}

/// 窗口透明（透出系统的窗口材质）时表面用的透明度模式：预乘透明度。Metal 只报告 `PostMultiplied`，它在 Metal 上的意思是
/// 图层不透明的标记关掉（`CAMetalLayer.opaque = false`），系统合成时还是按预乘算。这个表面不能透明（HWND 上的 DX12 交换链）时是 `None`
pub fn transparent_alpha_mode(caps: &wgpu::SurfaceCapabilities) -> Option<wgpu::CompositeAlphaMode> {
    [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::PostMultiplied].into_iter().find(|mode| caps.alpha_modes.contains(mode))
}
