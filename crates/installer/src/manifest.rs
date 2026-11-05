//! 安装包里写的东西：应用是谁、有哪些文件、要在系统里登记什么。
//!
//! 打包时写进安装包（JSON），安装时读。用 JSON 不用二进制：出了问题能直接看，新版本加字段时旧的安装程序不受影响。

use serde_json::{Value, json};

use crate::Error;

/// 能打开的一种文档
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentType {
    /// 给人看的名字
    pub name: String,
    /// 扩展名（不带点）
    pub extensions: Vec<String>,
    /// MIME 类型（Linux 用）
    pub mime: Option<String>,
    /// 应用自己的类型：默认用这个应用打开。系统已有的类型不抢，只进"打开方式"
    pub owner: bool,
}

/// 安装包里的一个文件
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEntry {
    /// 相对于安装目录的路径，分隔符是 `/`
    pub path: String,
    /// 解压之后的大小
    pub size: u64,
    /// 在包里的位置（从整个文件的开头算）
    pub offset: u64,
    /// 压缩之后的大小
    pub compressed: u64,
    /// 解压之后的校验和
    pub crc32: u32,
    /// 要有执行权限（Linux）
    pub executable: bool,
}

/// 安装包的清单
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// 给人看的应用名，可以是中文
    pub name: String,
    /// 包标识符：卸载信息、快捷方式的应用标识、协议和文档类型的登记都用它
    pub identifier: String,
    /// 版本
    pub version: String,
    /// 发布者（"应用和功能"里显示）
    pub publisher: String,
    /// 主程序：相对于安装目录的路径
    pub executable: String,
    /// 图标：相对于安装目录的路径（Windows 是 .ico，Linux 是 PNG）
    pub icon: Option<String>,
    /// 能打开的协议
    pub url_schemes: Vec<String>,
    /// 能打开的文档类型
    pub documents: Vec<DocumentType>,
    /// 文件
    pub files: Vec<FileEntry>,
    /// 这是卸载程序（只有程序本身和这份清单，没有应用的文件）
    pub uninstaller: bool,
}

impl Manifest {
    /// 解压之后一共多大
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// 写成 JSON
    pub fn to_json(&self) -> String {
        let documents: Vec<Value> = self.documents.iter().map(|d| json!({ "name": d.name, "extensions": d.extensions, "mime": d.mime, "owner": d.owner })).collect();
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|f| json!({ "path": f.path, "size": f.size, "offset": f.offset, "compressed": f.compressed, "crc32": f.crc32, "executable": f.executable }))
            .collect();
        json!({
            "name": self.name,
            "identifier": self.identifier,
            "version": self.version,
            "publisher": self.publisher,
            "executable": self.executable,
            "icon": self.icon,
            "url_schemes": self.url_schemes,
            "documents": documents,
            "files": files,
            "uninstaller": self.uninstaller,
        })
        .to_string()
    }

    /// 从 JSON 读。少了可选的字段没关系，少了必须有的报错
    pub fn from_json(text: &str) -> Result<Self, Error> {
        let bad = |what: &str| Error::NotAPackage(format!("清单里缺 {what}"));
        let value: Value = serde_json::from_str(text).map_err(|e| Error::NotAPackage(format!("清单读不了：{e}")))?;
        let text_of = |value: &Value, key: &str| value[key].as_str().map(str::to_owned);
        let strings = |value: &Value| -> Vec<String> { value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect() };
        let files = value["files"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                Ok(FileEntry {
                    path: text_of(f, "path").ok_or_else(|| bad("文件的路径"))?,
                    size: f["size"].as_u64().ok_or_else(|| bad("文件的大小"))?,
                    offset: f["offset"].as_u64().ok_or_else(|| bad("文件的位置"))?,
                    compressed: f["compressed"].as_u64().ok_or_else(|| bad("文件压缩后的大小"))?,
                    crc32: f["crc32"].as_u64().ok_or_else(|| bad("文件的校验和"))? as u32,
                    executable: f["executable"].as_bool().unwrap_or(false),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let documents = value["documents"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| DocumentType { name: text_of(d, "name").unwrap_or_default(), extensions: strings(&d["extensions"]), mime: text_of(d, "mime"), owner: d["owner"].as_bool().unwrap_or(false) })
            .collect();
        Ok(Self {
            name: text_of(&value, "name").ok_or_else(|| bad("应用名"))?,
            identifier: text_of(&value, "identifier").ok_or_else(|| bad("包标识符"))?,
            version: text_of(&value, "version").ok_or_else(|| bad("版本"))?,
            publisher: text_of(&value, "publisher").unwrap_or_default(),
            executable: text_of(&value, "executable").ok_or_else(|| bad("主程序"))?,
            icon: text_of(&value, "icon"),
            url_schemes: strings(&value["url_schemes"]),
            documents,
            files,
            uninstaller: value["uninstaller"].as_bool().unwrap_or(false),
        })
    }
}
