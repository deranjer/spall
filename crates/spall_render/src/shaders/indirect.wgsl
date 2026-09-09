// T13 fixed 128^3 occupancy/material cache. The trace is deliberately small:
// twelve deterministic rays, first-hit material/emission, one diffuse bounce.

struct Material { base_color: vec4<f32>, params: vec4<f32>, };
struct IndirectGlobals {
    origin_cell_size: vec4<f32>,
    dimensions: vec4<u32>,
    sky: vec4<f32>,
};

@group(0) @binding(0) var<storage, read> cells: array<u32>;
@group(0) @binding(1) var<storage, read> materials: array<Material>;
@group(0) @binding(2) var<storage, read> source_radiance: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> target_radiance: array<vec4<f32>>;
@group(0) @binding(4) var<uniform> globals: IndirectGlobals;

fn index_of(cell: vec3<i32>, dim: i32) -> u32 {
    return u32(cell.x + dim * (cell.y + dim * cell.z));
}

fn inside(cell: vec3<i32>, dim: i32) -> bool {
    return all(cell >= vec3<i32>(0)) && all(cell < vec3<i32>(dim));
}

fn material_radiance(id: u32, upward: f32) -> vec3<f32> {
    if id >= arrayLength(&materials) { return vec3<f32>(0.0); }
    let material = materials[id];
    return material.base_color.rgb * (material.params.z + 0.12 * max(upward, 0.0));
}

fn direction(i: u32) -> vec3<f32> {
    let dirs = array<vec3<f32>, 12>(
        vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(-1.0, 0.0, 0.0),
        vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(0.0, -1.0, 0.0),
        vec3<f32>(0.0, 0.0, 1.0), vec3<f32>(0.0, 0.0, -1.0),
        vec3<f32>(0.70710677, 0.70710677, 0.0),
        vec3<f32>(-0.70710677, 0.70710677, 0.0),
        vec3<f32>(0.0, 0.70710677, 0.70710677),
        vec3<f32>(0.0, 0.70710677, -0.70710677),
        vec3<f32>(0.57735026, 0.57735026, 0.57735026),
        vec3<f32>(-0.57735026, 0.57735026, -0.57735026)
    );
    return dirs[i];
}

@compute @workgroup_size(4, 4, 4)
fn trace_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dim = i32(globals.dimensions.x);
    let cell = vec3<i32>(gid);
    if !inside(cell, dim) { return; }
    let output_index = index_of(cell, dim);
    let own_material = cells[output_index];
    if own_material != 0u {
        target_radiance[output_index] = vec4<f32>(material_radiance(own_material, 0.0), 1.0);
        return;
    }
    var sum = vec3<f32>(0.0);
    for (var ray = 0u; ray < 12u; ray += 1u) {
        let dir = direction(ray);
        var pos = vec3<f32>(cell) + vec3<f32>(0.5);
        var sample = globals.sky.rgb * (0.35 + 0.65 * max(dir.y, 0.0));
        for (var step = 0u; step < globals.dimensions.y; step += 1u) {
            pos += dir;
            let at = vec3<i32>(floor(pos));
            if !inside(at, dim) { break; }
            let hit = cells[index_of(at, dim)];
            if hit != 0u {
                sample = material_radiance(hit, dir.y);
                break;
            }
        }
        sum += sample;
    }
    target_radiance[output_index] = vec4<f32>(sum / 12.0, 1.0);
}

@compute @workgroup_size(4, 4, 4)
fn denoise_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dim = i32(globals.dimensions.x);
    let cell = vec3<i32>(gid);
    if !inside(cell, dim) { return; }
    let output_index = index_of(cell, dim);
    if cells[output_index] != 0u {
        target_radiance[output_index] = source_radiance[output_index];
        return;
    }
    let offsets = array<vec3<i32>, 7>(
        vec3<i32>(0, 0, 0), vec3<i32>(1, 0, 0), vec3<i32>(-1, 0, 0),
        vec3<i32>(0, 1, 0), vec3<i32>(0, -1, 0),
        vec3<i32>(0, 0, 1), vec3<i32>(0, 0, -1)
    );
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    for (var i = 0u; i < 7u; i += 1u) {
        let at = cell + offsets[i];
        if inside(at, dim) && cells[index_of(at, dim)] == 0u {
            sum += source_radiance[index_of(at, dim)].rgb;
            weight += 1.0;
        }
    }
    target_radiance[output_index] = vec4<f32>(sum / max(weight, 1.0), 1.0);
}
