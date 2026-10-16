//! Independent source-connectivity contracts: material boundaries are solid.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::fracture::{
    FractureError, FracturePartSnapshot, FractureSnapshot, FractureSnapshotToken, prepare_fracture,
};
use spall_physics::{OccupancyGrid, PartTransform, SourcePart};

fn snapshot(origin: GlobalCell, mask: Vec<bool>, revision: u64) -> FractureSnapshot {
    let materials = (0..mask.len())
        .map(|i| MaterialId(if i % 2 == 0 { 8 } else { 9 }))
        .collect();
    let grid =
        OccupancyGrid::from_solid_mask(origin, [mask.len() as u32, 1, 1], mask, materials).unwrap();
    FractureSnapshot {
        token: FractureSnapshotToken {
            corpse_key: 42,
            component_key: None,
            topology_revision: revision,
            pose_epoch: 1,
            source_generation: 1,
        },
        parts: vec![FracturePartSnapshot {
            source: SourcePart {
                part_id: 1,
                parent_id: None,
                volume_id: VolumeId::new(1101).unwrap(),
                source_revision: revision,
                grid,
                cell_m: 0.0625,
                local_transform: PartTransform::IDENTITY,
            },
            root_local_pose: PartTransform::IDENTITY,
        }],
    }
}

#[test]
fn material_change_does_not_cut_a_solid_bridge_but_removing_its_cell_does() {
    let connected = prepare_fracture(&snapshot(
        GlobalCell::new(-2, 0, 0),
        vec![true, true, true],
        1,
    ))
    .unwrap();
    assert_eq!(connected.components.len(), 1);
    assert_eq!(connected.source_cells.len(), 3);
    assert_eq!(connected.components[0].cells.len(), 3);
    let disconnected = prepare_fracture(&snapshot(
        GlobalCell::new(-2, 0, 0),
        vec![true, false, true],
        2,
    ))
    .unwrap();
    assert_eq!(disconnected.components.len(), 2);
    assert_eq!(disconnected.source_cells.len(), 2);
    let mut cells: Vec<_> = disconnected
        .components
        .iter()
        .flat_map(|c| c.cells.iter().map(|v| v.cell))
        .collect();
    cells.sort_unstable();
    assert_eq!(cells, vec![[-2, 0, 0], [0, 0, 0]]);
}

#[test]
fn unrepresentable_origin_is_rejected_without_panicking() {
    let invalid = snapshot(GlobalCell::new(i64::MAX, 0, 0), vec![false, true], 1);
    assert!(matches!(
        prepare_fracture(&invalid),
        Err(FractureError::InvalidSnapshot)
    ));
}

// These rotations have exact canonical quaternions; derive box minima from
// reflected unit-cell corners, independently of the planner's cell mapping.
#[test]
fn half_turns_preserve_exact_overlap_face_edge_and_corner_connectivity() {
    for (rotation, signs) in [
        ([1.0, 0.0, 0.0, 0.0], [1, -1, -1]),
        ([0.0, 1.0, 0.0, 0.0], [-1, 1, -1]),
        ([0.0, 0.0, 1.0, 0.0], [-1, -1, 1]),
    ] {
        let rotated_min = signs.map(|sign| if sign < 0 { -1 } else { 0 });
        for dx in -1i32..=1 {
            for dy in -1i32..=1 {
                for dz in -1i32..=1 {
                    let offset = [dx, dy, dz];
                    let origin = GlobalCell::new(
                        i64::from(rotated_min[0] + dx),
                        i64::from(rotated_min[1] + dy),
                        i64::from(rotated_min[2] + dz),
                    );
                    let mut input = snapshot(origin, vec![true], 1);
                    let mut reflected = snapshot(GlobalCell::new(0, 0, 0), vec![true], 1)
                        .parts
                        .remove(0);
                    reflected.source.part_id = 2;
                    reflected.source.volume_id = VolumeId::new(1102).unwrap();
                    reflected.root_local_pose.rotation_xyzw = rotation;
                    input.parts.push(reflected);
                    let plan = prepare_fracture(&input).unwrap();
                    let expected = if dx.abs() + dy.abs() + dz.abs() <= 1 {
                        1
                    } else {
                        2
                    };
                    assert_eq!(
                        plan.components.len(),
                        expected,
                        "rotation {rotation:?}, offset {offset:?}"
                    );
                    assert_eq!(plan.source_cells.len(), 2);
                }
            }
        }
    }
}

#[test]
fn large_translation_fractional_offsets_require_correct_geometric_classification() {
    let mut input = snapshot(GlobalCell::new(-16_777_216, 0, 0), vec![true], 1);
    input.parts[0].source.cell_m = 1.0;
    input.parts[0].root_local_pose.translation_m = [16_777_216.0, 0.0, 0.0];
    let mut other = snapshot(GlobalCell::new(0, 0, 0), vec![true], 1)
        .parts
        .remove(0);
    other.source.part_id = 2;
    other.source.volume_id = VolumeId::new(1102).unwrap();
    other.source.cell_m = 1.0;
    other.root_local_pose.translation_m = [0.5, 0.0, 0.0];
    input.parts.push(other);
    // Exact affine boxes [0,1] and [.5,1.5] overlap, despite not sharing a
    // unit lattice. The separate interval certificate may now prove this.
    assert_eq!(prepare_fracture(&input).unwrap().components.len(), 1);
    input.parts[1].root_local_pose.translation_m[0] = 1.5;
    assert_eq!(prepare_fracture(&input).unwrap().components.len(), 2);
    // Partial-face tangency has no strict positive-volume certificate. The
    // lattice path must not erase the fractional x offset and guess a weld.
    input.parts[1].root_local_pose.translation_m = [0.5, 1.0, 0.0];
    assert!(matches!(
        prepare_fracture(&input),
        Err(FractureError::AmbiguousContact { .. })
    ));
}

#[test]
fn a_single_source_volume_does_not_spend_cross_volume_pair_budget() {
    let input = snapshot(GlobalCell::new(-1024, 0, 0), vec![true; 2048], 1);
    let plan = prepare_fracture(&input).unwrap();
    assert_eq!(plan.components.len(), 1);
    assert_eq!(plan.source_cells.len(), 2048);
    assert_eq!(plan.candidate_pairs_tested, 0);
}

#[test]
fn total_dense_scan_budget_covers_sparse_parts_together() {
    let mut mask = vec![false; 500_001];
    mask[0] = true;
    let mut input = snapshot(GlobalCell::new(0, 0, 0), mask.clone(), 1);
    let mut other = snapshot(GlobalCell::new(0, 0, 0), mask, 1).parts.remove(0);
    other.source.part_id = 2;
    other.source.volume_id = VolumeId::new(1102).unwrap();
    input.parts.push(other);
    assert!(matches!(
        prepare_fracture(&input),
        Err(FractureError::WorkBudget)
    ));
}

#[test]
fn distant_arbitrarily_rotated_parts_are_proven_separate() {
    let mut input = snapshot(GlobalCell::new(-2, 0, 0), vec![true], 1);
    let mut other = snapshot(GlobalCell::new(0, 0, 0), vec![true], 1)
        .parts
        .remove(0);
    other.source.part_id = 2;
    other.source.volume_id = VolumeId::new(1102).unwrap();
    other.root_local_pose.translation_m = [100.0, 0.0, 0.0];
    other.root_local_pose.rotation_xyzw =
        [0.0, 0.0, (0.37_f32 / 2.0).sin(), (0.37_f32 / 2.0).cos()];
    input.parts.push(other);
    let plan = prepare_fracture(&input).unwrap();
    assert_eq!(plan.components.len(), 2);
    assert_eq!(plan.source_cells.len(), 2);
    assert_eq!(plan.certified_cross_volume_edges, 0);
}
