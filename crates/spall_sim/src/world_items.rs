//! Server-owned world items (ENG-257).
//!
//! Small things lying in the world that a player can pick up: a stable id, a
//! game-defined kind and a pose ([`WorldItem`]). They own no voxels and have no
//! physics. Only the owning thread creates or removes them, between ticks, and
//! every change is queued here until the host drains it for replication.

use std::collections::HashMap;

use spall_protocol::{WIRE_SCHEMA_VERSION, WorldItem, WorldItemUpdate};
use spall_voxel::Volume;

/// How far from a player's eye an item can be picked up, metres. The server
/// measures it from its own player; clients use it only for the prompt.
pub const ITEM_PICKUP_REACH_M: f64 = 3.0;

/// Why an item could not be created or restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorldItemError {
    #[error("world item kind must be nonzero and its pose finite")]
    Invalid,
    #[error("world item id {0} is already used or beyond the id counter")]
    BadId(u64),
}

/// The live items plus the changes not yet replicated.
#[derive(Debug)]
pub(crate) struct WorldItems {
    items: HashMap<u64, WorldItem>,
    /// The id the next spawn gets. Ids are never reused within a world.
    next_id: u64,
    spawned: Vec<WorldItem>,
    removed: Vec<u64>,
}

impl Default for WorldItems {
    fn default() -> Self {
        Self {
            items: HashMap::new(),
            next_id: 1,
            spawned: Vec::new(),
            removed: Vec::new(),
        }
    }
}

impl WorldItems {
    pub(crate) fn spawn(
        &mut self,
        kind: u16,
        translation_m: [f64; 3],
        yaw_rad: f32,
    ) -> Result<WorldItem, WorldItemError> {
        let item = WorldItem {
            item_id: self.next_id,
            kind,
            translation_m,
            yaw_rad,
        };
        if !item.is_valid() {
            return Err(WorldItemError::Invalid);
        }
        self.next_id += 1;
        self.items.insert(item.item_id, item);
        self.spawned.push(item);
        Ok(item)
    }

    pub(crate) fn remove(&mut self, item_id: u64) -> Option<WorldItem> {
        let item = self.items.remove(&item_id)?;
        // A spawn not yet announced needs no announced removal.
        if let Some(i) = self.spawned.iter().position(|s| s.item_id == item_id) {
            self.spawned.swap_remove(i);
        } else {
            self.removed.push(item_id);
        }
        Some(item)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &WorldItem> {
        self.items.values()
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Replaces every item (save recovery or a world reset). Nothing is
    /// queued: a client receives the new set with its next baseline.
    pub(crate) fn restore(
        &mut self,
        next_id: u64,
        items: Vec<WorldItem>,
    ) -> Result<(), WorldItemError> {
        let mut by_id = HashMap::with_capacity(items.len());
        for item in items {
            if !item.is_valid() {
                return Err(WorldItemError::Invalid);
            }
            if item.item_id >= next_id || by_id.insert(item.item_id, item).is_some() {
                return Err(WorldItemError::BadId(item.item_id));
            }
        }
        *self = Self {
            items: by_id,
            next_id: next_id.max(1),
            spawned: Vec::new(),
            removed: Vec::new(),
        };
        Ok(())
    }

    pub(crate) fn take_changes(&mut self) -> Vec<WorldItemUpdate> {
        updates(
            std::mem::take(&mut self.spawned),
            std::mem::take(&mut self.removed),
        )
    }
}

/// Packs spawns and removals into as few [`WorldItemUpdate`]s as the per-record
/// cap allows; no update when there is nothing to say.
pub fn updates(spawned: Vec<WorldItem>, removed: Vec<u64>) -> Vec<WorldItemUpdate> {
    use spall_protocol::limits::MAX_WORLD_ITEMS_PER_UPDATE as CAP;
    let mut out = Vec::new();
    let mut removed = removed.chunks(CAP);
    for chunk in spawned.chunks(CAP) {
        out.push(WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: chunk.to_vec(),
            removed: Vec::new(),
        });
    }
    if let Some(last) = out.last_mut() {
        let room = CAP - last.spawned.len();
        if let Some(first) = removed.next() {
            let (fits, rest) = first.split_at(first.len().min(room));
            last.removed = fits.to_vec();
            if !rest.is_empty() {
                out.push(WorldItemUpdate {
                    schema: WIRE_SCHEMA_VERSION,
                    spawned: Vec::new(),
                    removed: rest.to_vec(),
                });
            }
        }
    }
    for chunk in removed {
        out.push(WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: Vec::new(),
            removed: chunk.to_vec(),
        });
    }
    out
}

/// The nearest item within `reach_m` of `eye` (metres) that terrain does not
/// hide. Server pickup and the client prompt share this rule.
pub fn nearest_visible_item<'a>(
    volume: &Volume,
    eye: [f64; 3],
    reach_m: f64,
    items: impl IntoIterator<Item = &'a WorldItem>,
) -> Option<&'a WorldItem> {
    if !(eye.iter().all(|v| v.is_finite()) && reach_m.is_finite() && reach_m > 0.0) {
        return None;
    }
    let cell_m = volume.cell_size().metres();
    let origin = glam::DVec3::from_array(eye);
    let mut best: Option<(&WorldItem, f64)> = None;
    for item in items {
        // Aim a little above the item's resting point so the ray does not
        // graze the floor it lies on.
        let target = glam::DVec3::from_array(item.translation_m) + glam::DVec3::Y * 0.1;
        let d = target.distance(origin);
        if d >= reach_m || best.is_some_and(|(_, b)| d >= b) {
            continue;
        }
        let visible = d < 0.05
            || matches!(
                spall_voxel::query::cast_ray(
                    volume,
                    spall_voxel::Ray::new(origin / cell_m, target - origin),
                    spall_voxel::query::RayConfig::new((d - 0.05) / cell_m),
                ),
                Ok(spall_voxel::RayOutcome::Miss { .. })
            );
        if visible {
            best = Some((item, d));
        }
    }
    best.map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(item_id: u64, x: f64) -> WorldItem {
        WorldItem {
            item_id,
            kind: 1,
            translation_m: [x, 1.0, 0.0],
            yaw_rad: 0.0,
        }
    }

    #[test]
    fn ids_are_never_reused_and_unannounced_spawns_vanish_quietly() {
        let mut items = WorldItems::default();
        let a = items.spawn(3, [0.0; 3], 0.0).unwrap();
        let b = items.spawn(3, [1.0; 3], 0.0).unwrap();
        assert_eq!((a.item_id, b.item_id), (1, 2));
        items.remove(b.item_id).unwrap();
        let changes = items.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].spawned, vec![a]);
        assert!(changes[0].removed.is_empty());
        assert!(items.remove(b.item_id).is_none(), "taken once");
        items.remove(a.item_id).unwrap();
        assert_eq!(items.take_changes()[0].removed, vec![a.item_id]);
        assert_eq!(items.spawn(3, [0.0; 3], 0.0).unwrap().item_id, 3);
        assert_eq!(
            items.spawn(0, [0.0; 3], 0.0),
            Err(WorldItemError::Invalid),
            "kind 0 is not an item"
        );
    }

    #[test]
    fn restore_rejects_ids_at_or_past_the_counter_and_duplicates() {
        let mut items = WorldItems::default();
        assert_eq!(
            items.restore(2, vec![item(2, 0.0)]),
            Err(WorldItemError::BadId(2))
        );
        assert_eq!(
            items.restore(5, vec![item(1, 0.0), item(1, 1.0)]),
            Err(WorldItemError::BadId(1))
        );
        items.restore(5, vec![item(4, 0.0)]).unwrap();
        assert!(items.take_changes().is_empty());
        assert_eq!(items.spawn(1, [0.0; 3], 0.0).unwrap().item_id, 5);
    }

    #[test]
    fn large_change_sets_split_under_the_record_cap() {
        use spall_protocol::limits::MAX_WORLD_ITEMS_PER_UPDATE as CAP;
        let spawned: Vec<_> = (1..=CAP as u64 + 10).map(|i| item(i, 0.0)).collect();
        let removed: Vec<u64> = (5000..5000 + CAP as u64).collect();
        let out = updates(spawned, removed);
        assert!(out.iter().all(|u| u.spawned.len() + u.removed.len() <= CAP));
        assert_eq!(out.iter().map(|u| u.spawned.len()).sum::<usize>(), CAP + 10);
        assert_eq!(out.iter().map(|u| u.removed.len()).sum::<usize>(), CAP);
        assert!(updates(Vec::new(), Vec::new()).is_empty());
        assert_eq!(updates(Vec::new(), vec![1]).len(), 1);
    }
}
