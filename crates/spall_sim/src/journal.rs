//! A stub authoritative journal built from real [`spall_protocol`] DTOs.
//!
//! T08 does not persist anything — that is T16 — but every committed transaction
//! is recorded here exactly as it will be journalled: the
//! [`TopologyTransaction`] plus the [`MotionSnapshot`] of every body that
//! participated, so `docs/protocol.md`'s rule ("Each topology journal
//! transaction also contains the participant body states required at that
//! transaction") is satisfied from the start. The sink is an in-memory `Vec`.
//!
//! # Bounded retention (ENG-50)
//!
//! The integrator flushes committed entries to `spall_store` and then calls
//! [`JournalSink::prune_through`] once a durable checkpoint covers them and no
//! snapshot transfer still needs them (`docs/protocol.md`: "Retire journal
//! records only after a newer durable checkpoint covers them"). Pruned entries
//! leave the `Vec` but the high-water sequence is retained, so the baseline
//! cursor ([`JournalSink::cursor`]) and the strictly-increasing `seq` invariant
//! both survive a prune to empty. [`JournalSink::entries_after`] is an
//! `O(log n)` tail lookup so a per-tick flush never rescans the whole history.

use spall_core::JournalSeq;
use spall_protocol::baseline::BaselineWorld;
use spall_protocol::{MotionSnapshot, TopologyTransaction};

/// One journal record: a committed transaction and the state of every body it
/// touched or created, at the committing tick.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub seq: JournalSeq,
    pub transaction: TopologyTransaction,
    /// State of every participant body (source and any children) at commit.
    pub participants: Vec<MotionSnapshot>,
    /// T17 increment 2: for a giant split whose `transaction.ops` are
    /// `SplitOffBulkBaseline` / `SourcePatchBulkBaseline` markers only, the
    /// out-of-band `BaselineWorld` the durable store must persist alongside the
    /// transaction so exact-replay and recovery can reconstruct it. `None` for
    /// every ordinary entry.
    pub bulk_baseline: Option<BaselineWorld>,
}

/// An in-memory append-only journal of the *retained* suffix. Ordered by
/// [`JournalEntry::seq`]; entries below [`JournalSink::retired_seq`] have been
/// flushed durably and pruned.
#[derive(Debug, Clone, Default)]
pub struct JournalSink {
    entries: Vec<JournalEntry>,
    /// Highest `seq` ever appended, retained across a prune to empty.
    high_water: u64,
    /// Highest `seq` that has been pruned (flushed durably and retired).
    retired_seq: u64,
}

impl JournalSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry. Panics if `seq` is not strictly greater than every
    /// sequence appended so far (including already-pruned ones) — the caller
    /// (the commit path) allocates sequences monotonically.
    pub fn append(&mut self, entry: JournalEntry) {
        assert!(
            self.high_water == 0 || entry.seq.0 > self.high_water,
            "journal sequences must strictly increase: {:?} after high-water {}",
            entry.seq,
            self.high_water
        );
        self.high_water = entry.seq.0;
        self.entries.push(entry);
    }

    /// The retained entries, ascending by `seq`.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// The retained entries with `seq` strictly greater than `after`. `O(log n)`
    /// — the flush path calls this every tick and must not rescan the history.
    pub fn entries_after(&self, after: u64) -> &[JournalEntry] {
        let idx = self.entries.partition_point(|e| e.seq.0 <= after);
        &self.entries[idx..]
    }

    /// Drops every retained entry with `seq <= through` and advances
    /// [`JournalSink::retired_seq`]. Returns how many entries were removed. A
    /// no-op when nothing is at or below `through`.
    pub fn prune_through(&mut self, through: u64) -> usize {
        let idx = self.entries.partition_point(|e| e.seq.0 <= through);
        if idx == 0 {
            return 0;
        }
        self.retired_seq = self.retired_seq.max(self.entries[idx - 1].seq.0);
        self.entries.drain(..idx);
        idx
    }

    /// Number of *retained* entries (not the lifetime total).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entry is currently retained (all pruned, or none yet).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recent *retained* entry, if any. `None` after a prune to empty —
    /// use [`JournalSink::cursor`] for the baseline journal cursor.
    pub fn last(&self) -> Option<&JournalEntry> {
        self.entries.last()
    }

    /// Highest journal sequence this sink has ever owned, retained across
    /// pruning. `0` before the first append. This is the value a baseline
    /// transfer records as its journal cursor.
    pub fn cursor(&self) -> u64 {
        self.high_water
    }

    /// Highest sequence that has been flushed durably and pruned (`0` if none).
    pub fn retired_seq(&self) -> u64 {
        self.retired_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{Tick, TransactionId};
    use spall_protocol::ControlSeq;

    fn entry(seq: u64) -> JournalEntry {
        JournalEntry {
            seq: JournalSeq(seq),
            transaction: TopologyTransaction {
                transaction_id: TransactionId::new(seq.max(1)).unwrap(),
                server_tick: Tick(seq),
                control_seq: ControlSeq(0),
                algorithm_version: 1,
                dependencies: vec![],
                before: vec![],
                after: vec![],
                ops: vec![],
                result_hashes: vec![],
            },
            participants: vec![],
            bulk_baseline: None,
        }
    }

    #[test]
    fn entries_after_returns_the_strict_tail() {
        let mut j = JournalSink::new();
        for s in 1..=5 {
            j.append(entry(s));
        }
        let tail: Vec<u64> = j.entries_after(2).iter().map(|e| e.seq.0).collect();
        assert_eq!(tail, vec![3, 4, 5]);
        assert!(j.entries_after(5).is_empty());
        assert_eq!(j.entries_after(0).len(), 5);
    }

    #[test]
    fn prune_keeps_the_cursor_and_the_increasing_invariant() {
        let mut j = JournalSink::new();
        for s in 1..=4 {
            j.append(entry(s));
        }
        assert_eq!(j.prune_through(2), 2);
        assert_eq!(j.len(), 2);
        assert_eq!(j.retired_seq(), 2);
        assert_eq!(j.cursor(), 4);
        // Tail lookup still correct after a prune.
        let tail: Vec<u64> = j.entries_after(2).iter().map(|e| e.seq.0).collect();
        assert_eq!(tail, vec![3, 4]);

        // Prune to empty; the cursor survives and a new append must still climb.
        assert_eq!(j.prune_through(9), 2);
        assert!(j.is_empty());
        assert_eq!(j.cursor(), 4);
        j.append(entry(5));
        assert_eq!(j.cursor(), 5);
    }

    #[test]
    #[should_panic(expected = "strictly increase")]
    fn a_stale_sequence_after_a_full_prune_still_panics() {
        let mut j = JournalSink::new();
        j.append(entry(3));
        j.prune_through(3);
        j.append(entry(3));
    }
}
