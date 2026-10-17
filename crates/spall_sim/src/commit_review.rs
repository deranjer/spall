//! Worker-owned regressions for the composable geometry commit boundary.
use super::*;

use crate::stage::StageInput;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CellSizeCode, EntityId, GlobalCell, MaterialId, SphereBrush, VolumeId};
use spall_jobs::{Generation, TopologyEpoch};
use spall_protocol::Hash32;
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, EvictedBricks, Volume};

fn brush(x: i64, y: i64, z: i64, radius: i64) -> SphereBrush {
    let half = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            x * BRUSH_UNIT + half,
            y * BRUSH_UNIT + half,
            z * BRUSH_UNIT + half,
        ),
        radius * BRUSH_UNIT,
    )
    .unwrap()
}

fn staged_bridged_cut() -> (SimWorld, StagedEdit) {
    let setup = crate::fixtures::bridged_terrain_setup();
    let terrain = setup.terrain.clone();
    let world = SimWorld::new(setup).unwrap();
    let volume = terrain.id();
    let intent = crate::intent::EditIntent::cut(
        spall_protocol::RequestId(1),
        EntityId::new(1).unwrap(),
        crate::intent::EditTarget::Terrain,
        brush(10, 4, 1, 2),
    );
    let input = StageInput::new(
        &intent,
        volume,
        terrain,
        EvictedBricks::new(),
        AnchorPlane::at(0),
        Generation::START,
        TopologyEpoch::START,
    );
    let staged = crate::stage::stage_edit(&input).unwrap();
    assert!(staged.splits());
    (world, staged)
}

fn source_part(
    part_id: u32,
    parent_id: Option<u32>,
    volume_id: VolumeId,
    translation_m: [f32; 3],
) -> spall_physics::SourcePart {
    let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            volume_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    spall_physics::SourcePart {
        part_id,
        parent_id,
        volume_id,
        source_revision: 1,
        grid: spall_physics::occupancy::OccupancyGrid::from_volume(&volume)
            .unwrap()
            .unwrap(),
        cell_m: 0.25,
        local_transform: spall_physics::PartTransform {
            translation_m,
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
    }
}

fn world_snapshot(world: &SimWorld, journal: &JournalSink) -> (Hash32, u64, (u64, u64, u64, u64)) {
    (
        world.world_hash(),
        journal.cursor(),
        world.registry().counters(),
    )
}

#[test]
fn preparation_owns_candidate_without_publishing_anything() {
    let (world, staged) = staged_bridged_cut();
    let journal = JournalSink::new();
    let before = world_snapshot(&world, &journal);
    let body_count = world.physics().body_count();
    let prepared = prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap();
    assert!(matches!(prepared, PrepareGeometryOutcome::Ready(_)));
    assert_eq!(world_snapshot(&world, &journal), before);
    assert_eq!(world.physics().body_count(), body_count);
    assert!(journal.is_empty());
}

#[test]
fn late_candidate_failure_leaves_world_registry_and_journal_untouched() {
    let (world, mut staged) = staged_bridged_cut();
    staged.explosion = Some(crate::intent::ExplosionImpulse {
        magnitude_ns: f64::NAN,
        direction: [1.0, 0.0, 0.0],
    });
    let journal = JournalSink::new();
    let before = world_snapshot(&world, &journal);
    let body_count = world.physics().body_count();
    assert!(prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).is_err());
    assert_eq!(world_snapshot(&world, &journal), before);
    assert_eq!(world.physics().body_count(), body_count);
}

fn prepared_candidate() -> (SimWorld, JournalSink, PreparedGeometryCommit) {
    let (world, staged) = staged_bridged_cut();
    let journal = JournalSink::new();
    let PrepareGeometryOutcome::Ready(prepared) =
        prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fixture token unexpectedly stale");
    };
    (world, journal, *prepared)
}

fn prepared_dynamic_candidate() -> (SimWorld, JournalSink, PreparedGeometryCommit) {
    let mut world = SimWorld::new(crate::fixtures::flat_terrain_setup()).unwrap();
    let entity = world
        .spawn_body(
            |id| {
                let mut volume = Volume::new(id, CellSizeCode::Quarter);
                volume
                    .apply_edit(&EditPlan::filled_box(
                        id,
                        GlobalCell::new(0, 0, 0),
                        GlobalCell::new(4, 4, 4),
                        MaterialId(1),
                    ))
                    .unwrap();
                volume
            },
            crate::body::BodyPose::new(glam::DQuat::IDENTITY, [8.0, 5.0, 8.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let body = world.body(entity).unwrap();
    let volume_id = body.volume_id;
    let snapshot = body.volume.clone();
    let intent = crate::intent::EditIntent::cut(
        spall_protocol::RequestId(2),
        EntityId::new(1).unwrap(),
        crate::intent::EditTarget::Body(entity),
        brush(1, 1, 1, 0),
    );
    let input = StageInput::new(
        &intent,
        volume_id,
        snapshot,
        EvictedBricks::new(),
        AnchorPlane::at(0),
        world.generation(),
        world.topology_epoch(),
    );
    let staged = crate::stage::stage_edit(&input).unwrap();
    let journal = JournalSink::new();
    let PrepareGeometryOutcome::Ready(prepared) =
        prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fixture token unexpectedly stale");
    };
    (world, journal, *prepared)
}

#[test]
fn prepared_fence_rejects_solver_step_drift() {
    let (mut world, journal, prepared) = prepared_candidate();
    world.step_physics();
    let before = world_snapshot(&world, &journal);
    assert!(matches!(
        validate_prepared_geometry(&world, &journal, prepared),
        Err(CommitError::PreparedStateChanged)
    ));
    assert_eq!(world_snapshot(&world, &journal), before);
}

#[test]
fn prepared_fence_rejects_another_identical_world_instance() {
    let (world, journal, prepared) = prepared_candidate();
    let other = SimWorld::new(crate::fixtures::bridged_terrain_setup()).unwrap();
    assert_eq!(world.world_hash(), other.world_hash());
    assert_eq!(world.registry().counters(), other.registry().counters());
    assert_eq!(world.topology_epoch(), other.topology_epoch());
    assert!(matches!(
        validate_prepared_geometry(&other, &journal, prepared),
        Err(CommitError::PreparedStateChanged)
    ));
}

#[test]
fn prepared_fence_rejects_physics_origin_drift() {
    let (mut world, journal, prepared) = prepared_candidate();
    world.set_physics_origin_for_test(spall_physics::PhysicsOrigin::new([1.0, 0.0, 0.0]).unwrap());
    let before = world_snapshot(&world, &journal);
    assert!(matches!(
        validate_prepared_geometry(&world, &journal, prepared),
        Err(CommitError::PreparedStateChanged)
    ));
    assert_eq!(world_snapshot(&world, &journal), before);
}

#[test]
fn prepared_fence_rejects_journal_cursor_drift_before_publication() {
    let (world, mut journal, prepared) = prepared_candidate();
    journal.append(JournalEntry {
        seq: spall_core::JournalSeq(1),
        transaction: TopologyTransaction {
            transaction_id: spall_core::TransactionId::new(1).unwrap(),
            server_tick: Tick(0),
            control_seq: ControlSeq(0),
            algorithm_version: ALGORITHM_VERSION,
            dependencies: Vec::new(),
            before: Vec::new(),
            after: Vec::new(),
            ops: Vec::new(),
            result_hashes: Vec::new(),
        },
        participants: Vec::new(),
        bulk_baseline: None,
    });
    let before = world_snapshot(&world, &journal);
    assert!(matches!(
        validate_prepared_geometry(&world, &journal, prepared),
        Err(CommitError::PreparedStateChanged)
    ));
    assert_eq!(world_snapshot(&world, &journal), before);
}

#[test]
fn prepared_fence_rejects_same_step_target_pose_or_twist_changes() {
    for drift in 0..3 {
        let (mut world, journal, prepared) = prepared_dynamic_candidate();
        let phys = world.volume_body(prepared.volume).unwrap().phys;
        match drift {
            0 => world
                .physics_mut()
                .set_body_pose(phys, [0.25, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0]),
            1 => world
                .physics_mut()
                .set_body_velocity(phys, [0.5, 0.0, 0.0], [0.0, 0.0, 0.25]),
            _ => {
                let grid = spall_physics::OccupancyGrid::from_volume(
                    world.volume_ref(prepared.volume).unwrap(),
                )
                .unwrap()
                .unwrap();
                let representation = world.physics().representation(phys);
                world
                    .physics_mut()
                    .rebuild_collider(phys, &grid, representation);
            }
        }
        let before = world_snapshot(&world, &journal);
        assert!(matches!(
            validate_prepared_geometry(&world, &journal, prepared),
            Err(CommitError::PreparedStateChanged)
        ));
        assert_eq!(world_snapshot(&world, &journal), before);
    }
}

#[test]
fn prepared_fence_rejects_registry_and_geometry_drift() {
    {
        let (mut world, journal, prepared) = prepared_candidate();
        world.registry_mut().allocate_entity().unwrap();
        let before = world_snapshot(&world, &journal);
        assert!(matches!(
            validate_prepared_geometry(&world, &journal, prepared),
            Err(CommitError::PreparedStateChanged)
        ));
        assert_eq!(world_snapshot(&world, &journal), before);
    }
    {
        let (mut world, journal, prepared) = prepared_candidate();
        world
            .volume_body_mut(VolumeId::new(1).unwrap())
            .unwrap()
            .volume
            .apply_edit(&EditPlan::filled_box(
                VolumeId::new(1).unwrap(),
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                MaterialId::AIR,
            ))
            .unwrap();
        let before = world_snapshot(&world, &journal);
        assert!(matches!(
            validate_prepared_geometry(&world, &journal, prepared),
            Err(CommitError::PreparedStateChanged)
        ));
        assert_eq!(world_snapshot(&world, &journal), before);
    }
}

#[test]
fn prepared_fence_tracks_affected_per_brick_terrain_colliders() {
    let (mut world, journal, prepared) = prepared_candidate();
    assert!(world.terrain_brick_colliders_enabled());
    let entries = prepared.fence.terrain_bricks_for_test();
    assert!(
        !entries.is_empty(),
        "terrain cut must fence affected bricks"
    );
    let Some((_, Some((body, _, true)))) = entries
        .iter()
        .find(|(_, entry)| entry.is_some_and(|(_, _, has_collider)| has_collider))
    else {
        panic!("fixture must include an affected terrain brick collider");
    };
    world
        .physics_mut()
        .set_body_pose(*body, [0.5, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0]);
    let before = world_snapshot(&world, &journal);
    assert!(matches!(
        validate_prepared_geometry(&world, &journal, prepared),
        Err(CommitError::PreparedStateChanged)
    ));
    assert_eq!(world_snapshot(&world, &journal), before);
}

#[test]
fn singleton_commit_keeps_existing_transaction_journal_hash_and_split_ids() {
    let (mut world, staged) = staged_bridged_cut();
    let before_solid = world.total_solid_cells();
    let mut journal = JournalSink::new();
    let CommitOutcome::Committed(committed) =
        commit(&mut world, &mut journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fresh fixture token unexpectedly stale");
    };
    assert_eq!(committed.transaction.get(), 1);
    assert_eq!(committed.journal_seq.0, 1);
    assert!(committed.children.len() == 1);
    assert_eq!(journal.cursor(), 1);
    assert_eq!(journal.entries()[0].transaction, committed.topology);
    assert!(world.validate_terrain_brick_colliders().is_ok());
    assert_eq!(
        world.total_solid_cells(),
        before_solid - staged.ledger.destroyed
    );
    for result in &committed.topology.result_hashes {
        assert_eq!(world.volume_hash(result.volume), Some(result.hash));
    }
    assert_eq!(
        world.registry().counters(),
        (2, 3, 2, 2),
        "entity/volume and transaction/journal sequences retain their legacy allocation order"
    );
}

#[test]
fn validated_geometry_can_publish_before_the_shared_journal_append() {
    let (mut world, staged) = staged_bridged_cut();
    let mut journal = JournalSink::new();
    let before = world.world_hash();
    let PrepareGeometryOutcome::Ready(prepared) =
        prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fixture token unexpectedly stale");
    };
    let validated = validate_prepared_geometry(&world, &journal, *prepared).unwrap();
    let (committed, entry) = publish_validated_geometry_without_journal(&mut world, validated);

    assert_ne!(world.world_hash(), before);
    assert_eq!(committed.topology, entry.transaction);
    assert_eq!(committed.journal_seq, entry.seq);
    assert!(journal.is_empty());

    journal.append(entry);
    assert_eq!(journal.cursor(), committed.journal_seq.0);
    assert_eq!(journal.entries()[0].transaction, committed.topology);
}

#[test]
fn geometry_and_articulated_birth_can_validate_then_publish_before_one_journal_append() {
    let (mut world, staged) = staged_bridged_cut();
    let mut journal = JournalSink::new();
    let PrepareGeometryOutcome::Ready(prepared_geometry) =
        prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fixture token unexpectedly stale");
    };
    let prepared_articulated = world
        .physics()
        .prepare_articulated_batch(
            vec![spall_physics::ArticulatedBatchOperation::AliveBirth {
                operation_tag: 77,
                translation_m: [20.0, 10.0, 20.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                topology_token: 1,
                parts: vec![
                    source_part(1, None, VolumeId::new(100).unwrap(), [0.0; 3]),
                    source_part(2, Some(1), VolumeId::new(101).unwrap(), [1.0, 0.0, 0.0]),
                ],
                representation: spall_physics::Representation::MergedCuboids,
            }],
            |_| 1.0,
        )
        .unwrap();

    // Both capabilities are checked while their shared PhysicsWorld is still
    // at its captured state. Geometry publication adds a child body, so the
    // articulated publisher must accept that explicitly permitted interleave.
    let validated_geometry =
        validate_prepared_geometry(&world, &journal, *prepared_geometry).unwrap();
    let validated_articulated = world
        .physics()
        .validate_articulated_batch(prepared_articulated)
        .unwrap();
    let (committed, entry) =
        publish_validated_geometry_without_journal(&mut world, validated_geometry);
    let outcomes = world
        .physics_mut()
        .publish_validated_articulated_batch(validated_articulated);

    let outcome = outcomes
        .iter()
        .find(|outcome| outcome.operation_tag == 77)
        .unwrap();
    assert_eq!(
        world.physics().articulated_collider_count(outcome.owner_id),
        2
    );
    assert_eq!(
        world
            .physics()
            .articulated_rigid_body_count(outcome.owner_id),
        3
    );
    let hit = world
        .physics_mut()
        .raycast_articulated(outcome.owner_id, [21.1, 10.1, 19.0], [0.0, 0.0, 1.0], 3.0)
        .unwrap();
    assert_eq!(hit.part_id, 2);
    assert_eq!(hit.volume_id, VolumeId::new(101).unwrap());
    for result in &committed.topology.result_hashes {
        assert_eq!(world.volume_hash(result.volume), Some(result.hash));
    }
    assert_eq!(journal.cursor(), 0);
    assert_eq!(committed.journal_seq.0, 1);
    assert_eq!(entry.seq, committed.journal_seq);
    journal.append(entry);
    assert_eq!(journal.cursor(), 1);
    assert_eq!(journal.entries()[0].transaction, committed.topology);
}

#[test]
fn failed_joint_validation_publishes_neither_prepared_participant() {
    let (mut world, staged) = staged_bridged_cut();
    let journal = JournalSink::new();
    let PrepareGeometryOutcome::Ready(prepared_geometry) =
        prepare_geometry(&world, &journal, &staged, Tick(1), ControlSeq(1)).unwrap()
    else {
        panic!("fixture token unexpectedly stale");
    };
    let prepared_articulated = world
        .physics()
        .prepare_articulated_batch(
            vec![spall_physics::ArticulatedBatchOperation::AliveBirth {
                operation_tag: 78,
                translation_m: [20.0, 10.0, 20.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                topology_token: 1,
                parts: vec![
                    source_part(1, None, VolumeId::new(100).unwrap(), [0.0; 3]),
                    source_part(2, Some(1), VolumeId::new(101).unwrap(), [1.0, 0.0, 0.0]),
                ],
                representation: spall_physics::Representation::MergedCuboids,
            }],
            |_| 1.0,
        )
        .unwrap();

    // Force only the geometry capability stale. The articulated token is
    // still valid, but the joint caller must publish neither side.
    world.registry_mut().allocate_entity().unwrap();
    let before = world_snapshot(&world, &journal);
    let body_count = world.physics().body_count();
    assert!(matches!(
        validate_prepared_geometry(&world, &journal, *prepared_geometry),
        Err(CommitError::PreparedStateChanged)
    ));
    assert!(
        world
            .physics()
            .validate_articulated_batch(prepared_articulated)
            .is_ok()
    );
    assert_eq!(world_snapshot(&world, &journal), before);
    assert_eq!(world.physics().body_count(), body_count);
    assert!(journal.is_empty());
}
