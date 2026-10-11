//! ENG-166 (W2) tests: sparse regions, sleep and wake, frozen boundaries.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use spall_core::GlobalCell;
use spall_fluid::columns::{
    ColumnConfig, ColumnWorld, REGION_EDGE, RegionKey, RegionTerrain, TerrainSource,
};

const HEIGHT: i32 = 16;
const DT: f64 = 1.0 / 20.0;

/// Flat world: solid below y=1, open above, with optional extra solids.
struct Flat {
    missing: Mutex<BTreeSet<RegionKey>>,
    wall: Box<dyn Fn(i64, i64, i64) -> bool + Send + Sync>,
}

impl Flat {
    fn new(wall: impl Fn(i64, i64, i64) -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            missing: Mutex::new(BTreeSet::new()),
            wall: Box::new(wall),
        })
    }
}

impl TerrainSource for Flat {
    fn y_range(&self) -> (i32, i32) {
        (0, HEIGHT)
    }

    fn region(&self, key: RegionKey) -> Option<RegionTerrain> {
        if self.missing.lock().unwrap().contains(&key) {
            return None;
        }
        let h = HEIGHT as i64;
        let mut solid = vec![false; (REGION_EDGE * REGION_EDGE * h) as usize];
        for lz in 0..REGION_EDGE {
            for y in 0..h {
                for lx in 0..REGION_EDGE {
                    let (x, z) = (
                        i64::from(key.0) * REGION_EDGE + lx,
                        i64::from(key.1) * REGION_EDGE + lz,
                    );
                    solid[(lx + REGION_EDGE * (y + h * lz)) as usize] =
                        y < 1 || (self.wall)(x, y, z);
                }
            }
        }
        Some(RegionTerrain { solid })
    }
}

fn run(world: &mut ColumnWorld, seconds: f64) -> u32 {
    let mut worked = 0;
    for _ in 0..(seconds / DT).round() as u32 {
        if world.step(DT).substeps > 0 {
            worked += 1;
        }
    }
    worked
}

/// Steps until every region sleeps (or `max_seconds` pass); returns seconds used.
fn run_until_asleep(world: &mut ColumnWorld, max_seconds: f64) -> f64 {
    let mut t = 0.0;
    while world.active_region_count() > 0 && t < max_seconds {
        run(world, 5.0);
        t += 5.0;
    }
    t
}

fn pool_cells(x0: i64, x1: i64, z0: i64, z1: i64, depth: i64) -> Vec<GlobalCell> {
    let mut cells = Vec::new();
    for z in z0..z1 {
        for x in x0..x1 {
            for y in 1..1 + depth {
                cells.push(GlobalCell::new(x, y, z));
            }
        }
    }
    cells
}

/// Only regions with water, plus their neighbours, exist; a world at rest costs
/// nothing.
#[test]
fn allocation_is_sparse_and_a_resting_world_costs_nothing() {
    let terrain = Flat::new(|_, _, _| false);
    let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
    // Equal surface at depth 3 over a 40 x 40 patch (spans 2x2 regions).
    let rejected = world.seed_cells(&pool_cells(10, 50, 10, 50, 3));
    assert!(rejected.is_empty());
    // Water in regions (0..=1, 0..=1) plus the ring of 8 around them: 4 + 8.
    assert_eq!(world.region_count(), 12, "water regions plus their ring");
    assert_eq!(world.active_region_count(), 0, "seeded at rest, asleep");
    let m = world.step(DT);
    assert_eq!((m.substeps, m.active_regions), (0, 0));
    assert!(m.active_pipes == 0 && m.active_spans == 0);
}

/// A flat pool released against nothing is not at rest: it spreads across
/// region borders, allocating new regions, with volume conserved.
#[test]
fn spreading_water_allocates_regions_and_conserves_volume() {
    let terrain = Flat::new(|_, _, _| false);
    let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
    world.seed_cells(&pool_cells(28, 36, 28, 36, 6));
    let start = world.total_volume();
    let regions0 = world.region_count();
    world.wake_all();
    let mut woke = 0;
    // Spreading across a border takes as long as the water takes to get there.
    for _ in 0..(300.0 / DT) as u32 {
        woke += world.step(DT).regions_woken;
        if world.region_count() > regions0 {
            break;
        }
    }
    println!(
        "regions {regions0} -> {}, woken {woke}, active {}",
        world.region_count(),
        world.active_region_count()
    );
    assert!(world.region_count() > regions0, "ring must grow with water");
    assert!((world.total_volume() - start).abs() < 1.0e-9 * start);
}

/// Quiet regions sleep, and flow across a border wakes the neighbour.
#[test]
fn quiet_regions_sleep_and_border_flow_wakes_a_neighbour() {
    // A closed box x 9..=64, z 4..=20 split by a divider at x=31 that gets a
    // one-cell gap later.
    let gap = Arc::new(Mutex::new(false));
    let g2 = gap.clone();
    let terrain = Flat::new(move |x, _y, z| {
        let edge = (9..=64).contains(&x)
            && (4..=20).contains(&z)
            && (x == 9 || x == 64 || z == 4 || z == 20);
        edge || (x == 31 && (4..=20).contains(&z) && !(*g2.lock().unwrap() && z == 10))
    });
    let mut world = ColumnWorld::with_terrain(terrain.clone(), ColumnConfig::default());
    world.seed_cells(&pool_cells(10, 31, 5, 20, 4));
    let start = world.total_volume();
    assert!(
        world.is_region_allocated((1, 0)),
        "ring includes the far side"
    );
    world.wake_all();
    run_until_asleep(&mut world, 600.0);
    assert_eq!(
        world.active_region_count(),
        0,
        "everything settles and sleeps"
    );
    assert!(world.column_volume(40, 10) == 0.0);

    // Open the wall (terrain edit) in region 0: it wakes, the neighbour is woken
    // by the flow crossing the border or by the edit's neighbour wake.
    *gap.lock().unwrap() = true;
    world.refresh_regions(&[(0, 0)]);
    assert!(world.is_region_active((0, 0)));
    run(&mut world, 5.0);
    assert!(world.is_region_active((1, 0)), "neighbour must wake");
    // Equalising through a one-cell gap is slow; allow it to finish.
    let mut t = 0.0;
    while world.active_region_count() > 0 && t < 1800.0 {
        run(&mut world, 10.0);
        t += 10.0;
    }
    println!("pools equalised and slept after {t} s");
    assert!(world.column_volume(40, 10) > 0.0, "water crossed the gap");
    assert!((world.total_volume() - start).abs() < 1.0e-9 * start);
    assert_eq!(world.active_region_count(), 0, "and sleeps again");
    let (left, right) = (
        world.spans_in_column(20, 10)[0],
        world.spans_in_column(50, 10)[0],
    );
    assert!(
        (left.head_y - right.head_y).abs() < 0.05,
        "levels {} vs {}",
        left.head_y,
        right.head_y
    );
}

/// Border flow alone wakes a sleeping neighbour (no edit involved).
#[test]
fn flow_across_a_border_wakes_a_sleeping_region() {
    let terrain = Flat::new(|x, _, z| {
        (19..=45).contains(&x) && (9..=13).contains(&z) && (x == 19 || x == 45 || z == 9 || z == 13)
    });
    let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
    world.seed_cells(&pool_cells(20, 31, 10, 12, 1));
    world.wake_all();
    run_until_asleep(&mut world, 600.0);
    assert_eq!(world.active_region_count(), 0);
    // A source dumps water at the border: only region (0,0) wakes by itself.
    world.add_water(GlobalCell::new(30, 1, 10), 20.0).unwrap();
    assert!(world.is_region_active((0, 0)));
    assert!(!world.is_region_active((1, 0)));
    let mut woken = 0;
    for _ in 0..40 {
        woken += world.step(DT).regions_woken;
    }
    assert!(
        woken > 0,
        "the neighbour must have been woken by border flow"
    );
    assert!(
        world.column_volume(33, 10) > 0.0,
        "water crossed the border"
    );
}

/// A non-resident neighbour is a closed wall: water neither drains into it nor
/// stops being counted. When it returns, both sides exchange.
#[test]
fn non_resident_region_is_a_closed_frozen_boundary() {
    let terrain = Flat::new(|_, _, _| false);
    terrain.missing.lock().unwrap().insert((1, 0));
    let mut world = ColumnWorld::with_terrain(terrain.clone(), ColumnConfig::default());
    // A pool against the missing region's border.
    world.seed_cells(&pool_cells(20, 32, 8, 24, 5));
    let start = world.total_volume();
    assert!(!world.is_region_allocated((1, 0)));
    world.wake_all();
    run(&mut world, 60.0);
    assert!(!world.is_region_allocated((1, 0)), "still absent");
    assert!(
        (world.total_volume() - start).abs() < 1.0e-9 * start,
        "nothing drained"
    );
    // The edge column stays as deep as its neighbours: a wall, not a drain.
    let edge = world.column_volume(31, 12);
    let inner = world.column_volume(27, 12);
    assert!((edge - inner).abs() < 0.05, "edge {edge} inner {inner}");

    terrain.missing.lock().unwrap().clear();
    world.notify_resident(&[(1, 0)]);
    assert!(world.is_region_allocated((1, 0)));
    assert!(world.is_region_active((0, 0)) && world.is_region_active((1, 0)));
    run(&mut world, 30.0);
    assert!(world.column_volume(35, 12) > 0.0, "water crossed into it");
    assert!((world.total_volume() - start).abs() < 1.0e-9 * start);
}

/// Freezing an allocated region stops its water without losing it.
#[test]
fn frozen_region_keeps_its_water_and_closes_its_borders() {
    let terrain = Flat::new(|_, _, _| false);
    let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
    world.seed_cells(&pool_cells(20, 44, 8, 24, 4));
    let start = world.total_volume();
    world.wake_all();
    world.freeze_regions(&[(1, 0)]);
    let frozen_before = (32..44).map(|x| world.column_volume(x, 12)).sum::<f64>();
    run(&mut world, 20.0);
    let frozen_after = (32..44).map(|x| world.column_volume(x, 12)).sum::<f64>();
    assert_eq!(frozen_before, frozen_after, "frozen water does not move");
    assert!((world.total_volume() - start).abs() < 1.0e-9 * start);
}

/// Wake and region growth give bit-identical results at any thread count.
#[test]
fn sparse_stepping_is_bit_identical_across_thread_counts() {
    fn scenario() -> Vec<u64> {
        let terrain = Flat::new(|x, _, z| (x * 7 + z * 3) % 29 == 0 && x % 5 != 0);
        let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
        world.seed_cells(&pool_cells(24, 40, 24, 40, 5));
        world.wake_all();
        run(&mut world, 12.0);
        let mut out = vec![world.region_count() as u64];
        for z in 0..72 {
            for x in 0..72 {
                for s in world.spans_in_column(x, z) {
                    out.push(s.volume.to_bits());
                }
            }
        }
        out
    }
    let pool1 = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let reference = pool1.install(scenario);
    for n in [2, 4, 7] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .unwrap();
        assert_eq!(pool.install(scenario), reference, "{n} threads diverged");
    }
}

/// A terrain edit in one region rebuilds only the pipes it touches, yet the
/// result is the pipe set a full rebuild would give, the flow of every
/// surviving pipe is kept, and water is conserved. Also after regions were
/// appended, which leaves the old pipes ungrouped by owner.
#[test]
fn refreshing_a_region_keeps_pipes_and_flow_equal_to_a_full_rebuild() {
    for appended in [false, true] {
        let wall = Arc::new(Mutex::new(false));
        let w2 = wall.clone();
        let terrain = Flat::new(move |x, y, z| {
            // A dam across the pool at x = 40 that an edit lowers, so spans in
            // region (1, 0) change shape.
            let dam =
                x == 40 && (2..=30).contains(&z) && y < if *w2.lock().unwrap() { 3 } else { 6 };
            dam || (x == 5 || x == 120) && (2..=30).contains(&z)
        });
        let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
        world.seed_cells(&pool_cells(6, 40, 3, 30, 5));
        if appended {
            // Regions added after the first build are appended, not interleaved.
            world.allocate_rect(70, 0, 110, 31);
        }
        world.wake_all();
        run(&mut world, 3.0);
        assert!(world.max_abs_flow() > 0.0, "water is moving");
        assert!(world.pipes_match_full_rebuild());
        let before = world.total_volume();

        *wall.lock().unwrap() = true;
        world.refresh_regions(&[(1, 0)]);
        assert!(
            world.pipes_match_full_rebuild(),
            "appended={appended}: incremental pipes differ from a full rebuild"
        );
        assert!(
            world.max_abs_flow() > 0.0,
            "appended={appended}: flow survived the refresh"
        );
        assert!((world.total_volume() - before).abs() < 1.0e-9 * before);
        run(&mut world, 3.0);
        assert!(world.pipes_match_full_rebuild());
        assert!((world.total_volume() - before).abs() < 1.0e-9 * before);
    }
}

/// Water released into an empty channel runs down it: the velocity of the
/// water mid-channel points the way it flows, along the channel only, and
/// standing water at rest has none.
#[test]
fn flow_at_a_point_follows_the_water_and_is_zero_at_rest() {
    // A channel 5 columns wide along +x, walled on both long sides and the ends.
    let terrain = Flat::new(|x, _y, z| {
        let wall_z = z == 9 || z == 15;
        (wall_z && (9..=60).contains(&x)) || ((x == 9 || x == 60) && (9..=15).contains(&z))
    });
    let mut world = ColumnWorld::with_terrain(terrain, ColumnConfig::default());
    // Water banked against the west end, 6 cells deep.
    world.seed_cells(&pool_cells(10, 16, 10, 15, 6));
    world.wake_all();
    run(&mut world, 1.0);
    let (x_m, z_m) = (20.0 * 0.25, 12.5 * 0.25);
    let at = |w: &ColumnWorld, x: f64| w.flow_at_m(x, 0.75, z_m);
    let moving = world
        .flow_at_m(x_m + 0.125, 0.75, z_m)
        .or_else(|| (14..40).find_map(|c| at(&world, f64::from(c) * 0.25 + 0.125)))
        .expect("the released water has reached the channel");
    assert!(moving[0] > 0.2, "water runs down the channel: {moving:?}");
    assert!(
        moving[1].abs() < 0.1 * moving[0],
        "and not across it: {moving:?}"
    );

    // At rest nothing moves, and dry points have no flow.
    run_until_asleep(&mut world, 600.0);
    for c in 10..60 {
        if let Some(v) = at(&world, f64::from(c) * 0.25 + 0.125) {
            assert!(v[0].abs() < 0.02 && v[1].abs() < 0.02, "column {c}: {v:?}");
        }
    }
    assert!(world.flow_at_m(100.0, 0.75, 100.0).is_none());
}
