//! 分组的纹理缓存：内容没变、只有分组自己的变换、透明度、滤镜在变时，直接拿上次画好的纹理合成。
//!
//! 分成两半：[`CacheState`] 是纯数据（每个分组的纹理按什么内容、什么变换、在屏幕上哪里画的，占多少字节），
//! 规划一帧时只读它，所以规划能脱离 GPU 测试；[`Cache`] 在它旁边放着纹理本身，纹理借自纹理池，留在缓存里时钉住。
//!
//! - **内容有没有变**：换显示列表时给每个有标识的分组算一个签名：组内图元、裁剪、渐变、特效、嵌套分组的参数，
//!   层号换成相对这个分组的，裁剪区域、渐变、颜色矩阵顺着下标换成它们的内容，所以别处增删元素（下标整体挪动）不算变。签名由显示列表算出，
//!   不会漏掉任何一种改动（颜色、主题、大小、文字、图片……）。组内各层相对分组的变换和透明度每帧比较（组内有东西在动时重画）。
//! - **变换**：纹理是按画的那一帧的变换 G 画在屏幕上的。之后分组的变换变成 G' 时，用 G'·G⁻¹ 把纹理贴过去
//!   （平移、缩放、旋转、透视都可以）。没变、或者只平移了整数个像素时逐像素和直接画一样；
//!   在动、变形不大时近似地用（放大不超过 1.25 倍，缩小不小于一半），停下的那一帧按此刻的变换重画一次。
//! - **预算**：纹理占用的字节数超过预算时按最久没用的先赶出去；空闲满 [`IDLE_RELEASE`]（按最近用到的时刻算，不按帧数）的也赶出去，渲染器空闲时也能这样归还显存。
//!   缩放系数变了、图集重建时全部作废；图片的内容变了（动图换帧）时有图片的分组作废。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;

use crate::IDLE_RELEASE;
use crate::pool::{Pool, Target};
use crate::{Clip, DisplayList, LayerTransform, Quad, QuadEffect};

/// 默认的缓存预算：64 MB
pub const DEFAULT_CACHE_BUDGET: u64 = 64 << 20;
/// 近似使用时允许的变形：放大不超过、缩小不小于
const MAX_STRETCH: f64 = 1.25;
const MIN_STRETCH: f64 = 0.5;

/// 屏幕上的矩形，物理像素
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl PxRect {
    /// 逻辑像素的外接矩形 `[x0, y0, x1, y1]` 向外取整到物理像素
    pub fn from_bounds(b: [f32; 4], scale: f32) -> Option<Self> {
        if !(b[0] < b[2] && b[1] < b[3]) {
            return None;
        }
        let (x0, y0) = ((b[0] * scale).floor(), (b[1] * scale).floor());
        let (x1, y1) = ((b[2] * scale).ceil(), (b[3] * scale).ceil());
        Some(Self { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
    }

    pub fn intersect(self, other: Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w as i32).min(other.x + other.w as i32);
        let y1 = (self.y + self.h as i32).min(other.y + other.h as i32);
        (x1 > x0 && y1 > y0).then(|| Self { x: x0, y: y0, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
    }

    pub fn expand(self, by: i32) -> Self {
        Self { x: self.x - by, y: self.y - by, w: self.w + 2 * by as u32, h: self.h + 2 * by as u32 }
    }

    /// 宽、高都不超过 `max`（左上角不动，超出的部分截掉）
    pub fn limit(self, max: u32) -> Self {
        Self { w: self.w.min(max), h: self.h.min(max), ..self }
    }
}

/// 一个分组的内容签名（换显示列表时算）
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Signature {
    pub key: u64,
    pub content: u64,
    /// 遮罩的签名，没有遮罩时为 0
    pub mask: u64,
    /// 组内有图片或路径：图片的内容（动图换帧）、路径图集里预留的一块（路径变形、画布重画）变了时作废
    pub images: bool,
}

/// 这一帧怎么画一个离屏分组
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Decision {
    /// 不离屏：直接画在原处，或者不可见
    Inline,
    /// 在一个用了缓存的祖先分组里：不用画
    Covered,
    /// 不缓存，照常画
    Fresh,
    /// 用缓存的纹理；`warp` 是从画的时候到现在分组在屏幕上的变化（`None` 表示没变）
    Hit { warp: Option<LayerTransform>, approximate: bool },
    /// 重画内容或遮罩（另一部分沿用缓存）
    Render { content: bool, mask: bool },
}

/// 缓存的一部分（内容或遮罩）的状态
struct PartState {
    signature: u64,
    /// 这部分的各层相对分组的变换和透明度
    relative: Vec<[f32; 11]>,
    /// 纹理占的字节数
    bytes: u64,
}

/// 一个分组的缓存状态（纹理另放在 [`Cache`] 里）
struct EntryState {
    content: Option<PartState>,
    mask: Option<PartState>,
    /// 画在屏幕上的范围（画的那一帧的）
    rect: PxRect,
    /// 范围被屏幕截掉了一部分：只有分组完全没动时能用
    partial: bool,
    /// 画的那一帧分组的变换
    transform: Mat3,
    /// 上一帧分组的变换（判断在不在动）
    previous: LayerTransform,
    scale: f32,
    /// 画的时候图片的版本（组里没有图片时不看）
    images: u64,
    /// 最后用到的帧号（这一帧用到的不因超预算赶出）
    last_used: u64,
    /// 最后用到的时刻：空闲满 [`IDLE_RELEASE`] 的赶出去
    used_at: Instant,
}

impl EntryState {
    fn bytes(&self) -> u64 {
        self.content.iter().chain(&self.mask).map(|p| p.bytes).sum()
    }
}

/// 一张离屏纹理占的字节数
pub(crate) fn target_bytes(target: &Target) -> u64 {
    let pixel = target.format.block_copy_size(None).unwrap_or(8) as u64;
    target.size[0] as u64 * target.size[1] as u64 * pixel
}

/// 分组缓存的状态：每个分组缓存的纹理是按什么内容、什么变换、在屏幕上哪里画的，占多少字节。
/// 纯数据，不碰 GPU：规划一帧时只读它（[`CacheState::decide`]），编码器画完再把结果记回来
#[derive(Default)]
pub(crate) struct CacheState {
    entries: HashMap<u64, EntryState>,
    bytes: u64,
    frame: u64,
    /// 图片和路径图集的版本：图片的内容、路径图集里的像素变了加一
    pub images: u64,
    /// 这一帧赶出去的分组数
    pub evictions: u32,
}

impl CacheState {
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.evictions = 0;
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 有这个分组的缓存（不管还能不能用）
    pub fn contains(&self, key: u64) -> bool {
        self.entries.contains_key(&key)
    }

    /// 这个分组缓存的纹理画在屏幕上的范围（画的那一帧的）
    pub fn rect(&self, key: u64) -> Option<PxRect> {
        self.entries.get(&key).map(|e| e.rect)
    }

    /// 这一帧这个分组怎么画。`rect` 是此刻在屏幕上的范围，`partial` 表示它被屏幕截掉了一部分。
    /// 只读：用到了缓存的分组由编码器之后调用 [`CacheState::touch`] 记下
    pub fn decide(
        &self,
        group: &crate::Group,
        signature: &Signature,
        layers: &[LayerTransform],
        rect: PxRect,
        partial: bool,
        scale: f32,
    ) -> Decision {
        let has_mask = group.mask.is_some();
        let Some(entry) = self.entries.get(&signature.key) else { return Decision::Render { content: true, mask: has_mask } };
        let now = layer(layers, group.layers.start);
        let moving = !close_layers(&now, &entry.previous);
        let (content_layers, mask_layers) = part_layers(group);
        let fresh = entry.scale == scale && (!signature.images || entry.images == self.images);
        let valid = |part: &Option<PartState>, sig: u64, range: Range<u32>| {
            part.as_ref().is_some_and(|p| fresh && p.signature == sig && same_relative(&p.relative, layers, group.layers.start, range))
        };
        let content_ok = valid(&entry.content, signature.content, content_layers);
        let mask_ok = !has_mask || valid(&entry.mask, signature.mask, mask_layers);
        let warp = mat(&now).and_then(|now| Some(mul(&now, &inverse(&entry.transform)?)));
        let Some(warp) = warp else { return Decision::Render { content: true, mask: has_mask } };
        let identity = is_identity(&warp);
        // 平移了整数个物理像素：纹素和像素仍然对齐，逐像素一样
        let aligned = identity || is_pixel_shift(&warp, scale);
        let covered = !entry.partial || (identity && entry.rect == rect);
        if content_ok && mask_ok && covered {
            let transform = (!identity).then(|| to_layer(&warp, now.group_opacity));
            if aligned {
                return Decision::Hit { warp: transform, approximate: false };
            }
            if moving && !entry.partial && mild(&warp, entry.rect, scale) {
                return Decision::Hit { warp: transform, approximate: true };
            }
            return Decision::Render { content: true, mask: has_mask };
        }
        // 分组没动、范围没变：只重画变了的那部分（滚动的内容重画，遮罩沿用）
        if identity && entry.rect == rect && entry.partial == partial {
            return Decision::Render { content: !content_ok, mask: has_mask && !mask_ok };
        }
        Decision::Render { content: true, mask: has_mask }
    }

    /// 这一帧规划时用到了这个分组的缓存：记下分组此刻的变换（下一帧判断在不在动）和用到的帧（这一帧不因超预算赶出它）
    pub fn touch(&mut self, key: u64, layer: LayerTransform, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.previous = layer;
            entry.last_used = self.frame;
            entry.used_at = now;
        }
    }

    /// 画好了（全部或一部分）：记下来。`content`、`mask` 是新画的那部分纹理的字节数（`None` 表示沿用缓存里的）。
    /// 返回因此赶出去的分组：超出预算时先赶最久没用的（这一帧用到的不动）；还超就是这一个也放不下，它也在里面。
    /// 分组此刻的变换不是有限值时不记，返回 `None`
    pub fn store(
        &mut self,
        group: &crate::Group,
        signature: &Signature,
        layers: &[LayerTransform],
        rect: PxRect,
        partial: bool,
        scale: f32,
        content: Option<u64>,
        mask: Option<u64>,
        budget: u64,
        now: Instant,
    ) -> Option<Vec<u64>> {
        let key = signature.key;
        let current = layer(layers, group.layers.start);
        let transform = mat(&current)?;
        let (content_layers, mask_layers) = part_layers(group);
        let part = |bytes: u64, sig: u64, range: Range<u32>| PartState { signature: sig, relative: relative(layers, group.layers.start, range), bytes };
        let mut entry = match self.entries.remove(&key) {
            Some(old) => {
                self.bytes -= old.bytes();
                old
            }
            None => EntryState {
                content: None,
                mask: None,
                rect,
                partial,
                transform,
                previous: current,
                scale,
                images: 0,
                last_used: self.frame,
                used_at: now,
            },
        };
        if let Some(bytes) = content {
            entry.content = Some(part(bytes, signature.content, content_layers));
        }
        if let Some(bytes) = mask {
            entry.mask = Some(part(bytes, signature.mask, mask_layers));
        }
        entry.rect = rect;
        entry.partial = partial;
        entry.transform = transform;
        entry.previous = current;
        entry.scale = scale;
        entry.images = self.images;
        entry.last_used = self.frame;
        entry.used_at = now;
        self.bytes += entry.bytes();
        self.entries.insert(key, entry);
        let mut evicted = Vec::new();
        while self.bytes > budget {
            let oldest = self.entries.iter().filter(|(_, e)| e.last_used < self.frame).min_by_key(|(_, e)| e.last_used).map(|(k, _)| *k);
            let Some(oldest) = oldest else { break };
            self.remove(oldest);
            evicted.push(oldest);
            self.evictions += 1;
        }
        if self.bytes > budget {
            self.remove(key);
            evicted.push(key);
            self.evictions += 1;
        }
        Some(evicted)
    }

    /// 去掉一个分组，返回原来有没有
    pub fn remove(&mut self, key: u64) -> bool {
        match self.entries.remove(&key) {
            Some(entry) => {
                self.bytes -= entry.bytes();
                true
            }
            None => false,
        }
    }

    /// 全部去掉，返回去掉的分组
    pub fn clear(&mut self) -> Vec<u64> {
        self.bytes = 0;
        self.entries.drain().map(|(k, _)| k).collect()
    }

    /// 只留下 `keys` 里的分组（新的显示列表里没有的不用留着了），返回去掉的
    pub fn retain(&mut self, keys: &HashSet<u64>) -> Vec<u64> {
        let gone: Vec<u64> = self.entries.keys().filter(|k| !keys.contains(k)).copied().collect();
        for &key in &gone {
            self.remove(key);
        }
        gone
    }

    /// 空闲满 [`IDLE_RELEASE`] 的去掉（`now` 是此刻；一帧结束时、渲染器空闲时都可以调用），返回去掉的
    pub fn release_idle(&mut self, now: Instant) -> Vec<u64> {
        let idle: Vec<u64> =
            self.entries.iter().filter(|(_, e)| now.saturating_duration_since(e.used_at) >= IDLE_RELEASE).map(|(k, _)| *k).collect();
        for &key in &idle {
            self.remove(key);
            self.evictions += 1;
        }
        idle
    }

    /// 最早什么时候有分组空闲满 [`IDLE_RELEASE`]；缓存是空的时是 `None`
    pub fn next_release(&self) -> Option<Instant> {
        self.entries.values().map(|e| e.used_at + IDLE_RELEASE).min()
    }
}

/// 一个分组缓存的纹理：内容、遮罩，和合成用的绑定（内容 + 遮罩，只在要滤镜、遮罩、混合时有）
#[derive(Default)]
pub(crate) struct Textures {
    pub content: Option<Target>,
    pub mask: Option<Target>,
    pub bind: Option<wgpu::BindGroup>,
}

/// 分组缓存：状态（[`CacheState`]，纯数据）加上纹理。纹理借自纹理池，留在缓存里时钉住，去掉时还回去
#[derive(Default)]
pub(crate) struct Cache {
    pub state: CacheState,
    textures: HashMap<u64, Textures>,
}

impl Cache {
    pub fn get(&self, key: u64) -> Option<&Textures> {
        self.textures.get(&key)
    }

    /// 画好了（全部或一部分）：记进缓存。`content`、`mask` 是新画的部分（`None` 表示沿用缓存里的）。
    /// 返回值：放进去了（没超预算）
    pub fn store(
        &mut self,
        group: &crate::Group,
        signature: &Signature,
        layers: &[LayerTransform],
        rect: PxRect,
        partial: bool,
        scale: f32,
        content: Option<Target>,
        mask: Option<Target>,
        bind: Option<wgpu::BindGroup>,
        budget: u64,
        now: Instant,
        pool: &mut Pool,
    ) -> bool {
        let key = signature.key;
        let bytes = |t: &Option<Target>| t.as_ref().map(target_bytes);
        // 没记下时新画的纹理不钉住，这一帧结束就回到纹理池
        let Some(evicted) = self.state.store(group, signature, layers, rect, partial, scale, bytes(&content), bytes(&mask), budget, now) else {
            return false;
        };
        // 新画的纹理钉住，换下来的还给纹理池
        let textures = self.textures.entry(key).or_default();
        for (slot, new) in [(&mut textures.content, content), (&mut textures.mask, mask)] {
            if let Some(new) = new {
                pool.pin(&new.texture);
                if let Some(old) = slot.replace(new) {
                    pool.unpin(&old.texture);
                }
            }
        }
        textures.bind = bind;
        let kept = !evicted.contains(&key);
        self.release(&evicted, pool);
        kept
    }

    /// 全部作废，纹理还给纹理池
    pub fn clear(&mut self, pool: &mut Pool) {
        let gone = self.state.clear();
        self.release(&gone, pool);
    }

    /// 新的显示列表里没有的分组不用留着了
    pub fn retain(&mut self, keys: &HashSet<u64>, pool: &mut Pool) {
        let gone = self.state.retain(keys);
        self.release(&gone, pool);
    }

    /// 空闲满 [`IDLE_RELEASE`] 的赶出去。它们的纹理已经空闲了这么久，直接释放、归还显存，不再回到纹理池
    pub fn release_idle(&mut self, now: Instant, pool: &mut Pool) {
        for key in self.state.release_idle(now) {
            if let Some(textures) = self.textures.remove(&key) {
                for target in textures.content.iter().chain(&textures.mask) {
                    pool.discard(&target.texture);
                }
            }
        }
    }

    /// 这些分组的纹理还给纹理池
    fn release(&mut self, keys: &[u64], pool: &mut Pool) {
        for key in keys {
            if let Some(textures) = self.textures.remove(key) {
                for target in textures.content.iter().chain(&textures.mask) {
                    pool.unpin(&target.texture);
                }
            }
        }
    }
}

/// 内容和遮罩各自的合成层（不含分组自己的层）
fn part_layers(group: &crate::Group) -> (Range<u32>, Range<u32>) {
    let start = group.layers.start + 1;
    match &group.mask {
        Some(mask) => (start..mask.first_layer.max(start), mask.first_layer.max(start)..group.layers.end),
        None => (start..group.layers.end, group.layers.end..group.layers.end),
    }
}

fn layer(layers: &[LayerTransform], index: u32) -> LayerTransform {
    layers.get(index as usize).copied().unwrap_or(LayerTransform::IDENTITY)
}

/// 组内各层相对分组的变换（齐次坐标的 3x3，规范化到右下角为 1）和透明度
fn relative(layers: &[LayerTransform], base: u32, range: Range<u32>) -> Vec<[f32; 11]> {
    let Some(inverse) = mat(&layer(layers, base)).and_then(|m| inverse(&m)) else { return vec![[f32::NAN; 11]; range.len()] };
    range
        .map(|k| {
            let t = layer(layers, k);
            let Some(m) = mat(&t) else { return [f32::NAN; 11] };
            let r = normalize(&mul(&inverse, &m));
            let mut out = [0.0; 11];
            for (o, v) in out.iter_mut().zip(r.iter().flatten()) {
                *o = *v as f32;
            }
            out[9] = t.opacity;
            out[10] = t.group_opacity;
            out
        })
        .collect()
}

fn same_relative(cached: &[[f32; 11]], layers: &[LayerTransform], base: u32, range: Range<u32>) -> bool {
    if cached.len() != range.len() {
        return false;
    }
    if cached.is_empty() {
        return true;
    }
    let now = relative(layers, base, range);
    cached.iter().zip(&now).all(|(a, b)| a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-4 * (1.0 + x.abs())))
}

fn close_layers(a: &LayerTransform, b: &LayerTransform) -> bool {
    let close = |x: &[f32], y: &[f32]| x.iter().zip(y).all(|(p, q)| (p - q).abs() <= 1e-5 * (1.0 + p.abs()));
    close(&a.linear, &b.linear) && close(&a.offset, &b.offset) && close(&a.projective[..3], &b.projective[..3])
}

type Mat3 = [[f64; 3]; 3];

/// 层的变换 -> 3x3 矩阵（行主序）；有非有限值时为 `None`
fn mat(t: &LayerTransform) -> Option<Mat3> {
    let [a, b, c, d] = t.linear.map(f64::from);
    let [e, f] = t.offset.map(f64::from);
    let [g, h, i, _] = t.projective.map(f64::from);
    let m = [[a, c, e], [b, d, f], [g, h, i]];
    m.iter().flatten().all(|v| v.is_finite()).then_some(m)
}

fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn inverse(m: &Mat3) -> Option<Mat3> {
    let [[a, b, c], [d, e, f], [g, h, i]] = *m;
    let co = [e * i - f * h, f * g - d * i, d * h - e * g];
    let det = a * co[0] + b * co[1] + c * co[2];
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        [co[0] * inv, (c * h - b * i) * inv, (b * f - c * e) * inv],
        [co[1] * inv, (a * i - c * g) * inv, (c * d - a * f) * inv],
        [co[2] * inv, (b * g - a * h) * inv, (a * e - b * d) * inv],
    ])
}

/// 齐次坐标的矩阵按右下角规范化（等比的矩阵是同一个变换）
fn normalize(m: &Mat3) -> Mat3 {
    let s = m[2][2];
    if s.abs() < 1e-12 { *m } else { m.map(|row| row.map(|v| v / s)) }
}

fn is_identity(m: &Mat3) -> bool {
    let m = normalize(m);
    let linear = [m[0][0] - 1.0, m[0][1], m[1][0], m[1][1] - 1.0, m[2][0], m[2][1]];
    linear.iter().all(|v| v.abs() < 1e-6) && m[0][2].abs() < 1e-3 && m[1][2].abs() < 1e-3
}

/// 只平移了整数个物理像素
fn is_pixel_shift(m: &Mat3, scale: f32) -> bool {
    let m = normalize(m);
    let linear = [m[0][0] - 1.0, m[0][1], m[1][0], m[1][1] - 1.0, m[2][0], m[2][1]];
    let whole = |v: f64| {
        let px = v * scale as f64;
        (px - px.round()).abs() < 1e-3
    };
    linear.iter().all(|v| v.abs() < 1e-6) && whole(m[0][2]) && whole(m[1][2])
}

/// 变形不大：缓存的范围四条边贴过去以后的长度比在 [`MIN_STRETCH`]..[`MAX_STRETCH`] 里，四个角都在视点前面
fn mild(m: &Mat3, rect: PxRect, scale: f32) -> bool {
    let s = scale as f64;
    let (x0, y0) = (rect.x as f64 / s, rect.y as f64 / s);
    let (x1, y1) = (x0 + rect.w as f64 / s, y0 + rect.h as f64 / s);
    let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
    let mut mapped = [[0.0; 2]; 4];
    for (out, [x, y]) in mapped.iter_mut().zip(corners) {
        let w = m[2][0] * x + m[2][1] * y + m[2][2];
        if w <= 1e-6 {
            return false;
        }
        *out = [(m[0][0] * x + m[0][1] * y + m[0][2]) / w, (m[1][0] * x + m[1][1] * y + m[1][2]) / w];
    }
    let length = |a: [f64; 2], b: [f64; 2]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    (0..4).all(|i| {
        let j = (i + 1) % 4;
        let before = length(corners[i], corners[j]).max(1e-6);
        let ratio = length(mapped[i], mapped[j]) / before;
        (MIN_STRETCH..=MAX_STRETCH).contains(&ratio)
    })
}

/// 3x3 矩阵 -> 合成用的层（透明度给分组合成）
fn to_layer(m: &Mat3, opacity: f32) -> LayerTransform {
    let m = normalize(m);
    LayerTransform {
        linear: [m[0][0] as f32, m[1][0] as f32, m[0][1] as f32, m[1][1] as f32],
        offset: [m[0][2] as f32, m[1][2] as f32],
        opacity,
        group_opacity: opacity,
        projective: [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32, 0.0],
    }
}

/// 简单快速的 64 位散列（内容签名用，不需要抗碰撞攻击）
struct Hash(u64);

impl Hash {
    fn word(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.word(u64::from_le_bytes(*chunk));
        }
        for &b in rest {
            self.word(b as u64);
        }
    }
}

/// 换显示列表时算每个有标识的分组的签名。返回 `None` 的分组不缓存（组里有随时间变化的着色器、
/// 引用的数据不在显示列表里）；同一个标识出现两次（拖动的影子里又画了一遍）时都不缓存
pub(crate) fn signatures(list: &DisplayList) -> Vec<Option<Signature>> {
    let mut seen: HashMap<u64, usize> = HashMap::new();
    let mut out: Vec<Option<Signature>> = Vec::with_capacity(list.groups.len());
    for (i, group) in list.groups.iter().enumerate() {
        if group.key == 0 {
            out.push(None);
            continue;
        }
        if let Some(&first) = seen.get(&group.key) {
            out[first] = None;
            out.push(None);
            continue;
        }
        seen.insert(group.key, i);
        out.push(signature(list, i));
    }
    out
}

fn signature(list: &DisplayList, index: usize) -> Option<Signature> {
    let group = &list.groups[index];
    let (content, mask) = match &group.mask {
        Some(m) => (group.quads.start..m.quads.start, Some(m.quads.clone())),
        None => (group.quads.clone(), None),
    };
    let mut images = false;
    let content = part_signature(list, index, content, &mut images)?;
    let mask = match mask {
        Some(quads) => part_signature(list, index, quads, &mut images)?,
        None => 0,
    };
    Some(Signature { key: group.key, content, mask, images })
}

/// 一部分（内容或遮罩）的签名。图元里的层号换成相对分组的；裁剪区域、渐变、滤镜的下标换成它们的内容
/// （顺着引用找，所以别处增删元素、下标整体挪动不算变）
fn part_signature(list: &DisplayList, index: usize, quads: Range<u32>, images: &mut bool) -> Option<u64> {
    let group = &list.groups[index];
    let base = group.layers.start;
    let mut refs = Refs { list, group_clip: group.clip, base, clips: HashMap::new(), data: HashMap::new() };
    let mut hash = Hash(0x9e37_79b9_7f4a_7c15);
    for quad in list.quads.get(quads.start as usize..quads.end as usize)? {
        let mut q = *quad;
        q.layer = q.layer.wrapping_sub(base);
        let clip = refs.clip(q.clip)?;
        let data = refs.data(q.gradient, matches!(q.kind, Quad::COLOR_GLYPH | Quad::IMAGE | Quad::IMAGE_TEXTURE))?;
        // 渐变边框的下标也换成渐变的内容
        let border = refs.data(q.border_gradient(), false)?;
        if border != 0 {
            q.uv[0] = 0.0;
        }
        (q.clip, q.gradient) = (0, 0);
        // 路径图集里预留的一块会原地重写（路径变形、画布重画）：和图片一样，图集有更新时不再用缓存
        *images |= q.kind == Quad::IMAGE || q.kind == Quad::IMAGE_TEXTURE || q.kind == Quad::PATH;
        hash.bytes(bytemuck::bytes_of(&q));
        hash.word(clip);
        hash.word(data);
        hash.word(border);
    }
    let first = list.quad_effects.partition_point(|e| e.quad() < quads.start);
    for effect in list.quad_effects[first..].iter().take_while(|e| e.quad() < quads.end) {
        let at = (effect.quad() - quads.start) as u64;
        match *effect {
            QuadEffect::Backdrop { blur, saturation, .. } => {
                hash.word(1 << 32 | at);
                hash.word(((blur.to_bits() as u64) << 32) | saturation.to_bits() as u64);
            }
            QuadEffect::Shader { shader, .. } => {
                // 随时间变化的着色器每帧都不一样
                if list.animated {
                    return None;
                }
                hash.word(2 << 32 | at);
                hash.bytes(list.shaders.get(shader as usize)?.as_bytes());
            }
            QuadEffect::Image { texture, .. } => {
                *images = true;
                hash.word(3 << 32 | at);
                hash.word(texture as u64);
            }
        }
    }
    // 范围里的嵌套分组：它们的参数也是内容（分组按前序排列，从范围的起点找起）
    let first = list.groups.partition_point(|g| g.quads.start < quads.start);
    for (k, inner) in list.groups.iter().enumerate().skip(first) {
        if inner.quads.start >= quads.end {
            break;
        }
        if k == index || inner.quads.end > quads.end {
            continue;
        }
        hash.word(((inner.quads.start - quads.start) as u64) << 32 | (inner.quads.end - quads.start) as u64);
        hash.word((inner.layers.start.wrapping_sub(base) as u64) << 32 | inner.layers.end.wrapping_sub(base) as u64);
        hash.word((inner.blur.to_bits() as u64) << 32 | inner.blend as u64);
        hash.word(refs.clip(inner.clip)?);
        hash.word(refs.data(inner.filter, true)?);
        let mask = inner.mask.as_ref().map_or(0, |m| (((m.quads.start - quads.start) as u64) << 8) | 2 | m.luminance as u64);
        hash.word((mask << 1) | inner.offscreen as u64);
    }
    Some(hash.0)
}

/// 顺着下标找到裁剪区域、渐变、颜色矩阵的内容，算出散列（记下算过的）
struct Refs<'a> {
    list: &'a DisplayList,
    group_clip: u32,
    base: u32,
    clips: HashMap<u32, u64>,
    data: HashMap<u32, u64>,
}

impl Refs<'_> {
    /// 裁剪链的散列：到分组外面的裁剪（合成时才用）为止；层号相对分组
    fn clip(&mut self, index: u32) -> Option<u64> {
        if index == self.group_clip {
            return Some(1);
        }
        if index == 0 {
            return Some(2);
        }
        if let Some(&h) = self.clips.get(&index) {
            return Some(h);
        }
        let mut c: Clip = *self.list.clips.get(index as usize - 1)?;
        // 链上的父裁剪下标比自己小，递归最多十几层
        let parent = if c.parent < index { self.clip(c.parent)? } else { return None };
        c.layer = ((c.layer & !Clip::PROJECTIVE).wrapping_sub(self.base)) | (c.layer & Clip::PROJECTIVE);
        c.parent = 0;
        let mut hash = Hash(parent);
        hash.bytes(bytemuck::bytes_of(&c));
        self.clips.insert(index, hash.0);
        Some(hash.0)
    }

    /// 渐变或颜色矩阵（`matrix`）的内容的散列；0 表示没有
    fn data(&mut self, index: u32, matrix: bool) -> Option<u64> {
        if index == 0 {
            return Some(0);
        }
        if let Some(&h) = self.data.get(&index) {
            return Some(h);
        }
        let g = &self.list.gradients;
        let start = index as usize;
        let len = if matrix {
            5
        } else {
            // 表头两项，色标位置每 4 个一组，再是色标颜色（见 `DisplayList::push_gradient`）
            let count = g.get(start + 1)?[3] as usize;
            2 + count.div_ceil(4) + count
        };
        let mut hash = Hash(3 + matrix as u64);
        hash.bytes(bytemuck::cast_slice(g.get(start..start + len)?));
        self.data.insert(index, hash.0);
        Some(hash.0)
    }
}
