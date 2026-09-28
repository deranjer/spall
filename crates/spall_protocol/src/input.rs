//! Turning the [`InputFrame`] wire record into the movement-kernel input.
//!
//! The wire record ([`crate::records::InputFrame`]) is frozen. This module adds
//! only *reader* helpers — no new wire field — so the server, the deterministic
//! capsule kernel (`spall_physics::character`), and the client predictor all
//! consume one [`spall_core::PlayerInput`] value.

use spall_core::{EntityId, PlayerInput};

use crate::records::{InputFrame, RecentInput};
use crate::session::{SessionId, SlotId};

/// The movement intent carried by one [`InputFrame`].
pub fn frame_input(frame: &InputFrame) -> PlayerInput {
    PlayerInput {
        movement: frame.movement,
        view_dir: frame.view_dir,
        buttons: frame.buttons,
    }
}

/// The movement intent carried by one redundant [`RecentInput`] copy.
pub fn recent_input(recent: &RecentInput) -> PlayerInput {
    PlayerInput {
        movement: recent.movement,
        view_dir: recent.view_dir,
        buttons: recent.buttons,
    }
}

/// The player [`EntityId`] a connection slot owns
/// ([`spall_core::player_entity_for`]).
pub fn player_entity(slot: SlotId) -> EntityId {
    spall_core::player_entity_for(slot.0)
}

/// The player [`EntityId`] for a session's connection slot.
pub fn session_player_entity(session: SessionId) -> EntityId {
    player_entity(session.slot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{EntityId, Tick};

    fn frame() -> InputFrame {
        InputFrame {
            session: SessionId::from_parts(SlotId(2), 1),
            player: EntityId::new(1).unwrap(),
            input_seq: crate::records::InputSeq(9),
            intended_tick: Tick::ZERO,
            movement: [0.3, 0.0, -1.0],
            view_dir: [0.0, 0.0, -1.0],
            buttons: spall_core::BUTTON_JUMP,
            recent: vec![],
        }
    }

    #[test]
    fn frame_input_copies_the_movement_fields() {
        let f = frame();
        let input = frame_input(&f);
        assert_eq!(input.movement, f.movement);
        assert_eq!(input.view_dir, f.view_dir);
        assert!(input.wants_jump());
    }

    #[test]
    fn player_entity_follows_the_session_slot() {
        let session = SessionId::from_parts(SlotId(5), 3);
        assert_eq!(
            session_player_entity(session),
            spall_core::player_entity_for(5)
        );
    }
}
