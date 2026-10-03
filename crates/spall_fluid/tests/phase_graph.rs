use std::sync::Arc;

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::phase_graph::{GraphError, GraphLimits, PhaseClass, PhaseGraph};
use spall_fluid::phase_water::{PhaseLimits, PhaseWater};
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

fn graph(phase: &PhaseWater) -> PhaseGraph {
    PhaseGraph::build(
        phase,
        GraphLimits {
            max_fine_cells: 30_000,
            max_rows: 30_000,
            max_connections: 90_000,
        },
    )
    .unwrap()
}

#[test]
fn refresh_reuses_only_identical_membership_and_liquid_edges() {
    let limits = GraphLimits {
        max_fine_cells: 30_000,
        max_rows: 30_000,
        max_connections: 90_000,
    };
    let phase = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, _| {
            if y == 0 {
                if x == 0 { 0.4 } else { 0.8 }
            } else {
                0.0
            }
        },
        100,
    );
    let g = graph(&phase);
    let mut changed = phase.clone();
    let mut flux = vec![0.0; phase.faces().len()];
    let f = phase
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    flux[f] = 0.01;
    changed.transport(0.01, &flux, 0.45).unwrap();
    let (updated, reused) = g.refresh(&changed, limits).unwrap();
    assert!(reused);
    let fresh = graph(&changed);
    for i in 0..changed.fractions().len() {
        assert_eq!(updated.row_at_index(i), fresh.row_at_index(i));
    }
    assert!(
        (updated.rows().iter().map(|r| r.water_m3).sum::<f64>() - changed.water_volume_m3()).abs()
            < 1e-15
    );
    let weights = vec![1.0; flux.len()];
    let top = vec![0.0; 27];
    assert!(updated.pressure_operator(&changed, &weights, &top).is_ok());
    assert!(matches!(
        g.pressure_operator(&changed, &weights, &top),
        Err(GraphError::StaleSnapshot)
    ));
    assert!(matches!(
        g.refresh(
            &changed,
            GraphLimits {
                max_rows: 0,
                ..limits
            }
        ),
        Err(GraphError::Limit { .. })
    ));

    let vertical = fixture(
        [3; 3],
        |_, _, _| false,
        |_, y, _| {
            if y == 0 {
                0.875
            } else if y == 1 {
                0.5
            } else {
                0.0
            }
        },
        100,
    );
    let old = graph(&vertical);
    let mut next = vertical.clone();
    let mut flux = vec![0.0; next.faces().len()];
    let f = next
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 3)
        .unwrap();
    flux[f] = -0.125 * 0.25_f64.powi(3) / 0.01;
    next.transport(0.01, &flux, 0.45).unwrap();
    assert_eq!(next.fractions()[0], 1.0);
    let (updated, reused) = old.refresh(&next, limits).unwrap();
    assert!(!reused);
    let fresh = graph(&next);
    for i in 0..27 {
        assert_eq!(updated.row_at_index(i), fresh.row_at_index(i));
    }
    let dry = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, _| if x == 1 && y == 0 { 0.8 } else { 0.0 },
        100,
    );
    let mut wet = dry.clone();
    let old = graph(&dry);
    let mut flux = vec![0.0; wet.faces().len()];
    let f = wet
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    flux[f] = -0.01;
    wet.transport(0.01, &flux, 0.45).unwrap();
    assert!(!old.refresh(&wet, limits).unwrap().1);
}

#[test]
fn dry_crest_keeps_two_pools_inside_one_open_component() {
    let phase = fixture(
        [3; 3],
        |x, y, _| x == 1 && y < 2,
        |x, y, _| {
            if y == 0 && x == 0 {
                0.5
            } else if y == 0 && x == 2 {
                0.3
            } else {
                0.0
            }
        },
        100,
    );
    assert_eq!(phase.component_amounts_m3().len(), 1);
    let g = graph(&phase);
    assert_eq!(g.rows().len(), 3);
    assert_eq!(
        g.rows()
            .iter()
            .filter(|r| r.class == PhaseClass::Wet)
            .count(),
        2
    );
    assert_ne!(g.row_at_index(0), g.row_at_index(2));
    assert_eq!(g.row_at_index(1), None);
    assert_eq!(g.rows().iter().map(|r| r.fine_cells).sum::<u32>(), 21);
    assert!(
        (g.rows().iter().map(|r| r.water_m3).sum::<f64>() - phase.water_volume_m3()).abs() < 1e-15
    );
    assert!(
        g.connections().iter().all(
            |e| !(g.rows()[e.lower_row as usize].class == PhaseClass::Wet
                && g.rows()[e.upper_row as usize].class == PhaseClass::Wet)
        )
    );
}

#[test]
fn liquid_overlap_not_just_nonzero_amount_defines_rows() {
    let phase = fixture(
        [3; 3],
        |_, _, _| false,
        |_, y, _| if y < 2 { 0.001 } else { 0.0 },
        100,
    );
    let g = graph(&phase);
    // Partial lower cells do not reach the liquid above, but horizontal films connect.
    assert_eq!(g.rows().len(), 3);
    assert_eq!(g.row_at_index(0), g.row_at_index(2));
    assert_ne!(g.row_at_index(0), g.row_at_index(3));
    assert_eq!(g.rows()[g.row_at_index(0).unwrap() as usize].fine_cells, 9);
}

#[test]
fn heterogeneous_pressure_is_exact_galerkin_restriction() {
    let phase = fixture(
        [6, 3, 3],
        |x, y, z| x == 1 && y == 0 && z == 1,
        |x, y, _| {
            if y == 0 {
                if x < 2 { 0.2 } else { 0.6 }
            } else {
                0.0
            }
        },
        100,
    );
    let g = graph(&phase);
    let weights: Vec<_> = (0..phase.faces().len())
        .map(|i| 0.125 + (i % 7) as f64)
        .collect();
    let mut top = vec![0.0; phase.fractions().len()];
    for (i, w) in top.iter_mut().enumerate() {
        if i / 6 % 3 == 2 {
            *w = 0.7;
        }
    }
    let p: Vec<_> = (0..g.rows().len()).map(|i| i as f64 * 0.3 - 0.8).collect();
    let operator = g.pressure_operator(&phase, &weights, &top).unwrap();
    let actual = operator.apply(&p).unwrap();
    let mut expected = vec![0.0; p.len()];
    let mut energy = 0.0;
    let mut cross_faces = 0;
    for (face, w) in phase.faces().iter().zip(&weights) {
        let a = g.row_at_index(face.lower).unwrap() as usize;
        let b = g.row_at_index(face.upper).unwrap() as usize;
        let d = p[a] - p[b];
        expected[a] += w * d;
        expected[b] -= w * d;
        energy += w * d * d;
        cross_faces += usize::from(a != b);
    }
    for (i, w) in top.iter().enumerate() {
        if let Some(a) = g.row_at_index(i) {
            expected[a as usize] += w * p[a as usize];
            energy += w * p[a as usize].powi(2);
        }
    }
    for (a, b) in actual.iter().zip(&expected) {
        assert!((a - b).abs() < 1e-12);
    }
    assert!((actual.iter().zip(&p).map(|(a, b)| a * b).sum::<f64>() - energy).abs() < 1e-12);
    assert_eq!(
        g.connections()
            .iter()
            .map(|e| e.fine_faces as usize)
            .sum::<usize>(),
        cross_faces
    );
    let q: Vec<_> = p.iter().map(|v| v * v + 0.1).collect();
    let aq = operator.apply(&q).unwrap();
    assert!(
        (p.iter().zip(aq).map(|(a, b)| a * b).sum::<f64>()
            - q.iter().zip(actual).map(|(a, b)| a * b).sum::<f64>())
        .abs()
            < 1e-12
    );
}

#[test]
fn blocked_water_transfer_also_blocks_momentum() {
    // Cross-coarse-cell full cells are different rows.
    let phase = fixture([6, 3, 3], |_, _, _| false, |_, _, _| 1.0, 100);
    let g = graph(&phase);
    let face = phase
        .faces()
        .iter()
        .position(|f| f.lower == 2 && f.upper == 3)
        .unwrap();
    let mut flux = vec![0.0; phase.faces().len()];
    flux[face] = 0.01;
    let velocity = vec![[123.0, -456.0, 789.0]; flux.len()];
    let (candidate, metrics, ledger) = g
        .transport_candidate(&phase, 0.01, &flux, 0.45, 1000.0, &velocity)
        .unwrap();
    assert!(metrics.limiter_passes > 0);
    assert_eq!(metrics.moved_water_m3, 0.0);
    assert_eq!(candidate.fractions(), phase.fractions());
    assert!(ledger.water_delta_m3.iter().all(|v| *v == 0.0));
    assert!(
        ledger
            .momentum_delta_kg_m_s
            .iter()
            .flatten()
            .all(|v| *v == 0.0)
    );
}

#[test]
fn accepted_transport_and_three_axis_momentum_share_signed_transfer() {
    let phase = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, _| if x == 1 && y == 0 { 0.8 } else { 0.0 },
        100,
    );
    let g = graph(&phase);
    let face = phase
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    let mut flux = vec![0.0; phase.faces().len()];
    flux[face] = -0.01;
    let velocity = vec![[2.0, -3.0, 5.0]; flux.len()];
    let (candidate, metrics, ledger) = g
        .transport_candidate(&phase, 0.01, &flux, 0.45, 1000.0, &velocity)
        .unwrap();
    let mut ordinary = phase.clone();
    let ordinary_metrics = ordinary.transport(0.01, &flux, 0.45).unwrap();
    assert_eq!(
        candidate
            .fractions()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        ordinary
            .fractions()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(metrics.moved_water_m3, ordinary_metrics.moved_water_m3);
    assert_eq!(phase.fractions()[0], 0.0);
    assert_eq!(ledger.water_delta_m3.iter().sum::<f64>(), 0.0);
    let receiver = g.row_at_index(0).unwrap() as usize;
    assert!((ledger.water_delta_m3[receiver] - metrics.moved_water_m3).abs() < 1e-15);
    for (axis, v) in velocity[face].iter().enumerate() {
        assert_eq!(
            ledger
                .momentum_delta_kg_m_s
                .iter()
                .map(|p| p[axis])
                .sum::<f64>(),
            0.0
        );
        assert!(
            (ledger.momentum_delta_kg_m_s[receiver][axis] - metrics.moved_water_m3 * 1000.0 * v)
                .abs()
                < 1e-12
        );
    }
    assert_eq!(g.rows()[receiver].class, PhaseClass::Dry);
    assert_eq!(
        graph(&candidate).rows()[graph(&candidate).row_at_index(0).unwrap() as usize].class,
        PhaseClass::Wet
    );
}

#[test]
fn limits_stale_snapshots_and_late_ledger_failure_are_atomic() {
    let phase = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, _| if x == 1 && y == 0 { 0.8 } else { 0.0 },
        100,
    );
    for limits in [
        GraphLimits {
            max_fine_cells: 26,
            max_rows: 100,
            max_connections: 100,
        },
        GraphLimits {
            max_fine_cells: 100,
            max_rows: 1,
            max_connections: 100,
        },
        GraphLimits {
            max_fine_cells: 100,
            max_rows: 100,
            max_connections: 0,
        },
    ] {
        assert!(matches!(
            PhaseGraph::build(&phase, limits),
            Err(GraphError::Limit { .. })
        ));
    }
    let g = graph(&phase);
    let weights = vec![1.0; phase.faces().len()];
    let mut top = vec![0.0; 27];
    assert!(g.pressure_operator(&phase.clone(), &weights, &top).is_ok());
    let other = fixture(
        [3; 3],
        |_, _, _| false,
        |x, y, _| if x == 0 && y == 0 { 0.8 } else { 0.0 },
        100,
    );
    assert_eq!(phase.component_amounts_m3(), other.component_amounts_m3());
    assert!(matches!(
        g.pressure_operator(&other, &weights, &top),
        Err(GraphError::StaleSnapshot)
    ));
    top[0] = 1.0;
    assert!(matches!(
        g.pressure_operator(&phase, &weights, &top),
        Err(GraphError::InvalidState)
    ));
    let face = phase
        .faces()
        .iter()
        .position(|f| f.lower == 0 && f.upper == 1)
        .unwrap();
    let mut flux = vec![0.0; weights.len()];
    flux[face] = -0.01;
    let velocity = vec![[f64::MAX; 3]; weights.len()];
    let before = phase.fractions().to_vec();
    assert!(matches!(
        g.transport_candidate(&phase, 0.01, &flux, 0.45, f64::MAX, &velocity),
        Err(GraphError::InvalidState)
    ));
    assert_eq!(phase.fractions(), before);
    assert!(
        g.pressure_operator(&phase, &weights, &vec![0.0; 27])
            .is_ok()
    );
    assert!(matches!(
        g.transfer_ledger(&phase, &[f64::NAN], &[], 1000.0),
        Err(GraphError::InvalidState)
    ));
}
