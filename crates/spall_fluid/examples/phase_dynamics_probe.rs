//! Same static geometry/time as component_dynamics_probe, fine MAC reference.
//! Motion gates only: not conservative momentum accuracy or generated trench.
use spall_fluid::grid_mac::{MacConfig, PressurePreconditioner};
use spall_fluid::phase_fixtures::PhaseReferenceScene;
use spall_fluid::phase_pressure::{PhasePressureConfig, PhasePressureWorld};
use std::time::Instant;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "low-dam".into());
    if !["channel", "low-dam", "full-wall"].contains(&mode.as_str()) {
        return Err("mode must be channel, low-dam or full-wall".into());
    }
    let steps = std::env::args()
        .nth(2)
        .map_or(Ok(600), |s| s.parse::<u32>())?;
    if steps == 0 || steps > 6000 {
        return Err("steps must be 1..=6000".into());
    }
    let predictor = match std::env::args().nth(3).as_deref() {
        None | Some("baseline") | Some("water-only") | Some("water-support") => false,
        Some("predictor") => true,
        Some("balanced") => false,
        _ => {
            return Err(
                "optional mode must be baseline, predictor, balanced, water-only or water-support"
                    .into(),
            );
        }
    };
    let balanced = std::env::args().nth(3).as_deref() == Some("balanced");
    let cut_support = std::env::args().nth(3).as_deref() == Some("water-support");
    let water_only = cut_support || std::env::args().nth(3).as_deref() == Some("water-only");
    let momentum = match std::env::args().nth(4).as_deref() {
        None => false,
        Some("momentum") if !water_only => true,
        _ => return Err("optional fourth argument must be momentum".into()),
    };
    let compressible_air = match std::env::args().nth(5).as_deref() {
        None => !water_only,
        Some("incompressible-air") if !water_only => false,
        _ => return Err("optional fifth argument must be incompressible-air".into()),
    };
    let scene = match mode.as_str() {
        "channel" => PhaseReferenceScene::Channel,
        "low-dam" => PhaseReferenceScene::LowDam,
        _ => PhaseReferenceScene::FullWall,
    };
    let dims = scene.dimensions();
    let phase = scene.build()?;
    let config = PhasePressureConfig {
        mac: MacConfig {
            pressure_max_iterations: 1000,
            ..MacConfig::default()
        },
        air_density_kg_m3: 1.2,
        preconditioner: PressurePreconditioner::Multigrid,
        max_retained_array_bytes: 100_000_000,
    };
    let mut world = if water_only {
        PhasePressureWorld::new_water_only(phase, config)
    } else {
        PhasePressureWorld::new(phase, config)
    }?;
    if cut_support {
        world.enable_cut_surface_support()?;
    }
    if predictor {
        world.enable_pressure_predictor(
            spall_fluid::phase_graph::GraphLimits {
                max_fine_cells: 30_000,
                max_rows: 30_000,
                max_connections: 90_000,
            },
            8,
        )?;
    }
    if balanced {
        world.enable_pressure_preconditioner(
            spall_fluid::phase_graph::GraphLimits {
                max_fine_cells: 30_000,
                max_rows: 30_000,
                max_connections: 90_000,
            },
            8,
        )?;
    }
    let initial = world.phase().water_volume_m3();
    println!(
        "{{\"scenario\":\"surface_support_reference_configuration\",\"cut_surface_support\":{cut_support},\"accepted_water_momentum_transport\":{cut_support},\"water_only\":{water_only},\"fixture\":{mode:?},\"steps\":{steps},\"dt_s\":0.01}}"
    );
    let initial_energy =
        world.solver().gravitational_potential_energy_j() + world.solver().kinetic_energy_j();
    let mut peak_energy = initial_energy;
    let mut peak_momentum = 0.0_f64;
    let mut peak_liquid_speed = 0.0_f64;
    let mut peak_all_speed = 0.0_f64;
    let mut max_divergence = 0.0_f64;
    let mut accepted = 0;
    let mut substeps = 0;
    if momentum {
        world.enable_conservative_momentum()?;
    }
    world.set_compressible_enclosed_air(compressible_air)?;
    let mut momentum_error = [0.0_f64; 3];
    let mut momentum_wall = [0.0_f64; 3];
    let mut momentum_exterior = [0.0_f64; 3];
    let mut momentum_scratch = 0;
    let mut momentum_subcycles = 0;
    let mut mass_defect = 0.0_f64;
    let mut pressure_rows = 0;
    let mut pressure_iterations = 0;
    let mut predictor_rows = 0;
    let mut predictor_reuses = 0;
    let mut predictor_rebuilds = 0;
    let mut predictor_micros = 0;
    let mut balanced_applications = 0;
    let mut balanced_micros = 0;
    let mut balanced_scratch = 0;
    let mut path_repairs = 0;
    let mut path_scratch_bytes = 0;
    let mut times = Vec::new();
    let mut failure = None;
    for _ in 0..steps {
        let start = Instant::now();
        let metrics = match world.step(0.01) {
            Ok(m) => m,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        times.push(start.elapsed().as_micros());
        accepted += 1;
        substeps += metrics.substeps;
        for axis in 0..3 {
            momentum_error[axis] += metrics.momentum_transport_error_kg_m_s[axis];
            momentum_wall[axis] += metrics.momentum_wall_impulse_kg_m_s[axis];
            momentum_exterior[axis] += metrics.momentum_open_outflow_kg_m_s[axis];
        }
        momentum_scratch = momentum_scratch.max(metrics.momentum_scratch_bytes);
        momentum_subcycles = momentum_subcycles.max(metrics.momentum_transport_subcycles);
        mass_defect = mass_defect.max(metrics.momentum_dual_mass_defect_kg);
        pressure_rows += metrics.pressure_active_rows_total;
        pressure_iterations += metrics.pressure_iterations;
        predictor_rows += metrics.phase_predictor_rows_total;
        predictor_reuses += metrics.phase_predictor_reuses;
        predictor_rebuilds += metrics.phase_predictor_rebuilds;
        predictor_micros += metrics.phase_predictor_micros;
        balanced_applications += metrics.phase_preconditioner_applications;
        balanced_micros += metrics.phase_preconditioner_micros;
        balanced_scratch = balanced_scratch.max(metrics.phase_preconditioner_scratch_bytes);
        path_repairs += metrics.strict_path_repair_count;
        path_scratch_bytes = path_scratch_bytes.max(metrics.strict_path_scratch_bytes);
        max_divergence = max_divergence.max(metrics.divergence_after_max_s);
        peak_momentum = peak_momentum.max(world.solver().liquid_momentum_kg_m_s()[0]);
        peak_liquid_speed = peak_liquid_speed.max(world.solver().max_liquid_speed_m_s());
        peak_all_speed = peak_all_speed.max(world.solver().max_face_component_velocity_m_s());
        peak_energy = peak_energy.max(
            world.solver().gravitational_potential_energy_j() + world.solver().kinetic_energy_j(),
        );
    }
    let [nx, ny, _] = dims.map(|n| n as usize);
    let downstream: f64 = world
        .phase()
        .fractions()
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let x = i % nx;
            let z = i / (nx * ny);
            if mode == "channel" {
                x >= 18 && (3..9).contains(&z)
            } else {
                x >= 5
            }
        })
        .map(|(_, f)| f * 0.25_f64.powi(3))
        .sum();
    times.sort_unstable();
    let median = times
        .get(times.len() / 2)
        .map_or_else(|| "null".into(), ToString::to_string);
    let p99 = times
        .get(times.len().saturating_sub(1) * 99 / 100)
        .map_or_else(|| "null".into(), ToString::to_string);
    let maximum = times
        .last()
        .map_or_else(|| "null".into(), ToString::to_string);
    let accounting =
        world.phase().water_volume_m3() + world.solver().cumulative_open_outflow_m3() - initial;
    let gate = accepted == steps
        && accounting.abs() < 1e-10
        && peak_energy <= initial_energy * 1.05
        && if mode == "channel" {
            downstream > 0.5 && peak_momentum > 0.1
        } else {
            downstream < 1e-9
        };
    let failure_json = failure
        .as_ref()
        .map_or_else(|| "null".into(), |e| format!("{e:?}"));
    println!(
        "{{\"water_only\":{water_only},\"fixture\":{mode:?},\"backend\":\"fine_mac_phase_reference\",\"fine_dimensions\":{dims:?},\"phase_geometry_factor\":3,\"solver_cell_size_m\":0.25,\"requested_steps\":{steps},\"accepted_steps\":{accepted},\"accepted_substeps\":{substeps},\"dt_s\":0.01,\"advanced_time_s\":{},\"initial_water_m3\":{initial},\"downstream_water_m3\":{downstream},\"peak_eastward_liquid_momentum_kg_m_s\":{peak_momentum},\"peak_liquid_speed_m_s\":{peak_liquid_speed},\"peak_all_face_speed_m_s\":{peak_all_speed},\"peak_divergence_per_s\":{max_divergence},\"pressure_rows_total\":{pressure_rows},\"pressure_iterations_total\":{pressure_iterations},\"phase_pressure_predictor\":{predictor},\"phase_balanced_preconditioner\":{balanced},\"balanced_applications\":{balanced_applications},\"balanced_scratch_peak_bytes\":{balanced_scratch},\"predictor_rows_total\":{predictor_rows},\"predictor_reuses\":{predictor_reuses},\"predictor_rebuilds\":{predictor_rebuilds},\"predictor_total_us\":{predictor_micros},\"balanced_application_total_us\":{balanced_micros},\"initial_liquid_energy_j\":{initial_energy},\"peak_liquid_energy_j\":{peak_energy},\"accounting_error_m3\":{accounting},\"retained_phase_solver_array_bytes\":{},\"coupled_iteration_median_us\":{median},\"coupled_iteration_p99_us\":{p99},\"coupled_iteration_max_us\":{maximum},\"strict_path_repairs\":{path_repairs},\"strict_path_scratch_peak_bytes\":{path_scratch_bytes},\"failure\":{failure_json},\"motion_gate_pass\":{gate},\"compressible_enclosed_air\":{compressible_air},\"conservative_momentum_enabled\":{momentum},\"momentum_transport_error_kg_m_s\":{momentum_error:?},\"momentum_wall_impulse_kg_m_s\":{momentum_wall:?},\"momentum_open_outflow_kg_m_s\":{momentum_exterior:?},\"momentum_scratch_peak_bytes\":{momentum_scratch},\"momentum_subcycles_max\":{momentum_subcycles},\"momentum_dual_mass_defect_peak_kg\":{mass_defect},\"conservative_momentum_gate_accepted\":false,\"production_steps\":0}}",
        f64::from(accepted) * 0.01,
        world.retained_array_bytes()
    );
    if !gate {
        return Err("fine reference motion gate failed; see metrics".into());
    }
    Ok(())
}
