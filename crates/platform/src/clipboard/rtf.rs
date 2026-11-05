//! 带格式文字的交换格式 RTF：写出和读入。不调用系统（读入时 `\'hh` 的代码页转换除外，见 [`decode_code_page`]），
//! 在任何平台上都能测试。
//!
//! 只认文字和字符样式：粗体、斜体、下划线、删除线、字号、颜色、字体、链接、换段。段落样式、表格、图片、页眉页脚不认，
//! 里面的文字照常取出来（图片、页眉页脚这些非正文的部分跳过）。

use std::fmt::Write as _;
use std::rc::Rc;

/// 一段样式相同的文字
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RtfRun {
    /// 文字，换段是 `\n`
    pub text: String,
    /// 粗体
    pub bold: bool,
    /// 斜体
    pub italic: bool,
    /// 下划线
    pub underline: bool,
    /// 删除线
    pub strikethrough: bool,
    /// 等宽字体
    pub monospaced: bool,
    /// 字号（点）；没写时跟着接收方的默认
    pub font_size: Option<f32>,
    /// 颜色（sRGB）；没写时跟着接收方的默认
    pub color: Option<[u8; 3]>,
    /// 字体族名；正文的默认字体（字体表的第一项）和等宽字体不算
    pub font_family: Option<String>,
    /// 链接的网址
    pub link: Option<String>,
}

impl RtfRun {
    fn same_style(&self, other: &Self) -> bool {
        self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strikethrough == other.strikethrough
            && self.monospaced == other.monospaced
            && self.font_size == other.font_size
            && self.color == other.color
            && self.font_family == other.font_family
            && self.link == other.link
    }
}

/// 正文默认的字体（字体表第一项）和等宽字体（第二项）：两个平台都有
const BODY_FONT: &str = "Helvetica";
const MONOSPACED_FONT: &str = "Courier New";

/// 写成 RTF。非 ASCII 的字一律写成 `\uN`，不依赖代码页
pub fn write(runs: &[RtfRun]) -> Vec<u8> {
    let mut families: Vec<&str> = Vec::new();
    let mut colors: Vec<[u8; 3]> = Vec::new();
    for run in runs {
        if let Some(family) = run.font_family.as_deref()
            && !families.contains(&family)
        {
            families.push(family);
        }
        if let Some(color) = run.color
            && !colors.contains(&color)
        {
            colors.push(color);
        }
    }
    let mut out = String::from("{\\rtf1\\ansi\\ansicpg1252\\uc0\\deff0{\\fonttbl");
    let _ = write!(out, "{{\\f0\\fswiss {BODY_FONT};}}{{\\f1\\fmodern {MONOSPACED_FONT};}}");
    for (index, family) in families.iter().enumerate() {
        let _ = write!(out, "{{\\f{}\\fnil ", index + 2);
        escape(&mut out, family);
        out.push_str(";}");
    }
    out.push('}');
    if !colors.is_empty() {
        out.push_str("{\\colortbl;");
        for [r, g, b] in &colors {
            let _ = write!(out, "\\red{r}\\green{g}\\blue{b};");
        }
        out.push('}');
    }
    for run in runs {
        out.push('{');
        if let Some(link) = &run.link {
            out.push_str("\\field{\\*\\fldinst{HYPERLINK \"");
            escape(&mut out, &link.replace('"', "%22"));
            out.push_str("\"}}{\\fldrslt{");
        }
        if run.monospaced {
            out.push_str("\\f1 ");
        } else if let Some(family) = run.font_family.as_deref() {
            let index = families.iter().position(|f| *f == family).unwrap_or(0) + 2;
            let _ = write!(out, "\\f{index} ");
        }
        if run.bold {
            out.push_str("\\b ");
        }
        if run.italic {
            out.push_str("\\i ");
        }
        if run.underline {
            out.push_str("\\ul ");
        }
        if run.strikethrough {
            out.push_str("\\strike ");
        }
        if let Some(size) = run.font_size {
            let _ = write!(out, "\\fs{} ", (size * 2.0).round() as i32);
        }
        if let Some(color) = run.color {
            let index = colors.iter().position(|c| *c == color).unwrap_or(0) + 1;
            let _ = write!(out, "\\cf{index} ");
        }
        escape(&mut out, &run.text);
        if run.link.is_some() {
            out.push_str("}}");
        }
        out.push('}');
    }
    out.push('}');
    out.into_bytes()
}

fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\\' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\par "),
            '\r' => {}
            '\t' => out.push_str("\\tab "),
            c if c.is_ascii() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{} ", *unit as i16);
                }
            }
        }
    }
}

/// 读入 RTF，代码页转换用 [`decode_code_page`]。认不出的控制字跳过，坏数据不会出错，能取多少取多少
pub fn parse(rtf: &[u8]) -> Vec<RtfRun> {
    parse_with(rtf, &decode_code_page)
}

/// 读入 RTF，`decode` 把这个代码页的字节换成文字（换不了时是 `None`，退回 [`decode_fallback`]）
pub fn parse_with(rtf: &[u8], decode: &dyn Fn(u32, &[u8]) -> Option<String>) -> Vec<RtfRun> {
    let mut parser = Parser::new(decode);
    parser.run(rtf);
    parser.finish()
}

/// 这个平台上把代码页的字节换成文字：macOS 用 `CFString` 的转换表，Windows 用 `MultiByteToWideChar`，
/// 别的系统不用它（Linux 的剪贴板不走 RTF），只有 UTF-8
pub fn decode_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    #[cfg(target_os = "macos")]
    return super::macos::decode_code_page(code_page, bytes);
    #[cfg(target_os = "windows")]
    return super::windows::decode_code_page(code_page, bytes);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (code_page, bytes);
        None
    }
}

/// 系统转换不了时：UTF-8 按 UTF-8，别的都按 Windows-1252（ASCII 和西欧文字）
pub fn decode_fallback(code_page: u32, bytes: &[u8]) -> String {
    if code_page == 65001 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    /// 0x80..0xA0 这一段和 Latin-1 不一样
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•',
        '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9f => HIGH[usize::from(b - 0x80)],
            b => char::from(b),
        })
        .collect()
}

/// 字符集编号（`\fcharsetN`）对应的代码页；0、1、2 是"跟着文档"
fn code_page_of_charset(charset: u32) -> Option<u32> {
    Some(match charset {
        77 => 10000,
        128 => 932,
        129 => 949,
        130 => 1361,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        _ => return None,
    })
}

/// 一个字可能占两个字节的代码页
fn is_double_byte(code_page: u32) -> bool {
    matches!(code_page, 932 | 936 | 949 | 950 | 1361)
}

/// 一组里的字符样式：进组时复制，出组时恢复
#[derive(Clone)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    font_size: Option<f32>,
    color: usize,
    font: Option<i32>,
    /// `\uN` 后面跳过几个退路字符
    fallback: usize,
    link: Option<Rc<str>>,
    destination: Destination,
}

/// 文字现在归谁
#[derive(Clone, Copy, PartialEq)]
enum Destination {
    Body,
    FontTable,
    ColorTable,
    FieldInstruction,
    Skip,
}

#[derive(Default)]
struct Font {
    id: i32,
    name: Vec<u8>,
    charset: Option<u32>,
    modern: bool,
    /// 字体表第一项：正文的默认字体
    first: bool,
}

impl Font {
    fn is_monospaced(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.modern || ["courier", "menlo", "monaco", "consolas", "sf mono", "lucida console"].iter().any(|m| lower.contains(m))
    }
}

/// 组的嵌套太深就不读了（坏数据不把栈撑大）
const MAX_DEPTH: usize = 256;

struct Parser<'a> {
    decode: &'a dyn Fn(u32, &[u8]) -> Option<String>,
    style: Style,
    stack: Vec<Style>,
    runs: Vec<RtfRun>,
    /// 文档的代码页（`\ansicpgN`）
    code_page: u32,
    fonts: Vec<Font>,
    /// 字体表里正在读的那一项
    font_entry: Option<usize>,
    colors: Vec<Option<[u8; 3]>>,
    pending_color: [u8; 3],
    color_set: bool,
    /// 还没换成文字的字节（`\'hh` 和原样写的高位字节）
    bytes: Vec<u8>,
    /// 样式相同、还没放进 `runs` 的文字：样式只在控制字、组的边界上变
    text: String,
    /// `\uN` 里没配对的高位代理
    high_surrogate: Option<u16>,
    /// 下一个控制字前面有 `\*`：认不出就跳过整组
    ignorable: bool,
    field_instruction: String,
    /// 文档以 `\par` 结束：最后的换段不算文字
    ends_with_paragraph: bool,
}

impl<'a> Parser<'a> {
    fn new(decode: &'a dyn Fn(u32, &[u8]) -> Option<String>) -> Self {
        Self {
            decode,
            style: Style {
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                font_size: None,
                color: 0,
                font: None,
                fallback: 1,
                link: None,
                destination: Destination::Body,
            },
            stack: Vec::new(),
            runs: Vec::new(),
            code_page: 1252,
            fonts: Vec::new(),
            font_entry: None,
            colors: Vec::new(),
            pending_color: [0; 3],
            color_set: false,
            bytes: Vec::new(),
            text: String::new(),
            high_surrogate: None,
            ignorable: false,
            field_instruction: String::new(),
            ends_with_paragraph: false,
        }
    }

    fn run(&mut self, rtf: &[u8]) {
        let mut at = 0;
        while let Some(&byte) = rtf.get(at) {
            match byte {
                b'{' => {
                    self.flush();
                    if self.stack.len() >= MAX_DEPTH {
                        return;
                    }
                    self.stack.push(self.style.clone());
                    at += 1;
                }
                b'}' => {
                    self.flush();
                    if let Some(style) = self.stack.pop() {
                        self.style = style;
                    }
                    self.font_entry = None;
                    at += 1;
                }
                b'\\' => at = self.control(rtf, at),
                b'\r' | b'\n' => at += 1,
                _ => {
                    self.text_byte(byte);
                    at += 1;
                }
            }
        }
    }

    fn finish(mut self) -> Vec<RtfRun> {
        self.flush();
        if self.ends_with_paragraph
            && let Some(last) = self.runs.last_mut()
        {
            last.text.pop();
            if last.text.is_empty() {
                self.runs.pop();
            }
        }
        self.runs
    }

    /// 一个不是控制字的字节
    fn text_byte(&mut self, byte: u8) {
        match self.style.destination {
            Destination::Body | Destination::FieldInstruction => {
                if byte.is_ascii() && !self.continues_double_byte() {
                    self.flush_bytes();
                    let mut buffer = [0; 4];
                    self.push_text(char::from(byte).encode_utf8(&mut buffer));
                } else {
                    self.bytes.push(byte);
                }
            }
            Destination::FontTable => {
                if byte == b';' {
                    self.font_entry = None;
                } else if let Some(entry) = self.font_entry {
                    self.fonts[entry].name.push(byte);
                }
            }
            Destination::ColorTable => {
                if byte == b';' {
                    self.colors.push(self.color_set.then_some(self.pending_color));
                    self.pending_color = [0; 3];
                    self.color_set = false;
                }
            }
            Destination::Skip => {}
        }
    }

    /// 攒着的字节最后停在双字节字符的第一个字节上：这个字节（可能是 ASCII）是它的第二个字节
    fn continues_double_byte(&self) -> bool {
        if self.bytes.is_empty() || !is_double_byte(self.current_code_page()) {
            return false;
        }
        let mut at = 0;
        while at < self.bytes.len() {
            at += if self.bytes[at] >= 0x80 { 2 } else { 1 };
        }
        at > self.bytes.len()
    }

    fn current_code_page(&self) -> u32 {
        self.style
            .font
            .and_then(|id| self.fonts.iter().find(|font| font.id == id))
            .and_then(|font| font.charset)
            .and_then(code_page_of_charset)
            .unwrap_or(self.code_page)
    }

    /// 攒着的字节换成文字
    fn flush_bytes(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.bytes);
        let code_page = self.current_code_page();
        let text = (self.decode)(code_page, &bytes).unwrap_or_else(|| decode_fallback(code_page, &bytes));
        self.push_text(&text);
    }

    /// 样式要变了：攒着的都放进 `runs`
    fn flush(&mut self) {
        self.flush_bytes();
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        let run = self.current_run();
        match self.runs.last_mut() {
            Some(last) if last.same_style(&run) => last.text.push_str(&text),
            _ => self.runs.push(RtfRun { text, ..run }),
        }
    }

    fn push_text(&mut self, text: &str) {
        match self.style.destination {
            Destination::FieldInstruction => self.field_instruction.push_str(text),
            Destination::Body if !text.is_empty() => {
                self.ends_with_paragraph = false;
                self.text.push_str(text);
            }
            _ => {}
        }
    }

    /// 现在的样式（没有文字）
    fn current_run(&self) -> RtfRun {
        let font = self.style.font.and_then(|id| self.fonts.iter().find(|font| font.id == id));
        let name = font.map(|font| String::from_utf8_lossy(&font.name).trim().to_owned());
        let monospaced = font.zip(name.as_deref()).is_some_and(|(font, name)| font.is_monospaced(name));
        RtfRun {
            text: String::new(),
            bold: self.style.bold,
            italic: self.style.italic,
            underline: self.style.underline,
            strikethrough: self.style.strikethrough,
            monospaced,
            font_size: self.style.font_size,
            color: self.colors.get(self.style.color).copied().flatten(),
            font_family: font.filter(|font| !font.first && !monospaced).and(name).filter(|name| !name.is_empty()),
            link: self.style.link.as_deref().map(str::to_owned),
        }
    }

    /// 一个反斜杠开头的东西，返回它之后的位置
    fn control(&mut self, rtf: &[u8], at: usize) -> usize {
        let Some(&next) = rtf.get(at + 1) else { return at + 1 };
        match next {
            b'\\' | b'{' | b'}' => {
                self.text_byte(next);
                at + 2
            }
            b'\'' => {
                if let Some(byte) = hex_byte(rtf, at + 2) {
                    if self.style.destination == Destination::FontTable {
                        if let Some(entry) = self.font_entry {
                            self.fonts[entry].name.push(byte);
                        }
                    } else if self.style.destination != Destination::Skip {
                        self.bytes.push(byte);
                    }
                }
                at + 4
            }
            b'*' => {
                self.ignorable = true;
                at + 2
            }
            b'~' => {
                self.literal("\u{a0}");
                at + 2
            }
            b'_' => {
                self.literal("\u{2011}");
                at + 2
            }
            // 换行符前的反斜杠就是换段
            b'\n' | b'\r' => {
                self.paragraph();
                at + 2
            }
            c if c.is_ascii_alphabetic() => self.word(rtf, at + 1),
            _ => at + 2,
        }
    }

    /// 控制字：字母，可选的带符号的数字，可选的一个空格（是分隔符，不算文字）
    fn word(&mut self, rtf: &[u8], start: usize) -> usize {
        let mut at = start;
        while rtf.get(at).is_some_and(u8::is_ascii_alphabetic) {
            at += 1;
        }
        let name = &rtf[start..at];
        let number_start = at;
        if rtf.get(at) == Some(&b'-') {
            at += 1;
        }
        while rtf.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        let param = std::str::from_utf8(&rtf[number_start..at]).ok().and_then(|n| n.parse::<i32>().ok());
        if rtf.get(at) == Some(&b' ') {
            at += 1;
        }
        // 二进制数据：后面跟着这么多字节
        if name == b"bin" {
            return at + param.unwrap_or(0).max(0) as usize;
        }
        let ignorable = std::mem::take(&mut self.ignorable);
        if name == b"u" {
            return self.unicode(rtf, at, param.unwrap_or(0));
        }
        self.flush();
        self.apply(name, param, ignorable);
        at
    }

    /// `\uN`：UTF-16 的一个单元，后面跟着 `\ucN` 个给不认 Unicode 的程序的退路字符
    fn unicode(&mut self, rtf: &[u8], mut at: usize, value: i32) -> usize {
        self.flush_bytes();
        let unit = value as i16 as u16;
        let mut buffer = [0; 4];
        match (self.high_surrogate.take(), unit) {
            (_, 0xD800..=0xDBFF) => self.high_surrogate = Some(unit),
            (Some(high), 0xDC00..=0xDFFF) => {
                let c = char::decode_utf16([high, unit]).next().and_then(Result::ok).unwrap_or(char::REPLACEMENT_CHARACTER);
                self.push_text(c.encode_utf8(&mut buffer));
            }
            (_, 0xDC00..=0xDFFF) => self.push_text("\u{fffd}"),
            (_, unit) => {
                let c = char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER);
                self.push_text(c.encode_utf8(&mut buffer));
            }
        }
        for _ in 0..self.style.fallback {
            match rtf.get(at) {
                Some(b'{' | b'}') | None => break,
                Some(b'\\') if rtf.get(at + 1) == Some(&b'\'') => at += 4,
                Some(b'\\') => break,
                Some(_) => at += 1,
            }
        }
        at
    }

    fn literal(&mut self, text: &str) {
        self.flush_bytes();
        self.push_text(text);
    }

    fn paragraph(&mut self) {
        self.flush_bytes();
        if self.style.destination == Destination::Body {
            self.push_text("\n");
            self.ends_with_paragraph = true;
        }
    }

    fn apply(&mut self, name: &[u8], param: Option<i32>, ignorable: bool) {
        match self.style.destination {
            Destination::Skip => return,
            Destination::FontTable => return self.font_table_word(name, param, ignorable),
            Destination::ColorTable => {
                let value = param.unwrap_or(0).clamp(0, 255) as u8;
                match name {
                    b"red" => self.pending_color[0] = value,
                    b"green" => self.pending_color[1] = value,
                    b"blue" => self.pending_color[2] = value,
                    _ => return,
                }
                self.color_set = true;
                return;
            }
            Destination::Body | Destination::FieldInstruction => {}
        }
        let on = param != Some(0);
        match name {
            b"ansicpg" => self.code_page = param.unwrap_or(1252).max(0) as u32,
            b"mac" => self.code_page = 10000,
            b"pca" => self.code_page = 850,
            b"pc" => self.code_page = 437,
            b"uc" => self.style.fallback = param.unwrap_or(1).max(0) as usize,
            b"b" => self.style.bold = on,
            b"i" => self.style.italic = on,
            b"ul" | b"uld" | b"uldb" | b"ulw" | b"ulwave" | b"ulth" | b"uldash" => self.style.underline = on,
            b"ulnone" => self.style.underline = false,
            b"strike" | b"striked" => self.style.strikethrough = on,
            b"fs" => self.style.font_size = param.filter(|size| *size > 0).map(|size| size as f32 / 2.0),
            b"cf" => self.style.color = param.unwrap_or(0).max(0) as usize,
            b"f" => self.style.font = param,
            b"plain" => {
                self.style.bold = false;
                self.style.italic = false;
                self.style.underline = false;
                self.style.strikethrough = false;
                self.style.font_size = None;
                self.style.color = 0;
                self.style.font = None;
            }
            b"par" | b"sect" | b"page" | b"line" => self.paragraph(),
            b"tab" => self.literal("\t"),
            b"emdash" => self.literal("—"),
            b"endash" => self.literal("–"),
            b"bullet" => self.literal("•"),
            b"lquote" => self.literal("‘"),
            b"rquote" => self.literal("’"),
            b"ldblquote" => self.literal("“"),
            b"rdblquote" => self.literal("”"),
            b"fonttbl" => self.style.destination = Destination::FontTable,
            b"colortbl" => self.style.destination = Destination::ColorTable,
            b"fldinst" => {
                self.field_instruction.clear();
                self.style.destination = Destination::FieldInstruction;
            }
            b"fldrslt" => {
                self.style.link = hyperlink_target(&self.field_instruction).map(Rc::from);
                self.style.destination = Destination::Body;
            }
            b"stylesheet" | b"info" | b"pict" | b"object" | b"header" | b"headerl" | b"headerr" | b"headerf" | b"footer" | b"footerl"
            | b"footerr" | b"footerf" | b"footnote" | b"xe" | b"tc" | b"rxe" | b"txe" => self.style.destination = Destination::Skip,
            _ if ignorable => self.style.destination = Destination::Skip,
            _ => {}
        }
    }

    fn font_table_word(&mut self, name: &[u8], param: Option<i32>, ignorable: bool) {
        match name {
            b"f" => {
                self.fonts.push(Font { id: param.unwrap_or(0), first: self.fonts.is_empty(), ..Font::default() });
                self.font_entry = Some(self.fonts.len() - 1);
            }
            b"fcharset" => {
                if let Some(entry) = self.font_entry {
                    self.fonts[entry].charset = param.map(|charset| charset.max(0) as u32);
                }
            }
            b"fmodern" => {
                if let Some(entry) = self.font_entry {
                    self.fonts[entry].modern = true;
                }
            }
            _ if ignorable => self.style.destination = Destination::Skip,
            _ => {}
        }
    }
}

fn hex_byte(rtf: &[u8], at: usize) -> Option<u8> {
    let digit = |b: Option<&u8>| b.and_then(|b| char::from(*b).to_digit(16));
    Some((digit(rtf.get(at))? * 16 + digit(rtf.get(at + 1))?) as u8)
}

/// `HYPERLINK "网址"` 里的网址；别的字段（页码、日期）是 `None`
fn hyperlink_target(instruction: &str) -> Option<&str> {
    let rest = instruction.trim_start();
    let keyword = rest.get(..9)?;
    if !keyword.eq_ignore_ascii_case("HYPERLINK") {
        return None;
    }
    let rest = rest[9..].trim_start();
    match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next(),
        None => rest.split_whitespace().next(),
    }
    .filter(|target| !target.is_empty())
}
