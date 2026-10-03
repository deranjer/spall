//! Same static geometry/time as component_dynamics_probe, fine MAC reference.
//! Motion gates only: not conservative momentum accuracy or generated trench.
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::grid_mac::{MacConfig, PressurePreconditioner};
use spall_fluid::phase_pressure::{PhasePressureConfig, PhasePressureWorld};
use spall_fluid::phase_water::{PhaseLimits, PhaseWater};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};
use std::{sync::Arc, time::Instant};

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
        None | Some("baseline") => false,
        Some("predictor") => true,
        _ => return Err("optional pressure mode must be baseline or predictor".into()),
    };
    let dims: [u32; 3] = if mode == "channel" {
        [48, 18, 12]
    } else {
        [9, 6, 3]
    };
    let id = VolumeId::new(7).unwrap();
    let mut terrain = Volume::new(id, CellSizeCode::Quarter);
    for z in 0..dims[2].div_ceil(32) {
        for y in 0..dims[1].div_ceil(32) {
            for x in 0..dims[0].div_ceil(32) {
                terrain.insert_brick(
                    BrickCoord::new(i64::from(x), i64::from(y), i64::from(z)),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )?;
            }
        }
    }
    let mut edits = EditPlan::new(id);
    let mut seed = Vec::new();
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let solid = if mode == "channel" {
                    y < 3 || (x >= 18 && !(3..9).contains(&z) && y < 12)
                } else {
                    x == 4 && (mode == "full-wall" || y < 5)
                };
                if solid {
                    edits.set(
                        GlobalCell::new(i64::from(x), i64::from(y), i64::from(z)),
                        MaterialId(1),
                    );
                }
                let water = !solid
                    && if mode == "channel" {
                        x < 18 && y < 9
                    } else {
                        x < 4 && y < 2
                    };
                seed.push(if water { 1.0 } else { 0.0 });
            }
        }
    }
    terrain.apply_edit(&edits)?;
    let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 30_000)?;
    let boundary = SolidBoundary::capture(&terrain, spec)?;
    let geometry = Arc::new(CutCellGeometry::build(
        &boundary,
        3,
        GeometryLimits {
            max_fine_cells: 30_000,
            max_components: 30_000,
            max_portals: 90_000,
        },
    )?);
    let phase = PhaseWater::new(
        geometry,
        &seed,
        0.25,
        PhaseLimits {
            max_fine_cells: 30_000,
            max_faces: 90_000,
            max_basins: 30_000,
        },
    )?;
    let mut world = PhasePressureWorld::new(
        phase,
        PhasePressureConfig {
            mac: MacConfig {
                pressure_max_iterations: 1000,
                ..MacConfig::default()
            },
            air_density_kg_m3: 1.2,
            preconditioner: PressurePreconditioner::Multigrid,
            max_retained_array_bytes: 100_000_000,
        },
    )?;
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
    let initial = world.phase().water_volume_m3();
    let initial_energy =
        world.solver().gravitational_potential_energy_j() + world.solver().kinetic_energy_j();
    let mut peak_energy = initial_energy;
    let mut peak_momentum = 0.0_f64;
    let mut peak_liquid_speed = 0.0_f64;
    let mut peak_all_speed = 0.0_f64;
    let mut max_divergence = 0.0_f64;
    let mut accepted = 0;
    let mut substeps = 0;
    let mut pressure_rows = 0;
    let mut pressure_iterations = 0;
    let mut predictor_rows = 0;
    let mut predictor_reuses = 0;
    let mut predictor_rebuilds = 0;
    let mut predictor_micros = 0;
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
        pressure_rows += metrics.pressure_active_rows_total;
        pressure_iterations += metrics.pressure_iterations;
        predictor_rows += metrics.phase_predictor_rows_total;
        predictor_reuses += metrics.phase_predictor_reuses;
        predictor_rebuilds += metrics.phase_predictor_rebuilds;
        predictor_micros += metrics.phase_predictor_micros;
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
        "{{\"fixture\":{mode:?},\"backend\":\"fine_mac_phase_reference\",\"fine_dimensions\":{dims:?},\"phase_geometry_factor\":3,\"solver_cell_size_m\":0.25,\"requested_steps\":{steps},\"accepted_steps\":{accepted},\"accepted_substeps\":{substeps},\"dt_s\":0.01,\"advanced_time_s\":{},\"initial_water_m3\":{initial},\"downstream_water_m3\":{downstream},\"peak_eastward_liquid_momentum_kg_m_s\":{peak_momentum},\"peak_liquid_speed_m_s\":{peak_liquid_speed},\"peak_all_face_speed_m_s\":{peak_all_speed},\"peak_divergence_per_s\":{max_divergence},\"pressure_rows_total\":{pressure_rows},\"pressure_iterations_total\":{pressure_iterations},\"phase_pressure_predictor\":{predictor},\"predictor_rows_total\":{predictor_rows},\"predictor_reuses\":{predictor_reuses},\"predictor_rebuilds\":{predictor_rebuilds},\"predictor_total_us\":{predictor_micros},\"initial_liquid_energy_j\":{initial_energy},\"peak_liquid_energy_j\":{peak_energy},\"accounting_error_m3\":{accounting},\"retained_phase_solver_array_bytes\":{},\"coupled_iteration_median_us\":{median},\"coupled_iteration_p99_us\":{p99},\"coupled_iteration_max_us\":{maximum},\"strict_path_repairs\":{path_repairs},\"strict_path_scratch_peak_bytes\":{path_scratch_bytes},\"failure\":{failure_json},\"motion_gate_pass\":{gate},\"conservative_momentum_gate_accepted\":false,\"production_steps\":0}}",
        f64::from(accepted) * 0.01,
        world.retained_array_bytes()
    );
    if !gate {
        return Err("fine reference motion gate failed; see metrics".into());
    }
    Ok(())
}
