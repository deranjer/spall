use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::component_fluid::{ComponentConfig, ComponentError, ComponentFluid};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};
use std::sync::Arc;

fn fixture(
    dims: [u32; 3],
    factor: u32,
    solid: impl Fn(GlobalCell) -> bool,
    fraction: impl Fn(GlobalCell) -> f64,
    config: ComponentConfig,
) -> (Arc<CutCellGeometry>, ComponentFluid) {
    let id = VolumeId::new(7).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..dims[2].div_ceil(32) {
        for by in 0..dims[1].div_ceil(32) {
            for bx in 0..dims[0].div_ceil(32) {
                volume
                    .insert_brick(
                        BrickCoord::new(i64::from(bx), i64::from(by), i64::from(bz)),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut plan = EditPlan::new(id);
    let mut amounts = Vec::new();
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let cell = GlobalCell::new(i64::from(x), i64::from(y), i64::from(z));
                let occupied = solid(cell);
                if occupied {
                    plan.set(cell, MaterialId(1));
                }
                amounts.push(if occupied { 0.0 } else { fraction(cell) });
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 30_000).unwrap();
    let fine = SolidBoundary::capture(&volume, spec).unwrap();
    let g = Arc::new(
        CutCellGeometry::build(
            &fine,
            factor,
            GeometryLimits {
                max_fine_cells: 30_000,
                max_components: 30_000,
                max_portals: 90_000,
            },
        )
        .unwrap(),
    );
    let fluid = ComponentFluid::new(g.clone(), &amounts, config).unwrap();
    (g, fluid)
}

#[test]
fn closed_two_component_projection_matches_analytic_pressure_drop() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            gravity: [0.0; 3],
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&[0.001]).unwrap();
    let metrics = f.project(0.1, false).unwrap();
    assert!((f.pressure_pa()[1] - f.pressure_pa()[0] - 40.0).abs() < 1e-9);
    assert!(f.flux_m3_s()[0].abs() < 1e-14);
    assert!(metrics.max_divergence_per_s < 1e-10);
}

#[test]
fn pressure_and_transport_do_not_exchange_across_an_internal_thin_wall() {
    let (g, mut f) = fixture(
        [6, 3, 3],
        3,
        |c| c.x == 1,
        |c| if c.x == 0 { 1.0 } else { 0.0 },
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    let left = g.component_at(GlobalCell::new(0, 0, 0)).unwrap() as usize;
    let before = f.amounts_m3().to_vec();
    for _ in 0..20 {
        f.advance_operators(0.05).unwrap();
    }
    assert_eq!(f.amounts_m3()[left], before[left]);
    assert_eq!(f.water_volume_m3(), before.iter().sum::<f64>());
    assert!(
        f.amounts_m3()
            .iter()
            .enumerate()
            .all(|(i, v)| i == left || *v == 0.0)
    );
}

#[test]
fn bounded_donor_transport_is_pairwise_conservative_and_has_a_known_reference() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |c| if c.x == 0 { 1.0 } else { 0.0 },
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    let initial = f.water_volume_m3();
    f.set_predicted_flux(&[0.01]).unwrap();
    f.transport(0.1).unwrap();
    assert!((f.amounts_m3()[0] - (initial - 0.001)).abs() < 1e-14);
    assert!((f.amounts_m3()[1] - 0.001).abs() < 1e-14);
    assert!((f.water_volume_m3() - initial).abs() < 1e-14);
}

#[test]
fn failed_transport_or_pressure_is_atomic() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |c| if c.x == 0 { 1.0 } else { 0.0 },
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&[10.0]).unwrap();
    let before = f.amounts_m3().to_vec();
    assert!(matches!(f.transport(1.0), Err(ComponentError::Cfl { .. })));
    assert_eq!(f.amounts_m3(), before);
    let flux = f.flux_m3_s().to_vec();
    assert!(f.project(f64::NAN, true).is_err());
    assert_eq!(f.flux_m3_s(), flux);
    assert!(f.set_predicted_flux(&[f64::INFINITY]).is_err());
    assert_eq!(f.flux_m3_s(), flux);
    let (_, mut f) = fixture(
        [6, 3, 3],
        1,
        |_| false,
        |_| 0.5,
        ComponentConfig {
            max_iterations: 1,
            ..ComponentConfig::default()
        },
    );
    let before = f.amounts_m3().to_vec();
    let before_flux = f.flux_m3_s().to_vec();
    assert!(matches!(
        f.advance_operators(0.05),
        Err(ComponentError::PressureNotConverged { .. })
    ));
    assert_eq!(f.amounts_m3(), before);
    assert_eq!(f.flux_m3_s(), before_flux);
}

#[test]
fn hydrostatic_unaligned_submerged_stairs_cancel_gravity() {
    let (_, mut f) = fixture(
        [48, 24, 24],
        3,
        |c| c.y < 3 + c.x / 5,
        |c| if c.y < 15 { 1.0 } else { 0.0 },
        ComponentConfig {
            absolute_tolerance: 1e-14,
            relative_tolerance: 1e-12,
            ..ComponentConfig::default()
        },
    );
    let initial = f.water_volume_m3();
    let metrics = f.project(0.05, true).unwrap();
    assert!(metrics.max_connection_speed_m_s < 1e-7, "{metrics:?}");
    assert!(metrics.max_divergence_per_s < 1e-8, "{metrics:?}");
    assert_eq!(f.water_volume_m3(), initial);
}

#[test]
fn partial_flat_surface_is_hydrostatic_without_treating_solid_capacity_as_air() {
    let (_, mut f) = fixture(
        [6, 18, 6],
        3,
        |c| c.y < 3,
        |c| if c.y < 7 { 1.0 } else { 0.0 },
        ComponentConfig {
            absolute_tolerance: 1e-14,
            relative_tolerance: 1e-12,
            ..ComponentConfig::default()
        },
    );
    let metrics = f.project(0.05, true).unwrap();
    assert!(metrics.max_connection_speed_m_s < 1e-7, "{metrics:?}");
}

#[test]
fn flux_into_full_capacity_is_limited_without_losing_mass() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&[0.001]).unwrap();
    let before = f.amounts_m3().to_vec();
    let before_flux = f.flux_m3_s().to_vec();
    let metrics = f.transport(0.05).unwrap();
    assert_eq!(metrics.limited_water_m3, 0.00005);
    assert!(metrics.limiter_passes > 0);
    assert_eq!(f.amounts_m3(), before);
    assert_eq!(f.flux_m3_s(), before_flux);
    assert_eq!(f.outflow_m3(), 0.0);
}

#[test]
fn saturated_chain_passes_incoming_water_using_simultaneous_outgoing_capacity() {
    let (_, mut f) = fixture(
        [3, 1, 1],
        1,
        |_| false,
        |c| if c.x < 2 { 1.0 } else { 0.0 },
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    let before = f.amounts_m3().to_vec();
    f.set_predicted_flux(&[0.006, 0.006]).unwrap();
    let metrics = f.transport(0.1).unwrap();
    assert_eq!(metrics.limiter_passes, 0);
    assert_eq!(f.amounts_m3()[1], before[1]);
    assert!((f.amounts_m3()[0] - (before[0] - 0.0006)).abs() < 1e-16);
    assert!((f.amounts_m3()[2] - 0.0006).abs() < 1e-16);
    assert!((f.water_volume_m3() - before.iter().sum::<f64>()).abs() < 1e-16);
}

#[test]
fn full_cycle_transports_without_clipping_or_artificial_blocking() {
    let (g, mut f) = fixture(
        [2, 1, 2],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    let flux: Vec<_> = g
        .portals()
        .iter()
        .map(|e| match (e.lower_component, e.upper_component) {
            (0, 1) | (1, 3) => 0.001,
            (0, 2) | (2, 3) => -0.001,
            other => panic!("unexpected square edge {other:?}"),
        })
        .collect();
    f.set_predicted_flux(&flux).unwrap();
    let p = f.project(0.1, true).unwrap();
    assert_eq!(f.flux_m3_s(), flux);
    assert_eq!(p.iterations, 0);
    let before = f.amounts_m3().to_vec();
    let m = f.transport(0.1).unwrap();
    assert_eq!(m.limiter_passes, 0);
    assert_eq!(m.limited_water_m3, 0.0);
    assert!((m.moved_water_m3 - 0.0004).abs() < 1e-16);
    assert_eq!(f.amounts_m3(), before);
}

#[test]
fn near_full_receiver_limits_paired_transfer_conservatively() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |c| if c.x == 0 { 1.0 } else { 0.9 },
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    let before = f.water_volume_m3();
    f.set_predicted_flux(&[0.02]).unwrap();
    let m = f.transport(0.1).unwrap();
    assert!(m.limiter_passes > 0 && m.limited_water_m3 > 0.0);
    assert!(f.amounts_m3().iter().all(|v| *v >= 0.0 && *v <= 0.015625));
    assert!((f.water_volume_m3() - before).abs() < 1e-16);
    assert!((m.moved_water_m3 - 0.0015625).abs() < 1e-15);
}

#[test]
fn hydrostatic_split_still_drives_level_difference_flow() {
    let (g, mut f) = fixture(
        [12, 12, 6],
        3,
        |c| c.y < 2,
        |c| {
            if c.y < if c.x < 6 { 8 } else { 4 } {
                1.0
            } else {
                0.0
            }
        },
        ComponentConfig::default(),
    );
    let left = |f: &ComponentFluid| {
        g.components()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.anchor.x < 6)
            .map(|(i, _)| f.amounts_m3()[i])
            .sum::<f64>()
    };
    let before_left = left(&f);
    let before_total = f.water_volume_m3();
    let mut peak = 0.0_f64;
    for _ in 0..600 {
        peak = peak.max(f.advance_operators(0.05).unwrap().max_connection_speed_m_s);
    }
    assert!(peak > 0.01, "perturbation did not drive flow: {peak}");
    assert!(
        left(&f) < before_left - 0.01,
        "left pool did not drain toward lower pool"
    );
    assert!((f.water_volume_m3() + f.outflow_m3() - before_total).abs() < 1e-12);
}

#[test]
fn reference_pressure_matches_two_phase_hydrostatic_column() {
    let (_, mut f) = fixture(
        [1, 2, 1],
        1,
        |_| false,
        |c| if c.y == 0 { 1.0 } else { 0.0 },
        ComponentConfig::default(),
    );
    let m = f.project(0.05, true).unwrap();
    assert_eq!(m.iterations, 0);
    assert_eq!(m.max_connection_speed_m_s, 0.0);
    assert!((f.pressure_pa()[0] - (1000.0 * 9.81 * 0.125 + 1.2 * 9.81 * 0.25)).abs() < 1e-10);
    assert!((f.pressure_pa()[1] - 1.2 * 9.81 * 0.125).abs() < 1e-10);
}

#[test]
fn partial_surface_across_irregular_submerged_geometry_stays_at_rest_for_one_minute() {
    for surface_fraction in [0.5, 0.3] {
        let (_, mut f) = fixture(
            [12, 12, 6],
            3,
            |c| c.y < 2 + c.x % 5 || ((c.x == 4 || c.x == 8) && c.y < 8),
            |c| {
                if c.y < 7 {
                    1.0
                } else if c.y == 7 {
                    surface_fraction
                } else {
                    0.0
                }
            },
            ComponentConfig::default(),
        );
        let initial = f.amounts_m3().to_vec();
        for _ in 0..1200 {
            let m = f.advance_operators(0.05).unwrap();
            assert!(m.max_connection_speed_m_s < 1e-10, "{m:?}");
        }
        assert!(
            f.amounts_m3()
                .iter()
                .zip(initial)
                .all(|(a, b)| (*a - b).abs() < 1e-12)
        );
        assert_eq!(f.outflow_m3(), 0.0);
    }
}

#[test]
fn limiter_pass_exhaustion_is_atomic() {
    let (_, mut f) = fixture(
        [100, 1, 1],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&[0.001; 99]).unwrap();
    let before = f.amounts_m3().to_vec();
    let before_flux = f.flux_m3_s().to_vec();
    assert!(matches!(
        f.transport(0.05),
        Err(ComponentError::Capacity { .. })
    ));
    assert_eq!(f.amounts_m3(), before);
    assert_eq!(f.flux_m3_s(), before_flux);
    assert_eq!(f.outflow_m3(), 0.0);
}

fn square_flux(g: &CutCellGeometry, q: f64) -> Vec<f64> {
    g.portals()
        .iter()
        .map(|e| match (e.lower_component, e.upper_component) {
            (0, 1) | (1, 3) => q,
            (0, 2) | (2, 3) => -q,
            other => panic!("unexpected square edge {other:?}"),
        })
        .collect()
}

#[test]
fn staggered_upwind_advection_matches_analytic_square_cycle_and_dissipates_energy() {
    let (g, mut f) = fixture(
        [2, 1, 2],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            gravity: [0.0; 3],
            ..ComponentConfig::default()
        },
    );
    let flux = square_flux(&g, 0.001);
    f.set_predicted_flux(&flux).unwrap();
    let initial = f.water_volume_m3();
    let before_energy = f.kinetic_energy_j();
    let (_, m) = f.transport_and_advect(0.1).unwrap();
    let expected_factor = 1.0 - 0.001 * 0.1 / 0.015625;
    for (&q, &old) in f.flux_m3_s().iter().zip(&flux) {
        assert!((q - old * expected_factor).abs() < 1e-15);
    }
    assert!((m.kinetic_before_j - before_energy).abs() < 1e-15);
    assert!((m.kinetic_after_j - before_energy * expected_factor.powi(2)).abs() < 1e-15);
    assert!(m.kinetic_after_j < m.kinetic_before_j);
    assert!(m.balance_error_kg_m_s.iter().all(|v| v.abs() < 1e-14));
    assert!(m.max_dual_mass_error_kg < 1e-14);
    assert_eq!(f.water_volume_m3(), initial);
}

#[test]
fn variable_density_advection_accounts_for_wall_and_boundary_momentum() {
    let (_, mut f) = fixture(
        [2, 1, 1],
        1,
        |_| false,
        |c| if c.x == 0 { 1.0 } else { 0.0 },
        ComponentConfig {
            gravity: [0.0; 3],
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_fluxes(&[-0.001], &[0.001, -0.001]).unwrap();
    let before = f.water_volume_m3();
    let before_momentum = f.momentum_kg_m_s();
    let (_, m) = f.transport_and_advect(0.1).unwrap();
    assert!(m.wall_impulse_kg_m_s.iter().any(|v| v.abs() > 1e-8));
    assert!(m.boundary_outflow_kg_m_s.iter().any(|v| v.abs() > 1e-8));
    assert!(
        m.balance_error_kg_m_s.iter().all(|v| v.abs() < 1e-12),
        "{m:?}"
    );
    assert!(m.max_dual_mass_error_kg < 1e-12);
    assert_eq!(m.before_kg_m_s, before_momentum);
    for (after, reported) in f.momentum_kg_m_s().into_iter().zip(m.after_kg_m_s) {
        assert!((after - reported).abs() < 1e-12);
    }
    assert!((f.water_volume_m3() + f.outflow_m3() - before).abs() < 1e-16);
    let flux = f.flux_m3_s().to_vec();
    let top = f.top_flux_m3_s().to_vec();
    assert!(f.set_predicted_fluxes(&[0.0], &[f64::NAN, 0.0]).is_err());
    assert_eq!(f.flux_m3_s(), flux);
    assert_eq!(f.top_flux_m3_s(), top);
}

#[test]
fn failed_coupled_momentum_step_preserves_all_state() {
    let (g, mut f) = fixture(
        [2, 1, 2],
        1,
        |_| false,
        |_| 1.0,
        ComponentConfig {
            open_top: false,
            gravity: [0.0; 3],
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&square_flux(&g, 10.0)).unwrap();
    let amounts = f.amounts_m3().to_vec();
    let pressure = f.pressure_pa().to_vec();
    let flux = f.flux_m3_s().to_vec();
    assert!(matches!(
        f.advance_momentum_operators(0.1),
        Err(ComponentError::Cfl { .. })
    ));
    assert_eq!(f.amounts_m3(), amounts);
    assert_eq!(f.pressure_pa(), pressure);
    assert_eq!(f.flux_m3_s(), flux);
    assert_eq!(f.outflow_m3(), 0.0);
}

#[test]
fn coupled_momentum_keeps_decimal_surface_at_rest() {
    let (_, mut f) = fixture(
        [12, 12, 6],
        3,
        |c| c.y < 2 + c.x % 5,
        |c| {
            if c.y < 7 {
                1.0
            } else if c.y == 7 {
                0.3
            } else {
                0.0
            }
        },
        ComponentConfig::default(),
    );
    let before = f.water_volume_m3();
    for _ in 0..1200 {
        let m = f.advance_momentum_operators(0.05).unwrap();
        assert!(m.final_projection.max_connection_speed_m_s < 1e-10);
        assert!(
            m.advection
                .balance_error_kg_m_s
                .iter()
                .all(|v| v.abs() < 1e-12)
        );
    }
    assert!((f.water_volume_m3() + f.outflow_m3() - before).abs() < 1e-12);
}

#[test]
fn wall_surface_counts_match_fine_faces_including_internal_thin_walls() {
    let (g, _) = fixture([6, 3, 3], 3, |_| false, |_| 0.0, ComponentConfig::default());
    assert_eq!(g.wall_faces(), vec![[9, 18, 18], [9, 18, 18]]);
    let (g, _) = fixture(
        [6, 3, 3],
        3,
        |c| c.x == 1,
        |_| 0.0,
        ComponentConfig::default(),
    );
    assert_eq!(g.wall_faces(), vec![[18, 6, 6], [9, 6, 6], [9, 18, 18]]);
}

#[test]
fn released_reservoir_carries_downstream_momentum_into_a_narrow_channel() {
    let (g, mut f) = fixture(
        [48, 18, 12],
        3,
        |c| c.y < 3 || (c.x >= 18 && !(3..9).contains(&c.z) && c.y < 12),
        |c| if c.x < 18 && c.y < 9 { 1.0 } else { 0.0 },
        ComponentConfig::default(),
    );
    let initial = f.water_volume_m3();
    let initial_energy = f.vertical_potential_energy_j() + f.kinetic_energy_j();
    let channel = |f: &ComponentFluid| {
        g.components()
            .iter()
            .enumerate()
            .filter(|(_, c)| c.anchor.x >= 18 && (3..9).contains(&c.anchor.z))
            .map(|(i, _)| f.amounts_m3()[i])
            .sum::<f64>()
    };
    let mut peak_momentum = 0.0_f64;
    let mut peak_speed = 0.0_f64;
    let mut max_balance = 0.0_f64;
    let mut max_energy = initial_energy;
    for _ in 0..600 {
        let m = f.advance_momentum_operators(0.01).unwrap();
        peak_momentum = peak_momentum.max(f.momentum_kg_m_s()[0]);
        peak_speed = peak_speed.max(m.final_projection.max_connection_speed_m_s);
        max_balance = max_balance.max(
            m.advection
                .balance_error_kg_m_s
                .into_iter()
                .map(f64::abs)
                .fold(0.0, f64::max),
        );
        max_energy = max_energy.max(f.vertical_potential_energy_j() + f.kinetic_energy_j());
    }
    eprintln!(
        "released channel: water={} initial={initial} peak_momentum={peak_momentum} peak_speed={peak_speed} balance={max_balance} initial_energy={initial_energy} max_energy={max_energy}",
        channel(&f)
    );
    assert!(
        channel(&f) > 0.5,
        "channel received too little water: {}",
        channel(&f)
    );
    assert!(
        peak_momentum > 0.1,
        "no downstream surge momentum: {peak_momentum}"
    );
    assert!(max_balance < 1e-9);
    assert!(
        max_energy <= initial_energy * 1.05,
        "energy creation exceeds diagnostic budget: {max_energy} / {initial_energy}"
    );
    assert!((f.water_volume_m3() + f.outflow_m3() - initial).abs() < 1e-10);
}

#[test]
fn pressure_projection_cannot_increase_staggered_energy_in_irregular_closed_geometry() {
    let (g, mut f) = fixture(
        [12, 9, 6],
        3,
        |c| c.y < c.x % 4 || (c.x == 4 && c.z < 3),
        |c| if c.x < 5 { 0.9 } else { 0.1 },
        ComponentConfig {
            open_top: false,
            gravity: [0.0; 3],
            ..ComponentConfig::default()
        },
    );
    let q: Vec<_> = g
        .portals()
        .iter()
        .enumerate()
        .map(|(i, _)| ((i * 17 % 31) as f64 - 15.0) * 0.0001)
        .collect();
    f.set_predicted_flux(&q).unwrap();
    let before = f.kinetic_energy_j();
    let m = f.project(0.05, false).unwrap();
    assert!(m.max_divergence_per_s < 1e-6);
    assert!(f.kinetic_energy_j() <= before * (1.0 + 1e-12));
}

#[test]
fn failed_final_projection_discards_completed_mass_and_momentum_advection() {
    let (g, mut f) = fixture(
        [2, 1, 2],
        1,
        |_| false,
        |c| match (c.x, c.z) {
            (0, 0) => 0.9,
            (1, 0) => 0.2,
            (0, 1) => 0.4,
            _ => 0.0,
        },
        ComponentConfig {
            open_top: false,
            gravity: [0.0; 3],
            max_iterations: 1,
            ..ComponentConfig::default()
        },
    );
    f.set_predicted_flux(&square_flux(&g, 0.001)).unwrap();
    let mut stage = f.clone();
    assert_eq!(stage.project(0.05, true).unwrap().iterations, 0);
    stage.transport_and_advect(0.05).unwrap();
    assert!(matches!(
        stage.project(0.05, false),
        Err(ComponentError::PressureNotConverged { .. })
    ));
    let amounts = f.amounts_m3().to_vec();
    let pressure = f.pressure_pa().to_vec();
    let flux = f.flux_m3_s().to_vec();
    assert!(matches!(
        f.advance_momentum_operators(0.05),
        Err(ComponentError::PressureNotConverged { .. })
    ));
    assert_eq!(f.amounts_m3(), amounts);
    assert_eq!(f.pressure_pa(), pressure);
    assert_eq!(f.flux_m3_s(), flux);
    assert_eq!(f.outflow_m3(), 0.0);
}
