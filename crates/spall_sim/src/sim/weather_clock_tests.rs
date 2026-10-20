use super::*;
#[test]
fn no_vegetation_world_uses_tick_clock_and_reset_starts_new_epoch() {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    assert_eq!(
        sim.weather_state().clock_source,
        spall_protocol::WeatherClockSource::ServerFallback
    );
    sim.tick().unwrap();
    assert_eq!(sim.weather_state().clock_ms, 16);
    assert_eq!(sim.weather_state().effective_tick, 1);
    let prior_epoch = sim.weather_state().world_epoch;
    let fresh =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.replace_world(fresh).unwrap();
    assert_eq!(sim.weather_state().world_epoch, prior_epoch + 1);
    assert_eq!(sim.weather_state().clock_ms, 0);
    sim.wait_for_retired();
}

#[test]
fn weather_override_survives_ticks_and_reset_clears_it() {
    let mut sim =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.set_weather_override(Some(spall_protocol::WeatherKind::Snow));
    assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
    assert_eq!(
        sim.weather_state().override_kind,
        Some(spall_protocol::WeatherKind::Snow)
    );
    sim.tick().unwrap();
    assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
    let fresh =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    sim.replace_world(fresh).unwrap();
    assert_eq!(sim.weather_state().override_kind, None);
    sim.wait_for_retired();
}
