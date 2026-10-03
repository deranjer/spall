//! ENG-122 pressure graph construction only; no pressure or momentum steps.
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
    let mut times = Vec::new();
    let mut result = None;
    for _ in 0..3 {
        let start = Instant::now();
        let graph = spall_fluid::phase_graph::PhaseGraph::build(
            &phase,
            spall_fluid::phase_graph::GraphLimits {
                max_fine_cells: 4_000_000,
                max_rows: 200_000,
                max_connections: 600_000,
            },
        )?;
        times.push(start.elapsed().as_micros());
        result = Some(graph);
    }
    times.sort_unstable();
    let graph = result.ok_or("missing graph")?;
    let open = graph
        .rows()
        .iter()
        .map(|r| r.fine_cells as usize)
        .sum::<usize>();
    let wet = graph
        .rows()
        .iter()
        .filter(|r| r.class == spall_fluid::phase_graph::PhaseClass::Wet)
        .count();
    let error = graph.rows().iter().map(|r| r.water_m3).sum::<f64>() - phase.water_volume_m3();
    let mut counts = vec![0usize; phase.geometry().components().len()];
    for r in graph.rows() {
        if r.class == spall_fluid::phase_graph::PhaseClass::Wet {
            counts[r.component as usize] += 1;
        }
    }
    let split = counts.iter().filter(|&&n| n > 1).count();
    let gate = error.abs() < 1e-9 && open > 0 && graph.rows().len() < open;
    println!(
        "{{\"size\":{size},\"factor\":{},\"fine_cells\":{},\"open_fine_cells\":{open},\"pressure_rows\":{},\"wet_rows\":{wet},\"dry_rows\":{},\"components_with_multiple_wet_rows\":{split},\"connections\":{},\"graph_build_median_us\":{},\"phase_array_storage_bytes\":{},\"graph_array_storage_bytes\":{},\"row_water_error_m3\":{error},\"pressure_steps\":0,\"momentum_steps\":0,\"coupled_steps\":0,\"production_steps\":0,\"gate_pass\":{gate}}}",
        setup.coarsen,
        fractions.len(),
        graph.rows().len(),
        graph.rows().len() - wet,
        graph.connections().len(),
        times[1],
        phase.array_storage_bytes(),
        graph.array_storage_bytes()
    );
    if !gate {
        return Err("pressure graph accounting gate failed".into());
    }
    Ok(())
}
