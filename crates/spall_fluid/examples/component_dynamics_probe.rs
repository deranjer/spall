//! Declared small dynamic/interface gates, not the ENG-121 generated trench.
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::component_fluid::{ComponentConfig, ComponentFluid};
use spall_fluid::cut_cell::{CutCellGeometry, GeometryLimits};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};
use std::{sync::Arc, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "channel".into());
    if !["channel", "low-dam", "full-wall"].contains(&mode.as_str()) {
        return Err("mode must be channel, low-dam or full-wall".into());
    }
    let steps = std::env::args()
        .nth(2)
        .map_or(Ok(600), |s| s.parse::<u32>())?;
    if steps == 0 || steps > 6000 {
        return Err("steps must be 1..=6000".into());
    }
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
                let cell = GlobalCell::new(i64::from(x), i64::from(y), i64::from(z));
                let solid = if mode == "channel" {
                    y < 3 || (x >= 18 && !(3..9).contains(&z) && y < 12)
                } else {
                    x == 4 && (mode == "full-wall" || y < 5)
                };
                if solid {
                    edits.set(cell, MaterialId(1));
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
    let mut fluid = ComponentFluid::new(geometry.clone(), &seed, ComponentConfig::default())?;
    let initial = fluid.water_volume_m3();
    let initial_energy = fluid.vertical_potential_energy_j() + fluid.kinetic_energy_j();
    let mut peak_energy = initial_energy;
    let mut peak_momentum = 0.0_f64;
    let mut peak_speed = 0.0_f64;
    let mut max_momentum_error = 0.0_f64;
    let mut max_mass_error = 0.0_f64;
    let mut accepted = 0;
    let mut times = Vec::new();
    let mut failure = None;
    for _ in 0..steps {
        let start = Instant::now();
        let metrics = match fluid.advance_momentum_operators(0.01) {
            Ok(m) => m,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        };
        times.push(start.elapsed().as_micros());
        accepted += 1;
        peak_energy =
            peak_energy.max(fluid.vertical_potential_energy_j() + fluid.kinetic_energy_j());
        peak_momentum = peak_momentum.max(fluid.momentum_kg_m_s()[0]);
        peak_speed = peak_speed.max(metrics.final_projection.max_connection_speed_m_s);
        max_mass_error = max_mass_error.max(metrics.advection.max_dual_mass_error_kg);
        max_momentum_error = max_momentum_error.max(
            metrics
                .advection
                .balance_error_kg_m_s
                .into_iter()
                .map(f64::abs)
                .fold(0.0, f64::max),
        );
    }
    let downstream: f64 = geometry
        .components()
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            if mode == "channel" {
                c.anchor.x >= 18 && (3..9).contains(&c.anchor.z)
            } else {
                c.anchor.x >= 5
            }
        })
        .map(|(i, _)| fluid.amounts_m3()[i])
        .sum();
    times.sort_unstable();
    let median = times
        .get(times.len() / 2)
        .map_or_else(|| "null".into(), ToString::to_string);
    let failure_json = failure
        .as_ref()
        .map_or_else(|| "null".into(), |e| format!("{e:?}"));
    let accounting = fluid.water_volume_m3() + fluid.outflow_m3() - initial;
    let gate_pass = accepted == steps
        && accounting.abs() < 1e-10
        && max_momentum_error < 1e-9
        && peak_energy <= initial_energy * 1.05
        && if mode == "channel" {
            downstream > 0.5 && peak_momentum > 0.1
        } else {
            downstream < 1e-9
        };
    println!(
        "{{\"fixture\":{mode:?},\"factor\":3,\"fine_dimensions\":{dims:?},\"requested_steps\":{steps},\"accepted_steps\":{accepted},\"dt_s\":0.01,\"advanced_operator_time_s\":{},\"initial_water_m3\":{initial},\"downstream_water_m3\":{downstream},\"peak_eastward_momentum_kg_m_s\":{peak_momentum},\"peak_speed_m_s\":{peak_speed},\"max_advection_balance_error_kg_m_s\":{max_momentum_error},\"max_dual_mass_error_kg\":{max_mass_error},\"initial_energy_j\":{initial_energy},\"peak_energy_j\":{peak_energy},\"accounting_error_m3\":{accounting},\"coupled_iteration_median_us\":{median},\"failure\":{failure_json},\"gate_pass\":{gate_pass},\"production_steps\":0}}",
        f64::from(accepted) * 0.01
    );
    if !gate_pass {
        return Err("declared dynamic/interface gate failed; see metrics".into());
    }
    Ok(())
}
