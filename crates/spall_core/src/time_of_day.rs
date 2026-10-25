//! Time of day derived from the authoritative server tick.
//!
//! By default a Spall day is 72,000 60 Hz simulation ticks (20 real minutes)
//! and tick 0 is 06:00 sunrise. The server may configure another day length
//! and starting hour ([`DayClock`]) and sends the clock in the handshake, so
//! server systems and client lighting read one mapping and agree on when it is
//! night. The lunar cycle lasts eight Spall days and starts at full moon.

use serde::{Deserialize, Serialize};

pub const DAY_CYCLE_TICKS: u64 = 72_000;
pub const LUNAR_CYCLE_DAYS: u64 = 8;
/// Shortest configurable day: one real minute.
pub const MIN_DAY_TICKS: u32 = 3_600;
/// Longest configurable day: 24 real hours.
pub const MAX_DAY_TICKS: u32 = 24 * 3_600 * 60;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayNightState {
    pub day_index: u64,
    /// 0 = 06:00 sunrise, .25 = noon, .5 = 18:00 sunset, .75 = midnight.
    pub day_fraction: f32,
    /// [0,1), new moon at 0, full moon at 0.5.
    pub lunar_phase: f32,
    pub hour: u8,
    pub minute: u8,
}

impl DayNightState {
    /// Sine of the sun's elevation: 1 at noon, -1 at midnight. The sun is
    /// below the horizon (night) when this is negative.
    pub fn sun_height(&self) -> f32 {
        (self.day_fraction * std::f32::consts::TAU).sin()
    }
}

/// The server's day length and where tick 0 falls inside the day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayClock {
    /// Simulation ticks in one full day/night cycle.
    pub day_ticks: u32,
    /// Ticks past 06:00 at server tick 0. Always below `day_ticks`.
    pub start_offset_ticks: u32,
}

impl Default for DayClock {
    /// The 20-minute day starting at sunrise.
    fn default() -> Self {
        Self {
            day_ticks: DAY_CYCLE_TICKS as u32,
            start_offset_ticks: 0,
        }
    }
}

impl DayClock {
    /// A clock with `day_ticks` per day whose tick 0 is `start_hour` o'clock
    /// (0.0..24.0, so 15.5 is 15:30).
    pub fn new(day_ticks: u32, start_hour: f32) -> Result<Self, &'static str> {
        if !(MIN_DAY_TICKS..=MAX_DAY_TICKS).contains(&day_ticks) {
            return Err("day length must be between 1 minute and 24 hours");
        }
        if !(0.0..24.0).contains(&start_hour) {
            return Err("start hour must be in 0..24");
        }
        let after_six = f64::from(start_hour + 18.0) % 24.0;
        let offset = (after_six / 24.0 * f64::from(day_ticks)).round() as u32;
        Ok(Self {
            day_ticks,
            start_offset_ticks: offset % day_ticks,
        })
    }

    /// Whether a received clock is usable (no division by zero downstream).
    pub fn is_valid(&self) -> bool {
        (MIN_DAY_TICKS..=MAX_DAY_TICKS).contains(&self.day_ticks)
            && self.start_offset_ticks < self.day_ticks
    }

    /// Ticks in one hour of Spall time.
    pub fn hour_ticks(&self) -> u64 {
        u64::from(self.day_ticks) / 24
    }

    pub fn state_at(&self, server_tick: u64) -> DayNightState {
        let day_ticks = u64::from(self.day_ticks);
        let shifted = server_tick + u64::from(self.start_offset_ticks);
        let day_index = shifted / day_ticks;
        let within_day = shifted % day_ticks;
        let day_fraction = within_day as f32 / day_ticks as f32;
        let lunar_phase =
            (0.5 + (day_index % LUNAR_CYCLE_DAYS) as f32 / LUNAR_CYCLE_DAYS as f32).fract();
        let minutes_after_six = (day_fraction * 1_440.0).floor() as u32;
        let total_minutes = (360 + minutes_after_six) % 1_440;
        DayNightState {
            day_index,
            day_fraction,
            lunar_phase,
            hour: (total_minutes / 60) as u8,
            minute: (total_minutes % 60) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_at_tick(server_tick: u64) -> DayNightState {
        DayClock::default().state_at(server_tick)
    }

    #[test]
    fn clock_landmarks_wrap_and_split_day_from_night() {
        let sunrise = state_at_tick(0);
        let noon = state_at_tick(DAY_CYCLE_TICKS / 4);
        let sunset = state_at_tick(DAY_CYCLE_TICKS / 2);
        let midnight = state_at_tick(DAY_CYCLE_TICKS * 3 / 4);
        assert_eq!((sunrise.hour, sunrise.minute), (6, 0));
        assert_eq!((noon.hour, noon.minute), (12, 0));
        assert_eq!((sunset.hour, sunset.minute), (18, 0));
        assert_eq!((midnight.hour, midnight.minute), (0, 0));
        let next_sunrise = state_at_tick(DAY_CYCLE_TICKS);
        assert_eq!((next_sunrise.hour, next_sunrise.minute), (6, 0));
        assert_eq!(next_sunrise.day_index, 1);
        assert!(noon.sun_height() > 0.99);
        assert!(midnight.sun_height() < -0.99);
        // Midnight on a later day is still night.
        assert!(state_at_tick(DAY_CYCLE_TICKS * 5 + DAY_CYCLE_TICKS * 3 / 4).sun_height() < 0.0);
    }

    #[test]
    fn configured_length_and_start_move_the_clock() {
        // A 10-minute day starting at 15:30.
        let clock = DayClock::new(36_000, 15.5).unwrap();
        let start = clock.state_at(0);
        assert_eq!((start.hour, start.minute), (15, 30));
        assert_eq!(start.day_index, 0);
        // Half a day later it is 03:30 and night.
        let later = clock.state_at(18_000);
        assert_eq!((later.hour, later.minute), (3, 30));
        assert!(later.sun_height() < 0.0);
        // 18:00 sunset arrives 2.5 of 24 hours in: 3,750 ticks.
        let sunset = clock.state_at(3_750);
        assert_eq!((sunset.hour, sunset.minute), (18, 0));
        // 06:00 is 14.5 h in: the day counter rolls there.
        assert_eq!(clock.state_at(21_749).day_index, 0);
        assert_eq!(clock.state_at(21_750).day_index, 1);
    }

    #[test]
    fn default_clock_keeps_the_72000_tick_day() {
        let clock = DayClock::default();
        assert_eq!(clock.day_ticks, 72_000);
        assert_eq!(clock.start_offset_ticks, 0);
        assert!(clock.is_valid());
        for tick in [0, 1, 17_999, 36_000, 71_999, 72_000, 500_000] {
            assert_eq!(clock.state_at(tick), state_at_tick(tick));
        }
        assert_eq!(state_at_tick(72_000).day_index, 1);
    }

    #[test]
    fn invalid_clocks_are_refused() {
        assert!(DayClock::new(0, 6.0).is_err());
        assert!(DayClock::new(36_000, 24.0).is_err());
        assert!(
            !DayClock {
                day_ticks: 0,
                start_offset_ticks: 0
            }
            .is_valid()
        );
        assert!(
            !DayClock {
                day_ticks: 36_000,
                start_offset_ticks: 36_000
            }
            .is_valid()
        );
    }

    #[test]
    fn lunar_phase_waxes_and_wanes_over_eight_days() {
        let ticks_per_day = DAY_CYCLE_TICKS;
        assert!((state_at_tick(0).lunar_phase - 0.5).abs() < 1e-6); // full
        assert!((state_at_tick(ticks_per_day).lunar_phase - 0.625).abs() < 1e-6);
        assert!((state_at_tick(ticks_per_day * 4).lunar_phase).abs() < 1e-6); // new
        assert!((state_at_tick(ticks_per_day * 8).lunar_phase - 0.5).abs() < 1e-6);
    }
}
