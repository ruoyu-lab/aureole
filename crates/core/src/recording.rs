//! 录下的输入：每行一个 JSON（第一行是文件头，带版本号、窗口大小和缩放），写和读都在这里。
//! 真实窗口设了 `AUREOLE_RECORD=文件` 时录下窗口宿主收到的每一次输入（见 `app.rs`），测试工具用 [`Recording`] 读回来回放。
//!
//! ```text
//! {"format":"aureole-input","version":1,"size":[960,640],"scale":2,"platform":"macos"}
//! {"t":0.512,"type":"pointer_move","pos":[120,48]}
//! {"t":0.6,"type":"pointer_down","pos":[120,48],"button":"left","buttons":["left"]}
//! {"t":1.25,"type":"key_down","key":{"char":"a"},"code":"KeyA","text":"a"}
//! {"t":3.5,"type":"end"}
//! ```
//!
//! 时间是秒（统一时钟的读数，窗口打开时接近 0），坐标是窗口坐标、逻辑像素。没有的字段省略：没按修饰键时没有 `mods`，
//! 不是重复的按键没有 `repeat`。读的时候认不出的字段不管，认不出的种类报错

use std::fs::File;
use std::io::{self, LineWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::event::{ExternalDataKinds, ExternalDropData};
use crate::image::RgbaImage;
use crate::input::{Buttons, GestureInput, Ime, InputEvent, InputKind, Key, KeyInput, Modifiers, MouseButton, NamedKey, PhysicalKey, PointerId, PointerInput, PointerKind, GesturePhase, WheelDelta, WheelInput};

/// 文件头里的格式名
const FORMAT: &str = "aureole-input";
/// 格式的版本：字段的含义变了时加一。第 3 版多了手指：`pointer` 是 `touch` 的指针事件是一根手指，`id` 是它的编号
/// （旧代码读不出 `id`，会把几根手指当成一个指针，所以不认新版本的文件）
pub(crate) const VERSION: u64 = 3;

/// 写录制文件
pub struct Recorder {
    out: LineWriter<File>,
    path: PathBuf,
    events: usize,
    /// 写失败过（只提示一次，之后不再写）
    failed: bool,
}

impl Recorder {
    /// 新建（覆盖）录制文件，写上文件头：窗口此刻的大小（逻辑像素）、缩放；`platform` 是录制时的系统
    /// （真实窗口里的录制带着系统的输入习惯，只在同一种系统上回放；测试工具生成的不填）
    pub fn create(path: &Path, size: [f32; 2], scale: f32, platform: Option<&str>) -> io::Result<Self> {
        let mut out = LineWriter::new(File::create(path)?);
        let mut header = vec![("format", json!(FORMAT)), ("version", json!(VERSION)), ("size", pair(size)), ("scale", json!(scale))];
        if let Some(platform) = platform {
            header.push(("platform", json!(platform)));
        }
        writeln!(out, "{}", line(&header))?;
        Ok(Self { out, path: path.to_owned(), events: 0, failed: false })
    }

    /// 录制文件的位置
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 写一次输入（每行写完就落盘：进程崩溃时已经录下的不丢）
    pub fn write(&mut self, event: &InputEvent) {
        if self.failed {
            return;
        }
        if let Err(error) = writeln!(self.out, "{}", to_line(event)) {
            self.failed = true;
            tracing::error!(path = %self.path.display(), "录制输入失败：{error}");
        }
        self.events += 1;
    }

    /// 写上结束的时间（回放推进到这里为止），返回一共录了几次输入
    pub fn finish(mut self, time: f64) -> usize {
        if !self.failed {
            let _ = writeln!(self.out, "{}", line(&[("t", json!(round_time(time))), ("type", json!("end"))]));
            let _ = self.out.flush();
        }
        self.events
    }
}

/// 读回来的录制
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    /// 开始录制时窗口的大小，逻辑像素
    pub size: [f32; 2],
    /// 开始录制时窗口的缩放
    pub scale: f32,
    /// 录制时的系统（测试工具生成的为 `None`）
    pub platform: Option<String>,
    /// 录下的每一次输入，按时间先后
    pub events: Vec<InputEvent>,
    /// 结束的时间：有结束标记时是它，没有（进程没正常结束）时是最后一次输入的时间
    pub end: f64,
}

impl Recording {
    /// 读一个录制文件；读不了、格式不对时是说明原因的错误
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读不了 {}：{e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}：{e}", path.display()))
    }

    /// 解析录制文件的内容
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty());
        let (_, first) = lines.next().ok_or("空文件")?;
        let header: Value = serde_json::from_str(first).map_err(|e| format!("第 1 行：{e}"))?;
        if header["format"] != FORMAT {
            return Err("第 1 行不是录制文件的文件头".into());
        }
        let version = header["version"].as_u64().ok_or("文件头没有版本号")?;
        if version > VERSION {
            return Err(format!("录制文件的版本 {version} 比这里认得的（{VERSION}）新"));
        }
        let size = point(&header["size"]).ok_or("文件头没有窗口大小")?;
        let scale = header["scale"].as_f64().unwrap_or(1.0) as f32;
        let platform = header["platform"].as_str().map(str::to_owned);
        let mut events = Vec::new();
        let mut end = None;
        for (i, line) in lines {
            let value: Value = serde_json::from_str(line).map_err(|e| format!("第 {} 行：{e}", i + 1))?;
            if value["type"] == "end" {
                end = value["t"].as_f64();
                break;
            }
            let mut event = from_json(&value).map_err(|e| format!("第 {} 行：{e}", i + 1))?;
            // 第 1 版的旋转是角度（第 2 版起和窗口换算出来的一样是弧度）
            if let (1, InputKind::Rotate(g)) = (version, &mut event.kind) {
                g.delta = g.delta.to_radians();
            }
            events.push(event);
        }
        let end = end.or_else(|| events.last().map(|e| crate::timer::seconds(e.time))).unwrap_or(0.0);
        Ok(Self { size, scale, platform, events, end })
    }
}

/// 时间取到微秒：文件好读，回放用不着更细
fn round_time(t: f64) -> f64 {
    (t * 1e6).round() / 1e6
}

/// 一行 JSON，字段按给的顺序（时间、种类在前面，读起来顺）
fn line(fields: &[(&str, Value)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json!(key).to_string());
        out.push(':');
        out.push_str(&value.to_string());
    }
    out.push('}');
    out
}

fn to_line(event: &InputEvent) -> String {
    let mut fields = Vec::new();
    let mut put = |key: &'static str, value: Value| fields.push((key, value));
    let kind = match &event.kind {
        InputKind::PointerDown(p) => pointer(&mut put, p, "pointer_down"),
        InputKind::PointerUp(p) => pointer(&mut put, p, "pointer_up"),
        InputKind::PointerMove(p) => pointer(&mut put, p, "pointer_move"),
        InputKind::PointerEnter(p) => pointer(&mut put, p, "pointer_enter"),
        InputKind::PointerLeave(p) => pointer(&mut put, p, "pointer_leave"),
        InputKind::PointerCancel(p) => pointer(&mut put, p, "pointer_cancel"),
        InputKind::Wheel(w) => {
            match w.delta {
                WheelDelta::Lines(d) => put("lines", pair(d)),
                WheelDelta::Pixels(d) => put("pixels", pair(d)),
            }
            put("phase", json!(phase_name(w.phase)));
            put("pos", w.position.map_or(Value::Null, pair));
            modifiers(&mut put, w.modifiers);
            "wheel"
        }
        InputKind::Pinch(g) => gesture(&mut put, g, "pinch"),
        InputKind::Rotate(g) => gesture(&mut put, g, "rotate"),
        InputKind::KeyDown(k) => key_input(&mut put, k, "key_down"),
        InputKind::KeyUp(k) => key_input(&mut put, k, "key_up"),
        InputKind::Modifiers(m) => {
            put("mods", modifier_names(*m));
            "modifiers"
        }
        InputKind::Ime(Ime::Enabled) => "ime_enabled",
        InputKind::Ime(Ime::Preedit(text, cursor)) => {
            put("text", json!(text));
            put("cursor", cursor.map_or(Value::Null, |(a, b)| json!([a, b])));
            "ime_preedit"
        }
        InputKind::Ime(Ime::Commit(text)) => {
            put("text", json!(text));
            "ime_commit"
        }
        InputKind::Ime(Ime::Disabled) => "ime_disabled",
        InputKind::WindowFocus(focused) => {
            put("focused", json!(focused));
            "window_focus"
        }
        InputKind::FileHover(position) => {
            put("pos", position.map_or(Value::Null, pair));
            "file_hover"
        }
        InputKind::FileDrop { position, paths } => {
            put("pos", position.map_or(Value::Null, pair));
            put("paths", paths.iter().map(|p| json!(p.to_string_lossy())).collect());
            "file_drop"
        }
        InputKind::FileCancel => "file_cancel",
        InputKind::ExternalDragOver { position, kinds } => {
            put("pos", pair(*position));
            put("kinds", kind_names(*kinds));
            "external_drag_over"
        }
        InputKind::ExternalDragLeave => "external_drag_leave",
        InputKind::ExternalDrop { position, data } => {
            put("pos", pair(*position));
            if let Some(text) = &data.text {
                put("text", json!(text));
            }
            if let Some(link) = &data.link {
                put("link", json!(link));
            }
            // 图片只记大小：录制文件不装用户的图片，回放时是一张同样大小的全透明的图
            if let Some(image) = &data.image {
                put("image", json!([image.width, image.height]));
            }
            if !data.files.is_empty() {
                put("paths", data.files.iter().map(|p| json!(p.to_string_lossy())).collect());
            }
            "external_drop"
        }
        InputKind::Resize { size, scale_factor } => {
            put("size", pair(*size));
            put("scale", json!(scale_factor));
            "resize"
        }
    };
    let mut all = vec![("t", json!(round_time(crate::timer::seconds(event.time)))), ("type", json!(kind))];
    all.extend(fields);
    line(&all)
}

fn pair([x, y]: [f32; 2]) -> Value {
    json!([x, y])
}

fn pointer(put: &mut impl FnMut(&'static str, Value), p: &PointerInput, kind: &'static str) -> &'static str {
    put("pos", p.position.map_or(Value::Null, pair));
    if let Some(button) = p.button {
        put("button", button_name(button));
    }
    if !p.buttons.is_empty() {
        put("buttons", p.buttons.iter().map(|b| button_name(button_of(b))).collect());
    }
    modifiers(put, p.modifiers);
    if p.device != PointerKind::Mouse {
        put("pointer", json!(pointer_name(p.device)));
    }
    if p.pointer_id != PointerId::MOUSE {
        put("id", json!(p.pointer_id.0));
    }
    kind
}

fn gesture(put: &mut impl FnMut(&'static str, Value), g: &GestureInput, kind: &'static str) -> &'static str {
    put("delta", json!(g.delta));
    put("phase", json!(phase_name(g.phase)));
    put("pos", g.position.map_or(Value::Null, pair));
    kind
}

fn key_input(put: &mut impl FnMut(&'static str, Value), k: &KeyInput, kind: &'static str) -> &'static str {
    put("key", key_json(&k.key));
    if let Some(base) = &k.base {
        put("base", key_json(base));
    }
    if let Some(physical) = &k.physical {
        put("code", json!(physical.name()));
    }
    if let Some(text) = &k.text {
        put("text", json!(text.as_str()));
    }
    modifiers(put, k.modifiers);
    if k.repeated {
        put("repeat", json!(true));
    }
    kind
}

fn modifiers(put: &mut impl FnMut(&'static str, Value), modifiers: Modifiers) {
    if !modifiers.is_empty() {
        put("mods", modifier_names(modifiers));
    }
}

fn modifier_names(modifiers: Modifiers) -> Value {
    let names = [(Modifiers::SHIFT, "shift"), (Modifiers::CONTROL, "ctrl"), (Modifiers::ALT, "alt"), (Modifiers::SUPER, "cmd")];
    names.iter().filter(|(m, _)| modifiers.contains(*m)).map(|(_, name)| json!(name)).collect()
}

fn key_json(key: &Key) -> Value {
    match key {
        Key::Named(named) => json!({ "named": named.name() }),
        Key::Character(c) => json!({ "char": c.as_str() }),
        Key::Dead(c) => json!({ "dead": c.map(String::from) }),
        Key::Unidentified => json!("unidentified"),
    }
}

fn button_name(button: MouseButton) -> Value {
    match button {
        MouseButton::Left => json!("left"),
        MouseButton::Right => json!("right"),
        MouseButton::Middle => json!("middle"),
        MouseButton::Back => json!("back"),
        MouseButton::Forward => json!("forward"),
        MouseButton::Other(n) => json!(n),
    }
}

fn button_of(bit: Buttons) -> MouseButton {
    match bit {
        Buttons::RIGHT => MouseButton::Right,
        Buttons::MIDDLE => MouseButton::Middle,
        Buttons::BACK => MouseButton::Back,
        Buttons::FORWARD => MouseButton::Forward,
        _ => MouseButton::Left,
    }
}

fn pointer_name(kind: PointerKind) -> &'static str {
    match kind {
        PointerKind::Mouse => "mouse",
        PointerKind::Trackpad => "trackpad",
        PointerKind::Pen => "pen",
        PointerKind::Touch => "touch",
    }
}

fn phase_name(phase: GesturePhase) -> &'static str {
    match phase {
        GesturePhase::Start => "started",
        GesturePhase::Move => "moved",
        GesturePhase::End => "ended",
        GesturePhase::Cancel => "cancelled",
    }
}

// ---- 读 ----

fn from_json(v: &Value) -> Result<InputEvent, String> {
    let time = v["t"].as_f64().ok_or("没有时间 t")?;
    let kind = v["type"].as_str().ok_or("没有种类 type")?;
    let pos = || point(&v["pos"]);
    let kind = match kind {
        "pointer_down" => InputKind::PointerDown(read_pointer(v)?),
        "pointer_up" => InputKind::PointerUp(read_pointer(v)?),
        "pointer_move" => InputKind::PointerMove(read_pointer(v)?),
        "pointer_enter" => InputKind::PointerEnter(read_pointer(v)?),
        "pointer_leave" => InputKind::PointerLeave(read_pointer(v)?),
        "pointer_cancel" => InputKind::PointerCancel(read_pointer(v)?),
        "wheel" => {
            let delta = match (point(&v["lines"]), point(&v["pixels"])) {
                (Some(lines), _) => WheelDelta::Lines(lines),
                (None, Some(pixels)) => WheelDelta::Pixels(pixels),
                _ => return Err("滚轮没有 lines 或 pixels".into()),
            };
            InputKind::Wheel(WheelInput { delta, phase: read_phase(v)?, position: pos(), modifiers: read_modifiers(&v["mods"])? })
        }
        "pinch" | "rotate" => {
            let gesture = GestureInput { delta: number(&v["delta"])?, phase: read_phase(v)?, position: pos() };
            if kind == "pinch" { InputKind::Pinch(gesture) } else { InputKind::Rotate(gesture) }
        }
        "key_down" => InputKind::KeyDown(read_key_input(v)?),
        "key_up" => InputKind::KeyUp(read_key_input(v)?),
        "modifiers" => InputKind::Modifiers(read_modifiers(&v["mods"])?),
        "ime_enabled" => InputKind::Ime(Ime::Enabled),
        "ime_preedit" => {
            let cursor = match &v["cursor"] {
                Value::Array(c) if c.len() == 2 => Some((index(&c[0])?, index(&c[1])?)),
                _ => None,
            };
            InputKind::Ime(Ime::Preedit(string(&v["text"])?, cursor))
        }
        "ime_commit" => InputKind::Ime(Ime::Commit(string(&v["text"])?)),
        "ime_disabled" => InputKind::Ime(Ime::Disabled),
        "window_focus" => InputKind::WindowFocus(v["focused"].as_bool().ok_or("没有 focused")?),
        "file_hover" => InputKind::FileHover(pos()),
        "file_drop" => {
            let paths = v["paths"].as_array().ok_or("没有 paths")?;
            let paths = paths.iter().map(|p| p.as_str().map(PathBuf::from).ok_or("文件名不是字符串")).collect::<Result<_, _>>()?;
            InputKind::FileDrop { position: pos(), paths }
        }
        "file_cancel" => InputKind::FileCancel,
        "external_drag_over" => InputKind::ExternalDragOver {
            position: point(&v["pos"]).ok_or("没有 pos")?,
            kinds: parse_kinds(&v["kinds"])?,
        },
        "external_drag_leave" => InputKind::ExternalDragLeave,
        "external_drop" => InputKind::ExternalDrop { position: point(&v["pos"]).ok_or("没有 pos")?, data: parse_drop_data(v)? },
        "resize" => InputKind::Resize { size: point(&v["size"]).ok_or("没有 size")?, scale_factor: number(&v["scale"])? },
        other => return Err(format!("认不出的事件 {other}")),
    };
    Ok(InputEvent { time: std::time::Duration::from_secs_f64(time.max(0.0)), kind })
}

const KIND_NAMES: [(ExternalDataKinds, &str); 4] =
    [(ExternalDataKinds::TEXT, "text"), (ExternalDataKinds::LINK, "link"), (ExternalDataKinds::IMAGE, "image"), (ExternalDataKinds::FILES, "files")];

fn kind_names(kinds: ExternalDataKinds) -> Value {
    KIND_NAMES.iter().filter(|(kind, _)| kinds.contains(*kind)).map(|(_, name)| json!(name)).collect()
}

fn parse_kinds(v: &Value) -> Result<ExternalDataKinds, String> {
    let names = v.as_array().ok_or("没有 kinds")?;
    let mut kinds = ExternalDataKinds::empty();
    for name in names {
        let name = name.as_str().ok_or("kinds 里不是字符串")?;
        kinds |= KIND_NAMES.iter().find(|(_, n)| *n == name).ok_or_else(|| format!("认不出的内容种类 {name}"))?.0;
    }
    Ok(kinds)
}

fn parse_drop_data(v: &Value) -> Result<ExternalDropData, String> {
    let image = match &v["image"] {
        Value::Null => None,
        size => {
            let [width, height] = point(size).ok_or("image 不是大小")?.map(|n| n as u32);
            // 坏数据不让这里分配大块内存
            let bytes = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).filter(|n| *n <= 1 << 30).ok_or("图片太大")?;
            Some(RgbaImage::new(width, height, vec![0; bytes]))
        }
    };
    let files = match v["paths"].as_array() {
        Some(paths) => paths.iter().map(|p| p.as_str().map(PathBuf::from).ok_or("文件名不是字符串")).collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    Ok(ExternalDropData { text: v["text"].as_str().map(str::to_owned), link: v["link"].as_str().map(str::to_owned), image, files })
}

fn number(v: &Value) -> Result<f32, String> {
    v.as_f64().map(|n| n as f32).ok_or_else(|| format!("{v} 不是数"))
}

fn index(v: &Value) -> Result<usize, String> {
    v.as_u64().map(|n| n as usize).ok_or_else(|| format!("{v} 不是非负整数"))
}

fn string(v: &Value) -> Result<String, String> {
    v.as_str().map(str::to_owned).ok_or_else(|| format!("{v} 不是字符串"))
}

fn point(v: &Value) -> Option<[f32; 2]> {
    let a = v.as_array().filter(|a| a.len() == 2)?;
    Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32])
}

fn read_pointer(v: &Value) -> Result<PointerInput, String> {
    let button = match &v["button"] {
        Value::Null => None,
        b => Some(read_button(b)?),
    };
    let mut buttons = Buttons::empty();
    for b in v["buttons"].as_array().into_iter().flatten() {
        buttons |= Buttons::from_button(read_button(b)?);
    }
    let pointer = match v["pointer"].as_str() {
        None | Some("mouse") => PointerKind::Mouse,
        Some("trackpad") => PointerKind::Trackpad,
        Some("pen") => PointerKind::Pen,
        Some("touch") => PointerKind::Touch,
        Some(other) => return Err(format!("认不出的指针设备 {other}")),
    };
    // 第 3 版之前没有编号：都是同一个指针
    let pointer_id = PointerId(v["id"].as_u64().unwrap_or(PointerId::MOUSE.0));
    Ok(PointerInput { position: point(&v["pos"]), button, buttons, modifiers: read_modifiers(&v["mods"])?, device: pointer, pointer_id })
}

fn read_button(v: &Value) -> Result<MouseButton, String> {
    Ok(match v {
        Value::String(s) => match s.as_str() {
            "left" => MouseButton::Left,
            "right" => MouseButton::Right,
            "middle" => MouseButton::Middle,
            "back" => MouseButton::Back,
            "forward" => MouseButton::Forward,
            other => return Err(format!("认不出的键 {other}")),
        },
        n => MouseButton::Other(n.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or_else(|| format!("认不出的键 {n}"))?),
    })
}

fn read_phase(v: &Value) -> Result<GesturePhase, String> {
    match v["phase"].as_str() {
        Some("started") => Ok(GesturePhase::Start),
        Some("moved") => Ok(GesturePhase::Move),
        Some("ended") => Ok(GesturePhase::End),
        Some("cancelled") => Ok(GesturePhase::Cancel),
        other => Err(format!("认不出的阶段 {other:?}")),
    }
}

fn read_modifiers(v: &Value) -> Result<Modifiers, String> {
    let mut modifiers = Modifiers::empty();
    for m in v.as_array().into_iter().flatten() {
        modifiers |= match m.as_str() {
            Some("shift") => Modifiers::SHIFT,
            Some("ctrl") => Modifiers::CONTROL,
            Some("alt") => Modifiers::ALT,
            Some("cmd") => Modifiers::SUPER,
            _ => return Err(format!("认不出的修饰键 {m}")),
        };
    }
    Ok(modifiers)
}

fn read_key(v: &Value) -> Result<Key, String> {
    if v == "unidentified" {
        return Ok(Key::Unidentified);
    }
    if let Some(named) = v["named"].as_str() {
        // 以后的版本多认的键，这里当作认不出的键
        return Ok(NamedKey::from_name(named).map_or(Key::Unidentified, Key::Named));
    }
    if let Some(c) = v["char"].as_str() {
        return Ok(Key::Character(c.into()));
    }
    if let Some(dead) = v.get("dead") {
        return Ok(Key::Dead(dead.as_str().and_then(|s| s.chars().next())));
    }
    Err(format!("认不出的按键 {v}"))
}

fn read_key_input(v: &Value) -> Result<KeyInput, String> {
    Ok(KeyInput {
        key: read_key(&v["key"])?,
        base: if v["base"].is_null() { None } else { Some(read_key(&v["base"])?) },
        physical: v["code"].as_str().map(PhysicalKey::new),
        text: v["text"].as_str().map(Into::into),
        modifiers: read_modifiers(&v["mods"])?,
        repeated: v["repeat"].as_bool().unwrap_or(false),
    })
}
