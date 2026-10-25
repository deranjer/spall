//! The Winter Crossing scenario profile (ENG-269): a held winter with frequent
//! random blizzards and heavy snow, and enough animals across the 1 km map that
//! a player crossing it will very likely see some. Selected by
//! `sandbox-server --scenario-winter`; the default sandbox is unchanged.
//!
//! The weather schedule itself lives in
//! [`spall_protocol::WeatherProfile::WinterCrossing`]; this module holds the
//! parts that are game balance: how long the season is held and where and how
//! many animals are born.

//!
//! ENG-251 adds the scenario itself: the buildings' [`layout`], the server
//! [`rules`] (objective, death, win) and, for the client, the [`hud`].

#[cfg(feature = "client")]
pub mod hud;
pub mod layout;
pub mod rules;

/// Real minutes in one in-game day under the scenario: a crossing takes about
/// a day, so it spans a full night.
pub const DAY_MINUTES: f32 = 11.0;
/// Hour of the day the scenario starts at: mid-afternoon, so night falls
/// partway across.
pub const START_HOUR: f32 = 15.5;

/// Generates the world `preset`, `seed`, `size_cells` in winter with the
/// scenario's buildings laid out in it (see [`layout`]).
pub fn generate(
    preset: &str,
    seed: u64,
    size_cells: u32,
) -> Result<
    (crate::worldgen_scene::GeneratedScene, layout::Layout),
    crate::worldgen_scene::WorldgenSceneError,
> {
    use crate::worldgen_scene::WorldgenSceneError;
    let buildings =
        layout::Buildings::load(&layout::fixtures_dir()).map_err(WorldgenSceneError::Layout)?;
    let mut laid_out = None;
    let scene = crate::worldgen_scene::generate_with(
        preset,
        seed,
        size_cells,
        spall_ecology::living::Season::Winter,
        |world| {
            let (layout, kept_clear) = layout::lay_out(world, &buildings)?;
            laid_out = Some(layout);
            Ok(kept_clear)
        },
    )?;
    let layout = laid_out.expect("the build step ran");
    let scene = scene
        .with_held_season(HELD_SEASON_MS)
        .with_spawns(layout.spawns.clone());
    Ok((scene, layout))
}

/// Length of one season under the profile: about 31 years, so a winter start
/// stays winter for the life of any world.
pub const HELD_SEASON_MS: u64 = 1_000_000_000_000;

/// Animals born at world start, in [`crate::animals::species`] order: deer,
/// crow, trout, bear, snow leopard, wolf (three packs of four).
pub const ANIMAL_TARGETS: [u32; 6] = [120, 60, 12, 4, 6, 12];

/// Spawn ring around each anchor, metres. Rings of this size around anchors
/// [`ANCHOR_SPACING_M`] apart cover the map without gaps.
pub const ANIMAL_RADIUS_M: (f64, f64) = (0.0, 150.0);

/// Distance between animal spawn anchors, metres.
pub const ANCHOR_SPACING_M: f64 = 200.0;

/// Animal spawn anchors on a grid across a square map `size_cells` wide
/// (0.25 m cells), all at height `y_m`: the height a snow leopard's
/// "high ground" is measured from.
pub fn animal_anchors(size_cells: u32, y_m: f64) -> Vec<[f64; 3]> {
    let side_m = f64::from(size_cells) * 0.25;
    let n = (side_m / ANCHOR_SPACING_M).floor().max(1.0) as u32;
    let step = side_m / f64::from(n);
    (0..n)
        .flat_map(|i| (0..n).map(move |j| (i, j)))
        .map(|(i, j)| {
            [
                (f64::from(i) + 0.5) * step,
                y_m,
                (f64::from(j) + 0.5) * step,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::{
        WEATHER_PROFILE_MS, WINTER_CROSSING_FRONT_MS, WeatherClock, WeatherClockSource,
        WeatherKind, WeatherProfile, WeatherState,
    };

    /// 20 minutes: the default in-game day (`spall_core::time_of_day::DAY_CYCLE_TICKS`).
    const DAY_MS: u64 = 1_200_000;

    fn state(profile: WeatherProfile, seed: u64, time_ms: u64, start_season: u8) -> WeatherState {
        WeatherState::for_clock_with(
            profile,
            seed,
            1,
            0,
            WeatherClock {
                time_ms,
                source: WeatherClockSource::Vegetation,
                start_season,
                season_ms: HELD_SEASON_MS,
            },
            0,
        )
    }

    #[test]
    fn the_profile_forces_winter_whatever_the_start_season() {
        for start in 0..4 {
            for step in 0..400u64 {
                let s = state(WeatherProfile::WinterCrossing, 7, step * 60_000, start);
                assert!(s.temperature_centi_c <= -700, "{start}: {s:?}");
                assert!(
                    matches!(
                        s.kind,
                        WeatherKind::Snow
                            | WeatherKind::Blizzard
                            | WeatherKind::Clear
                            | WeatherKind::HighWind
                    ),
                    "{:?} in the winter profile",
                    s.kind
                );
                s.validate().unwrap();
            }
        }
    }

    #[test]
    fn blizzards_come_every_day_or_two_and_snow_is_mostly_heavy() {
        let days = 400;
        for seed in [1u64, 2, 3] {
            let (mut onsets, mut snow, mut heavy_or_medium) = (0u32, 0u32, 0u32);
            let mut previous = false;
            let fronts = days * DAY_MS / WINTER_CROSSING_FRONT_MS;
            for front in 0..fronts {
                let s = state(
                    WeatherProfile::WinterCrossing,
                    seed,
                    front * WINTER_CROSSING_FRONT_MS,
                    3,
                );
                let blizzard = s.kind == WeatherKind::Blizzard;
                onsets += u32::from(blizzard && !previous);
                previous = blizzard;
                if s.kind == WeatherKind::Snow {
                    snow += 1;
                    heavy_or_medium += u32::from(s.precipitation_q16 >= 32_768);
                }
            }
            let days_per_blizzard = days as f64 / f64::from(onsets);
            assert!(
                (1.0..=2.0).contains(&days_per_blizzard),
                "seed {seed}: a blizzard every {days_per_blizzard:.2} days"
            );
            assert!(
                f64::from(heavy_or_medium) / f64::from(snow) >= 0.8,
                "seed {seed}: {heavy_or_medium} of {snow} snowfalls medium or heavy"
            );
        }
    }

    #[test]
    fn the_default_profile_is_unchanged() {
        let clock = |time_ms| WeatherClock {
            time_ms,
            source: WeatherClockSource::ServerFallback,
            start_season: 1,
            season_ms: 900_000,
        };
        for step in 0..200u64 {
            let t = step * 400_000;
            assert_eq!(
                WeatherState::for_clock(9, 1, 5, clock(t), 0),
                WeatherState::for_clock_with(WeatherProfile::Standard, 9, 1, 5, clock(t), 0)
            );
        }
        // Summer under the default profile is warm, not winter.
        let summer = state(WeatherProfile::Standard, 9, 0, 1);
        assert!(summer.temperature_centi_c > 1000);
        assert_eq!(WEATHER_PROFILE_MS, 1_800_000);
    }

    #[test]
    fn anchors_cover_a_square_kilometre() {
        let anchors = animal_anchors(4096, 3.0);
        assert_eq!(anchors.len(), 25);
        assert!(anchors.iter().all(|a| (0.0..1024.0).contains(&a[0])));
    }
}
