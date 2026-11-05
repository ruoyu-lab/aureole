//! 应用图标：用矢量路径画（渐变的圆角方块底 + 图形 + 阴影），或者从一张大 PNG 缩放；写成 .icns。
//!
//! 每个尺寸都按自己的像素大小重新栅格化，16x16 也是清晰的。颜色在线性空间里混合，渐变中段不发灰。

use std::io::Write;
use std::path::{Path, PathBuf};

use zeno::{Command, Fill, Mask, PathBuilder, Transform};

/// .icns 里放的尺寸：（类型码，边长）。同一边长的 1x 和 2x 各放一份，系统按需挑
const ENTRIES: [(&[u8; 4], u32); 10] = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic07", 128),
    (b"ic13", 256),
    (b"ic08", 256),
    (b"ic14", 512),
    (b"ic09", 512),
    (b"ic10", 1024),
];

/// 不透明度为 1 的 sRGB 颜色，分量 0～1
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub [f32; 3]);

impl Rgb {
    /// `#rrggbb` 或 `rrggbb`
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        Some(Self([(value >> 16) as u8, (value >> 8) as u8, value as u8].map(|c| c as f32 / 255.0)))
    }

    fn linear(self) -> [f32; 3] {
        self.0.map(to_linear)
    }
}

/// 用矢量路径画的图标：圆角方块（macOS 图标的轮廓）上放一个图形
#[derive(Clone, Debug)]
pub struct VectorIcon {
    /// 图形，SVG 路径语法
    pub path: String,
    /// 图形的画布 `[x, y, 宽, 高]`，等比缩放后放在方块中央
    pub view_box: [f32; 4],
    /// 描边宽度（画布坐标）；`None` 表示填充
    pub stroke: Option<f32>,
    /// 底色：从上到下的渐变
    pub background: [Rgb; 2],
    /// 图形的颜色
    pub foreground: Rgb,
}

impl Default for VectorIcon {
    /// 没配置图标时用的：蓝紫渐变上一个白色的窗口
    fn default() -> Self {
        Self {
            path: "M3 6.5 A2.5 2.5 0 0 1 5.5 4 H18.5 A2.5 2.5 0 0 1 21 6.5 V17.5 A2.5 2.5 0 0 1 18.5 20 H5.5 A2.5 2.5 0 0 1 3 17.5 Z M3 9 H21 M6 6.5 H6.01 M8.5 6.5 H8.51".into(),
            view_box: [0.0, 0.0, 24.0, 24.0],
            stroke: Some(1.6),
            background: [Rgb([0.42, 0.45, 0.98]), Rgb([0.24, 0.20, 0.72])],
            foreground: Rgb([1.0, 1.0, 1.0]),
        }
    }
}

/// 图标的来源
#[derive(Clone, Debug)]
pub enum IconSource {
    Vector(VectorIcon),
    /// 一张方形的图（PNG / JPEG / WebP），建议 1024x1024
    Image(std::path::PathBuf),
    /// 现成的 .icns，原样拷进去
    Icns(std::path::PathBuf),
}

/// 生成 .icns 写到 `out`
pub fn write_icns(source: &IconSource, out: &Path) -> Result<(), String> {
    let pngs: Vec<(&[u8; 4], Vec<u8>)> = match source {
        IconSource::Icns(path) => {
            return std::fs::copy(path, out).map(|_| ()).map_err(|e| format!("拷贝图标 {} 失败：{e}", path.display()));
        }
        IconSource::Vector(icon) => ENTRIES.iter().map(|&(kind, size)| Ok((kind, encode_png(&render(icon, size), size)?))).collect::<Result<_, String>>()?,
        IconSource::Image(path) => {
            let image = image::open(path).map_err(|e| format!("读取图标 {} 失败：{e}", path.display()))?.into_rgba8();
            if image.width() != image.height() {
                eprintln!("提示：图标 {} 不是正方形（{}x{}），会被拉伸", path.display(), image.width(), image.height());
            }
            if image.width() < 1024 {
                eprintln!("提示：图标 {} 只有 {}x{}，大尺寸会发虚，建议 1024x1024", path.display(), image.width(), image.height());
            }
            ENTRIES
                .iter()
                .map(|&(kind, size)| {
                    let resized = image::imageops::resize(&image, size, size, image::imageops::FilterType::Lanczos3);
                    Ok((kind, encode_png(resized.as_raw(), size)?))
                })
                .collect::<Result<_, String>>()?
        }
    };
    let bytes = icns(&pngs);
    std::fs::File::create(out).and_then(|mut f| f.write_all(&bytes)).map_err(|e| format!("写入 {} 失败：{e}", out.display()))
}

/// 把 PNG 按 .icns 的格式拼起来：文件头 `icns` + 总长度，每一项是类型码 + 这一项的长度 + PNG 数据（长度都是大端）
pub fn icns(entries: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let total = 8 + entries.iter().map(|(_, png)| 8 + png.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(total as u32).to_be_bytes());
    for (kind, png) in entries {
        out.extend_from_slice(*kind);
        out.extend_from_slice(&((8 + png.len()) as u32).to_be_bytes());
        out.extend_from_slice(png);
    }
    out
}

/// 画成 `size` x `size` 的 PNG。现成的 .icns 画不了
pub fn png_at(source: &IconSource, size: u32) -> Result<Vec<u8>, String> {
    let pixels = match source {
        IconSource::Vector(icon) => render(icon, size),
        IconSource::Image(path) => {
            let image = image::open(path).map_err(|e| format!("读取图标 {} 失败：{e}", path.display()))?.into_rgba8();
            image::imageops::resize(&image, size, size, image::imageops::FilterType::Lanczos3).into_raw()
        }
        IconSource::Icns(path) => return Err(format!("图标 {} 是现成的 .icns，这里要矢量图标或方形的图片", path.display())),
    };
    encode_png(&pixels, size)
}

/// 写一张预览 PNG（`--icon-preview` 用）
pub fn write_preview(source: &IconSource, size: u32, out: &Path) -> Result<(), String> {
    std::fs::write(out, png_at(source, size)?).map_err(|e| format!("写入 {} 失败：{e}", out.display()))
}

/// 把 PNG 按 .ico 的格式拼起来：文件头（类型 1、张数），每张一个目录项（边长，256 写成 0；32 位色；数据的长度和位置，都是小端），然后是数据
pub fn ico(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len();
    for (size, png) in entries {
        let side = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[side, side, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in entries {
        out.extend_from_slice(png);
    }
    out
}

/// 把一张图缩放成 `width` x `height`，存成 24 位的 BMP（NSIS 安装界面的图只认这种），底色是白色（透明的地方）
pub fn write_bmp(source: &Path, [width, height]: [u32; 2], out: &Path) -> Result<(), String> {
    let image = image::open(source).map_err(|e| format!("读取图片 {} 失败：{e}", source.display()))?.into_rgba8();
    let resized = image::imageops::resize(&image, width, height, image::imageops::FilterType::Lanczos3);
    std::fs::write(out, bmp_24(&resized)).map_err(|e| format!("写入 {} 失败：{e}", out.display()))
}

/// 24 位 BMP：文件头、信息头，然后从下往上一行行的蓝绿红，每行补齐到 4 字节
fn bmp_24(image: &image::RgbaImage) -> Vec<u8> {
    let (width, height) = image.dimensions();
    let stride = (width as usize * 3).div_ceil(4) * 4;
    let size = 54 + stride * height as usize;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&((stride * height as usize) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    for y in (0..height).rev() {
        let start = out.len();
        for x in 0..width {
            let [r, g, b, a] = image.get_pixel(x, y).0;
            let over_white = |c: u8| ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a))) / 255) as u8;
            out.extend_from_slice(&[over_white(b), over_white(g), over_white(r)]);
        }
        out.resize(start + stride, 0);
    }
    out
}

/// 应用的图标写成 .ico
pub fn write_ico_from(source: &IconSource, out: &Path) -> Result<(), String> {
    const SIZES: [u32; 5] = [16, 32, 48, 64, 256];
    let entries = SIZES.iter().map(|&size| Ok((size, png_at(source, size)?))).collect::<Result<Vec<_>, String>>()?;
    std::fs::write(out, ico(&entries)).map_err(|e| format!("写入 {} 失败：{e}", out.display()))
}

/// 一张图或者现成的 .ico 换成 .ico 放到 `out`
pub fn write_ico(source: &Path, sizes: &[u32], out: &Path) -> Result<(), String> {
    if source.extension().is_some_and(|e| e.eq_ignore_ascii_case("ico")) {
        return std::fs::copy(source, out).map(|_| ()).map_err(|e| format!("拷贝 {} 失败：{e}", source.display()));
    }
    let image = IconSource::Image(source.to_path_buf());
    let entries = sizes.iter().map(|&size| Ok((size, png_at(&image, size)?))).collect::<Result<Vec<_>, String>>()?;
    std::fs::write(out, ico(&entries)).map_err(|e| format!("写入 {} 失败：{e}", out.display()))
}

/// Windows、Linux 的安装包要的图标：各尺寸的 PNG 和一个 .ico，放进 `dir`，返回路径
pub fn write_installer_icons(source: &IconSource, png_sizes: &[u32], ico_sizes: &[u32], dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for &size in png_sizes {
        let path = dir.join(format!("icon-{size}.png"));
        std::fs::write(&path, png_at(source, size)?).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
        paths.push(path);
    }
    let entries = ico_sizes.iter().map(|&size| Ok((size, png_at(source, size)?))).collect::<Result<Vec<_>, String>>()?;
    let path = dir.join("icon.ico");
    std::fs::write(&path, ico(&entries)).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    paths.push(path);
    Ok(paths)
}

fn encode_png(rgba: &[u8], size: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut w| w.write_image_data(rgba)).map_err(|e| format!("编码 PNG 失败：{e}"))?;
    Ok(out)
}

/// 图标的版式，按 1024 的画布：方块 824x824 居中，四周留 100 给阴影；图形占方块的一半
const CANVAS: f32 = 1024.0;
const BODY: f32 = 824.0;
const GLYPH: f32 = 0.5;

/// 画成 `size` x `size` 的 RGBA（sRGB、非预乘）
pub fn render(icon: &VectorIcon, size: u32) -> Vec<u8> {
    let k = size as f32 / CANVAS;
    let n = (size * size) as usize;
    let center = size as f32 * 0.5;
    let half = BODY * 0.5 * k;
    let body = coverage(&squircle(center, center, half), None, size);

    // 线性空间、预乘的 RGBA
    let mut out = vec![[0.0f32; 4]; n];
    // 方块的投影：往下 12、模糊 20、30% 黑
    let shadow = blur(&shift(&body, size, (12.0 * k).round() as i32), size, 20.0 * k);
    for (px, &a) in out.iter_mut().zip(&shadow) {
        *px = [0.0, 0.0, 0.0, 0.3 * a];
    }
    // 方块：从上到下的渐变
    let [top, bottom] = icon.background.map(Rgb::linear);
    let (y0, y1) = (center - half, center + half);
    for y in 0..size as usize {
        let t = ((y as f32 + 0.5 - y0) / (y1 - y0)).clamp(0.0, 1.0);
        let color: [f32; 3] = std::array::from_fn(|i| top[i] + (bottom[i] - top[i]) * t);
        for x in 0..size as usize {
            let i = y * size as usize + x;
            over(&mut out[i], color, body[i]);
        }
    }

    // 图形：画布等比缩放进方块中央
    let [vx, vy, vw, vh] = icon.view_box;
    let fit = if vw > 0.0 && vh > 0.0 { BODY * GLYPH * k / vw.max(vh) } else { 1.0 };
    let offset = [center - (vx + vw * 0.5) * fit, center - (vy + vh * 0.5) * fit];
    let transform = Transform::scale(fit, fit).then_translate(offset[0], offset[1]);
    let glyph = coverage_svg(&icon.path, icon.stroke, transform, size);
    // 图形的投影只落在方块里面
    let glyph_shadow = blur(&shift(&glyph, size, (8.0 * k).round() as i32), size, 12.0 * k);
    let fg = icon.foreground.linear();
    for ((px, &g), (&s, &b)) in out.iter_mut().zip(&glyph).zip(glyph_shadow.iter().zip(&body)) {
        over(px, [0.0; 3], 0.22 * s * b);
        over(px, fg, g);
    }

    out.iter()
        .flat_map(|&[r, g, b, a]| {
            let un = |c: f32| if a > 0.0 { to_srgb(c / a) } else { 0.0 };
            [un(r), un(g), un(b), a].map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        })
        .collect()
}

/// 预乘颜色上叠一层不透明度为 `alpha` 的纯色
fn over(dst: &mut [f32; 4], color: [f32; 3], alpha: f32) {
    let keep = 1.0 - alpha;
    for i in 0..3 {
        dst[i] = color[i] * alpha + dst[i] * keep;
    }
    dst[3] = alpha + dst[3] * keep;
}

/// 超椭圆 |x|^5 + |y|^5 = 1：和 macOS 图标的连续曲率圆角几乎重合
fn squircle(cx: f32, cy: f32, half: f32) -> Vec<Command> {
    const N: usize = 256;
    const EXP: f32 = 2.0 / 5.0;
    let mut path = Vec::new();
    for i in 0..N {
        let angle = i as f32 / N as f32 * std::f32::consts::TAU;
        let (s, c) = angle.sin_cos();
        let p = [cx + half * c.signum() * c.abs().powf(EXP), cy + half * s.signum() * s.abs().powf(EXP)];
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.close();
    path
}

fn coverage(path: &[Command], transform: Option<Transform>, size: u32) -> Vec<f32> {
    let mut mask = Mask::new(path);
    mask.size(size, size).transform(transform).style(Fill::NonZero);
    mask.render().0.iter().map(|&a| a as f32 / 255.0).collect()
}

fn coverage_svg(path: &str, stroke: Option<f32>, transform: Transform, size: u32) -> Vec<f32> {
    let mut mask = Mask::new(path);
    mask.size(size, size).transform(Some(transform));
    if let Some(width) = stroke {
        let mut style = zeno::Stroke::new(width);
        style.cap(zeno::Cap::Round).join(zeno::Join::Round);
        mask.style(style);
    }
    mask.render().0.iter().map(|&a| a as f32 / 255.0).collect()
}

/// 往下挪 `dy` 个像素
fn shift(alpha: &[f32], size: u32, dy: i32) -> Vec<f32> {
    let w = size as usize;
    let mut out = vec![0.0; alpha.len()];
    for y in 0..w {
        let src = y as i32 - dy;
        if (0..w as i32).contains(&src) {
            out[y * w..(y + 1) * w].copy_from_slice(&alpha[src as usize * w..(src as usize + 1) * w]);
        }
    }
    out
}

/// 近似高斯模糊：横竖各做三遍盒式模糊，`radius` 约等于模糊半径（像素）
fn blur(alpha: &[f32], size: u32, radius: f32) -> Vec<f32> {
    let r = (radius / 2.0).round() as usize;
    if r == 0 {
        return alpha.to_vec();
    }
    let w = size as usize;
    let mut a = alpha.to_vec();
    let mut b = vec![0.0; a.len()];
    for _ in 0..3 {
        box_pass(&a, &mut b, w, r, 1, w);
        box_pass(&b, &mut a, w, r, w, 1);
    }
    a
}

/// 沿一个方向做一遍盒式模糊：`step` 是相邻像素的下标差，`line` 是相邻两条线的下标差
fn box_pass(src: &[f32], dst: &mut [f32], w: usize, r: usize, step: usize, line: usize) {
    let scale = 1.0 / (2 * r + 1) as f32;
    for l in 0..w {
        let at = |i: isize| if (0..w as isize).contains(&i) { src[l * line + i as usize * step] } else { 0.0 };
        let mut sum: f32 = (-(r as isize)..=r as isize).map(at).sum();
        for i in 0..w {
            dst[l * line + i * step] = sum * scale;
            sum += at(i as isize + r as isize + 1) - at(i as isize - r as isize);
        }
    }
}

fn to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}
