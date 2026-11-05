//! 测试读的状态：回放快照（见 `testing/replay.rs`）读的指针下的元素、按下的元素、浮层、输入框、滚动视口，
//! 都按元素名（[`crate::Element::name`]）认，没起名字的元素记成 `?`；
//! 元素树的快照和元素的详细信息（`Harness::element_tree`、`Harness::element_info`）。

use std::collections::HashSet;

use animation::{Curve, Track};
use taffy::{Display, FlexDirection};

use super::{DragOutRequest, NodeId, Tree, TreeRef};
use crate::geometry::Rect;
use crate::host::WindowTree;
use crate::input::ImeRequest;
use crate::interaction::Cursor;
use crate::kinds::NextFrame;
use crate::property::Property;
use crate::kinds::KindInfo;
use crate::text::TextBlock;
use crate::testing::{AnimationInfo, Edges, ElementFlags, ElementInfo, ElementNode, ElementTree, PaintInfo};

/// 测试工具读元素树的窄接口：元素树只有这一个口子给 `aureole-testing`。输入和帧照样走公开的
/// [`crate::host::WindowHost`]，这里只有读状态（名字、位置、文字、焦点、合成层、统计），和测试才要的两三样
/// （丢掉字形缓存、取走还没交出去的请求）。不稳定，不属于公开 API
#[doc(hidden)]
#[derive(Clone)]
pub struct TreeProbe(TreeRef);

/// 一行一个方法：借用元素树，转给同名的方法
macro_rules! probes {
    ($($borrow:ident { $(fn $name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)* })*) => {
        #[doc(hidden)]
        impl TreeProbe {
            $($(
                pub fn $name(&self, $($arg: $ty),*) $(-> $ret)? {
                    self.0.$borrow().$name($($arg),*)
                }
            )*)*
        }
    };
}

probes! {
    borrow {
        fn viewport() -> [f32; 2];
        fn scale() -> f32;
        fn text_atlas_usage(kind: scene::AtlasKind) -> (u32, f32);
        fn wants_frames() -> bool;
        fn next_frame() -> NextFrame;
        fn bounds(name: &str) -> Option<Rect>;
        fn text_content(name: &str) -> Option<String>;
        fn link_bounds(name: &str, k: usize) -> Option<Rect>;
        fn image_frame(name: &str) -> Option<usize>;
        fn shaped_paragraph_count() -> usize;
        fn generated_glyph_count() -> usize;
        fn ime_area() -> Option<Rect>;
        fn ime_request() -> Option<ImeRequest>;
        fn text_state(name: &str) -> Option<String>;
        fn scroll_offset(name: &str) -> Option<[f32; 2]>;
        fn cursor() -> Cursor;
        fn focused_name() -> Option<String>;
        fn focus_label() -> Option<String>;
        fn is_focus_ring_visible() -> bool;
        fn hovered_names() -> Vec<String>;
        fn pressed_name() -> Option<String>;
        fn overlay_names() -> Vec<String>;
        fn window_bounds(name: &str) -> Option<Rect>;
        fn window_corners(name: &str) -> Option<[[f32; 2]; 4]>;
        fn element_tree() -> ElementTree;
        fn element_info(name: &str) -> Option<ElementInfo>;
    }
    borrow_mut {
        fn layer_transforms() -> Vec<scene::LayerTransform>;
        fn take_drag_out() -> Option<DragOutRequest>;
        fn forget_glyphs();
        fn accessibility_update() -> Option<accesskit::TreeUpdate>;
    }
}

#[doc(hidden)]
impl TreeProbe {
    /// 读这棵树
    pub fn new(tree: &WindowTree) -> Self {
        Self(tree.0.clone())
    }

    /// 元素树此刻没被借用：调用窗口的另一半时必须是这样（系统可能在调用里同步送回事件）
    pub fn is_released(&self) -> bool {
        self.0.try_borrow_mut().is_ok()
    }

    /// 命名元素在无障碍树里的编号
    pub fn accessibility_id(&self, name: &str) -> Option<accesskit::NodeId> {
        self.0.borrow().named(name).map(super::accessibility_id)
    }
}

impl Tree {
    fn name_of(&self, id: NodeId) -> Option<&str> {
        self.nodes.name(id)
    }

    /// 有焦点的元素：没起名字时是 `? in 最近的起了名字的祖先`（多行输入框、数字输入的焦点在里面的输入框上）
    pub(crate) fn focus_label(&self) -> Option<String> {
        let id = self.router.focused()?;
        if let Some(name) = self.name_of(id) {
            return Some(name.to_owned());
        }
        let mut current = self.nodes.get(id).and_then(|n| n.parent);
        while let Some(ancestor) = current {
            if let Some(name) = self.name_of(ancestor) {
                return Some(format!("? in {name}"));
            }
            current = self.nodes.get(ancestor).and_then(|n| n.parent);
        }
        Some("?".to_owned())
    }

    /// 指针下起了名字的元素，从外到内
    pub(crate) fn hovered_names(&self) -> Vec<String> {
        self.router.hovered().iter().filter_map(|&id| self.name_of(id)).map(str::to_owned).collect()
    }

    /// 按下了、松开时会收到点击的元素（按下处最内层有 `on_click`、没被禁用的元素）
    pub(crate) fn pressed_name(&self) -> Option<String> {
        let press = self.router.press()?;
        let id = self.innermost_listening(&press.path, crate::handlers::EventType::Click)?;
        Some(self.name_of(id).unwrap_or("?").to_owned())
    }

    /// 命名元素（或者它里面第一个输入框、第一段文字）的状态：输入框是内容、选区（全文的字节偏移，锚点..焦点）和预编辑文字，
    /// 可选中的文字是选中的部分
    pub(crate) fn text_state(&self, name: &str) -> Option<String> {
        let mut stack = vec![self.named(name)?];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            if let Some(block) = self.nodes.kind::<TextBlock>(id) {
                let selected = block.selected_text().map(|s| format!(" selected {s:?}")).unwrap_or_default();
                return Some(format!("{:?}{selected}", block.content()));
            }
            if let Some(editor) = self.nodes.kind::<crate::editor::EditorBlock>(id) {
                let (text, anchor, focus, preedit) = editor.snapshot();
                let preedit = preedit.map(|p| format!(" preedit {p:?}")).unwrap_or_default();
                return Some(format!("{text:?} {anchor}..{focus}{preedit}"));
            }
            stack.extend(node.children.iter().rev());
        }
        None
    }

    /// 命名元素（或者它里面第一个滚动视口）的滚动偏移
    pub(crate) fn scroll_offset(&self, name: &str) -> Option<[f32; 2]> {
        let mut stack = vec![self.named(name)?];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            if let Some(scroll) = node.scroll.as_deref() {
                return Some(scroll.offset);
            }
            stack.extend(node.children.iter().rev());
        }
        None
    }

    /// 开着的浮层，从下到上：声明它的元素的名字，正在关的后面加 `~`
    pub(crate) fn overlay_names(&self) -> Vec<String> {
        self.overlay_owners()
            .into_iter()
            .map(|(owner, closing)| format!("{}{}", self.name_of(owner).unwrap_or("?"), if closing { "~" } else { "" }))
            .collect()
    }

    /// 元素树的快照：根元素，然后是开着的浮层
    pub(crate) fn element_tree(&self) -> ElementTree {
        let groups: HashSet<u64> = self.paint.layers().iter().filter(|l| l.group).map(|l| l.key).collect();
        let overlays: Vec<NodeId> = self.overlays.hosts().collect();
        let mut nodes = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<(NodeId, usize)> = self.nodes.roots().iter().chain(&overlays).rev().map(|&id| (id, 0)).collect();
        while let Some((id, depth)) = stack.pop() {
            let Some(node) = self.nodes.get(id) else { continue };
            nodes.push(self.describe(id, depth, &groups, &overlays));
            stack.extend(node.children.iter().rev().map(|&child| (child, depth + 1)));
        }
        ElementTree { nodes }
    }

    fn describe(&self, id: NodeId, depth: usize, groups: &HashSet<u64>, overlays: &[NodeId]) -> ElementNode {
        let node = &self.nodes[id];
        let content = self.nodes.element_kind(id);
        let kind = if overlays.contains(&id) {
            "overlay"
        } else if node.scroll.as_ref().is_some_and(|s| !s.own) {
            "scroll"
        } else {
            match content {
                Some(kind) => kind.name(),
                None => {
                    let style = self.layout.style(node.layout);
                    if style.display == Display::Grid {
                        "grid"
                    } else if node.children.is_empty() {
                        "rect"
                    } else if matches!(style.flex_direction, FlexDirection::Row | FlexDirection::RowReverse) {
                        "row"
                    } else {
                        "column"
                    }
                }
            }
        };
        let label = match content {
            Some(kind) => {
                let mut info = KindInfo::default();
                kind.describe(&mut info);
                info.summary
            }
            _ => None,
        };
        let key = super::layer_key(id);
        ElementNode {
            depth,
            kind,
            name: self.nodes.name(id).map(str::to_owned),
            summary: label,
            bounds: node.bounds,
            child_count: node.children.len(),
            flags: ElementFlags {
                composited: node.layer,
                grouped: groups.contains(&key),
                clipped: node.paint.clip,
                exiting: node.exiting,
                focused: self.router.focused() == Some(id),
                hovered: self.router.hovered().contains(&id),
                animating: node.animations.iter().any(|slot| slot.queued),
            },
        }
    }

    /// 命名元素此刻的详细信息
    pub(crate) fn element_info(&self, name: &str) -> Option<ElementInfo> {
        let id = self.named(name)?;
        let node = self.nodes.get(id)?;
        let groups: HashSet<u64> = self.paint.layers().iter().filter(|l| l.group).map(|l| l.key).collect();
        let overlays: Vec<NodeId> = self.overlays.hosts().collect();
        let depth = std::iter::successors(node.parent, |&p| self.nodes.get(p)?.parent).count();
        let layout = self.layout.try_get(node.layout)?;
        let edges = |r: taffy::Rect<f32>| Edges { top: r.top, right: r.right, bottom: r.bottom, left: r.left };
        let p = &node.paint;
        let font = self.nodes.kind::<TextBlock>(id).map(|block| [block.style().size, block.style().weight]);
        let scroll_offset = node.scroll.as_ref().map(|s| s.offset);
        let animations = node
            .animations
            .iter()
            .map(|slot| {
                let single = |values: &[f32]| (values.len() == 1).then(|| values[0]);
                let (value, target) = match &slot.track {
                    Some(track) if slot.mix.is_none() => {
                        // 这一帧的动画时间采样：和画在屏幕上的一样
                        let mut out = [0.0; animation::MAX_COMPONENTS];
                        track.sample(self.anim.time(), &mut out);
                        (single(&out[..track.len()]), single(track.target()))
                    }
                    _ => (None, None),
                };
                AnimationInfo {
                    property: Property::from_id(slot.property).expect("动画槽里的属性都能动画"),
                    running: slot.queued,
                    uses_spring: matches!(slot.curve, Curve::Spring(_)),
                    value,
                    target,
                }
            })
            .collect();
        Some(ElementInfo {
            node: self.describe(id, depth, &groups, &overlays),
            window_bounds: self.screen_rect(id).unwrap_or(node.bounds),
            margin: edges(layout.margin),
            padding: edges(layout.padding),
            paint: PaintInfo {
                background: p.background.clone(),
                color: p.color.clone(),
                radius: p.radius,
                border_width: p.border_width,
                border_color: p.border_fill(),
                shadow: p.shadows(),
                opacity: p.opacity,
                scale: p.scale,
                rotation: scene::rad(p.rotation),
                rotation_x: scene::rad(node.extra.as_deref().map_or(0.0, |e| e.rotate[0])),
                rotation_y: scene::rad(node.extra.as_deref().map_or(0.0, |e| e.rotate[1])),
                translation: p.translate,
                blur: p.blur,
                backdrop_blur: p.backdrop_blur,
                has_shader: p.shader > 0,
            },
            font,
            scroll_offset,
            stack_order: node.input.as_ref().map_or(0, |i| i.z),
            animations,
        })
    }
}
