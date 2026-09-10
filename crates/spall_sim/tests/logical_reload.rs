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

    // Backing that has the *wrong* geometry for `victim` (a different brick's
    // cells).
    let mut backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    let other = sim
        .world()
        .terrain()
        .volume
        .resident_brick_coords()
        .into_iter()
        .find(|&c| c != victim)
        .unwrap();
    let other_brick = spall_voxel::Brick::restored(
        &(0..spall_core::CELLS_PER_BRICK as u16)
            .map(|i| {
                sim.world()
                    .terrain()
                    .volume
                    .snapshot_brick(other)
                    .unwrap()
                    .unwrap()
                    .get(spall_core::LocalCell::from_linear_index(i).unwrap())
            })
            .collect::<Vec<_>>(),
        spall_core::Revision(999),
        true,
    );
    backing.insert(terrain, victim, other_brick);
    sim.world_mut().set_backing(Arc::new(backing));

    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());

    // The reload installs the wrong brick, `verify_reload` rejects it, the
    // digest stays.
    let reloaded = sim.world_mut().reload_brick(terrain, victim);
    assert!(reloaded.is_err(), "a mismatched reload is an error");
    assert!(sim.world().evicted(terrain).contains(victim));
}
