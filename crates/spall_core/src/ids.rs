//! Stable identity and monotonic counter types.
//!
//! `EntityId`, `VolumeId` and `TransactionId` are server-assigned `u64` values,
//! unique and never reused within a [`WorldId`]. Zero is reserved as "none".
//! `Tick`, `Revision` and `JournalSeq` are monotonic `u64` counters that fail
//! explicitly on exhaustion rather than wrapping.
//!
//! None of these are runtime ECS handles or library handles; they are the
//! values that get persisted and replicated.

use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// Persistent 128-bit world identity. Stored and compared as raw bytes; the
/// canonical wire form is little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorldId(u128);

impl WorldId {
    /// The reserved all-zero id, used for "no world".
    pub const NIL: Self = Self(0);

    pub const fn from_u128(value: u128) -> Self {
        Self(value)
    }

    pub const fn to_u128(self) -> u128 {
        self.0
    }

    /// Little-endian bytes, the canonical encoding used in hashes and on the
    /// wire.
    pub const fn to_le_bytes(self) -> [u8; 16] {
        self.0.to_le_bytes()
    }

    pub const fn from_le_bytes(bytes: [u8; 16]) -> Self {
        Self(u128::from_le_bytes(bytes))
    }

    pub const fn is_nil(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for WorldId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Error returned when a reserved zero value is supplied where a live id is
/// required, or when a counter is advanced past `u64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("id value zero is reserved")]
    Reserved,
    #[error("id space exhausted")]
    Exhausted,
}

macro_rules! server_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Wraps a raw value. `Err(IdError::Reserved)` if `value == 0`.
            #[inline]
            pub const fn new(value: u64) -> Result<Self, IdError> {
                match NonZeroU64::new(value) {
                    Some(v) => Ok(Self(v)),
                    None => Err(IdError::Reserved),
                }
            }

            #[inline]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.get())
            }
        }
    };
}

server_id!(
    /// Identity of a player, tool, creature or other simulation entity.
    EntityId
);
server_id!(
    /// Identity of a voxel volume: terrain, or one detached body's geometry.
    VolumeId
);
server_id!(
    /// Identity of one committed topology transaction.
    TransactionId
);

/// Monotonic allocator for one server id family. Persist [`IdAllocator::peek`]
/// as the next-id counter; never reuse a value it has handed out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdAllocator {
    next: u64,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl IdAllocator {
    /// A fresh allocator whose first handed-out value is `1`.
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// Resumes allocation so the next value handed out is `next`.
    /// `Err(IdError::Reserved)` if `next == 0`.
    pub const fn resume_at(next: u64) -> Result<Self, IdError> {
        if next == 0 {
            return Err(IdError::Reserved);
        }
        Ok(Self { next })
    }

    /// The value that will be handed out next. This is the counter to persist.
    pub const fn peek(&self) -> u64 {
        self.next
    }

    /// Hands out the next raw value, advancing the counter. The very top value
    /// `u64::MAX` is reserved as an exhaustion sentinel (like `0` is reserved
    /// for "none"), so the last id actually handed out is `u64::MAX - 1`;
    /// after that, `Err(IdError::Exhausted)`. The counter never wraps.
    pub const fn allocate_raw(&mut self) -> Result<u64, IdError> {
        let value = self.next;
        match self.next.checked_add(1) {
            Some(next) => {
                self.next = next;
                Ok(value)
            }
            None => Err(IdError::Exhausted),
        }
    }
}

/// Monotonic `u64` counters. Distinct newtypes so a tick cannot be passed where
/// a revision is expected.
macro_rules! counter {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
        )]
        pub struct $name(pub u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Returns the successor. `Err(IdError::Exhausted)` at `u64::MAX`;
            /// the value never wraps.
            #[inline]
            pub const fn checked_next(self) -> Result<Self, IdError> {
                match self.0.checked_add(1) {
                    Some(v) => Ok(Self(v)),
                    None => Err(IdError::Exhausted),
                }
            }

            /// Advances in place, returning the new value.
            #[inline]
            pub const fn advance(&mut self) -> Result<Self, IdError> {
                match self.checked_next() {
                    Ok(v) => {
                        *self = v;
                        Ok(v)
                    }
                    Err(e) => Err(e),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

counter!(
    /// A fixed 60 Hz server simulation step.
    Tick
);
counter!(
    /// A per-brick / per-layer authoritative revision.
    Revision
);
counter!(
    /// A position in the durable authoritative journal.
    JournalSeq
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_reserved_for_server_ids() {
        assert_eq!(EntityId::new(0), Err(IdError::Reserved));
        assert_eq!(VolumeId::new(0), Err(IdError::Reserved));
        assert_eq!(TransactionId::new(0), Err(IdError::Reserved));
        assert_eq!(EntityId::new(1).unwrap().get(), 1);
    }

    #[test]
    fn allocator_is_monotonic_and_persists_its_counter() {
        let mut alloc = IdAllocator::new();
        assert_eq!(alloc.allocate_raw().unwrap(), 1);
        assert_eq!(alloc.allocate_raw().unwrap(), 2);
        let saved = alloc.peek();
        assert_eq!(saved, 3);
        let mut resumed = IdAllocator::resume_at(saved).unwrap();
        assert_eq!(resumed.allocate_raw().unwrap(), 3);
        assert_eq!(IdAllocator::resume_at(0), Err(IdError::Reserved));
    }

    #[test]
    fn allocator_reports_exhaustion_without_wrapping() {
        let mut alloc = IdAllocator::resume_at(u64::MAX - 1).unwrap();
        assert_eq!(alloc.allocate_raw().unwrap(), u64::MAX - 1);
        assert_eq!(alloc.allocate_raw(), Err(IdError::Exhausted));
        assert_eq!(alloc.peek(), u64::MAX);
        // A resumed exhausted allocator stays exhausted; it never wraps to 0.
        let mut resumed = IdAllocator::resume_at(u64::MAX).unwrap();
        assert_eq!(resumed.allocate_raw(), Err(IdError::Exhausted));
    }

    #[test]
    fn counters_fail_explicitly_at_the_ceiling() {
        assert_eq!(Tick(5).checked_next().unwrap(), Tick(6));
        assert_eq!(Tick(u64::MAX).checked_next(), Err(IdError::Exhausted));
        let mut rev = Revision(u64::MAX);
        assert_eq!(rev.advance(), Err(IdError::Exhausted));
        assert_eq!(rev, Revision(u64::MAX));
    }

    #[test]
    fn world_id_little_endian_round_trips() {
        let id = WorldId::from_u128(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10);
        assert_eq!(WorldId::from_le_bytes(id.to_le_bytes()), id);
        assert_eq!(id.to_le_bytes()[0], 0x10);
        assert!(WorldId::NIL.is_nil());
    }
}
