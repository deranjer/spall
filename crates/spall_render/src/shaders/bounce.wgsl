// One diffuse bounce over the camera-local occupancy cache, with *lit* sources.
//
// For every air cell near a surface, 66 rays (the fixed set in `ray_direction`,
// rotated per cell, see `cell_jitter`) are walked through the occupancy. The first
// occupied cell they enter is a bounce source; its outgoing radiance is what
// that surface is actually emitting or reflecting right now:
//
//   L = albedo * (sun irradiance * N.L * sun visibility   -- shadow ray in the cache
//                 + sky visibility of that face * sky radiance) -- the sky-visibility volume
//     + albedo * emissive                                   -- material emission
//
// No fixed brightness stands in for any of it. Rays that leave the cache or
// reach the sky contribute nothing: skylight is applied by the opaque pass from
// sky visibility, so counting it here too would count it twice. Solid and
// unknown cells are opaque; an unknown cell emits nothing.
//
// Emission far from the receiver is not left to the rays: a lamp is too small a
// target, so distant emitters are lit analytically (next-event estimation, see
// the NEE notes below and `emitters.wgsl`).
//
// Each cell stores six face radiances (a directional ambient cube), the
// cosine-weighted average over the rays in that face's hemisphere with misses
// counting as zero. For a surface that sees uniform radiance L over its whole
// hemisphere the reflected light is `albedo * L` -- the furnace case. Bleed is
// soft and blocky (0.5 m cells, a few dozen rays); documented in
// docs/reports/ENG-97.md. Derived, client-local lighting only.

const PI: f32 = 3.14159265359;

struct Material { base_color: vec4<f32>, params: vec4<f32>, };
struct BounceGlobals {
    origin_cell_size: vec4<f32>,
    // x = cells per axis, y = max ray length in cells.
    dimensions: vec4<u32>,
    // Direction the light travels (from the sun toward the scene), xyz.
    sun_dir: vec4<f32>,
    // Sun colour * intensity.
    sun_radiance: vec4<f32>,
    sky_color: vec4<f32>,
    ground_color: vec4<f32>,
    // x, y = the half-open z range of cells this dispatch may write.
    region: vec4<u32>,
};

@group(0) @binding(0) var<storage, read> cells: array<u32>;
@group(0) @binding(1) var<storage, read> visibility: array<vec2<u32>>;
@group(0) @binding(2) var<storage, read> materials: array<Material>;
@group(0) @binding(3) var<storage, read_write> radiance: array<u32>;
@group(0) @binding(4) var<uniform> globals: BounceGlobals;
// Emissive bins gathered by emitters.wgsl: a header (x = how many there are,
// which can exceed MAX_EMITTERS, meaning the list overflowed) and 4 vec4 per
// emitter: (centroid xyz in cells, exposed faces +x), (radiance rgb, exposed
// faces -x), (exposed faces +y -y +z -z).
struct EmitterBlock {
    header: vec4<u32>,
    items: array<vec4<f32>, MAX_EMITTERS * 3u>,
};
@group(0) @binding(5) var<uniform> emitters: EmitterBlock;

// Emission is lit two ways. Close to a source, rays see it often enough: a lamp
// 1 m from a wall fills a good part of the hemisphere. Far from it, a lamp is a
// small target and rays hit it by luck, which speckles distant walls, so those
// receivers are lit analytically from the gathered emitters (one shadow ray
// each) and rays stop counting far emission. The hand-off is a smooth ramp over
// [NEE_NEAR_CELLS, NEE_FAR_CELLS] so no ring shows where one method takes over:
// a ray's emission is weighted by `1 - w` at the distance it hit, the analytic
// term by `w` at the emitter's centroid distance.
const NEE_NEAR_CELLS: f32 = 3.0;
const NEE_FAR_CELLS: f32 = 6.0;

fn nee_weight(distance_cells: f32) -> f32 {
    return smoothstep(NEE_NEAR_CELLS, NEE_FAR_CELLS, distance_cells);
}

// Whether the emitter list is complete. On overflow every emitter is sampled by
// rays as before, so a lamp is never silently dropped.
fn nee_active() -> bool {
    return emitters.header.x <= MAX_EMITTERS;
}

const RAY_COUNT: u32 = 66u;

// 66 fixed rays: the six axes, the twelve edge diagonals, and two rings of 24
// shallow rays (an axis tilted 71 and 82 degrees, i.e. 19 and 8.5 degrees off the
// perpendicular plane) so light arriving at a low angle -- a lamp several
// metres away on the same floor -- still has rays that can reach it. A
// cosine-weighted lobe puts real weight on those low angles; 14 rays (axes +
// cube corners) missed them entirely, 42 still missed anything below 19 degrees.
fn ray_direction(i: u32) -> vec3<f32> {
    let dirs = array<vec3<f32>, 66>(
        vec3<f32>(1.00000000, 0.00000000, 0.00000000),
        vec3<f32>(-1.00000000, 0.00000000, 0.00000000),
        vec3<f32>(0.00000000, 1.00000000, 0.00000000),
        vec3<f32>(0.00000000, -1.00000000, 0.00000000),
        vec3<f32>(0.00000000, 0.00000000, 1.00000000),
        vec3<f32>(0.00000000, 0.00000000, -1.00000000),
        vec3<f32>(0.70710678, 0.70710678, 0.00000000),
        vec3<f32>(0.70710678, -0.70710678, 0.00000000),
        vec3<f32>(-0.70710678, 0.70710678, 0.00000000),
        vec3<f32>(-0.70710678, -0.70710678, 0.00000000),
        vec3<f32>(0.70710678, 0.00000000, 0.70710678),
        vec3<f32>(0.70710678, 0.00000000, -0.70710678),
        vec3<f32>(-0.70710678, 0.00000000, 0.70710678),
        vec3<f32>(-0.70710678, 0.00000000, -0.70710678),
        vec3<f32>(0.00000000, 0.70710678, 0.70710678),
        vec3<f32>(0.00000000, 0.70710678, -0.70710678),
        vec3<f32>(0.00000000, -0.70710678, 0.70710678),
        vec3<f32>(0.00000000, -0.70710678, -0.70710678),
        vec3<f32>(0.33035042, 0.94385836, 0.00000000),
        vec3<f32>(0.33035042, -0.94385836, 0.00000000),
        vec3<f32>(0.33035042, 0.00000000, 0.94385836),
        vec3<f32>(0.33035042, 0.00000000, -0.94385836),
        vec3<f32>(-0.33035042, 0.94385836, 0.00000000),
        vec3<f32>(-0.33035042, -0.94385836, 0.00000000),
        vec3<f32>(-0.33035042, 0.00000000, 0.94385836),
        vec3<f32>(-0.33035042, 0.00000000, -0.94385836),
        vec3<f32>(0.94385836, 0.33035042, 0.00000000),
        vec3<f32>(-0.94385836, 0.33035042, 0.00000000),
        vec3<f32>(0.00000000, 0.33035042, 0.94385836),
        vec3<f32>(0.00000000, 0.33035042, -0.94385836),
        vec3<f32>(0.94385836, -0.33035042, 0.00000000),
        vec3<f32>(-0.94385836, -0.33035042, 0.00000000),
        vec3<f32>(0.00000000, -0.33035042, 0.94385836),
        vec3<f32>(0.00000000, -0.33035042, -0.94385836),
        vec3<f32>(0.94385836, 0.00000000, 0.33035042),
        vec3<f32>(-0.94385836, 0.00000000, 0.33035042),
        vec3<f32>(0.00000000, 0.94385836, 0.33035042),
        vec3<f32>(0.00000000, -0.94385836, 0.33035042),
        vec3<f32>(0.94385836, 0.00000000, -0.33035042),
        vec3<f32>(-0.94385836, 0.00000000, -0.33035042),
        vec3<f32>(0.00000000, 0.94385836, -0.33035042),
        vec3<f32>(0.00000000, -0.94385836, -0.33035042),
        vec3<f32>(0.14834045, 0.98893635, 0.00000000),
        vec3<f32>(0.14834045, -0.98893635, 0.00000000),
        vec3<f32>(0.14834045, 0.00000000, 0.98893635),
        vec3<f32>(0.14834045, 0.00000000, -0.98893635),
        vec3<f32>(-0.14834045, 0.98893635, 0.00000000),
        vec3<f32>(-0.14834045, -0.98893635, 0.00000000),
        vec3<f32>(-0.14834045, 0.00000000, 0.98893635),
        vec3<f32>(-0.14834045, 0.00000000, -0.98893635),
        vec3<f32>(0.98893635, 0.14834045, 0.00000000),
        vec3<f32>(-0.98893635, 0.14834045, 0.00000000),
        vec3<f32>(0.00000000, 0.14834045, 0.98893635),
        vec3<f32>(0.00000000, 0.14834045, -0.98893635),
        vec3<f32>(0.98893635, -0.14834045, 0.00000000),
        vec3<f32>(-0.98893635, -0.14834045, 0.00000000),
        vec3<f32>(0.00000000, -0.14834045, 0.98893635),
        vec3<f32>(0.00000000, -0.14834045, -0.98893635),
        vec3<f32>(0.98893635, 0.00000000, 0.14834045),
        vec3<f32>(-0.98893635, 0.00000000, 0.14834045),
        vec3<f32>(0.00000000, 0.98893635, 0.14834045),
        vec3<f32>(0.00000000, -0.98893635, 0.14834045),
        vec3<f32>(0.98893635, 0.00000000, -0.14834045),
        vec3<f32>(-0.98893635, 0.00000000, -0.14834045),
        vec3<f32>(0.00000000, 0.98893635, -0.14834045),
        vec3<f32>(0.00000000, -0.98893635, -0.14834045)
    );
    return dirs[i];
}

// A different small random rotation of the whole ray set for every cell. With
// one shared set, neighbouring cells sample the same ground spots along the
// same lines, which reads as fans of streaks and 1 m blobs on a surface built
// from 0.25 m voxels. Rotating per cell turns that structure into fine noise
// that the receiver's trilinear filter over eight cells then averages. The
// rotation is a pure function of the cell's *world* position (the cache origin
// is snapped to the cell grid), so a recompute is deterministic and re-centring
// the cache as the camera moves does not re-roll the noise: the same wall keeps
// the same rays, so lighting does not pop as you walk toward it.
const MAX_JITTER_RAD: f32 = 0.45;

fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

struct Jitter { axis: vec3<f32>, cos_a: f32, sin_a: f32, };

fn cell_jitter(cell: vec3<i32>) -> Jitter {
    let h0 = pcg(u32(cell.x) ^ pcg(u32(cell.y) ^ pcg(u32(cell.z))));
    let h1 = pcg(h0);
    let h2 = pcg(h1);
    let u0 = f32(h0) / 4294967295.0;
    let u1 = f32(h1) / 4294967295.0;
    let u2 = f32(h2) / 4294967295.0;
    let z = 2.0 * u0 - 1.0;
    let phi = 2.0 * PI * u1;
    let r = sqrt(max(1.0 - z * z, 0.0));
    let angle = (2.0 * u2 - 1.0) * MAX_JITTER_RAD;
    var j: Jitter;
    j.axis = vec3<f32>(r * cos(phi), r * sin(phi), z);
    j.cos_a = cos(angle);
    j.sin_a = sin(angle);
    return j;
}

fn jitter_dir(j: Jitter, v: vec3<f32>) -> vec3<f32> {
    return v * j.cos_a + cross(j.axis, v) * j.sin_a + j.axis * dot(j.axis, v) * (1.0 - j.cos_a);
}

fn face_of_normal(n: vec3<f32>) -> u32 {
    if n.x > 0.5 { return 0u; }
    if n.x < -0.5 { return 1u; }
    if n.y > 0.5 { return 2u; }
    if n.y < -0.5 { return 3u; }
    if n.z > 0.5 { return 4u; }
    return 5u;
}

fn face_value(packed: vec2<u32>, face: u32) -> f32 {
    let word = select(packed.x, packed.y, face >= 4u);
    return f32((word >> ((face & 3u) * 8u)) & 255u) / 255.0;
}

fn hemisphere_radiance(dy: f32) -> vec3<f32> {
    return mix(globals.ground_color.rgb, globals.sky_color.rgb, dy * 0.5 + 0.5);
}

// Outgoing radiance of the surface a ray hit.
fn source_radiance(hit: Hit, dim: i32, max_cells: f32, origin: vec3<f32>) -> vec3<f32> {
    if hit.value >= arrayLength(&materials) { return vec3<f32>(0.0); }
    let material = materials[hit.value];
    let base = material.base_color.rgb;
    var emitted = base * max(material.params.z, 0.0);
    if nee_active() {
        let hit_cells = length(vec3<f32>(hit.front) + vec3<f32>(0.5) - origin);
        emitted *= 1.0 - nee_weight(hit_cells);
    }
    var out = emitted;

    let sky_seen = face_value(visibility[index_of(hit.front, dim)], face_of_normal(hit.normal));
    // Same convention as the opaque pass: a vertical face's unblocked rays see
    // sky, not the sky/ground average.
    let sky_dy = select(hit.normal.y, 0.5, abs(hit.normal.y) < 0.5);
    out += base * sky_seen * hemisphere_radiance(sky_dy);

    let to_sun = -globals.sun_dir.xyz;
    let n_dot_l = dot(hit.normal, to_sun);
    if n_dot_l > 0.0 {
        let start = vec3<f32>(hit.front) + vec3<f32>(0.5);
        let lit = ray_reaches_sky(start, to_sun, dim, max_cells);
        out += base / PI * globals.sun_radiance.rgb * n_dot_l * lit;
    }
    return out;
}

// Shared-exponent RGB (9 bits per channel + 5-bit exponent) in one u32.
fn encode_rgb9e5(c: vec3<f32>) -> u32 {
    let clamped = clamp(c, vec3<f32>(0.0), vec3<f32>(65408.0));
    let peak = max(clamped.r, max(clamped.g, clamped.b));
    var shared_exp = max(-16, i32(floor(log2(max(peak, 1.0e-20))))) + 16;
    var denom = exp2(f32(shared_exp) - 24.0);
    if floor(peak / denom + 0.5) >= 512.0 {
        shared_exp += 1;
        denom *= 2.0;
    }
    let m = vec3<u32>(floor(clamped / denom + vec3<f32>(0.5)));
    return (u32(shared_exp) << 27u) | (m.b << 18u) | (m.g << 9u) | m.r;
}

fn face_axis(face: u32) -> vec3<f32> {
    let s = select(-1.0, 1.0, (face & 1u) == 0u);
    let a = face >> 1u;
    if a == 0u { return vec3<f32>(s, 0.0, 0.0); }
    if a == 1u { return vec3<f32>(0.0, s, 0.0); }
    return vec3<f32>(0.0, 0.0, s);
}

// Each cell stores, for each of the six axis faces, the cosine-weighted average
// source radiance over the rays in that face's hemisphere (rays that reach the
// sky or leave the cache count as zero). A receiver reads the faces its normal
// points to, so it never sees light "from behind" itself: a floor is not lit by
// itself, and open ground under a bright sky has nothing to bounce.
@compute @workgroup_size(4, 4, 4)
fn bounce_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dim = i32(globals.dimensions.x);
    let cell = vec3<i32>(gid);
    if !inside(cell, dim) { return; }
    if gid.z < globals.region.x || gid.z >= globals.region.y { return; }
    let index = index_of(cell, dim);
    // Solid cells and air far from any surface are never sampled.
    if cells[index] != 0u || !near_solid(cell, dim) {
        for (var f = 0u; f < 6u; f += 1u) { radiance[index * 6u + f] = 0u; }
        return;
    }
    let max_cells = f32(globals.dimensions.y);
    let origin = vec3<f32>(cell) + vec3<f32>(0.5);
    var sum: array<vec3<f32>, 6>;
    var weight_sum: array<f32, 6>;
    for (var f = 0u; f < 6u; f += 1u) {
        sum[f] = vec3<f32>(0.0);
        weight_sum[f] = 0.0;
    }
    let world_cell = cell + vec3<i32>(round(globals.origin_cell_size.xyz / globals.origin_cell_size.w));
    let jitter = cell_jitter(world_cell);
    for (var i = 0u; i < RAY_COUNT; i += 1u) {
        let dir = jitter_dir(jitter, ray_direction(i));
        var source = vec3<f32>(0.0);
        let hit = trace_first_hit(origin, dir, dim, max_cells);
        if hit.hit { source = source_radiance(hit, dim, max_cells, origin); }
        for (var f = 0u; f < 6u; f += 1u) {
            let w = max(dot(dir, face_axis(f)), 0.0);
            sum[f] += source * w;
            weight_sum[f] += w;
        }
    }
    // Far emitters, analytically: irradiance E = L * A_proj * cos(receiver) / d^2
    // per face, and the cache stores E / pi (see the furnace note above).
    var direct: array<vec3<f32>, 6>;
    for (var f = 0u; f < 6u; f += 1u) { direct[f] = vec3<f32>(0.0); }
    if nee_active() {
        let cell_m = globals.origin_cell_size.w;
        for (var e = 0u; e < emitters.header.x; e += 1u) {
            let position = emitters.items[e * 3u].xyz;
            let to_emitter = position - origin;
            let distance_cells = length(to_emitter);
            let weight = nee_weight(distance_cells);
            if weight <= 0.0 || distance_cells > max_cells { continue; }
            let dir = to_emitter / distance_cells;
            // Shadow ray to the emitter: it must reach emissive material near
            // the centroid before anything else.
            let first = trace_first_hit(origin, dir, dim, distance_cells + 2.0);
            if !first.hit || first.value >= arrayLength(&materials) { continue; }
            if materials[first.value].params.z <= 0.0 { continue; }
            if length(vec3<f32>(first.front) + vec3<f32>(0.5) - position) > 5.0 { continue; }
            // Faces the receiver can see: a face with outward normal n shows
            // when n points back toward the receiver.
            let back = -dir;
            let record_a = emitters.items[e * 3u];
            let record_b = emitters.items[e * 3u + 1u];
            let faces_yz = emitters.items[e * 3u + 2u];
            let area_cells = record_a.w * max(back.x, 0.0) + record_b.w * max(-back.x, 0.0)
                + faces_yz.x * max(back.y, 0.0) + faces_yz.y * max(-back.y, 0.0)
                + faces_yz.z * max(back.z, 0.0) + faces_yz.w * max(-back.z, 0.0);
            if area_cells <= 0.0 { continue; }
            let radiance_e = record_b.xyz;
            let distance_m = distance_cells * cell_m;
            let scale = weight * area_cells * cell_m * cell_m / (distance_m * distance_m * PI);
            for (var f = 0u; f < 6u; f += 1u) {
                direct[f] += radiance_e * scale * max(dot(dir, face_axis(f)), 0.0);
            }
        }
    }
    for (var f = 0u; f < 6u; f += 1u) {
        radiance[index * 6u + f] = encode_rgb9e5(sum[f] / max(weight_sum[f], 1.0e-4) + direct[f]);
    }
}
