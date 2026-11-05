//! 交给渲染线程的东西：自上次以来合成层的变化（动画的新轨道、额外平移、卸载的层、时间线、共享元素的飞行），
//! 发出去的帧号，以及渲染线程在主线程卡住时推进到的滚动状态（从这里收回来）。
//!
//! 字段私有：各处只通过方法记下变化，每帧由 [`super::Tree::take_layer_updates`] 一起取走。

use std::mem;
use std::sync::Arc;

use animation::CurveTrack;

use crate::compositor::{Handback, LayerTimeline, LayerTrack, LayerUpdates, Snapshot};

#[derive(Default)]
pub(super) struct Commit {
    /// 要发给渲染线程的合成层变化
    updates: LayerUpdates,
    /// 发给渲染线程的第几帧
    sent: u64,
    /// 交给渲染线程的动画时钟是第几版（[`crate::clock::version`]）：时钟改了（暂停、慢放、拖动）要再发一帧
    clock: Option<u64>,
    /// 主线程卡住时渲染线程推进到的滚动状态，从这里收回来
    handback: Arc<Handback>,
}

impl Commit {
    /// 层的新额外平移（布局位置的过渡、滚动、浮层的位置、拖放）
    pub(super) fn offset(&mut self, key: u64, offset: [f32; 2]) {
        self.updates.offsets.push((key, offset));
    }

    /// 半透明元素的透明度静态地改了（还是半透明）：只改层的透明度，不重建图元和层表
    pub(super) fn opacity(&mut self, key: u64, opacity: f32) {
        self.updates.opacities.push((key, opacity));
    }

    /// 刚生成了显示列表：之前给的额外平移、透明度都已经写进新的层表
    pub(super) fn clear_offsets(&mut self) {
        self.updates.offsets.clear();
        self.updates.opacities.clear();
    }

    /// 合成层动画的轨道（改了目标、跳到新值、从时间线接手）：渲染线程整个换掉自己那条
    pub(super) fn track(&mut self, track: LayerTrack) {
        self.updates.tracks.push(track);
    }

    pub(super) fn tracks(&mut self, tracks: impl IntoIterator<Item = LayerTrack>) {
        self.updates.tracks.extend(tracks);
    }

    /// 卸载了的层：渲染线程那边的动画一起丢掉
    pub(super) fn remove(&mut self, key: u64) {
        self.updates.removed.push(key);
    }

    pub(super) fn remove_all(&mut self, keys: impl IntoIterator<Item = u64>) {
        self.updates.removed.extend(keys);
    }

    /// 共享元素开始飞（或者飞到一半换了方向）
    pub(super) fn fly(&mut self, key: u64, flight: CurveTrack) {
        self.updates.flights.push((key, flight));
    }

    /// 时间线的轨道和播放头
    pub(super) fn timeline(&mut self, timeline: LayerTimeline) {
        self.updates.timelines.push(timeline);
    }

    /// 取走这一帧的变化，记上帧号、宿主时刻和动画时钟
    pub(super) fn take(&mut self, time: f64) -> LayerUpdates {
        self.sent += 1;
        let mut updates = mem::take(&mut self.updates);
        updates.time = time;
        updates.frame = self.sent;
        updates.clock = crate::clock::clock();
        self.clock = Some(crate::clock::version());
        updates
    }

    /// 动画时钟改过，渲染线程还不知道
    pub(super) fn clock_changed(&self) -> bool {
        self.clock != Some(crate::clock::version())
    }

    /// 合成层没有变化要交给渲染线程
    pub(super) fn idle(&self) -> bool {
        let u = &self.updates;
        u.tracks.is_empty()
            && u.timelines.is_empty()
            && u.flights.is_empty()
            && u.removed.is_empty()
            && u.offsets.is_empty()
            && u.opacities.is_empty()
            && !self.clock_changed()
    }

    /// 发给渲染线程的第几帧
    pub(super) fn sent(&self) -> u64 {
        self.sent
    }

    /// 渲染线程交回滚动状态的地方（创建渲染线程时交给它）
    pub(super) fn handback(&self) -> Arc<Handback> {
        self.handback.clone()
    }

    /// 取走渲染线程交回的最新状态
    pub(super) fn take_handback(&self) -> Option<Snapshot> {
        self.handback.take()
    }
}
