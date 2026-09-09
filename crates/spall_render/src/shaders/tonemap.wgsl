struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    if globals.debug_passthrough > 0.5 {
        return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    return vec4<f32>(aces_fitted(linear * globals.exposure), 1.0);
}
