//! Windows 的打印：`PrintDlgEx` 给打印机的设备上下文，`StartDoc` / `StretchDIBits` / `EndDoc` 一页一页印。
//! 对话框和印都在自己的线程里（对话框是模态的，在主线程上会挡住事件循环；印的时候驱动可能要一会儿）。

use std::sync::Mutex;

use raw_window_handle::HasWindowHandle;
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GlobalFree, S_OK};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, GDI_ERROR, GetDeviceCaps, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY,
    PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH, SRCCOPY, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, VERTRES,
};
use windows_sys::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows_sys::Win32::UI::Controls::Dialogs::{
    PD_NOCURRENTPAGE, PD_NOPAGENUMS, PD_NOSELECTION, PD_RESULT_PRINT, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGEXW, PrintDlgExW, START_PAGE_GENERAL,
};

use super::{PrintDialogDone, PrintDone, PrintSetup};
use crate::{PlatformError, RgbaImage};

/// 对话框给的、还没印的打印机：编号、设备上下文（地址）、驱动不自己印多份时要印几份
static DEVICES: Mutex<Vec<(u64, isize, u32)>> = Mutex::new(Vec::new());

fn devices() -> std::sync::MutexGuard<'static, Vec<(u64, isize, u32)>> {
    DEVICES.lock().unwrap_or_else(|e| e.into_inner())
}

/// 弹出打印对话框，马上返回；对话框在自己的线程里，关掉后在那个线程调用 `done`。没有父窗口时不弹（系统要求有）
pub fn print_dialog(parent: Option<&dyn HasWindowHandle>, done: PrintDialogDone) {
    let Some(owner) = parent.and_then(crate::system::windows::hwnd) else {
        return done(Err(PlatformError::System("打印对话框要有父窗口".into())));
    };
    let spawned = std::thread::Builder::new().name("打印对话框".into()).spawn(move || {
        let result = {
            let _com = crate::system::windows::Com::init();
            // SAFETY: COM 在这个线程上初始化好了；父窗口是活着的窗口（对话框开着时系统把它禁用）
            unsafe { show(owner) }
        };
        done(result);
    });
    if let Err(error) = spawned {
        tracing::error!("打印对话框的线程开不出来：{error}");
    }
}

/// # Safety
/// 在初始化了 COM 的线程上调用，`owner` 是活着的窗口
unsafe fn show(owner: isize) -> Result<Option<PrintSetup>, PlatformError> {
    let mut dialog = PRINTDLGEXW {
        lStructSize: std::mem::size_of::<PRINTDLGEXW>() as u32,
        hwndOwner: owner as _,
        // 页数要等选好纸张、排好版才知道：不选页码范围，打印全部；份数和逐份打印交给驱动
        Flags: PD_RETURNDC | PD_NOPAGENUMS | PD_NOSELECTION | PD_NOCURRENTPAGE | PD_USEDEVMODECOPIESANDCOLLATE,
        nCopies: 1,
        nStartPage: START_PAGE_GENERAL,
        ..Default::default()
    };
    // SAFETY: 结构体大小、父窗口、标志都填好了；对话框关掉才返回
    let hr = unsafe { PrintDlgExW(&mut dialog) };
    // SAFETY: 对话框分配的设备模式、设备名，不用了就还（空的不用还）
    unsafe {
        for handle in [dialog.hDevMode, dialog.hDevNames] {
            if !handle.is_null() {
                GlobalFree(handle);
            }
        }
    }
    if hr != S_OK {
        return Err(PlatformError::System(format!("打印对话框出错：{hr:#010x}").into()));
    }
    let dc = dialog.hDC;
    if dialog.dwResultAction != PD_RESULT_PRINT {
        if !dc.is_null() {
            // SAFETY: 对话框给的设备上下文，没别人用
            unsafe { DeleteDC(dc) };
        }
        return Ok(None);
    }
    if dc.is_null() {
        return Err(PlatformError::System("打印对话框没有给打印机".into()));
    }
    // SAFETY: 打印机的设备上下文，只读它的度量
    let caps = |index: u32| unsafe { GetDeviceCaps(dc, index as i32) } as f32;
    let dpi = [caps(LOGPIXELSX).max(1.0), caps(LOGPIXELSY).max(1.0)];
    // 设备单位换成点
    let points = |value: f32, axis: usize| value * 72.0 / dpi[axis];
    let setup = PrintSetup {
        paper: [points(caps(PHYSICALWIDTH), 0), points(caps(PHYSICALHEIGHT), 1)],
        printable: [points(caps(PHYSICALOFFSETX), 0), points(caps(PHYSICALOFFSETY), 1), points(caps(HORZRES), 0), points(caps(VERTRES), 1)],
        dpi: dpi[0].min(dpi[1]),
        id: super::next_id(),
    };
    devices().push((setup.id, dc as isize, dialog.nCopies.max(1)));
    Ok(Some(setup))
}

/// 印：在自己的线程里一页一页交给打印机，完成后在那个线程调用 `done`。没有页时只释放设备
pub fn print(setup: PrintSetup, title: &str, pages: Vec<RgbaImage>, done: PrintDone) {
    let device = {
        let mut devices = devices();
        devices.iter().position(|(id, ..)| *id == setup.id).map(|index| devices.remove(index))
    };
    let Some((_, dc, copies)) = device else {
        return done(Err(PlatformError::System("这次打印已经印过了，或者不是对话框给的".into())));
    };
    let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let spawned = std::thread::Builder::new().name("打印".into()).spawn(move || {
        let dc = dc as HDC;
        let result = super::check_pages(&pages).and_then(|()| {
            if pages.is_empty() {
                return Ok(true);
            }
            // SAFETY: 对话框给的设备上下文，只有这个线程在用；文档名以 0 结尾
            unsafe { print_pages(dc, &title, &pages, copies) }
        });
        // SAFETY: 印完了，设备上下文不再用
        unsafe { DeleteDC(dc) };
        done(result);
    });
    if let Err(error) = spawned {
        tracing::error!("打印的线程开不出来：{error}");
    }
}

/// # Safety
/// `dc` 是打印机的设备上下文、只有调用方在用；`title` 以 0 结尾
unsafe fn print_pages(dc: HDC, title: &[u16], pages: &[RgbaImage], copies: u32) -> Result<bool, PlatformError> {
    let failed = |what: &str| PlatformError::System(format!("打印出错：{what}（{}）", std::io::Error::last_os_error()).into());
    let info = DOCINFOW { cbSize: std::mem::size_of::<DOCINFOW>() as i32, lpszDocName: title.as_ptr(), ..Default::default() };
    // SAFETY: 文档信息活到调用结束
    if unsafe { StartDocW(dc, &info) } <= 0 {
        // 存成文件的打印机（Microsoft Print to PDF）先问文件名：用户取消了不算错
        let cancelled = std::io::Error::last_os_error().raw_os_error() == Some(ERROR_CANCELLED as i32);
        return if cancelled { Ok(false) } else { Err(failed("开始不了文档")) };
    }
    // 能印到的范围，设备像素：每页铺满它
    // SAFETY: 只读度量
    let [width, height] = [HORZRES, VERTRES].map(|index| unsafe { GetDeviceCaps(dc, index as i32) });
    for _ in 0..copies {
        for page in pages {
            // SAFETY: 文档开始了；位图的像素和描述在这一页里一直活着
            let printed = unsafe { print_page(dc, page, width, height) };
            if let Err(what) = printed {
                // SAFETY: 文档开始了，放弃它
                unsafe { AbortDoc(dc) };
                return Err(failed(what));
            }
        }
    }
    // SAFETY: 文档开始了
    if unsafe { EndDoc(dc) } <= 0 {
        return Err(failed("结束不了文档"));
    }
    Ok(true)
}

/// # Safety
/// 文档开始了
unsafe fn print_page(dc: HDC, page: &RgbaImage, width: i32, height: i32) -> Result<(), &'static str> {
    // 系统要 BGRA；页面底色是白的、不透明
    let bgra: Vec<u8> = page.rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [b, g, r, a]).collect();
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: page.width as i32,
        // 负的高：从上往下
        biHeight: -(page.height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..Default::default()
    };
    let bitmap = BITMAPINFO { bmiHeader: header, ..Default::default() };
    // SAFETY: 文档开始了
    if unsafe { StartPage(dc) } <= 0 {
        return Err("开始不了这一页");
    }
    // SAFETY: 像素有 宽×高×4 个字节（check_pages 查过），描述和它一致；放大时按半色调取样
    let drawn = unsafe {
        SetStretchBltMode(dc, HALFTONE);
        SetBrushOrgEx(dc, 0, 0, std::ptr::null_mut());
        StretchDIBits(dc, 0, 0, width, height, 0, 0, page.width as i32, page.height as i32, bgra.as_ptr().cast(), &bitmap, DIB_RGB_COLORS, SRCCOPY)
    };
    if drawn == 0 || drawn == GDI_ERROR {
        return Err("画不上这一页");
    }
    // SAFETY: 这一页开始了
    if unsafe { EndPage(dc) } <= 0 {
        return Err("结束不了这一页");
    }
    Ok(())
}
