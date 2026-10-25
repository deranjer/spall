//! Presentation lighting for the tick-derived clock.
//!
//! The tick -> time-of-day mapping lives in [`spall_core::time_of_day`] so
//! server systems and client lighting agree on when it is night. No separate
//! client wall clock is used, so connected players see the same celestial state.

use glam::Vec3;
use spall_core::time_of_day::{DayClock, DayNightState};
use spall_render::Environment;

const TAU: f32 = std::f32::consts::TAU;

#[allow(clippy::field_reassign_with_default)]
pub fn environment_at_tick(server_tick: u64, clock: DayClock) -> (Environment, DayNightState) {
    let state = clock.state_at(server_tick);
    let sun_angle = state.day_fraction * TAU;
    let sin_sun = state.sun_height();
    let cos_sun = sun_angle.cos();
    let sun_position = Vec3::new(cos_sun, sin_sun, 0.0);
    let day_light = smoothstep(-0.16, 0.22, sin_sun);
    let twilight = (1.0 - (sin_sun.abs() * 6.0).clamp(0.0, 1.0)).powi(2);

    // The moon's elongation follows its eight-day phase: new moon near the
    // sun, full moon opposite it. This lets quarter moons appear in daylight.
    let moon_angle = sun_angle + state.lunar_phase * TAU;
    let (sin_moon, cos_moon) = moon_angle.sin_cos();
    let moon_position = Vec3::new(cos_moon, sin_moon, 0.0);
    let illumination = 0.5 - 0.5 * (state.lunar_phase * TAU).cos();
    let moon_above = smoothstep(-0.16, 0.12, sin_moon);
    let nighttime = 1.0 - day_light;

    let mut environment = Environment::default();
    environment.sun_dir = -sun_position;
    environment.sun_color = mix3([1.0, 0.55, 0.30], [1.0, 0.96, 0.88], day_light);
    environment.sun_intensity = 5.0 * day_light;
    environment.sun_visibility = smoothstep(-0.12, 0.06, sin_sun);
    environment.sky = mix3([0.075, 0.095, 0.16], [0.30, 0.40, 0.60], day_light);
    environment.ground = mix3([0.028, 0.034, 0.055], [0.14, 0.13, 0.11], day_light);
    // Keep an intentional base night fill, then lift clear full-moon nights.
    for channel in &mut environment.sky {
        *channel += 0.035 * illumination * nighttime;
    }
    environment.sun_angular_diameter_deg = 1.0;
    environment.exposure = 1.0;
    let night = [8.0, 13.0, 32.0];
    let day = [120.0, 170.0, 220.0];
    let dusk = [228.0, 116.0, 72.0];
    environment.background = mix_rgb(mix_rgb(night, day, day_light), dusk, twilight * 0.72)
        .map(|channel| channel.round().clamp(0.0, 255.0) as u8);
    environment.moon_dir = -moon_position;
    environment.moon_color = [0.55, 0.66, 0.95];
    environment.moon_intensity = 0.11 * illumination * moon_above * (0.25 + 0.75 * nighttime);
    // The daytime disc is intentionally subtle, and it vanishes below the
    // horizon regardless of phase.
    environment.moon_visibility = moon_above * illumination * (0.12 + 0.88 * nighttime);
    environment.moon_phase = state.lunar_phase;
    (environment, state)
}

/// Select tick-driven lighting or preserve the caller's manual environment.
/// `None` state means the caller should retain its manual clock label too.
pub fn environment_for_clock(
    server_tick: u64,
    clock: DayClock,
    cycle_enabled: bool,
    manual_environment: Environment,
) -> (Environment, Option<DayNightState>) {
    if cycle_enabled {
        let (environment, state) = environment_at_tick(server_tick, clock);
        (environment, Some(state))
    } else {
        (manual_environment, None)
    }
}

fn smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    let t = ((value - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn mix_rgb(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::time_of_day::DAY_CYCLE_TICKS;

    const CLOCK: DayClock = DayClock {
        day_ticks: 72_000,
        start_offset_ticks: 0,
    };

    #[test]
    fn twenty_minute_cycle_has_minecraft_like_landmarks_and_wraps() {
        let sunrise = CLOCK.state_at(0);
        let noon = CLOCK.state_at(DAY_CYCLE_TICKS / 4);
        let sunset = CLOCK.state_at(DAY_CYCLE_TICKS / 2);
        let midnight = CLOCK.state_at(DAY_CYCLE_TICKS * 3 / 4);
        assert_eq!((sunrise.hour, sunrise.minute), (6, 0));
        assert_eq!((noon.hour, noon.minute), (12, 0));
        assert_eq!((sunset.hour, sunset.minute), (18, 0));
        assert_eq!((midnight.hour, midnight.minute), (0, 0));
        let next_sunrise = CLOCK.state_at(DAY_CYCLE_TICKS);
        assert_eq!((next_sunrise.hour, next_sunrise.minute), (6, 0));
        assert_eq!(next_sunrise.day_index, 1);
        let (noon_env, _) = environment_at_tick(DAY_CYCLE_TICKS / 4, CLOCK);
        let (night_env, _) = environment_at_tick(DAY_CYCLE_TICKS * 3 / 4, CLOCK);
        assert!(noon_env.sun_dir.y < -0.99 && noon_env.sun_intensity > 4.9);
        assert!(night_env.sun_dir.y > 0.99 && night_env.sun_intensity < 0.01);
        assert!(night_env.sky.iter().all(|light| *light >= 0.075));
    }

    #[test]
    fn quarter_moon_can_be_faintly_visible_in_daylight() {
        // Day six is first quarter; 10:00 puts it above the morning horizon.
        let tick = DAY_CYCLE_TICKS * 6 + DAY_CYCLE_TICKS / 6;
        let (environment, state) = environment_at_tick(tick, CLOCK);
        assert!((state.lunar_phase - 0.25).abs() < 1e-5);
        assert!(environment.sun_intensity > 3.0);
        assert!(environment.moon_visibility > 0.0);
    }

    #[test]
    fn sun_moon_separation_tracks_lunar_phase() {
        let (new_moon, _) = environment_at_tick(DAY_CYCLE_TICKS * 4, CLOCK);
        let (full_moon, _) = environment_at_tick(0, CLOCK);
        assert!(new_moon.sun_dir.dot(new_moon.moon_dir) > 0.999);
        assert!(full_moon.sun_dir.dot(full_moon.moon_dir) < -0.999);
    }

    #[test]
    fn configured_day_drives_the_label_and_the_sun() {
        // A 10-minute day starting at 15:30: 18:00 is 3,750 ticks in, and
        // half a day later (03:30) the sun is down.
        let clock = DayClock::new(36_000, 15.5).unwrap();
        let (dusk, state) = environment_at_tick(3_750, clock);
        assert_eq!((state.hour, state.minute), (18, 0));
        assert!(dusk.sun_intensity < 2.0);
        let (night, state) = environment_at_tick(18_000, clock);
        assert_eq!((state.hour, state.minute), (3, 30));
        assert!(night.sun_intensity < 0.01);
        // The default clock at the same tick is a different hour.
        assert_ne!(environment_at_tick(18_000, CLOCK).1.hour, 3);
    }

    #[test]
    fn disabling_the_cycle_preserves_manual_lighting() {
        let manual = spall_render::EnvironmentPreset::Sunset.environment();
        let (selected, state) = environment_for_clock(12345, DayClock::default(), false, manual);
        assert_eq!(selected, manual);
        assert!(state.is_none());
    }
}
