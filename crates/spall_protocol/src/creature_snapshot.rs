//! Complete portable animal population identity and ownership envelope.
//!
//! This DTO is a prerequisite for checkpoint/baseline integration; it does not
//! install simulation state or create a separate allocator.
use serde::{Deserialize, Deserializer, Serialize};
use spall_core::{CreatureAllocator, CreatureAllocatorSnapshotV1, WorldId};

use crate::{
    CanonicalWriter, CreatureOwnershipError, CreatureOwnershipSetV3, CreatureValidationCatalog,
    Hash32, creature_owner::MAX_CREATURE_OWNER_SET_BYTES,
};

pub const CREATURE_SNAPSHOT_SCHEMA: u16 = 1;
pub const MAX_CREATURE_SNAPSHOT_BYTES: usize = MAX_CREATURE_OWNER_SET_BYTES + 128;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureSnapshotV1 {
    pub schema: u16,
    pub allocator: CreatureAllocatorSnapshotV1,
    /// Capture of the world's existing shared volume counter, including gaps.
    /// Installation must match the accompanying geometry checkpoint counter.
    pub next_volume_id: u64,
    pub owners: CreatureOwnershipSetV3,
}

impl<'de> Deserialize<'de> for CreatureSnapshotV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The allocator has a strict local reader without changing the core DTO
        // layout. The ownership set uses its cumulative seeded resource reader.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AllocatorWire {
            schema: u16,
            world_id: WorldId,
            world_epoch: u64,
            next_creature_id: u64,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema: u16,
            allocator: AllocatorWire,
            next_volume_id: u64,
            owners: CreatureOwnershipSetV3,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            schema: wire.schema,
            allocator: CreatureAllocatorSnapshotV1 {
                schema: wire.allocator.schema,
                world_id: wire.allocator.world_id,
                world_epoch: wire.allocator.world_epoch,
                next_creature_id: wire.allocator.next_creature_id,
            },
            next_volume_id: wire.next_volume_id,
            owners: wire.owners,
        })
    }
}

impl CreatureSnapshotV1 {
    pub fn validate(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureOwnershipError> {
        if self.schema != CREATURE_SNAPSHOT_SCHEMA {
            return Err(CreatureOwnershipError::Schema);
        }
        CreatureAllocator::restore(self.allocator)
            .map_err(|_| CreatureOwnershipError::Allocator)?;
        if self.next_volume_id == 0 {
            return Err(CreatureOwnershipError::Allocator);
        }
        self.owners.validate(catalog)?;
        for owner in &self.owners.records {
            let key = owner.state.key;
            if key.world_id != self.allocator.world_id
                || key.world_epoch.get() != self.allocator.world_epoch
                || key.creature_id.get() >= self.allocator.next_creature_id
                || owner
                    .parts
                    .iter()
                    .any(|part| part.source_volume_id.get() >= self.next_volume_id)
            {
                return Err(CreatureOwnershipError::Allocator);
            }
        }
        Ok(())
    }

    pub fn canonical_hash(&self) -> Hash32 {
        let mut writer = CanonicalWriter::with_domain(b"spall.creature-snapshot.v1");
        writer
            .u16(self.schema)
            .u16(self.allocator.schema)
            .blob(&self.allocator.world_id.to_le_bytes())
            .u64(self.allocator.world_epoch)
            .u64(self.allocator.next_creature_id)
            .u64(self.next_volume_id)
            .blob(&self.owners.canonical_hash().0);
        writer.hash()
    }

    /// Resource-bounded decoding followed by complete catalog and identity
    /// validation. Raw Serde decoding enforces resource bounds only.
    pub fn decode(
        bytes: &[u8],
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Self, CreatureOwnershipError> {
        if bytes.len() > MAX_CREATURE_SNAPSHOT_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        let (snapshot, rest): (Self, &[u8]) = postcard::take_from_bytes(bytes)
            .map_err(|error| CreatureOwnershipError::Encoding(error.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureOwnershipError::Encoding("trailing bytes".into()));
        }
        snapshot.validate(catalog)?;
        Ok(snapshot)
    }
}
