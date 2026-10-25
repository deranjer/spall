//! Server-authoritative warmth and wetness (ENG-264, ENG-265).
//!
//! Each player carries a [`BodyHeat`]: warmth (1 warm, 0 freezing) and
//! wetness (0 dry, 1 soaked). Once a second the simulation gathers what the
//! body is exposed to ([`Exposure`]: air temperature from the weather and the
//! time of day, wind, shelter, water depth, rain and nearby fires) and
//! advances it. Warmth eases toward a target set by the felt temperature; below
//! [`DAMAGE_BELOW_WARMTH`] the cold hurts through
//! [`crate::Simulation::damage_player`].
//!
//! Shelter is coarse on purpose: solid cells straight overhead, plus how many
//! of a few horizontal rays from the chest meet a wall in reach. A cave or a
//! dug hole is enclosed; open ground is not. No wider scan is made.
//!
//! Every number to tune is in the block below.

use spall_protocol::{WeatherKind, WeatherState};
use spall_voxel::Volume;

// ---- Tuning -------------------------------------------------------------

/// How often warmth and wetness advance, in server ticks (once a second).
pub(crate) const BODY_HEAT_PERIOD_TICKS: u64 = 60;
/// Midnight is this much colder than the weather's temperature, noon this
/// much warmer; both scale with the sun's height.
const NIGHT_DROP_C: f32 = 10.0;
const DAY_WARMING_C: f32 = 3.0;
/// Wind chill in the open, any weather.
const WIND_CHILL_C: f32 = 3.0;
/// Extra chill in the open by weather kind: a blizzard is the coldest, but a
/// dry player in the open never reaches the damage threshold by day (cold
/// damage is for nights; see the noon blizzard test).
fn weather_chill_c(kind: WeatherKind) -> f32 {
    match kind {
        WeatherKind::Clear => 0.0,
        WeatherKind::Rain | WeatherKind::Snow => 1.0,
        WeatherKind::Storm => 1.5,
        WeatherKind::HighWind => 2.0,
        WeatherKind::Blizzard => 2.5,
    }
}
/// Fully enclosed (a cave, a dug hole) keeps off all wind and feels this
/// much warmer: still air and ground warmth.
const SHELTER_WARMTH_C: f32 = 6.0;
/// Solid cover overhead adds this much to the enclosure from the rays.
const ROOF_COVER: f32 = 0.25;
/// Cells straight up from the chest searched for a roof, metres.
const ROOF_SEARCH_M: f64 = 8.0;
/// Horizontal rays from the chest, and how far each looks for a wall.
const SHELTER_RAYS: usize = 8;
const SHELTER_REACH_M: f64 = 4.0;
/// Chest height above the feet: where shelter, fire heat are measured.
pub(crate) const CHEST_M: f64 = 1.2;
/// Being soaked feels this much colder.
const WET_CHILL_C: f32 = 8.0;
/// Felt temperature at and above which warmth recovers fully, and at and
/// below which it falls to zero.
const COMFORT_C: f32 = 5.0;
const FREEZING_C: f32 = -23.0;
/// Time constants for warmth falling and recovering, seconds.
const COOLING_TAU_S: f32 = 60.0;
const WARMING_TAU_S: f32 = 20.0;
/// Below this warmth the cold hurts, rising linearly to
/// [`MAX_COLD_DAMAGE_PER_S`] at zero warmth.
pub(crate) const DAMAGE_BELOW_WARMTH: f32 = 0.2;
const MAX_COLD_DAMAGE_PER_S: f32 = 0.5;
/// Water this deep over the feet soaks completely; shallower wets in
/// proportion (wading to the knees is about a quarter).
const SOAKED_DEPTH_M: f64 = 1.5;
/// Time constants for getting wet in water and in rain, seconds.
const SOAK_TAU_S: f32 = 2.0;
const RAIN_TAU_S: f32 = 25.0;
/// Drying in still air at 0 C, seconds; warmer air and fire heat shorten it.
const DRY_TAU_S: f32 = 300.0;
const DRY_AIR_PER_C: f32 = 0.1;
const DRY_FIRE_PER_C: f32 = 0.2;
/// Falling snow wets this much as rain of the same amount.
const SNOW_WETTING: f32 = 0.1;

// -------------------------------------------------------------------------

/// How sheltered a spot is.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Shelter {
    /// Solid cells within [`ROOF_SEARCH_M`] straight overhead.
    pub(crate) roof: bool,
    /// Fraction of the horizontal rays that met a wall in reach, 0..=1.
    pub(crate) enclosure: f32,
}

impl Shelter {
    /// 0 in the open, 1 fully enclosed.
    pub(crate) fn cover(&self) -> f32 {
        let roof = if self.roof { ROOF_COVER } else { 0.0 };
        (self.enclosure + roof).clamp(0.0, 1.0)
    }
}

/// What a body is exposed to for one update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Exposure {
    /// Air temperature, weather and time of day ([`air_temperature_c`]).
    pub(crate) air_c: f32,
    /// Wind chill in the open ([`wind_chill_c`]).
    pub(crate) wind_chill_c: f32,
    pub(crate) shelter: Shelter,
    /// Water depth over the feet as a fraction of soaking depth, 0..=1.
    pub(crate) immersion: f32,
    /// Falling rain at the spot, 0..=1, before any roof.
    pub(crate) rain: f32,
    /// Felt warming from lit fires (`crate::world_items::heat_at`).
    pub(crate) fire_heat_c: f32,
}

/// A player's warmth and wetness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyHeat {
    /// 1 warm, 0 freezing.
    pub warmth: f32,
    /// 0 dry, 1 soaked.
    pub wetness: f32,
    /// Cold damage owed but not yet a whole point.
    damage_carry: f32,
}

impl Default for BodyHeat {
    fn default() -> Self {
        Self {
            warmth: 1.0,
            wetness: 0.0,
            damage_carry: 0.0,
        }
    }
}

impl BodyHeat {
    /// Felt temperature for this body under `exposure`.
    pub(crate) fn felt_c(&self, exposure: &Exposure) -> f32 {
        let cover = exposure.shelter.cover();
        finite_or(exposure.air_c, 15.0)
            - finite_or(exposure.wind_chill_c, 0.0).max(0.0) * (1.0 - cover)
            + SHELTER_WARMTH_C * cover
            - WET_CHILL_C * self.wetness
            + finite_or(exposure.fire_heat_c, 0.0).max(0.0)
    }

    /// Advances `dt_s` seconds and returns whole points of cold damage due.
    pub(crate) fn advance(&mut self, exposure: &Exposure, dt_s: f32) -> u16 {
        let dt_s = finite_or(dt_s, 0.0).clamp(0.0, 10.0);
        let air = finite_or(exposure.air_c, 15.0);
        let fire = finite_or(exposure.fire_heat_c, 0.0).max(0.0);
        // Wetness first: being soaked this second already chills.
        let immersion = unit(exposure.immersion);
        let rain = if exposure.shelter.roof {
            0.0
        } else {
            unit(exposure.rain)
        };
        let wet_from = immersion.max(rain);
        self.wetness = if wet_from > self.wetness {
            let tau = if immersion >= rain {
                SOAK_TAU_S
            } else {
                RAIN_TAU_S
            };
            ease(self.wetness, wet_from, dt_s, tau)
        } else {
            let tau = DRY_TAU_S / (1.0 + DRY_AIR_PER_C * air.max(0.0) + DRY_FIRE_PER_C * fire);
            ease(self.wetness, wet_from, dt_s, tau)
        };

        let target =
            ((self.felt_c(exposure) - FREEZING_C) / (COMFORT_C - FREEZING_C)).clamp(0.0, 1.0);
        let tau = if target < self.warmth {
            COOLING_TAU_S
        } else {
            WARMING_TAU_S
        };
        self.warmth = ease(self.warmth, target, dt_s, tau);

        if self.warmth < DAMAGE_BELOW_WARMTH {
            let severity = (DAMAGE_BELOW_WARMTH - self.warmth) / DAMAGE_BELOW_WARMTH;
            self.damage_carry += MAX_COLD_DAMAGE_PER_S * severity * dt_s;
        } else {
            self.damage_carry = 0.0;
        }
        let whole = self.damage_carry.floor();
        self.damage_carry -= whole;
        whole as u16
    }
}

/// Air temperature at server `tick`: the weather's, colder at night and a
/// little warmer at midday.
pub(crate) fn air_temperature_c(
    weather: &WeatherState,
    clock: spall_core::time_of_day::DayClock,
    tick: u64,
) -> f32 {
    let sun = clock.state_at(tick).sun_height();
    f32::from(weather.temperature_centi_c) / 100.0 + DAY_WARMING_C * sun.max(0.0)
        - NIGHT_DROP_C * (-sun).max(0.0)
}

/// Wind chill in the open under `weather`.
pub(crate) fn wind_chill_c(weather: &WeatherState) -> f32 {
    WIND_CHILL_C + weather_chill_c(weather.kind)
}

/// Falling rain under `weather`, 0..=1; snow mostly brushes off.
pub(crate) fn rain(weather: &WeatherState) -> f32 {
    let amount = f32::from(weather.precipitation_q16) / f32::from(u16::MAX);
    match weather.kind {
        WeatherKind::Rain | WeatherKind::Storm => amount,
        WeatherKind::Snow | WeatherKind::Blizzard => SNOW_WETTING * amount,
        WeatherKind::Clear | WeatherKind::HighWind => 0.0,
    }
}

/// Water depth over the feet as a fraction of soaking depth.
pub(crate) fn immersion(feet_y_m: f64, surface_y_m: Option<f64>) -> f32 {
    surface_y_m.map_or(0.0, |surface| {
        ((surface - feet_y_m) / SOAKED_DEPTH_M).clamp(0.0, 1.0) as f32
    })
}

/// How sheltered `chest_m` (metres) is in `volume`.
pub(crate) fn shelter_at(volume: &Volume, chest_m: [f64; 3]) -> Shelter {
    use spall_voxel::query::{RayConfig, cast_ray};
    use spall_voxel::{Ray, RayOutcome, Sample};
    if !chest_m.iter().all(|v| v.is_finite()) {
        return Shelter::default();
    }
    let cell_m = volume.cell_size().metres();
    let [x, y, z] = chest_m.map(|v| (v / cell_m).floor() as i64);
    let steps = (ROOF_SEARCH_M / cell_m).ceil() as i64;
    let roof = (1..=steps).any(|dy| {
        matches!(
            volume.sample(spall_core::GlobalCell::new(x, y + dy, z)),
            Ok(Sample::Filled(_))
        )
    });
    let origin = glam::DVec3::from_array(chest_m) / cell_m;
    let walls = (0..SHELTER_RAYS)
        .filter(|i| {
            let angle = *i as f64 * std::f64::consts::TAU / SHELTER_RAYS as f64;
            let dir = glam::DVec3::new(angle.cos(), 0.0, angle.sin());
            matches!(
                cast_ray(
                    volume,
                    Ray::new(origin, dir),
                    RayConfig::new(SHELTER_REACH_M / cell_m)
                ),
                Ok(RayOutcome::Hit(_))
            )
        })
        .count();
    Shelter {
        roof,
        enclosure: walls as f32 / SHELTER_RAYS as f32,
    }
}

fn ease(value: f32, target: f32, dt_s: f32, tau_s: f32) -> f32 {
    value + (target - value) * (1.0 - (-dt_s / tau_s).exp())
}

fn unit(value: f32) -> f32 {
    finite_or(value, 0.0).clamp(0.0, 1.0)
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clear winter midnight in the open, dry.
    const NIGHT: Exposure = Exposure {
        air_c: -18.0,
        wind_chill_c: WIND_CHILL_C,
        shelter: Shelter {
            roof: false,
            enclosure: 0.0,
        },
        immersion: 0.0,
        rain: 0.0,
        fire_heat_c: 0.0,
    };

    /// Total cold damage over `seconds` of one-second updates.
    fn run(body: &mut BodyHeat, exposure: Exposure, seconds: u32) -> u32 {
        (0..seconds)
            .map(|_| u32::from(body.advance(&exposure, 1.0)))
            .sum()
    }

    #[test]
    fn a_night_in_the_open_hurts_and_a_fire_or_shelter_prevents_it() {
        let mut open = BodyHeat::default();
        // A ten-minute night.
        let hurt = run(&mut open, NIGHT, 600);
        assert!(
            hurt >= 100,
            "a night in the open is lethal: {hurt} {open:?}"
        );
        assert!(open.warmth < DAMAGE_BELOW_WARMTH);

        let mut by_fire = BodyHeat::default();
        let fire = Exposure {
            fire_heat_c: 13.0, // about 3 m from a fire
            ..NIGHT
        };
        assert_eq!(run(&mut by_fire, fire, 600), 0, "{by_fire:?}");

        let mut in_cave = BodyHeat::default();
        let cave = Exposure {
            shelter: Shelter {
                roof: true,
                enclosure: 1.0,
            },
            ..NIGHT
        };
        assert_eq!(run(&mut in_cave, cave, 600), 0, "{in_cave:?}");
        let mut in_hole = BodyHeat::default();
        let hole = Exposure {
            shelter: Shelter {
                roof: false,
                enclosure: 1.0,
            },
            ..NIGHT
        };
        assert_eq!(run(&mut in_hole, hole, 600), 0, "{in_hole:?}");
    }

    #[test]
    fn a_blizzard_is_colder_and_wet_cold_is_worse() {
        let body = BodyHeat::default();
        let blizzard = Exposure {
            wind_chill_c: WIND_CHILL_C + weather_chill_c(WeatherKind::Blizzard),
            ..NIGHT
        };
        assert!(body.felt_c(&blizzard) < body.felt_c(&NIGHT));
        let soaked = BodyHeat {
            wetness: 1.0,
            ..body
        };
        assert!(soaked.felt_c(&NIGHT) < body.felt_c(&NIGHT));
        // A sheltered night is survivable dry, but not soaked through.
        let cave = Exposure {
            shelter: Shelter {
                roof: true,
                enclosure: 1.0,
            },
            immersion: 1.0,
            ..NIGHT
        };
        let mut wet = BodyHeat::default();
        assert!(run(&mut wet, cave, 300) > 0, "{wet:?}");
    }

    #[test]
    fn deeper_water_wets_more() {
        let wading = immersion(10.0, Some(10.3));
        let waist = immersion(10.0, Some(10.9));
        let through_ice = immersion(10.0, Some(13.0));
        assert!(0.0 < wading && wading < waist && waist < through_ice);
        assert_eq!(through_ice, 1.0);
        assert_eq!(immersion(10.0, None), 0.0);
        assert_eq!(immersion(10.0, Some(9.0)), 0.0);
        let soak = |immersion| {
            let mut body = BodyHeat::default();
            let mild = Exposure {
                air_c: 15.0,
                immersion,
                ..NIGHT
            };
            run(&mut body, mild, 10);
            body.wetness
        };
        let (a, b, c) = (soak(wading), soak(waist), soak(through_ice));
        assert!(a < b && b < c && c > 0.95, "{a} {b} {c}");
    }

    #[test]
    fn wetness_dries_and_dries_faster_by_a_fire() {
        let dry = |fire_heat_c, seconds| {
            let mut body = BodyHeat {
                wetness: 1.0,
                ..BodyHeat::default()
            };
            let air = Exposure {
                air_c: 0.0,
                fire_heat_c,
                ..NIGHT
            };
            run(&mut body, air, seconds);
            body.wetness
        };
        let still = dry(0.0, 120);
        let by_fire = dry(20.0, 120);
        assert!(still < 1.0 && still > 0.5, "slow in still air: {still}");
        assert!(by_fire < 0.2, "quick by a fire: {by_fire}");
        assert!(dry(0.0, 3600) < 0.01, "dries out in the end");
    }

    #[test]
    fn rain_wets_only_without_a_roof() {
        let rain = Exposure {
            air_c: 10.0,
            rain: 1.0,
            ..NIGHT
        };
        let mut open = BodyHeat::default();
        run(&mut open, rain, 120);
        let mut roofed = BodyHeat::default();
        run(
            &mut roofed,
            Exposure {
                shelter: Shelter {
                    roof: true,
                    enclosure: 0.0,
                },
                ..rain
            },
            120,
        );
        assert!(
            open.wetness > 0.9 && roofed.wetness == 0.0,
            "{open:?} {roofed:?}"
        );
    }

    #[test]
    fn nights_are_colder_than_days() {
        use spall_core::time_of_day::DAY_CYCLE_TICKS;
        let weather = WeatherState::initial(
            1,
            1,
            spall_protocol::WeatherClock {
                time_ms: 0,
                source: spall_protocol::WeatherClockSource::ServerFallback,
                start_season: 3,
                season_ms: 900_000,
            },
        );
        let clock = spall_core::time_of_day::DayClock::default();
        let noon = air_temperature_c(&weather, clock, DAY_CYCLE_TICKS / 4);
        let midnight = air_temperature_c(&weather, clock, DAY_CYCLE_TICKS * 3 / 4);
        assert!((noon - midnight - (DAY_WARMING_C + NIGHT_DROP_C)).abs() < 1e-3);
    }
}
