//! Time of day derived from the authoritative server tick.
//!
//! A Spall day is 72,000 60 Hz simulation ticks (20 real minutes). The lunar
//! cycle lasts eight Spall days and starts at full moon. Server systems and
//! client lighting both read this mapping, so they agree on when it is night.

pub const DAY_CYCLE_TICKS: u64 = 72_000;
pub const LUNAR_CYCLE_DAYS: u64 = 8;

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

pub fn state_at_tick(server_tick: u64) -> DayNightState {
    let day_index = server_tick / DAY_CYCLE_TICKS;
    let within_day = server_tick % DAY_CYCLE_TICKS;
    let day_fraction = within_day as f32 / DAY_CYCLE_TICKS as f32;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn lunar_phase_waxes_and_wanes_over_eight_days() {
        let ticks_per_day = DAY_CYCLE_TICKS;
        assert!((state_at_tick(0).lunar_phase - 0.5).abs() < 1e-6); // full
        assert!((state_at_tick(ticks_per_day).lunar_phase - 0.625).abs() < 1e-6);
        assert!((state_at_tick(ticks_per_day * 4).lunar_phase).abs() < 1e-6); // new
        assert!((state_at_tick(ticks_per_day * 8).lunar_phase - 0.5).abs() < 1e-6);
    }
}
