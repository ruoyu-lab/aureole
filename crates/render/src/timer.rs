use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

const SLOTS: usize = 4;
const FREE: u8 = 0;
/// 时间戳已经写了，等下一帧的命令里取出
const WRITTEN: u8 = 1;
/// 已经取出，等回读
const IN_FLIGHT: u8 = 2;
const READY: u8 = 3;

/// 用时间戳查询测量一帧（可能有多个渲染通道）的 GPU 耗时：
/// 第一个渲染通道开始时、最后一个结束时各写一个时间戳。
///
/// Apple GPU 只能在渲染通道的边界写时间戳，而"片元结束"的时间戳在同一个命令缓冲里
/// 还读不到，所以留到下一帧的命令开头再取出。回读也是异步的：结果晚一两帧才拿到。
/// 几组查询轮流使用，全部在途时本帧不计时，绝不让 CPU 等 GPU。
pub struct GpuTimer {
    slots: Vec<Slot>,
    /// 这一帧写时间戳的那组
    current: Option<usize>,
    /// 上一帧写了、还没取出的那组
    written: Option<usize>,
    /// 在这一帧的命令里取出的那组
    resolving: Option<usize>,
    period_ns: f64,
}

struct Slot {
    query_set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    state: Arc<AtomicU8>,
}

impl GpuTimer {
    /// 设备没开启 `TIMESTAMP_QUERY` 时返回 `None`
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            return None;
        }
        let slots = (0..SLOTS)
            .map(|_| Slot {
                query_set: device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("gpu timer"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                }),
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("gpu timer resolve"),
                    size: 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("gpu timer readback"),
                    size: 16,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(FREE)),
            })
            .collect();
        Some(Self { slots, current: None, written: None, resolving: None, period_ns: queue.get_timestamp_period() as f64 })
    }

    /// 每帧录制命令之前调用：先取出上一帧的时间戳，再挑一组给这一帧。
    /// 返回这一帧要写时间戳的查询集：第一个渲染通道开始时写下标 0，最后一个结束时写下标 1
    pub fn begin_frame(&mut self, encoder: &mut wgpu::CommandEncoder) -> Option<wgpu::QuerySet> {
        if let Some(i) = self.written.take() {
            let slot = &self.slots[i];
            encoder.resolve_query_set(&slot.query_set, 0..2, &slot.resolve, 0);
            encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.readback, 0, 16);
            self.resolving = Some(i);
        }
        self.current = self.slots.iter().position(|slot| slot.state.load(Ordering::Acquire) == FREE);
        self.current.map(|i| self.slots[i].query_set.clone())
    }

    /// 提交后调用：发起上一帧时间戳的异步回读，记下这一帧写了哪组
    pub fn after_submit(&mut self) {
        if let Some(i) = self.resolving.take() {
            let slot = &self.slots[i];
            let state = slot.state.clone();
            state.store(IN_FLIGHT, Ordering::Release);
            slot.readback.map_async(wgpu::MapMode::Read, .., move |result| {
                state.store(if result.is_ok() { READY } else { FREE }, Ordering::Release);
            });
        }
        if let Some(i) = self.current.take() {
            self.slots[i].state.store(WRITTEN, Ordering::Release);
            self.written = Some(i);
        }
    }

    /// 取回最近一次已完成的测量；不阻塞
    pub fn collect(&self, device: &wgpu::Device) -> Option<Duration> {
        let _ = device.poll(wgpu::PollType::Poll);
        let mut latest = None;
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) != READY {
                continue;
            }
            if let Ok(view) = slot.readback.get_mapped_range(..) {
                let ticks: &[u64] = bytemuck::cast_slice(&view);
                if ticks[1] > ticks[0] {
                    latest = Some(Duration::from_secs_f64((ticks[1] - ticks[0]) as f64 * self.period_ns / 1e9));
                }
            }
            slot.readback.unmap();
            slot.state.store(FREE, Ordering::Release);
        }
        latest
    }
}
