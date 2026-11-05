use std::f32::consts::TAU;
use std::time::Duration;

/// 弹簧，用"响应时间 + 阻尼比"描述，比刚度和阻尼系数直观。
///
/// 弹簧没有固定时长：目标变了就从当前位置和速度出发奔向新目标，
/// 所以被打断时运动是连续的，这是"手感"的关键。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    /// 无阻尼时振动一个周期的时间；越小越快
    pub response: Duration,
    /// 阻尼比：1 为临界阻尼（不回弹），越小回弹越多
    pub damping_ratio: f32,
}

impl Spring {
    /// 指定响应时间和阻尼比
    pub const fn new(response: Duration, damping_ratio: f32) -> Self {
        Self { response, damping_ratio }
    }

    /// 平滑，不回弹
    pub const fn smooth() -> Self {
        Self::new(Duration::from_millis(450), 1.0)
    }

    /// 快、跟手，几乎不回弹，适合悬停和按压
    pub const fn snappy() -> Self {
        Self::new(Duration::from_millis(300), 0.86)
    }

    /// 明显回弹
    pub const fn bouncy() -> Self {
        Self::new(Duration::from_millis(500), 0.6)
    }

    /// 无阻尼角频率
    pub(crate) fn omega(&self) -> f32 {
        TAU / seconds(self.response).max(1e-4)
    }
}

impl Default for Spring {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), 0.85)
    }
}

/// 缓动函数：三次贝塞尔曲线，起点 (0, 0)、终点 (1, 1)，两个控制点决定快慢
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Easing {
    /// 匀速
    Linear,
    /// 两个控制点 `(x1, y1, x2, y2)`；x 应在 0..=1 内，y 可以超出范围形成过冲
    CubicBezier(f32, f32, f32, f32),
}

impl Easing {
    /// 起步稍快、收尾放慢，最常用的默认值
    pub const EASE: Self = Self::CubicBezier(0.25, 0.1, 0.25, 1.0);
    /// 慢起步，越来越快
    pub const EASE_IN: Self = Self::CubicBezier(0.42, 0.0, 1.0, 1.0);
    /// 快起步，逐渐放慢
    pub const EASE_OUT: Self = Self::CubicBezier(0.0, 0.0, 0.58, 1.0);
    /// 两端慢、中间快
    pub const EASE_IN_OUT: Self = Self::CubicBezier(0.42, 0.0, 0.58, 1.0);

    /// 进度 `x`（0..=1）映射到缓动后的进度
    pub fn apply(&self, x: f32) -> f32 {
        match *self {
            Self::Linear => x.clamp(0.0, 1.0),
            Self::CubicBezier(x1, y1, x2, y2) => cubic_bezier(x1, y1, x2, y2, x),
        }
    }

    /// 进度 `x` 处的斜率：缓动后的进度对进度的导数（补间的速度 = 两端之差 × 斜率 ÷ 时长）。
    /// `x` 超出 0..=1 时按端点算
    pub fn slope(&self, x: f32) -> f32 {
        match *self {
            Self::Linear => 1.0,
            Self::CubicBezier(x1, y1, x2, y2) => cubic_bezier_slope(x1, y1, x2, y2, x.clamp(0.0, 1.0)),
        }
    }
}

/// 补间结束后怎么办
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// 停在目标值
    Once,
    /// 回到起点重新开始，一直循环（转圈：0 → 2π）
    Forever,
    /// 来回往返，一直循环（呼吸、脉冲）
    Alternate,
}

/// 补间：固定时长，从当前值按缓动曲线走到目标
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tween {
    /// 时长
    pub duration: Duration,
    /// 缓动曲线
    pub easing: Easing,
    /// 结束后是否循环
    pub repeat: Repeat,
}

impl Tween {
    /// 指定时长，默认缓动为 `ease`（起步稍快、收尾放慢）
    pub const fn new(duration: Duration) -> Self {
        Self { duration, easing: Easing::EASE, repeat: Repeat::Once }
    }

    /// 一直循环：到达目标后回到起点重新开始，永不停止；和 [`Tween::alternate`] 连用时是来回往返
    pub const fn forever(mut self) -> Self {
        if matches!(self.repeat, Repeat::Once) {
            self.repeat = Repeat::Forever;
        }
        self
    }

    /// 往返：在起点和目标之间来回，永不停止（和 [`Tween::forever`] 连用，先后不论）；`false` 回到不往返
    pub const fn alternate(mut self, alternate: bool) -> Self {
        self.repeat = match (alternate, self.repeat) {
            (true, _) => Repeat::Alternate,
            (false, Repeat::Alternate) => Repeat::Forever,
            (false, repeat) => repeat,
        };
        self
    }

    /// 换缓动曲线
    pub const fn easing(mut self, easing: Easing) -> Self {
        self.easing = easing;
        self
    }

    /// 匀速
    pub const fn linear(self) -> Self {
        self.easing(Easing::Linear)
    }

    /// 使用 [`Easing::EASE_IN`]
    pub const fn ease_in(self) -> Self {
        self.easing(Easing::EASE_IN)
    }

    /// 使用 [`Easing::EASE_OUT`]
    pub const fn ease_out(self) -> Self {
        self.easing(Easing::EASE_OUT)
    }

    /// 使用 [`Easing::EASE_IN_OUT`]
    pub const fn ease_in_out(self) -> Self {
        self.easing(Easing::EASE_IN_OUT)
    }
}

/// 时长换成动画每帧求值用的秒（`f32`）：按毫秒写的时长和直接写的秒一模一样
pub(crate) fn seconds(duration: Duration) -> f32 {
    duration.as_secs_f64() as f32
}

/// 动画曲线：补间或弹簧，两者都可以 `into()` 成它
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Curve {
    /// 固定时长的补间
    Tween(Tween),
    /// 物理弹簧，没有固定时长
    Spring(Spring),
}

impl From<Tween> for Curve {
    fn from(tween: Tween) -> Self {
        Self::Tween(tween)
    }
}

impl From<Spring> for Curve {
    fn from(spring: Spring) -> Self {
        Self::Spring(spring)
    }
}

/// 三次贝塞尔一个坐标的多项式系数：B(t) = ((a t + b) t + c) t，端点为 0 和 1
fn coefficients(p1: f32, p2: f32) -> (f32, f32, f32) {
    let c = 3.0 * p1;
    let b = 3.0 * (p2 - p1) - c;
    (1.0 - c - b, b, c)
}

/// 求 x(t) = x 的 t（`x` 在 0..1 之间）。牛顿迭代，不收敛时退回二分
fn solve(x1: f32, x2: f32, x: f32) -> f32 {
    let (ax, bx, cx) = coefficients(x1, x2);
    let sample_x = |t: f32| ((ax * t + bx) * t + cx) * t;
    let slope_x = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;

    let mut t = x;
    for _ in 0..8 {
        let error = sample_x(t) - x;
        if error.abs() < 1e-6 {
            return t;
        }
        let slope = slope_x(t);
        if slope.abs() < 1e-6 {
            break;
        }
        t -= error / slope;
    }

    let (mut lo, mut hi) = (0.0, 1.0);
    t = x;
    for _ in 0..32 {
        let value = sample_x(t);
        if (value - x).abs() < 1e-6 {
            break;
        }
        if value < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) * 0.5;
    }
    t
}

/// 求 x(t) = x 的 t，再返回 y(t)
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let t = solve(x1, x2, x);
    let (ay, by, cy) = coefficients(y1, y2);
    ((ay * t + by) * t + cy) * t
}

/// dy/dx 在 x 处的值：y'(t) / x'(t)。x'(t) 接近 0（端点处控制点的 x 是 0 或 1）时用很小的一段差分
fn cubic_bezier_slope(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let t = if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else {
        solve(x1, x2, x)
    };
    let (ax, bx, cx) = coefficients(x1, x2);
    let (ay, by, cy) = coefficients(y1, y2);
    let dx = (3.0 * ax * t + 2.0 * bx) * t + cx;
    let dy = (3.0 * ay * t + 2.0 * by) * t + cy;
    if dx.abs() > 1e-4 {
        return dy / dx;
    }
    const H: f32 = 1e-3;
    let (a, b) = ((x - H).max(0.0), (x + H).min(1.0));
    (cubic_bezier(x1, y1, x2, y2, b) - cubic_bezier(x1, y1, x2, y2, a)) / (b - a)
}
