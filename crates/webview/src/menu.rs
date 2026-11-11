//! 右键菜单和编辑命令。
//!
//! 离屏的网页没有自己的窗口，内核的原生菜单弹不到对的地方，所以内核的菜单一律不弹。应用接手时
//! （[`crate::WebController::on_context_menu`]）把点在哪、点到了什么交给它（[`ContextMenu`]），菜单由应用画，
//! 菜单项用控制器上的编辑命令（复制、粘贴……落到这里的 [`edit`]）、下载、开发者工具实现。

use std::sync::Arc;

use cef::sys::{cef_context_menu_edit_state_flags_t as EditState, cef_context_menu_type_flags_t as TypeFlags};
use cef::{CefString, CefStringList, ContextMenuMediaType, ContextMenuParams, ImplBrowser, ImplBrowserHost, ImplContextMenuParams, ImplFrame};

use crate::state::{Hook, Page, Shared};

/// 右键点到的媒体
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaKind {
    /// 不是媒体
    #[default]
    None,
    /// 图片
    Image,
    /// 视频
    Video,
    /// 音频
    Audio,
    /// `<canvas>`
    Canvas,
    /// 文件（直接打开的文件）
    File,
    /// 插件（PDF 这类）
    Plugin,
}

/// 这时能做哪些编辑（按页面此刻的状态：有没有选中、能不能粘贴……）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EditFlags {
    /// 能撤销
    pub can_undo: bool,
    /// 能重做
    pub can_redo: bool,
    /// 能剪切（输入框里有选中的文字）
    pub can_cut: bool,
    /// 能复制（有选中的内容）
    pub can_copy: bool,
    /// 能粘贴（在输入框里，剪贴板里有东西）
    pub can_paste: bool,
    /// 能删除
    pub can_delete: bool,
    /// 能全选
    pub can_select_all: bool,
}

/// 页面里点了右键（[`crate::WebController::on_context_menu`]）：点在哪、点到了什么
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ContextMenu {
    /// 点在哪：网页视图内容区里的位置，逻辑像素
    pub position: [f32; 2],
    /// 点到的链接的地址（不在链接上时是空的）
    pub link_url: String,
    /// 点到的媒体
    pub media: MediaKind,
    /// 点到的媒体的地址（图片、视频的 `src`）
    pub source_url: String,
    /// 选中的文字
    pub selection: String,
    /// 点到的元素的 `title`，图片的话是它的 `alt`
    pub title: String,
    /// 页面的地址
    pub page_url: String,
    /// 点到的那个框架的地址（不在嵌进来的框架里时和页面的一样）
    pub frame_url: String,
    /// 点在能编辑的地方（输入框、`contenteditable`）
    pub editable: bool,
    /// 这时能做哪些编辑
    pub edit: EditFlags,
    /// 点到的拼错的词（没点在拼错的词上时是空的）
    pub misspelled_word: String,
    /// 拼写检查给的建议，用 [`crate::WebController::replace_misspelling`] 换掉拼错的词
    pub suggestions: Vec<String>,
}

/// 编辑命令
#[derive(Clone, Copy, Debug)]
pub(crate) enum Edit {
    Copy,
    Cut,
    Paste,
    PastePlain,
    Delete,
    SelectAll,
    Undo,
    Redo,
}

/// 内核的值换成应用看的。CEF 的界面线程上调用：`params` 只在回调里有效
pub(crate) fn read(params: &ContextMenuParams) -> ContextMenu {
    let string = |value: cef::CefStringUserfree| CefString::from(&value).to_string();
    let types = params.type_flags();
    let types: TypeFlags = types.into();
    let state: EditState = params.edit_state_flags().into();
    let can = |flag: EditState| state & flag == flag;
    let mut suggestions = CefStringList::new();
    params.dictionary_suggestions(Some(&mut suggestions));
    let media = params.media_type();
    ContextMenu {
        position: [params.xcoord() as f32, params.ycoord() as f32],
        link_url: if types & TypeFlags::CM_TYPEFLAG_LINK == TypeFlags::CM_TYPEFLAG_LINK { string(params.link_url()) } else { String::new() },
        media: media_kind(media),
        source_url: string(params.source_url()),
        selection: string(params.selection_text()),
        title: string(params.title_text()),
        page_url: string(params.page_url()),
        frame_url: string(params.frame_url()),
        editable: params.is_editable() != 0,
        edit: EditFlags {
            can_undo: can(EditState::CM_EDITFLAG_CAN_UNDO),
            can_redo: can(EditState::CM_EDITFLAG_CAN_REDO),
            can_cut: can(EditState::CM_EDITFLAG_CAN_CUT),
            can_copy: can(EditState::CM_EDITFLAG_CAN_COPY),
            can_paste: can(EditState::CM_EDITFLAG_CAN_PASTE),
            can_delete: can(EditState::CM_EDITFLAG_CAN_DELETE),
            can_select_all: can(EditState::CM_EDITFLAG_CAN_SELECT_ALL),
        },
        misspelled_word: string(params.misspelled_word()),
        suggestions: suggestions.into_iter().collect(),
    }
}

fn media_kind(media: ContextMenuMediaType) -> MediaKind {
    match media {
        ContextMenuMediaType::IMAGE => MediaKind::Image,
        ContextMenuMediaType::VIDEO => MediaKind::Video,
        ContextMenuMediaType::AUDIO => MediaKind::Audio,
        ContextMenuMediaType::CANVAS => MediaKind::Canvas,
        ContextMenuMediaType::FILE => MediaKind::File,
        ContextMenuMediaType::PLUGIN => MediaKind::Plugin,
        _ => MediaKind::None,
    }
}

/// CEF 的界面线程上：页面里点了右键。应用接手时把读出来的交给它（主线程上）；内核自己的菜单一律不弹
pub(crate) fn requested(page: &Arc<Page>, params: Option<&ContextMenuParams>) {
    if !page.has_hook(Hook::ContextMenu) {
        return;
    }
    let Some(params) = params else { return };
    let menu = read(params);
    page.on_main(move |shared| {
        let hook = shared.hooks.borrow().context_menu.clone();
        if let Some(hook) = hook {
            hook(menu);
        }
    });
}

/// 打开着的网页
fn browser(shared: &Shared) -> Option<cef::Browser> {
    shared.browser.borrow().clone()
}

/// 编辑命令：对有焦点的框架（选中的内容、输入框可能在嵌进来的框架里），没有时对主框架
pub(crate) fn edit(shared: &Shared, command: Edit) {
    let Some(frame) = browser(shared).and_then(|browser| browser.focused_frame().or_else(|| browser.main_frame())) else { return };
    match command {
        Edit::Copy => frame.copy(),
        Edit::Cut => frame.cut(),
        Edit::Paste => frame.paste(),
        Edit::PastePlain => frame.paste_and_match_style(),
        Edit::Delete => frame.del(),
        Edit::SelectAll => frame.select_all(),
        Edit::Undo => frame.undo(),
        Edit::Redo => frame.redo(),
    }
}

pub(crate) fn replace_misspelling(shared: &Shared, word: &str) {
    if let Some(host) = browser(shared).and_then(|browser| browser.host()) {
        host.replace_misspelling(Some(&CefString::from(word)));
    }
}

pub(crate) fn add_to_dictionary(shared: &Shared, word: &str) {
    if let Some(host) = browser(shared).and_then(|browser| browser.host()) {
        host.add_word_to_dictionary(Some(&CefString::from(word)));
    }
}

/// 下载一个地址：和页面里开始的下载一样经过下载的回调（应用的 `on_download`、存到哪里、进度）
pub(crate) fn download(shared: &Shared, url: &str) {
    if let Some(host) = browser(shared).and_then(|browser| browser.host()) {
        host.start_download(Some(&CefString::from(url)));
    }
}
