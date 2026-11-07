//! 画好的一幅（[`Drawing`]，画布和自己定义的元素种类都用它）怎么放进显示列表。
//!
//! 记下的图元在画布坐标里，经过元素到层的变换拷进显示列表（只有平移时只加偏移）；画布里的裁剪变成显示列表的
//! 裁剪区域，父裁剪是元素外面的；渐变这时才登记（它的仿射变换要用元素的变换）；路径栅格化进为它预留的一块路径图集，
//! 文字按内容和样式缓存排版。什么时候重画见 `tree/kinds.rs`（树外更新）

use scene::{Clip, DisplayList};

use crate::canvas::{Drawing, Item, TextKey};
use crate::geometry::Affine;
use crate::geometry::Rect;
use crate::gradient::Fill;
use crate::kinds::PaintContext;
use crate::layout::Align;
use crate::text::{Ink, TextSystem};
use crate::vector::CanvasRaster;

/// 画布在显示列表里的位置
pub(crate) struct CanvasArea {
    pub bounds: Rect,
    /// 布局坐标 -> 层坐标
    pub local: Affine,
    pub opacity: f32,
    pub layer: u32,
    /// 元素外面的裁剪
    pub clip: u32,
    /// 所在的层可能有透视：画布里的裁剪要按投影算
    pub projective: bool,
    /// 继承的文字颜色
    pub ink: Ink,
}

impl Drawing {
    /// 把记下的东西放进显示列表（[`crate::kinds::ElementKind::paint`] 里调用）：经过元素的变换、透明度，
    /// 画布里的裁剪接在元素外面的裁剪上。还没画过时什么都不做
    pub fn paint(&mut self, cx: &mut PaintContext) {
        let a = cx.area;
        let start = cx.list.quads.len();
        let projective = super::may_project(cx.layers, a.layer);
        let area = CanvasArea { bounds: a.bounds, local: a.local, opacity: a.opacity, layer: a.layer, clip: a.inner, projective, ink: cx.ink };
        self.paint_into(&mut cx.tree.text, area, cx.list);
        cx.placed.get_or_insert(start);
    }

    /// 把记下的东西放进显示列表。图元自己设好层和裁剪
    pub(crate) fn paint_into(&mut self, text: &mut TextSystem, area: CanvasArea, list: &mut DisplayList) {
        let Drawing { recording, rasters, texts, style } = self;
        let Some(rec) = recording.as_ref() else { return };
        // 画布坐标 -> 层坐标
        let m = area.local.compose(&Affine { linear: Affine::IDENTITY.linear, offset: [area.bounds.x, area.bounds.y] });
        let layer_flag = if area.projective { area.layer | Clip::PROJECTIVE } else { area.layer };

        // 裁剪：父裁剪是 0 的接在元素外面的裁剪上
        let clip_base = list.clips.len() as u32;
        for c in &rec.clips {
            // 退化的变换（缩放为 0）：裁成什么都看不见
            let (inverse, half) = match m.compose(&c.transform).inverse() {
                Some(inverse) => (inverse, [c.rect.width * 0.5, c.rect.height * 0.5]),
                None => (Affine::IDENTITY, [0.0, 0.0]),
            };
            list.clips.push(Clip {
                inverse_linear: inverse.linear,
                inverse_offset: inverse.offset,
                center: c.rect.center(),
                half_size: half,
                layer: layer_flag,
                parent: if c.parent == 0 { area.clip } else { clip_base + c.parent },
                radii: [c.radius; 4],
            });
        }
        let clip = |c: u32| if c == 0 { area.clip } else { clip_base + c };

        let translate = m.is_translation();
        let mut gradients = rec.gradients.iter().peekable();
        let mut paths = 0;
        let mut used_texts: Vec<TextKey> = Vec::new();
        for item in &rec.items {
            match item {
                Item::Quads(start, end) => {
                    let first = list.quads.len();
                    list.quads.reserve((end - start) as usize);
                    for q in &rec.quads[*start as usize..*end as usize] {
                        let mut q = *q;
                        if translate {
                            q.rect[0] += m.offset[0];
                            q.rect[1] += m.offset[1];
                        } else {
                            let [w, h] = [q.rect[2], q.rect[3]];
                            let [cx, cy] = m.apply([q.rect[0] + w * 0.5, q.rect[1] + h * 0.5]);
                            q.rect[0] = cx - w * 0.5;
                            q.rect[1] = cy - h * 0.5;
                            q.transform = m.compose(&Affine { linear: q.transform, offset: [0.0, 0.0] }).linear;
                        }
                        q.opacity *= area.opacity;
                        q.layer = area.layer;
                        q.clip = clip(q.clip);
                        list.quads.push(q);
                    }
                    // 这一段里的渐变：填充按画布的变换铺满图形的外接矩形，描边在图元自己的坐标里
                    while let Some(g) = gradients.next_if(|g| g.quad < *end) {
                        let index = first + (g.quad - start) as usize;
                        if g.border {
                            let local = g.gradient.push_local(list, [g.bounds.width, g.bounds.height]);
                            list.quads[index] = list.quads[index].with_border_gradient(local);
                        } else {
                            list.quads[index].gradient = g.gradient.push(list, g.bounds, &m.compose(&g.transform));
                        }
                    }
                }
                Item::Path(p) => {
                    let [a, b, c, d] = p.transform.linear;
                    let scale = (a * d - b * c).abs().sqrt();
                    if paths >= rasters.len() {
                        rasters.push(CanvasRaster::default());
                    }
                    let raster = &mut rasters[paths];
                    paths += 1;
                    if scale <= 1e-6 {
                        continue;
                    }
                    // 缩放交给栅格化（清晰），剩下的旋转、平移交给图元的变换
                    let rest = Affine { linear: [a / scale, b / scale, c / scale, d / scale], offset: p.transform.offset };
                    let world = m.compose(&rest);
                    let gradient = match &p.fill {
                        Fill::Gradient(g) => g.push(list, p.bounds, &m.compose(&p.transform)),
                        Fill::Color(_) => 0,
                    };
                    let ink = Ink { color: p.fill.color(), opacity: area.opacity * p.opacity, gradient };
                    let first = list.quads.len();
                    text.raster().push_canvas(raster, &p.path, scale, &world, ink, &mut list.quads);
                    for q in &mut list.quads[first..] {
                        q.layer = area.layer;
                        q.clip = clip(p.clip);
                    }
                }
                Item::Text(t) => {
                    let label = &t.text_style;
                    let text_style = label.resolve(style);
                    let key = TextKey {
                        text: t.text.clone(),
                        numbers: [text_style.size.to_bits(), text_style.weight.to_bits(), text_style.letter_spacing.to_bits()],
                        family: text_style.family,
                        flags: u8::from(text_style.tabular) | (u8::from(text_style.italic) << 1),
                    };
                    let layout = texts.entry(key.clone()).or_insert_with(|| {
                        let mut layout = text.build(&t.text, &text_style);
                        layout.break_all_lines(None);
                        layout
                    });
                    used_texts.push(key);
                    let (width, height) = (layout.width(), layout.height());
                    let baseline = layout.lines().next().map_or(0.0, |line| line.metrics().baseline);
                    let [h, v] = label.anchor();
                    let x = t.at[0]
                        - match h {
                            Align::Center => width * 0.5,
                            Align::End => width,
                            _ => 0.0,
                        };
                    let y = t.at[1]
                        - match v {
                            Align::Start | Align::Stretch => 0.0,
                            Align::Center => height * 0.5,
                            Align::End => height,
                            Align::Baseline => baseline,
                        };
                    let world = m.compose(&t.transform);
                    let mut ink = Ink { opacity: area.opacity * t.opacity, ..area.ink };
                    match label.fill() {
                        Some(Fill::Color(c)) => (ink.color, ink.gradient) = (*c, 0),
                        Some(fill @ Fill::Gradient(g)) => {
                            let bounds = Rect { x, y, width, height };
                            (ink.color, ink.gradient) = (fill.color(), g.push(list, bounds, &world));
                        }
                        None => {}
                    }
                    let first = list.quads.len();
                    text.push_glyphs(layout, [x, y], &world, ink, &[], &mut list.quads);
                    for q in &mut list.quads[first..] {
                        q.layer = area.layer;
                        q.clip = clip(t.clip);
                    }
                }
            }
        }
        // 这一次没用到的路径图集预留块还回去，没用到的排版丢掉
        for raster in rasters.drain(paths..) {
            text.release_slot(raster.slot);
        }
        if texts.len() > used_texts.len() {
            let used: std::collections::HashSet<&TextKey> = used_texts.iter().collect();
            texts.retain(|k, _| used.contains(k));
        }
    }
}
