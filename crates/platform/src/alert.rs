//! 出错退出前的提示框：macOS 是 NSAlert，Windows 是 MessageBoxW，别的平台不弹（调用方已经记下了错误）。

/// 弹出一个模态的错误提示框，用户关掉后返回。只在主线程调用（macOS 不在主线程时不弹）
pub(crate) fn show(title: &str, message: &str) {
    imp::alert(title, message);
}

#[cfg(target_os = "macos")]
mod imp {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication};
    use objc2_foundation::NSString;

    pub fn alert(title: &str, message: &str) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        // 应用可能还没有窗口、没被激活：提示框放到最前面
        NSApplication::sharedApplication(mtm).activate();
        alert.runModal();
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW};

    pub fn alert(title: &str, message: &str) {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (title, message) = (wide(title), wide(message));
        // SAFETY: 两个字符串都以 0 结尾，在调用期间有效；没有父窗口
        unsafe { MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND) };
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    pub fn alert(_title: &str, _message: &str) {}
}
