//! A stub authoritative journal built from real [`spall_protocol`] DTOs.
//!
//! T08 does not persist anything — that is T16 — but every committed transaction
//! is recorded here exactly as it will be journalled: the
//! [`TopologyTransaction`] plus the [`MotionSnapshot`] of every body that
//! participated, so `docs/protocol.md`'s rule ("Each topology journal
//! transaction also contains the participant body states required at that
//! transaction") is satisfied from the start. The sink is an in-memory `Vec`.

use spall_core::JournalSeq;
use spall_protocol::{MotionSnapshot, TopologyTransaction};

/// One journal record: a committed transaction and the state of every body it
/// touched or created, at the committing tick.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    pub seq: JournalSeq,
    pub transaction: TopologyTransaction,
    /// State of every participant body (source and any children) at commit.
    pub participants: Vec<MotionSnapshot>,
}

/// An in-memory append-only journal. Ordered by [`JournalEntry::seq`].
#[derive(Debug, Clone, Default)]
pub struct JournalSink {
    entries: Vec<JournalEntry>,
}

impl JournalSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry. Panics if `seq` is not strictly increasing — the caller
    /// (the commit path) allocates sequences monotonically.
    pub fn append(&mut self, entry: JournalEntry) {
        if let Some(last) = self.entries.last() {
            assert!(
                entry.seq > last.seq,
                "journal sequences must strictly increase: {:?} after {:?}",
                entry.seq,
                last.seq
            );
        }
        self.entries.push(entry);
    }

    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most recent entry, if any.
    pub fn last(&self) -> Option<&JournalEntry> {
        self.entries.last()
    }
}
