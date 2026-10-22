//! One checked runtime allocator for transfer IDs shared by all server paths.
//!
//! Low sequence values are global across kinds. The kind is encoded only in
//! the reset/split bits, so an abandoned reservation still burns its sequence
//! number across namespaces.

use spall_protocol::{SPLIT_BULK_TRANSFER_ID_BIT, TransferId, WORLD_RESET_TRANSFER_ID_BIT};
use thiserror::Error;

const KIND_BITS: u64 = SPLIT_BULK_TRANSFER_ID_BIT | WORLD_RESET_TRANSFER_ID_BIT;
const LOW_ID_MAX: u64 = (1 << 61) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferNamespace {
    Baseline,
    Reset,
    Split,
}

impl TransferNamespace {
    const fn bits(self) -> u64 {
        match self {
            Self::Baseline => 0,
            Self::Reset => WORLD_RESET_TRANSFER_ID_BIT,
            Self::Split => SPLIT_BULK_TRANSFER_ID_BIT,
        }
    }

    fn from_bits(bits: u64) -> Option<Self> {
        match bits {
            0 => Some(Self::Baseline),
            WORLD_RESET_TRANSFER_ID_BIT => Some(Self::Reset),
            SPLIT_BULK_TRANSFER_ID_BIT => Some(Self::Split),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransferIdError {
    #[error("transfer ID sequence is exhausted")]
    Exhausted,
    #[error("retained transfer ID is zero or has conflicting namespace bits")]
    InvalidRetainedId,
}

/// Checked global monotone low-sequence allocator. No reservation is returned
/// until its entire range fits, and dropped reservations are intentionally not
/// returned to the allocator.
#[derive(Debug)]
pub struct TransferIdAllocator {
    next_low: u64,
}

impl Default for TransferIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferIdAllocator {
    pub const fn new() -> Self {
        Self { next_low: 1 }
    }

    /// Creates an allocator whose next low value is above every retained
    /// composed transfer ID. Invalid or zero retained IDs fail closed.
    pub fn seeded_from_retained(
        retained: impl IntoIterator<Item = TransferId>,
    ) -> Result<Self, TransferIdError> {
        let mut allocator = Self::new();
        allocator.seed_past_retained(retained)?;
        Ok(allocator)
    }

    /// Advances this allocator beyond retained IDs without ever moving its
    /// highwater backwards. Existing reservations remain burned.
    pub fn seed_past_retained(
        &mut self,
        retained: impl IntoIterator<Item = TransferId>,
    ) -> Result<(), TransferIdError> {
        let mut max_low = 0;
        for id in retained {
            if id.0 == 0 {
                return Err(TransferIdError::InvalidRetainedId);
            }
            let namespace_bits = id.0 & KIND_BITS;
            if TransferNamespace::from_bits(namespace_bits).is_none() {
                return Err(TransferIdError::InvalidRetainedId);
            }
            let low = id.0 & !KIND_BITS;
            if low == 0 || low > LOW_ID_MAX {
                return Err(TransferIdError::InvalidRetainedId);
            }
            max_low = max_low.max(low);
        }
        if max_low > 0 {
            self.next_low = self
                .next_low
                .max(max_low.checked_add(1).ok_or(TransferIdError::Exhausted)?);
        }
        Ok(())
    }

    /// Reserves a bounded contiguous range in one namespace. Every call
    /// consumes the global low range immediately, even if the returned value
    /// is later dropped or only partly used. The reservation has constant
    /// storage; its maximum range is the remaining protocol ID space.
    pub fn reserve(
        &mut self,
        namespace: TransferNamespace,
        count: usize,
    ) -> Result<TransferIdReservation, TransferIdError> {
        let count = u64::try_from(count).map_err(|_| TransferIdError::Exhausted)?;
        if count == 0 {
            return Ok(TransferIdReservation {
                namespace,
                next_low: self.next_low,
                remaining: 0,
            });
        }
        let last = self
            .next_low
            .checked_add(count - 1)
            .filter(|last| *last <= LOW_ID_MAX)
            .ok_or(TransferIdError::Exhausted)?;
        let next_low = last.checked_add(1).ok_or(TransferIdError::Exhausted)?;
        let reservation = TransferIdReservation {
            namespace,
            next_low: self.next_low,
            remaining: count,
        };
        self.next_low = next_low;
        Ok(reservation)
    }

    pub const fn next_low(&self) -> u64 {
        self.next_low
    }

    #[cfg(test)]
    pub(crate) fn set_next_low_for_test(&mut self, next_low: u64) {
        self.next_low = next_low;
    }
}

/// Owned bounded range of already-consumed transfer IDs. It is deliberately
/// not `Clone`, so callers cannot duplicate the same reservation by copying it.
#[derive(Debug)]
pub struct TransferIdReservation {
    namespace: TransferNamespace,
    next_low: u64,
    remaining: u64,
}

impl TransferIdReservation {
    pub fn next_id(&mut self) -> Option<TransferId> {
        if self.remaining == 0 {
            return None;
        }
        let id = TransferId(self.next_low | self.namespace.bits());
        self.next_low += 1;
        self.remaining -= 1;
        Some(id)
    }

    pub const fn remaining(&self) -> u64 {
        self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_share_one_monotone_low_sequence_without_collisions() {
        let mut allocator = TransferIdAllocator::new();
        let mut baseline = allocator.reserve(TransferNamespace::Baseline, 2).unwrap();
        let mut reset = allocator.reserve(TransferNamespace::Reset, 1).unwrap();
        let mut split = allocator.reserve(TransferNamespace::Split, 1).unwrap();
        let ids = [
            baseline.next_id().unwrap(),
            baseline.next_id().unwrap(),
            reset.next_id().unwrap(),
            split.next_id().unwrap(),
        ];
        let lows: Vec<_> = ids.iter().map(|id| id.0 & !KIND_BITS).collect();
        assert_eq!(lows, [1, 2, 3, 4]);
        assert_eq!(
            ids.iter()
                .map(|id| id.0)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            ids.len()
        );
        assert_eq!(
            ids[2].0 & WORLD_RESET_TRANSFER_ID_BIT,
            WORLD_RESET_TRANSFER_ID_BIT
        );
        assert_eq!(
            ids[3].0 & SPLIT_BULK_TRANSFER_ID_BIT,
            SPLIT_BULK_TRANSFER_ID_BIT
        );
        assert_eq!(allocator.next_low(), 5);
    }

    #[test]
    fn zero_count_is_empty_and_sentinel_is_never_allocated() {
        let mut allocator = TransferIdAllocator::new();
        assert_eq!(
            allocator
                .reserve(TransferNamespace::Baseline, 0)
                .unwrap()
                .next_id(),
            None
        );
        let id = allocator
            .reserve(TransferNamespace::Baseline, 1)
            .unwrap()
            .next_id()
            .unwrap();
        assert_eq!(id, TransferId(1));
        assert_ne!(id, TransferId(0));
    }

    #[test]
    fn reservations_are_burned_even_when_dropped_or_partially_consumed() {
        let mut allocator = TransferIdAllocator::new();
        {
            let _burned = allocator.reserve(TransferNamespace::Baseline, 3).unwrap();
        }
        let mut next = allocator.reserve(TransferNamespace::Reset, 2).unwrap();
        assert_eq!(
            next.next_id(),
            Some(TransferId(WORLD_RESET_TRANSFER_ID_BIT | 4))
        );
        assert_eq!(next.remaining(), 1);
        assert_eq!(allocator.next_low(), 6);
    }

    #[test]
    fn retained_ids_seed_one_highwater_across_namespaces() {
        let mut allocator = TransferIdAllocator::seeded_from_retained([
            TransferId(WORLD_RESET_TRANSFER_ID_BIT | 37),
            TransferId(SPLIT_BULK_TRANSFER_ID_BIT | 12),
            TransferId(19),
        ])
        .unwrap();
        assert_eq!(allocator.next_low(), 38);
        assert_eq!(
            allocator
                .reserve(TransferNamespace::Split, 1)
                .unwrap()
                .next_id(),
            Some(TransferId(SPLIT_BULK_TRANSFER_ID_BIT | 38))
        );
    }

    #[test]
    fn retained_zero_or_conflicting_namespace_ids_are_rejected() {
        assert_eq!(
            TransferIdAllocator::seeded_from_retained([TransferId(0)]).unwrap_err(),
            TransferIdError::InvalidRetainedId
        );
        assert_eq!(
            TransferIdAllocator::seeded_from_retained([TransferId(
                WORLD_RESET_TRANSFER_ID_BIT | SPLIT_BULK_TRANSFER_ID_BIT | 4
            )])
            .unwrap_err(),
            TransferIdError::InvalidRetainedId
        );
    }

    #[test]
    fn sequence_exhaustion_does_not_wrap() {
        let mut allocator = TransferIdAllocator::new();
        allocator.set_next_low_for_test(LOW_ID_MAX);
        let mut last = allocator.reserve(TransferNamespace::Baseline, 1).unwrap();
        assert_eq!(last.next_id(), Some(TransferId(LOW_ID_MAX)));
        assert_eq!(
            allocator.reserve(TransferNamespace::Reset, 1).unwrap_err(),
            TransferIdError::Exhausted
        );
    }

    #[test]
    fn invalid_retained_suffix_does_not_advance_the_highwater() {
        let mut allocator = TransferIdAllocator::new();
        assert_eq!(
            allocator
                .seed_past_retained([TransferId(SPLIT_BULK_TRANSFER_ID_BIT | 100), TransferId(0),]),
            Err(TransferIdError::InvalidRetainedId)
        );
        assert_eq!(allocator.next_low(), 1);
    }
}
