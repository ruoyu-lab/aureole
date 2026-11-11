//! 字体：从文件或内存加载自定义字体（TTF、OTF、TTC，以及压缩过的 WOFF2、WOFF），再用 [`crate::Element::font_family`] 按名字使用。
//!
//! ```no_run
//! let families = aureole::font::load_file("assets/LXGWWenKai.ttf").expect("加载字体失败");
//! // families == ["LXGW WenKai"]
//! # use aureole::prelude::*;
//! text("落霞与孤鹜齐飞").font_family("LXGW WenKai");
//! ```
//!
//! 也可以写逗号隔开的几个名字（`.font_family("Monaco, Menlo")`），前面的优先。
//!
//! 加载的字体登记在一张全局的表里：每棵元素树（每个窗口）的字体库在下一帧开始前补登记，
//! 已经排好的文字随即重新排版，所以先显示、后加载字体也没问题。指定的字体里没有的字（比如英文字体里的中文）
//! 回退到系统字体。可以在任何线程加载（例如在 `spawn_blocking` 里读文件）。
//!
//! WOFF2、WOFF 要打开 `woff2` 特性（默认关，没打开时返回 `InvalidData`），先解压成 TTF / OTF 再登记：WOFF2 是 Brotli 压缩，`glyf` / `loca` 还要按
//! 压缩时的变换还原，几千个汉字的子集解压要二三十毫秒，大字体最好在后台线程加载。

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use parley::fontique::{Blob, Collection, CollectionOptions};

/// 加载过的字体数据，按加载顺序
static LOADED: Mutex<Vec<Blob<u8>>> = Mutex::new(Vec::new());
/// 加载过几份字体：元素树比较它和自己登记过的份数，决定要不要补登记
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// 用到过的字体族名，文字样式里存下标 + 1（0 是系统界面字体），样式保持 `Copy`、比较只比一个整数
static FAMILIES: Mutex<Vec<Arc<str>>> = Mutex::new(Vec::new());

/// 从内存加载字体（TTF、OTF、含多个字体的 TTC，或者 WOFF2、WOFF），返回其中的字体族名。
/// 数据不是能识别的字体（或者 WOFF2 解压失败）时返回 `InvalidData` 错误
pub fn load(data: impl Into<Vec<u8>>) -> io::Result<Vec<String>> {
    let blob = Blob::from(sfnt(data.into())?);
    // 先登记进一个不含系统字体的临时字体库：读出族名，顺便检查数据是不是字体
    let mut probe = Collection::new(CollectionOptions { shared: false, system_fonts: false });
    let mut names: Vec<String> = Vec::new();
    for (id, _) in probe.register_fonts(blob.clone(), None) {
        if let Some(name) = probe.family_name(id)
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    if names.is_empty() {
        return Err(invalid("不是能识别的字体（支持 TTF、OTF、TTC、WOFF2、WOFF）"));
    }
    LOADED.lock().expect("字体表被污染").push(blob);
    COUNT.fetch_add(1, Ordering::Release);
    Ok(names)
}

/// WOFF2（文件头 `wOF2`）、WOFF（`wOFF`）解压成 TTF / OTF；别的数据原样返回，由字体库判断是不是字体
fn sfnt(data: Vec<u8>) -> io::Result<Vec<u8>> {
    match data.get(..4) {
        #[cfg(feature = "woff2")]
        Some(b"wOF2") => wuff::decompress_woff2(&data).map_err(|_| invalid("WOFF2 解压失败")),
        #[cfg(feature = "woff2")]
        Some(b"wOFF") => wuff::decompress_woff1(&data).map_err(|_| invalid("WOFF 解压失败")),
        #[cfg(not(feature = "woff2"))]
        Some(b"wOF2" | b"wOFF") => Err(invalid("WOFF2、WOFF 要打开 `woff2` 特性")),
        _ => Ok(data),
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// 从文件加载字体，见 [`load`]
pub fn load_file(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    load(std::fs::read(path)?)
}

/// 加载过几份字体
pub(crate) fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

/// 第 `from` 份之后加载的字体
pub(crate) fn loaded_since(from: usize) -> Vec<Blob<u8>> {
    LOADED.lock().expect("字体表被污染").iter().skip(from).cloned().collect()
}

/// 字体族名的编号（0 留给系统界面字体）
pub(crate) fn family_id(name: &str) -> u32 {
    let mut families = FAMILIES.lock().expect("字体族表被污染");
    match families.iter().position(|f| &**f == name) {
        Some(index) => index as u32 + 1,
        None => {
            families.push(name.into());
            families.len() as u32
        }
    }
}

/// 编号对应的字体族名；0 是系统界面字体，返回 `None`
pub(crate) fn family_name(id: u32) -> Option<Arc<str>> {
    let index = id.checked_sub(1)? as usize;
    FAMILIES.lock().expect("字体族表被污染").get(index).cloned()
}
