//! T23 / G3 row 7, slice C — an edit that needs an evicted brick's cells
//! reloads it from the backing and proceeds; with no backing it is rejected
//! with a bounded explicit failure. Nothing is left mutated on rejection.

use std::sync::Arc;

use glam::DQuat;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, CELLS_PER_BRICK, EntityId, LocalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::{
    BodyPose, EditIntent, EditTarget, MemoryBacking, Simulation, SimulationConfig, fixtures,
};

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
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("post-commit resident collision must be current");
    assert_eq!(
        sim.world().world_hash(),
        full_post_seam_cut_hash(),
        "reload-and-retry reached a different world than a fully resident run"
    );

    // The newly committed revision must survive a second residency cycle.
    // Refresh the backing to that committed revision before evicting it again.
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    sim.world_mut().set_backing(Arc::new(backing));
    let before_re_evict = sim.world().terrain_brick_collider_count();
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());
    assert_eq!(
        sim.world().terrain_brick_collider_count(),
        before_re_evict - 1,
        "re-eviction retires the current brick collider"
    );
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("re-eviction leaves no old whole-terrain shape");
    assert!(sim.world_mut().reload_brick(terrain, victim).unwrap());
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("second reload restores a collider for the committed revision");
}

#[test]
fn emptying_terrain_retires_every_per_brick_collider() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    sim.world_mut().set_backing(Arc::new(backing));
    let victim = BrickCoord::new(0, 0, 0);
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());
    assert!(sim.world_mut().reload_brick(terrain, victim).unwrap());
    assert!(sim.world().terrain_brick_colliders_enabled());
    assert_eq!(sim.world().terrain_brick_collider_count(), 1);

    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(12 * BRUSH_UNIT + h, h, 12 * BRUSH_UNIT + h),
        17 * BRUSH_UNIT,
    )
    .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(2),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();

    assert!(sim.committed(RequestId(2)).is_some());
    assert_eq!(sim.world().total_solid_cells(), 0);
    assert_eq!(sim.world().terrain_brick_collider_count(), 0);
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("empty terrain has neither brick shapes nor its old whole-terrain collider");
}

#[test]
fn live_and_replayed_edits_install_equivalent_terrain_collision() {
    let mut live =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    let terrain = live.world().terrain_volume_id();
    let backing = MemoryBacking::from_volume(&live.world().terrain().volume);
    live.world_mut().set_backing(Arc::new(backing));
    let victim = BrickCoord::new(1, 0, 0);
    assert!(live.world_mut().evict_brick(terrain, victim).unwrap());
    assert!(live.world_mut().reload_brick(terrain, victim).unwrap());
    live.submit(seam_cut()).unwrap();
    live.run_until_idle(48).unwrap();
    live.world()
        .validate_terrain_brick_colliders()
        .expect("live edit collision is current");
    let entry = live.journal().entries()[0].clone();

    let mut replay =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
    let replay_terrain = replay.world().terrain_volume_id();
    let backing = MemoryBacking::from_volume(&replay.world().terrain().volume);
    replay.world_mut().set_backing(Arc::new(backing));
    assert!(
        replay
            .world_mut()
            .evict_brick(replay_terrain, victim)
            .unwrap()
    );
    assert!(
        replay
            .world_mut()
            .reload_brick(replay_terrain, victim)
            .unwrap()
    );
    replay
        .world_mut()
        .replay_transaction(
            &entry.transaction,
            &entry.participants,
            entry.bulk_baseline.as_ref(),
        )
        .expect("journalled edit replays into the brick collider representation");
    replay
        .world()
        .validate_terrain_brick_colliders()
        .expect("replayed edit collision is current");
    assert_eq!(live.world().world_hash(), replay.world().world_hash());

    // A body dropped onto the same surviving floor must observe equivalent
    // collision in the live and replayed worlds, beyond matching metadata.
    let drop_body = |sim: &mut Simulation| {
        sim.world_mut()
            .spawn_body(
                fixtures::solid_block(1),
                BodyPose::new(DQuat::IDENTITY, [6.0, 3.0, 0.5]),
                [0.0; 3],
                [0.0; 3],
                2600.0,
                0,
            )
            .unwrap()
    };
    let live_body = drop_body(&mut live);
    let replay_body = drop_body(&mut replay);
    for _ in 0..240 {
        live.step_physics_only();
        replay.step_physics_only();
    }
    let live_y = live.world().body(live_body).unwrap().pose.translation_m[1];
    let replay_y = replay.world().body(replay_body).unwrap().pose.translation_m[1];
    assert!(
        live_y > 0.0,
        "live collision did not support the dropped body: {live_y}"
    );
    assert!(
        (live_y - replay_y).abs() < 1.0e-4,
        "live {live_y} != replay {replay_y}"
    );
}

#[test]
fn terrain_collider_residency_tracks_eviction_and_reload_without_stale_shapes() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    sim.world_mut().set_backing(Arc::new(backing));

    let (victim, active) = sim
        .world()
        .terrain()
        .volume
        .resident_brick_coords()
        .into_iter()
        .filter_map(|coord| {
            let snapshot = sim
                .world()
                .terrain()
                .volume
                .snapshot_brick(coord)
                .ok()
                .flatten()?;
            let solid = (0..CELLS_PER_BRICK as u16).any(|index| {
                !snapshot
                    .get(LocalCell::from_linear_index(index).unwrap())
                    .is_air()
            });
            solid.then_some(coord)
        })
        .map(|coord| {
            let active = sim
                .world()
                .terrain()
                .volume
                .resident_brick_coords()
                .into_iter()
                .filter(|&candidate| {
                    sim.world()
                        .terrain()
                        .volume
                        .snapshot_brick(candidate)
                        .ok()
                        .flatten()
                        .is_some_and(|snapshot| {
                            (0..CELLS_PER_BRICK as u16).any(|index| {
                                !snapshot
                                    .get(LocalCell::from_linear_index(index).unwrap())
                                    .is_air()
                            })
                        })
                })
                .count();
            (coord, active)
        })
        .next()
        .expect("fixture has a resident solid terrain brick");
    assert!(!sim.world().terrain_brick_colliders_enabled());
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());
    assert!(sim.world().terrain_brick_colliders_enabled());
    assert_eq!(
        sim.world().terrain_brick_collider_count(),
        active - 1,
        "the evicted brick's derived collider was retired"
    );
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("resident colliders match resident revisions");

    assert!(sim.world_mut().reload_brick(terrain, victim).unwrap());
    assert_eq!(
        sim.world().terrain_brick_collider_count(),
        active,
        "reload reinstalls exactly the evicted brick collider"
    );
    sim.world()
        .validate_terrain_brick_colliders()
        .expect("reloaded collider matches the durable brick");
}

#[test]
fn evicted_terrain_has_no_collision_until_its_durable_brick_reloads() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    sim.world_mut().set_backing(Arc::new(backing));
    let victim = BrickCoord::new(0, 0, 0);
    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());

    let falling = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [1.0, 3.0, 1.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..240 {
        sim.step_physics_only();
    }
    assert!(
        sim.world().body(falling).unwrap().pose.translation_m[1] < -2.0,
        "evicted terrain retained a stale collision shape"
    );

    assert!(sim.world_mut().reload_brick(terrain, victim).unwrap());
    let landed = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [1.0, 3.0, 1.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..240 {
        sim.step_physics_only();
    }
    assert!(
        sim.world().body(landed).unwrap().pose.translation_m[1] > 0.0,
        "reloaded terrain did not restore collision"
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
