//! 图片：`image(来源)` 元素。
//!
//! - 来源：文件路径、内存里的编码数据（PNG、JPEG、WebP、GIF）、原始 RGBA（8 位 sRGB 或线性浮点）；可以绑定信号换图。
//! - 色彩：按嵌的 ICC 配置、PNG 的 cICP 换到工作空间（见 [`color`]）：没有配置的按 sRGB；
//!   广色域、HDR 的图存成半精度浮点的纹理，在广色域、HDR 显示器上原样显示，普通显示器上截断、色调映射。
//! - 动图（GIF、动态 WebP）：所有帧在后台解码好、留在缓存里；每个元素有自己的播放进度和一块当前帧的 GPU 位置，
//!   换帧时只传新的一帧（见 [`Player`]、`tree/animated.rs`），看不见时不换帧。播放控制见 [`PlaybackHandle`]
//! - 加载：后台线程解码、预乘、生成 mip（见 [`loader`]、[`pixels`]），好了以后元素自动重新布局、重画；
//!   同一个来源只解码一次，没人用了就释放（最近用过的留在有预算的缓存里）。
//! - 绘制：小图放进带 mip 的图片图集，和其他图元同一次 draw call；大图各自一张纹理，画到它时
//!   断开一次 draw call。都是 sRGB 纹理、预乘 alpha、三线性过滤，缩小的照片不闪烁、不发暗。
//!   圆角用距离场裁剪，裁剪、分组透明度、合成层、模糊和背景模糊照常适用。
//!
//! GPU 资源按窗口（元素树）管理：每棵树记下自己用到的图片放在图集哪里、哪些有单独的纹理，
//! 和字形图集一样把变化（[`scene::ImageUpdate`]）交给自己的渲染线程。
//!
//! SVG 文件（`svg` 特性）也是一种来源：缓存、后台解析、大小、摆放方式、出错都和图片一样，只是解析的结果是一份文档（`svg.rs`），
//! 画成路径图元（`svg_paint.rs`），不占图片图集；文件里的位图才占。

mod color;
mod loader;
mod pixels;
#[cfg(feature = "svg")]
mod svg;
#[cfg(feature = "svg")]
mod svg_paint;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use animation::Curve;
use reactive::{Effect, Signal, signal, untracked};
use scene::{AtlasUpdate, Color, ColorMatrix, DisplayList, IMAGE_ATLAS_LEVELS, ImageData, ImageEncoding, ImageUpdate, Quad, max_atlas_size};

use crate::property::Property;
use crate::element::Element;
use crate::geometry::Affine;
use crate::gradient::Fill;

use crate::property::{IntoProp, PropId, Prop};
use crate::geometry::Rect;
use crate::tree::{NodeId, Tree, TreeRef};

pub(crate) use loader::{ImageRef, Status, acquire};
use pixels::Frames;
#[cfg(feature = "svg")]
pub(crate) use svg_paint::SvgTarget;

/// 图片元素。来源可以是路径（`&str`、`String`、`PathBuf`）、[`ImageSource`]，
/// 也可以绑定信号或闭包来换图（新图解码好之前继续显示旧图）。
///
/// 没指定大小时用图片的原始大小（1 个像素 = 1 个逻辑像素），只指定一边时按宽高比算另一边；
/// 解码好之前大小为 0、什么都不画（可以给元素设背景色当占位）。
///
/// 打开 `svg` 特性时也认得 SVG 文件（按内容认，不看扩展名）：大小用文件写的大小，按需要的缩放画成矢量路径，放大也清晰。
/// 没打开时进入出错状态，原因里写明要打开这个特性。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let state = signal(ImageState::Loading);
/// image("photos/cat.jpg")
///     .size(240, 160)
///     .fit(ImageFit::Cover)        // 等比铺满内容区，裁掉多余的部分
///     .radius(12)                  // 圆角裁剪图片
///     .image_fade(Tween::new(ms(250)))     // 解码好后淡入
///     .report_load_state(state);          // 加载状态和原始大小写进信号
/// ```
pub fn image<V: Into<ImageSource> + 'static>(source: impl IntoProp<V>) -> Element {
    Element::from_kind(ImageBlock::default()).set(crate::property::SOURCE, source)
}

/// 图片从哪来。克隆便宜（数据都是共享的）；同一个来源（同一路径、同一块内存）只解码一次
#[derive(Clone)]
pub struct ImageSource {
    pub(crate) kind: SourceKind,
    pub(crate) max_size: Option<u32>,
    /// 文件的修订号（开发工具的图片监视，见 [`set_image_revision_source`]）：文件改了以后换一个键，重新解码
    pub(crate) revision: u64,
}

#[derive(Clone)]
pub(crate) enum SourceKind {
    File(Arc<Path>),
    Bytes(Arc<[u8]>),
    Static(&'static [u8]),
    Rgba { width: u32, height: u32, pixels: Arc<[u8]> },
    RgbaF32 { width: u32, height: u32, pixels: Arc<[f32]> },
}

/// 缓存的键：文件按路径，内存数据按地址（同一块数据才共享）
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct SourceKey {
    kind: KeyKind,
    max_size: Option<u32>,
    revision: u64,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum KeyKind {
    File(Arc<Path>),
    Memory(usize, usize),
    Rgba(usize, usize, u32),
}

impl ImageSource {
    /// 图片文件（PNG、JPEG、WebP、GIF，打开 `svg` 特性时还有 SVG），在后台线程读取和解码
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self { kind: SourceKind::File(path.as_ref().into()), max_size: None, revision: 0 }
    }

    /// 内存里的编码数据（PNG、JPEG、WebP、GIF，打开 `svg` 特性时还有 SVG）。按这块内存区分：要共享解码结果，
    /// 请克隆同一个 `ImageSource`，而不是每次用同样的字节新建一个
    pub fn bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self { kind: SourceKind::Bytes(bytes.into()), max_size: None, revision: 0 }
    }

    /// 编进程序里的编码数据（`include_bytes!`），不复制
    pub fn from_static(bytes: &'static [u8]) -> Self {
        Self { kind: SourceKind::Static(bytes), max_size: None, revision: 0 }
    }

    /// 原始像素：RGBA8，sRGB，直通（未预乘）alpha，逐行紧密排列
    pub fn rgba(width: u32, height: u32, pixels: impl Into<Arc<[u8]>>) -> Self {
        Self { kind: SourceKind::Rgba { width, height, pixels: pixels.into() }, max_size: None, revision: 0 }
    }

    /// 原始像素：RGBA 各一个 `f32`，线性光、sRGB 原色，直通 alpha，逐行紧密排列。分量可以小于 0
    /// （sRGB 色域外的颜色，见 [`scene::P3_TO_SRGB`]）或大于 1（比 SDR 白亮的 HDR 内容，1.0 是 SDR 白）。
    /// 存成半精度浮点的纹理（每像素 8 字节，不进图集），显示时按显示器的余量做色调映射
    pub fn rgba_f32(width: u32, height: u32, pixels: impl Into<Arc<[f32]>>) -> Self {
        Self { kind: SourceKind::RgbaF32 { width, height, pixels: pixels.into() }, max_size: None, revision: 0 }
    }

    /// 解码后最长边不超过 `pixels` 像素（等比缩小），省内存和上传时间，例如缩略图墙。
    /// 布局用的原始大小不变；显示得比它大时会糊。对 SVG 文件不起作用（按显示的大小画）
    pub fn max_size(mut self, pixels: u32) -> Self {
        self.max_size = Some(pixels.max(1));
        self
    }

    pub(crate) fn key(&self) -> SourceKey {
        let kind = match &self.kind {
            SourceKind::File(path) => KeyKind::File(path.clone()),
            SourceKind::Bytes(bytes) => KeyKind::Memory(bytes.as_ptr() as usize, bytes.len()),
            SourceKind::Static(bytes) => KeyKind::Memory(bytes.as_ptr() as usize, bytes.len()),
            SourceKind::Rgba { width, pixels, .. } => KeyKind::Rgba(pixels.as_ptr() as usize, pixels.len(), *width),
            SourceKind::RgbaF32 { width, pixels, .. } => KeyKind::Rgba(pixels.as_ptr() as usize, pixels.len() * 4, *width),
        };
        SourceKey { kind, max_size: self.max_size, revision: self.revision }
    }
}

impl PartialEq for ImageSource {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SourceKind::File(path) => write!(f, "ImageSource::file({path:?})")?,
            SourceKind::Bytes(bytes) => write!(f, "ImageSource::bytes({} 字节)", bytes.len())?,
            SourceKind::Static(bytes) => write!(f, "ImageSource::from_static({} 字节)", bytes.len())?,
            SourceKind::Rgba { width, height, .. } => write!(f, "ImageSource::rgba({width}x{height})")?,
            SourceKind::RgbaF32 { width, height, .. } => write!(f, "ImageSource::rgba_f32({width}x{height})")?,
        }
        if let Some(max) = self.max_size {
            write!(f, ".max_size({max})")?;
        }
        Ok(())
    }
}

impl From<&str> for ImageSource {
    fn from(path: &str) -> Self {
        Self::file(path)
    }
}

impl From<String> for ImageSource {
    fn from(path: String) -> Self {
        Self::file(path)
    }
}

impl From<PathBuf> for ImageSource {
    fn from(path: PathBuf) -> Self {
        Self::file(path)
    }
}

impl From<&Path> for ImageSource {
    fn from(path: &Path) -> Self {
        Self::file(path)
    }
}

impl IntoProp<ImageSource> for ImageSource {
    fn into_prop(self) -> Prop<ImageSource> {
        Prop::Static(self)
    }
}

/// 内存里的一张位图：RGBA8，sRGB，直通（未预乘）alpha，逐行紧密排列。克隆便宜（像素是共享的）。
/// 剪贴板里读出来的、从别的应用拖进来的图片都是它，能直接给 [`image()`]。
/// 同一张图（克隆出来的）只解码一次，换一张读出来的算新图
///
/// ```no_run
/// # use aureole::prelude::*;
/// if let Some(picture) = aureole::clipboard::read_image() {
///     image(picture).size(240, 160);
/// }
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 像素，`width × height × 4` 个字节
    pub pixels: Arc<[u8]>,
}

impl RgbaImage {
    /// 一张位图。像素数要和宽高对得上（放进剪贴板时检查，对不上时写入失败）
    pub fn new(width: u32, height: u32, pixels: impl Into<Arc<[u8]>>) -> Self {
        Self { width, height, pixels: pixels.into() }
    }
}

impl fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RgbaImage({}×{}, {} 字节)", self.width, self.height, self.pixels.len())
    }
}

impl From<RgbaImage> for ImageSource {
    fn from(image: RgbaImage) -> Self {
        Self::rgba(image.width, image.height, image.pixels)
    }
}

impl From<&RgbaImage> for ImageSource {
    fn from(image: &RgbaImage) -> Self {
        Self::rgba(image.width, image.height, image.pixels.clone())
    }
}

impl IntoProp<RgbaImage> for RgbaImage {
    fn into_prop(self) -> Prop<RgbaImage> {
        Prop::Static(self)
    }
}

/// 文件路径。`&str`、`String` 只有 `IntoProp<String>` 一个实现（`text("..")` 这类调用才推断得出来），
/// [`image()`] 收它们靠 `String: Into<ImageSource>`
impl IntoProp<PathBuf> for PathBuf {
    fn into_prop(self) -> Prop<PathBuf> {
        Prop::Static(self)
    }
}

/// 图片怎么放进元素的内容区
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageFit {
    /// 拉伸到框的大小，不保持宽高比（默认）
    #[default]
    Stretch,
    /// 等比缩放到完整放进去，留白
    Contain,
    /// 等比缩放到铺满，裁掉多出来的部分
    Cover,
    /// 原始大小，超出的部分裁掉
    None,
    /// `None` 和 `Contain` 里较小的那个：只缩小、不放大
    ScaleDown,
}

impl IntoProp<ImageFit> for ImageFit {
    fn into_prop(self) -> Prop<ImageFit> {
        Prop::Static(self)
    }
}

/// 图片的加载状态，见 [`Element::report_load_state`]
#[derive(Clone, Debug, PartialEq)]
pub enum ImageState {
    /// 正在读取或解码
    Loading,
    /// 解码好了，`width` x `height` 是原始大小（像素）
    Ready {
        /// 原始宽度，像素
        width: u32,
        /// 原始高度，像素
        height: u32,
    },
    /// 读不到文件、格式不支持、数据损坏
    Error(Arc<str>),
}

impl ImageState {
    /// 是否已经解码好
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// 原始大小（像素），解码好之前为 `None`
    pub fn size(&self) -> Option<[u32; 2]> {
        match *self {
            Self::Ready { width, height } => Some([width, height]),
            _ => None,
        }
    }
}

/// 图片文件的修订号从哪里来，见 [`set_image_revision_source`]
type RevisionSource = fn(&std::path::Path) -> u64;

thread_local! {
    static REVISION_SOURCE: std::cell::Cell<Option<RevisionSource>> = const { std::cell::Cell::new(None) };
}

/// 在这个线程上装上图片文件的修订号从哪里来（开发工具的图片监视：文件变了修订号加一，图片缓存的键带上它，
/// 于是重新解码）。函数在图片的绑定里调用，读了它的信号就订阅变化。没装时修订号都是 0
pub fn set_image_revision_source(source: RevisionSource) {
    REVISION_SOURCE.with(|s| s.set(Some(source)));
}

/// 图片文件现在的修订号
fn image_revision(path: &std::path::Path) -> u64 {
    REVISION_SOURCE.with(std::cell::Cell::get).map_or(0, |source| source(path))
}

/// 图片缓存的统计，见 [`image_cache_stats`]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImageCacheStats {
    /// 正在被元素使用的图片（含还在解码的）
    pub images: usize,
    /// 其中还在解码的
    pub loading_images: usize,
    /// 正在使用的图片的像素（含 mip）占多少字节
    pub bytes: u64,
    /// 没人用、但还留着的解码结果（再用到时不用重新解码）
    pub retained_images: usize,
    /// 留着的解码结果占多少字节
    pub retained_bytes: u64,
    /// 一共解码成功了几张
    pub decoded_images: usize,
    /// 解码累计耗时（后台线程上的解码 + 预乘 + mip）
    pub decode_time: Duration,
}

/// 当前线程图片缓存的统计
pub fn image_cache_stats() -> ImageCacheStats {
    loader::stats()
}

/// 没人用的解码结果最多留多少字节（默认 64MB），超出时先丢最久没用的；0 表示不留
pub fn set_image_cache_budget(bytes: u64) {
    loader::set_budget(bytes as usize);
}

/// 等这个线程提交的后台解码全部完成（测试、截图用）。按进展等：解码线程池（全进程共用）连续 `stall` 没有解完任何一张
/// 才算卡住，返回还在等的图片（来源的调试表示）
pub fn wait_idle(stall: Duration) -> Result<(), Vec<String>> {
    loader::wait_idle(stall)
}

/// 动图的播放控制，交给 [`Element::playback_handle`]。像信号一样是 `Copy` 的句柄，可以随便放进闭包；
/// 一个句柄可以控制几个元素（一起暂停、一起重新播）。
///
/// 默认（没调用过 [`PlaybackHandle::play`]、[`PlaybackHandle::pause`]）跟着系统：没打开"减少动态效果"时播，
/// 打开时停在第一帧。用户自己点了播放（`play`）就播，不管这个设置。
///
/// ```no_run
/// # use aureole::prelude::*;
/// let player = PlaybackHandle::new();
/// column()
///     .child(image("loading.gif").size(64, 64).playback_handle(player))
///     .child(button("重新播").on_click(move || player.restart()))
///     .child(text(move || if player.is_finished() { "播完了" } else { "" }.to_string()));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackHandle {
    /// `None`：跟着系统的减少动态效果
    playing: Signal<Option<bool>>,
    restarts: Signal<u64>,
    pub(crate) finished: Signal<bool>,
}

impl PlaybackHandle {
    /// 新的句柄：跟着系统，没打开减少动态效果时播
    pub fn new() -> Self {
        Self { playing: signal(None), restarts: signal(0), finished: signal(false) }
    }

    /// 播放（从停下的那一帧接着播；播完的不会重新开始，用 [`PlaybackHandle::restart`]）
    pub fn play(self) {
        self.playing.set(Some(true));
    }

    /// 暂停，停在当前这一帧
    pub fn pause(self) {
        self.playing.set(Some(false));
    }

    /// 在播就暂停，停着就播
    pub fn toggle(self) {
        let playing = self.is_playing_untracked();
        self.playing.set(Some(!playing));
    }

    /// 回到第一帧重新播（播完的也重新开始），播放次数重新算
    pub fn restart(self) {
        self.restarts.update(|n| *n += 1);
    }

    /// 是否要播（响应式）：调用过 `play` / `pause` 时按调用的，否则看系统有没有打开减少动态效果。
    /// 元素在屏幕外、窗口被挡住时也是 `true`，只是暂时不换帧
    pub fn is_playing(self) -> bool {
        self.playing.get().unwrap_or_else(|| !crate::system::is_motion_reduced())
    }

    fn is_playing_untracked(self) -> bool {
        untracked(|| self.is_playing())
    }

    /// 按文件里的循环次数播完了（响应式），停在最后一帧；一直循环的动图永远是 `false`
    pub fn is_finished(self) -> bool {
        self.finished.get()
    }
}

impl Default for PlaybackHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// 图片元素上不是属性的设置：淡入（过渡）、加载状态（写回信号）、播放句柄。挂载时和来源一起绑定
#[derive(Default)]
pub(crate) struct ImageExtras {
    fade_in: bool,
    state: Option<Signal<ImageState>>,
    player: Option<PlaybackHandle>,
}

impl Element {
    /// 图片的淡入、加载状态、播放句柄；设在还没有种类的元素上时，它成为图片元素
    fn image_extras(&mut self) -> &mut ImageExtras {
        if self.kind.is_none() {
            self.kind = Some(Box::new(ImageBlock::default()));
        }
        self.image.get_or_insert_with(Box::default)
    }

    /// 图片在内容区里的对齐：0 靠左 / 上，0.5 居中（默认），1 靠右 / 下，同 `.image_position_x(x).image_position_y(y)`。
    /// 对 `Contain`、`Cover`、`None`、`ScaleDown` 有效
    pub fn image_position(self, x: impl IntoProp<f32>, y: impl IntoProp<f32>) -> Self {
        self.image_position_x(x).image_position_y(y)
    }

    /// 解码好之后按曲线淡入；挂载时已经在缓存里的图片直接显示。
    /// 用的是元素的透明度（合成层动画，由渲染线程推进），和 [`Element::opacity`] 相乘
    pub fn image_fade(mut self, curve: impl Into<Curve>) -> Self {
        self.image_extras().fade_in = true;
        self.transition(Property::Opacity, curve)
    }

    /// 把加载状态（加载中 / 原始大小 / 出错）写进信号，换图时跟着变
    pub fn report_load_state(mut self, state: Signal<ImageState>) -> Self {
        self.image_extras().state = Some(state);
        self
    }

    /// 用句柄控制动图：暂停、播放、重新播，读它播完了没有（见 [`PlaybackHandle`]）。
    /// 同时设了 [`Element::playing`] 时，播不播按 `playing`
    pub fn playback_handle(mut self, player: PlaybackHandle) -> Self {
        self.image_extras().player = Some(player);
        self
    }
}

/// 注册表里 `source` 的绑定：没有淡入、加载状态、播放句柄时
pub(crate) fn bind_source(tree: &TreeRef, id: NodeId, source: crate::property::registry::Slot) {
    bind(tree, id, Some(source), None, false);
}

/// 挂载时绑定图片的来源，连同淡入、加载状态、播放句柄（`playing` 设了时播不播按它，由属性表绑定）。
/// 淡入时返回"有图片在显示了"的信号，透明度乘上它
pub(crate) fn bind(tree: &TreeRef, id: NodeId, source: Option<crate::property::registry::Slot>, extras: Option<Box<ImageExtras>>, has_playing: bool) -> Option<Signal<bool>> {
    let ImageExtras { fade_in, state, player } = extras.map(|e| *e).unwrap_or_default();
    if let Some(player) = player {
        // 动图的播放：没设 `playing` 时跟着句柄（句柄的 `None` 是跟着系统）
        if !has_playing {
            crate::tree::bind(tree, Some(Prop::Dynamic(Box::new(move || player.playing.get()))), move |tree, intent| tree.set_playing(id, intent));
        }
        tree.borrow_mut().update_image(id, |block| block.handle = Some(player));
        // 重新播：第一次执行（挂载时）不算
        let weak: Weak<std::cell::RefCell<Tree>> = Rc::downgrade(tree);
        let mut first = true;
        Effect::new(move || {
            player.restarts.get();
            if std::mem::take(&mut first) {
                return;
            }
            player.finished.set(false);
            if let Some(tree) = weak.upgrade() {
                tree.borrow_mut().restart_image(id);
            }
        });
    }

    // 有图片在显示：淡入用
    let loaded = fade_in.then(|| signal(false));
    if let Some(source) = source {
        // 解码好时加一，让下面的绑定重新执行
        let tick = signal(0u64);
        let weak: Weak<std::cell::RefCell<Tree>> = Rc::downgrade(tree);
        Effect::new(move || {
            tick.get();
            let Some(mut source) = source.get::<ImageSource>() else { return };
            // 打开了图片监视时，文件改了修订号就变，换一个缓存条目重新解码
            if let SourceKind::File(path) = &source.kind {
                source.revision = image_revision(path);
            }
            // 被覆盖层盖着：记下新的来源，不换图
            let source = match weak.upgrade() {
                Some(tree) => match tree.borrow_mut().hold_back(id, PropId::SOURCE, source) {
                    Some(source) => source,
                    None => return,
                },
                None => source,
            };
            let image = untracked(|| loader::acquire(&source));
            image.listen(tick);
            let status = image.state();
            let shown = weak.upgrade().is_some_and(|tree| tree.borrow_mut().set_image(id, image));
            if let Some(state) = state {
                state.set_if_changed(status);
            }
            if let Some(loaded) = loaded {
                loaded.set_if_changed(shown);
            }
        });
    }
    loaded
}

/// 图片元素的内容
pub(crate) struct ImageBlock {
    /// 绑定的图片（可能还在解码）
    requested: Option<ImageRef>,
    /// 正在显示的图片：换图时新图解码好之前继续显示旧的
    shown: Option<ImageRef>,
    pub fit: ImageFit,
    pub position: [f32; 2],
    /// 改色（[`Element::tint`]）：透明表示不改
    pub tint: Color,
    /// 显示的是动图时，这个元素自己的播放状态
    pub player: Option<Player>,
    /// 想不想播：绑定的 [`Element::playing`] 或者句柄上调用过的 `play` / `pause`；
    /// `None` 跟着系统（没打开减少动态效果时播，打开时回到第一帧）
    pub intent: Option<bool>,
    /// 播放控制的句柄：播完时写它的信号
    pub handle: Option<PlaybackHandle>,
}

impl Default for ImageBlock {
    fn default() -> Self {
        Self {
            requested: None,
            shown: None,
            fit: ImageFit::Stretch,
            position: [0.5, 0.5],
            tint: Color::TRANSPARENT,
            player: None,
            intent: None,
            handle: None,
        }
    }
}

impl ImageBlock {
    pub fn showing(&self) -> bool {
        self.shown.is_some()
    }

    /// 来源的说明（测试读的元素树快照用）
    pub fn source_label(&self) -> Option<String> {
        self.requested.as_ref().map(|image| format!("{:?}", image.source()))
    }

    /// 没指定大小时的大小：图片的原始大小，没在显示时为 0
    pub fn intrinsic(&self) -> [f32; 2] {
        match self.shown.as_ref().map(ImageRef::status) {
            Some(Status::Ready { natural, .. }) => [natural[0] as f32, natural[1] as f32],
            #[cfg(feature = "svg")]
            Some(Status::Vector { document }) => document.size,
            _ => [0.0; 2],
        }
    }

    /// 正在显示的是 SVG 文件：解析好的文档
    #[cfg(feature = "svg")]
    pub fn document(&self) -> Option<Arc<svg::SvgDocument>> {
        match self.shown.as_ref().map(ImageRef::status) {
            Some(Status::Vector { document }) => Some(document),
            _ => None,
        }
    }

    /// 把 SVG 文档放进显示列表（[`ImageBlock::document`] 有的时候代替 [`ImageSystem::push`]）
    #[cfg(feature = "svg")]
    pub fn paint_document(&self, document: &svg::SvgDocument, area: &ImageArea, target: SvgTarget) {
        svg_paint::paint(document, self, area, target);
    }
}

/// 动图在一个元素上的播放状态。帧的像素在缓存里（同一个来源的元素共用），
/// 每个元素在 GPU 上有自己的一块位置，换帧时只把新的一帧传到这块位置上：不重新布局、不重建显示列表
pub(crate) struct Player {
    /// 这个元素在 GPU 资源里的位置（[`ImageSystem`] 里的键）
    key: Key,
    frames: Arc<Frames>,
    /// 正在显示第几帧
    pub frame: usize,
    /// 下一帧的时间（树的时钟，秒）；`None` 表示下次推进时从那一刻开始计时
    pub due: Option<f64>,
    /// 播完了几遍
    played: u32,
    pub finished: bool,
}

/// 推进一个动图的结果
pub(crate) struct Stepped {
    /// 换了帧
    pub changed: bool,
    /// 这一次推进播完了（按文件里的循环次数）
    pub finished: bool,
}

impl Player {
    pub fn frames(&self) -> usize {
        self.frames.frames.len()
    }

    /// 当前这一帧还要显示多久
    pub fn delay(&self) -> f64 {
        self.frames.delays[self.frame] as f64
    }

    /// 推进到 `now`：到点了就换下一帧，一次可能跳过几帧（卡顿之后），播到循环次数时停在最后一帧。
    /// 落后超过 1 秒时从现在重新计时，不一帧一帧地追
    pub fn step(&mut self, now: f64) -> Stepped {
        let mut out = Stepped { changed: false, finished: false };
        let Some(mut due) = self.due else {
            self.due = Some(now + self.delay());
            return out;
        };
        let count = self.frames();
        while now >= due && !self.finished {
            if self.frame + 1 < count {
                self.frame += 1;
            } else {
                self.played += 1;
                if self.frames.loops.is_some_and(|n| self.played >= n) {
                    self.finished = true;
                    out.finished = true;
                    break;
                }
                self.frame = 0;
            }
            out.changed = true;
            due += self.delay();
            if now - due > 1.0 {
                due = now + self.delay();
            }
        }
        self.due = Some(due);
        out
    }

    /// 回到第一帧，重新开始计时（重新播、减少动态效果时停在第一帧）。返回是否换了帧
    pub fn rewind(&mut self) -> bool {
        let changed = self.frame != 0;
        self.frame = 0;
        self.played = 0;
        self.finished = false;
        self.due = None;
        changed
    }

    /// 当前这一帧的像素
    pub fn current(&self) -> Arc<ImageData> {
        self.frames.frames[self.frame].clone()
    }
}

/// 图片在这棵树的 GPU 资源里放在哪
#[derive(Clone, Copy, Debug)]
enum Slot {
    /// 图片图集里的位置（像素）
    Atlas([u32; 2]),
    /// 单独的纹理
    Texture(u32),
}

/// 树里一块 GPU 位置的用途：一张图片（同一个来源的元素共用），一个元素上播放的动图（每个元素一块），
/// 或者 SVG 文件里的一张位图（显示这个文件的元素共用）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key {
    Image(u64),
    Player(u64),
    #[cfg(feature = "svg")]
    Embedded(u64),
}

/// 这棵树用到的一张图片
struct TreeImage {
    image: ImageRef,
    /// 有几处（元素的"绑定的"和"显示的"各算一处）在用；动图的播放位置总是 1
    users: u32,
    slot: Option<Slot>,
    /// 动图的播放位置：现在放着哪一帧
    frame: Option<Arc<ImageData>>,
}

impl TreeImage {
    /// 要放进这块位置的像素；还没解码好、或者是动图本身（由各元素的播放位置显示，不占位置）时为 `None`
    fn data(&self) -> Option<Arc<ImageData>> {
        match (&self.frame, self.image.status()) {
            (Some(frame), _) => Some(frame.clone()),
            (None, Status::Ready { frames: Some(_), .. }) => None,
            (None, Status::Ready { data, .. }) => Some(data),
            _ => None,
        }
    }
}

/// 每棵元素树的图片：引用计数、图片图集的装箱、给渲染线程的更新
#[derive(Default)]
pub(crate) struct ImageSystem {
    images: HashMap<Key, TreeImage>,
    /// 图片图集边长，0 表示还没建
    atlas_size: u32,
    shelves: Shelves,
    /// 动图卸载后空出来的图集位置（按存储大小），同样大小的动图再挂载时直接用，虚拟列表来回滚动不用重新装箱
    spare: Vec<([u32; 2], [u32; 2])>,
    next_texture: u32,
    next_player: u64,
    updates: Vec<AtlasUpdate>,
    /// 有解码好、还没分配 GPU 位置的图片
    dirty: bool,
}

/// 图片图集的起始边长；放不下时先重新装箱，还放不下再翻倍，最大 [`max_atlas_size`]
const ATLAS_START: u32 = 1024;

impl ImageSystem {
    /// 元素绑定的图片变了或者解码好了：更新元素要显示的图片，返回画面是否要变
    pub fn update(&mut self, block: &mut ImageBlock, image: ImageRef) -> bool {
        if !block.requested.as_ref().is_some_and(|r| r.same(&image)) {
            self.retain(&image);
            if let Some(old) = block.requested.replace(image.clone()) {
                self.release(&old);
            }
        }
        match image.status() {
            Status::Ready { frames, .. } => {
                if block.shown.as_ref().is_some_and(|s| s.same(&image)) {
                    return false;
                }
                self.retain(&image);
                if let Some(old) = block.shown.replace(image.clone()) {
                    self.release_shown(&old);
                }
                // 动图：这个元素自己的播放状态和 GPU 位置，从第一帧开始
                self.stop(block);
                if let Some(frames) = frames {
                    self.next_player += 1;
                    let key = Key::Player(self.next_player);
                    let first = frames.frames[0].clone();
                    self.images.insert(key, TreeImage { image, users: 1, slot: None, frame: Some(first) });
                    block.player = Some(Player { key, frames, frame: 0, due: None, played: 0, finished: false });
                }
                self.dirty = true;
                true
            }
            #[cfg(feature = "svg")]
            Status::Vector { document } => {
                if block.shown.as_ref().is_some_and(|s| s.same(&image)) {
                    return false;
                }
                self.retain(&image);
                if let Some(old) = block.shown.replace(image.clone()) {
                    self.release_shown(&old);
                }
                self.stop(block);
                self.show_embedded(&document, &image);
                true
            }
            Status::Loading => false,
            Status::Error(_) => match block.shown.take() {
                Some(old) => {
                    self.release_shown(&old);
                    self.stop(block);
                    true
                }
                None => false,
            },
        }
    }

    /// 元素卸载：放掉它用的图片
    pub fn detach(&mut self, block: &mut ImageBlock) {
        self.stop(block);
        if let Some(image) = block.requested.take() {
            self.release(&image);
        }
        if let Some(image) = block.shown.take() {
            self.release_shown(&image);
        }
    }

    /// 元素不再显示这张图片（SVG 文件里的位图也放掉）
    fn release_shown(&mut self, image: &ImageRef) {
        #[cfg(feature = "svg")]
        if let Status::Vector { document } = image.status() {
            self.hide_embedded(&document);
        }
        self.release(image);
    }

    /// 元素开始显示这个 SVG 文件：里面的位图占 GPU 位置（这棵树里显示同一个文件的元素共用）
    #[cfg(feature = "svg")]
    fn show_embedded(&mut self, document: &svg::SvgDocument, owner: &ImageRef) {
        for embedded in &document.images {
            let entry = TreeImage { image: owner.clone(), users: 0, slot: None, frame: Some(embedded.data.clone()) };
            self.images.entry(Key::Embedded(embedded.id)).or_insert(entry).users += 1;
            self.dirty = true;
        }
    }

    /// 元素不再显示这个 SVG 文件：没人用的位图放掉，单独的纹理释放（图集里的位置在下次重新装箱时收回）
    #[cfg(feature = "svg")]
    fn hide_embedded(&mut self, document: &svg::SvgDocument) {
        for embedded in &document.images {
            let key = Key::Embedded(embedded.id);
            let Some(entry) = self.images.get_mut(&key) else { continue };
            entry.users -= 1;
            if entry.users == 0
                && let Some(entry) = self.images.remove(&key)
                && let Some(Slot::Texture(id)) = entry.slot
            {
                self.updates.push(AtlasUpdate::Image(ImageUpdate::Release { id }));
            }
        }
    }

    /// SVG 文件里的一张位图放在哪；还没分配时是 `None`
    #[cfg(feature = "svg")]
    fn embedded_slot(&self, id: u64) -> Option<Slot> {
        self.images.get(&Key::Embedded(id))?.slot
    }

    /// 元素不再播放这个动图：放掉它的 GPU 位置（图集里的位置留给同样大小的动图）
    fn stop(&mut self, block: &mut ImageBlock) {
        let Some(player) = block.player.take() else { return };
        let Some(entry) = self.images.remove(&player.key) else { return };
        match entry.slot {
            Some(Slot::Texture(id)) => self.updates.push(AtlasUpdate::Image(ImageUpdate::Release { id })),
            Some(Slot::Atlas(at)) => {
                if let Some(frame) = entry.frame {
                    self.spare.push((frame.storage(), at));
                }
            }
            None => {}
        }
    }

    /// 动图换了一帧：只把新的一帧传到这个元素的位置上（显示列表不变）
    pub fn show_frame(&mut self, player: &Player) {
        let Some(entry) = self.images.get_mut(&player.key) else { return };
        let data = player.current();
        entry.frame = Some(data.clone());
        match entry.slot {
            Some(Slot::Atlas([x, y])) => self.updates.push(AtlasUpdate::Image(ImageUpdate::Atlas { x, y, image: data })),
            Some(Slot::Texture(id)) => self.updates.push(AtlasUpdate::Image(ImageUpdate::Write { id, image: data })),
            // 还没分配位置：分配时传的就是当前这一帧
            None => {}
        }
    }

    fn retain(&mut self, image: &ImageRef) {
        let entry = TreeImage { image: image.clone(), users: 0, slot: None, frame: None };
        self.images.entry(Key::Image(image.id())).or_insert(entry).users += 1;
    }

    /// 这棵树里没人用了：释放单独的纹理（图集里的位置在下次重新装箱时收回）
    fn release(&mut self, image: &ImageRef) {
        let key = Key::Image(image.id());
        let Some(entry) = self.images.get_mut(&key) else { return };
        entry.users -= 1;
        if entry.users == 0 {
            let entry = self.images.remove(&key).expect("刚查到");
            if let Some(Slot::Texture(id)) = entry.slot {
                self.updates.push(AtlasUpdate::Image(ImageUpdate::Release { id }));
            }
        }
    }

    /// 绘制前：给解码好的图片分配 GPU 位置（图集或单独的纹理），记下要上传的内容。
    /// 图集放不下时重新装箱（可能翻倍），所有图片的位置都可能变，所以必须在生成图元之前做
    pub fn prepare(&mut self) {
        if !std::mem::take(&mut self.dirty) {
            return;
        }
        let mut pending: Vec<Key> = self.images.iter().filter(|(_, e)| e.slot.is_none() && e.data().is_some()).map(|(&k, _)| k).collect();
        pending.sort_unstable();
        let mut repack = Vec::new();
        for key in pending {
            let data = self.images[&key].data().expect("刚检查过");
            if !fits_atlas(&data) {
                self.create_texture(key, data);
                continue;
            }
            let [w, h] = data.storage();
            // 动图先用空出来的同样大小的位置
            let spare = matches!(key, Key::Player(_)).then(|| self.spare.iter().position(|(size, _)| *size == [w, h])).flatten();
            let at = match spare {
                Some(i) => Some(self.spare.swap_remove(i).1),
                None => self.shelves.allocate(w, h),
            };
            match at {
                Some(at) if self.atlas_size > 0 => {
                    self.updates.push(AtlasUpdate::Image(ImageUpdate::Atlas { x: at[0], y: at[1], image: data }));
                    self.images.get_mut(&key).expect("刚查到").slot = Some(Slot::Atlas(at));
                }
                _ => repack.push(key),
            }
        }
        if !repack.is_empty() {
            self.repack(repack);
        }
    }

    fn create_texture(&mut self, key: Key, data: Arc<ImageData>) {
        self.next_texture += 1;
        let texture = self.next_texture;
        self.updates.push(AtlasUpdate::Image(ImageUpdate::Create { id: texture, image: data }));
        self.images.get_mut(&key).expect("刚查到").slot = Some(Slot::Texture(texture));
    }

    /// 图集放不下：把图集里还在用的图片和新来的一起重新装箱（先试当前大小，不行再翻倍），
    /// 最大的图集也放不下的改用单独的纹理
    fn repack(&mut self, new: Vec<Key>) {
        let mut keys: Vec<Key> = self.images.iter().filter(|(_, e)| matches!(e.slot, Some(Slot::Atlas(_)))).map(|(&k, _)| k).collect();
        keys.extend(new);
        let data: HashMap<Key, Arc<ImageData>> = keys.iter().map(|&k| (k, self.images[&k].data().expect("图集里的都解码好了"))).collect();
        // 高的先放，货架装得更满
        keys.sort_by_key(|k| (std::cmp::Reverse(data[k].storage()[1]), *k));
        let mut size = self.atlas_size.max(ATLAS_START);
        let placed = loop {
            let mut shelves = Shelves::new(size);
            let placed: Vec<Option<[u32; 2]>> = keys.iter().map(|k| shelves.allocate(data[k].storage()[0], data[k].storage()[1])).collect();
            if placed.iter().all(Option::is_some) || size >= max_atlas_size() {
                self.shelves = shelves;
                break placed;
            }
            size = (size * 2).min(max_atlas_size());
        };
        self.atlas_size = size;
        // 空出来的位置都作废了
        self.spare.clear();
        self.updates.push(AtlasUpdate::Image(ImageUpdate::ResetAtlas { size }));
        for (key, at) in keys.into_iter().zip(placed) {
            let image = data[&key].clone();
            match at {
                Some(at) => {
                    self.updates.push(AtlasUpdate::Image(ImageUpdate::Atlas { x: at[0], y: at[1], image }));
                    self.images.get_mut(&key).expect("刚查到").slot = Some(Slot::Atlas(at));
                }
                None => self.create_texture(key, image),
            }
        }
    }

    /// 自上次以来的 GPU 更新，交给渲染线程
    pub fn take_updates(&mut self) -> Vec<AtlasUpdate> {
        std::mem::take(&mut self.updates)
    }

    /// 完整内容，用于给新的 GPU 侧（离屏渲染）建立图集和纹理；动图是各元素此刻显示的那一帧
    pub fn snapshot(&self) -> Vec<AtlasUpdate> {
        let mut out = Vec::new();
        if self.atlas_size > 0 {
            out.push(AtlasUpdate::Image(ImageUpdate::ResetAtlas { size: self.atlas_size }));
        }
        for entry in self.images.values() {
            let (Some(slot), Some(data)) = (entry.slot, entry.data()) else { continue };
            out.push(AtlasUpdate::Image(match slot {
                Slot::Atlas([x, y]) => ImageUpdate::Atlas { x, y, image: data },
                Slot::Texture(id) => ImageUpdate::Create { id, image: data },
            }));
        }
        out
    }

    /// 生成元素的图片图元：内容区按 fit 和对齐算出图片的位置，四边形就是内容区（圆角裁剪按它算），
    /// 图片之外的部分在着色器里不画。有边框时在图片上面再画一次边框
    pub fn push(&self, block: &ImageBlock, area: &ImageArea, list: &mut DisplayList) {
        let Some(shown) = &block.shown else { return };
        let key = block.player.as_ref().map_or(Key::Image(shown.id()), |p| p.key);
        let Some(entry) = self.images.get(&key) else { return };
        let (Some(slot), Status::Ready { natural, data, .. }) = (entry.slot, shown.status()) else { return };
        let content = area.content;
        if content.width <= 0.0 || content.height <= 0.0 || natural[0] == 0 || natural[1] == 0 {
            return;
        }
        let [dx, dy, dw, dh] = fit_rect(block.fit, block.position, [natural[0] as f32, natural[1] as f32], [content.width, content.height]);
        let (bounds, kind) = self.texture_range(slot, &data);
        // 内容区四角对应的纹理坐标（可以超出图片的范围）
        let u = |x: f32| bounds[0] + (x - dx) / dw * (bounds[2] - bounds[0]);
        let v = |y: f32| bounds[1] + (y - dy) / dh * (bounds[3] - bounds[1]);

        let [cx, cy] = area.local.apply(content.center());
        let mut quad = Quad::new(cx - content.width * 0.5, cy - content.height * 0.5, content.width, content.height);
        quad.transform = area.local.linear;
        quad.radii = [area.inner_radius; 4];
        quad.uv = [u(0.0), v(0.0), u(content.width), v(content.height)];
        quad.shadow_offset = [bounds[0], bounds[1]];
        quad.shadow_blur = bounds[2];
        quad.border_width = bounds[3];
        quad.kind = kind;
        quad.opacity = area.opacity;
        if !block.tint.is_transparent() {
            quad.gradient = list.push_filter(&ColorMatrix::tint(block.tint));
        }
        if let Slot::Texture(texture) = slot {
            list.quad_effects.push(scene::QuadEffect::Image { quad: list.quads.len() as u32, texture });
        }
        // 广色域、HDR 的图：窗口要换成扩展范围的表面才显示得出来
        list.extended |= data.encoding != ImageEncoding::Srgb8;
        list.quads.push(quad);
        area.push_border(list);
    }

    /// 图片在纹理里的范围和图元的种类（动图的每一帧大小都一样）
    fn texture_range(&self, slot: Slot, data: &ImageData) -> ([f32; 4], u32) {
        let [sw, sh] = data.storage();
        match slot {
            Slot::Atlas([x, y]) => {
                let s = self.atlas_size as f32;
                ([x as f32 / s, y as f32 / s, (x + data.size[0]) as f32 / s, (y + data.size[1]) as f32 / s], Quad::IMAGE)
            }
            Slot::Texture(_) => ([0.0, 0.0, data.size[0] as f32 / sw as f32, data.size[1] as f32 / sh as f32], Quad::IMAGE_TEXTURE),
        }
    }
}

/// 生成图片图元需要的元素信息
pub(crate) struct ImageArea {
    /// 元素的范围和内容区（去掉内边距），布局坐标
    pub bounds: Rect,
    pub content: Rect,
    /// 到当前合成层的累积变换
    pub local: Affine,
    pub opacity: f32,
    pub radius: f32,
    /// 内容区的圆角：元素的圆角减去内边距
    pub inner_radius: f32,
    /// 边框宽度和颜色（纯色或渐变），画在图片上面
    pub border: Option<(f32, Fill)>,
}

/// 外部纹理（编号 `texture`，见 [`scene::EXTERNAL_TEXTURE_BASE`]）铺满内容区：和单独纹理的大图一样的图元，
/// 纹理的整张对应内容区。纹理还没登记时渲染器跳过这个图元
pub(crate) fn push_external(texture: u32, area: &ImageArea, list: &mut DisplayList) {
    let content = area.content;
    if content.width <= 0.0 || content.height <= 0.0 {
        return;
    }
    let [cx, cy] = area.local.apply(content.center());
    let mut quad = Quad::new(cx - content.width * 0.5, cy - content.height * 0.5, content.width, content.height);
    quad.transform = area.local.linear;
    quad.radii = [area.inner_radius; 4];
    quad.uv = [0.0, 0.0, 1.0, 1.0];
    // 图片在纹理里的范围（见 `texture_range`）：整张
    quad.shadow_offset = [0.0, 0.0];
    quad.shadow_blur = 1.0;
    quad.border_width = 1.0;
    quad.kind = Quad::IMAGE_TEXTURE;
    quad.opacity = area.opacity;
    list.quad_effects.push(scene::QuadEffect::Image { quad: list.quads.len() as u32, texture });
    list.quads.push(quad);
    area.push_border(list);
}

impl ImageArea {
    /// 边框画在图片上面（有边框时）
    fn push_border(&self, list: &mut DisplayList) {
        let Some((width, color)) = &self.border else { return };
        let b = self.bounds;
        let [cx, cy] = self.local.apply(b.center());
        let mut border = Quad::new(cx - b.width * 0.5, cy - b.height * 0.5, b.width, b.height)
            .radius(self.radius)
            .fill(Color::TRANSPARENT)
            .border(*width, color.color())
            .opacity(self.opacity);
        border.transform = self.local.linear;
        if let Fill::Gradient(gradient) = color {
            border = border.with_border_gradient(gradient.push_local(list, [b.width, b.height]));
        }
        list.quads.push(border);
    }
}

/// 图集版的图片：8 位的、只有图集那几级 mip、补齐过，且放得进图集
fn fits_atlas(data: &ImageData) -> bool {
    let [w, h] = data.storage();
    data.encoding == ImageEncoding::Srgb8 && data.levels.len() == IMAGE_ATLAS_LEVELS as usize && w.max(h) <= max_atlas_size() / 2
}

/// 按 fit 和对齐，图片在内容区里的位置和大小 `[x, y, w, h]`（相对内容区左上角）
pub(crate) fn fit_rect(fit: ImageFit, position: [f32; 2], natural: [f32; 2], area: [f32; 2]) -> [f32; 4] {
    let [iw, ih] = natural;
    let [w, h] = area;
    let contain = (w / iw).min(h / ih);
    let scale = match fit {
        ImageFit::Stretch => return [0.0, 0.0, w, h],
        ImageFit::Contain => contain,
        ImageFit::Cover => (w / iw).max(h / ih),
        ImageFit::None => 1.0,
        ImageFit::ScaleDown => contain.min(1.0),
    };
    let (dw, dh) = (iw * scale, ih * scale);
    [(w - dw) * position[0], (h - dh) * position[1], dw, dh]
}

/// 图片图集的货架装箱。所有尺寸都是对齐单位的倍数，位置自然对齐
#[derive(Default)]
struct Shelves {
    size: u32,
    /// 每层货架：y、高、下一个 x
    shelves: Vec<[u32; 3]>,
    next_y: u32,
}

impl Shelves {
    fn new(size: u32) -> Self {
        Self { size, shelves: Vec::new(), next_y: 0 }
    }

    fn allocate(&mut self, width: u32, height: u32) -> Option<[u32; 2]> {
        if width > self.size || height > self.size {
            return None;
        }
        // 高度合适（不浪费超过一半）且还放得下的货架
        let size = self.size;
        if let Some(shelf) = self.shelves.iter_mut().find(|[_, h, x]| *h >= height && *h <= height + height / 2 && x + width <= size) {
            let x = shelf[2];
            shelf[2] += width;
            return Some([x, shelf[0]]);
        }
        if self.next_y + height > self.size {
            return None;
        }
        let y = self.next_y;
        self.next_y += height;
        self.shelves.push([y, height, width]);
        Some([0, y])
    }
}
