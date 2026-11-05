//! 安装和卸载：不依赖界面，也不依赖窗口。
//!
//! 安装分两个阶段，保证中途出错、取消时能回到原来的样子：
//!
//! 1. **解压**：每个文件先解压成旁边的 `名字.aureole-new`，校验。这时已有的文件一个都没动，取消、出错只要删掉这些临时文件。
//! 2. **提交**：先确认旧文件没有被占用，然后把旧文件改名成 `名字.aureole-old`、把新文件改成正式的名字。改名很快，也很少失败。
//!
//! 全部成功才删旧文件。任何一步失败都按相反的顺序撤销：[`Step::undo`]、系统集成、文件。
//! 安装目录里不是这个包带来的文件（用户的数据）不动：升级时只替换、删除清单和上次记录里列的。

use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::manifest::Manifest;
use crate::package::{Package, make_executable};
use crate::system::{self, Registered};
use crate::{CancelToken, Error};

/// 安装记录的文件名：放在安装目录里，升级和卸载靠它知道装过什么
pub const RECEIPT_FILE: &str = "aureole-install.json";
/// 卸载程序的文件名
pub const UNINSTALLER_NAME: &str = if cfg!(windows) { "uninstall.exe" } else { "uninstall" };

const NEW_SUFFIX: &str = ".aureole-new";
const OLD_SUFFIX: &str = ".aureole-old";
/// 解压占进度条的比例，剩下的是提交、系统集成和自定义的步骤
const EXTRACT_SHARE: f32 = 0.9;

/// 正在做什么
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Activity {
    /// 解压一个文件（相对于安装目录的路径）
    Extracting(String),
    /// 把新文件换上去
    Committing,
    /// 快捷方式、卸载项、协议和文档类型
    Registering,
    /// 自定义的步骤（它的名字）
    Step(String),
    /// 删除文件
    Removing,
    /// 收尾
    Finishing,
}

/// 进度
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// 0 到 1
    pub fraction: f32,
    /// 正在做什么
    pub activity: Activity,
}

/// 安装选项
#[derive(Clone, Debug)]
pub struct InstallOptions {
    /// 装到哪里
    pub directory: PathBuf,
    /// 开始菜单（Linux 的应用列表）里放快捷方式
    pub menu_shortcut: bool,
    /// 桌面上放快捷方式（Windows）
    pub desktop_shortcut: bool,
    /// 写系统集成：卸载项、协议、文档类型。预览和测试时关掉，就只解压文件
    pub register: bool,
}

impl InstallOptions {
    /// 默认都要，桌面快捷方式也要
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into(), menu_shortcut: true, desktop_shortcut: true, register: true }
    }
}

/// 自定义的一步：内置的都没有的事，比如注册服务、装驱动、改防火墙、先装依赖。
/// 在解压和系统集成之后运行，这时文件都在位了
pub trait Step: Send {
    /// 名字，显示在进度里
    fn name(&self) -> &str;

    /// 安装时做的事
    fn apply(&mut self, cx: &StepContext) -> Result<(), String>;

    /// 安装中途失败或者取消时撤销 [`Self::apply`] 做的事。默认和卸载时一样
    fn undo(&mut self, cx: &StepContext) {
        let _ = self.remove(cx);
    }

    /// 卸载时做的事。默认什么也不做
    fn remove(&mut self, cx: &StepContext) -> Result<(), String> {
        let _ = cx;
        Ok(())
    }
}

/// 步骤能看到的
#[derive(Clone, Debug)]
pub struct StepContext {
    /// 安装目录
    pub directory: PathBuf,
    /// 清单
    pub manifest: Manifest,
    /// 这个目录之前已经装过别的版本（升级）。这时的版本号
    pub previous_version: Option<String>,
}

/// 安装记录：装了哪些文件，在系统里登记了什么
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    /// 当时的清单（去掉了文件列表）：回滚升级时用它恢复上一个版本的登记
    pub manifest: Manifest,
    /// 装的文件，相对于安装目录
    pub files: Vec<String>,
    /// 在系统里登记的
    pub registered: Registered,
}

impl Receipt {
    /// 读安装目录里的记录
    pub fn read(directory: &Path) -> Result<Self, Error> {
        let path = directory.join(RECEIPT_FILE);
        let text = std::fs::read_to_string(&path).map_err(|_| Error::NotInstalled(directory.to_path_buf()))?;
        let value: Value = serde_json::from_str(&text).map_err(|_| Error::NotInstalled(directory.to_path_buf()))?;
        let manifest = Manifest::from_json(&value["manifest"].to_string()).map_err(|_| Error::NotInstalled(directory.to_path_buf()))?;
        let files = value["files"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        Ok(Self { manifest, files, registered: Registered::from_json(&value["registered"]) })
    }

    fn to_json(&self) -> String {
        let manifest: Value = serde_json::from_str(&Manifest { files: Vec::new(), ..self.manifest.clone() }.to_json()).unwrap_or(Value::Null);
        json!({ "manifest": manifest, "files": self.files, "registered": self.registered.to_json() }).to_string()
    }
}

/// 路径不能跑到安装目录外面（安装包被改过时）
fn checked_path(directory: &Path, relative: &str) -> Result<PathBuf, Error> {
    let path = Path::new(relative);
    if relative.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(Error::Corrupt(relative.to_owned()));
    }
    Ok(directory.join(path))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// 建目录，记下新建了哪些（回滚时从里往外删）
fn create_dirs(path: &Path, created: &mut Vec<PathBuf>) -> Result<(), Error> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_dirs(parent, created)?;
    }
    std::fs::create_dir(path).map_err(|e| Error::io(path, e))?;
    created.push(path.to_path_buf());
    Ok(())
}

/// 文件能写：不能写时（应用还开着）现在就报错，还什么都没动
fn check_not_in_use(path: &Path) -> Result<(), Error> {
    if !path.is_file() {
        return Ok(());
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Err(e) => Err(Error::io(path, e)),
        Ok(_) => Ok(()),
    }
}

/// 安装时做了什么：回滚按相反的顺序撤销
#[derive(Default)]
struct Journal {
    directories: Vec<PathBuf>,
    /// 解压出来还没换上去的
    staged: Vec<PathBuf>,
    /// 换上去的文件，和被它换下来、改了名的旧文件
    committed: Vec<(PathBuf, Option<PathBuf>)>,
    registered: Option<Registered>,
    steps_done: usize,
}

/// 安装
pub struct Installer {
    package: Package,
    options: InstallOptions,
    steps: Vec<Box<dyn Step>>,
}

impl Installer {
    /// 把 `package` 按 `options` 装上去
    pub fn new(package: Package, options: InstallOptions) -> Self {
        Self { package, options, steps: Vec::new() }
    }

    /// 加一个自定义的步骤
    pub fn step(mut self, step: Box<dyn Step>) -> Self {
        self.steps.push(step);
        self
    }

    /// 运行，直到完成或者失败。失败、取消时已经撤销了做过的事。`observer` 在这个线程上调用
    pub fn run(mut self, cancel: &CancelToken, mut observer: impl FnMut(Progress)) -> Result<Receipt, Error> {
        let previous = Receipt::read(&self.options.directory).ok();
        let mut journal = Journal::default();
        match self.apply(previous.as_ref(), &mut journal, cancel, &mut observer) {
            Ok(receipt) => {
                observer(Progress { fraction: 1.0, activity: Activity::Finishing });
                finish(&journal, previous.as_ref(), &receipt, &self.options.directory);
                Ok(receipt)
            }
            Err(error) => {
                self.roll_back(previous.as_ref(), journal);
                Err(error)
            }
        }
    }

    fn context(&self, previous: Option<&Receipt>) -> StepContext {
        StepContext { directory: self.options.directory.clone(), manifest: self.package.manifest().clone(), previous_version: previous.map(|p| p.manifest.version.clone()) }
    }

    fn apply(&mut self, previous: Option<&Receipt>, journal: &mut Journal, cancel: &CancelToken, observer: &mut impl FnMut(Progress)) -> Result<Receipt, Error> {
        let directory = self.options.directory.clone();
        let manifest = self.package.manifest().clone();
        create_dirs(&directory, &mut journal.directories)?;

        // 第一阶段：解压成临时文件，已有的文件不动
        let total = manifest.total_size().max(1);
        let mut done = 0u64;
        let mut targets = Vec::with_capacity(manifest.files.len() + 2);
        for entry in &manifest.files {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let target = checked_path(&directory, &entry.path)?;
            if let Some(parent) = target.parent() {
                create_dirs(parent, &mut journal.directories)?;
            }
            let staged = with_suffix(&target, NEW_SUFFIX);
            journal.staged.push(staged.clone());
            let mut file = std::fs::File::create(&staged).map_err(|e| Error::io(&staged, e))?;
            observer(Progress { fraction: done as f32 / total as f32 * EXTRACT_SHARE, activity: Activity::Extracting(entry.path.clone()) });
            let mut written = 0u64;
            let mut last = 0u64;
            self.package.extract(entry, &mut file, cancel, |n| {
                written += n;
                // 每 1 MB 报一次：小文件不报，免得太频繁
                if written - last >= 1 << 20 {
                    last = written;
                    observer(Progress { fraction: (done + written) as f32 / total as f32 * EXTRACT_SHARE, activity: Activity::Extracting(entry.path.clone()) });
                }
            })?;
            drop(file);
            if entry.executable {
                make_executable(&staged);
            }
            done += entry.size;
            targets.push(target);
        }
        // 卸载程序（预览的包没有程序本身）
        let uninstaller = directory.join(UNINSTALLER_NAME);
        if !self.package_is_preview() {
            let staged = with_suffix(&uninstaller, NEW_SUFFIX);
            journal.staged.push(staged.clone());
            self.package.write_uninstaller(&staged)?;
            targets.push(uninstaller);
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        // 第二阶段：确认旧文件没被占用，再换上去
        observer(Progress { fraction: EXTRACT_SHARE, activity: Activity::Committing });
        for target in &targets {
            check_not_in_use(target)?;
        }
        for target in targets {
            let staged = with_suffix(&target, NEW_SUFFIX);
            let old = if target.is_file() {
                let old = with_suffix(&target, OLD_SUFFIX);
                std::fs::rename(&target, &old).map_err(|e| Error::io(&target, e))?;
                Some(old)
            } else {
                None
            };
            journal.committed.push((target.clone(), old));
            std::fs::rename(&staged, &target).map_err(|e| Error::io(&target, e))?;
            journal.staged.retain(|s| *s != staged);
        }

        // 系统集成
        let mut registered = Registered::default();
        if self.options.register {
            observer(Progress { fraction: EXTRACT_SHARE + 0.03, activity: Activity::Registering });
            // 先记下来：登记到一半失败时，回滚也要处理
            journal.registered = Some(Registered::default());
            registered = system::register(&manifest, &self.options)?;
            journal.registered = Some(registered.clone());
        }

        // 自定义的步骤
        let context = self.context(previous);
        let count = self.steps.len().max(1);
        for i in 0..self.steps.len() {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let name = self.steps[i].name().to_owned();
            observer(Progress { fraction: EXTRACT_SHARE + 0.03 + 0.06 * i as f32 / count as f32, activity: Activity::Step(name.clone()) });
            self.steps[i].apply(&context).map_err(|message| Error::Step { step: name, message })?;
            journal.steps_done = i + 1;
        }

        // 记录放最后：有记录就说明装完整了
        let mut files: Vec<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
        if !self.package_is_preview() {
            files.push(UNINSTALLER_NAME.to_owned());
        }
        let receipt = Receipt { manifest: manifest.clone(), files, registered };
        let path = directory.join(RECEIPT_FILE);
        let staged = with_suffix(&path, NEW_SUFFIX);
        journal.staged.push(staged.clone());
        std::fs::write(&staged, receipt.to_json()).map_err(|e| Error::io(&staged, e))?;
        let old = if path.is_file() {
            let old = with_suffix(&path, OLD_SUFFIX);
            std::fs::rename(&path, &old).map_err(|e| Error::io(&path, e))?;
            Some(old)
        } else {
            None
        };
        journal.committed.push((path.clone(), old));
        std::fs::rename(&staged, &path).map_err(|e| Error::io(&path, e))?;
        journal.staged.retain(|s| *s != staged);
        Ok(receipt)
    }

    fn package_is_preview(&self) -> bool {
        self.package.is_preview()
    }

    /// 按相反的顺序撤销。撤销时再出错也继续：能恢复多少恢复多少
    fn roll_back(&mut self, previous: Option<&Receipt>, journal: Journal) {
        let context = self.context(previous);
        for step in self.steps[..journal.steps_done].iter_mut().rev() {
            step.undo(&context);
        }
        if let Some(registered) = &journal.registered {
            system::unregister(registered);
            // 升级失败：上一个版本的登记刚才可能被覆盖、被删掉了，恢复
            if let Some(previous) = previous {
                let _ = system::register(&previous.manifest, &self.options);
            }
        }
        for (target, old) in journal.committed.iter().rev() {
            let _ = std::fs::remove_file(target);
            if let Some(old) = old {
                let _ = std::fs::rename(old, target);
            }
        }
        for staged in &journal.staged {
            let _ = std::fs::remove_file(staged);
        }
        for directory in journal.directories.iter().rev() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

/// 全部成功：删掉换下来的旧文件，上一个版本有、这个版本没有的文件和快捷方式
fn finish(journal: &Journal, previous: Option<&Receipt>, receipt: &Receipt, directory: &Path) {
    for (_, old) in &journal.committed {
        if let Some(old) = old {
            let _ = std::fs::remove_file(old);
        }
    }
    let Some(previous) = previous else { return };
    for stale in previous.files.iter().filter(|f| !receipt.files.contains(f)) {
        if let Ok(path) = checked_path(directory, stale) {
            let _ = std::fs::remove_file(&path);
            remove_empty_parents(&path, directory);
        }
    }
    system::remove_stale(&previous.registered, &receipt.registered);
}

/// 文件删掉以后，往上删空了的目录，到 `root` 为止（`root` 自己不删）
fn remove_empty_parents(path: &Path, root: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root || std::fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}

/// 卸载
pub struct Uninstaller {
    directory: PathBuf,
    steps: Vec<Box<dyn Step>>,
}

impl Uninstaller {
    /// 卸载 `directory` 里装的
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into(), steps: Vec::new() }
    }

    /// 加一个自定义的步骤（调用它的 [`Step::remove`]）
    pub fn step(mut self, step: Box<dyn Step>) -> Self {
        self.steps.push(step);
        self
    }

    /// 运行。只删记录里列的文件，用户自己的文件留着；目录空了才删目录。
    /// 卸载程序正在运行时删不掉自己（Windows）：[`Setup`](crate::Setup) 会先把自己拷到临时目录再运行
    pub fn run(mut self, mut observer: impl FnMut(Progress)) -> Result<(), Error> {
        let receipt = Receipt::read(&self.directory)?;
        let context = StepContext { directory: self.directory.clone(), manifest: receipt.manifest.clone(), previous_version: None };
        // 应用还开着时现在就说，什么都没动
        let main = checked_path(&self.directory, &receipt.manifest.executable)?;
        check_not_in_use(&main)?;

        for step in self.steps.iter_mut().rev() {
            let name = step.name().to_owned();
            observer(Progress { fraction: 0.0, activity: Activity::Step(name.clone()) });
            step.remove(&context).map_err(|message| Error::Step { step: name, message })?;
        }
        observer(Progress { fraction: 0.1, activity: Activity::Registering });
        system::unregister(&receipt.registered);

        let count = receipt.files.len().max(1);
        for (i, file) in receipt.files.iter().enumerate() {
            observer(Progress { fraction: 0.1 + 0.85 * i as f32 / count as f32, activity: Activity::Removing });
            let path = checked_path(&self.directory, file)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // 卸载程序自己：之后再处理，不算失败
                Err(_) if file == UNINSTALLER_NAME => {}
                Err(e) => return Err(Error::io(&path, e)),
            }
            remove_empty_parents(&path, &self.directory);
        }
        let receipt_path = self.directory.join(RECEIPT_FILE);
        std::fs::remove_file(&receipt_path).map_err(|e| Error::io(&receipt_path, e))?;
        let _ = std::fs::remove_dir(&self.directory);
        observer(Progress { fraction: 1.0, activity: Activity::Finishing });
        Ok(())
    }
}
