use std::sync::Arc;

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::phase_water::{PhaseError, PhaseFace, PhaseLimits, PhaseWater};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

fn fixture(
    dims: [u32; 3],
    solid: impl Fn(u32, u32, u32) -> bool,
    water: impl Fn(u32, u32, u32) -> f64,
    max_basins: usize,
) -> PhaseWater {
    let id = VolumeId::new(7).unwrap();
    let mut terrain = Volume::new(id, CellSizeCode::Quarter);
    terrain
        .insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
    let mut edits = EditPlan::new(id);
    let mut fractions = Vec::new();
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                if solid(x, y, z) {
                    edits.set(GlobalCell::new(x as i64, y as i64, z as i64), MaterialId(1));
                    fractions.push(0.0);
                } else {
                    fractions.push(water(x, y, z));
                }
            }
        }
    }
    terrain.apply_edit(&edits).unwrap();
    let boundary = SolidBoundary::capture(
        &terrain,
        DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 30_000).unwrap(),
    )
    .unwrap();
    let geometry = Arc::new(
        CutCellGeometry::build(
            &boundary,
            3,
            GeometryLimits {
                max_fine_cells: 30_000,
                max_components: 30_000,
                max_portals: 90_000,
            },
        )
        .unwrap(),
    );
    PhaseWater::new(
        geometry,
        &fractions,
        0.25,
        PhaseLimits {
            max_fine_cells: 30_000,
            max_faces: 90_000,
            max_basins,
        },
    )
    .unwrap()
}

fn index(dims: [usize; 3], x: usize, y: usize, z: usize) -> usize {
    x + dims[0] * (y + dims[1] * z)
}

#[test]
fn same_coarse_total_retains_opposite_pool_provenance() {
    let state = |side| {
        fixture(
            [3; 3],
            |x, y, _| x == 1 && y < 2,
            |x, y, _| if x == side && y == 0 { 0.3 } else { 0.0 },
            100,
        )
    };
    let left = state(0);
    let right = state(2);
    assert_eq!(left.component_amounts_m3(), right.component_amounts_m3());
    assert_eq!(left.component_amounts_m3().len(), 1);
    assert_ne!(left.fractions(), right.fractions());
    assert_ne!(
        left.basins().unwrap()[0].anchor,
        right.basins().unwrap()[0].anchor
    );
    assert_eq!(
        left.basins().unwrap()[0].water_m3,
        right.basins().unwrap()[0].water_m3
    );
}

#[test]
fn below_crest_pools_separate_but_overtopped_pools_join() {
    let state = |height: f64| {
        fixture(
            [3; 3],
            |x, y, _| x == 1 && y < 2,
            |_, y, _| (height - y as f64).clamp(0.0, 1.0),
            100,
        )
    };
    let below = state(1.3);
    assert_eq!(below.basins().unwrap().len(), 2);
    let above = state(2.3);
    assert_eq!(above.basins().unwrap().len(), 1);
    let basin_total: f64 = below.basins().unwrap().iter().map(|b| b.water_m3).sum();
    assert!((basin_total - below.water_volume_m3()).abs() < 1e-15);
}

#[test]
fn partial_horizontal_overlap_and_dry_vertical_gap_are_exact() {
    let state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| {
            if z == 0 && y <= 1 && x <= 1 {
                if x == 0 { 0.3 } else { 0.7 }
            } else {
                0.0
            }
        },
        100,
    );
    let areas = state.wet_face_areas_m2();
    let horizontal = state
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    let vertical = state
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 3)
        .unwrap();
    assert_eq!(areas[horizontal], 0.3 * 0.25_f64.powi(2));
    assert_eq!(areas[vertical], 0.0);
    assert_eq!(state.basins().unwrap().len(), 2);
}

#[test]
fn tiny_water_is_retained_and_not_slept_or_merged_through_air() {
    let state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| {
            if y == 0 && z == 0 && x != 1 {
                1e-20
            } else {
                0.0
            }
        },
        100,
    );
    assert_eq!(state.basins().unwrap().len(), 2);
    assert_eq!(state.water_volume_m3(), 2e-20 * 0.25_f64.powi(3));
}

#[test]
fn donor_wet_area_admits_flow_into_dry_receiver_and_conserves_water() {
    let mut state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| if x == 0 && y == 0 && z == 0 { 0.3 } else { 0.0 },
        100,
    );
    let initial = state.water_volume_m3();
    let mut q = vec![0.0; state.faces().len()];
    let face = state
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    assert_eq!(state.wet_face_areas_m2()[face], 0.0);
    q[face] = 0.01;
    let m = state.transport(0.1, &q, 0.45).unwrap();
    assert!((m.moved_water_m3 - 0.0003).abs() < 1e-16);
    assert!((state.fractions()[1] - 0.0003 / 0.25_f64.powi(3)).abs() < 1e-15);
    assert!((state.water_volume_m3() - initial).abs() < 1e-16);
}

#[test]
fn saturated_chain_retains_simultaneous_throughflow() {
    let mut state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| if x < 2 && y == 0 && z == 0 { 1.0 } else { 0.0 },
        100,
    );
    let initial = state.water_volume_m3();
    let mut q = vec![0.0; state.faces().len()];
    for (i, f) in state.faces().iter().enumerate() {
        if f.axis == 0 && f.lower < 2 {
            q[i] = 0.01;
        }
    }
    let m = state.transport(0.1, &q, 0.45).unwrap();
    assert_eq!(m.limiter_passes, 0);
    assert_eq!(state.fractions()[1], 1.0);
    assert!((state.water_volume_m3() - initial).abs() < 1e-16);
    assert!(state.fractions()[2] > 0.0);
}

#[test]
fn vertical_downflow_limits_available_water_without_clipping() {
    let mut state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| {
            if x == 0 && y == 1 && z == 0 {
                0.01
            } else {
                0.0
            }
        },
        100,
    );
    let initial = state.water_volume_m3();
    let mut q = vec![0.0; state.faces().len()];
    let face = state
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 3)
        .unwrap();
    q[face] = -0.01;
    let m = state.transport(0.1, &q, 0.45).unwrap();
    assert!(m.limiter_passes > 0 && m.limited_water_m3 > 0.0);
    assert!(state.fractions()[0] > 0.0);
    assert!(state.fractions()[3] >= 0.0);
    assert!((state.water_volume_m3() - initial).abs() < 1e-16);
}

#[test]
fn dry_crest_air_circulation_does_not_transport_water() {
    let mut state = fixture(
        [9, 6, 3],
        |x, y, _| x == 4 && y < 5,
        |x, y, _| if x < 4 && y < 2 { 1.0 } else { 0.0 },
        100,
    );
    let initial = state.fractions().to_vec();
    // Divergence-free square circulation entirely in air above the dam crest.
    let cycle = [
        index([9, 6, 3], 3, 5, 0),
        index([9, 6, 3], 4, 5, 0),
        index([9, 6, 3], 4, 5, 1),
        index([9, 6, 3], 3, 5, 1),
    ];
    let mut q = vec![0.0; state.faces().len()];
    for edge in 0..4 {
        let a = cycle[edge];
        let b = cycle[(edge + 1) % 4];
        let face = state
            .faces()
            .iter()
            .position(|f| f.lower == a.min(b) && f.upper == a.max(b))
            .unwrap();
        q[face] = if a < b { 0.001 } else { -0.001 };
    }
    for _ in 0..600 {
        let metrics = state.transport(0.01, &q, 0.45).unwrap();
        assert_eq!(metrics.moved_water_m3, 0.0);
    }
    assert_eq!(state.fractions(), initial);
    assert_eq!(state.basins().unwrap().len(), 1);
}

#[test]
fn rejected_flux_and_fragmentation_leave_phase_state_unchanged() {
    let mut state = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, z| {
            if y == 0 && z == 0 {
                if x == 1 { 1.0 } else { 0.2 }
            } else {
                0.0
            }
        },
        1,
    );
    let initial = state.fractions().to_vec();
    let mut q = vec![0.0; state.faces().len()];
    let face = state
        .faces()
        .iter()
        .position(|f| f.lower == 1 && f.upper == 4)
        .unwrap();
    q[face] = f64::NAN;
    assert!(matches!(
        state.transport(1.0, &q, 1.0),
        Err(PhaseError::InvalidState)
    ));
    q[face] = 1.0;
    assert!(matches!(
        state.transport(1.0, &q, 1.0),
        Err(PhaseError::Cfl { .. })
    ));
    q[face] = 0.25_f64.powi(3);
    assert!(matches!(
        state.transport(1.0, &q, 1.0),
        Err(PhaseError::Limit { kind: "basins", .. })
    ));
    assert_eq!(state.fractions(), initial);
}

#[test]
fn explicit_phase_allocation_limits_and_invalid_seed_fail() {
    let state = fixture([3; 3], |_, _, _| false, |_, _, _| 0.0, 100);
    let geometry = state.geometry().clone();
    let normal = PhaseLimits {
        max_fine_cells: 27,
        max_faces: 54,
        max_basins: 1,
    };
    assert!(matches!(
        PhaseWater::new(
            geometry.clone(),
            state.fractions(),
            0.25,
            PhaseLimits {
                max_fine_cells: 26,
                ..normal
            }
        ),
        Err(PhaseError::Limit {
            kind: "fine cells",
            ..
        })
    ));
    assert!(matches!(
        PhaseWater::new(
            geometry.clone(),
            state.fractions(),
            0.25,
            PhaseLimits {
                max_faces: 53,
                ..normal
            }
        ),
        Err(PhaseError::Limit { kind: "faces", .. })
    ));
    for scale in [0.0, f64::NAN, f64::INFINITY, 1e-200, 1e200] {
        assert!(matches!(
            PhaseWater::new(geometry.clone(), state.fractions(), scale, normal),
            Err(PhaseError::InvalidState)
        ));
    }
    let mut bad = state.fractions().to_vec();
    for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
        bad[0] = value;
        assert!(PhaseWater::new(geometry.clone(), &bad, 0.25, normal).is_err());
    }
    let solid = fixture(
        [3; 3],
        |x, y, z| x == 1 && y == 1 && z == 1,
        |_, _, _| 0.0,
        100,
    );
    let mut bad = solid.fractions().to_vec();
    bad[13] = 0.1;
    assert!(PhaseWater::new(solid.geometry().clone(), &bad, 0.25, normal).is_err());
}

#[test]
fn fine_opening_passes_water_while_intact_wall_has_no_transport_face() {
    let state = |opening| {
        fixture(
            [3; 3],
            |x, y, z| x == 1 && !(opening && y == 0 && z == 1),
            |x, y, _| if x == 0 && y == 0 { 1.0 } else { 0.0 },
            100,
        )
    };
    let intact = state(false);
    assert!(intact.faces().iter().all(|f| f.axis != 0));
    let mut opened = state(true);
    let initial = opened.water_volume_m3();
    let a = index([3; 3], 0, 0, 1);
    let b = index([3; 3], 1, 0, 1);
    let c = index([3; 3], 2, 0, 1);
    let mut q = vec![0.0; opened.faces().len()];
    for (i, f) in opened.faces().iter().enumerate() {
        if f.lower == a && f.upper == b || f.lower == b && f.upper == c {
            q[i] = 0.01;
        }
    }
    for _ in 0..60 {
        opened.transport(0.01, &q, 0.45).unwrap();
    }
    assert!(opened.fractions()[c] > 0.0);
    assert!((opened.water_volume_m3() - initial).abs() < 1e-14);
}

#[test]
fn coarse_portal_summary_preserves_partial_donor_aperture_without_air_averaging() {
    let state = fixture(
        [6, 3, 3],
        |_, _, _| false,
        |x, y, _| if x == 2 && y == 0 { 0.3 } else { 0.0 },
        100,
    );
    let areas = state.portal_phase_areas_m2();
    assert_eq!(areas.len(), 1);
    assert_eq!(areas[0].overlap_m2, 0.0);
    assert_eq!(areas[0].upper_donor_m2, 0.0);
    assert!((areas[0].lower_donor_m2 - 3.0 * 0.3 * 0.25_f64.powi(2)).abs() < 1e-16);
    let far = fixture(
        [6, 3, 3],
        |_, _, _| false,
        |x, y, _| if x == 0 && y == 0 { 0.3 } else { 0.0 },
        100,
    );
    assert_eq!(state.component_amounts_m3(), far.component_amounts_m3());
    assert_eq!(far.portal_phase_areas_m2()[0].lower_donor_m2, 0.0);
    // Equal coarse totals have different directional wetted apertures.
    let fine_areas = state.wet_face_areas_m2();
    for (face, area) in state.faces().iter().zip(fine_areas) {
        if face.axis == 0 && face.lower % 6 == 2 {
            assert_eq!(area, 0.0);
        }
    }
}

#[test]
fn packed_faces_preserve_dense_geometry_endpoints_order_and_storage() {
    let dims = [6usize, 3, 3];
    let state = fixture(
        dims.map(|n| n as u32),
        |x, y, z| (x + 3 * y + 5 * z) % 7 == 0,
        |_, _, _| 0.0,
        100,
    );
    let mut expected = Vec::new();
    let strides = [1, dims[0], dims[0] * dims[1]];
    let spec = state.geometry().fine_spec();
    for lower in 0..54 {
        let xyz = [
            lower % dims[0],
            lower / dims[0] % dims[1],
            lower / strides[2],
        ];
        let cell = |p: [usize; 3]| GlobalCell::new(p[0] as i64, p[1] as i64, p[2] as i64);
        if state.geometry().component_at(cell(xyz)).is_none() {
            continue;
        }
        for axis in 0..3 {
            let mut next = xyz;
            next[axis] += 1;
            if next[axis] < dims[axis] && state.geometry().component_at(cell(next)).is_some() {
                expected.push(PhaseFace {
                    lower,
                    upper: lower + strides[axis],
                    axis: axis as u8,
                });
            }
        }
    }
    assert_eq!(state.faces().iter().collect::<Vec<_>>(), expected);
    assert_eq!(
        state.faces().iter().rev().collect::<Vec<_>>(),
        expected.iter().rev().copied().collect::<Vec<_>>()
    );
    assert_eq!(
        state.array_storage_bytes(),
        spec.cell_count() * 8 + expected.len() * 4
    );
}

// Frozen numerical reference from 6690a58, before buffer reuse. It deliberately
// retains the original candidate/donor/receiver arrays to detect changes in
// arithmetic order and limiter propagation, rather than sharing new helpers.
fn original_transport(
    fractions: &[f64],
    faces: &[PhaseFace],
    dt: f64,
    flux: &[f64],
) -> Result<(Vec<f64>, f64, f64, u32), PhaseError> {
    let volume = 0.25_f64.powi(3);
    let mut outgoing = vec![0.0; fractions.len()];
    let mut transfers = Vec::new();
    for (&f, &q) in faces.iter().zip(flux) {
        let donor = if q >= 0.0 { f.lower } else { f.upper };
        outgoing[donor] += dt * q.abs() / volume;
        let wet = if f.axis != 1 {
            fractions[donor]
        } else if q >= 0.0 {
            f64::from(fractions[donor] == 1.0)
        } else {
            f64::from(fractions[donor] > 0.0)
        };
        transfers.push(dt * q / volume * wet);
    }
    let measured = outgoing.into_iter().fold(0.0, f64::max);
    if measured > 0.45 * (1.0 + 32.0 * f64::EPSILON) {
        return Err(PhaseError::Cfl {
            measured,
            limit: 0.45,
        });
    }
    let original_moved: f64 = transfers.iter().map(|t| t.abs()).sum();
    let mut passes = 0;
    let next = loop {
        let mut incoming = vec![0.0; fractions.len()];
        let mut outgoing = vec![0.0; fractions.len()];
        for (&f, &t) in faces.iter().zip(&transfers) {
            let (a, b) = if t >= 0.0 {
                (f.lower, f.upper)
            } else {
                (f.upper, f.lower)
            };
            outgoing[a] += t.abs();
            incoming[b] += t.abs();
        }
        let next: Vec<_> = fractions
            .iter()
            .enumerate()
            .map(|(i, &v)| (v + incoming[i]) - outgoing[i])
            .collect();
        if next
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        {
            break next;
        }
        if passes == 64 {
            return Err(PhaseError::LimiterExhausted);
        }
        let mut donor = vec![1.0_f64; next.len()];
        let mut receiver = vec![1.0_f64; next.len()];
        for (i, &v) in next.iter().enumerate() {
            if v < 0.0 {
                donor[i] = ((fractions[i] + incoming[i]) / outgoing[i]).clamp(0.0, 1.0)
                    * (1.0 - 32.0 * f64::EPSILON);
            } else if v > 1.0 {
                receiver[i] = ((1.0 - fractions[i] + outgoing[i]) / incoming[i]).clamp(0.0, 1.0)
                    * (1.0 - 32.0 * f64::EPSILON);
            }
        }
        for (&f, t) in faces.iter().zip(&mut transfers) {
            let (a, b) = if *t >= 0.0 {
                (f.lower, f.upper)
            } else {
                (f.upper, f.lower)
            };
            *t *= donor[a].min(receiver[b]);
        }
        passes += 1;
    };
    let moved: f64 = transfers.iter().map(|t| t.abs()).sum();
    Ok((
        next,
        moved * volume,
        (original_moved - moved).max(0.0) * volume,
        passes,
    ))
}

#[test]
fn two_buffer_transport_is_bit_identical_to_original_limiter() {
    let mut accepted = 0;
    let mut limited = 0;
    for seed in 0..64_u64 {
        let mut state = fixture(
            [6, 3, 3],
            |x, y, z| (x + 3 * y + 5 * z) % 11 == 0,
            |x, y, z| {
                [0.0, 0.001, 0.3, 0.999, 1.0][((u64::from(x + 6 * y + 18 * z) + seed) % 5) as usize]
            },
            100,
        );
        let faces: Vec<_> = state.faces().iter().collect();
        let mut reference = state.fractions().to_vec();
        for step in 0..8_u64 {
            let flux: Vec<_> = (0..faces.len())
                .map(|i| (((i as u64 * 17 + seed * 13 + step * 19) % 11) as f64 - 5.0) * 0.002)
                .collect();
            let expected = original_transport(&reference, &faces, 0.1, &flux);
            let result = state.transport(0.1, &flux, 0.45);
            match (expected, result) {
                (Ok((next, moved, reduced, passes)), Ok(metrics)) => {
                    assert_eq!(metrics.moved_water_m3.to_bits(), moved.to_bits());
                    assert_eq!(metrics.limited_water_m3.to_bits(), reduced.to_bits());
                    assert_eq!(metrics.limiter_passes, passes);
                    assert_eq!(
                        metrics.numeric_scratch_bytes,
                        (2 * reference.len() + faces.len()) * 8
                    );
                    reference = next;
                    accepted += 1;
                    limited += usize::from(passes > 0);
                }
                (Err(expected), Err(actual)) => assert_eq!(expected, actual),
                results => {
                    panic!("reference/new result differs at seed {seed}, step {step}: {results:?}")
                }
            }
            assert_eq!(
                state
                    .fractions()
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
                reference.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
    }
    assert!(accepted > 0 && limited > 0);
}

#[test]
fn two_buffer_limiter_exhaustion_preserves_all_amount_bits() {
    let mut state = fixture([12, 3, 3], |_, _, _| false, |_, _, _| 1.0, 100);
    let mut chain = Vec::new();
    for z in 0..3 {
        for row in 0..3 {
            let y = if z % 2 == 0 { row } else { 2 - row };
            for column in 0..12 {
                let x = if (z * 3 + row) % 2 == 0 {
                    column
                } else {
                    11 - column
                };
                chain.push(index([12, 3, 3], x, y, z));
            }
        }
    }
    let faces: Vec<_> = state.faces().iter().collect();
    let mut q = vec![0.0; faces.len()];
    for pair in chain.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let f = faces
            .iter()
            .position(|f| f.lower == a.min(b) && f.upper == a.max(b))
            .unwrap();
        q[f] = if a < b { 0.01 } else { -0.01 };
    }
    let initial: Vec<_> = state.fractions().iter().map(|v| v.to_bits()).collect();
    assert_eq!(
        original_transport(state.fractions(), &faces, 0.01, &q),
        Err(PhaseError::LimiterExhausted)
    );
    assert!(matches!(
        state.transport(0.01, &q, 0.45),
        Err(PhaseError::LimiterExhausted)
    ));
    assert_eq!(
        state
            .fractions()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        initial
    );
}
