//! 按时间求值的轨道（[`Track`]）和时钟（[`Clock`]）：框架里所有动画的推进方式。
//!
//! 轨道的值是时钟时刻的纯函数：给一个时刻就算出那一刻的值，不逐帧累加。所以掉帧、卡顿、跳转、两条线程各算一份，
//! 结果都一样。改目标（[`Track::retarget`]）只是换一个锚点：从那一刻的位置（弹簧还有速度）接着走，运动连续。
//!
//! - [`CurveTrack`]：弹簧（[`SpringTrack`]）或补间（[`TweenTrack`]），声明的过渡、退场、合成层动画、共享元素的飞行用它
//! - [`KeyframePlayback`]：时间线上一个值的几段关键帧（[`KeyframePlacement`]）按播放头（[`crate::Playhead`]）放
//! - [`Clock`]：宿主时间换算成动画时间，可以暂停、慢放、拖动；主线程和渲染线程各持一份同样的

use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::sync::Arc;

use crate::MAX_COMPONENTS;
use crate::spring::SpringStep;
use crate::curve::{Curve, Repeat, Spring, Tween, seconds};
use crate::keyframes::{KeyframeSeries, Playhead, Timing, local_time, local_time_rate};

type Components = [f32; MAX_COMPONENTS];

/// 离结尾不到这么久（1 微秒）就算到了：帧的时刻是一帧帧累加出来的浮点数，补间、关键帧的时长又是 `f32`，
/// 正好落在结尾的那一帧常常差一点点没到；不算到的话要多出一帧才停
pub(crate) const EPSILON: f64 = 1e-6;

/// 采样那一刻轨道在不在动
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackState {
    /// 还在动：之后的值还会变，要接着出帧
    Moving,
    /// 停在终点：从这一刻起值不再变（直到下一次改目标），不用再出帧
    Settled,
}

impl TrackState {
    /// 停在终点了
    pub fn is_settled(self) -> bool {
        self == Self::Settled
    }
}

/// 轨道用的曲线
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TrackKind {
    /// 弹簧（[`SpringTrack`]）
    Spring(Spring),
    /// 补间（[`TweenTrack`]）
    Tween(Tween),
    /// 一串关键帧（[`KeyframePlayback`]）
    Keyframes {
        /// 关键帧数，不算时刻 0 的起始值
        keys: usize,
        /// 从第一帧到最后一帧的秒数
        duration: f32,
    },
}

/// 轨道的概况（[`Track::describe`]）：调试工具显示它，调度帧时看它什么时候停
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackInfo {
    /// 曲线
    pub kind: TrackKind,
    /// 分量数
    pub len: usize,
    /// 这一段从时钟的哪一刻开始：最近一次改目标的时刻，关键帧是开始播放的时刻；没在动的轨道为 `None`
    pub start: Option<f64>,
    /// 停下的时刻：补间、关键帧是确定的，弹簧按衰减的包络算（不早于真正停稳）；一直循环的补间、不衰减的弹簧为 `None`
    pub end: Option<f64>,
    /// 终点，前 [`TrackInfo::len`] 个分量有效
    pub target: [f32; MAX_COMPONENTS],
}

impl TrackInfo {
    /// 终点的有效分量
    pub fn target(&self) -> &[f32] {
        &self.target[..self.len]
    }
}

/// 按时间求值的轨道：值是时钟时刻的纯函数。
///
/// 时刻是 [`Clock::now`] 给的秒数（`f64`：应用开几天也不丢精度）。早于这一段开始的时刻按开始那一刻算
/// （改目标之前的轨迹不保存）。
///
/// ```
/// use aureole_animation::{TrackState, Spring, SpringTrack, Track};
///
/// let mut x = SpringTrack::new(Spring::snappy(), &[0.0], 0.01);
/// x.retarget(10.0, &[100.0]); // 时钟的第 10 秒改目标
/// let mut out = [0.0];
/// assert_eq!(x.sample(10.0, &mut out), TrackState::Moving);
/// assert_eq!(out, [0.0]);
/// x.sample(10.1, &mut out); // 不用逐帧推进，直接问任意时刻
/// assert!(out[0] > 30.0 && out[0] < 100.0);
/// assert_eq!(x.sample(20.0, &mut out), TrackState::Settled);
/// assert_eq!(out, [100.0]);
/// ```
pub trait Track {
    /// 时刻 `t` 的值写进 `out`（长度不小于分量数），返回那一刻在不在动
    fn sample(&self, t: f64, out: &mut [f32]) -> TrackState;

    /// 从时刻 `t` 起改去 `target`（长度等于分量数）：位置连续，弹簧的速度也连续
    fn retarget(&mut self, t: f64, target: &[f32]);

    /// 概况
    fn describe(&self) -> TrackInfo;
}

fn components(values: &[f32]) -> Components {
    assert!(values.len() <= MAX_COMPONENTS, "轨道的值最多 {MAX_COMPONENTS} 个分量");
    let mut out = [0.0; MAX_COMPONENTS];
    out[..values.len()].copy_from_slice(values);
    out
}

// ---- 弹簧 ----

/// 弹簧的模态：角频率和阻尼比（和 `SpringStep` 的取法一致：响应时间下限 1e-4 秒，阻尼比不小于 0，离 1 很近算临界）
#[derive(Clone, Copy, Debug, PartialEq)]
enum Modes {
    /// `delta`：位移、速度的绝对值的峰在相位 −δ（模 π）处，δ = atan(ζω / ω_d)；`peak` 是峰上 |cos| 的值 cos δ 的对数
    Under { zeta_omega: f32, omega_d: f32, delta: f32, peak: f32 },
    Critical { omega: f32 },
    Over { r1: f32, r2: f32 },
}

impl Modes {
    fn of(spring: Spring) -> Self {
        let omega = TAU / f64::from(seconds(spring.response).max(1e-4));
        let zeta = f64::from(spring.damping_ratio.max(0.0));
        // 系数按 f32 存：采样本来就用 f32 算，轨道小一点（一万个元素各有一两条）
        if (zeta - 1.0).abs() < 1e-4 {
            Self::Critical { omega: omega as f32 }
        } else if zeta < 1.0 {
            let (zeta_omega, omega_d) = (zeta * omega, omega * (1.0 - zeta * zeta).sqrt());
            let delta = (zeta_omega / omega_d).atan();
            Self::Under { zeta_omega: zeta_omega as f32, omega_d: omega_d as f32, delta: delta as f32, peak: delta.cos().ln() as f32 }
        } else {
            let root = (zeta * zeta - 1.0).sqrt();
            Self::Over { r1: (-omega * (zeta - root)) as f32, r2: (-omega * (zeta + root)) as f32 }
        }
    }

    /// 经过 τ 秒的系数：和逐帧推进的 `SpringStep` 同一组公式、同样用 `f32`（每帧每个弹簧都要算一次，
    /// `f32` 的指数、三角函数快得多；锚点的时刻和停稳的时刻仍是 `f64`）
    #[inline]
    fn step(self, tau: f32) -> SpringStep {
        match self {
            Self::Under { zeta_omega, omega_d, .. } => {
                let (sin, cos) = (omega_d * tau).sin_cos();
                SpringStep::Under { decay: (-zeta_omega * tau).exp(), cos, sin, zeta_omega, omega_d }
            }
            Self::Critical { omega } => SpringStep::Critical { decay: (-omega * tau).exp(), omega, dt: tau },
            Self::Over { r1, r2 } => SpringStep::Over { r1, r2, e1: (r1 * tau).exp(), e2: (r2 * tau).exp() },
        }
    }

    /// 停稳的判断窗口（相对锚点的秒数）`(quiet, calm)`：各分量从 (x0, v0)（相对目标的位移、速度）出发，
    /// `quiet` 之前一定还在动；`calm` 之后一定停了（|x| 都不超过 `tolerance`、|v| 都不超过它的十倍，并且之后一直如此）；
    /// 两者之间每个分量的 |x|、|v| 都只会越来越小（或者已经越过了最后一个超过容差的峰），按那一刻的值判断就是准的。
    ///
    /// 改目标时只算这两个界，都是几个对数、指数的闭式：一万个弹簧同时改目标也不贵。真正停稳的那一刻在采样时顺带判断出来，
    /// 是时刻的纯函数，两条线程同一刻停
    fn window(self, start: impl Iterator<Item = (f64, f64)>, tolerance: f64) -> (f64, f64) {
        let (lx, lv) = (tolerance, tolerance * 10.0);
        let (mut quiet, mut calm) = (0.0f64, 0.0f64);
        // 一个分量从 (x0, v0) 出发的位移、速度两个窗口
        let mut widen = |x0: f64, v0: f64| {
            let windows = match self {
                Self::Under { zeta_omega, omega_d, delta, peak } => {
                    let (zeta_omega, omega_d) = (f64::from(zeta_omega), f64::from(omega_d));
                    let shape = (zeta_omega, omega_d, f64::from(delta), f64::from(peak));
                    // 位移 e^{-ζωτ}(x0 cos + b sin)，速度 e^{-ζωτ}(v0 cos + d sin)
                    let b = (v0 + zeta_omega * x0) / omega_d;
                    let d = -(x0 * omega_d + zeta_omega * b);
                    [oscillation_window(x0, b, shape, lx), oscillation_window(v0, d, shape, lv)]
                }
                Self::Critical { omega } => {
                    // 位移 e^{-ωτ}(x0 + cτ)，速度 e^{-ωτ}(v0 − ωcτ)，c = v0 + ω x0
                    let omega = f64::from(omega);
                    let c = v0 + omega * x0;
                    [linear_window(x0, c, omega, lx), linear_window(v0, -omega * c, omega, lv)]
                }
                Self::Over { r1, r2 } => {
                    // 位移 c1 e^{r1 τ} + c2 e^{r2 τ}，速度 c1 r1 e^{r1 τ} + c2 r2 e^{r2 τ}
                    let (r1, r2) = (f64::from(r1), f64::from(r2));
                    let c2 = (v0 - r1 * x0) / (r2 - r1);
                    let c1 = x0 - c2;
                    [exponential_window(c1, c2, r1, r2, lx), exponential_window(c1 * r1, c2 * r2, r1, r2, lv)]
                }
            };
            for (q, c) in windows {
                quiet = quiet.max(q);
                calm = calm.max(c);
            }
        };
        // 从静止出发的分量（第一次改目标、布局位置的平移，最常见）位移、速度都和起点成正比：只看离目标最远的那一个
        let mut farthest = 0.0f64;
        for (x0, v0) in start {
            if v0 != 0.0 {
                widen(x0, v0);
            } else if x0.abs() > farthest.abs() {
                farthest = x0;
            }
        }
        if farthest != 0.0 {
            widen(farthest, 0.0);
        }
        (quiet, calm.max(quiet))
    }
}

/// e^{-ατ} (p cos ωτ + q sin ωτ)（欠阻尼弹簧的位移、速度）的停稳窗口，见 [`Modes::window`]。
///
/// 写成 k e^{-ατ} cos(ωτ − φ)：绝对值的峰在 tan(ωτ − φ) = −α/ω 处，每半个周期一个，峰值 k cos δ e^{-ατ} 按指数衰减，
/// 所以"峰值超过 limit 的最后一个峰"直接算得出来：它之前一定还在动；它之后到下一个零点绝对值单调变小，再往后的峰都不超过 limit。
/// 那个零点和包络 k e^{-ατ} 降到 limit 的时刻都是上界
fn oscillation_window(p: f64, q: f64, (alpha, omega, delta, peak): (f64, f64, f64, f64), limit: f64) -> (f64, f64) {
    let k = p.hypot(q);
    if k <= limit {
        return (0.0, 0.0);
    }
    if alpha <= 0.0 {
        return (f64::INFINITY, f64::INFINITY);
    }
    let envelope = (k / limit).ln() / alpha;
    let phi = q.atan2(p);
    // 峰值超过 limit 的峰都在 last 之前；最后一个是第 n 个（可能在 0 之前）
    let last = envelope + peak / alpha;
    let n = ((last * omega - phi + delta) / PI).floor();
    let top = (phi - delta + n * PI) / omega;
    let zero = (phi + FRAC_PI_2 + n * PI) / omega;
    if zero <= 0.0 {
        return (0.0, 0.0);
    }
    let quiet = top.max(0.0);
    (quiet, zero.min(envelope).max(quiet))
}

/// e^{-ωτ} (a + bτ)（临界阻尼弹簧的位移、速度）的停稳窗口：绝对值的极值在 1/ω − a/b，之后单调变小；
/// 上界是 (|a| + |b|τ) e^{-ωτ/2} 的最大值乘上 e^{-ωτ/2} 降到 limit 的时刻
fn linear_window(a: f64, b: f64, omega: f64, limit: f64) -> (f64, f64) {
    let (a, b) = if b < 0.0 { (-a, -b) } else { (a, b) };
    if b == 0.0 {
        return if a.abs() <= limit { (0.0, 0.0) } else { (0.0, (a.abs() / limit).ln() / omega) };
    }
    let quiet = (1.0 / omega - a / b).max(0.0);
    let half = (2.0 / omega - a.abs() / b).max(0.0);
    let most = (a.abs() + b * half) * (-0.5 * omega * half).exp();
    let calm = if most <= limit { 0.0 } else { 2.0 * (most / limit).ln() / omega };
    (quiet, calm.max(quiet))
}

/// c1 e^{r1 τ} + c2 e^{r2 τ}（过阻尼弹簧的位移、速度，r2 < r1 < 0）的停稳窗口：导数至多一个零点，之后单调变小；
/// 上界是包络 (|c1| + |c2|) e^{r1 τ} 降到 limit 的时刻
fn exponential_window(c1: f64, c2: f64, r1: f64, r2: f64, limit: f64) -> (f64, f64) {
    let k = c1.abs() + c2.abs();
    if k <= limit {
        return (0.0, 0.0);
    }
    let turn = -(c2 * r2) / (c1 * r1);
    let quiet = if turn > 0.0 && turn.is_finite() { (turn.ln() / (r1 - r2)).max(0.0) } else { 0.0 };
    let calm = if r1 < 0.0 { (k / limit).ln() / -r1 } else { f64::INFINITY };
    (quiet, calm.max(quiet))
}

/// 弹簧轨道：从锚点（时刻、位置、速度）出发奔向目标，任意时刻的位置和速度按解析解一步求出。
///
/// 欠阻尼、临界阻尼、过阻尼三种形式，系数和关键帧的弹簧段是同一组公式（`f32`）。停稳：离目标小于容差、速度小于十倍容差，
/// 并且之后一直如此的那一刻起，值就是目标本身。改目标时只算出一个窗口：窗口之前一定在动，
/// 之后一定停了，窗口里按那一刻的位移和速度判断。都是时刻的纯函数，两条线程同一刻停。
/// 改目标时从那一刻的位置和速度接着走，被打断时运动连续
#[derive(Clone, Debug, PartialEq)]
pub struct SpringTrack {
    spring: Spring,
    /// 弹簧的模态：构造时算好，采样时不用再开方、相除
    modes: Modes,
    /// 分量数（`len()`）
    count: u8,
    tolerance: f32,
    /// 锚点的时刻；静止的轨道为 `None`
    start: Option<f64>,
    /// 锚点的位置和速度
    from: Components,
    velocity: Components,
    target: Components,
    /// 停稳的窗口（时钟时刻）：`quiet` 之前一定在动，`calm` 起一定停了
    quiet: f64,
    calm: f64,
}

impl SpringTrack {
    /// 分量数
    #[inline]
    fn len(&self) -> usize {
        usize::from(self.count)
    }

    /// 静止在 `initial`。`tolerance` 是停稳的阈值，单位同值本身。分量超过 [`MAX_COMPONENTS`] 时 panic
    pub fn new(spring: Spring, initial: &[f32], tolerance: f32) -> Self {
        let value = components(initial);
        Self {
            spring,
            modes: Modes::of(spring),
            count: initial.len() as u8,
            tolerance,
            start: None,
            from: value,
            velocity: [0.0; MAX_COMPONENTS],
            target: value,
            quiet: f64::NEG_INFINITY,
            calm: f64::NEG_INFINITY,
        }
    }

    /// 曲线
    pub fn spring(&self) -> Spring {
        self.spring
    }

    /// 目标
    pub fn target(&self) -> &[f32] {
        &self.target[..self.len()]
    }

    /// 时刻 `t` 的速度（每秒变化量）写进 `out`；停稳以后是 0
    pub fn velocity(&self, t: f64, out: &mut [f32]) {
        let mut position = [0.0; MAX_COMPONENTS];
        self.state(t, &mut position, &mut out[..self.len()]);
    }

    /// 从时刻 `t` 的 `value`、`velocity` 重新出发，目标不变：从别的来源（另一条线程画到的状态、关键帧）接手时用
    pub fn restart(&mut self, t: f64, value: &[f32], velocity: &[f32]) {
        debug_assert_eq!(value.len(), self.len());
        debug_assert_eq!(velocity.len(), self.len());
        self.anchor(t, components(value), components(velocity));
    }

    /// 直接停在 `value`（目标也是它），不再动
    pub fn jump(&mut self, value: &[f32]) {
        debug_assert_eq!(value.len(), self.len());
        self.start = None;
        self.from = components(value);
        self.target = self.from;
        self.velocity = [0.0; MAX_COMPONENTS];
        self.quiet = f64::NEG_INFINITY;
        self.calm = f64::NEG_INFINITY;
    }

    /// 从时刻 `t` 起把位置平移 `delta`，目标不变，保留速度接着走（参照系变了：比如元素换了布局位置）
    pub fn shift(&mut self, t: f64, delta: &[f32]) {
        let (mut position, mut velocity) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
        self.state(t, &mut position, &mut velocity);
        for (p, d) in position.iter_mut().zip(delta) {
            *p += d;
        }
        if position[..self.len()] == self.target[..self.len()] && velocity[..self.len()].iter().all(|v| *v == 0.0) {
            self.jump(&position[..self.len()]);
        } else {
            self.anchor(t, position, velocity);
        }
    }

    fn anchor(&mut self, t: f64, from: Components, velocity: Components) {
        self.start = Some(t);
        self.from = from;
        self.velocity = velocity;
        let start = (0..self.len()).map(|i| (f64::from(from[i] - self.target[i]), f64::from(velocity[i])));
        let (quiet, calm) = self.modes.window(start, f64::from(self.tolerance));
        (self.quiet, self.calm) = (t + quiet, t + calm);
    }

    /// 停下的时刻：窗口里二分出"按那一刻的值已经停了"的最早时刻（只有查询时才算）；没在动、不衰减时为 `None`
    fn settle(&self) -> Option<f64> {
        self.start?;
        if !self.calm.is_finite() {
            return None;
        }
        let (mut lo, mut hi) = (self.quiet, self.calm);
        let (mut position, mut velocity) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
        if lo >= hi || self.state(lo, &mut position, &mut velocity).is_settled() {
            return Some(lo.min(hi));
        }
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if mid <= lo || mid >= hi {
                break;
            }
            if self.state(mid, &mut position, &mut velocity).is_settled() {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Some(hi)
    }

    /// 时刻 `t` 的位置、速度，返回在不在动
    fn state(&self, t: f64, position: &mut [f32], velocity: &mut [f32]) -> TrackState {
        let len = self.len();
        let start = match self.start {
            Some(start) if t < self.calm => start,
            // 停稳了（或者从没动过）：就在目标上
            _ => return self.rest(position, velocity),
        };
        let step = self.modes.step((t - start).max(0.0) as f32);
        for i in 0..len {
            let (x, v) = step.apply(self.from[i] - self.target[i], self.velocity[i]);
            position[i] = self.target[i] + x;
            velocity[i] = v;
        }
        if t >= self.quiet && self.within(position, velocity) {
            return self.rest(position, velocity);
        }
        TrackState::Moving
    }

    /// 停在目标上
    fn rest(&self, position: &mut [f32], velocity: &mut [f32]) -> TrackState {
        position[..self.len()].copy_from_slice(&self.target[..self.len()]);
        velocity[..self.len()].fill(0.0);
        TrackState::Settled
    }

    /// 位置离目标都不超过容差、速度都不超过十倍容差（停稳的窗口里，这时起就一直如此）
    fn within(&self, position: &[f32], velocity: &[f32]) -> bool {
        let (lx, lv) = (self.tolerance, self.tolerance * 10.0);
        (0..self.len()).all(|i| (position[i] - self.target[i]).abs() <= lx && velocity[i].abs() <= lv)
    }
}

impl Track for SpringTrack {
    #[inline]
    fn sample(&self, t: f64, out: &mut [f32]) -> TrackState {
        let len = self.len();
        match self.start {
            Some(start) if t < self.calm => {
                let out = &mut out[..len];
                // 锚点那一刻（刚改了目标就采样）就是锚点的位置，不用算指数和三角函数
                if t <= start && t < self.quiet {
                    out.copy_from_slice(&self.from[..len]);
                    return TrackState::Moving;
                }
                // 停稳的窗口里要连速度一起判断
                if t >= self.quiet {
                    let mut velocity = [0.0; MAX_COMPONENTS];
                    return self.state(t, out, &mut velocity);
                }
                for ((out, from), target) in out.iter_mut().zip(&self.from).zip(&self.target) {
                    *out = from - target;
                }
                self.modes.step((t - start) as f32).positions(out, &self.velocity[..len]);
                for (out, target) in out.iter_mut().zip(&self.target) {
                    *out += target;
                }
                TrackState::Moving
            }
            // 停稳了（或者从没动过）：就在目标上
            _ => {
                out[..len].copy_from_slice(&self.target[..len]);
                TrackState::Settled
            }
        }
    }

    fn retarget(&mut self, t: f64, target: &[f32]) {
        debug_assert_eq!(target.len(), self.len());
        let (mut position, mut velocity) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
        self.state(t, &mut position, &mut velocity);
        self.target = components(target);
        self.anchor(t, position, velocity);
    }

    fn describe(&self) -> TrackInfo {
        let start = self.start.filter(|_| self.calm > f64::NEG_INFINITY);
        TrackInfo { kind: TrackKind::Spring(self.spring), len: self.len(), start, end: start.and(self.settle()), target: self.target }
    }
}

// ---- 补间 ----

/// 补间轨道：从锚点的值按缓动曲线走到目标，固定时长；可以循环、往返。
///
/// 改目标时从那一刻的值重新计时（被打断时从零速度出发；`velocity` 给出的是缓动曲线上的速度）
#[derive(Clone, Debug, PartialEq)]
pub struct TweenTrack {
    tween: Tween,
    /// 时长（秒）：构造时算好。`Duration` 换成秒要做一次浮点除法，一万层每帧采样一万次，不值得每次算
    duration: f64,
    /// 分量数（`len()`）
    count: u8,
    /// 这一段开始的时刻；静止的轨道为 `None`
    start: Option<f64>,
    from: Components,
    target: Components,
}

impl TweenTrack {
    /// 分量数
    #[inline]
    fn len(&self) -> usize {
        usize::from(self.count)
    }

    /// 静止在 `initial`。分量超过 [`MAX_COMPONENTS`] 时 panic
    pub fn new(tween: Tween, initial: &[f32]) -> Self {
        let value = components(initial);
        let duration = f64::from(seconds(tween.duration));
        Self { tween, duration, count: initial.len() as u8, start: None, from: value, target: value }
    }

    /// 曲线
    pub fn tween(&self) -> Tween {
        self.tween
    }

    /// 目标
    pub fn target(&self) -> &[f32] {
        &self.target[..self.len()]
    }

    /// 时刻 `t` 的速度（每秒变化量）写进 `out`：两端之差乘以缓动曲线在那一刻的斜率、除以时长。停下以后是 0
    pub fn velocity(&self, t: f64, out: &mut [f32]) {
        let len = self.len();
        let out = &mut out[..len];
        out.fill(0.0);
        let duration = self.duration;
        let Some(start) = self.start.filter(|_| duration > 0.0) else { return };
        let elapsed = (t - start).max(0.0);
        let (from, to, progress) = match self.tween.repeat {
            Repeat::Once if t >= start + duration - EPSILON => return,
            Repeat::Once => (&self.from, &self.target, elapsed / duration),
            Repeat::Forever | Repeat::Alternate => {
                let rounds = (elapsed / duration).floor();
                let phase = (elapsed - rounds * duration) / duration;
                if self.tween.repeat == Repeat::Alternate && rounds % 2.0 == 1.0 {
                    (&self.target, &self.from, phase)
                } else {
                    (&self.from, &self.target, phase)
                }
            }
        };
        let slope = self.tween.easing.slope(progress as f32) / self.duration as f32;
        for i in 0..len {
            out[i] = (to[i] - from[i]) * slope;
        }
    }

    /// 从时刻 `t` 的 `value` 重新计时，目标不变（补间从零速度出发，不接着给的速度走）；已经在目标上时停住
    pub fn restart(&mut self, t: f64, value: &[f32]) {
        debug_assert_eq!(value.len(), self.len());
        if value == self.target() {
            self.jump(value);
            return;
        }
        self.from = components(value);
        self.start = Some(t);
    }

    /// 直接停在 `value`（目标也是它），不再动
    pub fn jump(&mut self, value: &[f32]) {
        debug_assert_eq!(value.len(), self.len());
        self.start = None;
        self.from = components(value);
        self.target = self.from;
    }

    /// 从时刻 `t` 起把此刻的值平移 `delta`，目标不变，从新的位置重新计时
    pub fn shift(&mut self, t: f64, delta: &[f32]) {
        let mut value = [0.0; MAX_COMPONENTS];
        self.sample(t, &mut value);
        for (v, d) in value.iter_mut().zip(delta) {
            *v += d;
        }
        self.restart(t, &value[..self.len()]);
    }
}

impl Track for TweenTrack {
    fn sample(&self, t: f64, out: &mut [f32]) -> TrackState {
        let len = self.len();
        let duration = self.duration;
        let Some(start) = self.start.filter(|_| duration > 0.0) else {
            out[..len].copy_from_slice(&self.target[..len]);
            return TrackState::Settled;
        };
        let elapsed = (t - start).max(0.0);
        let (from, to, progress, motion) = match self.tween.repeat {
            Repeat::Once if t >= start + duration - EPSILON => {
                out[..len].copy_from_slice(&self.target[..len]);
                return TrackState::Settled;
            }
            Repeat::Once => (&self.from, &self.target, elapsed / duration, TrackState::Moving),
            Repeat::Forever | Repeat::Alternate => {
                let rounds = (elapsed / duration).floor();
                let phase = (elapsed - rounds * duration) / duration;
                // 往返：奇数轮倒着走
                let backwards = self.tween.repeat == Repeat::Alternate && rounds % 2.0 == 1.0;
                if backwards {
                    (&self.target, &self.from, phase, TrackState::Moving)
                } else {
                    (&self.from, &self.target, phase, TrackState::Moving)
                }
            }
        };
        let eased = self.tween.easing.apply(progress as f32);
        for i in 0..len {
            out[i] = from[i] + (to[i] - from[i]) * eased;
        }
        motion
    }

    fn retarget(&mut self, t: f64, target: &[f32]) {
        debug_assert_eq!(target.len(), self.len());
        if self.start.is_some() && target == self.target() {
            return;
        }
        let mut current = [0.0; MAX_COMPONENTS];
        self.sample(t, &mut current);
        self.from = current;
        self.target = components(target);
        self.start = Some(t);
    }

    fn describe(&self) -> TrackInfo {
        let end = match self.tween.repeat {
            Repeat::Once => self.start.map(|start| start + self.duration - EPSILON),
            Repeat::Forever | Repeat::Alternate => None,
        };
        TrackInfo { kind: TrackKind::Tween(self.tween), len: self.len(), start: self.start, end, target: self.target }
    }
}

// ---- 弹簧或补间 ----

/// 弹簧或补间的轨道：按 [`Curve`] 是 [`SpringTrack`] 还是 [`TweenTrack`]。声明的过渡、退场、布局位置的过渡、
/// 共享元素的飞行、拖放的影子，以及交给渲染线程的合成层动画都是它：纯数据，主线程改了目标以后整份交给渲染线程，
/// 两边按同一个时钟时刻采样。
///
/// 改成同一个目标什么都不做（不重新定锚：补间不重新计时，弹簧停稳的时刻也不变）
#[derive(Clone, Debug, PartialEq)]
pub enum CurveTrack {
    /// 弹簧
    Spring(SpringTrack),
    /// 补间
    Tween(TweenTrack),
}

impl CurveTrack {
    /// 静止在 `initial`。`tolerance` 是弹簧停稳的阈值，单位同值本身（补间不用）。分量超过 [`MAX_COMPONENTS`] 时 panic
    pub fn new(curve: impl Into<Curve>, initial: &[f32], tolerance: f32) -> Self {
        match curve.into() {
            Curve::Spring(spring) => Self::Spring(SpringTrack::new(spring, initial, tolerance)),
            Curve::Tween(tween) => Self::Tween(TweenTrack::new(tween, initial)),
        }
    }

    /// 曲线
    pub fn curve(&self) -> Curve {
        match self {
            Self::Spring(track) => Curve::Spring(track.spring()),
            Self::Tween(track) => Curve::Tween(track.tween()),
        }
    }

    /// 分量数
    pub fn len(&self) -> usize {
        match self {
            Self::Spring(track) => track.len(),
            Self::Tween(track) => track.len(),
        }
    }

    /// 没有分量
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 目标
    pub fn target(&self) -> &[f32] {
        match self {
            Self::Spring(track) => track.target(),
            Self::Tween(track) => track.target(),
        }
    }

    /// 时刻 `t` 的速度（每秒变化量）写进 `out`；停下以后是 0
    pub fn velocity(&self, t: f64, out: &mut [f32]) {
        match self {
            Self::Spring(track) => track.velocity(t, out),
            Self::Tween(track) => track.velocity(t, out),
        }
    }

    /// 从时刻 `t` 的 `value`、`velocity` 重新出发，目标不变：从别的来源（时间线、另一段动画）接手时用。
    /// 补间不带速度，从 `value` 重新计时；已经在目标上、又没有速度时停住
    pub fn restart(&mut self, t: f64, value: &[f32], velocity: &[f32]) {
        match self {
            Self::Spring(track) if value == track.target() && velocity.iter().all(|v| *v == 0.0) => track.jump(value),
            Self::Spring(track) => track.restart(t, value, velocity),
            Self::Tween(track) => track.restart(t, value),
        }
    }

    /// 直接停在 `value`（目标也是它）
    pub fn jump(&mut self, value: &[f32]) {
        match self {
            Self::Spring(track) => track.jump(value),
            Self::Tween(track) => track.jump(value),
        }
    }

    /// 从时刻 `t` 起把此刻的值平移 `delta`，目标不变：弹簧保留速度，补间从新的位置重新计时
    pub fn shift(&mut self, t: f64, delta: &[f32]) {
        match self {
            Self::Spring(track) => track.shift(t, delta),
            Self::Tween(track) => track.shift(t, delta),
        }
    }

    /// 停下的时刻；没在动、或者一直在动（循环的补间、不衰减的弹簧）时为 `None`
    pub fn end(&self) -> Option<f64> {
        match self {
            Self::Spring(track) => track.settle(),
            Self::Tween(track) => match track.tween.repeat {
                Repeat::Once => track.start.map(|start| start + track.duration - EPSILON),
                Repeat::Forever | Repeat::Alternate => None,
            },
        }
    }

    /// 时刻 `t` 还在动（之后的值还会变）
    pub fn is_moving(&self, t: f64) -> bool {
        match self {
            Self::Spring(track) => {
                let (mut position, mut velocity) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
                !track.state(t, &mut position, &mut velocity).is_settled()
            }
            Self::Tween(track) => match track.start {
                Some(_) if track.tween.duration.is_zero() => false,
                Some(start) => track.tween.repeat != Repeat::Once || t < start + track.duration - EPSILON,
                None => false,
            },
        }
    }
}

impl Track for CurveTrack {
    #[inline]
    fn sample(&self, t: f64, out: &mut [f32]) -> TrackState {
        match self {
            Self::Spring(track) => track.sample(t, out),
            Self::Tween(track) => track.sample(t, out),
        }
    }

    fn retarget(&mut self, t: f64, target: &[f32]) {
        if target == self.target() {
            return;
        }
        match self {
            Self::Spring(track) => track.retarget(t, target),
            Self::Tween(track) => track.retarget(t, target),
        }
    }

    fn describe(&self) -> TrackInfo {
        match self {
            Self::Spring(track) => track.describe(),
            Self::Tween(track) => track.describe(),
        }
    }
}

// ---- 关键帧 ----

/// 改目标时叠加在关键帧上的修正：三次 Hermite，从改目标那一刻的修正量和它的变化速度出发，
/// 到 `start + span` 时正好是 `to`、变化速度为 0
#[derive(Clone, Debug, PartialEq)]
struct Blend {
    start: f64,
    span: f64,
    from: Components,
    velocity: Components,
    to: Components,
}

impl Blend {
    /// 时刻 `t` 的修正量和它的变化速度（每秒）
    fn at(&self, t: f64, len: usize) -> (Components, Components) {
        let (mut offset, mut rate) = ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]);
        if self.span <= 0.0 || t >= self.start + self.span {
            offset[..len].copy_from_slice(&self.to[..len]);
            return (offset, rate);
        }
        let s = ((t - self.start) / self.span).max(0.0);
        let (s2, s3) = (s * s, s * s * s);
        let (h00, h10, h01) = (2.0 * s3 - 3.0 * s2 + 1.0, s3 - 2.0 * s2 + s, -2.0 * s3 + 3.0 * s2);
        let (d00, d10, d01) = (6.0 * s2 - 6.0 * s, 3.0 * s2 - 4.0 * s + 1.0, -6.0 * s2 + 6.0 * s);
        for i in 0..len {
            let (p0, m0, p1) = (f64::from(self.from[i]), f64::from(self.velocity[i]) * self.span, f64::from(self.to[i]));
            offset[i] = (h00 * p0 + h10 * m0 + h01 * p1) as f32;
            rate[i] = ((d00 * p0 + d10 * m0 + d01 * p1) / self.span) as f32;
        }
        (offset, rate)
    }

    fn end(&self) -> f64 {
        self.start + self.span.max(0.0)
    }
}

/// 一串关键帧（[`KeyframeSeries`]，按 `Arc` 共享）在时间线里的排期：从时间线的位置逐层换算到关键帧的时间
/// （[`Timing`]：序列、并行组、延迟、按元素错开、重复、往返），`index` 是这个元素按界面先后的序号（错开用）
#[derive(Clone, Debug)]
pub struct KeyframePlacement {
    /// 关键帧
    pub keys: Arc<KeyframeSeries>,
    /// 从时间线的根到这一段的各层排期；空的表示时间线的位置就是关键帧的时间
    pub levels: Arc<[Timing]>,
    /// 元素的序号
    pub index: f64,
}

impl KeyframePlacement {
    /// 一串关键帧，不分层：时间线的位置就是关键帧的时间
    pub fn new(keys: Arc<KeyframeSeries>) -> Self {
        Self { keys, levels: Arc::from([]), index: 0.0 }
    }

    /// 时间线的位置换算成关键帧的时间和它随位置走的方向；有一层还没开始时为 `None`
    #[inline]
    fn local(&self, position: f64) -> Option<(f32, f32)> {
        local_time_rate(&self.levels, self.index, position)
    }
}

/// 关键帧的播放：时间线里同一个值的几段关键帧（[`KeyframePlacement`]），按播放头（[`Playhead`]）从时钟时刻
/// 换算到时间线的位置，再逐层换算到每段关键帧的时间。几段里取已经开始的最后一段，都还没开始时停在第一段的第一帧；
/// `reduced`（减少动态效果）时直接是最后一段的最后的值。
///
/// 时间线的每个（元素，属性）是一个：主线程按它写值，动合成层的整份交给渲染线程，两边按同一个时刻采样。
/// 播放、暂停、倒放、跳转换的是播放头（[`KeyframePlayback::set_playhead`]）。
///
/// 改目标：编好的路径照走，从这一刻起叠加一段平滑的修正，位置和变化速度都连续，到原来的结尾正好落在新目标上；
/// 已经放完（或者剩下的时间比最后一段还短、停着、一直循环）时，修正用最后一段的时长
#[derive(Clone, Debug)]
pub struct KeyframePlayback {
    segments: Arc<[KeyframePlacement]>,
    playhead: Playhead,
    reduced: bool,
    blend: Option<Box<Blend>>,
}

impl KeyframePlayback {
    /// 一串关键帧从时钟的 `start` 秒起正放一次
    pub fn new(keys: Arc<KeyframeSeries>, start: f64) -> Self {
        let duration = f64::from(keys.duration());
        let playhead = Playhead { anchor: start, position: 0.0, duration, reversed: false, playing: true };
        Self::timeline(Arc::from([KeyframePlacement::new(keys)]), playhead, false)
    }

    /// 时间线里的一个值：`segments` 是它在时间线上的几段（不能是空的），按 `playhead` 放。
    /// `reduced`：减少动态效果，直接是最后的值
    pub fn timeline(segments: Arc<[KeyframePlacement]>, playhead: Playhead, reduced: bool) -> Self {
        assert!(!segments.is_empty(), "时间线里的一个值至少有一段关键帧");
        Self { segments, playhead, reduced, blend: None }
    }

    /// 换播放头（播放、暂停、倒放、跳转之后）
    pub fn set_playhead(&mut self, playhead: Playhead) {
        self.playhead = playhead;
    }

    /// 播放头
    pub fn playhead(&self) -> &Playhead {
        &self.playhead
    }

    /// 几段关键帧
    pub fn segments(&self) -> &Arc<[KeyframePlacement]> {
        &self.segments
    }

    /// 减少动态效果：直接是最后的值
    pub fn is_reduced(&self) -> bool {
        self.reduced
    }

    /// 分量数
    pub fn len(&self) -> usize {
        self.segments[0].keys.len()
    }

    /// 没有分量
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 最后一段的最后一帧
    fn last(&self) -> &[f32] {
        self.segments[self.segments.len() - 1].keys.last()
    }

    /// 时刻 `t` 的速度（每秒变化量）写进 `out`：关键帧在那一刻的变化速度，乘上各层排期和播放头的方向；
    /// 停着、放完了、减少动态效果时是 0（加上改目标的修正的变化速度）
    pub fn velocity(&self, t: f64, out: &mut [f32]) {
        let len = self.len();
        out[..len].fill(0.0);
        let direction = self.playhead.direction_at(t) as f32;
        if !self.reduced && direction != 0.0 {
            let position = self.playhead.position_at(t);
            if let Some((segment, (local, rate))) = self.segments.iter().rev().find_map(|s| s.local(position).map(|l| (s, l))) {
                let scale = rate * direction;
                if scale != 0.0 {
                    segment.keys.velocity(local, out);
                    out[..len].iter_mut().for_each(|v| *v *= scale);
                }
            }
        }
        if let Some(blend) = &self.blend {
            let (_, rate) = blend.at(t, len);
            for i in 0..len {
                out[i] += rate[i];
            }
        }
    }

    /// 编好的路径（不算改目标的修正）在时间线的位置 `position` 上的值
    #[inline]
    fn sample_keys(&self, position: f64, out: &mut [f32]) {
        Self::sample_segments(&self.segments, self.reduced, position, out);
    }

    /// 几段关键帧在时间线的位置 `position` 上的值（和 [`KeyframePlayback`] 采样同一个函数）：同一条时间线的几千个目标共用一个播放头时，
    /// 只交出各自的几段关键帧和减少动态效果的标记，播放头另外交一份（渲染线程这样存，省地方）。`segments` 不能是空的
    #[inline]
    pub fn sample_segments(segments: &[KeyframePlacement], reduced: bool, position: f64, out: &mut [f32]) {
        if reduced {
            let last = segments[segments.len() - 1].keys.last();
            out[..last.len()].copy_from_slice(last);
            return;
        }
        for segment in segments.iter().rev() {
            if let Some(local) = local_time(&segment.levels, segment.index, position) {
                segment.keys.sample(local, out);
                return;
            }
        }
        let first = segments[0].keys.first();
        out[..first.len()].copy_from_slice(first);
    }

    /// 时刻 `t` 的值，时间线的位置已经由播放头算好（`position`，等于 `playhead().position_at(t)`）：
    /// 同一条时间线的几千个目标共用一个播放头，位置只算一次
    #[inline]
    pub fn sample_position(&self, position: f64, t: f64, out: &mut [f32]) {
        self.sample_keys(position, out);
        if let Some(blend) = &self.blend {
            let len = self.len();
            let (offset, _) = blend.at(t, len);
            for i in 0..len {
                out[i] += offset[i];
            }
        }
    }

    fn offset(&self, t: f64) -> (Components, Components) {
        match &self.blend {
            Some(blend) => blend.at(t, self.len()),
            None => ([0.0; MAX_COMPONENTS], [0.0; MAX_COMPONENTS]),
        }
    }
}

impl Track for KeyframePlayback {
    #[inline]
    fn sample(&self, t: f64, out: &mut [f32]) -> TrackState {
        self.sample_position(self.playhead.position_at(t), t, out);
        let keys_moving = !self.reduced && self.playhead.direction_at(t) != 0.0;
        let blending = self.blend.as_ref().is_some_and(|blend| t < blend.end());
        if keys_moving || blending { TrackState::Moving } else { TrackState::Settled }
    }

    fn retarget(&mut self, t: f64, target: &[f32]) {
        let len = self.len();
        debug_assert_eq!(target.len(), len);
        let (from, velocity) = self.offset(t);
        let last = self.last();
        let mut to = [0.0; MAX_COMPONENTS];
        for i in 0..len {
            to[i] = target[i] - last[i];
        }
        let last_span = f64::from(self.segments[self.segments.len() - 1].keys.last_span());
        let span = self.playhead.end().map_or(last_span, |end| (end - t).max(last_span));
        self.blend = Some(Box::new(Blend { start: t, span, from, velocity, to }));
    }

    fn describe(&self) -> TrackInfo {
        let len = self.len();
        let mut target = components(self.last());
        let (offset, _) = self.offset(f64::INFINITY);
        for i in 0..len {
            target[i] += offset[i];
        }
        let keys = &self.segments[self.segments.len() - 1].keys;
        let blend_end = self.blend.as_ref().map(|blend| blend.end());
        // 编好的路径：在走时到头的那一刻（一直循环的没有）；停着的就是锚点那一刻
        let keys_end = if self.playhead.playing { self.playhead.end() } else { Some(self.playhead.anchor) };
        let end = match (keys_end, blend_end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (keys, None) => keys,
            (None, Some(_)) => None,
        };
        TrackInfo {
            kind: TrackKind::Keyframes { keys: keys.key_count(), duration: keys.duration() },
            len,
            start: Some(self.playhead.anchor),
            end,
            target,
        }
    }
}

// ---- 时钟 ----

/// 动画的时钟：把宿主时间（显示链接给的上屏时刻、测试工具里的虚拟时间，秒）换算成动画时间。
///
/// 可以暂停、改倍速（慢放）、拖到任意时刻。是 `Copy` 的小结构体，`Send`：主线程改了以后随帧消息交给渲染线程，
/// 两条线程按同一个宿主时刻算出同一个动画时间，轨道按它采样，结果一样。换算是线性的：
/// 锚点（宿主时刻 ↔ 动画时刻）加倍速，改倍速、暂停、继续、拖动都在当时的宿主时刻重新定锚，动画时间不跳。
///
/// ```
/// use aureole_animation::Clock;
///
/// let mut clock = Clock::new(100.0); // 宿主时间 100 秒时，动画时间从 0 开始
/// assert_eq!(clock.now(101.5), 1.5);
/// clock.set_speed(101.5, 0.5); // 从这一刻起慢放一半
/// assert_eq!(clock.now(103.5), 2.5);
/// clock.pause(103.5);
/// assert_eq!(clock.now(200.0), 2.5);
/// clock.seek(200.0, 0.25); // 拖回 0.25 秒
/// clock.resume(200.0);
/// assert_eq!(clock.now(201.0), 0.75);
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clock {
    /// 锚点的宿主时刻
    host: f64,
    /// 锚点那一刻的动画时间
    time: f64,
    speed: f64,
    paused: bool,
}

impl Clock {
    /// 宿主时刻 `host` 时动画时间为 0，正常速度，在走
    pub const fn new(host: f64) -> Self {
        Self { host, time: 0.0, speed: 1.0, paused: false }
    }

    /// 宿主时刻 `host` 对应的动画时间，秒。暂停时停在暂停那一刻
    pub fn now(&self, host: f64) -> f64 {
        if self.paused { self.time } else { self.time + (host - self.host) * self.speed }
    }

    /// 倍速：1 是正常，小于 1 慢放
    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 暂停着
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// 在走：没暂停，倍速大于 0。停着的时钟上什么都不会动，不用出帧
    pub fn is_running(&self) -> bool {
        !self.paused && self.speed > 0.0
    }

    /// 从宿主时刻 `host` 起按 `speed` 倍速走（负数、非数按 0）
    pub fn set_speed(&mut self, host: f64, speed: f64) {
        self.reanchor(host);
        self.speed = if speed > 0.0 { speed } else { 0.0 };
    }

    /// 在宿主时刻 `host` 暂停
    pub fn pause(&mut self, host: f64) {
        self.reanchor(host);
        self.paused = true;
    }

    /// 在宿主时刻 `host` 继续走
    pub fn resume(&mut self, host: f64) {
        self.reanchor(host);
        self.paused = false;
    }

    /// 在宿主时刻 `host` 把动画时间拖到 `time`（暂停着时停在那里）
    pub fn seek(&mut self, host: f64, time: f64) {
        self.host = host;
        self.time = time;
    }

    fn reanchor(&mut self, host: f64) {
        self.time = self.now(host);
        self.host = host;
    }
}

impl Default for Clock {
    /// 宿主时刻 0 时动画时间为 0
    fn default() -> Self {
        Self::new(0.0)
    }
}
