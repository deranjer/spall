//! The terrain's structure index is kept warm across non-splitting commits and dropped the moment
//! anything else touches the volume; every staging against a warm index is cross-checked against
//! a rebuilt one in debug builds, so these sequences also prove the reuse exact.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn cut(sim: &mut Simulation, id: u64, x: i64, z: i64) {
    sim.submit(EditIntent::cut(
        RequestId(id),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(x, 1, z, 2),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    assert!(sim.committed(RequestId(id)).is_some(), "cut {id} committed");
}

fn cut_at(sim: &mut Simulation, id: u64, x: i64, y: i64, z: i64, radius: i64) {
    sim.submit(EditIntent::cut(
        RequestId(id),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(x, y, z, radius),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    assert!(sim.committed(RequestId(id)).is_some(), "cut {id} committed");
}

fn warm(sim: &Simulation) -> bool {
    let world = sim.world();
    let terrain = world.terrain_volume_id();
    let stamp = world.volume_ref(terrain).unwrap().state_stamp();
    world.warm_structure_index(terrain, stamp).is_some()
}

#[test]
fn a_non_splitting_commit_leaves_the_index_warm_and_later_cuts_reuse_it() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    assert!(!warm(&sim), "nothing is warm before the first commit");
    let reuses_before = spall_sim::warm_index_reuses();
    // Each cut after the first stages against the index the previous commit left; in a debug
    // build every one is rebuilt and compared, so a wrong reuse would fail inside staging.
    for (i, x) in [10_i64, 22, 34, 46, 58].into_iter().enumerate() {
        cut(&mut sim, i as u64 + 1, x, 8);
        assert!(warm(&sim), "warm after cut {}", i + 1);
    }
    assert!(
        spall_sim::warm_index_reuses() - reuses_before >= 4,
        "every cut after the first staged against the warm index"
    );
    // The world the warm index describes is the one the commits produced.
    let fresh = {
        let mut other =
            Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
        for (i, x) in [10_i64, 22, 34, 46, 58].into_iter().enumerate() {
            cut(&mut other, i as u64 + 1, x, 8);
        }
        other.world().world_hash()
    };
    assert_eq!(sim.world().world_hash(), fresh);
}

#[test]
fn any_other_change_to_the_volume_makes_the_warm_index_stale() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    cut(&mut sim, 1, 10, 8);
    assert!(warm(&sim));
    let terrain = sim.world().terrain_volume_id();
    let stamp = sim.world().volume_ref(terrain).unwrap().state_stamp();

    // A brick evicted behind the commit's back changes the stamp, so the index is not reused.
    let coord = *sim
        .world()
        .volume_ref(terrain)
        .unwrap()
        .resident_brick_coords()
        .first()
        .expect("a resident brick");
    sim.world_mut().evict_brick(terrain, coord).ok();
    let now = sim.world().volume_ref(terrain).unwrap().state_stamp();
    assert_ne!(now, stamp, "mutating the volume changes its stamp");
    assert!(
        sim.world().warm_structure_index(terrain, now).is_none(),
        "no index was recorded for the mutated state"
    );
}

#[test]
fn a_splitting_commit_keeps_the_index_warm_with_the_new_epoch() {
    // Cutting the column detaches the beam: a real split that bumps the topology epoch.
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let epoch_before = sim.world().topology_epoch();
    cut_at(&mut sim, 1, 10, 4, 1, 2);
    assert_eq!(sim.world().body_count(), 1, "the beam detached");
    assert_ne!(
        sim.world().topology_epoch(),
        epoch_before,
        "a split moves the topology epoch"
    );
    // Debug builds already proved the kept index equals a fresh build, token included, inside
    // the commit; here: it is kept at all, for the world the split produced.
    assert!(warm(&sim), "warm after a splitting commit");

    // A later cut stages against it and commits (its token must match the new epoch).
    let reuses_before = spall_sim::warm_index_reuses();
    cut_at(&mut sim, 2, 4, 1, 1, 1);
    assert!(
        spall_sim::warm_index_reuses() > reuses_before,
        "the cut after the split reused the kept index"
    );
}
