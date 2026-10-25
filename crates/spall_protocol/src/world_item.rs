//! Replicated world items (ENG-257).
//!
//! A world item is a small server-owned thing lying in the world that a player
//! can pick up into their pack: a stable item id (never an ECS id), a
//! game-defined kind, and a pose. The server creates and removes items at tick
//! boundaries and announces the changes in [`WorldItemUpdate`]s on the ordered
//! reliable stream. A joining client receives the items that exist at its
//! baseline capture as spawns ahead of every later update.

use serde::{Deserialize, Serialize};

use crate::limits::MAX_WORLD_ITEMS_PER_UPDATE;
use crate::records::{Record, RecordError, WIRE_SCHEMA_VERSION, WireTag};

/// One item lying in the world.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldItem {
    /// Server-assigned stable id, unique for the life of the world (and saved).
    pub item_id: u64,
    /// Game-defined item kind: the same numeric id the game's inventory uses
    /// in [`crate::InventoryEntry::item_id`].
    pub kind: u16,
    /// Item origin in world metres.
    pub translation_m: [f64; 3],
    /// Rotation about +Y, radians.
    pub yaw_rad: f32,
}

impl WorldItem {
    /// Nonzero id and kind, finite pose.
    pub fn is_valid(&self) -> bool {
        self.item_id != 0
            && self.kind != 0
            && self.translation_m.iter().all(|v| v.is_finite())
            && self.yaw_rad.is_finite()
    }
}

/// Items that appeared (or, in a join list, exist) and items that left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldItemUpdate {
    pub schema: u16,
    pub spawned: Vec<WorldItem>,
    pub removed: Vec<u64>,
}

impl Record for WorldItemUpdate {
    const TAG: WireTag = WireTag::WorldItemUpdate;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent("unsupported world item schema"));
        }
        if self.spawned.len() + self.removed.len() > MAX_WORLD_ITEMS_PER_UPDATE {
            return Err(RecordError::OutOfRange {
                field: "WorldItemUpdate",
                detail: "too many items in one update",
            });
        }
        if !self.spawned.iter().all(WorldItem::is_valid) || self.removed.contains(&0) {
            return Err(RecordError::Inconsistent("invalid world item"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_control, encode_control};

    fn item(item_id: u64) -> WorldItem {
        WorldItem {
            item_id,
            kind: 7,
            translation_m: [1.0, 2.0, 3.0],
            yaw_rad: 0.5,
        }
    }

    #[test]
    fn round_trips_and_rejects_bad_items() {
        let update = WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: vec![item(1), item(2)],
            removed: vec![9],
        };
        let bytes = encode_control(&update).unwrap();
        assert_eq!(decode_control::<WorldItemUpdate>(&bytes).unwrap(), update);

        let mut bad = update.clone();
        bad.spawned[0].translation_m[1] = f64::NAN;
        assert!(bad.validate().is_err());
        let mut bad = update.clone();
        bad.removed.push(0);
        assert!(bad.validate().is_err());
        let full = WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: (1..=MAX_WORLD_ITEMS_PER_UPDATE as u64).map(item).collect(),
            removed: Vec::new(),
        };
        // A full update still fits one control record.
        assert!(encode_control(&full).is_ok());
        let mut over = full;
        over.removed.push(1);
        assert!(over.validate().is_err());
    }
}
