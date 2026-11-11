//! Windows：拖放目标 `IDropTarget`。一个窗口只能登记一个，所以运行时关掉窗口库的文件拖放（`with_drag_and_drop(false)`），
//! 文件也从这里接，交给回调；运行时换成和窗口库一样的文件事件，文件拖放的行为不变。
//!
//! 窗口库的拖放目标只在进入时给位置（文件落在哪个元素上靠最后已知的指针位置）。这里每次都有位置，记下来给
//! [`drag_position`]，所以拖文件时悬停的元素是对的；离开、放下以后忘掉（之后的指针位置问系统）。
//!
//! 系统在注册窗口的线程（主线程）上调这些方法：别的程序在 `DoDragDrop` 里，消息从主线程的消息循环进来。
//! 拖放目标不是"随便哪个线程都能调"的对象（`Agile = false`）：同一个进程里别的线程（文件对话框）拖过来时，
//! 调用也送回主线程。
//!
//! 拖着经过窗口时把事件也交给外壳的 `IDropTargetHelper`：来源给了拖动的图像（资源管理器拖来的文件、自己拖出去的东西）时，
//! 图像由它画在指针旁边，经过这个窗口时也看得见。
//!
//! 操作按 Windows 的习惯：用户按着 Ctrl 是拷贝、Shift 是移动、Ctrl+Shift 或 Alt 是建链接，没按时元素说了算。
//! 放下时告诉来源的是回调说真正做了的；文件只交出了路径，没有真的挪走时不说"移动"（不然资源管理器会删掉原来的文件）。
//!
//! 登记不了拖放目标（这个线程的 OLE 初始化不了）时退回到系统的文件拖放（`DragAcceptFiles`、`WM_DROPFILES`）：
//! 只收得到文件，拖着经过时没有位置和光标的回答，放下时照样交给回调。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, POINT, POINTL, S_OK};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper};
use windows::core::Ref;
use windows_core::implement;
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;

use super::{ExternalDataKinds, ExternalDrag, ExternalDropData, ExternalDropEffect, OnExternalDrag, guarded};
use crate::PlatformError;
use crate::clipboard::dib;
use crate::clipboard::windows::{ansi_to_wide, custom_format, decode_png, global_bytes, paths_from_list, png_format, rtf_format};

thread_local! {
    /// 每个窗口这次拖放最后的位置（逻辑像素）
    static POSITIONS: RefCell<HashMap<isize, [f32; 2]>> = RefCell::new(HashMap::new());
    /// 退回到系统的文件拖放的窗口和它们的回调
    static FALLBACK: RefCell<HashMap<isize, OnExternalDrag>> = RefCell::new(HashMap::new());
}

/// 拖放时指针的位置：拖放期间窗口收不到指针移动事件，运行时每帧问一次。没在拖时是 `None`
pub fn drag_position(window: &dyn HasWindowHandle) -> Option<[f32; 2]> {
    let hwnd = hwnd_of(window)?;
    POSITIONS.with(|positions| positions.borrow().get(&(hwnd.0 as isize)).copied())
}

fn hwnd_of(window: &dyn HasWindowHandle) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// 窗口的缩放比例（每英寸点数 / 96）
fn scale_of(hwnd: HWND) -> f32 {
    // SAFETY: 窗口活着（关窗口前撤销了登记）
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

/// 向系统登记过的格式：别的程序拖链接时用的那两种
fn registered_format(name: &str, slot: &OnceLock<u16>) -> u16 {
    *slot.get_or_init(|| {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` 以 0 结尾，调用期间活着
        unsafe { RegisterClipboardFormatW(wide.as_ptr()) as u16 }
    })
}

fn link_format_wide() -> u16 {
    static FORMAT: OnceLock<u16> = OnceLock::new();
    registered_format("UniformResourceLocatorW", &FORMAT)
}

fn link_format_narrow() -> u16 {
    static FORMAT: OnceLock<u16> = OnceLock::new();
    registered_format("UniformResourceLocator", &FORMAT)
}

fn format_of(format: u16) -> FORMATETC {
    FORMATETC { cfFormat: format, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 }
}

fn has_format(data: &IDataObject, format: u16) -> bool {
    // SAFETY: 格式描述活到调用结束
    unsafe { data.QueryGetData(&format_of(format)) == S_OK }
}

/// 这种格式的内容，拷出来
fn read_bytes(data: &IDataObject, format: u16) -> Option<Vec<u8>> {
    // SAFETY: 格式描述活到调用结束；要的是全局内存（TYMED_HGLOBAL），给的不是时不读；用完配对释放
    unsafe {
        let mut medium = data.GetData(&format_of(format)).ok()?;
        let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 { global_bytes(medium.u.hGlobal.0) } else { None };
        ReleaseStgMedium(&mut medium);
        bytes
    }
}

/// UTF-16、以 0 结束的内容换成文字
fn wide_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|unit| u16::from_le_bytes(*unit)).take_while(|unit| *unit != 0).collect();
    String::from_utf16_lossy(&units)
}

fn kinds_of(data: &IDataObject) -> ExternalDataKinds {
    let custom = super::custom_formats().iter().any(|name| custom_format(name).is_some_and(|format| has_format(data, format as u16)));
    // 自己拖出去的会话（模态循环里）：同一时刻只有一次拖放，经过的就是它
    let own = crate::drag::dragging_out();
    if has_format(data, CF_HDROP.0) {
        return ExternalDataKinds { files: true, custom, own, ..ExternalDataKinds::default() };
    }
    ExternalDataKinds {
        text: has_format(data, CF_UNICODETEXT.0),
        link: has_format(data, link_format_wide()) || has_format(data, link_format_narrow()),
        image: has_format(data, png_format() as u16) || has_format(data, CF_DIBV5.0) || has_format(data, CF_DIB.0),
        files: false,
        rich_text: has_format(data, rtf_format() as u16),
        custom,
        own,
    }
}

fn files_of(data: &IDataObject) -> Vec<PathBuf> {
    read_bytes(data, CF_HDROP.0).map_or_else(Vec::new, |bytes| paths_from_list(&bytes))
}

fn data_of(data: &IDataObject, kinds: ExternalDataKinds) -> ExternalDropData {
    let link = || {
        read_bytes(data, link_format_wide()).map(|bytes| wide_text(&bytes)).or_else(|| {
            // 窄字符的链接按系统的代码页（中文系统上是 GBK）
            read_bytes(data, link_format_narrow()).map(|bytes| {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                OsString::from_wide(&ansi_to_wide(&bytes[..end])).to_string_lossy().into_owned()
            })
        })
    };
    // 图片优先 PNG（有的程序拖来的图片透明度只在它里面）
    let image = || {
        read_bytes(data, png_format() as u16)
            .and_then(|bytes| decode_png(&bytes))
            .or_else(|| read_bytes(data, CF_DIBV5.0).or_else(|| read_bytes(data, CF_DIB.0)).and_then(|bytes| dib::decode(&bytes)))
    };
    // 系统的内存块可能多出结尾的 0：RTF 到第一个 0 为止
    let rich = || {
        read_bytes(data, rtf_format() as u16).map(|mut bytes| {
            if let Some(end) = bytes.iter().position(|b| *b == 0) {
                bytes.truncate(end);
            }
            bytes
        })
    };
    let custom = || {
        super::custom_formats()
            .into_iter()
            .filter_map(|name| {
                let bytes = custom_format(&name).and_then(|format| read_bytes(data, format as u16))?;
                Some((name, bytes))
            })
            .collect()
    };
    ExternalDropData {
        text: kinds.text.then(|| read_bytes(data, CF_UNICODETEXT.0)).flatten().map(|bytes| wide_text(&bytes)),
        link: kinds.link.then(link).flatten(),
        image: kinds.image.then(image).flatten(),
        files: if kinds.files { files_of(data) } else { Vec::new() },
        rich_text: kinds.rich_text.then(rich).flatten(),
        custom: if kinds.custom { custom() } else { Vec::new() },
    }
}

/// 拷贝、移动、建链接（`DROPEFFECT` 的几位）
const COPY: u32 = 1;
const MOVE: u32 = 2;
const LINK: u32 = 4;
/// 拖放时按着 Alt（`MK_ALT`：只有 OLE 的拖放有这一位，`MODIFIERKEYS_FLAGS` 里没有）
const MK_ALT: u32 = 0x20;

fn effect_bits(effect: Option<ExternalDropEffect>) -> Option<u32> {
    effect.map(|effect| match effect {
        ExternalDropEffect::Copy => COPY,
        ExternalDropEffect::Move => MOVE,
        ExternalDropEffect::Link => LINK,
    })
}

/// 用户按着的键限定能做的操作（Windows 的习惯）：Ctrl 拷贝、Shift 移动、Ctrl+Shift 或 Alt 建链接；都没按时来源许的都行
fn keys_allow(allowed: u32, keys: u32) -> u32 {
    let (control, shift, alt) = (keys & MK_CONTROL.0 != 0, keys & MK_SHIFT.0 != 0, keys & MK_ALT != 0);
    let only = match (control, shift, alt) {
        (true, true, _) | (_, _, true) => LINK,
        (true, false, false) => COPY,
        (false, true, false) => MOVE,
        (false, false, false) => return allowed,
    };
    allowed & only
}

/// 想要的操作许可里有就用它，没有时按拷贝、移动、链接的顺序挑一个许的；不接受（`None`）、一个都不许时是 0
fn choose(wanted: Option<u32>, allowed: u32) -> u32 {
    let Some(wanted) = wanted else { return 0 };
    [wanted, COPY, MOVE, LINK].into_iter().find(|effect| allowed & effect != 0).unwrap_or(0)
}

/// 放下以后告诉来源这次算什么操作（移动的话来源会删掉原来的）：回调说真正做了的（`performed`），按来源许的、用户按着的键挑。
/// 文件只交出了路径：回调不说"移动"时（应用自己没挪走）不告诉来源移动，按着 Shift 也不行
fn dropped_effect(performed: Option<u32>, source: u32, keys: u32, files: bool) -> u32 {
    let Some(performed) = performed else { return 0 };
    if files {
        let allowed = if performed == MOVE { source } else { source & !MOVE };
        return choose(Some(performed), allowed);
    }
    choose(Some(performed), keys_allow(source, keys))
}

#[implement(IDropTarget, Agile = false)]
struct DropTarget {
    hwnd: HWND,
    on_drag: OnExternalDrag,
    /// 这次拖的内容
    kinds: Cell<ExternalDataKinds>,
    /// 外壳画拖动图像的帮手（建不了时没有图像，拖放照常）
    images: Option<IDropTargetHelper>,
}

impl DropTarget {
    /// 屏幕坐标换成窗口里的逻辑像素，记下来
    fn position(&self, point: &POINTL) -> [f32; 2] {
        let mut client = POINT { x: point.x, y: point.y };
        // SAFETY: 窗口活着（关窗口前撤销了登记）；`client` 活到调用结束
        let _ = unsafe { ScreenToClient(self.hwnd, &mut client) };
        let scale = scale_of(self.hwnd);
        let position = [client.x as f32 / scale, client.y as f32 / scale];
        POSITIONS.with(|positions| positions.borrow_mut().insert(self.hwnd.0 as isize, position));
        position
    }

    /// 拖放结束了：位置不再算数，之后的指针位置问系统
    fn forget_position(&self) {
        POSITIONS.with(|positions| positions.borrow_mut().remove(&(self.hwnd.0 as isize)));
    }

    /// 告诉外壳画拖动图像的帮手（没有帮手时什么都不做；它出错不影响拖放）
    fn images(&self, f: impl FnOnce(&IDropTargetHelper) -> windows::core::Result<()>) {
        if let Some(helper) = &self.images {
            let _ = f(helper);
        }
    }

    /// 回答换成系统的效果：用户按着的键限定、来源不许这种操作时换一种许的
    fn answer(&self, drag: ExternalDrag, keys: MODIFIERKEYS_FLAGS, effect: *mut DROPEFFECT) {
        let answer = guarded(None, || (self.on_drag)(drag));
        if effect.is_null() {
            return;
        }
        // SAFETY: 系统给的出入参数（上面查过不是空的），进来时是来源许的操作，出去时是接受的操作
        unsafe {
            let allowed = keys_allow((*effect).0, keys.0);
            *effect = DROPEFFECT(choose(effect_bits(answer), allowed));
        }
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let kinds = data.as_ref().map_or_else(ExternalDataKinds::default, kinds_of);
        self.kinds.set(kinds);
        let entered_files = if kinds.files { data.as_ref().map_or_else(Vec::new, files_of) } else { Vec::new() };
        let position = self.position(point);
        if kinds.is_empty() {
            refuse(effect);
        } else {
            self.answer(ExternalDrag::Over { kinds, position, entered_files }, keys, effect);
        }
        if let Some(data) = data.as_ref() {
            // SAFETY: 窗口活着；数据对象、位置活到调用结束
            self.images(|helper| unsafe { helper.DragEnter(self.hwnd, data, &screen(point), effect_of(effect)) });
        }
        Ok(())
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let kinds = self.kinds.get();
        let position = self.position(point);
        if kinds.is_empty() {
            refuse(effect);
        } else {
            self.answer(ExternalDrag::Over { kinds, position, entered_files: Vec::new() }, keys, effect);
        }
        // SAFETY: 位置活到调用结束
        self.images(|helper| unsafe { helper.DragOver(&screen(point), effect_of(effect)) });
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        // SAFETY: 没有参数
        self.images(|helper| unsafe { helper.DragLeave() });
        self.forget_position();
        if !self.kinds.replace(ExternalDataKinds::default()).is_empty() {
            guarded(None, || (self.on_drag)(ExternalDrag::Leave));
        }
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        // 先收起拖动的图像：回调里应用可能要弹东西、做一会儿
        if let Some(data) = data.as_ref() {
            // SAFETY: 数据对象、位置活到调用结束
            self.images(|helper| unsafe { helper.Drop(data, &screen(point), effect_of(effect)) });
        }
        let kinds = self.kinds.replace(ExternalDataKinds::default());
        let position = self.position(point);
        self.forget_position();
        let content = data.as_ref().map_or_else(ExternalDropData::default, |data| data_of(data, kinds));
        let performed = if kinds.is_empty() { None } else { guarded(None, || (self.on_drag)(ExternalDrag::Drop { data: content, position })) };
        if !effect.is_null() {
            // SAFETY: 系统给的出入参数（上面查过不是空的）：进来时是来源许的操作，出去时是这次真正做了的
            unsafe { *effect = DROPEFFECT(dropped_effect(effect_bits(performed), (*effect).0, keys.0, kinds.files)) };
        }
        Ok(())
    }
}

/// 不接受：效果写成"没有"（出参是空的时不写）
fn refuse(effect: *mut DROPEFFECT) {
    if !effect.is_null() {
        // SAFETY: 系统给的出参（上面查过不是空的）
        unsafe { effect.write(DROPEFFECT(0)) };
    }
}

/// 出入参数此刻的值（空的时是"没有"）
fn effect_of(effect: *mut DROPEFFECT) -> DROPEFFECT {
    if effect.is_null() {
        DROPEFFECT(0)
    } else {
        // SAFETY: 系统给的出入参数（上面查过不是空的）
        unsafe { effect.read() }
    }
}

/// 拖放给的屏幕坐标（物理像素）换成外壳要的类型
fn screen(point: &POINTL) -> POINT {
    POINT { x: point.x, y: point.y }
}

/// 开始接收拖来的内容。主线程上调用。登记不了拖放目标时退回到系统的文件拖放（只收文件，见模块说明）
pub fn listen(window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window).ok_or_else(|| PlatformError::System("不是 Windows 的窗口".into()))?;
    match register(hwnd, on_drag.clone()) {
        Ok(()) => Ok(()),
        Err(error) => {
            tracing::warn!("{error}：退回到系统的文件拖放，只收得到文件");
            accept_files(hwnd, on_drag).map_err(|fallback| PlatformError::System(format!("{error}；{fallback}").into()))
        }
    }
}

fn register(hwnd: HWND, on_drag: OnExternalDrag) -> Result<(), String> {
    // SAFETY: 参数是保留的空指针；同一个线程多次初始化是允许的（配对的 OleUninitialize 留给线程结束）
    unsafe { OleInitialize(None) }.map_err(|error| format!("初始化不了 OLE：{error}"))?;
    // SAFETY: 系统的拖放帮手（进程内的 COM 对象），这个线程上初始化过 OLE
    let images = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }
        .inspect_err(|error| tracing::debug!("建不了画拖动图像的帮手，拖进来时没有图像：{error}"))
        .ok();
    let target: IDropTarget = DropTarget { hwnd, on_drag, kinds: Cell::new(ExternalDataKinds::default()), images }.into();
    // SAFETY: 窗口活着，属于这个线程；COM 对象由系统增加引用，撤销登记时释放
    unsafe { RegisterDragDrop(hwnd, &target) }.map_err(|error| format!("登记不了拖放目标：{error}"))
}

/// 退回的文件拖放在窗口子类里的编号
const FALLBACK_SUBCLASS: usize = 0x4155_5244;

/// 系统的文件拖放：窗口收 `WM_DROPFILES`（挂一个窗口子类接它）
fn accept_files(hwnd: HWND, on_drag: OnExternalDrag) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::{DragAcceptFiles, SetWindowSubclass};
    let key = hwnd.0 as isize;
    FALLBACK.with(|fallback| fallback.borrow_mut().insert(key, on_drag));
    let raw = hwnd.0 as windows_sys::Win32::Foundation::HWND;
    // SAFETY: 窗口活着，属于这个线程；子类的回调签名和 SUBCLASSPROC 一致，窗口销毁（WM_NCDESTROY）或 `stop` 时摘掉
    unsafe {
        if SetWindowSubclass(raw, Some(drop_files_proc), FALLBACK_SUBCLASS, 0) == 0 {
            FALLBACK.with(|fallback| fallback.borrow_mut().remove(&key));
            return Err("挂不上接文件拖放的窗口子类".into());
        }
        DragAcceptFiles(raw, 1);
    }
    Ok(())
}

/// 退回的文件拖放的窗口子类：只接 `WM_DROPFILES`，别的照旧交下去
unsafe extern "system" fn drop_files_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    message: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
    id: usize,
    _data: usize,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_DROPFILES, WM_NCDESTROY};
    match message {
        WM_DROPFILES => {
            dropped_files(hwnd, wparam as windows_sys::Win32::UI::Shell::HDROP);
            0
        }
        WM_NCDESTROY => {
            FALLBACK.with(|fallback| fallback.borrow_mut().remove(&(hwnd as isize)));
            // SAFETY: 窗口在销毁，摘掉自己的子类，消息照旧交下去
            unsafe {
                RemoveWindowSubclass(hwnd, Some(drop_files_proc), id);
                DefSubclassProc(hwnd, message, wparam, lparam)
            }
        }
        // SAFETY: 别的消息原样交给下一层
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

/// 系统交来的一次文件放下：读出路径和位置、还掉句柄，按"经过、放下"交给回调
fn dropped_files(hwnd: windows_sys::Win32::Foundation::HWND, hdrop: windows_sys::Win32::UI::Shell::HDROP) {
    use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW, DragQueryPoint};
    let mut files = Vec::new();
    let mut point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    // SAFETY: 句柄是系统随 WM_DROPFILES 给的；每个路径先问长度（不含结尾的 0），缓冲区多留一个；最后还掉句柄
    unsafe {
        let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
        for index in 0..count {
            let len = DragQueryFileW(hdrop, index, std::ptr::null_mut(), 0);
            let mut buffer = vec![0u16; len as usize + 1];
            let copied = DragQueryFileW(hdrop, index, buffer.as_mut_ptr(), len + 1) as usize;
            if copied > 0 {
                files.push(PathBuf::from(OsString::from_wide(&buffer[..copied.min(len as usize)])));
            }
        }
        DragQueryPoint(hdrop, &mut point);
        DragFinish(hdrop);
    }
    let scale = scale_of(HWND(hwnd));
    let position = [point.x as f32 / scale, point.y as f32 / scale];
    let on_drag = FALLBACK.with(|fallback| fallback.borrow().get(&(hwnd as isize)).cloned());
    if let Some(on_drag) = on_drag
        && !files.is_empty()
    {
        let kinds = ExternalDataKinds { files: true, ..ExternalDataKinds::default() };
        guarded(None, || on_drag(ExternalDrag::Over { kinds, position, entered_files: files.clone() }));
        guarded(None, || on_drag(ExternalDrag::Drop { data: ExternalDropData { files, ..ExternalDropData::default() }, position }));
    }
}

/// 不再接收（关窗口前）
pub fn stop(window: &dyn HasWindowHandle) {
    let Some(hwnd) = hwnd_of(window) else { return };
    // SAFETY: 窗口活着，属于这个线程；没登记过时只是返回错误
    let _ = unsafe { RevokeDragDrop(hwnd) };
    let key = hwnd.0 as isize;
    POSITIONS.with(|positions| positions.borrow_mut().remove(&key));
    if FALLBACK.with(|fallback| fallback.borrow_mut().remove(&key)).is_some() {
        use windows_sys::Win32::UI::Shell::{DragAcceptFiles, RemoveWindowSubclass};
        let raw = hwnd.0 as windows_sys::Win32::Foundation::HWND;
        // SAFETY: 窗口活着，属于这个线程；子类是 `accept_files` 挂的
        unsafe {
            DragAcceptFiles(raw, 0);
            RemoveWindowSubclass(raw, Some(drop_files_proc), FALLBACK_SUBCLASS);
        }
    }
}
