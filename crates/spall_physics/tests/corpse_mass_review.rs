//! Independent closed-form solid-body oracle for frozen source assemblies.
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    MassProperties, OccupancyGrid, PartTransform, SourcePart, articulated_mass_properties,
};

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 2e-5,
        "actual={actual}, expected={expected}"
    );
}
fn check(actual: MassProperties, mass: f64, center: [f64; 3], inertia: [[f64; 3]; 3]) {
    assert_close(actual.mass_kg, mass);
    for (i, expected_center) in center.iter().enumerate() {
        assert_close(actual.com_m[i], *expected_center);
        for (j, expected_entry) in inertia[i].iter().enumerate() {
            assert_close(actual.inertia_com[i][j], *expected_entry);
        }
    }
}

#[test]
fn rotated_negative_origin_box_and_nested_dense_cube_match_closed_form_tensor() {
    // A 2 x 1 x 1 metre box, rho=2: mass=4, principal inertia=(2/3,5/3,5/3).
    // Rotate it 45 degrees about Z. This oracle uses solid-box formulas rather
    // than enumerating voxel masses or invoking the production transform code.
    let angle = std::f32::consts::FRAC_PI_4 * 0.5;
    let root_pose = PartTransform {
        translation_m: [-2.0, 3.0, 1.0],
        rotation_xyzw: [0.0, 0.0, angle.sin(), angle.cos()],
    };
    let root = SourcePart {
        part_id: 1,
        parent_id: None,
        volume_id: VolumeId::new(1101).unwrap(),
        source_revision: 7,
        cell_m: 1.0,
        local_transform: root_pose,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(-1, 0, 0),
            [2, 1, 1],
            vec![true; 2],
            vec![MaterialId(8); 2],
        )
        .unwrap(),
    };
    let q = std::f64::consts::FRAC_1_SQRT_2;
    let box_center = [-2.0 - 0.5 * q, 3.0 + 0.5 * q, 1.5];
    let box_tensor = [
        [7.0 / 6.0, -0.5, 0.0],
        [-0.5, 7.0 / 6.0, 0.0],
        [0.0, 0.0, 5.0 / 3.0],
    ];
    let density = |m: MaterialId| if m == MaterialId(8) { 2.0 } else { 4.0 };
    check(
        articulated_mass_properties(std::slice::from_ref(&root), &[root_pose], density).unwrap(),
        4.0,
        box_center,
        box_tensor,
    );

    // Nested cube has rho=4, mass=4 and intrinsic inertia=2/3 on every axis.
    // Its local centre before the parent's rotation is (1.5,-0.5,1).
    let child_pose = PartTransform {
        translation_m: [1.0, -1.0, 0.5],
        ..PartTransform::IDENTITY
    };
    let child = SourcePart {
        part_id: 2,
        parent_id: Some(1),
        volume_id: VolumeId::new(1102).unwrap(),
        source_revision: 9,
        cell_m: 1.0,
        local_transform: child_pose,
        grid: OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 1, 1],
            vec![true],
            vec![MaterialId(9)],
        )
        .unwrap(),
    };
    let cube_center = [-2.0 + 2.0 * q, 3.0 + q, 2.0];
    let center = std::array::from_fn(|i| (box_center[i] + cube_center[i]) * 0.5);
    let delta: [f64; 3] = std::array::from_fn(|i| cube_center[i] - box_center[i]);
    let squared = delta.iter().map(|v| v * v).sum::<f64>();
    // Equal masses => combined parallel-axis term = 2 (|delta|² I - delta deltaᵀ).
    let tensor = std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let diagonal = if i == j { 1.0 } else { 0.0 };
            box_tensor[i][j]
                + (2.0 / 3.0) * diagonal
                + 2.0 * (squared * diagonal - delta[i] * delta[j])
        })
    });
    // Deliberately reverse part order: hierarchy resolution must not depend on it.
    check(
        articulated_mass_properties(&[child, root], &[child_pose, root_pose], density).unwrap(),
        8.0,
        center,
        tensor,
    );
}
