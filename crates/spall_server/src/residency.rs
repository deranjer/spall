//! Streamed authoritative residency integration (T18).
//!
//! The shared LRU/hysteresis/collision policy lives in `spall_voxel`; this
//! module joins it to `SimWorld`, durable brick DTOs, streamed structural
//! analysis, and a spatial *reference* index for indivisible dynamic bodies.
//! Storage partitions never become geometry owners.

use std::collections::{BTreeMap, BTreeSet};

use glam::DVec3;
use spall_core::{
    BrickCoord, CELLS_PER_BRICK, CellSizeCode, EntityId, LocalCell, MaterialId, Revision, VolumeId,
};
use spall_protocol::{CanonicalOwner, canonical_topology_hash};
use spall_sim::{SimWorld, Simulation};
use spall_store::{Checkpoint, StoredBodyKind, StoredBrick, decode_cells, encode_cells};
use spall_structure::{
    AnchorPlane, CancelToken, ResidencyMode, StructureIndex, Support, SupportReport,
};
use spall_voxel::{
    Brick, BrickBounds, BrickCacheKey, CacheBudget, CollisionReadiness, MemoryReport,
    ResidencyCache, Volume,
};

/// One fixed world-space storage partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionCoord {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl PartitionCoord {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }
}

/// Compact metadata retained when geometry is evicted. It can identify a
/// relevant dependency, but geometry is loaded before support is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphBrickMeta {
    pub revision: Revision,
    pub solid_cells: u32,
    /// Bits `-X,+X,-Y,+Y,-Z,+Z` identify faces touched by solid cells.
    pub occupied_faces: u8,
    pub touches_anchor: bool,
}

/// Explicit backing result; unavailable is never collapsed into empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackingLoad {
    Brick(StoredBrick),
    KnownEmpty,
    Unavailable,
}

/// Persistence/generation boundary. `persist` must return only after the brick
/// revision is durable; failed calls are never acknowledged or evicted.
pub trait ResidencyBacking {
    type Error: std::error::Error + Send + Sync + 'static;

    fn load(&self, key: BrickCacheKey) -> Result<BackingLoad, Self::Error>;
    fn persist(&mut self, brick: StoredBrick) -> Result<(), Self::Error>;
    /// Durable brick records available to complete a checkpoint after live
    /// voxel eviction. Implementations return a bounded snapshot, not a live
    /// database iterator held across the simulation tick.
    fn records(&self) -> Result<Vec<StoredBrick>, Self::Error>;
}

/// Deterministic backing for fixtures and embedded/offline hosts.
#[derive(Debug, Default, Clone)]
pub struct MemoryBrickBacking {
    bricks: BTreeMap<BrickCacheKey, StoredBrick>,
    known_empty: BTreeSet<BrickCacheKey>,
}

#[derive(Debug, thiserror::Error)]
#[error("infallible memory backing")]
pub struct MemoryBackingError;

impl MemoryBrickBacking {
    pub fn insert(&mut self, brick: StoredBrick) {
        let key = key_of(&brick);
        self.known_empty.remove(&key);
        self.bricks.insert(key, brick);
    }

    pub fn mark_known_empty(&mut self, key: BrickCacheKey) {
        self.bricks.remove(&key);
        self.known_empty.insert(key);
    }

    pub fn get(&self, key: BrickCacheKey) -> Option<&StoredBrick> {
        self.bricks.get(&key)
    }
}

impl ResidencyBacking for MemoryBrickBacking {
    type Error = MemoryBackingError;

    fn load(&self, key: BrickCacheKey) -> Result<BackingLoad, Self::Error> {
        Ok(if let Some(brick) = self.bricks.get(&key) {
            BackingLoad::Brick(brick.clone())
        } else if self.known_empty.contains(&key) {
            BackingLoad::KnownEmpty
        } else {
            BackingLoad::Unavailable
        })
    }

    fn persist(&mut self, brick: StoredBrick) -> Result<(), Self::Error> {
        self.insert(brick);
        Ok(())
    }

    fn records(&self) -> Result<Vec<StoredBrick>, Self::Error> {
        Ok(self.bricks.values().cloned().collect())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResidencyError {
    #[error("unknown volume {0}")]
    UnknownVolume(VolumeId),
    #[error("brick access: {0}")]
    Access(#[from] spall_voxel::AccessError),
    #[error("brick codec: {0}")]
    Codec(#[from] spall_store::BrickCodecError),
    #[error("backing: {0}")]
    Backing(String),
    #[error("structural analysis interrupted: {0}")]
    Structural(#[from] spall_structure::Interrupted),
    #[error("checkpoint completion: {0}")]
    Checkpoint(String),
}

/// Ready only when a streamed graph has no unknown support.
#[derive(Debug, Clone)]
pub enum StructuralResolution {
    Ready {
        index: Box<StructureIndex>,
        loaded: Vec<BrickCacheKey>,
    },
    Pending {
        missing: Vec<BrickCacheKey>,
        loaded: Vec<BrickCacheKey>,
    },
}

/// Server-side owner of cache state and retained structural metadata.
pub struct ResidencyController<B: ResidencyBacking> {
    pub cache: ResidencyCache,
    pub collision: CollisionReadiness,
    backing: B,
    graph_meta: BTreeMap<BrickCacheKey, GraphBrickMeta>,
    body_pins: BTreeSet<BrickCacheKey>,
}

impl<B: ResidencyBacking> ResidencyController<B> {
    pub fn new(budget: CacheBudget, max_collision_sweep_bricks: usize, backing: B) -> Self {
        Self {
            cache: ResidencyCache::new(budget),
            collision: CollisionReadiness::new(max_collision_sweep_bricks),
            backing,
            graph_meta: BTreeMap::new(),
            body_pins: BTreeSet::new(),
        }
    }

    pub fn backing(&self) -> &B {
        &self.backing
    }

    pub fn backing_mut(&mut self) -> &mut B {
        &mut self.backing
    }

    pub fn graph_metadata(&self, key: BrickCacheKey) -> Option<GraphBrickMeta> {
        self.graph_meta.get(&key).copied()
    }

    /// Registers resident geometry. Dynamic-body bricks are pinned complete:
    /// partitions reference a body but never split its volume ownership.
    pub fn register_world(&mut self, world: &SimWorld, durable: bool) {
        let mut seen = BTreeSet::new();
        let terrain = world.terrain();
        self.register_volume(&terrain.volume, world.anchor(), durable, false);
        seen.extend(
            terrain
                .volume
                .resident_brick_coords()
                .into_iter()
                .map(|coord| BrickCacheKey::new(terrain.volume_id, coord)),
        );
        for body in world.bodies() {
            self.register_volume(&body.volume, world.anchor(), durable, true);
            seen.extend(
                body.volume
                    .resident_brick_coords()
                    .into_iter()
                    .map(|coord| BrickCacheKey::new(body.volume_id, coord)),
            );
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

    fn register_volume(
        &mut self,
        volume: &Volume,
        anchor: AnchorPlane,
        durable: bool,
        pin_complete: bool,
    ) {
        for coord in volume.resident_brick_coords() {
            let snap = volume
                .snapshot_brick(coord)
                .ok()
                .flatten()
                .expect("resident coordinate has a snapshot");
            let key = BrickCacheKey::new(volume.id(), coord);
            let dense = usize::from(snap.is_dense()) * MemoryReport::DENSE_BRICK_BYTES;
            self.cache.register(key, snap.revision(), dense, durable);
            self.graph_meta
                .insert(key, graph_meta(&snap, coord, anchor));
            if pin_complete && self.body_pins.insert(key) {
                self.cache.pin(key);
            }
        }
    }

    /// Writes all resident bricks to the backing and marks their exact
    /// revisions durable. This models checkpoint acknowledgement, not enqueue.
    pub fn persist_volume(&mut self, volume: &Volume) -> Result<(), ResidencyError> {
        for coord in volume.resident_brick_coords() {
            let record = stored_brick(volume, coord)?;
            let revision = Revision(record.revision);
            self.backing
                .persist(record)
                .map_err(|e| ResidencyError::Backing(e.to_string()))?;
            self.cache
                .mark_durable(BrickCacheKey::new(volume.id(), coord), revision);
        }
        Ok(())
    }

    /// Captures a checkpoint that includes both live and durably evicted
    /// bricks, then recomputes its canonical full-world hash. This prevents a
    /// later checkpoint from deleting older durable terrain merely because it
    /// was outside the live cache at capture time.
    pub fn capture_checkpoint(
        &self,
        sim: &Simulation,
        config: &crate::PersistConfig,
        journal_cursor: u64,
    ) -> Result<Checkpoint, ResidencyError> {
        let mut checkpoint = crate::persist::capture(sim, config, journal_cursor)
            .map_err(|e| ResidencyError::Checkpoint(e.to_string()))?;
        let live_volumes: BTreeSet<u64> = checkpoint.bodies.iter().map(|b| b.volume_id).collect();
        let mut bricks: BTreeMap<BrickCacheKey, StoredBrick> = self
            .backing
            .records()
            .map_err(|e| ResidencyError::Backing(e.to_string()))?
            .into_iter()
            .filter(|b| live_volumes.contains(&b.volume_id))
            .map(|b| (key_of(&b), b))
            .collect();
        for brick in checkpoint.bricks.drain(..) {
            bricks.insert(key_of(&brick), brick);
        }
        checkpoint.bricks = bricks.into_values().collect();
        checkpoint
            .bricks
            .sort_by_key(|b| (b.volume_id, b.coord[2], b.coord[1], b.coord[0]));
        checkpoint.world_hash = checkpoint_hash(&checkpoint)?;
        Ok(checkpoint)
    }

    pub fn mark_dirty(
        &mut self,
        world: &SimWorld,
        key: BrickCacheKey,
    ) -> Result<(), ResidencyError> {
        let volume = world
            .volume_ref(key.volume)
            .ok_or(ResidencyError::UnknownVolume(key.volume))?;
        let revision = volume
            .brick_revision(key.coord)?
            .ok_or(ResidencyError::UnknownVolume(key.volume))?;
        self.cache.mark_dirty(key, revision);
        self.collision.invalidate(key);
        Ok(())
    }

    /// Persists dirty eviction candidates synchronously, then drops only clean
    /// acknowledged bricks. A backing error leaves geometry resident.
    pub fn enforce_budget(
        &mut self,
        world: &mut SimWorld,
    ) -> Result<Vec<BrickCacheKey>, ResidencyError> {
        let first = self.cache.plan_evictions();
        for key in first.blocked_dirty {
            let volume = world
                .volume_ref(key.volume)
                .ok_or(ResidencyError::UnknownVolume(key.volume))?;
            let record = stored_brick(volume, key.coord)?;
            let revision = Revision(record.revision);
            self.backing
                .persist(record)
                .map_err(|e| ResidencyError::Backing(e.to_string()))?;
            self.cache.mark_durable(key, revision);
        }

        let plan = self.cache.plan_evictions();
        let mut evicted = Vec::new();
        for key in plan.evict {
            let body = world
                .volume_body_mut(key.volume)
                .ok_or(ResidencyError::UnknownVolume(key.volume))?;
            body.volume.evict_brick(key.coord);
            self.cache.remove(key);
            self.collision.invalidate(key);
            evicted.push(key);
        }
        Ok(evicted)
    }

    /// Loads one brick without guessing. A known empty slot becomes a resident
    /// air brick; unavailable data stays absent and keeps callers pending.
    pub fn load_brick(
        &mut self,
        world: &mut SimWorld,
        key: BrickCacheKey,
    ) -> Result<bool, ResidencyError> {
        let loaded = self
            .backing
            .load(key)
            .map_err(|e| ResidencyError::Backing(e.to_string()))?;
        let brick = match loaded {
            BackingLoad::Brick(record) => brick_from_stored(&record)?,
            BackingLoad::KnownEmpty => Brick::empty(),
            BackingLoad::Unavailable => return Ok(false),
        };
        let anchor = world.anchor();
        let revision = brick.revision();
        let dense = usize::from(brick.is_dense()) * MemoryReport::DENSE_BRICK_BYTES;
        let budget = self.cache.budget();
        if self.cache.state(key).is_none()
            && (self.cache.resident_bricks().saturating_add(1) > budget.max_bricks
                || self.cache.resident_dense_bytes().saturating_add(dense) > budget.max_dense_bytes)
        {
            // Reserve one slot/its payload before installation. Existing
            // interest and pins still win; if they consume the whole budget,
            // the load remains pending instead of oversubscribing silently.
            self.cache.set_budget(CacheBudget::new(
                budget.max_bricks.saturating_sub(1),
                budget.max_dense_bytes.saturating_sub(dense),
            ));
            let eviction = self.enforce_budget(world);
            self.cache.set_budget(budget);
            eviction?;
        }
        if self.cache.state(key).is_none()
            && (self.cache.resident_bricks().saturating_add(1) > budget.max_bricks
                || self.cache.resident_dense_bytes().saturating_add(dense) > budget.max_dense_bytes)
        {
            return Ok(false);
        }
        let volume = &mut world
            .volume_body_mut(key.volume)
            .ok_or(ResidencyError::UnknownVolume(key.volume))?
            .volume;
        volume.insert_brick(key.coord, brick)?;
        self.cache.register(key, revision, dense, true);
        let snap = volume.snapshot_brick(key.coord)?.expect("just inserted");
        self.graph_meta
            .insert(key, graph_meta(&snap, key.coord, anchor));
        Ok(true)
    }

    /// Iteratively follows `Support::Unknown` dependencies. Retained metadata
    /// guides loading, but only resident voxels can resolve support.
    pub fn resolve_structure(
        &mut self,
        world: &mut SimWorld,
        volume_id: VolumeId,
        max_loads: usize,
    ) -> Result<StructuralResolution, ResidencyError> {
        let mut loaded = Vec::new();
        loop {
            let volume = world
                .volume_ref(volume_id)
                .ok_or(ResidencyError::UnknownVolume(volume_id))?;
            let index = StructureIndex::build(
                volume,
                world.anchor(),
                ResidencyMode::Streamed,
                world.generation(),
                world.topology_epoch(),
                &CancelToken::new(),
            )?;
            let missing = unknown_dependencies(&index.report(), volume_id);
            if missing.is_empty() {
                for key in &loaded {
                    self.cache.pin(*key);
                }
                return Ok(StructuralResolution::Ready {
                    index: Box::new(index),
                    loaded,
                });
            }
            let mut progressed = false;
            for key in &missing {
                if loaded.len() >= max_loads {
                    return Ok(StructuralResolution::Pending { missing, loaded });
                }
                if self.load_brick(world, *key)? {
                    loaded.push(*key);
                    progressed = true;
                }
            }
            if !progressed {
                return Ok(StructuralResolution::Pending { missing, loaded });
            }
        }
    }
}

fn unknown_dependencies(report: &SupportReport, volume: VolumeId) -> Vec<BrickCacheKey> {
    let mut set = BTreeSet::new();
    for component in &report.components {
        if let Support::Unknown { missing } = &component.support {
            set.extend(
                missing
                    .iter()
                    .copied()
                    .map(|coord| BrickCacheKey::new(volume, coord)),
            );
        }
    }
    set.into_iter().collect()
}

fn graph_meta(
    snap: &spall_voxel::BrickSnapshot,
    coord: BrickCoord,
    anchor: AnchorPlane,
) -> GraphBrickMeta {
    let mut solid_cells = 0u32;
    let mut occupied_faces = 0u8;
    let mut touches_anchor = false;
    for i in 0..CELLS_PER_BRICK as u16 {
        let local = LocalCell::from_linear_index(i).expect("brick index");
        if snap.get(local).is_air() {
            continue;
        }
        solid_cells += 1;
        occupied_faces |= u8::from(local.x() == 0);
        occupied_faces |= u8::from(local.x() == 31) << 1;
        occupied_faces |= u8::from(local.y() == 0) << 2;
        occupied_faces |= u8::from(local.y() == 31) << 3;
        occupied_faces |= u8::from(local.z() == 0) << 4;
        occupied_faces |= u8::from(local.z() == 31) << 5;
        let global_y = coord.y.saturating_mul(32) + i64::from(local.y());
        touches_anchor |= global_y == anchor.y;
    }
    GraphBrickMeta {
        revision: snap.revision(),
        solid_cells,
        occupied_faces,
        touches_anchor,
    }
}

fn stored_brick(volume: &Volume, coord: BrickCoord) -> Result<StoredBrick, ResidencyError> {
    let snap = volume.snapshot_brick(coord)?.expect("resident coordinate");
    let mut cells = vec![0u16; CELLS_PER_BRICK];
    for (i, value) in cells.iter_mut().enumerate() {
        let local = LocalCell::from_linear_index(i as u16).expect("brick index");
        *value = snap.get(local).raw();
    }
    Ok(StoredBrick {
        volume_id: volume.id().get(),
        coord: [coord.x, coord.y, coord.z],
        revision: snap.revision().get(),
        edited: snap.is_edited(),
        payload: encode_cells(&cells)?,
    })
}

fn brick_from_stored(record: &StoredBrick) -> Result<Brick, ResidencyError> {
    let cells = decode_cells(&record.payload)?;
    let materials: Vec<MaterialId> = cells.into_iter().map(MaterialId).collect();
    Ok(Brick::restored(
        &materials,
        Revision(record.revision),
        record.edited,
    ))
}

fn key_of(record: &StoredBrick) -> BrickCacheKey {
    BrickCacheKey::new(
        VolumeId::new(record.volume_id).expect("stored volume ids are non-zero"),
        BrickCoord::new(record.coord[0], record.coord[1], record.coord[2]),
    )
}

fn checkpoint_hash(checkpoint: &Checkpoint) -> Result<[u8; 32], ResidencyError> {
    let mut canonical = Vec::new();
    for body in &checkpoint.bodies {
        let volume_id =
            VolumeId::new(body.volume_id).map_err(|e| ResidencyError::Checkpoint(e.to_string()))?;
        let cell_size = CellSizeCode::from_u8(body.cell_size_code)
            .ok_or_else(|| ResidencyError::Checkpoint("unknown cell-size code".to_string()))?;
        let mut volume = if let Some([min, max]) = body.volume_bounds {
            let bounds = BrickBounds::new(
                BrickCoord::new(min[0], min[1], min[2]),
                BrickCoord::new(max[0], max[1], max[2]),
            )
            .ok_or_else(|| ResidencyError::Checkpoint("invalid volume bounds".to_string()))?;
            Volume::bounded(volume_id, cell_size, bounds)
        } else {
            Volume::new(volume_id, cell_size)
        };
        for record in checkpoint
            .bricks
            .iter()
            .filter(|record| record.volume_id == body.volume_id)
        {
            volume.insert_brick(
                BrickCoord::new(record.coord[0], record.coord[1], record.coord[2]),
                brick_from_stored(record)?,
            )?;
        }
        let owner = match body.kind {
            StoredBodyKind::Terrain => CanonicalOwner::Terrain,
            StoredBodyKind::Dynamic => CanonicalOwner::Body(
                EntityId::new(body.entity_id)
                    .map_err(|e| ResidencyError::Checkpoint(e.to_string()))?,
            ),
        };
        canonical.push(spall_sim::world::canonical_volume_for(&volume, owner));
    }
    Ok(canonical_topology_hash(&canonical).0)
}

/// Spatial references for whole dynamic bodies. Every intersected partition
/// points to the same `EntityId`; no partition owns or clips its voxel volume.
#[derive(Debug, Clone)]
pub struct BodySpatialIndex {
    partition_edge_m: f64,
    by_partition: BTreeMap<PartitionCoord, BTreeSet<EntityId>>,
    by_body: BTreeMap<EntityId, BTreeSet<PartitionCoord>>,
}

impl BodySpatialIndex {
    pub fn new(partition_edge_m: f64) -> Option<Self> {
        (partition_edge_m.is_finite() && partition_edge_m > 0.0).then_some(Self {
            partition_edge_m,
            by_partition: BTreeMap::new(),
            by_body: BTreeMap::new(),
        })
    }

    pub fn rebuild(&mut self, world: &SimWorld) {
        self.by_partition.clear();
        self.by_body.clear();
        for body in world.bodies() {
            let entity = body.entity.expect("dynamic body has an entity");
            let (lo, hi) = body.collider_region;
            let mut world_min = DVec3::splat(f64::INFINITY);
            let mut world_max = DVec3::splat(f64::NEG_INFINITY);
            for &x in &[lo.x as f64, hi.x.saturating_add(1) as f64] {
                for &y in &[lo.y as f64, hi.y.saturating_add(1) as f64] {
                    for &z in &[lo.z as f64, hi.z.saturating_add(1) as f64] {
                        let p = body
                            .pose
                            .local_cell_to_world_m(DVec3::new(x, y, z), body.cell_size());
                        world_min = world_min.min(p);
                        world_max = world_max.max(p);
                    }
                }
            }
            let min = partition_of(world_min, self.partition_edge_m);
            let max = partition_of(world_max, self.partition_edge_m);
            let mut refs = BTreeSet::new();
            for z in min.z..=max.z {
                for y in min.y..=max.y {
                    for x in min.x..=max.x {
                        let partition = PartitionCoord::new(x, y, z);
                        refs.insert(partition);
                        self.by_partition
                            .entry(partition)
                            .or_default()
                            .insert(entity);
                    }
                }
            }
            self.by_body.insert(entity, refs);
        }
    }

    pub fn bodies_in(&self, partition: PartitionCoord) -> impl Iterator<Item = EntityId> + '_ {
        self.by_partition
            .get(&partition)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    pub fn partitions_of(&self, body: EntityId) -> Option<&BTreeSet<PartitionCoord>> {
        self.by_body.get(&body)
    }
}

fn partition_of(p: DVec3, edge_m: f64) -> PartitionCoord {
    PartitionCoord::new(
        (p.x / edge_m).floor() as i64,
        (p.y / edge_m).floor() as i64,
        (p.z / edge_m).floor() as i64,
    )
}
