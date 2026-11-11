//! Windows：窗口所在的显示器（DXGI 的适配器和输出）。
//!
//! - 接在哪块 GPU 上（[`display_adapter`]）：渲染先用这块，画好的画面不用跨显卡拷过去；
//! - 颜色能力（[`display_color`]）：系统给这台显示器打开了 HDR 时，合成器按扩展范围（scRGB）合成，
//!   比 SDR 白亮的颜色、广色域的颜色才显示得出来。余量是显示器的最高亮度比系统设置里 SDR 内容的亮度。

use std::ffi::c_void;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::core::Interface;

use crate::{DisplayAdapter, DisplayColor};

/// scRGB 的 1.0 是 80 尼特；系统的 SDR 白点亮度按它的千分之一报
const SCRGB_WHITE_NITS: f32 = 80.0;

/// 驱动窗口所在显示器的适配器；不是 Win32 窗口、找不到（显示器接在 DXGI 列不出的虚拟显卡上）时是 `None`
pub(crate) fn display_adapter(window: &dyn HasWindowHandle) -> Option<DisplayAdapter> {
    let (adapter, _) = find_output(window)?;
    // SAFETY: 适配器是刚列出来的 COM 对象
    let desc = unsafe { adapter.GetDesc1() }.ok()?;
    Some(DisplayAdapter { vendor: desc.VendorId, device: desc.DeviceId })
}

/// 窗口所在显示器的颜色能力；系统没给这台显示器打开 HDR 时是普通的 SDR（合成器按 8 位 sRGB 合成，扩展范围显示不出来）
pub(crate) fn display_color(window: &dyn HasWindowHandle) -> Option<DisplayColor> {
    let (_, output) = find_output(window)?;
    let output: IDXGIOutput6 = output.cast().ok()?;
    // SAFETY: 输出是刚列出来的 COM 对象，GetDesc1 只写返回的结构体
    let desc = unsafe { output.GetDesc1() }.ok()?;
    let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    let sdr_white = if hdr { sdr_white_nits(&desc.DeviceName) } else { None };
    Some(hdr_display_color(hdr, desc.MaxLuminance, sdr_white))
}

/// 显示器的 HDR 开没开、最高亮度、系统设置里 SDR 内容的亮度（尼特）换成颜色能力。HDR 开着时合成器的色域是 BT.2020，
/// 广色域的颜色显示得出来；SDR 亮度读不到时按 scRGB 的白（80 尼特）算，余量至少是 1
fn hdr_display_color(hdr: bool, max_nits: f32, sdr_white_nits: Option<f32>) -> DisplayColor {
    if !hdr {
        return DisplayColor::SDR;
    }
    let white = sdr_white_nits.filter(|n| n.is_finite() && *n > 0.0).unwrap_or(SCRGB_WHITE_NITS);
    let headroom = if max_nits.is_finite() { (max_nits / white).max(1.0) } else { 1.0 };
    DisplayColor { wide_gamut: true, headroom, potential_headroom: headroom }
}

/// 窗口所在显示器对应的 DXGI 适配器和输出
fn find_output(window: &dyn HasWindowHandle) -> Option<(IDXGIAdapter1, IDXGIOutput)> {
    let RawWindowHandle::Win32(handle) = window.window_handle().ok()?.as_raw() else { return None };
    // SAFETY: MonitorFromWindow 只读窗口的位置，句柄无效时按 MONITOR_DEFAULTTONEAREST 返回最近的显示器；
    // DXGI 的工厂、适配器、输出是普通的 COM 调用，不需要先初始化 COM，对象出了作用域就放掉
    unsafe {
        let monitor = MonitorFromWindow(HWND(handle.hwnd.get() as *mut c_void), MONITOR_DEFAULTTONEAREST);
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut adapter_index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            adapter_index += 1;
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                output_index += 1;
                if output.GetDesc().is_ok_and(|desc| desc.Monitor == monitor) {
                    return Some((adapter, output));
                }
            }
        }
    }
    None
}

/// 系统设置里这台显示器上 SDR 内容的亮度（尼特）。`device` 是 DXGI 输出的 GDI 设备名（`\.\DISPLAY1`），
/// 按它在当前的显示配置里找到对应的目标再问
fn sdr_white_nits(device: &[u16; 32]) -> Option<f32> {
    use windows_sys::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
        GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };
    let (mut path_count, mut mode_count) = (0u32, 0u32);
    // SAFETY: 两个计数是调用方的变量
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) } != 0 {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: 两个数组按上面要的大小分配，计数和数组一致；不要拓扑编号，传空指针
    let queried =
        unsafe { QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut path_count, paths.as_mut_ptr(), &mut mode_count, modes.as_mut_ptr(), std::ptr::null_mut()) };
    if queried != 0 {
        return None;
    }
    let name_len = device.iter().position(|&c| c == 0).unwrap_or(device.len());
    for path in &paths[..(path_count as usize).min(paths.len())] {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            viewGdiDeviceName: [0; 32],
        };
        // SAFETY: 请求包的头写好了类型和整个结构体的大小，系统只写这个结构体
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 || source.viewGdiDeviceName[..name_len] != device[..name_len] {
            continue;
        }
        let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            SDRWhiteLevel: 0,
        };
        // SAFETY: 同上
        if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0 {
            return None;
        }
        return Some(white.SDRWhiteLevel as f32 / 1000.0 * SCRGB_WHITE_NITS);
    }
    None
}
