//! Warmth and wetness for every player (ENG-264, ENG-265): the model is
//! [`crate::warmth`]; this gathers each player's exposure from the world and
//! applies cold damage through [`Simulation::damage_player`].

use super::*;
use crate::warmth::{self, Exposure};

impl Simulation {
    /// Advances every player's [`warmth::BodyHeat`] by `dt_s` seconds and
    /// applies the cold damage due. Runs on the owning thread at the end of a
    /// tick, every [`warmth::BODY_HEAT_PERIOD_TICKS`].
    pub(super) fn advance_body_heat(&mut self, dt_s: f32) {
        if self.world.player_count() == 0 {
            return;
        }
        let air_c = warmth::air_temperature_c(&self.weather, self.tick.get());
        let wind_chill_c = warmth::wind_chill_c(&self.weather);
        let rain = warmth::rain(&self.weather);
        let frame = self.column_water.as_ref().map(|w| w.frame());
        let sampler = frame.as_ref().map(|f| f.sampler());
        let volume = &self.world.terrain().volume;
        let exposures: Vec<(EntityId, Exposure)> = self
            .world
            .players()
            .map(|player| {
                let [x, y, z] = player.state.position_m;
                let chest = [x, y + warmth::CHEST_M, z];
                let surface = sampler
                    .as_ref()
                    .and_then(|s| s.sample_at_m(x, y + 0.05, z))
                    .map(|w| w.surface_y);
                let exposure = Exposure {
                    air_c,
                    wind_chill_c,
                    shelter: warmth::shelter_at(volume, chest),
                    immersion: warmth::immersion(y, surface),
                    rain,
                    fire_heat_c: crate::world_items::heat_at(self.world_items.iter(), chest),
                };
                (player.entity, exposure)
            })
            .collect();
        for (entity, exposure) in exposures {
            let Some(player) = self.world.players.get_mut(&entity.get()) else {
                continue;
            };
            let damage = player.body.advance(&exposure, dt_s);
            if damage > 0 {
                self.damage_player(entity, damage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::walk_arena_setup;
    use spall_core::player_entity_for;

    /// Midnight in winter, as one second of the clock.
    const MIDNIGHT: u64 = spall_core::time_of_day::DAY_CYCLE_TICKS * 3 / 4;

    /// The walk arena (floor top 1 m, 4 m wide in z) at a winter midnight
    /// with `build` applied to the terrain first, and one player at `feet`.
    fn night(
        build: impl FnOnce(&mut spall_voxel::Volume),
        feet: [f64; 3],
    ) -> (Simulation, EntityId) {
        let mut setup = walk_arena_setup();
        build(&mut setup.terrain);
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        sim.tick = Tick(MIDNIGHT);
        sim.set_season(3).unwrap();
        sim.set_weather_override(Some(spall_protocol::WeatherKind::Clear), None);
        let player = sim.add_player(player_entity_for(0), feet);
        (sim, player)
    }

    /// Lowest health over `seconds` of one-second body updates.
    fn endure(sim: &mut Simulation, player: EntityId, seconds: u32) -> u16 {
        let mut lowest = PLAYER_MAX_HEALTH;
        for _ in 0..seconds {
            sim.advance_body_heat(1.0);
            lowest = lowest.min(sim.player_health(player).unwrap());
        }
        lowest
    }

    fn fill(volume: &mut spall_voxel::Volume, lo: [i64; 3], hi: [i64; 3], material: MaterialId) {
        let id = volume.id();
        volume
            .apply_edit(&spall_voxel::EditPlan::filled_box(
                id,
                GlobalCell::new(lo[0], lo[1], lo[2]),
                GlobalCell::new(hi[0], hi[1], hi[2]),
                material,
            ))
            .unwrap();
    }

    /// A stone hut around x 8..12 m on the arena floor: walls 0.5 m thick,
    /// inside x 8.5..11.5 m, z 0.5..3.5 m, air to y 3 m and, with a roof, stone
    /// above that. Without one it is a pit's walls open to the sky.
    fn hut(roof: bool) -> impl FnOnce(&mut spall_voxel::Volume) {
        move |volume| {
            fill(volume, [32, 4, 0], [47, 13, 15], crate::fixtures::STONE);
            let top = if roof { 11 } else { 13 };
            fill(volume, [34, 4, 2], [45, top, 13], MaterialId::AIR);
        }
    }

    use crate::player::PLAYER_MAX_HEALTH;

    #[test]
    fn a_winter_night_in_the_open_drains_health() {
        let (mut sim, player) = night(|_| {}, [10.0, 1.0, 2.0]);
        let shelter = warmth::shelter_at(&sim.world().terrain().volume, [10.0, 2.2, 2.0]);
        assert_eq!(shelter, warmth::Shelter::default(), "open ground");
        let lowest = endure(&mut sim, player, 400);
        assert!(lowest < PLAYER_MAX_HEALTH - 20, "lowest health {lowest}");
    }

    /// A winter blizzard in the open at server `tick`: the lowest warmth and
    /// health over ten minutes, dry or soaked from the start.
    fn blizzard_day(tick: u64, soaked: bool) -> (f32, u16) {
        let (mut sim, player) = night(|_| {}, [10.0, 1.0, 2.0]);
        sim.tick = Tick(tick);
        sim.set_weather_override(Some(spall_protocol::WeatherKind::Blizzard), None);
        if soaked {
            sim.world
                .players
                .get_mut(&player.get())
                .unwrap()
                .body
                .wetness = 1.0;
        }
        let mut floor = 1.0_f32;
        let mut lowest = PLAYER_MAX_HEALTH;
        for _ in 0..600 {
            sim.advance_body_heat(1.0);
            floor = floor.min(sim.world().player(player).unwrap().body.warmth);
            lowest = lowest.min(sim.player_health(player).unwrap());
        }
        (floor, lowest)
    }

    #[test]
    fn a_winter_blizzard_by_day_never_hurts_a_dry_player_in_the_open() {
        use spall_core::time_of_day::DAY_CYCLE_TICKS;
        // Noon, and sunrise (the coldest daylight: no day warming yet).
        for (name, tick) in [("noon", DAY_CYCLE_TICKS / 4), ("sunrise", DAY_CYCLE_TICKS)] {
            let (floor, lowest) = blizzard_day(tick, false);
            eprintln!("dry blizzard {name}: warmth floor {floor:.3}, lowest health {lowest}");
            assert_eq!(lowest, PLAYER_MAX_HEALTH, "{name}");
            assert!(floor > warmth::DAMAGE_BELOW_WARMTH, "{name}: {floor}");
        }
        // Soaked is allowed to hurt by day; reported, not asserted.
        for (name, tick) in [("noon", DAY_CYCLE_TICKS / 4), ("sunrise", DAY_CYCLE_TICKS)] {
            let (floor, lowest) = blizzard_day(tick, true);
            eprintln!("soaked blizzard {name}: warmth floor {floor:.3}, lowest health {lowest}");
        }
    }
    #[test]
    fn a_fire_beside_the_player_prevents_cold_damage() {
        let (mut sim, player) = night(|_| {}, [10.0, 1.0, 2.0]);
        sim.place_fire(6, [11.5, 1.0, 2.0], 0.0, 10_000.0).unwrap();
        assert_eq!(endure(&mut sim, player, 400), PLAYER_MAX_HEALTH);
    }

    #[test]
    fn a_hut_or_a_pit_prevents_cold_damage() {
        for roof in [true, false] {
            let (mut sim, player) = night(hut(roof), [10.0, 1.0, 2.0]);
            let shelter = warmth::shelter_at(&sim.world().terrain().volume, [10.0, 2.2, 2.0]);
            assert_eq!(shelter.roof, roof);
            assert_eq!(shelter.enclosure, 1.0, "roof {roof}");
            assert_eq!(
                endure(&mut sim, player, 400),
                PLAYER_MAX_HEALTH,
                "roof {roof}"
            );
        }
    }

    #[test]
    fn the_tick_advances_body_heat_and_death_restores_it() {
        let (mut sim, player) = night(|_| {}, [10.0, 1.0, 2.0]);
        for _ in 0..warmth::BODY_HEAT_PERIOD_TICKS * 3 {
            sim.tick().unwrap();
        }
        let body = sim.world().player(player).unwrap().body;
        assert!(body.warmth < 1.0, "{body:?}");
        sim.damage_player(player, PLAYER_MAX_HEALTH).unwrap();
        assert_eq!(
            sim.world().player(player).unwrap().body,
            warmth::BodyHeat::default()
        );
    }
}
