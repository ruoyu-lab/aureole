//! 编码器：照着 [`RenderPlan`] 写一帧的命令。
//!
//! 它不做决定：哪些分组离屏、用不用缓存、每个目标画哪几段、每段用哪种管线变体都在计划里。
//! 编码器只按顺序开渲染通道、选管线、画图元、拷出后面的内容、模糊、合成，把这一帧的全局参数、额外图元、
//! 合成用的层、模糊参数攒在 [`Frame`] 里，提交前统一写入缓冲区。

use std::ops::Range;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use scene::ShaderFeatures;

use crate::cache::{Cache, Decision, PxRect};
use crate::pass::CompositeInput;
use crate::pipeline::Entry;
use crate::plan::{self, ListInfo, RenderPlan, Step, Steps};
use crate::pool::Target;
use crate::renderer::Resources;
use crate::{Color, LayerTransform, Quad, RenderStats};

/// 离屏分组、模糊用的格式：半精度浮点，存线性预乘的颜色，多次合成不掉色阶
pub(crate) const OFFSCREEN: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// 一致缓冲里每段数据的间隔（动态偏移要求 256 字节对齐）
pub(crate) const UNIFORM_STRIDE: usize = 256;
/// 每帧最多的模糊通道数；超出的模糊这一帧跳过
pub(crate) const MAX_BLUR_PASSES: usize = 512;
/// 模糊前逐级缩小，直到标准差不超过这么多像素
const BLUR_SIGMA_LIMIT: f32 = 3.0;
const MAX_BLUR_LEVELS: u32 = 6;

/// 图元种类：把分组的离屏纹理合成上屏（`rect` 是屏幕坐标，不经过层变换）。只在渲染器内部用，
/// 和 [`Quad::SHAPE`] 这些公开的种类编在一起（[`Quad`] 给它留着 3），着色器里同样按 3 判断
const GROUP_KIND: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct Globals {
    pub viewport: [f32; 2],
    pub origin: [f32; 2],
    pub scale: f32,
    pub time: f32,
    /// 1：8 位 sRGB 目标，渐变和合成要抖动；0：浮点的离屏纹理，不需要
    pub dither: f32,
    /// 这一帧最终的目标能显示到多亮（SDR 白的倍数）：8 位目标是 1，扩展范围的表面是显示器此刻的余量。
    /// 扩展范围的颜色和图片按它放进去（离屏的分组用这一帧最终目标的值）
    pub headroom: f32,
    /// 1：这一帧最终的目标是扩展范围的（可以有负的分量，广色域）；0：8 位 sRGB，负的分量截掉
    pub extended: f32,
    pub _pad: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct BlurParams {
    pub src_extent: [f32; 2],
    pub src_texel: [f32; 2],
    pub direction: [f32; 2],
    pub sigma: f32,
    pub mode: u32,
}

/// 要画进去的一个目标：窗口的表面、离屏纹理
#[derive(Clone)]
pub(crate) struct Surface {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub format: wgpu::TextureFormat,
    /// 左上角在屏幕上的位置，物理像素
    pub origin: [f32; 2],
    pub size: [u32; 2],
}

impl Target {
    fn surface(&self) -> Surface {
        Surface { texture: self.texture.clone(), view: self.view.clone(), format: self.format, origin: self.origin, size: self.used }
    }
}

/// 一个离屏分组这一帧的结果：内容（和遮罩）的纹理、画的时候在屏幕上的范围、贴过去的变换
#[derive(Clone)]
pub(crate) struct Rendered {
    pub content: Target,
    pub mask: Option<Target>,
    pub rect: PxRect,
    /// 缓存的纹理画好以后分组又动了：从当时的屏幕位置到此刻的变换（`None` 表示没动）
    pub warp: Option<LayerTransform>,
    /// 滤镜、遮罩、混合模式合成用的绑定（内容 + 遮罩）
    pub bind: Option<wgpu::BindGroup>,
}

/// 这一帧攒下的、提交前统一写入的数据
pub(crate) struct Frame {
    pub stats: RenderStats,
    /// GPU 计时：第一个渲染通道开始、最后一个结束时写时间戳
    pub timestamps: Option<wgpu::QuerySet>,
    pub first_pass: bool,
    pub globals: Vec<u8>,
    pub extras: Vec<Quad>,
    pub blur: Vec<u8>,
    pub blur_passes: usize,
    pub layers: Vec<LayerTransform>,
    pub scale_factor: f32,
    pub time: f32,
    /// 这一帧最终的目标能显示到多亮、是不是扩展范围的，见 [`Globals`]
    pub headroom: f32,
    pub extended: bool,
}

impl Frame {
    /// 这个目标的全局参数，返回动态偏移
    fn push_globals(&mut self, surface: &Surface) -> u32 {
        let offset = self.globals.len();
        let globals = Globals {
            viewport: [surface.size[0] as f32, surface.size[1] as f32],
            origin: surface.origin,
            scale: self.scale_factor,
            time: self.time,
            dither: if surface.format.is_srgb() { 1.0 } else { 0.0 },
            headroom: self.headroom,
            extended: if self.extended { 1.0 } else { 0.0 },
            _pad: [0.0; 3],
        };
        self.globals.extend_from_slice(bytemuck::bytes_of(&globals));
        self.globals.resize(offset + UNIFORM_STRIDE, 0);
        offset as u32
    }

    fn push_extra(&mut self, quad: Quad) -> u32 {
        self.extras.push(quad);
        self.extras.len() as u32 - 1
    }

    /// 合成分组用的层：分组此刻的透明度，缓存的纹理画好以后分组又动了时带上贴过去的变换
    fn push_layer(&mut self, warp: Option<LayerTransform>, opacity: f32) -> u32 {
        let mut layer = warp.unwrap_or(LayerTransform::IDENTITY);
        layer.opacity = opacity;
        layer.group_opacity = opacity;
        self.layers.push(layer);
        self.layers.len() as u32 - 1
    }

    pub fn layer(&self, index: u32) -> LayerTransform {
        self.layers.get(index as usize).copied().unwrap_or(LayerTransform::IDENTITY)
    }

    /// 这个渲染通道要写的时间戳
    fn timestamps(&mut self, last: bool) -> Option<wgpu::RenderPassTimestampWrites<'_>> {
        self.stats.passes += 1;
        let first = std::mem::take(&mut self.first_pass);
        let query_set = self.timestamps.as_ref()?;
        (first || last).then_some(wgpu::RenderPassTimestampWrites {
            query_set,
            beginning_of_pass_write_index: first.then_some(0),
            end_of_pass_write_index: last.then_some(1),
        })
    }
}

/// 正在画的目标
struct Open {
    surface: Surface,
    /// 全局参数的动态偏移
    globals: u32,
    /// 后面还要断开几次渲染通道：为 0 时开的通道是这个目标的最后一段
    breaks: u32,
    /// 这是这一帧最后画的目标（最后一段写结束的时间戳）
    last: bool,
    /// 开着的渲染通道；断开以后、还没接着画时是 `None`
    pass: Option<wgpu::RenderPass<'static>>,
    /// 通道上此刻的管线（自定义着色器的不记）
    pipeline: Option<(Entry, ShaderFeatures)>,
}

/// 照着计划写命令
pub(crate) struct Encoder<'a> {
    device: &'a wgpu::Device,
    commands: wgpu::CommandEncoder,
    res: &'a mut Resources,
    list: &'a ListInfo,
    frame: Frame,
    open: Option<Open>,
}

impl<'a> Encoder<'a> {
    pub fn new(device: &'a wgpu::Device, commands: wgpu::CommandEncoder, res: &'a mut Resources, list: &'a ListInfo, frame: Frame) -> Self {
        Self { device, commands, res, list, frame, open: None }
    }

    /// 写完了：命令和这一帧攒下的数据
    pub fn finish(mut self) -> (wgpu::CommandEncoder, Frame) {
        self.open = None;
        (self.commands, self.frame)
    }

    /// 由内向外画离屏分组（前序排列里，子分组在父分组后面），画好的记进缓存（`now` 是这一帧的时刻）。返回每个分组的结果
    pub fn groups(&mut self, plan: &RenderPlan, cache: &mut Cache, budget: u64, now: Instant) -> Vec<Option<Rendered>> {
        let list = self.list;
        let scale = self.frame.scale_factor;
        // 规划时用到了缓存的分组：记下这一帧用过、此刻的变换
        for (i, group) in plan.groups.iter().enumerate() {
            if let (true, Some(signature)) = (group.touch, &list.signatures[i]) {
                cache.state.touch(signature.key, self.frame.layer(list.groups[i].layers.start), now);
            }
        }
        let mut results: Vec<Option<Rendered>> = vec![None; plan.groups.len()];
        for (i, planned) in plan.groups.iter().enumerate().rev() {
            match planned.decision {
                Decision::Hit { warp, .. } => {
                    let Some(key) = list.signatures[i].map(|s| s.key) else { continue };
                    let (Some(textures), Some(rect)) = (cache.get(key), cache.state.rect(key)) else { continue };
                    let Some(content) = textures.content.clone() else { continue };
                    results[i] = Some(Rendered { content, mask: textures.mask.clone(), rect, warp, bind: textures.bind.clone() });
                }
                Decision::Fresh | Decision::Render { .. } => {
                    let Some(rect) = planned.rect else { continue };
                    let (group, info) = (&list.groups[i], list.group_info[i]);
                    // 内容画好以后交给分组用到的效果加工（模糊）
                    let content = planned.content.as_ref().map(|steps| {
                        let target = self.draw_offscreen(rect, steps, &results);
                        info.passes.iter().fold(target, |target, pass| pass.encode(self, group, target))
                    });
                    let mask = planned.mask.as_ref().map(|steps| self.draw_offscreen(rect, steps, &results));
                    self.frame.stats.groups_drawn += 1;
                    // 没重画的那部分沿用缓存
                    let signature = list.signatures[i];
                    let cached = signature.and_then(|s| cache.get(s.key));
                    let Some(content_target) = content.clone().or_else(|| cached.and_then(|t| t.content.clone())) else { continue };
                    let mask_target = mask.clone().or_else(|| cached.and_then(|t| t.mask.clone()));
                    let bind = info.features.composite.then(|| self.res.composite_bind(self.device, &content_target, mask_target.as_ref()));
                    if let (Some(signature), Decision::Render { .. }) = (signature, planned.decision) {
                        let layers = &self.frame.layers;
                        cache.store(group, &signature, layers, rect, planned.partial, scale, content, mask, bind.clone(), budget, now, &mut self.res.pool);
                    }
                    results[i] = Some(Rendered { content: content_target, mask: mask_target, rect, warp: None, bind });
                }
                Decision::Inline | Decision::Covered => {}
            }
        }
        results
    }

    /// 画根。目标不能拷贝、又要读后面的内容时，先画进一张中间纹理再贴上去
    pub fn root(&mut self, plan: &RenderPlan, target: &wgpu::Texture, size: [u32; 2], clear: Color, results: &[Option<Rendered>]) {
        let surface = Surface { texture: target.clone(), view: target.create_view(&Default::default()), format: target.format(), origin: [0.0; 2], size };
        if !plan.root_copy {
            self.draw(surface, Some(clear), &plan.root, results, true);
            return;
        }
        let mut whole = self.acquire(surface.format, size[0], size[1]);
        whole.origin = [0.0; 2];
        self.draw(whole.surface(), Some(clear), &plan.root, results, false);
        if plan.transparent {
            self.transparent_output(&whole, &surface);
            return;
        }
        let screen = PxRect { x: 0, y: 0, w: size[0], h: size[1] };
        let blit = composite_quad(&whole, screen, 0, 0, self.frame.scale_factor);
        self.begin(surface, Some(clear), 0, true);
        self.draw_extra(blit, &whole.effect, plan.blit);
        self.open = None;
    }

    /// 窗口透明时最后一步：画好的根换算成系统合成要的样子写进窗口表面（见 `transparent.rs`）
    fn transparent_output(&mut self, whole: &Target, surface: &Surface) {
        let output = self.res.transparent_output(self.device, surface.format);
        let bind = output.bind(self.device, &whole.view);
        let mut pass = self.commands.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("transparent output"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &surface.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: self.frame.timestamps(true),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(output.pipeline());
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
        self.frame.stats.draws += 1;
    }

    /// 按步骤画进一张离屏纹理（清成透明），范围是 `rect`
    fn draw_offscreen(&mut self, rect: PxRect, steps: &Steps, results: &[Option<Rendered>]) -> Target {
        let mut target = self.acquire(OFFSCREEN, rect.w, rect.h);
        target.origin = [rect.x as f32, rect.y as f32];
        self.draw(target.surface(), Some(Color::TRANSPARENT), steps, results, false);
        target
    }

    /// 按步骤画一个目标。`last`：这是这一帧最后画的目标
    fn draw(&mut self, surface: Surface, clear: Option<Color>, steps: &Steps, results: &[Option<Rendered>], last: bool) {
        let list = self.list;
        self.begin(surface, clear, steps.breaks, last);
        for step in &steps.steps {
            if plan::breaks_pass(list, step)
                && let Some(open) = self.open.as_mut()
            {
                open.breaks = open.breaks.saturating_sub(1);
            }
            match *step {
                Step::Draw { ref quads, features } => self.draw_quads(quads.clone(), features),
                Step::Composite { group, features } => self.composite(group, features, results),
                Step::QuadEffect { index, features } => self.quad_effect(index, features),
            }
        }
        self.open = None;
    }

    /// 开始画一个目标：`clear` 是清成的颜色（`None` 保留原来的内容），`breaks` 是要断开渲染通道的步骤数
    fn begin(&mut self, surface: Surface, clear: Option<Color>, breaks: u32, last: bool) {
        let globals = self.frame.push_globals(&surface);
        self.open = Some(Open { surface, globals, breaks, last, pass: None, pipeline: None });
        self.begin_pass(clear);
    }

    /// 在正在画的目标上开一个渲染通道
    fn begin_pass(&mut self, clear: Option<Color>) {
        let Some(open) = self.open.as_mut() else { return };
        let load = match clear {
            Some(color) => {
                let [r, g, b, a] = color.to_linear();
                wgpu::LoadOp::Clear(wgpu::Color { r: r as f64, g: g as f64, b: b as f64, a: a as f64 })
            }
            None => wgpu::LoadOp::Load,
        };
        let last = open.last && open.breaks == 0;
        let mut pass = self
            .commands
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("quads"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &open.surface.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: self.frame.timestamps(last),
                occlusion_query_set: None,
                multiview_mask: None,
            })
            .forget_lifetime();
        pass.set_viewport(0.0, 0.0, open.surface.size[0] as f32, open.surface.size[1] as f32, 0.0, 1.0);
        pass.set_bind_group(0, &self.res.bind_group, &[open.globals]);
        pass.set_bind_group(1, &self.res.no_effect, &[]);
        pass.set_bind_group(2, self.res.images.default_group(), &[]);
        pass.set_vertex_buffer(0, self.res.instances.slice(..));
        open.pass = Some(pass);
        open.pipeline = None;
    }

    /// 断开渲染通道（之后要用命令编码器拷贝、模糊）；下次画的时候接着开一个，不清空
    fn end_pass(&mut self) {
        if let Some(open) = self.open.as_mut() {
            open.pass = None;
        }
    }

    /// 开着的渲染通道（断开了就接着开一个）和资源、这一帧的数据
    fn pass(&mut self) -> Option<(&mut wgpu::RenderPass<'static>, &Resources, &mut Frame)> {
        if self.open.as_ref().is_some_and(|open| open.pass.is_none()) {
            self.begin_pass(None);
        }
        let pass = self.open.as_mut()?.pass.as_mut()?;
        Some((pass, &*self.res, &mut self.frame))
    }

    /// 通道上换成这个入口、这些特性的管线（已经是了就不换）
    fn use_pipeline(&mut self, entry: Entry, features: ShaderFeatures) {
        if self.pass().is_none() {
            return;
        }
        let Some(open) = self.open.as_mut() else { return };
        if open.pipeline == Some((entry, features)) {
            return;
        }
        let pipeline = self.res.pipelines.get(self.device, entry, open.surface.format, features);
        if let Some(pass) = open.pass.as_mut() {
            pass.set_pipeline(pipeline);
        }
        open.pipeline = Some((entry, features));
    }

    /// 主缓冲里的一段图元
    fn draw_quads(&mut self, quads: Range<u32>, features: ShaderFeatures) {
        self.use_pipeline(Entry::Quad, features);
        if let Some((pass, _, frame)) = self.pass() {
            pass.draw(0..4, quads);
            frame.stats.draws += 1;
        }
    }

    /// 额外缓冲里的一个图元，采样 `texture` 画一次，然后恢复默认状态
    pub fn draw_extra(&mut self, quad: Quad, texture: &wgpu::BindGroup, features: ShaderFeatures) {
        let index = self.frame.push_extra(quad);
        self.use_pipeline(Entry::Quad, features);
        let Some((pass, res, frame)) = self.pass() else { return };
        pass.set_bind_group(1, texture, &[]);
        pass.set_vertex_buffer(0, res.extras.slice(..));
        pass.draw(0..4, index..index + 1);
        frame.stats.draws += 1;
        pass.set_bind_group(1, &res.no_effect, &[]);
        pass.set_vertex_buffer(0, res.instances.slice(..));
    }

    /// 带滤镜、遮罩、混合模式的分组合成：合成管线画额外缓冲里的一个图元，再恢复默认状态
    fn draw_composite(&mut self, quad: Quad, bind: &wgpu::BindGroup, backdrop: Option<&wgpu::BindGroup>, features: ShaderFeatures) {
        let index = self.frame.push_extra(quad);
        self.use_pipeline(Entry::Composite, features);
        let Some((pass, res, frame)) = self.pass() else { return };
        pass.set_bind_group(1, bind, &[]);
        pass.set_bind_group(3, backdrop.unwrap_or(&res.no_effect), &[]);
        pass.set_vertex_buffer(0, res.extras.slice(..));
        pass.draw(0..4, index..index + 1);
        frame.stats.draws += 1;
        pass.set_bind_group(1, &res.no_effect, &[]);
        pass.set_vertex_buffer(0, res.instances.slice(..));
    }

    /// 合成一个离屏分组。没有效果的分组按透明度直接贴上去；有效果的换成合成管线，参数由分组用到的各个效果写进合成图元
    fn composite(&mut self, g: usize, features: ShaderFeatures, results: &[Option<Rendered>]) {
        let Some(result) = &results[g] else { return };
        let list = self.list;
        let (group, info) = (&list.groups[g], list.group_info[g]);
        let opacity = self.frame.layer(group.layers.start).group_opacity;
        let layer = self.frame.push_layer(result.warp, opacity);
        let mut quad = composite_quad(&result.content, result.rect, layer, group.clip, self.frame.scale_factor);
        let (Some(bind), true) = (&result.bind, info.features.composite) else {
            // 合成图元不用边框，借 border_width 标记"要抖动"
            quad.border_width = if info.smooth { 1.0 } else { 0.0 };
            self.draw_extra(quad, &result.content.effect, features);
            return;
        };
        // 合成管线的图元：颜色的第三个字节标记"要抖动"，其余的由效果写（见着色器里 fs_composite 的说明）
        quad.fill = Color::rgb(0, 0, u8::from(info.smooth));
        let mut backdrop = None;
        if info.features.backdrop {
            // 断开渲染通道：拷出合成范围后面已经画好的内容，再接着画
            let Some(region) = self.composite_region(&quad) else { return };
            let Some(behind) = self.copy_behind(region) else { return };
            backdrop = Some(behind);
        }
        let input = CompositeInput { mask: result.mask.as_ref(), backdrop: backdrop.as_ref().map(|b| b.origin) };
        for pass in info.passes.iter() {
            pass.composite(group, &input, &mut quad);
        }
        self.draw_composite(quad, bind, backdrop.as_ref().map(|b| &b.effect), features);
    }

    /// 一个特效图元：交给它的通道
    fn quad_effect(&mut self, e: usize, features: ShaderFeatures) {
        let list = self.list;
        let info = &list.effect_info[e];
        info.pass.encode(self, &list.quad_effects[e], &info.quad, features);
    }

    /// 每逻辑像素对应的物理像素数
    pub fn scale_factor(&self) -> f32 {
        self.frame.scale_factor
    }

    /// 单独纹理的大图：换成它自己的纹理（`texture` 是 [`crate::ImageUpdate::Create`] 的编号）画主缓冲里的第 `quad` 个图元，再换回来
    pub fn draw_image(&mut self, quad: u32, texture: u32, features: ShaderFeatures) {
        self.use_pipeline(Entry::Quad, features);
        let Some((pass, res, frame)) = self.pass() else { return };
        if let Some(group) = res.images.texture_group(texture) {
            pass.set_bind_group(2, group, &[]);
            pass.draw(0..4, quad..quad + 1);
            frame.stats.draws += 1;
            pass.set_bind_group(2, res.images.default_group(), &[]);
        }
    }

    /// 自定义着色器（`shader` 是 [`crate::DisplayList::shaders`] 的下标）画主缓冲里的第 `quad` 个图元；
    /// 编译失败的不画
    pub fn draw_custom(&mut self, quad: u32, shader: u32, features: ShaderFeatures) {
        let list = self.list;
        let (Some(source), Some(format)) = (list.shaders.get(shader as usize), self.open.as_ref().map(|o| o.surface.format)) else { return };
        let Some(pipeline) = self.res.pipelines.custom(self.device, source, format, features) else { return };
        let Some((pass, _, frame)) = self.pass() else { return };
        pass.set_pipeline(&pipeline);
        pass.draw(0..4, quad..quad + 1);
        frame.stats.draws += 1;
        // 下一步换回图元管线
        if let Some(open) = self.open.as_mut() {
            open.pipeline = None;
        }
    }

    /// 正在画的目标在屏幕上的范围
    fn target_rect(&self) -> Option<PxRect> {
        let surface = &self.open.as_ref()?.surface;
        Some(PxRect { x: surface.origin[0] as i32, y: surface.origin[1] as i32, w: surface.size[0], h: surface.size[1] })
    }

    /// 混合模式要读取的区域：合成图元此刻在屏幕上的范围，限制在目标之内
    fn composite_region(&self, quad: &Quad) -> Option<PxRect> {
        let [x, y, w, h] = quad.rect;
        let b = plan::transform_bounds(&self.frame.layer(quad.layer), [x, y, x + w, y + h]);
        PxRect::from_bounds(b, self.frame.scale_factor)?.intersect(self.target_rect()?)
    }

    /// 背景模糊要读取的区域：形状在屏幕上的范围，外扩 3 个标准差，限制在目标之内
    pub fn backdrop_region(&self, quad: &Quad, blur: f32) -> Option<PxRect> {
        let b = plan::transform_bounds(&self.frame.layer(quad.layer), quad.bounds());
        let margin = 3.0 * blur;
        PxRect::from_bounds([b[0] - margin, b[1] - margin, b[2] + margin, b[3] + margin], self.frame.scale_factor)?.intersect(self.target_rect()?)
    }

    /// 从正在画的目标里拷出 `region`（屏幕上的物理像素）到一张新纹理。先断开渲染通道；没有在画的目标时是 `None`
    pub fn copy_behind(&mut self, region: PxRect) -> Option<Target> {
        self.end_pass();
        let surface = &self.open.as_ref()?.surface;
        let (texture, format, origin) = (surface.texture.clone(), surface.format, surface.origin);
        let mut behind = self.acquire(format, region.w, region.h);
        behind.origin = [region.x as f32, region.y as f32];
        self.commands.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: (region.x as f32 - origin[0]) as u32, y: (region.y as f32 - origin[1]) as u32, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            behind.texture.as_image_copy(),
            wgpu::Extent3d { width: region.w, height: region.h, depth_or_array_layers: 1 },
        );
        Some(behind)
    }

    fn acquire(&mut self, format: wgpu::TextureFormat, width: u32, height: u32) -> Target {
        self.res.acquire(self.device, format, width, height)
    }

    /// 高斯模糊（标准差 `sigma`，物理像素）：先逐级对半缩小，再横竖各一遍。
    /// 返回的纹理比原来小，合成时按 `extent` 放大回原来的范围
    pub fn blur(&mut self, source: &Target, sigma: f32) -> Target {
        self.end_pass();
        let mut levels = 0;
        let mut level_sigma = sigma;
        while level_sigma > BLUR_SIGMA_LIMIT && levels < MAX_BLUR_LEVELS {
            level_sigma /= 2.0;
            levels += 1;
        }
        if self.frame.blur_passes + levels as usize + 2 > MAX_BLUR_PASSES {
            return source.clone();
        }
        let mut current = source.clone();
        for _ in 0..levels {
            let extent = [(current.extent[0] / 2.0).max(1.0), (current.extent[1] / 2.0).max(1.0)];
            let mut next = self.acquire(OFFSCREEN, extent[0].ceil() as u32, extent[1].ceil() as u32);
            next.extent = extent;
            next.origin = current.origin;
            self.blur_pass(&current, &next, 0, [0.0, 0.0], 0.0);
            current = next;
        }
        for direction in [[1.0, 0.0], [0.0, 1.0]] {
            let mut next = self.acquire(OFFSCREEN, current.used[0], current.used[1]);
            next.extent = current.extent;
            next.origin = current.origin;
            self.blur_pass(&current, &next, 1, direction, level_sigma);
            current = next;
        }
        current
    }

    fn blur_pass(&mut self, source: &Target, target: &Target, mode: u32, direction: [f32; 2], sigma: f32) {
        let offset = self.frame.blur.len();
        let params = BlurParams {
            src_extent: source.uv_extent(),
            src_texel: [1.0 / source.size[0] as f32, 1.0 / source.size[1] as f32],
            direction,
            sigma,
            mode,
        };
        self.frame.blur.extend_from_slice(bytemuck::bytes_of(&params));
        self.frame.blur.resize(offset + UNIFORM_STRIDE, 0);
        self.frame.blur_passes += 1;
        let mut pass = self.commands.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("blur"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: self.frame.timestamps(false),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_viewport(0.0, 0.0, target.extent[0], target.extent[1], 0.0, 1.0);
        pass.set_pipeline(self.res.pipelines.get(self.device, Entry::Blur, OFFSCREEN, ShaderFeatures::NONE));
        pass.set_bind_group(0, &source.blur, &[offset as u32]);
        pass.draw(0..3, 0..1);
        self.frame.stats.draws += 1;
    }
}

/// 把离屏纹理按原来的屏幕范围合成上去的图元
fn composite_quad(texture: &Target, rect: PxRect, layer: u32, clip: u32, scale: f32) -> Quad {
    let mut quad = Quad::new(rect.x as f32 / scale, rect.y as f32 / scale, rect.w as f32 / scale, rect.h as f32 / scale);
    let [u, v] = texture.uv_extent();
    quad.uv = [0.0, 0.0, u, v];
    quad.kind = GROUP_KIND;
    quad.layer = layer;
    quad.clip = clip;
    quad
}
