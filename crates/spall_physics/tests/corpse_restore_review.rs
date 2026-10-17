//! Independent coordinator checks of the public reconstruction adapter.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    DerivedFractureComponentId, DynamicCorpseComponentRestore, DynamicCorpseRestore, OccupancyGrid,
    PartTransform, PhysicsConfig, PhysicsWorld, Representation, SourceCellRef, SourcePart,
};

fn bundle(owner_key: u128, volume: u64) -> DynamicCorpseRestore {
    let volume_id = VolumeId::new(volume).unwrap();
    DynamicCorpseRestore {
        owner_key,
        source_generation: 4,
        topology_revision: 3,
        pose_epoch: 29,
        next_component_serial: 100,
        source_parts: vec![SourcePart {
            part_id: 1,
            parent_id: None,
            volume_id,
            source_revision: 7,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(-3, 0, 0),
                [1, 1, 1],
                vec![true],
                vec![MaterialId(1)],
            )
            .unwrap(),
            cell_m: 0.5,
            local_transform: PartTransform::IDENTITY,
        }],
        components: vec![DynamicCorpseComponentRestore {
            component_id: Some(DerivedFractureComponentId {
                owner_key,
                serial: 41,
            }),
            translation_m: [10.0, 0.0, 0.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [1.0, 2.0, 3.0],
            angular_velocity_rad_s: [0.25, 0.0, 0.5],
            cells: vec![SourceCellRef {
                volume_id,
                part_id: 1,
                cell: [-3, 0, 0],
                material: MaterialId(1),
                source_revision: 7,
            }],
        }],
    }
}

#[test]
fn public_restore_preserves_negative_source_coordinates_and_live_world_on_failure() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let key = DerivedFractureComponentId {
        owner_key: 77,
        serial: 41,
    };
    let owner = world
        .restore_dynamic_corpse(bundle(77, 101), Representation::MergedCuboids, |_| 900.0)
        .unwrap();
    let before = world.fracture_component_state(owner, key).unwrap();
    assert_eq!(before.translation_m, [10.0, 0.0, 0.0]);
    assert_eq!(before.linvel_m_s, [1.0, 2.0, 3.0]);
    assert_eq!(before.angvel_rad_s, [0.25, 0.0, 0.5]);
    assert!((before.mass_kg - 112.5).abs() < 1.0e-4);
    let hit = world
        .raycast_articulated(owner, [8.75, 0.25, -1.0], [0.0, 0.0, 1.0], 3.0)
        .unwrap();
    assert_eq!(hit.cell, [-3, 0, 0]);
    assert_eq!(hit.volume_id, VolumeId::new(101).unwrap());
    assert_eq!(hit.source_revision, 7);
    assert_eq!(hit.topology_token, 3);
    assert_eq!(hit.pose_token, 29);
    let count = world.body_count();
    for case in 0..3 {
        let mut invalid = bundle(78, 102);
        match case {
            0 => invalid.components[0].cells[0].material = MaterialId(2),
            1 => invalid.components[0].angular_velocity_rad_s[0] = f32::NAN,
            2 => invalid.components[0].component_id.as_mut().unwrap().serial = 100,
            _ => unreachable!(),
        }
        assert!(
            world
                .restore_dynamic_corpse(invalid, Representation::MergedCuboids, |_| 900.0)
                .is_err()
        );
        assert_eq!(world.body_count(), count);
        let after = world.fracture_component_state(owner, key).unwrap();
        assert_eq!(after.translation_m, before.translation_m);
        assert_eq!(after.linvel_m_s, before.linvel_m_s);
        assert_eq!(after.angvel_rad_s, before.angvel_rad_s);
        assert_eq!(after.mass_kg, before.mass_kg);
    }
    // The failed preparations must not consume the owner key or source ID.
    assert!(
        world
            .restore_dynamic_corpse(bundle(78, 102), Representation::MergedCuboids, |_| 900.0)
            .is_ok()
    );
}
