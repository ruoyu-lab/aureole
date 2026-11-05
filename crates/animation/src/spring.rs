//! 弹簧的解析解：阻尼谐振子 x'' + 2ζω x' + ω² x = 0 从 (x0, v0) 出发经过一段时间以后的位移和速度。
//! 弹簧轨道（[`crate::SpringTrack`]）、关键帧的弹簧段都用它。

use crate::curve::Spring;

/// 阻尼谐振子的解析解：对任意时长都精确、稳定，卡顿很久也不发散。
/// 系数只依赖弹簧和时长，算一次，所有分量共用。
pub(crate) enum SpringStep {
    /// 欠阻尼：x(t) = e^{-ζω t} (A cos ω_d t + B sin ω_d t)
    Under { decay: f32, cos: f32, sin: f32, zeta_omega: f32, omega_d: f32 },
    /// 临界阻尼：x(t) = e^{-ω t} (x0 + (v0 + ω x0) t)
    Critical { decay: f32, omega: f32, dt: f32 },
    /// 过阻尼：两个实指数之和
    Over { r1: f32, r2: f32, e1: f32, e2: f32 },
}

impl SpringStep {
    pub(crate) fn new(spring: Spring, dt: f32) -> Self {
        let omega = spring.omega();
        let zeta = spring.damping_ratio.max(0.0);
        if (zeta - 1.0).abs() < 1e-4 {
            Self::Critical { decay: (-omega * dt).exp(), omega, dt }
        } else if zeta < 1.0 {
            let omega_d = omega * (1.0 - zeta * zeta).sqrt();
            let (sin, cos) = (omega_d * dt).sin_cos();
            Self::Under { decay: (-zeta * omega * dt).exp(), cos, sin, zeta_omega: zeta * omega, omega_d }
        } else {
            let root = (zeta * zeta - 1.0).sqrt();
            let r1 = -omega * (zeta - root);
            let r2 = -omega * (zeta + root);
            Self::Over { r1, r2, e1: (r1 * dt).exp(), e2: (r2 * dt).exp() }
        }
    }

    /// 输入相对目标的位移和速度，返回 dt 之后的位移和速度
    /// 从相对目标的位移、速度出发，经过这段时长以后的位移写回 `x`（只要位置时用，每个分量少算速度）
    #[inline]
    pub(crate) fn positions(&self, x: &mut [f32], v: &[f32]) {
        match *self {
            Self::Under { decay, cos, sin, zeta_omega, omega_d } => {
                for (x, v0) in x.iter_mut().zip(v) {
                    let b = (v0 + zeta_omega * *x) / omega_d;
                    *x = decay * (*x * cos + b * sin);
                }
            }
            Self::Critical { decay, omega, dt } => {
                for (x, v0) in x.iter_mut().zip(v) {
                    let c = v0 + omega * *x;
                    *x = decay * (*x + c * dt);
                }
            }
            Self::Over { r1, r2, e1, e2 } => {
                for (x, v0) in x.iter_mut().zip(v) {
                    let c2 = (v0 - r1 * *x) / (r2 - r1);
                    *x = (*x - c2) * e1 + c2 * e2;
                }
            }
        }
    }

    pub(crate) fn apply(&self, x0: f32, v0: f32) -> (f32, f32) {
        match *self {
            Self::Under { decay, cos, sin, zeta_omega, omega_d } => {
                let a = x0;
                let b = (v0 + zeta_omega * x0) / omega_d;
                let x = decay * (a * cos + b * sin);
                let v = decay * (v0 * cos - (a * omega_d + zeta_omega * b) * sin);
                (x, v)
            }
            Self::Critical { decay, omega, dt } => {
                let c = v0 + omega * x0;
                (decay * (x0 + c * dt), decay * (v0 - omega * c * dt))
            }
            Self::Over { r1, r2, e1, e2 } => {
                let c2 = (v0 - r1 * x0) / (r2 - r1);
                let c1 = x0 - c2;
                (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
            }
        }
    }
}
