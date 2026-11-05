//! 元素树里的效果：滤镜、混合模式、遮罩、缓存成纹理。绘制遍历里有效果的元素走这里的冷路径
//! （进入时 [`Tree::enter_effects`]，离开时决定要不要离屏），还有滤镜的过渡、遮罩元素的挂载。
//! 对外的链式方法在 [`crate::effects`]。
//!
//! 绘制遍历的循环里只多判断一次"有没有效果"，其余都在这里，是冷的、不内联的函数：平的界面不为效果付出什么
//! （循环里多一点代码，一万个元素的重画就会因为寄存器和代码布局慢上 10% ~ 30%，量过）。

use animation::{CurveTrack, Track};
use scene::{DisplayList, Group, GroupMask, Quad};

use super::{Mix, NodeId, Paint, Step, Tree, Walk, animations_disabled, layer_key};
use crate::property::{PropId, PropertyValue};
use crate::compositor::LayerInfo;
use crate::visual_effects::{Filter, FilterMix, PaintEffects};

/// 离开一个有效果（滤镜、混合模式、遮罩、缓存成纹理）的元素。`mask`：遮罩从哪个图元、哪个合成层开始
/// （画到遮罩时填上，见 [`EffectsStep::MaskStart`]）
#[derive(Clone, Copy)]
pub(in crate::tree) struct EffectsExit {
    pub id: NodeId,
    pub layer: u32,
    pub first: u32,
    pub clip: u32,
    pub mask: Option<[u32; 2]>,
}

/// 绘制遍历里有效果的元素的一步
#[derive(Clone, Copy)]
pub(in crate::tree) enum EffectsStep {
    /// 离开这个元素（[`EffectsExit`] 在遍历的表里的下标：步骤不因它变大）
    Exit(u32),
    /// 画完内容，接着画遮罩：在下面的 [`EffectsStep::Exit`] 里记下遮罩从哪个图元、哪一层开始
    MaskStart,
}

impl Paint {
    /// 写入滤镜，返回变了没有（不常用，不放进 `Paint::set` 里撑大它）
    #[cold]
    #[inline(never)]
    pub(super) fn set_filter(&mut self, filter: Filter) -> bool {
        match &mut self.effects {
            Some(effects) if effects.filter == filter => false,
            Some(effects) => {
                effects.filter = filter;
                true
            }
            None if filter.is_none() => false,
            None => {
                self.effects = Some(Box::new(PaintEffects { filter, ..Default::default() }));
                true
            }
        }
    }
}

impl Tree {
    /// 进入一个有效果的元素（绘制遍历里，是分组候选的时候）：离开时决定要不要离屏；有遮罩时，
    /// 遮罩元素排在子元素之后画。`frame` 是子元素的上下文（遮罩也用它）
    #[cold]
    #[inline(never)]
    pub(super) fn enter_effects(&self, frame: u32, exit: EffectsExit, w: &mut Walk) {
        w.exits.push(exit);
        w.stack.push(Step::Effects(EffectsStep::Exit(w.exits.len() as u32 - 1)));
        if let Some(mask) = self.nodes[exit.id].mask() {
            w.stack.push(Step::Enter(mask, frame));
            w.stack.push(Step::Effects(EffectsStep::MaskStart));
        }
    }

    /// 绘制遍历里有效果的元素的一步（不撑大遍历的循环）
    #[cold]
    #[inline(never)]
    pub(super) fn effects_step(&self, step: EffectsStep, w: &mut Walk) {
        match step {
            EffectsStep::Exit(index) => self.exit_effects(w.exits[index as usize], &mut w.list, &mut w.layers, &mut w.folded),
            // 遮罩从这里开始：此刻的图元、合成层的个数记进栈里等着的离开那一步（它在遮罩元素的下面）
            EffectsStep::MaskStart => {
                let len = w.stack.len();
                if let Some(Step::Effects(EffectsStep::Exit(index))) = len.checked_sub(2).and_then(|i| w.stack.get(i)) {
                    w.exits[*index as usize].mask = Some([w.list.quads.len() as u32, w.layers.len() as u32 + 1]);
                }
            }
        }
    }

    /// 离开有效果的元素：子树里不止一个图元（或者有模糊、遮罩、混合模式、缓存成纹理）时记成分组；
    /// 只有一个图元、只要滤镜时直接改那个图元
    fn exit_effects(&self, exit: EffectsExit, list: &mut DisplayList, layers: &mut [LayerInfo], folded: &mut Vec<u32>) {
        let EffectsExit { id, layer, first, clip, mask } = exit;
        let end = list.quads.len() as u32;
        let paint = &self.nodes[id].paint;
        let Some(effects) = paint.effects.as_deref() else { return };
        let blur = effects.blur(paint.blur);
        let mask = mask.filter(|_| effects.mask.is_some()).map(|[quads, layers]| GroupMask { quads: quads..end, first_layer: layers, luminance: effects.luminance });
        let filter = effects.filter.color_matrix();
        let forced = effects.forces_group();
        // 只有一个图元、只要滤镜：直接在这个图元上算，不离屏
        if !forced && blur == 0.0 && end - first == 1 && filter.is_some_and(|m| fold_filter(list, first as usize, &m)) {
            // 这个图元生成之后改过颜色：不能再按"节点 → 图元"的表原地修补
            folded.push(first);
            return;
        }
        if blur > 0.0 || end - first >= 2 || (end > first && (forced || filter.is_some())) {
            let filter = filter.map_or(0, |m| list.push_filter(&m));
            list.groups.push(Group {
                quads: first..end,
                layers: layer..layers.len() as u32 + 1,
                blur,
                clip,
                key: layer_key(id),
                filter,
                blend: effects.blend,
                mask,
                offscreen: effects.cache,
            });
            layers[layer as usize - 1].group = true;
        }
    }

    /// 滤镜的新值：从此刻的样子按进度混合到新值（初始值、`without_animation` 里的直接到位）
    pub(super) fn animate_filter(&mut self, id: NodeId, index: usize, target: Filter) {
        let PropertyValue::Filter(current) = self.current_value(id, PropId::FILTER) else { return };
        let now = self.anim.now();
        let slot = &mut self.nodes[id].animations[index];
        let tolerance = PropId::FILTER.tolerance();
        if slot.track.is_none() || animations_disabled() {
            slot.track = Some(CurveTrack::new(slot.curve, &[1.0], tolerance));
            slot.mix = Some(Box::new(Mix::Filter(FilterMix { from: target.clone(), to: target.clone() })));
            self.write(id, PropId::FILTER, PropertyValue::Filter(target));
            return;
        }
        if matches!(slot.mix.as_deref(), Some(Mix::Filter(mix)) if mix.to == target) {
            return;
        }
        let mut track = CurveTrack::new(slot.curve, &[0.0], tolerance);
        track.retarget(now, &[1.0]);
        slot.track = Some(track);
        slot.mix = Some(Box::new(Mix::Filter(FilterMix { from: current, to: target })));
        if !slot.queued {
            slot.queued = true;
            self.anim.queue(id, index);
        }
    }

    /// 改元素的效果（混合模式、缓存成纹理）
    pub(crate) fn update_effects(&mut self, id: NodeId, f: impl FnOnce(&mut PaintEffects)) {
        self.update_paint(id, |p| f(p.effects.get_or_insert_with(Default::default)));
    }

    /// 元素的遮罩元素
    pub(crate) fn mask_of(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id)?.mask()
    }

    /// 卸下遮罩元素（绑定的遮罩换了）
    pub(crate) fn detach_mask(&mut self, id: NodeId, mask: NodeId) {
        self.update_effects(id, |e| e.mask = None);
        self.unmount(mask);
        self.rebuild_children(id);
        self.layout.restyle();
    }

    /// 遮罩元素挂在 `id` 下面：参与布局（铺满它），但不在子元素里（不排列、不命中、不进无障碍树），
    /// 绘制时画在分组的末尾、只画进遮罩纹理
    pub(crate) fn attach_mask(&mut self, id: NodeId, mask: NodeId, luminance: bool) {
        self.nodes[mask].parent = Some(id);
        self.update_effects(id, |e| {
            e.mask = Some(mask);
            e.luminance = luminance;
        });
        self.rebuild_children(id);
        self.layout.restyle();
    }

}

/// 只有一个图元的元素带滤镜：直接改这个图元，不离屏。纯色的形状、文字、路径、单独的投影和内阴影把颜色换成滤镜后的颜色
/// （和离屏后再算一样），图片、彩色字形在着色器里逐像素算（`gradient` 是颜色矩阵）。其他图元（渐变、渐变边框、着色器、
/// 背景模糊）返回 false，照常离屏
fn fold_filter(list: &mut DisplayList, index: usize, matrix: &scene::ColorMatrix) -> bool {
    let quad = list.quads[index];
    match quad.kind {
        Quad::SHAPE | Quad::MASK_GLYPH | Quad::PATH | Quad::SHADOW | Quad::INSET_SHADOW
            if quad.gradient == 0 && quad.border_gradient() == 0 =>
        {
            let q = &mut list.quads[index];
            q.fill = matrix.apply(q.fill);
            q.border_color = matrix.apply(q.border_color);
            q.shadow_color = matrix.apply(q.shadow_color);
            true
        }
        // 已经有颜色矩阵（`.tint`）的图片：离屏后再算滤镜
        Quad::IMAGE | Quad::IMAGE_TEXTURE | Quad::COLOR_GLYPH if quad.gradient == 0 => {
            list.quads[index].gradient = list.push_filter(matrix);
            true
        }
        _ => false,
    }
}
