//! 默认的向导：许可协议（可选）、自己加的页、选目录、进度、完成；卸载是确认、进度、完成。
//!
//! 只用 [`Session`] 的公开接口。要换颜色、图片、文字、加页时用这里的设置；要完全不一样时自己写一个 `fn(Session) -> Element`。

use aureole::prelude::*;
use aureole::{ImageFit, ImageSource, image};

use crate::engine::Activity;
use crate::session::{Mode, Session, State};

/// 向导上的文字。默认是中文，`{name}`、`{version}` 换成应用名、版本
#[derive(Clone, Debug)]
pub struct Texts {
    /// 安装页的标题
    pub install_title: String,
    /// 已经装了别的版本时的说明，`{version}` 是这次的，`{previous}` 是已经装的。可能是降级，所以默认不写"升级"
    pub upgrade_note: String,
    /// 已经装了同一个版本时的说明
    pub reinstall_note: String,
    /// "安装位置"
    pub location: String,
    /// "更改…"
    pub change: String,
    /// "需要多少空间"，`{size}` 是大小
    pub space_needed: String,
    /// "在开始菜单放快捷方式"（Linux 上是应用列表）
    pub menu_shortcut: String,
    /// "在桌面放快捷方式"
    pub desktop_shortcut: String,
    /// "安装"
    pub install: String,
    /// "取消"
    pub cancel: String,
    /// "上一步"
    pub back: String,
    /// "下一步"
    pub next: String,
    /// "许可协议"
    pub license_title: String,
    /// "我同意"
    pub license_agree: String,
    /// "正在安装"
    pub installing: String,
    /// "安装完成"
    pub installed: String,
    /// "现在打开"
    pub launch: String,
    /// "完成"
    pub finish: String,
    /// "已取消"
    pub cancelled: String,
    /// 取消之后的说明
    pub cancelled_note: String,
    /// "没有装好"
    pub failed: String,
    /// "重试"
    pub retry: String,
    /// "关闭"
    pub close: String,
    /// 卸载页的标题
    pub uninstall_title: String,
    /// 卸载页的说明
    pub uninstall_note: String,
    /// "卸载"
    pub uninstall: String,
    /// "正在卸载"
    pub uninstalling: String,
    /// "已卸载"
    pub uninstalled: String,
    /// 进度里的：解压、换文件、登记、收尾、删除
    pub extracting: String,
    /// 把新文件换上去
    pub committing: String,
    /// 快捷方式和系统登记
    pub registering: String,
    /// 收尾
    pub finishing: String,
    /// 删除文件
    pub removing: String,
}

impl Default for Texts {
    fn default() -> Self {
        Self {
            install_title: "安装 {name}".into(),
            upgrade_note: "已经装了 {previous}，这次安装 {version}。".into(),
            reinstall_note: "已经装了 {version}，这次重新安装。".into(),
            location: "安装位置".into(),
            change: "更改…".into(),
            space_needed: "需要 {size} 空间".into(),
            menu_shortcut: if cfg!(windows) { "在开始菜单放快捷方式".into() } else { "放进应用列表".into() },
            desktop_shortcut: "在桌面放快捷方式".into(),
            install: "安装".into(),
            cancel: "取消".into(),
            back: "上一步".into(),
            next: "下一步".into(),
            license_title: "许可协议".into(),
            license_agree: "我同意上面的协议".into(),
            installing: "正在安装 {name}".into(),
            installed: "{name} 装好了".into(),
            launch: "现在打开 {name}".into(),
            finish: "完成".into(),
            cancelled: "已取消".into(),
            cancelled_note: "做到一半的都撤销了，这台电脑上没有留下东西。".into(),
            failed: "没有装好".into(),
            retry: "重试".into(),
            close: "关闭".into(),
            uninstall_title: "卸载 {name}".into(),
            uninstall_note: "会删除装在下面这个位置的程序。你自己放在里面的文件不会动。".into(),
            uninstall: "卸载".into(),
            uninstalling: "正在卸载 {name}".into(),
            uninstalled: "{name} 已卸载".into(),
            extracting: "解压".into(),
            committing: "正在换上新文件".into(),
            registering: "正在建快捷方式".into(),
            finishing: "收尾".into(),
            removing: "正在删除文件".into(),
        }
    }
}

impl Texts {
    /// 英文
    pub fn english() -> Self {
        Self {
            install_title: "Install {name}".into(),
            upgrade_note: "{previous} is installed. This will replace it with {version}.".into(),
            reinstall_note: "{version} is already installed. This will reinstall it.".into(),
            location: "Location".into(),
            change: "Change…".into(),
            space_needed: "Needs {size} of disk space".into(),
            menu_shortcut: if cfg!(windows) { "Add to the Start menu".into() } else { "Add to the applications list".into() },
            desktop_shortcut: "Add a desktop shortcut".into(),
            install: "Install".into(),
            cancel: "Cancel".into(),
            back: "Back".into(),
            next: "Next".into(),
            license_title: "License agreement".into(),
            license_agree: "I agree to the terms above".into(),
            installing: "Installing {name}".into(),
            installed: "{name} is ready".into(),
            launch: "Open {name} now".into(),
            finish: "Finish".into(),
            cancelled: "Cancelled".into(),
            cancelled_note: "Everything done so far was undone. Nothing was left on this computer.".into(),
            failed: "Installation failed".into(),
            retry: "Try again".into(),
            close: "Close".into(),
            uninstall_title: "Uninstall {name}".into(),
            uninstall_note: "This removes the program from the location below. Your own files in there stay.".into(),
            uninstall: "Uninstall".into(),
            uninstalling: "Uninstalling {name}".into(),
            uninstalled: "{name} was uninstalled".into(),
            extracting: "Extracting".into(),
            committing: "Putting new files in place".into(),
            registering: "Creating shortcuts".into(),
            finishing: "Finishing".into(),
            removing: "Removing files".into(),
        }
    }
}

type Page = Box<dyn Fn(Session) -> Element>;

/// 默认的向导。不设置也能用；颜色、图片、文字、协议、自己的页都能换
#[must_use]
pub struct Wizard {
    accent: Color,
    image: Option<ImageSource>,
    tagline: Option<String>,
    license: Option<String>,
    pages: Vec<Page>,
    texts: Texts,
}

impl Default for Wizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Wizard {
    /// 默认的样子
    pub fn new() -> Self {
        Self { accent: Color::hex(0x138a84), image: None, tagline: None, license: None, pages: Vec::new(), texts: Texts::default() }
    }

    /// 主色：左边一栏、主要的按钮、进度条
    pub fn accent(mut self, accent: Color) -> Self {
        self.accent = accent;
        self
    }

    /// 左边一栏的图（铺满，建议 176x440）。不设时是主色上写应用名的第一个字
    pub fn image(mut self, image: impl Into<ImageSource>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// 左边一栏应用名下面的一句话
    pub fn tagline(mut self, tagline: impl Into<String>) -> Self {
        self.tagline = Some(tagline.into());
        self
    }

    /// 许可协议：安装之前要同意
    pub fn license(mut self, license: impl Into<String>) -> Self {
        self.license = Some(license.into());
        self
    }

    /// 加一页，放在安装选项前面，按加的顺序
    pub fn page(mut self, page: impl Fn(Session) -> Element + 'static) -> Self {
        self.pages.push(Box::new(page));
        self
    }

    /// 换文字
    pub fn texts(mut self, texts: Texts) -> Self {
        self.texts = texts;
        self
    }

    /// 建界面
    pub fn build(self, session: Session) -> Element {
        let look = Look::new(self.accent);
        let texts = std::rc::Rc::new(self.texts);
        let side = side_panel(session, self.accent, self.image, self.tagline);
        let body: Element = match session.mode() {
            Mode::Install => install_flow(session, look.clone(), texts.clone(), self.license, self.pages),
            Mode::Uninstall => uninstall_flow(session, look.clone(), texts.clone()),
        };
        // 右边的最小宽度是 0：长的错误信息在里面折行，不把左边一栏挤窄
        row().width(pct(100)).height(pct(100)).background(Color::WHITE).child(side).child(column().grow(1.0).min_width(0).padding(28).gap(14).child(body))
    }
}

/// 按钮的样子
#[derive(Clone)]
struct Look {
    primary: Style,
    secondary: Style,
    indicator: Style,
    accent: Color,
}

impl Look {
    fn new(accent: Color) -> Self {
        let base = Style::new().height(34).padding_x(18).radius(6).align(Align::Center).justify(Justify::Center);
        // 悬停、按下都要写底色：默认的按钮这两个状态是浅灰底，白字就看不清了。复选框默认的悬停是另一种颜色
        let (hover, press) = (darker(accent, 0.9), darker(accent, 0.8));
        let primary = base.clone()
            + &Style::new()
                .background(accent)
                .border_color(accent)
                .color(Color::WHITE)
                .hovered(|s| s.background(hover).border_color(hover))
                .pressed(|s| s.background(press).border_color(press));
        let indicator = Style::new().checked(|s| {
            s.background(accent)
                .border_color(accent)
                .hovered(|s| s.background(hover).border_color(hover))
                .pressed(|s| s.background(press).border_color(press))
        });
        Self { primary, secondary: base, indicator, accent }
    }

    fn primary(&self, label: &str, on_click: impl Fn() + 'static) -> Element {
        button(label.to_owned()).style(&self.primary).on_click(on_click).name(format!("按钮：{label}"))
    }

    /// 复选框：勾选时是主色
    fn checkbox(&self, value: Signal<bool>, label: String) -> Checkbox {
        checkbox(value).label(label).indicator_style(&self.indicator)
    }

    fn secondary(&self, label: &str, on_click: impl Fn() + 'static) -> Element {
        button(label.to_owned()).style(&self.secondary).on_click(on_click).name(format!("按钮：{label}"))
    }
}

/// 压暗一点，透明度不变
fn darker(Color([r, g, b, a]): Color, factor: f32) -> Color {
    let scale = |channel: u8| (f32::from(channel) * factor) as u8;
    Color([scale(r), scale(g), scale(b), a])
}

/// 反斜杠后面可以折行：Windows 的路径里没有别的断点，长了就伸到窗口外面（只用来显示）
fn breakable_path(message: &str) -> String {
    message.replace('\\', "\\\u{200b}")
}

/// 左边一栏：图，或者主色上的第一个字、应用名、版本
fn side_panel(session: Session, accent: Color, picture: Option<ImageSource>, tagline: Option<String>) -> Element {
    let manifest = session.manifest();
    let panel = column().width(176).shrink(0).height(pct(100)).background(accent).padding(24).gap(10);
    if let Some(picture) = picture {
        return column().width(176).shrink(0).height(pct(100)).background(accent).child(image(picture).width(pct(100)).height(pct(100)).fit(ImageFit::Cover));
    }
    let initial: String = manifest.name.chars().take(1).collect();
    let panel = panel
        .child(
            row()
                .size(56, 56)
                .radius(14)
                .background(Color::WHITE.with_alpha(0.2))
                .align(Align::Center)
                .justify(Justify::Center)
                .child(text(initial).font_size(28).color(Color::WHITE)),
        )
        .child(text(manifest.name.clone()).font_size(18).color(Color::WHITE).margin_top(6))
        .child(text(manifest.version.clone()).font_size(12).color(Color::WHITE.with_alpha(0.75)));
    match tagline {
        Some(tagline) => panel.child(text(tagline).font_size(12).color(Color::WHITE.with_alpha(0.85)).margin_top(8)),
        None => panel,
    }
}

fn title(text_value: String) -> Element {
    text(text_value).font_size(20).name("标题")
}

fn muted(text_value: impl Into<String>) -> Element {
    text(text_value.into()).font_size(13).color(Color::hex(0x5b6470))
}

/// 底部的按钮：靠右一排，把上面撑开
fn footer() -> Element {
    row().gap(10).justify(Justify::End).align(Align::Center).margin_top(aureole::auto())
}

fn activity_text(activity: &Activity, texts: &Texts) -> String {
    match activity {
        Activity::Extracting(file) => format!("{} {file}", texts.extracting),
        Activity::Committing => texts.committing.clone(),
        Activity::Registering => texts.registering.clone(),
        Activity::Step(name) => name.clone(),
        Activity::Removing => texts.removing.clone(),
        Activity::Finishing => texts.finishing.clone(),
    }
}

/// 进度页：两种模式共用
fn progress_page(session: Session, look: &Look, texts: &std::rc::Rc<Texts>, heading: String, can_cancel: bool) -> Element {
    let accent = look.accent;
    let fill = Style::new().background(accent);
    let activity_texts = texts.clone();
    let page = column()
        .grow(1.0)
        .gap(14)
        .child(title(heading))
        .child(progress_bar(move || session.progress().get().fraction).fill_style(&fill).build().width(pct(100)).name("进度条"));
    let page = page.child(text(move || activity_text(&session.progress().get().activity, &activity_texts)).font_size(13).color(Color::hex(0x5b6470)).max_lines(1).name("正在做什么"));
    if can_cancel { page.child(footer().child(look.secondary(&texts.cancel, move || session.cancel()))) } else { page }
}

fn install_flow(session: Session, look: Look, texts: std::rc::Rc<Texts>, license: Option<String>, pages: Vec<Page>) -> Element {
    let manifest = session.manifest();
    let fill = move |template: &str| template.replace("{name}", &manifest.name).replace("{version}", &manifest.version);
    // 安装选项之前的页：协议、自己加的
    let before = usize::from(license.is_some()) + pages.len();
    let page = signal(0usize);
    let agreed = signal(license.is_none());

    let ready = {
        let (look, texts) = (look.clone(), texts.clone());
        let pages = std::rc::Rc::new(pages);
        let license = license.map(std::rc::Rc::new);
        let has_license = license.is_some();
        let mut flow = column().grow(1.0);
        if let Some(license) = license {
            let (look, texts) = (look.clone(), texts.clone());
            flow = flow.child_when(move || page.get() == 0, move || {
                let (next_look, next) = (look.clone(), texts.next.clone());
                column()
                    .grow(1.0)
                    .gap(12)
                    .child(title(texts.license_title.clone()))
                    .child(scroll_y(text(license.as_str().to_owned()).font_size(12)).grow(1.0).height(0).padding(10).border(1, Color::hex(0xdfe3e8)).radius(6).name("协议"))
                    .child(next_look.checkbox(agreed, texts.license_agree.clone()))
                    .child(footer().child(next_look.primary(&next, move || {
                        if agreed.get_untracked() {
                            page.set(1);
                        }
                    })))
            });
        }
        for (i, _) in pages.iter().enumerate() {
            let at = i + usize::from(has_license);
            let (look, texts, pages) = (look.clone(), texts.clone(), pages.clone());
            flow = flow.child_when(move || page.get() == at, move || {
                let mut buttons = footer();
                if at > 0 {
                    buttons = buttons.child(look.secondary(&texts.back, move || page.set(at - 1)));
                }
                column().grow(1.0).gap(12).child(pages[i](session).grow(1.0)).child(buttons.child(look.primary(&texts.next, move || page.set(at + 1))))
            });
        }
        let options = options_page(session, look, texts, page, before);
        flow.child_when(move || page.get() >= before, options)
    };

    let running = {
        let (look, texts, heading) = (look.clone(), texts.clone(), fill(&texts.installing));
        move || progress_page(session, &look, &texts, heading.clone(), true)
    };
    let done = {
        let (look, texts) = (look.clone(), texts.clone());
        move || {
            let launch = fill(&texts.launch);
            column()
                .grow(1.0)
                .gap(14)
                .child(title(fill(&texts.installed)))
                .child(look.checkbox(session.launch_after(), launch))
                .child(footer().child(look.primary(&texts.finish, move || session.finish())))
        }
    };
    let cancelled = {
        let (look, texts) = (look.clone(), texts.clone());
        move || {
            column()
                .grow(1.0)
                .gap(14)
                .child(title(texts.cancelled.clone()))
                .child(muted(texts.cancelled_note.clone()))
                .child(footer().child(look.secondary(&texts.close, move || session.finish())).child(look.primary(&texts.retry, move || session.start())))
        }
    };
    let failed = {
        let (look, texts) = (look.clone(), texts.clone());
        move || {
            column()
                .grow(1.0)
                .gap(14)
                .child(title(texts.failed.clone()))
                .child(text(move || match session.state().get() {
                    State::Failed { message, .. } => breakable_path(&message),
                    _ => String::new(),
                }).font_size(13).color(Color::hex(0xb3261e)).name("原因"))
                .child(footer().child(look.secondary(&texts.close, move || session.finish())).child(look.primary(&texts.retry, move || session.start())))
        }
    };
    by_state(session, ready, running, done, cancelled, failed)
}

/// 选目录、快捷方式
fn options_page(session: Session, look: Look, texts: std::rc::Rc<Texts>, page: Signal<usize>, before: usize) -> impl Fn() -> Element + 'static {
    move || {
        let manifest = session.manifest();
        let fill = |template: &str| template.replace("{name}", &manifest.name).replace("{version}", &manifest.version);
        let space = texts.space_needed.replace("{size}", &session.size_text());
        let upgrade = fill(&texts.upgrade_note);
        let reinstall = fill(&texts.reinstall_note);
        let version = manifest.version.clone();
        let mut buttons = footer();
        if before > 0 {
            buttons = buttons.child(look.secondary(&texts.back, move || page.set(before - 1)));
        }
        let shortcuts = column().gap(8).child(look.checkbox(session.menu_shortcut(), texts.menu_shortcut.clone()));
        let shortcuts = if cfg!(windows) { shortcuts.child(look.checkbox(session.desktop_shortcut(), texts.desktop_shortcut.clone())) } else { shortcuts };
        column()
            .grow(1.0)
            .gap(14)
            .child(title(fill(&texts.install_title)))
            .child_when(
                move || session.previous_version().get().is_some(),
                move || {
                    let previous = session.previous_version().get().unwrap_or_default();
                    let note = if previous == version { reinstall.clone() } else { upgrade.replace("{previous}", &previous) };
                    muted(note).name("升级说明")
                },
            )
            .child(
                column().gap(6).child(muted(texts.location.clone())).child(
                    row()
                        .gap(8)
                        .align(Align::Center)
                        .child(
                            row()
                                .grow(1.0)
                                .width(0)
                                .height(34)
                                .padding_x(10)
                                .radius(6)
                                .align(Align::Center)
                                .border(1, Color::hex(0xdfe3e8))
                                .child(text(move || session.directory().get().display().to_string()).font_size(13).max_lines(1).name("安装位置")),
                        )
                        .child(look.secondary(&texts.change, move || session.choose_directory())),
                ),
            )
            .child(muted(space))
            .child(shortcuts)
            .child(buttons.child(look.primary(&texts.install, move || session.start())))
    }
}

fn uninstall_flow(session: Session, look: Look, texts: std::rc::Rc<Texts>) -> Element {
    let manifest = session.manifest();
    let fill = move |template: &str| template.replace("{name}", &manifest.name).replace("{version}", &manifest.version);
    let ready = {
        let (look, texts) = (look.clone(), texts.clone());
        column()
            .grow(1.0)
            .gap(14)
            .child(title(fill(&texts.uninstall_title)))
            .child(muted(texts.uninstall_note.clone()))
            .child(text(move || session.directory().get().display().to_string()).font_size(13).name("安装位置"))
            .child(footer().child(look.secondary(&texts.cancel, move || session.finish())).child(look.primary(&texts.uninstall, move || session.start())))
    };
    let running = {
        let (look, texts, heading) = (look.clone(), texts.clone(), fill(&texts.uninstalling));
        move || progress_page(session, &look, &texts, heading.clone(), false)
    };
    let done = {
        let (look, texts) = (look.clone(), texts.clone());
        move || column().grow(1.0).gap(14).child(title(fill(&texts.uninstalled))).child(footer().child(look.primary(&texts.finish, move || session.finish())))
    };
    let failed = {
        let (look, texts) = (look.clone(), texts.clone());
        move || {
            column()
                .grow(1.0)
                .gap(14)
                .child(title(texts.failed.clone()))
                .child(text(move || match session.state().get() {
                    State::Failed { message, .. } => message,
                    _ => String::new(),
                }).font_size(13).color(Color::hex(0xb3261e)).name("原因"))
                .child(footer().child(look.secondary(&texts.close, move || session.finish())).child(look.primary(&texts.retry, move || session.start())))
        }
    };
    let cancelled = {
        let (look, texts) = (look, texts);
        move || column().grow(1.0).gap(14).child(title(texts.cancelled.clone())).child(footer().child(look.primary(&texts.close, move || session.finish())))
    };
    by_state(session, ready, running, done, cancelled, failed)
}

/// 按状态显示哪一页
fn by_state(
    session: Session,
    ready: Element,
    running: impl Fn() -> Element + 'static,
    done: impl Fn() -> Element + 'static,
    cancelled: impl Fn() -> Element + 'static,
    failed: impl Fn() -> Element + 'static,
) -> Element {
    let ready = std::cell::RefCell::new(Some(ready));
    let is = move |wanted: fn(&State) -> bool| move || wanted(&session.state().get());
    column()
        .grow(1.0)
        .width(pct(100))
        .child_when(is(|s| *s == State::Ready), move || ready.borrow_mut().take().unwrap_or_else(column))
        .child_when(is(|s| *s == State::Running), running)
        .child_when(is(|s| *s == State::Done), done)
        .child_when(is(|s| *s == State::Cancelled), cancelled)
        .child_when(is(|s| matches!(s, State::Failed { .. })), failed)
}
