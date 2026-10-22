// `grade`: saturation, edge blur; `tint_frost`: rgb tint, frost amount;
// `vignette`: edge colour, strength. See `ScreenGrade`.
struct ToneGlobals {
    exposure: f32,
    debug_passthrough: f32,
    saturation: f32,
    edge_blur: f32,
    tint_frost: vec4<f32>,
    vignette: vec4<f32>,
};
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // ENG-60: the equivalent `array<vec2<f32>, 3>` indexed by `index` (three
    // literal corners of an over-sized fullscreen triangle) reproduces the
    // documented Vulkan pipeline-creation crash on the recorded NVIDIA driver
    // whenever this fragment shader also reads a uniform buffer — see
    // docs/reports/ENG-60.md. This index-arithmetic form is the standard
    // fullscreen-triangle trick and yields byte-identical `x`/`y` for
    // index in 0..3: (-1,-1), (3,-1), (-1,3).
    let x = f32(i32(index << 1u) & 2) * 2.0 - 1.0;
    let y = f32(i32(index) & 2) * 2.0 - 1.0;
    var out: VsOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x, y) * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
fn hash2(p: vec2<f32>) -> f32 {
    return fract(sin(dot(p, vec2<f32>(127.1, 311.7))) * 43758.5453);
}
fn value_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash2(i);
    let b = hash2(i + vec2<f32>(1.0, 0.0));
    let c = hash2(i + vec2<f32>(0.0, 1.0));
    let d = hash2(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}
// Distance from the screen centre: 0 there, ~1 at the middle of an edge.
fn edge_distance(uv: vec2<f32>, aspect: f32) -> f32 {
    let p = (uv - vec2<f32>(0.5)) * 2.0;
    return length(vec2<f32>(p.x * sqrt(aspect), p.y / sqrt(aspect)));
}
// Pale ice veins that grow inward from the edge as `amount` rises.
fn frost(uv: vec2<f32>, aspect: f32, edge: f32, amount: f32) -> f32 {
    let q = vec2<f32>(uv.x * aspect, uv.y) * 18.0;
    var n = 0.0;
    var scale = 1.0;
    var weight = 0.5;
    for (var octave = 0; octave < 4; octave = octave + 1) {
        n = n + weight * value_noise(q * scale);
        scale = scale * 2.1;
        weight = weight * 0.5;
    }
    let veins = pow(1.0 - abs(n * 2.0 - 1.0), 6.0);
    let reach = 1.25 - amount * 0.75;
    let mask = smoothstep(reach - 0.25, reach + 0.1, edge + (n - 0.5) * 0.35);
    return clamp(mask * (0.35 + 0.65 * veins), 0.0, 1.0) * amount;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let centre = textureSample(hdr, linear_sampler, in.uv).rgb;
    if globals.debug_passthrough > 0.5 {
        return vec4<f32>(clamp(centre, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    let size = vec2<f32>(textureDimensions(hdr));
    let aspect = size.x / max(size.y, 1.0);
    let edge = edge_distance(in.uv, aspect);
    // Edge blur: an 8-tap ring whose radius grows toward the screen edge.
    var linear = centre;
    let blur = globals.edge_blur * smoothstep(0.2, 1.1, edge);
    if blur > 0.001 {
        let radius = blur * 0.012 * vec2<f32>(1.0 / aspect, 1.0);
        var sum = centre;
        for (var tap = 0; tap < 8; tap = tap + 1) {
            let angle = f32(tap) * 0.785398;
            // Level 0 explicitly: implicit-derivative sampling is not allowed
            // in this non-uniform branch.
            sum = sum + textureSampleLevel(hdr, linear_sampler, in.uv + vec2<f32>(cos(angle), sin(angle)) * radius, 0.0).rgb;
        }
        linear = sum / 9.0;
    }
    var color = aces_fitted(linear * globals.exposure);
    let luma = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = mix(vec3<f32>(luma), color, globals.saturation) * globals.tint_frost.rgb;
    let ice = frost(in.uv, aspect, edge, globals.tint_frost.w);
    color = mix(color, vec3<f32>(0.86, 0.93, 1.0), ice * 0.85);
    let vignette = smoothstep(0.35, 1.25, edge) * globals.vignette.w;
    color = mix(color, globals.vignette.rgb, vignette);
    return vec4<f32>(color, 1.0);
}
