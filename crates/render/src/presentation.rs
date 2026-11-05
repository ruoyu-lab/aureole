//! 窗口表面怎么呈现：改窗口大小时画面和窗口一起变。
//!
//! macOS 上有两件事（别的平台什么都不做）：
//! - [`keep_surface_contents_unscaled`]：新的一帧到之前，层里的旧画面不拉伸（系统默认拉伸到新的大小，
//!   内容先被拉变形，下一帧又弹回来）；
//! - [`set_surface_present_in_transaction`]：这一帧的呈现进系统动画事务，由调用线程的 `Queue::present` 提交。
//!   主线程在窗口改大小的回调里呈现，新画面和新的窗口大小在同一个事务里生效。

/// 新的一帧到之前，层里的旧画面保持原来的大小，贴着左上角，不拉伸
pub fn keep_surface_contents_unscaled(surface: &wgpu::Surface<'_>) {
    #[cfg(target_os = "macos")]
    with_layer(surface, |layer| {
        // SAFETY: 这个常量是系统框架导出的，进程的整个生命周期都有效
        layer.setContentsGravity(unsafe { objc2_quartz_core::kCAGravityTopLeft });
    });
    #[cfg(not(target_os = "macos"))]
    let _ = surface;
}

/// 之后取到的表面纹理，`Queue::present` 时进调用线程的系统动画事务（并等命令排上队）。
/// 只影响设了之后取的纹理；不是这一帧要同步时关掉：平时走系统的呈现队列，不挡调用线程
pub fn set_surface_present_in_transaction(surface: &wgpu::Surface<'_>, on: bool) {
    #[cfg(target_os = "macos")]
    with_layer(surface, |layer| layer.setPresentsWithTransaction(on));
    #[cfg(not(target_os = "macos"))]
    let _ = (surface, on);
}

/// 不在主线程上呈现了一张进事务的纹理（主线程等不及先走了）：这个线程没有运行循环替它提交事务，手动提交
pub fn flush_surface_transaction() {
    #[cfg(target_os = "macos")]
    objc2_quartz_core::CATransaction::flush();
}

#[cfg(target_os = "macos")]
fn with_layer(surface: &wgpu::Surface<'_>, f: impl FnOnce(&objc2_quartz_core::CAMetalLayer)) {
    // SAFETY: 只在这个调用里用底层的表面，不改它的所有权，也不让它活过这个调用。层的这些属性可以在任何线程上设
    let hal = unsafe { surface.as_hal::<wgpu::hal::api::Metal>() };
    if let Some(hal) = hal {
        f(&hal.render_layer().lock());
    }
}
