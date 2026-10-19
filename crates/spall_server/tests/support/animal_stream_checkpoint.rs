use super::*;
use spall_server::{ResidencyLimits, ResidencyPass};
use spall_sim::MemoryBacking;
use std::sync::Arc;

fn install(sim: &mut Simulation) -> (ResidencyPass, Arc<MemoryBacking>) {
    let backing = Arc::new(MemoryBacking::default());
    let pass = ResidencyPass::install_with_backing(
        sim.world_mut(),
        ResidencyLimits {
            budget_bricks: 1,
            max_dense_bytes: u64::MAX,
            interest_radius_bricks: 0,
        },
        backing.clone(),
    );
    (pass, backing)
}

// Exercise checkpoint assembly independently of edit scheduling: change the
// canonical volume, then acknowledge the new revision through the pass API.
fn dirty(sim: &mut Simulation, pass: &ResidencyPass) -> BrickCoord {
    let terrain = sim.world().terrain_volume_id();
    sim.world_mut()
        .volume_body_mut(terrain)
        .unwrap()
        .volume
        .apply_edit(&spall_voxel::EditPlan::filled_box(
            terrain,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId::AIR,
        ))
        .unwrap();
    let coord = BrickCoord::new(0, 0, 0);
    pass.on_commit(sim.world(), [coord]);
    coord
}

fn counters(pass: &ResidencyPass) -> (u64, u64) {
    let s = pass.stats();
    (
        s.checkpoint_bricks_captured_total,
        s.checkpoint_bricks_logical_total,
    )
}

#[test]
fn mixed_and_empty_animals_roundtrip_streamed_checkpoint_without_reloading_live_terrain() {
    for empty in [false, true] {
        let (mut sim, cat) = fixture(empty);
        let (mut pass, _) = install(&mut sim);
        let coord = dirty(&mut sim, &pass);
        let expected = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
        let saved = sim.capture_creature_snapshot(&cat.view()).unwrap();
        let registry = sim.world().registry().counters();
        let terrain = sim.world().terrain_volume_id();
        assert!(sim.world_mut().evict_brick(terrain, coord).unwrap());
        let resident = sim.world().terrain().volume.resident_brick_coords();
        assert!(!resident.contains(&coord));
        assert!(pass.capture_checkpoint(&sim, &cfg(), 0).is_err());
        assert_eq!(counters(&pass), (0, 0));
        let cp = pass
            .capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view())
            .unwrap();
        same_checkpoint(&cp, &expected);
        assert_eq!(
            counters(&pass).0,
            1,
            "dirty evicted brick must read backing"
        );
        assert_eq!(
            sim.world().terrain().volume.resident_brick_coords(),
            resident
        );
        assert_eq!(sim.world().registry().counters(), registry);
        assert_eq!(sim.journal_cursor(), 0);
        assert_eq!(sim.capture_creature_snapshot(&cat.view()).unwrap(), saved);
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&cp).unwrap();
        drop(writer);
        let recovered = spall_store::recover(scratch.db()).unwrap();
        assert!(recovered.checkpoint == cp);
        let (restored, cursor) = restore(&recovered, &cat).unwrap();
        assert_eq!(cursor, 0);
        assert!(!restored.world().has_evicted());
        assert_eq!(
            restored.capture_creature_snapshot(&cat.view()).unwrap(),
            saved
        );
        let again = persist::capture_with_catalog(&restored, &cfg(), 0, &cat.view()).unwrap();
        assert_eq!(again.world_hash, cp.world_hash);
        assert_eq!(again.meta, cp.meta);
        assert!(again.bricks == cp.bricks);
        assert!(again.creatures == cp.creatures);
    }
}

#[test]
fn warm_animal_checkpoint_uses_verified_cache_when_backing_is_unavailable() {
    let (mut sim, cat) = fixture(false);
    let (mut pass, backing) = install(&mut sim);
    let terrain = sim.world().terrain_volume_id();
    let coord = sim.world().terrain().volume.resident_brick_coords()[0];
    let expected = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    assert!(sim.world_mut().evict_brick(terrain, coord).unwrap());
    backing.mark_unavailable(terrain, coord);
    let cp = pass
        .capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view())
        .unwrap();
    same_checkpoint(&cp, &expected);
    assert_eq!(counters(&pass).0, 0);
    assert!(counters(&pass).1 > 0);
}

#[test]
fn cold_animal_checkpoint_refuses_bad_backing_without_warming_cache() {
    for fault in 0..3 {
        let (mut sim, cat) = fixture(false);
        let (mut pass, backing) = install(&mut sim);
        let coord = dirty(&mut sim, &pass);
        let terrain = sim.world().terrain_volume_id();
        let good = sim
            .world()
            .terrain()
            .volume
            .snapshot_brick(coord)
            .unwrap()
            .unwrap();
        let cells = good.material_cells();
        let revision = good.revision();
        let edited = good.is_edited();
        assert!(sim.world_mut().evict_brick(terrain, coord).unwrap());
        let before = sim.capture_creature_snapshot(&cat.view()).unwrap();
        if fault == 0 {
            backing.mark_unavailable(terrain, coord);
        } else {
            backing.insert(
                terrain,
                coord,
                spall_voxel::Brick::uniform(
                    MaterialId(1),
                    if fault == 1 {
                        Revision(revision.get() + 1)
                    } else {
                        revision
                    },
                ),
            );
        }
        let err = pass
            .capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view())
            .unwrap_err();
        if fault == 0 {
            assert!(matches!(
                err,
                persist::PersistError::EvictedBrickUnavailable { .. }
            ));
        } else {
            assert!(matches!(
                err,
                persist::PersistError::EvictedBrickDigestMismatch { .. }
            ));
        }
        assert_eq!(counters(&pass), (0, 0));
        assert_eq!(sim.capture_creature_snapshot(&cat.view()).unwrap(), before);
        assert!(sim.world().evicted(terrain).contains(coord));
        backing.insert(
            terrain,
            coord,
            spall_voxel::Brick::restored(&cells, revision, edited),
        );
        pass.capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view())
            .unwrap();
        assert_eq!(
            counters(&pass).0,
            1,
            "failed capture must not cache a candidate"
        );
    }
}

#[test]
fn catalog_failure_does_not_publish_staged_terrain_cache() {
    let (mut sim, cat) = fixture(false);
    let (mut pass, backing) = install(&mut sim);
    let coord = dirty(&mut sim, &pass);
    let terrain = sim.world().terrain_volume_id();
    assert!(sim.world_mut().evict_brick(terrain, coord).unwrap());
    let before = sim.capture_creature_snapshot(&cat.view()).unwrap();
    let mut wrong = cat.view();
    wrong.material_mapping_version += 1;
    assert!(
        pass.capture_checkpoint_with_catalog(&sim, &cfg(), 0, &wrong)
            .is_err()
    );
    assert_eq!(counters(&pass), (0, 0));
    backing.mark_unavailable(terrain, coord);
    assert!(matches!(
        pass.capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view()),
        Err(persist::PersistError::EvictedBrickUnavailable { .. })
    ));
    assert_eq!(counters(&pass), (0, 0));
    assert_eq!(sim.capture_creature_snapshot(&cat.view()).unwrap(), before);
}

#[test]
fn residency_cache_refuses_another_world_with_identical_ids_and_revisions() {
    let (mut sim, cat) = fixture(false);
    let (mut pass, _) = install(&mut sim);
    let (other, _) = fixture(false);
    assert_eq!(
        sim.world().terrain_volume_id(),
        other.world().terrain_volume_id()
    );
    assert_eq!(
        sim.world().geometry_topology_hash(),
        other.world().geometry_topology_hash()
    );
    for catalog in [false, true] {
        let err = if catalog {
            pass.capture_checkpoint_with_catalog(&other, &cfg(), 0, &cat.view())
        } else {
            pass.capture_checkpoint(&other, &cfg(), 0)
        }
        .unwrap_err();
        assert!(matches!(
            err,
            persist::PersistError::BadField("checkpoint residency world instance mismatch")
        ));
        assert_eq!(counters(&pass), (0, 0));
    }
    pass.capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view())
        .unwrap();
}

#[test]
fn streamed_animal_checkpoint_refuses_evicted_ordinary_body() {
    let (mut sim, cat) = fixture(false);
    sim.world_mut()
        .spawn_body(
            |id| {
                let mut volume =
                    spall_voxel::Volume::new(id, spall_core::units::CellSizeCode::Quarter);
                volume
                    .apply_edit(&spall_voxel::EditPlan::filled_box(
                        id,
                        GlobalCell::new(0, 0, 0),
                        GlobalCell::new(0, 0, 0),
                        MaterialId(1),
                    ))
                    .unwrap();
                volume
            },
            spall_sim::BodyPose::new(glam::DQuat::IDENTITY, [30.0, 3.0, 30.0]),
            [0.0; 3],
            [0.0; 3],
            1000.0,
            0,
        )
        .unwrap();
    let (mut pass, _) = install(&mut sim);
    let body = sim.world().bodies().next().unwrap().volume.id();
    assert!(
        sim.world_mut()
            .evict_brick(body, BrickCoord::new(0, 0, 0))
            .unwrap()
    );
    let saved = sim.capture_creature_snapshot(&cat.view()).unwrap();
    assert!(matches!(
        pass.capture_checkpoint_with_catalog(&sim, &cfg(), 0, &cat.view()),
        Err(persist::PersistError::BadField(
            "checkpoint requires complete resident body geometry"
        ))
    ));
    assert_eq!(counters(&pass), (0, 0));
    assert_eq!(sim.capture_creature_snapshot(&cat.view()).unwrap(), saved);
}

fn same_checkpoint(a: &spall_store::Checkpoint, b: &spall_store::Checkpoint) {
    assert_eq!(a.meta, b.meta);
    assert_eq!(a.world_hash, b.world_hash);
    // Eviction rebuilds the terrain collider and advances its runtime revision.
    // Require every stored body field except that known terrain transition equal.
    let mut prior_bodies = b.bodies.clone();
    assert_eq!(
        a.bodies[0].collider_revision,
        prior_bodies[0].collider_revision + 1
    );
    prior_bodies[0].collider_revision = a.bodies[0].collider_revision;
    assert_eq!(a.bodies, prior_bodies);
    assert!(a.creatures == b.creatures, "animals differ");
    assert_eq!(a.weather, b.weather);
    assert_eq!(a.snow, b.snow);
    assert_eq!(a.vegetation, b.vegetation);
    assert_eq!(a.column_water, b.column_water);
    assert!(a.bricks == b.bricks, "brick records differ");
}
