use std::time::{Duration, Instant};

use spall_fluid::fixtures::{RESERVOIR_CELL_SIZE_M, RESERVOIR_DIMENSIONS, TwoReservoirFixture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = parse_args()?;
    let dimensions = RESERVOIR_DIMENSIONS.map(|v| v.saturating_mul(cfg.scale));
    let mut fixture = if cfg.tunnel {
        TwoReservoirFixture::new_tunnel_under_separate_pool(cfg.scale, cfg.dt)?
    } else if cfg.stability {
        TwoReservoirFixture::new_stability_basin(cfg.scale, cfg.dt)?
    } else {
        TwoReservoirFixture::new_scaled(cfg.scale, cfg.dt)?
    };
    fixture
        .fluid_mut()
        .set_fixed_substeps_per_tick(cfg.substeps)?;
    let params = fixture.fluid().dfsph_parameters().expect("DFSPH solver");
    println!(
        "{{\"type\":\"fluid_run\",\"solver\":\"Salva DFSPH\",\"salva_version\":\"0.10.0\",\"scale\":{},\"dimensions_cells\":[{},{},{}],\"solid_cell_m\":{},\"particle_radius_m\":{},\"particle_spacing_m\":{},\"kernel_support_radius_m\":{},\"smoothing_factor\":2.0,\"configured_tick_s\":{},\"fixed_substeps_per_tick\":{},\"solver_dt_s\":{},\"gravity_m_s2\":[0,-9.81,0],\"parallel_feature\":{},\"boundary_sampling\":\"voxel faces ceil(cell_size/(2r)), deduplicated\",\"initial_particles\":{},\"initial_water_volume_m3\":{:.9},\"initial_particle_array_bytes_lower_bound\":{},\"pressure_min_iter\":{},\"pressure_max_iter\":{},\"pressure_max_density_error\":{},\"divergence_min_iter\":{},\"divergence_max_iter\":{},\"divergence_max_error\":{},\"density_sampling\":\"phase end only; diagnostic estimate absolute relative error\",\"boundaries\":\"closed finite floor, side/end walls, open top; dam y=1..9*scale-1\"}}",
        cfg.scale,
        dimensions[0],
        dimensions[1],
        dimensions[2],
        RESERVOIR_CELL_SIZE_M,
        fixture.fluid().world_particle_spacing_m() * 0.5,
        fixture.fluid().world_particle_spacing_m(),
        fixture.fluid().kernel_support_radius_m(),
        cfg.dt,
        cfg.substeps,
        cfg.dt / cfg.substeps as f32,
        cfg!(feature = "salva-parallel"),
        fixture.fluid().initial_particles(),
        fixture.fluid().initial_volume_m3(),
        fixture.fluid().particle_array_bytes(),
        params.min_pressure_iter,
        params.max_pressure_iter,
        params.max_density_error,
        params.min_divergence_iter,
        params.max_divergence_iter,
        params.max_divergence_error
    );

    if cfg.tunnel {
        run_tunnel(&mut fixture, cfg.ticks)?;
        return Ok(());
    }
    if cfg.stability {
        run_stability(&mut fixture, cfg.ticks);
        return Ok(());
    }
    run_phase(&mut fixture, "block_release_sealed", cfg.ticks);
    fixture.excavate_canal()?;
    let canal_rebuild_ms = fixture.fluid().last_boundary_rebuild_time().as_secs_f64() * 1000.0;
    run_phase(&mut fixture, "block_release_canal", cfg.ticks);
    fixture.break_dam()?;
    let breach_rebuild_ms = fixture.fluid().last_boundary_rebuild_time().as_secs_f64() * 1000.0;
    run_phase(&mut fixture, "block_release_breach", cfg.ticks);
    println!(
        "{{\"type\":\"boundary_rebuild\",\"canal_ms\":{canal_rebuild_ms:.6},\"breach_ms\":{breach_rebuild_ms:.6}}}"
    );
    Ok(())
}

fn run_tunnel(
    fixture: &mut TwoReservoirFixture,
    ticks: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let initial_pool_particles = fixture.elevated_pool_particle_count();
    let initial_east = fixture.particles_east_of_dam();
    fixture.excavate_canal()?;
    let rebuild = fixture.fluid().last_boundary_rebuild_time();
    let mut solver = Duration::ZERO;
    let mut wrapper = Duration::ZERO;
    let mut max_solid = 0usize;
    for _ in 0..ticks {
        let m = fixture.fluid_mut().advance();
        solver += m.solver_call_time;
        wrapper += m.wrapper_diagnostic_time;
        max_solid = max_solid.max(m.solid_penetration_count);
    }
    let m = fixture.fluid().metrics(Duration::ZERO);
    println!(
        "{{\"type\":\"tunnel_result\",\"ticks\":{ticks},\"simulated_seconds\":{:.6},\"initial_upper_pool_particles\":{initial_pool_particles},\"final_upper_pool_particles\":{},\"initial_downstream_particles\":{initial_east},\"final_downstream_particles\":{},\"retained_mass_kg\":{:.9},\"current_outside_domain_volume_m3\":{:.9},\"cumulative_permitted_open_top_outflow_m3\":{:.9},\"max_solid_crossings_in_a_tick\":{max_solid},\"first_invalid_crossing\":{},\"phase_end_mean_absolute_density_error\":{:.6},\"phase_end_max_absolute_density_error\":{:.6},\"solver_ms\":{:.3},\"wrapper_diagnostics_ms\":{:.3},\"boundary_rebuild_ms\":{:.6},\"upper_pool_particles_lost\":{}}}",
        ticks as f64 * f64::from(fixture.fluid().configured_tick_seconds()),
        fixture.elevated_pool_particle_count(),
        fixture.particles_east_of_dam(),
        m.retained_particle_mass_kg,
        m.current_outside_domain_volume_m3,
        m.cumulative_permitted_outflow_m3,
        format_crossing(m.first_invalid_crossing),
        m.mean_absolute_density_error,
        m.max_absolute_density_error,
        solver.as_secs_f64() * 1000.0,
        wrapper.as_secs_f64() * 1000.0,
        rebuild.as_secs_f64() * 1000.0,
        initial_pool_particles.saturating_sub(fixture.elevated_pool_particle_count())
    );
    Ok(())
}

fn run_stability(fixture: &mut TwoReservoirFixture, ticks: usize) {
    let initial = fixture.fluid().metrics(Duration::ZERO);
    let mut late_speeds = Vec::new();
    let mut late_energy_max = 0.0f64;
    let mut max_penetrations = 0usize;
    let mut first_crossing = None;
    let late_start = ticks * 2 / 3;
    let started = Instant::now();
    let mut solver_sum = Duration::ZERO;
    let mut wrapper_sum = Duration::ZERO;
    for tick in 0..ticks {
        let m = fixture.fluid_mut().advance();
        solver_sum += m.solver_call_time;
        wrapper_sum += m.wrapper_diagnostic_time;
        max_penetrations = max_penetrations.max(m.solid_penetration_count);
        first_crossing = first_crossing.or(m.first_invalid_crossing);
        if tick >= late_start {
            late_speeds.push(m.speed_p95_m_s);
            late_energy_max = late_energy_max.max(m.kinetic_energy_j);
        }
    }
    let elapsed = started.elapsed();
    let final_m = fixture.fluid().metrics(Duration::ZERO);
    late_speeds.sort_by(f64::total_cmp);
    let speed_p95 = late_speeds
        .get(
            ((late_speeds.len().saturating_sub(1) as f64 * 0.95).ceil() as usize)
                .min(late_speeds.len().saturating_sub(1)),
        )
        .copied()
        .unwrap_or(0.0);
    let drift = (final_m.surface_level_p95_m - initial.surface_level_p95_m).abs();
    println!(
        "{{\"type\":\"stability_result\",\"ticks\":{ticks},\"simulated_seconds\":{:.6},\"late_window_start_tick\":{late_start},\"late_window_p95_of_tick_p95_speed_m_s\":{speed_p95:.6},\"initial_kinetic_energy_j\":{:.6},\"late_window_max_kinetic_energy_j\":{late_energy_max:.6},\"surface_p95_initial_y_m\":{:.6},\"surface_p95_final_y_m\":{:.6},\"absolute_surface_level_drift_m\":{drift:.6},\"phase_end_mean_absolute_density_error\":{:.6},\"phase_end_max_absolute_density_error\":{:.6},\"max_solid_penetrations_in_one_tick\":{max_penetrations},\"first_invalid_crossing\":{},\"solver_total_ms\":{:.3},\"wrapper_diagnostics_total_ms\":{:.3},\"wall_elapsed_ms\":{:.3},\"solver_cost_per_1_60_sim_second_ms\":{:.3},\"all_substeps\":{}}}",
        ticks as f64 * f64::from(fixture.fluid().configured_tick_seconds()),
        initial.kinetic_energy_j,
        initial.surface_level_p95_m,
        final_m.surface_level_p95_m,
        final_m.mean_absolute_density_error,
        final_m.max_absolute_density_error,
        format_crossing(first_crossing),
        solver_sum.as_secs_f64() * 1000.0,
        wrapper_sum.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0,
        solver_sum.as_secs_f64() * 1000.0 / ticks as f64
            * (1.0 / 60.0 / f64::from(fixture.fluid().configured_tick_seconds())),
        fixture.fluid().total_substeps()
    );
}

fn run_phase(fixture: &mut TwoReservoirFixture, phase: &str, ticks: usize) {
    let east_before = fixture.particles_east_of_dam();
    let (west_level_before, east_level_before) = fixture.reservoir_p95_levels_m();
    let started = Instant::now();
    let mut solver = Vec::with_capacity(ticks);
    let mut wrapper = Vec::with_capacity(ticks);
    let mut max_speed = 0.0_f64;
    let mut max_p95_speed = 0.0_f64;
    let mut max_outside = 0usize;
    let mut max_solid_penetration = 0usize;
    let mut max_kinetic_energy = 0.0_f64;
    let mut pressure_iterations = 0u64;
    let mut divergence_iterations = 0u64;
    let mut pressure_converged = 0u64;
    let mut divergence_converged = 0u64;
    let mut last_pressure_residual = f64::NAN;
    let mut last_divergence_residual = f64::NAN;
    for _ in 0..ticks {
        let m = fixture.fluid_mut().advance();
        solver.push(m.solver_call_time);
        wrapper.push(m.wrapper_diagnostic_time);
        max_speed = max_speed.max(m.max_speed_m_s);
        max_p95_speed = max_p95_speed.max(m.speed_p95_m_s);
        max_outside = max_outside.max(m.particles_outside_domain);
        max_solid_penetration = max_solid_penetration.max(m.solid_penetration_count);
        max_kinetic_energy = max_kinetic_energy.max(m.kinetic_energy_j);
        pressure_iterations += m.pressure_iterations_per_tick;
        divergence_iterations += m.divergence_iterations_per_tick;
        pressure_converged += u64::from(m.pressure_converged_substeps);
        divergence_converged += u64::from(m.divergence_converged_substeps);
        last_pressure_residual = m.pressure_residual_last;
        last_divergence_residual = m.divergence_residual_last;
    }
    let measured_wall = started.elapsed();
    let density_started = Instant::now();
    let final_metrics = fixture.fluid().metrics(Duration::ZERO);
    let (west_level_after, east_level_after) = fixture.reservoir_p95_levels_m();
    let density_diagnostic = density_started.elapsed();
    let solver_sum: Duration = solver.iter().copied().sum();
    let wrapper_sum: Duration = wrapper.iter().copied().sum();
    let normalization = 1.0 / 60.0 / f64::from(fixture.fluid().configured_tick_seconds());
    let density_amortized_ms =
        density_diagnostic.as_secs_f64() * 1000.0 / ticks as f64 * normalization;
    let simulation_cost_60hz_ms =
        (solver_sum + wrapper_sum).as_secs_f64() * 1000.0 / ticks as f64 * normalization;
    println!(
        "{{\"type\":\"fluid_phase\",\"phase\":\"{phase}\",\"ticks\":{ticks},\"simulated_seconds\":{:.6},\"wall_elapsed_ms\":{:.3},\"solver_call_sum_ms\":{:.3},\"solver_call_p50_ms\":{:.3},\"solver_call_p95_ms\":{:.3},\"wrapper_diagnostic_sum_ms\":{:.3},\"density_diagnostic_end_ms\":{:.3},\"solver_cost_per_1_60_sim_second_ms\":{:.3},\"simulation_step_cost_per_1_60_sim_second_ms\":{:.3},\"density_probe_amortized_per_1_60_sim_second_ms\":{:.3},\"total_including_probe_per_1_60_sim_second_ms\":{:.3},\"pressure_iterations_mean_per_substep\":{:.3},\"divergence_iterations_mean_per_substep\":{:.3},\"pressure_converged_substeps\":{pressure_converged},\"divergence_converged_substeps\":{divergence_converged},\"total_solver_substeps\":{},\"last_pressure_residual\":{last_pressure_residual:.6},\"last_divergence_residual\":{last_divergence_residual:.6},\"salva_reported_solver_timer_s\":{:.9},\"retained_particle_mass_kg\":{:.9},\"retained_particle_volume_m3\":{:.9},\"current_outside_domain_volume_m3\":{:.9},\"cumulative_permitted_open_top_outflow_m3\":{:.9},\"outside_particles_max\":{max_outside},\"open_top_particles_end\":{},\"solid_penetrations_max\":{max_solid_penetration},\"first_invalid_crossing\":{},\"water_particles\":{},\"east_particles_before\":{east_before},\"east_particles_after\":{},\"reservoir_p95_levels_before_m\":[{:.6},{:.6}],\"reservoir_p95_levels_after_m\":[{:.6},{:.6}],\"eastward_momentum_end_kg_m_s\":{:.6},\"max_speed_m_s\":{max_speed:.6},\"max_step_speed_p95_m_s\":{max_p95_speed:.6},\"max_kinetic_energy_j\":{max_kinetic_energy:.6},\"volume_in_domain_m3\":{:.9},\"absolute_volume_balance_error_m3\":{:.12},\"active_cells\":{},\"particle_array_bytes_lower_bound\":{},\"phase_end_mean_absolute_density_error\":{:.6},\"phase_end_max_absolute_density_error\":{:.6},\"phase_end_surface_p95_y_m\":{:.6},\"last_step_substeps\":{}}}",
        ticks as f64 * f64::from(fixture.fluid().configured_tick_seconds()),
        measured_wall.as_secs_f64() * 1000.0,
        solver_sum.as_secs_f64() * 1000.0,
        percentile(&solver, 0.50).as_secs_f64() * 1000.0,
        percentile(&solver, 0.95).as_secs_f64() * 1000.0,
        wrapper_sum.as_secs_f64() * 1000.0,
        density_diagnostic.as_secs_f64() * 1000.0,
        solver_sum.as_secs_f64() * 1000.0 / ticks as f64 * normalization,
        simulation_cost_60hz_ms,
        density_amortized_ms,
        simulation_cost_60hz_ms + density_amortized_ms,
        pressure_iterations as f64
            / (ticks as f64 * fixture.fluid().fixed_substeps_per_tick() as f64),
        divergence_iterations as f64
            / (ticks as f64 * fixture.fluid().fixed_substeps_per_tick() as f64),
        ticks as u64 * fixture.fluid().fixed_substeps_per_tick() as u64,
        fixture.fluid().salva_solver_timer_seconds(),
        final_metrics.retained_particle_mass_kg,
        final_metrics.particle_volume_m3,
        final_metrics.current_outside_domain_volume_m3,
        final_metrics.cumulative_permitted_outflow_m3,
        final_metrics.open_top_particle_count,
        format_crossing(final_metrics.first_invalid_crossing),
        final_metrics.particle_count,
        fixture.particles_east_of_dam(),
        west_level_before,
        east_level_before,
        west_level_after,
        east_level_after,
        fixture.eastward_momentum_kg_m_s(),
        final_metrics.volume_in_domain_m3,
        final_metrics.absolute_balance_error_m3,
        final_metrics.active_cells,
        fixture.fluid().particle_array_bytes(),
        final_metrics.mean_absolute_density_error,
        final_metrics.max_absolute_density_error,
        final_metrics.surface_level_p95_m,
        final_metrics.substeps
    );
}

fn format_crossing(c: Option<spall_fluid::sph::InvalidCrossing>) -> String {
    match c {
        None => "null".to_owned(),
        Some(c) => format!(
            "{{\"kind\":\"{:?}\",\"tick\":{},\"particle\":{},\"position_m\":{:?},\"velocity_m_s\":{:?},\"voxel\":[{},{},{}]}}",
            c.kind,
            c.tick,
            c.particle_index,
            c.position_m,
            c.velocity_m_s,
            c.affected_voxel.x,
            c.affected_voxel.y,
            c.affected_voxel.z
        ),
    }
}
fn percentile(values: &[Duration], q: f64) -> Duration {
    let mut values = values.to_vec();
    values.sort_unstable();
    if values.is_empty() {
        Duration::ZERO
    } else {
        values[((values.len() - 1) as f64 * q).ceil() as usize].min(*values.last().unwrap())
    }
}
struct Config {
    scale: u32,
    ticks: usize,
    dt: f32,
    substeps: u32,
    stability: bool,
    tunnel: bool,
}
fn parse_args() -> Result<Config, Box<dyn std::error::Error>> {
    let (mut scale, mut ticks, mut dt, mut substeps): (u32, usize, f32, u32) =
        (1, 120, 1.0 / 60.0, 1);
    let mut stability = false;
    let mut tunnel = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--stability" {
            stability = true;
            continue;
        }
        if arg == "--tunnel" {
            tunnel = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value after {arg}"))?;
        match arg.as_str() {
            "--scale" => scale = value.parse()?,
            "--ticks" => ticks = value.parse()?,
            "--dt" => dt = value.parse()?,
            "--substeps" => substeps = value.parse()?,
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    if scale == 0 || ticks == 0 || substeps == 0 || !dt.is_finite() || dt <= 0.0 {
        return Err("scale/ticks/dt/substeps must be positive and finite".into());
    }
    Ok(Config {
        scale,
        ticks,
        dt,
        substeps,
        stability,
        tunnel,
    })
}
