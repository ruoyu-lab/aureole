//! 矢量路径（[`crate::path`]）是一种元素种类：没指定大小时用画布大小，用继承的文字颜色栅格化进路径图集。

use crate::kinds::{AvailableSpace, ElementKind, KindInfo, MeasureContext, PaintContext};
use crate::property::registry::KindMask;
use crate::vector::PathBlock;

impl ElementKind for PathBlock {
    fn name(&self) -> &'static str {
        "path"
    }

    fn kind_mask(&self) -> KindMask {
        KindMask::PATH
    }

    /// 没指定大小时用画布大小；只指定了一边时按宽高比
    fn measure(&mut self, _: &mut MeasureContext, known: [Option<f32>; 2], _: [AvailableSpace; 2]) -> [f32; 2] {
        super::keep_aspect(known, self.intrinsic())
    }

    fn paint(&mut self, cx: &mut PaintContext) {
        let (area, ink) = (cx.area, cx.ink);
        let trim = cx.tree.nodes[cx.id].paint.trim;
        cx.tree.text.raster().push(self, area.content, &area.local, ink, trim, &mut cx.list.quads);
    }

    fn describe(&self, info: &mut KindInfo) {
        if let Some(stroke) = self.stroke {
            info.fields.push(("stroke", stroke.width.to_string()));
        }
        if let Some(morph) = &self.morph {
            info.fields.push(("morph", morph.progress.to_string()));
        }
    }
}
