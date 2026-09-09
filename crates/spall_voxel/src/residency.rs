//! Shared server/client brick-residency policy (T18).
//!
//! This module deliberately owns policy, not I/O: callers register the bricks
//! they actually hold, mark revisions durable after a checkpoint acknowledgement,
//! and apply the returned eviction plan to their authoritative world or replica.
//! That keeps disk and network runtimes out of voxel algorithms while giving both
//! hosts identical budget, hysteresis, and collision-readiness rules.

use std::collections::BTreeMap;

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, Revision, VolumeId};

/// Stable key for one brick in a multi-volume cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BrickCacheKey {
    pub volume: VolumeId,
    pub coord: BrickCoord,
}

impl BrickCacheKey {
    pub const fn new(volume: VolumeId, coord: BrickCoord) -> Self {
        Self { volume, coord }
    }
}

/// Hard resident ceilings. Dense bytes cover authoritative material payloads;
/// callers report renderer, physics, and snapshot memory separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheBudget {
    pub max_bricks: usize,
    pub max_dense_bytes: usize,
}

impl CacheBudget {
    pub const fn new(max_bricks: usize, max_dense_bytes: usize) -> Self {
        Self {
            max_bricks,
            max_dense_bytes,
        }
    }
}

/// Enter/retain radii in bricks. `retain` must be at least `enter`; the gap is
/// the hysteresis band that prevents boundary thrashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterestRadii {
    pub enter: i64,
    pub retain: i64,
}

impl InterestRadii {
    pub fn new(enter: i64, retain: i64) -> Option<Self> {
        (enter >= 0 && retain >= enter).then_some(Self { enter, retain })
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    revision: Revision,
    dense_bytes: usize,
    last_used: u64,
    interested: bool,
    dirty: bool,
    durable_revision: Option<Revision>,
    pins: u32,
}

/// Public diagnostic state for one tracked brick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEntryState {
    pub revision: Revision,
    pub interested: bool,
    pub dirty: bool,
    pub durable_revision: Option<Revision>,
    pub pins: u32,
}

/// A bounded eviction decision. Dirty or pinned bricks are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidencyPlan {
    pub evict: Vec<BrickCacheKey>,
    pub blocked_dirty: Vec<BrickCacheKey>,
    pub still_over_budget: bool,
}

/// LRU residency accounting shared by authoritative and replica hosts.
#[derive(Debug, Clone)]
pub struct ResidencyCache {
    budget: CacheBudget,
    entries: BTreeMap<BrickCacheKey, Entry>,
    clock: u64,
}

impl ResidencyCache {
    pub fn new(budget: CacheBudget) -> Self {
        Self {
            budget,
            entries: BTreeMap::new(),
            clock: 0,
        }
    }

    pub fn budget(&self) -> CacheBudget {
        self.budget
    }

    pub fn set_budget(&mut self, budget: CacheBudget) {
        self.budget = budget;
    }

    pub fn resident_bricks(&self) -> usize {
        self.entries.len()
    }

    pub fn resident_dense_bytes(&self) -> usize {
        self.entries.values().map(|e| e.dense_bytes).sum()
    }

    pub fn keys(&self) -> impl Iterator<Item = BrickCacheKey> + '_ {
        self.entries.keys().copied()
    }

    /// Register or refresh an actually resident brick.
    pub fn register(
        &mut self,
        key: BrickCacheKey,
        revision: Revision,
        dense_bytes: usize,
        durable: bool,
    ) {
        self.clock = self.clock.saturating_add(1);
        let prior = self.entries.get(&key).copied();
        self.entries.insert(
            key,
            Entry {
                revision,
                dense_bytes,
                last_used: self.clock,
                interested: prior.is_some_and(|e| e.interested),
                dirty: prior.is_some_and(|e| e.dirty && e.revision == revision) || !durable,
                durable_revision: if durable {
                    Some(revision)
                } else {
                    prior.and_then(|e| e.durable_revision)
                },
                pins: prior.map_or(0, |e| e.pins),
            },
        );
    }

    pub fn remove(&mut self, key: BrickCacheKey) {
        self.entries.remove(&key);
    }

    pub fn state(&self, key: BrickCacheKey) -> Option<CacheEntryState> {
        self.entries.get(&key).map(|e| CacheEntryState {
            revision: e.revision,
            interested: e.interested,
            dirty: e.dirty,
            durable_revision: e.durable_revision,
            pins: e.pins,
        })
    }

    pub fn mark_dirty(&mut self, key: BrickCacheKey, revision: Revision) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.revision = revision;
            entry.dirty = true;
        }
    }

    /// Called only after durable acknowledgement of a checkpoint containing
    /// this exact-or-newer revision.
    pub fn mark_durable(&mut self, key: BrickCacheKey, revision: Revision) {
        if let Some(entry) = self.entries.get_mut(&key)
            && revision >= entry.revision
        {
            entry.durable_revision = Some(revision);
            entry.dirty = false;
        }
    }

    /// Pin count supports overlapping physics, structural, baseline, and job
    /// users without a global singleton or a single boolean owner.
    pub fn pin(&mut self, key: BrickCacheKey) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pins = entry.pins.saturating_add(1);
        }
    }

    pub fn unpin(&mut self, key: BrickCacheKey) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pins = entry.pins.saturating_sub(1);
        }
    }

    /// Refresh interest for one volume. Existing entries inside `enter` are
    /// touched; entries already interested remain so through `retain`.
    pub fn update_interest(&mut self, volume: VolumeId, center: BrickCoord, radii: InterestRadii) {
        self.clock = self.clock.saturating_add(1);
        for (key, entry) in &mut self.entries {
            if key.volume != volume {
                continue;
            }
            let distance = chebyshev(key.coord, center);
            let interested = distance <= radii.enter as u64
                || (entry.interested && distance <= radii.retain as u64);
            entry.interested = interested;
            if distance <= radii.enter as u64 {
                entry.last_used = self.clock;
            }
        }
    }

    /// Coordinates the caller should request for the inner interest cube.
    pub fn desired_coords(
        center: BrickCoord,
        enter_radius: i64,
        max_bricks: usize,
    ) -> Option<Vec<BrickCoord>> {
        let diameter = enter_radius.checked_mul(2)?.checked_add(1)?;
        let count = usize::try_from(diameter).ok()?.checked_pow(3)?;
        if enter_radius < 0 || count > max_bricks {
            return None;
        }
        let min = BrickCoord::new(
            center.x.checked_sub(enter_radius)?,
            center.y.checked_sub(enter_radius)?,
            center.z.checked_sub(enter_radius)?,
        );
        let max = BrickCoord::new(
            center.x.checked_add(enter_radius)?,
            center.y.checked_add(enter_radius)?,
            center.z.checked_add(enter_radius)?,
        );
        let mut out = Vec::with_capacity(count);
        for z in min.z..=max.z {
            for y in min.y..=max.y {
                for x in min.x..=max.x {
                    out.push(BrickCoord::new(x, y, z));
                }
            }
        }
        Some(out)
    }

    /// Chooses oldest clean, durable, unpinned, uninterested entries until both
    /// ceilings are met. The caller removes entries only after world eviction.
    pub fn plan_evictions(&self) -> ResidencyPlan {
        let mut bricks = self.resident_bricks();
        let mut dense = self.resident_dense_bytes();
        let mut candidates: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.interested && e.pins == 0 && !e.dirty)
            .map(|(&key, e)| (e.last_used, key, e.dense_bytes))
            .collect();
        candidates.sort_by_key(|(last, key, _)| (*last, *key));

        let mut evict = Vec::new();
        for (_, key, bytes) in candidates {
            if bricks <= self.budget.max_bricks && dense <= self.budget.max_dense_bytes {
                break;
            }
            evict.push(key);
            bricks = bricks.saturating_sub(1);
            dense = dense.saturating_sub(bytes);
        }
        let blocked_dirty =
            if bricks > self.budget.max_bricks || dense > self.budget.max_dense_bytes {
                self.entries
                    .iter()
                    .filter(|(_, e)| !e.interested && e.pins == 0 && e.dirty)
                    .map(|(&key, _)| key)
                    .collect()
            } else {
                Vec::new()
            };
        ResidencyPlan {
            evict,
            blocked_dirty,
            still_over_budget: bricks > self.budget.max_bricks
                || dense > self.budget.max_dense_bytes,
        }
    }
}

fn chebyshev(a: BrickCoord, b: BrickCoord) -> u64 {
    a.x.abs_diff(b.x)
        .max(a.y.abs_diff(b.y))
        .max(a.z.abs_diff(b.z))
}

/// Result of checking a swept local-space sphere/capsule against collision
/// residency. Unknown collision is never interpreted as empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionAdmission {
    Ready,
    Blocked { missing: Vec<BrickCoord> },
    SweepTooLarge,
    NonFinite,
}

/// Versioned set of bricks whose collision matches authoritative geometry.
#[derive(Debug, Clone)]
pub struct CollisionReadiness {
    ready: BTreeMap<BrickCacheKey, Revision>,
    max_sweep_bricks: usize,
}

impl CollisionReadiness {
    pub fn new(max_sweep_bricks: usize) -> Self {
        Self {
            ready: BTreeMap::new(),
            max_sweep_bricks,
        }
    }

    pub fn mark_ready(&mut self, key: BrickCacheKey, revision: Revision) {
        self.ready.insert(key, revision);
    }

    pub fn ready_revision(&self, key: BrickCacheKey) -> Option<Revision> {
        self.ready.get(&key).copied()
    }

    pub fn invalidate(&mut self, key: BrickCacheKey) {
        self.ready.remove(&key);
    }

    /// Checks every brick touched by the swept AABB. Positions are volume-local
    /// metres; dynamic bodies transform the sweep into local space first.
    pub fn admit_sweep(
        &self,
        volume: VolumeId,
        cell_size: CellSizeCode,
        from_m: [f64; 3],
        to_m: [f64; 3],
        radius_m: f64,
    ) -> CollisionAdmission {
        if !radius_m.is_finite()
            || radius_m < 0.0
            || from_m.iter().chain(&to_m).any(|v| !v.is_finite())
        {
            return CollisionAdmission::NonFinite;
        }
        let cell_m = cell_size.metres();
        let mut lo = [0i64; 3];
        let mut hi = [0i64; 3];
        for axis in 0..3 {
            lo[axis] = ((from_m[axis].min(to_m[axis]) - radius_m) / cell_m).floor() as i64;
            hi[axis] = ((from_m[axis].max(to_m[axis]) + radius_m) / cell_m).floor() as i64;
        }
        let (min_b, _) = GlobalCell::new(lo[0], lo[1], lo[2]).split();
        let (max_b, _) = GlobalCell::new(hi[0], hi[1], hi[2]).split();
        let nx = min_b.x.abs_diff(max_b.x).saturating_add(1) as usize;
        let ny = min_b.y.abs_diff(max_b.y).saturating_add(1) as usize;
        let nz = min_b.z.abs_diff(max_b.z).saturating_add(1) as usize;
        let count = nx.saturating_mul(ny).saturating_mul(nz);
        if count > self.max_sweep_bricks {
            return CollisionAdmission::SweepTooLarge;
        }
        let mut missing = Vec::new();
        for z in min_b.z..=max_b.z {
            for y in min_b.y..=max_b.y {
                for x in min_b.x..=max_b.x {
                    let coord = BrickCoord::new(x, y, z);
                    if !self.ready.contains_key(&BrickCacheKey::new(volume, coord)) {
                        missing.push(coord);
                    }
                }
            }
        }
        if missing.is_empty() {
            CollisionAdmission::Ready
        } else {
            CollisionAdmission::Blocked { missing }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vid() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    #[test]
    fn hysteresis_retains_then_releases_a_boundary_brick() {
        let mut cache = ResidencyCache::new(CacheBudget::new(8, usize::MAX));
        let key = BrickCacheKey::new(vid(), BrickCoord::new(2, 0, 0));
        cache.register(key, Revision(1), 0, true);
        let radii = InterestRadii::new(2, 3).unwrap();
        cache.update_interest(vid(), BrickCoord::new(0, 0, 0), radii);
        assert!(cache.state(key).unwrap().interested);
        cache.update_interest(vid(), BrickCoord::new(-1, 0, 0), radii);
        assert!(
            cache.state(key).unwrap().interested,
            "retained in hysteresis band"
        );
        cache.update_interest(vid(), BrickCoord::new(-2, 0, 0), radii);
        assert!(!cache.state(key).unwrap().interested);
    }

    #[test]
    fn dirty_data_blocks_eviction_until_durable_ack() {
        let mut cache = ResidencyCache::new(CacheBudget::new(0, 0));
        let key = BrickCacheKey::new(vid(), BrickCoord::new(0, 0, 0));
        cache.register(key, Revision(4), 65_536, false);
        let blocked = cache.plan_evictions();
        assert!(blocked.evict.is_empty());
        assert_eq!(blocked.blocked_dirty, vec![key]);
        assert!(blocked.still_over_budget);
        cache.mark_durable(key, Revision(4));
        assert_eq!(cache.plan_evictions().evict, vec![key]);
    }

    #[test]
    fn desired_interest_is_bounded_before_allocation() {
        assert_eq!(
            ResidencyCache::desired_coords(BrickCoord::new(0, 0, 0), 1, 27)
                .unwrap()
                .len(),
            27
        );
        assert!(ResidencyCache::desired_coords(BrickCoord::new(0, 0, 0), 2, 64).is_none());
        assert!(ResidencyCache::desired_coords(BrickCoord::new(i64::MAX, 0, 0), 1, 27).is_none());
    }

    #[test]
    fn collision_sweep_stops_at_the_first_unready_neighborhood() {
        let mut gate = CollisionReadiness::new(16);
        let a = BrickCacheKey::new(vid(), BrickCoord::new(0, 0, 0));
        gate.mark_ready(a, Revision(1));
        let blocked = gate.admit_sweep(
            vid(),
            CellSizeCode::Quarter,
            [1.0, 1.0, 1.0],
            [12.0, 1.0, 1.0],
            0.25,
        );
        assert_eq!(
            blocked,
            CollisionAdmission::Blocked {
                missing: vec![BrickCoord::new(1, 0, 0)]
            }
        );
        gate.mark_ready(
            BrickCacheKey::new(vid(), BrickCoord::new(1, 0, 0)),
            Revision(1),
        );
        assert_eq!(
            gate.admit_sweep(
                vid(),
                CellSizeCode::Quarter,
                [1.0, 1.0, 1.0],
                [12.0, 1.0, 1.0],
                0.25,
            ),
            CollisionAdmission::Ready
        );
    }
}
