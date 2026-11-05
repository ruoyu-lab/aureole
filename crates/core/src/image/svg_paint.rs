//! 把 SVG 文档（[`SvgDocument`]）放进显示列表：`image()` 元素显示的来源是 SVG 时的内容（`svg` 特性）。
//!
//! 文档按 `fit`、对齐放进内容区（和位图同一份计算）。每条路径是路径图集里的一块覆盖率加一个图元，和 [`crate::path`] 走同一个
//! 图集、同一个缓存：缓存的键是路径和文档到物理像素的缩放（两个轴分开），所以同一个文件的很多个元素只栅格化一次，
//! 改色、移动、改透明度不重新栅格化，大小、窗口缩放变了按新的缩放重新栅格化，始终清晰。
//!
//! 元素只有平移时，文档的原点对齐到物理像素（路径、裁剪、渐变都按对齐后的位置算）；有缩放、旋转时，缩放交给栅格化（清晰），
//! 旋转交给图元的变换。
//! 分组（透明度、遮罩）是显示列表里的分组：一个新的合成层加一段图元；组内从布局坐标重新开始，透明度从 1 重新累积。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use scene::{Clip, Color, ColorMatrix, ColorSpace, DisplayList, Group, GroupMask, ImageEncoding, Quad, QuadEffect, max_atlas_size};

use super::svg::{SvgBitmap, SvgClip, SvgDocument, SvgGroup, SvgNode, SvgPaint, SvgPath};
use super::{ImageArea, ImageBlock, ImageSystem, Slot, fit_rect};
use crate::compositor::{LayerInfo, LayerValues};
use crate::geometry::{Affine, Rect};
use crate::gradient::Fill;
use crate::text::Ink;
use crate::tree::may_project;
use crate::vector::Raster;

/// 一个文档最多占路径图集最大一页的多大比例：路径的外接矩形加起来超过它时降低分辨率（图元放大显示，变软），
/// 画面是完整的，不会因为图集放不下缺几条路径
const MAX_ATLAS_SHARE: f32 = 0.35;

/// 画一个文档要用的元素树的东西
pub(crate) struct SvgTarget<'a> {
    pub raster: Raster<'a>,
    pub images: &'a ImageSystem,
    pub list: &'a mut DisplayList,
    pub layers: &'a mut Vec<LayerInfo>,
    /// 继承的文字颜色（SVG 里的当前颜色），渐变已经登记好
    pub ink: Ink,
    pub color: Fill,
    /// 元素所在的合成层、元素的裁剪
    pub layer: u32,
    pub clip: u32,
    /// 元素的标识：分组缓存的键由它和分组在文档里的编号组成
    pub key: u64,
}

/// 把文档放进显示列表。图元自己设好层和裁剪
pub(crate) fn paint(document: &SvgDocument, block: &ImageBlock, area: &ImageArea, target: SvgTarget) {
    let SvgTarget { raster, images, list, layers, ink, color, layer, clip, key } = target;
    let content = area.content;
    let [width, height] = document.size;
    if content.width <= 0.0 || content.height <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }
    let world = area.local;
    let [dx, dy, dw, dh] = fit_rect(block.fit, block.position, document.size, [content.width, content.height]);
    let scale = [dw / width, dh / height];
    // 文档的原点。元素只有平移时对齐到物理像素，栅格化的结果原样贴上去才清晰
    let mut origin = [content.x + dx, content.y + dy];
    if world.is_translation() {
        let physical = raster.scale;
        origin = [0, 1].map(|axis| ((origin[axis] + world.offset[axis]) * physical).round() / physical - world.offset[axis]);
    }
    let place = Affine { linear: [scale[0], 0.0, 0.0, scale[1]], offset: origin };

    // 布局坐标 -> 物理像素的缩放：元素自己的缩放也交给栅格化。太大时降低，图集放得下
    let [a, b, c, d] = world.linear;
    let world_scale = if world.is_translation() { 1.0 } else { (a * d - b * c).abs().sqrt() };
    if world_scale <= 1e-6 {
        return;
    }
    let mut density = raster.scale * world_scale;
    let limit = (max_atlas_size() - 2) as f32;
    let pixels = document.area * scale[0] * scale[1] * density * density;
    let budget = limit * limit * MAX_ATLAS_SHARE;
    if pixels > budget {
        density *= (budget / pixels).sqrt();
    }
    let extent = document.extent * scale[0].max(scale[1]) * density;
    if extent > limit {
        density *= limit / extent;
    }

    let start_clip = clip;
    let projective = may_project(layers, layer);
    let clip_to = |rect: Rect, radius: f32, parent: u32, list: &mut DisplayList| {
        let (inverse, half) = match world.inverse() {
            Some(inverse) => (inverse, [rect.width * 0.5, rect.height * 0.5]),
            None => (Affine::IDENTITY, [0.0, 0.0]),
        };
        list.clips.push(Clip {
            inverse_linear: inverse.linear,
            inverse_offset: inverse.offset,
            center: rect.center(),
            half_size: half,
            layer: if projective { layer | Clip::PROJECTIVE } else { layer },
            parent,
            radii: [radius; 4],
        });
        list.clips.len() as u32
    };
    let mut clip = start_clip;
    // 内容区的圆角
    if area.inner_radius > 0.0 {
        clip = clip_to(content, area.inner_radius, clip, list);
    }
    // 画到画布外面的（SVG 的根元素裁掉），`Cover`、`None` 时超出内容区的
    const EPSILON: f32 = 0.01;
    let beyond = dx < -EPSILON || dy < -EPSILON || dx + dw > content.width + EPSILON || dy + dh > content.height + EPSILON;
    if document.overflows || beyond {
        let (left, top) = (origin[0].max(content.x), origin[1].max(content.y));
        let (right, bottom) = ((origin[0] + dw).min(content.x + content.width), (origin[1] + dh).min(content.y + content.height));
        clip = clip_to(Rect { x: left, y: top, width: right - left, height: bottom - top }, 0.0, clip, list);
    }

    let mut painter = Painter {
        raster,
        images,
        list,
        layers,
        document,
        place,
        raster_scale: [scale[0] * density, scale[1] * density],
        density,
        color,
        bounds: area.bounds,
        key,
    };
    let tint = (!block.tint.is_transparent()).then_some(block.tint);
    painter.nodes(&document.nodes, &Frame { world, opacity: area.opacity, layer, clip, ink, tint });

    let first = painter.list.quads.len();
    area.push_border(painter.list);
    for quad in &mut painter.list.quads[first..] {
        quad.layer = layer;
        quad.clip = start_clip;
    }
}

/// 这一段内容所在的上下文
#[derive(Clone, Copy)]
struct Frame {
    /// 布局坐标 -> 所在的合成层
    world: Affine,
    opacity: f32,
    layer: u32,
    clip: u32,
    ink: Ink,
    /// 改色（`.tint`）：遮罩的内容不受影响
    tint: Option<Color>,
}

struct Painter<'a> {
    raster: Raster<'a>,
    images: &'a ImageSystem,
    list: &'a mut DisplayList,
    layers: &'a mut Vec<LayerInfo>,
    document: &'a SvgDocument,
    /// 文档坐标 -> 布局坐标
    place: Affine,
    /// 文档坐标 -> 栅格化的像素
    raster_scale: [f32; 2],
    /// 布局坐标里的一个单位是几个栅格化的像素
    density: f32,
    color: Fill,
    bounds: Rect,
    key: u64,
}

impl Painter<'_> {
    fn nodes(&mut self, nodes: &[SvgNode], frame: &Frame) {
        for node in nodes {
            match node {
                SvgNode::Path(path) => self.path(path, frame),
                SvgNode::Image(bitmap) => self.bitmap(bitmap, frame),
                SvgNode::Group(group) => self.group(group, frame),
                SvgNode::Clip(clip) => self.clip(clip, frame),
            }
        }
    }

    fn path(&mut self, path: &SvgPath, f: &Frame) {
        let opacity = f.opacity * path.opacity;
        if opacity <= 0.0 {
            return;
        }
        let Some(entry) = self.raster.svg_path(path.id, self.raster_scale, || path.rasterize(self.raster_scale)) else { return };
        let (color, gradient) = match (f.tint, &path.paint) {
            (Some(tint), _) => (tint, 0),
            (None, SvgPaint::Color(color)) => (*color, 0),
            (None, SvgPaint::Current) => (f.ink.color, f.ink.gradient),
            (None, SvgPaint::Gradient(g)) => {
                let Some(inverse) = f.world.compose(&self.place).compose(&path.transform).inverse() else { return };
                let to_gradient = g.to_gradient.compose(&inverse);
                let index = self.list.push_gradient(&scene::GradientPaint {
                    shape: g.shape,
                    linear: to_gradient.linear,
                    offset: to_gradient.offset,
                    color_space: ColorSpace::Srgb,
                    stops: &g.stops,
                    extended: false,
                });
                (Color::BLACK, index)
            }
        };
        let density = self.density;
        let (w, h) = (entry.width as f32 / density, entry.height as f32 / density);
        let (left, top) = (entry.left as f32 / density, entry.top as f32 / density);
        let origin = self.place.offset;
        let [cx, cy] = f.world.apply([origin[0] + left + w * 0.5, origin[1] + top + h * 0.5]);
        let mut quad = Quad::path(cx - w * 0.5, cy - h * 0.5, w, h, entry.uv, color);
        quad.transform = f.world.linear;
        quad.opacity = opacity;
        quad.gradient = gradient;
        quad.layer = f.layer;
        quad.clip = f.clip;
        self.list.quads.push(quad);
    }

    fn bitmap(&mut self, bitmap: &SvgBitmap, f: &Frame) {
        let opacity = f.opacity * bitmap.opacity;
        if opacity <= 0.0 {
            return;
        }
        let image = &self.document.images[bitmap.index];
        let Some(slot) = self.images.embedded_slot(image.id) else { return };
        let (bounds, kind) = self.images.texture_range(slot, &image.data);
        let m = f.world.compose(&self.place).compose(&bitmap.transform);
        let [w, h] = bitmap.size;
        let [cx, cy] = m.apply([w * 0.5, h * 0.5]);
        let mut quad = Quad::new(cx - w * 0.5, cy - h * 0.5, w, h);
        quad.transform = m.linear;
        quad.uv = bounds;
        quad.shadow_offset = [bounds[0], bounds[1]];
        quad.shadow_blur = bounds[2];
        quad.border_width = bounds[3];
        quad.kind = kind;
        quad.opacity = opacity;
        quad.layer = f.layer;
        quad.clip = f.clip;
        if let Some(tint) = f.tint {
            quad.gradient = self.list.push_filter(&ColorMatrix::tint(tint));
        }
        if let Slot::Texture(texture) = slot {
            self.list.quad_effects.push(QuadEffect::Image { quad: self.list.quads.len() as u32, texture });
        }
        self.list.extended |= image.data.encoding != ImageEncoding::Srgb8;
        self.list.quads.push(quad);
    }

    /// 一个矩形的裁剪路径：显示列表的裁剪区域，不离屏
    fn clip(&mut self, clip: &SvgClip, f: &Frame) {
        let m = f.world.compose(&self.place).compose(&clip.transform);
        // 退化的变换（缩放为 0）：什么都看不见
        let Some(inverse) = m.inverse() else { return };
        self.list.clips.push(Clip {
            inverse_linear: inverse.linear,
            inverse_offset: inverse.offset,
            center: clip.rect.center(),
            half_size: [clip.rect.width * 0.5, clip.rect.height * 0.5],
            layer: if may_project(self.layers, f.layer) { f.layer | Clip::PROJECTIVE } else { f.layer },
            parent: f.clip,
            radii: [0.0; 4],
        });
        let inner = Frame { clip: self.list.clips.len() as u32, ..*f };
        self.nodes(&clip.children, &inner);
    }

    /// 分组：一个新的合成层加一段图元，先画进离屏纹理，再按透明度、遮罩合成
    fn group(&mut self, group: &SvgGroup, f: &Frame) {
        let quads_before = self.list.quads.len() as u32;
        let (layers_before, groups_before, effects_before) = (self.layers.len(), self.list.groups.len(), self.list.quad_effects.len());
        self.layers.push(LayerInfo {
            // 这一层不是元素的层：渲染线程上没有它的动画
            key: 0,
            parent: f.layer,
            base: f.world,
            base_opacity: f.opacity,
            center: [0.0; 2],
            values: LayerValues { translate: [0.0; 2], scale: 1.0, rotation: 0.0, opacity: group.opacity, offset: [0.0; 2] },
            group: false,
            depth: None,
        });
        let layer = self.layers.len() as u32;
        let mut ink = f.ink;
        // 文字颜色是渐变：渐变的变换要到这一层的坐标里
        if let Fill::Gradient(gradient) = &self.color {
            ink.gradient = gradient.push(self.list, self.bounds, &Affine::IDENTITY);
        }
        let inner = Frame { world: Affine::IDENTITY, opacity: 1.0, layer, clip: f.clip, ink, tint: f.tint };
        self.nodes(&group.children, &inner);
        let mask = group.matte.as_ref().map(|matte| {
            let (start, first_layer) = (self.list.quads.len() as u32, self.layers.len() as u32 + 1);
            self.nodes(&matte.nodes, &Frame { tint: None, ..inner });
            GroupMask { quads: start..self.list.quads.len() as u32, first_layer, luminance: matte.luminance }
        });
        let end = self.list.quads.len() as u32;
        // 没有内容，或者遮罩什么都没画（图集满了、都退化了）：什么都看不见
        let content_end = mask.as_ref().map_or(end, |m| m.quads.start);
        if content_end == quads_before || mask.as_ref().is_some_and(|m| m.quads.is_empty()) {
            self.list.quads.truncate(quads_before as usize);
            self.list.groups.truncate(groups_before);
            self.list.quad_effects.truncate(effects_before);
            self.layers.truncate(layers_before);
            return;
        }
        let mut hasher = DefaultHasher::new();
        (self.key, group.index).hash(&mut hasher);
        self.list.groups.push(Group { quads: quads_before..end, layers: layer..self.layers.len() as u32 + 1, clip: f.clip, key: hasher.finish() | 1, mask, ..Default::default() });
        self.layers[layer as usize - 1].group = true;
    }
}
