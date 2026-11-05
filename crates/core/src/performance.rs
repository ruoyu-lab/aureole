//! 每帧的耗时：主线程每帧推进用了多久、渲染线程每次绘制用了多久（渲染线程报告过来）。
//!
//! 性能和内存的其余测量（本进程的资源用量、渲染器分配的显存、选中的 GPU、从启动到第一帧上屏的时间）要问系统和渲染器，
//! 在运行时（`aureole-runtime`）里；门面 `aureole::performance` 把两边合在一起，例子见那里。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 记录下来的每帧耗时
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameTimes {
    /// 主线程每帧：推进滚动和动画、布局、生成图元、交给渲染线程
    pub main: Vec<Duration>,
    /// 渲染线程每次绘制：收下消息、推进合成层和滚动、上传、编码、提交（不含等交换链的时间）
    pub render: Vec<Duration>,
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static FRAMES: Mutex<FrameTimes> = Mutex::new(FrameTimes { main: Vec::new(), render: Vec::new() });

/// 开始（`true`）或停止记录每帧的耗时。不记录时只多读一个原子量
pub fn set_frame_recording(on: bool) {
    RECORDING.store(on, Ordering::Relaxed);
}

/// 取走记录下来的每帧耗时（之后从空的开始记）
pub fn take_frame_times() -> FrameTimes {
    FRAMES.lock().map(|mut frames| std::mem::take(&mut *frames)).unwrap_or_default()
}

/// 样本的分位数（`p` 在 0 到 1 之间，0.5 是中位数），没有样本时为 0
pub fn percentile(samples: &[Duration], p: f32) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[((sorted.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize]
}

/// 主线程：一帧从 `start` 开始，到现在结束
pub(crate) fn record_main(start: Instant) {
    if RECORDING.load(Ordering::Relaxed)
        && let Ok(mut frames) = FRAMES.lock()
    {
        frames.main.push(start.elapsed());
    }
}

/// 渲染线程：一次绘制用了 `took`
pub fn record_render(took: Duration) {
    if RECORDING.load(Ordering::Relaxed)
        && let Ok(mut frames) = FRAMES.lock()
    {
        frames.render.push(took);
    }
}
