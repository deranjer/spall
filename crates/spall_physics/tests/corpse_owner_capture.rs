//! Whole-owner capture must survive actual fracture, not only a restore fixture.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    ArticulatedBatchOperation, ArticulatedBodyId, BodyState, DerivedFractureComponentId,
    DynamicCorpseComponentRestore, DynamicCorpseOwnerFence, DynamicCorpseRestore, OccupancyGrid,
    PartTransform, PhysicsConfig, PhysicsWorld, Representation, SourceCellRef, SourcePart,
    prepare_fracture,
};

const OWNER: u128 = 0x0002_1851;

fn key(serial: u64) -> DerivedFractureComponentId {
    DerivedFractureComponentId {
        owner_key: OWNER,
        serial,
    }
}

fn source_cell(x: i32) -> SourceCellRef {
    SourceCellRef {
        volume_id: VolumeId::new(501).unwrap(),
        part_id: 1,
        cell: [x, 0, 0],
        material: MaterialId(1),
        source_revision: 7,
    }
}

fn sources() -> Vec<SourcePart> {
    let occupied = [true, false, true, false, false, false, true];
    let grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [7, 1, 1],
        occupied.to_vec(),
        occupied
            .map(|solid| {
                if solid {
                    MaterialId(1)
                } else {
                    MaterialId::AIR
                }
            })
            .to_vec(),
    )
    .unwrap();
    let empty = OccupancyGrid::from_solid_mask(
        GlobalCell::new(20, 0, 0),
        [1, 1, 1],
        vec![false],
        vec![MaterialId::AIR],
    )
    .unwrap();
    vec![
        SourcePart {
            part_id: 1,
            parent_id: None,
            volume_id: VolumeId::new(501).unwrap(),
            source_revision: 7,
            cell_m: 0.25,
            grid,
            local_transform: PartTransform::IDENTITY,
        },
        SourcePart {
            part_id: 2,
            parent_id: Some(1),
            volume_id: VolumeId::new(502).unwrap(),
            source_revision: 8,
            cell_m: 0.25,
            grid: empty,
            local_transform: PartTransform {
                translation_m: [0.0, 0.0, 0.5],
                ..PartTransform::IDENTITY
            },
        },
    ]
}

fn restore(world: &mut PhysicsWorld, value: DynamicCorpseRestore) -> ArticulatedBodyId {
    let prepared = world
        .prepare_articulated_batch(
            vec![ArticulatedBatchOperation::ColdRestore {
                operation_tag: 1,
                restore: value,
                representation: Representation::MergedCuboids,
            }],
            |_| 1000.0,
        )
        .unwrap();
    world.commit_articulated_batch(prepared).unwrap()[0].owner_id
}

fn assert_same_motion(before: BodyState, after: BodyState) {
    assert!((before.mass_kg - after.mass_kg).abs() < 1.0e-5);
    for (lhs, rhs) in before
        .translation_m
        .iter()
        .zip(after.translation_m)
        .chain(before.rotation.iter().zip(after.rotation))
        .chain(before.linvel_m_s.iter().zip(after.linvel_m_s))
        .chain(before.angvel_rad_s.iter().zip(after.angvel_rad_s))
    {
        assert!((lhs - rhs).abs() < 1.0e-5, "{lhs} != {rhs}");
    }
}

#[test]
fn fracture_then_capture_keeps_primary_empty_binding_and_every_component() {
    let canonical = sources();
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = restore(
        &mut world,
        DynamicCorpseRestore {
            owner_key: OWNER,
            source_generation: 9,
            topology_revision: 11,
            pose_epoch: 17,
            next_component_serial: 6,
            source_parts: canonical.clone(),
            components: vec![
                DynamicCorpseComponentRestore {
                    component_id: Some(key(5)),
                    translation_m: [2.0, 4.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    linear_velocity_m_s: [1.0, 0.0, 0.0],
                    angular_velocity_rad_s: [0.0, 0.0, 0.5],
                    cells: vec![source_cell(0), source_cell(2)],
                },
                DynamicCorpseComponentRestore {
                    component_id: Some(key(2)),
                    translation_m: [8.0, 4.0, 0.0],
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    linear_velocity_m_s: [-1.0, 0.0, 0.0],
                    angular_velocity_rad_s: [0.0, 0.0, -0.2],
                    cells: vec![source_cell(6)],
                },
            ],
        },
    );
    let selected = world
        .capture_dynamic_corpse_fracture_snapshot(id, OWNER, Some(key(5)))
        .unwrap();
    let plan = prepare_fracture(&selected).unwrap();
    assert_eq!(plan.components.len(), 2);
    let split = world
        .commit_dynamic_corpse_fracture(id, &selected, &plan, |_| 1000.0)
        .unwrap();
    assert_eq!(
        split
            .components
            .iter()
            .map(|(id, _)| id.serial)
            .collect::<Vec<_>>(),
        [6, 7]
    );

    let owner = world.articulated_owner_snapshot(id).unwrap();
    let captured = world
        .capture_dynamic_corpse_owner(
            id,
            DynamicCorpseOwnerFence {
                owner_key: OWNER,
                source_generation: owner.source_generation,
                topology_revision: owner.topology_token,
                pose_token: owner.pose_token,
                next_component_serial: owner.next_component_serial,
            },
            &canonical,
        )
        .unwrap();
    assert_eq!(captured.pose_epoch, 17); // The physics step is still zero.
    assert_eq!(captured.next_component_serial, 8);
    assert_eq!(captured.components[0].component_id, Some(key(6)));
    assert_eq!(
        captured
            .components
            .iter()
            .map(|c| c.component_id.unwrap().serial)
            .collect::<Vec<_>>(),
        [6, 2, 7]
    );
    assert_eq!(
        captured
            .components
            .iter()
            .map(|c| c.cells.len())
            .sum::<usize>(),
        3
    );
    assert_eq!(captured.source_parts.len(), 2);
    let empty = &captured.source_parts[1];
    assert_eq!(empty.volume_id, VolumeId::new(502).unwrap());
    assert_eq!(empty.parent_id, Some(1));
    assert_eq!(empty.grid.solid_count(), 0);
    assert_eq!(empty.local_transform, canonical[1].local_transform);

    let keys = captured
        .components
        .iter()
        .map(|c| c.component_id.unwrap())
        .collect::<Vec<_>>();
    let mut rebuilt = PhysicsWorld::new(PhysicsConfig::default());
    let rebuilt_id = restore(&mut rebuilt, captured);
    assert_same_motion(
        owner.root,
        rebuilt.articulated_owner_snapshot(rebuilt_id).unwrap().root,
    );
    for key in keys {
        assert_same_motion(
            world.fracture_component_state(id, key).unwrap(),
            rebuilt.fracture_component_state(rebuilt_id, key).unwrap(),
        );
    }
    for (x, cell) in [(2.125, 0), (2.625, 2), (9.625, 6)] {
        let original = world
            .raycast_articulated(id, [x, 4.125, -1.0], [0.0, 0.0, 1.0], 2.0)
            .unwrap();
        let restored = rebuilt
            .raycast_articulated(rebuilt_id, [x, 4.125, -1.0], [0.0, 0.0, 1.0], 2.0)
            .unwrap();
        assert_eq!(original.volume_id, VolumeId::new(501).unwrap());
        assert_eq!(original.cell, [cell, 0, 0]);
        assert_eq!(
            (restored.volume_id, restored.part_id, restored.cell),
            (original.volume_id, original.part_id, original.cell)
        );
    }
}

#[test]
fn air_padding_at_negative_source_coordinates_does_not_move_the_restored_component() {
    let volume = VolumeId::new(601).unwrap();
    let mut solid = vec![false; 1001];
    let mut material = vec![MaterialId::AIR; 1001];
    solid[1000] = true;
    material[1000] = MaterialId(1);
    let canonical = vec![SourcePart {
        part_id: 1,
        parent_id: None,
        volume_id: volume,
        source_revision: 3,
        cell_m: 0.25,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(-1000, 0, 0),
            [1001, 1, 1],
            solid,
            material,
        )
        .unwrap(),
        local_transform: PartTransform::IDENTITY,
    }];
    let owner_key = OWNER + 1;
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = restore(
        &mut world,
        DynamicCorpseRestore {
            owner_key,
            source_generation: 2,
            topology_revision: 3,
            pose_epoch: 4,
            next_component_serial: 1,
            source_parts: canonical.clone(),
            components: vec![DynamicCorpseComponentRestore {
                component_id: None,
                translation_m: [-900.0, 0.0, 0.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                linear_velocity_m_s: [0.5, 0.0, 0.0],
                angular_velocity_rad_s: [0.0, 0.0, 0.1],
                cells: vec![SourceCellRef {
                    volume_id: volume,
                    part_id: 1,
                    cell: [0, 0, 0],
                    material: MaterialId(1),
                    source_revision: 3,
                }],
            }],
        },
    );
    // The original grid has -250m of empty padding. The packed component
    // starts at source cell zero, so -900m is inside the restore range.
    let captured = world
        .capture_dynamic_corpse_owner(
            id,
            DynamicCorpseOwnerFence {
                owner_key,
                source_generation: 2,
                topology_revision: 3,
                pose_token: 4,
                next_component_serial: 1,
            },
            &canonical,
        )
        .unwrap();
    assert_eq!(
        captured.source_parts[0].grid.origin(),
        GlobalCell::new(-1000, 0, 0)
    );
    assert_eq!(captured.components[0].cells[0].cell, [0, 0, 0]);
    let before = world.articulated_owner_snapshot(id).unwrap().root;
    let mut rebuilt = PhysicsWorld::new(PhysicsConfig::default());
    let rebuilt_id = restore(&mut rebuilt, captured);
    assert_same_motion(
        before,
        rebuilt.articulated_owner_snapshot(rebuilt_id).unwrap().root,
    );
    let hit = rebuilt
        .raycast_articulated(rebuilt_id, [-899.875, 0.125, -1.0], [0.0, 0.0, 1.0], 2.0)
        .unwrap();
    assert_eq!((hit.volume_id, hit.cell), (volume, [0, 0, 0]));
}
