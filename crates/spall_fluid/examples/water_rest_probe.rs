//! Bounded rest-state diagnostic, not a performance or gameplay acceptance gate.
//! Run: cargo run --release -p spall_fluid --example water_rest_probe
//! Compares resolved stairs with partially solid cells represented as water/air.
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::{MacConfig, MacGridWorld, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for case in ["flat", "aligned_stairs", "unaligned_stairs"] {
        let id = VolumeId::new(7)?;
        let mut terrain = Volume::new(id, CellSizeCode::Quarter);
        for bx in 0..2 {
            terrain.insert_brick(
                BrickCoord::new(bx, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )?;
        }
        let floor = |x: i64| match case {
            "flat" => 3,
            "aligned_stairs" => 3 * (1 + x / 12),
            _ => 3 + x / 5,
        };
        let mut edit = EditPlan::new(id);
        for z in 0..24 {
            for x in 0..48 {
                for y in 0..floor(x) {
                    edit.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
        terrain.apply_edit(&edit)?;
        let fine_spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [48, 24, 24], 27_648)?;
        let boundary = SolidBoundary::capture(&terrain, fine_spec)?.coarsened(3)?;
        let mut grid = MacGridWorld::new(
            &boundary,
            MacConfig {
                cell_size_m: 0.75,
                open_top: true,
                ..MacConfig::default()
            },
        )?;
        grid.set_ambient_density(1.2)?;
        grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
        let mut dropped_m3 = 0.0;
        let mut partial_cells = 0;
        for z in 0..8 {
            for y in 0..8 {
                for x in 0..16 {
                    let mut wet = 0;
                    for dx in 0..3 {
                        for dy in 0..3 {
                            let fy = y * 3 + dy;
                            if fy >= floor(x * 3 + dx) && fy < 15 {
                                wet += 3;
                            }
                        }
                    }
                    let fraction = f64::from(wet) / 27.0;
                    let cell = GlobalCell::new(x, y, z);
                    if boundary.is_solid(cell) == Some(true) {
                        dropped_m3 += fraction * 0.75_f64.powi(3);
                    } else {
                        grid.set_fraction(cell, fraction)?;
                        partial_cells += usize::from(fraction > 0.0 && fraction < 1.0);
                    }
                }
            }
        }
        let initial = grid.fractions().to_vec();
        let seeded_m3 = grid.water_volume_m3();
        let mut step_times = Vec::new();
        let mut peak_speed = 0.0_f64;
        let mut first_ke = 0.0;
        let mut unconverged = 0;
        for step in 0..120 {
            let m = grid.step(0.05)?;
            unconverged += m.substeps - m.pressure_converged_substeps;
            step_times.push(m.total_micros);
            peak_speed = peak_speed.max(grid.max_liquid_speed_m_s());
            if step == 0 {
                first_ke = grid.kinetic_energy_j();
            }
        }
        step_times.sort_unstable();
        let drift = grid
            .fractions()
            .iter()
            .zip(initial)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        println!(
            "{{\"case\":\"{case}\",\"fluid_cells\":1024,\"cell_m\":0.75,\"dt_s\":0.05,\"steps\":120,\"initial_partial_cells\":{partial_cells},\"seeded_m3\":{seeded_m3},\"dropped_in_solid_m3\":{dropped_m3},\"first_step_ke_j\":{first_ke},\"final_ke_j\":{},\"peak_liquid_speed_m_s\":{peak_speed},\"final_liquid_speed_m_s\":{},\"max_fraction_drift\":{drift},\"accounting_error_m3\":{},\"unconverged_substeps\":{unconverged},\"median_step_us\":{}}}",
            grid.kinetic_energy_j(),
            grid.max_liquid_speed_m_s(),
            grid.water_volume_m3() + grid.cumulative_open_outflow_m3() - seeded_m3,
            step_times[step_times.len() / 2],
        );
    }
    Ok(())
}
