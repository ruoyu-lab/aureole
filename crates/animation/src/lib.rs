//! 动画曲线与插值器：纯数据和数学，不依赖 UI、响应式和渲染。
//!
//! 曲线参数都是普通的 `Copy` 结构体，插值器只操作定长的 `f32` 分量，
//! 可以原样交给渲染线程执行。
//!
//! - [`Spring`]：弹簧，用响应时间和阻尼比描述，没有固定时长，被打断时运动连续
//! - [`Tween`]：固定时长的补间，配合 [`Easing`] 缓动曲线，可以循环或往返
//! - [`Curve`]：两者之一，`aureole` 里声明属性动画时传它
//! - [`KeyframeSeries`]、[`Timing`]、[`Playhead`]：一串关键帧、时间线的排期、把时钟时刻换算成时间线位置的播放头，都是时刻的纯函数
//! - [`Track`]、[`Clock`]：按时间求值的轨道和两条线程共用的时钟。轨道有弹簧或补间（[`CurveTrack`]：[`SpringTrack`]、
//!   [`TweenTrack`]）和时间线上的关键帧（[`KeyframePlayback`]，几段 [`KeyframePlacement`]）。动画中的值最多
//!   [`MAX_COMPONENTS`] 个分量
//!
//! 轨道的值是时刻的纯函数，不逐帧累加：`aureole` 的声明的过渡、退场、时间线、合成层动画、共享元素的飞行都是轨道，
//! 主线程和渲染线程按同一个时钟时刻采样同一条轨道，算出同一个值。
//!
//! ```
//! use std::time::Duration;
//!
//! use aureole_animation::{CurveTrack, Spring, Track, TrackState, Tween};
//!
//! // 弹簧：时钟的第 0 秒从 0 奔向 100；按 120 帧每秒的时刻采样，直到停稳
//! let mut x = CurveTrack::new(Spring::snappy(), &[0.0], 0.01);
//! x.retarget(0.0, &[100.0]);
//! let mut out = [0.0];
//! let mut frames = 0;
//! while x.sample(f64::from(frames) / 120.0, &mut out) == TrackState::Moving {
//!     frames += 1;
//! }
//! assert_eq!(out, [100.0]);
//! assert!(frames < 120);
//!
//! // 补间：200 毫秒，先快后慢；两个分量一起插值。任意时刻都能直接问
//! let mut pos = CurveTrack::new(Tween::new(Duration::from_millis(200)).ease_out(), &[0.0, 0.0], 0.01);
//! pos.retarget(0.0, &[10.0, 20.0]);
//! let mut out = [0.0; 2];
//! pos.sample(0.1, &mut out);
//! assert!(out[0] > 5.0); // 过了一半时间，已经走过一半以上
//! assert_eq!(pos.sample(0.2, &mut out), TrackState::Settled);
//! ```

// 库里只用 `tracing` 报告情况，不直接往终端写（测试、示例、演示、工具 crate 不受这条限制）
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

mod curve;
mod keyframes;
mod spring;
mod track;

/// 一个动画值最多的分量数（例如投影：x、y、模糊、颜色 4 个分量 = 7）
pub const MAX_COMPONENTS: usize = 8;

pub use curve::{Curve, Easing, Repeat, Spring, Tween};
pub use keyframes::{KeyCurve, KeyframeSeries, Playhead, Timing, local_time, local_time_rate};
pub use track::{Clock, CurveTrack, KeyframePlacement, KeyframePlayback, TrackState, SpringTrack, Track, TrackInfo, TrackKind, TweenTrack};
