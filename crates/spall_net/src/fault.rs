//! Deterministic application-message fault injection.
//!
//! `docs/tasks.md` (T09): "Add deterministic application-message delay / drop /
//! reorder tests ... application faults do not reproduce QUIC retransmission /
//! congestion behavior."
//!
//! This channel operates on *decoded messages*, on a logical tick clock, with a
//! seeded PRNG. Given the same seed and the same sequence of `push` / `advance`
//! calls it produces byte-identical output, so a test can assert exact
//! delivery order under loss and reordering without any timing flakiness. For
//! real encrypted-packet loss see [`crate::proxy`].

use std::collections::BinaryHeap;

/// Knobs for [`FaultChannel`]. Ratios are probabilities in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppFaultPlan {
    /// PRNG seed. Any fixed value makes the run reproducible.
    pub seed: u64,
    /// Probability a pushed message is dropped outright.
    pub drop_ratio: f64,
    /// Probability a delivered message is also delivered a second time.
    pub duplicate_ratio: f64,
    /// Maximum extra delivery delay, in ticks. Only has an effect when
    /// [`Self::reorder`] is set.
    pub max_delay_ticks: u32,
    /// When true, per-message delay is randomised in `0..=max_delay_ticks`, so
    /// later messages can overtake earlier ones. When false the channel is
    /// order-preserving (delay is always `0`).
    pub reorder: bool,
}

impl AppFaultPlan {
    /// A lossless, order-preserving channel (identity).
    pub fn perfect(seed: u64) -> Self {
        Self {
            seed,
            drop_ratio: 0.0,
            duplicate_ratio: 0.0,
            max_delay_ticks: 0,
            reorder: false,
        }
    }
}

/// Counters describing what a [`FaultChannel`] has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaultStats {
    pub pushed: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub delivered: u64,
    /// Largest gap, in push-index units, between two consecutively delivered
    /// messages arriving out of push order. `0` means delivery stayed ordered.
    pub max_reorder_distance: u64,
}

struct Pending<T> {
    due: u64,
    order: u64,
    push_index: u64,
    msg: T,
}

impl<T> PartialEq for Pending<T> {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.order == other.order
    }
}
impl<T> Eq for Pending<T> {}
impl<T> PartialOrd for Pending<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Pending<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap on (due, order): reverse the natural ordering.
        (other.due, other.order).cmp(&(self.due, self.order))
    }
}

/// A deterministic delay / drop / reorder queue for decoded messages.
pub struct FaultChannel<T> {
    plan: AppFaultPlan,
    rng: SplitMix64,
    now: u64,
    seq: u64,
    push_index: u64,
    last_delivered_index: Option<u64>,
    queue: BinaryHeap<Pending<T>>,
    stats: FaultStats,
}

impl<T: Clone> FaultChannel<T> {
    /// Builds a channel from `plan`.
    pub fn new(plan: AppFaultPlan) -> Self {
        Self {
            plan,
            rng: SplitMix64::new(plan.seed),
            now: 0,
            seq: 0,
            push_index: 0,
            last_delivered_index: None,
            queue: BinaryHeap::new(),
            stats: FaultStats::default(),
        }
    }

    /// The current logical tick.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Counters so far.
    pub fn stats(&self) -> FaultStats {
        self.stats
    }

    /// Messages still scheduled for future delivery.
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }

    /// Submits `msg` at the current tick. It is dropped, scheduled once, or
    /// scheduled twice per the plan.
    pub fn push(&mut self, msg: T) {
        let idx = self.push_index;
        self.push_index += 1;
        self.stats.pushed += 1;

        if self.rng.next_f64() < self.plan.drop_ratio {
            self.stats.dropped += 1;
            return;
        }
        self.schedule(msg.clone(), idx);
        if self.plan.duplicate_ratio > 0.0 && self.rng.next_f64() < self.plan.duplicate_ratio {
            self.stats.duplicated += 1;
            self.schedule(msg, idx);
        }
    }

    fn schedule(&mut self, msg: T, push_index: u64) {
        let delay = if self.plan.reorder && self.plan.max_delay_ticks > 0 {
            self.rng.next_u64() % (self.plan.max_delay_ticks as u64 + 1)
        } else {
            0
        };
        let order = self.seq;
        self.seq += 1;
        self.queue.push(Pending {
            due: self.now + delay,
            order,
            push_index,
            msg,
        });
    }

    /// Advances the clock by `ticks` and returns everything now deliverable, in
    /// delivery order.
    pub fn advance(&mut self, ticks: u64) -> Vec<T> {
        self.now += ticks;
        self.drain_ready()
    }

    /// Delivers everything still queued regardless of its due tick. Use at
    /// teardown to prove nothing is stranded.
    pub fn flush(&mut self) -> Vec<T> {
        self.now = u64::MAX;
        self.drain_ready()
    }

    fn drain_ready(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(top) = self.queue.peek() {
            if top.due > self.now {
                break;
            }
            let item = self.queue.pop().unwrap();
            if let Some(prev) = self.last_delivered_index
                && item.push_index < prev
            {
                self.stats.max_reorder_distance =
                    self.stats.max_reorder_distance.max(prev - item.push_index);
            }
            self.last_delivered_index = Some(item.push_index);
            self.stats.delivered += 1;
            out.push(item.msg);
        }
        out
    }
}

/// Small deterministic PRNG (SplitMix64). Matches the generator the other
/// `spall_*` crates use for reproducible fixtures.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_channel_is_identity_in_order() {
        let mut ch = FaultChannel::new(AppFaultPlan::perfect(1));
        for i in 0..20 {
            ch.push(i);
        }
        let out = ch.advance(1);
        assert_eq!(out, (0..20).collect::<Vec<_>>());
        assert_eq!(ch.stats().dropped, 0);
        assert_eq!(ch.stats().max_reorder_distance, 0);
    }

    #[test]
    fn same_seed_same_result_different_seed_diverges() {
        let plan = AppFaultPlan {
            seed: 0xABCD,
            drop_ratio: 0.25,
            duplicate_ratio: 0.1,
            max_delay_ticks: 5,
            reorder: true,
        };
        let run = |plan: AppFaultPlan| {
            let mut ch = FaultChannel::new(plan);
            let mut got = Vec::new();
            for i in 0..200 {
                ch.push(i);
                got.extend(ch.advance(1));
            }
            got.extend(ch.flush());
            (got, ch.stats())
        };
        let (a, sa) = run(plan);
        let (b, sb) = run(plan);
        assert_eq!(a, b);
        assert_eq!(sa, sb);

        let (c, _) = run(AppFaultPlan {
            seed: 0x1234,
            ..plan
        });
        assert_ne!(a, c);
        // Every non-dropped push is delivered at least once by flush().
        assert_eq!(sa.delivered, sa.pushed - sa.dropped + sa.duplicated);
        assert!(sa.dropped > 0 && sa.max_reorder_distance > 0);
    }

    #[test]
    fn order_preserving_plan_never_reorders_even_with_loss() {
        let mut ch = FaultChannel::new(AppFaultPlan {
            seed: 7,
            drop_ratio: 0.3,
            duplicate_ratio: 0.0,
            max_delay_ticks: 9,
            reorder: false,
        });
        let mut got = Vec::new();
        for i in 0..100 {
            ch.push(i);
            got.extend(ch.advance(1));
        }
        got.extend(ch.flush());
        assert!(got.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ch.stats().max_reorder_distance, 0);
    }
}
