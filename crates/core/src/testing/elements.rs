//! 测试读的元素树：测试工具的 `element_tree` 的快照（每个元素的种类、名字、文字、
//! 布局位置和标记）和 测试工具的 `element_info` 的详细信息（外边距、内边距、
//! 此刻的绘制属性、属性动画）。属性契约测试用它们核对一个链式方法改了布局还是只改了绘制。

use std::fmt;

use crate::property::Property;
use crate::geometry::Rect;

/// 元素的几个标记
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ElementFlags {
    /// 是合成层：变换、透明度动画由渲染线程推进
    pub composited: bool,
    /// 是分组：先画进离屏纹理再整体合成（半透明的子树、模糊）
    pub grouped: bool,
    /// 把子元素裁剪在自己的圆角矩形里
    pub clipped: bool,
    /// 正在退场（播完退场动画后卸载）
    pub exiting: bool,
    /// 有键盘焦点
    pub focused: bool,
    /// 在指针下
    pub hovered: bool,
    /// 有属性动画正在运动
    pub animating: bool,
}

/// 快照里的一个元素
#[derive(Clone, Debug, PartialEq)]
pub struct ElementNode {
    /// 在树里的深度，根元素是 0
    pub depth: usize,
    /// 种类的名字。带内容的元素是它的种类名（[`crate::kinds::ElementKind::name`]：`text`、`text_field`、
    /// `text_area`（多行输入框里的编辑区）、`path`、`image`、`char_grid`、`canvas`，自己定义的种类是它自己的名字）；
    /// 没有内容的：浮层的宿主 `overlay`，滚动视口 `scroll`（也包括虚拟列表和多行输入框外面的视口），网格 `grid`，
    /// 没有子元素的 `rect`，横排的 `row`，竖排的 `column`
    pub kind: &'static str,
    /// [`crate::Element::name`] 起的名字
    pub name: Option<String>,
    /// 简短的说明：文字的内容、图片的来源
    pub summary: Option<String>,
    /// 最近一次布局后的位置和大小（窗口坐标，逻辑像素，不含变换和滚动）
    pub bounds: Rect,
    /// 子元素个数
    pub child_count: usize,
    /// 标记
    pub flags: ElementFlags,
}

/// 元素树的快照，按树的顺序（父元素在前，子元素紧跟其后）。根元素之后是开着的浮层
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ElementTree {
    /// 所有元素
    pub nodes: Vec<ElementNode>,
}

impl ElementTree {
    /// 按名字找元素
    pub fn find(&self, name: &str) -> Option<&ElementNode> {
        self.nodes.iter().find(|n| n.name.as_deref() == Some(name))
    }
}

impl fmt::Display for ElementNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind)?;
        if let Some(name) = &self.name {
            write!(f, " #{name}")?;
        }
        if let Some(label) = &self.summary {
            write!(f, " {label:?}")?;
        }
        let b = self.bounds;
        write!(f, " {}x{} @{},{}", round(b.width), round(b.height), round(b.x), round(b.y))
    }
}

/// 每个元素一行，按深度缩进两格：`text #title "标题" 60x20 @16,14`
impl fmt::Display for ElementTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for node in &self.nodes {
            writeln!(f, "{:indent$}{node}", "", indent = node.depth * 2)?;
        }
        Ok(())
    }
}

/// 显示用：取整到 0.1，整数不带小数点
fn round(v: f32) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r.fract() == 0.0 { format!("{}", r as i64) } else { format!("{r:.1}") }
}

/// 上右下左四边（外边距、内边距），逻辑像素
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    /// 上
    pub top: f32,
    /// 右
    pub right: f32,
    /// 下
    pub bottom: f32,
    /// 左
    pub left: f32,
}

/// 元素此刻的绘制属性（动画中的属性是此刻的值）
#[derive(Clone, Debug, PartialEq)]
pub struct PaintInfo {
    /// 背景
    pub background: crate::Fill,
    /// 文字和路径的颜色（继承之后的）
    pub color: crate::Fill,
    /// 圆角半径
    pub radius: f32,
    /// 边框宽度
    pub border_width: f32,
    /// 边框颜色：纯色或渐变
    pub border_color: crate::Fill,
    /// 投影，一层或几层（含内阴影）
    pub shadow: crate::Shadows,
    /// 透明度
    pub opacity: f32,
    /// 缩放
    pub scale: f32,
    /// 旋转
    pub rotation: crate::Angle,
    /// 绕 x 轴的旋转
    pub rotation_x: crate::Angle,
    /// 绕 y 轴的旋转
    pub rotation_y: crate::Angle,
    /// 平移（不含滚动）
    pub translation: [f32; 2],
    /// 模糊的标准差
    pub blur: f32,
    /// 背景模糊的标准差
    pub backdrop_blur: f32,
    /// 用了自定义着色器
    pub has_shader: bool,
}

/// 元素上声明的一个属性动画
#[derive(Clone, Debug, PartialEq)]
pub struct AnimationInfo {
    /// 哪个属性
    pub property: Property,
    /// 正在运动
    pub running: bool,
    /// 用的是弹簧（否则是补间）
    pub uses_spring: bool,
    /// 只有一个分量的属性（透明度、缩放、宽度……）此刻的值；颜色、投影等为 `None`
    pub value: Option<f32>,
    /// 只有一个分量的属性的目标值
    pub target: Option<f32>,
}

/// 一个元素的详细信息，见 测试工具的 `element_info`
#[derive(Clone, Debug, PartialEq)]
pub struct ElementInfo {
    /// 树里的这一项（种类、名字、布局位置、标记）
    pub node: ElementNode,
    /// 此刻在窗口里的外接矩形（算上自己和祖先的变换、滚动、合成层动画）
    pub window_bounds: Rect,
    /// 外边距
    pub margin: Edges,
    /// 内边距（边框画在元素里面，不占布局空间，见 [`PaintInfo::border_width`]）
    pub padding: Edges,
    /// 绘制属性
    pub paint: PaintInfo,
    /// 文字元素的字号和字重（继承之后的）
    pub font: Option<[f32; 2]>,
    /// 是滚动视口：此刻的偏移
    pub scroll_offset: Option<[f32; 2]>,
    /// 兄弟之间的叠放次序
    pub stack_order: i32,
    /// 声明的属性动画
    pub animations: Vec<AnimationInfo>,
}
