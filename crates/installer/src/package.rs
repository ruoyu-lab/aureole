//! 安装包的格式：一个可执行文件，后面接着压缩的文件和清单。
//!
//! ```text
//! [安装程序本身][文件 1 的 deflate 数据][文件 2 ...][清单 JSON][文件尾 40 字节]
//! ```
//!
//! 文件尾是魔数、程序本身的长度、清单的位置和长度。接在可执行文件后面不影响它运行，系统只按文件头里写的范围加载。
//! 这样安装程序只要编一次，换应用时不用重新编译；要把应用的文件也编进程序的话，每打一次包都要重新链接。
//! 用 deflate（miniz_oxide，纯 Rust）：比 zstd 压得差一点，但不用 C 编译器，也不多依赖。

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use crate::manifest::{FileEntry, Manifest};
use crate::{CancelToken, Error};

const MAGIC: &[u8; 16] = b"AUREOLE-SETUP-01";
const FOOTER_LEN: u64 = 16 + 8 * 3;

/// 打包：把程序、文件、清单拼成一个安装包
pub struct Builder {
    manifest: Manifest,
    files: Vec<(String, PathBuf, bool)>,
}

impl Builder {
    /// `manifest` 里的 `files` 由 [`Self::add`] 填
    pub fn new(manifest: Manifest) -> Self {
        Self { manifest, files: Vec::new() }
    }

    /// 加一个文件：`path` 是相对于安装目录的路径（分隔符 `/`），`source` 是现在在哪
    pub fn add(&mut self, path: impl Into<String>, source: impl Into<PathBuf>, executable: bool) {
        self.files.push((path.into(), source.into(), executable));
    }

    /// 递归加一个文件夹
    pub fn add_directory(&mut self, path: &str, source: &Path) -> Result<(), Error> {
        let mut entries = std::fs::read_dir(source).map_err(|e| Error::io(source, e))?.collect::<Result<Vec<_>, _>>().map_err(|e| Error::io(source, e))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let inner = format!("{path}/{name}");
            if entry.path().is_dir() {
                self.add_directory(&inner, &entry.path())?;
            } else {
                self.add(inner, entry.path(), false);
            }
        }
        Ok(())
    }

    /// 写出安装包：`stub` 是安装程序本身
    pub fn write(mut self, stub: &Path, out: &Path) -> Result<(), Error> {
        let file = File::create(out).map_err(|e| Error::io(out, e))?;
        let mut writer = BufWriter::new(file);
        let mut position = io::copy(&mut File::open(stub).map_err(|e| Error::io(stub, e))?, &mut writer).map_err(|e| Error::io(stub, e))?;
        let stub_len = position;
        let mut entries = Vec::with_capacity(self.files.len());
        for (path, source, executable) in std::mem::take(&mut self.files) {
            let data = std::fs::read(&source).map_err(|e| Error::io(&source, e))?;
            let mut hasher = flate2::Crc::new();
            hasher.update(&data);
            let mut encoder = DeflateEncoder::new(&mut writer, Compression::best());
            encoder.write_all(&data).map_err(|e| Error::io(out, e))?;
            let mut counted = 0;
            encoder.try_finish().map_err(|e| Error::io(out, e))?;
            counted += encoder.total_out();
            drop(encoder);
            entries.push(FileEntry { path, size: data.len() as u64, offset: position, compressed: counted, crc32: hasher.sum(), executable });
            position += counted;
        }
        self.manifest.files = entries;
        let manifest = self.manifest.to_json();
        writer.write_all(manifest.as_bytes()).map_err(|e| Error::io(out, e))?;
        write_footer(&mut writer, stub_len, position, manifest.len() as u64).map_err(|e| Error::io(out, e))?;
        writer.flush().map_err(|e| Error::io(out, e))?;
        drop(writer);
        make_executable(out);
        Ok(())
    }
}

fn write_footer(writer: &mut impl Write, stub_len: u64, manifest_offset: u64, manifest_len: u64) -> io::Result<()> {
    writer.write_all(MAGIC)?;
    writer.write_all(&stub_len.to_le_bytes())?;
    writer.write_all(&manifest_offset.to_le_bytes())?;
    writer.write_all(&manifest_len.to_le_bytes())
}

/// 给文件加上执行权限（Windows 上没有这回事）
pub(crate) fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut permissions = meta.permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// 打开的安装包
#[derive(Clone, Debug)]
pub struct Package {
    path: PathBuf,
    stub_len: u64,
    manifest: Manifest,
}

impl Package {
    /// 这个程序自己带的安装包
    pub fn current() -> Result<Self, Error> {
        let exe = std::env::current_exe().map_err(|e| Error::NotAPackage(format!("找不到程序自己：{e}")))?;
        Self::open(&exe)
    }

    /// 打开一个安装包
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
        let len = file.metadata().map_err(|e| Error::io(path, e))?.len();
        let not_a_package = || Error::NotAPackage(format!("{} 后面没有接着安装包（要用 cargo bundle --format installer 打）", path.display()));
        if len < FOOTER_LEN {
            return Err(not_a_package());
        }
        file.seek(SeekFrom::Start(len - FOOTER_LEN)).map_err(|e| Error::io(path, e))?;
        let mut footer = [0u8; FOOTER_LEN as usize];
        file.read_exact(&mut footer).map_err(|e| Error::io(path, e))?;
        if &footer[..16] != MAGIC {
            return Err(not_a_package());
        }
        let number = |at: usize| u64::from_le_bytes(footer[at..at + 8].try_into().expect("正好 8 字节"));
        let (stub_len, offset, manifest_len) = (number(16), number(24), number(32));
        if offset.checked_add(manifest_len) != Some(len - FOOTER_LEN) || stub_len > offset {
            return Err(Error::NotAPackage(format!("{} 的文件尾对不上，文件可能不完整", path.display())));
        }
        file.seek(SeekFrom::Start(offset)).map_err(|e| Error::io(path, e))?;
        let mut text = vec![0; manifest_len as usize];
        file.read_exact(&mut text).map_err(|e| Error::io(path, e))?;
        let text = String::from_utf8(text).map_err(|_| Error::NotAPackage("清单不是 UTF-8".into()))?;
        Ok(Self { path: path.to_path_buf(), stub_len, manifest: Manifest::from_json(&text)? })
    }

    /// 没有文件的包：写界面时预览、测试用
    pub fn preview(manifest: Manifest) -> Self {
        Self { path: PathBuf::new(), stub_len: 0, manifest }
    }

    /// 是 [`Self::preview`] 建的
    pub fn is_preview(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// 清单
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// 解压一个文件到 `writer`，每写一块调用 `written`（解压出来的字节数）。校验和不对时报错；`cancel` 置位时中途停下
    pub fn extract(&self, entry: &FileEntry, writer: &mut impl Write, cancel: &CancelToken, mut written: impl FnMut(u64)) -> Result<(), Error> {
        let mut file = File::open(&self.path).map_err(|e| Error::io(&self.path, e))?;
        file.seek(SeekFrom::Start(entry.offset)).map_err(|e| Error::io(&self.path, e))?;
        let mut decoder = DeflateDecoder::new(file.take(entry.compressed));
        let mut hasher = flate2::Crc::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let n = decoder.read(&mut buffer).map_err(|_| Error::Corrupt(entry.path.clone()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            writer.write_all(&buffer[..n]).map_err(|e| Error::io(Path::new(&entry.path), e))?;
            total += n as u64;
            written(n as u64);
        }
        if total != entry.size || hasher.sum() != entry.crc32 {
            return Err(Error::Corrupt(entry.path.clone()));
        }
        Ok(())
    }

    /// 写出卸载程序：程序本身加上去掉文件的清单。卸载时靠它，不用留着整个安装包
    pub fn write_uninstaller(&self, out: &Path) -> Result<(), Error> {
        let mut source = File::open(&self.path).map_err(|e| Error::io(&self.path, e))?;
        let mut writer = BufWriter::new(File::create(out).map_err(|e| Error::io(out, e))?);
        let copied = io::copy(&mut (&mut source).take(self.stub_len), &mut writer).map_err(|e| Error::io(out, e))?;
        let manifest = Manifest { files: Vec::new(), uninstaller: true, ..self.manifest.clone() }.to_json();
        writer.write_all(manifest.as_bytes()).map_err(|e| Error::io(out, e))?;
        write_footer(&mut writer, copied, copied, manifest.len() as u64).map_err(|e| Error::io(out, e))?;
        writer.flush().map_err(|e| Error::io(out, e))?;
        drop(writer);
        make_executable(out);
        Ok(())
    }
}
