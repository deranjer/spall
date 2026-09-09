//! Client application of the shared T18 residency policy.

use std::collections::BTreeSet;

use spall_core::{BrickCoord, VolumeId};
use spall_voxel::{
    BrickCacheKey, CacheBudget, CollisionReadiness, InterestRadii, MemoryReport, ResidencyCache,
};

use crate::ReplicaWorld;

/// Bounded client topology cache. Dynamic-body volumes stay complete while
/// terrain bricks can be evicted and re-enter through dependency baselines.
pub struct ClientResidency {
    pub cache: ResidencyCache,
    pub collision: CollisionReadiness,
    body_pins: BTreeSet<BrickCacheKey>,
}

impl ClientResidency {
    pub fn new(budget: CacheBudget, max_collision_sweep_bricks: usize) -> Self {
        Self {
            cache: ResidencyCache::new(budget),
            collision: CollisionReadiness::new(max_collision_sweep_bricks),
            body_pins: BTreeSet::new(),
        }
    }

    /// Reconciles accounting after a baseline, repair, or topology transaction.
    /// Replica geometry is already authoritative, so every registered revision
    /// is clean from the client's perspective.
    pub fn sync(&mut self, replica: &ReplicaWorld) {
        let mut volumes = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(terrain) = replica.terrain_volume() {
            volumes.push((terrain.id(), false));
        }
        volumes.extend(replica.body_volumes().map(|(_, volume)| (volume, true)));
        for (volume_id, pin_complete) in volumes {
            let Some(volume) = replica.volume(volume_id) else {
                continue;
            };
            for coord in volume.resident_brick_coords() {
                let snap = volume
                    .snapshot_brick(coord)
                    .ok()
                    .flatten()
                    .expect("resident coordinate");
                let key = BrickCacheKey::new(volume_id, coord);
                seen.insert(key);
                let bytes = usize::from(snap.is_dense()) * MemoryReport::DENSE_BRICK_BYTES;
                if self
                    .cache
                    .state(key)
                    .is_some_and(|state| state.revision != snap.revision())
                {
                    self.collision.invalidate(key);
                }
                self.cache.register(key, snap.revision(), bytes, true);
                if pin_complete && self.body_pins.insert(key) {
                    self.cache.pin(key);
                }
            }
        }
        let stale: Vec<_> = self
            .cache
            .keys()
            .filter(|key| !seen.contains(key))
            .collect();
        for key in stale {
            self.cache.remove(key);
            self.collision.invalidate(key);
        }
        self.body_pins.retain(|key| seen.contains(key));
    }

    pub fn update_terrain_interest(
        &mut self,
        replica: &ReplicaWorld,
        center: BrickCoord,
        radii: InterestRadii,
    ) {
        self.cache
            .update_interest(replica.terrain_volume_id(), center, radii);
    }

    pub fn enforce_budget(&mut self, replica: &mut ReplicaWorld) -> Vec<BrickCacheKey> {
        let plan = self.cache.plan_evictions();
        let mut evicted = Vec::new();
        for key in plan.evict {
            if replica.evict_brick(key.volume, key.coord) {
                self.cache.remove(key);
                self.collision.invalidate(key);
                evicted.push(key);
            }
        }
        evicted
    }

    pub fn desired_terrain(
        replica: &ReplicaWorld,
        center: BrickCoord,
        enter_radius: i64,
        max_bricks: usize,
    ) -> Option<Vec<BrickCacheKey>> {
        let volume: VolumeId = replica.terrain_volume_id();
        ResidencyCache::desired_coords(center, enter_radius, max_bricks).map(|coords| {
            coords
                .into_iter()
                .map(|coord| BrickCacheKey::new(volume, coord))
                .collect()
        })
    }
}
