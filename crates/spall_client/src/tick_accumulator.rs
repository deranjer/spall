//! A fixed-timestep catch-up accumulator (ENG-69, PR #109 review finding).
//!
//! The interactive mover loop (`net.rs`) wants to call
//! [`crate::predict::PredictedPlayer::tick`] once per `MOVEMENT_DT_S`-wide
//! server tick that has actually elapsed in real time — not once per loop
//! iteration, since a loop iteration's own real cost varies (a slow
//! terrain-hash/clone pass, a scheduler hiccup) and can easily exceed one
//! tick's worth of time. Calling `tick()` only once per iteration regardless
//! silently starves `Record::tick`'s tagging scheme (`predict.rs`) of the
//! "local tick count tracks real elapsed ticks" correspondence it depends
//! on: confirmed against a real interactive session's own correction log
//! (`.local/runs/interactive-corrections.jsonl`, 2026-09-12) showing 844 of
//! 844 reconciles unmatched and `records_replayed` pinned at 0 for the
//! entire session — every single reconcile was silently resetting
//! prediction to bare authority instead of ever replaying local input,
//! because the mover's own per-iteration cost meant only ~1 local tick ran
//! for every 3 real server ticks.
//!
//! [`TickAccumulator`] is the standard fixed-timestep "accumulator" pattern:
//! it tracks real elapsed time and reports how many whole ticks that now
//! covers, keeping the leftover fractional remainder for next time so a
//! consistently-slightly-slow loop doesn't permanently lose time to
//! flooring. The mover loop calls [`TickAccumulator::advance`] once per
//! iteration and calls `tick()` that many times (with the same sampled
//! input each time — already a supported, tested pattern, see
//! `g1_realistic_input_timing_trace.rs`'s held-input-reuse modelling).

use std::time::Duration;

/// How many ticks one [`TickAccumulator::advance`] call will ever report at
/// once, regardless of how much real time has passed. Bounds one mover
/// iteration's replay-catch-up cost after a genuine long stall (a debugger
/// pause, the OS suspending the process) — `PredictedPlayer::reconcile`'s
/// own per-interval re-anchoring (see its doc) already self-heals from a
/// client that falls further behind than this on its own, the next time a
/// snapshot arrives; this cap only stops one iteration from trying to
/// replay an unbounded backlog in one go. `8` ticks (~133ms at 60Hz) is
/// comfortably past ordinary jitter (the interactive session's own
/// measured worst case was a ~3x slowdown, i.e. 3 ticks) while still small
/// enough that one slow iteration can't stall the loop doing catch-up work.
pub const MAX_CATCHUP_TICKS: u32 = 8;

/// Accumulates real elapsed time and reports how many whole `tick_dt`-sized
/// ticks it now covers, keeping the leftover fractional remainder for the
/// next call.
pub struct TickAccumulator {
    tick_dt: Duration,
    carry: Duration,
}

impl TickAccumulator {
    pub fn new(tick_dt: Duration) -> Self {
        Self {
            tick_dt,
            carry: Duration::ZERO,
        }
    }

    /// `elapsed` real time since the last call (or since construction, for
    /// the first). Returns how many whole ticks that covers, capped at
    /// [`MAX_CATCHUP_TICKS`] — beyond the cap the whole backlog (not just
    /// the excess) is dropped, not carried forward, so a single
    /// catastrophic stall can't force an oversized catch-up on every later
    /// call too; normal operation instead preserves the exact fractional
    /// remainder so a consistently slightly-slow or slightly-fast loop
    /// tracks real time exactly over the long run rather than drifting from
    /// repeated flooring.
    pub fn advance(&mut self, elapsed: Duration) -> u32 {
        self.carry += elapsed;
        let ticks = (self.carry.as_secs_f64() / self.tick_dt.as_secs_f64()).floor() as u32;
        if ticks <= MAX_CATCHUP_TICKS {
            self.carry -= self.tick_dt * ticks;
            ticks
        } else {
            self.carry = Duration::ZERO;
            MAX_CATCHUP_TICKS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK_DT: Duration = Duration::from_millis(16); // ~62.5 Hz, close enough to 1/60s for these tests

    #[test]
    fn steady_cadence_returns_one_tick_per_call() {
        let mut acc = TickAccumulator::new(TICK_DT);
        for _ in 0..20 {
            assert_eq!(acc.advance(TICK_DT), 1);
        }
    }

    #[test]
    fn fractional_time_accumulates_across_calls_instead_of_being_lost() {
        let mut acc = TickAccumulator::new(TICK_DT);
        let half = TICK_DT / 2;
        assert_eq!(acc.advance(half), 0, "half a tick should not fire yet");
        assert_eq!(
            acc.advance(half),
            1,
            "the two halves together should fire exactly one tick"
        );
        // Confirm no drift: repeating this pattern stays exactly in sync
        // over many calls rather than slowly losing or gaining time.
        let mut ticks_seen = 0u32;
        for _ in 0..200 {
            ticks_seen += acc.advance(half);
        }
        assert_eq!(ticks_seen, 100);
    }

    #[test]
    fn a_slow_iteration_reports_multiple_ticks_to_catch_up() {
        // The real bug's exact signature: one mover iteration costs ~3
        // ticks' worth of real time (terrain-hash/clone overhead), so
        // catching up must report 3, not 1.
        let mut acc = TickAccumulator::new(TICK_DT);
        assert_eq!(acc.advance(TICK_DT * 3), 3);
    }

    #[test]
    fn a_catastrophic_stall_is_capped_and_does_not_leak_into_later_calls() {
        let mut acc = TickAccumulator::new(TICK_DT);
        assert_eq!(acc.advance(TICK_DT * 1000), MAX_CATCHUP_TICKS);
        // The excess was dropped outright, not carried — the very next
        // ordinary call behaves exactly like a fresh accumulator's would.
        assert_eq!(acc.advance(TICK_DT), 1);
    }

    #[test]
    fn zero_elapsed_time_reports_no_ticks() {
        let mut acc = TickAccumulator::new(TICK_DT);
        assert_eq!(acc.advance(Duration::ZERO), 0);
    }
}
