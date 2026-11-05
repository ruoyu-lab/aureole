//! 关键帧和时间线的纯数学：一个值随时间怎么变（[`KeyframeSeries`]）、时间线里一层的排期（[`Timing`]）、
//! 播放头（[`Playhead`]）。都是普通数据、纯函数：主线程和渲染线程按同一个时刻各算各的，结果一样。

use crate::MAX_COMPONENTS;
use crate::spring::SpringStep;
use crate::track::EPSILON;
use crate::curve::{Easing, Spring};

type Components = [f32; MAX_COMPONENTS];

/// 关键帧之间一段的曲线
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyCurve {
    /// 在这一段的时长里按缓动曲线走完
    Easing(Easing),
    /// 弹簧：从上一段结束时的位置和速度出发奔向这一帧的值，到这一帧的时刻正好到达
    /// （弹簧在这段时间里没停稳时，剩下的一点在这一段里匀速补上）
    Spring(Spring),
}

#[derive(Clone, Debug)]
struct Key {
    /// 秒，从第一帧算起
    time: f32,
    value: Components,
    /// 从上一帧到这一帧的曲线
    curve: KeyCurve,
    /// 这一段开始时的速度（弹簧段用，构造时按上一段的结尾算好）
    velocity: Components,
    /// 弹簧段到这一帧的时刻还差的距离（匀速补上）
    residual: Components,
}

/// 一个值（最多 [`MAX_COMPONENTS`] 个分量）的一串关键帧：第一帧在时刻 0，之后每帧给出时刻、值和到达它的曲线。
///
/// ```
/// use aureole_animation::{Easing, KeyCurve, KeyframeSeries};
///
/// // 0 → 120 毫秒先快后慢放大到 1.2 → 400 毫秒时匀速回到 1
/// let mut pop = KeyframeSeries::new(&[1.0]);
/// pop.push(0.12, &[1.2], KeyCurve::Easing(Easing::EASE_OUT));
/// pop.push(0.4, &[1.0], KeyCurve::Easing(Easing::Linear));
/// let mut out = [0.0; 1];
/// pop.sample(0.12, &mut out);
/// assert_eq!(out, [1.2]);
/// pop.sample(0.26, &mut out);
/// assert!((out[0] - 1.1).abs() < 1e-4);
/// assert_eq!(pop.duration(), 0.4);
/// ```
#[derive(Clone, Debug)]
pub struct KeyframeSeries {
    len: usize,
    start: Components,
    keys: Vec<Key>,
}

impl KeyframeSeries {
    /// 第一帧（时刻 0）的值。分量超过 [`MAX_COMPONENTS`] 时 panic
    pub fn new(start: &[f32]) -> Self {
        assert!(start.len() <= MAX_COMPONENTS, "关键帧的值最多 {MAX_COMPONENTS} 个分量");
        let mut value = [0.0; MAX_COMPONENTS];
        value[..start.len()].copy_from_slice(start);
        Self { len: start.len(), start: value, keys: Vec::new() }
    }

    /// 加一帧：`time` 秒时是 `value`，从上一帧按 `curve` 过来。时刻比上一帧早时当作和上一帧同时（直接跳过去）
    pub fn push(&mut self, time: f32, value: &[f32], curve: KeyCurve) {
        debug_assert_eq!(value.len(), self.len);
        let (from_time, from) = self.keys.last().map_or((0.0, self.start), |k| (k.time, k.value));
        let time = time.max(from_time);
        let velocity = self.keys.len().checked_sub(1).map_or([0.0; MAX_COMPONENTS], |last| self.end_velocity(last));
        let mut to = [0.0; MAX_COMPONENTS];
        to[..self.len].copy_from_slice(value);
        let mut residual = [0.0; MAX_COMPONENTS];
        if let KeyCurve::Spring(spring) = curve {
            let step = SpringStep::new(spring, time - from_time);
            for i in 0..self.len {
                residual[i] = step.apply(from[i] - to[i], velocity[i]).0;
            }
        }
        self.keys.push(Key { time, value: to, curve, velocity, residual });
    }

    /// 分量数
    pub fn len(&self) -> usize {
        self.len
    }

    /// 没有分量（零个分量的值）
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 关键帧数，不算时刻 0 的起始值
    pub(crate) fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// 最后一段的时长，秒；没有关键帧时为 0
    pub(crate) fn last_span(&self) -> f32 {
        match self.keys.as_slice() {
            [] => 0.0,
            [only] => only.time,
            [.., before, last] => last.time - before.time,
        }
    }

    /// 最后一帧的时刻，秒
    pub fn duration(&self) -> f32 {
        self.keys.last().map_or(0.0, |k| k.time)
    }

    /// 第一帧的值
    pub fn first(&self) -> &[f32] {
        &self.start[..self.len]
    }

    /// 最后一帧的值
    pub fn last(&self) -> &[f32] {
        &self.keys.last().map_or(&self.start, |k| &k.value)[..self.len]
    }

    /// 时刻 `t`（秒，超出范围时取两端）的值写进 `out`（长度不小于 [`Self::len`]）
    pub fn sample(&self, t: f32, out: &mut [f32]) {
        let out = &mut out[..self.len];
        // 第一个时刻不早于 t 的帧：t 落在它和前一帧之间
        let k = self.keys.partition_point(|key| key.time < t);
        if k >= self.keys.len() {
            out.copy_from_slice(self.last());
            return;
        }
        let key = &self.keys[k];
        let (from_time, from) = if k == 0 { (0.0, &self.start) } else { (self.keys[k - 1].time, &self.keys[k - 1].value) };
        let span = key.time - from_time;
        if t <= from_time || span <= 0.0 {
            out.copy_from_slice(&from[..self.len]);
            return;
        }
        let tau = t - from_time;
        match key.curve {
            KeyCurve::Easing(easing) => {
                let p = easing.apply(tau / span);
                for i in 0..self.len {
                    out[i] = from[i] + (key.value[i] - from[i]) * p;
                }
            }
            KeyCurve::Spring(spring) => {
                let step = SpringStep::new(spring, tau);
                let ramp = tau / span;
                for i in 0..self.len {
                    let x = step.apply(from[i] - key.value[i], key.velocity[i]).0;
                    out[i] = key.value[i] + x - key.residual[i] * ramp;
                }
            }
        }
    }

    /// 时刻 `t`（秒）的变化速度（每秒变化量）写进 `out`（长度不小于 [`Self::len`]）。第一帧之前、最后一帧之后是 0
    pub fn velocity(&self, t: f32, out: &mut [f32]) {
        let out = &mut out[..self.len];
        out.fill(0.0);
        let k = self.keys.partition_point(|key| key.time < t);
        if k >= self.keys.len() {
            return;
        }
        let key = &self.keys[k];
        let (from_time, from) = if k == 0 { (0.0, &self.start) } else { (self.keys[k - 1].time, &self.keys[k - 1].value) };
        let span = key.time - from_time;
        if t <= from_time || span <= 0.0 {
            return;
        }
        let tau = t - from_time;
        match key.curve {
            KeyCurve::Easing(easing) => {
                let slope = easing.slope(tau / span) / span;
                for i in 0..self.len {
                    out[i] = (key.value[i] - from[i]) * slope;
                }
            }
            KeyCurve::Spring(spring) => {
                let step = SpringStep::new(spring, tau);
                for i in 0..self.len {
                    out[i] = step.apply(from[i] - key.value[i], key.velocity[i]).1 - key.residual[i] / span;
                }
            }
        }
    }

    /// 第 `k` 帧所在一段结束时的速度（每秒变化量）
    fn end_velocity(&self, k: usize) -> Components {
        let key = &self.keys[k];
        let (from_time, from) = if k == 0 { (0.0, &self.start) } else { (self.keys[k - 1].time, &self.keys[k - 1].value) };
        let span = key.time - from_time;
        let mut v = [0.0; MAX_COMPONENTS];
        if span <= 0.0 {
            return v;
        }
        match key.curve {
            KeyCurve::Easing(easing) => {
                // 缓动曲线在终点的斜率（数值差分）
                const H: f32 = 1e-3;
                let slope = (1.0 - easing.apply(1.0 - H)) / H;
                for i in 0..self.len {
                    v[i] = (key.value[i] - from[i]) * slope / span;
                }
            }
            KeyCurve::Spring(spring) => {
                let step = SpringStep::new(spring, span);
                for i in 0..self.len {
                    v[i] = step.apply(from[i] - key.value[i], key.velocity[i]).1 - key.residual[i] / span;
                }
            }
        }
        v
    }
}

/// 时间线里一层的排期：在上一层的一轮里什么时候开始、按元素错开多少、一轮多长、几轮、是否往返。
/// 一条轨道从时间线的根到自己有好几层（序列、并行组、自己），时间逐层换算（[`local_time`]）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    /// 在上一层的一轮里从什么时候开始，秒
    pub start: f64,
    /// 第 `i` 个元素再晚 `i * stagger` 秒开始
    pub stagger: f64,
    /// 一轮的时长，秒
    pub period: f64,
    /// 轮数，至少 1；`f64::INFINITY` 表示一直循环
    pub iterations: f64,
    /// 往返：第 2、4……轮倒着放
    pub alternate: bool,
}

impl Timing {
    /// 只放一轮，没有延迟和错开
    pub const fn once(period: f64) -> Self {
        Self { start: 0.0, stagger: 0.0, period, iterations: 1.0, alternate: false }
    }

    /// 第 `index` 个元素这一层结束的时刻（相对上一层一轮的开始）；一直循环时是无穷
    pub fn end(&self, index: f64) -> f64 {
        let total = if self.iterations.is_infinite() { f64::INFINITY } else { self.period * self.iterations };
        self.start + index * self.stagger + total
    }

    /// 和 [`Timing::map`] 一样，另外给出这一层的时间随上一层怎么走：1 同向，-1 倒着（往返的偶数轮），0 停着（放完了）
    pub fn map_rate(&self, t: f64, index: f64) -> Option<(f64, f32)> {
        let local = self.map(t, index)?;
        let t = t - self.start - index * self.stagger;
        let rounds = self.iterations.max(1.0);
        if self.period <= 0.0 || (rounds.is_finite() && t >= self.period * rounds) {
            return Some((local, 0.0));
        }
        let k = (t / self.period).floor();
        Some((local, if self.alternate && k % 2.0 == 1.0 { -1.0 } else { 1.0 }))
    }

    /// 上一层一轮里的时间 `t` 换算成这一层一轮里的时间；还没开始时为 `None`，放完了停在最后一轮的结尾
    pub fn map(&self, t: f64, index: f64) -> Option<f64> {
        let t = t - self.start - index * self.stagger;
        if t < 0.0 {
            return None;
        }
        if self.period <= 0.0 {
            return Some(0.0);
        }
        let rounds = self.iterations.max(1.0);
        if rounds.is_finite() && t >= self.period * rounds {
            let reversed = self.alternate && (rounds - 1.0) % 2.0 == 1.0;
            return Some(if reversed { 0.0 } else { self.period });
        }
        let k = (t / self.period).floor();
        let phase = t - k * self.period;
        Some(if self.alternate && k % 2.0 == 1.0 { self.period - phase } else { phase })
    }
}

/// 时间线的时刻 `t` 逐层换算成最里层（关键帧）的时间；有一层还没开始时为 `None`
pub fn local_time(levels: &[Timing], index: f64, t: f64) -> Option<f32> {
    let mut t = t;
    for level in levels {
        t = level.map(t, index)?;
    }
    Some(t as f32)
}

/// 和 [`local_time`] 一样换算，另外给出最里层的时间随时间线的时间怎么走：1 是同向，-1 是往返倒着放的一轮，
/// 0 是有一层已经放完、停在结尾（算速度用）
pub fn local_time_rate(levels: &[Timing], index: f64, t: f64) -> Option<(f32, f32)> {
    let (mut t, mut rate) = (t, 1.0f32);
    for level in levels {
        let (local, r) = level.map_rate(t, index)?;
        t = local;
        rate *= r;
    }
    Some((t as f32, rate))
}

/// 播放头：把时钟的时刻换算成时间线的位置。锚在时钟的某一刻（`anchor`）上：那一刻在 `position`，在走时按方向
/// 一秒走一秒，正放到结尾、倒放到开头停住。位置是时刻的纯函数，两条线程拿同一份播放头、按同一个时刻算出同一个位置；
/// 播放、暂停、倒放、跳转都在发命令的那一刻重新定锚（[`Playhead::reanchor`]），位置不跳
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playhead {
    /// 锚点：时钟的这一刻（秒）
    pub anchor: f64,
    /// 锚点那一刻的位置，秒
    pub position: f64,
    /// 总时长，秒；`f64::INFINITY` 表示一直循环
    pub duration: f64,
    /// 倒着放
    pub reversed: bool,
    /// 在走
    pub playing: bool,
}

impl Default for Playhead {
    fn default() -> Self {
        Self { anchor: 0.0, position: 0.0, duration: 0.0, reversed: false, playing: false }
    }
}

impl Playhead {
    /// 时钟时刻 `t` 的位置：停着时是锚点的位置；在走时从锚点按方向走，到头停住。早于锚点的时刻按锚点算
    pub fn position_at(&self, t: f64) -> f64 {
        if !self.playing {
            return self.position;
        }
        // 到头了（差不到 1 微秒也算）：正好在头上，和停在头上的那一份逐位相同
        if self.end().is_some_and(|end| t >= end) {
            return if self.reversed { 0.0 } else { self.duration };
        }
        let elapsed = (t - self.anchor).max(0.0);
        if self.reversed { (self.position.min(self.duration) - elapsed).max(0.0) } else { (self.position + elapsed).min(self.duration) }
    }

    /// 时钟时刻 `t` 的方向：正放 1，倒放 -1，停着或者已经走到头 0
    pub fn direction_at(&self, t: f64) -> f64 {
        match self.end() {
            _ if !self.playing => 0.0,
            Some(end) if t >= end => 0.0,
            _ if self.reversed => -1.0,
            _ => 1.0,
        }
    }

    /// 走到头的时钟时刻：在走、而且会到头时才有（停着的、一直循环正放的为 `None`）。离头不到 1 微秒就算到了
    /// （帧的时刻是累加出来的浮点数，时长又是 `f32`：不然正好该到头的那一帧常常差一点点，要多一帧）
    pub fn end(&self) -> Option<f64> {
        let remaining = if !self.playing {
            return None;
        } else if self.reversed {
            self.position.min(self.duration).max(0.0)
        } else if self.duration.is_finite() {
            (self.duration - self.position).max(0.0)
        } else {
            return None;
        };
        Some(self.anchor + (remaining - EPSILON).max(0.0))
    }

    /// 走到头了（时钟时刻 `t` 不早于 [`Playhead::end`]）：停在头上，锚点挪到走到头的那一刻，返回 `true`。
    /// 没走到头时什么都不改（锚点不动：主线程每帧检查它，不能因此和渲染线程手里的那份不一样）
    pub fn finish(&mut self, t: f64) -> bool {
        match self.end() {
            Some(end) if t >= end => {
                self.position = if self.reversed { 0.0 } else { self.duration };
                self.anchor = end;
                self.playing = false;
                true
            }
            _ => false,
        }
    }

    /// 在时钟时刻 `t` 重新定锚：锚点挪到 `t`，位置换成那一刻的（走到头了就停在头上）。发命令之前调用，之后的改动从这一刻算起
    pub fn reanchor(&mut self, t: f64) {
        if !self.finish(t) {
            self.position = self.position_at(t);
        }
        self.anchor = t;
    }

    /// 锚点那一刻走到头了：正放时在结尾，倒放时在开头
    pub fn is_at_end(&self) -> bool {
        if self.reversed { self.position <= 0.0 } else { self.position >= self.duration }
    }

    /// 时钟时刻 `t` 的播放进度 0..=1；一直循环的时间线是 0
    pub fn progress_at(&self, t: f64) -> f32 {
        if self.duration.is_finite() && self.duration > 0.0 { (self.position_at(t) / self.duration).clamp(0.0, 1.0) as f32 } else { 0.0 }
    }
}
