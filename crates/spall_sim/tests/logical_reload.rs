//! T23 / G3 row 7, slice C — an edit that needs an evicted brick's cells
//! reloads it from the backing and proceeds; with no backing it is rejected
//! with a bounded explicit failure. Nothing is left mutated on rejection.

use std::sync::Arc;

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::{EditIntent, EditTarget, MemoryBacking, Simulation, SimulationConfig, fixtures};

/// Sever the seam column of `cross_brick_bridged_setup` — the brush writes cells
/// in **both** brick `x = 0` and brick `x = 1`.
fn seam_cut() -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(31 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
        2 * BRUSH_UNIT,
    )
    .unwrap();
    EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

fn full_post_seam_cut_hash() -> spall_protocol::Hash32 {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    sim.submit(seam_cut()).unwrap();
    sim.run_until_idle(24).unwrap();
    sim.world().world_hash()
}

#[test]
fn an_edit_that_needs_evicted_geometry_reloads_it_and_commits() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();

    // Durable backing = every brick as it stands now.
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    sim.world_mut().set_backing(Arc::new(backing));

    // Evict brick x = 1 — the seam column + beam + floor all reach into it, so
    // the cut cannot stage/commit without it. The pipeline must reload + retry.
    let victim = BrickCoord::new(1, 0, 0);
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());
    assert!(sim.world().has_evicted());

    sim.submit(seam_cut()).unwrap();
    sim.run_until_idle(48).unwrap();

    assert!(
        sim.committed(RequestId(1)).is_some(),
        "the cut committed after reloading brick {victim:?}; status = {:?}",
        sim.action_status(RequestId(1))
    );
    assert!(
        !sim.world().evicted(terrain).contains(victim),
        "the reloaded brick's digest was cleared"
    );
    assert_eq!(
        sim.world().world_hash(),
        full_post_seam_cut_hash(),
        "reload-and-retry reached a different world than a fully resident run"
    );
}

#[test]
fn an_edit_that_needs_evicted_geometry_with_no_backing_is_rejected_cleanly() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    // No backing installed.

    let victim = BrickCoord::new(1, 0, 0);
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());
    let hash_before = sim.world().world_hash();
    let solid_before = sim.world().total_solid_cells();

    sim.submit(seam_cut()).unwrap();
    sim.run_until_idle(24).unwrap();

    let status = format!("{:?}", sim.action_status(RequestId(1)).unwrap());
    assert!(
        status.contains("evicted geometry unavailable"),
        "expected a bounded explicit rejection, got {status}"
    );
    assert!(sim.committed(RequestId(1)).is_none());
    // The world is untouched — the rejected edit changed nothing.
    assert_eq!(sim.world().world_hash(), hash_before);
    assert_eq!(sim.world().total_solid_cells(), solid_before);
    assert!(sim.world().has_evicted(), "the digest is still retained");
}

#[test]
fn a_wrong_backing_record_is_refused_and_keeps_the_digest() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();

    let victim = BrickCoord::new(1, 0, 0);
    let original = sim
        .world()
        .terrain()
        .volume
        .snapshot_brick(victim)
        .unwrap()
        .unwrap();
    let original_cells = (0..spall_core::CELLS_PER_BRICK as u16)
        .map(|i| original.get(spall_core::LocalCell::from_linear_index(i).unwrap()))
        .collect::<Vec<_>>();

    // First offer the correct cells at the wrong revision.
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    let other = sim
        .world()
        .terrain()
        .volume
        .resident_brick_coords()
        .into_iter()
        .find(|&c| c != victim)
        .unwrap();
    let other_cells = (0..spall_core::CELLS_PER_BRICK as u16)
        .map(|i| {
            sim.world()
                .terrain()
                .volume
                .snapshot_brick(other)
                .unwrap()
                .unwrap()
                .get(spall_core::LocalCell::from_linear_index(i).unwrap())
        })
        .collect::<Vec<_>>();
    let wrong_revision = spall_voxel::Brick::restored(
        &original_cells,
        spall_core::Revision(999),
        original.is_edited(),
    );
    backing.insert(terrain, victim, wrong_revision);
    sim.world_mut().set_backing(Arc::new(backing));

    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());

    let hash_before = sim.world().world_hash();
    let solids_before = sim.world().total_solid_cells();
    let retained = sim.world().evicted(terrain).get(victim).unwrap();

    // The reload validates before publication: the wrong revision is rejected,
    // the live slot stays absent, and the digest remains authoritative.
    let reloaded = sim.world_mut().reload_brick(terrain, victim);
    assert!(reloaded.is_err(), "a mismatched reload is an error");
    assert!(sim.world().evicted(terrain).contains(victim));
    assert!(
        sim.world()
            .terrain()
            .volume
            .snapshot_brick(victim)
            .unwrap()
            .is_none(),
        "a rejected backing candidate must not become resident"
    );
    assert_eq!(sim.world().world_hash(), hash_before);
    assert_eq!(sim.world().total_solid_cells(), solids_before);
    assert_eq!(sim.world().evicted(terrain).get(victim), Some(retained));

    // The same atomicity holds for wrong content at the correct revision.
    let wrong_content = MemoryBacking::default();
    wrong_content.insert(
        terrain,
        victim,
        spall_voxel::Brick::restored(&other_cells, retained.revision, true),
    );
    sim.world_mut().set_backing(Arc::new(wrong_content));
    assert!(sim.world_mut().reload_brick(terrain, victim).is_err());
    assert!(
        sim.world()
            .terrain()
            .volume
            .snapshot_brick(victim)
            .unwrap()
            .is_none()
    );
    assert_eq!(sim.world().world_hash(), hash_before);
    assert_eq!(sim.world().total_solid_cells(), solids_before);
    assert_eq!(sim.world().evicted(terrain).get(victim), Some(retained));

    // A correct retry can still complete the lifecycle after the rejection.
    let correct = MemoryBacking::default();
    correct.insert(
        terrain,
        victim,
        spall_voxel::Brick::restored(&original_cells, retained.revision, original.is_edited()),
    );
    sim.world_mut().set_backing(Arc::new(correct));
    assert!(sim.world_mut().reload_brick(terrain, victim).unwrap());
    assert!(!sim.world().evicted(terrain).contains(victim));
    assert!(
        sim.world()
            .terrain()
            .volume
            .snapshot_brick(victim)
            .unwrap()
            .is_some()
    );
    assert_eq!(sim.world().world_hash(), hash_before);
    assert_eq!(sim.world().total_solid_cells(), solids_before);
}
