//! Player capsule methods.

use super::*;
use crate::player::{PLAYER_MAX_HEALTH, PlayerDamage};
use spall_protocol::PlayerId;

impl Simulation {
    /// Registers an authoritative player capsule at `feet_m` (metres). `entity`
    /// is a reserved-band id from [`spall_core::player_entity_for`].
    pub fn add_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> EntityId {
        // A capsule replaced in place (a reconnect into the same slot) hands its
        // account's health back to the roster first.
        if let Some(old) = self.world.player(entity)
            && let Some(principal) = old.principal
        {
            self.player_health.insert(principal, old.health);
        }
        self.world
            .add_player(entity, feet_m, CharacterParams::DEFAULT)
    }

    /// Binds a capsule to the authenticated account that owns it and restores
    /// that account's remembered health (full health on its first join). Any
    /// other capsule still bound to the same account is unbound first, so one
    /// account never has two healths. Returns `false` for an unknown player.
    pub fn bind_player_principal(&mut self, entity: EntityId, principal: PlayerId) -> bool {
        if self.world.player(entity).is_none() {
            return false;
        }
        for player in self.world.players.values_mut() {
            if player.entity != entity && player.principal == Some(principal) {
                player.principal = None;
                self.player_health.insert(principal, player.health);
            }
        }
        let remembered = self.player_health.remove(&principal);
        let player = self
            .world
            .players
            .get_mut(&entity.get())
            .expect("checked above");
        player.principal = Some(principal);
        if let Some(health) = remembered {
            player.health = health;
        }
        true
    }

    /// ENG-223: the one way to hurt a player. The caller runs on the owning
    /// thread between ticks, so the damage lands at a tick boundary. Damage
    /// that would leave no health kills the player, who respawns at once at
    /// its spawn point with [`PLAYER_MAX_HEALTH`]. `None` for an unknown
    /// player.
    pub fn damage_player(&mut self, entity: EntityId, amount: u16) -> Option<PlayerDamage> {
        let player = self.world.players.get_mut(&entity.get())?;
        if amount < player.health {
            player.health -= amount;
            return Some(PlayerDamage::Hurt {
                health: player.health,
            });
        }
        player.health = PLAYER_MAX_HEALTH;
        let spawn = player.spawn.position_m;
        self.world.teleport_player(entity, spawn, TICK_DT_S);
        Some(PlayerDamage::Killed)
    }

    /// A player's current health, if it exists.
    pub fn player_health(&self, entity: EntityId) -> Option<u16> {
        self.world.player(entity).map(|p| p.health)
    }

    /// Every authenticated account's health, bound or not, sorted by account
    /// for the checkpoint.
    pub fn player_health_records(&self) -> Vec<(PlayerId, u16)> {
        let mut records: Vec<(PlayerId, u16)> = self
            .player_health
            .iter()
            .map(|(id, health)| (*id, *health))
            .chain(
                self.world
                    .players()
                    .filter_map(|p| p.principal.map(|id| (id, p.health))),
            )
            .collect();
        records.sort_unstable_by_key(|(id, _)| *id);
        records
    }

    /// Installs recovered per-account health. Each value must be
    /// `1..=PLAYER_MAX_HEALTH`; anything else is refused whole.
    pub fn restore_player_health(&mut self, records: Vec<(PlayerId, u16)>) -> Result<(), u16> {
        if let Some((_, bad)) = records
            .iter()
            .find(|(_, health)| !(1..=PLAYER_MAX_HEALTH).contains(health))
        {
            return Err(*bad);
        }
        self.player_health = records.into_iter().collect();
        Ok(())
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
