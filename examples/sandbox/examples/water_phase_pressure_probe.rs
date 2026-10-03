//! ENG-122 bounded generated fine-reference startup; not minute/trench acceptance.
use sandbox::worldgen_scene;
use spall_fluid::SolidBoundary;
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::phase_water::{PhaseLimits, PhaseWater};
use std::{sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = std::env::args()
        .nth(1)
        .map_or(Ok(512), |s| s.parse::<u32>())?;
    if ![512, 1024].contains(&size) {
        return Err("size must be 512 or 1024".into());
    }
    let scene = worldgen_scene::generate_with_season(
        "showcase",
        1,
        size,
        spall_ecology::living::Season::Autumn,
    )?;
    let setup = scene.water_setup().ok_or("no water")?;
    let boundary = SolidBoundary::capture(&scene.world().terrain, setup.domain)?;
    let geometry = Arc::new(CutCellGeometry::build(
        &boundary,
        setup.coarsen,
        GeometryLimits {
            max_fine_cells: 4_000_000,
            max_components: 200_000,
            max_portals: 600_000,
        },
    )?);
    let mut fractions = vec![0.0; setup.domain.cell_count()];
    let origin = setup.domain.origin();
    let dims = setup.domain.dimensions();
    for &(cell, fraction) in &setup.initial_fractions {
        let [x, y, z] = [cell.x - origin.x, cell.y - origin.y, cell.z - origin.z];
        if [x, y, z]
            .into_iter()
            .zip(dims)
            .any(|(p, n)| p < 0 || p >= i64::from(n))
        {
            return Err("seed outside domain".into());
        }
        let index = x as usize + dims[0] as usize * (y as usize + dims[1] as usize * z as usize);
        fractions[index] += fraction;
    }
    let phase = PhaseWater::new(
        geometry,
        &fractions,
        0.25,
        PhaseLimits {
            max_fine_cells: 4_000_000,
            max_faces: 12_000_000,
            max_basins: 200_000,
        },
    )?;
    let predictor = match std::env::args().nth(2).as_deref() {
        None | Some("baseline") => false,
        Some("predictor") => true,
        Some("balanced") => false,
        _ => return Err("mode must be baseline, predictor or balanced".into()),
    };
    let balanced = std::env::args().nth(2).as_deref() == Some("balanced");
    let momentum = match std::env::args().nth(3).as_deref() {
        None => false,
        Some("momentum") => true,
        _ => return Err("optional third argument must be momentum".into()),
    };
    let mut world = spall_fluid::phase_pressure::PhasePressureWorld::new(
        phase,
        spall_fluid::phase_pressure::PhasePressureConfig {
            mac: spall_fluid::grid_mac::MacConfig {
                pressure_max_iterations: 1000,
                ..Default::default()
            },
            air_density_kg_m3: 1.2,
            preconditioner: spall_fluid::grid_mac::PressurePreconditioner::Multigrid,
            max_retained_array_bytes: 1_000_000_000,
        },
    )?;
    if predictor {
        world.enable_pressure_predictor(
            spall_fluid::phase_graph::GraphLimits {
                max_fine_cells: 4_000_000,
                max_rows: 200_000,
                max_connections: 600_000,
            },
            8,
        )?;
    }
    if balanced {
        world.enable_pressure_preconditioner(
            spall_fluid::phase_graph::GraphLimits {
                max_fine_cells: 4_000_000,
                max_rows: 200_000,
                max_connections: 600_000,
            },
            8,
        )?;
    }
    let initial = world.phase().water_volume_m3();
    let mut accepted = 0;
    let mut substeps = 0;
    let mut fine_rows = 0;
    let mut fine_iterations = 0;
    let mut coarse_rows = 0;
    let mut reuses = 0;
    let mut rebuilds = 0;
    let mut coarse_us = 0;
    let mut balanced_applications = 0;
    if momentum {
        world.enable_conservative_momentum()?;
    }
    let mut momentum_scratch = 0;
    let mut momentum_subcycles = 0;
    let mut momentum_error = [0.0_f64; 3];
    let mut mass_defect = 0.0_f64;
    let mut balanced_us = 0;
    let mut balanced_scratch = 0;
    let mut times = Vec::new();
    let mut pressure_times = Vec::new();
    let mut max_speed = 0.0_f64;
    let mut failure = None;
    for _ in 0..2 {
        let start = Instant::now();
        let m = match world.step(0.05) {
            Ok(m) => m,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        times.push(start.elapsed().as_micros());
        pressure_times.push(m.pressure_solve_micros);
        accepted += 1;
        momentum_scratch = momentum_scratch.max(m.momentum_scratch_bytes);
        momentum_subcycles = momentum_subcycles.max(m.momentum_transport_subcycles);
        mass_defect = mass_defect.max(m.momentum_dual_mass_defect_kg);
        for (axis, error) in momentum_error.iter_mut().enumerate() {
            *error += m.momentum_transport_error_kg_m_s[axis];
        }
        substeps += m.substeps;
        fine_rows += m.pressure_active_rows_total;
        fine_iterations += m.pressure_iterations;
        coarse_rows += m.phase_predictor_rows_total;
        reuses += m.phase_predictor_reuses;
        rebuilds += m.phase_predictor_rebuilds;
        coarse_us += m.phase_predictor_micros;
        balanced_applications += m.phase_preconditioner_applications;
        balanced_us += m.phase_preconditioner_micros;
        balanced_scratch = balanced_scratch.max(m.phase_preconditioner_scratch_bytes);
        max_speed = max_speed.max(world.solver().max_liquid_speed_m_s());
    }
    let error =
        world.phase().water_volume_m3() + world.solver().cumulative_open_outflow_m3() - initial;
    let failure_json = failure
        .as_ref()
        .map_or_else(|| "null".into(), |e| format!("{e:?}"));
    let gate = accepted == 2 && error.abs() < 1e-9;
    println!(
        "{{\"size\":{size},\"seed\":1,\"season\":\"autumn\",\"backend\":\"fine_mac_phase_reference\",\"predictor\":{predictor},\"requested_steps\":2,\"accepted_steps\":{accepted},\"dt_s\":0.05,\"substeps\":{substeps},\"fine_pressure_rows_total\":{fine_rows},\"fine_pressure_iterations\":{fine_iterations},\"phase_balanced_preconditioner\":{balanced},\"balanced_applications\":{balanced_applications},\"balanced_scratch_peak_bytes\":{balanced_scratch},\"predictor_rows_total\":{coarse_rows},\"predictor_reuses\":{reuses},\"predictor_rebuilds\":{rebuilds},\"predictor_total_us\":{coarse_us},\"balanced_application_total_us\":{balanced_us},\"all_in_step_us\":{times:?},\"pressure_step_us\":{pressure_times:?},\"peak_liquid_speed_m_s\":{max_speed},\"accounting_error_m3\":{error},\"retained_array_upper_bound_bytes\":{},\"failure\":{failure_json},\"startup_gate_pass\":{gate},\"minute_rest_gate_accepted\":false,\"generated_trench_gate_accepted\":false,\"conservative_momentum_enabled\":{momentum},\"momentum_scratch_peak_bytes\":{momentum_scratch},\"momentum_subcycles_max\":{momentum_subcycles},\"momentum_transport_error_kg_m_s\":{momentum_error:?},\"momentum_dual_mass_defect_peak_kg\":{mass_defect},\"conservative_momentum_gate_accepted\":false,\"production_steps\":0}}",
        world.retained_array_bytes()
    );
    if !gate {
        return Err("fine reference startup gate failed".into());
    }
    Ok(())
}
