//! Deterministic, untimed ENG-122 numerical replay; no game/worker/edit clock.
use spall_fluid::grid_mac::MacGridWorld;
use std::io::BufReader;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: water_step_replay FILE [STEPS] [trace]")?;
    let steps = std::env::args()
        .nth(2)
        .map_or(Ok(1), |s| s.parse::<u32>())?;
    if steps == 0 || steps > 1000 {
        return Err("steps must be 1..=1000".into());
    }
    let trace = match std::env::args().nth(3).as_deref() {
        None => false,
        Some("trace") => true,
        _ => return Err("optional diagnostic must be trace".into()),
    };
    let mut grid =
        MacGridWorld::read_reconstructed_replay(BufReader::new(std::fs::File::open(path)?))?;
    grid.set_pressure_diagnostics(trace);
    let initial = grid.water_volume_m3();
    for step in 0..steps {
        let (cell, speed) = grid.cfl_limiting_cell_diagnostic();
        println!(
            "{{\"scenario\":\"reconstructed_water_replay\",\"source_revision\":{:?},\"step\":{step},\"dt_s\":0.05,\"dimensions\":{:?},\"limiting_cell\":[{},{},{}],\"limiting_outflow_l1_m_s\":{speed:.17e},\"face_speed_m_s\":{:.17e},\"interface_speed_m_s\":{:.17e},\"water_change_m3\":{:.17e},\"timing_acceptance\":false,\"owner_state_advanced\":false}}",
            option_env!("SPALL_SOURCE_REVISION").unwrap_or("unspecified"),
            grid.spec().dimensions(),
            cell.x,
            cell.y,
            cell.z,
            grid.max_face_component_velocity_m_s(),
            grid.reconstructed_interface_speed_m_s(),
            grid.water_volume_m3() - initial
        );
        let m = grid.step(0.05)?;
        if m.pressure_converged_substeps != m.substeps {
            return Err("pressure did not converge".into());
        }
        println!(
            "{{\"scenario\":\"reconstructed_water_replay_accepted\",\"step\":{step},\"substeps\":{},\"pressure_iterations\":{},\"conservation_error_m3\":{:.17e}}}",
            m.substeps, m.pressure_iterations, m.conservation_error_m3
        );
    }
    if let Some(path) = std::env::var_os("SPALL_WATER_REPLAY_OUTPUT") {
        grid.write_reconstructed_replay(std::io::BufWriter::new(std::fs::File::create(path)?))?;
    }
    Ok(())
}
