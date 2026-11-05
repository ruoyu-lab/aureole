use bytemuck::{Pod, Zeroable};

use crate::Color;

/// 一个实例化图元：圆角矩形（可带边框、阴影），或者图集里的一个字形；都可以带变换。
///
/// 内存布局与渲染器着色器（`crates/render/src/shaders/quad.wgsl`）里的实例属性一一对应，改字段时两边要同步；
/// 渲染器的测试核对两边的偏移。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Quad {
    /// x、y、宽、高，逻辑像素
    pub rect: [f32; 4],
    /// 左上、右上、右下、左下的圆角半径
    pub radii: [f32; 4],
    /// 绕矩形中心的 2x2 线性变换，列主序 (a, b, c, d)
    pub transform: [f32; 4],
    /// 阴影偏移 (dx, dy)，逻辑像素
    pub shadow_offset: [f32; 2],
    /// 模糊半径：高斯模糊的标准差是它的一半（sigma = blur / 2）
    pub shadow_blur: f32,
    /// 边框向内绘制。单独的投影、内阴影（[`Quad::SHADOW`]、[`Quad::INSET_SHADOW`]）用它存扩展
    pub border_width: f32,
    /// 填充色；字形和路径是它们的颜色
    pub fill: Color,
    /// 边框颜色
    pub border_color: Color,
    /// 阴影颜色，透明表示没有阴影
    pub shadow_color: Color,
    /// 透明度 0..=1，乘到整个图元上
    pub opacity: f32,
    /// 字形在图集里的归一化 UV（u0, v0, u1, v1），只对字形有效。
    /// 形状（[`Quad::SHAPE`]）用 `uv[0]` 存渐变边框的下标（见 [`Quad::border_gradient`]），0 表示纯色边框
    pub uv: [f32; 4],
    /// 种类：[`Quad::SHAPE`]、[`Quad::MASK_GLYPH`]、[`Quad::PATH`]、[`Quad::IMAGE`] 等常量之一
    pub kind: u32,
    /// 所在的合成层（[`LayerTransform`] 的下标），0 是根层。
    /// `rect` 和 `transform` 都是层内坐标，上屏前再经过层的变换和透明度
    pub layer: u32,
    /// 裁剪区域：[`crate::DisplayList::clips`] 的下标 + 1，0 表示不裁剪
    pub clip: u32,
    /// 渐变填充（代替 `fill`）：[`crate::DisplayList::push_gradient`] 返回的下标，0 表示纯色。
    /// 对形状、单色字形、路径有效
    pub gradient: u32,
}

/// 一个合成层的变换和透明度。下标 0 是根层（单位变换）。
/// 层的变换每帧可以单独更新，层内图元不用重新上传。
///
/// 变换是平面上的投影变换（齐次坐标的 3x3 矩阵）：层坐标 `(x, y)` 先算出
/// `(a x + c y + e, b x + d y + f, g x + h y + i)`，再除以第三个分量。`linear` 是 `(a, b, c, d)`，
/// `offset` 是 `(e, f)`，`projective` 是 `(g, h, i)`；没有透视（平的层）时 `projective` 是 `(0, 0, 1)`，
/// 就是普通的仿射变换。绕 x、y 轴转过、带透视的层，整层的内容都在同一个平面上，投到屏幕上正好是这样一个变换
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct LayerTransform {
    /// 列主序 2x2
    pub linear: [f32; 4],
    /// 平移，逻辑像素
    pub offset: [f32; 2],
    /// 乘到层内图元上的透明度
    pub opacity: f32,
    /// 层是一个分组（[`crate::Group`]）时，整组合成上屏用的透明度；
    /// 这时组内图元的 `opacity` 从 1 重新累积
    pub group_opacity: f32,
    /// 投影的第三行 `(g, h, i)` 和一个占位：平的层是 `(0, 0, 1, 0)`
    pub projective: [f32; 4],
}

impl LayerTransform {
    /// 单位变换，不透明
    pub const IDENTITY: Self = Self {
        linear: [1.0, 0.0, 0.0, 1.0],
        offset: [0.0, 0.0],
        opacity: 1.0,
        group_opacity: 1.0,
        projective: Self::FLAT,
    };

    /// 平的层的 `projective`
    pub const FLAT: [f32; 4] = [0.0, 0.0, 1.0, 0.0];

    /// 有透视（不是仿射变换）
    #[inline]
    pub fn is_projective(&self) -> bool {
        self.projective[..3] != Self::FLAT[..3]
    }

    /// 层坐标 -> 屏幕逻辑像素。点在视点后面（投不到屏幕上）时结果没有意义，见 [`Self::project`]
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        let [x, y] = p;
        let [a, b, c, d] = self.linear;
        let (px, py) = (a * x + c * y + self.offset[0], b * x + d * y + self.offset[1]);
        if !self.is_projective() {
            return [px, py];
        }
        let w = self.w(p).max(1e-6);
        [px / w, py / w]
    }

    /// 层坐标 -> 屏幕逻辑像素；点在视点后面时为 `None`
    pub fn project(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        (self.w(p) > 1e-6).then(|| self.apply(p))
    }

    /// 齐次坐标的第三个分量：大于 0 时点在视点前面
    #[inline]
    fn w(&self, [x, y]: [f32; 2]) -> f32 {
        let [g, h, i, _] = self.projective;
        g * x + h * y + i
    }
}

/// 圆角矩形裁剪区域：子孙只在这个形状里可见。形状在元素自己的坐标里，
/// 经过 `inverse`（层坐标 -> 元素坐标）和所在层的变换跟着元素走
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Clip {
    /// 层坐标 -> 元素坐标的仿射变换：列主序 2x2
    pub inverse_linear: [f32; 4],
    /// 层坐标 -> 元素坐标的仿射变换：平移
    pub inverse_offset: [f32; 2],
    /// 矩形中心和半宽高，元素坐标
    pub center: [f32; 2],
    /// 半宽、半高，元素坐标
    pub half_size: [f32; 2],
    /// 所在的合成层。这一层（或祖先层）可能有透视时再或上 [`Clip::PROJECTIVE`]
    pub layer: u32,
    /// 外层裁剪（下标 + 1，0 表示没有），嵌套时逐层相交
    pub parent: u32,
    /// 左上、右上、右下、左下的圆角半径
    pub radii: [f32; 4],
}

impl Clip {
    /// `layer` 的最高位：所在的层可能有透视（[`LayerTransform::projective`]）。
    /// 没有这一位的裁剪按平的层算，不用读投影的那一行
    pub const PROJECTIVE: u32 = 1 << 31;
}

impl Quad {
    /// 圆角矩形，用距离场着色
    pub const SHAPE: u32 = 0;
    /// 单色字形：图集覆盖率 × 填充色
    pub const MASK_GLYPH: u32 = 1;
    /// 彩色字形（emoji）：直接用图集颜色
    pub const COLOR_GLYPH: u32 = 2;
    // 3 留给渲染器内部：把分组的离屏纹理合成上屏（`rect` 是屏幕坐标，不经过层变换），见渲染器的 `GROUP_KIND`
    /// 背景模糊：形状里填充模糊后的背景（由 [`crate::QuadEffect::Backdrop`] 引用）
    pub const BACKDROP: u32 = 4;
    /// 自定义着色器填充（由 [`crate::QuadEffect::Shader`] 引用），`uv` 是传给着色器的 4 个参数
    pub const CUSTOM: u32 = 5;
    /// 矢量路径：路径图集的覆盖率 × 填充色
    pub const PATH: u32 = 6;
    /// 图片图集里的图片：`uv` 是矩形四角对应的纹理坐标（可以超出图片，超出的部分不画），
    /// `shadow_offset`、`shadow_blur`、`border_width` 依次是图片在纹理里的范围 (u0, v0, u1, v1)。
    /// 圆角照常按距离场裁剪。编号和其他种类隔开，给以后的新种类留位置
    pub const IMAGE: u32 = 10;
    /// 单独纹理里的大图（由 [`crate::QuadEffect::Image`] 引用），字段同 [`Quad::IMAGE`]
    pub const IMAGE_TEXTURE: u32 = 11;
    /// 单独的一层投影：只画在形状（`rect`、`radii`、`transform`）外面，`border_width` 是扩展（正值向外扩，
    /// 圆角跟着变大）。`gradient` 不为 0 时颜色取渐变，`shadow_color` 只提供透明度。
    /// 一个元素有几层投影、或者投影带扩展时用它；最常见的一层不带扩展的投影直接画在 [`Quad::SHAPE`] 里
    pub const SHADOW: u32 = 16;
    /// 内阴影：画在形状里面，`rect`、`radii` 是边框以内的区域。形状外面像有一块往 `shadow_offset` 挪过、
    /// 向里扩了 `border_width`（扩展）的挡板，投下模糊 `shadow_blur` 的阴影。颜色同 [`Quad::SHADOW`]
    pub const INSET_SHADOW: u32 = 17;

    /// 渐变边框：形状的边框颜色来自这个渐变（[`crate::DisplayList::push_gradient`] 返回的下标），
    /// 渐变的仿射变换把相对图元中心、不含 `transform` 的局部坐标变成渐变坐标。0 表示纯色边框
    #[inline]
    pub fn border_gradient(&self) -> u32 {
        if self.kind == Self::SHAPE { self.uv[0] as u32 } else { 0 }
    }

    /// 设置渐变边框（见 [`Quad::border_gradient`]）
    pub fn with_border_gradient(mut self, gradient: u32) -> Self {
        self.uv[0] = gradient as f32;
        self
    }

    /// 单独的投影、内阴影、渐变边框：着色器里这几段代码只编译进显示列表里有它们时用的管线
    #[inline]
    pub fn is_rich(&self) -> bool {
        self.kind == Self::SHADOW || self.kind == Self::INSET_SHADOW || self.border_gradient() != 0
    }

    /// 白色填充的直角矩形，逻辑像素；其余字段为默认（无边框、无阴影、不透明、根层、不裁剪）
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            rect: [x, y, width, height],
            radii: [0.0; 4],
            transform: [1.0, 0.0, 0.0, 1.0],
            shadow_offset: [0.0; 2],
            shadow_blur: 0.0,
            border_width: 0.0,
            fill: Color::WHITE,
            border_color: Color::TRANSPARENT,
            shadow_color: Color::TRANSPARENT,
            opacity: 1.0,
            uv: [0.0; 4],
            kind: Self::SHAPE,
            layer: 0,
            clip: 0,
            gradient: 0,
        }
    }

    /// 在层坐标里的外接矩形 `[x0, y0, x1, y1]`：含变换、阴影和抗锯齿的余量
    pub fn bounds(&self) -> [f32; 4] {
        let [x, y, w, h] = self.rect;
        let (cx, cy) = (x + w * 0.5, y + h * 0.5);
        let [mut lo_x, mut lo_y, mut hi_x, mut hi_y] = [-w * 0.5, -h * 0.5, w * 0.5, h * 0.5];
        let outer = self.kind == Self::SHAPE || self.kind == Self::SHADOW;
        if outer && !self.shadow_color.is_transparent() {
            // 3 sigma，加上单独的投影向外的扩展
            let spread = if self.kind == Self::SHADOW { self.border_width.max(0.0) } else { 0.0 };
            let extent = 1.5 * self.shadow_blur + spread;
            lo_x = lo_x.min(-w * 0.5 + self.shadow_offset[0] - extent);
            lo_y = lo_y.min(-h * 0.5 + self.shadow_offset[1] - extent);
            hi_x = hi_x.max(w * 0.5 + self.shadow_offset[0] + extent);
            hi_y = hi_y.max(h * 0.5 + self.shadow_offset[1] + extent);
        }
        let [a, b, c, d] = self.transform;
        let mut out = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        for (px, py) in [(lo_x, lo_y), (hi_x, lo_y), (lo_x, hi_y), (hi_x, hi_y)] {
            let (tx, ty) = (cx + a * px + c * py, cy + b * px + d * py);
            out = [out[0].min(tx), out[1].min(ty), out[2].max(tx), out[3].max(ty)];
        }
        // 抗锯齿外扩约 1.5 个物理像素
        [out[0] - 1.0, out[1] - 1.0, out[2] + 1.0, out[3] + 1.0]
    }

    /// 图集里的字形。`rect` 应与位图的物理像素对齐，才能清晰
    pub fn glyph(x: f32, y: f32, width: f32, height: f32, uv: [f32; 4], color: Color, colored: bool) -> Self {
        let mut quad = Self::new(x, y, width, height).fill(color);
        quad.uv = uv;
        quad.kind = if colored { Self::COLOR_GLYPH } else { Self::MASK_GLYPH };
        quad
    }

    /// 路径图集里的一块覆盖率。`rect` 应与位图的物理像素对齐，才能清晰
    pub fn path(x: f32, y: f32, width: f32, height: f32, uv: [f32; 4], color: Color) -> Self {
        let mut quad = Self::new(x, y, width, height).fill(color);
        quad.uv = uv;
        quad.kind = Self::PATH;
        quad
    }

    /// 四个角相同的圆角半径，逻辑像素
    pub fn radius(mut self, radius: f32) -> Self {
        self.radii = [radius; 4];
        self
    }

    /// 分别设置四个角的圆角半径，逻辑像素
    pub fn radii(mut self, top_left: f32, top_right: f32, bottom_right: f32, bottom_left: f32) -> Self {
        self.radii = [top_left, top_right, bottom_right, bottom_left];
        self
    }

    /// 填充色
    pub fn fill(mut self, color: Color) -> Self {
        self.fill = color;
        self
    }

    /// 向内绘制的边框，宽度为逻辑像素
    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.border_width = width;
        self.border_color = color;
        self
    }

    /// 外阴影：偏移 (dx, dy) 和模糊半径都是逻辑像素
    pub fn shadow(mut self, dx: f32, dy: f32, blur: f32, color: Color) -> Self {
        self.shadow_offset = [dx, dy];
        self.shadow_blur = blur;
        self.shadow_color = color;
        self
    }

    /// 绕中心缩放并旋转，角度为弧度，正值顺时针
    pub fn scale_rotate(mut self, scale: f32, radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        self.transform = [cos * scale, sin * scale, -sin * scale, cos * scale];
        self
    }

    /// 透明度 0..=1
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }
}
