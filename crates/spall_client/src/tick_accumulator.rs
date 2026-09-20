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
        self.advance_scaled(elapsed, 1.0)
    }

    /// [`Self::advance`] with the local timeline run at `rate` x real time
    /// (`1.0` = exactly the fixed tick rate). The physics step itself is
    /// unchanged — only how often whole fixed ticks are *issued* — so a
    /// [`LeadController`] can slew the prediction clock against the server's
    /// without ever changing `dt`.
    pub fn advance_scaled(&mut self, elapsed: Duration, rate: f64) -> u32 {
        self.carry += elapsed.mul_f64(rate.max(0.0));
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

/// Keeps the client's prediction timeline a bounded distance ahead of the
/// server's, however long the session runs.
///
/// The client runs fixed ticks from its own clock; the server runs its own. Any
/// rate mismatch (even 1.7%, as measured from a 58.8 Hz server against a 60 Hz
/// client) makes the number of unacknowledged predicted ticks — the "lead",
/// `records_replayed` in the correction log — grow without bound, and every
/// reconcile then replays that many steps against stale inputs. Re-tagging the
/// surviving records preserves their count, so nothing else ever removes the
/// excess.
///
/// The right lead is one round trip (input out, snapshot back) plus a margin.
/// The controller measures the round trip from input-frame acks, and each time
/// a reconcile reports the true lead, nudges the timeline rate by at most
/// [`Self::MAX_SLEW`] (5%) toward it. A lead far beyond the target slows the
/// timeline to [`Self::MIN_RATE`] (never a full stop, so the player never
/// freezes on one delayed snapshot) instead of dropping queued inputs; the
/// queue drains as acknowledgements arrive and the slow-down releases itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeadController {
    rtt_s: Option<f64>,
    lead: Option<u32>,
}

impl LeadController {
    pub const TICK_HZ: f64 = 60.0;
    /// Ticks of margin on top of the measured round trip (jitter, one publish).
    pub const MARGIN_TICKS: f64 = 1.0;
    /// Never target less than this many ticks of lead.
    pub const MIN_TARGET_TICKS: f64 = 2.0;
    /// Never target more than this (a ~400 ms round trip): a worse link is
    /// served by a bounded lead plus corrections, not an unbounded one.
    pub const MAX_TARGET_TICKS: f64 = 24.0;
    /// No slewing while within this many ticks of the target.
    pub const DEAD_BAND_TICKS: f64 = 1.0;
    /// Beyond `target + HOLD_EXCESS_TICKS` the timeline runs at `MIN_RATE`.
    pub const HOLD_EXCESS_TICKS: f64 = 12.0;
    /// Slowest timeline rate (the bounded catch-down when far ahead).
    pub const MIN_RATE: f64 = 0.5;
    /// Largest speed-up / slow-down applied to the timeline.
    pub const MAX_SLEW: f64 = 0.05;
    /// Weight of each new round-trip sample in the running average.
    const RTT_EMA: f64 = 0.5;

    /// Folds in one round-trip sample (input sent -> snapshot acknowledging it).
    pub fn observe_rtt(&mut self, sample: Duration) {
        let s = sample.as_secs_f64();
        self.rtt_s = Some(match self.rtt_s {
            Some(prev) => prev + (s - prev) * Self::RTT_EMA,
            None => s,
        });
    }

    /// Records the lead a reconcile just observed: predicted ticks the
    /// snapshot's server tick had not yet covered.
    pub fn observe_lead(&mut self, lead_ticks: usize) {
        self.lead = Some(lead_ticks.min(u32::MAX as usize) as u32);
    }

    /// The lead to steer toward, in ticks.
    pub fn target_lead_ticks(&self) -> f64 {
        let rtt_ticks = self.rtt_s.unwrap_or(0.0) * Self::TICK_HZ;
        (rtt_ticks + Self::MARGIN_TICKS).clamp(Self::MIN_TARGET_TICKS, Self::MAX_TARGET_TICKS)
    }

    /// The most recently observed lead, if any reconcile has reported one.
    pub fn lead_ticks(&self) -> Option<u32> {
        self.lead
    }

    /// Timeline rate multiplier for [`TickAccumulator::advance_scaled`].
    pub fn rate(&self) -> f64 {
        let Some(lead) = self.lead else { return 1.0 };
        let target = self.target_lead_ticks();
        let error = f64::from(lead) - target;
        if error > Self::HOLD_EXCESS_TICKS {
            Self::MIN_RATE
        } else if error.abs() <= Self::DEAD_BAND_TICKS {
            1.0
        } else {
            1.0 - (error * 0.02).clamp(-Self::MAX_SLEW, Self::MAX_SLEW)
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

    /// Closed loop: a client whose clock is faster than the server's by the
    /// measured 1.7% (server 58.82 Hz vs client 60 Hz), for ten minutes. Without
    /// the controller the lead grows ~1 tick per 0.85 s (~700 ticks); with it,
    /// the lead settles at the target and stays there.
    #[test]
    fn a_1_7_percent_clock_mismatch_leaves_the_lead_bounded_over_ten_minutes() {
        const SERVER_HZ: f64 = 58.82;
        let tick = Duration::from_nanos(1_000_000_000 / 60);
        let mut acc = TickAccumulator::new(tick);
        let mut ctl = LeadController::default();
        ctl.observe_rtt(Duration::from_millis(40));
        let frame = Duration::from_micros(16_667);
        let (mut client_ticks, mut server_ticks) = (0.0f64, 0.0f64);
        let (mut lead_max, mut lead_end) = (0.0f64, 0.0f64);
        let frames = 600 * 60;
        for i in 0..frames {
            client_ticks += f64::from(acc.advance_scaled(frame, ctl.rate()));
            server_ticks += SERVER_HZ / 60.0;
            // A snapshot lands every 3 server ticks; the client sees the
            // difference between its own tick count and that snapshot's tick.
            if i % 3 == 0 {
                let lead = (client_ticks - server_ticks).max(0.0);
                ctl.observe_lead(lead as usize);
                lead_end = lead;
                if i > 60 * 20 {
                    lead_max = lead_max.max(lead);
                }
            }
        }
        let target = ctl.target_lead_ticks();
        assert!(
            lead_max <= target + LeadController::HOLD_EXCESS_TICKS,
            "lead reached {lead_max} (target {target})"
        );
        assert!(
            (lead_end - target).abs() <= LeadController::DEAD_BAND_TICKS + 1.0,
            "lead ended at {lead_end}, target {target}"
        );
    }

    #[test]
    fn a_lead_far_past_the_target_slows_the_timeline_without_dropping_ticks() {
        let mut ctl = LeadController::default();
        ctl.observe_lead(80);
        assert_eq!(ctl.rate(), LeadController::MIN_RATE);
        ctl.observe_lead(ctl.target_lead_ticks() as usize);
        assert_eq!(ctl.rate(), 1.0);
        ctl.observe_lead(ctl.target_lead_ticks() as usize + 6);
        assert!((0.94..1.0).contains(&ctl.rate()), "slews down, bounded");
    }
}
