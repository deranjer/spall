// Emitter gather for the bounce pass (next-event estimation of emissive light).
//
// A lamp is a tiny, extremely bright target. Hoping that a few of a cell's 66
// rays happen to hit it makes distant walls speckle: a cell whose ray lands on
// the lamp gets the full hit and its neighbours get nothing. Instead every
// emissive cell is collected here, pooled into 4x4x4-cell bins (2 m), and the
// bounce pass lights each receiver from every nearby bin analytically, with one
// shadow ray per emitter.
//
// Two dispatches, run before each bounce slice so they always match the
// occupancy and material table the slice reads:
//
//   gather_main   one thread per cell: an emissive cell adds itself to its bin
//                 (count, centroid, radiance, and how many of its faces are
//                 exposed to air in each of the six axis directions, which is
//                 what gives a bin its projected area seen from any direction).
//   compact_main  one thread per bin: a non-empty bin appends one record to the
//                 emitter list.
//
// `DIM` and `MAX_EMITTERS` are prepended by the host. The list holds
// `MAX_EMITTERS` records; the counter keeps counting past that, so the bounce
// pass can tell the list overflowed and fall back to sampling emission with
// rays (noisy, but nothing is silently dropped).

struct Material { base_color: vec4<f32>, params: vec4<f32>, };

@group(0) @binding(0) var<storage, read> cells: array<u32>;
@group(0) @binding(1) var<storage, read> materials: array<Material>;
// 16 u32 per bin, then the emitter counter in the word after the last bin.
// Per bin: [0] cells, [1..3] summed centre * 16, [4..6] summed emission
// * 256, [7..12] exposed faces (+x -x +y -y +z -z).
@group(0) @binding(2) var<storage, read_write> bins: array<atomic<u32>>;
// [0] header (x = emitter count, as u32 bits), then 3 vec4 per emitter:
//   (centroid xyz in cells, exposed faces +x)
//   (radiance rgb,          exposed faces -x)
//   (exposed faces +y -y +z -z)
// The host copies this into the uniform the bounce pass reads (the device
// allows only four storage buffers per stage and the bounce pass already uses
// four, and a uniform binding is capped at 16 KiB).
@group(0) @binding(3) var<storage, read_write> records: array<vec4<f32>>;

const BIN_CELLS: i32 = 4;
const BINS_PER_AXIS: i32 = DIM / BIN_CELLS;
const BIN_STRIDE: u32 = 16u;
const COUNTER: u32 = u32(BINS_PER_AXIS * BINS_PER_AXIS * BINS_PER_AXIS) * BIN_STRIDE;
const POS_SCALE: f32 = 16.0;
const RADIANCE_SCALE: f32 = 256.0;
// Keeps the fixed-point sums of a full bin (64 cells) inside a u32.
const MAX_EMISSION: f32 = 1000.0;

fn cell_index(cell: vec3<i32>) -> u32 {
    return u32(cell.x + DIM * (cell.y + DIM * cell.z));
}

fn in_cache(cell: vec3<i32>) -> bool {
    return all(cell >= vec3<i32>(0)) && all(cell < vec3<i32>(DIM));
}

fn axis_of_face(face: u32) -> vec3<i32> {
    let s = select(-1, 1, (face & 1u) == 0u);
    let a = face >> 1u;
    if a == 0u { return vec3<i32>(s, 0, 0); }
    if a == 1u { return vec3<i32>(0, s, 0); }
    return vec3<i32>(0, 0, s);
}

@compute @workgroup_size(4, 4, 4)
fn gather_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = vec3<i32>(gid);
    if !in_cache(cell) { return; }
    let value = cells[cell_index(cell)];
    if value == 0u || value >= arrayLength(&materials) { return; }
    let material = materials[value];
    let emission = min(material.base_color.rgb * max(material.params.z, 0.0), vec3<f32>(MAX_EMISSION));
    if max(emission.r, max(emission.g, emission.b)) <= 0.0 { return; }

    let bin = cell / BIN_CELLS;
    let base = u32(bin.x + BINS_PER_AXIS * (bin.y + BINS_PER_AXIS * bin.z)) * BIN_STRIDE;
    atomicAdd(&bins[base], 1u);
    let centre = vec3<u32>((vec3<f32>(cell) + vec3<f32>(0.5)) * POS_SCALE);
    atomicAdd(&bins[base + 1u], centre.x);
    atomicAdd(&bins[base + 2u], centre.y);
    atomicAdd(&bins[base + 3u], centre.z);
    let e = vec3<u32>(emission * RADIANCE_SCALE);
    atomicAdd(&bins[base + 4u], e.x);
    atomicAdd(&bins[base + 5u], e.y);
    atomicAdd(&bins[base + 6u], e.z);
    for (var f = 0u; f < 6u; f += 1u) {
        let neighbour = cell + axis_of_face(f);
        // Only air exposes a face: solid, unknown and out-of-cache neighbours
        // do not, so a buried or edge face never radiates.
        if in_cache(neighbour) && cells[cell_index(neighbour)] == 0u {
            atomicAdd(&bins[base + 7u + f], 1u);
        }
    }
}

@compute @workgroup_size(64)
fn compact_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bin_count = u32(BINS_PER_AXIS * BINS_PER_AXIS * BINS_PER_AXIS);
    if gid.x >= bin_count { return; }
    let base = gid.x * BIN_STRIDE;
    let k = atomicLoad(&bins[base]);
    if k == 0u { return; }
    let slot = atomicAdd(&bins[COUNTER], 1u);
    if slot >= MAX_EMITTERS { return; }
    let kf = f32(k);
    let position = vec3<f32>(
        f32(atomicLoad(&bins[base + 1u])),
        f32(atomicLoad(&bins[base + 2u])),
        f32(atomicLoad(&bins[base + 3u])),
    ) / (POS_SCALE * kf);
    let radiance = vec3<f32>(
        f32(atomicLoad(&bins[base + 4u])),
        f32(atomicLoad(&bins[base + 5u])),
        f32(atomicLoad(&bins[base + 6u])),
    ) / (RADIANCE_SCALE * kf);
    var faces: array<f32, 6>;
    for (var f = 0u; f < 6u; f += 1u) { faces[f] = f32(atomicLoad(&bins[base + 7u + f])); }
    let out = 1u + slot * 3u;
    records[out] = vec4<f32>(position, faces[0]);
    records[out + 1u] = vec4<f32>(radiance, faces[1]);
    records[out + 2u] = vec4<f32>(faces[2], faces[3], faces[4], faces[5]);
}

// Runs after every bin has been compacted: publishes the count (which can
// exceed MAX_EMITTERS, meaning the list overflowed).
@compute @workgroup_size(1)
fn finish_main() {
    records[0] = vec4<f32>(bitcast<f32>(atomicLoad(&bins[COUNTER])), 0.0, 0.0, 0.0);
}
