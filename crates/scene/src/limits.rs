//! 设备的限制：CPU 侧装箱（字形、路径、图片图集）和定离屏目标的大小时要守的上限。
//!
//! 数值由渲染后端在建好设备时登记（[`set_max_texture_size`]），这样主线程上装箱的代码不用认识 GPU。

use std::sync::atomic::{AtomicU32, Ordering};

static MAX_TEXTURE: AtomicU32 = AtomicU32::new(8192);

/// 设备支持的纹理最大边长（还没有设备时按常见的 8192 算）。图集、图片、窗口表面都不超过它
pub fn max_texture_size() -> u32 {
    MAX_TEXTURE.load(Ordering::Relaxed)
}

/// 登记设备支持的纹理最大边长：渲染后端每次建好设备时调用，之后 [`max_texture_size`] 返回它
pub fn set_max_texture_size(size: u32) {
    MAX_TEXTURE.store(size, Ordering::Relaxed);
}
