//! Session identity and per-stream sequencing.
//!
//! A [`SessionId`] is a `u64`: the high 32 bits are a connection slot, the low
//! 32 bits a generation that increases on every (re)connect for that slot.
//! Reconnect therefore produces a strictly newer session; packets and queued
//! inputs stamped with an older generation are stale and must be dropped
//! (`docs/protocol.md`: "Reconnect uses a new session generation; old queued
//! inputs and packets are invalid.").
//!
//! [`SequenceGate`] enforces per-stream ordering: strictly increasing `u64`
//! sequence numbers, duplicates and regressions rejected, gaps tolerated
//! (global `TransactionId` gaps are normal on a filtered stream).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Connection slot: which player seat on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotId(pub u32);

/// A session identity. Ordering compares slot first, then generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(u64);

impl SessionId {
    pub const fn from_parts(slot: SlotId, generation: u32) -> Self {
        Self(((slot.0 as u64) << 32) | generation as u64)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn slot(self) -> SlotId {
        SlotId((self.0 >> 32) as u32)
    }

    pub const fn generation(self) -> u32 {
        self.0 as u32
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "slot{}#gen{}", self.slot().0, self.generation())
    }
}

/// The three logical streams per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamKind {
    /// Single reliable ordered control / topology stream.
    Control,
    /// Bounded bulk streams carrying baseline parts.
    Bulk,
    /// Unreliable motion / input datagrams.
    Motion,
}

/// A per-connection stream sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StreamSeq(pub u64);

impl StreamSeq {
    pub const ZERO: Self = Self(0);
}

/// Raised when an action is attempted with a session that is no longer current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("session {stale} is stale; current generation for its slot is {current_generation}")]
pub struct StaleSession {
    pub stale: SessionId,
    pub current_generation: u32,
}

/// Error advancing a session generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("session generation space for the slot is exhausted")]
pub struct GenerationExhausted;

/// Tracks the current generation for each slot. The server consults this before
/// accepting any client record.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    current: HashMap<u32, u32>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens (or reopens) a slot, returning the new current session. The first
    /// session for a slot has generation `1`.
    pub fn open(&mut self, slot: SlotId) -> Result<SessionId, GenerationExhausted> {
        let generation = match self.current.get(&slot.0) {
            None => 1,
            Some(prev) => prev.checked_add(1).ok_or(GenerationExhausted)?,
        };
        self.current.insert(slot.0, generation);
        Ok(SessionId::from_parts(slot, generation))
    }

    /// The current generation for a slot, if it has ever been opened.
    pub fn current_generation(&self, slot: SlotId) -> Option<u32> {
        self.current.get(&slot.0).copied()
    }

    /// True if `session` is the live session for its slot.
    pub fn is_current(&self, session: SessionId) -> bool {
        self.current.get(&session.slot().0).copied() == Some(session.generation())
    }

    /// `Ok(())` if `session` is current, else [`StaleSession`]. Use this to gate
    /// input frames, action requests, and queued packets on reconnect.
    pub fn accept(&self, session: SessionId) -> Result<(), StaleSession> {
        match self.current.get(&session.slot().0).copied() {
            Some(current) if current == session.generation() => Ok(()),
            other => Err(StaleSession {
                stale: session,
                current_generation: other.unwrap_or(0),
            }),
        }
    }
}

/// Verdict from [`SequenceGate::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqVerdict {
    /// Newer than anything seen; accepted and now the high-water mark. Any skip
    /// from the previous mark is reported as `gap`.
    Fresh { gap: u64 },
    /// Already seen (equal to the high-water mark or below it).
    Duplicate,
}

/// Enforces strictly increasing sequence numbers on one stream, tolerating
/// gaps. Construct one per `(session, StreamKind)`.
#[derive(Debug, Clone, Default)]
pub struct SequenceGate {
    high_water: Option<u64>,
}

impl SequenceGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The highest sequence number accepted so far.
    pub fn high_water(&self) -> Option<u64> {
        self.high_water
    }

    /// Observes `seq`. Advances the high-water mark only on `Fresh`.
    pub fn observe(&mut self, seq: u64) -> SeqVerdict {
        match self.high_water {
            Some(hw) if seq <= hw => SeqVerdict::Duplicate,
            Some(hw) => {
                self.high_water = Some(seq);
                SeqVerdict::Fresh { gap: seq - hw - 1 }
            }
            None => {
                self.high_water = Some(seq);
                SeqVerdict::Fresh { gap: seq }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_advances_generation_and_stales_the_old_session() {
        let mut reg = SessionRegistry::new();
        let slot = SlotId(3);
        let first = reg.open(slot).unwrap();
        assert_eq!(first.generation(), 1);
        assert!(reg.accept(first).is_ok());

        let second = reg.open(slot).unwrap();
        assert_eq!(second.generation(), 2);
        assert!(reg.is_current(second));
        assert!(!reg.is_current(first));

        let err = reg.accept(first).unwrap_err();
        assert_eq!(err.stale, first);
        assert_eq!(err.current_generation, 2);
    }

    #[test]
    fn unknown_slot_is_never_accepted() {
        let reg = SessionRegistry::new();
        let phantom = SessionId::from_parts(SlotId(9), 1);
        assert!(!reg.is_current(phantom));
        assert_eq!(reg.accept(phantom).unwrap_err().current_generation, 0);
    }

    #[test]
    fn session_id_packs_and_unpacks() {
        let id = SessionId::from_parts(SlotId(0xABCD), 0x1234_5678);
        assert_eq!(id.slot(), SlotId(0xABCD));
        assert_eq!(id.generation(), 0x1234_5678);
        assert_eq!(SessionId::from_parts(id.slot(), id.generation()), id);
    }

    #[test]
    fn sequence_gate_accepts_increasing_reports_gaps_and_rejects_replays() {
        let mut gate = SequenceGate::new();
        assert_eq!(gate.observe(5), SeqVerdict::Fresh { gap: 5 });
        assert_eq!(gate.observe(6), SeqVerdict::Fresh { gap: 0 });
        assert_eq!(gate.observe(10), SeqVerdict::Fresh { gap: 3 });
        assert_eq!(gate.observe(10), SeqVerdict::Duplicate);
        assert_eq!(gate.observe(7), SeqVerdict::Duplicate);
        assert_eq!(gate.high_water(), Some(10));
    }
}
