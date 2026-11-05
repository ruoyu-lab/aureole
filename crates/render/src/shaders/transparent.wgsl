// 窗口透出系统材质时最后一步：把画好的根（线性、预乘透明度）换成系统合成要的样子。
//
// 系统把交换链的像素当作"sRGB 编码的值，按编码后的值预乘"来合成；渲染器画出来的是线性的值预乘。
// sRGB 编码的目标写入时只编码 RGB、不管透明度，直接写的话半透明的地方比应有的亮很多
// （一半透明的白：线性预乘是 0.5，编码后是 0.735，应该是 1.0 编码后再乘 0.5，也就是 0.5）。
// 所以先去掉预乘、编码、按编码后的值预乘，再解码回去，让目标写入时编码回这个值。不透明的像素原样不动。

@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // 盖住整个目标的一个三角形
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn encode(linear: vec3<f32>) -> vec3<f32> {
    let low = linear * 12.92;
    let high = 1.055 * pow(linear, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, low, linear <= vec3<f32>(0.0031308));
}

fn decode(encoded: vec3<f32>) -> vec3<f32> {
    let low = encoded / 12.92;
    let high = pow((encoded + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, encoded <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let color = textureLoad(source, vec2<i32>(position.xy), 0);
    if color.a <= 0.0 || color.a >= 1.0 {
        return color;
    }
    let straight = clamp(color.rgb / color.a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(decode(encode(straight) * color.a), color.a);
}
