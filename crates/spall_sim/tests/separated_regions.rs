//! T23 / G3 increment 1 — the `separated-regions` integrated-acceptance scene.
//!
//! CPU-side proof that the two collapsible structures in
//! [`spall_sim::fixtures::separated_regions_setup`] are independent: cutting one
//! region's column detaches only that region's beam, both beams come to rest on
//! their own floor, and total occupied mass is conserved across the two cuts
//! (terrain + both detached bodies == start − cells the two brushes removed).
//! The multi-process separated-players / multi-region-collapse evidence is
//! `fixtures/scenarios/t23-g3.json` under `cargo xtask scenario`.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    CELLS_PER_BRICK, EntityId, GlobalCell, LocalCell, PlayerInput, SphereBrush, player_entity_for,
};
use spall_protocol::{InputSeq, RequestId};
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::{Sample, Volume};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

fn solid_count(v: &Volume) -> u64 {
    let mut n = 0u64;
    for c in v.resident_brick_coords() {
        let s = v.snapshot_brick(c).unwrap().unwrap();
        for i in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(i).unwrap();
            if !s.get(lc).is_air() {
                n += 1;
            }
        }
    }
    n
}

/// Total occupied cells across the terrain volume and every detached body.
fn world_solid_total(sim: &Simulation) -> u64 {
    let mut n = solid_count(&sim.world().terrain().volume);
    for b in sim.world().bodies() {
        n += solid_count(&b.volume);
    }
    n
}

#[test]
fn cutting_one_region_leaves_the_other_region_intact() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();

    let start_total = world_solid_total(&sim);
    assert_eq!(sim.world().body_count(), 0, "no bodies before any cut");

    // West column: x 10..=11, z 3..=4, y 4..=9.
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 6, 3, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    assert_eq!(
        sim.world().body_count(),
        1,
        "the west column cut detaches exactly the west beam"
    );

    // East column: the west column shifted by the +72,+0,+72 cell region offset.
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_EAST_OFFSET;
    sim.submit(EditIntent::cut(
        RequestId(2),
        actor(),
        EditTarget::Terrain,
        brush_cell(10 + e.x, 6 + e.y, 3 + e.z, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    assert_eq!(
        sim.world().body_count(),
        2,
        "the east column cut detaches a second, independent beam"
    );

    // One beam sits in the west bricks (x < 32), the other in the east bricks
    // (x >= 64): the two collapses did not bleed into one another.
    let mut xs: Vec<i64> = sim
        .world()
        .bodies()
        .map(|b| b.collider_region.0.x)
        .collect();
    xs.sort_unstable();
    assert!(
        xs[0] < 32 && xs[1] >= 64,
        "detached beams should be one per region, got collider-region min-x {xs:?}"
    );

    // Conservation: the only cells that left the world are the ones the two
    // spherical brushes removed. A radius-2-cell cut clears at most a 5³ = 125
    // cell ball; two of them bound the loss well under 300 cells.
    let end_total = world_solid_total(&sim);
    assert!(
        end_total < start_total,
        "the cuts removed some column cells ({start_total} -> {end_total})"
    );
    assert!(
        start_total - end_total <= 300,
        "only the two brush volumes should be gone ({start_total} -> {end_total})"
    );
}

#[test]
fn both_detached_beams_come_to_rest_on_their_own_floor() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_EAST_OFFSET;

    for (req, ox) in [(1u64, 0i64), (2, e.x)] {
        sim.submit(EditIntent::cut(
            RequestId(req),
            actor(),
            EditTarget::Terrain,
            brush_cell(10 + ox, 6, 3 + if ox == 0 { 0 } else { e.z }, 2),
        ))
        .unwrap();
    }

    // Enough ticks for both freed beams to drop ~2.5 m and settle.
    for _ in 0..600 {
        sim.tick().unwrap();
    }

    assert_eq!(sim.world().body_count(), 2, "both beams stayed detached");
    for b in sim.world().bodies() {
        let v = b.linvel_m_s;
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        assert!(b.sleeping, "a settled beam is asleep");
        assert!(speed < 0.05, "beam still moving at {speed} m/s");
        // It rested near the floor top (y ~ 1 m), not fell through the world.
        let y = b.pose.translation_m[1];
        assert!(y > -1.0, "beam fell through its floor (y = {y})");
    }

    // The world's declared lower support plane is unbroken: the terrain volume
    // still holds both floors.
    assert!(
        solid_count(&sim.world().terrain().volume) > 0,
        "terrain floors survive the two collapses"
    );
}

// --- T23 / G3 open item row 2: full-envelope (> 100 m) separation ----------

/// The connecting scenario's spawn table puts west- and east-region players
/// genuinely `> 100 m` apart within the bounded world — not merely `18 m`
/// apart as increment 1 left it.
#[test]
fn full_envelope_spawns_are_genuinely_over_100_m_apart() {
    let spawns = fixtures::SEPARATED_REGION_FAR_SPAWNS;
    // Even slots are west, odd slots are east (see `Scene::player_spawns`).
    for (west, east) in [(0usize, 1usize), (2, 3)] {
        let [wx, wy, wz] = spawns[west];
        let [ex, ey, ez] = spawns[east];
        let d = ((ex - wx).powi(2) + (ey - wy).powi(2) + (ez - wz).powi(2)).sqrt();
        assert!(
            d > 100.0,
            "west slot {west} {spawns0:?} and east slot {east} {spawns1:?} are only {d} m apart",
            spawns0 = spawns[west],
            spawns1 = spawns[east],
        );
    }
}

/// Mirrors [`cutting_one_region_leaves_the_other_region_intact`] on the
/// full-envelope scene: the two regions collapse independently even though
/// they are `110 m` apart along one axis, and the resident world stays
/// CPU-cheap (a thin corridor, not a growing rectangle).
#[test]
fn cutting_one_far_region_leaves_the_other_region_intact() {
    let mut sim = Simulation::new(SimulationConfig::new(
        fixtures::separated_regions_full_envelope_setup(),
    ))
    .unwrap();
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_FAR_EAST_OFFSET;

    // Only the two regions and the connecting causeway are resident — cheap
    // despite the 110 m separation: at 32 cells/brick the corridor is one
    // brick tall and wide, and at most `~15` bricks long.
    let resident_bricks = sim.world().terrain().volume.resident_brick_count();
    assert!(
        resident_bricks <= 20,
        "full-envelope corridor should stay a thin resident strip, got {resident_bricks} bricks"
    );

    let start_total = world_solid_total(&sim);
    assert_eq!(sim.world().body_count(), 0, "no bodies before any cut");

    // West column: x 10..=11, z 3..=4, y 4..=9.
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 6, 3, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    assert_eq!(
        sim.world().body_count(),
        1,
        "the west column cut detaches exactly the west beam"
    );

    // East column: the west column shifted by the far east-region offset.
    sim.submit(EditIntent::cut(
        RequestId(2),
        actor(),
        EditTarget::Terrain,
        brush_cell(10 + e.x, 6 + e.y, 3 + e.z, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    assert_eq!(
        sim.world().body_count(),
        2,
        "the east column cut detaches a second, independent beam"
    );

    // One beam sits near the west region, the other near the east region —
    // partition on the midpoint so the assertion generalises to the offset.
    let mid = e.x / 2;
    let mut xs: Vec<i64> = sim
        .world()
        .bodies()
        .map(|b| b.collider_region.0.x)
        .collect();
    xs.sort_unstable();
    assert!(
        xs[0] < mid && xs[1] >= mid,
        "detached beams should be one per region, got collider-region min-x {xs:?} (midpoint {mid})"
    );

    let end_total = world_solid_total(&sim);
    assert!(
        end_total < start_total,
        "the cuts removed some column cells ({start_total} -> {end_total})"
    );
    assert!(
        start_total - end_total <= 300,
        "only the two brush volumes should be gone ({start_total} -> {end_total})"
    );
}

/// Mirrors [`both_detached_beams_come_to_rest_on_their_own_floor`] on the
/// full-envelope scene.
#[test]
fn both_far_detached_beams_come_to_rest_on_their_own_floor() {
    let mut sim = Simulation::new(SimulationConfig::new(
        fixtures::separated_regions_full_envelope_setup(),
    ))
    .unwrap();
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_FAR_EAST_OFFSET;

    for (req, ox) in [(1u64, 0i64), (2, e.x)] {
        sim.submit(EditIntent::cut(
            RequestId(req),
            actor(),
            EditTarget::Terrain,
            brush_cell(10 + ox, 6, 3, 2),
        ))
        .unwrap();
    }

    // Enough ticks for both freed beams to drop ~2.5 m and settle.
    for _ in 0..600 {
        sim.tick().unwrap();
    }

    assert_eq!(sim.world().body_count(), 2, "both beams stayed detached");
    for b in sim.world().bodies() {
        let v = b.linvel_m_s;
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        assert!(b.sleeping, "a settled beam is asleep");
        assert!(speed < 0.05, "beam still moving at {speed} m/s");
        let y = b.pose.translation_m[1];
        assert!(y > -1.0, "beam fell through its floor (y = {y})");
    }

    assert!(
        solid_count(&sim.world().terrain().volume) > 0,
        "terrain floors survive the two collapses"
    );
}

/// The causeway between the two far regions is continuous, solid ground —
/// a scripted player walking the full `110 m` never has to cross open air.
#[test]
fn causeway_connects_the_two_far_regions_with_continuous_solid_ground() {
    let v = spall_voxel::fixtures::separated_regions_full_envelope_scene(
        spall_core::VolumeId::new(1).unwrap(),
    );
    let e = spall_voxel::fixtures::SEPARATED_REGIONS_FAR_EAST_OFFSET;

    // Spot-check floor cells across the whole corridor, including both region
    // interiors and the causeway strip between them; `y = 1` and `z = 3` are
    // inside every region's own floor slab (`y 0..=3`, `z 0..=7`).
    let xs = [
        0i64,
        20,
        24,
        100,
        200,
        300,
        e.x - 1,
        e.x,
        e.x + 10,
        e.x + 20,
    ];
    for x in xs {
        let sample = v.sample(GlobalCell::new(x, 1, 3)).unwrap();
        assert!(
            matches!(sample, Sample::Filled(_)),
            "expected solid ground at x = {x}, got {sample:?}"
        );
    }
}

/// CPU-side proof of the `> 100 m` scripted walk itself (mirroring
/// `t23-g3-full-envelope.json`'s player-path leg): starting from
/// [`fixtures::SEPARATED_REGION_FAR_SPAWNS`]'s slot-0 spawn, after the west
/// column is cut (as client 0's own scripted action does at `at_tick: 40`), a
/// fixed `+X` walk input for the leg's own tick span (`1600` ticks at `60 Hz`,
/// matching `to - from` in the fixture) covers `> 100 m` and lands the player
/// on solid ground inside the causeway/east-region span, matching the
/// wire-harness scenario's `movement.min_distance_m` gate.
///
/// This also pins the reason slot 0's spawn is *not* at the same local offset
/// as [`fixtures::SEPARATED_REGION_SPAWNS`] (see that constant's doc comment):
/// spawning within the first few metres of `x = 0` leaves a freshly-created
/// player capsule unresponsive to horizontal input for hundreds of ticks — a
/// pre-existing defect this test's spawn deliberately avoids, not a distance
/// or collapse-independence property of this scene.
#[test]
fn scripted_walk_from_slot_0_spawn_covers_over_100_m() {
    let mut sim = Simulation::new(SimulationConfig::new(
        fixtures::separated_regions_full_envelope_setup(),
    ))
    .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 6, 3, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();

    let entity = player_entity_for(0);
    let spawn = fixtures::SEPARATED_REGION_FAR_SPAWNS[0];
    sim.add_player(entity, spawn);

    let forward = PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    for seq in 1..=1600u64 {
        assert!(sim.set_player_input(entity, forward, InputSeq(seq)));
        sim.tick().unwrap();
    }

    let p = sim.world().players().next().unwrap();
    let travelled = p.state.position_m[0] - spawn[0];
    assert!(
        travelled > 100.0,
        "expected the scripted leg to cover > 100 m, got {travelled} m (final x = {})",
        p.state.position_m[0]
    );
    assert!(
        p.state.grounded,
        "the player should still be on solid ground"
    );
}
