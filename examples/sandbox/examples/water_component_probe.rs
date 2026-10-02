//! ENG-122 operator feasibility probe; not a production fluid-step benchmark.
use sandbox::worldgen_scene;
use spall_fluid::SolidBoundary;
use spall_fluid::component_fluid::{ComponentConfig, ComponentFluid};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use std::{sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = std::env::args()
        .nth(1)
        .map_or(Ok(512), |v| v.parse::<u32>())?;
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
    let dims = setup.domain.dimensions();
    let origin = setup.domain.origin();
    let mut fractions = vec![0.0; setup.domain.cell_count()];
    for &(cell, fraction) in &setup.initial_fractions {
        let [x, y, z] = [cell.x - origin.x, cell.y - origin.y, cell.z - origin.z];
        if [x, y, z]
            .into_iter()
            .zip(dims)
            .any(|(p, n)| p < 0 || p >= i64::from(n))
        {
            return Err("seed outside domain".into());
        }
        fractions[x as usize + dims[0] as usize * (y as usize + dims[1] as usize * z as usize)] +=
            fraction;
    }
    let initial = ComponentFluid::new(geometry.clone(), &fractions, ComponentConfig::default())?;
    let mut fluid = initial.clone();
    let mut accepted = 0;
    let mut projection_us = Vec::new();
    let mut transport_us = Vec::new();
    let mut peak_speed = 0.0_f64;
    let mut peak_divergence = 0.0_f64;
    let mut max_iterations = 0;
    let mut failure = None;
    for _ in 0..20 {
        let mut candidate = fluid.clone();
        let start = Instant::now();
        let metrics = match candidate.project(0.05, true) {
            Ok(m) => m,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        projection_us.push(start.elapsed().as_micros());
        peak_speed = peak_speed.max(metrics.max_connection_speed_m_s);
        peak_divergence = peak_divergence.max(metrics.max_divergence_per_s);
        max_iterations = max_iterations.max(metrics.iterations);
        let start = Instant::now();
        match candidate.transport(0.05) {
            Ok(()) => {}
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
        transport_us.push(start.elapsed().as_micros());
        fluid = candidate;
        accepted += 1;
    }
    projection_us.sort_unstable();
    transport_us.sort_unstable();
    let projection_median = projection_us
        .get(projection_us.len() / 2)
        .map_or_else(|| "null".to_owned(), ToString::to_string);
    let transport_median = transport_us
        .get(transport_us.len() / 2)
        .map_or_else(|| "null".to_owned(), ToString::to_string);
    let failure_json = failure.map_or_else(|| "null".to_owned(), |e| format!("{e:?}"));
    println!(
        "{{\"size\":{size},\"seed\":1,\"season\":\"autumn\",\"components\":{},\"portals\":{},\"accepted_operator_iterations\":{accepted},\"requested_iterations\":20,\"dt_s\":0.05,\"peak_speed_m_s\":{peak_speed},\"peak_divergence_per_s\":{peak_divergence},\"max_pressure_iterations\":{max_iterations},\"projection_median_us\":{projection_median},\"transport_median_us\":{transport_median},\"accounting_error_m3\":{},\"failure\":{failure_json},\"production_fluid_steps\":0}}",
        geometry.components().len(),
        geometry.portals().len(),
        fluid.water_volume_m3() + fluid.outflow_m3() - initial.water_volume_m3()
    );
    Ok(())
}
