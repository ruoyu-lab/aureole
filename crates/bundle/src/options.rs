//! 打包设置：`[package.metadata.bundle]` 里写的，加上命令行的覆盖，补齐默认值

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::associations::{self, DocumentType};
use crate::icon::{IconSource, Rgb, VectorIcon};
use crate::plist::{self, Dict};

/// 命令行上能覆盖的设置
#[derive(Default, Debug)]
pub struct Overrides {
    pub bin: Option<String>,
    pub name: Option<String>,
    pub identifier: Option<String>,
    pub version: Option<String>,
    pub icon: Option<PathBuf>,
}

/// Windows 安装程序的设置（`[package.metadata.bundle.nsis]`）。路径都相对于包的目录
#[derive(Debug, Default)]
pub struct NsisOptions {
    /// 安装界面顶部的图（任意常见格式，缩放成 150x57）
    pub header_image: Option<PathBuf>,
    /// 欢迎页、完成页侧边的图（任意常见格式，缩放成 164x314）
    pub sidebar_image: Option<PathBuf>,
    /// 安装程序自己的图标（.ico 或图片）；不写时用应用的图标
    pub installer_icon: Option<PathBuf>,
    /// 自己的 NSIS 脚本模板：页面、流程都能改
    pub template: Option<PathBuf>,
    /// 安装界面的语言；不写时是中文和英文
    pub languages: Vec<String>,
    /// `currentUser`（默认，不要管理员权限）、`perMachine`（装给所有用户）、`both`（让用户选）
    pub install_mode: Option<String>,
}

/// 自己的安装程序的设置（`[package.metadata.bundle.installer]`）
#[derive(Debug, Default)]
pub struct InstallerOptions {
    /// 安装程序在哪个包里；不写时用默认的向导
    pub package: Option<String>,
    /// 那个包里的可执行文件；不写时和包同名
    pub bin: Option<String>,
    /// 发布者（"应用和功能"里显示）；不写时是维护者的名字，再没有时是应用名
    pub publisher: Option<String>,
}

impl InstallerOptions {
    /// 用哪个包的哪个可执行文件当安装程序
    pub fn stub(&self) -> (&str, &str) {
        match &self.package {
            Some(package) => (package, self.bin.as_deref().unwrap_or(package)),
            None => crate::self_installer::DEFAULT_STUB,
        }
    }
}

#[derive(Debug)]
pub struct BundleOptions {
    /// Cargo 包名（.deb 的包名用它）
    pub package: String,
    /// 应用名：.app 的文件名、访达和程序坞里显示的名字、菜单栏里的应用名
    pub name: String,
    /// 包的说明（`Cargo.toml` 的 `description`）
    pub description: Option<String>,
    /// .deb 的维护者：`maintainer` 设置，没写时是包的作者（`Cargo.toml` 的 `authors`）
    pub maintainer: Option<String>,
    /// .deb 另外依赖的运行库（`linux-depends`）
    pub linux_depends: Vec<String>,
    /// Windows 安装程序的外观和行为（`[package.metadata.bundle.nsis]`）
    pub nsis: NsisOptions,
    /// 自己的安装程序（`[package.metadata.bundle.installer]`）
    pub installer: InstallerOptions,
    pub identifier: String,
    /// 给人看的版本号（CFBundleShortVersionString）
    pub version: String,
    /// 构建号（CFBundleVersion）
    pub build: String,
    /// 可执行文件名（包里的 bin）
    pub bin: String,
    pub category: Option<String>,
    pub minimum_system_version: String,
    pub copyright: Option<String>,
    pub icon: IconSource,
    /// 拷进 Contents/Resources 的文件或文件夹
    pub resources: Vec<PathBuf>,
    pub entitlements: Option<PathBuf>,
    pub development_region: String,
    pub localizations: Vec<String>,
    /// 能打开的协议（`url-schemes`），如 `myapp` 对应 `myapp://…`
    pub url_schemes: Vec<String>,
    /// 能打开的文档类型（`document-types`）
    pub document_types: Vec<DocumentType>,
    /// `info-plist` 表：原样写进 Info.plist，覆盖同名的键
    pub extra: Dict,
}

impl BundleOptions {
    /// `package` 是 `cargo metadata` 输出里的一个包。返回设置和要提示用户的话
    pub fn resolve(package: &Value, overrides: Overrides) -> Result<(Self, Vec<String>), String> {
        let mut notes = Vec::new();
        let package_name = package["name"].as_str().ok_or("cargo metadata 里的包没有名字")?;
        let dir = package["manifest_path"].as_str().map(Path::new).and_then(Path::parent).ok_or("cargo metadata 里的包没有清单路径")?;
        let meta = &package["metadata"]["bundle"];
        let text = |key: &str| meta[key].as_str().map(str::to_owned);
        let in_package = |p: &str| dir.join(p);

        let bins: Vec<&str> = package["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "bin")))
            .filter_map(|t| t["name"].as_str())
            .collect();
        let bin = match overrides.bin.or_else(|| text("bin")) {
            Some(bin) if bins.contains(&bin.as_str()) => bin,
            Some(bin) => return Err(format!("包 {package_name} 里没有可执行文件 {bin}（有：{}）", bins.join("、"))),
            None => match bins.as_slice() {
                [] => return Err(format!("包 {package_name} 没有可执行文件，没法打包")),
                [only] => (*only).to_owned(),
                _ if bins.contains(&package_name) => package_name.to_owned(),
                _ => return Err(format!("包 {package_name} 有多个可执行文件（{}），用 --bin 选一个", bins.join("、"))),
            },
        };

        let name = overrides.name.or_else(|| text("name")).unwrap_or_else(|| package_name.to_owned());
        if name.trim().is_empty() || name.contains(['/', ':']) {
            return Err(format!("应用名 {name:?} 不能为空，也不能有 / 和 :"));
        }
        let identifier = match overrides.identifier.or_else(|| text("identifier")) {
            Some(id) => id,
            None => {
                let id = format!("com.example.{}", sanitize_identifier(package_name));
                notes.push(format!("没有设置包标识符，暂用 {id}；发布前在 [package.metadata.bundle] 里写 identifier = \"com.你的域名.{package_name}\""));
                id
            }
        };
        if identifier.is_empty() || !identifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return Err(format!("包标识符 {identifier:?} 只能有英文字母、数字、点和连字符"));
        }

        let full_version = overrides.version.or_else(|| text("version")).or_else(|| package["version"].as_str().map(str::to_owned)).unwrap_or_else(|| "0.1.0".into());
        let version = numeric_version(&full_version);
        if version != full_version {
            notes.push(format!("版本号 {full_version} 去掉了预发布和构建部分，写成 {version}（系统只认数字和点）"));
        }
        let build = text("build").unwrap_or_else(|| version.clone());

        let category = text("category").map(|c| if c.contains('.') { c } else { format!("public.app-category.{c}") });
        let icon = match overrides.icon {
            Some(path) => image_source(path),
            None => match &meta["icon"] {
                Value::Null => VectorIcon::default().into_source(),
                Value::String(path) => image_source(in_package(path)),
                Value::Object(_) => vector_icon(&meta["icon"])?.into_source(),
                other => return Err(format!("icon 应该是图片路径或矢量图标的表，现在是 {other}")),
            },
        };
        let resources = meta["resources"].as_array().into_iter().flatten().filter_map(Value::as_str).map(in_package).collect();
        let localizations = match meta["localizations"].as_array() {
            Some(list) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            None => vec!["en".into(), "zh-Hans".into()],
        };
        let url_schemes = associations::parse_schemes(&meta["url-schemes"])?;
        let document_types = associations::parse_document_types(&meta["document-types"], &identifier, dir)?;
        for doc in document_types.iter().filter(|d| d.exported && d.mime.is_none()) {
            notes.push(format!("文档类型 {} 没写 mime，Linux 上用 {}", doc.name, doc.linux_mime().unwrap_or_default()));
        }
        let extra = match plist::from_json(&meta["info-plist"]) {
            Some(plist::Value::Dict(dict)) => dict,
            Some(_) => return Err("info-plist 应该是一个表".into()),
            None => Dict::default(),
        };
        let authors = package["authors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
        let maintainer = text("maintainer").or_else(|| (!authors.is_empty()).then_some(authors));
        let nsis_table = &meta["nsis"];
        let nsis_path = |key: &str| nsis_table[key].as_str().map(in_package);
        let install_mode = nsis_table["install-mode"].as_str().map(str::to_owned);
        if let Some(mode) = &install_mode
            && !["currentUser", "perMachine", "both"].contains(&mode.as_str())
        {
            return Err(format!("nsis.install-mode 只能是 currentUser、perMachine、both，现在是 {mode}"));
        }
        let nsis = NsisOptions {
            header_image: nsis_path("header-image"),
            sidebar_image: nsis_path("sidebar-image"),
            installer_icon: nsis_path("installer-icon"),
            template: nsis_path("template"),
            languages: nsis_table["languages"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
            install_mode,
        };
        let installer_table = &meta["installer"];
        let installer = InstallerOptions {
            package: installer_table["package"].as_str().map(str::to_owned),
            bin: installer_table["bin"].as_str().map(str::to_owned),
            publisher: installer_table["publisher"].as_str().map(str::to_owned),
        };
        let options = BundleOptions {
            package: package_name.to_owned(),
            nsis,
            installer,
            name,
            description: package["description"].as_str().filter(|d| !d.is_empty()).map(str::to_owned),
            maintainer,
            linux_depends: meta["linux-depends"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect(),
            identifier,
            version,
            build,
            bin,
            category,
            minimum_system_version: text("minimum-system-version").unwrap_or_else(|| "11.0".into()),
            copyright: text("copyright"),
            icon,
            resources,
            entitlements: text("entitlements").map(|p| in_package(&p)),
            development_region: text("development-region").unwrap_or_else(|| "en".into()),
            localizations,
            url_schemes,
            document_types,
            extra,
        };
        Ok((options, notes))
    }

    /// Info.plist 的内容
    pub fn info_plist(&self) -> Dict {
        let mut d = Dict::default();
        d.set("CFBundleDevelopmentRegion", self.development_region.as_str());
        d.set("CFBundleDisplayName", self.name.as_str());
        d.set("CFBundleExecutable", self.bin.as_str());
        d.set("CFBundleIconFile", "AppIcon");
        d.set("CFBundleIdentifier", self.identifier.as_str());
        d.set("CFBundleInfoDictionaryVersion", "6.0");
        d.set("CFBundleName", self.name.as_str());
        d.set("CFBundlePackageType", "APPL");
        d.set("CFBundleShortVersionString", self.version.as_str());
        d.set("CFBundleSignature", "????");
        d.set("CFBundleVersion", self.build.as_str());
        // 声明支持的语言：系统自带的界面（文件对话框、菜单里的系统项）按用户的语言显示
        d.set("CFBundleLocalizations", self.localizations.clone());
        d.set("CFBundleAllowMixedLocalizations", true);
        if let Some(category) = &self.category {
            d.set("LSApplicationCategoryType", category.as_str());
        }
        d.set("LSMinimumSystemVersion", self.minimum_system_version.as_str());
        d.set("NSHighResolutionCapable", true);
        d.set("NSPrincipalClass", "NSApplication");
        // 有两块显卡的机器上允许用集成显卡，不强制切到独立显卡
        d.set("NSSupportsAutomaticGraphicsSwitching", true);
        if let Some(copyright) = &self.copyright {
            d.set("NSHumanReadableCopyright", copyright.as_str());
        }
        // 能打开的协议和文档类型（应用里用 App::on_open 接收）
        associations::add_to_info_plist(&mut d, &self.identifier, &self.url_schemes, &self.document_types);
        for (key, value) in &self.extra.0 {
            d.set(key, value.clone());
        }
        d
    }
}

impl VectorIcon {
    fn into_source(self) -> IconSource {
        IconSource::Vector(self)
    }
}

fn image_source(path: PathBuf) -> IconSource {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("icns")) { IconSource::Icns(path) } else { IconSource::Image(path) }
}

/// `icon = { path = "M…", view-box = [0, 0, 24, 24], stroke = 2, background = ["#…", "#…"], foreground = "#fff…" }`
fn vector_icon(table: &Value) -> Result<VectorIcon, String> {
    let default = VectorIcon::default();
    let color = |v: &Value| v.as_str().and_then(Rgb::parse).ok_or_else(|| format!("颜色 {v} 应该写成 \"#rrggbb\""));
    let path = table["path"].as_str().ok_or("矢量图标需要 path（SVG 路径语法）")?.to_owned();
    let view_box = match table["view-box"].as_array() {
        Some(v) if v.len() == 4 => {
            let n: Vec<f32> = v.iter().filter_map(Value::as_f64).map(|x| x as f32).collect();
            <[f32; 4]>::try_from(n).map_err(|_| "view-box 应该是 4 个数字".to_owned())?
        }
        Some(_) => return Err("view-box 应该是 4 个数字：[x, y, 宽, 高]".into()),
        None => default.view_box,
    };
    let stroke = table["stroke"].as_f64().map(|w| w as f32);
    let background = match &table["background"] {
        Value::Null => default.background,
        Value::String(_) => {
            let c = color(&table["background"])?;
            [c, c]
        }
        Value::Array(v) if v.len() == 2 => [color(&v[0])?, color(&v[1])?],
        other => return Err(format!("background 应该是一个颜色或两个颜色（从上到下的渐变），现在是 {other}")),
    };
    let foreground = match &table["foreground"] {
        Value::Null => default.foreground,
        v => color(v)?,
    };
    Ok(VectorIcon { path, view_box, stroke, background, foreground })
}

/// 包标识符只能有字母、数字、点和连字符
fn sanitize_identifier(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

/// `1.2.3-beta.1+build` → `1.2.3`
fn numeric_version(version: &str) -> String {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    if !core.is_empty() && core.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) { core.to_owned() } else { "0.1.0".into() }
}
