//! 管线：图元着色器（`fs_main`）、带滤镜 / 遮罩 / 混合模式的分组合成（`fs_composite`）、模糊、自定义着色器（`fs_custom`），
//! 都按目标格式和着色器特性（[`ShaderFeatures`]）分成变体，第一次用到时创建，之后缓存着。
//! 建渲染器时一条管线都不编译：第一帧用到哪条编哪条（一般只有窗口表面格式上什么都不开的那一条），
//! 窗口的渲染器在第一帧画完以后把常用的几种放到后台线程上先编译好（[`Pipelines::precompile`]），
//! 第一次出现投影、分组、模糊的那一帧不用当场编译。
//!
//! 着色器特性是 `quad.wgsl` 里的管线常量（`override`）：一段少用的代码只编进要它的变体，平常的变体不为它多占寄存器。
//! 规划按绘制区间选变体，所以一帧里可能同时用到几种。每加一种特性变体数就翻一倍，一种目标格式上一个入口的变体数
//! 有上限（[`MAX_VARIANTS_PER_FORMAT`]）：新的效果要么做成离屏的通道（`pass.rs`），要么占一个特性位，
//! 不往 `fs_main` 里加常开的分支。

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use scene::ShaderFeatures;

use crate::Quad;
use crate::disk_cache::DiskCache;
use crate::encoder::OFFSCREEN;

pub(crate) const QUAD_SHADER: &str = include_str!("shaders/quad.wgsl");
const BLUR_SHADER: &str = include_str!("shaders/blur.wgsl");
const CUSTOM_SHADER: &str = include_str!("shaders/custom.wgsl");

/// 着色器、管线的编译耗时记在这个目标下（debug 级别）：启动慢、第一次用到某种效果时卡一下，看这里
pub(crate) const COMPILE_LOG: &str = "aureole::pipeline";

/// 一种目标格式上，一个着色器入口（图元、合成、每个自定义着色器）最多的变体数：特性的每种组合一个
pub(crate) const MAX_VARIANTS_PER_FORMAT: usize = 8;
const _: () = assert!(1 << ShaderFeatures::COUNT <= MAX_VARIANTS_PER_FORMAT, "着色器特性太多：变体数超过上限");

/// 图元的逐实例顶点属性：和 [`Quad`] 的字段、`shaders/quad.wgsl` 里 `QuadIn` 的 `@location` 一一对应
/// （`layout_guard.rs` 核对）
pub(crate) const QUAD_ATTRIBUTES: [wgpu::VertexAttribute; 13] = wgpu::vertex_attr_array![
    0 => Float32x4, // rect
    1 => Float32x4, // radii
    2 => Float32x4, // transform
    3 => Float32x4, // shadow_offset, shadow_blur, border_width
    4 => Unorm8x4,  // fill
    5 => Unorm8x4,  // border_color
    6 => Unorm8x4,  // shadow_color
    7 => Float32,   // opacity
    8 => Float32x4, // uv
    9 => Uint32,    // kind
    10 => Uint32,   // layer
    11 => Uint32,   // clip
    12 => Uint32,   // gradient
];

/// 管线的入口
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Entry {
    /// `fs_main`：所有图元、普通的分组合成
    Quad,
    /// `fs_composite`：带滤镜、遮罩、混合模式的分组合成
    Composite,
    /// 模糊的一遍（`blur.wgsl`）：只有离屏格式、不带特性的一种
    Blur,
}

/// 一条管线：入口、目标格式、着色器特性
pub(crate) type Key = (Entry, wgpu::TextureFormat, ShaderFeatures);

/// 窗口的渲染器第一帧画完以后在后台先编译的管线（目标是窗口表面的格式 `format`），按用到的可能性排：
/// 投影、内阴影（`RICH_SHAPES`）最常见，然后是分组的内容和合成、模糊、分组里的投影
pub(crate) fn common(format: wgpu::TextureFormat) -> [Key; 6] {
    [
        (Entry::Quad, format, ShaderFeatures::RICH_SHAPES),
        (Entry::Quad, OFFSCREEN, ShaderFeatures::NONE),
        (Entry::Composite, format, ShaderFeatures::NONE),
        (Entry::Blur, OFFSCREEN, ShaderFeatures::NONE),
        (Entry::Quad, OFFSCREEN, ShaderFeatures::RICH_SHAPES),
        (Entry::Composite, OFFSCREEN, ShaderFeatures::NONE),
    ]
}

/// 编译一个着色器模块，记下耗时
pub(crate) fn shader_module(device: &wgpu::Device, label: &str, source: wgpu::ShaderSource<'_>) -> wgpu::ShaderModule {
    let start = Instant::now();
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source });
    tracing::debug!(target: COMPILE_LOG, "着色器 {label}：{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    module
}

/// 建管线要的东西：着色器模块、管线布局、设备的磁盘缓存。克隆只加引用计数，后台线程拿一份
#[derive(Clone)]
pub(crate) struct Sources {
    pub module: wgpu::ShaderModule,
    pub blur_module: wgpu::ShaderModule,
    /// `fs_main` 和自定义着色器的管线布局
    pub quad_layout: wgpu::PipelineLayout,
    /// `fs_composite` 的管线布局：第 1 组绑定是内容和遮罩，第 3 组是后面的内容
    pub composite_layout: wgpu::PipelineLayout,
    pub blur_layout: wgpu::PipelineLayout,
    /// 设备的管线缓存（后端支持、设了目录时），见 `disk_cache.rs`
    pub disk: Option<Arc<DiskCache>>,
}

impl Sources {
    /// 编译一条管线
    fn create(&self, device: &wgpu::Device, (entry, format, features): Key) -> wgpu::RenderPipeline {
        let cache = self.cache();
        match entry {
            Entry::Quad => create(device, &self.quad_layout, &self.module, "fs_main", format, features, cache),
            Entry::Composite => create(device, &self.composite_layout, &self.module, "fs_composite", format, features, cache),
            Entry::Blur => create_blur(device, &self.blur_layout, &self.blur_module, format, cache),
        }
    }

    fn cache(&self) -> Option<&wgpu::PipelineCache> {
        self.disk.as_ref().map(|disk| &disk.cache)
    }

    /// 编了新管线以后：缓存变大了就存盘
    fn save(&self) {
        if let Some(disk) = &self.disk {
            disk.save();
        }
    }
}

/// 编译过的自定义着色器；编译失败的记为 `None`，不再重试
struct Custom {
    module: Option<wgpu::ShaderModule>,
    pipelines: HashMap<(wgpu::TextureFormat, ShaderFeatures), Option<wgpu::RenderPipeline>>,
}

/// 渲染器的全部管线变体
pub(crate) struct Pipelines {
    sources: Sources,
    built: HashMap<Key, wgpu::RenderPipeline>,
    /// 按源码的散列
    custom: HashMap<u64, Custom>,
    /// 后台线程在先编译的
    background: Option<Background>,
    /// 一共编译了几条（这个线程上当场编的，加上后台编好的），测试看有没有重复编译
    compiled: Arc<AtomicUsize>,
}

/// 后台的编译：排着的、正在编的，编好的从通道交回来
struct Background {
    queue: Arc<Mutex<Queue>>,
    done: Receiver<(Key, wgpu::RenderPipeline)>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Key>,
    building: Option<Key>,
}

impl Pipelines {
    /// 一条管线都不编译：第一次用到时再编
    pub fn new(sources: Sources) -> Self {
        Self { sources, built: HashMap::new(), custom: HashMap::new(), background: None, compiled: Arc::default() }
    }

    /// 这个入口在 `format` 的目标上带 `features` 的管线，第一次用到时创建。后台正在编它时等它编完，
    /// 还排着时从队列里拿出来当场编
    pub fn get(&mut self, device: &wgpu::Device, entry: Entry, format: wgpu::TextureFormat, features: ShaderFeatures) -> &wgpu::RenderPipeline {
        let key = (entry, format, features);
        if !self.built.contains_key(&key) {
            let pipeline = self.take_background(key).unwrap_or_else(|| {
                self.compiled.fetch_add(1, Ordering::Relaxed);
                self.sources.create(device, key)
            });
            self.built.insert(key, pipeline);
        }
        &self.built[&key]
    }

    /// 在后台线程上把 `keys` 里还没有的管线先编译好（同一个设备上几个线程同时建管线是允许的）。
    /// 编好的下次 [`Self::get`] 时收下；渲染器释放时还没开始编的不再编
    pub fn precompile(&mut self, device: &wgpu::Device, keys: impl IntoIterator<Item = Key>) {
        let waiting: VecDeque<Key> = keys.into_iter().filter(|key| !self.built.contains_key(key)).collect();
        if waiting.is_empty() || self.background.is_some() {
            return;
        }
        let queue = Arc::new(Mutex::new(Queue { waiting, building: None }));
        let (send, done) = mpsc::channel();
        let job = Job { device: device.clone(), sources: self.sources.clone(), queue: queue.clone(), send, compiled: self.compiled.clone() };
        match std::thread::Builder::new().name("pipelines".into()).spawn(move || job.run()) {
            Ok(_) => self.background = Some(Background { queue, done }),
            // 开不了线程：第一次用到时照样当场编
            Err(error) => tracing::warn!("开不了预编译管线的线程：{error}"),
        }
    }

    /// 收下后台编好的；`key` 正在后台编时等它，还排着时从队列里拿掉（由调用方当场编）
    fn take_background(&mut self, key: Key) -> Option<wgpu::RenderPipeline> {
        let background = self.background.as_ref()?;
        // 先从队列里拿掉（后台不会再开始编它），看它是不是正在编
        let in_flight = {
            let mut queue = background.queue.lock().unwrap_or_else(PoisonError::into_inner);
            queue.waiting.retain(|k| *k != key);
            queue.building == Some(key)
        };
        // 后台交回一条以后才去拿下一条：上面看到的不是它，它就已经在通道里了（或者从来没在后台编过）
        while let Ok((done, pipeline)) = background.done.try_recv() {
            self.built.insert(done, pipeline);
        }
        if let Some(pipeline) = self.built.remove(&key) {
            return Some(pipeline);
        }
        if !in_flight {
            return None;
        }
        // 正在编：等它（后台线程出错退出时通道断开，当场编）
        while let Ok((done, pipeline)) = background.done.recv() {
            if done == key {
                return Some(pipeline);
            }
            self.built.insert(done, pipeline);
        }
        None
    }

    /// 自定义着色器（WGSL 源码）在 `format` 的目标上带 `features` 的管线，第一次用到时编译；
    /// 编译失败返回 `None`（记一条错误，不画，不再重试）
    pub fn custom(&mut self, device: &wgpu::Device, source: &str, format: wgpu::TextureFormat, features: ShaderFeatures) -> Option<wgpu::RenderPipeline> {
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        let custom = self.custom.entry(hasher.finish()).or_insert_with(|| Custom { module: compile_custom(device, source), pipelines: HashMap::new() });
        let module = custom.module.as_ref()?;
        let (layout, cache) = (&self.sources.quad_layout, self.sources.cache());
        custom
            .pipelines
            .entry((format, features))
            .or_insert_with(|| {
                let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
                let pipeline = create(device, layout, module, "fs_custom", format, features, cache);
                match pollster::block_on(scope.pop()) {
                    Some(error) => {
                        tracing::error!("自定义着色器创建管线失败：\n{error}");
                        None
                    }
                    None => Some(pipeline),
                }
            })
            .clone()
    }

}

impl Drop for Pipelines {
    /// 渲染器不要了：还没开始编的不再编（正在编的那一条编完，交不出去，后台线程结束）；当场编过的存进磁盘缓存
    fn drop(&mut self) {
        if let Some(background) = &self.background {
            background.queue.lock().unwrap_or_else(PoisonError::into_inner).waiting.clear();
        }
        self.sources.save();
    }
}

/// 后台线程：按队列的顺序一条条编，编好交回去
struct Job {
    device: wgpu::Device,
    sources: Sources,
    queue: Arc<Mutex<Queue>>,
    send: Sender<(Key, wgpu::RenderPipeline)>,
    compiled: Arc<AtomicUsize>,
}

impl Job {
    fn run(self) {
        let start = Instant::now();
        let mut count = 0;
        loop {
            let key = {
                let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
                queue.building = queue.waiting.pop_front();
                queue.building
            };
            let Some(key) = key else { break };
            self.compiled.fetch_add(1, Ordering::Relaxed);
            let pipeline = self.sources.create(&self.device, key);
            count += 1;
            // 渲染器已经释放：不用再编了
            if self.send.send((key, pipeline)).is_err() {
                break;
            }
        }
        tracing::debug!(target: COMPILE_LOG, "后台预编译了 {count} 条管线：{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
        self.sources.save();
    }
}

fn compile_custom(device: &wgpu::Device, source: &str) -> Option<wgpu::ShaderModule> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = shader_module(device, "custom shader", wgpu::ShaderSource::Wgsl(format!("{QUAD_SHADER}\n{CUSTOM_SHADER}\n{source}").into()));
    match pollster::block_on(scope.pop()) {
        Some(error) => {
            tracing::error!("自定义着色器编译失败：\n{error}");
            None
        }
        None => Some(module),
    }
}

/// 模糊的着色器模块
pub(crate) fn blur_module(device: &wgpu::Device) -> wgpu::ShaderModule {
    shader_module(device, "blur", wgpu::ShaderSource::Wgsl(BLUR_SHADER.into()))
}

/// `features` 决定着色器里按透视算裁剪的那一段（`PROJECTIVE_CLIPS`）、把扩展范围的颜色放进目标的那一段
/// （`EXTENDED_COLORS`）、单独的投影 / 内阴影 / 渐变边框（`RICH_SHAPES`）要不要编译进来
fn create(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    fragment: &str,
    format: wgpu::TextureFormat,
    features: ShaderFeatures,
    cache: Option<&wgpu::PipelineCache>,
) -> wgpu::RenderPipeline {
    let flag = |feature: ShaderFeatures| if features.contains(feature) { 1.0 } else { 0.0 };
    let constants = [
        ("PROJECTIVE_CLIPS", flag(ShaderFeatures::PROJECTIVE_CLIPS)),
        ("EXTENDED_COLORS", flag(ShaderFeatures::EXTENDED_COLORS)),
        ("RICH_SHAPES", flag(ShaderFeatures::RICH_SHAPES)),
    ];
    let start = Instant::now();
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(fragment),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: size_of::<Quad>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &QUAD_ATTRIBUTES,
            })],
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            compilation_options: wgpu::PipelineCompilationOptions { constants: &constants, ..Default::default() },
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache,
    });
    tracing::debug!(target: COMPILE_LOG, "管线 {fragment}（{format:?}，特性 {features:?}）：{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    pipeline
}

/// 模糊的一遍：全屏三角形，不混合
fn create_blur(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    cache: Option<&wgpu::PipelineCache>,
) -> wgpu::RenderPipeline {
    let start = Instant::now();
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("blur"),
        layout: Some(layout),
        vertex: wgpu::VertexState { module, entry_point: Some("vs_main"), compilation_options: Default::default(), buffers: &[] },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache,
    });
    tracing::debug!(target: COMPILE_LOG, "管线 blur（{format:?}）：{:.1} ms", start.elapsed().as_secs_f64() * 1000.0);
    pipeline
}
