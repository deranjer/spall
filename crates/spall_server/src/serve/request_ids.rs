//! ENG-273: request ids are chosen by clients and every client counts from the
//! same base, so the simulation's retry ledger (keyed by request id alone)
//! cannot be shared as-is. [`RequestIds`] gives each (player, client request
//! id) pair its own server-wide id; the tick loop hands that id to the
//! simulation and puts the client's own id back on every answer.
//!
//! The player is the authenticated account when the session has one, else the
//! transport slot. Both survive a reconnect, so a reliable retry sent on a
//! replacement session finds the same entry (ENG-57).
use std::collections::{HashMap, VecDeque};

use spall_protocol::{ActionRequest, ActionStatus, PlayerId, RequestId, SessionId};
use spall_sim::{SERVER_REQUEST_ID_BAND, Simulation};

use super::replay_admitted_status;

/// Who staged a ledger id, and the id their client chose for it.
pub(super) type SubmittedBy = (SessionId, Option<PlayerId>, RequestId);

/// Server-authored ids start here, above the simulation's own damage and
/// water ids, which count up from the bottom of the band.
use spall_protocol::limits::MAX_RETAINED_REQUESTS_PER_PLAYER;
const LEDGER_REQUEST_ID_BAND: u64 = SERVER_REQUEST_ID_BAND | (1 << 61);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Owner {
    Account(PlayerId),
    Slot(u32),
}

#[derive(Default)]
pub(super) struct RequestIds {
    next: u64,
    ids: HashMap<(Owner, u64), RequestId>,
    /// Each player's client ids, oldest first.
    recent: HashMap<Owner, VecDeque<u64>>,
}

impl RequestIds {
    fn owner(session: SessionId, account: Option<PlayerId>) -> Owner {
        account.map_or(Owner::Slot(session.slot().0), Owner::Account)
    }

    /// The server-wide id for a request this player already sent, if still
    /// remembered.
    fn lookup(
        &self,
        session: SessionId,
        account: Option<PlayerId>,
        client_id: RequestId,
    ) -> Option<RequestId> {
        self.ids
            .get(&(Self::owner(session, account), client_id.0))
            .copied()
    }

    /// The recorded answer to a retry of a request this player already had
    /// admitted, under the id the client chose.
    pub(super) fn replay(
        &self,
        sim: &Simulation,
        session: SessionId,
        account: Option<PlayerId>,
        req: &ActionRequest,
    ) -> Option<ActionStatus> {
        let mut status =
            replay_admitted_status(sim, self.lookup(session, account, req.request_id)?)?;
        status.request_id = req.request_id;
        Some(status)
    }

    /// The server-wide id for the request, allocating one the first time. A
    /// player past its cap forgets its oldest request ids.
    pub(super) fn assign(
        &mut self,
        session: SessionId,
        account: Option<PlayerId>,
        client_id: RequestId,
    ) -> RequestId {
        let owner = Self::owner(session, account);
        if let Some(id) = self.ids.get(&(owner, client_id.0)) {
            return *id;
        }
        let id = RequestId(LEDGER_REQUEST_ID_BAND | self.next);
        self.next += 1;
        self.ids.insert((owner, client_id.0), id);
        let recent = self.recent.entry(owner).or_default();
        recent.push_back(client_id.0);
        while recent.len() > MAX_RETAINED_REQUESTS_PER_PLAYER {
            if let Some(old) = recent.pop_front() {
                self.ids.remove(&(owner, old));
            }
        }
        id
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::SlotId;

    fn session(slot: u32, generation: u32) -> SessionId {
        SessionId::from_parts(SlotId(slot), generation)
    }

    #[test]
    fn players_sending_the_same_id_get_distinct_ledger_ids() {
        let mut ids = RequestIds::default();
        let a = ids.assign(session(0, 1), None, RequestId(2_000_000));
        let b = ids.assign(session(1, 1), None, RequestId(2_000_000));
        assert_ne!(a, b);
        assert_eq!(ids.assign(session(0, 1), None, RequestId(2_000_000)), a);
    }

    #[test]
    fn a_replacement_session_finds_the_same_entry() {
        let mut ids = RequestIds::default();
        let account = PlayerId([7; 16]);
        let first = ids.assign(session(0, 1), Some(account), RequestId(5));
        // Reconnect lands on another slot and generation: the account is the identity.
        assert_eq!(
            ids.lookup(session(3, 2), Some(account), RequestId(5)),
            Some(first)
        );
        // Without an account the slot is the identity.
        let slot_id = ids.assign(session(2, 1), None, RequestId(5));
        assert_eq!(ids.lookup(session(2, 9), None, RequestId(5)), Some(slot_id));
        assert_eq!(ids.lookup(session(4, 1), None, RequestId(5)), None);
    }

    #[test]
    fn memory_is_bounded_per_player_and_one_player_cannot_evict_another() {
        let mut ids = RequestIds::default();
        let quiet = ids.assign(session(1, 1), None, RequestId(1));
        let cap = MAX_RETAINED_REQUESTS_PER_PLAYER as u64;
        for n in 0..cap * 3 {
            ids.assign(session(0, 1), None, RequestId(n));
        }
        assert_eq!(ids.len(), MAX_RETAINED_REQUESTS_PER_PLAYER + 1);
        assert_eq!(ids.lookup(session(0, 1), None, RequestId(0)), None);
        assert!(
            ids.lookup(session(0, 1), None, RequestId(cap * 3 - 1))
                .is_some()
        );
        assert_eq!(ids.lookup(session(1, 1), None, RequestId(1)), Some(quiet));
    }
}
