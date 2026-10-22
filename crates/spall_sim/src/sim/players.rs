//! Player capsule methods.

use super::*;

impl Simulation {
    /// Registers an authoritative player capsule at `feet_m` (metres). `entity`
    /// is a reserved-band id from [`spall_core::player_entity_for`].
    pub fn add_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> EntityId {
        self.world
            .add_player(entity, feet_m, CharacterParams::DEFAULT)
    }

    /// Feeds one validated input frame to a player (at most one per tick per
    /// player). Returns `false` for an unknown player or a stale / duplicate /
    /// non-finite frame.
    pub fn set_player_input(
        &mut self,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
    ) -> bool {
        self.world.set_player_input(entity, input, seq)
    }

    /// Moves a player to `feet_m` at rest (see `SimWorld::teleport_player`).
    pub fn teleport_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> bool {
        self.world.teleport_player(entity, feet_m, 1.0 / 60.0)
    }

    /// The authoritative kinematic state of a player, if it exists.
    pub fn player_state(&self, entity: EntityId) -> Option<CharacterState> {
        self.world.player(entity).map(|p| p.state)
    }

    /// A player's prediction-invalidation epoch (bumped by a nearby commit).
    pub fn player_movement_epoch(&self, entity: EntityId) -> Option<u64> {
        self.world.player(entity).map(|p| p.movement_epoch)
    }

    /// The last input sequence this simulation accepted for a player.
    pub fn player_acked_input(&self, entity: EntityId) -> Option<InputSeq> {
        self.world.player(entity).map(|p| p.last_input_seq)
    }

    pub fn player_count(&self) -> usize {
        self.world.player_count()
    }
}
