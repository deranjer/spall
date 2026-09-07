//! Per-stream sequence gates: the deduplication plumbing.
//!
//! `docs/protocol.md`: "Duplicate transactions are ignored using session/stream
//! sequencing and IDs." A QUIC control stream is already ordered and
//! exactly-once, but a client that reconnects, or a datagram path, can replay a
//! record. Every inbound record carries a per-stream `u64` sequence; this gate
//! decides accept / drop.
//!
//! This is a thin, deterministic wrapper over
//! [`spall_protocol::SequenceGate`] that also keeps a small window of recently
//! seen sequence numbers so an out-of-order-but-fresh datagram (motion) is not
//! mistaken for a duplicate.

use std::collections::VecDeque;

use spall_protocol::{SeqVerdict, SequenceGate};

/// What [`StreamDeduper::admit`] decided about one sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupVerdict {
    /// Not seen before; caller should process it.
    Accept {
        /// Sequence numbers skipped since the previous high-water mark. `0` for
        /// a contiguous stream.
        gap: u64,
    },
    /// Already delivered (a replay); caller must drop it.
    Duplicate,
}

/// Tracks delivered sequence numbers for one logical stream.
///
/// `reorder_window` controls how far below the high-water mark a *not
/// previously seen* sequence is still accepted (used for datagrams). Set it to
/// `0` for a strictly increasing stream such as the control channel.
#[derive(Debug)]
pub struct StreamDeduper {
    gate: SequenceGate,
    reorder_window: u64,
    recent: VecDeque<u64>,
    accepted: u64,
    duplicates: u64,
}

impl StreamDeduper {
    /// A strictly-increasing gate (control stream).
    pub fn strict() -> Self {
        Self::with_window(0)
    }

    /// A gate that tolerates `window` worth of out-of-order-but-new sequence
    /// numbers below the high-water mark (datagrams).
    pub fn with_window(window: u64) -> Self {
        Self {
            gate: SequenceGate::new(),
            reorder_window: window,
            recent: VecDeque::new(),
            accepted: 0,
            duplicates: 0,
        }
    }

    /// Number of admitted / rejected records so far.
    pub fn stats(&self) -> (u64, u64) {
        (self.accepted, self.duplicates)
    }

    /// The highest sequence number admitted so far.
    pub fn high_water(&self) -> Option<u64> {
        self.gate.high_water()
    }

    /// Decides whether `seq` is fresh.
    pub fn admit(&mut self, seq: u64) -> DedupVerdict {
        match self.gate.observe(seq) {
            SeqVerdict::Fresh { gap } => {
                self.remember(seq);
                self.accepted += 1;
                DedupVerdict::Accept { gap }
            }
            SeqVerdict::Duplicate => {
                let hw = self.gate.high_water().unwrap_or(0);
                let within_window = self.reorder_window > 0
                    && seq >= hw.saturating_sub(self.reorder_window)
                    && !self.recent.contains(&seq);
                if within_window {
                    self.remember(seq);
                    self.accepted += 1;
                    DedupVerdict::Accept { gap: 0 }
                } else {
                    self.duplicates += 1;
                    DedupVerdict::Duplicate
                }
            }
        }
    }

    fn remember(&mut self, seq: u64) {
        self.recent.push_back(seq);
        let cap = (self.reorder_window as usize).max(1) * 4;
        while self.recent.len() > cap {
            self.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_gate_rejects_every_replay_and_reports_gaps() {
        let mut d = StreamDeduper::strict();
        assert_eq!(d.admit(1), DedupVerdict::Accept { gap: 1 });
        assert_eq!(d.admit(2), DedupVerdict::Accept { gap: 0 });
        assert_eq!(d.admit(2), DedupVerdict::Duplicate);
        assert_eq!(d.admit(1), DedupVerdict::Duplicate);
        assert_eq!(d.admit(5), DedupVerdict::Accept { gap: 2 });
        assert_eq!(d.stats(), (3, 2));
    }

    #[test]
    fn windowed_gate_accepts_late_but_new_and_still_drops_true_replays() {
        let mut d = StreamDeduper::with_window(8);
        assert_eq!(d.admit(10), DedupVerdict::Accept { gap: 10 });
        assert_eq!(d.admit(12), DedupVerdict::Accept { gap: 1 });
        // 11 arrives late but was never delivered: accept.
        assert_eq!(d.admit(11), DedupVerdict::Accept { gap: 0 });
        // 11 again: now a real duplicate.
        assert_eq!(d.admit(11), DedupVerdict::Duplicate);
        // Far below the window: treated as a replay.
        assert_eq!(d.admit(1), DedupVerdict::Duplicate);
    }

    #[test]
    fn windowed_gate_handles_sequences_near_u64_max_without_overflow() {
        let mut d = StreamDeduper::with_window(256);
        assert_eq!(d.admit(u64::MAX), DedupVerdict::Accept { gap: u64::MAX });
        assert_eq!(d.admit(u64::MAX - 1), DedupVerdict::Accept { gap: 0 });
        assert_eq!(d.admit(u64::MAX - 1), DedupVerdict::Duplicate);
    }
}
