//! Windows：拖放目标 `IDropTarget`。一个窗口只能登记一个，所以运行时关掉窗口库的文件拖放（`with_drag_and_drop(false)`），
//! 文件也从这里接，交给回调；运行时换成和窗口库一样的文件事件，文件拖放的行为不变。
//!
//! 窗口库的拖放目标只在进入时给位置（文件落在哪个元素上靠最后已知的指针位置）。这里每次都有位置，记下来给
//! [`drag_position`]，所以拖文件时悬停的元素是对的。
//!
//! 系统在注册窗口的线程（主线程）上调这些方法：别的程序在 `DoDragDrop` 里，消息从主线程的消息循环进来。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, POINT, POINTL, S_OK};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget,
    IDropTarget_Impl, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::core::Ref;
use windows_core::implement;
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;

use super::{ExternalDataKinds, ExternalDrag, ExternalDropData, ExternalDropEffect, OnExternalDrag, guarded};
use crate::PlatformError;
use crate::clipboard::dib;
use crate::clipboard::windows::{global_bytes, paths_from_list};

thread_local! {
    /// 每个窗口这次拖放最后的位置（逻辑像素）
    static POSITIONS: RefCell<HashMap<isize, [f32; 2]>> = RefCell::new(HashMap::new());
}

/// 拖放时指针的位置：拖放期间窗口收不到指针移动事件，运行时每帧问一次
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
    // SAFETY: 格式描述活到调用结束；要的是全局内存（TYMED_HGLOBAL），所以联合体里是 hGlobal；用完配对释放
    unsafe {
        let mut medium = data.GetData(&format_of(format)).ok()?;
        let bytes = global_bytes(medium.u.hGlobal.0);
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
    if has_format(data, CF_HDROP.0) {
        return ExternalDataKinds { files: true, ..ExternalDataKinds::default() };
    }
    ExternalDataKinds {
        text: has_format(data, CF_UNICODETEXT.0),
        link: has_format(data, link_format_wide()) || has_format(data, link_format_narrow()),
        image: has_format(data, CF_DIBV5.0) || has_format(data, CF_DIB.0),
        files: false,
    }
}

fn files_of(data: &IDataObject) -> Vec<PathBuf> {
    read_bytes(data, CF_HDROP.0).map_or_else(Vec::new, |bytes| paths_from_list(&bytes))
}

fn data_of(data: &IDataObject, kinds: ExternalDataKinds) -> ExternalDropData {
    let link = || {
        read_bytes(data, link_format_wide()).map(|bytes| wide_text(&bytes)).or_else(|| {
            read_bytes(data, link_format_narrow()).map(|bytes| {
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                OsString::from_wide(&bytes[..end].iter().map(|b| u16::from(*b)).collect::<Vec<_>>()).to_string_lossy().into_owned()
            })
        })
    };
    ExternalDropData {
        text: kinds.text.then(|| read_bytes(data, CF_UNICODETEXT.0)).flatten().map(|bytes| wide_text(&bytes)),
        link: kinds.link.then(link).flatten(),
        image: kinds.image.then(|| read_bytes(data, CF_DIBV5.0).or_else(|| read_bytes(data, CF_DIB.0)).and_then(|bytes| dib::decode(&bytes))).flatten(),
        files: if kinds.files { files_of(data) } else { Vec::new() },
    }
}

#[implement(IDropTarget)]
struct DropTarget {
    hwnd: HWND,
    on_drag: OnExternalDrag,
    /// 这次拖的内容
    kinds: Cell<ExternalDataKinds>,
    /// 最后一次回答的操作：放下时告诉来源
    last_effect: Cell<DROPEFFECT>,
}

impl DropTarget {
    /// 屏幕坐标换成窗口里的逻辑像素，记下来
    fn position(&self, point: &POINTL) -> [f32; 2] {
        let mut client = POINT { x: point.x, y: point.y };
        // SAFETY: 窗口活着（关窗口前撤销了登记）；`client` 活到调用结束
        let _ = unsafe { ScreenToClient(self.hwnd, &mut client) };
        // SAFETY: 窗口活着
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let position = [client.x as f32 / scale, client.y as f32 / scale];
        POSITIONS.with(|positions| positions.borrow_mut().insert(self.hwnd.0 as isize, position));
        position
    }

    /// 回答换成系统的效果：来源不许这种操作时换一种它许的
    fn answer(&self, drag: ExternalDrag, effect: *mut DROPEFFECT) {
        let answer = guarded(None, || (self.on_drag)(drag));
        if effect.is_null() {
            return;
        }
        // SAFETY: 系统给的出入参数（上面查过不是空的），进来时是来源许的操作，出去时是接受的操作
        unsafe {
            let allowed = *effect;
            let wanted = match answer {
                Some(ExternalDropEffect::Copy) => DROPEFFECT_COPY,
                Some(ExternalDropEffect::Move) => DROPEFFECT_MOVE,
                Some(ExternalDropEffect::Link) => DROPEFFECT_LINK,
                None => DROPEFFECT_NONE,
            };
            let chosen = if answer.is_none() {
                DROPEFFECT_NONE
            } else {
                [wanted, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_LINK].into_iter().find(|choice| allowed.contains(*choice)).unwrap_or(DROPEFFECT_NONE)
            };
            self.last_effect.set(chosen);
            *effect = chosen;
        }
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let kinds = data.as_ref().map_or_else(ExternalDataKinds::default, kinds_of);
        self.kinds.set(kinds);
        let entered_files = if kinds.files { data.as_ref().map_or_else(Vec::new, files_of) } else { Vec::new() };
        let position = self.position(point);
        if kinds.is_empty() {
            // SAFETY: 系统给的出参
            unsafe { effect.write(DROPEFFECT_NONE) };
            return Ok(());
        }
        self.answer(ExternalDrag::Over { kinds, position, entered_files }, effect);
        Ok(())
    }

    fn DragOver(&self, _keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let kinds = self.kinds.get();
        let position = self.position(point);
        if kinds.is_empty() {
            // SAFETY: 系统给的出参
            unsafe { effect.write(DROPEFFECT_NONE) };
            return Ok(());
        }
        self.answer(ExternalDrag::Over { kinds, position, entered_files: Vec::new() }, effect);
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if !self.kinds.replace(ExternalDataKinds::default()).is_empty() {
            guarded(None, || (self.on_drag)(ExternalDrag::Leave));
        }
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _keys: MODIFIERKEYS_FLAGS, point: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let kinds = self.kinds.replace(ExternalDataKinds::default());
        let position = self.position(point);
        let content = data.as_ref().map_or_else(ExternalDropData::default, |data| data_of(data, kinds));
        guarded(None, || (self.on_drag)(ExternalDrag::Drop { data: content, position }));
        // 告诉来源这次算什么操作（移动的话它会删掉原来的）：就是最后一次经过时回答的
        if !effect.is_null() {
            // SAFETY: 系统给的出参（上面查过不是空的）
            unsafe { effect.write(self.last_effect.get()) };
        }
        Ok(())
    }
}

/// 开始接收拖来的内容。主线程上调用
pub fn listen(window: &dyn HasWindowHandle, on_drag: OnExternalDrag) -> Result<(), PlatformError> {
    let hwnd = hwnd_of(window).ok_or_else(|| PlatformError::System("不是 Windows 的窗口".into()))?;
    // SAFETY: 参数是保留的空指针；同一个线程多次初始化是允许的（配对的 OleUninitialize 留给线程结束）
    unsafe { OleInitialize(None) }.map_err(|error| PlatformError::System(format!("初始化不了 OLE：{error}").into()))?;
    let target: IDropTarget = DropTarget { hwnd, on_drag, kinds: Cell::new(ExternalDataKinds::default()), last_effect: Cell::new(DROPEFFECT_NONE) }.into();
    // SAFETY: 窗口活着，属于这个线程；COM 对象由系统增加引用，撤销登记时释放
    unsafe { RegisterDragDrop(hwnd, &target) }.map_err(|error| PlatformError::System(format!("登记不了拖放目标：{error}").into()))
}

/// 不再接收（关窗口前）
pub fn stop(window: &dyn HasWindowHandle) {
    let Some(hwnd) = hwnd_of(window) else { return };
    // SAFETY: 窗口活着，属于这个线程；没登记过时只是返回错误
    let _ = unsafe { RevokeDragDrop(hwnd) };
    POSITIONS.with(|positions| positions.borrow_mut().remove(&(hwnd.0 as isize)));
}
