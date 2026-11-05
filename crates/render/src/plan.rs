//! 规划一帧：只看显示列表的元数据、此刻的层变换和分组缓存的状态，决定这一帧怎么画。
//!
//! - 哪些分组画进离屏纹理（多大、在屏幕上哪里），哪些直接用缓存的纹理、只重画内容或遮罩、整个重画；
//! - 每个目标（离屏分组的内容和遮罩、根）按什么顺序画哪几段图元，在哪里合成离屏分组、在哪里画特效图元；
//! - 哪些步骤要断开渲染通道（读后面已经画好的内容），根要不要先画进中间纹理；
//! - 每段用哪种管线变体（着色器特性）：按绘制区间选，只有用到特性的那几段图元走带特性的管线，
//!   中间夹着的普通图元太少时并进去（少换一次管线、少一次绘制）。
//!
//! [`plan`] 是纯函数，不碰 GPU，能直接单元测试；编码器（`encoder.rs`）照着 [`RenderPlan`] 写命令。
//! 换显示列表时先把列表整理成 [`ListInfo`]（分组的父子关系、内容签名、着色器特性的区间等，也是纯数据），每帧规划只读它。

use std::ops::Range;
use std::sync::Arc;

use scene::{FeatureRange, ShaderFeatures};

use crate::cache::{CacheState, Decision, PxRect, Signature};
use crate::pass::{QuadEffectPass, PassFeatures, PassSet, quad_effect_pass};
use crate::{DisplayList, Group, LayerTransform, Quad, QuadEffect};

/// 隔开两段带特性的图元的普通图元少于这么多个时，把它们并成一段带特性的绘制：少换一次管线、少一次绘制，
/// 代价是这几个普通图元也走带特性的管线。一段绘制开头、结尾不足这么多个的普通图元同样并进来
pub(crate) const MERGE_GAP: u32 = 16;

/// 管线变体怎么选
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Variants {
    /// 按绘制区间：只有用到特性的那几段走带特性的管线
    #[default]
    PerRange,
    /// 整个显示列表一种：用到的特性的并，所有图元都走它（按区间选之前的做法，对照测试用）
    #[cfg_attr(not(test), allow(dead_code))]
    WholeList,
}

/// 一个分组用到的效果
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupInfo {
    /// 用到哪几种效果（`pass.rs` 里登记的）
    pub passes: PassSet,
    /// 这些效果合在一起要的
    pub features: PassFeatures,
    /// 组里有渐变：合成到 8 位目标时要抖动
    pub smooth: bool,
    /// 合成时裁剪链要的着色器特性（合成图元只用得到透视的裁剪）
    pub clip_features: ShaderFeatures,
}

/// 一个特效图元
pub(crate) struct QuadEffectInfo {
    /// 图元的副本：背景模糊每帧要填上采样区域再画
    pub quad: Quad,
    /// 画它的通道
    pub pass: &'static dyn QuadEffectPass,
    /// 通道要的
    pub features: PassFeatures,
    /// 这个图元要的着色器特性
    pub shader_features: ShaderFeatures,
}

/// 换显示列表时从列表整理出来、每帧规划要用的部分
#[derive(Default)]
pub(crate) struct ListInfo {
    /// 图元数
    pub count: u32,
    pub groups: Vec<Group>,
    /// 每个分组用到的效果
    pub group_info: Vec<GroupInfo>,
    /// 每个分组直接包含的分组
    pub children: Vec<Vec<usize>>,
    pub parents: Vec<Option<usize>>,
    /// 不在任何分组里的分组
    pub roots: Vec<usize>,
    pub quad_effects: Vec<QuadEffect>,
    pub effect_info: Vec<QuadEffectInfo>,
    pub shaders: Vec<Arc<str>>,
    pub layer_bounds: Vec<[f32; 4]>,
    /// 每个分组的内容签名，`None` 的不缓存
    pub signatures: Vec<Option<Signature>>,
    /// 要用到着色器特性的图元区间（[`DisplayList::feature_ranges`]）
    pub features: Vec<FeatureRange>,
    /// 整个列表用到的着色器特性
    pub all_features: ShaderFeatures,
}

impl ListInfo {
    /// 整理显示列表：按前序用一个栈找出分组的父子关系，记下哪些分组含渐变、用到哪些效果，
    /// 有带 `key` 的分组时算内容签名，算出要用着色器特性的图元区间
    pub fn new(list: &DisplayList) -> Self {
        let quads = &list.quads;
        let groups = list.groups.clone();
        let mut children = vec![Vec::new(); groups.len()];
        let mut parents = Vec::with_capacity(groups.len());
        let mut roots = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        for (i, group) in groups.iter().enumerate() {
            while stack.last().is_some_and(|&top| groups[top].quads.end <= group.quads.start && groups[top].quads != group.quads) {
                stack.pop();
            }
            let parent = stack.last().copied();
            match parent {
                Some(p) => children[p].push(i),
                None => roots.push(i),
            }
            parents.push(parent);
            stack.push(i);
        }
        let has_gradient = |g: &Group| quads[g.quads.start as usize..g.quads.end as usize].iter().any(|q| q.gradient != 0 || q.border_gradient() != 0);
        let group_info = groups
            .iter()
            .map(|g| {
                let (passes, features) = PassSet::of(g);
                let smooth = !list.gradients.is_empty() && has_gradient(g);
                GroupInfo { passes, features, smooth, clip_features: list.clip_features(g.clip) }
            })
            .collect();
        let features = list.feature_ranges();
        let effect_info = list
            .quad_effects
            .iter()
            .map(|e| {
                let pass = quad_effect_pass(e);
                let shader_features = features_at(&features, e.quad());
                QuadEffectInfo { quad: quads[e.quad() as usize], pass, features: pass.features(e), shader_features }
            })
            .collect();
        let signatures = if groups.iter().any(|g| g.key != 0) { crate::cache::signatures(list) } else { vec![None; groups.len()] };
        // 整页选管线：扩展范围只看列表的标记，透视的裁剪看列表里有没有这种裁剪
        let mut all_features = features.iter().fold(ShaderFeatures::NONE, |all, range| all | range.features);
        if list.extended {
            all_features |= ShaderFeatures::EXTENDED_COLORS;
        }
        if list.clips.iter().any(|c| c.layer & crate::Clip::PROJECTIVE != 0) {
            all_features |= ShaderFeatures::PROJECTIVE_CLIPS;
        }
        Self {
            count: quads.len() as u32,
            groups,
            group_info,
            children,
            parents,
            roots,
            quad_effects: list.quad_effects.clone(),
            effect_info,
            shaders: list.shaders.clone(),
            layer_bounds: list.layer_bounds.clone(),
            signatures,
            features,
            all_features,
        }
    }

    /// 只有 `count` 个普通图元（没有裁剪、分组、特效）
    pub fn plain(count: u32) -> Self {
        Self { count, ..Default::default() }
    }

    /// 分组的内容、遮罩各是哪一段图元（遮罩在最后）
    fn parts(&self, group: usize) -> (Range<u32>, Range<u32>) {
        let g = &self.groups[group];
        match &g.mask {
            Some(mask) => (g.quads.start..mask.quads.start, mask.quads.clone()),
            None => (g.quads.clone(), g.quads.end..g.quads.end),
        }
    }
}

/// 这一帧的参数：规划时要知道的目标和合成层
pub(crate) struct FrameInfo<'a> {
    /// 合成层此刻的变换（至少有根层）
    pub layers: &'a [LayerTransform],
    /// 目标的大小，物理像素
    pub width: u32,
    pub height: u32,
    /// 每逻辑像素对应的物理像素数
    pub scale_factor: f32,
    /// 分组缓存的预算，字节；0 表示不缓存
    pub budget: u64,
    /// 目标能拷贝：背景模糊、混合模式可以直接从里面拷出背后的内容，不用先画进中间纹理
    pub copyable: bool,
    /// 窗口透明、目标是 sRGB 编码的：根先画进中间纹理，最后换算成系统合成要的样子（见 `transparent.rs`）
    pub transparent: bool,
    /// 离屏纹理的边长上限
    pub max_texture: u32,
    /// 管线变体怎么选
    pub variants: Variants,
}

impl FrameInfo<'_> {
    fn layer(&self, index: u32) -> LayerTransform {
        self.layers.get(index as usize).copied().unwrap_or(LayerTransform::IDENTITY)
    }
}

/// 画一个目标的一步
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    /// 主缓冲里的一段普通图元，用带 `features` 的管线
    Draw { quads: Range<u32>, features: ShaderFeatures },
    /// 合成一个离屏分组（下标同 [`ListInfo::groups`]）
    Composite { group: usize, features: ShaderFeatures },
    /// 一个特效图元（下标同 [`ListInfo::quad_effects`]）
    QuadEffect { index: usize, features: ShaderFeatures },
}

/// 一个目标要画的步骤
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Steps {
    pub steps: Vec<Step>,
    /// 其中要断开渲染通道（读后面已经画好的内容）的步骤数
    pub breaks: u32,
}

/// 一个分组这一帧怎么画
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupPlan {
    /// 离屏纹理在屏幕上的范围，物理像素；`None` 表示看不见
    pub rect: Option<PxRect>,
    /// 范围被屏幕（或纹理边长上限）截掉了一部分
    pub partial: bool,
    pub decision: Decision,
    /// 要重画的内容、遮罩；`None` 表示不画（沿用缓存、不离屏、看不见）
    pub content: Option<Steps>,
    pub mask: Option<Steps>,
    /// 规划时用到了缓存里这个分组的记录：编码时记下它这一帧用过
    pub touch: bool,
}

/// 一帧的计划
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RenderPlan {
    /// 每个分组怎么画（下标同 [`ListInfo::groups`]）。前序排列里子分组在父分组后面，编码时倒着画（由内向外）
    pub groups: Vec<GroupPlan>,
    /// 根目标的步骤
    pub root: Steps,
    /// 根要先画进中间纹理再贴上去：要读后面的内容，目标又不能拷贝
    pub root_copy: bool,
    /// 把中间纹理贴回根的那一步用的着色器特性
    pub blit: ShaderFeatures,
    /// 贴回根时换算成透明窗口要的样子（这时一定有中间纹理）
    pub transparent: bool,
    /// 这一帧要画的目标数（每个一份全局参数）
    pub targets: usize,
    /// 额外图元数的上限（合成、特效、贴回根）
    pub extras: usize,
    /// 合成分组的次数（每次加一个层）
    pub composites: usize,
    /// 直接用缓存的分组数，其中近似用的
    pub hits: u32,
    pub approximate: u32,
}

/// 规划一帧（纯函数）
pub(crate) fn plan(list: &ListInfo, frame: &FrameInfo, cache: &CacheState) -> RenderPlan {
    let n = list.groups.len();
    let screen = PxRect { x: 0, y: 0, w: frame.width, h: frame.height };

    // 1. 每个分组在屏幕上的范围，要不要离屏（透明度为 1、没有效果的分组直接画在原处）
    let mut rects: Vec<Option<PxRect>> = Vec::with_capacity(n);
    let mut partial = Vec::with_capacity(n);
    let mut offscreen = Vec::with_capacity(n);
    for (i, group) in list.groups.iter().enumerate() {
        let wanted = list.group_info[i].features;
        let opacity = frame.layer(group.layers.start).group_opacity;
        let parent_visible = list.parents[i].is_none_or(|p| rects[p].is_some());
        let full = (parent_visible && opacity > 0.0).then(|| group_rect(list, group, wanted.margin, frame)).flatten();
        let rect = full.and_then(|r| r.intersect(screen.expand((wanted.margin * frame.scale_factor).ceil() as i32))).map(|r| r.limit(frame.max_texture));
        partial.push(rect != full);
        rects.push(rect);
        // 半透明、要求缓存成纹理，或者有要离屏的效果（模糊、滤镜、遮罩、混合模式）；透明度为 1、没有效果的直接画在原处
        offscreen.push(opacity < 0.999 || group.offscreen || wanted.offscreen);
    }

    // 2. 每个离屏分组这一帧怎么画：用缓存、重画（全部或一部分）。用了缓存的分组里面的分组不用画
    let mut decisions: Vec<Decision> = Vec::with_capacity(n);
    let mut touches = Vec::with_capacity(n);
    for i in 0..n {
        let covered = list.parents[i].is_some_and(|p| matches!(decisions[p], Decision::Hit { .. } | Decision::Covered));
        let (decision, touch) = match (rects[i], &list.signatures[i]) {
            _ if covered => (Decision::Covered, false),
            (Some(_), _) if !offscreen[i] => (Decision::Inline, false),
            (None, _) => (Decision::Inline, false),
            (Some(rect), Some(signature)) if frame.budget > 0 => {
                let decision = cache.decide(&list.groups[i], signature, frame.layers, rect, partial[i], frame.scale_factor);
                (decision, cache.contains(signature.key))
            }
            (Some(_), _) => (Decision::Fresh, false),
        };
        decisions.push(decision);
        touches.push(touch);
    }

    // 3. 每个目标要画的步骤：内容、遮罩、根
    let planner = Planner { list, rects: &rects, offscreen: &offscreen, variants: frame.variants };
    let mut plan = RenderPlan::default();
    for i in 0..n {
        let has_mask = list.group_info[i].features.mask;
        let (content, mask) = match decisions[i] {
            Decision::Fresh => (true, has_mask),
            Decision::Render { content, mask } => (content, mask),
            _ => (false, false),
        };
        let (content_range, mask_range) = list.parts(i);
        let split = list.children[i].partition_point(|&c| list.groups[c].quads.start < content_range.end);
        let (inner, masks) = list.children[i].split_at(split);
        if let Decision::Hit { approximate, .. } = decisions[i] {
            plan.hits += 1;
            plan.approximate += u32::from(approximate);
        }
        plan.groups.push(GroupPlan {
            rect: rects[i],
            partial: partial[i],
            decision: decisions[i],
            content: content.then(|| planner.steps(content_range, inner)),
            mask: mask.then(|| planner.steps(mask_range, masks)),
            touch: touches[i],
        });
    }
    plan.root = planner.steps(0..list.count, &list.roots);
    // 背景模糊、混合模式要从目标里拷出已经画好的内容；目标不能拷贝时先画到中间纹理
    plan.root_copy = plan.root.breaks > 0 && !frame.copyable || frame.transparent;
    plan.transparent = frame.transparent;
    // 贴回去的图元在屏幕坐标里、不裁剪，用不到任何特性
    plan.blit = planner.pick(ShaderFeatures::NONE);

    // 4. 这一帧要的缓冲
    let lists = || plan.groups.iter().flat_map(|g| g.content.iter().chain(&g.mask)).chain([&plan.root]);
    plan.targets = 1 + plan.groups.iter().flat_map(|g| g.content.iter().chain(&g.mask)).count() + usize::from(plan.root_copy);
    plan.extras = lists().flat_map(|s| &s.steps).filter(|step| !matches!(step, Step::Draw { .. })).count() + 1;
    plan.composites = lists().flat_map(|s| &s.steps).filter(|step| matches!(step, Step::Composite { .. })).count();
    plan
}

/// 这一步要断开渲染通道、拷出后面已经画好的内容（背景模糊、混合模式的分组）
pub(crate) fn breaks_pass(list: &ListInfo, step: &Step) -> bool {
    match *step {
        Step::QuadEffect { index, .. } => list.effect_info[index].features.backdrop,
        Step::Composite { group, .. } => list.group_info[group].features.backdrop,
        Step::Draw { .. } => false,
    }
}

/// 分组在屏幕上的范围：组内各层的图元范围经过此刻的层变换，再外扩 `margin`（逻辑像素，模糊的光晕）
fn group_rect(list: &ListInfo, group: &Group, margin: f32, frame: &FrameInfo) -> Option<PxRect> {
    let mut bounds = crate::EMPTY_BOUNDS;
    for layer in group.layers.clone() {
        let Some(&b) = list.layer_bounds.get(layer as usize) else { continue };
        if b[0] > b[2] {
            continue;
        }
        bounds = union(bounds, transform_bounds(&frame.layer(layer), b));
    }
    PxRect::from_bounds([bounds[0] - margin, bounds[1] - margin, bounds[2] + margin, bounds[3] + margin], frame.scale_factor)
}

/// 层坐标里的矩形经过层的变换后在屏幕上的外接矩形。有透视时矩形的像是凸四边形，四个角的外接矩形就是它的；
/// 有角落在视点后面时（投到无穷远）当作铺满屏幕，交给调用方和目标求交
pub(crate) fn transform_bounds(layer: &LayerTransform, b: [f32; 4]) -> [f32; 4] {
    const FAR: f32 = 1e6;
    let mut out = crate::EMPTY_BOUNDS;
    for corner in [[b[0], b[1]], [b[2], b[1]], [b[0], b[3]], [b[2], b[3]]] {
        let Some([x, y]) = layer.project(corner) else { return [-FAR, -FAR, FAR, FAR] };
        out = [out[0].min(x), out[1].min(y), out[2].max(x), out[3].max(y)];
    }
    out.map(|v| v.clamp(-FAR, FAR))
}

fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// 图元 `quad` 要的着色器特性（`ranges` 按图元下标排列）
fn features_at(ranges: &[FeatureRange], quad: u32) -> ShaderFeatures {
    let i = ranges.partition_point(|r| r.quads.end <= quad);
    ranges.get(i).filter(|r| r.quads.start <= quad).map_or(ShaderFeatures::NONE, |r| r.features)
}

/// 排一个目标的步骤
struct Planner<'a> {
    list: &'a ListInfo,
    rects: &'a [Option<PxRect>],
    offscreen: &'a [bool],
    variants: Variants,
}

impl Planner<'_> {
    /// 一步要的着色器特性 `needed` 实际用哪种变体：按区间选时就是它，整页选时是整个列表的并
    fn pick(&self, needed: ShaderFeatures) -> ShaderFeatures {
        match self.variants {
            Variants::PerRange => needed,
            Variants::WholeList => self.list.all_features,
        }
    }

    /// 一段图元（和其中直接包含的分组）按顺序要做的步骤
    fn steps(&self, range: Range<u32>, children: &[usize]) -> Steps {
        let mut steps = Steps::default();
        self.collect(range, children, &mut steps);
        steps
    }

    fn push(&self, steps: &mut Steps, step: Step) {
        steps.breaks += u32::from(breaks_pass(self.list, &step));
        steps.steps.push(step);
    }

    /// 普通图元合并成一次绘制，遇到离屏分组和特效断开；不离屏的分组展开画在原处
    fn collect(&self, range: Range<u32>, children: &[usize], steps: &mut Steps) {
        let mut cursor = range.start;
        for &g in children {
            let group = &self.list.groups[g];
            self.collect_plain(cursor..group.quads.start, steps);
            if self.rects[g].is_some() {
                if self.offscreen[g] {
                    let features = self.pick(self.list.group_info[g].clip_features);
                    self.push(steps, Step::Composite { group: g, features });
                } else {
                    self.collect(group.quads.clone(), &self.list.children[g], steps);
                }
            }
            cursor = group.quads.end;
        }
        self.collect_plain(cursor..range.end, steps);
    }

    /// 一段不含分组的图元：遇到特效图元断开
    fn collect_plain(&self, range: Range<u32>, steps: &mut Steps) {
        if range.is_empty() {
            return;
        }
        let effects = &self.list.quad_effects;
        let first = effects.partition_point(|e| e.quad() < range.start);
        let mut cursor = range.start;
        for (i, effect) in effects.iter().enumerate().skip(first) {
            let quad = effect.quad();
            if quad >= range.end {
                break;
            }
            self.draw(cursor..quad, steps);
            let features = self.pick(self.list.effect_info[i].shader_features);
            self.push(steps, Step::QuadEffect { index: i, features });
            cursor = quad + 1;
        }
        self.draw(cursor..range.end, steps);
    }

    /// 一段普通图元：按要的着色器特性切成几次绘制。带特性的几段之间、和这一段的开头结尾之间夹着的普通图元
    /// 不足 [`MERGE_GAP`] 个时并进带特性的那次绘制
    fn draw(&self, range: Range<u32>, steps: &mut Steps) {
        if range.is_empty() {
            return;
        }
        if self.variants == Variants::WholeList {
            self.push_draw(steps, range, self.list.all_features);
            return;
        }
        let ranges = &self.list.features;
        let first = ranges.partition_point(|r| r.quads.end <= range.start);
        // 已经排进步骤的图元到这里为止；正在攒的一段带特性的绘制
        let mut cursor = range.start;
        let mut pending: Option<(Range<u32>, ShaderFeatures)> = None;
        for r in &ranges[first..] {
            if r.quads.start >= range.end {
                break;
            }
            let quads = r.quads.start.max(range.start)..r.quads.end.min(range.end);
            pending = match pending {
                Some((p, features)) if quads.start - p.end < MERGE_GAP => Some((p.start..quads.end, features | r.features)),
                Some(p) => {
                    cursor = self.flush(steps, cursor, p, None);
                    Some((quads, r.features))
                }
                None => Some((quads, r.features)),
            };
        }
        if let Some(p) = pending {
            cursor = self.flush(steps, cursor, p, Some(range.end));
        }
        self.push_draw(steps, cursor..range.end, ShaderFeatures::NONE);
    }

    /// 排出攒好的一段带特性的绘制和它前面的普通图元，返回排到了哪里。前面的普通图元太少时并进来；
    /// `end` 是这一段普通图元的结尾（最后一段时给出），后面剩下的太少时也并进来
    fn flush(&self, steps: &mut Steps, cursor: u32, (quads, features): (Range<u32>, ShaderFeatures), end: Option<u32>) -> u32 {
        let start = if quads.start - cursor < MERGE_GAP { cursor } else { quads.start };
        let stop = match end {
            Some(end) if end - quads.end < MERGE_GAP => end,
            _ => quads.end,
        };
        self.push_draw(steps, cursor..start, ShaderFeatures::NONE);
        self.push_draw(steps, start..stop, features);
        stop
    }

    /// 一次绘制；紧接着上一次、用同样的管线时并进上一次
    fn push_draw(&self, steps: &mut Steps, quads: Range<u32>, features: ShaderFeatures) {
        if quads.is_empty() {
            return;
        }
        if let Some(Step::Draw { quads: last, features: same }) = steps.steps.last_mut()
            && *same == features
            && last.end == quads.start
        {
            last.end = quads.end;
            return;
        }
        steps.steps.push(Step::Draw { quads, features });
    }
}
