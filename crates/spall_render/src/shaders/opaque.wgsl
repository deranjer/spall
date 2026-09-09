// Linear-HDR direct-light pass. Display encoding happens only in tonemap.wgsl.
const PI: f32 = 3.14159265359;

struct Globals {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    light_view_proj: array<mat4x4<f32>, 4>,
    camera_pos: vec4<f32>,
    sun_dir: vec4<f32>,
    cascade_splits: vec4<f32>,
    params: vec4<f32>,
    light: vec4<f32>,
};
struct Material { base_color: vec4<f32>, params: vec4<f32>, };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> materials: array<Material>;
@group(0) @binding(2) var shadow_maps: texture_depth_2d_array;
@group(0) @binding(3) var shadow_sampler: sampler_comparison;
struct IndirectGlobals {
    origin_cell_size: vec4<f32>,
    dimensions: vec4<u32>,
    sky: vec4<f32>,
};
@group(1) @binding(0) var<storage, read> indirect_radiance: array<vec4<f32>>;
@group(1) @binding(1) var<uniform> indirect_globals: IndirectGlobals;

struct VsIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) local_uv: vec2<f32>,
    @location(3) ao: f32,
    @location(4) material: u32,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world_pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) local_uv: vec2<f32>,
    @location(3) ao: f32,
    @location(4) @interpolate(flat) material: u32,
};

@vertex fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = globals.view_proj * vec4<f32>(in.position, 1.0);
    out.world_pos = in.position;
    out.normal = in.normal;
    out.local_uv = in.local_uv;
    out.ao = in.ao;
    out.material = in.material;
    return out;
}

fn get_material(id: u32) -> Material {
    if id < arrayLength(&materials) { return materials[id]; }
    return Material(vec4<f32>(0.8, 0.1, 0.8, 1.0), vec4<f32>(0.8, 0.0, 0.0, 0.0));
}
fn cascade_index(eye_depth: f32) -> i32 {
    if eye_depth <= globals.cascade_splits.x { return 0; }
    if eye_depth <= globals.cascade_splits.y { return 1; }
    if eye_depth <= globals.cascade_splits.z { return 2; }
    return 3;
}
fn shadow_visibility(world_pos: vec3<f32>, n_dot_l: f32, cascade: i32) -> f32 {
    let clip = globals.light_view_proj[cascade] * vec4<f32>(world_pos, 1.0);
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) || ndc.z <= 0.0 || ndc.z >= 1.0 {
        return 1.0;
    }
    let dims = vec2<f32>(textureDimensions(shadow_maps));
    let texel = 1.0 / dims;
    let bias = 0.00025 + 0.0012 * (1.0 - n_dot_l);
    var sum = 0.0;
    for (var y = -1; y <= 1; y += 1) {
        for (var x = -1; x <= 1; x += 1) {
            sum += textureSampleCompare(shadow_maps, shadow_sampler, uv + vec2<f32>(f32(x), f32(y)) * texel, cascade, ndc.z - bias);
        }
    }
    return sum / 9.0;
}
fn fresnel_schlick(cos_theta: f32, f0: vec3<f32>) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(1.0 - cos_theta, 5.0);
}
fn distribution_ggx(n: vec3<f32>, h: vec3<f32>, roughness: f32) -> f32 {
    let a = roughness * roughness;
    let a2 = a * a;
    let n_dot_h = max(dot(n, h), 0.0);
    let denom = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    return a2 / max(PI * denom * denom, 0.0001);
}
fn geometry_schlick(n_dot_v: f32, roughness: f32) -> f32 {
    let r = roughness + 1.0;
    let k = (r * r) / 8.0;
    return n_dot_v / max(n_dot_v * (1.0 - k) + k, 0.0001);
}

fn sample_indirect(world_pos: vec3<f32>, normal: vec3<f32>, base: vec3<f32>) -> vec3<f32> {
    if indirect_globals.dimensions.z == 0u { return vec3<f32>(0.0); }
    let dim = i32(indirect_globals.dimensions.x);
    // Step past the occupied cache cell that owns the raster surface. A
    // sub-cell offset can quantise back into the wall at negative faces.
    let sample_pos = world_pos + normal * indirect_globals.origin_cell_size.w * 1.1;
    let coord = vec3<i32>(floor((sample_pos - indirect_globals.origin_cell_size.xyz) / indirect_globals.origin_cell_size.w));
    if any(coord < vec3<i32>(0)) || any(coord >= vec3<i32>(dim)) { return vec3<f32>(0.0); }
    let index = u32(coord.x + dim * (coord.y + dim * coord.z));
    return base * indirect_radiance[index].rgb / PI;
}

@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let mode = i32(round(globals.params.x));
    let mat = get_material(in.material);
    var base = mat.base_color.rgb;
    let roughness = clamp(mat.params.x, 0.04, 1.0);
    let metallic = clamp(mat.params.y, 0.0, 1.0);
    let grid = fract(in.local_uv);
    let line = min(min(grid.x, grid.y), min(1.0 - grid.x, 1.0 - grid.y));
    base *= 0.92 + 0.08 * smoothstep(0.0, 0.06, line);

    if mode == 1 { return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0); }
    let eye_depth = -(globals.view * vec4<f32>(in.world_pos, 1.0)).z;
    if mode == 2 {
        let g = clamp((eye_depth - globals.params.y) / (globals.params.z - globals.params.y), 0.0, 1.0);
        return vec4<f32>(vec3<f32>(1.0 - g), 1.0);
    }
    if mode == 3 { return vec4<f32>(base, 1.0); }
    if mode == 5 { return vec4<f32>(vec3<f32>(roughness), 1.0); }

    let indirect = sample_indirect(in.world_pos, n, base);
    if mode == 6 { return vec4<f32>(indirect, 1.0); }

    let cascade = cascade_index(eye_depth);
    let l = normalize(-globals.sun_dir.xyz);
    let v = normalize(globals.camera_pos.xyz - in.world_pos);
    let h = normalize(v + l);
    let n_dot_l = max(dot(n, l), 0.0);
    let n_dot_v = max(dot(n, v), 0.001);
    let visibility = shadow_visibility(in.world_pos, n_dot_l, cascade);
    if mode == 4 {
        let colors = array<vec3<f32>, 4>(
            vec3<f32>(0.95, 0.22, 0.18), vec3<f32>(0.20, 0.85, 0.28),
            vec3<f32>(0.20, 0.42, 0.95), vec3<f32>(0.90, 0.34, 0.88)
        );
        return vec4<f32>(colors[cascade] * (0.3 + 0.7 * visibility), 1.0);
    }
    let f0 = mix(vec3<f32>(0.04), base, metallic);
    let f = fresnel_schlick(max(dot(h, v), 0.0), f0);
    let d = distribution_ggx(n, h, roughness);
    let g = geometry_schlick(n_dot_v, roughness) * geometry_schlick(n_dot_l, roughness);
    let specular = (d * g * f) / max(4.0 * n_dot_v * n_dot_l, 0.001);
    let diffuse = (vec3<f32>(1.0) - f) * (1.0 - metallic) * base / PI;
    let direct = (diffuse + specular) * globals.light.x * n_dot_l * visibility;
    let sky = mix(globals.light.y, globals.light.z, n.y * 0.5 + 0.5);
    let ambient = base * sky * mix(0.35, 1.0, clamp(in.ao, 0.0, 1.0));
    return vec4<f32>(ambient + direct + indirect, 1.0);
}
