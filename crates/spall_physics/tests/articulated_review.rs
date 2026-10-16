use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    ArticulatedPose, OccupancyGrid, PartTransform, PhysicsConfig, PhysicsWorld, Representation,
    SourcePart, articulated_mass_properties,
};

fn part(
    id: u32,
    parent: Option<u32>,
    origin: [i64; 3],
    mask: Vec<bool>,
    pose: PartTransform,
) -> SourcePart {
    let material = mask
        .iter()
        .map(|solid| {
            if *solid {
                MaterialId(1)
            } else {
                MaterialId::AIR
            }
        })
        .collect();
    SourcePart {
        part_id: id,
        parent_id: parent,
        volume_id: VolumeId::new(100 + u64::from(id)).unwrap(),
        source_revision: 7,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(origin[0], origin[1], origin[2]),
            [mask.len() as u32, 1, 1],
            mask,
            material,
        )
        .unwrap(),
        cell_m: 1.0,
        local_transform: pose,
    }
}

fn world() -> PhysicsWorld {
    PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        disable_ccd: true,
        ..PhysicsConfig::default()
    })
}

#[test]
fn living_rig_transfer_is_atomic_conserves_sampled_motion_and_dynamic_pose_apis_reject() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let source = vec![
            part(
                1,
                None,
                [-1, 0, 0],
                vec![true, true],
                PartTransform::IDENTITY,
            ),
            part(
                2,
                Some(1),
                [2, 0, 0],
                vec![true],
                PartTransform {
                    translation_m: [3.0, 0.0, 0.0],
                    rotation_xyzw: PartTransform::IDENTITY.rotation_xyzw,
                },
            ),
        ];
        let id = world
            .add_articulated_kinematic_parts(
                [0.0, 4.0, 0.0],
                PartTransform::IDENTITY.rotation_xyzw,
                9,
                source.clone(),
                representation,
            )
            .unwrap();
        world.step();
        let pose = PartTransform {
            translation_m: [4.0, 0.0, 0.0],
            ..source[1].local_transform
        };
        world
            .update_articulated_pose_with_swept_root_pose(
                id,
                ArticulatedPose {
                    topology_token: 9,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(1, source[0].local_transform), (2, pose)],
                },
                PartTransform {
                    translation_m: [0.0, 4.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            )
            .unwrap();
        assert_eq!(
            world
                .transfer_articulated_to_dynamic_corpse(id, 9, 0, |_| 1_000.0)
                .unwrap_err(),
            spall_physics::ArticulatedError::PendingRootPose
        );
        assert_eq!(world.articulated_rigid_body_count(id), 3);
        world.step();
        let stale = world
            .transfer_articulated_to_dynamic_corpse(id, 8, 1, |_| 1000.0)
            .unwrap_err();
        assert_eq!(stale, spall_physics::ArticulatedError::StaleTopology);
        assert_eq!(
            world.articulated_rigid_body_count(id),
            3,
            "failed transfer retains old rig"
        );
        assert_eq!(
            world
                .transfer_articulated_to_dynamic_corpse(id, 9, 1, |_| f64::NAN)
                .unwrap_err(),
            spall_physics::ArticulatedError::InvalidDensity
        );
        assert_eq!(world.articulated_rigid_body_count(id), 3);
        let transfer = world
            .transfer_articulated_to_dynamic_corpse(id, 9, 1, |_| 1000.0)
            .unwrap();
        assert!(
            transfer
                .linear_momentum_kg_m_s
                .iter()
                .any(|v| v.abs() > 1e-4)
        );
        assert_eq!(world.articulated_rigid_body_count(id), 1);
        assert_eq!(world.articulated_collider_count(id), 2);
        let hit = world
            .raycast_articulated(id, [0.5, 4.5, -2.0], [0.0, 0.0, 1.0], 4.0)
            .unwrap();
        assert_eq!(
            (hit.part_id, hit.volume_id, hit.source_revision),
            (1, VolumeId::new(101).unwrap(), 7)
        );
        assert_eq!(
            world
                .update_articulated_pose(
                    id,
                    ArticulatedPose {
                        topology_token: 9,
                        base_pose_token: 1,
                        pose_token: 2,
                        parts: vec![(1, PartTransform::IDENTITY), (2, pose)]
                    }
                )
                .unwrap_err(),
            spall_physics::ArticulatedError::DynamicCorpsePose
        );
    }
}

#[test]
fn empty_source_parts_retire_atomically_without_moving_survivors_then_last_part_retires_body() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let source = vec![
            part(1, None, [0, 0, 0], vec![true], PartTransform::IDENTITY),
            part(
                2,
                Some(1),
                [4, 0, 0],
                vec![true],
                PartTransform {
                    translation_m: [5.0, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            ),
        ];
        let id = world
            .add_dynamic_corpse_aggregate(
                [0.0, 4.0, 0.0],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                source.clone(),
                representation,
                |_| 1000.0,
            )
            .unwrap();
        let before = world
            .raycast_articulated(id, [0.5, 4.5, -2.0], [0.0, 0.0, 1.0], 4.0)
            .unwrap();
        let empty = OccupancyGrid::from_solid_mask(
            GlobalCell::new(4, 0, 0),
            [1, 1, 1],
            vec![false],
            vec![MaterialId::AIR],
        )
        .unwrap();
        let retired = world
            .rebuild_dynamic_corpse_part_grid(id, 1, 2, 2, 7, 8, empty, |_| 1000.0)
            .unwrap();
        assert!(matches!(
            retired,
            spall_physics::CorpseEditOutcome::PartRetired { .. }
        ));
        assert_eq!(world.articulated_rigid_body_count(id), 1);
        assert_eq!(world.articulated_collider_count(id), 1);
        assert_eq!(
            world.articulated_body_state(id).translation_m,
            [0.0, 4.0, 0.0]
        );
        assert_eq!(
            world
                .raycast_articulated(id, [0.5, 4.5, -2.0], [0.0, 0.0, 1.0], 4.0)
                .unwrap()
                .cell,
            before.cell
        );
        let empty = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 1, 1],
            vec![false],
            vec![MaterialId::AIR],
        )
        .unwrap();
        let retired = world
            .rebuild_dynamic_corpse_part_grid(id, 2, 3, 1, 7, 8, empty, |_| 1000.0)
            .unwrap();
        assert!(matches!(
            retired,
            spall_physics::CorpseEditOutcome::AggregateRetired { .. }
        ));
        assert_eq!(world.articulated_rigid_body_count(id), 0);
        assert_eq!(world.articulated_collider_count(id), 0);
    }
}

#[test]
fn frozen_pose_corpse_aggregate_is_one_editable_dynamic_owner_with_source_hits() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let source = vec![
            part(
                1,
                None,
                [-2, 0, 0],
                vec![true, true],
                PartTransform::IDENTITY,
            ),
            part(
                2,
                Some(1),
                [0, 0, 0],
                vec![true, true],
                PartTransform {
                    translation_m: [3.0, 0.0, 0.0],
                    rotation_xyzw: [
                        0.0,
                        0.0,
                        std::f32::consts::FRAC_1_SQRT_2,
                        std::f32::consts::FRAC_1_SQRT_2,
                    ],
                },
            ),
        ];
        let poses = [PartTransform::IDENTITY, source[1].local_transform];
        let mass = articulated_mass_properties(&source, &poses, |_| 1_000.0)
            .unwrap()
            .to_body_properties();
        assert!((mass.mass_kg - 4_000.0).abs() < 0.1);
        assert!(
            mass.local_com_m[0] > 0.5 && mass.inertia_com_kg_m2[0][1].abs() > 1.0,
            "off-center cells should shift COM: {mass:?}"
        );

        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let corpse = world
            .add_dynamic_corpse_aggregate(
                [0.0, 3.0, 0.0],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                source.clone(),
                representation,
                |_| 1_000.0,
            )
            .unwrap();
        assert_eq!(world.articulated_rigid_body_count(corpse), 1);
        assert_eq!(world.articulated_collider_count(corpse), 2);
        assert!(world.articulated_body_state(corpse).mass_kg > 0.0);
        // Exact source queries report source IDs/cells before and after solver motion.
        let hit = world
            .raycast_articulated(corpse, [-1.5, 3.5, -2.0], [0.0, 0.0, 1.0], 4.0)
            .unwrap();
        assert_eq!(
            (hit.part_id, hit.volume_id, hit.cell),
            (1, VolumeId::new(101).unwrap(), [-2, 0, 0])
        );
        for _ in 0..30 {
            world.step();
        }
        assert!(world.articulated_body_state(corpse).translation_m[1] < 3.0);
        let after = world
            .raycast_articulated(corpse, [-1.5, 2.0, -2.0], [0.0, 0.0, 1.0], 4.0)
            .unwrap();
        assert_eq!(
            (after.part_id, after.volume_id, after.cell),
            (1, VolumeId::new(101).unwrap(), [-2, 0, 0])
        );
        let edited_grid = part(
            1,
            None,
            [-2, 0, 0],
            vec![false, true],
            PartTransform::IDENTITY,
        )
        .grid;
        let mut edited_sources = source.clone();
        edited_sources[0].grid = edited_grid.clone();
        edited_sources[0].source_revision = 8;
        let body_origin_before_edit = world.articulated_body_state(corpse).translation_m;
        assert_eq!(
            world
                .rebuild_dynamic_corpse_part_grid(
                    corpse,
                    1,
                    2,
                    1,
                    7,
                    8,
                    edited_grid.clone(),
                    |_| f64::NAN
                )
                .unwrap_err(),
            spall_physics::ArticulatedError::InvalidDensity
        );
        assert_eq!(
            world.articulated_body_state(corpse).translation_m,
            body_origin_before_edit
        );
        assert_eq!(
            world
                .raycast_articulated(
                    corpse,
                    [-1.5, body_origin_before_edit[1] + 0.5, -2.0],
                    [0.0, 0.0, 1.0],
                    4.0
                )
                .unwrap()
                .cell,
            [-2, 0, 0]
        );
        world
            .rebuild_dynamic_corpse_part_grid(corpse, 1, 2, 1, 7, 8, edited_grid.clone(), |_| {
                1_000.0
            })
            .unwrap();
        assert_eq!(
            world.articulated_body_state(corpse).translation_m,
            body_origin_before_edit
        );
        assert!(
            world
                .raycast_articulated(
                    corpse,
                    [-1.5, body_origin_before_edit[1] + 0.5, -2.0],
                    [0.0, 0.0, 1.0],
                    4.0,
                )
                .is_err()
        );
        let survivor = world
            .raycast_articulated(
                corpse,
                [-0.5, body_origin_before_edit[1] + 0.5, -2.0],
                [0.0, 0.0, 1.0],
                4.0,
            )
            .unwrap();
        assert_eq!((survivor.cell, survivor.source_revision), ([-1, 0, 0], 8));
        assert_eq!(
            world
                .rebuild_dynamic_corpse_part_grid(
                    corpse,
                    1,
                    3,
                    1,
                    8,
                    9,
                    source[0].grid.clone(),
                    |_| 1_000.0,
                )
                .unwrap_err(),
            spall_physics::ArticulatedError::StaleTopology,
        );
        let after_stale = world
            .raycast_articulated(
                corpse,
                [-0.5, body_origin_before_edit[1] + 0.5, -2.0],
                [0.0, 0.0, 1.0],
                4.0,
            )
            .unwrap();
        assert_eq!(after_stale, survivor);
    }
}

#[test]
fn negative_source_cells_keep_a_thin_gap_for_both_exact_shapes() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                9,
                vec![part(
                    1,
                    None,
                    [-2, 0, 0],
                    vec![true, false, true],
                    PartTransform::IDENTITY,
                )],
                representation,
            )
            .unwrap();
        // Independent cell boxes are [-2,-1] and [0,1] on X, [0,1] on Y/Z.
        for (x, expected) in [(-1.5, [-2, 0, 0]), (0.5, [0, 0, 0])] {
            let hit = world
                .raycast_articulated(body, [x, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .unwrap();
            assert_eq!(hit.cell, expected);
            assert_eq!(hit.volume_id, VolumeId::new(101).unwrap());
            assert_eq!(hit.source_revision, 7);
            assert!((hit.toi_m - 2.0).abs() < 1.0e-5);
        }
        assert!(
            world
                .raycast_articulated(body, [-0.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .is_err()
        );
    }
}

#[test]
fn nested_quarter_turn_matches_independently_calculated_cell_box() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let parent = PartTransform {
            translation_m: [5.0, 0.0, 0.0],
            rotation_xyzw: [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
        };
        let joint = PartTransform {
            translation_m: [2.0, 0.0, 0.0],
            ..PartTransform::IDENTITY
        };
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                3,
                vec![
                    part(1, None, [0, 8, 0], vec![true], parent),
                    part(2, Some(1), [0, 4, 0], vec![true], joint),
                    part(3, Some(2), [-2, 0, 0], vec![true], joint),
                ],
                representation,
            )
            .unwrap();
        // Grandchild starts at X=[-2,-1]. Two +2 joints give [2,3].
        // Rz(90) maps (x,y)->(-y,x), then root +5 gives X=[4,5], Y=[2,3].
        let hit = world
            .raycast_articulated(body, [4.5, 2.5, -3.0], [0.0, 0.0, 1.0], 8.0)
            .unwrap();
        assert_eq!(hit.part_id, 3);
        assert_eq!(hit.cell, [-2, 0, 0]);
        assert!((hit.toi_m - 3.0).abs() < 1.0e-5);
        assert!(
            world
                .raycast_articulated(body, [5.5, 2.5, -3.0], [0.0, 0.0, 1.0], 8.0)
                .is_err()
        );
    }
}

#[test]
fn pose_install_removes_rest_hits_and_rejects_stale_or_invalid_updates_atomically() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                11,
                vec![part(1, None, [0; 3], vec![true], PartTransform::IDENTITY)],
                representation,
            )
            .unwrap();
        world.step();
        let shifted = PartTransform {
            translation_m: [4.0, 0.0, 0.0],
            ..PartTransform::IDENTITY
        };
        world
            .update_articulated_pose(
                body,
                ArticulatedPose {
                    topology_token: 11,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(1, shifted)],
                },
            )
            .unwrap();
        assert!(
            world
                .raycast_articulated(body, [0.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .is_err()
        );
        let accepted = world
            .raycast_articulated(body, [4.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
            .unwrap();
        assert_eq!(accepted.pose_token, 1);
        for (base, token, transform) in [
            (0, 2, PartTransform::IDENTITY),
            (1, 1, PartTransform::IDENTITY),
            (
                1,
                2,
                PartTransform {
                    translation_m: [f32::NAN, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            ),
            (
                1,
                2,
                PartTransform {
                    rotation_xyzw: [f32::MAX; 4],
                    ..PartTransform::IDENTITY
                },
            ),
        ] {
            assert!(
                world
                    .update_articulated_pose(
                        body,
                        ArticulatedPose {
                            topology_token: 11,
                            base_pose_token: base,
                            pose_token: token,
                            parts: vec![(1, transform)]
                        }
                    )
                    .is_err()
            );
            assert_eq!(
                world
                    .raycast_articulated(body, [4.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                    .unwrap(),
                accepted
            );
        }
    }
}

#[test]
fn overlapping_parts_tie_by_stable_part_id_not_input_order() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![
                    part(2, Some(1), [0; 3], vec![true], PartTransform::IDENTITY),
                    part(1, None, [0; 3], vec![true], PartTransform::IDENTITY),
                ],
                representation,
            )
            .unwrap();
        let hit = world
            .raycast_articulated(body, [0.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
            .unwrap();
        assert_eq!(hit.part_id, 1);
        assert_eq!(hit.cell, [0; 3]);
        assert_eq!(hit.volume_id, VolumeId::new(101).unwrap());
    }
}

#[test]
fn ray_started_on_an_internal_cell_face_uses_the_declared_lexicographic_tie() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![part(
                    1,
                    None,
                    [-1, 0, 0],
                    vec![true, true],
                    PartTransform::IDENTITY,
                )],
                representation,
            )
            .unwrap();
        for direction in [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 1.0]] {
            let hit = world
                .raycast_articulated(body, [0.0, 0.5, 0.5], direction, 4.0)
                .unwrap();
            assert_eq!(hit.toi_m, 0.0);
            assert_eq!(hit.cell, [-1, 0, 0]);
        }
    }
}

#[test]
fn scheduled_root_pose_keeps_current_hit_epoch_until_the_physics_boundary() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let body = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![part(
                    1,
                    None,
                    [1, 0, 0],
                    vec![true],
                    PartTransform::IDENTITY,
                )],
                representation,
            )
            .unwrap();
        let quarter_turn = [
            0.0,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        ];
        world
            .set_next_articulated_root_pose(body, 1, 0, 1, [4.0, 0.0, 0.0], quarter_turn)
            .unwrap();
        assert!(
            world
                .update_articulated_pose(
                    body,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 2,
                        parts: vec![(1, PartTransform::IDENTITY)]
                    }
                )
                .is_err()
        );
        let before = world
            .raycast_articulated(body, [1.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
            .unwrap();
        assert_eq!(before.pose_token, 0);
        assert!(
            world
                .set_next_articulated_root_pose(body, 1, 0, 2, [8.0, 0.0, 0.0], quarter_turn)
                .is_err()
        );
        world.step();
        // Rz(90) maps source X=[1,2], Y=[0,1] to X=[-1,0], Y=[1,2]; root adds +4 X.
        let after = world
            .raycast_articulated(body, [3.5, 1.5, -2.0], [0.0, 0.0, 1.0], 8.0)
            .unwrap();
        assert_eq!(after.pose_token, 1);
        assert_eq!(after.cell, [1, 0, 0]);
        assert!((after.toi_m - 2.0).abs() < 1.0e-5);
        assert!(
            world
                .raycast_articulated(body, [1.5, 0.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .is_err()
        );
        assert!(
            world
                .set_articulated_root_pose(body, 1, 0, 2, [8.0, 0.0, 0.0], quarter_turn)
                .is_err()
        );
        assert_eq!(
            world
                .raycast_articulated(body, [3.5, 1.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .unwrap(),
            after
        );
        world.retire_articulated_body(body);
        assert_eq!(world.articulated_collider_count(body), 0);
        assert!(
            world
                .raycast_articulated(body, [3.5, 1.5, -2.0], [0.0, 0.0, 1.0], 8.0)
                .is_err()
        );
    }
}

#[test]
fn rebuilding_a_rotated_negative_grid_preserves_pose_across_steps_and_restoration() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = world();
        let rotated = PartTransform {
            translation_m: [2.0, 0.0, 0.0],
            rotation_xyzw: [
                0.0,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
        };
        let original = part(1, None, [-2, 0, 0], vec![true, false, true], rotated);
        let original_grid = original.grid.clone();
        let body = world
            .add_articulated_body(
                [4.0, 0.0, 0.0],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![original],
                representation,
            )
            .unwrap();
        let removed_ray = [5.5, -1.5, -2.0];
        let kept_ray = [5.5, 0.5, -2.0];
        let direction = [0.0, 0.0, 1.0];
        assert_eq!(
            world
                .raycast_articulated(body, removed_ray, direction, 8.0)
                .unwrap()
                .cell,
            [-2, 0, 0]
        );
        world.step();
        let changed = part(1, None, [-2, 0, 0], vec![false, false, true], rotated).grid;
        world
            .rebuild_articulated_part_grid(body, 1, 2, 1, 7, 8, changed)
            .unwrap();
        assert!(
            world
                .raycast_articulated(body, removed_ray, direction, 8.0)
                .is_err()
        );
        let kept = world
            .raycast_articulated(body, kept_ray, direction, 8.0)
            .unwrap();
        assert_eq!(kept.cell, [0, 0, 0]);
        assert_eq!(kept.source_revision, 8);
        assert_eq!(kept.topology_token, 2);
        world.step();
        world
            .rebuild_articulated_part_grid(body, 2, 3, 1, 8, 9, original_grid)
            .unwrap();
        let restored = world
            .raycast_articulated(body, removed_ray, direction, 8.0)
            .unwrap();
        assert_eq!(restored.cell, [-2, 0, 0]);
        assert_eq!(restored.source_revision, 9);
        assert!((restored.toi_m - 2.0).abs() < 1.0e-5);
        world.step();
        assert_eq!(
            world
                .raycast_articulated(body, removed_ray, direction, 8.0)
                .unwrap(),
            restored
        );
    }
}

#[test]
fn a_dynamic_probe_rests_on_the_kinematic_assembly_then_falls_after_retirement() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let floor = world
            .add_articulated_body(
                [0.0; 3],
                PartTransform::IDENTITY.rotation_xyzw,
                1,
                vec![part(
                    1,
                    None,
                    [-1, 0, 0],
                    vec![true, true, true],
                    PartTransform::IDENTITY,
                )],
                representation,
            )
            .unwrap();
        let probe = world.add_body(spall_physics::BodySpec {
            kind: spall_physics::BodyKind::Dynamic { ccd: false },
            representation,
            grid: part(1, None, [0; 3], vec![true], PartTransform::IDENTITY).grid,
            cell_m: 1.0,
            density_kg_m3: 1000.0,
            mass_properties: None,
            translation_m: [0.0, 3.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let mut impulse_seen = false;
        for _ in 0..150 {
            world.step();
            impulse_seen |= world
                .contact_impulses()
                .iter()
                .any(|contact| contact.normal_impulse_n_s > 0.0);
        }
        let resting = world.body_state(probe);
        assert!(resting.is_finite());
        assert!(resting.mass_kg > 0.0);
        assert!(
            impulse_seen,
            "no actual dynamic/kinematic solver impulse: {representation:?}, probe={resting:?}"
        );
        assert!(
            (resting.translation_m[1] - 1.0).abs() < 0.1,
            "probe did not rest on the source cells: {representation:?}, {resting:?}"
        );
        world.retire_articulated_body(floor);
        for _ in 0..90 {
            world.step();
        }
        assert!(
            world.body_state(probe).translation_m[1] < 0.0,
            "retired assembly kept hidden support"
        );
    }
}
