//! Coordinator-owned public API checks for mixed articulated lifecycle batches.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    ArticulatedBatchOperation, OccupancyGrid, PartTransform, PhysicsConfig, PhysicsWorld,
    Representation, SourcePart,
};

fn parts(first_volume: u64) -> Vec<SourcePart> {
    (0..2)
        .map(|index| SourcePart {
            part_id: index + 1,
            parent_id: (index != 0).then_some(1),
            volume_id: VolumeId::new(first_volume + u64::from(index)).unwrap(),
            source_revision: 3,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(-1, 0, 0),
                [1, 1, 1],
                vec![true],
                vec![MaterialId(1)],
            )
            .unwrap(),
            cell_m: 0.25,
            local_transform: PartTransform {
                translation_m: [index as f32 * 0.5, 0.0, 0.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            },
        })
        .collect()
}

fn birth(tag: u64, first_volume: u64) -> ArticulatedBatchOperation {
    ArticulatedBatchOperation::AliveBirth {
        operation_tag: tag,
        translation_m: [4.0, 2.0, 0.0],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        topology_token: 7,
        parts: parts(first_volume),
        representation: Representation::MergedCuboids,
    }
}

#[test]
fn generic_birth_is_living_and_death_retains_lifetime_source_ids() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = world
        .prepare_articulated_batch(vec![birth(17, 701)], |_| 900.0)
        .unwrap();
    assert_eq!(world.body_count(), 0);
    let born = world.commit_articulated_batch(prepared).unwrap();
    assert_eq!(born.len(), 1);
    assert_eq!(born[0].operation_tag, 17);
    assert!(born[0].transfer.is_none());
    let owner = born[0].owner_id;
    assert_eq!(
        world.articulated_rigid_body_count(owner),
        3,
        "living owner retains an aggregate root plus two kinematic part bodies"
    );
    let prepared = world
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 18,
                id: owner,
                expected_topology_token: 7,
                expected_pose_token: 0,
            }],
            |_| 900.0,
        )
        .unwrap();
    assert_eq!(world.articulated_rigid_body_count(owner), 3);
    let dead = world.commit_articulated_batch(prepared).unwrap();
    assert_eq!(dead[0].operation_tag, 18);
    assert_eq!(dead[0].owner_id, owner);
    assert!(dead[0].transfer.is_some());
    assert_eq!(world.articulated_rigid_body_count(owner), 1);
    assert!((world.articulated_body_state(owner).mass_kg - 28.125).abs() < 1.0e-4);
    let before = world.body_count();
    assert!(
        world
            .prepare_articulated_batch(vec![birth(19, 701)], |_| 900.0)
            .is_err()
    );
    assert_eq!(
        world.body_count(),
        before,
        "death does not release original source ownership"
    );
    assert_eq!(world.articulated_rigid_body_count(owner), 1);
}

#[test]
fn public_batch_rejects_density_change_and_duplicate_tags_without_publishing_birth() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let born = world
        .prepare_articulated_batch(vec![birth(1, 801)], |_| 900.0)
        .unwrap();
    let owner = world.commit_articulated_batch(born).unwrap()[0].owner_id;
    let before = world.body_count();
    assert!(
        world
            .prepare_articulated_batch(
                vec![
                    birth(2, 803),
                    ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 3,
                        id: owner,
                        expected_topology_token: 7,
                        expected_pose_token: 0,
                    },
                ],
                |_| 1000.0
            )
            .is_err()
    );
    assert_eq!(world.body_count(), before);
    assert_eq!(world.articulated_rigid_body_count(owner), 3);
    assert!(
        world
            .prepare_articulated_batch(vec![birth(4, 803), birth(4, 805)], |_| 900.0)
            .is_err()
    );
    assert_eq!(world.body_count(), before);
    assert_eq!(world.articulated_rigid_body_count(owner), 3);
    let retry = world
        .prepare_articulated_batch(vec![birth(5, 803)], |_| 900.0)
        .unwrap();
    assert_eq!(
        world.commit_articulated_batch(retry).unwrap()[0].operation_tag,
        5
    );
}
