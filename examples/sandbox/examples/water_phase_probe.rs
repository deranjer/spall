//! ENG-122 phase-placement construction cost only; no coupled fluid steps.
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
    let limits = PhaseLimits {
        max_fine_cells: 4_000_000,
        max_faces: 12_000_000,
        max_basins: 200_000,
    };
    let mut times = Vec::new();
    let mut state = None;
    for _ in 0..5 {
        let start = Instant::now();
        let phase = PhaseWater::new(geometry.clone(), &fractions, 0.25, limits)?;
        times.push(start.elapsed().as_micros());
        state = Some(phase);
    }
    times.sort_unstable();
    let phase = state.ok_or("missing phase state")?;
    let start = Instant::now();
    let basins = phase.basins()?;
    let basin_us = start.elapsed().as_micros();
    let authored = fractions.iter().sum::<f64>() * 0.25_f64.powi(3);
    let basin_total = basins.iter().map(|b| b.water_m3).sum::<f64>();
    let components = phase.component_amounts_m3().iter().sum::<f64>();
    let wet_faces = phase
        .wet_face_areas_m2()
        .iter()
        .filter(|&&a| a > 0.0)
        .count();
    let error = basin_total - authored;
    let component_error = components - authored;
    let gate_pass = error.abs() < 1e-9 && component_error.abs() < 1e-9;
    println!(
        "{{\"worldgen\":\"showcase\",\"seed\":1,\"season\":\"autumn\",\"size\":{size},\"factor\":{},\"fine_cells\":{},\"fine_faces\":{},\"wet_faces\":{wet_faces},\"water_basins\":{},\"authored_water_m3\":{authored},\"phase_water_m3\":{},\"basin_accounting_error_m3\":{error},\"component_accounting_error_m3\":{component_error},\"phase_build_median_us\":{},\"basin_query_us\":{basin_us},\"phase_array_storage_bytes\":{},\"coupled_steps\":0,\"production_steps\":0,\"gate_pass\":{gate_pass}}}",
        setup.coarsen,
        fractions.len(),
        phase.faces().len(),
        basins.len(),
        phase.water_volume_m3(),
        times[2],
        phase.array_storage_bytes(),
    );
    if !gate_pass {
        return Err("phase seed accounting gate failed".into());
    }
    Ok(())
}
