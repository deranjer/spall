//! The client-side authoritative replica (T10).
//!
//! [`ReplicaWorld`] holds the last *consistent* view of server topology as plain
//! [`spall_voxel`] volumes and applies committed
//! [`spall_protocol::TopologyTransaction`]s to it. It runs **no** server-only
//! structural decision: a split arrives fully described (`SplitOff` markers plus
//! canonical `CellRun`s from `spall_sim`), and is replayed op by op into a
//! *candidate* that only replaces the live replica once the whole transaction's
//! `before` revisions and `result_hashes` check out
//! (`docs/protocol.md` "Transaction application"):
//!
//! > Publish the candidate only when the whole transaction validates. Retain the
//! > previous consistent replica while dependencies or derived client collision
//! > data are pending. [...] A failed candidate cannot partly replace live
//! > state.
//!
//! A transaction whose `TransactionId` has already been committed is always a
//! no-op; the control-stream [`SequenceGate`] additionally tracks the high-water
//! mark for gap reporting but never drops an unseen id, so a catch-up / repair
//! delivery of an earlier transaction is still evaluated. An unexpected source
//! revision produces a [`spall_protocol::RepairRequest`], never a guessed
//! replay. Motion is handled by [`MotionTrack`]: at most one newest snapshot is
//! held for a body the replica has not created yet, and stale or
//! too-new-topology snapshots wait or are dropped.

use std::collections::{BTreeMap, BTreeSet};

use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Pose, Revision, TransactionId,
    VolumeId,
};
use spall_protocol::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, Hash32, MotionSnapshot,
    Record, RepairKey, RepairRequest, SequenceGate, TopologyOp, TopologyTransaction,
    canonical_topology_hash,
};
use spall_voxel::{Brick, BrickHash, EditPlan, Volume};

/// Stable numeric layer code for the material layer (mirrors
/// `spall_sim`'s canonical volume construction).
const MATERIAL_LAYER_KIND: u16 = 0;

/// Tunables for a [`ReplicaWorld`].
#[derive(Debug, Clone, Copy)]
pub struct ReplicaConfig {
    /// How long (in server ticks) a snapshot for a not-yet-created body is held
    /// before it is dropped as stale. `docs/protocol.md`: "Keep at most one
    /// newest pending snapshot per unknown body for a short bounded window".
    pub pending_snapshot_ticks: u64,
    /// Render lag applied by [`ReplicaWorld::interpolated_pose`], in seconds.
    pub interpolation_delay_s: f64,
    /// Maximum extrapolation past the newest snapshot, in seconds.
    pub max_extrapolation_s: f64,
    /// Fixed server tick rate, for tick↔time conversion.
    pub server_tick_hz: f64,
}

impl Default for ReplicaConfig {
    fn default() -> Self {
        Self {
            pending_snapshot_ticks: 30,
            interpolation_delay_s: 0.1,
            max_extrapolation_s: 0.1,
            server_tick_hz: 60.0,
        }
    }
}

/// Outcome of [`ReplicaWorld::apply_transaction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The candidate validated and is now the live replica.
    Published {
        transaction: TransactionId,
        /// Bodies this transaction created.
        new_bodies: Vec<EntityId>,
        /// Bodies this transaction emptied and retired.
        tombstoned: Vec<EntityId>,
    },
    /// Already applied (by id) or below the control high-water mark.
    Duplicate,
    /// A `before` revision did not match; nothing was applied. The caller should
    /// send these rate-limited and wait for repair, not replay.
    NeedsRepair(Vec<RepairRequest>),
    /// The candidate failed to build or its result hash did not match. The live
    /// replica is unchanged.
    Rejected { reason: String },
}

/// One observed motion state for a body.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MotionState {
    server_tick: u64,
    topology_revision: Revision,
    pose: Pose,
    linear_velocity: [f32; 3],
    angular_velocity: [f32; 3],
    sleeping: bool,
}

impl From<&MotionSnapshot> for MotionState {
    fn from(s: &MotionSnapshot) -> Self {
        Self {
            server_tick: s.server_tick.get(),
            topology_revision: s.topology_revision,
            pose: s.pose,
            linear_velocity: s.linear_velocity,
            angular_velocity: s.angular_velocity,
            sleeping: s.sleeping,
        }
    }
}

/// Interpolation history for one replicated body: the two most recent accepted
/// states, plus the first state ever accepted so a caller can measure how far
/// the body has actually travelled.
#[derive(Debug, Clone, Copy, Default)]
pub struct MotionTrack {
    first: Option<MotionState>,
    prev: Option<MotionState>,
    latest: Option<MotionState>,
}

impl MotionTrack {
    fn accept(&mut self, state: MotionState) {
        match self.latest {
            Some(cur) if state.server_tick <= cur.server_tick => {}
            _ => {
                if self.first.is_none() {
                    self.first = Some(state);
                }
                self.prev = self.latest;
                self.latest = Some(state);
            }
        }
    }

    /// The newest accepted server tick, if any.
    pub fn latest_tick(&self) -> Option<u64> {
        self.latest.map(|s| s.server_tick)
    }

    /// Straight-line distance (metres) between the first and newest accepted
    /// body positions. `0.0` until at least one state has been seen — a body
    /// that only ever reports one stationary pose therefore measures `0.0`.
    fn displacement_from_start_m(&self) -> f64 {
        match (self.first, self.latest) {
            (Some(a), Some(b)) => {
                let (p, q) = (a.pose.translation_m, b.pose.translation_m);
                let dx = q[0] - p[0];
                let dy = q[1] - p[1];
                let dz = q[2] - p[2];
                (dx * dx + dy * dy + dz * dz).sqrt()
            }
            _ => 0.0,
        }
    }
}

/// A replicated body: its volume, its stable id, and its motion history.
#[derive(Debug)]
struct ReplicaBody {
    entity: EntityId,
    volume_id: VolumeId,
    track: MotionTrack,
}

/// The client's consistent replica of server topology.
pub struct ReplicaWorld {
    config: ReplicaConfig,
    terrain_id: VolumeId,
    /// Live, consistent volumes: terrain plus every replicated body volume.
    volumes: BTreeMap<u64, Volume>,
    owner: BTreeMap<u64, CanonicalOwner>,
    bodies: BTreeMap<u64, ReplicaBody>,
    volume_of_entity: BTreeMap<u64, u64>,
    /// Retired body ids; a late packet can never resurrect one.
    tombstoned: BTreeSet<u64>,
    applied_tx: BTreeSet<u64>,
    control_gate: SequenceGate,
    /// Newest held snapshot for a body that does not exist yet: entity → (snap,
    /// server tick it was received at).
    pending_snapshots: BTreeMap<u64, (MotionSnapshot, u64)>,
    /// Highest server tick the client has observed on any record.
    now_tick: u64,
}

impl ReplicaWorld {
    /// Installs a fixed-scene baseline: `terrain` is the world grid. Bodies
    /// present before play are added with [`Self::install_body`].
    /// `docs/tasks.md` T10: "Initial fixed-scene baseline can be installed
    /// before play".
    pub fn from_baseline(terrain: Volume, config: ReplicaConfig) -> Self {
        let terrain_id = terrain.id();
        let mut volumes = BTreeMap::new();
        let mut owner = BTreeMap::new();
        owner.insert(terrain_id.get(), CanonicalOwner::Terrain);
        volumes.insert(terrain_id.get(), terrain);
        Self {
            config,
            terrain_id,
            volumes,
            owner,
            bodies: BTreeMap::new(),
            volume_of_entity: BTreeMap::new(),
            tombstoned: BTreeSet::new(),
            applied_tx: BTreeSet::new(),
            control_gate: SequenceGate::new(),
            pending_snapshots: BTreeMap::new(),
            now_tick: 0,
        }
    }

    /// An empty replica with no terrain yet — [`Self::install_baseline_world`]
    /// must run before any query. Used by the late-join client, which receives
    /// its whole world over a bulk transfer instead of a fixed scene.
    pub fn empty(config: ReplicaConfig) -> Self {
        let placeholder = VolumeId::new(1).expect("1 is a valid volume id");
        Self {
            config,
            terrain_id: placeholder,
            volumes: BTreeMap::new(),
            owner: BTreeMap::new(),
            bodies: BTreeMap::new(),
            volume_of_entity: BTreeMap::new(),
            tombstoned: BTreeSet::new(),
            applied_tx: BTreeSet::new(),
            control_gate: SequenceGate::new(),
            pending_snapshots: BTreeMap::new(),
            now_tick: 0,
        }
    }

    /// Builds a replica directly from a decoded late-join baseline.
    pub fn from_baseline_world(
        world: &spall_protocol::BaselineWorld,
        config: ReplicaConfig,
    ) -> Result<Self, String> {
        let mut replica = Self::empty(config);
        replica.install_baseline_world(world)?;
        Ok(replica)
    }

    /// Installs a full late-join baseline atomically (`docs/protocol.md` "Late
    /// join" step 3: "Client validates and installs the baseline atomically").
    /// Every prior volume, body, tombstone, and applied-transaction record is
    /// dropped and replaced by `world`'s geometry — every brick at its
    /// authoritative revision, so [`Self::world_hash`] equals the server's
    /// `world_hash()` at the snapshot tick with no follow-up repair. `now_tick`
    /// advances to the snapshot tick so the catch-up barrier and the
    /// pending-snapshot window are anchored correctly.
    pub fn install_baseline_world(
        &mut self,
        world: &spall_protocol::BaselineWorld,
    ) -> Result<(), String> {
        use spall_protocol::{BaselineCells, BaselineOwner};

        world.validate().map_err(|e| e.to_string())?;

        let mut volumes = BTreeMap::new();
        let mut owner = BTreeMap::new();
        let mut bodies = BTreeMap::new();
        let mut volume_of_entity = BTreeMap::new();
        let mut terrain_id = None;

        for bv in &world.volumes {
            let vid = bv.volume_id;
            let cs = CellSizeCode::from_u8(bv.cell_size_code).ok_or_else(|| {
                format!(
                    "baseline volume {vid} has unknown cell-size code {}",
                    bv.cell_size_code
                )
            })?;
            let mut volume = match bv.bounds {
                Some([mn, mx]) => {
                    let bb = spall_voxel::BrickBounds::new(
                        BrickCoord::new(mn[0], mn[1], mn[2]),
                        BrickCoord::new(mx[0], mx[1], mx[2]),
                    )
                    .ok_or_else(|| format!("baseline volume {vid} has inverted bounds"))?;
                    Volume::bounded(vid, cs, bb)
                }
                None => Volume::new(vid, cs),
            };
            for bb in &bv.bricks {
                let cells: Vec<MaterialId> = match &bb.cells {
                    BaselineCells::Uniform(id) => {
                        vec![MaterialId(*id); spall_core::CELLS_PER_BRICK]
                    }
                    BaselineCells::Dense(raw) => {
                        if raw.len() != spall_core::CELLS_PER_BRICK {
                            return Err(format!(
                                "baseline brick in {vid} has {} cells, expected {}",
                                raw.len(),
                                spall_core::CELLS_PER_BRICK
                            ));
                        }
                        raw.iter().copied().map(MaterialId).collect()
                    }
                };
                let brick = Brick::restored(&cells, Revision(bb.revision), bb.edited);
                volume
                    .insert_brick(
                        BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                        brick,
                    )
                    .map_err(|e| format!("baseline brick insert into {vid} failed: {e}"))?;
            }
            match bv.owner {
                BaselineOwner::Terrain => {
                    owner.insert(vid.get(), CanonicalOwner::Terrain);
                    terrain_id = Some(vid);
                }
                BaselineOwner::Body(entity) => {
                    owner.insert(vid.get(), CanonicalOwner::Body(entity));
                    volume_of_entity.insert(entity.get(), vid.get());
                    bodies.insert(
                        entity.get(),
                        ReplicaBody {
                            entity,
                            volume_id: vid,
                            track: MotionTrack::default(),
                        },
                    );
                }
            }
            volumes.insert(vid.get(), volume);
        }
        let terrain_id = terrain_id.ok_or("baseline has no terrain volume")?;

        self.terrain_id = terrain_id;
        self.volumes = volumes;
        self.owner = owner;
        self.bodies = bodies;
        self.volume_of_entity = volume_of_entity;
        self.tombstoned = BTreeSet::new();
        self.applied_tx = BTreeSet::new();
        self.control_gate = SequenceGate::new();
        self.pending_snapshots = BTreeMap::new();
        self.now_tick = world.checkpoint_tick;
        Ok(())
    }

    /// Merges a targeted baseline patch into the live replica — a hash repair
    /// (`docs/protocol.md`: "hash repairs"). Each named brick of each named
    /// volume is overwritten at its **authoritative revision**, so a brick that
    /// diverged client-side (or a `RepairRequest` answer) is restored to exact
    /// parity, revision included — something a `CellRun` replay cannot do
    /// because it would re-stamp the replica's own next revision.
    ///
    /// The patch may only touch volumes the replica already holds; re-adding a
    /// missing body is a full re-baseline, not a patch.
    pub fn apply_baseline_patch(
        &mut self,
        world: &spall_protocol::BaselineWorld,
    ) -> Result<(), String> {
        use spall_protocol::BaselineCells;

        world.validate().map_err(|e| e.to_string())?;
        for bv in &world.volumes {
            let vid = bv.volume_id;
            let volume = self.volumes.get_mut(&vid.get()).ok_or_else(|| {
                format!("repair patch names volume {vid} the replica does not hold")
            })?;
            for bb in &bv.bricks {
                let cells: Vec<MaterialId> = match &bb.cells {
                    BaselineCells::Uniform(id) => {
                        vec![MaterialId(*id); spall_core::CELLS_PER_BRICK]
                    }
                    BaselineCells::Dense(raw) => {
                        if raw.len() != spall_core::CELLS_PER_BRICK {
                            return Err(format!("repair brick has {} cells", raw.len()));
                        }
                        raw.iter().copied().map(MaterialId).collect()
                    }
                };
                let brick = Brick::restored(&cells, Revision(bb.revision), bb.edited);
                volume
                    .insert_brick(
                        BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                        brick,
                    )
                    .map_err(|e| format!("repair brick insert into {vid} failed: {e}"))?;
            }
        }
        Ok(())
    }

    /// Adds a body that exists in the baseline scene.
    pub fn install_body(&mut self, entity: EntityId, volume: Volume) {
        let vid = volume.id();
        self.owner.insert(vid.get(), CanonicalOwner::Body(entity));
        self.volumes.insert(vid.get(), volume);
        self.volume_of_entity.insert(entity.get(), vid.get());
        self.bodies.insert(
            entity.get(),
            ReplicaBody {
                entity,
                volume_id: vid,
                track: MotionTrack::default(),
            },
        );
    }

    /// The canonical topology hash of the whole replica — the value that must
    /// equal the server's `world_hash()` at quiescence.
    pub fn world_hash(&self) -> Hash32 {
        let volumes: Vec<CanonicalVolume> = self
            .volumes
            .values()
            .map(|v| canonical_volume(v, self.owner[&v.id().get()]))
            .collect();
        canonical_topology_hash(&volumes)
    }

    /// The canonical hash of one volume.
    pub fn volume_hash(&self, volume: VolumeId) -> Option<Hash32> {
        let v = self.volumes.get(&volume.get())?;
        Some(canonical_topology_hash(&[canonical_volume(
            v,
            self.owner[&volume.get()],
        )]))
    }

    /// Whether a transaction id has already been applied.
    pub fn has_applied(&self, transaction: TransactionId) -> bool {
        self.applied_tx.contains(&transaction.get())
    }

    /// Entity ids of every live replicated body.
    pub fn body_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.bodies.values().map(|b| b.entity)
    }

    /// Whether `entity` has been retired.
    pub fn is_tombstoned(&self, entity: EntityId) -> bool {
        self.tombstoned.contains(&entity.get())
    }

    /// Current solid-cell total across the replica (conservation check helper).
    pub fn total_solid_cells(&self) -> u64 {
        self.volumes.values().map(solid_cells).sum()
    }

    /// Solid cells in the volume owned by `entity`, or `None` if that body is
    /// not resident. Used to confirm a body-targeted cut actually removed
    /// material from the body it claimed.
    pub fn body_solid_cells(&self, entity: EntityId) -> Option<u64> {
        let vol = self.volume_of_entity.get(&entity.get())?;
        self.volumes.get(vol).map(solid_cells)
    }

    /// Largest straight-line distance (metres) any replicated body has moved
    /// from its first observed position to its most recent one. `0.0` when no
    /// body has produced two distinct poses — a body that only ever reports a
    /// stationary snapshot does not count as motion.
    pub fn max_body_displacement_m(&self) -> f64 {
        self.bodies
            .values()
            .map(|b| b.track.displacement_from_start_m())
            .fold(0.0_f64, f64::max)
    }

    // --- transaction application --------------------------------------------

    /// Applies one committed transaction. See [`ApplyOutcome`].
    pub fn apply_transaction(&mut self, tx: &TopologyTransaction) -> ApplyOutcome {
        self.now_tick = self.now_tick.max(tx.server_tick.get());

        // `applied_tx` is the authoritative replay guard: a transaction id we
        // have already committed is always a no-op. The control-stream gate only
        // tracks the high-water mark for gap reporting; a low sequence with an
        // id we have *not* applied is an out-of-order / repair delivery and must
        // still be evaluated, not dropped.
        if self.applied_tx.contains(&tx.transaction_id.get()) {
            return ApplyOutcome::Duplicate;
        }
        let _ = self.control_gate.observe(tx.control_seq.0);
        if let Err(e) = tx.validate() {
            return ApplyOutcome::Rejected {
                reason: format!("invalid transaction: {e}"),
            };
        }

        // 1. Every `before` revision must match the live replica exactly. A
        //    `before` entry of `Revision::ZERO` means the brick was absent
        //    server-side, so an absent replica brick is a match.
        let mut repairs = Vec::new();
        for br in &tx.before {
            let current = self
                .volumes
                .get(&br.volume.get())
                .and_then(|v| v.brick_revision(br.coord).ok().flatten());
            let matches = match (current, br.revision) {
                (None, Revision::ZERO) => true,
                (Some(have), want) => have == want,
                _ => false,
            };
            if !matches {
                repairs.push(RepairRequest {
                    key: RepairKey::Brick {
                        volume: br.volume,
                        coord: br.coord,
                    },
                    expected_revision: br.revision,
                    current_revision: current.unwrap_or(Revision::ZERO),
                    expected_hash: Hash32::ZERO,
                    current_hash: self.volume_hash(br.volume).unwrap_or(Hash32::ZERO),
                });
            }
        }
        if !repairs.is_empty() {
            return ApplyOutcome::NeedsRepair(repairs);
        }

        // 2. Replay every op into an isolated candidate.
        let cell_size = match self.volumes.get(&self.terrain_id.get()) {
            Some(v) => v.cell_size(),
            None => {
                return ApplyOutcome::Rejected {
                    reason: "replica has no terrain volume".into(),
                };
            }
        };
        let mut candidate: BTreeMap<u64, Volume> = self.volumes.clone();
        let mut new_owner: Vec<(VolumeId, EntityId)> = Vec::new();
        if let Err(reason) = replay_ops(&tx.ops, &mut candidate, &mut new_owner, cell_size) {
            return ApplyOutcome::Rejected { reason };
        }

        // 3. Every declared result hash must match the candidate.
        for vh in &tx.result_hashes {
            let owner = self.owner.get(&vh.volume.get()).copied().or_else(|| {
                new_owner
                    .iter()
                    .find(|(v, _)| *v == vh.volume)
                    .map(|(_, e)| CanonicalOwner::Body(*e))
            });
            let Some(owner) = owner else {
                return ApplyOutcome::Rejected {
                    reason: format!("result hash names unknown volume {}", vh.volume),
                };
            };
            let Some(v) = candidate.get(&vh.volume.get()) else {
                return ApplyOutcome::Rejected {
                    reason: format!("result hash names missing volume {}", vh.volume),
                };
            };
            if canonical_topology_hash(&[canonical_volume(v, owner)]) != vh.hash {
                return ApplyOutcome::Rejected {
                    reason: format!("result hash mismatch for volume {}", vh.volume),
                };
            }
        }

        // 4. Commit the candidate. Nothing above mutated live state.
        self.volumes = candidate;
        for (vid, entity) in &new_owner {
            self.owner.insert(vid.get(), CanonicalOwner::Body(*entity));
            self.volume_of_entity.insert(entity.get(), vid.get());
            self.bodies.insert(
                entity.get(),
                ReplicaBody {
                    entity: *entity,
                    volume_id: *vid,
                    track: MotionTrack::default(),
                },
            );
        }
        self.applied_tx.insert(tx.transaction_id.get());

        // 5. Retire any body (including the source) that this left with no
        //    solid cell — a late motion packet can never bring it back.
        let mut tombstoned = Vec::new();
        let empty: Vec<u64> = self
            .bodies
            .values()
            .filter(|b| {
                self.volumes
                    .get(&b.volume_id.get())
                    .map(|v| solid_cells(v) == 0)
                    .unwrap_or(true)
            })
            .map(|b| b.entity.get())
            .collect();
        for id in empty {
            if let Some(body) = self.bodies.remove(&id) {
                self.volumes.remove(&body.volume_id.get());
                self.owner.remove(&body.volume_id.get());
                self.volume_of_entity.remove(&id);
                self.tombstoned.insert(id);
                self.pending_snapshots.remove(&id);
                tombstoned.push(body.entity);
            }
        }

        // 6. A held snapshot for a body we just created can now be applied.
        let new_bodies: Vec<EntityId> = new_owner.iter().map(|(_, e)| *e).collect();
        for entity in &new_bodies {
            if let Some((snap, received)) = self.pending_snapshots.remove(&entity.get())
                && self.now_tick.saturating_sub(received) <= self.config.pending_snapshot_ticks
            {
                self.ingest_snapshot(&snap);
            }
        }
        self.prune_pending();

        ApplyOutcome::Published {
            transaction: tx.transaction_id,
            new_bodies,
            tombstoned,
        }
    }

    // --- motion -----------------------------------------------------------

    /// Ingests one motion snapshot. Returns `true` if it was accepted into a
    /// body's history, `false` if it was ignored, held pending, or dropped.
    pub fn ingest_snapshot(&mut self, snap: &MotionSnapshot) -> bool {
        self.now_tick = self.now_tick.max(snap.server_tick.get());
        let id = snap.body.get();
        if self.tombstoned.contains(&id) {
            return false;
        }
        let Some(&vol_key) = self.volume_of_entity.get(&id) else {
            // Unknown body: hold only the newest, bounded.
            let keep = self
                .pending_snapshots
                .get(&id)
                .map(|(existing, _)| snap.server_tick.get() >= existing.server_tick.get())
                .unwrap_or(true);
            if keep {
                self.pending_snapshots.insert(id, (*snap, self.now_tick));
            }
            return false;
        };

        // Snapshot that refers to topology newer than we have applied: wait.
        let replica_rev = self
            .volumes
            .get(&vol_key)
            .map(latest_revision)
            .unwrap_or(Revision::ZERO);
        if snap.topology_revision > replica_rev {
            self.pending_snapshots.insert(id, (*snap, self.now_tick));
            return false;
        }

        if let Some(body) = self.bodies.get_mut(&id) {
            body.track.accept(MotionState::from(snap));
            true
        } else {
            false
        }
    }

    /// Interpolated pose for `entity` at `render_tick` (fractional server
    /// ticks). Interpolates between the two newest states about
    /// `interpolation_delay_s` behind the newest, extrapolates up to
    /// `max_extrapolation_s`, then holds. `None` if the body has no state yet.
    pub fn interpolated_pose(&self, entity: EntityId, render_tick: f64) -> Option<Pose> {
        let track = &self.bodies.get(&entity.get())?.track;
        let latest = track.latest?;
        let delay_ticks = self.config.interpolation_delay_s * self.config.server_tick_hz;
        let target = render_tick - delay_ticks;

        let Some(prev) = track.prev else {
            return Some(latest.pose);
        };
        let (a, b) = if prev.server_tick <= latest.server_tick {
            (prev, latest)
        } else {
            (latest, prev)
        };
        if b.server_tick == a.server_tick {
            return Some(b.pose);
        }
        let span = (b.server_tick - a.server_tick) as f64;
        let max_extra = self.config.max_extrapolation_s * self.config.server_tick_hz;
        let t = ((target - a.server_tick as f64) / span).clamp(0.0, 1.0 + max_extra / span);
        Some(lerp_pose(&a.pose, &b.pose, t))
    }

    /// The newest raw motion state's server tick for `entity`.
    pub fn latest_motion_tick(&self, entity: EntityId) -> Option<u64> {
        self.bodies.get(&entity.get())?.track.latest_tick()
    }

    fn prune_pending(&mut self) {
        let now = self.now_tick;
        let window = self.config.pending_snapshot_ticks;
        self.pending_snapshots
            .retain(|_, (_, received)| now.saturating_sub(*received) <= window);
    }
}

// --- op replay --------------------------------------------------------------

/// A batch of same-volume writes not yet applied.
struct PendingGroup {
    volume: VolumeId,
    /// `Some(entity)` if this group builds a brand-new child volume.
    new_child: Option<EntityId>,
    writes: Vec<(GlobalCell, MaterialId)>,
}

/// Replays `ops` into `candidate`. Consecutive same-volume writes are batched
/// into a single `apply_edit`, and a `SplitOff` group is built exactly like
/// `spall_sim::transfer::build_child_volume` (bounded to the fill bricks, every
/// brick in that box resident at revision 1) so brick revisions — and therefore
/// the canonical hash — match the server.
fn replay_ops(
    ops: &[TopologyOp],
    candidate: &mut BTreeMap<u64, Volume>,
    new_owner: &mut Vec<(VolumeId, EntityId)>,
    cell_size: CellSizeCode,
) -> Result<(), String> {
    let mut pending: Option<PendingGroup> = None;

    for op in ops {
        match op {
            TopologyOp::IntegerBrush {
                volume,
                brush,
                material,
            } => {
                flush(pending.take(), candidate, cell_size)?;
                let plan = EditPlan::sphere(*volume, *brush, *material);
                candidate
                    .get_mut(&volume.get())
                    .ok_or_else(|| format!("brush targets unknown volume {volume}"))?
                    .apply_edit(&plan)
                    .map_err(|e| format!("brush replay failed: {e}"))?;
            }
            TopologyOp::SplitOff {
                child,
                child_entity,
                ..
            } => {
                flush(pending.take(), candidate, cell_size)?;
                new_owner.push((*child, *child_entity));
                pending = Some(PendingGroup {
                    volume: *child,
                    new_child: Some(*child_entity),
                    writes: Vec::new(),
                });
            }
            TopologyOp::CellRun {
                volume,
                start,
                len,
                material,
            } => {
                if pending.as_ref().map(|g| g.volume) != Some(*volume) {
                    flush(pending.take(), candidate, cell_size)?;
                    pending = Some(PendingGroup {
                        volume: *volume,
                        new_child: None,
                        writes: Vec::new(),
                    });
                }
                let group = pending.as_mut().expect("just set");
                let last_x = start
                    .x
                    .checked_add(i64::from(*len).saturating_sub(1))
                    .ok_or("cell run overflows i64")?;
                for x in start.x..=last_x {
                    group
                        .writes
                        .push((GlobalCell::new(x, start.y, start.z), *material));
                }
            }
        }
    }
    flush(pending.take(), candidate, cell_size)
}

fn flush(
    group: Option<PendingGroup>,
    candidate: &mut BTreeMap<u64, Volume>,
    cell_size: CellSizeCode,
) -> Result<(), String> {
    let Some(group) = group else { return Ok(()) };
    if group.writes.is_empty() {
        return if group.new_child.is_some() {
            Err("split-off child received no cells".into())
        } else {
            Ok(())
        };
    }

    if group.new_child.is_some() {
        // Build the child volume the way the server does.
        let mut min = BrickCoord::new(i64::MAX, i64::MAX, i64::MAX);
        let mut max = BrickCoord::new(i64::MIN, i64::MIN, i64::MIN);
        for (cell, _) in &group.writes {
            let b = cell.split().0;
            min = BrickCoord::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
            max = BrickCoord::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
        }
        let bounds =
            spall_voxel::BrickBounds::new(min, max).ok_or("child brick bounds are inverted")?;
        let mut child = Volume::bounded(group.volume, cell_size, bounds);
        for bz in min.z..=max.z {
            for by in min.y..=max.y {
                for bx in min.x..=max.x {
                    child
                        .insert_brick(
                            BrickCoord::new(bx, by, bz),
                            Brick::uniform(MaterialId::AIR, Revision(1)),
                        )
                        .map_err(|e| format!("child brick insert failed: {e}"))?;
                }
            }
        }
        apply_writes(&mut child, group.volume, &group.writes)?;
        candidate.insert(group.volume.get(), child);
    } else {
        let volume = candidate
            .get_mut(&group.volume.get())
            .ok_or_else(|| format!("cell run targets unknown volume {}", group.volume))?;
        apply_writes(volume, group.volume, &group.writes)?;
    }
    Ok(())
}

fn apply_writes(
    volume: &mut Volume,
    vid: VolumeId,
    writes: &[(GlobalCell, MaterialId)],
) -> Result<(), String> {
    let mut plan = EditPlan::new(vid);
    for (cell, material) in writes {
        plan.set(*cell, *material);
    }
    volume
        .apply_edit(&plan)
        .map(|_| ())
        .map_err(|e| format!("cell run replay failed: {e}"))
}

// --- canonical form (mirrors spall_sim::world::SimWorld::canonical_volume) ---

fn canonical_volume(v: &Volume, owner: CanonicalOwner) -> CanonicalVolume {
    let mut bricks = Vec::new();
    for coord in v.resident_brick_coords() {
        let snap = v
            .snapshot_brick(coord)
            .ok()
            .flatten()
            .expect("coord came from the resident set");
        bricks.push(CanonicalBrick {
            coord,
            revision: snap.revision(),
            layers: vec![CanonicalLayer {
                kind: MATERIAL_LAYER_KIND,
                bytes: BrickHash::to_bytes(snap.content_hash()).to_vec(),
            }],
        });
    }
    CanonicalVolume {
        volume_id: v.id(),
        cell_size: v.cell_size(),
        owner,
        bricks,
    }
}

fn solid_cells(v: &Volume) -> u64 {
    let mut n = 0;
    for coord in v.resident_brick_coords() {
        let Some(snap) = v.snapshot_brick(coord).ok().flatten() else {
            continue;
        };
        for i in 0..spall_core::CELLS_PER_BRICK as u16 {
            let lc = spall_core::LocalCell::from_linear_index(i).expect("index < 32768");
            if !snap.get(lc).is_air() {
                n += 1;
            }
        }
    }
    n
}

fn latest_revision(v: &Volume) -> Revision {
    Revision(v.next_revision().get().saturating_sub(1))
}

fn lerp_pose(a: &Pose, b: &Pose, t: f64) -> Pose {
    let mut out = *b;
    for i in 0..3 {
        out.translation_m[i] = a.translation_m[i] + (b.translation_m[i] - a.translation_m[i]) * t;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::CellSizeCode;
    use spall_voxel::EditPlan;

    fn terrain() -> Volume {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(9, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
        v
    }

    #[test]
    fn a_before_revision_mismatch_asks_for_repair_without_touching_state() {
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        let before_hash = replica.world_hash();

        let tx = TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![spall_protocol::BrickRevision {
                volume: VolumeId::new(1).unwrap(),
                coord: BrickCoord::new(0, 0, 0),
                revision: Revision(999), // we actually hold revision 1
            }],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 1,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![],
        };
        match replica.apply_transaction(&tx) {
            ApplyOutcome::NeedsRepair(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert!(matches!(reqs[0].key, RepairKey::Brick { .. }));
            }
            other => panic!("expected NeedsRepair, got {other:?}"),
        }
        assert_eq!(replica.world_hash(), before_hash, "state untouched");
    }

    #[test]
    fn a_duplicate_transaction_id_is_a_no_op() {
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        let tx = TopologyTransaction {
            transaction_id: TransactionId::new(7).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 2,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![],
        };
        assert!(matches!(
            replica.apply_transaction(&tx),
            ApplyOutcome::Published { .. }
        ));
        let after_first = replica.world_hash();
        // Same id, fresh control_seq: still a duplicate by id.
        let mut again = tx.clone();
        again.control_seq = spall_protocol::ControlSeq(2);
        assert_eq!(replica.apply_transaction(&again), ApplyOutcome::Duplicate);
        assert_eq!(replica.world_hash(), after_first);
    }

    #[test]
    fn a_result_hash_mismatch_rejects_the_whole_transaction() {
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        let before = replica.world_hash();
        let tx = TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 3,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![spall_protocol::VolumeHash {
                volume: VolumeId::new(1).unwrap(),
                hash: Hash32([9u8; 32]),
            }],
        };
        match replica.apply_transaction(&tx) {
            ApplyOutcome::Rejected { .. } => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(
            replica.world_hash(),
            before,
            "failed candidate did not leak"
        );
    }

    #[test]
    fn a_snapshot_for_an_unknown_body_is_held_then_applied() {
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        let ghost = EntityId::new(42).unwrap();
        let snap = MotionSnapshot {
            server_tick: spall_core::Tick(5),
            snapshot_seq: spall_protocol::SnapshotSeq(1),
            acked_input: spall_protocol::InputSeq(0),
            body: ghost,
            topology_revision: Revision(0),
            pose: Pose {
                translation_m: [1.0, 2.0, 3.0],
                rotation: spall_core::QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
            },
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: false,
        };
        assert!(!replica.ingest_snapshot(&snap), "held, not applied");
        assert!(replica.interpolated_pose(ghost, 5.0).is_none());

        // The body is created by installing it (stands in for a SplitOff).
        replica.install_body(ghost, {
            let mut v = Volume::new(VolumeId::new(9).unwrap(), CellSizeCode::Quarter);
            v.apply_edit(&EditPlan::filled_box(
                VolumeId::new(9).unwrap(),
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
            v
        });
        // Re-feeding the held snapshot now lands.
        assert!(replica.ingest_snapshot(&snap));
        let pose = replica.interpolated_pose(ghost, 5.0).unwrap();
        assert_eq!(pose.translation_m, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn install_baseline_world_replaces_prior_state_and_leaves_transactions_appliable() {
        use spall_protocol::{
            BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume, BaselineWorld,
        };

        // Start from a fixed scene, then install a completely different world.
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        replica.apply_transaction(&TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: spall_core::Tick(1),
            control_seq: spall_protocol::ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 1,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![],
        });

        let world = BaselineWorld {
            schema: spall_protocol::BASELINE_WORLD_SCHEMA,
            checkpoint_tick: 500,
            volumes: vec![BaselineVolume {
                volume_id: VolumeId::new(7).unwrap(),
                cell_size_code: CellSizeCode::Quarter.to_u8(),
                owner: BaselineOwner::Terrain,
                bounds: None,
                bricks: vec![BaselineBrick {
                    coord: [0, 0, 0],
                    revision: 12,
                    edited: false,
                    cells: BaselineCells::Uniform(MaterialId(1).0),
                }],
            }],
        };
        replica.install_baseline_world(&world).unwrap();

        assert_eq!(replica.terrain_id, VolumeId::new(7).unwrap());
        assert_eq!(replica.body_ids().count(), 0);
        assert!(!replica.has_applied(TransactionId::new(1).unwrap()));
        assert_eq!(replica.now_tick, 500);
        // Reinstalling the same baseline is idempotent.
        let hash = replica.world_hash();
        replica.install_baseline_world(&world).unwrap();
        assert_eq!(replica.world_hash(), hash);

        // A fresh transaction against the installed terrain still applies.
        let tx = TopologyTransaction {
            transaction_id: TransactionId::new(2).unwrap(),
            server_tick: spall_core::Tick(501),
            control_seq: spall_protocol::ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![spall_protocol::BrickRevision {
                volume: VolumeId::new(7).unwrap(),
                coord: BrickCoord::new(0, 0, 0),
                revision: Revision(12),
            }],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(7).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 4,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![],
        };
        assert!(matches!(
            replica.apply_transaction(&tx),
            ApplyOutcome::Published { .. }
        ));
    }

    #[test]
    fn a_snapshot_for_a_tombstoned_body_is_ignored() {
        let mut replica = ReplicaWorld::from_baseline(terrain(), ReplicaConfig::default());
        replica.tombstoned.insert(3);
        let snap = MotionSnapshot {
            server_tick: spall_core::Tick(5),
            snapshot_seq: spall_protocol::SnapshotSeq(1),
            acked_input: spall_protocol::InputSeq(0),
            body: EntityId::new(3).unwrap(),
            topology_revision: Revision(0),
            pose: Pose {
                translation_m: [0.0; 3],
                rotation: spall_core::QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
            },
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: false,
        };
        assert!(!replica.ingest_snapshot(&snap));
        assert!(replica.pending_snapshots.is_empty());
    }
}
