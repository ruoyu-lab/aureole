//! 属性的来源和覆盖层：节点上每个属性的值从哪来（[`Source`]），临时盖掉一个值（[`Tree::override_prop`]）。
//!
//! - 挂载时记下元素设了哪些属性、其中哪些是绑定、哪些是样式对象设的：三个按编号的位集，放在节点不常读的那一半，
//!   不用分配。正在动画、继承、覆盖着这几种在查询时现算。
//! - 覆盖层：被覆盖的属性立刻写成覆盖的值（不做动画）。之后绑定、样式对象的状态再给新值时不写进节点，
//!   记下来；撤掉覆盖时写回记下的值：覆盖前读出来的，或者覆盖期间绑定给的最新值。
//!   读不回来（网格轨道、图片来源）又没有新值的，撤掉时回到默认值。
//! - 遮罩（要挂一个元素）、布局位置（布局算出来的）不能覆盖。时间线开始播放时会接管被覆盖的属性，
//!   播放期间按时间线走，停下以后回到覆盖的值。

use std::collections::HashMap;

use super::{NodeId, Tree};
use crate::property::registry::{AnyValue, PropId, PropSet, PropType, Props, Slot, ValueSource};

/// 节点上设了的属性（挂载时从建造者记下）
#[derive(Clone, Copy, Default)]
pub(super) struct Sources {
    set: PropSet,
    bound: PropSet,
    span: PropSet,
}

impl Sources {
    pub(super) fn of(props: &Props) -> Self {
        let mut sources = Self::default();
        for entry in props.entries() {
            sources.set.insert(entry.id);
            if entry.span {
                sources.span.insert(entry.id);
            } else if !matches!(entry.slot, Slot::Static(_)) {
                sources.bound.insert(entry.id);
            }
        }
        sources
    }

    /// 设在了不适用的元素上，没有绑定
    pub(super) fn forget(&mut self, prop: PropId) {
        self.set.remove(prop);
        self.bound.remove(prop);
        self.span.remove(prop);
    }
}

/// 被覆盖着的属性：撤掉覆盖时回到的值（`None` 是读不回来，回到默认值）
pub(super) type Overrides = HashMap<(NodeId, PropId), Option<AnyValue>>;

impl Tree {
    /// 节点上一个属性此刻的值从哪来。节点不在时是 `None`。
    /// 先后：覆盖着、正在动画、样式对象设的、绑定、静态值、从祖先继承、默认值
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) fn prop_source(&self, id: NodeId, prop: PropId) -> Option<ValueSource> {
        let node = self.nodes.get(id)?;
        if !self.overrides.is_empty() && self.overrides.contains_key(&(id, prop)) {
            return Some(ValueSource::Override);
        }
        let animating = node.animations.iter().any(|s| s.property == prop && s.queued);
        if animating || (node.motion & super::timeline::TAKEN != 0 && self.motion.is_taken(id, prop)) {
            return Some(ValueSource::Animated);
        }
        let sources = &self.nodes.cold(id)?.sources;
        Some(if sources.span.contains(prop) {
            ValueSource::Style
        } else if sources.bound.contains(prop) {
            ValueSource::Bound
        } else if sources.set.contains(prop) {
            ValueSource::Static
        } else if prop.metadata().inherited && self.ancestors(id).any(|a| self.nodes.cold(a).is_some_and(|c| c.sources.set.contains(prop))) {
            ValueSource::Inherited
        } else {
            ValueSource::Default
        })
    }

    /// 节点上不是默认值的属性和它们的来源，按编号
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) fn prop_sources(&self, id: NodeId) -> Vec<(PropId, ValueSource)> {
        PropId::all().filter_map(|p| Some((p, self.prop_source(id, p)?))).filter(|(_, s)| *s != ValueSource::Default).collect()
    }

    /// 覆盖层：把节点上一个属性临时换成 `value`（立刻生效，不做动画），盖过链式方法、绑定和样式对象；
    /// 再覆盖一次换成新的值。`None` 撤掉覆盖，回到底下的值（覆盖期间绑定给过新值的，回到最新的）。
    /// 值的类型不对、节点不在、属性不能覆盖（遮罩、布局位置）、要撤掉的没被覆盖时什么都不做，返回 `false`
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) fn override_prop(&mut self, id: NodeId, prop: PropId, value: Option<AnyValue>) -> bool {
        if !self.nodes.contains_key(id) {
            return false;
        }
        let meta = prop.metadata();
        match value {
            Some(value) => {
                if matches!(prop, PropId::MASK | PropId::LAYOUT_POSITION) || !meta.accepts(&value) {
                    return false;
                }
                if !self.overrides.contains_key(&(id, prop)) {
                    let base = (meta.read)(self, id);
                    self.overrides.insert((id, prop), base);
                }
                super::anim::instantly(|| (meta.write)(self, id, &value))
            }
            None => {
                let Some(base) = self.overrides.remove(&(id, prop)) else { return false };
                super::anim::instantly(|| match base {
                    Some(base) => {
                        (meta.write)(self, id, &base);
                    }
                    None => (meta.reset)(self, id),
                });
                true
            }
        }
    }

    /// 被覆盖着的属性
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    pub(crate) fn is_overridden(&self, id: NodeId, prop: PropId) -> bool {
        !self.overrides.is_empty() && self.overrides.contains_key(&(id, prop))
    }

    /// 绑定要写新值时先过这里：属性被覆盖着就把新值记成撤掉覆盖后回到的值，返回 `None`（不写）；否则原样还回去
    #[inline]
    pub(crate) fn hold_back<T: PropType>(&mut self, id: NodeId, prop: PropId, value: T) -> Option<T> {
        if self.overrides.is_empty() {
            return Some(value);
        }
        match self.overrides.get_mut(&(id, prop)) {
            Some(base) => {
                *base = Some(AnyValue::new(value));
                None
            }
            None => Some(value),
        }
    }

    /// 卸载了的节点上的覆盖一起去掉
    pub(super) fn forget_overrides(&mut self) {
        if !self.overrides.is_empty() {
            let nodes = &self.nodes;
            self.overrides.retain(|(id, _), _| nodes.contains_key(*id));
        }
    }

    /// 设在了不适用的元素上的属性不算设了
    pub(super) fn forget_source(&mut self, id: NodeId, prop: PropId) {
        if let Some(cold) = self.nodes.cold_mut(id) {
            cold.sources.forget(prop);
        }
    }

    /// 父节点、祖父节点……直到根
    #[allow(dead_code, reason = "属性系统按名字读写、覆盖、查来源的底层能力，现在只有测试用")]
    fn ancestors(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self.nodes.get(id).and_then(|n| n.parent), |&p| self.nodes.get(p).and_then(|n| n.parent))
    }
}
