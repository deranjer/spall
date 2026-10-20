//! Weather clock and state methods.

use super::*;

impl Simulation {
    /// Initializes the deterministic state for a legacy save with no weather checkpoint.
    pub fn initialize_legacy_weather(&mut self, seed: u64) {
        self.weather_seed = seed;
        let source = if self.vegetation.is_some() {
            spall_protocol::WeatherClockSource::Vegetation
        } else {
            spall_protocol::WeatherClockSource::ServerFallback
        };
        let time_ms = self
            .vegetation
            .as_ref()
            .map_or(self.tick.get().saturating_mul(1000) / 60, |v| {
                v.state.time_ms
            });
        self.weather = spall_protocol::WeatherState::for_clock(
            seed,
            self.weather.world_epoch,
            self.tick.get(),
            spall_protocol::WeatherClock {
                time_ms,
                source,
                start_season: self
                    .vegetation
                    .as_ref()
                    .map_or(self.weather.start_season, |v| v.state.start_season as u8),
                season_ms: self
                    .vegetation
                    .as_ref()
                    .map_or(self.weather.season_ms, |v| v.state.season_ms),
            },
            self.weather.world_revision,
        );
        self.weather_dirty = true;
    }

    /// Set the deterministic world seed before the first authoritative tick.
    pub fn set_weather_seed(&mut self, seed: u64) {
        self.weather_seed = seed;
        self.weather = spall_protocol::WeatherState::for_clock(
            seed,
            self.weather.world_epoch,
            self.tick.get(),
            spall_protocol::WeatherClock {
                time_ms: self.weather.clock_ms,
                source: self.weather.clock_source,
                start_season: self.weather.start_season,
                season_ms: self.weather.season_ms,
            },
            self.weather.world_revision,
        );
        self.weather_dirty = true;
    }

    /// Apply or clear a persisted server-authorized weather override at the current tick.
    pub fn set_weather_override(&mut self, kind: Option<spall_protocol::WeatherKind>) {
        self.weather.world_revision = self.weather.world_revision.saturating_add(1);
        self.weather.effective_tick = self.tick.get();
        if let Some(kind) = kind {
            self.weather.force_kind(Some(kind));
        } else {
            self.weather = spall_protocol::WeatherState::for_clock(
                self.weather_seed,
                self.weather.world_epoch,
                self.tick.get(),
                spall_protocol::WeatherClock {
                    time_ms: self.weather.clock_ms,
                    source: self.weather.clock_source,
                    start_season: self.weather.start_season,
                    season_ms: self.weather.season_ms,
                },
                self.weather.world_revision,
            );
        }
        self.weather_dirty = true;
    }

    /// Current authoritative weather state. Clock source is fixed per world.
    pub fn weather_state(&self) -> &spall_protocol::WeatherState {
        &self.weather
    }

    /// Latest weather state owed to durable journal.
    pub fn take_weather_journal(&mut self) -> Option<spall_protocol::WeatherState> {
        if !std::mem::take(&mut self.weather_dirty) {
            return None;
        }
        Some(self.weather.clone())
    }

    /// Verifies recovered weather still points at the world's frozen clock source.
    pub fn validate_weather_clock_source(&self) -> Result<(), String> {
        if self.weather.clock_source == spall_protocol::WeatherClockSource::Vegetation {
            let Some(vegetation) = &self.vegetation else {
                return Err("weather clock requires recovered vegetation".into());
            };
            if vegetation.state.start_season as u8 != self.weather.start_season
                || vegetation.state.season_ms != self.weather.season_ms
            {
                return Err("weather and vegetation season clocks disagree".into());
            }
        }
        Ok(())
    }

    /// Restores the explicit weather/fallback clock record from a checkpoint or journal.
    pub fn restore_weather(&mut self, state: spall_protocol::WeatherState) -> Result<(), String> {
        state.validate().map_err(|e| e.to_string())?;
        self.weather_seed = state.weather_seed;
        let mut state = state;
        state.schema = spall_protocol::WEATHER_SCHEMA;
        self.weather = state;
        self.weather_dirty = false;
        Ok(())
    }

    pub(super) fn advance_weather(&mut self) {
        let (time_ms, source, start_season, season_ms) = match self.weather.clock_source {
            spall_protocol::WeatherClockSource::Vegetation => match self.vegetation.as_ref() {
                Some(v) => (
                    v.state.time_ms,
                    spall_protocol::WeatherClockSource::Vegetation,
                    v.state.start_season as u8,
                    v.state.season_ms,
                ),
                None => (
                    self.weather.clock_ms,
                    spall_protocol::WeatherClockSource::Vegetation,
                    self.weather.start_season,
                    self.weather.season_ms,
                ),
            },
            spall_protocol::WeatherClockSource::ServerFallback => (
                self.tick.get().saturating_mul(1000) / 60,
                spall_protocol::WeatherClockSource::ServerFallback,
                self.weather.start_season,
                self.weather.season_ms,
            ),
        };
        let clock = spall_protocol::WeatherClock {
            time_ms,
            source,
            start_season,
            season_ms,
        };
        let next = spall_protocol::WeatherState::for_clock(
            self.weather_seed,
            self.weather.world_epoch,
            self.tick.get(),
            clock,
            self.weather.world_revision,
        );
        let mut next = next;
        if let Some(kind) = self.weather.override_kind {
            next.force_kind(Some(kind));
        }
        let profile_changed = next.transition_id != self.weather.transition_id
            || next.kind != self.weather.kind
            || next.precipitation_q16 != self.weather.precipitation_q16
            || next.gust_q16 != self.weather.gust_q16
            || next.temperature_centi_c != self.weather.temperature_centi_c
            || next.lightning_id != self.weather.lightning_id;
        self.weather = next;
        self.weather_dirty |= profile_changed;
    }
}
