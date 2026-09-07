// Opaque voxel-surface shading for the T05 baseline.
//
// debug_mode selects the output: 0 = shaded, 1 = world normal, 2 = linear depth.

struct Globals {
    view_proj: mat4x4<f32>,
    camera_pos: vec4<f32>,   // xyz, w unused
    sun_dir: vec4<f32>,      // normalised direction the sunlight travels, xyz
    params: vec4<f32>,       // x = debug_mode, y = z_near, z = z_far, w unused
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> palette: array<vec4<f32>>;

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

@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.clip = globals.view_proj * vec4<f32>(in.position, 1.0);
    out.world_pos = in.position;
    out.normal = in.normal;
    out.local_uv = in.local_uv;
    out.ao = in.ao;
    out.material = in.material;
    return out;
}

fn material_color(id: u32) -> vec3<f32> {
    if id < arrayLength(&palette) {
        return palette[id].rgb;
    }
    return vec3<f32>(0.8, 0.1, 0.8); // missing-material magenta
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let n = normalize(in.normal);
    let mode = i32(round(globals.params.x));

    if mode == 1 {
        return vec4<f32>(n * 0.5 + vec3<f32>(0.5), 1.0);
    }

    if mode == 2 {
        let z_near = globals.params.y;
        let z_far = globals.params.z;
        let view_z = length(in.world_pos - globals.camera_pos.xyz);
        let g = clamp((view_z - z_near) / (z_far - z_near), 0.0, 1.0);
        return vec4<f32>(vec3<f32>(1.0 - g), 1.0);
    }

    // Shaded: sun lambert + sky ambient + baked AO, then a gentle Reinhard.
    let l = normalize(-globals.sun_dir.xyz);
    let ndl = max(dot(n, l), 0.0);
    let sky = 0.18 + 0.12 * (n.y * 0.5 + 0.5);
    var base = material_color(in.material);

    // Faint procedural grid so large merged quads read as many cells.
    let grid = fract(in.local_uv);
    let line = min(min(grid.x, grid.y), min(1.0 - grid.x, 1.0 - grid.y));
    let detail = 0.9 + 0.1 * smoothstep(0.0, 0.06, line);
    base = base * detail;

    var color = base * (sky + ndl * 1.15) * in.ao;
    color = color / (vec3<f32>(1.0) + color);
    return vec4<f32>(color, 1.0);
}
