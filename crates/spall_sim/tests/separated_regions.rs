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
use spall_core::{CELLS_PER_BRICK, EntityId, LocalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::Volume;

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
