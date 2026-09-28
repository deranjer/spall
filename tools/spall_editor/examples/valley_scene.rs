//! Playable valley showcase: a 56 m x 88 m river valley with a spring-fed
//! reservoir held by a stone dam and spillway, the river below it, an
//! east-bank tributary, a sandy pond draining through an outlet creek, a
//! timber bridge, a small village of the three lighting-room houses, lamp
//! posts along the paths, and a forest of every current tree asset.
//!
//! Water is authored with portable keys the sandbox understands: `water`
//! (initial water, always full), `water.source` (the tributary spring, always
//! running), `water.sink` (the drain at the end of the outlet creek),
//! `water.basin` (the reservoir's footprint, reserving its fluid domain
//! without seeding it — it starts dry) and `water.source.gated` /
//! `.gated.fast` / `.gated.max` (the reservoir's head spring at rate 1, 2, and
//! 3 — off until an admin selects a rate; a higher rate is a bigger authored
//! footprint, so it fills faster). `player.spawn` marks the start in the
//! village square. `dam.gate` marks a straight notch through the wall's
//! crest, closed (ordinary stone) until an admin opens it — the old
//! permanent spillway notch is sealed, so this is the reservoir's only
//! outlet, and it behaves the same way that one did: a shallow overflow, so
//! the lake spills into the river once nearly full rather than draining
//! through a hole partway down the wall. See
//! `spall_protocol::AdminCommand::{SetWaterSpring, SetDamGate}` and the
//! admin menu's "Water" section.
//!
//! The server solves the water on 0.5 m cells, so every surface water touches
//! is built on a 0.5 m grid (even voxel coordinates) and the whole water system
//! is kept inside one compact corridor (see `editor_scene`'s domain budget).
//!
//! Regenerate with `cargo run -p spall_editor --example valley_scene`.

use std::path::PathBuf;

use spall_editor::{
    AssetId, EditorCommand, EditorModel, SCENE_VOXELS_NAME, Transform, VoxelAssetFile, VoxelCoord,
};

// Material slots (project-local; the portable keys are what the game reads).
const GRASS: u16 = 1;
const DIRT: u16 = 2;
const STONE: u16 = 3;
const WOOD: u16 = 4;
const WATER: u16 = 5;
const SAND: u16 = 6;
const SLATE: u16 = 7;
const LOAM: u16 = 8;
const LAMP: u16 = 9;
const SPRING: u16 = 10;
const DRAIN: u16 = 11;
const SPAWN: u16 = 12;
const FOLIAGE: u16 = 13;
/// The reservoir's footprint: reserves fluid-domain space without seeding
/// water, so the lake starts dry.
const BASIN: u16 = 14;
/// The reservoir's head spring at rate 1 (normal): off until an admin
/// selects rate 1 or higher.
const GATED_SPRING: u16 = 15;
/// The reservoir's head spring at rate 2 (fast): an additional footprint,
/// only refilled at rate 2 or higher.
const GATED_SPRING_FAST: u16 = 17;
/// The reservoir's head spring at rate 3 (max): the biggest footprint, only
/// refilled at rate 3.
const GATED_SPRING_MAX: u16 = 18;
/// A straight notch through the dam wall's crest: closed (stone) until an
/// admin opens it. The old permanent spillway notch is sealed, so this is
/// the reservoir's only outlet.
const GATE: u16 = 16;

const MATERIALS: &[(u16, &str)] = &[
    (GRASS, "grass"),
    (DIRT, "dirt"),
    (STONE, "stone.granite"),
    (WOOD, "wood.oak"),
    (WATER, "water"),
    (SAND, "sandstone"),
    (SLATE, "slate"),
    (LOAM, "loam"),
    (LAMP, "emissive.lamp"),
    (SPRING, "water.source"),
    (DRAIN, "water.sink"),
    (SPAWN, "player.spawn"),
    (FOLIAGE, "foliage.oak"),
    (BASIN, "water.basin"),
    (GATED_SPRING, "water.source.gated"),
    (GATED_SPRING_FAST, "water.source.gated.fast"),
    (GATED_SPRING_MAX, "water.source.gated.max"),
    (GATE, "dam.gate"),
];

/// Voxels per metre.
const VPM: f32 = 4.0;
/// Valley footprint in voxels (56 m x 88 m).
const SIZE_X: i32 = 224;
const SIZE_Z: i32 = 352;
/// The lowest solid layer (bedrock and the structural anchor plane).
const BEDROCK: i32 = 4;

// Elevations in voxels (0.25 m). Everything water touches is even.
const LAKE_BED: i32 = 16;
const LAKE_SURFACE: i32 = 24;
const DAM_TOP: i32 = 32;
const POOL_BED: i32 = 12;
const POND_SURFACE: i32 = 16;
const TRIB_SPRING_BED: i32 = 20;
/// The gate notch's bottom (`LAKE_SURFACE`, less one) and top (a few voxels
/// under `DAM_TOP`, leaving a stone rim below the footbridge deck): a
/// shallow overflow notch like the old permanent spillway it replaces, not a
/// deep drain — opening it lets the reservoir spill into the river once it
/// is nearly full, rather than draining the basin through a hole in the
/// middle of the wall. Stopping short of the crest also keeps the scene's
/// fluid domain (which must cover every authored water feature, including
/// this one) under its cell budget — see `valley_water_region_is_bounded...`
/// in `examples/sandbox/tests/editor_scene.rs`.
const GATE_BOTTOM_Y: i32 = LAKE_SURFACE - 1;
const GATE_TOP_Y: i32 = DAM_TOP - 3;
/// Authored gate half-width, voxels: a real floodgate width, not a sluice
/// you have to go looking for.
const GATE_HALF_WIDTH_VOX: i32 = 6;

// Plan positions in metres.
const DAM_Z: (f32, f32) = (34.0, 36.0);
const SPILLWAY_HALF_M: f32 = 1.5;
/// The gate's offset west of the dam's centre, in metres. Zero: dead centre,
/// exactly where the old permanent spillway notch was — which is also where
/// the plunge pool and the river channel below the dam actually are (see
/// `POOL`, `RIVER_Z`, both centred on `cx(z)` same as this). An offset gate
/// looks fine from above but drops its water onto the bank, not into the
/// pool. Clear of that notch's footbridge posts and lamps regardless: those
/// stand above the deck (`DAM_TOP + 1` and up), well above this notch's own
/// top (`GATE_TOP_Y`, `DAM_TOP - 3`).
const GATE_OFFSET_M: f32 = 0.0;
const LAKE: (f32, f32, f32, f32) = (0.0, 25.0, 7.5, 9.0); // x offset, z, rx, rz
const POOL: (f32, f32) = (38.5, 3.0); // z, radius
const RIVER_Z: (f32, f32) = (38.0, 55.0);
const RIVER_HALF_M: f32 = 1.75;
const POND: (f32, f32, f32) = (-1.0, 61.0, 7.0); // x offset, z, radius
const BEACH_M: f32 = 2.0;
const OUTLET_Z: (f32, f32) = (67.0, 71.5);
const TRIB_SPRING: (f32, f32) = (7.0, 41.0); // x offset, z
const TRIB_MOUTH: (f32, f32) = (1.5, 49.0);
const BRIDGE_Z: (f32, f32) = (45.0, 47.0);
const FLOOR_HALF_M: f32 = 10.0;
/// Ridge height above the floor, in voxels. The server builds one occupancy
/// grid over the whole scene's solid bounds (capped at 2^23 cells), so the
/// tallest tree on the tallest hill sets how large the valley can be.
const HILL_MAX: f32 = 40.0;

fn cx(z_m: f32) -> f32 {
    28.0 + 2.0 * (z_m / 9.0).sin()
}

fn even(v: f32) -> i32 {
    ((v / 2.0).round() as i32) * 2
}

fn hash(x: i32, z: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343)
        ^ (z as u32).wrapping_mul(0xd816_3841)
        ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0xffff) as f32 / 65535.0
}

/// Smooth value noise in [0, 1) over metres.
fn noise(x_m: f32, z_m: f32, scale_m: f32, seed: u32) -> f32 {
    let (fx, fz) = (x_m / scale_m, z_m / scale_m);
    let (ix, iz) = (fx.floor() as i32, fz.floor() as i32);
    let (tx, tz) = (fx - ix as f32, fz - iz as f32);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sz) = (s(tx), s(tz));
    let a = hash(ix, iz, seed) + (hash(ix + 1, iz, seed) - hash(ix, iz, seed)) * sx;
    let b = hash(ix, iz + 1, seed) + (hash(ix + 1, iz + 1, seed) - hash(ix, iz + 1, seed)) * sx;
    a + (b - a) * sz
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    let (abx, abz) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * abx + (p.1 - a.1) * abz) / (abx * abx + abz * abz)).clamp(0.0, 1.0);
    let (qx, qz) = (a.0 + abx * t, a.1 + abz * t);
    (((p.0 - qx).powi(2) + (p.1 - qz).powi(2)).sqrt(), t)
}

/// What sits on top of a column.
#[derive(Clone, Copy, PartialEq)]
enum Surface {
    Grass,
    Path,
    Sand,
    Bed,
    Rock,
    Pad,
}

/// One terrain column: its top solid voxel, surface, and water span (from,
/// to, material — `WATER` everywhere except the reservoir, which is `BASIN`
/// so it starts dry).
struct Column {
    top: i32,
    surface: Surface,
    water: Option<(i32, i32, u16)>,
}

/// Flat building pads: `(x0, z0, x1, z1)` in metres, all at the south floor.
const PADS: [(f32, f32, f32, f32); 3] = [
    (18.0, 74.0, 25.5, 81.5),
    (31.0, 74.0, 38.5, 81.5),
    (24.5, 81.0, 32.0, 87.5),
];
const PAD_TOP: i32 = 20;

/// Paths as polylines in metres.
fn paths() -> Vec<Vec<(f32, f32)>> {
    vec![
        // Village square north along the west bank to the dam walkway.
        vec![
            (28.0, 73.0),
            (21.5, 69.0),
            (19.0, 60.0),
            (21.0, 50.0),
            (cx(46.0) - 4.5, 46.0),
            (21.0, 40.0),
            (cx(35.0) - 11.0, 36.5),
        ],
        // Across the bridge and up the east bank to the tributary spring.
        vec![
            (cx(46.0) + 4.5, 46.0),
            (cx(44.0) + 6.5, 43.5),
            (cx(41.0) + TRIB_SPRING.0 + 2.5, 41.0),
        ],
        // East bank down to the pond beach.
        vec![(cx(46.0) + 4.5, 46.0), (37.0, 55.0), (35.5, 62.0)],
    ]
}

fn on_path(x_m: f32, z_m: f32, paths: &[Vec<(f32, f32)>]) -> bool {
    paths.iter().any(|line| {
        line.windows(2)
            .any(|w| segment_distance((x_m, z_m), w[0], w[1]).0 < 0.9)
    })
}

fn column(x: i32, z: i32, paths: &[Vec<(f32, f32)>]) -> Column {
    // Everything near water uses the 0.5 m column grid so its banks land on
    // whole fluid cells.
    let (qx, qz) = (x & !1, z & !1);
    let (xm, zm) = ((qx as f32 + 1.0) / VPM, (qz as f32 + 1.0) / VPM);
    let (fxm, fzm) = ((x as f32 + 0.5) / VPM, (z as f32 + 0.5) / VPM);
    let c = cx(zm);
    let d = (fxm - cx(fzm)).abs();

    // Valley floor: high above the dam, falling toward the pond, then flat.
    let floor = if zm < DAM_Z.1 {
        26.0
    } else if zm < 56.0 {
        24.0 - 4.0 * (zm - DAM_Z.1) / (56.0 - DAM_Z.1)
    } else {
        20.0
    };
    let floor = even(floor);

    // Hills: rise past the floor edge with two octaves of noise.
    let past = d - FLOOR_HALF_M;
    let hills = if past > 0.0 {
        let rise = past * VPM * 0.55;
        let bumps = noise(fxm, fzm, 9.0, 1) * 8.0 + noise(fxm, fzm, 23.0, 2) * 14.0;
        // A soft ceiling keeps ridges rounded instead of clipping them flat.
        let raw = rise + bumps * smoothstep(0.0, 5.0, past);
        HILL_MAX * (1.0 - (-raw / HILL_MAX).exp())
    } else {
        0.0
    };
    // The valley head closes to the north.
    let head = if fzm < 12.0 {
        ((12.0 - fzm) * VPM * 0.7)
            .min((HILL_MAX - hills) * 0.8)
            .max(0.0)
    } else {
        0.0
    };
    let mut top = if past > -1.0 || fzm < 12.0 {
        (floor as f32 + hills + head).round() as i32
    } else {
        floor
    };
    // Steep faces turn to slate in `terrain`; a few outcrops break the grass.
    let mut surface = if past > 4.0 && noise(fxm, fzm, 5.0, 4) > 0.84 {
        Surface::Rock
    } else {
        Surface::Grass
    };
    let mut water = None;

    // Reservoir.
    let (lx, lz) = (
        (xm - (cx(LAKE.1) + LAKE.0)) / LAKE.2,
        (zm - LAKE.1) / LAKE.3,
    );
    let lake = lx * lx + lz * lz;
    if lake < 1.0 && zm < DAM_Z.0 {
        top = LAKE_BED + if lake > 0.55 { 2 } else { 0 } + if lake > 0.82 { 2 } else { 0 } - 1;
        surface = Surface::Bed;
        // BASIN, not WATER: the reservoir starts dry and fills once its
        // gated spring is switched on (still reserves the fluid domain).
        water = Some((top + 1, LAKE_SURFACE, BASIN));
    } else if lake < 1.35 && zm < DAM_Z.0 {
        // A stony shore a little above the waterline.
        top = top.min(LAKE_SURFACE + 1);
        surface = Surface::Rock;
    }

    // Plunge pool under the spillway and the river below it.
    let pool = ((xm - cx(POOL.0)).powi(2) + (zm - POOL.0).powi(2)).sqrt();
    let river = (xm - c).abs();
    if zm >= DAM_Z.1 && pool < POOL.1 {
        top = POOL_BED - 1;
        surface = Surface::Bed;
        water = Some((POOL_BED, POOL_BED + 6, WATER));
    } else if (RIVER_Z.0..RIVER_Z.1).contains(&zm) && river < RIVER_HALF_M {
        let bed = even(16.0 - 2.0 * (zm - RIVER_Z.0) / (RIVER_Z.1 - RIVER_Z.0));
        top = bed - 1;
        surface = Surface::Bed;
        water = Some((bed, bed + 4, WATER));
    } else if (RIVER_Z.0..RIVER_Z.1).contains(&zm) && river < RIVER_HALF_M + 1.0 {
        // Low banks so the river reads as a channel, not a slot.
        top = top.min(floor - 2);
        surface = Surface::Grass;
    }

    // Tributary: a spring pool on the east terrace and a creek to the river.
    let spring = (cx(TRIB_SPRING.1) + TRIB_SPRING.0, TRIB_SPRING.1);
    let mouth = (cx(TRIB_MOUTH.1) + TRIB_MOUTH.0, TRIB_MOUTH.1);
    let (creek_d, creek_t) = segment_distance((xm, zm), spring, mouth);
    let spring_d = ((xm - spring.0).powi(2) + (zm - spring.1).powi(2)).sqrt();
    if spring_d < 1.5 {
        top = TRIB_SPRING_BED - 1;
        surface = Surface::Bed;
        water = Some((TRIB_SPRING_BED, TRIB_SPRING_BED + 2, WATER));
    } else if creek_d < 0.8 && water.is_none() {
        let bed = even(TRIB_SPRING_BED as f32 - 4.0 * creek_t);
        top = bed - 1;
        surface = Surface::Bed;
        water = Some((bed, bed + 2, WATER));
    } else if creek_d < 1.6 && water.is_none() {
        top = top.min(even(TRIB_SPRING_BED as f32 - 4.0 * creek_t) + 3);
    }

    // Pond, its sandy beach, and the outlet creek to the drain.
    let pond_c = (cx(POND.1) + POND.0, POND.1);
    let pond = ((xm - pond_c.0).powi(2) + (zm - pond_c.1).powi(2)).sqrt() / POND.2;
    if pond < 1.0 {
        top = if pond < 0.45 {
            9
        } else if pond < 0.75 {
            11
        } else {
            13
        };
        surface = Surface::Bed;
        water = Some((top + 1, POND_SURFACE, WATER));
    } else if pond < 1.0 + BEACH_M / POND.2 && water.is_none() {
        top = POND_SURFACE + 1;
        surface = Surface::Sand;
    }
    // The dam asset supplies the wall (a uniformly solid stone slab, no
    // permanent notch — see `dam`) and fully overwrites the terrain under its
    // footprint, so no special-casing is needed here.
    let outlet_x = pond_c.0 + 1.0;
    if (OUTLET_Z.0..OUTLET_Z.1).contains(&zm) && (xm - outlet_x).abs() < 1.0 {
        top = POND_SURFACE - 1;
        surface = Surface::Bed;
    }

    // Building pads and paths.
    for &(x0, z0, x1, z1) in &PADS {
        if (x0..x1).contains(&fxm) && (z0..z1).contains(&fzm) {
            top = PAD_TOP - 1;
            surface = Surface::Pad;
        }
    }
    if surface == Surface::Grass && on_path(fxm, fzm, paths) {
        surface = Surface::Path;
    }
    Column {
        top: top.max(BEDROCK),
        surface,
        water,
    }
}

fn grass_tint(x: i32, z: i32) -> [u8; 3] {
    let n = noise(x as f32 / VPM, z as f32 / VPM, 6.0, 7) * 0.7 + hash(x, z, 8) * 0.3;
    let g = 112.0 + n * 32.0;
    [(g * 0.52) as u8, g as u8, (g * 0.40) as u8]
}

fn terrain(paths: &[Vec<(f32, f32)>]) -> VoxelAssetFile {
    let mut out = VoxelAssetFile::new(AssetId(0), SCENE_VOXELS_NAME);
    out.cell_size_code = 0;
    for &(slot, key) in MATERIALS {
        out.material_keys.insert(slot, key.into());
    }
    let tops: Vec<i32> = (0..SIZE_Z)
        .flat_map(|z| (0..SIZE_X).map(move |x| (x, z)))
        .map(|(x, z)| column(x, z, paths).top)
        .collect();
    let top_at =
        |x: i32, z: i32| tops[(z.clamp(0, SIZE_Z - 1) * SIZE_X + x.clamp(0, SIZE_X - 1)) as usize];
    for z in 0..SIZE_Z {
        for x in 0..SIZE_X {
            let col = column(x, z, paths);
            let steep = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dz)| (top_at(x + dx, z + dz) - col.top).abs() > 3);
            for y in BEDROCK..=col.top {
                let depth = col.top - y;
                let material = if depth == 0 {
                    match col.surface {
                        Surface::Grass if steep => SLATE,
                        Surface::Grass => GRASS,
                        Surface::Path => DIRT,
                        Surface::Sand => SAND,
                        Surface::Bed => LOAM,
                        Surface::Rock => SLATE,
                        Surface::Pad => STONE,
                    }
                } else if depth <= 2 && col.surface != Surface::Rock {
                    if col.surface == Surface::Sand {
                        SAND
                    } else {
                        DIRT
                    }
                } else if y < BEDROCK + 2 || depth > 8 {
                    STONE
                } else {
                    SLATE
                };
                let at = VoxelCoord { x, y, z };
                out.voxels.insert(at, material);
                if material == GRASS {
                    out.colors.insert(at, grass_tint(x, z));
                } else if material == DIRT && depth == 0 {
                    out.colors.insert(at, [104, 78, 52]);
                }
            }
            if let Some((from, to, material)) = col.water {
                for y in from..to {
                    out.voxels.insert(VoxelCoord { x, y, z }, material);
                }
            }
        }
    }

    // Springs: the reservoir's head (a ledge above the lake bed; gated, so the
    // reservoir starts dry until an admin selects a rate) and the tributary
    // pool (always running). The three gated rates are separate, larger
    // footprints stacked on the same ledge (not the same cells refilled more
    // often — a spring cell is already full after one step, so only more
    // source area actually fills faster).
    let head_x = (cx(LAKE.1 - LAKE.3 + 1.0) * VPM) as i32 & !1;
    let head_z = ((LAKE.1 - LAKE.3 + 1.0) * VPM) as i32 & !1;
    // (material, dx cells, dz cells, y band) — each roughly doubling the
    // previous rate's footprint.
    let gated_rates: [(u16, i32, i32, i32); 3] = [
        (GATED_SPRING, 8, 2, LAKE_SURFACE),
        (GATED_SPRING_FAST, 8, 4, LAKE_SURFACE + 2),
        (GATED_SPRING_MAX, 16, 4, LAKE_SURFACE + 4),
    ];
    for (material, dx_n, dz_n, y0) in gated_rates {
        for dx in 0..dx_n {
            for dz in 0..dz_n {
                for y in y0..y0 + 2 {
                    out.voxels.insert(
                        VoxelCoord {
                            x: head_x + dx,
                            y,
                            z: head_z + dz,
                        },
                        material,
                    );
                }
            }
        }
    }
    let ts_x = ((cx(TRIB_SPRING.1) + TRIB_SPRING.0) * VPM) as i32 & !1;
    let ts_z = (TRIB_SPRING.1 * VPM) as i32 & !1;
    for dx in 0..2 {
        for dz in 0..2 {
            for y in TRIB_SPRING_BED + 2..TRIB_SPRING_BED + 4 {
                out.voxels.insert(
                    VoxelCoord {
                        x: ts_x + dx,
                        y,
                        z: ts_z + dz,
                    },
                    SPRING,
                );
            }
        }
    }
    // The drain at the end of the outlet creek, under a stone culvert lip.
    let pond_x = cx(POND.1) + POND.0 + 1.0;
    let dz0 = ((OUTLET_Z.1 - 1.0) * VPM) as i32 & !1;
    for x in ((pond_x - 1.0) * VPM) as i32..((pond_x + 1.0) * VPM) as i32 {
        for z in dz0..dz0 + 4 {
            for y in POND_SURFACE..POND_SURFACE + 4 {
                out.voxels.insert(VoxelCoord { x, y, z }, DRAIN);
            }
        }
        for y in POND_SURFACE + 4..POND_SURFACE + 6 {
            out.voxels.insert(VoxelCoord { x, y, z: dz0 + 4 }, STONE);
        }
    }

    // Player start in the village square, facing north up the valley.
    out.voxels.insert(
        VoxelCoord {
            x: (28.0 * VPM) as i32,
            y: top_at(112, 292) + 1,
            z: (73.0 * VPM) as i32,
        },
        SPAWN,
    );
    out
}

/// The dam: a stone wall across the whole floor into both hillsides, an
/// admin-togglable spillway notch at the crest, a timber walkway with
/// rails, and lamps.
fn dam() -> VoxelAssetFile {
    let mut out = VoxelAssetFile::new(AssetId(0), "Valley Dam");
    out.cell_size_code = 0;
    for &(slot, key) in &[
        (STONE, "stone.granite"),
        (WOOD, "wood.oak"),
        (LAMP, "emissive.lamp"),
        (GATE, "dam.gate"),
    ] {
        out.material_keys.insert(slot, key.into());
    }
    let (z0, z1) = ((DAM_Z.0 * VPM) as i32, (DAM_Z.1 * VPM) as i32);
    let centre = cx((DAM_Z.0 + DAM_Z.1) / 2.0);
    let half = FLOOR_HALF_M + 5.0;
    let x0 = ((centre - half) * VPM) as i32;
    let x1 = ((centre + half) * VPM) as i32;
    let notch = (
        (((centre - SPILLWAY_HALF_M) * VPM) as i32) & !1,
        (((centre + SPILLWAY_HALF_M) * VPM) as i32 + 1) & !1,
    );
    // A uniformly solid wall: no permanent spillway notch. The `GATE` sluice
    // below is the reservoir's only outlet until an admin opens it.
    for z in z0..z1 {
        for x in x0..x1 {
            for y in 10..=DAM_TOP {
                out.voxels.insert(VoxelCoord { x, y, z }, STONE);
            }
        }
    }
    // The gate: a straight notch through the wall's crest, west of the
    // (former) spillway notch, closed (ordinary stone, marked `GATE`) until
    // an admin opens it — a shallow overflow, not a hole in the middle of
    // the wall, so opening it spills the reservoir into the river rather
    // than draining into a void. A box, not a ball: the server covers an
    // authored shape like this with several modest spheres rather than one
    // sized to the whole thing (`covering_spheres` in `spall_sim::sim`), so
    // the cut stays close to this straight-sided outline instead of
    // ballooning out to a round crater.
    let gate_x = (((centre - GATE_OFFSET_M) * VPM) as i32) & !1;
    for z in z0..z1 {
        for x in gate_x - GATE_HALF_WIDTH_VOX..=gate_x + GATE_HALF_WIDTH_VOX {
            for y in GATE_BOTTOM_Y..=GATE_TOP_Y {
                if out.voxels.get(&VoxelCoord { x, y, z }) == Some(&STONE) {
                    out.voxels.insert(VoxelCoord { x, y, z }, GATE);
                }
            }
        }
    }
    // Timber footbridge over the spillway, and rails along the crest.
    for x in notch.0 - 2..notch.1 + 2 {
        for z in z0..z1 {
            out.voxels.insert(VoxelCoord { x, y: DAM_TOP, z }, WOOD);
        }
    }
    for x in x0..x1 {
        for z in [z0, z1 - 1] {
            out.voxels.insert(
                VoxelCoord {
                    x,
                    y: DAM_TOP + 3,
                    z,
                },
                WOOD,
            );
            if x % 8 == 0 {
                for y in DAM_TOP + 1..DAM_TOP + 3 {
                    out.voxels.insert(VoxelCoord { x, y, z }, WOOD);
                }
            }
        }
    }
    for x in [notch.0 - 3, notch.1 + 2] {
        for y in DAM_TOP + 1..DAM_TOP + 7 {
            out.voxels.insert(VoxelCoord { x, y, z: z0 }, WOOD);
        }
        out.voxels.insert(
            VoxelCoord {
                x,
                y: DAM_TOP + 7,
                z: z0,
            },
            LAMP,
        );
    }
    out
}

/// A timber footbridge across the river with posts and rails.
fn bridge() -> VoxelAssetFile {
    let mut out = VoxelAssetFile::new(AssetId(0), "Timber Bridge");
    out.cell_size_code = 0;
    out.material_keys.insert(WOOD, "wood.oak".into());
    let centre = cx((BRIDGE_Z.0 + BRIDGE_Z.1) / 2.0);
    let (x0, x1) = (((centre - 4.0) * VPM) as i32, ((centre + 4.0) * VPM) as i32);
    let (z0, z1) = ((BRIDGE_Z.0 * VPM) as i32, (BRIDGE_Z.1 * VPM) as i32);
    let deck = 22;
    for x in x0..x1 {
        for z in z0..z1 {
            out.voxels.insert(VoxelCoord { x, y: deck, z }, WOOD);
            if (x - x0) % 3 == 0 {
                out.colors
                    .insert(VoxelCoord { x, y: deck, z }, [128, 98, 58]);
            }
        }
        for z in [z0, z1 - 1] {
            out.voxels.insert(VoxelCoord { x, y: deck + 4, z }, WOOD);
            if (x - x0) % 6 == 0 {
                for y in deck + 1..deck + 4 {
                    out.voxels.insert(VoxelCoord { x, y, z }, WOOD);
                }
            }
        }
    }
    out
}

/// A timber post with a glowing lantern, 3 m tall.
fn lamp_post() -> VoxelAssetFile {
    let mut out = VoxelAssetFile::new(AssetId(0), "Lamp Post");
    out.cell_size_code = 0;
    out.material_keys.insert(WOOD, "wood.oak".into());
    out.material_keys.insert(LAMP, "emissive.lamp".into());
    for y in 0..10 {
        out.voxels.insert(VoxelCoord { x: 0, y, z: 0 }, WOOD);
    }
    for (x, z) in [(-1, 0), (1, 0), (0, -1), (0, 1), (0, 0)] {
        out.voxels.insert(VoxelCoord { x, y: 10, z }, LAMP);
    }
    out.voxels.insert(VoxelCoord { x: 0, y: 11, z: 0 }, WOOD);
    out
}

fn add_asset(
    model: &mut EditorModel,
    mut asset: VoxelAssetFile,
    file_name: &str,
) -> Result<AssetId, Box<dyn std::error::Error>> {
    let command = model.new_voxel_asset_command(asset.name.clone());
    let EditorCommand::CreateVoxelAsset { mut record, .. } = command else {
        unreachable!()
    };
    record.storage = PathBuf::from("assets").join(file_name);
    asset.id = record.id;
    let id = record.id;
    EditorCommand::CreateVoxelAsset { record, asset }.apply(model)?;
    Ok(id)
}

fn load_named_asset(
    project: &str,
    name: &str,
) -> Result<VoxelAssetFile, Box<dyn std::error::Error>> {
    let model = EditorModel::load(project)?;
    model
        .voxel_assets
        .into_values()
        .find(|asset| asset.name == name)
        .ok_or_else(|| format!("{project} is missing asset {name}").into())
}

fn place(model: &mut EditorModel, name: &str, asset: AssetId, at: [f32; 3], yaw: f32) {
    let mut entity = model.scene.new_entity(name);
    entity.voxel_asset = Some(asset);
    entity.transform = Transform {
        translation: at,
        rotation_degrees: [0.0, yaw, 0.0],
        ..Transform::default()
    };
    model.scene.entities.insert(entity.id, entity);
}

/// The centre of an asset's lowest layer (its trunk foot), in metres of the
/// asset's own frame. Handles either cell size.
fn trunk_base(asset: &VoxelAssetFile) -> [f32; 3] {
    let cell_m = if asset.cell_size_code == 1 {
        0.0625
    } else {
        0.25
    };
    let low = asset.voxels.keys().map(|c| c.y).min().unwrap_or(0);
    let foot: Vec<_> = asset.voxels.keys().filter(|c| c.y == low).collect();
    let n = foot.len().max(1) as f32;
    let mean = |f: fn(&VoxelCoord) -> i32| foot.iter().map(|c| f(c) as f32 + 0.5).sum::<f32>() / n;
    [
        mean(|c| c.x) * cell_m,
        low as f32 * cell_m,
        mean(|c| c.z) * cell_m,
    ]
}

/// Minimum corner and size of an asset's cells, in its own cells.
fn bounds(asset: &VoxelAssetFile) -> ([i32; 3], [i32; 3]) {
    let mut lo = [i32::MAX; 3];
    let mut hi = [i32::MIN; 3];
    for c in asset.voxels.keys() {
        for (a, v) in [c.x, c.y, c.z].into_iter().enumerate() {
            lo[a] = lo[a].min(v);
            hi[a] = hi[a].max(v);
        }
    }
    (lo, [0, 1, 2].map(|a| hi[a] - lo[a] + 1))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/valley-showcase"));
    // A stale asset from an earlier layout must not linger in the project.
    let assets_dir = out.join("assets");
    if assets_dir.is_dir() {
        for entry in std::fs::read_dir(&assets_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "spvox") {
                std::fs::remove_file(path)?;
            }
        }
    }
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Spall Valley Showcase");
    model.scene.environment = "daylight".into();
    for &(slot, key) in MATERIALS {
        model.project.material_mapping.insert(key.into(), slot);
    }

    let paths = paths();
    let ground_asset = terrain(&paths);
    // Surface height (feet level, metres) of any column, for placing props.
    let tops: std::collections::HashMap<(i32, i32), i32> = ground_asset
        .voxels
        .iter()
        .filter(|(_, m)| {
            !matches!(
                **m,
                WATER
                    | SPRING
                    | DRAIN
                    | SPAWN
                    | BASIN
                    | GATED_SPRING
                    | GATED_SPRING_FAST
                    | GATED_SPRING_MAX
            )
        })
        .fold(std::collections::HashMap::new(), |mut acc, (c, _)| {
            let slot = acc.entry((c.x, c.z)).or_insert(c.y);
            *slot = (*slot).max(c.y);
            acc
        });
    let wet: std::collections::HashSet<(i32, i32)> = ground_asset
        .voxels
        .iter()
        .filter(|(_, m)| matches!(**m, WATER | BASIN))
        .map(|(c, _)| (c.x, c.z))
        .collect();
    let surface_m = |x_m: f32, z_m: f32| {
        let (x, z) = ((x_m * VPM) as i32, (z_m * VPM) as i32);
        (tops.get(&(x, z)).copied().unwrap_or(BEDROCK) + 1) as f32 / VPM
    };
    let ground = add_asset(&mut model, ground_asset.clone(), "valley_terrain.spvox")?;
    let mut terrain_entity = model.scene.new_entity(SCENE_VOXELS_NAME);
    terrain_entity.voxel_asset = Some(ground);
    model
        .scene
        .entities
        .insert(terrain_entity.id, terrain_entity);

    let dam = add_asset(&mut model, dam(), "valley_dam.spvox")?;
    place(&mut model, "Dam and spillway", dam, [0.0; 3], 0.0);
    let bridge = add_asset(&mut model, bridge(), "timber_bridge.spvox")?;
    place(&mut model, "Timber bridge", bridge, [0.0; 3], 0.0);

    // Every current tree asset.
    let tree_sources = [
        (
            "fixtures/terrain-trees",
            "Terrain Palm Tree",
            "palm_tree.spvox",
        ),
        (
            "fixtures/terrain-trees",
            "Terrain Weeping Willow",
            "weeping_willow.spvox",
        ),
        (
            "fixtures/terrain-trees-v2",
            "Terrain Palm Tree 2",
            "palm_tree_2.spvox",
        ),
        (
            "fixtures/terrain-trees-v2",
            "Terrain Weeping Willow 2",
            "weeping_willow_2.spvox",
        ),
    ];
    let mut trees = Vec::new();
    let mut trunk_bases = std::collections::HashMap::new();
    for (project, name, file) in tree_sources {
        let asset = load_named_asset(project, name)?;
        let base = trunk_base(&asset);
        let id = add_asset(&mut model, asset, file)?;
        trunk_bases.insert(id, base);
        trees.push(id);
    }
    let [palm, willow, palm2, willow2] = [trees[0], trees[1], trees[2], trees[3]];
    let pond_c = (cx(POND.1) + POND.0, POND.1);
    let mut planted: Vec<(f32, f32)> = Vec::new();
    let clear_of = |x: f32, z: f32, planted: &[(f32, f32)], gap: f32| {
        planted.iter().all(|(px, pz)| (px - x).hypot(pz - z) >= gap)
    };
    let dry = |x: f32, z: f32| {
        let (vx, vz) = ((x * VPM) as i32, (z * VPM) as i32);
        (-4..=4).all(|dx| (-4..=4).all(|dz| !wet.contains(&(vx + dx, vz + dz))))
    };
    let plant = |model: &mut EditorModel,
                 planted: &mut Vec<(f32, f32)>,
                 asset: AssetId,
                 x: f32,
                 z: f32,
                 label: &str| {
        let yaw: f32 = [0.0, 90.0, 180.0, 270.0][(hash(x as i32, z as i32, 9) * 4.0) as usize % 4];
        let index = planted.len();
        // Put the trunk, not the asset origin, on the chosen spot: rotate
        // the trunk base like the editor does (+X toward +Z) and subtract it.
        let [bx, by, bz] = trunk_bases[&asset];
        let (s, c) = yaw.to_radians().sin_cos();
        let (rx, rz) = (c * bx - s * bz, s * bx + c * bz);
        place(
            model,
            &format!("{label} {index}"),
            asset,
            [x - rx, surface_m(x, z) - by, z - rz],
            yaw,
        );
        planted.push((x, z));
    };
    // Palms ring the pond beach.
    for i in 0..7 {
        let a = i as f32 / 7.0 * std::f32::consts::TAU + 0.4;
        let (x, z) = (
            pond_c.0 + a.cos() * (POND.2 + 1.2),
            pond_c.1 + a.sin() * (POND.2 + 1.2),
        );
        if z < 66.0 && dry(x, z) && !on_path(x, z, &paths) {
            plant(
                &mut model,
                &mut planted,
                if i % 2 == 0 { palm } else { palm2 },
                x,
                z,
                "Beach palm",
            );
        }
    }
    // Willows lean over the river and the reservoir shore.
    for (i, z) in [40.0f32, 44.0, 50.0, 53.0, 20.0, 27.0, 31.0]
        .into_iter()
        .enumerate()
    {
        let side = if i % 2 == 0 { -1.0 } else { 1.0 };
        let off = if z < DAM_Z.0 {
            LAKE.2 + 2.5
        } else {
            RIVER_HALF_M + 2.5
        };
        let (x, z) = (cx(z) + side * off, z);
        if dry(x, z) && !on_path(x, z, &paths) && clear_of(x, z, &planted, 3.0) {
            plant(
                &mut model,
                &mut planted,
                if i % 2 == 0 { willow } else { willow2 },
                x,
                z,
                "River willow",
            );
        }
    }
    // A mixed forest on both hillsides.
    for gz in 0..11 {
        for gx in 0..8 {
            let x = 2.0 + gx as f32 * 7.2 + hash(gx, gz, 11) * 4.0;
            let z = 4.0 + gz as f32 * 7.9 + hash(gx, gz, 12) * 4.0;
            let hillside = (x - cx(z)).abs() > FLOOR_HALF_M + 2.0;
            if hillside
                && dry(x, z)
                && clear_of(x, z, &planted, 4.5)
                && !(DAM_Z.0 - 2.0..DAM_Z.1 + 2.0).contains(&z)
                && x > 1.0
                && x < 55.0
            {
                let pick = [palm, willow, palm2, willow2][(hash(gx, gz, 13) * 4.0) as usize % 4];
                plant(&mut model, &mut planted, pick, x, z, "Hill tree");
            }
        }
    }

    // The three lighting-room houses on the village pads.
    let houses = [
        ("House (sealed)", "house_sealed.spvox", 0, 90.0),
        ("House (doorway)", "house_doorway.spvox", 1, 270.0),
        ("House (roofless)", "house_roofless.spvox", 2, 180.0),
    ];
    for (name, file, pad, yaw) in houses {
        let asset = load_named_asset("fixtures/lighting-room", name)?;
        let (lo, size) = bounds(&asset);
        let (x0, z0, x1, z1) = PADS[pad];
        // Centre the footprint on the pad, feet on the pad top.
        let (cxm, czm) = ((x0 + x1) / 2.0, (z0 + z1) / 2.0);
        let (hx, hz) = (size[0] as f32 / 2.0 / VPM, size[2] as f32 / 2.0 / VPM);
        let id = add_asset(&mut model, asset, file)?;
        // A quarter turn about the origin swaps which corner is the minimum.
        let (ox, oz) = match yaw as i32 {
            90 => (cxm + hz, czm - hx),
            180 => (cxm + hx, czm + hz),
            270 => (cxm - hz, czm + hx),
            _ => (cxm - hx, czm - hz),
        };
        let (lx, lz) = (lo[0] as f32 / VPM, lo[2] as f32 / VPM);
        let (rx, rz) = match yaw as i32 {
            90 => (ox + lz, oz - lx),
            180 => (ox + lx, oz + lz),
            270 => (ox - lz, oz + lx),
            _ => (ox - lx, oz - lz),
        };
        place(
            &mut model,
            name,
            id,
            [rx, PAD_TOP as f32 / VPM - lo[1] as f32 / VPM, rz],
            yaw,
        );
    }

    // Lamp posts along the paths, the bridge, and the village square.
    let lamp = add_asset(&mut model, lamp_post(), "lamp_post.spvox")?;
    let mut lamps: Vec<(f32, f32)> = vec![
        (cx(45.0) - 4.3, 44.6),
        (cx(47.0) + 4.3, 47.4),
        (24.0, 72.0),
        (32.5, 72.0),
    ];
    for line in &paths {
        let mut carried = 0.0;
        for w in line.windows(2) {
            let len = (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1);
            let mut s = 9.0 - carried;
            while s < len {
                let t = s / len;
                let (x, z) = (
                    w[0].0 + (w[1].0 - w[0].0) * t,
                    w[0].1 + (w[1].1 - w[0].1) * t,
                );
                lamps.push((x + 1.3, z));
                s += 9.0;
            }
            carried = (carried + len) % 9.0;
        }
    }
    for (i, (x, z)) in lamps.into_iter().enumerate() {
        if dry(x, z) {
            place(
                &mut model,
                &format!("Lamp post {i}"),
                lamp,
                [x, surface_m(x, z), z],
                0.0,
            );
        }
    }

    model.save_all()?;
    println!(
        "wrote playable valley showcase to {} ({} entities)",
        out.display(),
        model.scene.entities.len()
    );
    Ok(())
}
