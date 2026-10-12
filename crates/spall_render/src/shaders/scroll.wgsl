// Scrolls the bounce radiance when the light cache re-centres: every cell of the
// new cache that was also inside the old one takes the old cell's six face
// radiances, so only the strip that entered the cache has to be lit again.
//
// New cell `c` is old cell `c + shift` (the cache origin moved by `shift` cells).

struct ScrollParams {
    // xyz = how far the origin moved, in cells; w unused.
    shift: vec4<i32>,
    // x = cells per axis.
    dimensions: vec4<u32>,
};

@group(0) @binding(0) var<storage, read> source: array<u32>;
@group(0) @binding(1) var<storage, read_write> destination: array<u32>;
@group(0) @binding(2) var<uniform> params: ScrollParams;

fn cell_index(cell: vec3<i32>, dim: i32) -> u32 {
    return u32(cell.x + dim * (cell.y + dim * cell.z));
}

@compute @workgroup_size(4, 4, 4)
fn scroll_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dim = i32(params.dimensions.x);
    let cell = vec3<i32>(gid);
    if cell.x >= dim || cell.y >= dim || cell.z >= dim { return; }
    let old_cell = cell + params.shift.xyz;
    let to = cell_index(cell, dim) * 6u;
    let inside = all(old_cell >= vec3<i32>(0)) && all(old_cell < vec3<i32>(dim));
    if inside {
        let source_at = cell_index(old_cell, dim) * 6u;
        for (var face = 0u; face < 6u; face += 1u) {
            destination[to + face] = source[source_at + face];
        }
    } else {
        for (var face = 0u; face < 6u; face += 1u) {
            destination[to + face] = 0u;
        }
    }
}
