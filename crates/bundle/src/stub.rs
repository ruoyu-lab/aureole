//! 默认的安装程序从哪来。
//!
//! 它在 `aureole-installer` 这个包里。框架的工作区、依赖了这个包的应用可以直接 `cargo build -p`；别的应用的
//! 依赖图里没有它，`-p` 找不到。这时在输出目录里生成一个小项目依赖它，来源和应用用的框架相同（路径、Git、
//! crates.io），版本也一致：安装程序和应用用同一份框架，`Cargo.lock` 也拷过去。

use std::path::{Path, PathBuf};

use serde_json::Value;

/// 包名
const PACKAGE: &str = "aureole-installer";
/// 框架的门面，用来判断应用用的是哪一份框架
const FACADE: &str = "aureole";

/// 怎么拿到默认的安装程序
#[derive(Debug, PartialEq)]
pub enum Source {
    /// 依赖图里已经有了：直接 `cargo build -p`
    Direct,
    /// 生成一个小项目，依赖写成这样（`Cargo.toml` 里等号右边的内容）
    Project(String),
}

/// 看应用的依赖图决定。`metadata` 要带依赖（不能 `--no-deps`），`own_version` 是打包工具自己的版本：
/// 应用没用框架时（打包别的程序）按它取
pub fn source(metadata: &Value, own_version: &str) -> Source {
    let packages = metadata["packages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let find = |name: &str| packages.iter().find(|p| p["name"] == name);
    if find(PACKAGE).is_some() {
        return Source::Direct;
    }
    let dependency = find(FACADE).and_then(same_source).unwrap_or_else(|| format!("{{ version = {} }}", toml_string(&format!("={own_version}"))));
    Source::Project(dependency)
}

/// 和门面同一来源的依赖写法
fn same_source(facade: &Value) -> Option<String> {
    let version = facade["version"].as_str()?;
    let Some(source) = facade["source"].as_str() else {
        // 路径依赖：在框架仓库里，门面的旁边
        let manifest = Path::new(facade["manifest_path"].as_str()?);
        let installer = manifest.parent()?.parent()?.join("installer");
        return installer.join("Cargo.toml").is_file().then(|| format!("{{ path = {} }}", toml_string(&installer.to_string_lossy())));
    };
    if source.starts_with("registry+") {
        return Some(format!("{{ version = {} }}", toml_string(&format!("={version}"))));
    }
    // git+https://host/repo?rev=abc#完整提交号：钉在完整的提交号上
    let rest = source.strip_prefix("git+")?;
    let (url, commit) = rest.split_once('#')?;
    let url = url.split('?').next()?;
    Some(format!("{{ git = {}, rev = {} }}", toml_string(url), toml_string(commit)))
}

/// TOML 的基本字符串：转义规则和 JSON 的相同
fn toml_string(value: &str) -> String {
    Value::from(value).to_string()
}

/// 小项目的 `Cargo.toml`。空的 `[workspace]`：它在应用的 target 目录里，不能被当成应用工作区的一员
fn manifest(dependency: &str) -> String {
    format!(
        "# cargo bundle 生成的，用来编默认的安装程序；不用提交，改了也会被覆盖\n\
         [package]\nname = \"aureole-setup-host\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
         [lib]\npath = \"lib.rs\"\n\n\
         [dependencies]\n{PACKAGE} = {dependency}\n\n\
         [workspace]\n"
    )
}

/// 写出小项目，返回它的 `Cargo.toml`。`lock` 是应用的 `Cargo.lock`：依赖的版本和应用的一致，也能复用编译结果
pub fn write_project(directory: &Path, dependency: &str, lock: Option<&Path>) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory).map_err(|e| format!("创建 {} 失败：{e}", directory.display()))?;
    let path = directory.join("Cargo.toml");
    std::fs::write(&path, manifest(dependency)).map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    let source = directory.join("lib.rs");
    std::fs::write(&source, "").map_err(|e| format!("写 {} 失败：{e}", source.display()))?;
    if let Some(lock) = lock.filter(|lock| lock.is_file()) {
        let copy = directory.join("Cargo.lock");
        std::fs::copy(lock, &copy).map_err(|e| format!("拷贝 {} 失败：{e}", lock.display()))?;
    }
    Ok(path)
}
