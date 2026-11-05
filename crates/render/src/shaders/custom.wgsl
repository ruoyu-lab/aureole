// 自定义着色器：拼在 quad.wgsl 和用户的 `shade` 函数后面。
//
// 用户写：
//     fn shade(in: ShaderInput) -> vec4<f32> { ... }
// 返回 sRGB、未预乘的颜色（和 `Color::hex` 写的颜色一样），圆角、透明度、裁剪由这里套上。

// 自定义着色器拿到的输入
struct ShaderInput {
    // 在元素里的位置，0..1
    uv: vec2<f32>,
    // 元素大小，逻辑像素
    size: vec2<f32>,
    // 秒
    time: f32,
    // 元素上传入的 4 个参数
    params: vec4<f32>,
    // 元素的背景色（sRGB，未预乘）
    color: vec4<f32>,
}

// 自定义着色器的入口：调用用户写的 `shade`，再套上圆角、透明度和裁剪。
// `shade` 返回 sRGB、未预乘的颜色（和 `Color::hex` 写的颜色一样）
fn custom_fragment(in: VsOut) -> vec4<f32> {
    let p = in.local;
    let d = sd_rounded_rect(p, in.half_size, corner_radius(p, in.radii));
    let aa = max(length(vec2(dpdx(d), dpdy(d))), 1e-4);
    var clip = 1.0;
    if in.clip != 0u {
        clip = clip_coverage(in.clip, screen_position(in.position));
    }
    var input: ShaderInput;
    input.uv = (p + in.half_size) / max(in.half_size * 2.0, vec2(1e-6));
    input.size = in.half_size * 2.0;
    input.time = globals.time;
    input.params = in.extra;
    input.color = in.color;
    let c = clamp(shade(input), vec4(0.0), vec4(1.0));
    let coverage = clamp(0.5 - d / aa, 0.0, 1.0);
    return premultiply(c) * coverage * in.opacity * clip;
}

@fragment
fn fs_custom(in: VsOut) -> @location(0) vec4<f32> {
    return custom_fragment(in);
}
