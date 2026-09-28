// Sky visibility: for every air cell near a surface of the camera-local
// occupancy cache, how much of the sky each of the six axis directions sees.
//
// Each face direction is a cosine-weighted cone of five rays (the axis plus four
// rays tilted 45 degrees) walked through the occupancy with an exact 3D DDA. A
// ray that travels `max_ray_cells` without hitting anything, or leaves through
// the top of the cache, has reached the sky; a ray that hits a solid or unknown
// cell, or leaves through a side or the bottom (unknown territory), has not.
// Occupied cells are 0 = air, anything else = solid or unknown.
//
// The result is derived, client-local lighting data: it never affects
// collision, topology, replication or saved geometry.

struct SkyGlobals {
    origin_cell_size: vec4<f32>,
    // x = cells per axis, y = enabled (1) or a disabled stub (0), z = max ray
    // length in cells.
    dimensions: vec4<u32>,
    // x, y = the half-open z range of cells this dispatch may write; the rest
    // keep their previous values (time-sliced and dirty-region updates).
    region: vec4<u32>,
};

@group(0) @binding(0) var<storage, read> cells: array<u32>;
@group(0) @binding(1) var<storage, read_write> visibility: array<vec2<u32>>;
@group(0) @binding(2) var<uniform> globals: SkyGlobals;

struct FaceBasis { axis: vec3<f32>, u: vec3<f32>, v: vec3<f32>, };

fn face_basis(face: u32) -> FaceBasis {
    var basis: FaceBasis;
    let sign = select(-1.0, 1.0, (face & 1u) == 0u);
    let a = face >> 1u;
    if a == 0u {
        basis.axis = vec3<f32>(sign, 0.0, 0.0);
        basis.u = vec3<f32>(0.0, 1.0, 0.0);
        basis.v = vec3<f32>(0.0, 0.0, 1.0);
    } else if a == 1u {
        basis.axis = vec3<f32>(0.0, sign, 0.0);
        basis.u = vec3<f32>(0.0, 0.0, 1.0);
        basis.v = vec3<f32>(1.0, 0.0, 0.0);
    } else {
        basis.axis = vec3<f32>(0.0, 0.0, sign);
        basis.u = vec3<f32>(1.0, 0.0, 0.0);
        basis.v = vec3<f32>(0.0, 1.0, 0.0);
    }
    return basis;
}

fn face_visibility(origin: vec3<f32>, face: u32, dim: i32, max_cells: f32) -> f32 {
    let basis = face_basis(face);
    let tilt = 0.70710678;
    // Cosine weights: the axis ray counts 1, each 45-degree ray cos(45).
    let w_axis = 1.0;
    let w_tilt = 0.70710678;
    var sum = w_axis * ray_reaches_sky(origin, basis.axis, dim, max_cells);
    sum += w_tilt * ray_reaches_sky(origin, normalize(basis.axis + basis.u), dim, max_cells);
    sum += w_tilt * ray_reaches_sky(origin, normalize(basis.axis - basis.u), dim, max_cells);
    sum += w_tilt * ray_reaches_sky(origin, normalize(basis.axis + basis.v), dim, max_cells);
    sum += w_tilt * ray_reaches_sky(origin, normalize(basis.axis - basis.v), dim, max_cells);
    return sum / (w_axis + 4.0 * w_tilt);
}

fn pack4(a: f32, b: f32, c: f32, d: f32) -> u32 {
    let q = vec4<u32>(clamp(vec4<f32>(a, b, c, d), vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0 + vec4<f32>(0.5));
    return q.x | (q.y << 8u) | (q.z << 16u) | (q.w << 24u);
}

@compute @workgroup_size(4, 4, 4)
fn sky_visibility_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dim = i32(globals.dimensions.x);
    let cell = vec3<i32>(gid);
    if !inside(cell, dim) { return; }
    if gid.z < globals.region.x || gid.z >= globals.region.y { return; }
    let index = index_of(cell, dim);
    if cells[index] != 0u {
        // Inside geometry: nothing sees the sky from here.
        visibility[index] = vec2<u32>(0u, 0u);
        return;
    }
    if !near_solid(cell, dim) {
        // Open air away from every surface; never read by shading.
        visibility[index] = vec2<u32>(0xFFFFFFFFu, 0xFFFFFFFFu);
        return;
    }
    let origin = vec3<f32>(cell) + vec3<f32>(0.5);
    let max_cells = f32(globals.dimensions.z);
    let px = face_visibility(origin, 0u, dim, max_cells);
    let nx = face_visibility(origin, 1u, dim, max_cells);
    let py = face_visibility(origin, 2u, dim, max_cells);
    let ny = face_visibility(origin, 3u, dim, max_cells);
    let pz = face_visibility(origin, 4u, dim, max_cells);
    let nz = face_visibility(origin, 5u, dim, max_cells);
    // Byte 2 of the second word is the air marker the opaque pass reads in
    // place of the occupancy buffer (solid and unknown cells store zero).
    visibility[index] = vec2<u32>(pack4(px, nx, py, ny), pack4(pz, nz, 1.0, 0.0));
}
