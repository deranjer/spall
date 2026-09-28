//! Rough cost of the two-phase solver on a valley-shaped open-top domain.
//! `cargo run --release -p spall_fluid --example valley_water_bench -- NX NY NZ CELL_M`
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::{MacConfig, MacGridWorld, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let nx: u32 = a.first().map_or(96, |s| s.parse().unwrap());
    let ny: u32 = a.get(1).map_or(12, |s| s.parse().unwrap());
    let nz: u32 = a.get(2).map_or(64, |s| s.parse().unwrap());
    let cell_m: f64 = a.get(3).map_or(0.25, |s| s.parse().unwrap());
    let ticks: u32 = a.get(4).map_or(120, |s| s.parse().unwrap());
    let tol: f64 = a.get(5).map_or(1.0e-8, |s| s.parse().unwrap());
    let id = VolumeId::new(7).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..nz.div_ceil(32) as i64 {
        for by in 0..ny.div_ceil(32) as i64 {
            for bx in 0..nx.div_ceil(32) as i64 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    // V-shaped valley along z with a dam at z = nz/2 holding a higher pool.
    let mut plan = EditPlan::new(id);
    let cx = nx as i64 / 2;
    let floor = |x: i64| 1 + ((x - cx).abs() / 3).min(ny as i64 - 3);
    for z in 0..nz as i64 {
        for x in 0..nx as i64 {
            for y in 0..floor(x) {
                plan.set(GlobalCell::new(x, y, z), MaterialId(1));
            }
        }
    }
    let dam_z = nz as i64 / 2;
    for x in 0..nx as i64 {
        for y in 0..(ny as i64 - 3) {
            plan.set(GlobalCell::new(x, y, dam_z), MaterialId(1));
        }
    }
    volume.apply_edit(&plan).unwrap();
    let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [nx, ny, nz], usize::MAX).unwrap();
    let boundary = SolidBoundary::capture(&volume, spec).unwrap();
    let mut grid = MacGridWorld::new(
        &boundary,
        MacConfig {
            cell_size_m: cell_m,
            open_top: true,
            ..MacConfig::default()
        },
    )
    .unwrap();
    grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
    grid.set_ambient_density(1.2).unwrap();
    grid.set_pressure_tolerances(tol, tol).unwrap();
    let start_volume = grid.water_volume_m3();
    let mut water = 0;
    for z in 0..nz as i64 {
        let level = if z < dam_z { ny as i64 - 4 } else { 3 };
        for x in 0..nx as i64 {
            for y in floor(x)..level {
                if boundary.is_solid(GlobalCell::new(x, y, z)) == Some(false) {
                    grid.set_fraction(GlobalCell::new(x, y, z), 1.0).unwrap();
                    water += 1;
                }
            }
        }
    }
    println!(
        "domain {nx}x{ny}x{nz} = {} cells, water {water}, cell {cell_m} m",
        spec.cell_count()
    );
    let mut worst = 0.0f64;
    let started = Instant::now();
    let mut skipped = 0;
    for tick in 0..ticks {
        // Breach the dam after one second to exercise real flow.
        if tick == 60 {
            let mut v2 = volume.clone();
            let mut p = EditPlan::new(id);
            for x in cx - 3..=cx + 3 {
                for y in 1..(ny as i64 - 3) {
                    p.set(GlobalCell::new(x, y, dam_z), MaterialId::AIR);
                }
            }
            v2.apply_edit(&p).unwrap();
            volume = v2;
            let b = SolidBoundary::capture(&volume, spec).unwrap();
            grid.refresh_boundary_displacing(&b).unwrap();
        }
        let t = Instant::now();
        match grid.step(1.0 / 60.0) {
            Ok(m) => {
                if tick % 30 == 29 {
                    println!(
                        "  adv {} us, pressure {} us ({} iters, {} rows), transport {} us, boundary {} us, total {} us",
                        m.velocity_advection_micros,
                        m.pressure_solve_micros,
                        m.pressure_iterations,
                        m.pressure_active_rows_total,
                        m.transport_micros,
                        m.boundary_micros,
                        m.total_micros
                    );
                }
            }
            Err(_) => skipped += 1,
        }
        worst = worst.max(t.elapsed().as_secs_f64() * 1e3);
    }
    let mean = started.elapsed().as_secs_f64() * 1e3 / f64::from(ticks);
    println!(
        "mean {mean:.2} ms/tick, worst {worst:.2} ms, skipped {skipped}, max speed {:.2} m/s, volume drift {:.3e} m3, KE {:.1} J",
        grid.max_liquid_speed_m_s(),
        grid.water_volume_m3() + grid.cumulative_open_outflow_m3() - start_volume,
        grid.kinetic_energy_j()
    );
}
