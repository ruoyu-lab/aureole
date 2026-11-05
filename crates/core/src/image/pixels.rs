//! 后台线程上的像素处理：解码、按 EXIF 方向转正、按色彩配置换到工作空间（[`super::color`]）、预乘、按需缩小、
//! 补齐、生成 mip；动图逐帧解码、逐帧处理。
//!
//! 普通的图全程保持 RGBA8、sRGB 编码；需要混合的地方（预乘、缩小、mip）先查表转成 16 位线性值，
//! 算完再查表转回 sRGB，所以缩小后的照片亮度正确（不会像按 sRGB 数值平均那样发暗）。
//! 广色域、HDR 的图（换到工作空间后超出 sRGB 色域或比白亮）在线性浮点里做同样的事，最后存成半精度浮点。

use std::io::Cursor;
use std::sync::{Arc, OnceLock};

use image::AnimationDecoder;
use scene::{IMAGE_ATLAS_ALIGN, IMAGE_ATLAS_LEVELS, ImageData, ImageEncoding, ImageLevel};

use super::color::{Conversion, Converted, Samples};

/// 最长边不超过这么多（补齐之前）的图放进图片图集，和其他图元同一次 draw call
pub(crate) const ATLAS_MAX: u32 = 512;
/// GPU 纹理的最大边长（wgpu 默认限制），更大的图解码后先缩小
pub(crate) const MAX_TEXTURE: u32 = 8192;

/// 这台设备上图片纹理的最大边长：[`MAX_TEXTURE`] 和设备的上限取小的（软件渲染、老显卡可能更小）
fn max_texture() -> u32 {
    MAX_TEXTURE.min(scene::max_texture_size())
}
/// 动图最多留这么多帧，后面的不要（还按原来的节奏循环）
pub(crate) const MAX_FRAMES: usize = 1000;
/// 动图为了放进内存上限缩小时，最长边不小于这么多；再小也放不下时后面的帧不要
const MIN_ANIMATION_EDGE: u32 = 32;

/// 动图的全部帧：每帧都处理好（预乘、补齐、mip），大小相同；`delays` 是每帧显示多久（秒）
pub(crate) struct Frames {
    pub frames: Vec<Arc<ImageData>>,
    pub delays: Vec<f32>,
    /// 一共播几遍，`None` 是一直循环
    pub loops: Option<u32>,
}

impl Frames {
    /// 所有帧（含 mip）占多少字节
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.bytes()).sum()
    }
}

/// 解码结果：静止的图（直通 alpha 的 RGBA8；或者扩展范围的、直通 alpha 的线性 RGBA），或者动图（已经处理好的各帧）
pub(crate) enum Decoded {
    Still(Vec<u8>, u32, u32),
    /// 换到工作空间后超出 sRGB 色域、或比白亮的图：线性 RGBA（sRGB 原色），直通 alpha
    Wide(Vec<f32>, u32, u32),
    Animated { natural: [u32; 2], frames: Frames },
    /// SVG 文件（`svg` 特性）：解析好的文档，不是像素
    #[cfg(feature = "svg")]
    Vector(super::svg::SvgDocument),
}

/// 解码，按 EXIF 方向转正（手机照片），按色彩配置（PNG 的 cICP，或者嵌的 ICC）换到工作空间。
/// 返回直通 alpha 的像素（sRGB RGBA8，或者扩展范围的线性 RGBA）和宽高
pub(crate) fn decode(bytes: &[u8]) -> Result<(Converted, u32, u32), String> {
    use image::ImageDecoder;
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| format!("读取失败：{e}"))?;
    let mut decoder = reader.into_decoder().map_err(|e| format!("无法识别的图片格式：{e}"))?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    // PNG 的 cICP 优先于 ICC（PNG 规范的顺序）；都没有时按 sRGB
    let conversion = png_cicp(bytes).or_else(|| decoder.icc_profile().ok().flatten().map(|icc| Conversion::from_icc(&icc)));
    let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(|e| format!("解码失败：{e}"))?;
    decoded.apply_orientation(orientation);
    let (width, height) = (decoded.width(), decoded.height());
    let pixels = match conversion.unwrap_or(Conversion::Srgb) {
        Conversion::Srgb => Converted::Srgb(decoded.into_rgba8().into_raw()),
        Conversion::Shaper(shaper) => {
            // 16 位的源（HDR 的 PNG）按 16 位换，不先丢精度
            let deep = decoded.color().bytes_per_pixel() > decoded.color().channel_count();
            let samples = if deep { Samples::U16(decoded.into_rgba16().into_raw()) } else { Samples::U8(decoded.into_rgba8().into_raw()) };
            match shaper.convert(&samples) {
                Some(converted) => converted,
                // 曲线建不出表（不应该发生）：按 sRGB
                None => Converted::Srgb(match samples {
                    Samples::U8(p) => p,
                    Samples::U16(p) => p.iter().map(|&v| (v >> 8) as u8).collect(),
                }),
            }
        }
    };
    Ok((pixels, width, height))
}

/// PNG 的 cICP 块：读到 IDAT 之前就停（不解码像素）
fn png_cicp(bytes: &[u8]) -> Option<Conversion> {
    if !bytes.starts_with(b"\x89PNG") {
        return None;
    }
    let reader = png::Decoder::new(Cursor::new(bytes)).read_info().ok()?;
    let cicp = reader.info().coding_independent_code_points?;
    Conversion::from_cicp(cicp.color_primaries, cicp.transfer_function, cicp.matrix_coefficients)
}

/// 解码：GIF 和动态 WebP 逐帧解码（每帧都是合成好的整张画布，帧的叠放和清除由 `image` crate 按文件处理），
/// 只有一帧的、其他格式按静止的图。`max_size` 同 [`process`]；所有帧加起来（含 mip）不超过 `cap` 字节：
/// 超出时把已经处理好的帧和之后的帧都缩小一半，缩到最长边 [`MIN_ANIMATION_EDGE`] 还放不下时后面的帧不要
pub(crate) fn decode_any(bytes: &[u8], max_size: Option<u32>, cap: usize) -> Result<Decoded, String> {
    let frames = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Gif) => {
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("解码失败：{e}"))?;
            let loops = loops(decoder.loop_count());
            Some((decoder.into_frames(), loops))
        }
        Ok(image::ImageFormat::WebP) => {
            let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|e| format!("解码失败：{e}"))?;
            match decoder.has_animation() {
                true => {
                    let loops = loops(decoder.loop_count());
                    Some((decoder.into_frames(), loops))
                }
                false => None,
            }
        }
        _ => None,
    };
    let Some((frames, loops)) = frames else {
        return Ok(match decode(bytes)? {
            (Converted::Srgb(rgba), width, height) => Decoded::Still(rgba, width, height),
            (Converted::Linear(rgba), width, height) => Decoded::Wide(rgba, width, height),
        });
    };
    let mut limit = max_size.unwrap_or(MAX_TEXTURE).clamp(1, max_texture());
    let mut out: Vec<Arc<ImageData>> = Vec::new();
    let mut delays = Vec::new();
    let mut total = 0usize;
    let mut natural = [0, 0];
    let mut first: Option<(Vec<u8>, u32, u32)> = None;
    for frame in frames {
        let frame = match frame {
            Ok(frame) => frame,
            // 后面的帧坏了：留下已经解码好的
            Err(e) if out.is_empty() && first.is_none() => return Err(format!("解码失败：{e}")),
            Err(_) => break,
        };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let ms = numer as f32 / denom.max(1) as f32;
        // 很多文件把延迟写成 0 或 10 毫秒（"尽快"），照着放太快，按 100 毫秒
        let delay = if ms <= 10.0 { 0.1 } else { ms / 1000.0 };
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        natural = [width, height];
        // 第一帧先留着：只有一帧时按静止的图处理
        if out.is_empty() && first.is_none() {
            first = Some((buffer.into_raw(), width, height));
            delays.push(delay);
            continue;
        }
        if let Some((rgba, w, h)) = first.take() {
            match fit_frame(&mut out, &mut total, &mut limit, [w, h], cap) {
                true => {
                    let data = Arc::new(process(rgba, w, h, Some(limit)));
                    total += data.bytes();
                    out.push(data);
                }
                false => break,
            }
        }
        if out.len() >= MAX_FRAMES || !fit_frame(&mut out, &mut total, &mut limit, [width, height], cap) {
            break;
        }
        let data = Arc::new(process(buffer.into_raw(), width, height, Some(limit)));
        total += data.bytes();
        out.push(data);
        delays.push(delay);
    }
    if let Some((rgba, width, height)) = first {
        return Ok(Decoded::Still(rgba, width, height));
    }
    delays.truncate(out.len());
    Ok(Decoded::Animated { natural, frames: Frames { frames: out, delays, loops } })
}

fn loops(count: image::metadata::LoopCount) -> Option<u32> {
    match count {
        image::metadata::LoopCount::Infinite => None,
        image::metadata::LoopCount::Finite(n) => Some(n.get()),
    }
}

/// 再放一帧（画布 `size`）会不会超出 `cap`：超出时把已有的帧缩小一半（`limit` 跟着减半），直到放得下。
/// 缩到最长边 [`MIN_ANIMATION_EDGE`] 还放不下时返回 `false`
fn fit_frame(frames: &mut [Arc<ImageData>], total: &mut usize, limit: &mut u32, size: [u32; 2], cap: usize) -> bool {
    loop {
        if *total + estimate(size, *limit) <= cap {
            return true;
        }
        let edge = scaled(size, *limit)[0].max(scaled(size, *limit)[1]);
        if edge <= MIN_ANIMATION_EDGE {
            return false;
        }
        *limit = (edge / 2).max(MIN_ANIMATION_EDGE);
        for frame in frames.iter_mut() {
            *frame = Arc::new(shrink(frame, *limit));
        }
        *total = frames.iter().map(|f| f.bytes()).sum();
    }
}

/// 按最长边 `limit` 等比缩小后的大小
fn scaled([width, height]: [u32; 2], limit: u32) -> [u32; 2] {
    if width <= limit && height <= limit {
        return [width, height];
    }
    let s = limit as f64 / width.max(height) as f64;
    [((width as f64 * s).round() as u32).clamp(1, limit), ((height as f64 * s).round() as u32).clamp(1, limit)]
}

/// 一帧处理好以后大约占多少字节（补齐 + mip 约多三分之一）
fn estimate(size: [u32; 2], limit: u32) -> usize {
    let [w, h] = scaled(size, limit);
    let [w, h] = if w.max(h) <= ATLAS_MAX { [w.next_multiple_of(IMAGE_ATLAS_ALIGN), h.next_multiple_of(IMAGE_ATLAS_ALIGN)] } else { [w, h] };
    w as usize * h as usize * 4 * 4 / 3
}

/// 已经处理好的一帧缩小到最长边 `limit`：从第 0 级（预乘过）去掉补齐的部分重新缩小、补齐、生成 mip
fn shrink(data: &ImageData, limit: u32) -> ImageData {
    let level = &data.levels[0];
    let [w, h] = data.size;
    let row = level.width as usize * 4;
    let pixels = (0..h as usize).flat_map(|y| level.pixels[y * row..][..w as usize * 4].iter().copied()).collect();
    finish(ImageLevel { width: w, height: h, pixels }, Some(limit))
}

/// 直通 alpha 的 sRGB RGBA8 -> 上传用的 [`ImageData`]：预乘；超过 `max_size`（或纹理上限）时等比缩小；
/// 小图补齐到图集对齐的倍数、生成图集用的几级 mip，大图生成完整的 mip 链
pub(crate) fn process(pixels: Vec<u8>, width: u32, height: u32, max_size: Option<u32>) -> ImageData {
    let mut base = ImageLevel { width, height, pixels };
    premultiply(&mut base.pixels);
    finish(base, max_size)
}

/// 预乘过的第 0 级 -> 缩小、补齐、生成 mip
fn finish(mut base: ImageLevel, max_size: Option<u32>) -> ImageData {
    let (width, height) = (base.width, base.height);
    let limit = max_size.unwrap_or(MAX_TEXTURE).clamp(1, max_texture());
    if width > limit || height > limit {
        let [w, h] = scaled([width, height], limit);
        base = resize(base, w, h);
    }
    let size = [base.width, base.height];
    let (base, count) = if size[0].max(size[1]) <= ATLAS_MAX {
        (pad(base, IMAGE_ATLAS_ALIGN), IMAGE_ATLAS_LEVELS as usize)
    } else {
        (base, full_chain(size))
    };
    // 各级 mip 在线性空间里逐级生成，最后才编码回 sRGB，误差不会逐级累积
    let mut current = Linear::from(&base);
    let mut levels = Vec::with_capacity(count);
    levels.push(base);
    while levels.len() < count {
        current = tent_half(&current);
        levels.push(current.encode());
    }
    ImageData { size, levels, encoding: ImageEncoding::Srgb8 }
}

/// 完整的 mip 链有几级：floor(log2(最长边)) + 1，最后一级 1 像素
fn full_chain([width, height]: [u32; 2]) -> usize {
    (32 - width.max(height).max(1).leading_zeros()) as usize
}

/// 扩展范围的图（直通 alpha 的线性 RGBA，sRGB 原色）-> 半精度浮点的 [`ImageData`]：预乘，超过 `max_size`
/// （或纹理上限）时等比缩小，生成完整的 mip 链。不补齐、不进图集（图集是 8 位的）
pub(crate) fn process_wide(mut pixels: Vec<f32>, width: u32, height: u32, max_size: Option<u32>) -> ImageData {
    // 峰值按没预乘的颜色、P3 的分量算（半透明的像素也是那么亮；P3 色域里的不超过 1），见 `display_peak`
    let mut peak = 0.0f32;
    for p in pixels.as_chunks_mut::<4>().0 {
        if p[3] > 0.0 {
            peak = peak.max(scene::display_peak([p[0], p[1], p[2]]));
        }
        let a = p[3];
        p[0] *= a;
        p[1] *= a;
        p[2] *= a;
    }
    let mut base = Float { width: width as usize, height: height as usize, data: pixels };
    let limit = max_size.unwrap_or(MAX_TEXTURE).clamp(1, max_texture());
    if width > limit || height > limit {
        let [w, h] = scaled([width, height], limit);
        base = base.resize(w as usize, h as usize);
    }
    let size = [base.width as u32, base.height as u32];
    let count = full_chain(size);
    let mut levels = Vec::with_capacity(count);
    levels.push(base.encode());
    while levels.len() < count {
        base = base.tent_half();
        levels.push(base.encode());
    }
    ImageData { size, levels, encoding: ImageEncoding::Float16 { peak: peak.max(1.0) } }
}

/// 浮点的一级：RGBA 各一个 f32，线性的预乘颜色
struct Float {
    width: usize,
    height: usize,
    data: Vec<f32>,
}

impl Float {
    /// 存成半精度浮点（RGBA 各 2 字节，本机字节序），用硬件指令批量转换
    fn encode(&self) -> ImageLevel {
        use half::slice::{HalfBitsSliceExt, HalfFloatSliceExt};
        let mut bits = vec![0u16; self.data.len()];
        bits.reinterpret_cast_mut::<half::f16>().convert_from_f32_slice(&self.data);
        let pixels = bytemuck::cast_slice(&bits).to_vec();
        ImageLevel { width: self.width as u32, height: self.height as u32, pixels }
    }

    /// 下一级 mip：和 8 位的图一样的 [1, 3, 3, 1] / 8 帐篷滤波（见 [`tent_half`]），在浮点里算
    fn tent_half(&self) -> Self {
        let (sw, sh) = (self.width, self.height);
        let (w, h) = ((sw / 2).max(1), (sh / 2).max(1));
        let taps = |i: usize, n: usize| {
            let n = n as isize;
            [-1isize, 0, 1, 2].map(|d| {
                let j = 2 * i as isize + d;
                let j = if j < 0 { -j } else if j >= n { 2 * n - 2 - j } else { j };
                j.clamp(0, n - 1) as usize
            })
        };
        let filter = |a: f32, b: f32, c: f32, d: f32| (a + 3.0 * (b + c) + d) * 0.125;
        let mut mid = vec![0f32; w * sh * 4];
        for y in 0..sh {
            let row = &self.data[y * sw * 4..][..sw * 4];
            let out = &mut mid[y * w * 4..][..w * 4];
            for x in 0..w {
                let [a, b, c, d] = taps(x, sw).map(|t| t * 4);
                for ch in 0..4 {
                    out[x * 4 + ch] = filter(row[a + ch], row[b + ch], row[c + ch], row[d + ch]);
                }
            }
        }
        let mut data = vec![0f32; w * h * 4];
        for y in 0..h {
            let [a, b, c, d] = taps(y, sh).map(|r| &mid[r * w * 4..][..w * 4]);
            for (i, out) in data[y * w * 4..][..w * 4].iter_mut().enumerate() {
                *out = filter(a[i], b[i], c[i], d[i]);
            }
        }
        Self { width: w, height: h, data }
    }

    /// 缩小到 `width` x `height`：按面积加权平均（和 8 位的 [`resize`] 一样）
    fn resize(&self, width: usize, height: usize) -> Self {
        let columns = area_weights(self.width as u32, width as u32);
        let rows = area_weights(self.height as u32, height as u32);
        let mut data = vec![0f32; width * height * 4];
        for (y, (row_start, row_weights)) in rows.iter().enumerate() {
            for (x, (col_start, col_weights)) in columns.iter().enumerate() {
                let out = &mut data[(y * width + x) * 4..][..4];
                for (j, wy) in row_weights.iter().enumerate() {
                    let row = (*row_start as usize + j) * self.width;
                    for (i, wx) in col_weights.iter().enumerate() {
                        let p = &self.data[(row + *col_start as usize + i) * 4..][..4];
                        let w = wy * wx;
                        for c in 0..4 {
                            out[c] += p[c] * w;
                        }
                    }
                }
            }
        }
        Self { width, height, data }
    }
}

/// sRGB 8 位 -> 线性 16 位
fn to_linear() -> &'static [u16; 256] {
    static LUT: OnceLock<[u16; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        std::array::from_fn(|i| {
            let c = i as f64 / 255.0;
            let l = if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
            (l * 65535.0).round() as u16
        })
    })
}

/// 线性 16 位 -> sRGB 8 位（65536 项，64KB，一次建好）
pub(super) fn to_srgb() -> &'static [u8] {
    static LUT: OnceLock<Box<[u8]>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=u16::MAX)
            .map(|i| {
                let l = i as f64 / 65535.0;
                let s = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
                (s * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect()
    })
}

/// 在线性空间预乘，再编码回 sRGB：存进 sRGB 纹理后，采样得到的就是线性预乘的颜色。
/// 不透明像素（绝大多数照片）原样不动
fn premultiply(pixels: &mut [u8]) {
    let (linear, srgb) = (to_linear(), to_srgb());
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a == 255 {
            continue;
        }
        if a == 0 {
            p.copy_from_slice(&[0; 4]);
            continue;
        }
        for c in &mut p[..3] {
            *c = srgb[((linear[*c as usize] as u32 * a + 127) / 255) as usize];
        }
    }
}

/// 线性空间的一级：RGBA 各 16 位，颜色是线性的预乘值，alpha 是 0..=65535
struct Linear {
    width: usize,
    height: usize,
    data: Vec<u16>,
}

impl Linear {
    fn from(level: &ImageLevel) -> Self {
        let linear = to_linear();
        let data = level.pixels.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| {
            [linear[r as usize], linear[g as usize], linear[b as usize], a as u16 * 257]
        });
        Self { width: level.width as usize, height: level.height as usize, data: data.collect() }
    }

    fn encode(&self) -> ImageLevel {
        let srgb = to_srgb();
        let pixels = self.data.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| {
            [srgb[r as usize], srgb[g as usize], srgb[b as usize], ((a as u32 + 128) / 257) as u8]
        });
        ImageLevel { width: self.width as u32, height: self.height as u32, pixels: pixels.collect() }
    }
}

/// 下一级 mip：[1, 3, 3, 1] / 8 的帐篷滤波，先横后竖。和 2x2 平均相比，接近一半频率的细纹
/// （细密的条纹、同心圆）衰减得多，缩小显示时不容易出现摩尔纹。
/// 边缘外的采样按边缘镜像取（不重复边缘像素），权重保持对称，边缘不会偏亮或偏暗
fn tent_half(src: &Linear) -> Linear {
    let (sw, sh) = (src.width, src.height);
    let (w, h) = ((sw / 2).max(1), (sh / 2).max(1));
    let taps = |i: usize, n: usize| {
        let n = n as isize;
        [-1isize, 0, 1, 2].map(|d| {
            let j = 2 * i as isize + d;
            let j = if j < 0 { -j } else if j >= n { 2 * n - 2 - j } else { j };
            j.clamp(0, n - 1) as usize
        })
    };
    let filter = |a: u16, b: u16, c: u16, d: u16| ((a as u32 + 3 * (b as u32 + c as u32) + d as u32 + 4) / 8) as u16;
    let mut mid = vec![0u16; w * sh * 4];
    for y in 0..sh {
        let row = &src.data[y * sw * 4..][..sw * 4];
        let out = &mut mid[y * w * 4..][..w * 4];
        for x in 0..w {
            let [a, b, c, d] = taps(x, sw).map(|t| t * 4);
            for ch in 0..4 {
                out[x * 4 + ch] = filter(row[a + ch], row[b + ch], row[c + ch], row[d + ch]);
            }
        }
    }
    let mut data = vec![0u16; w * h * 4];
    for y in 0..h {
        let [a, b, c, d] = taps(y, sh).map(|r| &mid[r * w * 4..][..w * 4]);
        for (i, out) in data[y * w * 4..][..w * 4].iter_mut().enumerate() {
            *out = filter(a[i], b[i], c[i], d[i]);
        }
    }
    Linear { width: w, height: h, data }
}

/// 2x2 平均（线性空间），只用来把很大的图先对半缩小。奇数边长时最后一行 / 列重复使用
fn box_half(src: &ImageLevel) -> ImageLevel {
    let (linear, srgb) = (to_linear(), to_srgb());
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (w, h) = ((sw / 2).max(1), (sh / 2).max(1));
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        let (r0, r1) = ((2 * y).min(sh - 1) * sw, (2 * y + 1).min(sh - 1) * sw);
        for x in 0..w {
            let (c0, c1) = ((2 * x).min(sw - 1), (2 * x + 1).min(sw - 1));
            let taps = [(r0 + c0) * 4, (r0 + c1) * 4, (r1 + c0) * 4, (r1 + c1) * 4];
            let out = &mut pixels[(y * w + x) * 4..][..4];
            for c in 0..3 {
                let sum: u32 = taps.iter().map(|&t| linear[src.pixels[t + c] as usize] as u32).sum();
                out[c] = srgb[((sum + 2) / 4) as usize];
            }
            let alpha: u32 = taps.iter().map(|&t| src.pixels[t + 3] as u32).sum();
            out[3] = ((alpha + 2) / 4) as u8;
        }
    }
    ImageLevel { width: w as u32, height: h as u32, pixels }
}

/// 缩小到 `width` x `height`：先对半缩小到不足目标的两倍，再按面积加权平均（线性空间）
fn resize(mut src: ImageLevel, width: u32, height: u32) -> ImageLevel {
    while src.width >= width * 2 && src.height >= height * 2 {
        src = box_half(&src);
    }
    if (src.width, src.height) == (width, height) {
        return src;
    }
    let (linear, srgb) = (to_linear(), to_srgb());
    let columns = area_weights(src.width, width);
    let rows = area_weights(src.height, height);
    let sw = src.width as usize;
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for (y, (row_start, row_weights)) in rows.iter().enumerate() {
        for (x, (col_start, col_weights)) in columns.iter().enumerate() {
            let mut acc = [0.0f32; 4];
            for (j, wy) in row_weights.iter().enumerate() {
                let row = (*row_start as usize + j) * sw;
                for (i, wx) in col_weights.iter().enumerate() {
                    let p = &src.pixels[(row + *col_start as usize + i) * 4..][..4];
                    let w = wy * wx;
                    for c in 0..3 {
                        acc[c] += linear[p[c] as usize] as f32 * w;
                    }
                    acc[3] += p[3] as f32 * w;
                }
            }
            let out = &mut pixels[(y * width as usize + x) * 4..][..4];
            for c in 0..3 {
                out[c] = srgb[acc[c].round().clamp(0.0, 65535.0) as usize];
            }
            out[3] = acc[3].round().clamp(0.0, 255.0) as u8;
        }
    }
    ImageLevel { width, height, pixels }
}

/// 每个输出像素覆盖的输入像素：起点和各自的权重（覆盖面积占比，和为 1）
fn area_weights(src: u32, dst: u32) -> Vec<(u32, Vec<f32>)> {
    let scale = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let (a, b) = (i as f64 * scale, (i + 1) as f64 * scale);
            let start = a.floor() as u32;
            let end = (b.ceil() as u32).min(src).max(start + 1);
            let weights = (start..end).map(|j| ((b.min(j as f64 + 1.0) - a.max(j as f64)) / scale) as f32).collect();
            (start, weights)
        })
        .collect()
}

/// 右边和下边补齐到 `align` 的倍数，补的部分重复边缘像素（缩小时边缘颜色不会被拉向透明）
fn pad(src: ImageLevel, align: u32) -> ImageLevel {
    let (w, h) = (src.width.next_multiple_of(align), src.height.next_multiple_of(align));
    if (w, h) == (src.width, src.height) {
        return src;
    }
    let (sw, sh) = (src.width as usize, src.height as usize);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        let from = &src.pixels[y.min(sh - 1) * sw * 4..][..sw * 4];
        let row = &mut pixels[y * w as usize * 4..][..w as usize * 4];
        row[..sw * 4].copy_from_slice(from);
        let edge: [u8; 4] = from[(sw - 1) * 4..].try_into().expect("一个像素 4 字节");
        for x in sw..w as usize {
            row[x * 4..][..4].copy_from_slice(&edge);
        }
    }
    ImageLevel { width: w, height: h, pixels }
}
