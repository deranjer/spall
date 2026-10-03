//! ENG-122 shallow-shore reproduction. A failed flow gate must exit nonzero.
//! Static, fully resolved shelf geometry; no growth, worker or partial solids.
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::{MacConfig, MacGridWorld, PressurePreconditioner};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

fn fixture(depth: f64, water_only: bool) -> Result<MacGridWorld, Box<dyn std::error::Error>> {
    // Fine quarter-metre voxels exactly resolve both full-cell solid layers.
    // Coarsening gives 12x6x2 cells at 1 m: shelf top at 2 m, trench floor at
    // 1 m, lip at x=6 m. Water has a flat 2+depth m surface over the shelf.
    let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [48, 24, 8], 10_000)?;
    let mut volume = Volume::new(VolumeId::new(901)?, CellSizeCode::Quarter);
    for x in 0..2 {
        volume.insert_brick(
            BrickCoord::new(x, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )?;
    }
    let mut edits = EditPlan::new(volume.id());
    for z in 0..8 {
        for y in 0..8 {
            for x in 0..48 {
                if y < 4 || x < 24 {
                    edits.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&edits)?;
    let boundary = SolidBoundary::capture(&volume, spec)?.coarsened(4)?;
    let mut grid = MacGridWorld::new(
        &boundary,
        MacConfig {
            cell_size_m: 1.0,
            ..Default::default()
        },
    )?;
    grid.set_pressure_preconditioner(PressurePreconditioner::Multigrid);
    if water_only {
        grid.set_freely_displaced_air()?;
    } else {
        grid.set_ambient_density(1.2)?;
    }
    for z in 0..2 {
        for x in 0..6 {
            grid.set_fraction(GlobalCell::new(x, 2, z), depth)?;
        }
    }
    Ok(grid)
}

struct Outcome {
    accepted: usize,
    first_rows: Option<u64>,
    rows: u64,
    initial: f64,
    error: f64,
    downstream: f64,
    max_speed: f64,
    bit_identical: bool,
    min: f64,
    max: f64,
    failure: Option<String>,
}

fn run(depth: f64, water_only: bool) -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut grid = fixture(depth, water_only)?;
    let initial = grid.water_volume_m3();
    let before = grid.fractions().to_vec();
    let (mut accepted, mut first_rows, mut rows, mut max_speed) = (0, None, 0, 0.0_f64);
    let mut failure = None;
    let trace = std::env::var("SHALLOW_SHORE_TRACE").as_deref() == Ok("1");
    for step in 0..600 {
        grid.set_stage_diagnostics(trace && step == 0);
        match grid.step(0.05) {
            Ok(m) => {
                accepted += 1;
                first_rows.get_or_insert(m.pressure_active_rows_total);
                rows += m.pressure_active_rows_total;
                max_speed = max_speed.max(grid.max_face_component_velocity_m_s());
                if m.pressure_converged_substeps != m.substeps {
                    failure = Some("pressure did not converge".into());
                    break;
                }
            }
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    let downstream = grid
        .fractions()
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 12 >= 6)
        .map(|(_, f)| f)
        .sum();
    Ok(Outcome {
        accepted,
        first_rows,
        rows,
        initial,
        downstream,
        max_speed,
        error: grid.water_volume_m3()
            + grid.trapped_volume_m3()
            + grid.cumulative_open_outflow_m3()
            - initial,
        bit_identical: before
            .iter()
            .zip(grid.fractions())
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        min: grid.fractions().iter().copied().fold(1.0, f64::min),
        max: grid.fractions().iter().copied().fold(0.0, f64::max),
        failure,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut shallow_passed = false;
    for (depth, water_only) in [(0.2, true), (0.75, true), (0.2, false)] {
        eprintln!("shallow-shore case: depth={depth}, water_only={water_only}");
        let o = run(depth, water_only)?;
        let flow = o.accepted == 600
            && o.failure.is_none()
            && o.downstream > 1e-9
            && o.error.abs() < 1e-10;
        if depth == 0.2 && water_only {
            shallow_passed = flow;
        }
        let failure = o
            .failure
            .as_ref()
            .map_or_else(|| "null".into(), |e| format!("{e:?}"));
        println!(
            "{{\"scenario\":\"resolved_shallow_shelf_to_lower_trench\",\"cell_size_m\":1,\"dimensions\":[12,6,2],\"water_depth_m\":{depth},\"water_only\":{water_only},\"requested_steps\":600,\"accepted_steps\":{},\"dt_s\":0.05,\"first_pressure_rows\":{},\"pressure_rows_total\":{},\"initial_water_m3\":{},\"downstream_water_m3\":{},\"water_error_m3\":{},\"max_face_speed_m_s\":{},\"fractions_bit_identical\":{},\"fraction_min\":{},\"fraction_max\":{},\"failure\":{failure},\"flow_gate_pass\":{flow}}}",
            o.accepted,
            o.first_rows
                .map_or_else(|| "null".into(), |n| n.to_string()),
            o.rows,
            o.initial,
            o.downstream,
            o.error,
            o.max_speed,
            o.bit_identical,
            o.min,
            o.max
        );
    }
    if !shallow_passed {
        return Err("ENG-122 shallow water cannot drain into the open trench".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeper_water_control_drains_and_conserves_volume() {
        let o = run(0.75, true).unwrap();
        assert_eq!(o.accepted, 600);
        assert!(o.failure.is_none(), "{:?}", o.failure);
        assert!(o.first_rows.unwrap() > 0);
        assert!(
            o.downstream > 0.5,
            "control did not drain: {}",
            o.downstream
        );
        assert!(o.error.abs() < 1e-10);
        assert!(o.min >= 0.0 && o.max <= 1.0);
    }

    #[test]
    #[ignore = "ENG-122 known failing shallow-lip flow gate; run explicitly, remove ignore with physical fix"]
    fn shallow_water_must_drain_into_the_open_lower_trench() {
        let o = run(0.2, true).unwrap();
        assert_eq!(o.accepted, 600);
        assert!(o.failure.is_none());
        assert!(o.error.abs() < 1e-10);
        assert!(
            o.downstream > 0.5,
            "shallow water froze: downstream={}, pressure rows={}, speed={}, bit-identical={}",
            o.downstream,
            o.rows,
            o.max_speed,
            o.bit_identical
        );
    }
}
