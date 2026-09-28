// Shared voxel traversal over the camera-local occupancy cache. Concatenated in
// front of sky_visibility.wgsl and bounce.wgsl; each declares its own
// `@group(0) @binding(0) var<storage, read> cells: array<u32>` (0 = air, any
// other value = solid or unknown).

// Chebyshev radius (in cells) around a solid cell within which visibility is
// computed. The shading lookup interpolates the 8 cells around a point one cell
// in front of a surface, so everything it can read is within 2 cells of solid.
const NEAR_SOLID_RADIUS: i32 = 2;

fn index_of(cell: vec3<i32>, dim: i32) -> u32 {
    return u32(cell.x + dim * (cell.y + dim * cell.z));
}

fn inside(cell: vec3<i32>, dim: i32) -> bool {
    return all(cell >= vec3<i32>(0)) && all(cell < vec3<i32>(dim));
}

fn axis_t_max(origin: f32, cell: i32, d: f32) -> f32 {
    if d > 0.0 { return (f32(cell + 1) - origin) / d; }
    if d < 0.0 { return (f32(cell) - origin) / d; }
    return 1.0e30;
}

fn axis_t_delta(d: f32) -> f32 {
    if d != 0.0 { return 1.0 / abs(d); }
    return 1.0e30;
}

// 1 when the ray reaches the sky, 0 when it is blocked or leaves through
// unknown territory.
fn ray_reaches_sky(origin: vec3<f32>, dir: vec3<f32>, dim: i32, max_cells: f32) -> f32 {
    var cell = vec3<i32>(floor(origin));
    let step = vec3<i32>(sign(dir));
    let t_delta = vec3<f32>(axis_t_delta(dir.x), axis_t_delta(dir.y), axis_t_delta(dir.z));
    var t_max = vec3<f32>(
        axis_t_max(origin.x, cell.x, dir.x),
        axis_t_max(origin.y, cell.y, dir.y),
        axis_t_max(origin.z, cell.z, dir.z),
    );
    for (var i = 0; i < 256; i += 1) {
        var t = 0.0;
        var stepped_up = false;
        if t_max.x <= t_max.y && t_max.x <= t_max.z {
            t = t_max.x;
            cell.x += step.x;
            t_max.x += t_delta.x;
        } else if t_max.y <= t_max.z {
            t = t_max.y;
            cell.y += step.y;
            t_max.y += t_delta.y;
            stepped_up = step.y > 0;
        } else {
            t = t_max.z;
            cell.z += step.z;
            t_max.z += t_delta.z;
        }
        if t > max_cells { return 1.0; }
        if !inside(cell, dim) {
            // Out through the top is open sky; out through a side or the
            // bottom is beyond what the cache knows.
            if stepped_up && cell.y >= dim { return 1.0; }
            return 0.0;
        }
        if cells[index_of(cell, dim)] != 0u { return 0.0; }
    }
    return 1.0;
}


// A first-hit record from `trace_first_hit`.
struct Hit {
    hit: bool,
    // The air cell the ray was in when it entered the hit cell.
    front: vec3<i32>,
    // Outward normal of the face that was hit.
    normal: vec3<f32>,
    // The cell's occupancy value (a material id, or unknown).
    value: u32,
};

// First occupied cell along a ray, with the face it entered through. A ray
// that leaves the cache or travels `max_cells` reports no hit.
fn trace_first_hit(origin: vec3<f32>, dir: vec3<f32>, dim: i32, max_cells: f32) -> Hit {
    var result: Hit;
    result.hit = false;
    var cell = vec3<i32>(floor(origin));
    let step = vec3<i32>(sign(dir));
    let t_delta = vec3<f32>(axis_t_delta(dir.x), axis_t_delta(dir.y), axis_t_delta(dir.z));
    var t_max = vec3<f32>(
        axis_t_max(origin.x, cell.x, dir.x),
        axis_t_max(origin.y, cell.y, dir.y),
        axis_t_max(origin.z, cell.z, dir.z),
    );
    for (var i = 0; i < 256; i += 1) {
        var t = 0.0;
        let previous = cell;
        var normal = vec3<f32>(0.0);
        if t_max.x <= t_max.y && t_max.x <= t_max.z {
            t = t_max.x;
            cell.x += step.x;
            t_max.x += t_delta.x;
            normal = vec3<f32>(-f32(step.x), 0.0, 0.0);
        } else if t_max.y <= t_max.z {
            t = t_max.y;
            cell.y += step.y;
            t_max.y += t_delta.y;
            normal = vec3<f32>(0.0, -f32(step.y), 0.0);
        } else {
            t = t_max.z;
            cell.z += step.z;
            t_max.z += t_delta.z;
            normal = vec3<f32>(0.0, 0.0, -f32(step.z));
        }
        if t > max_cells { return result; }
        if !inside(cell, dim) { return result; }
        let value = cells[index_of(cell, dim)];
        if value != 0u {
            result.hit = true;
            result.front = previous;
            result.normal = normal;
            result.value = value;
            return result;
        }
    }
    return result;
}

fn near_solid(cell: vec3<i32>, dim: i32) -> bool {
    for (var z = -NEAR_SOLID_RADIUS; z <= NEAR_SOLID_RADIUS; z += 1) {
        for (var y = -NEAR_SOLID_RADIUS; y <= NEAR_SOLID_RADIUS; y += 1) {
            for (var x = -NEAR_SOLID_RADIUS; x <= NEAR_SOLID_RADIUS; x += 1) {
                let at = cell + vec3<i32>(x, y, z);
                if inside(at, dim) && cells[index_of(at, dim)] != 0u { return true; }
            }
        }
    }
    return false;
}

