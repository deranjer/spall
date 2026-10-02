//! ENG-122 geometry build/seed probe on actual generated worlds; no fluid steps.
//! cargo run --release -p sandbox --example water_geometry_probe -- 512
use sandbox::worldgen_scene;
use spall_fluid::SolidBoundary;
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size = std::env::args()
        .nth(1)
        .map_or(Ok(512), |arg| arg.parse::<u32>())?;
    let scene = worldgen_scene::generate_with_season(
        "showcase",
        1,
        size,
        spall_ecology::living::Season::Autumn,
    )?;
    let setup = scene.water_setup().ok_or("generated scene has no water")?;
    let started = Instant::now();
    let boundary = SolidBoundary::capture(&scene.world().terrain, setup.domain)?;
    let capture_us = started.elapsed().as_micros();
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..3 {
        let started = Instant::now();
        let geometry = CutCellGeometry::build(
            &boundary,
            setup.coarsen,
            GeometryLimits {
                max_fine_cells: 4_000_000,
                max_components: 200_000,
                max_portals: 600_000,
            },
        )?;
        times.push(started.elapsed().as_micros());
        last = Some(geometry);
    }
    times.sort_unstable();
    let geometry = last.unwrap();
    let dims = setup.domain.dimensions();
    let origin = setup.domain.origin();
    let mut amounts = vec![0.0; setup.domain.cell_count()];
    for &(cell, fraction) in &setup.initial_fractions {
        let [x, y, z] = [cell.x - origin.x, cell.y - origin.y, cell.z - origin.z];
        if [x, y, z]
            .into_iter()
            .zip(dims)
            .any(|(p, n)| p < 0 || p >= i64::from(n))
        {
            return Err("seed outside domain".into());
        }
        let i = x as usize + dims[0] as usize * (y as usize + dims[1] as usize * z as usize);
        amounts[i] += fraction;
    }
    let component_amounts = geometry.aggregate_amounts(&amounts)?;
    let seeded_m3 = component_amounts.iter().sum::<f64>() * 0.25_f64.powi(3);
    let authored_m3 = amounts.iter().sum::<f64>() * 0.25_f64.powi(3);
    let coarse_count = dims
        .into_iter()
        .map(|n| (n / setup.coarsen) as usize)
        .product::<usize>();
    let mut counts = vec![0; coarse_count];
    for c in geometry.components() {
        counts[c.coarse_index] += 1;
    }
    let split_cells = counts.iter().filter(|&&n| n > 1).count();
    let geometry_payload_bytes = setup.domain.cell_count() * std::mem::size_of::<u32>()
        + std::mem::size_of_val(geometry.components())
        + std::mem::size_of_val(geometry.portals());
    println!(
        "{{\"worldgen\":\"showcase\",\"seed\":1,\"size\":{size},\"season\":\"autumn\",\"fine_dimensions\":{:?},\"coarsen\":{},\"coarse_cells\":{coarse_count},\"components\":{},\"portals\":{},\"split_coarse_cells\":{split_cells},\"authored_m3\":{authored_m3},\"seeded_m3\":{seeded_m3},\"accounting_error_m3\":{},\"capture_us\":{capture_us},\"geometry_build_median_us\":{},\"geometry_payload_bytes\":{geometry_payload_bytes},\"fluid_steps\":0}}",
        dims,
        setup.coarsen,
        geometry.components().len(),
        geometry.portals().len(),
        seeded_m3 - authored_m3,
        times[1]
    );
    Ok(())
}
