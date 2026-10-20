//! Late-join creature synchronization record.
//!
//! A joining replica installs terrain from the baseline and then receives one
//! `CreatureSync` per living or dead creature the server held at the join
//! cursor, followed by the ordered transactions committed since. Each record is
//! a complete portable ownership record plus the allocator floor needed to
//! validate later births, and is installed idempotently.

use crate::records::WIRE_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use spall_core::CreatureAllocatorSnapshotV1;

use crate::CreatureOwnershipRecordV4;
use crate::records::{Record, RecordError, WireTag};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureSync {
    pub schema: u16,
    /// Server allocator at capture; every later birth id is at least this.
    pub allocator: CreatureAllocatorSnapshotV1,
    /// First shared source-volume id not used at capture.
    pub next_volume_id: u64,
    pub record: CreatureOwnershipRecordV4,
}

impl Record for CreatureSync {
    const TAG: WireTag = WireTag::CreatureSync;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent(
                "unsupported creature sync schema",
            ));
        }
        if self.allocator.schema != spall_core::STORE_SCHEMA_VERSION_U16
            || self.allocator.world_id.is_nil()
            || self.allocator.world_epoch == 0
            || self.allocator.next_creature_id == 0
            || self.next_volume_id == 0
        {
            return Err(RecordError::Inconsistent("invalid creature sync allocator"));
        }
        let key = self.record.state.key;
        if key.world_id != self.allocator.world_id
            || key.world_epoch.get() != self.allocator.world_epoch
            || key.creature_id.get() >= self.allocator.next_creature_id
            || self
                .record
                .parts
                .iter()
                .any(|part| part.source_volume_id.get() >= self.next_volume_id)
        {
            return Err(RecordError::Inconsistent(
                "creature sync record is outside its allocator",
            ));
        }
        Ok(())
    }
}
