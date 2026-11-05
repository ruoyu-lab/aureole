// 模糊：先逐级对半缩小，直到标准差不超过几个像素，再在小图上做横、竖两遍高斯。
// 每遍画一个铺满目标已用区域的三角形。源纹理可能比已用区域大（纹理池按块分配），
// 采样时限制在已用区域之内，边缘像素向外延伸。

struct Params {
    // 源纹理已用区域占整个纹理的比例
    src_extent: vec2<f32>,
    // 源纹理一个像素的 uv 大小
    src_texel: vec2<f32>,
    // 高斯：采样方向（单位像素）；缩小：不用
    direction: vec2<f32>,
    // 高斯的标准差，源纹理像素
    sigma: f32,
    // 0 = 对半缩小，1 = 高斯
    mode: u32,
}

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(0) @binding(2) var<uniform> params: Params;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    // 0..1，目标已用区域里的位置
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32) -> VsOut {
    // 一个盖住整个视口的大三角形
    let corner = vec2(f32((vertex << 1u) & 2u), f32(vertex & 2u));
    var out: VsOut;
    out.position = vec4(corner * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
    out.uv = corner;
    return out;
}

fn sample_clamped(uv: vec2<f32>) -> vec4<f32> {
    let lo = params.src_texel * 0.5;
    let hi = params.src_extent - params.src_texel * 0.5;
    return textureSampleLevel(source, source_sampler, clamp(uv, lo, hi), 0.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv * params.src_extent;
    if params.mode == 0u {
        // 4 次双线性采样 = 4x4 个源像素的平均，缩小时不闪烁
        let t = params.src_texel;
        return 0.25 * (sample_clamped(uv + vec2(-t.x, -t.y)) + sample_clamped(uv + vec2(t.x, -t.y))
            + sample_clamped(uv + vec2(-t.x, t.y)) + sample_clamped(uv + vec2(t.x, t.y)));
    }
    let radius = i32(ceil(params.sigma * 3.0));
    let step = params.direction * params.src_texel;
    var sum = vec4(0.0);
    var weights = 0.0;
    for (var i = -radius; i <= radius; i++) {
        let x = f32(i);
        let w = exp(-x * x / (2.0 * params.sigma * params.sigma));
        sum += sample_clamped(uv + step * x) * w;
        weights += w;
    }
    return sum / weights;
}
