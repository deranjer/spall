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
