// 实例化图元：圆角矩形 + 边框 + 阴影（距离场），或者图集里的字形。
// 每个实例画一个三角形带（4 个顶点）；圆角矩形的四边形会按阴影范围外扩。
// 同一份着色器也画分组的合成、背景模糊的填充；带滤镜、遮罩、混合模式的分组合成用 fs_composite（单独的管线）；
// 自定义着色器拼在这个文件后面。

struct Globals {
    viewport: vec2<f32>, // 当前渲染目标的大小，物理像素
    origin: vec2<f32>,   // 当前渲染目标左上角在屏幕上的位置，物理像素（离屏分组不在原点）
    scale: f32,          // 逻辑像素 -> 物理像素
    time: f32,           // 秒，给自定义着色器
    dither: f32,         // 1：8 位 sRGB 目标，平滑的颜色要抖动
    headroom: f32,       // 这一帧最终的目标能显示到多亮（SDR 白的倍数）：8 位目标是 1，扩展范围的表面是显示器此刻的余量
    extended: f32,       // 1：这一帧最终的目标是扩展范围的（负的分量是广色域的颜色）；0：8 位 sRGB
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_atlas: texture_2d<f32>;
@group(0) @binding(2) var color_atlas: texture_2d<f32>;
@group(0) @binding(3) var atlas_sampler: sampler;

// 合成层：层内图元先经过层的变换和透明度再上屏。下标 0 是根层。
// 变换是齐次坐标的 3x3 矩阵：(linear, offset) 是前两行，projective.xyz 是第三行；
// 平的层第三行是 (0, 0, 1)。绕 x、y 轴转过、带透视的层整层在一个平面上，投到屏幕上就是这样一个变换
struct Layer {
    linear: vec4<f32>,
    offset: vec2<f32>,
    opacity: f32,
    group_opacity: f32,
    projective: vec4<f32>,
}

// 显示列表里有可能带透视的裁剪：按透视算裁剪的那一段只编译进这种管线（平的界面用的管线里没有它，寄存器占得少）
override PROJECTIVE_CLIPS: bool = false;
// 显示列表里有单独的投影、内阴影、渐变边框：这几段代码也只编译进这种管线
override RICH_SHAPES: bool = false;

// 显示列表里有扩展范围的内容（广色域、HDR 的渐变和浮点纹理）：把它们放进目标能显示的范围的那几段
// 只编译进这种管线（只有 sRGB 内容时没有它，渐变不多占寄存器）
override EXTENDED_COLORS: bool = false;

// 这一层有透视（第三行不是 (0, 0, 1)）
fn is_projective(layer: Layer) -> bool {
    return any(layer.projective.xyz != vec3(0.0, 0.0, 1.0));
}

// 层的整个 3x3 矩阵（按列）
fn layer_matrix(layer: Layer) -> mat3x3<f32> {
    return mat3x3<f32>(
        vec3(layer.linear.xy, layer.projective.x),
        vec3(layer.linear.zw, layer.projective.y),
        vec3(layer.offset, layer.projective.z),
    );
}

// 2x2 矩阵的较小奇异值：沿最"扁"的方向，1 个单位变成多少个单位
fn min_stretch(m: mat2x2<f32>) -> f32 {
    let a = dot(m[0], m[0]);
    let c = dot(m[1], m[1]);
    let b = dot(m[0], m[1]);
    let half = 0.5 * (a - c);
    return sqrt(max(0.5 * (a + c) - sqrt(half * half + b * b), 0.0));
}

@group(0) @binding(4) var<storage, read> layers: array<Layer>;

// 圆角矩形裁剪。下标 0 是"不裁剪"
struct Clip {
    inverse_linear: vec4<f32>,
    inverse_offset: vec2<f32>,
    center: vec2<f32>,
    half_size: vec2<f32>,
    layer: u32,
    parent: u32,
    radii: vec4<f32>,
}

@group(0) @binding(5) var<storage, read> clips: array<Clip>;
@group(0) @binding(6) var path_atlas: texture_2d<f32>;

// 渐变：所有渐变的编码排成一个 vec4 数组，图元的 gradient 是表头下标（0 表示纯色）。
// 表头：层坐标 -> 渐变坐标的仿射变换（列主序 2x2、平移）、形状和插值空间、色标数；
// 后面是色标位置（每 4 个一组）和插值空间里预乘过的颜色。
// 滤镜的颜色矩阵也放在这里（5 个 vec4）：图片、彩色字形、分组合成的 gradient 是它的下标
@group(0) @binding(7) var<storage, read> gradients: array<vec4<f32>>;

// 分组的离屏纹理或模糊后的背景
@group(1) @binding(0) var effect_texture: texture_2d<f32>;
// 分组的遮罩（只有 fs_composite 用）
@group(1) @binding(1) var mask_texture: texture_2d<f32>;
// 混合模式要的、拷出来的后面的内容（只有 fs_composite 用）
@group(3) @binding(0) var backdrop_texture: texture_2d<f32>;

// 图片：带 mip 的图片图集、正在画的大图的纹理、三线性采样器。都是 sRGB 编码的预乘颜色，
// 采样时硬件先转成线性再过滤
@group(2) @binding(0) var image_atlas: texture_2d<f32>;
@group(2) @binding(1) var image_texture: texture_2d<f32>;
@group(2) @binding(2) var image_sampler: sampler;

// 正在画的大图：peak > 0 表示半精度浮点的纹理（线性、扩展范围，广色域或 HDR），peak 是它最亮的分量
struct ImageInfo {
    peak: f32,
    _a: f32,
    _b: f32,
    _c: f32,
}
@group(2) @binding(3) var<uniform> image_info: ImageInfo;

const SHAPE: u32 = 0u;
const MASK_GLYPH: u32 = 1u;
const COLOR_GLYPH: u32 = 2u;
const GROUP: u32 = 3u;
const BACKDROP: u32 = 4u;
const CUSTOM: u32 = 5u;
const PATH: u32 = 6u;
const IMAGE: u32 = 10u;
const IMAGE_TEXTURE: u32 = 11u;
const SHADOW: u32 = 16u;
const INSET_SHADOW: u32 = 17u;

struct QuadIn {
    @location(0) rect: vec4<f32>,
    @location(1) radii: vec4<f32>,
    @location(2) transform: vec4<f32>,
    @location(3) params: vec4<f32>, // shadow_offset.xy, shadow_blur, border_width
    @location(4) fill: vec4<f32>,
    @location(5) border_color: vec4<f32>,
    @location(6) shadow_color: vec4<f32>,
    @location(7) opacity: f32,
    @location(8) uv: vec4<f32>,
    @location(9) kind: u32,
    @location(10) layer: u32,
    @location(11) clip: u32,
    @location(12) gradient: u32,
}

struct VsOut {
    @builtin(position) position: vec4<f32>,
    // 相对矩形中心、未变换的局部坐标（逻辑像素）
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) half_size: vec2<f32>,
    @location(2) @interpolate(flat) radii: vec4<f32>,
    @location(3) @interpolate(flat) fill: vec4<f32>,
    @location(4) @interpolate(flat) border_color: vec4<f32>,
    @location(5) @interpolate(flat) shadow_color: vec4<f32>,
    @location(6) @interpolate(flat) params: vec4<f32>, // shadow_offset.xy, sigma, border_width
    @location(7) @interpolate(flat) opacity: f32,
    @location(8) @interpolate(flat) kind: u32,
    @location(9) uv: vec2<f32>,
    // 文字颜色的感知明度，用于覆盖率校正
    @location(10) @interpolate(flat) lightness: f32,
    @location(11) @interpolate(flat) clip: u32,
    // 原样的 uv 字段：背景模糊的采样区域、自定义着色器的参数
    @location(12) @interpolate(flat) extra: vec4<f32>,
    // 原样的填充色（sRGB，未预乘），给自定义着色器
    @location(13) @interpolate(flat) color: vec4<f32>,
    // 渐变坐标（xy）和渐变下标（z，四个顶点相同，插值后取整还是它，省一个插值变量）
    @location(14) gradient: vec3<f32>,
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return select(hi, lo, c <= vec3(0.04045));
}

fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3(0.0031308));
}

// sRGB 直通 alpha -> 线性预乘
fn premultiply(c: vec4<f32>) -> vec4<f32> {
    return vec4(srgb_to_linear(c.rgb) * c.a, c.a);
}

// 滤镜：颜色矩阵在 sRGB 编码、未预乘的颜色上算（和调色的习惯一致），再换回线性预乘。
// gradients[index..index + 4] 是 r、g、b、a 四行系数，gradients[index + 4] 是偏移
fn color_filter(c: vec4<f32>, index: u32) -> vec4<f32> {
    if c.a <= 0.0 {
        return c;
    }
    let s = vec4(srgb_encode(clamp(c.rgb / c.a, vec3(0.0), vec3(1.0))), c.a);
    let v = vec4(dot(gradients[index], s), dot(gradients[index + 1u], s), dot(gradients[index + 2u], s), dot(gradients[index + 3u], s));
    let o = clamp(v + gradients[index + 4u], vec4(0.0), vec4(1.0));
    return vec4(srgb_to_linear(o.rgb) * o.a, o.a);
}

// 扩展范围的 sRGB 编码值 -> 线性：0..1 之外按原点对称延伸
fn srgb_to_linear_extended(c: vec3<f32>) -> vec3<f32> {
    return sign(c) * srgb_to_linear(abs(c));
}

// HDR 的色调映射，和 Rust 里的 `tone_map` 一致：把最亮为 peak 的内容放进余量 headroom。
// 不超出时原样；否则膝点（0.75 headroom）以下不变，以上用 k + x / (1 + c x) 平滑地压缩，peak 正好落在 headroom 上
fn tone_map(value: f32, peak: f32, headroom: f32) -> f32 {
    let knee = 0.75 * headroom;
    if peak <= headroom || value <= knee {
        return value;
    }
    let c = 1.0 / (headroom - knee) - 1.0 / (peak - knee);
    let x = value - knee;
    return min(knee + x / (1.0 + c * x), headroom);
}

// 有多亮：Display P3 原色上最大的分量（和 Rust 里的 `display_peak` 一致）。P3 色域里的颜色不超过 1
fn display_peak(rgb: vec3<f32>) -> f32 {
    let r = dot(rgb, vec3(0.82246197, 0.17753802, 0.0));
    let g = dot(rgb, vec3(0.0331942, 0.9668058, 0.0));
    let b = dot(rgb, vec3(0.017082632, 0.07239744, 0.9105199));
    return max(max(r, g), b);
}

// 8 位目标：负的分量截掉，最大的分量超过 1 时等比压到 1（色相不变）
fn fit_srgb(rgb: vec3<f32>) -> vec3<f32> {
    let c = max(rgb, vec3(0.0));
    let m = max(max(c.r, c.g), c.b);
    return select(c, c / m, m > 1.0);
}

// 扩展范围的颜色放进这一帧的目标能显示的范围：扩展范围的目标上比余量亮的（按 P3 的分量量）等比压下来，
// 负的分量（广色域）留着；8 位目标见 `fit_srgb`
fn fit_range(rgb: vec3<f32>) -> vec3<f32> {
    if globals.extended == 0.0 {
        return fit_srgb(rgb);
    }
    let m = display_peak(rgb);
    if m > globals.headroom {
        return rgb * (globals.headroom / m);
    }
    return rgb;
}

// 浮点纹理里采样到的线性预乘颜色：最亮为 peak（按 P3 的分量量）的图片按这一帧的余量做色调映射；
// 8 位目标映射到 1 以后再截到 sRGB
fn fit_image(c: vec4<f32>, peak: f32) -> vec4<f32> {
    if c.a <= 1e-6 {
        return vec4(0.0);
    }
    var rgb = c.rgb / c.a;
    let m = display_peak(rgb);
    if m > 0.0 {
        rgb *= tone_map(m, peak, globals.headroom) / m;
    }
    if globals.extended == 0.0 {
        rgb = fit_srgb(rgb);
    }
    return vec4(rgb * c.a, c.a);
}

fn inverse2(m: mat2x2<f32>) -> mat2x2<f32> {
    let det = determinant(m);
    let inv = 1.0 / select(det, 1e-12, abs(det) < 1e-12);
    return mat2x2<f32>(vec2(m[1].y, -m[0].y) * inv, vec2(-m[1].x, m[0].x) * inv);
}

// 屏幕上一点（逻辑像素）在裁剪链里的覆盖率：逐层求圆角矩形的距离场再相乘。
// 在分支和循环里不能求导，抗锯齿宽度用变换的缩放系数算
fn clip_coverage(first: u32, screen: vec2<f32>) -> f32 {
    var coverage = 1.0;
    var index = first;
    for (var depth = 0; depth < 16 && index != 0u; depth++) {
        let clip = clips[index];
        // 最高位：所在的层可能有透视
        let layer = layers[clip.layer & 0x7fffffffu];
        let layer_linear = mat2x2<f32>(layer.linear.xy, layer.linear.zw);
        let inverse = mat2x2<f32>(clip.inverse_linear.xy, clip.inverse_linear.zw);
        // 屏幕 -> 层坐标，和层坐标的 1 个单位在屏幕上的面积（透视下随位置变）
        var layer_point: vec2<f32>;
        var area: f32;
        if PROJECTIVE_CLIPS && (clip.layer & 0x80000000u) != 0u && is_projective(layer) {
            // 逆矩阵用伴随矩阵：最后要除以第三个分量，行列式的倍数约掉了
            let m = layer_matrix(layer);
            let adjugate = transpose(mat3x3<f32>(cross(m[1], m[2]), cross(m[2], m[0]), cross(m[0], m[1])));
            let h = adjugate * vec3(screen, 1.0);
            layer_point = h.xy / h.z;
            let w = dot(layer.projective.xy, layer_point) + layer.projective.z;
            if w <= 0.0 {
                return 0.0;
            }
            area = determinant(m) / (w * w * w);
        } else {
            layer_point = inverse2(layer_linear) * (screen - layer.offset);
            area = determinant(layer_linear);
        }
        let p = inverse * layer_point + clip.inverse_offset - clip.center;
        let d = sd_rounded_rect(p, clip.half_size, corner_radius(p, clip.radii));
        // 元素坐标的 1 个单位 = 多少物理像素
        let pixels = sqrt(abs(area / determinant(inverse))) * globals.scale;
        coverage *= clamp(0.5 - d * pixels, 0.0, 1.0);
        index = clip.parent;
    }
    return coverage;
}

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32, quad: QuadIn) -> VsOut {
    let half_size = quad.rect.zw * 0.5;
    // 层内坐标 -> 屏幕：中心经过层的仿射变换，图元自身的线性变换叠在层的线性变换之后。
    // 分组的合成图元在屏幕坐标里，它的层是渲染器每次合成时加的（缓存的纹理贴到分组此刻的位置：
    // 平时是单位变换），取分组的透明度
    let layer = layers[quad.layer];
    let layer_linear = mat2x2<f32>(layer.linear.xy, layer.linear.zw);
    let layer_offset = layer.offset;
    var layer_opacity = layer.opacity;
    if quad.kind == GROUP {
        layer_opacity = layer.group_opacity;
    }
    let center = layer_linear * (quad.rect.xy + half_size) + layer_offset;
    let transform = mat2x2<f32>(quad.transform.xy, quad.transform.zw);
    let m = layer_linear * transform;
    let projected = is_projective(layer);

    // 四边形范围：矩形本身，有阴影时再并上阴影的 3 sigma 范围（单独的投影再加上向外的扩展）
    var lo = -half_size;
    var hi = half_size;
    let sigma = quad.params.z * 0.5;
    if (quad.kind == SHAPE || quad.kind == SHADOW) && quad.shadow_color.a > 0.0 {
        let spread = select(0.0, max(quad.params.w, 0.0), quad.kind == SHADOW);
        let extent = vec2(3.0 * sigma + spread);
        lo = min(lo, -half_size + quad.params.xy - extent);
        hi = max(hi, half_size + quad.params.xy + extent);
    }
    let image = quad.kind == IMAGE || quad.kind == IMAGE_TEXTURE;
    // 留出约 1.5 个物理像素给抗锯齿；字形和合成图元不外扩
    let expand = quad.kind == SHAPE || quad.kind == BACKDROP || quad.kind == CUSTOM || image || quad.kind == SHADOW
        || quad.kind == INSET_SHADOW;

    // 三角形带顶点顺序：(0,0) (1,0) (0,1) (1,1)
    let corner = vec2(f32(vertex & 1u), f32((vertex >> 1u) & 1u));
    var local: vec2<f32>;
    // 局部坐标 -> 屏幕逻辑像素的导数（抗锯齿外扩、图片的 mip 级别用）
    var stretch = m;
    var w = 1.0;
    if projected {
        // 透视下每处的缩放不同：按这个顶点处的导数外扩，沿最扁的方向也留够 1.5 个物理像素
        let q = mix(lo, hi, corner);
        stretch = projected_stretch(layer, quad.rect.xy + half_size + transform * q) * transform;
        local = q;
        if expand {
            local += (corner * 2.0 - 1.0) * (1.5 / max(min_stretch(stretch) * globals.scale, 0.02));
        }
        // 齐次坐标的第三个分量：交给光栅化做透视除法，插值也就按透视校正（贴图、渐变、距离场都对）
        w = dot(layer.projective.xy, quad.rect.xy + half_size + transform * local) + layer.projective.z;
    } else {
        if expand {
            let pixel_scale = sqrt(abs(determinant(m))) * globals.scale;
            let margin = vec2(1.5 / max(pixel_scale, 1e-3));
            lo -= margin;
            hi += margin;
        }
        local = mix(lo, hi, corner);
    }
    // 平的层 w = 1，和以前逐位相同；w <= 0（视点后面）的部分由光栅化裁掉
    let physical = (center + m * local) * globals.scale - globals.origin * w;
    let ndc = vec2(
        physical.x / globals.viewport.x * 2.0 - w,
        w - physical.y / globals.viewport.y * 2.0,
    );

    var out: VsOut;
    out.position = vec4(ndc, 0.0, w);
    out.local = local;
    out.half_size = half_size;
    out.radii = min(quad.radii, vec4(min(half_size.x, half_size.y)));
    out.fill = premultiply(quad.fill);
    out.border_color = premultiply(quad.border_color);
    out.shadow_color = premultiply(quad.shadow_color);
    out.params = vec4(quad.params.xy, sigma, quad.params.w);
    out.opacity = quad.opacity * layer_opacity;
    out.kind = quad.kind;
    out.uv = mix(quad.uv.xy, quad.uv.zw, corner);
    let luminance = dot(srgb_to_linear(quad.fill.rgb), vec3(0.2126, 0.7152, 0.0722));
    out.lightness = pow(luminance, 1.0 / 3.0);
    out.clip = quad.clip;
    out.extra = quad.uv;
    out.color = quad.fill;
    if image {
        image_vertex(quad, local, half_size, stretch, &out);
    }
    // 渐变：顶点在层坐标里的位置经过渐变的仿射变换，所以渐变跟着元素和合成层变换。
    // 采样纹理的图元（图片、彩色字形、分组合成）没有渐变，z 是负的滤镜的颜色矩阵下标
    out.gradient = vec3(0.0);
    if quad.gradient != 0u {
        if samples_texture(quad.kind) {
            out.gradient = vec3(0.0, 0.0, -f32(quad.gradient));
        } else {
            let g = gradients[quad.gradient];
            let o = gradients[quad.gradient + 1u];
            let position = quad.rect.xy + half_size + mat2x2<f32>(quad.transform.xy, quad.transform.zw) * local;
            out.gradient = vec3(mat2x2<f32>(g.xy, g.zw) * position + o.xy, f32(quad.gradient));
        }
    }
    // 透视下的图片：近处和远处的 mip 级别不同，每个顶点按自己处的缩放算，借渐变的插值变量逐片元插值
    // （y = 1 表示用它；z 仍是滤镜）
    if projected && image {
        out.gradient = vec3(out.lightness, 1.0, out.gradient.z);
    }
    return out;
}

// 采样纹理、不用渐变的图元：gradient 是滤镜的颜色矩阵
fn samples_texture(kind: u32) -> bool {
    return kind == COLOR_GLYPH || kind == GROUP || kind == IMAGE || kind == IMAGE_TEXTURE;
}

// 投影层里层坐标 p 处，层坐标 -> 屏幕逻辑像素的导数：(A - s gᵀ) / w，s 是 p 投到屏幕上的点
fn projected_stretch(layer: Layer, p: vec2<f32>) -> mat2x2<f32> {
    let a = mat2x2<f32>(layer.linear.xy, layer.linear.zw);
    let g = layer.projective;
    let w = max(dot(g.xy, p) + g.z, 1e-4);
    let s = (a * p + layer.offset) / w;
    return mat2x2<f32>((a[0] - s * g.x) / w, (a[1] - s * g.y) / w);
}

// 图片的顶点数据，借用其他种类的插值变量：
//   uv           = 这个顶点的纹理坐标（按局部坐标算，抗锯齿外扩的部分也对）
//   params       = 图片在纹理里的范围 (u0, v0, u1, v1)，采样时夹在里面，不会采到图集里的邻居
//   border_color = 图片在局部坐标里的矩形 (x0, y0, x1, y1)，contain 等留白的边缘做抗锯齿
//   lightness    = mip 级别：每个物理像素对应几个纹素（取两个方向里大的那个）
fn image_vertex(quad: QuadIn, local: vec2<f32>, half_size: vec2<f32>, m: mat2x2<f32>, out: ptr<function, VsOut>) {
    let size = max(quad.rect.zw, vec2(1e-6));
    let span = quad.uv.zw - quad.uv.xy;
    (*out).uv = quad.uv.xy + (local + half_size) / size * span;
    (*out).params = quad.params;
    let safe = select(span, vec2(1e-6), abs(span) < vec2(1e-9));
    let a = (quad.params.xy - quad.uv.xy) / safe * size - half_size;
    let b = (quad.params.zw - quad.uv.xy) / safe * size - half_size;
    (*out).border_color = vec4(min(a, b), max(a, b));

    var texture_size: vec2<f32>;
    var levels: u32;
    if quad.kind == IMAGE {
        texture_size = vec2<f32>(textureDimensions(image_atlas));
        levels = textureNumLevels(image_atlas);
    } else {
        texture_size = vec2<f32>(textureDimensions(image_texture));
        levels = textureNumLevels(image_texture);
    }
    // 局部坐标的 1 个单位在屏幕上是多少物理像素（两个轴分别算，旋转、非等比缩放也对）
    let pixels = vec2(length(m[0]), length(m[1])) * globals.scale;
    let texels = abs(span) * texture_size / size;
    let rate = texels / max(pixels, vec2(1e-6));
    (*out).lightness = clamp(log2(max(max(rate.x, rate.y), 1e-6)), 0.0, f32(levels - 1u));
}

// 片元在屏幕上的位置，逻辑像素
fn screen_position(position: vec4<f32>) -> vec2<f32> {
    return (position.xy + globals.origin) / globals.scale;
}

// 按象限选圆角半径；radii = (左上, 右上, 右下, 左下)，y 轴向下
fn corner_radius(p: vec2<f32>, radii: vec4<f32>) -> f32 {
    let top = select(radii.y, radii.x, p.x < 0.0);
    let bottom = select(radii.z, radii.w, p.x < 0.0);
    return select(bottom, top, p.y < 0.0);
}

fn sd_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let q = abs(p) - half_size + vec2(radius);
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

// Abramowitz & Stegun 7.1.27 近似
fn erf(x: f32) -> f32 {
    let s = sign(x);
    let a = abs(x);
    var y = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    y = y * y;
    return s - s / (y * y);
}

// 单独的一层投影（外面）或内阴影（里面）。p 是局部坐标，d、aa 是形状的距离场和它的抗锯齿宽度。
// 投影：形状往外扩 spread（params.w）、挪到 offset，模糊后只留形状外面的部分。
// 内阴影：形状外面是一块挡板，挡板的"洞"是形状往里缩 spread、挪到 offset，模糊后只留形状里面的部分。
// 圆角跟着加减（直角保持直角）
fn layered_shadow(in: VsOut, p: vec2<f32>, d: f32, aa: f32, color: vec4<f32>) -> vec4<f32> {
    let coverage = clamp(0.5 - d / aa, 0.0, 1.0);
    let inset = in.kind == INSET_SHADOW;
    let grow = select(in.params.w, -in.params.w, inset);
    let half = max(in.half_size + vec2(grow), vec2(0.0));
    let rounded = select(max(in.radii + vec4(grow), vec4(0.0)), vec4(0.0), in.radii <= vec4(0.0));
    let radii = min(rounded, vec4(min(half.x, half.y)));
    let sp = p - in.params.xy;
    let ds = sd_rounded_rect(sp, half, corner_radius(sp, radii));
    let sigma = in.params.z;
    // 在扩展后的形状外面的程度
    var outside: f32;
    if sigma > 0.0 {
        outside = 0.5 + 0.5 * erf(ds / (sigma * 1.41421356));
    } else {
        outside = clamp(0.5 + ds / aa, 0.0, 1.0);
    }
    if inset {
        return color * (outside * coverage);
    }
    return color * ((1.0 - outside) * (1.0 - coverage));
}

// 渐变边框：渐变的仿射变换把局部坐标（相对图元中心）变成渐变坐标
fn local_gradient(header: u32, p: vec2<f32>, pixel: vec2<f32>) -> vec4<f32> {
    let g = gradients[header];
    let o = gradients[header + 1u];
    return gradient_color(header, mat2x2<f32>(g.xy, g.zw) * p + o.xy, pixel);
}

// 在线性空间混合时，深色字看起来偏细、浅色字偏粗。按文字明度做伽马校正，
// 粗细接近设计稿里看到的样子（按 sRGB 混合的效果）
fn text_coverage(coverage: f32, lightness: f32) -> f32 {
    let gamma = 1.8;
    let dark = 1.0 - pow(1.0 - coverage, gamma);
    let light = pow(coverage, gamma);
    return mix(dark, light, lightness);
}

const INV_TAU: f32 = 0.15915494;

fn oklab_to_linear(lab: vec3<f32>) -> vec3<f32> {
    let l = lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z;
    let m = lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z;
    let s = lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z;
    let lms = vec3(l * l * l, m * m * m, s * s * s);
    return vec3(
        dot(lms, vec3(4.0767416621, -3.3077115913, 0.2309699292)),
        dot(lms, vec3(-1.2684380046, 2.6097574011, -0.3413193965)),
        dot(lms, vec3(-0.0041960863, -0.7034186147, 1.7076147010)),
    );
}

fn gradient_position(positions: u32, i: u32) -> f32 {
    return gradients[positions + i / 4u][i % 4u];
}

// 渐变坐标 p 处的颜色（线性预乘）。色标在插值空间里预乘后插值，再换到线性。
// 两端颜色不同的一段才抖动：纯色（两端同色、超出两端）一个像素都不变
fn gradient_color(header: u32, p: vec2<f32>, pixel: vec2<f32>) -> vec4<f32> {
    let info = gradients[header + 1u];
    let flags = u32(info.z);
    let count = u32(info.w);
    let shape = flags & 3u;
    var t = p.x;
    if shape == 1u {
        t = length(p);
    } else if shape == 2u {
        // 锥形：从正上方开始顺时针（y 轴向下）
        t = fract(atan2(p.x, -p.y) * INV_TAU);
    }
    let positions = header + 2u;
    let colors = positions + (count + 3u) / 4u;
    // 位置不超过 t 的最后一个色标；在第一个之前、最后一个之后取端点的颜色
    var i = 0u;
    for (var k = 1u; k < count; k++) {
        if gradient_position(positions, k) > t {
            break;
        }
        i = k;
    }
    var c = gradients[colors + i];
    var ramp = false;
    let p0 = gradient_position(positions, i);
    if i + 1u < count && t > p0 {
        let p1 = gradient_position(positions, i + 1u);
        let next = gradients[colors + i + 1u];
        ramp = any(next != c);
        c = mix(c, next, clamp((t - p0) / max(p1 - p0, 1e-6), 0.0, 1.0));
    }
    if c.a <= 1e-6 {
        return vec4(0.0);
    }
    let v = c.rgb / c.a;
    let space = (flags >> 2u) & 3u;
    var rgb = v;
    if space == 0u {
        rgb = oklab_to_linear(v);
    } else if space == 2u {
        rgb = srgb_to_linear_extended(v);
    }
    // 有扩展范围的色标（广色域、HDR）：放进目标能显示的范围；普通的渐变逐个分量截到 0..1
    if EXTENDED_COLORS && (flags & 16u) != 0u {
        rgb = fit_range(rgb);
    } else {
        rgb = clamp(rgb, vec3(0.0), vec3(1.0));
    }
    let color = vec4(rgb * c.a, c.a);
    if ramp {
        return dither(color, pixel);
    }
    return color;
}

// 8 位 sRGB 目标上的抖动：大面积平滑的颜色量化到 8 位会出现一圈圈色带。
// 在 sRGB 编码域加 ±0.45 个色阶的均匀噪声（交错梯度噪声，固定不闪烁），按"一个色阶对应多少
// 线性值"换算到线性空间。硬件的 sRGB 编码在色阶边界附近不精确，纯色加了噪声也会偶尔跳一级，
// 所以只用在平滑变化的颜色上
fn dither(c: vec4<f32>, pixel: vec2<f32>) -> vec4<f32> {
    if globals.dither == 0.0 || c.a <= 0.0 {
        return c;
    }
    let noise = fract(52.9829189 * fract(dot(pixel, vec2(0.06711056, 0.00583715)))) - 0.5;
    let linear = max(c.rgb / c.a, vec3(0.0));
    let step = select(2.2749 * pow(linear, vec3(0.58333)), vec3(1.0 / 12.92), linear < vec3(0.0031308));
    return vec4(max(c.rgb + step * (noise * 0.9 / 255.0), vec3(0.0)), c.a);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // 导数必须在统一控制流里计算，所以放在按种类分支之前
    let p = in.local;
    let d = sd_rounded_rect(p, in.half_size, corner_radius(p, in.radii));
    // 用距离场自身的屏幕空间导数做抗锯齿，旋转、缩放后依然正确
    let aa = max(length(vec2(dpdx(d), dpdy(d))), 1e-4);
    var clip = 1.0;
    if in.clip != 0u {
        clip = clip_coverage(in.clip, screen_position(in.position));
    }
    // 渐变代替填充色（形状、单色字形、路径）；采样纹理的图元这一项是负的，是滤镜
    var fill = in.fill;
    var lightness = in.lightness;
    let gradient = u32(max(round(in.gradient.z), 0.0));
    if gradient != 0u {
        fill = gradient_color(gradient, in.gradient.xy, in.position.xy);
        if in.kind == MASK_GLYPH {
            // HDR 的文字比白还亮：明度按 1 算
            let rgb = fill.rgb / max(fill.a, 1e-6);
            lightness = pow(clamp(dot(rgb, vec3(0.2126, 0.7152, 0.0722)), 0.0, 1.0), 1.0 / 3.0);
        }
    }

    if in.kind == MASK_GLYPH {
        let coverage = textureSampleLevel(mask_atlas, atlas_sampler, in.uv, 0.0).r;
        return fill * text_coverage(coverage, lightness) * in.opacity * clip;
    }
    if in.kind == COLOR_GLYPH {
        let t = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        var c = vec4(t.rgb * t.a, t.a);
        let matrix = u32(max(-round(in.gradient.z), 0.0));
        if matrix != 0u {
            c = color_filter(c, matrix);
        }
        return c * in.opacity * clip;
    }
    if in.kind == PATH {
        // 和距离场图形一样按线性覆盖率混合（文字才做伽马校正）
        let coverage = textureSampleLevel(path_atlas, atlas_sampler, in.uv, 0.0).r;
        return fill * coverage * in.opacity * clip;
    }
    if in.kind == IMAGE || in.kind == IMAGE_TEXTURE {
        // 元素的圆角矩形 × 图片自己的矩形（contain、none 时图片比元素小，边缘也要抗锯齿）
        let shape = clamp(0.5 - d / aa, 0.0, 1.0);
        let r = in.border_color;
        let outside = max(max(r.x - p.x, p.x - r.z), max(r.y - p.y, p.y - r.w));
        let inside = clamp(0.5 - outside / aa, 0.0, 1.0);
        var lod = in.lightness;
        if in.gradient.y > 0.5 {
            lod = in.gradient.x;
        }
        var c: vec4<f32>;
        if in.kind == IMAGE {
            // 夹在图片范围往里半个纹素（按较粗的那一级算），双线性过滤不会采到图集里的邻居
            let half = 0.5 * exp2(ceil(lod)) / vec2<f32>(textureDimensions(image_atlas));
            let uv = clamp(in.uv, in.params.xy + half, max(in.params.zw - half, in.params.xy + half));
            c = textureSampleLevel(image_atlas, image_sampler, uv, lod);
        } else {
            let uv = clamp(in.uv, in.params.xy, in.params.zw);
            c = textureSampleLevel(image_texture, image_sampler, uv, lod);
            // 浮点纹理（广色域、HDR 的图片）：放进这一帧的目标能显示的范围
            if EXTENDED_COLORS && image_info.peak > 0.0 {
                c = fit_image(c, image_info.peak);
            }
        }
        // 只有一张图片的元素带滤镜时不离屏，逐像素在这里算
        let matrix = u32(max(-round(in.gradient.z), 0.0));
        if matrix != 0u {
            c = color_filter(c, matrix);
        }
        return c * (shape * inside * in.opacity * clip);
    }
    if RICH_SHAPES && (in.kind == SHADOW || in.kind == INSET_SHADOW) {
        // 有渐变时颜色取渐变，shadow_color 只给透明度
        let color = select(in.shadow_color, fill * in.shadow_color.a, gradient != 0u);
        return layered_shadow(in, p, d, aa, color) * in.opacity * clip;
    }
    if in.kind == GROUP {
        // 离屏纹理里已经是线性预乘的颜色（半精度浮点）。组里有渐变时（params.w > 0）合成到 8 位目标要抖动
        let c = textureSampleLevel(effect_texture, atlas_sampler, in.uv, 0.0) * in.opacity * clip;
        if in.params.w > 0.0 {
            return dither(c, in.position.xy);
        }
        return c;
    }
    if in.kind == BACKDROP {
        // extra = 采样区域在屏幕上的位置和大小（物理像素）；params.xy = 区域在纹理里占的比例
        let uv = (in.position.xy + globals.origin - in.extra.xy) / in.extra.zw * in.params.xy;
        var c = textureSampleLevel(effect_texture, atlas_sampler, uv, 0.0);
        let gray = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
        c = vec4(max(mix(vec3(gray), c.rgb, in.params.w), vec3(0.0)), c.a);
        let coverage = clamp(0.5 - d / aa, 0.0, 1.0);
        return c * coverage * in.opacity * clip;
    }

    let coverage = clamp(0.5 - d / aa, 0.0, 1.0);
    var color: vec4<f32>;
    let border_width = in.params.w;
    if border_width > 0.0 {
        let inner = clamp(0.5 - (d + border_width) / aa, 0.0, 1.0);
        var border = in.border_color;
        // 渐变边框：下标在 uv.x 里（形状不用 uv）
        if RICH_SHAPES && in.extra.x > 0.0 {
            border = local_gradient(u32(in.extra.x), p, in.position.xy);
        }
        color = fill * inner + border * (coverage - inner);
    } else {
        color = fill * coverage;
    }

    if in.shadow_color.a > 0.0 {
        let sp = p - in.params.xy;
        let ds = sd_rounded_rect(sp, in.half_size, corner_radius(sp, in.radii));
        let sigma = in.params.z;
        var shadow: f32;
        if sigma > 0.0 {
            shadow = 0.5 - 0.5 * erf(ds / (sigma * 1.41421356));
        } else {
            shadow = clamp(0.5 - ds / aa, 0.0, 1.0);
        }
        // 阴影只出现在形状外部：形状半透明时，下面不会透出阴影
        color += in.shadow_color * shadow * (1.0 - coverage);
    }

    return color * in.opacity * clip;
}

// 带滤镜、遮罩、混合模式的分组合成（单独的管线，平常的图元不用为这些付出寄存器）。图元字段的用法：
//   uv            = 内容在纹理里的范围 (0, 0, u, v)
//   gradient      = 滤镜的颜色矩阵（0 表示没有）
//   params.xy     = 拷出来的后面的内容左上角在屏幕上的位置（物理像素），混合模式用
//   params.z * 2, params.w = 遮罩纹理的范围 (u, v)（遮罩和内容画在屏幕上同一个范围里）
//   color         = (混合模式, 遮罩方式, 要不要抖动, -) 各一个字节：遮罩方式 1 是透明度，2 是明度
@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    var clip = 1.0;
    if in.clip != 0u {
        clip = clip_coverage(in.clip, screen_position(in.position));
    }
    let codes = vec3<u32>(round(in.color.rgb * 255.0));
    var c = textureSampleLevel(effect_texture, atlas_sampler, in.uv, 0.0);
    if codes.y != 0u {
        let uv = in.uv / max(in.extra.zw, vec2(1e-9)) * vec2(in.params.z * 2.0, in.params.w);
        let m = textureSampleLevel(mask_texture, atlas_sampler, uv, 0.0);
        var coverage = m.a;
        if codes.y == 2u {
            coverage = dot(m.rgb, vec3(0.2126, 0.7152, 0.0722));
        }
        c = c * clamp(coverage, 0.0, 1.0);
    }
    let matrix = u32(max(-round(in.gradient.z), 0.0));
    if matrix != 0u {
        c = color_filter(c, matrix);
    }
    c = c * in.opacity * clip;
    if codes.x != 0u && c.a > 0.0 {
        // 后面的内容是线性预乘的；按定义混合（未预乘的颜色），硬件再按"盖在上面"加上露出来的后面
        let p = vec2<i32>(floor(in.position.xy + globals.origin - in.params.xy));
        let size = vec2<i32>(textureDimensions(backdrop_texture));
        let b = textureLoad(backdrop_texture, clamp(p, vec2(0), size - 1), 0);
        let cs = clamp(c.rgb / c.a, vec3(0.0), vec3(1.0));
        let cb = clamp(b.rgb / max(b.a, 1e-6), vec3(0.0), vec3(1.0));
        let mixed = blend_mode(codes.x, cb, cs);
        c = vec4(c.rgb * (1.0 - b.a) + c.a * b.a * mixed, c.a);
    }
    if codes.z != 0u {
        return dither(c, in.position.xy);
    }
    return c;
}

const BLEND_LUMA: vec3<f32> = vec3(0.2126, 0.7152, 0.0722);

fn blend_screen(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    return b + s - b * s;
}

fn blend_hard_light(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    return select(blend_screen(b, 2.0 * s - 1.0), b * 2.0 * s, s <= vec3(0.5));
}

fn blend_clip_color(c: vec3<f32>) -> vec3<f32> {
    let l = dot(c, BLEND_LUMA);
    let n = min(min(c.r, c.g), c.b);
    let x = max(max(c.r, c.g), c.b);
    var out = c;
    if n < 0.0 {
        out = l + (out - l) * l / max(l - n, 1e-6);
    }
    if x > 1.0 {
        out = l + (out - l) * (1.0 - l) / max(x - l, 1e-6);
    }
    return out;
}

fn blend_set_lum(c: vec3<f32>, l: f32) -> vec3<f32> {
    return blend_clip_color(c + (l - dot(c, BLEND_LUMA)));
}

fn blend_sat(c: vec3<f32>) -> f32 {
    return max(max(c.r, c.g), c.b) - min(min(c.r, c.g), c.b);
}

fn blend_set_sat(c: vec3<f32>, s: f32) -> vec3<f32> {
    let n = min(min(c.r, c.g), c.b);
    let x = max(max(c.r, c.g), c.b);
    if x - n <= 1e-6 {
        return vec3(0.0);
    }
    return (c - n) * s / (x - n);
}

// 混合模式（编号同 BlendMode），后面的颜色 b、自己的颜色 s 都是线性、未预乘的
fn blend_mode(mode: u32, b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    switch mode {
        case 1u: { return b * s; }
        case 2u: { return blend_screen(b, s); }
        case 3u: { return blend_hard_light(s, b); }
        case 4u: { return min(b, s); }
        case 5u: { return max(b, s); }
        case 6u: {
            let dodge = min(b / max(1.0 - s, vec3(1e-6)), vec3(1.0));
            return select(select(dodge, vec3(1.0), s >= vec3(1.0)), vec3(0.0), b <= vec3(0.0));
        }
        case 7u: {
            let burn = 1.0 - min((1.0 - b) / max(s, vec3(1e-6)), vec3(1.0));
            return select(select(burn, vec3(0.0), s <= vec3(0.0)), vec3(1.0), b >= vec3(1.0));
        }
        case 8u: { return blend_hard_light(b, s); }
        case 9u: {
            let d = select(sqrt(b), ((16.0 * b - 12.0) * b + 4.0) * b, b <= vec3(0.25));
            return select(b + (2.0 * s - 1.0) * (d - b), b - (1.0 - 2.0 * s) * b * (1.0 - b), s <= vec3(0.5));
        }
        case 10u: { return abs(b - s); }
        case 11u: { return b + s - 2.0 * b * s; }
        case 12u: { return blend_set_lum(blend_set_sat(s, blend_sat(b)), dot(b, BLEND_LUMA)); }
        case 13u: { return blend_set_lum(blend_set_sat(b, blend_sat(s)), dot(b, BLEND_LUMA)); }
        case 14u: { return blend_set_lum(s, dot(b, BLEND_LUMA)); }
        case 15u: { return blend_set_lum(b, dot(s, BLEND_LUMA)); }
        default: { return s; }
    }
}
