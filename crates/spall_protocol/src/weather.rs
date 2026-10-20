use crate::records::WIRE_SCHEMA_VERSION;
use crate::records::{Record, RecordError, WireTag};
use serde::{Deserialize, Serialize};

pub const WEATHER_MAX_CLOCK_MS: u64 = 100_000_000_000_000;
pub const WEATHER_TRANSITION_MS: u64 = 300_000;
pub const WEATHER_PROFILE_MS: u64 = 1_800_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum WeatherKind {
    Clear = 0,
    Rain = 1,
    Storm = 2,
    Snow = 3,
    HighWind = 4,
    Blizzard = 5,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum WeatherClockSource {
    Vegetation = 0,
    ServerFallback = 1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeatherClock {
    pub time_ms: u64,
    pub source: WeatherClockSource,
    /// Existing ecology start season code (Spring=0, Summer=1, Autumn=2, Winter=3).
    pub start_season: u8,
    pub season_ms: u64,
}

/// Complete authoritative state. Precipitation/gust are Q0.16, wind components Q1.15,
/// and temperature is centi-degrees C.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeatherState {
    pub schema: u16,
    pub world_epoch: u64,
    pub world_revision: u64,
    pub effective_tick: u64,
    pub clock_ms: u64,
    pub clock_source: WeatherClockSource,
    pub start_season: u8,
    pub season_ms: u64,
    pub weather_seed: u64,
    pub transition_id: u64,
    pub transition_start_ms: u64,
    pub transition_duration_ms: u32,
    pub kind: WeatherKind,
    pub precipitation_q16: u16,
    pub wind_x_q15: i16,
    pub wind_z_q15: i16,
    pub gust_q16: u16,
    pub temperature_centi_c: i16,
    pub lightning_id: u64,
    pub lightning_tick: u64,
    pub override_kind: Option<WeatherKind>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WeatherError {
    #[error("unsupported weather schema {0}")]
    Schema(u16),
    #[error("invalid weather state: {0}")]
    Invalid(&'static str),
}
impl WeatherState {
    pub fn initial(seed: u64, epoch: u64, clock: WeatherClock) -> Self {
        let mut s = Self::for_clock(seed, epoch, 0, clock, 0);
        s.transition_duration_ms = 0;
        s
    }
    /// Pure seeded 30-minute fronts with five-minute ramps. Climate follows the supplied ecology season clock;
    /// snow occurs at <=2 C. Latitude/biome modulation is deferred until climate integration.
    pub fn for_clock(seed: u64, epoch: u64, tick: u64, clock: WeatherClock, revision: u64) -> Self {
        let bucket = clock.time_ms / WEATHER_PROFILE_MS;
        let roll = mix(seed ^ bucket.wrapping_mul(0x9E37_79B9_7F4A_7C15)) % 100;
        let season = (u64::from(clock.start_season) + clock.time_ms / clock.season_ms.max(1)) % 4;
        let seasonal_c = match season {
            0 => 8,
            1 => 22,
            2 => 10,
            _ => -8,
        };
        let (kind, precip, gust, offset) = match roll {
            0..=29 => (WeatherKind::Clear, 0, 6554, 0),
            30..=59 => (WeatherKind::Rain, 26214, 9830, 1),
            60..=69 => (WeatherKind::Storm, 58982, 39321, 1),
            70..=84 if seasonal_c <= 2 => (WeatherKind::Snow, 19661, 13107, -1),
            70..=84 => (WeatherKind::Rain, 19661, 13107, 0),
            85..=92 => (WeatherKind::HighWind, 0, 42000, 0),
            _ if seasonal_c <= 2 => (WeatherKind::Blizzard, 52429, 55000, -1),
            _ => (WeatherKind::Storm, 39321, 32768, 1),
        };
        let dirs: [(i16, i16); 8] = [
            (32767, 0),
            (23170, 23170),
            (0, 32767),
            (-23170, 23170),
            (-32768, 0),
            (-23170, -23170),
            (0, -32768),
            (23170, -23170),
        ];
        let (wx, wz) = dirs[(mix(seed ^ bucket.rotate_left(17)) % 8) as usize];
        let lightning_id = if kind == WeatherKind::Storm
            && clock.time_ms % WEATHER_PROFILE_MS < WEATHER_TRANSITION_MS
        {
            mix(seed ^ bucket.rotate_left(23) ^ 0x4C49_4748_544E_494E).max(1)
        } else {
            0
        };
        Self {
            schema: WIRE_SCHEMA_VERSION,
            world_epoch: epoch,
            world_revision: revision,
            effective_tick: tick,
            clock_ms: clock.time_ms,
            clock_source: clock.source,
            start_season: clock.start_season,
            season_ms: clock.season_ms,
            weather_seed: seed,
            transition_id: bucket,
            transition_start_ms: bucket * WEATHER_PROFILE_MS,
            transition_duration_ms: WEATHER_TRANSITION_MS as u32,
            kind,
            override_kind: None,
            precipitation_q16: precip,
            wind_x_q15: wx,
            wind_z_q15: wz,
            gust_q16: gust,
            temperature_centi_c: ((seasonal_c + offset) * 100) as i16,
            lightning_id,
            lightning_tick: if lightning_id == 0 {
                0
            } else {
                tick.saturating_sub((clock.time_ms % WEATHER_PROFILE_MS) * 60 / 1000)
                    .max(1)
            },
        }
    }
    /// Apply an authoritative preset with normalized presentation values.
    pub fn force_kind(&mut self, kind: Option<WeatherKind>) {
        let previous_kind = self.override_kind;
        let previous_lightning_id = self.lightning_id;
        let previous_lightning_tick = self.lightning_tick;
        self.schema = WIRE_SCHEMA_VERSION;
        self.override_kind = kind;
        if let Some(kind) = kind {
            self.kind = kind;
            let (precip, gust, offset) = match kind {
                WeatherKind::Clear => (0, 6554, 0),
                WeatherKind::Rain => (26214, 9830, 1),
                WeatherKind::Storm => (58982, 39321, 1),
                WeatherKind::Snow => (19661, 13107, -1),
                WeatherKind::HighWind => (0, 42000, 0),
                WeatherKind::Blizzard => (52429, 55000, -1),
            };
            self.precipitation_q16 = precip;
            self.gust_q16 = gust;
            let season = (u64::from(self.start_season) + self.clock_ms / self.season_ms.max(1)) % 4;
            let base_c = match season {
                0 => 8,
                1 => 22,
                2 => 10,
                _ => -8,
            };
            self.temperature_centi_c = (base_c + offset) * 100;
            self.lightning_id = if kind == WeatherKind::Storm {
                mix(self.weather_seed
                    ^ self.world_epoch.rotate_left(11)
                    ^ self.world_revision
                    ^ 0x4C49_4748_544E_494E)
                .max(1)
            } else {
                0
            };
            self.lightning_tick = if self.lightning_id == 0 {
                0
            } else if previous_kind == Some(kind) && previous_lightning_id == self.lightning_id {
                previous_lightning_tick
            } else {
                self.effective_tick.max(1)
            };
        } else {
            self.lightning_id = 0;
            self.lightning_tick = 0;
        }
    }
    pub fn validate(&self) -> Result<(), WeatherError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(WeatherError::Schema(self.schema));
        }
        if self.start_season > 3 || self.season_ms == 0 || self.season_ms > WEATHER_MAX_CLOCK_MS {
            return Err(WeatherError::Invalid("season clock bounds"));
        }
        if self.clock_ms > WEATHER_MAX_CLOCK_MS
            || self.transition_start_ms > self.clock_ms
            || self.clock_ms - self.transition_start_ms > WEATHER_PROFILE_MS
        {
            return Err(WeatherError::Invalid("clock/transition bounds"));
        }
        if self.transition_duration_ms as u64 > WEATHER_TRANSITION_MS
            || self.transition_id != self.transition_start_ms / WEATHER_PROFILE_MS
        {
            return Err(WeatherError::Invalid("transition bounds"));
        }
        if self.temperature_centi_c < -8000 || self.temperature_centi_c > 6000 {
            return Err(WeatherError::Invalid("temperature range"));
        }
        if (self.lightning_id == 0) != (self.lightning_tick == 0) {
            return Err(WeatherError::Invalid("lightning pair"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    pub state: WeatherState,
}
impl Record for WeatherSnapshot {
    const TAG: WireTag = WireTag::WeatherSnapshot;
    fn validate(&self) -> Result<(), RecordError> {
        self.state
            .validate()
            .map_err(|_| RecordError::Inconsistent("weather snapshot bounds"))
    }
}
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeded_policy_is_repeatable_and_bounded() {
        let c = WeatherClock {
            time_ms: 12_345_678,
            source: WeatherClockSource::ServerFallback,
            start_season: 0,
            season_ms: 900_000,
        };
        let a = WeatherState::for_clock(7, 2, 99, c, 4);
        assert_eq!(a, WeatherState::for_clock(7, 2, 99, c, 4));
        a.validate().unwrap();
        assert_eq!(a.transition_id, c.time_ms / WEATHER_PROFILE_MS)
    }
    #[test]
    fn every_weather_family_has_a_seeded_cold_or_warm_profile() {
        let cold = WeatherClock {
            time_ms: 0,
            source: WeatherClockSource::ServerFallback,
            start_season: 3,
            season_ms: 900_000,
        };
        let warm = WeatherClock {
            time_ms: 0,
            source: WeatherClockSource::ServerFallback,
            start_season: 1,
            season_ms: 900_000,
        };
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..512 {
            seen.insert(WeatherState::for_clock(seed, 1, 0, warm, 0).kind as u8);
            seen.insert(WeatherState::for_clock(seed, 1, 0, cold, 0).kind as u8);
        }
        assert_eq!(seen, (0..=5).collect());
    }

    #[test]
    fn weather_wire_record_round_trips_and_rejects_malformed_body() {
        let clock = WeatherClock {
            time_ms: 1,
            source: WeatherClockSource::ServerFallback,
            start_season: 0,
            season_ms: 900_000,
        };
        let record = WeatherSnapshot {
            state: WeatherState::initial(5, 1, clock),
        };
        let bytes = crate::encode_control(&record).unwrap();
        assert_eq!(
            crate::decode_control::<WeatherSnapshot>(&bytes).unwrap(),
            record
        );
        let mut bad = record.clone();
        bad.state.temperature_centi_c = 9_000;
        let body = postcard::to_stdvec(&bad).unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&crate::records::WIRE_SCHEMA_VERSION.to_le_bytes());
        frame.extend_from_slice(&(WireTag::WeatherSnapshot as u16).to_le_bytes());
        frame.extend(body);
        assert!(crate::decode_control::<WeatherSnapshot>(&frame).is_err());
    }

    #[test]
    fn forced_weather_presets_are_normalized() {
        let clock = WeatherClock {
            time_ms: 0,
            source: WeatherClockSource::ServerFallback,
            start_season: 1,
            season_ms: 900_000,
        };
        for seed in 0..256 {
            WeatherState::initial(seed, 1, clock).validate().unwrap();
        }
        let mut state = WeatherState::initial(33, 1, clock);
        state.force_kind(Some(WeatherKind::Snow));
        assert_eq!(state.kind, WeatherKind::Snow);
        assert_eq!(state.precipitation_q16, 19661);
        assert_eq!(state.temperature_centi_c, 2100);
        assert_eq!(state.override_kind, Some(WeatherKind::Snow));
        state.validate().unwrap();
        let storm_clock = WeatherClock {
            start_season: 3,
            ..clock
        };
        let mut storm = WeatherState::for_clock(44, 1, 7, storm_clock, 1);
        storm.force_kind(Some(WeatherKind::Storm));
        let id = storm.lightning_id;
        let event_tick = storm.lightning_tick;
        storm.effective_tick = 8;
        storm.force_kind(Some(WeatherKind::Storm));
        assert_eq!((storm.lightning_id, storm.lightning_tick), (id, event_tick));
    }

    #[test]
    fn malformed_bounds_reject() {
        let c = WeatherClock {
            time_ms: 0,
            source: WeatherClockSource::Vegetation,
            start_season: 0,
            season_ms: 900_000,
        };
        let mut s = WeatherState::initial(1, 1, c);
        s.schema += 1;
        assert!(s.validate().is_err());
        s.schema = WIRE_SCHEMA_VERSION;
        s.lightning_tick = 1;
        assert!(s.validate().is_err());
        s.lightning_tick = 0;
        s.clock_ms = WEATHER_MAX_CLOCK_MS + 1;
        assert!(s.validate().is_err())
    }
}
