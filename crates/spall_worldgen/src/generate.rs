//! The 3D half: fill bricks from the column map and cave field.

use crate::caves::{CaveField, CornerGrid, MOUTH_RADIUS};
use crate::columns::{Biome, ColumnMap, Stack};
use crate::noise::fbm3;
use crate::spec::{BORDER_CELLS, BRICK, GenError, SEA_LEVEL, WorldGenSpec};
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
/// region, so cost follows the wet regions, not the bounding box of the water.
pub const WATER_REGION_BUDGET: u64 = 8192;
/// Clear cells above a spawn surface (2.25 m for the player capsule).
pub const SPAWN_HEADROOM_CELLS: i64 = 9;
const MAX_SPAWNS: usize = 4;

/// One wet column: water fills the open span above the terrain surface `bed`
/// up to and including cell `top`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WetColumn {
    pub x: i32,
    pub z: i32,
    pub bed: i32,
    pub top: i32,
}

/// A river head: the cell it adds water to and how much per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    pub cell: GlobalCell,
    /// Cells^3 per second (see [`crate::rivers`]).
    pub flow_cells3_s: f32,
}

/// Initial water, per column: the fluid solver is handed this, never voxel
/// materials. Springs and sinks are explicit exchange cells, not hidden ones.
#[derive(Debug, Clone, Default)]
pub struct WaterPlan {
    /// Water level of the sea; other water bodies carry their own `top`.
    pub surface_y: i64,
    /// Wet columns in `(z, x)` order.
    pub columns: Vec<WetColumn>,
    /// Inclusive cell box around the water, or `None` for a dry world.
    pub bounds: Option<(GlobalCell, GlobalCell)>,
    /// River heads, each with a fixed discharge.
    pub springs: Vec<Spring>,
    /// Cells emptied every step: where a landlocked map's water ends.
    pub sinks: Vec<GlobalCell>,
}

impl WaterPlan {
    /// Distinct 32x32 column regions holding water.
    pub fn wet_regions(&self) -> u64 {
        let mut regions = std::collections::BTreeSet::new();
        for c in &self.columns {
            regions.insert((
                i64::from(c.x).div_euclid(WATER_REGION_EDGE),
                i64::from(c.z).div_euclid(WATER_REGION_EDGE),
            ));
        }
        regions.len() as u64
    }

    /// Full water cells in the plan.
    pub fn cell_count(&self) -> u64 {
        self.columns.iter().map(|c| (c.top - c.bed) as u64).sum()
    }

    /// Every full water cell, in `(z, x, y)` order.
    pub fn cells(&self) -> impl Iterator<Item = GlobalCell> + '_ {
        self.columns.iter().flat_map(|c| {
            (c.bed + 1..=c.top)
                .map(move |y| GlobalCell::new(i64::from(c.x), i64::from(y), i64::from(c.z)))
        })
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
    /// The surface height caves measure their roof from: the column's own
    /// height for the showcase, the lowest surface within [`ROOF_WINDOW`]
    /// columns for natural worlds, whose cliffs would otherwise leave roofs
    /// hanging free.
    cave_roof: Vec<i32>,
    stacks: Vec<Stack>,
    lowland: Vec<u8>,
    hmin: i64,
    hmax: i64,
}

/// Horizontal reach, in columns, of the natural preset's cave roof rule.
const ROOF_WINDOW: i64 = 6;

/// Lowest surface within [`ROOF_WINDOW`] columns of every column of a footprint.
fn windowed_roof(columns: &ColumnMap, x0: i64, z0: i64) -> Vec<i32> {
    let side = BRICK + 2 * ROOF_WINDOW;
    // Minimum along x for every row of the padded footprint, then along z.
    let mut rows = vec![0i32; (side * BRICK) as usize];
    for pz in 0..side {
        for lx in 0..BRICK {
            let z = z0 - ROOF_WINDOW + pz;
            rows[(pz * BRICK + lx) as usize] = (-ROOF_WINDOW..=ROOF_WINDOW)
                .map(|dx| columns.height(x0 + lx + dx, z))
                .min()
                .unwrap_or(0);
        }
    }
    let mut out = Vec::with_capacity((BRICK * BRICK) as usize);
    for lz in 0..BRICK {
        for lx in 0..BRICK {
            out.push(
                (0..=2 * ROOF_WINDOW)
                    .map(|k| rows[((lz + k) * BRICK + lx) as usize])
                    .min()
                    .unwrap_or(0),
            );
        }
    }
    out
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
    let cave_roof = if columns.is_natural() {
        windowed_roof(columns, bx * BRICK, bz * BRICK)
    } else {
        heights.clone()
    };
    Footprint {
        x0: bx * BRICK,
        z0: bz * BRICK,
        heights,
        cave_roof,
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
                        || (nat > 0.0
                            && CaveField::natural_allowed(
                                y,
                                i64::from(fp.cave_roof[col]),
                                fp.lowland[col],
                            ))
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
    let mut wet = Vec::new();
    let mut lo = [i64::MAX; 3];
    let mut hi = [i64::MIN; 3];
    for z in 0..size {
        for x in 0..size {
            if !columns.is_water(x, z) {
                continue;
            }
            let h = i64::from(columns.height(x, z));
            let top = i64::from(columns.water_surface(x, z));
            wet.push(WetColumn {
                x: x as i32,
                z: z as i32,
                bed: h as i32,
                top: top as i32,
            });
            lo = [lo[0].min(x), lo[1].min(h + 1), lo[2].min(z)];
            hi = [hi[0].max(x), hi[1].max(top), hi[2].max(z)];
        }
    }
    let bounds = (!wet.is_empty()).then(|| {
        (
            GlobalCell::new(lo[0], lo[1], lo[2]),
            GlobalCell::new(hi[0], hi[1], hi[2]),
        )
    });
    let cell = |p: &[i64; 3]| GlobalCell::new(p[0], p[1], p[2]);
    WaterPlan {
        surface_y: i64::from(SEA_LEVEL),
        columns: wet,
        bounds,
        springs: columns
            .springs()
            .iter()
            .map(|(p, flow)| Spring {
                cell: cell(p),
                flow_cells3_s: *flow,
            })
            .collect(),
        sinks: columns.sinks().iter().map(cell).collect(),
    }
}

/// Wet columns and the regions they fall in, counted from the column map the
/// way [`water_plan`] fills it, without allocating the plan.
fn wet_extent(columns: &ColumnMap) -> (u64, u64) {
    let size = i64::from(columns.size());
    let mut regions = std::collections::BTreeSet::new();
    let mut cells = 0u64;
    for z in 0..size {
        for x in 0..size {
            if !columns.is_water(x, z) {
                continue;
            }
            let h = i64::from(columns.height(x, z));
            cells += (i64::from(columns.water_surface(x, z)) - h) as u64;
            regions.insert((
                x.div_euclid(WATER_REGION_EDGE),
                z.div_euclid(WATER_REGION_EDGE),
            ));
        }
    }
    (regions.len() as u64, cells)
}

fn check_water_budget(regions: u64) -> Result<(), GenError> {
    if regions > WATER_REGION_BUDGET {
        return Err(GenError::WaterBudget {
            what: "regions",
            measured: regions,
            budget: WATER_REGION_BUDGET,
        });
    }
    Ok(())
}

/// Checks the water capacity before allocating terrain: the wet 32x32 regions,
/// not the water's bounding box.
pub fn validate_water_budget(columns: &ColumnMap) -> Result<(), GenError> {
    check_water_budget(wet_extent(columns).0)
}

/// Dry spots with a flat 5x5 footprint, clear of cave mouth tunnels. In a
/// natural world they sit a short walk from a river or lake (2 to 20 m) for a
/// good first view, falling back to any flat dry spot if none does.
fn find_spawns(columns: &ColumnMap, caves: &CaveField) -> Vec<[f64; 3]> {
    if columns.is_gallery() {
        // One spawn per dry pad, at its middle (the pond's pad a little aside).
        return crate::columns::gallery_patches(columns.size())
            .iter()
            .filter(|p| p.biome != Biome::Ocean)
            .map(|p| {
                let (mut x, z) = p.centre();
                if p.biome == Biome::Riparian {
                    x = p.x0 + (p.x1 - p.x0) / 6;
                }
                [
                    x as f64 * CELL_M + CELL_M / 2.0,
                    (i64::from(columns.height(x, z)) + 1) as f64 * CELL_M,
                    z as f64 * CELL_M + CELL_M / 2.0,
                ]
            })
            .collect();
    }
    if columns.is_natural() {
        let near_water = search_spawns(columns, caves, true);
        if !near_water.is_empty() {
            return near_water;
        }
    }
    search_spawns(columns, caves, false)
}

fn search_spawns(columns: &ColumnMap, caves: &CaveField, near_water: bool) -> Vec<[f64; 3]> {
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
                            matches!(
                                columns.biome(x + ox, z + oz),
                                Biome::Meadow | Biome::TemperateForest | Biome::Steppe
                            ) && columns.lowland(x + ox, z + oz) == 0
                                && (columns.height(x + ox, z + oz) - h).abs() <= 1
                        })
                    });
                    // The ground under a spawn must not be a cave mouth.
                    let clear_of_mouths =
                        caves.mouth_distance_xz(x as f64, z as f64) > MOUTH_RADIUS + 6.0;
                    let water_ok = !near_water || (8..=80).contains(&columns.river_dist(x, z));
                    if flat && h > SEA_LEVEL + 8 && clear_of_mouths && water_ok {
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
    check_water_budget(water.wet_regions())?;
    let cave_mouths = caves.mouth_sites();
    let spawns = find_spawns(&columns, &caves);
    if spawns.is_empty() {
        return Err(GenError::NoSpawn);
    }
    let edge = i64::from(spec.size_cells);
    Ok((
        GeneratedWorld {
            version: spec.preset.gen_version(),
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
        for (preset, height) in [
            (Preset::Showcase, 12),
            (Preset::Showcase, 16),
            (Preset::Natural, 16),
        ] {
            let spec = WorldGenSpec::new(preset, 1, 4096, WorldgenPalette::sequential(1))
                .with_height_bricks(height);
            let name = preset.name();
            let started = std::time::Instant::now();
            let (world, t) = match generate_with_timings(&spec) {
                Ok(done) => done,
                Err(error) => {
                    eprintln!("gen_probe {name} height_bricks={height} FAILED: {error}");
                    continue;
                }
            };
            let total = started.elapsed();
            eprintln!(
                "gen_probe {name} height_bricks={height} total_ms={} columns_ms={} fill_ms={} packing_ms={} insert_ms={} water_ms={}",
                total.as_millis(),
                t.columns.as_millis(),
                t.fill.as_millis(),
                t.packing.as_millis(),
                t.insert.as_millis(),
                t.water_and_spawns.as_millis()
            );
            eprintln!(
                "gen_probe {name} height_bricks={height} digest={} bricks={} memory={:?} wet_regions={} wet_cells={}",
                spall_voxel::fixtures::digest_hex(&world.terrain),
                world.terrain.resident_brick_count(),
                world.terrain.memory_report(),
                world.water.wet_regions(),
                world.water.cell_count()
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
        for preset in [Preset::Showcase, Preset::Natural] {
            for seed in [1, 7, 42] {
                let columns = ColumnMap::compute(&WorldGenSpec::new(
                    preset,
                    seed,
                    512,
                    WorldgenPalette::sequential(1),
                ))
                .unwrap();
                let plan = water_plan(&columns);
                assert_eq!(
                    wet_extent(&columns),
                    (plan.wet_regions(), plan.cell_count()),
                    "{preset:?} seed {seed}"
                );
                assert_eq!(plan.cells().count() as u64, plan.cell_count());
                validate_water_budget(&columns).unwrap();
            }
        }
    }

    #[test]
    fn budget_failures_name_what_was_exceeded() {
        assert!(check_water_budget(WATER_REGION_BUDGET).is_ok());
        assert!(matches!(
            check_water_budget(WATER_REGION_BUDGET + 1),
            Err(GenError::WaterBudget {
                what: "regions",
                ..
            })
        ));
    }

    #[test]
    #[ignore = "large-world capacity regression; computes 4096-cell column maps only"]
    fn large_world_water_fits_the_region_budget() {
        for preset in [Preset::Showcase, Preset::Natural] {
            for seed in 1..=12 {
                let spec = WorldGenSpec::new(preset, seed, 4096, WorldgenPalette::sequential(1));
                let columns = ColumnMap::compute(&spec).unwrap();
                let (regions, cells) = wet_extent(&columns);
                eprintln!(
                    "capacity: {preset:?} seed={seed} wet_regions={regions} wet_cells={cells} springs={} sinks={} ok={}",
                    columns.springs().len(),
                    columns.sinks().len(),
                    validate_water_budget(&columns).is_ok()
                );
            }
        }
    }
}
