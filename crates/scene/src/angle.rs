//! 角度和数字：角度一律带单位写（[`deg`]、[`rad`]、[`turns`]），不从裸数转换；内部存弧度。

use std::ops::{Add, Div, Mul, Neg, Sub};

/// 能当长度、角度、时长的数字（让 `px(200)` 和 `px(200.0)`、`deg(90)` 和 `deg(90.0)`、`ms(300)` 和 `ms(16.5)` 都能写）
pub trait Number: Copy {
    /// 转成 `f32`
    fn to_f32(self) -> f32;
    /// 转成 `f64`
    fn to_f64(self) -> f64;
}

macro_rules! numbers {
    ($($t:ty),*) => {
        $(
            impl Number for $t {
                fn to_f32(self) -> f32 {
                    self as f32
                }

                fn to_f64(self) -> f64 {
                    self as f64
                }
            }
        )*
    };
}

numbers!(f32, f64, i32, u32);

/// 角度：旋转、渐变的方向、颜色的色相。写的地方必须写出单位：[`deg`]`(90)`、[`rad`]`(PI / 4.0)`、[`turns`]`(0.25)`，
/// 读出来用 [`Angle::radians`]、[`Angle::degrees`]、[`Angle::turns`]。内部存弧度（`f64`：`deg(90).degrees()` 正好是 90，
/// 换算来回不丢 `f32` 的精度）
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct Angle(f64);

impl Angle {
    /// 零度
    pub const ZERO: Angle = Angle(0.0);

    /// 弧度
    pub fn radians(self) -> f32 {
        self.0 as f32
    }

    /// 度
    pub fn degrees(self) -> f32 {
        self.0.to_degrees() as f32
    }

    /// 圈数：一圈是 1
    pub fn turns(self) -> f32 {
        (self.0 / std::f64::consts::TAU) as f32
    }
}

/// 度：`deg(90)` 是直角
pub fn deg(degrees: impl Number) -> Angle {
    Angle(f64::from(degrees.to_f32()).to_radians())
}

/// 弧度：`rad(PI)` 是半圈
pub fn rad(radians: impl Number) -> Angle {
    Angle(f64::from(radians.to_f32()))
}

/// 圈数：`turns(0.25)` 是直角，`turns(1)` 是一整圈
pub fn turns(turns: impl Number) -> Angle {
    Angle(f64::from(turns.to_f32()) * std::f64::consts::TAU)
}

impl Neg for Angle {
    type Output = Angle;

    fn neg(self) -> Angle {
        Angle(-self.0)
    }
}

impl Add for Angle {
    type Output = Angle;

    fn add(self, other: Angle) -> Angle {
        Angle(self.0 + other.0)
    }
}

impl Sub for Angle {
    type Output = Angle;

    fn sub(self, other: Angle) -> Angle {
        Angle(self.0 - other.0)
    }
}

/// 放大几倍：`deg(30) * 2.0`
impl Mul<f32> for Angle {
    type Output = Angle;

    fn mul(self, times: f32) -> Angle {
        Angle(self.0 * f64::from(times))
    }
}

/// 分成几份：`turns(1) / 6.0`
impl Div<f32> for Angle {
    type Output = Angle;

    fn div(self, parts: f32) -> Angle {
        Angle(self.0 / f64::from(parts))
    }
}
