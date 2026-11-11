//! macOS 上的快路径：CEF 交来的 IOSurface 直接包成 Metal 纹理（不复制），在 GPU 上一次复制进登记的纹理
//! （两边的格式只差 sRGB），主线程不等复制做完。复制做完以前 IOSurface 记着"在用"（系统的使用计数），
//! 复制做完的回调里减回去。

use std::ptr::NonNull;

use cef::AcceleratedPaintInfo;
use cef::sys::cef_color_type_t;
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

/// 包好的一帧：底下是 IOSurface 的纹理（只能当复制的来源），和记着在用的 IOSurface
pub(super) struct Frame {
    pub texture: wgpu::Texture,
    pub surface: InUse,
}

/// 记着在用的 IOSurface：丢掉时减回使用计数、放掉引用
pub(super) struct InUse(CFRetained<IOSurfaceRef>);

// SAFETY: IOSurface 的使用计数、引用计数可以在任何线程上改（复制做完的回调在 GPU 设备轮询的线程上）
unsafe impl Send for InUse {}

impl Drop for InUse {
    fn drop(&mut self) {
        self.0.decrement_use_count();
    }
}

/// 把 `info` 里的 IOSurface 包成 `device` 上的纹理。不是 Metal 后端、格式不认识、大小不对时返回原因
///
/// # Safety
///
/// 只在 CEF 的离屏回调里调用：回调期间 `info` 里的 IOSurface 有效
pub(super) unsafe fn wrap(device: &wgpu::Device, info: &AcceleratedPaintInfo) -> Result<Frame, String> {
    let handle = NonNull::new(info.shared_texture_io_surface.cast::<IOSurfaceRef>()).ok_or("没有 IOSurface")?;
    // SAFETY: 回调期间 CEF 的 IOSurface 有效；加一个引用以后归我们，复制做完才放掉
    let surface = unsafe { CFRetained::retain(handle) };
    let (format, pixel_format) = match *info.format.as_ref() {
        cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 => (wgpu::TextureFormat::Bgra8Unorm, MTLPixelFormat::BGRA8Unorm),
        cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888 => (wgpu::TextureFormat::Rgba8Unorm, MTLPixelFormat::RGBA8Unorm),
        other => return Err(format!("不认识的像素格式 {other:?}")),
    };
    let size = &info.extra.coded_size;
    let (width, height) = (size.width.max(0) as u32, size.height.max(0) as u32);
    let max = device.limits().max_texture_dimension_2d;
    if width == 0 || height == 0 || width > max || height > max {
        return Err(format!("大小不对：{width}×{height}"));
    }

    let descriptor = MTLTextureDescriptor::new();
    // SAFETY: 都是普通的属性设置，值在 Metal 允许的范围里（2D、一层、一级、单采样，大小没超过上限）
    unsafe {
        descriptor.setTextureType(MTLTextureType::Type2D);
        descriptor.setPixelFormat(pixel_format);
        descriptor.setWidth(width as usize);
        descriptor.setHeight(height as usize);
        descriptor.setMipmapLevelCount(1);
        descriptor.setSampleCount(1);
        descriptor.setArrayLength(1);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Managed);
    }
    // SAFETY: 只在 Metal 后端上取出底层设备；用它建的纹理交给同一个设备
    let hal = unsafe { device.as_hal::<wgpu::wgc::api::Metal>() }.ok_or("GPU 设备不是 Metal 后端")?;
    let raw = hal.raw_device().newTextureWithDescriptor_iosurface_plane(&descriptor, &surface, 0).ok_or("IOSurface 包不成 Metal 纹理")?;
    // SAFETY: `raw` 是这个设备上刚建的 2D 纹理，格式、层数、级数、大小和下面的描述一致
    let hal_texture = unsafe {
        <wgpu::wgc::api::Metal as wgpu::hal::Api>::Device::texture_from_raw(
            raw,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent { width, height, depth: 1 },
            None,
        )
    };
    drop(hal);
    let desc = wgpu::TextureDescriptor {
        label: Some("CEF 视图的 IOSurface"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    };
    // SAFETY: 底层纹理建在这个设备上，描述和它一致，只当复制的来源
    let texture = unsafe { device.create_texture_from_hal::<wgpu::wgc::api::Metal>(hal_texture, &desc, wgpu::TextureUses::RESOURCE) };
    surface.increment_use_count();
    Ok(Frame { texture, surface: InUse(surface) })
}
