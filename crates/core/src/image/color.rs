//! 图片的色彩配置：解码时（后台线程）把带色彩配置的像素换到工作空间（线性光、sRGB 原色）。
//!
//! - 没有色彩配置、配置就是 sRGB：原样不动（绝大多数图片，除了读一下配置没有开销）
//! - ICC（PNG、JPEG、WebP 里嵌的）：矩阵加传递曲线的配置（Display P3、Adobe RGB、ProPhoto……，
//!   很多同时带查表的旧配置也有这几项）按每个通道的曲线查表线性化，再乘到 sRGB 原色的矩阵。
//!   只有查表（LUT）的 RGB 配置很少见，按 sRGB：做它的转换要多带约 270 KB 的代码
//! - PNG 的 cICP：BT.709 / Display P3 / BT.2020 原色，sRGB、BT.709、线性、PQ、HLG 曲线。PQ、HLG 是 HDR：
//!   按参考白 203 尼特换成 SDR 白的倍数（PQ 的 1000 尼特约是 4.9 倍）
//!
//! 换完以后都在 sRGB 色域里、不比白亮的图编码回 8 位 sRGB，走原来的路（能进图集）；
//! 否则留成浮点（色域外的分量小于 0、HDR 的大于 1），交给 [`super::pixels::process_wide`]。

use moxcms::ColorProfile;
use scene::{BT2020_TO_SRGB, P3_TO_SRGB, decode_srgb, multiply_matrix3};

/// 参考白（BT.2408）：PQ、HLG 里这么亮的算 SDR 白（1.0）
const REFERENCE_WHITE_NITS: f32 = 203.0;
/// HLG 的标称峰值亮度，尼特（BT.2100 的参考显示）
const HLG_PEAK_NITS: f32 = 1000.0;
/// 换完以后超出 0..=1 不到这么多的当作没超出（计算误差），按 8 位 sRGB 存
const GAMUT_EPS: f32 = 2e-3;

/// 图片的颜色怎么换到工作空间
pub(crate) enum Conversion {
    /// sRGB（或者没有色彩配置）：不用换
    Srgb,
    /// 每个通道的传递曲线（查表）+ 到线性 sRGB 的矩阵
    Shaper(Shaper),
}

/// 传递曲线和矩阵
pub(crate) struct Shaper {
    /// 怎么线性化
    curves: Curves,
    /// 线性的源原色 -> 线性 sRGB，`None` 是同样的原色
    matrix: Option<[[f32; 3]; 3]>,
    /// HLG：线性化得到的是场景光，还要按 BT.2100 的参考显示换成显示光
    hlg: bool,
}

/// 传递曲线：ICC 配置里每个通道各一条（moxcms 建表），或者 cICP 写的一条标准曲线
enum Curves {
    Icc(Box<ColorProfile>),
    Standard(Transfer),
}

/// cICP 的传递曲线
#[derive(Clone, Copy, Debug, PartialEq)]
enum Transfer {
    Srgb,
    /// BT.709、BT.601、BT.2020 的 SDR 曲线：按显示端的 2.4 次幂（BT.1886）
    Gamma24,
    Linear,
    /// SMPTE ST 2084（HDR10）
    Pq,
    /// ARIB STD-B67（BT.2100 HLG）
    Hlg,
}

impl Conversion {
    /// PNG 的 cICP 块（原色、传递曲线、矩阵、全范围）：认得的组合返回换法；sRGB 的、不认得的返回 `None`（按 sRGB）
    pub fn from_cicp(primaries: u8, transfer: u8, matrix: u8) -> Option<Self> {
        // 只认 RGB 的（矩阵系数 0）
        if matrix != 0 {
            return None;
        }
        let matrix = match primaries {
            1 => None,
            12 => Some(P3_TO_SRGB),
            9 => Some(BT2020_TO_SRGB),
            _ => return None,
        };
        let transfer = match transfer {
            13 => Transfer::Srgb,
            1 | 6 | 14 | 15 => Transfer::Gamma24,
            8 => Transfer::Linear,
            16 => Transfer::Pq,
            18 => Transfer::Hlg,
            _ => return None,
        };
        if matrix.is_none() && transfer == Transfer::Srgb {
            return Some(Self::Srgb);
        }
        Some(Self::Shaper(Shaper { curves: Curves::Standard(transfer), matrix, hlg: transfer == Transfer::Hlg }))
    }

    /// 嵌在图片里的 ICC 配置。解析不了、不是 RGB 的（灰度、CMYK）、只有查表没有矩阵和曲线的按 sRGB；
    /// 和 sRGB 一样的不用换
    pub fn from_icc(icc: &[u8]) -> Self {
        let Ok(profile) = ColorProfile::new_from_slice(icc) else { return Self::Srgb };
        if profile.color_space != moxcms::DataColorSpace::Rgb || !profile.is_matrix_shaper() {
            return Self::Srgb;
        }
        let srgb = ColorProfile::new_srgb();
        let m = profile.transform_matrix(&srgb).v;
        let identity = (0..3).all(|r| (0..3).all(|c| (m[r][c] - if r == c { 1.0 } else { 0.0 }).abs() < 2e-3));
        let matrix: [[f32; 3]; 3] = m.map(|row| row.map(|v| v as f32));
        if identity && is_srgb_curve(&profile) {
            return Self::Srgb;
        }
        Self::Shaper(Shaper { curves: Curves::Icc(Box::new(profile)), matrix: (!identity).then_some(matrix), hlg: false })
    }
}

/// 三个通道的传递曲线都和 sRGB 的差不多（误差在 8 位的一个色阶以内）
fn is_srgb_curve(profile: &ColorProfile) -> bool {
    (0..3).all(|channel| {
        let Some(table) = icc_table::<u8, 256, 8>(profile, channel) else { return false };
        table.iter().enumerate().all(|(i, &v)| (v - decode_srgb(i as f32 / 255.0)).abs() < 1.5e-3)
    })
}

fn icc_table<T: moxcms::PointeeSizeExpressible, const N: usize, const BITS: usize>(profile: &ColorProfile, channel: usize) -> Option<Vec<f32>> {
    let table = match channel {
        0 => profile.build_r_linearize_table::<T, N, BITS>(false),
        1 => profile.build_g_linearize_table::<T, N, BITS>(false),
        _ => profile.build_b_linearize_table::<T, N, BITS>(false),
    };
    table.ok().map(|t| t.to_vec())
}

impl Transfer {
    /// 编码值（0..=1）-> 线性光，SDR 白是 1（PQ、HLG 按参考白换算，可以大于 1）
    fn linearize(self, e: f32) -> f32 {
        match self {
            Self::Srgb => decode_srgb(e),
            Self::Gamma24 => e.max(0.0).powf(2.4),
            Self::Linear => e,
            Self::Pq => pq_eotf(e) / REFERENCE_WHITE_NITS,
            // 场景光 0..=1，显示光在 [`hlg_ootf`] 里算
            Self::Hlg => hlg_inverse_oetf(e),
        }
    }
}

/// PQ（SMPTE ST 2084）的 EOTF：编码值 -> 绝对亮度，尼特
fn pq_eotf(e: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    10_000.0 * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

/// HLG 的 OETF 的逆：编码值 -> 场景光（0..=1）
fn hlg_inverse_oetf(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 1.0 - 4.0 * A;
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    if e <= 0.5 { e * e / 3.0 } else { (((e - C) / A).exp() + B) / 12.0 }
}

/// HLG 的 OOTF（BT.2100，标称峰值 1000 尼特、系统伽马 1.2）：场景光（BT.2020 原色）-> 显示光，再按参考白换成 SDR 白的倍数
fn hlg_ootf([r, g, b]: [f32; 3]) -> [f32; 3] {
    let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
    let scale = HLG_PEAK_NITS / REFERENCE_WHITE_NITS * y.max(0.0).powf(0.2);
    [r * scale, g * scale, b * scale]
}

/// 换完的像素
pub(crate) enum Converted {
    /// 都在 sRGB 色域里、不比白亮：直通 alpha 的 sRGB RGBA8
    Srgb(Vec<u8>),
    /// 扩展范围：直通 alpha 的线性 RGBA（sRGB 原色）
    Linear(Vec<f32>),
}

/// 源像素：直通 alpha 的 RGBA，8 位或 16 位
pub(crate) enum Samples {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

impl Shaper {
    /// 按源的位深建好每个通道的线性化表
    fn tables(&self, bits16: bool) -> Option<[Vec<f32>; 3]> {
        let size = if bits16 { 65536 } else { 256 };
        let build = |channel: usize| -> Option<Vec<f32>> {
            match &self.curves {
                Curves::Icc(profile) if bits16 => icc_table::<u16, 65536, 16>(profile, channel),
                Curves::Icc(profile) => icc_table::<u8, 256, 8>(profile, channel),
                Curves::Standard(transfer) => Some((0..size).map(|i| transfer.linearize(i as f32 / (size - 1) as f32)).collect()),
            }
        };
        if let Curves::Standard(_) = self.curves {
            let table = build(0)?;
            return Some([table.clone(), table.clone(), table]);
        }
        Some([build(0)?, build(1)?, build(2)?])
    }

    /// 换到工作空间。全都在 sRGB 色域里、不比白亮时编码回 8 位
    pub fn convert(&self, samples: &Samples) -> Option<Converted> {
        let (bits16, count) = match samples {
            Samples::U8(p) => (false, p.len() / 4),
            Samples::U16(p) => (true, p.len() / 4),
        };
        let [r_table, g_table, b_table] = self.tables(bits16)?;
        let mut out = Vec::with_capacity(count * 4);
        let mut wide = false;
        let mut pixel = |r: usize, g: usize, b: usize, alpha: f32| {
            let mut rgb = [r_table[r], g_table[g], b_table[b]];
            if self.hlg {
                rgb = hlg_ootf(rgb);
            }
            if let Some(m) = &self.matrix {
                rgb = multiply_matrix3(m, rgb);
            }
            wide |= rgb.iter().any(|&c| !(-GAMUT_EPS..=1.0 + GAMUT_EPS).contains(&c));
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], alpha]);
        };
        match samples {
            Samples::U8(p) => p.as_chunks::<4>().0.iter().for_each(|&[r, g, b, a]| pixel(r as usize, g as usize, b as usize, a as f32 / 255.0)),
            Samples::U16(p) => {
                p.as_chunks::<4>().0.iter().for_each(|&[r, g, b, a]| pixel(r as usize, g as usize, b as usize, a as f32 / 65535.0))
            }
        }
        if wide {
            return Some(Converted::Linear(out));
        }
        Some(Converted::Srgb(encode_srgb8(&out)))
    }
}

/// 直通 alpha 的线性 RGBA（在 0..=1 里）-> sRGB RGBA8
fn encode_srgb8(linear: &[f32]) -> Vec<u8> {
    let srgb = super::pixels::to_srgb();
    linear
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| {
            let c = |v: f32| srgb[(v.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize];
            [c(r), c(g), c(b), (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8]
        })
        .collect()
}
