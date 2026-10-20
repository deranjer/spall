use super::*;

#[test]
fn diagnostic_bounded_root_motion_vs_part_local_motion() {
    fn run(root_motion: bool, representation: Representation) -> (f32, f32, usize) {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let mut part = one_part(grid.clone(), if root_motion { 74 } else { 75 }, 1);
        if !root_motion {
            part.local_transform.translation_m[0] = -0.75;
        }
        let animal = world
            .add_articulated_body(
                if root_motion {
                    [-0.75, 0.0, 0.0]
                } else {
                    [0.0; 3]
                },
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![part],
                representation,
            )
            .unwrap();
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid,
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let mut impulse_sum = 0.0f32;
        let mut contact_steps = 0usize;
        for tick in 1..=60u64 {
            let x = -0.75 + tick as f32 * 0.01;
            if root_motion {
                world
                    .set_next_articulated_root_pose(
                        animal,
                        1,
                        tick - 1,
                        tick,
                        [x, 0.0, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    )
                    .unwrap();
            } else {
                world
                    .update_articulated_pose(
                        animal,
                        ArticulatedPose {
                            topology_token: 1,
                            base_pose_token: tick - 1,
                            pose_token: tick,
                            parts: vec![(
                                7,
                                PartTransform {
                                    translation_m: [x, 0.0, 0.0],
                                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                                },
                            )],
                        },
                    )
                    .unwrap();
            }
            world.step();
            for contact in world
                .contact_impulses()
                .into_iter()
                .filter(|c| c.bodies.contains(&probe))
            {
                impulse_sum += contact.normal_impulse_n_s;
                contact_steps += 1;
            }
        }
        let state = world.body_state(probe);
        (state.translation_m[0], impulse_sum, contact_steps)
    }

    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let root = run(true, representation);
        let part = run(false, representation);
        println!(
            "bounded 60-step root motion {representation:?}: probe_x={}, impulse_sum={}, contact_steps={}; part-local: probe_x={}, impulse_sum={}, contact_steps={}",
            root.0, root.1, root.2, part.0, part.1, part.2
        );
        assert!(
            root.1 > 0.0,
            "root-driven motion should produce a real Rapier response for {representation:?}"
        );
    }
}

#[test]
fn articulated_kinematic_part_body_motion_has_solver_surface_velocity() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        let mut part = one_part(grid.clone(), 174, 1);
        part.local_transform.translation_m[0] = -0.75;
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![part],
                representation,
            )
            .unwrap();
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid,
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let mut impulse_sum = 0.0f32;
        let mut contact_steps = 0usize;
        for tick in 1..=60u64 {
            let x = -0.75 + tick as f32 * 0.01;
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: tick - 1,
                        pose_token: tick,
                        parts: vec![(
                            7,
                            PartTransform {
                                translation_m: [x, 0.0, 0.0],
                                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                            },
                        )],
                    },
                )
                .unwrap();
            let before_step = world
                .raycast_articulated(animal, [-2.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            assert_eq!(before_step.pose_token, tick - 1);
            world.step();
            for contact in world
                .contact_impulses()
                .into_iter()
                .filter(|contact| contact.bodies.contains(&probe))
            {
                assert_eq!(contact.source_cells, [None, None]);
                if contact.bodies.contains(&animal.0) {
                    assert!(contact.source_parts.contains(&Some(7)));
                }
                impulse_sum += contact.normal_impulse_n_s;
                contact_steps += 1;
            }
        }
        let state = world.body_state(probe);
        println!(
            "candidate 60-step part-body motion {representation:?}: probe_x={}, impulse_sum={}, contact_steps={}, body_count={}, collider_count={}",
            state.translation_m[0],
            impulse_sum,
            contact_steps,
            world.articulated_rigid_body_count(animal),
            world.articulated_collider_count(animal)
        );
        assert!(impulse_sum > 0.0);
        assert!(contact_steps > 0);
        assert_eq!(world.articulated_rigid_body_count(animal), 2);
        assert_eq!(world.articulated_collider_count(animal), 1);
    }
}

#[test]
fn articulated_sweep_allows_grounded_horizontal_and_upward_recovery() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(-4, 0, -4),
            [8, 1, 8],
            vec![true; 64],
            vec![fixtures::STONE; 64],
        )
        .unwrap(),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    // Wall begins at x=.75, directly adjacent to the floor surface.
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(3, 1, -4),
            [1, 4, 8],
            vec![true; 32],
            vec![fixtures::STONE; 32],
        )
        .unwrap(),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    // The conservative sphere is tangent to the floor top at y=.25.
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0, 0.34150636, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 194, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let lateral = world
        .sweep_articulated_root_translation(animal, 1, 0, 1, [0.25, 0.0, 0.0])
        .unwrap();
    assert!(lateral.fraction > 0.99, "grounded lateral={lateral:?}");
    world.step();
    let upward = world
        .sweep_articulated_root_translation(animal, 1, 1, 2, [0.0, 0.2, 0.0])
        .unwrap();
    assert!(upward.fraction > 0.99, "grounded upward={upward:?}");
    world.step();
    let wall_adjacent_to_floor = world
        .sweep_articulated_root_translation(animal, 1, 2, 3, [0.25, 0.0, 0.0])
        .unwrap();
    assert!(
        wall_adjacent_to_floor.blocked,
        "wall/floor={wall_adjacent_to_floor:?}"
    );
    world.step();
    let downward = world
        .sweep_articulated_root_translation(animal, 1, 3, 4, [0.0, -0.3, 0.0])
        .unwrap();
    assert!(downward.blocked, "movement into floor={downward:?}");
    assert!(downward.fraction < 0.99, "movement into floor={downward:?}");
}

#[test]
fn articulated_sweep_does_not_skip_later_wall_in_shared_l_terrain_collider() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut solid = vec![false; 4 * 4 * 4];
        for z in 0..4 {
            for x in 0..4 {
                solid[(z * 4) + x] = true;
            }
        }
        for y in 1..4 {
            for z in 0..4 {
                solid[(y * 4 * 4) + (z * 4) + 2] = true;
            }
        }
        let terrain = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [4, 4, 4],
            solid,
            vec![fixtures::STONE; 4 * 4 * 4],
        )
        .unwrap();
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            ..PhysicsConfig::default()
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation,
            grid: terrain,
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        assert_eq!(
            world.colliders.len(),
            1,
            "expected one shared terrain collider"
        );
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0, 0.34150636, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 197, 1)],
                Representation::MergedCuboids,
            )
            .unwrap();
        let movement = world
            .sweep_articulated_root_translation(animal, 1, 0, 1, [1.0, 0.0, 0.0])
            .unwrap();
        assert!(movement.blocked, "{representation:?}: {movement:?}");
        assert!(movement.fraction < 0.5, "{representation:?}: {movement:?}");
    }
}

#[test]
fn articulated_sweep_encloses_rotated_off_center_source_volume() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: one_cell_grid(GlobalCell::new(0, 0, -12)),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
                std::f32::consts::FRAC_1_SQRT_2,
            ],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(8, 0, 0)), 195, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let movement = world
        .sweep_articulated_root_translation(animal, 1, 0, 1, [0.0, 0.0, -1.0])
        .unwrap();
    assert!(movement.blocked, "rotated off-center sweep={movement:?}");
    assert!(movement.fraction < 0.8, "sweep={movement:?}");
}

#[test]
fn articulated_sweep_rejects_initial_penetration_and_allows_small_ledge_departure() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0, 0.3, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 196, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let into_overlap = world
        .sweep_articulated_root_translation(animal, 1, 0, 1, [0.0, -0.1, 0.0])
        .unwrap();
    assert!(into_overlap.blocked, "initial overlap={into_overlap:?}");
    assert_eq!(into_overlap.fraction, 0.0);
    world.step();
    let tiny_into_surface = world
        .sweep_articulated_root_translation(animal, 1, 1, 2, [0.0, -1.0e-7, 0.0])
        .unwrap();
    assert!(
        tiny_into_surface.blocked,
        "tiny inward move={tiny_into_surface:?}"
    );
    assert_eq!(tiny_into_surface.fraction, 0.0);
    world.step();
    let ledge_departure = world
        .sweep_articulated_root_translation(animal, 1, 2, 3, [0.5, 0.0, 0.0])
        .unwrap();
    assert!(
        !ledge_departure.blocked,
        "ledge departure={ledge_departure:?}"
    );
    assert_eq!(ledge_departure.fraction, 1.0);
}

#[test]
fn articulated_combined_range_rejection_does_not_leave_part_targets_pending() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let mut source = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 198, 1);
    source.local_transform.translation_m[0] = 9.6;
    let animal = world
        .add_articulated_kinematic_parts(
            [990.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![source],
            Representation::MergedCuboids,
        )
        .unwrap();
    let before = world.body_state(animal.0);
    let hit_before = world
        .raycast_articulated(animal, [990.0, 0.125, 0.125], [1.0, 0.0, 0.0], 20.0)
        .unwrap();
    let near_limit_update = ArticulatedPose {
        topology_token: 1,
        base_pose_token: 0,
        pose_token: 1,
        parts: vec![(
            7,
            PartTransform {
                translation_m: [9.6, 0.0, 0.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            },
        )],
    };
    assert!(
        world
            .update_articulated_pose_with_swept_root_translation(
                animal,
                near_limit_update.clone(),
                [0.5, 0.0, 0.0],
            )
            .is_err()
    );
    assert!(
        world
            .update_articulated_pose_with_swept_root_pose(
                animal,
                near_limit_update,
                PartTransform {
                    translation_m: [990.5, 0.0, 0.0],
                    ..PartTransform::IDENTITY
                },
            )
            .is_err()
    );
    let after = world.body_state(animal.0);
    assert_eq!(after.translation_m, before.translation_m);
    let hit_after = world
        .raycast_articulated(animal, [990.0, 0.125, 0.125], [1.0, 0.0, 0.0], 20.0)
        .unwrap();
    assert_eq!(hit_after, hit_before);
    world.step();
    assert_eq!(
        world.body_state(animal.0).translation_m,
        before.translation_m
    );
}

#[test]
fn articulated_conservative_sweep_blocks_wall_and_rejects_stale_updates() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let wall_grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [1, 4, 4],
        vec![true; 16],
        vec![fixtures::STONE; 16],
    )
    .unwrap();
    let wall = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: wall_grid,
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.5, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 181, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let turn = [
        0.0,
        0.0,
        std::f32::consts::FRAC_1_SQRT_2,
        std::f32::consts::FRAC_1_SQRT_2,
    ];
    assert!(
        world
            .set_next_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], turn)
            .is_err()
    );
    assert!(
        world
            .set_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], turn)
            .is_err()
    );
    let movement = world
        .sweep_articulated_root_translation(animal, 1, 0, 1, [2.0, 0.0, 0.0])
        .unwrap();
    println!("candidate conservative 2m fixed-wall sweep: {movement:?}");
    assert!(movement.blocked);
    assert!(movement.fraction < 0.2, "movement={movement:?}");
    assert!(movement.applied_translation_m[0] < 0.5);
    assert!(
        world
            .set_next_articulated_root_pose(animal, 1, 0, 2, [1.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0],)
            .is_err()
    );
    assert!(
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 2,
                    parts: vec![(7, PartTransform::IDENTITY)],
                },
            )
            .is_err()
    );
    let before = world
        .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert_eq!(before.pose_token, 0);
    world.step();
    let state = world.body_state(animal.0);
    assert!(state.translation_m[0] < 0.5, "state={state:?}");
    let after = world
        .raycast_articulated(
            animal,
            [state.translation_m[0] - 1.0, 0.125, 0.125],
            [1.0, 0.0, 0.0],
            4.0,
        )
        .unwrap();
    assert_eq!(after.pose_token, 1);
    world.remove_collider(wall);
    let after_removal = world
        .sweep_articulated_root_translation(animal, 1, 1, 2, [0.5, 0.0, 0.0])
        .unwrap();
    assert!(
        !after_removal.blocked,
        "after terrain removal={after_removal:?}"
    );
    assert_eq!(after_removal.fraction, 1.0);
    world.step();
    let unchanged_body = world.body_state(animal.0);
    let unchanged_hit = world
        .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert!(
        world
            .update_articulated_pose_with_swept_root_translation(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 2,
                    pose_token: 3,
                    parts: vec![(999, PartTransform::IDENTITY)],
                },
                [0.2, 0.0, 0.0],
            )
            .is_err()
    );
    let after_rejection_body = world.body_state(animal.0);
    assert_eq!(
        after_rejection_body.translation_m,
        unchanged_body.translation_m
    );
    assert_eq!(after_rejection_body.rotation, unchanged_body.rotation);
    assert_eq!(
        world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap(),
        unchanged_hit
    );
    let combined = world
        .update_articulated_pose_with_swept_root_translation(
            animal,
            ArticulatedPose {
                topology_token: 1,
                base_pose_token: 2,
                pose_token: 3,
                parts: vec![(7, PartTransform::IDENTITY)],
            },
            [0.1, 0.0, 0.0],
        )
        .unwrap();
    assert!(!combined.blocked);
    let before_combined_step = world
        .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert_eq!(before_combined_step.pose_token, 2);
    world.step();
    let after_combined_step = world
        .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert_eq!(after_combined_step.pose_token, 3);
}

#[test]
fn articulated_part_body_fast_single_step_contact_is_measured_without_sweep() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        for ccd in [false, true] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                ..Default::default()
            });
            let mut source = one_part(grid.clone(), 183, 1);
            source.local_transform.translation_m[0] = -0.5;
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![source],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(
                            7,
                            PartTransform {
                                translation_m: [0.0, 0.0, 0.0],
                                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                            },
                        )],
                    },
                )
                .unwrap();
            world.step();
            let broad_contact_pairs = world.contact_pair_count();
            let contacts = world
                .contact_impulses()
                .into_iter()
                .filter(|contact| contact.bodies.contains(&probe))
                .collect::<Vec<_>>();
            let pair_count = contacts.len();
            let impulse = contacts
                .iter()
                .map(|contact| contact.normal_impulse_n_s)
                .sum::<f32>();
            println!(
                "original unswept 0.5m one-step part target {representation:?} dynamic_ccd={ccd}: contact_pairs={}, positive_impulse_pairs={}, impulse_sum={impulse}",
                broad_contact_pairs, pair_count
            );
        }
        // Keep the original no-CCD result and measure the same exact
        // workload with candidate dynamic CCD enabled; neither is assumed
        // to provide a kinematic-part impact response.
    }
}

#[test]
fn articulated_contact_witness_resolves_only_unique_source_face_contacts() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 4,
            ..Default::default()
        });
        let mut root_part = one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 186, 3);
        root_part.local_transform.translation_m[0] = 3.0;
        let nested_source = crate::articulated::SourcePart {
            part_id: 8,
            parent_id: Some(7),
            volume_id: VolumeId::new(188).unwrap(),
            source_revision: 10,
            grid: one_cell_grid(GlobalCell::new(-2, 0, 0)),
            cell_m: 0.25,
            local_transform: PartTransform {
                translation_m: [-3.0, 0.0, 0.0],
                ..PartTransform::IDENTITY
            },
        };
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                23,
                vec![root_part, nested_source],
                representation,
            )
            .unwrap();
        let probe_grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid: probe_grid,
            cell_m: 0.0625,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [-0.25, 0.09375, 0.09375],
            linvel_m_s: [0.0; 3],
        });
        // Source cell at grid-local x=[0,.25] has its authored global cell
        // at x=-2, and therefore world x=[-.5,-.25]. A small probe touches
        // only the center of its +X face.
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 23,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![
                        (
                            7,
                            PartTransform {
                                translation_m: [3.0, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                        ),
                        (
                            8,
                            PartTransform {
                                translation_m: [-2.995, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                        ),
                    ],
                },
            )
            .unwrap();
        world.step();
        let contacts = world
            .articulated_contact_witnesses()
            .iter()
            .filter(|contact| contact.bodies.contains(&probe))
            .collect::<Vec<_>>();
        assert!(
            !contacts.is_empty(),
            "{representation:?} produced no per-substep witnesses"
        );
        assert!(
            contacts
                .iter()
                .all(|contact| contact.version == CONTACT_WITNESS_VERSION)
        );
        assert!(contacts.iter().all(|contact| contact.outer_step == 1));
        assert!(contacts.iter().all(|contact| contact.solver_substep < 4));
        let resolved = contacts
            .iter()
            .flat_map(|contact| contact.source_sides.iter().flatten())
            .filter(|side| matches!(side.attribution, ContactCellAttribution::Resolved { .. }))
            .collect::<Vec<_>>();
        let positive_point_impulses = resolved
            .iter()
            .filter(|side| side.normal_impulse_n_s.is_some_and(|impulse| impulse > 0.0))
            .count();
        println!(
            "source witness candidate {representation:?}: point_records={} resolved={} positive_point_impulses={} overflow={} final_pair_records={}",
            contacts.len(),
            resolved.len(),
            positive_point_impulses,
            world.contact_witness_overflow(),
            world.contact_impulses().len()
        );
        assert!(
            !resolved.is_empty(),
            "centered face contact should resolve without rounding: {:?}",
            contacts
        );
        for side in resolved {
            assert_eq!(side.part_id, 8);
            assert_eq!(side.volume_id, VolumeId::new(188).unwrap());
            assert_eq!(side.source_revision, 10);
            assert_eq!(side.topology_token, 23);
            assert_eq!(side.pose_token, 1);
            assert_eq!(
                side.attribution,
                ContactCellAttribution::Resolved {
                    cell: [-2, 0, 0],
                    material: fixtures::STONE,
                }
            );
            assert!(
                side.normal_impulse_n_s
                    .is_some_and(|impulse| impulse.is_finite())
            );
        }
        assert!(positive_point_impulses > 0);
        assert!(
            world.contact_impulses().is_empty(),
            "this fixture's point witnesses are transient, not current-final pair records"
        );
        assert!(
            world
                .contact_impulses()
                .iter()
                .all(|pair| pair.source_cells == [None, None])
        );

        // Stale and pending authored updates cannot mutate the prior
        // published witness snapshot or its query provenance.
        let original_hit = world
            .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
            .unwrap();
        let snapshot = world.articulated_contact_witnesses().to_vec();
        assert!(
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 23,
                        base_pose_token: 0,
                        pose_token: 2,
                        parts: vec![
                            (
                                7,
                                PartTransform {
                                    translation_m: [3.0, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                }
                            ),
                            (
                                8,
                                PartTransform {
                                    translation_m: [-3.0, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                }
                            ),
                        ],
                    }
                )
                .is_err()
        );
        assert_eq!(world.articulated_contact_witnesses(), snapshot.as_slice());
        assert_eq!(
            world
                .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
                .unwrap(),
            original_hit
        );
        world
            .rebuild_articulated_part_grid(
                animal,
                23,
                24,
                8,
                10,
                11,
                one_cell_grid(GlobalCell::new(-2, 0, 0)),
            )
            .unwrap();
        assert_eq!(world.articulated_contact_witnesses(), snapshot.as_slice());
        assert!(
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 23,
                        base_pose_token: 1,
                        pose_token: 2,
                        parts: vec![
                            (
                                7,
                                PartTransform {
                                    translation_m: [3.0, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                }
                            ),
                            (
                                8,
                                PartTransform {
                                    translation_m: [-2.995, 0.0, 0.0],
                                    ..PartTransform::IDENTITY
                                }
                            ),
                        ],
                    }
                )
                .is_err()
        );
        let replacement_hit = world
            .raycast_articulated(animal, [-0.6, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
            .unwrap();
        assert_eq!(replacement_hit.topology_token, 24);
        assert_eq!(replacement_hit.source_revision, 11);
        assert_eq!(replacement_hit.cell, [-2, 0, 0]);
    }
}

#[test]
fn articulated_contact_witness_resolves_source_when_it_is_pair_side_two() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 4,
            ..Default::default()
        });
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.0625,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.25, 0.09375, 0.09375],
            linvel_m_s: [0.0; 3],
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                31,
                vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 190, 2)],
                representation,
            )
            .unwrap();
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 31,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            translation_m: [0.005, 0.0, 0.0],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            )
            .unwrap();
        world.step();
        let source_sides = world
            .articulated_contact_witnesses()
            .iter()
            .filter(|contact| contact.bodies.contains(&probe))
            .filter_map(|contact| contact.source_sides[1])
            .collect::<Vec<_>>();
        assert!(
            !source_sides.is_empty(),
            "{representation:?} did not put source collider second"
        );
        assert!(source_sides.iter().all(|side| {
            side.part_id == 7
                && side.volume_id == VolumeId::new(190).unwrap()
                && matches!(side.attribution, ContactCellAttribution::Resolved { cell: [0, 0, 0], material } if material == fixtures::STONE)
        }));
    }
}

#[test]
fn articulated_substeps_preserve_fast_target_time_and_produce_contact_response() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        for ccd in [false, true] {
            let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
            let mut world = PhysicsWorld::new(PhysicsConfig {
                gravity_m_s2: [0.0; 3],
                solver_substeps: 20,
                ..Default::default()
            });
            let mut source = one_part(grid.clone(), 183, 1);
            source.local_transform.translation_m[0] = -0.5;
            let animal = world
                .add_articulated_kinematic_parts(
                    [0.0; 3],
                    [0.0, 0.0, 0.0, 1.0],
                    1,
                    vec![source],
                    representation,
                )
                .unwrap();
            let probe = world.add_body(BodySpec {
                kind: BodyKind::Dynamic { ccd },
                representation,
                grid,
                cell_m: 0.25,
                density_kg_m3: 1_000.0,
                mass_properties: None,
                translation_m: [0.0; 3],
                linvel_m_s: [0.0; 3],
            });
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: 0,
                        pose_token: 1,
                        parts: vec![(7, PartTransform::IDENTITY)],
                    },
                )
                .unwrap();
            let start = Instant::now();
            let timing = world.step();
            let elapsed = start.elapsed();
            let contacts = world
                .contact_impulses()
                .into_iter()
                .filter(|contact| contact.bodies.contains(&probe))
                .collect::<Vec<_>>();
            let impulse_sum = contacts
                .iter()
                .map(|contact| contact.normal_impulse_n_s)
                .sum::<f32>();
            let state = world.body_state(probe);
            println!(
                "substep candidate {representation:?} dynamic_ccd={ccd} dt={} elapsed_us={} pipeline_us={} outer_steps={} substeps={} last_contacts={} last_impulse={} accumulated_impulse={} impulse_pair_substeps={} probe_x={} probe_vx={} mass_kg={} momentum_x={} kinetic_energy_j={}",
                1.0 / 60.0,
                elapsed.as_secs_f64() * 1.0e6,
                timing.pipeline.as_secs_f64() * 1.0e6,
                world.steps_run(),
                timing.substeps,
                contacts.len(),
                impulse_sum,
                timing.normal_impulse_n_s,
                timing.impulse_pair_substeps,
                state.translation_m[0],
                state.linvel_m_s[0],
                state.mass_kg,
                state.mass_kg * state.linvel_m_s[0],
                0.5 * state.mass_kg * state.linvel_m_s.iter().map(|v| v * v).sum::<f32>(),
            );
            assert_eq!(world.steps_run(), 1, "substeps publish one outer step");
            assert!(state.is_finite());
            assert!(timing.normal_impulse_n_s.is_finite() && timing.normal_impulse_n_s > 0.0);
            assert!(timing.impulse_pair_substeps > 0);
            assert!(state.linvel_m_s[0] > 0.0);
            let animal_hit = world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap();
            assert_eq!(animal_hit.pose_token, 1);
            assert!(animal_hit.toi_m > 0.0);
        }
    }
}

#[test]
fn articulated_substeps_produce_contact_response_for_fast_angular_target() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 64,
            ..Default::default()
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(grid.clone(), 185, 1)],
                representation,
            )
            .unwrap();
        let probe_lo = [-0.15625_f32, 0.21875, 0.09375];
        let probe_hi = [
            probe_lo[0] + 0.0625,
            probe_lo[1] + 0.0625,
            probe_lo[2] + 0.0625,
        ];
        let intervals_overlap = |a0: f32, a1: f32, b0: f32, b1: f32| a0 < b1 && b0 < a1;
        // Independent source-cell boxes: initially x-disjoint; after the
        // exact 90-degree rigid rotation, source and probe overlap on all
        // three axes. This verifies the fixture intersects geometrically.
        assert!(!intervals_overlap(probe_lo[0], probe_hi[0], 0.0, 0.25));
        assert!(intervals_overlap(probe_lo[0], probe_hi[0], -0.25, 0.0));
        assert!(intervals_overlap(probe_lo[1], probe_hi[1], 0.0, 0.25));
        assert!(intervals_overlap(probe_lo[2], probe_hi[2], 0.0, 0.25));
        // The probe begins wholly outside the source cell at x < 0. A
        // 90-degree part-local turn carries the source cell across it.
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.0625,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [-0.15625, 0.21875, 0.09375],
            linvel_m_s: [0.0; 3],
        });
        let half_angle = std::f32::consts::FRAC_PI_4;
        let before = world
            .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
            .unwrap();
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(
                        7,
                        PartTransform {
                            rotation_xyzw: [0.0, 0.0, half_angle.sin(), half_angle.cos()],
                            ..PartTransform::IDENTITY
                        },
                    )],
                },
            )
            .unwrap();
        let staged = world
            .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
            .unwrap();
        assert_eq!(staged, before, "pending angular pose stays unpublished");
        let timing = world.step();
        let state = world.body_state(probe);
        println!(
            "substep angular candidate {representation:?} angle_deg=90 dt={} substeps={} impulse_n_s={} impulse_pair_substeps={} probe_v={:?} mass_kg={} energy_j={}",
            1.0 / 60.0,
            timing.substeps,
            timing.normal_impulse_n_s,
            timing.impulse_pair_substeps,
            state.linvel_m_s,
            state.mass_kg,
            0.5 * state.mass_kg * state.linvel_m_s.iter().map(|v| v * v).sum::<f32>(),
        );
        assert_eq!(timing.substeps, 64);
        assert!(timing.normal_impulse_n_s.is_finite() && timing.normal_impulse_n_s > 0.0);
        assert!(state.is_finite());
        assert!(
            state
                .linvel_m_s
                .iter()
                .any(|component| component.abs() > 1.0e-3)
        );
        assert_eq!(world.steps_run(), 1);
        let hit = world
            .raycast_articulated(animal, [-0.2, 0.125, 0.125], [1.0, 0.0, 0.0], 1.0)
            .unwrap();
        assert_eq!(hit.pose_token, 1);
    }
}

#[test]
fn articulated_substeps_advance_unrelated_world_dynamics_by_one_outer_dt() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0, -9.81, 0.0],
        solver_substeps: 20,
        ..Default::default()
    });
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 186, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let unrelated = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::NativeVoxels,
        grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
        cell_m: 0.25,
        density_kg_m3: 1_000.0,
        mass_properties: None,
        translation_m: [10.0, 10.0, 10.0],
        linvel_m_s: [0.0; 3],
    });
    world
        .update_articulated_pose(
            animal,
            ArticulatedPose {
                topology_token: 1,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(
                    7,
                    PartTransform {
                        translation_m: [0.1, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        )
        .unwrap();
    let timing = world.step();
    let state = world.body_state(unrelated);
    let expected_vy = -9.81 / 60.0;
    println!(
        "unrelated dynamic gravity substeps={} outer_dt={} vy={} expected_vy={} y={} steps={}",
        timing.substeps,
        1.0 / 60.0,
        state.linvel_m_s[1],
        expected_vy,
        state.translation_m[1],
        world.steps_run(),
    );
    assert_eq!(timing.substeps, 20);
    assert_eq!(world.steps_run(), 1);
    assert!((state.linvel_m_s[1] - expected_vy).abs() < 1.0e-5);
    assert!(state.translation_m[1] < 10.0);
    let hit = world
        .raycast_articulated(animal, [0.5, 0.125, 0.125], [-1.0, 0.0, 0.0], 1.0)
        .unwrap();
    assert_eq!(hit.pose_token, 1);
}

#[test]
fn articulated_substeps_admit_slow_motion_and_reject_over_budget_atomically() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 20,
            ..Default::default()
        });
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut source = one_part(grid.clone(), 193, 1);
        source.local_transform.translation_m[0] = -0.5;
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![source],
                representation,
            )
            .unwrap();
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid,
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
        });
        let mut accumulated_impulse = 0.0_f32;
        for pose_token in 1..=60 {
            let target_x = -0.5 + pose_token as f32 * 0.01;
            world
                .update_articulated_pose(
                    animal,
                    ArticulatedPose {
                        topology_token: 1,
                        base_pose_token: pose_token - 1,
                        pose_token,
                        parts: vec![(
                            7,
                            PartTransform {
                                translation_m: [target_x, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                        )],
                    },
                )
                .unwrap();
            let timing = world.step();
            assert_eq!(timing.substeps, 20);
            accumulated_impulse += timing.normal_impulse_n_s;
        }
        let state = world.body_state(probe);
        let hit = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        println!(
            "substep slow candidate {representation:?} displacement=0.6m outer_ticks=60 substeps=1200 elapsed_s={} impulse_n_s={} probe_x={} probe_vx={} probe_mass_kg={} dynamic_momentum_x={} kinetic_energy_j={} published_pose_token={}",
            60.0 / 60.0,
            accumulated_impulse,
            state.translation_m[0],
            state.linvel_m_s[0],
            state.mass_kg,
            state.mass_kg * state.linvel_m_s[0],
            0.5 * state.mass_kg * state.speed_m_s().powi(2),
            hit.pose_token,
        );
        assert!(accumulated_impulse.is_finite() && accumulated_impulse > 0.0);
        assert!(state.is_finite());
        assert_eq!(hit.pose_token, 60);
        assert_eq!(world.steps_run(), 60);
    }

    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 20,
            ..Default::default()
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(grid, 194, 1)],
                representation,
            )
            .unwrap();
        let before = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        let rejected_translation = world.update_articulated_pose(
            animal,
            ArticulatedPose {
                topology_token: 1,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(
                    7,
                    PartTransform {
                        translation_m: [0.8, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        );
        assert!(matches!(
            rejected_translation,
            Err(crate::articulated::ArticulatedError::WorkBudget)
        ));
        let after_translation = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(after_translation, before);

        let rejected_rotation = world.update_articulated_pose(
            animal,
            ArticulatedPose {
                topology_token: 1,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(
                    7,
                    PartTransform {
                        rotation_xyzw: [0.0, 0.0, 1.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        );
        assert!(matches!(
            rejected_rotation,
            Err(crate::articulated::ArticulatedError::WorkBudget)
        ));
        let after_rotation = world
            .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
            .unwrap();
        assert_eq!(after_rotation, before);
        let timing = world.step();
        assert_eq!(timing.substeps, 20);
        assert_eq!(world.steps_run(), 1);
        assert_eq!(
            world
                .raycast_articulated(animal, [-1.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
                .unwrap(),
            before
        );
    }

    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps: 20,
            ..Default::default()
        });
        let animal = world
            .add_articulated_kinematic_parts(
                [0.0; 3],
                [0.0, 0.0, 0.0, 1.0],
                1,
                vec![one_part(grid.clone(), 195, 1)],
                representation,
            )
            .unwrap();
        let probe = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation,
            grid,
            cell_m: 0.25,
            density_kg_m3: 1_000.0,
            mass_properties: None,
            translation_m: [10.0; 3],
            linvel_m_s: [0.0; 3],
        });
        world
            .update_articulated_pose(
                animal,
                ArticulatedPose {
                    topology_token: 1,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(7, PartTransform::IDENTITY)],
                },
            )
            .unwrap();
        let timing = world.step();
        let state = world.body_state(probe);
        assert_eq!(timing.substeps, 20);
        assert_eq!(timing.normal_impulse_n_s, 0.0);
        assert_eq!(state.linvel_m_s, [0.0; 3]);
        assert_eq!(world.steps_run(), 1);
    }
}

#[test]
fn nonlinear_pose_and_root_rotation_sweep_blocks_wall_then_accepts_after_removal() {
    let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..Default::default()
    });
    let wall = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0, -0.125, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let mut part = one_part(grid.clone(), 189, 1);
    part.local_transform.translation_m[0] = -0.5;
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0; 3],
            PartTransform::IDENTITY.rotation_xyzw,
            1,
            vec![part],
            Representation::MergedCuboids,
        )
        .unwrap();
    let update = ArticulatedPose {
        topology_token: 1,
        base_pose_token: 0,
        pose_token: 1,
        parts: vec![(
            7,
            PartTransform {
                translation_m: [-0.5, 0.0, 0.0],
                ..PartTransform::IDENTITY
            },
        )],
    };
    let rotated_root = PartTransform {
        translation_m: [0.0; 3],
        rotation_xyzw: [0.0, 0.0, 1.0, 0.0],
    };
    let blocked = world
        .update_articulated_pose_with_swept_root_pose(animal, update.clone(), rotated_root)
        .unwrap();
    assert!(blocked.blocked);
    assert_eq!(blocked.part_id, Some(7));
    assert!(blocked.time_of_impact.unwrap() < 1.0);
    assert_eq!(world.body_state(animal.0).translation_m, [0.0; 3]);
    let before = world
        .raycast_articulated(animal, [-2.0, 0.125, 0.125], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert_eq!(before.pose_token, 0);

    world.retire_body(wall);
    let accepted = world
        .update_articulated_pose_with_swept_root_pose(animal, update, rotated_root)
        .unwrap();
    assert!(!accepted.blocked);
    world.step();
    let after = world
        .raycast_articulated(animal, [-2.0, 0.0, 0.0], [1.0, 0.0, 0.0], 4.0)
        .unwrap();
    assert_eq!(after.pose_token, 1);
    assert_eq!(before.cell, after.cell);

    // A replacement obstacle at the same world location must re-enter
    // the current candidate set; an edit that moves its solid cell away
    // must remove it from the next sweep query.
    let replacement_wall = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0, -0.125, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let reverse_update = ArticulatedPose {
        topology_token: 1,
        base_pose_token: 1,
        pose_token: 2,
        parts: vec![(
            7,
            PartTransform {
                translation_m: [-0.5, 0.0, 0.0],
                ..PartTransform::IDENTITY
            },
        )],
    };
    let replacement_hit = world
        .update_articulated_pose_with_swept_root_pose(
            animal,
            reverse_update.clone(),
            PartTransform::IDENTITY,
        )
        .unwrap();
    assert!(
        replacement_hit.blocked,
        "replacement obstacle={replacement_hit:?}"
    );
    let moved_cell = OccupancyGrid::from_solid_mask(
        GlobalCell::new(8, 0, 0),
        [1, 1, 1],
        vec![true],
        vec![fixtures::STONE],
    )
    .unwrap();
    world.rebuild_collider(replacement_wall, &moved_cell, Representation::MergedCuboids);
    let after_edit = world
        .update_articulated_pose_with_swept_root_pose(
            animal,
            reverse_update,
            PartTransform::IDENTITY,
        )
        .unwrap();
    assert!(!after_edit.blocked, "edited obstacle={after_edit:?}");
}

#[test]
fn articulated_sweep_localizes_full_world_fixed_colliders_before_work_budget() {
    // 285 exact source cells represent the authored deer workload. The
    // legacy Cartesian loop exceeded its one-million cast budget above
    // 3,508 fixed colliders, even when every collider was hundreds of
    // metres from the declared transition.
    let source_grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [285, 1, 1],
        vec![true; 285],
        vec![fixtures::STONE; 285],
    )
    .unwrap();
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    for index in 0..3_509 {
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation: Representation::MergedCuboids,
            grid: one_cell_grid(GlobalCell::new(0, 0, 0)),
            cell_m: 0.25,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [20.0 + index as f32 * 0.5, 100.0, 100.0],
            linvel_m_s: [0.0; 3],
        });
    }
    assert_eq!(world.colliders.len(), 3_509);
    let animal = world
        .add_articulated_kinematic_parts(
            [250.0, 0.0, 0.0],
            PartTransform::IDENTITY.rotation_xyzw,
            1,
            vec![one_part(source_grid, 213, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    let transition = world
        .update_articulated_pose_with_swept_root_pose(
            animal,
            ArticulatedPose {
                topology_token: 1,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(7, PartTransform::IDENTITY)],
            },
            PartTransform {
                translation_m: [250.25, 0.0, 0.0],
                ..PartTransform::IDENTITY
            },
        )
        .unwrap();
    assert!(!transition.blocked);
    world.step();
    assert_eq!(world.body_state(animal.0).translation_m, [250.25, 0.0, 0.0]);
    assert_eq!(transition.terrain_candidates, 0);
    assert_eq!(transition.cell_casts, 0);
}

#[test]
fn articulated_sweep_loses_ground_support_and_falls_after_voxel_removal() {
    let dt = 1.0 / 60.0;
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let floor_grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [8, 1, 8],
        vec![true; 64],
        vec![fixtures::STONE; 64],
    )
    .unwrap();
    let floor = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid,
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [-1.0, 0.0, -1.0],
        linvel_m_s: [0.0; 3],
    });
    let animal = world
        .add_articulated_kinematic_parts(
            [0.0, 2.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(one_cell_grid(GlobalCell::new(0, 0, 0)), 182, 1)],
            Representation::MergedCuboids,
        )
        .unwrap();
    world.sync_queries();
    let mut token = 0;
    let mut vertical_speed = 0.0f32;
    let mut supported = false;
    for _ in 0..240 {
        vertical_speed -= 9.81 * dt;
        let movement = world
            .sweep_articulated_root_translation(
                animal,
                1,
                token,
                token + 1,
                [0.0, vertical_speed * dt, 0.0],
            )
            .unwrap();
        token += 1;
        if movement.blocked {
            supported = true;
            vertical_speed = 0.0;
        }
        world.step();
        if supported {
            break;
        }
    }
    assert!(supported, "conservative rig never found floor support");
    let supported_y = world.body_state(animal.0).translation_m[1];
    world.remove_collider(floor);
    world.sync_queries();
    let fall_start = world.body_state(animal.0).translation_m[1];
    for _ in 0..30 {
        vertical_speed -= 9.81 * dt;
        let movement = world
            .sweep_articulated_root_translation(
                animal,
                1,
                token,
                token + 1,
                [0.0, vertical_speed * dt, 0.0],
            )
            .unwrap();
        assert!(!movement.blocked);
        token += 1;
        world.step();
    }
    let fallen_y = world.body_state(animal.0).translation_m[1];
    println!(
        "conservative ground support: root_y={supported_y}, after support removal/fall root_y={fallen_y}"
    );
    assert!(fallen_y < fall_start - 0.5);
}

#[test]
fn diagnostic_fast_one_tick_root_target_contacts_without_probe_response() {
    let grid = one_cell_grid(GlobalCell::new(0, 0, 0));
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let animal = world
        .add_articulated_body(
            [-0.5, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(grid.clone(), 76, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    let probe = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::NativeVoxels,
        grid,
        cell_m: 0.25,
        density_kg_m3: 1_000.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    world
        .set_next_articulated_root_pose(animal, 1, 0, 1, [0.0; 3], [0.0, 0.0, 0.0, 1.0])
        .unwrap();
    world.step();
    let state = world.body_state(probe);
    let impulse: f32 = world
        .contact_impulses()
        .into_iter()
        .filter(|contact| contact.bodies.contains(&probe))
        .map(|contact| contact.normal_impulse_n_s)
        .sum();
    println!(
        "fast one-tick 0.5m root target over 16.67ms: contact_pairs={}, probe_vx={}, probe_x={}, impulse_sum={impulse}",
        world.contact_pair_count(),
        state.linvel_m_s[0],
        state.translation_m[0]
    );
    assert!(world.contact_pair_count() > 0);
    assert!(state.linvel_m_s[0].abs() < 1.0e-6);
    assert_eq!(impulse, 0.0);
}

#[test]
fn diagnostic_kinematic_root_crosses_fixed_terrain_but_character_sweep_blocks() {
    let assembly_grid = one_cell_grid(GlobalCell::new(0, 0, 0));
    let terrain_grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [1, 8, 1],
        vec![true; 8],
        vec![fixtures::STONE; 8],
    )
    .unwrap();
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: terrain_grid,
        cell_m: 0.25,
        density_kg_m3: 0.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    let animal = world
        .add_articulated_body(
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
            1,
            vec![one_part(assembly_grid, 85, 1)],
            Representation::NativeVoxels,
        )
        .unwrap();
    for tick in 1..=120u64 {
        world
            .set_next_articulated_root_pose(
                animal,
                1,
                tick - 1,
                tick,
                [-1.0 + (tick as f32 / 60.0), 0.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            )
            .unwrap();
        world.step();
    }
    let root_x = world.body_state(animal.0).translation_m[0];
    println!(
        "2m root path through 0.25m fixed terrain over 2s: root_x={root_x}, contact_pairs={}, max_penetration={}m",
        world.contact_pair_count(),
        world.max_penetration_m()
    );
    assert!(
        (root_x - 1.0).abs() < 0.001,
        "kinematic target teleports through the fixed obstacle"
    );
    assert_eq!(
        world.contact_pair_count(),
        0,
        "default Rapier active collision types omit kinematic-fixed pairs"
    );

    world.retire_articulated_body(animal);
    world.sync_queries();
    let movement = world.sweep_character_excluding(
        crate::character::CharacterParams::DEFAULT,
        [-1.0, 0.0, 0.125],
        [2.0, 0.0, 0.0],
        1.0 / 60.0,
        &[],
    );
    println!(
        "character query sweep through the same fixed terrain: moved_x={}",
        movement.translation_m[0]
    );
    assert!(
        movement.translation_m[0] < 1.5,
        "query-based locomotion sweep should see and stop at terrain"
    );
}
