//! The server's creature entities.
//!
//! A creature is a plain record: species, root pose, and the clip it is playing.
//! It owns no world voxels and has no physics body; the game decides where it
//! goes and the simulation only keeps the authoritative pose, sweeping a capsule
//! against terrain when the game asks it to move. Spawning and despawning are
//! explicit, and ids are never reused, so a replica can ignore motion for an id
//! it does not hold.
//!
//! The game may install a [`CreatureShape`]: the posed voxels of a creature.
//! The server uses it to tell what a tool strike hits and, when a creature
//! dies, to leave a carcass of those voxels in the terrain
//! (`creature_death.rs`).

use std::collections::BTreeMap;
use std::sync::Arc;

use spall_core::{CreatureAllocator, CreatureId, MaterialId};
use spall_protocol::CreatureRecord;

const MAX_CREATURE_HOST_RECORDS: usize = 1_024;

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

/// One posed source voxel of a creature, in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreatureVoxel {
    /// Cell centre, world metres.
    pub centre_m: [f64; 3],
    /// Source cell edge length, metres.
    pub edge_m: f32,
    /// A material of the world's manifest.
    pub material: MaterialId,
}

type ShapeFn = dyn Fn(&CreatureRecord) -> Vec<CreatureVoxel> + Send + Sync;

/// Game-owned function returning a creature's voxels at its record's pose
/// (root pose, clip and clip phase). An unknown species returns no voxels.
#[derive(Clone)]
pub struct CreatureShape(Arc<ShapeFn>);

impl CreatureShape {
    pub fn new(
        shape: impl Fn(&CreatureRecord) -> Vec<CreatureVoxel> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(shape))
    }

    pub(crate) fn voxels(&self, record: &CreatureRecord) -> Vec<CreatureVoxel> {
        (self.0)(record)
    }
}

impl std::fmt::Debug for CreatureShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatureShape").finish_non_exhaustive()
    }
}

/// A creature's authoritative state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Creature {
    pub(crate) record: CreatureRecord,
    /// Average velocity over the last applied move, m/s.
    pub(crate) velocity_m_s: [f32; 3],
    pub(crate) moved_at_tick: u64,
}

#[derive(Clone, Default)]
pub(crate) struct CreatureHost {
    allocator: CreatureAllocator,
    creatures: BTreeMap<u64, Creature>,
    shape: Option<CreatureShape>,
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

    pub(crate) fn set_shape(&mut self, shape: CreatureShape) {
        self.shape = Some(shape);
    }

    pub(crate) fn shape(&self) -> Option<&CreatureShape> {
        self.shape.as_ref()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Creature> {
        self.creatures.values()
    }
}
