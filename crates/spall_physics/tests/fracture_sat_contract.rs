//! Adversarial contract fixtures for the bounded interval rotated-cell predicate.
//! These exercise public behavior; they are not an independent numerical oracle.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::fracture::{
    FracturePartSnapshot, FractureSnapshot, FractureSnapshotToken, MAX_FRACTURE_SAT_AXIS_TESTS,
    prepare_fracture,
};
use spall_physics::{OccupancyGrid, PartTransform, SourcePart};

fn one_cell(part_id: u32, volume: u64, translation: [f32; 3], angle: f32) -> FracturePartSnapshot {
    let grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [1, 1, 1],
        vec![true],
        vec![MaterialId(1)],
    )
    .unwrap();
    FracturePartSnapshot {
        source: SourcePart {
            part_id,
            parent_id: None,
            volume_id: VolumeId::new(volume).unwrap(),
            source_revision: 1,
            grid,
            cell_m: 1.0,
            local_transform: PartTransform::IDENTITY,
        },
        root_local_pose: PartTransform {
            translation_m: translation,
            rotation_xyzw: [0.0, 0.0, (angle * 0.5).sin(), (angle * 0.5).cos()],
        },
    }
}

fn pair(offset: [f32; 3]) -> FractureSnapshot {
    FractureSnapshot {
        token: FractureSnapshotToken {
            corpse_key: 0x214,
            component_key: None,
            topology_revision: 1,
            pose_epoch: 1,
            source_generation: 1,
        },
        parts: vec![
            one_cell(1, 2101, [0.0; 3], std::f32::consts::FRAC_PI_4),
            one_cell(2, 2102, offset, std::f32::consts::FRAC_PI_4),
        ],
    }
}

#[test]
fn rotated_boxes_separate_when_origin_spheres_and_world_aabbs_overlap() {
    // The relative displacement projects to >1 cell along both rotated local
    // axes, although the world AABBs and deliberately loose origin spheres overlap.
    let plan = prepare_fracture(&pair([1.1, 1.1, 0.0])).unwrap();
    assert_eq!(plan.components.len(), 2);
    assert_eq!(plan.certified_cross_volume_edges, 0);
    assert!(plan.sat_axes_tested > 0);
    assert!(plan.sat_axes_tested <= MAX_FRACTURE_SAT_AXIS_TESTS);
}

#[test]
fn rotated_boxes_connect_only_after_strict_overlap_is_certified() {
    let plan = prepare_fracture(&pair([0.9, 0.0, 0.0])).unwrap();
    assert_eq!(plan.components.len(), 1);
    assert_eq!(plan.certified_cross_volume_edges, 1);
    assert!(plan.sat_axes_tested > 0);
}

#[test]
fn nearly_parallel_clear_gap_does_not_become_a_weld() {
    let mut input = pair([1.0001, 0.0, 0.0]);
    input.parts[1].root_local_pose.rotation_xyzw = [
        0.0,
        0.0,
        (0.000_001_f32 * 0.5).sin(),
        (0.000_001_f32 * 0.5).cos(),
    ];
    let result = prepare_fracture(&input);
    // The tiny rotation makes the boxes non-lattice. A clear gap must be
    // certified as separate; uncertainty is acceptable, a guessed weld is not.
    match result {
        Ok(plan) => {
            assert_eq!(plan.components.len(), 2);
            assert_eq!(plan.certified_cross_volume_edges, 0);
        }
        Err(spall_physics::fracture::FractureError::AmbiguousContact { .. }) => {}
        Err(other) => panic!("unexpected bounded-planner failure: {other:?}"),
    }
}
