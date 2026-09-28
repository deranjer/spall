use std::{collections::HashMap, time::Instant};

#[cfg(feature = "allocation-diagnostics")]
#[global_allocator]
static GRID_ALLOCATOR: allocation_trace::CountingAllocator =
    allocation_trace::CountingAllocator::new();

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::{
    DomainSpec, SolidBoundary,
    grid_mac::{
        GridReservoirFixture, LiquidSpeedSample, MacConfig, MacGridWorld, PressurePreconditioner,
        StandingWaveFixture, amplitude_ratio_per_period, oscillation_peaks, zero_crossing_period,
    },
};
use spall_voxel::{Brick, Volume};

mod allocation_trace {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct AllocationStats {
        pub allocator_time_ns: u64,
        pub allocation_count: u64,
        pub allocated_bytes: u64,
        pub deallocated_bytes: u64,
    }

    #[cfg(feature = "allocation-diagnostics")]
    pub struct CountingAllocator {
        active: std::sync::atomic::AtomicBool,
        nanos: std::sync::atomic::AtomicU64,
        count: std::sync::atomic::AtomicU64,
        allocated: std::sync::atomic::AtomicU64,
        deallocated: std::sync::atomic::AtomicU64,
    }

    #[cfg(feature = "allocation-diagnostics")]
    impl CountingAllocator {
        pub const fn new() -> Self {
            Self {
                active: std::sync::atomic::AtomicBool::new(false),
                nanos: std::sync::atomic::AtomicU64::new(0),
                count: std::sync::atomic::AtomicU64::new(0),
                allocated: std::sync::atomic::AtomicU64::new(0),
                deallocated: std::sync::atomic::AtomicU64::new(0),
            }
        }

        fn record(
            &self,
            start: Option<std::time::Instant>,
            allocation_call: bool,
            allocated: usize,
            deallocated: usize,
        ) {
            if let Some(start) = start {
                self.nanos.fetch_add(
                    start.elapsed().as_nanos() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                if allocation_call {
                    self.count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                self.allocated
                    .fetch_add(allocated as u64, std::sync::atomic::Ordering::Relaxed);
                self.deallocated
                    .fetch_add(deallocated as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }

        fn start(&self) {
            use std::sync::atomic::Ordering::Relaxed;
            self.active.store(false, Relaxed);
            self.nanos.store(0, Relaxed);
            self.count.store(0, Relaxed);
            self.allocated.store(0, Relaxed);
            self.deallocated.store(0, Relaxed);
            self.active.store(true, Relaxed);
        }

        fn finish(&self) -> AllocationStats {
            use std::sync::atomic::Ordering::Relaxed;
            self.active.store(false, Relaxed);
            AllocationStats {
                allocator_time_ns: self.nanos.load(Relaxed),
                allocation_count: self.count.load(Relaxed),
                allocated_bytes: self.allocated.load(Relaxed),
                deallocated_bytes: self.deallocated.load(Relaxed),
            }
        }
    }

    #[cfg(feature = "allocation-diagnostics")]
    unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            let start = self
                .active
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(std::time::Instant::now);
            let ptr = unsafe { std::alloc::System.alloc(layout) };
            self.record(
                start,
                true,
                if ptr.is_null() { 0 } else { layout.size() },
                0,
            );
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            let start = self
                .active
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(std::time::Instant::now);
            let ptr = unsafe { std::alloc::System.alloc_zeroed(layout) };
            self.record(
                start,
                true,
                if ptr.is_null() { 0 } else { layout.size() },
                0,
            );
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            let start = self
                .active
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(std::time::Instant::now);
            unsafe { std::alloc::System.dealloc(ptr, layout) };
            self.record(start, false, 0, layout.size());
        }

        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            let start = self
                .active
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(std::time::Instant::now);
            let new_ptr = unsafe { std::alloc::System.realloc(ptr, layout, new_size) };
            self.record(
                start,
                true,
                if new_ptr.is_null() { 0 } else { new_size },
                layout.size(),
            );
            new_ptr
        }
    }

    pub fn begin(enabled: bool) -> bool {
        #[cfg(feature = "allocation-diagnostics")]
        if enabled {
            super::GRID_ALLOCATOR.start();
            return true;
        }
        let _ = enabled;
        false
    }

    pub fn finish(active: bool) -> AllocationStats {
        #[cfg(feature = "allocation-diagnostics")]
        if active {
            return super::GRID_ALLOCATOR.finish();
        }
        let _ = active;
        AllocationStats::default()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::parse()?;
    if cfg.acceptance
        && (!matches!(cfg.scenario.as_str(), "basin" | "equilibrium")
            || cfg.scale != 1
            || cfg.refinement != 1
            || cfg.ticks != 1800
            || (cfg.dt - 1.0 / 60.0).abs() > 1.0e-12)
    {
        return Err(
            "--acceptance requires the base 30-second basin or equilibrium case: --scenario basin --scale 1 --refinement 1 --ticks 1800 --dt 0.0166666666666667".into(),
        );
    }
    if cfg.allocation_diagnostics && !cfg!(feature = "allocation-diagnostics") {
        return Err(
            "--allocation-diagnostics requires --features spall_fluid/allocation-diagnostics"
                .into(),
        );
    }
    if cfg.scenario == "sensitivity" {
        return run_hydrostatic_sensitivity();
    }
    if cfg.scenario == "standing-wave" {
        return run_standing_wave(&cfg);
    }
    if cfg.refinement > 1 && !matches!(cfg.scenario.as_str(), "tunnel" | "basin" | "equilibrium") {
        return Err("refinement is currently measured only for basin and tunnel fixtures".into());
    }
    let setup_start = Instant::now();
    let mut fixture = if cfg.scenario == "tunnel" {
        GridReservoirFixture::new_tunnel_refined(cfg.scale, cfg.refinement)?
    } else if cfg.scenario == "equilibrium" {
        GridReservoirFixture::new_equilibrium_basin(cfg.scale, cfg.refinement)?
    } else if cfg.scenario == "basin" {
        GridReservoirFixture::new_basin_refined(cfg.scale, cfg.refinement)?
    } else {
        GridReservoirFixture::new(cfg.scale, true)?
    };
    fixture
        .grid_mut()
        .set_pressure_tolerances(cfg.pressure_tolerance, 1.0e-8)?;
    fixture
        .grid_mut()
        .set_pressure_diagnostics(cfg.pressure_diagnostics);
    fixture
        .grid_mut()
        .set_stage_diagnostics(cfg.stage_diagnostics);
    if let Some(density) = cfg.ambient_density {
        fixture.grid_mut().set_ambient_density(density)?;
    }
    if cfg.incompressible_air {
        fixture.grid_mut().set_compressible_enclosed_air(false)?;
    }
    println!(
        "{{\"type\":\"grid_physical_model\",\"pressure_model\":\"{}\",\"ambient_density_kg_m3\":{},\"transport\":\"{}\",\"step_order\":\"{}\",\"sealed_air\":\"{}\"}}",
        if cfg.ambient_density.is_some() {
            "two_phase_variable_density"
        } else {
            "single_phase_C_gt_0"
        },
        cfg.ambient_density
            .map_or_else(|| "null".to_owned(), |v| v.to_string()),
        if cfg.ambient_density.is_some() {
            "PLIC_target_FCT"
        } else {
            "MUSCL_target_FCT"
        },
        if cfg.ambient_density.is_some() {
            "advection,gravity,pressure,transport"
        } else {
            "gravity,advection,pressure,transport"
        },
        match (cfg.ambient_density.is_some(), cfg.incompressible_air) {
            (false, _) => "not_simulated",
            (true, false) => "isothermal_compressible",
            (true, true) => "incompressible",
        }
    );
    fixture
        .grid_mut()
        .set_pressure_preconditioner(match cfg.preconditioner.as_str() {
            "jacobi" => PressurePreconditioner::Jacobi,
            "ic0" => PressurePreconditioner::Ic0,
            "mic0" => PressurePreconditioner::Mic0,
            "mg" => PressurePreconditioner::Multigrid,
            _ => return Err("--preconditioner must be jacobi, ic0, mic0, or mg".into()),
        });
    let initial_volume = fixture.grid().water_volume_m3();
    let initial_downstream = fixture.downstream_volume_m3();
    let initial_level_difference = fixture.p95_level_difference_m();
    let initial_basin_surface = fixture.basin_surface_p95_m();
    let initial_upper_pool = fixture.upper_pool_volume_m3();
    let initial_lower_tunnel = fixture.lower_tunnel_volume_m3();
    let initial_kinetic_energy = fixture.grid().kinetic_energy_j();
    let initial_potential_energy = fixture.grid().gravitational_potential_energy_j();
    match cfg.scenario.as_str() {
        "sealed" => {}
        "canal" => fixture.excavate_canal()?,
        "breach" => fixture.breach_dam()?,
        "closure" => {
            fixture.excavate_canal()?;
            fixture.close_canal()?;
        }
        "tunnel" => {}
        "basin" | "equilibrium" => {}
        _ => return Err(format!("unknown scenario {}", cfg.scenario).into()),
    }
    let setup_ms = setup_start.elapsed().as_secs_f64() * 1000.0;
    let g = fixture.grid();
    let dims = g.spec().dimensions();
    println!(
        "{{\"type\":\"grid_config\",\"backend\":\"MAC-FCT-VOF\",\"preconditioner\":\"{}\",\"scenario\":\"{}\",\"scale\":{},\"refinement_factor\":{},\"grid_dimensions\":[{},{},{}],\"terrain_cell_m\":0.25,\"water_cell_m\":{},\"initial_water_volume_m3\":{:.12},\"density_kg_m3\":{},\"gravity_m_s2\":[0,-9.81,0],\"fixed_outer_tick_s\":{},\"max_substeps_per_outer_tick\":{},\"cfl_limit\":{},\"pressure_tolerance_relative\":{},\"pressure_tolerance_absolute_pa_per_m2_l2\":1e-8,\"pressure_diagnostics\":{},\"fraction_diagnostics\":{},\"stage_diagnostics\":{},\"allocation_diagnostics\":{},\"allocation_tracking_feature_enabled\":{},\"pressure_iteration_limit\":{},\"build_profile\":\"{}\",\"rayon_threads\":{},\"boundary_conditions\":\"closed x/z/floor, atmospheric free surface, solid faces zero-normal-flow\",\"method_doc\":\"docs/reports/ENG-103-grid-method.md\",\"allocated_solver_bytes\":{},\"hardware\":\"not queried by runner; record host separately\"}}",
        cfg.preconditioner,
        cfg.scenario,
        cfg.scale,
        cfg.refinement,
        dims[0],
        dims[1],
        dims[2],
        g.config().cell_size_m,
        g.water_volume_m3(),
        g.config().density_kg_m3,
        cfg.dt,
        g.config().max_substeps,
        g.config().cfl_limit,
        g.config().pressure_relative_tolerance,
        g.config().pressure_diagnostics,
        cfg.fraction_diagnostics,
        cfg.stage_diagnostics,
        cfg.allocation_diagnostics,
        cfg!(feature = "allocation-diagnostics"),
        g.config().pressure_max_iterations,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        rayon::current_num_threads(),
        g.allocated_bytes(),
    );
    let start = Instant::now();
    let mut costs = Vec::with_capacity(cfg.ticks);
    let mut diagnostic_costs = Vec::with_capacity(cfg.ticks);
    let mut all_in_tick_costs = Vec::with_capacity(cfg.ticks);
    let mut pressure_residual_max: f64 = 0.0;
    let mut divergence_after_max: f64 = 0.0;
    let mut pressure_converged = 0u64;
    let mut pressure_total = 0u64;
    let mut pressure_active_rows_total = 0u64;
    let mut max_discharge: f64 = 0.0;
    let mut max_momentum: f64 = 0.0;
    let mut conservation_error_max: f64 = 0.0;
    let mut late_window_max_speed: f64 = 0.0;
    let mut late_speed_histograms: [Vec<f64>; 4] = std::array::from_fn(|_| vec![0.0; 400]);
    let mut late_speed_water_total = 0.0;
    let mut late_speed_water_over_limit = 0.0;
    const OCCUPANCY_CUTS: [f64; 5] = [0.0, 1.0e-6, 1.0e-4, 1.0e-3, 1.0e-2];
    let mut occupancy_histograms: [Vec<f64>; 5] = std::array::from_fn(|_| vec![0.0; 400]);
    let mut occupancy_volume = [0.0; 5];
    let mut occupancy_fast_volume = [0.0; 5];
    let mut occupancy_cells = [0usize; 5];
    let mut tiny_neighbor_speed_histogram = vec![0.0; 400];
    let mut tiny_neighbor_volume = 0.0;
    let mut tiny_neighbor_fast_volume = 0.0;
    let mut tiny_neighbor_samples = 0usize;
    let mut tiny_neighbor_max_speed: f64 = 0.0;
    let mut late_maximum_sample: Option<(usize, LiquidSpeedSample)> = None;
    let mut late_high_speed_ticks = 0usize;
    let mut upper_pool_overtopping_peak = 0.0f64;
    let mut mechanical_energy_peak = initial_kinetic_energy + initial_potential_energy;
    let mut upper_pool_outflow = 0.0f64;
    let mut upper_pool_inflow = 0.0f64;
    let mut boundary_handling_total_ms = 0.0f64;
    let mut allocation_trace_ns_total = 0u64;
    let mut allocation_count_total = 0u64;
    let mut allocation_bytes_total = 0u64;
    let mut deallocation_bytes_total = 0u64;
    for tick in 0..cfg.ticks {
        let allocation_tracking = allocation_trace::begin(cfg.allocation_diagnostics);
        let tick_start = Instant::now();
        let m = fixture.step(cfg.dt)?;
        let allocation_stats = allocation_trace::finish(allocation_tracking);
        allocation_trace_ns_total += allocation_stats.allocator_time_ns;
        allocation_count_total += allocation_stats.allocation_count;
        allocation_bytes_total += allocation_stats.allocated_bytes;
        deallocation_bytes_total += allocation_stats.deallocated_bytes;
        boundary_handling_total_ms += m.boundary_micros as f64 / 1000.0;
        upper_pool_outflow += m.tracked_region_outflow_m3;
        upper_pool_inflow += m.tracked_region_inflow_m3;
        let wall = tick_start.elapsed().as_secs_f64() * 1000.0;
        costs.push(wall);
        pressure_residual_max = pressure_residual_max.max(m.pressure_residual_final_max);
        divergence_after_max = divergence_after_max.max(m.divergence_after_max_s);
        pressure_converged += u64::from(m.pressure_converged_substeps);
        pressure_total += u64::from(m.substeps);
        pressure_active_rows_total += m.pressure_active_rows_total;
        conservation_error_max = conservation_error_max.max(m.conservation_error_m3);
        max_discharge = max_discharge.max(fixture.discharge_through_dam_m3_s());
        max_momentum = max_momentum.max(fixture.eastward_momentum_kg_m_s());
        let diagnostic_start = Instant::now();
        if tick >= cfg.ticks * 2 / 3 {
            late_window_max_speed =
                late_window_max_speed.max(fixture.grid().max_liquid_speed_m_s());
            if cfg.scenario == "basin" {
                let samples = fixture.grid().liquid_speed_samples();
                let sample_map: HashMap<(i64, i64, i64), LiquidSpeedSample> = samples
                    .iter()
                    .map(|s| ((s.cell.x, s.cell.y, s.cell.z), *s))
                    .collect();
                for sample in samples.iter().filter(|s| s.fraction < 1.0e-6) {
                    for (dx, dy, dz) in [
                        (1, 0, 0),
                        (-1, 0, 0),
                        (0, 1, 0),
                        (0, -1, 0),
                        (0, 0, 1),
                        (0, 0, -1),
                    ] {
                        if let Some(neighbor) = sample_map
                            .get(&(sample.cell.x + dx, sample.cell.y + dy, sample.cell.z + dz))
                            .filter(|n| n.fraction >= 1.0e-3)
                        {
                            let volume =
                                neighbor.fraction * fixture.grid().config().cell_size_m.powi(3);
                            tiny_neighbor_samples += 1;
                            tiny_neighbor_volume += volume;
                            if neighbor.speed_m_s > 0.5 {
                                tiny_neighbor_fast_volume += volume;
                            }
                            tiny_neighbor_max_speed =
                                tiny_neighbor_max_speed.max(neighbor.speed_m_s);
                            tiny_neighbor_speed_histogram
                                [((neighbor.speed_m_s / 0.05).floor() as usize).min(399)] += volume;
                        }
                    }
                }
                if samples.iter().any(|s| s.speed_m_s > 0.5) {
                    late_high_speed_ticks += 1;
                }
                for sample in samples {
                    let volume = sample.fraction * fixture.grid().config().cell_size_m.powi(3);
                    late_speed_water_total += volume;
                    if sample.speed_m_s > 0.5 {
                        late_speed_water_over_limit += volume;
                    }
                    for (k, cut) in OCCUPANCY_CUTS.iter().enumerate() {
                        if sample.fraction >= *cut {
                            occupancy_volume[k] += volume;
                            occupancy_cells[k] += 1;
                            occupancy_histograms[k]
                                [((sample.speed_m_s / 0.05).floor() as usize).min(399)] += volume;
                            if sample.speed_m_s > 0.5 {
                                occupancy_fast_volume[k] += volume;
                            }
                        }
                    }
                    let band = if sample.fraction < 0.2 {
                        1
                    } else if sample.fraction < 0.95 {
                        2
                    } else {
                        3
                    };
                    let bucket = ((sample.speed_m_s / 0.05).floor() as usize).min(399);
                    late_speed_histograms[0][bucket] += volume;
                    late_speed_histograms[band][bucket] += volume;
                    if late_maximum_sample.is_none_or(|(_, old)| sample.speed_m_s > old.speed_m_s) {
                        late_maximum_sample = Some((tick, sample));
                    }
                }
            }
        }
        upper_pool_overtopping_peak =
            upper_pool_overtopping_peak.max(fixture.upper_pool_above_wall_volume_m3());
        let (cmin, cmax) = fixture.grid().fraction_bounds();
        let balance = initial_volume
            - fixture.grid().water_volume_m3()
            - fixture.grid().cumulative_open_outflow_m3();
        let downstream_volume = fixture.downstream_volume_m3();
        let level_difference = fixture.p95_level_difference_m();
        let basin_surface = fixture.basin_surface_p95_m();
        let discharge = fixture.discharge_through_dam_m3_s();
        let momentum = fixture.eastward_momentum_kg_m_s();
        let kinetic_energy = fixture.grid().kinetic_energy_j();
        let potential_energy = fixture.grid().gravitational_potential_energy_j();
        mechanical_energy_peak = mechanical_energy_peak.max(kinetic_energy + potential_energy);
        let max_liquid_speed = fixture.grid().max_liquid_speed_m_s();
        let upper_pool_volume = fixture.upper_pool_volume_m3();
        let lower_tunnel_volume = fixture.lower_tunnel_volume_m3();
        let tunnel_roof_speed = fixture.tunnel_roof_normal_speed_max_m_s();
        let diagnostics_ms = diagnostic_start.elapsed().as_secs_f64() * 1000.0;
        diagnostic_costs.push(diagnostics_ms);
        all_in_tick_costs.push(wall + diagnostics_ms);
        println!(
            "{{\"type\":\"grid_tick\",\"tick\":{},\"simulated_time_s\":{:.9},\"wall_ms\":{:.6},\"diagnostic_overhead_ms\":{:.6},\"allocation_time_ns\":{},\"allocation_count\":{},\"allocated_bytes_this_tick\":{},\"deallocated_bytes_this_tick\":{},\"substeps\":{},\"velocity_advection_ms\":{:.6},\"pressure_ms\":{:.6},\"transport_ms\":{:.6},\"boundary_handling_ms\":{:.6},\"boundary_update_ms\":0.0,\"pressure_iterations\":{},\"pressure_active_rows_total\":{},\"pressure_residual_initial\":{:.12e},\"pressure_residual_final\":{:.12e},\"pressure_converged\":{},\"divergence_before_max_s\":{:.12e},\"divergence_after_max_s\":{:.12e},\"water_volume_m3\":{:.12},\"permitted_outflow_this_tick_m3\":{:.12},\"cumulative_permitted_open_outflow_m3\":{:.12},\"absolute_conservation_error_m3\":{:.12e},\"relative_conservation_error\":{:.12e},\"fraction_min\":{:.12},\"fraction_max\":{:.12},\"active_cells\":{},\"downstream_volume_m3\":{:.12},\"p95_level_difference_m\":{:.9},\"basin_surface_p95_m\":{:.9},\"discharge_m3_s\":{:.9},\"downstream_momentum_kg_m_s\":{:.9},\"kinetic_energy_j\":{:.9},\"gravitational_potential_energy_j\":{:.9},\"max_liquid_speed_m_s\":{:.9},\"upper_pool_volume_m3\":{:.12},\"upper_pool_face_outflow_this_tick_m3\":{:.12},\"upper_pool_face_inflow_this_tick_m3\":{:.12},\"upper_pool_face_net_outflow_this_tick_m3\":{:.12},\"upper_pool_face_cumulative_outflow_m3\":{:.12},\"upper_pool_face_cumulative_inflow_m3\":{:.12},\"lower_tunnel_volume_m3\":{:.12},\"tunnel_roof_normal_speed_max_m_s\":{:.9}}}",
            tick,
            (tick + 1) as f64 * cfg.dt,
            wall,
            diagnostics_ms,
            allocation_stats.allocator_time_ns,
            allocation_stats.allocation_count,
            allocation_stats.allocated_bytes,
            allocation_stats.deallocated_bytes,
            m.substeps,
            m.velocity_advection_micros as f64 / 1000.0,
            m.pressure_solve_micros as f64 / 1000.0,
            m.transport_micros as f64 / 1000.0,
            m.boundary_micros as f64 / 1000.0,
            m.pressure_iterations,
            m.pressure_active_rows_total,
            m.pressure_residual_initial_max,
            m.pressure_residual_final_max,
            m.pressure_converged_substeps,
            m.divergence_before_max_s,
            m.divergence_after_max_s,
            fixture.grid().water_volume_m3(),
            m.permitted_outflow_m3,
            fixture.grid().cumulative_open_outflow_m3(),
            balance.abs(),
            balance.abs() / initial_volume.max(f64::MIN_POSITIVE),
            cmin,
            cmax,
            m.active_cells,
            downstream_volume,
            level_difference,
            basin_surface,
            discharge,
            momentum,
            kinetic_energy,
            potential_energy,
            max_liquid_speed,
            upper_pool_volume,
            m.tracked_region_outflow_m3,
            m.tracked_region_inflow_m3,
            m.tracked_region_net_outflow_m3,
            upper_pool_outflow,
            upper_pool_inflow,
            lower_tunnel_volume,
            tunnel_roof_speed,
        );
        if cfg.fraction_diagnostics {
            let bands = fixture.grid().fraction_band_diagnostics();
            let mut speed_histograms = vec![vec![0.0; 400]; bands.len()];
            let mut fast_water_volume = vec![0.0; bands.len()];
            let cell_volume = fixture.grid().config().cell_size_m.powi(3);
            for sample in fixture.grid().liquid_speed_samples() {
                let band = fraction_band_index(sample.fraction);
                let water_volume = sample.fraction * cell_volume;
                let speed_bin = ((sample.speed_m_s / 0.05).floor() as usize).min(399);
                speed_histograms[band][speed_bin] += water_volume;
                if sample.speed_m_s > 0.5 {
                    fast_water_volume[band] += water_volume;
                }
            }
            let components = fixture
                .grid()
                .pressure_component_counts_at_fraction_thresholds();
            let overall_speed_histogram = (0..400)
                .map(|bin| speed_histograms.iter().map(|h| h[bin]).sum::<f64>())
                .collect::<Vec<_>>();
            let overall_water_volume = bands.iter().map(|b| b.water_volume_m3).sum::<f64>();
            let overall_fast_water_volume = fast_water_volume.iter().sum::<f64>();
            let (maximum_band_speed, maximum_band_speed_fraction) = bands
                .iter()
                .max_by(|a, b| {
                    a.maximum_cell_speed_m_s
                        .total_cmp(&b.maximum_cell_speed_m_s)
                })
                .map(|b| (b.maximum_cell_speed_m_s, b.maximum_cell_speed_fraction))
                .unwrap_or((0.0, 0.0));
            let bands_json = bands
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    format!(
                        "{{\"band\":\"{}\",\"cells\":{},\"pressure_active_cells\":{},\"water_volume_m3\":{:.12e},\"maximum_cell_speed_m_s\":{:.12e},\"maximum_cell_speed_fraction\":{:.12e},\"volume_weighted_p95_speed_m_s\":{:.9},\"water_volume_share_over_0_5_m_s\":{:.12e},\"maximum_face_outflow_l1_m_s\":{:.12e}}}",
                        b.label,
                        b.cells,
                        b.pressure_active_cells,
                        b.water_volume_m3,
                        b.maximum_cell_speed_m_s,
                        b.maximum_cell_speed_fraction,
                        weighted_histogram_percentile(&speed_histograms[i], 0.95),
                        fast_water_volume[i] / b.water_volume_m3.max(f64::MIN_POSITIVE),
                        b.maximum_face_outflow_l1_m_s,
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let occupancy_cuts = [0.0, 1.0e-8, 1.0e-6, 1.0e-4, 1.0e-3, 1.0e-2, 1.0e-1];
            let filtered_substeps = occupancy_cuts
                .iter()
                .map(|&cut| {
                    let first_band = fraction_band_index(cut);
                    let speed = bands[first_band..]
                        .iter()
                        .map(|band| band.maximum_face_outflow_l1_m_s)
                        .fold(0.0, f64::max);
                    let advective_dt = if speed > 0.0 {
                        fixture.grid().config().cfl_limit * fixture.grid().config().cell_size_m
                            / speed
                    } else {
                        f64::INFINITY
                    };
                    let gravity = fixture
                        .grid()
                        .config()
                        .gravity_m_s2
                        .iter()
                        .map(|g| g.abs())
                        .fold(0.0, f64::max);
                    let gravity_dt = if gravity > 0.0 {
                        (2.0 * fixture.grid().config().cfl_limit
                            * fixture.grid().config().cell_size_m
                            / gravity)
                            .sqrt()
                    } else {
                        f64::INFINITY
                    };
                    (cfg.dt / advective_dt.min(gravity_dt)).ceil().max(1.0) as u32
                })
                .collect::<Vec<_>>();
            let components_json = components
                .iter()
                .map(|(threshold, count)| format!("[{},{}]", threshold, count))
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "{{\"type\":\"fraction_distribution\",\"tick\":{},\"bands\":[{}],\"volume_weighted_p95_speed_m_s\":{:.9},\"water_volume_share_over_0_5_m_s\":{:.12e},\"maximum_cell_speed_m_s\":{:.12e},\"maximum_cell_speed_fraction\":{:.12e},\"pressure_components_at_minimum_C_thresholds\":[{}],\"diagnostic_occupancy_cuts_C\":{:?},\"filtered_required_substeps_diagnostic_only\":{:?},\"threshold_policy\":\"diagnostic-only exclusion; actual solver uses every C > 0\"}}",
                tick,
                bands_json,
                weighted_histogram_percentile(&overall_speed_histogram, 0.95),
                overall_fast_water_volume / overall_water_volume.max(f64::MIN_POSITIVE),
                maximum_band_speed,
                maximum_band_speed_fraction,
                components_json,
                occupancy_cuts,
                filtered_substeps
            );
        }
    }
    costs.sort_by(f64::total_cmp);
    all_in_tick_costs.sort_by(f64::total_cmp);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let p50 = percentile(&costs, 0.50);
    let p95 = percentile(&costs, 0.95);
    let all_in_p50 = percentile(&all_in_tick_costs, 0.50);
    let all_in_p95 = percentile(&all_in_tick_costs, 0.95);
    let normalization = cost_normalization_to_60hz(cfg.dt);
    let solver_cost_per_60hz_second = costs.iter().sum::<f64>() / cfg.ticks as f64 * normalization;
    let all_in_cost_per_60hz_second =
        (costs.iter().sum::<f64>() + diagnostic_costs.iter().sum::<f64>()) / cfg.ticks as f64
            * normalization;
    let (fraction_min, fraction_max) = fixture.grid().fraction_bounds();
    if let Some((max_tick, max_sample)) = late_maximum_sample {
        let h = fixture.grid().config().cell_size_m;
        let max_cell = max_sample.cell;
        let max_location = if max_sample.wall_adjacent {
            "wall_adjacent"
        } else if max_sample.fraction < 0.999 {
            "free_surface_or_interface"
        } else {
            "interior"
        };
        println!(
            "{{\"type\":\"basin_speed_diagnostics\",\"sample_window_ticks\":[{},{}],\"samples_per_water_cell_per_tick\":true,\"percentile_bin_width_m_s\":0.05,\"late_ticks_with_speed_over_0_5_m_s\":{},\"late_tick_count\":{},\"maximum_speed_tick\":{},\"maximum_speed_m_s\":{:.9},\"maximum_speed_cell\":[{},{},{}],\"maximum_speed_position_m\":[{:.6},{:.6},{:.6}],\"maximum_speed_fraction\":{:.12e},\"maximum_speed_class\":\"{}\",\"volume_weighted_p50_m_s\":{:.9},\"volume_weighted_p95_m_s\":{:.9},\"water_volume_share_over_0_5_m_s\":{:.12e},\"occupancy_thresholds_C\":{:?},\"occupancy_cell_samples\":{:?},\"occupancy_water_volume_m3\":{:?},\"occupancy_p95_speed_m_s\":{:?},\"occupancy_volume_share_over_0_5_m_s\":{:?},\"tiny_cell_adjacent_eligible_neighbor_samples\":{},\"tiny_cell_adjacent_eligible_neighbor_weighted_p95_m_s\":{:.9},\"tiny_cell_adjacent_eligible_neighbor_max_m_s\":{:.9},\"tiny_cell_adjacent_eligible_neighbor_volume_share_over_0_5_m_s\":{:.12e},\"near_empty_fraction_band\":\"0 < C < 0.2\",\"near_empty_p95_m_s\":{:.9},\"interface_fraction_band\":\"0.2 <= C < 0.95\",\"interface_p95_m_s\":{:.9},\"bulk_fraction_band\":\"C >= 0.95\",\"bulk_p95_m_s\":{:.9},\"maximum_wall_adjacent\":{}}}",
            cfg.ticks * 2 / 3,
            cfg.ticks,
            late_high_speed_ticks,
            cfg.ticks - cfg.ticks * 2 / 3,
            max_tick,
            max_sample.speed_m_s,
            max_cell.x,
            max_cell.y,
            max_cell.z,
            (max_cell.x as f64 + 0.5) * h,
            (max_cell.y as f64 + 0.5) * h,
            (max_cell.z as f64 + 0.5) * h,
            max_sample.fraction,
            max_location,
            weighted_histogram_percentile(&late_speed_histograms[0], 0.50),
            weighted_histogram_percentile(&late_speed_histograms[0], 0.95),
            late_speed_water_over_limit / late_speed_water_total.max(f64::MIN_POSITIVE),
            OCCUPANCY_CUTS,
            occupancy_cells,
            occupancy_volume,
            std::array::from_fn::<_, 5, _>(|i| weighted_histogram_percentile(
                &occupancy_histograms[i],
                0.95
            )),
            std::array::from_fn::<_, 5, _>(
                |i| occupancy_fast_volume[i] / occupancy_volume[i].max(f64::MIN_POSITIVE)
            ),
            tiny_neighbor_samples,
            weighted_histogram_percentile(&tiny_neighbor_speed_histogram, 0.95),
            tiny_neighbor_max_speed,
            tiny_neighbor_fast_volume / tiny_neighbor_volume.max(f64::MIN_POSITIVE),
            weighted_histogram_percentile(&late_speed_histograms[1], 0.95),
            weighted_histogram_percentile(&late_speed_histograms[2], 0.95),
            weighted_histogram_percentile(&late_speed_histograms[3], 0.95),
            max_sample.wall_adjacent
        );
    }
    let relative_conservation_error_end = (initial_volume
        - fixture.grid().water_volume_m3()
        - fixture.grid().cumulative_open_outflow_m3())
    .abs()
        / initial_volume.max(f64::MIN_POSITIVE);
    println!(
        "{{\"type\":\"grid_summary\",\"ticks\":{},\"simulated_seconds\":{:.9},\"setup_ms\":{:.6},\"wall_elapsed_ms\":{:.3},\"fluid_update_p50_ms\":{:.6},\"fluid_update_p95_ms\":{:.6},\"all_in_tick_p50_ms\":{:.6},\"all_in_tick_p95_ms\":{:.6},\"cost_per_1_60_simulated_second_ms\":{:.6},\"solver_cost_per_1_60_simulated_second_ms\":{:.6},\"diagnostic_overhead_p50_ms\":{:.6},\"diagnostic_overhead_p95_ms\":{:.6},\"boundary_handling_total_ms\":{:.6},\"allocation_diagnostics_time_ns\":{},\"allocation_count\":{},\"allocated_bytes_total\":{},\"deallocated_bytes_total\":{},\"solver_allocated_bytes\":{},\"process_working_set_bytes\":null,\"initial_water_volume_m3\":{:.12},\"retained_water_volume_m3\":{:.12},\"cumulative_permitted_outflow_m3\":{:.12},\"absolute_conservation_error_max_m3\":{:.12e},\"relative_conservation_error_end\":{:.12e},\"active_cells_end\":{},\"pressure_active_rows_total\":{},\"fraction_bounds_end\":[{:.12},{:.12}],\"pressure_substeps_converged\":{},\"pressure_substeps_total\":{},\"pressure_residual_max\":{:.12e},\"divergence_after_max_s\":{:.12e},\"downstream_volume_gain_m3\":{:.12},\"level_difference_initial_m\":{:.9},\"level_difference_end_m\":{:.9},\"maximum_discharge_m3_s\":{:.9},\"maximum_downstream_momentum_kg_m_s\":{:.9},\"maximum_liquid_speed_m_s\":{:.9},\"late_window_max_liquid_speed_m_s\":{:.9},\"basin_surface_initial_p95_m\":{:.9},\"basin_surface_end_p95_m\":{:.9},\"basin_surface_drift_m\":{:.9},\"kinetic_energy_initial_j\":{:.9},\"kinetic_energy_end_j\":{:.9},\"kinetic_energy_change_j\":{:.9},\"potential_energy_initial_j\":{:.9},\"potential_energy_end_j\":{:.9},\"total_mechanical_energy_initial_j\":{:.9},\"total_mechanical_energy_end_j\":{:.9},\"upper_pool_volume_initial_m3\":{:.12},\"upper_pool_volume_end_m3\":{:.12},\"upper_pool_volume_change_m3\":{:.12},\"upper_pool_face_cumulative_outflow_m3\":{:.12},\"upper_pool_face_cumulative_inflow_m3\":{:.12},\"upper_pool_flux_balance_error_m3\":{:.12e},\"upper_pool_above_wall_peak_m3\":{:.12},\"lower_tunnel_volume_initial_m3\":{:.12},\"lower_tunnel_volume_end_m3\":{:.12},\"tunnel_roof_normal_speed_max_m_s\":{:.9},\"boundary_update_ms_last\":{:.6},\"timing_includes_all_substeps\":true}}",
        cfg.ticks,
        cfg.ticks as f64 * cfg.dt,
        setup_ms,
        elapsed_ms,
        p50,
        p95,
        all_in_p50,
        all_in_p95,
        all_in_cost_per_60hz_second,
        solver_cost_per_60hz_second,
        percentile(&diagnostic_costs, 0.50),
        percentile(&diagnostic_costs, 0.95),
        boundary_handling_total_ms,
        allocation_trace_ns_total,
        allocation_count_total,
        allocation_bytes_total,
        deallocation_bytes_total,
        fixture.grid().allocated_bytes(),
        initial_volume,
        fixture.grid().water_volume_m3(),
        fixture.grid().cumulative_open_outflow_m3(),
        conservation_error_max,
        relative_conservation_error_end,
        fixture.grid().active_cells(),
        pressure_active_rows_total,
        fraction_min,
        fraction_max,
        pressure_converged,
        pressure_total,
        pressure_residual_max,
        divergence_after_max,
        fixture.downstream_volume_m3() - initial_downstream,
        initial_level_difference,
        fixture.p95_level_difference_m(),
        max_discharge,
        max_momentum,
        fixture.grid().max_liquid_speed_m_s(),
        late_window_max_speed,
        initial_basin_surface,
        fixture.basin_surface_p95_m(),
        (fixture.basin_surface_p95_m() - initial_basin_surface).abs(),
        initial_kinetic_energy,
        fixture.grid().kinetic_energy_j(),
        fixture.grid().kinetic_energy_j() - initial_kinetic_energy,
        initial_potential_energy,
        fixture.grid().gravitational_potential_energy_j(),
        initial_kinetic_energy + initial_potential_energy,
        fixture.grid().kinetic_energy_j() + fixture.grid().gravitational_potential_energy_j(),
        initial_upper_pool,
        fixture.upper_pool_volume_m3(),
        fixture.upper_pool_volume_m3() - initial_upper_pool,
        upper_pool_outflow,
        upper_pool_inflow,
        fixture.upper_pool_volume_m3() - initial_upper_pool + upper_pool_outflow
            - upper_pool_inflow,
        upper_pool_overtopping_peak,
        initial_lower_tunnel,
        fixture.lower_tunnel_volume_m3(),
        fixture.tunnel_roof_normal_speed_max_m_s(),
        fixture.last_boundary_update_micros() as f64 / 1000.0,
    );
    let late_eligible_p95 = weighted_histogram_percentile(&occupancy_histograms[3], 0.95);
    let late_fast_share = occupancy_fast_volume[3] / occupancy_volume[3].max(f64::MIN_POSITIVE);
    let surface_drift = (fixture.basin_surface_p95_m() - initial_basin_surface).abs();
    let kinetic_energy_change = fixture.grid().kinetic_energy_j() - initial_kinetic_energy;
    let mechanical_energy_peak_rise =
        mechanical_energy_peak - (initial_kinetic_energy + initial_potential_energy);
    // The level `equilibrium` case must stay at rest. The historical `basin`
    // seeds 0.6 m of water beside dry strips, so it is a small dam break:
    // released potential energy legitimately becomes sloshing kinetic energy.
    // There the physical requirement is that total energy never increases.
    let energy_gate = if cfg.scenario == "equilibrium" {
        EnergyGate::AtRest {
            kinetic_energy_change_j: kinetic_energy_change,
        }
    } else {
        EnergyGate::NonIncreasing {
            mechanical_energy_peak_rise_j: mechanical_energy_peak_rise,
        }
    };
    let outflow = fixture.grid().cumulative_open_outflow_m3();
    let mut acceptance_failed = false;
    if cfg.acceptance {
        let gates = physical_basin_gates(
            conservation_error_max,
            outflow,
            surface_drift,
            (late_eligible_p95, late_fast_share),
            energy_gate,
            (pressure_converged, pressure_total),
            (fraction_min, fraction_max),
        );
        let physical_pass = gates.iter().all(|(_, passed)| *passed);
        println!(
            "{{\"type\":\"grid_acceptance\",\"version\":2,\"acceptance_kind\":\"physical_basin\",\"result\":\"{}\",\"scenario\":\"{}\",\"energy_gate\":\"{}\",\"scale\":{},\"refinement\":{},\"ticks\":{},\"simulated_seconds\":{:.9},\"thresholds\":{{\"max_absolute_conservation_error_m3\":1e-8,\"max_permitted_top_outflow_m3\":1e-9,\"max_surface_p95_drift_m_exclusive\":0.15,\"max_C_ge_1e-3_weighted_p95_speed_m_s\":0.5,\"max_C_ge_1e-3_volume_share_above_0_5_m_s\":0.01,\"max_energy_gate_j\":1.0}},\"measurements\":{{\"absolute_conservation_error_max_m3\":{:.12e},\"relative_conservation_error_end\":{:.12e},\"cumulative_permitted_top_outflow_m3\":{:.12},\"surface_p95_initial_m\":{:.9},\"surface_p95_end_m\":{:.9},\"surface_p95_drift_m\":{:.9},\"C_ge_1e-3_late_weighted_p95_speed_m_s\":{:.9},\"C_ge_1e-3_late_water_volume_share_over_0_5_m_s\":{:.12e},\"kinetic_energy_initial_j\":{:.9},\"kinetic_energy_end_j\":{:.9},\"kinetic_energy_change_j\":{:.9},\"mechanical_energy_initial_j\":{:.9},\"mechanical_energy_peak_rise_j\":{:.9},\"mechanical_energy_end_change_j\":{:.9},\"pressure_converged_substeps\":{},\"pressure_total_substeps\":{},\"fraction_bounds_end\":[{:.12},{:.12}],\"all_in_cost_per_1_60_simulated_second_ms\":{:.6}}},\"gates\":{{{}}},\"performance_target\":{{\"target_ms_per_1_60_s\":2.0,\"measured_ms_per_1_60_s\":{:.6},\"met\":{}}}}}",
            if physical_pass { "passed" } else { "failed" },
            cfg.scenario,
            energy_gate.name(),
            cfg.scale,
            cfg.refinement,
            cfg.ticks,
            cfg.ticks as f64 * cfg.dt,
            conservation_error_max,
            relative_conservation_error_end,
            outflow,
            initial_basin_surface,
            fixture.basin_surface_p95_m(),
            surface_drift,
            late_eligible_p95,
            late_fast_share,
            initial_kinetic_energy,
            fixture.grid().kinetic_energy_j(),
            kinetic_energy_change,
            initial_kinetic_energy + initial_potential_energy,
            mechanical_energy_peak_rise,
            fixture.grid().kinetic_energy_j() + fixture.grid().gravitational_potential_energy_j()
                - initial_kinetic_energy
                - initial_potential_energy,
            pressure_converged,
            pressure_total,
            fraction_min,
            fraction_max,
            all_in_cost_per_60hz_second,
            gates
                .iter()
                .map(|(name, passed)| format!("\"{name}\":{}", passed))
                .collect::<Vec<_>>()
                .join(","),
            all_in_cost_per_60hz_second,
            all_in_cost_per_60hz_second <= 2.0,
        );
        acceptance_failed = !physical_pass;
    }
    if acceptance_failed {
        return Err("MAC_PHYSICAL_ACCEPTANCE_FAILED".into());
    }
    Ok(())
}

/// Linear standing-wave benchmark (see `StandingWaveFixture`). Measured
/// decay per period isolates numerical wave damping.
fn run_standing_wave(cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let density = cfg
        .ambient_density
        .ok_or("standing-wave requires --ambient-density")?;
    let mut wave = StandingWaveFixture::new(cfg.refinement, density)?;
    wave.grid_mut()
        .set_pressure_preconditioner(match cfg.preconditioner.as_str() {
            "jacobi" => PressurePreconditioner::Jacobi,
            "ic0" => PressurePreconditioner::Ic0,
            "mic0" => PressurePreconditioner::Mic0,
            "mg" => PressurePreconditioner::Multigrid,
            _ => return Err("unknown preconditioner".into()),
        });
    let period = wave.analytic_period_s();
    let initial_volume = wave.grid().water_volume_m3();
    let mut samples = vec![(0.0, wave.end_elevation_m())];
    let mut conservation_error_max: f64 = 0.0;
    let mut elapsed_ms = 0.0;
    for tick in 0..cfg.ticks {
        let start = Instant::now();
        let m = wave.grid_mut().step(cfg.dt)?;
        elapsed_ms += start.elapsed().as_secs_f64() * 1000.0;
        conservation_error_max = conservation_error_max.max(m.conservation_error_m3);
        let t = (tick + 1) as f64 * cfg.dt;
        let eta = wave.end_elevation_m();
        samples.push((t, eta));
        println!(
            "{{\"type\":\"standing_wave_tick\",\"t\":{t:.9},\"end_elevation_m\":{eta:.9},\"kinetic_energy_j\":{:.9},\"water_volume_m3\":{:.12}}}",
            wave.grid().kinetic_energy_j(),
            wave.grid().water_volume_m3()
        );
    }
    let peaks = oscillation_peaks(&samples, 0.1 * StandingWaveFixture::AMPLITUDE_M);
    let measured_period = zero_crossing_period(&samples).unwrap_or(f64::NAN);
    let ratio = amplitude_ratio_per_period(&peaks, period).unwrap_or(f64::NAN);
    println!(
        "{{\"type\":\"standing_wave_summary\",\"refinement\":{},\"cell_size_m\":{},\"dt_s\":{:.12},\"ticks\":{},\"amplitude_initial_m\":{:.9},\"analytic_period_s\":{period:.6},\"measured_period_s\":{},\"amplitude_ratio_per_period\":{},\"peak_count\":{},\"peaks\":{:?},\"water_volume_change_m3\":{:.3e},\"conservation_error_max_m3\":{:.3e},\"ms_per_tick\":{:.4}}}",
        cfg.refinement,
        wave.grid().config().cell_size_m,
        cfg.dt,
        cfg.ticks,
        samples[0].1,
        json_number(measured_period),
        json_number(ratio),
        peaks.len(),
        peaks
            .iter()
            .map(|(t, a)| [(t * 1000.0).round() / 1000.0, (a * 1e5).round() / 1e5])
            .collect::<Vec<_>>(),
        wave.grid().water_volume_m3() - initial_volume,
        conservation_error_max,
        elapsed_ms / cfg.ticks as f64
    );
    Ok(())
}

fn json_number(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.6}")
    } else {
        "null".into()
    }
}

fn run_hydrostatic_sensitivity() -> Result<(), Box<dyn std::error::Error>> {
    let make_column = |cells: u32, h: f64| -> Result<MacGridWorld, Box<dyn std::error::Error>> {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [1, cells, 1], 16)?;
        let mut volume = Volume::new(
            VolumeId::new(990 + u64::from(cells)).unwrap(),
            CellSizeCode::Quarter,
        );
        volume.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )?;
        let boundary = SolidBoundary::capture(&volume, spec)?;
        let config = MacConfig {
            cell_size_m: h,
            ..MacConfig::default()
        };
        let mut grid = MacGridWorld::new(&boundary, config)?;
        for y in 0..cells {
            grid.set_fraction(GlobalCell::new(0, y as i64, 0), 1.0)?;
        }
        Ok(grid)
    };

    let mut coarse = make_column(2, 0.25)?;
    let mut fine = make_column(4, 0.125)?;
    let coarse_metrics = coarse.step(1.0 / 120.0)?;
    let fine_metrics = fine.step(1.0 / 120.0)?;
    for (name, cells, grid, metrics) in [
        ("coarse", 2, coarse, coarse_metrics),
        ("fine", 4, fine, fine_metrics),
    ] {
        println!(
            "{{\"type\":\"hydrostatic_resolution\",\"resolution\":\"{name}\",\"cell_size_m\":{},\"cells_y\":{cells},\"physical_height_m\":0.5,\"dt_s\":0.0083333333333333,\"pressure_iterations\":{},\"pressure_residual\":{:.12e},\"divergence_after_max_s\":{:.12e},\"mean_liquid_pressure_pa\":{:.9},\"max_liquid_speed_m_s\":{:.12e},\"water_volume_m3\":{:.12},\"conservation_error_m3\":{:.12e}}}",
            grid.config().cell_size_m,
            metrics.pressure_iterations,
            metrics.pressure_residual_final_max,
            metrics.divergence_after_max_s,
            grid.mean_liquid_pressure_pa(),
            grid.max_liquid_speed_m_s(),
            grid.water_volume_m3(),
            metrics.conservation_error_m3
        );
    }

    let mut full = make_column(2, 0.25)?;
    let mut split = make_column(2, 0.25)?;
    let full_metrics = full.step(1.0 / 60.0)?;
    let half_a = split.step(1.0 / 120.0)?;
    let half_b = split.step(1.0 / 120.0)?;
    println!(
        "{{\"type\":\"hydrostatic_timestep\",\"cell_size_m\":0.25,\"simulated_duration_s\":0.0166666666666667,\"full_dt_s\":0.0166666666666667,\"half_dt_s\":0.0083333333333333,\"full_mean_liquid_pressure_pa\":{:.9},\"two_half_steps_mean_liquid_pressure_pa\":{:.9},\"full_max_liquid_speed_m_s\":{:.12e},\"two_half_steps_max_liquid_speed_m_s\":{:.12e},\"full_conservation_error_m3\":{:.12e},\"half_steps_conservation_error_m3\":{:.12e}}}",
        full.mean_liquid_pressure_pa(),
        split.mean_liquid_pressure_pa(),
        full.max_liquid_speed_m_s(),
        split.max_liquid_speed_m_s(),
        full_metrics.conservation_error_m3,
        half_a.conservation_error_m3 + half_b.conservation_error_m3
    );
    Ok(())
}

fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values[((values.len() - 1) as f64 * q).ceil() as usize].min(*values.last().unwrap())
    }
}

fn weighted_histogram_percentile(histogram: &[f64], q: f64) -> f64 {
    let total: f64 = histogram.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let target = q.clamp(0.0, 1.0) * total;
    let mut cumulative = 0.0;
    for (bin, weight) in histogram.iter().enumerate() {
        cumulative += weight;
        if cumulative >= target {
            return (bin as f64 + 0.5) * 0.05;
        }
    }
    (histogram.len() as f64 - 0.5) * 0.05
}

/// Energy criterion for the 30-second basin acceptance, chosen by whether the
/// initial state is a discrete equilibrium. Both use a 1 J threshold.
#[derive(Debug, Clone, Copy)]
enum EnergyGate {
    /// Level water at rest: kinetic energy must not grow.
    AtRest { kinetic_energy_change_j: f64 },
    /// Out-of-equilibrium start: peak KE+PE over the run must not exceed its
    /// initial value (inviscid flow may only conserve or dissipate energy).
    NonIncreasing { mechanical_energy_peak_rise_j: f64 },
}

impl EnergyGate {
    fn name(self) -> &'static str {
        match self {
            Self::AtRest { .. } => "kinetic_energy_growth",
            Self::NonIncreasing { .. } => "mechanical_energy_non_increasing",
        }
    }

    fn passed(self) -> bool {
        match self {
            Self::AtRest {
                kinetic_energy_change_j,
            } => kinetic_energy_change_j <= 1.0,
            Self::NonIncreasing {
                mechanical_energy_peak_rise_j,
            } => mechanical_energy_peak_rise_j <= 1.0,
        }
    }
}

fn physical_basin_gates(
    conservation_error_max_m3: f64,
    permitted_outflow_m3: f64,
    surface_drift_m: f64,
    (eligible_weighted_p95_speed_m_s, eligible_volume_share_over_limit): (f64, f64),
    energy_gate: EnergyGate,
    (pressure_converged_substeps, pressure_total_substeps): (u64, u64),
    (fraction_min, fraction_max): (f64, f64),
) -> [(&'static str, bool); 8] {
    [
        ("conservation", conservation_error_max_m3 <= 1.0e-8),
        ("permitted_top_outflow", permitted_outflow_m3 <= 1.0e-9),
        ("surface_drift", surface_drift_m < 0.15),
        (
            "eligible_water_weighted_p95_speed",
            eligible_weighted_p95_speed_m_s <= 0.5,
        ),
        (
            "eligible_water_share_over_0_5_m_s",
            eligible_volume_share_over_limit <= 0.01,
        ),
        (energy_gate.name(), energy_gate.passed()),
        (
            "pressure_convergence",
            pressure_converged_substeps == pressure_total_substeps,
        ),
        (
            "fraction_bounds",
            fraction_min >= -1.0e-10 && fraction_max <= 1.0 + 1.0e-10,
        ),
    ]
}

fn fraction_band_index(fraction: f64) -> usize {
    if fraction <= 0.0 {
        0
    } else if fraction < 1.0e-8 {
        1
    } else if fraction < 1.0e-6 {
        2
    } else if fraction < 1.0e-4 {
        3
    } else if fraction < 1.0e-3 {
        4
    } else if fraction < 1.0e-2 {
        5
    } else if fraction < 1.0e-1 {
        6
    } else {
        7
    }
}

fn cost_normalization_to_60hz(dt_s: f64) -> f64 {
    (1.0 / 60.0) / dt_s
}

struct Config {
    scale: u32,
    refinement: u32,
    ticks: usize,
    dt: f64,
    scenario: String,
    pressure_tolerance: f64,
    pressure_diagnostics: bool,
    fraction_diagnostics: bool,
    stage_diagnostics: bool,
    allocation_diagnostics: bool,
    preconditioner: String,
    acceptance: bool,
    ambient_density: Option<f64>,
    incompressible_air: bool,
}

impl Config {
    fn parse() -> Result<Self, Box<dyn std::error::Error>> {
        let (mut scale, mut refinement, mut ticks, mut dt, mut scenario, mut pressure_tolerance): (
            u32,
            u32,
            usize,
            f64,
            String,
            f64,
        ) = (1, 1, 120, 1.0 / 60.0, String::from("sealed"), 1.0e-8);
        let mut pressure_diagnostics = false;
        let mut fraction_diagnostics = false;
        let mut stage_diagnostics = false;
        let mut allocation_diagnostics = false;
        let mut acceptance = false;
        let mut ambient_density = None;
        let mut incompressible_air = false;
        let mut preconditioner = String::from("jacobi");
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--pressure-diagnostics" {
                pressure_diagnostics = true;
                continue;
            }
            if arg == "--fraction-diagnostics" {
                fraction_diagnostics = true;
                continue;
            }
            if arg == "--stage-diagnostics" {
                stage_diagnostics = true;
                continue;
            }
            if arg == "--allocation-diagnostics" {
                allocation_diagnostics = true;
                continue;
            }
            if arg == "--incompressible-air" {
                incompressible_air = true;
                continue;
            }
            if arg == "--acceptance" {
                acceptance = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value after {arg}"))?;
            match arg.as_str() {
                "--scale" => scale = value.parse()?,
                "--refinement" => refinement = value.parse()?,
                "--ticks" => ticks = value.parse()?,
                "--dt" => dt = value.parse()?,
                "--scenario" => scenario = value,
                "--pressure-tolerance" => pressure_tolerance = value.parse()?,
                "--preconditioner" => preconditioner = value,
                "--ambient-density" => ambient_density = Some(value.parse()?),
                _ => return Err(format!("unknown option {arg}").into()),
            }
        }
        if scale == 0
            || refinement == 0
            || ticks == 0
            || !dt.is_finite()
            || dt <= 0.0
            || !pressure_tolerance.is_finite()
            || pressure_tolerance <= 0.0
        {
            return Err("scale/ticks/dt must be positive and finite".into());
        }
        Ok(Self {
            scale,
            refinement,
            ticks,
            dt,
            scenario,
            pressure_tolerance,
            pressure_diagnostics,
            fraction_diagnostics,
            stage_diagnostics,
            allocation_diagnostics,
            preconditioner,
            acceptance,
            ambient_density,
            incompressible_air,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{EnergyGate, cost_normalization_to_60hz, physical_basin_gates};

    #[test]
    fn fixed_timestep_cost_normalization_counts_outer_steps_per_60hz_interval() {
        assert!((cost_normalization_to_60hz(1.0 / 60.0) - 1.0).abs() < 1.0e-12);
        assert!((cost_normalization_to_60hz(1.0 / 120.0) - 2.0).abs() < 1.0e-12);
        assert!((cost_normalization_to_60hz(1.0 / 30.0) - 0.5).abs() < 1.0e-12);
    }

    #[test]
    fn physical_basin_acceptance_rejects_open_top_ejection() {
        let passing = physical_basin_gates(
            1.0e-12,
            0.0,
            0.01,
            (0.1, 0.0),
            EnergyGate::AtRest {
                kinetic_energy_change_j: 0.1,
            },
            (1800, 1800),
            (0.0, 1.0),
        );
        assert!(passing.iter().all(|(_, pass)| *pass));

        let ejected = physical_basin_gates(
            1.0e-12,
            0.309,
            0.01,
            (0.1, 0.0),
            EnergyGate::AtRest {
                kinetic_energy_change_j: 0.1,
            },
            (1800, 1800),
            (0.0, 1.0),
        );
        assert_eq!(ejected[1], ("permitted_top_outflow", false));
        assert!(!ejected.iter().all(|(_, pass)| *pass));
    }

    #[test]
    fn energy_gate_allows_released_sloshing_but_rejects_energy_gain() {
        let gate = |energy_gate| {
            physical_basin_gates(
                1.0e-12,
                0.0,
                0.1,
                (0.3, 0.005),
                energy_gate,
                (1800, 1800),
                (0.0, 1.0),
            )
        };
        // Historical basin: 36.8 J of sloshing from ~1.16 kJ released PE.
        let sloshing = gate(EnergyGate::NonIncreasing {
            mechanical_energy_peak_rise_j: 0.0,
        });
        assert_eq!(sloshing[5], ("mechanical_energy_non_increasing", true));
        // Legacy single-phase basin gained ~15 kJ climbing out of the basin.
        let gaining = gate(EnergyGate::NonIncreasing {
            mechanical_energy_peak_rise_j: 15_087.0,
        });
        assert_eq!(gaining[5], ("mechanical_energy_non_increasing", false));
        // Level water must stay at rest.
        let at_rest = gate(EnergyGate::AtRest {
            kinetic_energy_change_j: 36.8,
        });
        assert_eq!(at_rest[5], ("kinetic_energy_growth", false));
    }
}
