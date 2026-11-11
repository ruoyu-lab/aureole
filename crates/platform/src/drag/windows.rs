//! 拖到应用外面（Windows）：外壳的 `SHDoDragDrop`（OLE 的 `DoDragDrop` 加上拖动的图像），数据对象是外壳自己的
//! （`CF_HDROP`、外壳的条目列表，资源管理器和别的程序都认），别的格式和剪贴板一样各放一份：`CF_UNICODETEXT`、
//! `Rich Text Format`、图片（`PNG`、`CF_DIBV5`、`CF_DIB`）、应用自己定的格式（按名字登记）。
//! 只许拷贝：拖到资源管理器里不会把原来的文件挪走。
//!
//! 拖动的图像：有文件时外壳按它们生成（和在资源管理器里拖文件一样）；没有文件、有图片时是这张图（缩到 96 逻辑像素以内）；
//! 只有文字时外壳给的是一个空白的方块，换成纯文本文件的图标（和 macOS 一样，48 逻辑像素，指针在中间）。
//!
//! `SHDoDragDrop` 在里面转一个模态循环，指针松开才返回。在窗口库的事件回调里开始的话，窗口库把这期间的事件都攒到回调返回，
//! 界面就停住了。所以先投递出去（[`run_outside_event_callback`]：没有窗口的计时器，到点时由外层的消息循环分发，
//! 不在窗口库的回调里），开始以后拿着 [`crate::modal::ModalGuard`]：运行时知道在模态循环里，照样在收到的事件里自己推进一轮。
//!
//! 应用要退出时先取消（[`cancel_drag_out`]）：拖放的循环下一次问"要不要继续"时回答取消。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{COLORREF, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HGLOBAL, HWND, POINT, S_OK, SIZE};
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HBITMAP};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, DVASPECT_CONTENT, FORMATETC, IBindCtx, IDataObject, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropSource, IDropSource_Impl, OleInitialize};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CLSID_DragDropHelper, IDragSourceHelper, IShellItemArray, IShellItemImageFactory, SHCreateDataObject,
    SHCreateItemFromIDList, SHCreateShellItemArrayFromIDLists, SHDRAGIMAGE, SHDoDragDrop, SHSimpleIDListFromPath, SIIGBF_ICONONLY,
};
use windows::core::{BOOL, HRESULT, PCWSTR};
use windows_core::implement;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_LBUTTON};
use windows_sys::Win32::UI::Shell::{ILCreateFromPathW, ILFree};
use windows_sys::Win32::UI::WindowsAndMessaging::{KillTimer, PostThreadMessageW, SetTimer, WM_NULL};

use crate::clipboard::windows::{Global, custom_format, encode_png, png_format, rtf_format, to_crlf};
use crate::clipboard::dib;
use crate::{ClipboardContent, RgbaImage};
use crate::system::windows::shell_path;

thread_local! {
    /// 投递出去、还没执行的（计时器的编号 → 要做的）
    static DEFERRED: RefCell<HashMap<usize, Box<dyn FnOnce()>>> = RefCell::new(HashMap::new());
    /// 有一次拖出去的会话（开始了、或者排着要开始）
    static SESSION: Cell<bool> = const { Cell::new(false) };
    /// 会话被取消了（应用要退出）
    static CANCEL: Cell<bool> = const { Cell::new(false) };
}

/// 等这次系统事件处理完，在窗口库的事件回调外面执行 `f`（主线程）：定一个没有窗口的计时器（系统按最短的间隔算，约 10 毫秒），
/// 计时器的消息由外层的消息循环分发，回调不在窗口库的处理器里。定不了计时器时就地执行。`f` 里的 panic 接住、记一条错误
/// （不能穿过系统的栈帧）
pub(crate) fn run_outside_event_callback(f: Box<dyn FnOnce()>) {
    // SAFETY: 没有窗口的计时器：编号由系统给；回调是下面的 `extern "system"` 函数，签名和 `TIMERPROC` 一致
    let id = unsafe { SetTimer(std::ptr::null_mut(), 0, 0, Some(run_deferred)) };
    if id == 0 {
        tracing::debug!("定不了计时器，就地执行");
        f();
        return;
    }
    DEFERRED.with(|deferred| deferred.borrow_mut().insert(id, f));
}

/// 计时器到点（主线程，消息循环分发的）：执行一次就取消计时器
unsafe extern "system" fn run_deferred(_window: windows_sys::Win32::Foundation::HWND, _message: u32, id: usize, _time: u32) {
    // SAFETY: 编号是系统交来的、这个线程上的计时器
    unsafe { KillTimer(std::ptr::null_mut(), id) };
    let f = DEFERRED.with(|deferred| deferred.borrow_mut().remove(&id));
    if let Some(f) = f
        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
    {
        tracing::error!("投递到事件回调外面执行的代码 panic 了");
    }
}

/// 左键此刻按着（这个线程收到的输入里的状态；左右键对调了时是用户的"主按键"）
fn left_button_down() -> bool {
    // SAFETY: 只读这个线程的键盘状态，没有指针
    unsafe { GetKeyState(i32::from(VK_LBUTTON)) < 0 }
}

/// 开始把文件（要已经存在）或文字拖到应用外面。要在指针按着拖动时调用；会话在事件回调外面开始（见模块说明），
/// 从指针此刻的位置拖起（`at` 用不上：系统按指针的位置）。结束时调用 `on_end`，参数是有没有被接住。
/// 内容可以是空的：只有这个进程自己的窗口认得这次拖放（[`crate::external_drag::ExternalDataKinds::own`]）。
/// 开始不了（不是 Win32 窗口、左键已经松开、已经有一次在拖）时返回 `false`，不调用 `on_end`
pub fn begin_drag_out(window: &dyn HasWindowHandle, content: &ClipboardContent, at: [f32; 2], on_end: Box<dyn FnOnce(bool)>) -> bool {
    let _ = at;
    let hwnd = window.window_handle().ok().and_then(|handle| match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    });
    let Some(hwnd) = hwnd.filter(|_| !SESSION.get()) else { return false };
    let files: Vec<PathBuf> = content.files.iter().filter(|path| path.exists()).filter_map(|path| std::path::absolute(path).ok()).map(|path| shell_path(&path)).collect();
    let text = content.text.as_deref().filter(|text| !text.is_empty()).map(|text| to_crlf(text).into_owned());
    let content = ClipboardContent { files, text, image: content.image.clone().filter(RgbaImage::is_valid), ..content.clone() };
    if !left_button_down() {
        return false;
    }
    SESSION.set(true);
    CANCEL.set(false);
    run_outside_event_callback(Box::new(move || {
        // 投递出去的这一小会儿里松开了左键、或者被取消了：不开始（不然会在原地放下）
        let dropped = left_button_down()
            && !CANCEL.get()
            && drag(HWND(hwnd as *mut _), &content).unwrap_or_else(|error| {
                tracing::warn!("拖不到应用外面：{error}");
                false
            });
        SESSION.set(false);
        CANCEL.set(false);
        on_end(dropped);
    }));
    true
}

/// 有一次拖出去的会话（开始了、或者排着要开始）：自己的窗口上经过的拖放就是它
pub(crate) fn dragging_out() -> bool {
    SESSION.get()
}

/// 取消正在进行（或者排着要开始）的拖出去的会话（应用要退出时）：会话按没被接住结束（调用 `on_end(false)`）。
/// 没有会话时什么都不做，返回 `false`
pub fn cancel_drag_out() -> bool {
    if !SESSION.get() {
        return false;
    }
    CANCEL.set(true);
    // 叫醒拖放的循环，让它马上问一次要不要继续
    // SAFETY: 往这个线程自己的消息队列放一条空消息，没有指针
    unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_NULL, 0, 0) };
    true
}

/// 转 `SHDoDragDrop`（模态循环，指针松开、按了 Esc 或者被取消才返回）。返回有没有被接住
fn drag(window: HWND, content: &ClipboardContent) -> windows::core::Result<bool> {
    // SAFETY: 主线程，单线程套间；拖放目标那边已经初始化过时只是多一层（不配对，留给线程结束）
    unsafe { OleInitialize(None)? };
    let data = data_object(&content.files)?;
    if let Some(text) = &content.text {
        let bytes: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
        set_bytes(&data, CF_UNICODETEXT.0, &bytes)?;
    }
    if let Some(rich) = &content.rich_text {
        set_bytes(&data, rtf_format() as u16, &[rich.as_slice(), &[0]].concat())?;
    }
    if let Some(image) = &content.image {
        // 编码不了 PNG（没有图片编解码组件）时只放位图
        if let Some(png) = encode_png(image) {
            set_bytes(&data, png_format() as u16, &png)?;
        }
        set_bytes(&data, windows::Win32::System::Ole::CF_DIBV5.0, &dib::encode(image))?;
        set_bytes(&data, windows::Win32::System::Ole::CF_DIB.0, &dib::encode_opaque(image))?;
    }
    for (name, bytes) in &content.custom {
        match custom_format(name) {
            Some(format) => set_bytes(&data, format as u16, bytes)?,
            None => tracing::debug!("登记不了拖放格式「{name}」，不带它"),
        }
    }
    if content.files.is_empty() {
        let image = match &content.image {
            Some(image) => image_drag_image(window, &data, image),
            None => text_drag_image(window, &data),
        };
        if let Err(error) = image {
            tracing::debug!("准备不了拖动的图像（照样拖）：{error}");
        }
    }
    let source: IDropSource = DropSource.into();
    let _modal = crate::modal::ModalGuard::enter();
    // SAFETY: 窗口活着（这一轮的事件循环里）；数据对象和源都活到调用结束；只许拷贝。
    // 取消时也是成功（DRAGDROP_S_CANCEL），效果是"没有"
    let effect = unsafe { SHDoDragDrop(Some(window), &data, &source, DROPEFFECT_COPY)? };
    Ok(effect != DROPEFFECT_NONE)
}

/// 只拖文字时图标的大小（逻辑像素，和 macOS 一样）
const TEXT_ICON: f32 = 48.0;

/// 只拖文字时的图像放进数据对象（`SHDoDragDrop` 看到已经有了就不再生成）：纯文本文件的图标，指针在中间
fn text_drag_image(window: HWND, data: &IDataObject) -> windows::core::Result<()> {
    // SAFETY: 系统的拖放帮手（进程内的 COM 对象），这个线程上初始化过 OLE
    let helper: IDragSourceHelper = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)? };
    // SAFETY: 窗口活着
    let dpi = unsafe { GetDpiForWindow(window) }.max(96);
    let size = (TEXT_ICON * dpi as f32 / 96.0).round() as i32;
    let bitmap = text_icon(size)?;
    /// 不用颜色键：位图带透明度
    const CLR_NONE: u32 = 0xFFFF_FFFF;
    let image = SHDRAGIMAGE { sizeDragImage: SIZE { cx: size, cy: size }, ptOffset: POINT { x: size / 2, y: size / 2 }, hbmpDragImage: bitmap, crColorKey: COLORREF(CLR_NONE) };
    // SAFETY: 描述、数据对象活到调用结束；成功后位图归帮手（它删），失败时这里删
    unsafe {
        helper.InitializeFromBitmap(&image, data).inspect_err(|_| {
            let _ = DeleteObject(bitmap.into());
        })
    }
}

/// 只拖图片时图像的最大边长（逻辑像素）
const IMAGE_PREVIEW: f32 = 96.0;

/// 没有文件、有图片时的拖动图像：这张图缩到 [`IMAGE_PREVIEW`] 以内（不放大），指针在中间
fn image_drag_image(window: HWND, data: &IDataObject, image: &RgbaImage) -> windows::core::Result<()> {
    // SAFETY: 系统的拖放帮手（进程内的 COM 对象），这个线程上初始化过 OLE
    let helper: IDragSourceHelper = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)? };
    // SAFETY: 窗口活着
    let dpi = unsafe { GetDpiForWindow(window) }.max(96);
    let limit = IMAGE_PREVIEW * dpi as f32 / 96.0;
    let fit = (limit / image.width.max(image.height) as f32).min(1.0);
    let size = [((image.width as f32 * fit).round() as u32).max(1), ((image.height as f32 * fit).round() as u32).max(1)];
    let bitmap = premultiplied_bitmap(&shrink(image, size), size)?;
    /// 不用颜色键：位图带透明度
    const CLR_NONE: u32 = 0xFFFF_FFFF;
    let [w, h] = size.map(|v| v as i32);
    let shdi = SHDRAGIMAGE { sizeDragImage: SIZE { cx: w, cy: h }, ptOffset: POINT { x: w / 2, y: h / 2 }, hbmpDragImage: bitmap, crColorKey: COLORREF(CLR_NONE) };
    // SAFETY: 描述、数据对象活到调用结束；成功后位图归帮手（它删），失败时这里删
    unsafe {
        helper.InitializeFromBitmap(&shdi, data).inspect_err(|_| {
            let _ = DeleteObject(bitmap.into());
        })
    }
}

/// 缩小到 `size`（每个目标像素取它盖住的源像素的平均，按预乘的透明度算），返回直通透明度的 RGBA
fn shrink(image: &RgbaImage, [w, h]: [u32; 2]) -> Vec<u8> {
    let (sw, sh) = (image.width as usize, image.height as usize);
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1).min(sh));
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1).min(sw));
            let mut sum = [0u64; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &image.rgba[(sy * sw + sx) * 4..][..4];
                    let a = u64::from(p[3]);
                    sum[0] += u64::from(p[0]) * a;
                    sum[1] += u64::from(p[1]) * a;
                    sum[2] += u64::from(p[2]) * a;
                    sum[3] += a;
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u64;
            let o = &mut out[(y * w + x) * 4..][..4];
            for c in 0..3 {
                o[c] = sum[c].checked_div(sum[3]).unwrap_or(0) as u8;
            }
            o[3] = (sum[3] / count) as u8;
        }
    }
    out
}

/// 直通透明度的 RGBA 做成从上到下的 32 位位图（BGRA，预乘透明度：拖动图像要这样的）
fn premultiplied_bitmap(rgba: &[u8], [w, h]: [u32; 2]) -> windows::core::Result<HBITMAP> {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: -(h as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let info = BITMAPINFO { bmiHeader: header, ..Default::default() };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 描述在栈上、活到调用结束；成功时 `bits` 指向 `w × h × 4` 字节、归位图所有
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    // SAFETY: 位图刚建好，`bits` 有 `w × h × 4` 字节，只在这里写
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), rgba.len()) };
    for (to, from) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
        let a = u16::from(from[3]);
        let premultiply = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
        to.copy_from_slice(&[premultiply(from[2]), premultiply(from[1]), premultiply(from[0]), from[3]]);
    }
    Ok(bitmap)
}

/// 纯文本文件（.txt）的图标，`size` × `size` 物理像素，带透明度。路径不用存在：外壳按扩展名找图标
fn text_icon(size: i32) -> windows::core::Result<HBITMAP> {
    let path: Vec<u16> = r"C:\aureole.txt".encode_utf16().chain(Some(0)).collect();
    // SAFETY: 路径以 0 结尾、活到调用结束；编号由 `Pidl` 还给外壳，外壳的条目拷了一份
    unsafe {
        let pidl = Pidl(SHSimpleIDListFromPath(PCWSTR(path.as_ptr())));
        if pidl.0.is_null() {
            return Err(E_FAIL.into());
        }
        SHCreateItemFromIDList::<IShellItemImageFactory>(pidl.0)?.GetImage(SIZE { cx: size, cy: size }, SIIGBF_ICONONLY)
    }
}

/// 外壳的条目编号（绝对的 PIDL），释放时还给外壳
struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn of(path: &Path) -> Option<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: 路径以 0 结尾、活到调用结束；返回的编号由 Drop 用 ILFree 还掉
        let pidl = unsafe { ILCreateFromPathW(wide.as_ptr()) };
        (!pidl.is_null()).then(|| Self(pidl.cast()))
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: 编号是 ILCreateFromPathW 给的，只还一次
        unsafe { ILFree(self.0.cast()) };
    }
}

/// 外壳的数据对象：有文件时是这些条目的（`CF_HDROP` 和外壳认的那些格式，几个文件夹里的也行），只有文字时是空的
fn data_object(files: &[PathBuf]) -> windows::core::Result<IDataObject> {
    let pidls: Vec<Pidl> = files.iter().filter_map(|path| Pidl::of(path)).collect();
    if pidls.is_empty() {
        if !files.is_empty() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: 没有父文件夹、没有条目、没有内层对象：空的数据对象，之后往里放文字
        return unsafe { SHCreateDataObject(None, None, None::<&IDataObject>) };
    }
    let list: Vec<*const ITEMIDLIST> = pidls.iter().map(|pidl| pidl.0.cast_const()).collect();
    // SAFETY: 编号都是活着的绝对编号（`pidls` 拿着，活到函数结束，外壳拷了一份）
    unsafe {
        let items: IShellItemArray = SHCreateShellItemArrayFromIDLists(&list)?;
        items.BindToHandler(None::<&IBindCtx>, &BHID_DataObject)
    }
}

/// 往数据对象里放一份这种格式的内容（全局内存），内存交给数据对象
fn set_bytes(data: &IDataObject, format: u16, bytes: &[u8]) -> windows::core::Result<()> {
    let global = Global::new(bytes).ok_or_else(|| windows::core::Error::from(E_OUTOFMEMORY))?;
    let format = FORMATETC { cfFormat: format, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: HGLOBAL(global.handle()) }, pUnkForRelease: ManuallyDrop::new(None) };
    // SAFETY: 格式和内存都活到调用结束；`frelease` 为真：成功后内存归数据对象，失败时还归这里（`global` 释放时还掉）
    unsafe { data.SetData(&format, &medium, true)? };
    global.release();
    Ok(())
}

/// 拖放的源：左键松开就放下，按了 Esc 或者被取消就取消；光标用系统的
#[implement(IDropSource, Agile = false)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() || CANCEL.get() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
