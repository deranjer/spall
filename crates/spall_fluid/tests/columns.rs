//! ENG-165 (W1) acceptance tests for the stacked-column water solver.
//! All fixtures are tiny headless CPU worlds.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::columns::{CELL_SIZE_M, ColumnConfig, ColumnWorld};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

/// Relative volume tolerance for conservation tests. Volumes are f64 and every
/// pipe applies the same f32 flow to both ends, so error is f64 rounding only.
const CONSERVATION_TOLERANCE: f64 = 1.0e-9;

fn terrain(dims: [i64; 3], solid: impl Fn(i64, i64, i64) -> bool) -> SolidBoundary {
    let spec = DomainSpec::new(
        GlobalCell::new(0, 0, 0),
        dims.map(|d| d as u32),
        (dims[0] * dims[1] * dims[2]) as usize,
    )
    .unwrap();
    let id = VolumeId::new(0x165).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..=(dims[2] - 1) / 32 {
        for by in 0..=(dims[1] - 1) / 32 {
            for bx in 0..=(dims[0] - 1) / 32 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                if solid(x, y, z) {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    SolidBoundary::capture(&volume, spec).unwrap()
}

fn walls(dims: [i64; 3], x: i64, z: i64) -> bool {
    x == 0 || z == 0 || x == dims[0] - 1 || z == dims[2] - 1
}

const DT: f64 = 1.0 / 20.0;

fn run(world: &mut ColumnWorld, seconds: f64) {
    for _ in 0..(seconds / DT).round() as u32 {
        world.step(DT);
    }
}

/// Runs until the flow has died away or the time budget is spent; returns the
/// simulated time used.
fn run_to_rest(world: &mut ColumnWorld, max_seconds: f64) -> f64 {
    let mut t = 0.0;
    while t < max_seconds {
        run(world, 1.0);
        t += 1.0;
        if world.max_abs_flow() < 1.0e-4 {
            break;
        }
    }
    t
}

fn assert_conserved(world: &ColumnWorld, expected: f64) {
    let err = (world.total_volume() - expected).abs();
    assert!(
        err <= CONSERVATION_TOLERANCE * expected.max(1.0),
        "volume drifted by {err} cells^3 of {expected}"
    );
}

fn level_at(world: &ColumnWorld, x: i64, z: i64) -> f64 {
    let spans = world.spans_in_column(x, z);
    spans.last().map(|s| s.floor_y as f64 + s.depth).unwrap()
}

/// Most surface heights below `level` of all open columns with water.
fn surface_spread(world: &ColumnWorld, dims: [i64; 3]) -> (f64, f64) {
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    for z in 0..dims[2] {
        for x in 0..dims[0] {
            for s in world.spans_in_column(x, z) {
                if s.depth > 0.05 {
                    let level = s.floor_y as f64 + s.depth;
                    lo = lo.min(level);
                    hi = hi.max(level);
                }
            }
        }
    }
    (lo, hi)
}

fn stair_floor(x: i64, z: i64) -> i64 {
    1 + (x / 5) % 3 + z / 8
}

fn stair_world() -> (ColumnWorld, [i64; 3]) {
    let dims = [40, 16, 24];
    let boundary = terrain(dims, |x, y, z| walls(dims, x, z) || y < stair_floor(x, z));
    let mut world = ColumnWorld::new(&boundary, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..dims[0] - 1 {
            let (dx, dz) = ((x - 12) as f64, (z - 10) as f64);
            if dx * dx + dz * dz < 40.0 {
                world.fill_column_to(x, z, 11.0);
            }
        }
    }
    (world, dims)
}

/// Anti-wall gate: after settling on stair-stepped terrain no wetted connection
/// has a head difference above 0.1 cell.
#[test]
fn irregular_blob_on_stairs_settles_flat() {
    let (mut world, _) = stair_world();
    let start = world.total_volume();
    assert!(start > 100.0);
    let t = run_to_rest(&mut world, 240.0);
    let diff = world.max_wetted_head_difference();
    println!(
        "settled after {t} s, head difference {diff}, flow {}",
        world.max_abs_flow()
    );
    assert!(diff <= 0.1, "wetted head difference {diff} after {t} s");
    assert_conserved(&world, start);
}

/// A wide pool released against a wall with no divider: the case grid_mac walls
/// up (docs/reports/ENG-164.md). Same geometry, 40x20x12 cells, floor 4 thick.
#[test]
fn eng164_released_pool_levels_out() {
    let dims = [40, 20, 12];
    let boundary = terrain(dims, |x, y, z| walls(dims, x, z) || y < 4);
    let mut world = ColumnWorld::new(&boundary, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..=12 {
            world.fill_column_to(x, z, 12.0);
        }
    }
    let start = world.total_volume();
    let t = run_to_rest(&mut world, 120.0);
    let (lo, hi) = surface_spread(&world, dims);
    println!("rest after {t} s, surfaces {lo}..{hi}");
    assert!(world.max_wetted_head_difference() <= 0.1);
    assert!(hi - lo <= 0.1, "surfaces span {lo}..{hi}");
    assert!((hi - 6.5).abs() < 0.2, "level {hi}, expected about 6.5");
    assert_conserved(&world, start);
}

/// Dam removal produces a surge; it ends with equal levels.
#[test]
fn dam_break_surges_then_levels() {
    let dims = [48, 16, 8];
    let dam = |x: i64, y: i64, _z: i64| x == 16 && y >= 1;
    let closed = terrain(dims, |x, y, z| walls(dims, x, z) || y < 1 || dam(x, y, z));
    let open = terrain(dims, |x, y, z| walls(dims, x, z) || y < 1);
    let mut world = ColumnWorld::new(&closed, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..16 {
            world.fill_column_to(x, z, 9.0);
        }
    }
    let start = world.total_volume();
    run(&mut world, 3.0);
    assert!(world.max_abs_flow() < 1.0e-6, "dam must hold");
    assert!(world.column_volume(30, 3) == 0.0);

    world.apply_boundary(&open).unwrap();
    let mut peak = 0.0f32;
    let mut arrival = None;
    for step in 0..(60.0 / DT) as u32 {
        let m = world.step(DT);
        peak = peak.max(m.peak_speed_m_s);
        if arrival.is_none() && world.column_volume(40, 3) > 0.05 {
            arrival = Some(f64::from(step) * DT);
        }
    }
    println!("peak speed {peak} m/s, front reaches x=40 after {arrival:?} s");
    let wave_speed = (9.81 * 2.0f64).sqrt(); // sqrt(g * 8 cells) in m/s
    assert!(
        peak > 0.5 * wave_speed as f32 && (peak as f64) < 3.0 * wave_speed,
        "peak {peak} vs wave speed {wave_speed}"
    );
    assert!(arrival.is_some(), "surge must reach downstream");
    let t = run_to_rest(&mut world, 240.0);
    let (lo, hi) = surface_spread(&world, dims);
    println!("rest after another {t} s, surfaces {lo}..{hi}");
    assert!(hi - lo <= 0.15, "levels {lo}..{hi}");
    assert_conserved(&world, start);
}

/// A dug channel levels two pools; refilling it stops exchange without losing
/// water.
#[test]
fn canal_levels_pools_and_refill_stops_exchange() {
    let dims = [40, 16, 9];
    let wall = |x: i64| x == 20;
    let closed = terrain(dims, |x, y, z| walls(dims, x, z) || y < 1 || wall(x));
    // Channel through the wall at z=4, from the floor up.
    let canal = |x: i64, y: i64, z: i64| wall(x) && y >= 1 && z == 4;
    let dug = terrain(dims, |x, y, z| {
        walls(dims, x, z) || y < 1 || (wall(x) && !canal(x, y, z))
    });
    let mut world = ColumnWorld::new(&closed, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..20 {
            world.fill_column_to(x, z, 9.0);
        }
        for x in 21..39 {
            world.fill_column_to(x, z, 4.0);
        }
    }
    let start = world.total_volume();
    run(&mut world, 3.0);
    assert!((level_at(&world, 10, 4) - 9.0).abs() < 1.0e-3);
    assert!((level_at(&world, 30, 4) - 4.0).abs() < 1.0e-3);

    world.apply_boundary(&dug).unwrap();
    run_to_rest(&mut world, 300.0);
    let (lo, hi) = surface_spread(&world, dims);
    println!("after canal: surfaces {lo}..{hi}");
    assert!(hi - lo <= 0.15, "pools did not level: {lo}..{hi}");
    assert_conserved(&world, start);

    // Fill the channel back in: the water inside it is displaced, kept, and
    // the pools stop trading.
    world.apply_boundary(&closed).unwrap();
    assert_conserved(&world, start);
    run_to_rest(&mut world, 60.0);
    assert_conserved(&world, start);
    let left = |w: &ColumnWorld| {
        (1..20)
            .flat_map(|x| (1..8).map(move |z| (x, z)))
            .map(|(x, z)| w.column_volume(x, z))
            .sum::<f64>()
    };
    let left_before = left(&world);
    run(&mut world, 20.0);
    let left_after = left(&world);
    assert!(
        (left_after - left_before).abs() < 1.0e-6,
        "exchange continued: {left_before} -> {left_after}"
    );
    assert!(world.max_abs_flow() < 1.0e-3);
}

/// A one-cell wall, crossing the 32-cell brick boundary, holds indefinitely.
#[test]
fn one_cell_wall_across_a_brick_boundary_holds() {
    let dims = [64, 12, 8];
    let wall = |x: i64| x == 32;
    let boundary = terrain(dims, |x, y, z| walls(dims, x, z) || y < 1 || wall(x));
    let mut world = ColumnWorld::new(&boundary, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..32 {
            world.fill_column_to(x, z, 9.0);
        }
    }
    let start = world.total_volume();
    run(&mut world, 120.0);
    let right: f64 = (33..63)
        .flat_map(|x| (1..7).map(move |z| (x, z)))
        .map(|(x, z)| world.column_volume(x, z))
        .sum();
    assert_eq!(right, 0.0, "water crossed an intact wall");
    assert_conserved(&world, start);
}

/// A flooded tunnel beneath a river: both are separate spans in the same
/// column and the tunnel stays full under the lake head.
#[test]
fn flooded_tunnel_under_a_river_stays_full() {
    let dims = [28, 16, 7];
    // Lake x=1..8 (open from floor). x=8..24 tunnel at z=3: open y2..4, roof
    // y4..6 solid, river open above y>=6. Everything off z=3 is solid there.
    let solid = |x: i64, y: i64, z: i64| {
        if walls(dims, x, z) || y < 2 {
            return true;
        }
        if (8..24).contains(&x) {
            return z != 3 || (4..6).contains(&y);
        }
        x >= 24
    };
    let boundary = terrain(dims, solid);
    let mut world = ColumnWorld::new(&boundary, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..8 {
            world.fill_column_to(x, z, 9.0);
        }
    }
    let start = world.total_volume();
    assert_eq!(world.spans_in_column(15, 3).len(), 2, "tunnel and river");
    run_to_rest(&mut world, 240.0);
    let spans = world.spans_in_column(15, 3);
    let (tunnel, river) = (spans[0], spans[1]);
    println!(
        "tunnel depth {} / 2, river depth {}, head {}, lake level {}, flow {}, total {} / {}",
        tunnel.depth,
        river.depth,
        tunnel.head_y,
        level_at(&world, 4, 3),
        world.max_abs_flow(),
        world.total_volume(),
        start
    );
    assert!(tunnel.depth > 1.999, "tunnel not full: {}", tunnel.depth);
    // 51 L = 379 by volume balance: lake 35 (L-2) + tunnel 32 + river 16 (L-6).
    let lake = level_at(&world, 4, 3);
    assert!((lake - 379.0 / 51.0).abs() < 0.02, "lake level {lake}");
    assert!(
        (river.floor_y as f64 + river.depth - lake).abs() < 0.02,
        "river surface {} vs lake {lake}",
        river.floor_y as f64 + river.depth
    );
    assert!(world.max_wetted_head_difference() < 0.2);
    assert_conserved(&world, start);
}

/// Two shafts joined by a flooded tunnel equalise approximately.
#[test]
fn u_tube_equalises() {
    let dims = [12, 24, 4];
    let solid = |x: i64, y: i64, z: i64| {
        if walls(dims, x, z) || y < 2 {
            return true;
        }
        if z != 1 && z != 2 {
            return true;
        }
        // shafts at x=2 and x=9, tunnel y2..3 between, roof above it.
        !(x == 2 || x == 9 || ((3..9).contains(&x) && y == 2))
    };
    let boundary = terrain(dims, solid);
    let mut world = ColumnWorld::new(&boundary, ColumnConfig::default());
    world.fill_column_to(2, 1, 14.0);
    world.fill_column_to(2, 2, 14.0);
    for x in 3..9 {
        world.fill_column_to(x, 1, 3.0);
        world.fill_column_to(x, 2, 3.0);
    }
    let start = world.total_volume();
    run_to_rest(&mut world, 300.0);
    let (l, r) = (level_at(&world, 2, 1), level_at(&world, 9, 1));
    println!("shaft levels {l} / {r}");
    assert!((l - r).abs() < 0.3, "shafts {l} vs {r}");
    assert_conserved(&world, start);
}

/// Flow, terrain edits that cut spans, and displacement all keep Σ volume.
#[test]
fn conservation_through_flow_edits_and_displacement() {
    let dims = [32, 16, 12];
    let base = |x: i64, y: i64, z: i64| walls(dims, x, z) || y < 1;
    let open = terrain(dims, base);
    let mut world = ColumnWorld::new(&open, ColumnConfig::default());
    for z in 1..dims[2] - 1 {
        for x in 1..10 {
            world.fill_column_to(x, z, 8.0);
        }
    }
    let start = world.total_volume();
    run(&mut world, 2.0);
    assert_conserved(&world, start);

    // Place a block into the water (displaces), a roof over part of the pool
    // (splits columns), then bury one column entirely.
    type Edit = Box<dyn Fn(i64, i64, i64) -> bool>;
    let edits: [Edit; 3] = [
        Box::new(|x, y, z| (4..6).contains(&x) && (4..6).contains(&z) && (2..4).contains(&y)),
        Box::new(|x, y, z| (6..12).contains(&x) && (3..9).contains(&z) && y == 5),
        Box::new(|x, _y, z| x == 3 && z == 3),
    ];
    for k in 1..=3 {
        let boundary = terrain(dims, |x, y, z| {
            base(x, y, z) || edits[..k].iter().any(|e| e(x, y, z))
        });
        let report = world.apply_boundary(&boundary).unwrap();
        println!("edit {k}: {report:?}");
        assert_conserved(&world, start);
        run(&mut world, 3.0);
        assert_conserved(&world, start);
    }
    run_to_rest(&mut world, 120.0);
    assert_conserved(&world, start);
}

fn fingerprint(world: &ColumnWorld) -> Vec<u64> {
    let mut out = Vec::new();
    for z in 0..24 {
        for x in 0..40 {
            for s in world.spans_in_column(x, z) {
                out.push(s.volume.to_bits());
            }
        }
    }
    out
}

/// Results are bit-identical across runs and thread counts.
#[test]
fn identical_across_threads_and_runs() {
    let reference = {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let (mut w, _) = stair_world();
            run(&mut w, 10.0);
            fingerprint(&w)
        })
    };
    for threads in [1, 2, 4, 7] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let again = pool.install(|| {
            let (mut w, _) = stair_world();
            run(&mut w, 10.0);
            fingerprint(&w)
        });
        assert_eq!(again, reference, "{threads} threads diverged");
    }
    let _ = CELL_SIZE_M;
}
