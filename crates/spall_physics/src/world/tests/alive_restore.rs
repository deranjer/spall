use super::*;

#[test]
fn alive_restore_roundtrips_applied_multibody_motion_and_death_witness() {
    use crate::articulated::{AliveArticulatedRestore, ArticulatedPose};

    let source_parts = alive_restore_fixture(99).parts;
    let mut original = PhysicsWorld::new(PhysicsConfig::default());
    let original_id = original
        .add_articulated_kinematic_parts(
            [3.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            44,
            source_parts.clone(),
            Representation::MergedCuboids,
        )
        .unwrap();
    original
        .update_articulated_pose(
            original_id,
            ArticulatedPose {
                topology_token: 44,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![
                    (1, crate::articulated::PartTransform::IDENTITY),
                    (
                        2,
                        crate::articulated::PartTransform {
                            translation_m: [0.9, 0.0, 0.0],
                            rotation_xyzw: [0.0, 0.0, 0.09983342, 0.9950042],
                        },
                    ),
                    (
                        3,
                        crate::articulated::PartTransform {
                            translation_m: [0.2, 0.7, 0.0],
                            rotation_xyzw: [0.0, 0.0, -0.04997917, 0.99875027],
                        },
                    ),
                ],
            },
        )
        .unwrap();
    original.step();

    let owner = original.articulated_owner_snapshot(original_id).unwrap();
    let applied_pose = original.articulated_pose_snapshot(original_id).unwrap();
    let part_motion = original
        .articulated_part_motion_snapshot(original_id)
        .expect("an applied live pose has a portable part-motion snapshot");
    let root_rigid_velocity = owner.root.linvel_m_s;
    assert!(
        part_motion.iter().any(|part| {
            part.linear_velocity_m_s
                .iter()
                .zip(root_rigid_velocity)
                .any(|(part, root)| (part - root).abs() > 1.0e-4)
                || part
                    .angular_velocity_rad_s
                    .iter()
                    .zip(owner.root.angvel_rad_s)
                    .any(|(part, root)| (part - root).abs() > 1.0e-4)
        }),
        "pose animation must give at least one part a non-rigid root twist"
    );

    let mut restored_parts = source_parts;
    for part in &mut restored_parts {
        part.local_transform = applied_pose
            .parts
            .iter()
            .find(|(part_id, _)| *part_id == part.part_id)
            .unwrap()
            .1;
    }
    let restore = AliveArticulatedRestore {
        owner_key: 99,
        source_generation: owner.source_generation,
        topology_token: owner.topology_token,
        pose_token: owner.pose_token,
        next_component_serial: owner.next_component_serial,
        root_translation_m: owner.root.translation_m,
        root_rotation_xyzw: owner.root.rotation,
        root_linear_velocity_m_s: owner.root.linvel_m_s,
        root_angular_velocity_rad_s: owner.root.angvel_rad_s,
        part_motion: part_motion.clone(),
        parts: restored_parts,
    };
    let mut restored = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = restored
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 7,
                restore,
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        )
        .unwrap();
    let restored_id = restored.commit_articulated_batch(prepared).unwrap()[0].owner_id;
    let restored_owner = restored.articulated_owner_snapshot(restored_id).unwrap();
    assert_eq!(restored_owner.owner_key, Some(99));
    assert_eq!(restored_owner.source_generation, owner.source_generation);
    assert_eq!(restored_owner.next_component_serial, 1);
    assert_eq!(restored_owner.topology_token, owner.topology_token);
    assert_eq!(restored_owner.pose_token, owner.pose_token);
    assert!(!restored_owner.pending_pose);
    assert_eq!(
        restored.articulated_pose_snapshot(restored_id).unwrap(),
        applied_pose
    );
    assert_eq!(
        restored
            .articulated_part_motion_snapshot(restored_id)
            .unwrap(),
        part_motion
    );
    let ray_hit = restored
        .raycast_articulated(restored_id, [2.5, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
        .unwrap();
    assert_eq!(ray_hit.part_id, 1);

    let original_death = original
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 8,
                id: original_id,
                expected_topology_token: owner.topology_token,
                expected_pose_token: owner.pose_token,
            }],
            mix_density,
        )
        .unwrap()
        .transfer_outcome(8)
        .copied()
        .unwrap();
    let restored_death = restored
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 9,
                id: restored_id,
                expected_topology_token: owner.topology_token,
                expected_pose_token: owner.pose_token,
            }],
            mix_density,
        )
        .unwrap()
        .transfer_outcome(9)
        .copied()
        .unwrap();
    assert!((original_death.mass_kg - restored_death.mass_kg).abs() < 1.0e-10);
    for axis in 0..3 {
        assert!(
            (original_death.linear_momentum_kg_m_s[axis]
                - restored_death.linear_momentum_kg_m_s[axis])
                .abs()
                < 1.0e-8
        );
        assert!(
            (original_death.angular_momentum_about_com_kg_m2_s[axis]
                - restored_death.angular_momentum_about_com_kg_m2_s[axis])
                .abs()
                < 1.0e-8
        );
    }
    assert!(
        restored_death
            .linear_velocity_m_s
            .iter()
            .all(|v| v.is_finite())
    );
    assert!(
        restored_death
            .angular_velocity_rad_s
            .iter()
            .all(|v| v.is_finite())
    );
    let restored_outcome = restored
        .commit_articulated_batch(
            restored
                .prepare_articulated_batch(
                    vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: 10,
                        id: restored_id,
                        expected_topology_token: owner.topology_token,
                        expected_pose_token: owner.pose_token,
                    }],
                    mix_density,
                )
                .unwrap(),
        )
        .unwrap()
        .remove(0)
        .transfer
        .unwrap();
    assert!((restored_outcome.mass_kg - restored_death.mass_kg).abs() < 1.0e-10);
    let installed = restored.articulated_body_state(restored_id);
    assert!((f64::from(installed.mass_kg) - restored_outcome.mass_kg).abs() < 1.0e-6);
    for axis in 0..3 {
        assert!(
            (f64::from(installed.linvel_m_s[axis]) - restored_outcome.linear_velocity_m_s[axis])
                .abs()
                < 1.0e-5
        );
        assert!(
            (f64::from(installed.angvel_rad_s[axis])
                - restored_outcome.angular_velocity_rad_s[axis])
                .abs()
                < 1.0e-5
        );
    }
    let transferred_owner = restored.articulated_owner_snapshot(restored_id).unwrap();
    assert_eq!(transferred_owner.owner_key, Some(99));
    assert_eq!(transferred_owner.source_generation, owner.source_generation);
    assert_eq!(transferred_owner.next_component_serial, 1);
    // The queued kinematic reset must not convert an immediately
    // transferred corpse back into a kinematic body on the next step.
    restored.step();
    let assembly = &restored.articulated[&restored_id.0];
    assert!(assembly.dynamic_corpse);
    assert!(!assembly.velocity_based_restore);
    let root = restored.entries[entry_index(restored_id.0, restored.id_namespace)].body;
    assert_eq!(restored.bodies[root].body_type(), RigidBodyType::Dynamic);
}

#[test]
fn alive_motion_snapshot_rejects_pending_root_and_local_pose_frames() {
    use crate::articulated::ArticulatedPose;

    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = world
        .commit_articulated_batch(
            world
                .prepare_articulated_batch(
                    vec![ArticulatedBatchOperation::AliveRestore {
                        operation_tag: 1,
                        restore: alive_restore_fixture(105),
                        representation: Representation::MergedCuboids,
                    }],
                    mix_density,
                )
                .unwrap(),
        )
        .unwrap()[0]
        .owner_id;
    assert!(world.articulated_part_motion_snapshot(id).is_some());

    let root_move = world
        .sweep_articulated_root_translation(id, 44, 1, 2, [3.01, 0.0, 0.0])
        .unwrap();
    assert!(world.articulated_owner_snapshot(id).unwrap().pending_pose);
    assert!(world.articulated_part_motion_snapshot(id).is_none());
    world.step();
    let root_after_root_update = world.articulated_owner_snapshot(id).unwrap();
    assert_eq!(root_after_root_update.pose_token, 2);
    assert!(
        (root_after_root_update.root.translation_m[0] - (3.0 + root_move.applied_translation_m[0]))
            .abs()
            < 1.0e-4
    );
    let applied = world.articulated_pose_snapshot(id).unwrap();
    world
        .update_articulated_pose(
            id,
            ArticulatedPose {
                topology_token: 44,
                base_pose_token: 2,
                pose_token: 3,
                parts: applied
                    .parts
                    .into_iter()
                    .map(|(part_id, mut transform)| {
                        if part_id == 2 {
                            transform.translation_m[0] += 0.01;
                        }
                        (part_id, transform)
                    })
                    .collect(),
            },
        )
        .unwrap();
    assert!(world.articulated_owner_snapshot(id).unwrap().pending_pose);
    assert!(world.articulated_part_motion_snapshot(id).is_none());
}

#[test]
fn alive_restore_twists_are_one_shot_when_next_step_has_no_authored_pose() {
    let mut restore = alive_restore_fixture(106);
    restore.root_linear_velocity_m_s = [30.0, 0.0, 0.0];
    restore.root_angular_velocity_rad_s = [0.0, 0.0, 4.0];
    for motion in &mut restore.part_motion {
        motion.linear_velocity_m_s = [30.0, 0.0, 0.0];
        motion.angular_velocity_rad_s = [0.0, 0.0, 4.0];
    }
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = world
        .commit_articulated_batch(
            world
                .prepare_articulated_batch(
                    vec![ArticulatedBatchOperation::AliveRestore {
                        operation_tag: 1,
                        restore,
                        representation: Representation::MergedCuboids,
                    }],
                    mix_density,
                )
                .unwrap(),
        )
        .unwrap()[0]
        .owner_id;
    let owner_before = world.articulated_owner_snapshot(id).unwrap();
    let pose_before = world.articulated_pose_snapshot(id).unwrap();
    let part_world_poses = world.articulated[&id.0]
        .parts
        .iter()
        .map(|part| (part.body, *world.bodies[part.body].position()))
        .collect::<Vec<_>>();
    assert_eq!(owner_before.root.linvel_m_s, [30.0, 0.0, 0.0]);
    assert!(world.articulated[&id.0].velocity_based_restore);

    world.step();

    let owner_after = world.articulated_owner_snapshot(id).unwrap();
    assert!(!world.articulated[&id.0].velocity_based_restore);
    assert_eq!(
        owner_after.root.translation_m,
        owner_before.root.translation_m
    );
    assert_eq!(owner_after.pose_token, owner_before.pose_token);
    assert!(!owner_after.pending_pose);
    assert_eq!(world.articulated_pose_snapshot(id).unwrap(), pose_before);
    assert_eq!(owner_after.root.linvel_m_s, [0.0; 3]);
    assert_eq!(owner_after.root.angvel_rad_s, [0.0; 3]);
    for (body, pose) in part_world_poses {
        assert_eq!(*world.bodies[body].position(), pose);
    }
    assert_eq!(
        world
            .raycast_articulated(id, [2.5, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
            .unwrap()
            .part_id,
        1
    );
    let death = world
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 2,
                id,
                expected_topology_token: owner_before.topology_token,
                expected_pose_token: owner_before.pose_token,
            }],
            mix_density,
        )
        .unwrap()
        .transfer_outcome(2)
        .copied()
        .unwrap();
    assert!(
        death
            .linear_momentum_kg_m_s
            .iter()
            .all(|value| value.abs() < 1.0e-8)
    );
    assert!(
        death
            .angular_momentum_about_com_kg_m2_s
            .iter()
            .all(|value| value.abs() < 1.0e-8)
    );
}

#[test]
fn alive_restore_accepts_local_animation_before_first_step() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let prepared = world
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 1,
                restore: alive_restore_fixture(107),
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        )
        .unwrap();
    let id = world.commit_articulated_batch(prepared).unwrap()[0].owner_id;
    let mut parts = world.articulated_pose_snapshot(id).unwrap().parts;
    parts[2].1.translation_m[0] = 0.01;
    world
        .update_articulated_pose(
            id,
            crate::articulated::ArticulatedPose {
                topology_token: 44,
                base_pose_token: 1,
                pose_token: 2,
                parts: parts.clone(),
            },
        )
        .unwrap();
    assert!(world.articulated_part_motion_snapshot(id).is_none());
    world.step();
    let pose = world.articulated_pose_snapshot(id).unwrap();
    assert_eq!(pose.pose_token, 2);
    assert_eq!(pose.parts, parts);
    let assembly = &world.articulated[&id.0];
    assert!(!assembly.velocity_based_restore);
    let tip = assembly
        .parts
        .iter()
        .find(|part| part.part_id == 3)
        .unwrap();
    let position = world.bodies[tip.body].translation();
    assert!((position.x - 3.51).abs() < 1.0e-5);
    assert!((position.y - 0.5).abs() < 1.0e-5);
    assert!(position.z.abs() < 1.0e-5);
    let motion = world.articulated_part_motion_snapshot(id).unwrap();
    assert!(motion[2].linear_velocity_m_s[0] > 0.1);
}

#[test]
fn alive_restore_rejects_bad_motion_duplicate_owner_and_stale_batch_atomically() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let before = world.body_count();

    let mut missing_motion = alive_restore_fixture(101);
    missing_motion.part_motion.pop();
    assert!(matches!(
        world.prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 1,
                restore: missing_motion,
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        ),
        Err(crate::articulated::ArticulatedError::InvalidRestore)
    ));

    let mut non_finite = alive_restore_fixture(102);
    non_finite.part_motion[1].angular_velocity_rad_s[2] = f32::INFINITY;
    assert!(matches!(
        world.prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 2,
                restore: non_finite,
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        ),
        Err(crate::articulated::ArticulatedError::InvalidRestore)
    ));

    let duplicate_owner = world.prepare_articulated_batch(
        vec![
            ArticulatedBatchOperation::AliveRestore {
                operation_tag: 3,
                restore: alive_restore_fixture(103),
                representation: Representation::MergedCuboids,
            },
            ArticulatedBatchOperation::AliveRestore {
                operation_tag: 4,
                restore: alive_restore_fixture(103),
                representation: Representation::MergedCuboids,
            },
        ],
        mix_density,
    );
    assert!(matches!(
        duplicate_owner,
        Err(crate::articulated::ArticulatedError::InvalidRestore)
    ));
    assert_eq!(world.body_count(), before);

    let prepared = world
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::AliveRestore {
                operation_tag: 5,
                restore: alive_restore_fixture(104),
                representation: Representation::MergedCuboids,
            }],
            mix_density,
        )
        .unwrap();
    world.step();
    assert!(matches!(
        world.validate_articulated_batch(prepared),
        Err(crate::articulated::ArticulatedError::StalePreparedBatch)
    ));
    assert_eq!(world.body_count(), before);
    assert_eq!(world.body_count(), 0);
}
