//! Stable server-assigned identity for one world.
//!
//! Every [`EntityId`], [`VolumeId`] and [`TransactionId`] comes from here, is
//! unique within the world, and is never reused. [`JournalSeq`] counts journal
//! entries. The `next_*` counters are exactly the values `docs/protocol.md`
//! requires persisted so a restart cannot re-hand a live id.

use spall_core::{EntityId, IdAllocator, IdError, JournalSeq, TransactionId, VolumeId};

/// Monotonic id allocation for one world. Cheap to clone for a snapshot; the
/// authoritative copy lives in [`crate::world::SimWorld`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdRegistry {
    entities: IdAllocator,
    volumes: IdAllocator,
    transactions: IdAllocator,
    next_journal_seq: u64,
}

impl Default for IdRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IdRegistry {
    /// A fresh registry. `terrain` reserves volume id `1` for the world grid so
    /// the first detached body is volume `2`.
    pub fn new() -> Self {
        Self {
            entities: IdAllocator::new(),
            volumes: IdAllocator::new(),
            transactions: IdAllocator::new(),
            next_journal_seq: 1,
        }
    }

    /// Resumes allocation from persisted counters.
    pub fn resume(
        next_entity: u64,
        next_volume: u64,
        next_transaction: u64,
        next_journal_seq: u64,
    ) -> Result<Self, IdError> {
        Ok(Self {
            entities: IdAllocator::resume_at(next_entity)?,
            volumes: IdAllocator::resume_at(next_volume)?,
            transactions: IdAllocator::resume_at(next_transaction)?,
            next_journal_seq,
        })
    }

    /// `(next_entity, next_volume, next_transaction, next_journal_seq)` — the
    /// tuple a checkpoint must store.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.entities.peek(),
            self.volumes.peek(),
            self.transactions.peek(),
            self.next_journal_seq,
        )
    }

    pub fn allocate_entity(&mut self) -> Result<EntityId, IdError> {
        EntityId::new(self.entities.allocate_raw()?)
    }

    pub fn allocate_volume(&mut self) -> Result<VolumeId, IdError> {
        VolumeId::new(self.volumes.allocate_raw()?)
    }

    pub fn allocate_transaction(&mut self) -> Result<TransactionId, IdError> {
        TransactionId::new(self.transactions.allocate_raw()?)
    }

    pub fn allocate_journal_seq(&mut self) -> Result<JournalSeq, IdError> {
        let value = self.next_journal_seq;
        self.next_journal_seq = value.checked_add(1).ok_or(IdError::Exhausted)?;
        Ok(JournalSeq(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_monotonic_unique_and_resumable() {
        let mut reg = IdRegistry::new();
        let a = reg.allocate_entity().unwrap();
        let b = reg.allocate_entity().unwrap();
        assert_ne!(a, b);
        assert_eq!(reg.allocate_volume().unwrap(), VolumeId::new(1).unwrap());
        assert_eq!(reg.allocate_volume().unwrap(), VolumeId::new(2).unwrap());

        let (ne, nv, nt, nj) = reg.counters();
        let mut resumed = IdRegistry::resume(ne, nv, nt, nj).unwrap();
        assert_eq!(
            resumed.allocate_volume().unwrap(),
            VolumeId::new(3).unwrap()
        );
        // A resumed registry never re-hands an id it already gave out.
        assert!(resumed.allocate_entity().unwrap().get() > b.get());
    }

    #[test]
    fn journal_seq_counts_from_one_and_reports_exhaustion() {
        let mut reg = IdRegistry::new();
        assert_eq!(reg.allocate_journal_seq().unwrap(), JournalSeq(1));
        assert_eq!(reg.allocate_journal_seq().unwrap(), JournalSeq(2));

        let mut end = IdRegistry::resume(1, 1, 1, u64::MAX).unwrap();
        assert!(end.allocate_journal_seq().is_err());
    }
}
