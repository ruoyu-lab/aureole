//! 图片（[`crate::image()`]）是一种元素种类：没指定大小时用原始大小，画进图集（或单独的纹理），边框画在图片上面；
//! 卸载时还回图片的引用。解码、动图的播放见 `image.rs`、`tree/animated.rs`。

use crate::image::{ImageArea, ImageBlock};
use crate::kinds::{AvailableSpace, ElementKind, KindInfo, MeasureContext, PaintContext, UnmountContext};
use crate::property::registry::KindMask;

impl ElementKind for ImageBlock {
    fn name(&self) -> &'static str {
        "image"
    }

    fn kind_mask(&self) -> KindMask {
        KindMask::IMAGE
    }

    /// 没指定大小时用原始大小；只指定了一边时按宽高比
    fn measure(&mut self, _: &mut MeasureContext, known: [Option<f32>; 2], _: [AvailableSpace; 2]) -> [f32; 2] {
        super::keep_aspect(known, self.intrinsic())
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        let area = image_area(cx.tree, cx.id, cx.area);
        #[cfg(feature = "svg")]
        if let Some(document) = self.document() {
            let a = cx.area;
            let start = cx.list.quads.len();
            let target = crate::image::SvgTarget {
                color: cx.tree.nodes[cx.id].paint.color.clone(),
                raster: cx.tree.text.raster(),
                images: cx.tree.images.system(),
                list: cx.list,
                layers: cx.layers,
                ink: cx.ink,
                layer: a.layer,
                clip: a.inner,
                key: crate::tree::layer_key(cx.id),
            };
            self.paint_document(&document, &area, target);
            cx.placed.get_or_insert(start);
            return;
        }
        cx.tree.images.system_mut().push(self, &area, cx.list);
    }

    fn unmount(&mut self, cx: &mut UnmountContext) {
        cx.images.detach(self);
    }

    fn describe(&self, info: &mut KindInfo) {
        info.summary = self.source_label();
        info.fields.push(("fit", format!("{:?}", self.fit)));
        if let Some(player) = &self.player {
            info.fields.push(("frame", player.frame.to_string()));
        }
        #[cfg(feature = "svg")]
        if let Some(document) = self.document() {
            info.fields.push(("svg_paths", document.paths.to_string()));
        }
    }
}

/// 图片（和外部纹理）铺在哪：元素的范围、内容区、变换、透明度、圆角（内容区的圆角是元素的圆角减去内边距）、边框
pub(crate) fn image_area(tree: &crate::tree::Tree, id: crate::tree::NodeId, a: &crate::tree::ContentArea) -> ImageArea {
    let paint = &tree.nodes[id].paint;
    let inset = (a.content.x - a.bounds.x).min(a.content.y - a.bounds.y);
    ImageArea {
        bounds: a.bounds,
        content: a.content,
        local: a.local,
        opacity: a.opacity,
        radius: paint.radius,
        inner_radius: (paint.radius - inset).max(0.0),
        border: (paint.border_width > 0.0 && !paint.border_color.is_transparent()).then(|| (paint.border_width, paint.border_fill())),
    }
}
