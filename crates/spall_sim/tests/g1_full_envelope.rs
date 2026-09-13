//! T11a / ENG-62: CPU-side evidence for the G1 gate's full `64 x 32 x 64 m`
//! envelope — real, resident, walkable terrain across the whole footprint
//! (not just isolated structures), a hollow tower/bridge spanning brick
//! boundaries, an excavatable ramp, and a moving hollow test-volume body.
//! `docs/reports/G1.md` records the measured evidence this proves.

use std::time::{Duration, Instant};

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{g1_full_envelope_setup, spawn_g1_hollow_test_volume};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn sim() -> Simulation {
    let mut setup = g1_full_envelope_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).expect("g1 full-envelope world stands up")
}

/// This is the fixture's core feasibility claim: the earlier continuously-
/// varying heightmap attempt blew `ColliderInfeasible::TooLarge` outright
/// (7137 greedy boxes over the 4096 budget, so the ~6 000 000-cell native
/// fallback was checked and rejected). The flat-plain-plus-ramp redesign
/// stands the world up and steps it well inside the 16.7 ms / 60 Hz tick —
/// generously bounded here (not a tight per-tick assertion, which would be
/// machine-dependent) so a real regression still fails loudly.
#[test]
fn the_full_envelope_stands_up_and_steps_well_inside_budget() {
    let mut world = sim();
    spawn_g1_hollow_test_volume(world.world_mut()).expect("hollow test volume spawns");

    let started = Instant::now();
    for _ in 0..200 {
        world.tick().expect("tick");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "200 ticks of the full G1 envelope took {elapsed:?}, expected well under 5 s \
         even unoptimized (measured ~30 ms debug / ~1 ms release on dev hardware)"
    );
}

/// Cutting the tower severs the bridge from the anchored terrain — the
/// destructible-structure half of the gate's "12 m hollow tower/bridge
/// spanning brick boundaries".
#[test]
fn cutting_the_tower_detaches_the_bridge_as_one_body() {
    let mut world = sim();
    let actor = EntityId::new(1).unwrap();
    let before_solid = world.world().total_solid_cells();

    // A sphere through the tower's shell, well below the bridge (which
    // starts at y = 46 + 18 = 64), centred on the tower footprint
    // (x 24..=39, z 32..=47). Radius 12, not 9: a sphere's reach at a square
    // shell's *corners* is the diagonal distance (~11.3 cells here), not the
    // face distance (~7-8 cells) — a smaller radius holes the middle of each
    // wall but leaves the four corner posts uncut, so the top of the tower
    // stays connected through them and nothing detaches.
    world
        .submit(EditIntent::cut(
            RequestId(1),
            actor,
            EditTarget::Terrain,
            brush_cell(31, 48, 39, 12),
        ))
        .expect("tower cut admitted");

    for _ in 0..40 {
        world.tick().expect("tick");
    }

    assert_eq!(
        world.world().body_count(),
        1,
        "the tower cut detaches exactly the upper tower + bridge as one body"
    );
    let after_solid = world.world().total_solid_cells();
    assert!(
        after_solid < before_solid,
        "the cut destroyed the material inside its brush, so total solid cells (terrain + \
         every body, total_solid_cells already sums both) must strictly decrease"
    );
    assert!(
        after_solid > 0,
        "the cut should not have destroyed the entire tower/bridge"
    );
}

/// The excavatable ramp: a real cut into the sloped surface commits like any
/// other terrain edit (no separate machinery — the ramp is ordinary terrain).
#[test]
fn the_ramp_is_ordinary_excavatable_terrain() {
    let mut world = sim();
    let actor = EntityId::new(1).unwrap();
    let before = world.world().total_solid_cells();

    // Partway down the ramp (x 180..=211, z 100..=115): a real dig into the
    // slope.
    world
        .submit(EditIntent::cut(
            RequestId(1),
            actor,
            EditTarget::Terrain,
            brush_cell(195, 38, 107, 3),
        ))
        .expect("ramp cut admitted");
    for _ in 0..10 {
        world.tick().expect("tick");
    }

    let after = world.world().total_solid_cells();
    assert!(after < before, "the ramp cut actually removed solid cells");
}

/// The gate's "moving hollow test volume": a hollow body that free-falls from
/// its spawn point and comes to rest, real motion and real settling, not a
/// static prop.
#[test]
fn the_hollow_test_volume_falls_and_settles() {
    let mut world = sim();
    let entity = spawn_g1_hollow_test_volume(world.world_mut()).expect("hollow body spawns");
    let start_y = world.world().body(entity).unwrap().pose.translation_m[1];

    let mut max_speed_seen = 0.0_f64;
    for _ in 0..300 {
        world.tick().expect("tick");
        let body = world.world().body(entity).unwrap();
        let v = body.linvel_m_s;
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        max_speed_seen = max_speed_seen.max(speed);
    }

    let body = world.world().body(entity).unwrap();
    assert!(
        max_speed_seen > 1.0,
        "the hollow volume should show real fall speed at some point, saw {max_speed_seen}"
    );
    assert!(
        body.sleeping,
        "the hollow volume should have settled to rest within 300 ticks"
    );
    assert!(
        body.pose.translation_m[1] < start_y,
        "the hollow volume should have fallen from its spawn height"
    );
}

/// Determinism: two independent runs of the same script reach the same
/// canonical hash.
#[test]
fn the_full_envelope_is_deterministic() {
    let run = || {
        let mut world = sim();
        spawn_g1_hollow_test_volume(world.world_mut()).expect("hollow body spawns");
        let actor = EntityId::new(1).unwrap();
        for tick in 1..=60u64 {
            if tick == 4 {
                world
                    .submit(EditIntent::cut(
                        RequestId(1),
                        actor,
                        EditTarget::Terrain,
                        brush_cell(31, 48, 39, 12),
                    ))
                    .expect("tower cut admitted");
            }
            world.tick().expect("tick");
        }
        world.world().world_hash()
    };
    assert_eq!(run(), run());
}
