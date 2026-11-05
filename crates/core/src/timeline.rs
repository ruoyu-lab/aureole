//! 关键帧时间线：把几个元素、几个属性的多步动画编排在一起，用代码控制播放。
//!
//! - [`Keyframes`]：一个属性的一串关键帧（时刻、值、到达它的缓动或弹簧）
//! - [`MotionHandle`]：时间线要动的元素，用 [`crate::Element::motion_handle`] 交给一个或多个元素
//! - [`Motion`]：时间线上的一段（动画段）：`handle.animate(属性, 关键帧)`，或者用 [`sequence`]、[`parallel`] 组合；
//!   可以延迟、按元素错开、重复、往返
//! - [`Timeline`]：播放、暂停、倒放、跳转、重新开始、停止，读进度，播完时调用处理器或等待
//!
//! 时间线的定义和播放头在这里（主线程的登记表）。播放头锚在动画时钟上（[`Playhead`]：锚点的时刻、位置、方向），
//! 位置是时刻的纯函数，命令在发出的那一刻重新定锚；每个窗口的元素树按它排期、写属性值，
//! 只动合成层变换和透明度的轨道连同播放头交给渲染线程，两边按同一个时刻采样（见 `tree/timeline.rs`、`compositor.rs`）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use animation::{Easing, KeyCurve, KeyframeSeries, MAX_COMPONENTS, Playhead, Spring, Timing};
use reactive::{Signal, on_cleanup, signal};

use crate::property::{PropId, Property, PropertyValue};

/// 一个属性的一串关键帧：从第一帧的值出发，在给定的时刻（从这一段开始算）到达每一帧的值。
/// 每一段默认按 [`Easing::EASE`] 走，[`Keyframes::easing`]、[`Keyframes::spring`] 改刚加的那一段。
///
/// ```
/// # use aureole::prelude::*;
/// // 弹一下：120 毫秒先快后慢放大到 1.12，再用不回弹的弹簧在 420 毫秒时回到 1
/// let pop = Keyframes::new(1.0).at(ms(120), 1.12).easing(Easing::EASE_OUT).at(ms(420), 1.0).spring(Spring::smooth());
/// // 颜色也行（在预乘的 OKLab 里插值）
/// let flash = Keyframes::new(Color::hex(0x138a84)).at(ms(150), Color::WHITE).at(ms(400), Color::hex(0x138a84));
/// ```
#[derive(Clone, Debug)]
pub struct Keyframes {
    start: PropertyValue,
    keys: Vec<(Duration, PropertyValue, KeyCurve)>,
}

impl Keyframes {
    /// 第一帧（时刻 0）的值
    pub fn new(start: impl Into<PropertyValue>) -> Self {
        Self { start: start.into(), keys: Vec::new() }
    }

    /// 在 `time` 时到达 `value`，这一段默认按 [`Easing::EASE`] 走
    pub fn at(mut self, time: Duration, value: impl Into<PropertyValue>) -> Self {
        self.keys.push((time, value.into(), KeyCurve::Easing(Easing::EASE)));
        self
    }

    /// 刚加的那一段（到达上一个 [`Keyframes::at`] 的值）用这条缓动曲线
    pub fn easing(mut self, easing: Easing) -> Self {
        self.last_segment().2 = KeyCurve::Easing(easing);
        self
    }

    /// 刚加的那一段用弹簧：带着上一段结束时的速度奔向这一帧的值，到这一帧的时刻正好到达。
    /// 时长最好不短于弹簧自己停稳的时间（临界阻尼约响应时间的 1.2 倍），否则会被收得急一些
    pub fn spring(mut self, spring: Spring) -> Self {
        self.last_segment().2 = KeyCurve::Spring(spring);
        self
    }

    /// 最后一帧的时刻
    pub fn duration(&self) -> Duration {
        self.keys.iter().map(|k| k.0).max().unwrap_or_default()
    }

    fn last_segment(&mut self) -> &mut (Duration, PropertyValue, KeyCurve) {
        self.keys.last_mut().expect("先用 at 加一帧，再设这一段的曲线")
    }

    /// 换成某个属性的分量。值的种类不对（比如给透明度一个颜色）或者用了渐变、内阴影、几层投影时 panic
    fn compile(&self, property: PropId) -> KeyframeSeries {
        let check = |value: &PropertyValue| {
            assert!(
                property.accepts(value) && value.is_simple(),
                "{property:?} 的关键帧不接受 {value:?}（关键帧只支持数值、纯色和一层外投影）"
            );
            let mut components = [0.0; MAX_COMPONENTS];
            let len = value.to_components(&mut components);
            (components, len)
        };
        let (components, len) = check(&self.start);
        let mut track = KeyframeSeries::new(&components[..len]);
        for (time, value, curve) in &self.keys {
            let (components, len) = check(value);
            track.push(time.as_secs_f64() as f32, &components[..len], *curve);
        }
        track
    }
}

/// 时间线要动的元素。和信号一样是 `Copy` 的：先创建，用 [`crate::Element::motion_handle`] 交给一个或多个元素
/// （比如列表的每一行），再用 [`MotionHandle::animate`] 写它们的关键帧。交给多个元素时一起动，
/// [`Motion::stagger`] 按它们在界面里的先后错开
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MotionHandle(pub(crate) u64);

impl Default for MotionHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl MotionHandle {
    /// 新句柄
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// 时间线上的一段：这些元素的 `property` 按 `keyframes` 变。值的种类和属性不符时 panic
    pub fn animate(&self, property: Property, keyframes: Keyframes) -> Motion {
        let property = property.id();
        let track = Arc::new(keyframes.compile(property));
        Motion::new(MotionKind::Track { handle: self.0, property, track })
    }
}

/// 时间线上的一段：一组元素的一个属性的关键帧（[`MotionHandle::animate`]），或者几段的组合（[`sequence`]、[`parallel`]）。
/// 可以延迟、按元素错开、重复几轮、往返
#[derive(Clone, Debug)]
pub struct Motion {
    kind: MotionKind,
    delay: f64,
    stagger: f64,
    iterations: f64,
    alternate: bool,
}

#[derive(Clone, Debug)]
enum MotionKind {
    Track { handle: u64, property: PropId, track: Arc<KeyframeSeries> },
    Sequence(Vec<Motion>),
    Parallel(Vec<Motion>),
}

/// 一个接一个地放：后一段等前一段（包括它的延迟、错开、重复）放完才开始
pub fn sequence(motions: impl IntoIterator<Item = Motion>) -> Motion {
    Motion::new(MotionKind::Sequence(motions.into_iter().collect()))
}

/// 同时放：整段的时长是最长的那一段
pub fn parallel(motions: impl IntoIterator<Item = Motion>) -> Motion {
    Motion::new(MotionKind::Parallel(motions.into_iter().collect()))
}

impl Motion {
    fn new(kind: MotionKind) -> Self {
        Self { kind, delay: 0.0, stagger: 0.0, iterations: 1.0, alternate: false }
    }

    /// 晚 `delay` 开始（在序列里是接着上一段之后再等这么久）
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay.as_secs_f64();
        self
    }

    /// 按元素错开：句柄交给了多个元素时，第 i 个（按界面里的先后）晚 `i × stagger` 开始。
    /// 对组合用时，组合里每个句柄的元素各自按序号错开
    pub fn stagger(mut self, stagger: Duration) -> Self {
        self.stagger = stagger.as_secs_f64();
        self
    }

    /// 一共放 `n` 轮（至少 1）
    pub fn times(mut self, n: u32) -> Self {
        self.iterations = n.max(1) as f64;
        self
    }

    /// 一直循环
    pub fn forever(mut self) -> Self {
        self.iterations = f64::INFINITY;
        self
    }

    /// 往返：第 2、4……轮倒着放（配合 [`Motion::times`]、[`Motion::forever`]）
    pub fn alternate(mut self, alternate: bool) -> Self {
        self.alternate = alternate;
        self
    }

    /// 一轮的时长（不含延迟和错开）
    fn period(&self, counts: &dyn Fn(u64) -> usize) -> f64 {
        match &self.kind {
            MotionKind::Track { track, .. } => track.duration() as f64,
            MotionKind::Sequence(motions) => motions.iter().map(|c| c.measure(counts)).sum(),
            MotionKind::Parallel(motions) => motions.iter().map(|c| c.measure(counts)).fold(0.0, f64::max),
        }
    }

    /// 从开始到最后一个元素放完：延迟 + 错开 + 每轮时长 × 轮数
    fn measure(&self, counts: &dyn Fn(u64) -> usize) -> f64 {
        let period = self.period(counts);
        let total = if self.iterations.is_infinite() { f64::INFINITY } else { period * self.iterations };
        self.delay + self.last_index(counts) * self.stagger + total
    }

    /// 里面各个句柄最后一个元素的序号的最大值
    fn last_index(&self, counts: &dyn Fn(u64) -> usize) -> f64 {
        match &self.kind {
            MotionKind::Track { handle, .. } => counts(*handle).saturating_sub(1) as f64,
            MotionKind::Sequence(motions) | MotionKind::Parallel(motions) => motions.iter().map(|c| c.last_index(counts)).fold(0.0, f64::max),
        }
    }

    fn handles(&self, out: &mut Vec<u64>) {
        match &self.kind {
            MotionKind::Track { handle, .. } => {
                if !out.contains(handle) {
                    out.push(*handle);
                }
            }
            MotionKind::Sequence(motions) | MotionKind::Parallel(motions) => motions.iter().for_each(|c| c.handles(out)),
        }
    }

    /// 排期：每条轨道从根到自己的各层时间
    fn place(&self, start: f64, chain: &mut Vec<Timing>, counts: &dyn Fn(u64) -> usize, out: &mut Vec<Placed>) {
        let period = self.period(counts);
        chain.push(Timing { start: start + self.delay, stagger: self.stagger, period, iterations: self.iterations, alternate: self.alternate });
        match &self.kind {
            MotionKind::Track { handle, property, track } => out.push(Placed {
                handle: *handle,
                property: *property,
                track: track.clone(),
                levels: chain.as_slice().into(),
            }),
            MotionKind::Sequence(motions) => {
                let mut offset = 0.0;
                for motion in motions {
                    motion.place(offset, chain, counts, out);
                    offset += motion.measure(counts);
                }
            }
            MotionKind::Parallel(motions) => motions.iter().for_each(|motion| motion.place(0.0, chain, counts, out)),
        }
        chain.pop();
    }
}

/// 排好期的一条轨道（还没落到具体元素上）
#[derive(Clone, Debug)]
pub(crate) struct Placed {
    pub handle: u64,
    pub property: PropId,
    pub track: Arc<KeyframeSeries>,
    pub levels: Arc<[Timing]>,
}

/// 减少动态效果时去掉的属性：移动、缩放、旋转、尺寸、描线；透明度、颜色、模糊等照常
pub(crate) fn is_movement(property: PropId) -> bool {
    matches!(
        property,
        PropId::TRANSLATION_X
            | PropId::TRANSLATION_Y
            | PropId::SCALE
            | PropId::ROTATION
            | PropId::ROTATION_X
            | PropId::ROTATION_Y
            | PropId::PERSPECTIVE
            | PropId::WIDTH
            | PropId::HEIGHT
            | PropId::PADDING
            | PropId::GAP
            | PropId::STROKE_START
            | PropId::STROKE_END
    )
}

/// 登记表里的一条时间线
pub(crate) struct Entry {
    pub root: Motion,
    /// 用到的句柄
    pub handles: Vec<u64>,
    /// 播放头，锚在动画时钟上
    pub playhead: Playhead,
    /// 每次命令加一：窗口在下一帧把新状态交给渲染线程
    pub version: u64,
    /// 每次从头开始（播放、重新开始、跳转一条停着的时间线）加一：窗口重新排期
    pub generation: u64,
    /// 在用：开始之后、停止之前。停着的时间线不动任何属性
    pub active: bool,
    /// 倒放从结尾开始，结尾在哪要等排好期才知道
    pub from_end: bool,
    /// 这次开始以后排好期了：[`Playhead::duration`] 是真的总时长，不是估计的
    pub scheduled: bool,
    /// 排好期之前的跳转：落点等排好期再算
    pub pending_seek: Option<PendingSeek>,
    /// 开始时系统开着"减少动态效果"：移动、缩放直接到最后的值，淡入淡出照常
    pub reduced: bool,
    /// 推进播放头的窗口（元素树的编号）；其他窗口只读
    pub driver: Option<u64>,
    on_finish: Vec<Rc<dyn Fn()>>,
    wakers: Vec<Waker>,
    /// 放完的次数（等待放完的 future 用）
    finishes: u64,
    position: Signal<f64>,
    playing: Signal<bool>,
}

/// 排好期之前的跳转。总时长取决于句柄有几个元素（错开），要等窗口排好期才知道，排好期之前不能按它夹、按它算比例
#[derive(Clone, Copy, Debug)]
pub(crate) enum PendingSeek {
    /// 跳到这个时刻，秒
    Time(f64),
    /// 跳到这个进度，0..=1
    Progress(f64),
}

impl Entry {
    /// 从头开始：重新排期，属性交给这条时间线
    fn start(&mut self) {
        self.generation += 1;
        self.active = true;
        self.from_end = false;
        self.scheduled = false;
        self.pending_seek = None;
        self.playhead.position = 0.0;
        self.reduced = reactive::untracked(crate::system::is_motion_reduced);
    }

    /// 跳转：排好期了马上落到新位置，还没有就记下来（[`Entry::scheduled_with`]）
    fn seek(&mut self, seek: PendingSeek) {
        if !self.active {
            self.start();
        }
        self.from_end = false;
        // 没排好期时，窗口排期之前读到的位置是按估计的时长算的
        self.playhead.position = seek.position(self.playhead.duration);
        self.pending_seek = (!self.scheduled).then_some(seek);
    }

    /// 窗口排好了期：换上真的总时长，还没落的跳转现在落。返回有没有落
    pub fn scheduled_with(&mut self, duration: f64) -> bool {
        self.scheduled = true;
        self.playhead.duration = duration;
        let seek = self.pending_seek.take();
        if let Some(seek) = seek {
            self.playhead.position = seek.position(duration);
        }
        seek.is_some()
    }
}

impl PendingSeek {
    /// 总时长是 `duration` 时的落点。一直循环的时间线没有进度，跳到开头
    fn position(self, duration: f64) -> f64 {
        match self {
            Self::Time(seconds) => seconds.clamp(0.0, duration.max(0.0)),
            Self::Progress(progress) if duration.is_finite() => progress * duration,
            Self::Progress(_) => 0.0,
        }
    }
}

#[derive(Default)]
struct Registry {
    entries: HashMap<u64, Entry>,
    next: u64,
    /// 任何时间线有变化（命令、新建、释放）就加一，窗口据此判断要不要逐个检查
    changes: u64,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::default();
}

/// 改登记表（窗口的元素树每帧调用；里面不能执行用户代码）
pub(crate) fn with_registry<R>(f: impl FnOnce(&mut HashMap<u64, Entry>, u64) -> R) -> R {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let changes = r.changes;
        f(&mut r.entries, changes)
    })
}

/// 时间线放完了（或者被释放了）之后要做的事：处理器、唤醒等待的任务、写回进度信号。
/// 在元素树的借用之外执行
#[derive(Default)]
pub(crate) struct Events {
    pub callbacks: Vec<Rc<dyn Fn()>>,
    pub wakers: Vec<Waker>,
    pub signals: Vec<(Signal<f64>, f64, Signal<bool>, bool)>,
}

impl Events {
    pub fn is_empty(&self) -> bool {
        self.callbacks.is_empty() && self.wakers.is_empty() && self.signals.is_empty()
    }

    /// 时间线 `entry` 这一步走到了头
    pub fn finished(&mut self, entry: &mut Entry) {
        entry.finishes += 1;
        self.callbacks.extend(entry.on_finish.iter().cloned());
        self.wakers.append(&mut entry.wakers);
    }

    /// 播放头走到了 `position`：把位置和是否在走写回信号
    pub fn report(&mut self, entry: &Entry, position: f64) {
        self.signals.push((entry.position, position, entry.playing, entry.playhead.playing));
    }

    pub fn run(self) {
        reactive::batch(|| {
            for (position, at, playing, on) in self.signals {
                position.set_if_changed(at);
                playing.set_if_changed(on);
            }
        });
        for waker in self.wakers {
            waker.wake();
        }
        if !self.callbacks.is_empty() {
            reactive::batch(|| self.callbacks.iter().for_each(|f| f()));
        }
    }
}

impl Entry {
    /// 按这个窗口里各句柄的元素数排期，返回每条轨道和总时长
    pub fn schedule(&self, counts: &dyn Fn(u64) -> usize) -> (Vec<Placed>, f64) {
        let mut placed = Vec::new();
        self.root.place(0.0, &mut Vec::new(), counts, &mut placed);
        (placed, self.root.measure(counts))
    }
}

/// 关键帧时间线：几个元素、几个属性的多步动画编在一起，用代码控制。
/// 和信号一样是 `Copy` 的，归创建它的作用域所有，作用域释放时停止（属性回到绑定的值）。
///
/// ```
/// # use aureole::prelude::*;
/// let rows = MotionHandle::new();
/// let intro = Timeline::new(parallel([
///     rows.animate(Property::Opacity, Keyframes::new(0.0).at(ms(180), 1.0)),
///     rows.animate(Property::TranslationY, Keyframes::new(12.0).at(ms(320), 0.0).spring(Spring::smooth())),
/// ]).stagger(ms(40))); // 每行晚 40 毫秒
/// column()
///     .on_mount(move || intro.play())
///     .children((0..5).map(move |i| text(format!("第 {i} 行")).motion_handle(rows)));
/// ```
///
/// **和 [`crate::Element::transition`] 一起用**：时间线开始后接管它动的属性，元素显示时间线算出的值；
/// 放完后停在最后的值。属性绑定的值在这期间变了（比如悬停），这个属性立刻交还给绑定，按声明的过渡
/// 从此刻的值和速度接着走，时间线之后不再动它（重新开始时再接管）。[`Timeline::stop`] 把所有属性交还给绑定，
/// 同样按声明的过渡回去。同一个属性同一时刻只听一个来源：后开始的时间线接管前一条在动的属性
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeline {
    id: u64,
    position: Signal<f64>,
    playing: Signal<bool>,
}

impl Timeline {
    /// 创建，先不播放。归当前作用域所有
    pub fn new(motion: Motion) -> Self {
        let (position, playing) = (signal(0.0), signal(false));
        let mut handles = Vec::new();
        motion.handles(&mut handles);
        // 还不知道各句柄有几个元素：先按一个估计总时长，窗口排好期时换成真的（[`Entry::scheduled_with`]）
        let duration = motion.measure(&|_| 1);
        let id = REGISTRY.with(|r| {
            let mut r = r.borrow_mut();
            r.next += 1;
            r.changes += 1;
            let id = r.next;
            let entry = Entry {
                root: motion,
                handles,
                playhead: Playhead { duration, ..Default::default() },
                version: 0,
                generation: 0,
                active: false,
                from_end: false,
                scheduled: false,
                pending_seek: None,
                reduced: false,
                driver: None,
                on_finish: Vec::new(),
                wakers: Vec::new(),
                finishes: 0,
                position,
                playing,
            };
            r.entries.insert(id, entry);
            id
        });
        if reactive::Scope::current().is_some() {
            on_cleanup(move || {
                // 作用域可能在线程退出时才释放（放在别的线程局部量里的），登记表可能已经析构
                let _ = REGISTRY.try_with(|r| {
                    let mut r = r.borrow_mut();
                    r.changes += 1;
                    if let Some(entry) = r.entries.remove(&id) {
                        entry.wakers.into_iter().for_each(Waker::wake);
                    }
                });
            });
        }
        Self { id, position, playing }
    }

    /// 发一个命令：播放头先在此刻（动画时钟的时间）重新定锚，命令从这一刻起生效
    fn command(&self, f: impl FnOnce(&mut Entry)) {
        let now = crate::clock::now();
        let report = REGISTRY.with(|r| {
            let mut r = r.borrow_mut();
            r.changes += 1;
            let entry = r.entries.get_mut(&self.id)?;
            entry.playhead.reanchor(now);
            f(entry);
            entry.version += 1;
            Some((entry.playhead.position, entry.playhead.playing))
        });
        if let Some((position, playing)) = report {
            self.position.set_if_changed(position);
            self.playing.set_if_changed(playing);
        }
    }

    /// 正着放：从此刻的位置接着走；已经在结尾时从头开始。停着的时间线从头开始，接管它动的属性
    pub fn play(&self) {
        self.command(|e| {
            if !e.active {
                e.start();
            } else if e.scheduled && e.playhead.position >= e.playhead.duration {
                e.playhead.position = 0.0;
            } else if matches!(e.pending_seek, Some(PendingSeek::Progress(progress)) if progress >= 1.0) {
                // 刚跳到结尾、还没排期：结尾在哪都一样，从头开始
                e.pending_seek = None;
                e.playhead.position = 0.0;
            }
            e.playhead.reversed = false;
            e.playhead.playing = true;
        });
    }

    /// 倒着放：从此刻的位置往回走；已经在开头时从结尾开始。一直循环的时间线不能从结尾倒放
    pub fn reverse(&self) {
        self.command(|e| {
            if !e.active {
                e.start();
                e.from_end = true;
            } else if e.playhead.position <= 0.0 {
                e.from_end = true;
                e.pending_seek = None;
                e.playhead.position = e.playhead.duration;
            }
            e.playhead.reversed = true;
            e.playhead.playing = e.playhead.duration.is_finite();
        });
    }

    /// 暂停在此刻的位置
    pub fn pause(&self) {
        self.command(|e| e.playhead.playing = false);
    }

    /// 从头正着放（不管现在在哪）
    pub fn restart(&self) {
        self.command(|e| {
            e.start();
            e.playhead.reversed = false;
            e.playhead.playing = true;
        });
    }

    /// 停止：所有属性交还给绑定（声明了过渡的按过渡回去），回到开头
    pub fn stop(&self) {
        self.command(|e| {
            e.active = false;
            e.from_end = false;
            e.pending_seek = None;
            e.playhead.playing = false;
            e.playhead.position = 0.0;
        });
    }

    /// 跳到 `time` 处，播放状态不变。停着的时间线先接管属性（停在那一刻，可以拿来拖动预览）。
    /// 还没排过期的时间线（总时长要等窗口的下一帧才知道，见 [`Timeline::duration`]）在排好期时落到这里
    pub fn seek(&self, time: Duration) {
        // 关键帧的时刻存成 `f32` 的秒：跳到某一帧的时刻时正好落在它上面
        let seconds = f64::from(time.as_secs_f64() as f32);
        self.command(|e| e.seek(PendingSeek::Time(seconds)));
    }

    /// 跳到进度 `progress`（0..=1）处，见 [`Timeline::seek`]。一直循环的时间线没有进度，跳到开头
    pub fn seek_progress(&self, progress: f32) {
        let progress = f64::from(progress.clamp(0.0, 1.0));
        self.command(|e| e.seek(PendingSeek::Progress(progress)));
    }

    /// 总时长；一直循环时是 `None`。错开的时长取决于句柄有几个元素，按开始播放时算：
    /// 窗口排期（开始以后的下一帧）之前是估计的，按每个句柄一个元素算
    pub fn duration(&self) -> Option<Duration> {
        let seconds = self.duration_seconds();
        seconds.is_finite().then(|| rounded(seconds))
    }

    fn duration_seconds(&self) -> f64 {
        REGISTRY.with(|r| r.borrow().entries.get(&self.id).map_or(0.0, |e| e.playhead.duration))
    }

    /// 播放头的位置。在绑定里读会订阅，播放时每帧更新
    pub fn position(&self) -> Duration {
        rounded(self.position.get())
    }

    /// 播放进度 0..=1（一直循环的时间线是 0）。在绑定里读会订阅，播放时每帧更新
    pub fn progress(&self) -> f32 {
        let position = self.position.get();
        let duration = self.duration_seconds();
        if duration.is_finite() && duration > 0.0 { (position / duration).clamp(0.0, 1.0) as f32 } else { 0.0 }
    }

    /// 是否在走，响应式
    pub fn is_playing(&self) -> bool {
        self.playing.get()
    }

    /// 每次正放到结尾、倒放到开头时调用（暂停、停止、一直循环的不算）。处理器在创建它时的作用域里执行
    pub fn on_finish<R>(&self, f: impl Fn() -> R + 'static) {
        let f = crate::handlers::owned(move |()| f());
        REGISTRY.with(|r| {
            if let Some(entry) = r.borrow_mut().entries.get_mut(&self.id) {
                entry.on_finish.push(Rc::new(move || f(())));
            }
        });
    }

    /// 等这一轮放完：下一次正放到结尾或倒放到开头时完成（时间线被释放时也完成）。
    /// 配合 [`crate::spawn_local`] 写成先后顺序：
    ///
    /// ```no_run
    /// # use aureole::prelude::*;
    /// # let intro = Timeline::new(MotionHandle::new().animate(Property::Opacity, Keyframes::new(0.0).at(ms(200), 1.0)));
    /// aureole::spawn_local(async move {
    ///     intro.play();
    ///     intro.finished().await;
    ///     println!("放完了");
    /// });
    /// ```
    pub fn finished(&self) -> Finished {
        let after = REGISTRY.with(|r| r.borrow().entries.get(&self.id).map_or(0, |e| e.finishes));
        Finished { id: self.id, after }
    }
}

/// 播放头算的是 `f32` 累出来的秒：换成时长时取到微秒，读到的是写下的那个数（300 毫秒的段是 300 毫秒）
fn rounded(seconds: f64) -> Duration {
    Duration::from_micros((seconds * 1e6).round().max(0.0) as u64)
}

/// [`Timeline::finished`] 返回的 future
pub struct Finished {
    id: u64,
    after: u64,
}

impl Future for Finished {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        REGISTRY.with(|r| match r.borrow_mut().entries.get_mut(&self.id) {
            None => Poll::Ready(()),
            Some(entry) if entry.finishes > self.after => Poll::Ready(()),
            Some(entry) => {
                entry.wakers.push(cx.waker().clone());
                Poll::Pending
            }
        })
    }
}
