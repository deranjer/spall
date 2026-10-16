//! Independent closed-form motion oracle for separated unequal-density cubes.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    OccupancyGrid, PartTransform, PhysicsConfig, PhysicsWorld, Representation, SourcePart,
    prepare_fracture,
};

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn rotate(q: [f32; 4], p: [f64; 3]) -> [f64; 3] {
    let v = [f64::from(q[0]), f64::from(q[1]), f64::from(q[2])];
    let a = cross(v, p);
    let b = cross(v, a);
    std::array::from_fn(|i| p[i] + 2.0 * (f64::from(q[3]) * a[i] + b[i]))
}
fn near(actual: [f64; 3], expected: [f64; 3], tolerance: f64) {
    for i in 0..3 {
        assert!(
            (actual[i] - expected[i]).abs() < tolerance,
            "{actual:?} != {expected:?}"
        );
    }
}

#[test]
fn translated_rotating_split_preserves_point_velocities_momenta_and_moving_source_queries() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let source = SourcePart {
            part_id: 1,
            parent_id: None,
            volume_id: VolumeId::new(7001).unwrap(),
            source_revision: 9,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(-2, 0, 0),
                [3, 1, 1],
                vec![true, false, true],
                vec![MaterialId(2), MaterialId::AIR, MaterialId(6)],
            )
            .unwrap(),
            cell_m: 1.0,
            local_transform: PartTransform::IDENTITY,
        };
        let density = |m: MaterialId| f64::from(m.0);
        let owner = world
            .add_dynamic_corpse_aggregate(
                [10.0, -4.0, 7.0],
                [0.0, 0.0, 1.0, 0.0],
                5,
                vec![source],
                representation,
                density,
            )
            .unwrap();
        let linear = [0.4, -0.2, 0.7];
        let angular = [0.3, 0.5, 0.8];
        world
            .set_dynamic_corpse_component_velocity(owner, None, linear, angular)
            .unwrap();
        let snapshot = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 7000, None)
            .unwrap();
        let plan = prepare_fracture(&snapshot).unwrap();
        let split = world
            .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, density)
            .unwrap();
        assert_eq!(split.components.len(), 2);
        let old_com = [10.0, -4.5, 7.5];
        let mut total_p = [0.0; 3];
        let mut total_l = [0.0; 3];
        for (index, (key, mass)) in split.components.iter().enumerate() {
            let state = world.fracture_component_state(owner, *key).unwrap();
            let center = if index == 0 {
                [11.5, -4.5, 7.5]
            } else {
                [9.5, -4.5, 7.5]
            };
            let expected_mass = if index == 0 { 2.0 } else { 6.0 };
            assert!((mass - expected_mass).abs() < 1e-6);
            assert!((f64::from(state.mass_kg) - expected_mass).abs() < 1e-6);
            let r = std::array::from_fn(|i| center[i] - old_com[i]);
            let spin = cross(angular.map(f64::from), r);
            let expected_v = std::array::from_fn(|i| f64::from(linear[i]) + spin[i]);
            near(state.linvel_m_s.map(f64::from), expected_v, 2e-5);
            near(
                state.angvel_rad_s.map(f64::from),
                angular.map(f64::from),
                1e-6,
            );
            let p = state.linvel_m_s.map(|v| f64::from(v) * mass);
            let orbital = cross(r, p);
            for i in 0..3 {
                total_p[i] += p[i];
                // Unit cube central inertia is m/6 in every direction.
                total_l[i] += orbital[i] + mass / 6.0 * f64::from(state.angvel_rad_s[i]);
            }
        }
        near(total_p, linear.map(|v| f64::from(v) * 8.0), 2e-4);
        near(
            total_l,
            [
                8.0 / 6.0 * f64::from(angular[0]),
                (8.0 / 6.0 + 6.0) * f64::from(angular[1]),
                (8.0 / 6.0 + 6.0) * f64::from(angular[2]),
            ],
            2e-4,
        );
        for _ in 0..12 {
            world.step();
        }
        for (index, (key, _)) in split.components.iter().enumerate() {
            let state = world.fracture_component_state(owner, *key).unwrap();
            let source_center = if index == 0 {
                [-1.5, 0.5, 0.5]
            } else {
                [0.5, 0.5, 0.5]
            };
            let rotated = rotate(state.rotation, source_center);
            let center: [f32; 3] =
                std::array::from_fn(|i| (f64::from(state.translation_m[i]) + rotated[i]) as f32);
            let hit = world
                .raycast_articulated(
                    owner,
                    [center[0], center[1], center[2] - 2.0],
                    [0.0, 0.0, 1.0],
                    4.0,
                )
                .unwrap();
            assert_eq!(hit.volume_id, VolumeId::new(7001).unwrap());
            assert_eq!(hit.source_revision, 9);
            assert_eq!(hit.cell, if index == 0 { [-2, 0, 0] } else { [0, 0, 0] });
        }
    }
}

#[test]
fn nonprimary_fragment_contacts_and_witnesses_retain_the_logical_owner() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let owner = world
            .add_dynamic_corpse_aggregate(
                [0.0, 2.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                5,
                vec![SourcePart {
                    part_id: 1,
                    parent_id: None,
                    volume_id: VolumeId::new(7002).unwrap(),
                    source_revision: 9,
                    grid: OccupancyGrid::from_solid_mask(
                        GlobalCell::new(-2, 0, 0),
                        [3, 1, 1],
                        vec![true, false, true],
                        vec![MaterialId(1); 3],
                    )
                    .unwrap(),
                    cell_m: 1.0,
                    local_transform: PartTransform::IDENTITY,
                }],
                representation,
                |_| 1.0,
            )
            .unwrap();
        let snapshot = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 7003, None)
            .unwrap();
        let plan = prepare_fracture(&snapshot).unwrap();
        let split = world
            .commit_dynamic_corpse_fracture(owner, &snapshot, &plan, |_| 1.0)
            .unwrap();
        world.add_body(spall_physics::BodySpec {
            kind: spall_physics::BodyKind::Fixed,
            representation,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(0, 0, 0),
                [1, 1, 1],
                vec![true],
                vec![MaterialId(1)],
            )
            .unwrap(),
            cell_m: 1.0,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0, -1.0, 0.0],
            linvel_m_s: [0.0; 3],
        });
        let mut impulse_seen = false;
        let mut witness_seen = false;
        for _ in 0..90 {
            world.step();
            impulse_seen |= world.contact_impulses().iter().any(|contact| {
                contact.normal_impulse_n_s > 0.0 && contact.source_parts.contains(&Some(1))
            });
            witness_seen |= world.articulated_contact_witnesses().iter().any(|contact| {
                contact.source_sides.iter().flatten().any(|source| {
                    source.volume_id == VolumeId::new(7002).unwrap() && source.source_revision == 9
                })
            });
        }
        let primary = world
            .fracture_component_state(owner, split.components[0].0)
            .unwrap();
        let secondary = world
            .fracture_component_state(owner, split.components[1].0)
            .unwrap();
        assert!(primary.translation_m[1] < -5.0);
        assert!(secondary.translation_m[1].abs() < 0.1);
        assert!(
            impulse_seen,
            "secondary fragment contact impulse omitted: {representation:?}"
        );
        assert!(
            witness_seen,
            "secondary fragment source witness omitted: {representation:?}"
        );
    }
}

#[test]
fn owner_keys_cannot_alias_and_failed_mass_preparation_changes_no_owner() {
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        disable_ccd: true,
        ..PhysicsConfig::default()
    });
    let mut owners = Vec::new();
    for n in 0..2 {
        owners.push(
            world
                .add_dynamic_corpse_aggregate(
                    [0.0, 0.0, n as f32 * 10.0],
                    [0.0, 0.0, 0.0, 1.0],
                    5,
                    vec![SourcePart {
                        part_id: 1,
                        parent_id: None,
                        volume_id: VolumeId::new(7100 + n).unwrap(),
                        source_revision: 1,
                        grid: OccupancyGrid::from_solid_mask(
                            GlobalCell::new(-2, 0, 0),
                            [3, 1, 1],
                            vec![true, false, true],
                            vec![MaterialId(1); 3],
                        )
                        .unwrap(),
                        cell_m: 1.0,
                        local_transform: PartTransform::IDENTITY,
                    }],
                    Representation::MergedCuboids,
                    |_| 1.0,
                )
                .unwrap(),
        );
    }
    let first = world
        .capture_dynamic_corpse_fracture_snapshot(owners[0], 7100, None)
        .unwrap();
    let first_plan = prepare_fracture(&first).unwrap();
    let other = world
        .capture_dynamic_corpse_fracture_snapshot(owners[1], 7100, None)
        .unwrap();
    let other_plan = prepare_fracture(&other).unwrap();
    assert!(
        world
            .rebuild_dynamic_corpse_part_grid(
                owners[0],
                5,
                6,
                1,
                1,
                2,
                first.parts[0].source.grid.clone(),
                |_| 2.0
            )
            .is_err(),
        "legacy corpse edit bypassed density manifest"
    );
    assert!(
        world
            .commit_dynamic_corpse_fracture(owners[0], &first, &first_plan, |_| 2.0)
            .is_err(),
        "density change silently changed surviving mass"
    );
    assert_eq!(world.articulated_rigid_body_count(owners[0]), 1);
    assert!(
        world
            .commit_dynamic_corpse_fracture(owners[0], &first, &first_plan, |_| f64::NAN)
            .is_err()
    );
    assert_eq!(world.articulated_rigid_body_count(owners[0]), 1);
    assert_eq!(world.articulated_collider_count(owners[0]), 1);
    assert_eq!(
        world
            .raycast_articulated(owners[0], [-1.5, 0.5, -2.0], [0.0, 0.0, 1.0], 4.0)
            .unwrap()
            .cell,
        [-2, 0, 0]
    );
    world
        .commit_dynamic_corpse_fracture(owners[0], &first, &first_plan, |_| 1.0)
        .unwrap();
    assert!(
        world
            .commit_dynamic_corpse_fracture(owners[1], &other, &other_plan, |_| 1.0)
            .is_err()
    );
    assert_eq!(world.articulated_rigid_body_count(owners[1]), 1);
    assert_eq!(world.articulated_collider_count(owners[1]), 1);
    world.retire_articulated_body(owners[0]);
    assert!(
        world
            .commit_dynamic_corpse_fracture(owners[1], &other, &other_plan, |_| 1.0)
            .is_err(),
        "retired owner key was reused"
    );
    assert_eq!(world.articulated_rigid_body_count(owners[1]), 1);
}

#[test]
fn moving_component_edit_and_resplit_leave_siblings_unchanged() {
    for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            disable_ccd: true,
            ..PhysicsConfig::default()
        });
        let volume = VolumeId::new(7201).unwrap();
        let owner = world
            .add_dynamic_corpse_aggregate(
                [10.0, 2.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
                5,
                vec![SourcePart {
                    part_id: 1,
                    parent_id: None,
                    volume_id: volume,
                    source_revision: 1,
                    grid: OccupancyGrid::from_solid_mask(
                        GlobalCell::new(-3, 0, 0),
                        [7, 1, 1],
                        vec![true, true, true, false, false, false, true],
                        vec![MaterialId(1); 7],
                    )
                    .unwrap(),
                    cell_m: 1.0,
                    local_transform: PartTransform::IDENTITY,
                }],
                representation,
                |_| 1.0,
            )
            .unwrap();
        let initial = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 7200, None)
            .unwrap();
        let initial_plan = prepare_fracture(&initial).unwrap();
        let first = world
            .commit_dynamic_corpse_fracture(owner, &initial, &initial_plan, |_| 1.0)
            .unwrap();
        let left = first.components[0].0;
        let right = first.components[1].0;
        world
            .set_dynamic_corpse_component_velocity(
                owner,
                Some(left),
                [0.0, 0.0, 1.0],
                [0.0, 0.4, 0.0],
            )
            .unwrap();
        world
            .set_dynamic_corpse_component_velocity(owner, Some(right), [0.0, 0.0, -1.0], [0.0; 3])
            .unwrap();
        for _ in 0..12 {
            world.step();
        }
        let before = world.fracture_component_state(owner, right).unwrap();
        let stale = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 7200, Some(left))
            .unwrap();
        let edited = OccupancyGrid::from_solid_mask(
            GlobalCell::new(-3, 0, 0),
            [3, 1, 1],
            vec![true, false, true],
            vec![MaterialId(1), MaterialId::AIR, MaterialId(1)],
        )
        .unwrap();
        let outcome = world
            .rebuild_dynamic_corpse_component_part_grid(
                owner,
                left,
                stale.token.topology_revision,
                stale.token.topology_revision + 1,
                1,
                1,
                2,
                edited,
                |_| 1.0,
            )
            .unwrap();
        assert!(
            matches!(outcome,spall_physics::CorpseEditOutcome::Rebuilt { removed_mass_kg,.. } if (removed_mass_kg-1.0).abs() < 1e-6)
        );
        let stale_plan = prepare_fracture(&stale).unwrap();
        assert!(
            world
                .commit_dynamic_corpse_fracture(owner, &stale, &stale_plan, |_| 1.0)
                .is_err()
        );
        let current = world
            .capture_dynamic_corpse_fracture_snapshot(owner, 7200, Some(left))
            .unwrap();
        let next_plan = prepare_fracture(&current).unwrap();
        assert_eq!(next_plan.components.len(), 2);
        let second = world
            .commit_dynamic_corpse_fracture(owner, &current, &next_plan, |_| 1.0)
            .unwrap();
        assert_eq!(world.articulated_rigid_body_count(owner), 3);
        assert!(world.fracture_component_state(owner, left).is_err());
        let after = world.fracture_component_state(owner, right).unwrap();
        assert_eq!(before.translation_m, after.translation_m);
        assert_eq!(before.rotation, after.rotation);
        assert_eq!(before.linvel_m_s, after.linvel_m_s);
        assert_eq!(before.angvel_rad_s, after.angvel_rad_s);
        let mut surviving = Vec::new();
        for key in [right, second.components[0].0, second.components[1].0] {
            let captured = world
                .capture_dynamic_corpse_fracture_snapshot(owner, 7200, Some(key))
                .unwrap();
            for part in captured.parts {
                assert_eq!(part.source.volume_id, volume);
                assert_eq!(part.source.source_revision, 2);
                let origin = part.source.grid.origin();
                for cell in part.source.grid.solid_indices() {
                    surviving.push(origin.x + i64::from(cell[0]));
                }
            }
        }
        surviving.sort_unstable();
        assert_eq!(surviving, vec![-3, -1, 3]);
        for key in [second.components[0].0, second.components[1].0, right] {
            let captured = world
                .capture_dynamic_corpse_fracture_snapshot(owner, 7200, Some(key))
                .unwrap();
            let source = &captured.parts[0].source;
            let dims = source.grid.dims();
            let dense = dims.into_iter().map(|v| v as usize).product();
            let empty = OccupancyGrid::from_solid_mask(
                source.grid.origin(),
                dims,
                vec![false; dense],
                vec![MaterialId::AIR; dense],
            )
            .unwrap();
            world
                .rebuild_dynamic_corpse_component_part_grid(
                    owner,
                    key,
                    captured.token.topology_revision,
                    captured.token.topology_revision + 1,
                    source.part_id,
                    source.source_revision,
                    source.source_revision + 1,
                    empty,
                    |_| 1.0,
                )
                .unwrap();
            assert!(world.fracture_component_state(owner, key).is_err());
        }
        assert_eq!(world.articulated_rigid_body_count(owner), 0);
        assert_eq!(world.articulated_collider_count(owner), 0);
    }
}
