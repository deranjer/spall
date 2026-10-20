//! The server's creature entities.
//!
//! A creature is a plain record: species, root pose, and the clip it is playing.
//! It owns no world voxels and has no physics body; the game decides where it
//! goes and the simulation only keeps the authoritative pose, sweeping a capsule
//! against terrain when the game asks it to move. Spawning and despawning are
//! explicit, and ids are never reused, so a replica can ignore motion for an id
//! it does not hold.

use std::collections::BTreeMap;

use spall_core::{CreatureAllocator, CreatureId};
use spall_protocol::CreatureRecord;

pub const MAX_CREATURE_HOST_RECORDS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreatureHostInitError {
    #[error("creature host is already initialized")]
    AlreadyInitialized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreatureSpawnError {
    #[error("creature host is not initialized")]
    NoHost,
    #[error("creature record is invalid")]
    Invalid,
    #[error("creature id was already used")]
    IdUsed,
    #[error("too many creatures")]
    Capacity,
}

/// A creature's authoritative state.
#[derive(Debug, Clone, PartialEq)]
pub struct Creature {
    pub record: CreatureRecord,
    /// Average velocity over the last applied move, m/s.
    pub velocity_m_s: [f32; 3],
    pub(crate) moved_at_tick: u64,
}

#[derive(Clone, Default)]
pub(crate) struct CreatureHost {
    allocator: CreatureAllocator,
    creatures: BTreeMap<u64, Creature>,
}

impl CreatureHost {
    pub(crate) fn next_creature_id(&self) -> u64 {
        self.allocator.next_creature_id()
    }

    pub(crate) fn spawn(
        &mut self,
        record: CreatureRecord,
        tick: u64,
    ) -> Result<(), CreatureSpawnError> {
        let spawn = spall_protocol::CreatureSpawn {
            schema: spall_protocol::WIRE_SCHEMA_VERSION,
            creature: record,
        };
        spall_protocol::Record::validate(&spawn).map_err(|_| CreatureSpawnError::Invalid)?;
        let record = spawn.creature;
        if self.creatures.len() >= MAX_CREATURE_HOST_RECORDS {
            return Err(CreatureSpawnError::Capacity);
        }
        let id = CreatureId::new(record.creature_id).map_err(|_| CreatureSpawnError::Invalid)?;
        self.allocator
            .claim(id)
            .map_err(|_| CreatureSpawnError::IdUsed)?;
        self.creatures.insert(
            record.creature_id,
            Creature {
                record,
                velocity_m_s: [0.0; 3],
                moved_at_tick: tick,
            },
        );
        Ok(())
    }

    pub(crate) fn despawn(&mut self, creature_id: u64) -> bool {
        self.creatures.remove(&creature_id).is_some()
    }

    /// A world reset removes every creature but keeps the id counter.
    pub(crate) fn clear(&mut self) {
        self.creatures.clear();
    }

    pub(crate) fn get(&self, creature_id: u64) -> Option<&Creature> {
        self.creatures.get(&creature_id)
    }

    pub(crate) fn get_mut(&mut self, creature_id: u64) -> Option<&mut Creature> {
        self.creatures.get_mut(&creature_id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Creature> {
        self.creatures.values()
    }
}
