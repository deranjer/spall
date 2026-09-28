//! Drift-free fixed-rate tick pacing.
//!
//! The tick loop used to sleep `tick_dt - work_time` after every iteration.
//! That is a *relative* sleep: every iteration silently loses whatever the
//! loop spent outside the timed region plus the OS sleep overshoot (about a
//! millisecond on Windows), so the "60 Hz" server measured ~58.8 Hz in a
//! real session and a true-60 Hz client's prediction ran ahead by one extra
//! tick every ~0.85 s, forever. [`TickPacer`] schedules against *absolute*
//! deadlines instead: an overshoot in one tick shortens the next sleep, so
//! the long-run rate is exactly the configured rate.

use std::time::{Duration, Instant};

/// A tick this many periods late is not caught up by running ticks
/// back-to-back (that would be a burst of simulated time the clients cannot
/// tell from a stall); the schedule is re-anchored instead and the event
/// counted.
pub const MAX_LAG_PERIODS: u32 = 4;

/// Absolute-deadline pacer for a fixed-timestep loop.
#[derive(Debug)]
pub struct TickPacer {
    period: Duration,
    next: Instant,
    started: Instant,
    ticks: u64,
    resyncs: u64,
    max_lag: Duration,
}

impl TickPacer {
    /// Starts the schedule at `now`; the first tick's deadline is `now + period`.
    pub fn new(period: Duration, now: Instant) -> Self {
        Self {
            period,
            next: now + period,
            started: now,
            ticks: 0,
            resyncs: 0,
            max_lag: Duration::ZERO,
        }
    }

    /// Call once per finished tick at time `now`. Returns how long to sleep so
    /// the next tick starts on schedule (`None` when already late).
    pub fn finish_tick(&mut self, now: Instant) -> Option<Duration> {
        self.ticks += 1;
        let deadline = self.next;
        self.next += self.period;
        if now <= deadline {
            return Some(deadline - now);
        }
        let lag = now - deadline;
        self.max_lag = self.max_lag.max(lag);
        if lag > self.period * MAX_LAG_PERIODS {
            self.resyncs += 1;
            self.next = now + self.period;
            return None;
        }
        None
    }

    /// Ticks finished so far.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Times the schedule was re-anchored after a stall past [`MAX_LAG_PERIODS`].
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Worst observed lateness of a tick's completion past its deadline.
    pub fn max_lag(&self) -> Duration {
        self.max_lag
    }

    /// Mean achieved tick rate in Hz since construction, measured at `now`.
    pub fn achieved_hz(&self, now: Instant) -> f64 {
        let secs = now.saturating_duration_since(self.started).as_secs_f64();
        if secs > 0.0 {
            self.ticks as f64 / secs
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Duration = Duration::from_nanos(1_000_000_000 / 60);

    /// Runs `n` ticks against a simulated clock where each tick's work costs
    /// `work` and each sleep overshoots by `overshoot`.
    fn simulate(n: u64, work: Duration, overshoot: Duration) -> (TickPacer, Instant, Instant) {
        let t0 = Instant::now();
        let mut pacer = TickPacer::new(P, t0);
        let mut now = t0;
        for _ in 0..n {
            now += work;
            if let Some(rem) = pacer.finish_tick(now) {
                now += rem + overshoot;
            }
        }
        (pacer, t0, now)
    }

    #[test]
    fn oversleeping_every_tick_does_not_lower_the_long_run_rate() {
        // 1 ms overshoot per sleep is what the relative sleep lost: 16.667 ms
        // period + 1 ms = ~56.6 Hz. Absolute deadlines absorb it.
        let (pacer, t0, end) = simulate(3600, Duration::from_millis(2), Duration::from_millis(1));
        let hz = pacer.achieved_hz(end);
        assert!((hz - 60.0).abs() < 0.05, "achieved {hz} Hz");
        assert_eq!(pacer.resyncs(), 0);
        assert!(end - t0 >= P * 3599);
    }

    #[test]
    fn a_short_stall_is_caught_up_but_a_long_one_is_reanchored() {
        let t0 = Instant::now();
        let mut pacer = TickPacer::new(P, t0);
        // Two periods late: no sleep, schedule kept (catches up).
        assert_eq!(pacer.finish_tick(t0 + P * 3), None);
        assert_eq!(pacer.resyncs(), 0);
        // Stall far past MAX_LAG_PERIODS: re-anchored exactly one period ahead.
        let late = t0 + P * 50;
        assert_eq!(pacer.finish_tick(late), None);
        assert_eq!(pacer.resyncs(), 1);
        assert_eq!(pacer.finish_tick(late + P / 2), Some(P / 2));
    }
}
