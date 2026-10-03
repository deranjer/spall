//! Matched generated coarse-grid lake/breach comparison, solver cost only.
//! Not ENG-121's paced hammer/growth workload or full generated-world gate.
use sandbox::worldgen_scene;
use spall_core::{GlobalCell, MaterialId};
use spall_fluid::grid_mac::{MacGridWorld, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_sim::{AuthoritativeWater, WaterExecution};
use spall_voxel::EditPlan;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = std::env::args()
        .nth(1)
        .map_or(Ok(512), |s| s.parse::<u32>())?;
    let steps = std::env::args()
        .nth(2)
        .map_or(Ok(120), |s| s.parse::<usize>())?;
    if ![512, 1024].contains(&size) || !(40..=1200).contains(&steps) {
        return Err("size must be 512/1024; steps 40..=1200".into());
    }
    let scene = worldgen_scene::generate_with_season(
        "showcase",
        1,
        size,
        spall_ecology::living::Season::Autumn,
    )?;
    let mut setup = scene.water_setup().ok_or("dry world")?.clone();
    setup.execution = WaterExecution::Inline;
    let owner = AuthoritativeWater::new(&scene.world().terrain, setup.clone())?;
    let initial_fluid_cells = owner
        .grid()
        .ok_or("missing inline grid")?
        .spec()
        .cell_count();
    let mut extended_dims = setup.domain.dimensions();
    extended_dims[0] += 40u32.div_ceil(setup.coarsen) * setup.coarsen;
    setup.domain = DomainSpec::new(setup.domain.origin(), extended_dims, 32 * 1024 * 1024)?;
    let owner = owner.grown(&scene.world().terrain, setup.domain)?;
    let initial = owner
        .grid()
        .ok_or("missing inline grid")?
        .fractions()
        .to_vec();
    let boundary =
        SolidBoundary::capture(&scene.world().terrain, setup.domain)?.coarsened(setup.coarsen)?;
    // Extend the domain before timing, then open a 40-voxel eastward trench.
    // Growth, hammer timing, collision work and worker scheduling are excluded.
    let start = setup
        .initial_fractions
        .iter()
        .max_by_key(|(p, _)| (p.x, p.y))
        .ok_or("no water")?
        .0;
    let c = i64::from(setup.coarsen);
    let origin = setup.domain.origin();
    let dims = setup.domain.dimensions();
    let end_x = (start.x + 40).min(origin.x + i64::from(dims[0]) - 2);
    let mut terrain = scene.world().terrain.clone();
    let mut edits = EditPlan::new(terrain.id());
    for z in (start.z - c).max(origin.z)..=(start.z + c).min(origin.z + i64::from(dims[2]) - 1) {
        for y in (start.y - c).max(origin.y)..=(start.y + c).min(origin.y + i64::from(dims[1]) - 1)
        {
            for x in start.x - c..=end_x {
                edits.set(GlobalCell::new(x, y, z), MaterialId::AIR);
            }
        }
    }
    terrain.apply_edit(&edits)?;
    let breached = SolidBoundary::capture(&terrain, setup.domain)?.coarsened(setup.coarsen)?;
    for water_only in [false, true] {
        let mut grid = MacGridWorld::new(&boundary, setup.config)?;
        grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
        if water_only {
            grid.set_freely_displaced_air()?;
        } else {
            grid.set_ambient_density(1.2)?;
        }
        grid.restore_fractions(&initial)?;
        let initial_volume = grid.water_volume_m3();
        let (mut rows, mut iterations, mut repairs, mut scratch) = (0, 0, 0, 0);
        let mut times = Vec::new();
        let mut flowing_times = Vec::new();
        let mut failure = None;
        for step in 0..steps {
            if step == 20 {
                grid.refresh_boundary_displacing(&breached)?;
            }
            let timer = Instant::now();
            match grid.step(0.05) {
                Ok(m) => {
                    let elapsed = timer.elapsed().as_micros() as u64;
                    times.push(elapsed);
                    if step >= 20 {
                        flowing_times.push(elapsed);
                    }
                    rows += m.pressure_active_rows_total;
                    iterations += m.pressure_iterations;
                    repairs += m.strict_path_repair_count;
                    scratch = scratch.max(m.strict_path_scratch_bytes);
                    if m.pressure_converged_substeps != m.substeps {
                        failure = Some("pressure did not converge".to_owned());
                        break;
                    }
                }
                Err(e) => {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
        let mut downstream = 0.0;
        let coarse_origin = grid.spec().origin();
        let [nx, _, _] = grid.spec().dimensions().map(|d| d as usize);
        for (i, f) in grid.fractions().iter().enumerate() {
            let center_x = coarse_origin.x as f64 + ((i % nx) as f64 + 0.5) * c as f64;
            if center_x > (start.x + c) as f64 {
                downstream += f * setup.config.cell_size_m.powi(3);
            }
        }
        times.sort_unstable();
        flowing_times.sort_unstable();
        let median = |v: &[u64]| v.get(v.len() / 2).copied();
        println!(
            "{}",
            serde_json::json!({
                "scenario":"generated_expanded_domain_rectangular_trench", "size":size,
                "seed":1,"season":"autumn","water_only":water_only,
                "coarsen":setup.coarsen,"fluid_cells":grid.spec().cell_count(),
                "initial_fluid_cells":initial_fluid_cells,
                "requested_steps":steps,"accepted_steps":times.len(),"dt_s":0.05,
                "step_median_us":median(&times),"flowing_median_us":median(&flowing_times),
                "pressure_rows_total":rows,"pressure_iterations_total":iterations,
                "downstream_m3":downstream,"initial_water_m3":initial_volume,
                "water_error_m3":grid.water_volume_m3()+grid.cumulative_open_outflow_m3()-initial_volume,
                "retained_grid_array_bytes":grid.allocated_bytes(),
                "strict_path_repairs":repairs,"strict_path_scratch_bytes":scratch,
                "fraction_min":grid.fractions().iter().copied().fold(1.0,f64::min),
                "fraction_max":grid.fractions().iter().copied().fold(0.0,f64::max),
                "breach_start":[start.x,start.y,start.z],"breach_end_x":end_x,
                "failure":failure,"production_default":water_only,
                "paced_trench_gate_accepted":false,
            })
        );
    }
    Ok(())
}
