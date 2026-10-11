//! The 3D half: fill bricks from the column map and cave field.

use crate::caves::{CaveField, CornerGrid, MOUTH_RADIUS};
use crate::columns::{Biome, ColumnMap, Stack};
use crate::noise::fbm3;
use crate::spec::{BORDER_CELLS, BRICK, GEN_VERSION, GenError, SEA_LEVEL, WorldGenSpec};
use spall_core::{BrickCoord, CELLS_PER_BRICK, CellSizeCode, GlobalCell, MaterialId, Revision};
use spall_voxel::brick::Brick;
use spall_voxel::volume::Volume;

/// Metres per cell at the only cell size worlds are generated at.
const CELL_M: f64 = 0.25;
/// Deepest surface stack (sand over sandstone): below this under the lowest
/// surface cell of a brick footprint, the brick is pure rock.
const MAX_STACK: i64 = 40;
// Noise streams for subsurface pockets. Changing one changes the world: bump
// GEN_VERSION.
const S_POCKET_A: u64 = 0x81;
const S_POCKET_B: u64 = 0x82;
/// Rock bands by height: bedrock below, deep stone up to, then stone.
const BEDROCK_TOP: i64 = BRICK;
const DEEP_TOP: i64 = 3 * BRICK;
/// Edge of a column-water region in cells (mirrors `spall_fluid`'s `REGION_EDGE`).
const WATER_REGION_EDGE: i64 = 32;
/// Most 32x32 column regions that may hold water. The column model is sparse by
/// region, so cost follows the wet regions, not the bounding box of the water:
/// 4096 regions is a quarter of a 1 km world.
pub const WATER_REGION_BUDGET: u64 = 4096;
/// Most water cells a plan may list (each is listed in `WaterPlan::cells`).
pub const WATER_CELL_BUDGET: u64 = 64 * 1024 * 1024;
/// Clear cells above a spawn surface (2.25 m for the player capsule).
pub const SPAWN_HEADROOM_CELLS: i64 = 9;
const MAX_SPAWNS: usize = 4;

/// Initial water: every cell between the terrain and the sea surface. The
/// fluid solver is handed this, never voxel materials.
#[derive(Debug, Clone, Default)]
pub struct WaterPlan {
    /// Cells with `y <= surface_y` above the terrain are full of water.
    pub surface_y: i64,
    /// Full water cells, in `(z, x, y)` order.
    pub cells: Vec<GlobalCell>,
    /// Inclusive cell box around `cells`, or `None` for a dry world.
    pub bounds: Option<(GlobalCell, GlobalCell)>,
}

impl WaterPlan {
    /// Distinct 32x32 column regions holding at least one water cell.
    pub fn wet_regions(&self) -> u64 {
        let mut regions = std::collections::BTreeSet::new();
        for c in &self.cells {
            regions.insert((
                c.x.div_euclid(WATER_REGION_EDGE),
                c.z.div_euclid(WATER_REGION_EDGE),
            ));
        }
        regions.len() as u64
    }
}

pub struct GeneratedWorld {
    pub version: u32,
    pub terrain: Volume,
    /// Inclusive global-cell box of the arena (full height).
    pub region: (GlobalCell, GlobalCell),
    pub water: WaterPlan,
    /// Player start positions in metres, feet on the ground.
    pub spawns: Vec<[f64; 3]>,
    /// Global `y` of the declared support plane (the bedrock floor).
    pub anchor_y: i64,
    pub columns: ColumnMap,
    pub cave_mouths: Vec<(i64, i64)>,
}

fn base_material(spec: &WorldGenSpec, y: i64) -> MaterialId {
    let p = &spec.palette;
    if y < BEDROCK_TOP {
        p.bedrock
    } else if y < DEEP_TOP {
        p.deep_stone
    } else {
        p.stone
    }
}

struct Footprint {
    /// Global cell `x` / `z` of the footprint's first column.
    x0: i64,
    z0: i64,
    heights: Vec<i32>,
    stacks: Vec<Stack>,
    lowland: Vec<u8>,
    hmin: i64,
    hmax: i64,
}

fn footprint(spec: &WorldGenSpec, columns: &ColumnMap, bx: i64, bz: i64) -> Footprint {
    let n = (BRICK * BRICK) as usize;
    let (mut heights, mut stacks, mut lowland) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let (mut hmin, mut hmax) = (i64::MAX, i64::MIN);
    for lz in 0..BRICK {
        for lx in 0..BRICK {
            let (x, z) = (bx * BRICK + lx, bz * BRICK + lz);
            let h = columns.height(x, z);
            hmin = hmin.min(i64::from(h));
            hmax = hmax.max(i64::from(h));
            heights.push(h);
            stacks.push(columns.stack(&spec.palette, x, z));
            lowland.push(columns.lowland(x, z));
        }
    }
    Footprint {
        x0: bx * BRICK,
        z0: bz * BRICK,
        heights,
        stacks,
        lowland,
        hmin,
        hmax,
    }
}

/// Pockets of other rock inside the rock near the surface, so a cut shows
/// strata and not one flat colour all the way down. Only within `MAX_STACK`
/// cells of the surface: deeper bricks stay uniform and cheap. `subsoil` is the
/// softer layer under the topsoil (mostly dirt), which only gets gravel and
/// stone pebbles.
fn pocket(spec: &WorldGenSpec, x: i64, y: i64, z: i64, subsoil: bool) -> Option<MaterialId> {
    let (xf, yf, zf) = (x as f64, y as f64, z as f64);
    let a = fbm3(spec.seed ^ S_POCKET_A, xf / 14.0, yf / 5.0, zf / 14.0, 2);
    let b = fbm3(spec.seed ^ S_POCKET_B, xf / 11.0, yf / 6.0, zf / 11.0, 2);
    let p = &spec.palette;
    if subsoil {
        if a > 0.5 {
            Some(p.gravel)
        } else if b > 0.5 {
            Some(p.stone)
        } else {
            None
        }
    } else if a > 0.38 {
        Some(p.gravel)
    } else if a < -0.42 {
        Some(p.clay)
    } else if b > 0.42 {
        Some(p.slate)
    } else if b < -0.48 {
        Some(p.dirt)
    } else {
        None
    }
}

/// Fills `cells` (a worker's reused scratch buffer) for the brick at `y0` and builds it in its
/// final compact form: uniform when every cell is equal, a palette otherwise.
fn fill_dense(
    spec: &WorldGenSpec,
    fp: &Footprint,
    corners: Option<&CornerGrid>,
    y0: i64,
    cells: &mut Vec<MaterialId>,
) -> Brick {
    cells.clear();
    cells.resize(CELLS_PER_BRICK, MaterialId::AIR);
    for lz in 0..BRICK {
        for lx in 0..BRICK {
            let col = (lz * BRICK + lx) as usize;
            let h = i64::from(fp.heights[col]);
            let stack = fp.stacks[col];
            for ly in 0..BRICK {
                let y = y0 + ly;
                if y > h {
                    break;
                }
                if let Some(c) = corners {
                    let (nat, mouth) = c.at(lx, ly, lz);
                    if (mouth > 0.0 && CaveField::mouth_allowed(y))
                        || (nat > 0.0 && CaveField::natural_allowed(y, h, fp.lowland[col]))
                    {
                        continue;
                    }
                }
                let depth = h - y;
                let (x, z) = (fp.x0 + lx, fp.z0 + lz);
                let material = if depth < i64::from(stack.top_cells) {
                    stack.top
                } else if depth < i64::from(stack.depth()) {
                    let soft = stack.sub == spec.palette.dirt || stack.sub == spec.palette.mud;
                    soft.then(|| pocket(spec, x, y, z, true))
                        .flatten()
                        .unwrap_or(stack.sub)
                } else if depth < MAX_STACK && y >= BEDROCK_TOP {
                    pocket(spec, x, y, z, false).unwrap_or_else(|| base_material(spec, y))
                } else {
                    base_material(spec, y)
                };
                cells[(lx + BRICK * (ly + BRICK * lz)) as usize] = material;
            }
        }
    }
    Brick::restored(cells, Revision::ZERO, false)
}

fn generate_column(
    spec: &WorldGenSpec,
    columns: &ColumnMap,
    caves: &CaveField,
    bx: i64,
    bz: i64,
    scratch: &mut Vec<MaterialId>,
) -> Vec<(BrickCoord, Brick)> {
    let last = i64::from(spec.size_cells) / BRICK - 1;
    if bx == 0 || bz == 0 || bx == last || bz == last {
        // The arena wall: solid bedrock up to `WALL_TOP`, open sky above.
        return (0..spec.height_bricks)
            .map(|by| {
                let material = if by * BRICK <= spec.wall_top() {
                    spec.palette.bedrock
                } else {
                    MaterialId::AIR
                };
                (
                    BrickCoord::new(bx, by, bz),
                    Brick::uniform(material, Revision::ZERO),
                )
            })
            .collect();
    }
    let fp = footprint(spec, columns, bx, bz);
    let mut out = Vec::with_capacity(spec.height_bricks as usize);
    for by in 0..spec.height_bricks {
        let (y0, y1) = (by * BRICK, by * BRICK + BRICK - 1);
        let brick = if y0 > fp.hmax {
            Brick::uniform(MaterialId::AIR, Revision::ZERO)
        } else {
            let corners = (y1 >= crate::caves::CAVE_FLOOR)
                .then(|| caves.corners([bx * BRICK, y0, bz * BRICK]))
                .filter(CornerGrid::may_carve);
            if corners.is_none() && y1 <= fp.hmin - MAX_STACK {
                Brick::uniform(base_material(spec, y0), Revision::ZERO)
            } else {
                fill_dense(spec, &fp, corners.as_ref(), y0, scratch)
            }
        };
        out.push((BrickCoord::new(bx, by, bz), brick));
    }
    out
}

fn water_plan(columns: &ColumnMap) -> WaterPlan {
    let size = i64::from(columns.size());
    let mut cells = Vec::new();
    let mut lo = [i64::MAX; 3];
    let mut hi = [i64::MIN; 3];
    for z in 0..size {
        for x in 0..size {
            if !columns.is_water(x, z) {
                continue;
            }
            let h = i64::from(columns.height(x, z));
            for y in h + 1..=i64::from(SEA_LEVEL) {
                cells.push(GlobalCell::new(x, y, z));
            }
            lo = [lo[0].min(x), lo[1].min(h + 1), lo[2].min(z)];
            hi = [hi[0].max(x), hi[1].max(i64::from(SEA_LEVEL)), hi[2].max(z)];
        }
    }
    let bounds = (!cells.is_empty()).then(|| {
        (
            GlobalCell::new(lo[0], lo[1], lo[2]),
            GlobalCell::new(hi[0], hi[1], hi[2]),
        )
    });
    WaterPlan {
        surface_y: i64::from(SEA_LEVEL),
        cells,
        bounds,
    }
}

/// Wet columns and the regions they fall in, counted from the column map the
/// way [`water_plan`] fills it, without allocating any water cell.
fn wet_extent(columns: &ColumnMap) -> (u64, u64) {
    let size = i64::from(columns.size());
    let mut regions = std::collections::BTreeSet::new();
    let mut cells = 0u64;
    for z in 0..size {
        for x in 0..size {
            let h = i64::from(columns.height(x, z));
            if !columns.is_water(x, z) || h >= i64::from(SEA_LEVEL) {
                continue;
            }
            cells += (i64::from(SEA_LEVEL) - h) as u64;
            regions.insert((
                x.div_euclid(WATER_REGION_EDGE),
                z.div_euclid(WATER_REGION_EDGE),
            ));
        }
    }
    (regions.len() as u64, cells)
}

fn check_water_budget(regions: u64, cells: u64) -> Result<(), GenError> {
    for (what, measured, budget) in [
        ("regions", regions, WATER_REGION_BUDGET),
        ("cells", cells, WATER_CELL_BUDGET),
    ] {
        if measured > budget {
            return Err(GenError::WaterBudget {
                what,
                measured,
                budget,
            });
        }
    }
    Ok(())
}

/// Checks the water capacity before allocating terrain or water cells: wet
/// 32x32 regions and listed water cells, not the water's bounding box.
pub fn validate_water_budget(columns: &ColumnMap) -> Result<(), GenError> {
    let (regions, cells) = wet_extent(columns);
    check_water_budget(regions, cells)
}

/// Dry meadow spots with a flat 5x5 footprint, clear of cave mouth tunnels.
fn find_spawns(columns: &ColumnMap, caves: &CaveField) -> Vec<[f64; 3]> {
    let size = i64::from(columns.size());
    let targets = [(0.50, 0.50), (0.42, 0.46), (0.58, 0.44), (0.40, 0.58)];
    let mut spawns: Vec<[f64; 3]> = Vec::new();
    for (tu, tv) in targets {
        let (cx, cz) = ((tu * size as f64) as i64, (tv * size as f64) as i64);
        'search: for ring in (0..size / 2).step_by(6) {
            for i in -ring..=ring {
                for (dx, dz) in [(i, -ring), (i, ring), (-ring, i), (ring, i)] {
                    let (x, z) = (cx + dx, cz + dz);
                    if x < BORDER_CELLS + 8
                        || z < BORDER_CELLS + 8
                        || x >= size - BORDER_CELLS - 8
                        || z >= size - BORDER_CELLS - 8
                    {
                        continue;
                    }
                    let h = columns.height(x, z);
                    let flat = (-2..=2).all(|oz| {
                        (-2..=2).all(|ox| {
                            columns.biome(x + ox, z + oz) == Biome::Meadow
                                && columns.lowland(x + ox, z + oz) == 0
                                && (columns.height(x + ox, z + oz) - h).abs() <= 1
                        })
                    });
                    // The ground under a spawn must not be a cave mouth.
                    let clear_of_mouths =
                        caves.mouth_distance_xz(x as f64, z as f64) > MOUTH_RADIUS + 6.0;
                    if flat && h > SEA_LEVEL + 8 && clear_of_mouths {
                        let p = [
                            x as f64 * CELL_M + CELL_M / 2.0,
                            (i64::from(h) + 1) as f64 * CELL_M,
                            z as f64 * CELL_M + CELL_M / 2.0,
                        ];
                        if spawns
                            .iter()
                            .all(|s| (s[0] - p[0]).abs().max((s[2] - p[2]).abs()) > 4.0)
                        {
                            spawns.push(p);
                        }
                        break 'search;
                    }
                }
            }
        }
        if spawns.len() == MAX_SPAWNS {
            break;
        }
    }
    spawns
}

/// Generates the whole arena. A pure function of the spec (and `GEN_VERSION`).
pub fn generate(spec: &WorldGenSpec) -> Result<GeneratedWorld, GenError> {
    generate_with_timings(spec).map(|(world, _)| world)
}

/// Diagnostic sidecar; these durations never enter generated, saved or replicated state.
pub struct GenerationTimings {
    pub columns: std::time::Duration,
    pub fill: std::time::Duration,
    pub packing: std::time::Duration,
    pub compact: std::time::Duration,
    pub insert: std::time::Duration,
    pub source_release: std::time::Duration,
    pub water_and_spawns: std::time::Duration,
}

pub fn generate_with_timings(
    spec: &WorldGenSpec,
) -> Result<(GeneratedWorld, GenerationTimings), GenError> {
    let started = std::time::Instant::now();
    let columns = ColumnMap::compute(spec)?;
    validate_water_budget(&columns)?;
    let caves = CaveField::new(spec.seed, &columns);
    let column_time = started.elapsed();
    let started = std::time::Instant::now();
    let bricks_per_side = i64::from(spec.size_cells) / BRICK;

    let jobs: Vec<(i64, i64)> = (0..bricks_per_side)
        .flat_map(|bz| (0..bricks_per_side).map(move |bx| (bx, bz)))
        .collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, |p| p.get())
        .min(16);
    let per = jobs.len().div_ceil(threads);
    let mut built: Vec<Vec<(BrickCoord, Brick)>> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .chunks(per)
            .map(|chunk| {
                let (columns, caves) = (&columns, &caves);
                scope.spawn(move || {
                    // One full-width scratch buffer per worker, reused for every brick, so no
                    // full-width array is ever kept or freed per brick.
                    let mut scratch = Vec::with_capacity(CELLS_PER_BRICK);
                    chunk
                        .iter()
                        .flat_map(|&(bx, bz)| {
                            generate_column(spec, columns, caves, bx, bz, &mut scratch)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            // A panicked worker must fail generation, not yield a partial world.
            built.push(handle.join().expect("worldgen worker panicked"));
        }
    });
    let fill_time = started.elapsed();
    let started = std::time::Instant::now();

    let mut terrain = Volume::new(spec.volume_id, CellSizeCode::Quarter);
    // Bricks arrive already in their final compact form, built from per-worker scratch buffers,
    // so there is no raw source array to retain through packing or to free afterwards.
    let insert_started = std::time::Instant::now();
    for (coord, brick) in built.into_iter().flatten() {
        terrain.insert_brick(coord, brick)?;
    }
    let insert_time = insert_started.elapsed();
    let packing_time = started.elapsed();
    let started = std::time::Instant::now();

    let water = water_plan(&columns);
    check_water_budget(water.wet_regions(), water.cells.len() as u64)?;
    let cave_mouths = caves.mouth_sites();
    let spawns = find_spawns(&columns, &caves);
    if spawns.is_empty() {
        return Err(GenError::NoSpawn);
    }
    let edge = i64::from(spec.size_cells);
    Ok((
        GeneratedWorld {
            version: GEN_VERSION,
            terrain,
            region: (
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(edge - 1, spec.height_cells() - 1, edge - 1),
            ),
            water,
            spawns,
            anchor_y: 0,
            columns,
            cave_mouths,
        },
        GenerationTimings {
            columns: column_time,
            fill: fill_time,
            packing: packing_time,
            // Retired stages: bricks are built compact and nothing is retained or released.
            compact: std::time::Duration::ZERO,
            insert: insert_time,
            source_release: std::time::Duration::ZERO,
            water_and_spawns: started.elapsed(),
        },
    ))
}

#[cfg(test)]
mod timing_probe {
    use super::*;
    use crate::{Preset, WorldgenPalette};

    /// Manual: full 4096-cell generation with each stage's time and a digest of the terrain, to
    /// show a change to generation is exact and what it costs. Runs the showcase layout at its
    /// own 12-brick height and at the 16-brick height 1 km presets use, to measure what the extra
    /// (mostly uniform air) bricks cost. Several GiB.
    #[test]
    #[ignore = "manual full-world generation timing; several GiB"]
    fn full_world_generation_timings() {
        for height in [12, 16] {
            let spec = WorldGenSpec::new(Preset::Showcase, 1, 4096, WorldgenPalette::sequential(1))
                .with_height_bricks(height);
            let started = std::time::Instant::now();
            let (world, t) = generate_with_timings(&spec).unwrap();
            let total = started.elapsed();
            eprintln!(
                "gen_probe height_bricks={height} total_ms={} columns_ms={} fill_ms={} packing_ms={} insert_ms={} water_ms={}",
                total.as_millis(),
                t.columns.as_millis(),
                t.fill.as_millis(),
                t.packing.as_millis(),
                t.insert.as_millis(),
                t.water_and_spawns.as_millis()
            );
            eprintln!(
                "gen_probe height_bricks={height} digest={} bricks={} memory={:?} wet_regions={} wet_cells={}",
                spall_voxel::fixtures::digest_hex(&world.terrain),
                world.terrain.resident_brick_count(),
                world.terrain.memory_report(),
                world.water.wet_regions(),
                world.water.cells.len()
            );
        }
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use crate::{Preset, WorldgenPalette};

    #[test]
    fn preflight_agrees_with_exact_water_plan() {
        for seed in [1, 7, 42] {
            let columns = ColumnMap::compute(&WorldGenSpec::new(
                Preset::Showcase,
                seed,
                512,
                WorldgenPalette::sequential(1),
            ))
            .unwrap();
            let plan = water_plan(&columns);
            assert_eq!(
                wet_extent(&columns),
                (plan.wet_regions(), plan.cells.len() as u64),
                "seed {seed}"
            );
            validate_water_budget(&columns).unwrap();
        }
    }

    #[test]
    fn budget_failures_name_what_was_exceeded() {
        assert!(check_water_budget(WATER_REGION_BUDGET, WATER_CELL_BUDGET).is_ok());
        assert!(matches!(
            check_water_budget(WATER_REGION_BUDGET + 1, 0),
            Err(GenError::WaterBudget {
                what: "regions",
                ..
            })
        ));
        assert!(matches!(
            check_water_budget(0, WATER_CELL_BUDGET + 1),
            Err(GenError::WaterBudget { what: "cells", .. })
        ));
    }

    #[test]
    #[ignore = "large-world capacity regression; computes a 4096-cell column map only"]
    fn large_world_water_fits_the_region_budget() {
        let spec = WorldGenSpec::new(Preset::Showcase, 1, 4096, WorldgenPalette::sequential(1));
        let columns = ColumnMap::compute(&spec).unwrap();
        validate_water_budget(&columns).unwrap();
        let (regions, cells) = wet_extent(&columns);
        eprintln!("capacity: wet_regions={regions} wet_cells={cells}");
    }
}
