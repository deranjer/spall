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
    sun: vec4<f32>,
    sky_color: vec4<f32>,
    ground_color: vec4<f32>,
    moon_dir: vec4<f32>,
    moon: vec4<f32>,
    moon_phase: vec4<f32>,
    // Point lights (xyz position, w range) and colour x intensity; count in x.
    point_lights: array<vec4<f32>, 8>,
    point_colors: array<vec4<f32>, 8>,
    point_count: vec4<f32>,
    // x: camera under the water surface, y: that surface's height, z: water
    // clock in seconds, w: a water field is bound.
    water_cam: vec4<f32>,
    // Water field origin xz, cell size, cells per side.
    water_field: vec4<f32>,
};
struct Material { base_color: vec4<f32>, params: vec4<f32>, };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> materials: array<Material>;
@group(0) @binding(2) var shadow_maps: texture_depth_2d_array;
@group(0) @binding(3) var shadow_sampler: sampler_comparison;
// Water surface height per horizontal cell; very negative where there is none.
@group(0) @binding(4) var water_field_tex: texture_2d<f32>;
struct IndirectGlobals {
    origin_cell_size: vec4<f32>,
    dimensions: vec4<u32>,
    sky: vec4<f32>,
};
@group(1) @binding(0) var<storage, read> indirect_radiance: array<vec4<f32>>;
@group(1) @binding(1) var<uniform> indirect_globals: IndirectGlobals;

// Sky visibility (see sky_visibility.wgsl). `dimensions.y == 0` is the disabled
// stub: shading then falls back to the unconditional hemispheric ambient.
struct SkyGlobals {
    origin_cell_size: vec4<f32>,
    dimensions: vec4<u32>,
};
// Six unorm8 face visibilities in two words; byte 2 of the second word is 255
// for an air cell and 0 for a solid or unknown one (so no occupancy buffer is
// bound here: the stage's storage-buffer budget is 4 on downlevel devices).
@group(2) @binding(0) var<storage, read> sky_faces: array<vec2<u32>>;
@group(2) @binding(1) var<uniform> sky_globals: SkyGlobals;
// One-bounce radiance per air cell (see bounce.wgsl); used when dimensions.w == 1.
// Six shared-exponent RGB face radiances per cell.
@group(2) @binding(2) var<storage, read> sky_bounce: array<u32>;

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
    // 1 for static, world-aligned terrain (cube instances or terrain meshes),
    // where per-voxel colour jitter is stable; 0 for body meshes and rotated
    // bodies, whose cells move.
    @location(5) @interpolate(flat) jitter_on: f32,
};

fn mesh_vertex(in: VsIn, jitter_on: f32) -> VsOut {
    var out: VsOut;
    out.clip = globals.view_proj * vec4<f32>(in.position, 1.0);
    out.world_pos = in.position;
    out.normal = in.normal;
    out.local_uv = in.local_uv;
    out.ao = in.ao;
    out.material = in.material;
    out.jitter_on = jitter_on;
    return out;
}

@vertex fn vs_main(in: VsIn) -> VsOut {
    return mesh_vertex(in, 0.0);
}

// Static terrain meshes: same as `vs_main`, but world-aligned and fixed, so the
// per-voxel colour jitter (keyed on the world cell) is stable.
@vertex fn vs_terrain(in: VsIn) -> VsOut {
    return mesh_vertex(in, 1.0);
}

// Instanced-cube geometry path: one shared unit cube per instance. Everything
// after the vertex stage (`fs_main`) is shared with the mesh path.
struct CubeIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) local_uv: vec2<f32>,
    @location(3) offset: vec3<f32>,
    @location(4) material: u32,
    @location(5) size: vec3<f32>,
    @location(6) rotation: vec4<f32>,
};
fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}
@vertex fn vs_cube(in: CubeIn) -> VsOut {
    var out: VsOut;
    let world = quat_rotate(in.rotation, in.position * in.size) + in.offset;
    out.clip = globals.view_proj * vec4<f32>(world, 1.0);
    out.world_pos = world;
    out.normal = quat_rotate(in.rotation, in.normal);
    out.local_uv = in.local_uv;
    // No baked AO on this path; full ambient (see docs/reports/ENG-94.md).
    out.ao = 1.0;
    out.material = in.material;
    out.jitter_on = select(0.0, 1.0, abs(in.rotation.w) > 0.99999);
    return out;
}

// Three decorrelated values in [0, 1) from a voxel's integer cell (Hoskins hash).
fn hash_cell(c: vec3<f32>) -> vec3<f32> {
    var p = fract(c * vec3<f32>(0.1031, 0.1030, 0.0973));
    p += dot(p, p.yxz + 33.33);
    return fract((p.xxy + p.yyx) * p.zyx);
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
// Sun shadows: cascaded maps, normal-offset + slope-scaled bias in world texel
// units, and percentage-closer soft shadows (PCSS). The penumbra is physically
// motivated -- it widens with the receiver-to-occluder distance and the sun's
// angular size (`globals.sun_dir.w` = tan of the half angle) -- with a floor of
// one texel of filtering so edges are never aliased. See docs/reports/ENG-96.md.
const SHADOW_TAPS: i32 = 16;
// Occluders further than this above a receiver are not searched for.
const MAX_OCCLUDER_DISTANCE_M: f32 = 24.0;
const MAX_FILTER_TEXELS: f32 = 24.0;

fn interleaved_gradient_noise(pixel: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(pixel, vec2<f32>(0.06711056, 0.00583715))));
}
fn vogel_disk(i: i32, phi: f32) -> vec2<f32> {
    let r = sqrt((f32(i) + 0.5) / f32(SHADOW_TAPS));
    let theta = f32(i) * 2.39996323 + phi;
    return r * vec2<f32>(cos(theta), sin(theta));
}

fn shadow_visibility(world_pos: vec3<f32>, normal: vec3<f32>, n_dot_l: f32, cascade: i32, pixel: vec2<f32>) -> f32 {
    let m = globals.light_view_proj[cascade];
    let dims = vec2<f32>(textureDimensions(shadow_maps));
    // The light projection is orthographic: its row lengths are 1/half-extent
    // (x) and 1/depth-range (z), so a texel's world size and the world length
    // of a depth step fall out of the matrix.
    let half_extent = 1.0 / length(vec3<f32>(m[0][0], m[1][0], m[2][0]));
    let depth_range = 1.0 / length(vec3<f32>(m[0][2], m[1][2], m[2][2]));
    let texel_world = 2.0 * half_extent / dims.x;
    let grazing = 1.0 - n_dot_l;
    let offset_pos = world_pos + normal * texel_world * (0.75 + 1.5 * grazing);
    let clip = m * vec4<f32>(offset_pos, 1.0);
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 1.0 - (ndc.y * 0.5 + 0.5));
    if any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0)) || ndc.z <= 0.0 || ndc.z >= 1.0 {
        return 1.0;
    }
    let z = ndc.z - texel_world * (0.5 + 1.0 * grazing) / depth_range;
    let phi = interleaved_gradient_noise(pixel) * 6.2831853;
    let tan_half = globals.sun_dir.w;

    // 1. Blocker search: average depth of occluders within the widest
    //    penumbra this receiver could have.
    let search_texels = clamp(MAX_OCCLUDER_DISTANCE_M * tan_half / texel_world, 1.5, MAX_FILTER_TEXELS);
    var blocker_sum = 0.0;
    var blockers = 0.0;
    let max_index = vec2<i32>(dims) - vec2<i32>(1);
    for (var i = 0; i < SHADOW_TAPS; i += 1) {
        let tap_uv = uv + vogel_disk(i, phi) * search_texels / dims;
        let coord = clamp(vec2<i32>(floor(tap_uv * dims)), vec2<i32>(0), max_index);
        let occluder = textureLoad(shadow_maps, coord, cascade, 0);
        if occluder < z {
            blocker_sum += occluder;
            blockers += 1.0;
        }
    }
    if blockers < 0.5 { return 1.0; }

    // 2. Penumbra: distance to the occluder times the sun's angular size.
    let occluder_distance_m = (z - blocker_sum / blockers) * depth_range;
    let filter_texels = clamp(occluder_distance_m * tan_half / texel_world, 1.0, MAX_FILTER_TEXELS);

    // 3. Filter.
    var lit = 0.0;
    for (var i = 0; i < SHADOW_TAPS; i += 1) {
        let tap_uv = uv + vogel_disk(i, phi) * filter_texels / dims;
        lit += textureSampleCompareLevel(shadow_maps, shadow_sampler, tap_uv, cascade, z);
    }
    return lit / f32(SHADOW_TAPS);
}
fn sky_face_value(packed: vec2<u32>, face: u32) -> f32 {
    let word = select(packed.x, packed.y, face >= 4u);
    return f32((word >> ((face & 3u) * 8u)) & 255u) / 255.0;
}

// Sky/ground radiance seen along a direction whose vertical component is dy.
fn hemisphere_radiance(dy: f32) -> vec3<f32> {
    return mix(globals.ground_color.rgb, globals.sky_color.rgb, dy * 0.5 + 0.5);
}

// Sky colour a vertical face receives through its unblocked rays. Sky
// visibility already discards every ray that hits the ground or a wall, so the
// rays that remain all point above the horizon and see sky, not the average of
// sky and ground the legacy unconditional ambient used for side faces. Blending
// the dark ground colour in here counted the blocked half of the hemisphere a
// second time and left open-air walls in shade about half as bright as they
// should be; the ground's contribution comes from the bounce term instead.
const SIDE_SKY_DY: f32 = 0.5;

fn decode_rgb9e5(v: u32) -> vec3<f32> {
    let scale = exp2(f32(v >> 27u) - 24.0);
    return vec3<f32>(f32(v & 511u), f32((v >> 9u) & 511u), f32((v >> 18u) & 511u)) * scale;
}

struct SkyLight { radiance: vec3<f32>, visibility: f32, bounce: vec3<f32>, };

// Skylight arriving at a surface: sky visibility of the three axis faces the
// normal points to (an ambient cube), trilinearly interpolated over the air
// cells around a point one cell in front of the surface, times the sky/ground
// radiance for those faces. Without occupancy data, or outside the cache, this
// is the legacy unconditional hemispheric ambient.
fn sky_light(world_pos: vec3<f32>, n: vec3<f32>) -> SkyLight {
    var result: SkyLight;
    result.radiance = hemisphere_radiance(n.y);
    result.visibility = 1.0;
    result.bounce = vec3<f32>(0.0);
    if sky_globals.dimensions.y == 0u { return result; }
    let dim = i32(sky_globals.dimensions.x);
    let cell_size = sky_globals.origin_cell_size.w;
    let sample_pos = world_pos + n * (cell_size * 1.1);
    let c = (sample_pos - sky_globals.origin_cell_size.xyz) / cell_size - vec3<f32>(0.5);
    if any(c < vec3<f32>(0.0)) || any(c >= vec3<f32>(f32(dim - 1))) { return result; }
    let base = vec3<i32>(floor(c));
    let f = c - vec3<f32>(base);
    let face_x = select(1u, 0u, n.x >= 0.0);
    let face_y = select(3u, 2u, n.y >= 0.0);
    let face_z = select(5u, 4u, n.z >= 0.0);
    var acc = vec3<f32>(0.0);
    var bounce_acc = vec3<f32>(0.0);
    var weight = 0.0;
    let bounce_on = sky_globals.dimensions.w == 1u;
    for (var i = 0; i < 8; i += 1) {
        let o = vec3<i32>(i & 1, (i >> 1) & 1, (i >> 2) & 1);
        let cell = base + o;
        let idx = u32(cell.x + dim * (cell.y + dim * cell.z));
        // Solid and unknown cells contribute nothing (and no weight).
        let packed = sky_faces[idx];
        if ((packed.y >> 16u) & 255u) == 0u { continue; }
        let w = select(1.0 - f.x, f.x, o.x == 1)
            * select(1.0 - f.y, f.y, o.y == 1)
            * select(1.0 - f.z, f.z, o.z == 1);
        acc += w * vec3<f32>(
            sky_face_value(packed, face_x),
            sky_face_value(packed, face_y),
            sky_face_value(packed, face_z),
        );
        weight += w;
    }
    // Bounce is a low-frequency term estimated from a handful of rays per cell,
    // so a single cell is noisy. Reconstruct it with a wider tent filter (3
    // cells per axis, radius 1.5 cells) instead of the 2-cell trilinear the
    // skylight uses; solid and unknown cells still contribute nothing.
    var bounce_weight = 0.0;
    if bounce_on {
        let nearest = vec3<i32>(floor(c + vec3<f32>(0.5)));
        for (var k = 0; k < 27; k += 1) {
            let o = vec3<i32>(k % 3 - 1, (k / 3) % 3 - 1, k / 9 - 1);
            let cell = nearest + o;
            if any(cell < vec3<i32>(0)) || any(cell >= vec3<i32>(dim)) { continue; }
            let d = abs(vec3<f32>(cell) - c);
            let tw = max(1.5 - d, vec3<f32>(0.0));
            let w = tw.x * tw.y * tw.z;
            if w <= 0.0 { continue; }
            let idx = u32(cell.x + dim * (cell.y + dim * cell.z));
            let packed = sky_faces[idx];
            if ((packed.y >> 16u) & 255u) == 0u { continue; }
            bounce_acc += w * (n.x * n.x * decode_rgb9e5(sky_bounce[idx * 6u + face_x])
                + n.y * n.y * decode_rgb9e5(sky_bounce[idx * 6u + face_y])
                + n.z * n.z * decode_rgb9e5(sky_bounce[idx * 6u + face_z]));
            bounce_weight += w;
        }
    }
    // Every neighbour solid: buried, nothing sees the sky.
    let v = select(vec3<f32>(0.0), acc / max(weight, 0.0001), weight > 0.0001);
    let w = n * n;
    let up = select(-1.0, 1.0, n.y >= 0.0);
    result.radiance = w.x * v.x * hemisphere_radiance(SIDE_SKY_DY)
        + w.y * v.y * hemisphere_radiance(up)
        + w.z * v.z * hemisphere_radiance(SIDE_SKY_DY);
    result.visibility = w.x * v.x + w.y * v.y + w.z * v.z;
    if bounce_on && bounce_weight > 0.0001 { result.bounce = bounce_acc / bounce_weight; }
    return result;
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

// ---- Water look (presentation only; see spall_render::water) ----
const WATER_NONE: f32 = -1.0e8;
// Per-metre absorption in linear RGB: red dies first, blue last.
const WATER_ABSORB: vec3<f32> = vec3<f32>(0.45, 0.11, 0.05);
// Fog distance snaps to this many metres (one voxel is 0.25 m) so the
// falloff stays chunky instead of reading as smooth volumetric haze.
const WATER_BAND_M: f32 = 0.25;

// Size of the faint grain cells on the water surface (one voxel) and how strong
// they are: 0.0 is glass, 0.2 is plainly checkered.
const WATER_VOXEL_M: f32 = 0.25;
const WATER_VOXEL_HINT: f32 = 0.22;

// Top and bottom of the selected wet run, or WATER_NONE for a dry column.
fn water_bounds_at(xz: vec2<f32>) -> vec2<f32> {
    if globals.water_cam.w < 0.5 { return vec2<f32>(WATER_NONE); }
    let c = vec2<i32>(floor((xz - globals.water_field.xy) / globals.water_field.z));
    let dim = i32(globals.water_field.w);
    if any(c < vec2<i32>(0)) || any(c >= vec2<i32>(dim)) { return vec2<f32>(WATER_NONE); }
    return textureLoad(water_field_tex, c, 0).rg;
}

// Light scattered back toward the viewer by water at `depth` below the surface.
fn water_scatter(depth: f32) -> vec3<f32> {
    let sky_lum = dot(globals.sky_color.rgb, vec3<f32>(0.3, 0.59, 0.11));
    let sun_up = max(-globals.sun_dir.y, 0.0);
    let available = 0.6 * sky_lum + 0.05 * globals.sun.w * sun_up;
    let fade = max(exp(-0.08 * max(depth, 0.0)), 0.10);
    return vec3<f32>(0.04, 0.34, 0.44) * available * fade;
}

// Blends `lit` toward the water's scatter colour over the stretch of the view
// ray that runs through water, from `enter` to `leave`.
fn water_fog(lit: vec3<f32>, enter: vec3<f32>, leave: vec3<f32>, surface_y: f32) -> vec3<f32> {
    let path = floor(length(leave - enter) / WATER_BAND_M + 0.5) * WATER_BAND_M;
    let t = exp(-WATER_ABSORB * path);
    let depth = surface_y - 0.5 * (enter.y + leave.y);
    return lit * t + water_scatter(depth) * (vec3<f32>(1.0) - t);
}

// Fog for a fragment at `p` seen from the camera, if the ray crosses water.
fn apply_water_view(lit: vec3<f32>, p: vec3<f32>, probe_xz: vec2<f32>) -> vec3<f32> {
    if globals.water_cam.w < 0.5 { return lit; }
    let cam = globals.camera_pos.xyz;
    let cam_in = globals.water_cam.x > 0.5;
    let bounds = water_bounds_at(probe_xz);
    // Dry spaces below a pool are not underwater, even at the same X/Z.
    if p.y < bounds.y { return lit; }
    var s = bounds.x;
    if cam_in { s = globals.water_cam.y; }
    if s <= WATER_NONE { return lit; }
    let d = p - cam;
    var enter = cam;
    var leave = p;
    if cam_in {
        if p.y > s { leave = cam + d * ((s - cam.y) / max(d.y, 1.0e-5)); }
    } else {
        if p.y >= s { return lit; }
        if cam.y > s { enter = cam + d * ((s - cam.y) / min(d.y, -1.0e-5)); }
    }
    return water_fog(lit, enter, leave, s);
}

// Drifting caustic veins (Worley F2-F1) sampled on a quarter-metre grid and
// stepped at four frames a second, so the pattern is blocky like the world.
fn water_caustics(p: vec3<f32>, depth: f32, n: vec3<f32>) -> f32 {
    let time = floor(globals.water_cam.z * 4.0) * 0.25;
    let q = (floor(p.xz * 4.0) + vec2<f32>(0.5)) * 0.25;
    let g = q * 0.8 + vec2<f32>(time * 0.31, time * 0.19);
    let cell = floor(g);
    let f = g - cell;
    var f1 = 8.0;
    var f2 = 8.0;
    for (var j = -1; j <= 1; j += 1) {
        for (var i = -1; i <= 1; i += 1) {
            let o = vec2<f32>(f32(i), f32(j));
            let h = hash_cell(vec3<f32>(cell + o, 11.0)).xy;
            let dd = length(o + h - f);
            if dd < f1 {
                f2 = f1;
                f1 = dd;
            } else if dd < f2 {
                f2 = dd;
            }
        }
    }
    let vein = 1.0 - smoothstep(0.0, 0.22, f2 - f1);
    let strength = exp(-0.12 * depth) * clamp(n.y, 0.0, 1.0);
    return mix(1.0, 0.72 + 1.0 * vein, strength);
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
    // Per-voxel colour jitter: the cell just inside this face (boxes merge many
    // voxels, so the variation comes from position, not from the instance).
    let jitter = mat.params.w * in.jitter_on;
    if jitter > 0.0 {
        let cell = floor((in.world_pos - n * 0.02) / 0.25);
        let h = hash_cell(cell) * 2.0 - vec3<f32>(1.0);
        base = max(base * (vec3<f32>(1.0) + jitter * (vec3<f32>(h.x) + 0.45 * h.yzx)), vec3<f32>(0.0));
    }

    if mode == 1 { return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0); }
    let eye_depth = -(globals.view * vec4<f32>(in.world_pos, 1.0)).z;
    if mode == 2 {
        let g = clamp((eye_depth - globals.params.y) / (globals.params.z - globals.params.y), 0.0, 1.0);
        return vec4<f32>(vec3<f32>(1.0 - g), 1.0);
    }
    if mode == 3 { return vec4<f32>(base, 1.0); }
    if mode == 5 { return vec4<f32>(vec3<f32>(roughness), 1.0); }

    let skylight = sky_light(in.world_pos, n);
    let indirect = sample_indirect(in.world_pos, n, base);
    if mode == 6 { return vec4<f32>(indirect + base * skylight.bounce, 1.0); }

    if mode == 8 { return vec4<f32>(vec3<f32>(skylight.visibility), 1.0); }
    let cascade = cascade_index(eye_depth);
    let l = normalize(-globals.sun_dir.xyz);
    let v = normalize(globals.camera_pos.xyz - in.world_pos);
    let h = normalize(v + l);
    let n_dot_l = max(dot(n, l), 0.0);
    let n_dot_v = max(dot(n, v), 0.001);
    let visibility = shadow_visibility(in.world_pos, n, n_dot_l, cascade, in.clip.xy);
    if mode == 7 { return vec4<f32>(vec3<f32>(visibility), 1.0); }
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
    var direct = (diffuse + specular) * globals.sun.rgb * globals.sun.w * n_dot_l * visibility;
    let moon_l = normalize(-globals.moon_dir.xyz);
    let moon_ndl = max(dot(n, moon_l), 0.0);
    let moon_direct = base * globals.moon.rgb * globals.moon.w * moon_ndl / PI;
    var ambient = base * skylight.radiance * mix(0.35, 1.0, clamp(in.ao, 0.0, 1.0));
    // Under water the light that reached this surface lost its reds on the way
    // down, and the sun's share is broken into drifting caustic veins.
    let probe_xz = in.world_pos.xz + n.xz * 0.06;
    let wet_bounds = water_bounds_at(probe_xz);
    let wet_h = wet_bounds.x;
    var sub_light = vec3<f32>(1.0);
    if in.world_pos.y >= wet_bounds.y && in.world_pos.y < wet_h {
        let depth = wet_h - in.world_pos.y;
        let sun_path = depth / max(l.y, 0.3);
        direct = direct * exp(-WATER_ABSORB * sun_path) * water_caustics(in.world_pos, depth, n);
        ambient = ambient * exp(-WATER_ABSORB * 0.5 * depth);
        sub_light = exp(-WATER_ABSORB * 0.5 * depth);
    }
    let emission = mat.base_color.rgb * max(mat.params.z, 0.0);
    // Bounce is reflected radiance: the receiver's albedo times the average
    // source radiance around it (furnace-calibrated, see bounce.wgsl).
    let bounced = base * skylight.bounce;
    // Real-time point lights (torches): inverse-square-ish falloff that reaches
    // exactly zero at the light's range, cosine-weighted, no shadows.
    var point = vec3<f32>(0.0);
    let point_count = i32(globals.point_count.x);
    for (var i = 0; i < point_count; i = i + 1) {
        let light = globals.point_lights[i];
        let to = light.xyz - in.world_pos;
        let d2 = max(dot(to, to), 0.0004);
        let falloff = clamp(1.0 - d2 / (light.w * light.w), 0.0, 1.0);
        let ndl = max(dot(n, to * inverseSqrt(d2)), 0.0);
        point += globals.point_colors[i].rgb * ndl * falloff * falloff / (1.0 + d2);
    }
    let torch_light = base * point / PI;
    let lit = ambient + direct + moon_direct + (indirect + bounced) * sub_light + emission + torch_light;
    return vec4<f32>(apply_water_view(lit, in.world_pos, probe_xz), mat.base_color.a);
}

// Water surface sheet: the smoothed heightfield mesh, flat-shaded per triangle.
// From above it is a faint sky mirror with sun glints; from below, a Snell's
// window onto the sky ringed by a mirror of the underwater scene.
@fragment fn fs_water(in: VsOut) -> @location(0) vec4<f32> {
    let cam = globals.camera_pos.xyz;
    let to_cam = cam - in.world_pos;
    let dist = max(length(to_cam), 0.0001);
    let v = to_cam / dist;
    var n = normalize(in.normal);
    if dot(n, v) < 0.0 { n = -n; }
    let cos_t = clamp(dot(n, v), 0.0, 1.0);
    let l = normalize(-globals.sun_dir.xyz);
    let below = cam.y < in.world_pos.y;
    // A faint voxel grain so still water never reads as a sheet of glass: each
    // quarter-metre cell is a touch lighter or darker, with a hairline between
    // cells. World-aligned, so it matches the terrain's voxels and holds still.
    let grid = in.world_pos.xz / WATER_VOXEL_M;
    let rnd = hash_cell(vec3<f32>(floor(grid), 5.0)).x * 2.0 - 1.0;
    let g = fract(grid);
    let edge = min(min(g.x, g.y), min(1.0 - g.x, 1.0 - g.y));
    let seam = 1.0 - smoothstep(0.0, 0.07, edge);
    let voxel = 1.0 + WATER_VOXEL_HINT * (rnd - 0.8 * seam);
    if below {
        let window = smoothstep(0.60, 0.72, cos_t);
        let depth = max(in.world_pos.y - cam.y, 0.0);
        // Outside the window: total internal reflection shows the water itself.
        let mirror = water_scatter(depth);
        let sky = hemisphere_radiance(1.0) * vec3<f32>(0.55, 0.85, 1.0);
        var color = mix(mirror, sky, window) * voxel;
        color = water_fog(color, cam, in.world_pos, in.world_pos.y);
        return vec4<f32>(color, clamp(mix(0.92, 0.18, window) * voxel, 0.0, 1.0));
    }
    let fresnel = 0.02 + 0.98 * pow(1.0 - cos_t, 5.0);
    let r = reflect(-v, n);
    let sky = hemisphere_radiance(max(r.y, 0.0));
    let glint = pow(max(dot(r, l), 0.0), 220.0) * globals.sun.w * 0.5;
    let tint = vec3<f32>(0.03, 0.17, 0.22) * (0.5 * dot(globals.sky_color.rgb, vec3<f32>(0.3, 0.59, 0.11)) + 0.1);
    let color = (sky * fresnel + globals.sun.rgb * glint + tint * (1.0 - fresnel)) * voxel;
    return vec4<f32>(color, clamp((0.16 + fresnel * 0.8 + glint) * voxel, 0.0, 1.0));
}
