/// 逻辑像素坐标下的矩形
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// 左边
    pub x: f32,
    /// 上边
    pub y: f32,
    /// 宽
    pub width: f32,
    /// 高
    pub height: f32,
}

impl Rect {
    /// 点是否在矩形里（含左上边，不含右下边）
    #[inline]
    pub fn contains(&self, [x, y]: [f32; 2]) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    /// 中心点
    #[inline]
    pub fn center(&self) -> [f32; 2] {
        [self.x + self.width * 0.5, self.y + self.height * 0.5]
    }
}

/// 2D 仿射变换：`p' = linear * p + offset`，`linear` 为列主序 2x2
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Affine {
    pub linear: [f32; 4],
    pub offset: [f32; 2],
}

impl Affine {
    pub const IDENTITY: Self = Self { linear: [1.0, 0.0, 0.0, 1.0], offset: [0.0, 0.0] };

    /// `self ∘ other`：先做 `other`，再做本变换
    #[inline]
    pub fn compose(&self, other: &Affine) -> Self {
        Self { linear: mul(self.linear, other.linear), offset: self.apply(other.offset) }
    }

    /// 只有平移，没有缩放旋转：这时字形可以对齐到物理像素
    #[inline]
    pub fn is_translation(&self) -> bool {
        self.linear == Self::IDENTITY.linear
    }

    #[inline]
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        let [x, y] = apply_linear(self.linear, p);
        [x + self.offset[0], y + self.offset[1]]
    }

    /// 逆变换本身；退化时返回 `None`
    pub fn inverse(&self) -> Option<Self> {
        let [a, b, c, d] = self.linear;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        let linear = [d / det, -b / det, -c / det, a / det];
        let [x, y] = apply_linear(linear, self.offset);
        Some(Self { linear, offset: [-x, -y] })
    }

    /// 逆变换；退化（缩放为 0）时返回 `None`
    #[inline]
    pub fn invert(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        let [a, b, c, d] = self.linear;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        let inverse = [d / det, -b / det, -c / det, a / det];
        Some(apply_linear(inverse, [p[0] - self.offset[0], p[1] - self.offset[1]]))
    }

    /// 先在本地做"绕 `center` 缩放旋转、再平移"，再套上本变换
    #[inline]
    pub fn then_local(&self, center: [f32; 2], translate: [f32; 2], scale: f32, rotation: f32) -> Self {
        if translate == [0.0, 0.0] && scale == 1.0 && rotation == 0.0 {
            return *self;
        }
        let (sin, cos) = rotation.sin_cos();
        let local = [cos * scale, sin * scale, -sin * scale, cos * scale];
        // 本地变换：p -> local * p + (center + translate - local * center)
        let moved = apply_linear(local, center);
        let local_offset = [center[0] + translate[0] - moved[0], center[1] + translate[1] - moved[1]];
        Self { linear: mul(self.linear, local), offset: self.apply(local_offset) }
    }
}

/// 没设观察距离、父元素也没给（[`crate::Element::child_perspective`]）时，绕 x、y 轴的旋转按这个距离看，逻辑像素
pub(crate) const DEFAULT_PERSPECTIVE: f32 = 1000.0;

/// 观察距离（逻辑像素）的倒数；`f32::INFINITY` 是没有透视（正投影）。没设（0）时为 `None`
pub(crate) fn inverse_distance(distance: f32) -> Option<f32> {
    (distance > 0.0).then(|| if distance.is_finite() { 1.0 / distance } else { 0.0 })
}

/// 平面上的投影变换（齐次坐标的 3x3 矩阵）：`(x, y)` 先变成 `(a x + c y + e, b x + d y + f, g x + h y + i)`，
/// 再除以第三个分量。`affine` 是前两行，`perspective` 是第三行 `(g, h, i)`。
///
/// 绕 x、y 轴转过、带透视的元素整个在一个平面上，从视点看过去投到屏幕上正好是这样一个变换，
/// 嵌套时直接相乘（每一层先投回父元素的平面，再接着往上）。没有透视时第三行是 `(0, 0, 1)`，
/// 和 [`Affine`] 完全一样，计算也走仿射的捷径
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Transform {
    pub affine: Affine,
    pub perspective: [f32; 3],
}

impl From<Affine> for Transform {
    #[inline]
    fn from(affine: Affine) -> Self {
        Self { affine, perspective: FLAT }
    }
}

const FLAT: [f32; 3] = [0.0, 0.0, 1.0];

impl Transform {
    pub const IDENTITY: Self = Self { affine: Affine::IDENTITY, perspective: FLAT };

    /// 前两行是 `affine`，第三行是 `row`（`None` 表示没有透视）
    #[inline(always)]
    pub fn with_row(affine: Affine, row: Option<[f32; 3]>) -> Self {
        Self { affine, perspective: row.unwrap_or(FLAT) }
    }

    /// 没有透视：就是仿射变换
    #[inline]
    pub fn is_affine(&self) -> bool {
        self.perspective == FLAT
    }

    /// `self ∘ other`：先做 `other`，再做本变换
    #[inline]
    pub fn compose(&self, other: &Transform) -> Self {
        if other.is_affine() {
            return self.then_affine(&other.affine);
        }
        let [a, b, c, d] = self.affine.linear;
        let [e, f] = self.affine.offset;
        let [g, h, i] = self.perspective;
        let [a2, b2, c2, d2] = other.affine.linear;
        let [e2, f2] = other.affine.offset;
        let [g2, h2, i2] = other.perspective;
        // 按列：第 k 列 = self × (other 的第 k 列)
        let column = |x: f32, y: f32, z: f32| [a * x + c * y + e * z, b * x + d * y + f * z, g * x + h * y + i * z];
        let [m00, m10, m20] = column(a2, b2, g2);
        let [m01, m11, m21] = column(c2, d2, h2);
        let [m02, m12, m22] = column(e2, f2, i2);
        Self { affine: Affine { linear: [m00, m10, m01, m11], offset: [m02, m12] }, perspective: [m20, m21, m22] }
    }

    /// `self ∘ other`，`other` 是仿射变换
    #[inline(always)]
    pub fn then_affine(&self, other: &Affine) -> Self {
        let affine = self.affine.compose(other);
        if self.is_affine() {
            return Self { affine, perspective: FLAT };
        }
        let [g, h, i] = self.perspective;
        let [a2, b2, c2, d2] = other.linear;
        let [e2, f2] = other.offset;
        Self { affine, perspective: [g * a2 + h * b2, g * c2 + h * d2, g * e2 + h * f2 + i] }
    }

    /// 先在本地做"绕 `center` 缩放旋转、再平移"，再套上本变换（同 [`Affine::then_local`]）。
    /// 绘制遍历里每个元素都要算一次，一定内联（平的元素和以前一样只算仿射的部分）
    #[inline(always)]
    pub fn then_local(&self, center: [f32; 2], translate: [f32; 2], scale: f32, rotation: f32) -> Self {
        if translate == [0.0, 0.0] && scale == 1.0 && rotation == 0.0 {
            return *self;
        }
        let affine = self.affine.then_local(center, translate, scale, rotation);
        if self.is_affine() {
            return Self { affine, perspective: FLAT };
        }
        self.projective_local(center, translate, scale, rotation, affine)
    }

    /// [`Self::then_local`] 有透视的部分：前两行已经算好（`affine`），补上第三行
    #[cold]
    fn projective_local(&self, center: [f32; 2], translate: [f32; 2], scale: f32, rotation: f32, affine: Affine) -> Self {
        let local = Affine::IDENTITY.then_local(center, translate, scale, rotation);
        Self { affine, perspective: self.then_affine(&local).perspective }
    }

    /// 齐次坐标的第三个分量：大于 0 时点在视点前面
    #[inline]
    fn w(&self, [x, y]: [f32; 2]) -> f32 {
        let [g, h, i] = self.perspective;
        g * x + h * y + i
    }

    /// 投到屏幕上；点在视点后面时为 `None`
    #[inline]
    pub fn project(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        if self.is_affine() {
            return Some(self.affine.apply(p));
        }
        let w = self.w(p);
        (w > 1e-6).then(|| {
            let [x, y] = self.affine.apply(p);
            [x / w, y / w]
        })
    }

    /// 投到屏幕上；视点后面的点按很近的距离算（结果没有意义，只是不会出现无穷大）
    #[inline]
    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        if self.is_affine() {
            return self.affine.apply(p);
        }
        let w = self.w(p).max(1e-6);
        let [x, y] = self.affine.apply(p);
        [x / w, y / w]
    }

    /// 屏幕上的点对应平面上的哪一点；退化或者对应到视点后面（看不见的部分）时返回 `None`
    #[inline]
    pub fn invert(&self, p: [f32; 2]) -> Option<[f32; 2]> {
        if self.is_affine() {
            return self.affine.invert(p);
        }
        let [a, b, c, d] = self.affine.linear;
        let [e, f] = self.affine.offset;
        let [g, h, i] = self.perspective;
        // 伴随矩阵乘 (x, y, 1)；最后除以第三个分量，行列式的倍数约掉了
        let [x, y] = p;
        let lx = (d * i - f * h) * x + (e * h - c * i) * y + (c * f - d * e);
        let ly = (f * g - b * i) * x + (a * i - e * g) * y + (b * e - a * f);
        let lw = (b * h - d * g) * x + (c * g - a * h) * y + (a * d - b * c);
        if lw.abs() < 1e-12 {
            return None;
        }
        let local = [lx / lw, ly / lw];
        (self.w(local) > 1e-6).then_some(local)
    }

    /// 行列式
    #[inline]
    fn determinant(&self) -> f32 {
        let [a, b, c, d] = self.affine.linear;
        let [e, f] = self.affine.offset;
        let [g, h, i] = self.perspective;
        a * (d * i - f * h) - c * (b * i - f * g) + e * (b * h - d * g)
    }

    /// 平面在 `p` 处背朝屏幕（翻过去了，或者镜像）：局部的方向和屏幕上相反
    #[inline]
    pub fn faces_back(&self, p: [f32; 2]) -> bool {
        // 雅可比行列式 = det / w³，符号是 det × w 的符号
        self.determinant() * self.w(p) < 0.0
    }

    /// 矩形四个角（左上、右上、右下、左下）投到屏幕上；有角在视点后面时为 `None`
    pub fn corners(&self, x: f32, y: f32, width: f32, height: f32) -> Option<[[f32; 2]; 4]> {
        Some([
            self.project([x, y])?,
            self.project([x + width, y])?,
            self.project([x + width, y + height])?,
            self.project([x, y + height])?,
        ])
    }

    /// 给渲染器的层变换（不含透明度）
    #[inline]
    pub fn layer(&self) -> scene::LayerTransform {
        let [g, h, i] = self.perspective;
        scene::LayerTransform {
            linear: self.affine.linear,
            offset: self.affine.offset,
            opacity: 1.0,
            group_opacity: 1.0,
            projective: [g, h, i, 0.0],
        }
    }
}

/// 元素自身的变换（布局坐标里）。顺序：绕变换中心 `origin` 缩放、在平面里旋转（`rotation`），再绕 y 轴、绕 x 轴转
/// （`rotate`，右边、上边转向远处为正），平移；有 3D 旋转时从 `eye` 正上方、观察距离的倒数为 `k` 处看过去，投回平面。
/// 没有 3D 旋转时和 [`Affine::then_local`] 逐位相同
#[derive(Clone, Copy, Debug)]
pub(crate) struct Local {
    pub origin: [f32; 2],
    pub translate: [f32; 2],
    pub scale: f32,
    pub rotation: f32,
    pub rotate: [f32; 2],
    pub eye: [f32; 2],
    pub k: f32,
}

impl Local {
    pub fn transform(&self) -> Transform {
        let Self { origin: [ox, oy], translate: [tx, ty], scale: s, rotation, rotate: [rx, ry], eye: [ex, ey], k } = *self;
        if rx == 0.0 && ry == 0.0 {
            return Affine::IDENTITY.then_local(self.origin, self.translate, s, rotation).into();
        }
        let (sin_z, cos_z) = rotation.sin_cos();
        let (sin_x, cos_x) = rx.sin_cos();
        let (sin_y, cos_y) = ry.sin_cos();
        // 平面里的两个方向 (1, 0)、(0, 1) 经过缩放、绕 z、绕 y、绕 x 之后的三维方向（z 朝向看的人）
        let column = |x: f32, y: f32| {
            let (x, y) = (cos_z * x * s - sin_z * y * s, sin_z * x * s + cos_z * y * s);
            let (x, z) = (cos_y * x, -sin_y * x);
            let (y, z) = (cos_x * y - sin_x * z, sin_x * y + cos_x * z);
            [x, y, z]
        };
        let [l00, l10, l20] = column(1.0, 0.0);
        let [l01, l11, l21] = column(0.0, 1.0);
        // 透视：屏幕上的点 = 视点 + (点 - 视点) / (1 - k z)，写成齐次坐标是线性的
        let (m00, m01, m02) = (l00 - k * ex * l20, l01 - k * ex * l21, ox + tx);
        let (m10, m11, m12) = (l10 - k * ey * l20, l11 - k * ey * l21, oy + ty);
        let (m20, m21) = (-k * l20, -k * l21);
        // 上面是相对变换中心的坐标；再乘上"先减去中心"
        let offset = [m02 - m00 * ox - m01 * oy, m12 - m10 * ox - m11 * oy];
        let i = 1.0 - m20 * ox - m21 * oy;
        Transform { affine: Affine { linear: [m00, m10, m01, m11], offset }, perspective: [m20, m21, i] }
    }
}

/// 屏幕上的轴对齐外接矩形 `[x0, y0, x1, y1]`
pub(crate) fn bounding(points: &[[f32; 2]]) -> [f32; 4] {
    points.iter().fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, &[x, y]| [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)])
}

#[inline]
fn apply_linear(m: [f32; 4], [x, y]: [f32; 2]) -> [f32; 2] {
    [m[0] * x + m[2] * y, m[1] * x + m[3] * y]
}

#[inline]
fn mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
    ]
}
