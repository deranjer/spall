use crate::records::WIRE_SCHEMA_VERSION;
use crate::records::{Record, RecordError, WireTag};
use serde::{Deserialize, Serialize};

pub const WEATHER_MAX_CLOCK_MS: u64 = 100_000_000_000_000;
pub const WEATHER_TRANSITION_MS: u64 = 300_000;
pub const WEATHER_PROFILE_MS: u64 = 1_800_000;
/// Length of one weather front under [`WeatherProfile::WinterCrossing`]: a
/// blizzard (one front) lasts about six in-game hours at the default day length.
pub const WINTER_CROSSING_FRONT_MS: u64 = 300_000;
/// Blizzard precipitation, 0.95 as Q0.16.
pub const BLIZZARD_PRECIPITATION_Q16: u16 = 62_259;

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
/// How hard it snows: light, medium and heavy are the same `Snow` kind at
/// different `precipitation_q16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnowTier {
    Light,
    Medium,
    Heavy,
}
impl SnowTier {
    pub const ALL: [SnowTier; 3] = [SnowTier::Light, SnowTier::Medium, SnowTier::Heavy];
    /// Precipitation as Q0.16: 0.25, 0.5 and 0.85.
    pub fn precipitation_q16(self) -> u16 {
        match self {
            SnowTier::Light => 16_384,
            SnowTier::Medium => 32_768,
            SnowTier::Heavy => 55_705,
        }
    }
    /// The tier whose precipitation is nearest `q16`.
    pub fn from_precipitation(q16: u16) -> Self {
        Self::ALL
            .into_iter()
            .min_by_key(|t| t.precipitation_q16().abs_diff(q16))
            .unwrap_or(SnowTier::Medium)
    }
    pub fn label(self) -> &'static str {
        match self {
            SnowTier::Light => "light",
            SnowTier::Medium => "medium",
            SnowTier::Heavy => "heavy",
        }
    }
}
/// Which seeded weather schedule a world follows. Not persisted: the server
/// chooses it at startup from its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WeatherProfile {
    /// Four seasons of mixed 30-minute fronts.
    #[default]
    Standard,
    /// Winter Crossing: always cold, 5-minute fronts, snow most of the time
    /// (mostly the heavy tiers) and a blizzard in about one front in six.
    WinterCrossing,
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
        Self::for_clock_with(WeatherProfile::Standard, seed, epoch, tick, clock, revision)
    }
    /// [`Self::for_clock`] under a chosen schedule.
    pub fn for_clock_with(
        profile: WeatherProfile,
        seed: u64,
        epoch: u64,
        tick: u64,
        clock: WeatherClock,
        revision: u64,
    ) -> Self {
        let front_ms = match profile {
            WeatherProfile::Standard => WEATHER_PROFILE_MS,
            WeatherProfile::WinterCrossing => WINTER_CROSSING_FRONT_MS,
        };
        let bucket = clock.time_ms / front_ms;
        let roll = mix(seed ^ bucket.wrapping_mul(0x9E37_79B9_7F4A_7C15)) % 100;
        let season = (u64::from(clock.start_season) + clock.time_ms / clock.season_ms.max(1)) % 4;
        let seasonal_c = match (profile, season) {
            (WeatherProfile::WinterCrossing, _) => -8,
            (_, 0) => 8,
            (_, 1) => 22,
            (_, 2) => 10,
            _ => -8,
        };
        let (kind, precip, gust, offset) = match roll {
            _ if profile == WeatherProfile::WinterCrossing => {
                winter_crossing_front(seed, bucket, roll)
            }
            0..=29 => (WeatherKind::Clear, 0, 6554, 0),
            30..=59 => (WeatherKind::Rain, 26214, 9830, 1),
            60..=69 => (WeatherKind::Storm, 58982, 39321, 1),
            70..=84 if seasonal_c <= 2 => {
                let tier = SnowTier::ALL[(mix(seed ^ bucket.rotate_left(29)) % 3) as usize];
                (WeatherKind::Snow, tier.precipitation_q16(), 13107, -1)
            }
            70..=84 => (WeatherKind::Rain, 19661, 13107, 0),
            85..=92 => (WeatherKind::HighWind, 0, 42000, 0),
            _ if seasonal_c <= 2 => (WeatherKind::Blizzard, BLIZZARD_PRECIPITATION_Q16, 55000, -1),
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
            transition_start_ms: bucket * front_ms,
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
                tick.saturating_sub((clock.time_ms % front_ms) * 60 / 1000)
                    .max(1)
            },
        }
    }
    /// Apply an authoritative preset with normalized presentation values.
    /// `snow_tier` picks the precipitation of a forced `Snow` (default medium).
    pub fn force_kind(&mut self, kind: Option<WeatherKind>, snow_tier: Option<SnowTier>) {
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
                WeatherKind::Snow => (
                    snow_tier.unwrap_or(SnowTier::Medium).precipitation_q16(),
                    13107,
                    -1,
                ),
                WeatherKind::HighWind => (0, 42000, 0),
                WeatherKind::Blizzard => (BLIZZARD_PRECIPITATION_Q16, 55000, -1),
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
            || (self.transition_id != self.transition_start_ms / WEATHER_PROFILE_MS
                && self.transition_id != self.transition_start_ms / WINTER_CROSSING_FRONT_MS)
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
/// One Winter Crossing front: a blizzard on 18 of 100 rolls, snow on 67 (the
/// heavy tier 60%, medium 30%, light 10%), then clear spells and high wind.
fn winter_crossing_front(seed: u64, bucket: u64, roll: u64) -> (WeatherKind, u16, u16, i64) {
    match roll {
        0..=17 => (WeatherKind::Blizzard, BLIZZARD_PRECIPITATION_Q16, 55000, -1),
        18..=84 => {
            let tier = match mix(seed ^ bucket.rotate_left(29)) % 10 {
                0 => SnowTier::Light,
                1..=3 => SnowTier::Medium,
                _ => SnowTier::Heavy,
            };
            (WeatherKind::Snow, tier.precipitation_q16(), 13107, -1)
        }
        85..=94 => (WeatherKind::Clear, 0, 6554, 0),
        _ => (WeatherKind::HighWind, 0, 42000, 0),
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
/// Wind at one instant: a heading and a 0..=1 strength. See [`wind_at`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wind {
    /// Unit horizontal direction the wind blows toward, (x, z).
    pub direction: [f32; 2],
    /// 0 is a lull, about 0.25-0.5 a moderate breeze, 0.75 and up a strong gust.
    pub strength: f32,
}

impl Wind {
    /// Horizontal wind speed in m/s that `kind` shows for this strength: a
    /// blizzard peaks near 17.5 m/s, snow tiers stay gentle, wind-only
    /// weather never drops below a 2 m/s breeze so its streaks keep an axis.
    pub fn speed_mps(&self, kind: WeatherKind) -> f32 {
        let s = self.strength.clamp(0.0, 1.0);
        match kind {
            WeatherKind::Clear => 0.0,
            WeatherKind::Rain => 3.5 * s.powf(1.3),
            WeatherKind::Storm => 8.0 * s.powf(1.3),
            WeatherKind::Snow => 3.0 * s.powf(1.8),
            WeatherKind::HighWind => 2.0 + 10.0 * s,
            WeatherKind::Blizzard => 17.5 * s.powf(1.3),
        }
    }
}

/// Length of one strength phase slot; phases are lull, moderate or strong.
const WIND_SLOT_S: f64 = 20.0;
/// Heading changes land once per 55 s slot, jittered by up to 35 s, so the
/// gap between two changes is 20-90 s.
const WIND_TURN_SLOT_S: f64 = 55.0;
const WIND_TURN_JITTER_S: f64 = 35.0;

fn unit(x: u64) -> f64 {
    (mix(x) >> 11) as f64 / (1u64 << 53) as f64
}

fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn turn_hash(seed: u64, k: i64, salt: u64) -> u64 {
    seed ^ (k as u64).wrapping_mul(0xA24B_AED4_963E_E407) ^ salt
}

fn turn_time_s(seed: u64, k: i64) -> f64 {
    k as f64 * WIND_TURN_SLOT_S + WIND_TURN_JITTER_S * unit(turn_hash(seed, k, 1))
}

fn turn_heading(seed: u64, k: i64) -> f64 {
    unit(turn_hash(seed, k, 2)) * std::f64::consts::TAU
}

/// Slot level: lull ~0-0.03, moderate 0.25-0.5, strong 0.75-0.9. A lull never
/// directly follows another, so lulls last one slot (about 10-20 s once the
/// 6 s blends to the neighbours are counted).
fn slot_level(seed: u64, k: i64) -> f64 {
    let roll = |k: i64| unit(turn_hash(seed, k, 3));
    let r = roll(k);
    let u = unit(turn_hash(seed, k, 4));
    if r < 0.2 && roll(k - 1) >= 0.2 {
        0.03 * u
    } else if r < 0.6 {
        0.25 + 0.25 * u
    } else {
        0.75 + 0.15 * u
    }
}

/// The wind at `clock_s` seconds of the authoritative weather clock
/// (`WeatherState::clock_ms / 1000`, plus any local extrapolation since that
/// snapshot). A pure function of `(seed, clock_s)`, so every client and the
/// server see the same wind at the same moment.
///
/// Heading: a new random heading anywhere in 360 degrees every 20-90 s, reached
/// by turning over 5-15 s, plus a wobble of up to about 9 degrees. Strength:
/// 20 s slots that are lulls (about 20%), moderate (40%) or strong (40%),
/// blended over 6 s at the edges, with short surges riding on the stronger slots.
pub fn wind_at(seed: u64, clock_s: f64) -> Wind {
    let t = clock_s.max(0.0);
    let tau = std::f64::consts::TAU;

    let mut k = (t / WIND_TURN_SLOT_S).floor() as i64;
    if turn_time_s(seed, k) > t {
        k -= 1;
    }
    let delta = {
        let d = (turn_heading(seed, k) - turn_heading(seed, k - 1)).rem_euclid(tau);
        if d > std::f64::consts::PI { d - tau } else { d }
    };
    let turn_s = 5.0 + 10.0 * unit(turn_hash(seed, k, 5));
    let progress = smoothstep((t - turn_time_s(seed, k)) / turn_s);
    let phase = |salt: u64| unit(seed ^ salt) * tau;
    let wobble =
        0.09 * (t * tau / 9.0 + phase(0xB1)).sin() + 0.06 * (t * tau / 23.0 + phase(0xB2)).sin();
    let heading = turn_heading(seed, k - 1) + delta * progress + wobble;

    let x = (t + WIND_SLOT_S * unit(seed ^ 0xC1)) / WIND_SLOT_S;
    let slot = x.floor() as i64;
    let f = x - slot as f64;
    let level = if f < 0.15 {
        let blend = smoothstep(0.5 + f / 0.3);
        slot_level(seed, slot - 1) * (1.0 - blend) + slot_level(seed, slot) * blend
    } else if f > 0.85 {
        let blend = smoothstep((f - 0.85) / 0.3);
        slot_level(seed, slot) * (1.0 - blend) + slot_level(seed, slot + 1) * blend
    } else {
        slot_level(seed, slot)
    };
    let ripple =
        0.5 * (t * tau / 4.1 + phase(0xB3)).sin() + 0.5 * (t * tau / 7.3 + phase(0xB4)).sin();
    let surge = 0.12 * smoothstep((level - 0.2) / 0.5);
    Wind {
        direction: [heading.cos() as f32, heading.sin() as f32],
        strength: (level + surge * ripple).clamp(0.0, 1.0) as f32,
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
    fn wind_is_deterministic_bounded_and_has_lulls_gusts_and_turns() {
        let seed = 0x5EED;
        let (mut lull, mut strong) = (false, false);
        let mut max_speed = 0.0f32;
        let mut headings = Vec::new();
        let mut longest_lull = 0.0f64;
        let mut lull_run = 0.0f64;
        let mut previous = wind_at(seed, 0.0);
        for step in 0..=24_000 {
            let t = f64::from(step) * 0.05; // 20 minutes
            let wind = wind_at(seed, t);
            assert_eq!(wind, wind_at(seed, t));
            assert!((0.0..=1.0).contains(&wind.strength));
            let [x, z] = wind.direction;
            assert!((x.hypot(z) - 1.0).abs() < 1e-4);
            assert!((wind.strength - previous.strength).abs() < 0.05, "smooth");
            assert!(
                (x - previous.direction[0]).abs() < 0.1 && (z - previous.direction[1]).abs() < 0.1,
                "heading is continuous"
            );
            previous = wind;
            let speed = wind.speed_mps(WeatherKind::Blizzard);
            max_speed = max_speed.max(speed);
            if speed < 0.6 {
                lull = true;
                lull_run += 0.05;
                longest_lull = longest_lull.max(lull_run);
            } else {
                lull_run = 0.0;
            }
            strong |= wind.strength > 0.8;
            if step % 20 == 0 {
                headings.push(z.atan2(x));
            }
        }
        assert!(lull && strong, "lull {lull}, strong {strong}");
        assert!(longest_lull < 25.0, "lulls are short: {longest_lull}");
        assert!((15.0..=18.0).contains(&max_speed), "peak {max_speed} m/s");
        // Within a 3 minute window the heading swings more than 90 degrees.
        let swing = |a: f32, b: f32| {
            let d = (a - b).rem_euclid(std::f32::consts::TAU);
            d.min(std::f32::consts::TAU - d)
        };
        assert!(headings.windows(180).any(|w| {
            w.iter()
                .any(|&a| w.iter().any(|&b| swing(a, b) > std::f32::consts::FRAC_PI_2))
        }));
        assert_ne!(wind_at(seed, 100.0), wind_at(seed + 1, 100.0));
        let gust = Wind {
            direction: [1.0, 0.0],
            strength: 0.9,
        };
        assert!(gust.speed_mps(WeatherKind::Snow) < 0.2 * gust.speed_mps(WeatherKind::Blizzard));
    }

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
    fn snow_tiers_are_ordered_forced_and_rolled() {
        let q: Vec<u16> = SnowTier::ALL.map(SnowTier::precipitation_q16).to_vec();
        assert_eq!(q, [16_384, 32_768, 55_705]);
        for t in SnowTier::ALL {
            assert_eq!(SnowTier::from_precipitation(t.precipitation_q16()), t);
        }
        let clock = WeatherClock {
            time_ms: 0,
            source: WeatherClockSource::ServerFallback,
            start_season: 3,
            season_ms: 900_000,
        };
        let mut state = WeatherState::initial(1, 1, clock);
        for t in SnowTier::ALL {
            state.force_kind(Some(WeatherKind::Snow), Some(t));
            assert_eq!(state.precipitation_q16, t.precipitation_q16());
            state.validate().unwrap();
        }
        // Automatic rolls in winter reach every tier, and Blizzard is stronger than heavy snow.
        let mut seen = [false; 3];
        for bucket in 0..2_000u64 {
            let c = WeatherClock {
                time_ms: bucket * WEATHER_PROFILE_MS % (3 * 900_000 * 4),
                ..clock
            };
            let s = WeatherState::for_clock(bucket, 1, 0, c, 0);
            if s.kind == WeatherKind::Snow {
                seen[SnowTier::ALL
                    .iter()
                    .position(|t| *t == SnowTier::from_precipitation(s.precipitation_q16))
                    .unwrap()] = true;
                assert!(
                    SnowTier::ALL
                        .iter()
                        .any(|t| t.precipitation_q16() == s.precipitation_q16)
                );
            }
        }
        assert_eq!(seen, [true; 3]);
        assert!(BLIZZARD_PRECIPITATION_Q16 > SnowTier::Heavy.precipitation_q16());
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
        state.force_kind(Some(WeatherKind::Snow), None);
        assert_eq!(state.kind, WeatherKind::Snow);
        assert_eq!(
            state.precipitation_q16,
            SnowTier::Medium.precipitation_q16()
        );
        assert_eq!(state.temperature_centi_c, 2100);
        assert_eq!(state.override_kind, Some(WeatherKind::Snow));
        state.validate().unwrap();
        let storm_clock = WeatherClock {
            start_season: 3,
            ..clock
        };
        let mut storm = WeatherState::for_clock(44, 1, 7, storm_clock, 1);
        storm.force_kind(Some(WeatherKind::Storm), None);
        let id = storm.lightning_id;
        let event_tick = storm.lightning_tick;
        storm.effective_tick = 8;
        storm.force_kind(Some(WeatherKind::Storm), None);
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
