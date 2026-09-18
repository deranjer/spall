//! T23 / G3 row 7, slice D — the default-off serve-loop residency pass.
//!
//! Per tick, after the sim step and commits: the interest set is the union of a
//! brick box around every player capsule. Terrain bricks inside it are kept
//! resident (reloaded from the durable [`MemoryBacking`] if they had been
//! evicted); terrain bricks outside it are evicted, retaining a
//! `(revision, content_hash)` digest so `world_hash`, conservation, and
//! `result_hashes` are unchanged (slices A–C). An edit that later needs an
//! evicted brick's cells reloads it through the same backing (slice C).
//!
//! Body volumes are never evicted. This pass owns the backing (default
//! in-process `MemoryBacking`, or a real disk-backed store -- see
//! [`spall_sim::BrickBackingWriter`] and `crate::disk_backing`) and hands the
//! same handle to [`SimWorld::set_backing`], so its `on_commit` updates and
//! the pipeline's reloads see one store.
//!
//! ## ENG-30 row 7 increment 13 — pin lifecycle + admission
//!
//! `docs/reports/ENG-30-row7-remaining.md` (coordinator review, 2026-09-18)
//! found two gaps in the increment-12 state of this pass: the interest set
//! reflected only player proximity, with no explicit reservation for a
//! pending edit's dependencies, a moving player/body's swept-collision
//! footprint, or a brick the edit pipeline had *just* reactively reloaded —
//! so a brick could be evicted again immediately after that reload, before
//! the retry that needed it actually ran; and `budget_bricks` was a reported
//! ceiling only, with no dense-byte ceiling at all. Both are addressed here:
//!
//! - [`ResidencyPass::run`] takes an explicit `pending_edit_bricks` set (see
//!   [`spall_sim::Simulation::pending_edit_bricks`]) and internally tracks two
//!   more pin sources: a short reload-grace window seeded by
//!   [`ResidencyPass::note_pipeline_reloads`] (fed from
//!   `TickReport::reloaded_bricks`), and the swept path of every player and
//!   every active (non-sleeping, non-dormant) body between the previous and
//!   current tick. The union is **never** evicted, by hysteresis or by
//!   pressure, and is always attempted for reload even when a brick-count or
//!   dense-byte cap would otherwise defer it — dropping a truly required
//!   dependency (rather than deferring the *optional* work around it) is
//!   exactly the "sampled evicted geometry as air" failure this contract
//!   forbids.
//! - [`ResidencyLimits::max_dense_bytes`] is a real second ceiling. Both caps
//!   are now enforced on the **admission** (reload) path, not just reported:
//!   an interest-driven reload that is not pinned/required is deferred (left
//!   evicted) rather than admitted when it would push resident bricks or
//!   dense bytes over either cap (using a conservative per-brick estimate —
//!   every candidate is costed as if dense,
//!   `spall_voxel::MemoryReport::DENSE_BRICK_BYTES` — so the enforced cap can
//!   only be tighter than the true measured footprint, never looser). The
//!   ordinary `EVICT_SETTLE_TICKS` hysteresis is unchanged and keeps reclaiming
//!   every unpinned, out-of-interest resident brick on the same schedule as
//!   before; [`PassTick::required_over_budget`] reports, without evicting
//!   anything required to force a fit, when even that is not enough because
//!   the required (interest ∪ pinned) set alone exceeds `budget_bricks`.
//!
//! ## ENG-30 row 7 increment 15 — incremental checkpoint capture
//!
//! Increment 22 stopped `capture_checkpoint` from `reload_all`-ing every
//! evicted brick back into the live world before every periodic/shutdown
//! checkpoint, but the capture itself still re-snapshotted, re-loaded, and
//! re-encoded **every** logical (resident ∪ evicted) terrain brick on every
//! call — a full logical-world walk, just one that no longer touched live
//! residency placement to do it. [`ResidencyPass::capture_checkpoint`] now
//! keeps a per-brick cache of the last checkpoint that actually captured each
//! coordinate ([`ResidencyPass::checkpoint_cache`]); a brick whose revision
//! has not changed since reuses that prior record untouched, so only bricks
//! dirtied (edited, or evicted/reloaded at a new revision) since the last
//! successful checkpoint do real work. Every checkpoint's `bricks` list is
//! still the *complete* logical set — checkpoints are pruned
//! (`RETAIN_CHECKPOINTS` in `serve.rs`) and each one must stand alone for a
//! cold recovery — only how that list is *assembled* changed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use spall_core::{BrickCoord, CELLS_PER_BRICK, GlobalCell, MaterialId, Revision, VolumeId};
use spall_sim::{
    BackingBrick, BrickBacking, BrickBackingWriter, MemoryBacking, SimWorld, Simulation,
};
use spall_store::{Checkpoint, StoredBrick};
use spall_voxel::{Brick, MemoryReport};

use crate::persist::{
    PersistConfig, PersistError, capture_with_terrain_bricks, stored_brick_from_backing,
    stored_brick_from_snapshot,
};

/// Consecutive ticks a terrain brick must be resident *and* outside every
/// player's interest before the pass evicts it. The hysteresis stops the pass
/// from fighting the edit pipeline, which reloads a brick a queued edit needs
/// (`EvictedGeometryRequired`) and needs a few ticks to re-stage and commit it.
const EVICT_SETTLE_TICKS: u32 = 4;

/// Ticks a brick the *edit pipeline itself* reloaded (not this pass) stays
/// pinned afterward. A re-queued intent's retry runs on the next tick's
/// `run_tick` (`docs/architecture.md`'s conflict/retry ordering), so `2`
/// covers the tick of the reload plus one full retry cycle without leaning on
/// the ordinary interest/hysteresis mechanism to happen to agree.
const PIN_GRACE_TICKS: u32 = 2;

/// Residency limits for a [`serve`](crate::serve) run.
#[derive(Debug, Clone, Copy)]
pub struct ResidencyLimits {
    /// Hard ceiling on resident terrain bricks. Interest-driven reloads that
    /// are not pinned/required are deferred once admitting them would exceed
    /// this; a pinned/required reload always proceeds, and
    /// [`PassTick::required_over_budget`] reports when that alone is not
    /// enough to stay within budget.
    pub budget_bricks: usize,
    /// Hard ceiling on resident dense-material bytes across the terrain
    /// volume, enforced the same way as `budget_bricks` (T23 / G3 row 7
    /// increment 13). `u64::MAX` disables this cap while still enforcing
    /// `budget_bricks` — the historical behaviour before this field existed.
    pub max_dense_bytes: u64,
    /// Chebyshev radius, in bricks, of the kept-resident box around each player.
    pub interest_radius_bricks: i64,
}

/// What one [`ResidencyPass::run`] did.
#[derive(Debug, Clone, Copy, Default)]
pub struct PassTick {
    pub evicted: u64,
    pub reloaded: u64,
    pub resident_terrain_bricks: usize,
    pub over_budget: bool,
    /// T23 / G3 row 7 increment 13: bricks pinned this tick (pending-edit
    /// dependencies, swept-collision footprints, and pipeline reload grace) —
    /// none of these were eligible for eviction this tick regardless of
    /// interest or budget pressure.
    pub pinned_bricks: usize,
    /// An interest-driven (not pinned/required) reload was skipped this tick
    /// because admitting it would have exceeded a hard cap. The brick stays
    /// evicted; a future tick may retry once ordinary eviction frees room.
    pub admission_deferred: u64,
    /// The **required** set (every currently in-interest or pinned brick)
    /// alone exceeds `budget_bricks` this tick — capacity pressure real
    /// enough that even evicting every optional brick could not fit it. This
    /// pass never evicts required geometry to force a fit; it only reports
    /// the pressure honestly.
    pub required_over_budget: bool,
}

/// Cumulative residency stats for a run.
#[derive(Debug, Clone, Copy)]
pub struct ResidencyStats {
    pub evictions_total: u64,
    pub reloads_total: u64,
    pub resident_terrain_bricks_min: usize,
    pub resident_terrain_bricks_max: usize,
    pub resident_terrain_bricks_final: usize,
    pub budget_miss_ticks: u64,
    /// T23 / G3 row 7 item 3: total resident dense-material bytes across the
    /// whole `SimWorld` (terrain + every body volume), sampled every tick
    /// this pass runs. This is the live, in-process working set the
    /// residency budget exists to bound -- see [`total_resident_dense_bytes`].
    pub resident_dense_bytes_min: u64,
    pub resident_dense_bytes_max: u64,
    pub resident_dense_bytes_final: u64,
    /// T23 / G3 row 7 increment 13: peak/final pin-set size and the two
    /// admission-pressure counters, cumulative across the run — the
    /// "blocked-load / pin metrics" the frozen contract calls for.
    pub pinned_bricks_max: usize,
    pub admission_deferred_total: u64,
    /// Ticks where the required (interest ∪ pinned) set alone exceeded
    /// `budget_bricks` — see [`PassTick::required_over_budget`].
    pub required_over_budget_ticks: u64,
    /// Approximate bytes of retained evicted-brick digest metadata
    /// (`evicted.len() * size_of::<(BrickCoord, BrickDigest)>()`). Small, but
    /// the frozen contract lists it as its own retained-memory line item
    /// distinct from resident dense payload and disk footprint.
    pub digest_bytes_final: u64,
    /// The durable backing's own retained in-process bytes
    /// (`spall_sim::BrickBackingWriter::resident_bytes`) — `None` when the
    /// installed backing does not keep a resident cache (a pure disk store).
    pub backing_bytes_final: Option<u64>,
    /// T23 / G3 row 7 increment 15: cumulative, across every periodic +
    /// shutdown [`ResidencyPass::capture_checkpoint`] call this run, of
    /// terrain bricks actually re-snapshotted/reloaded and re-encoded (a
    /// revision the pass's checkpoint cache did not already hold, or holds a
    /// stale copy of). A brick whose revision has not changed since the last
    /// checkpoint that captured it reuses that prior record untouched and is
    /// **not** counted here — see [`Self::checkpoint_bricks_logical_total`]
    /// for the size of the full logical set each of those calls produced.
    pub checkpoint_bricks_captured_total: u64,
    /// T23 / G3 row 7 increment 15: cumulative count of terrain bricks each
    /// `capture_checkpoint` call's *complete* logical (resident ∪ evicted)
    /// set contained, summed the same way as
    /// [`Self::checkpoint_bricks_captured_total`]. `captured_total <
    /// logical_total` (once more than one checkpoint has actually changed
    /// state) is the direct evidence that capture is incremental, not a
    /// full-walk no-op wearing a cache.
    pub checkpoint_bricks_logical_total: u64,
}

/// Total resident dense-material bytes across `world`'s terrain and every
/// body volume, via [`spall_voxel::Volume::memory_report`]. Independent of
/// whether residency is on: with it off, this is simply the whole world's
/// resident footprint (a useful baseline to compare a budgeted run against).
pub fn total_resident_dense_bytes(world: &SimWorld) -> u64 {
    let mut total = world.terrain().volume.memory_report().total_dense_bytes() as u64;
    for body in world.bodies() {
        total += body.volume.memory_report().total_dense_bytes() as u64;
    }
    total
}

/// Approximate retained bytes of one volume's digest metadata
/// (`BrickCoord` key + `BrickDigest` value) — the "digest-metadata bytes"
/// line item in the frozen contract's memory/observability table.
fn digest_bytes(world: &SimWorld, volume: VolumeId) -> u64 {
    const PER_ENTRY: u64 =
        (size_of::<BrickCoord>() + size_of::<spall_voxel::logical::BrickDigest>()) as u64;
    world.evicted(volume).len() as u64 * PER_ENTRY
}

pub struct ResidencyPass {
    limits: ResidencyLimits,
    terrain: VolumeId,
    cell_m: f64,
    backing: Arc<dyn BrickBackingWriter>,
    /// Bricks the pass itself evicted (so a reload by the edit pipeline is
    /// distinguishable and resets the settle counter).
    evicted_by_pass: BTreeSet<BrickCoord>,
    /// Consecutive ticks each resident, out-of-interest brick has waited.
    out_of_interest: BTreeMap<BrickCoord, u32>,
    /// T23 / G3 row 7 increment 13: bricks the *pipeline* (not this pass)
    /// reloaded recently, still counting down. `> 0` pins the brick.
    reload_grace: BTreeMap<BrickCoord, u32>,
    /// Previous tick's player feet, keyed by the caller-supplied stable id
    /// (a player's `EntityId`, not a list position -- a late joiner inserted
    /// anywhere in `world.players()`'s iteration order must never be
    /// misattributed to a different player's previous position, which would
    /// manufacture a spurious "teleport" swept-pin).
    prev_player_feet: BTreeMap<u64, [f64; 3]>,
    /// Previous tick's active-body bounding spheres, keyed by entity id.
    prev_body_spheres: BTreeMap<u64, ([f64; 3], f64)>,
    evictions_total: u64,
    reloads_total: u64,
    resident_min: usize,
    resident_max: usize,
    resident_final: usize,
    budget_miss_ticks: u64,
    dense_bytes_min: u64,
    dense_bytes_max: u64,
    dense_bytes_final: u64,
    pinned_max: usize,
    admission_deferred_total: u64,
    required_over_budget_ticks: u64,
    /// T23 / G3 row 7 increment 15: the last successful `capture_checkpoint`
    /// call's per-brick records (terrain only), keyed by coordinate, so an
    /// unchanged brick's record is reused rather than re-snapshotted /
    /// reloaded / re-encoded on the next call. Seeded at install time from
    /// every then-resident brick (the pass's own natural "first checkpoint"
    /// baseline, mirroring how the backing itself is seeded at install) so
    /// even a run's *first* real capture already skips untouched bricks.
    checkpoint_cache: BTreeMap<BrickCoord, (Revision, StoredBrick)>,
    checkpoint_bricks_captured_total: u64,
    checkpoint_bricks_logical_total: u64,
}

impl ResidencyPass {
    /// Builds the pass, seeds an in-process [`MemoryBacking`] from the current
    /// terrain, and installs it on `world`. This is the historical default
    /// and every prior caller's exact behaviour; see
    /// [`Self::install_with_backing`] for a real disk-backed store.
    pub fn install(world: &mut SimWorld, limits: ResidencyLimits) -> Self {
        Self::install_with_backing(world, limits, Arc::new(MemoryBacking::default()))
    }

    /// Builds the pass against any [`BrickBackingWriter`] -- the T23/G3 row 7
    /// unification point (item 1): `MemoryBacking` (in-process) and
    /// `spall_server::disk_backing::DiskBrickBacking` (real disk, item 2)
    /// both satisfy this trait, so this constructor and everything else in
    /// this module is written once, against the trait, and does not care
    /// which is installed. Seeds `backing` from every currently-resident
    /// terrain brick (idempotent -- a fresh backing is fully populated, a
    /// restart-recovered one is simply re-captured at its current, already
    /// correct values) and installs it on `world`.
    ///
    /// T23 / G3 row 7 increment 15: the same walk also seeds
    /// [`Self::checkpoint_cache`] with every currently-resident brick's
    /// encoded checkpoint record, so this pass's *first* real
    /// `capture_checkpoint` call already treats every brick untouched since
    /// install as a cache hit, not just later ones treating a prior real
    /// checkpoint as the baseline. A brick that fails to encode here (should
    /// not happen -- the same encode path `capture_checkpoint` itself uses)
    /// simply misses this seed and is captured fresh on the first real call.
    pub fn install_with_backing(
        world: &mut SimWorld,
        limits: ResidencyLimits,
        backing: Arc<dyn BrickBackingWriter>,
    ) -> Self {
        let terrain = world.terrain_volume_id();
        let cell_m = world.terrain().volume.cell_size().metres();
        let mut checkpoint_cache = BTreeMap::new();
        for coord in world.terrain().volume.resident_brick_coords() {
            backing.capture(&world.terrain().volume, coord);
            if let Ok(Some(snap)) = world.terrain().volume.snapshot_brick(coord)
                && let Ok(stored) = stored_brick_from_snapshot(terrain, coord, &snap)
            {
                checkpoint_cache.insert(coord, (snap.revision(), stored));
            }
        }
        world.set_backing(backing.clone());
        let resident = world.terrain().volume.resident_brick_count();
        let dense_bytes = total_resident_dense_bytes(world);
        Self {
            limits,
            terrain,
            cell_m,
            backing,
            evicted_by_pass: BTreeSet::new(),
            out_of_interest: BTreeMap::new(),
            reload_grace: BTreeMap::new(),
            prev_player_feet: BTreeMap::new(),
            prev_body_spheres: BTreeMap::new(),
            checkpoint_cache,
            evictions_total: 0,
            reloads_total: 0,
            resident_min: resident,
            resident_max: resident,
            resident_final: resident,
            budget_miss_ticks: 0,
            dense_bytes_min: dense_bytes,
            dense_bytes_max: dense_bytes,
            dense_bytes_final: dense_bytes,
            pinned_max: 0,
            admission_deferred_total: 0,
            required_over_budget_ticks: 0,
            checkpoint_bricks_captured_total: 0,
            checkpoint_bricks_logical_total: 0,
        }
    }

    /// The durable backing this pass owns — hand it to the late-join / repair
    /// capture path so a baseline can fill an evicted brick. Read-only
    /// (`BrickBacking`, not `BrickBackingWriter`): baseline/repair capture
    /// only ever loads.
    pub fn backing(&self) -> Arc<dyn BrickBacking> {
        self.backing.clone()
    }

    /// Current [`ResidencyLimits`].
    pub fn limits(&self) -> ResidencyLimits {
        self.limits
    }

    /// Adjusts the enforced limits without reinstalling the pass or its
    /// backing (an operator raising/lowering a running server's budget, or a
    /// test exercising admission behaviour under a controlled cap change).
    pub fn set_limits(&mut self, limits: ResidencyLimits) {
        self.limits = limits;
    }

    /// Record the current geometry of every brick a committed transaction
    /// touched, so a later reload gets the right revision.
    pub fn on_commit(&self, world: &SimWorld, touched: impl IntoIterator<Item = BrickCoord>) {
        for coord in touched {
            self.backing.capture(&world.terrain().volume, coord);
        }
    }

    /// T23 / G3 row 7 increment 13: pins every brick the edit pipeline itself
    /// reloaded this tick (`TickReport::reloaded_bricks`, terrain entries
    /// only) for [`PIN_GRACE_TICKS`] more ticks, so this pass cannot evict a
    /// dependency the pipeline just went to the trouble of reloading before
    /// the re-queued intent's retry actually runs. Call once per tick, before
    /// [`Self::run`], with every `(volume, coord)` the tick's `TickReport`
    /// named regardless of volume — non-terrain entries are ignored here
    /// (this pass never evicts body geometry).
    pub fn note_pipeline_reloads(
        &mut self,
        reloaded: impl IntoIterator<Item = (VolumeId, BrickCoord)>,
    ) {
        for (volume, coord) in reloaded {
            if volume == self.terrain {
                self.reload_grace.insert(coord, PIN_GRACE_TICKS);
            }
        }
    }

    /// One post-tick residency pass over `player_feet_m` (`(stable id,
    /// world-space feet)` pairs -- the id is a caller-chosen stable key, an
    /// `EntityId::get()` in practice, used to track each player's *own*
    /// previous position across ticks for swept-pin purposes even as other
    /// players join/leave and the set's order changes) and
    /// `pending_edit_bricks` (see
    /// [`spall_sim::Simulation::pending_edit_bricks`]). No-op stats when
    /// there are no players, no pending edits, and nothing pinned.
    pub fn run(
        &mut self,
        world: &mut SimWorld,
        player_feet_m: &[(u64, [f64; 3])],
        pending_edit_bricks: &HashSet<BrickCoord>,
    ) -> PassTick {
        let interest = self.interest_bricks(player_feet_m);
        let pinned = self.pinned_bricks(world, player_feet_m, pending_edit_bricks);

        let resident: Vec<BrickCoord> = world.terrain().volume.resident_brick_coords();
        let evicted_now: Vec<BrickCoord> =
            world.evicted(self.terrain).iter().map(|(c, _)| c).collect();
        let resident_set: BTreeSet<BrickCoord> = resident.iter().copied().collect();

        let mut tick = PassTick {
            pinned_bricks: pinned.len(),
            ..Default::default()
        };

        // The edit pipeline reloaded a brick we had evicted (it is resident and
        // in interest, or resident again without us asking): forget it so its
        // settle timer restarts.
        self.evicted_by_pass.retain(|c| !resident_set.contains(c));

        // Admission bookkeeping: a conservative running estimate, costing every
        // candidate as if it were a full dense brick so the enforced cap can
        // only be tighter than the real measured total, never looser.
        let mut resident_count = resident.len();
        let mut dense_estimate = total_resident_dense_bytes(world);

        // Reload anything back in interest, or required (pinned). A merely
        // desired (interest, not pinned) reload is deferred when it would
        // breach a hard cap; a required one always proceeds.
        for coord in evicted_now {
            let required = pinned.contains(&coord);
            if !required && !interest.contains(&coord) {
                continue;
            }
            if !required {
                let projected_bricks = resident_count.saturating_add(1);
                let projected_dense =
                    dense_estimate.saturating_add(MemoryReport::DENSE_BRICK_BYTES as u64);
                let fits = projected_bricks <= self.limits.budget_bricks
                    && projected_dense <= self.limits.max_dense_bytes;
                if !fits {
                    tick.admission_deferred += 1;
                    continue;
                }
            }
            if matches!(world.reload_brick(self.terrain, coord), Ok(true)) {
                tick.reloaded += 1;
                self.evicted_by_pass.remove(&coord);
                self.out_of_interest.remove(&coord);
                resident_count += 1;
                // Re-measure exactly rather than keep adding the conservative
                // per-candidate worst case: most reloaded bricks are actually
                // `Uniform` (free), so a batch of them within one tick must
                // not phantom-inflate the running estimate and starve a real
                // `Dense` candidate later in the same batch, or artificially
                // block a raised cap from ever being reachable again. The
                // admission *check* above stays conservative (never risks
                // exceeding the real cap); only the bookkeeping after a
                // confirmed load is corrected to reality.
                dense_estimate = total_resident_dense_bytes(world);
            }
        }

        // Evict resident terrain bricks that have been out of interest for
        // `EVICT_SETTLE_TICKS` consecutive ticks. Pinned bricks are never
        // considered, regardless of interest or wait time. This hysteresis
        // window is unchanged by admission enforcement above -- every
        // unpinned, out-of-interest resident brick is still evicted on
        // exactly the schedule it always was; enforcement instead governs
        // whether new (interest-driven, non-required) geometry is *admitted*
        // back in, and the pressure signal below reports when even that is
        // not enough.
        self.out_of_interest.retain(|c, _| resident_set.contains(c));
        for &coord in &resident {
            if interest.contains(&coord) || pinned.contains(&coord) {
                self.out_of_interest.remove(&coord);
                continue;
            }
            let waited = self.out_of_interest.entry(coord).or_insert(0);
            *waited += 1;
            if *waited < EVICT_SETTLE_TICKS {
                continue;
            }
            // T23 / G3 row 7: ack-before-evict. A fresh, gated capture right
            // here guarantees the backing holds this exact revision before the
            // brick leaves the live cache -- the same contract T18's
            // `ResidencyController::enforce_budget` enforces ("persist dirty
            // candidates synchronously ... a backing error leaves geometry
            // resident"). A failed capture skips eviction this tick; `waited`
            // stays elevated so the very next tick retries.
            if self.backing.capture(&world.terrain().volume, coord)
                && matches!(world.evict_brick(self.terrain, coord), Ok(true))
            {
                tick.evicted += 1;
                self.evicted_by_pass.insert(coord);
                self.out_of_interest.remove(&coord);
                resident_count = resident_count.saturating_sub(1);
                dense_estimate =
                    dense_estimate.saturating_sub(MemoryReport::DENSE_BRICK_BYTES as u64);
            }
        }

        let resident_bricks = world.terrain().volume.resident_brick_count();
        tick.resident_terrain_bricks = resident_bricks;
        tick.over_budget = resident_bricks > self.limits.budget_bricks
            || total_resident_dense_bytes(world) > self.limits.max_dense_bytes;
        // T23 / G3 row 7 increment 13: the required (pinned ∪ in-interest)
        // set alone would not fit even if every evictable brick were gone --
        // an explicit, honest "cannot fit" signal per the frozen contract
        // ("explicitly deferring work when the required pinned set cannot
        // fit ... never silently claim the budget passed"), rather than
        // fabricating admission or silently evicting something still needed.
        let required_bricks = interest.union(&pinned).count();
        tick.required_over_budget = required_bricks > self.limits.budget_bricks;

        self.evictions_total += tick.evicted;
        self.reloads_total += tick.reloaded;
        self.resident_min = self.resident_min.min(resident_bricks);
        self.resident_max = self.resident_max.max(resident_bricks);
        self.resident_final = resident_bricks;
        if tick.over_budget {
            self.budget_miss_ticks += 1;
        }
        self.pinned_max = self.pinned_max.max(tick.pinned_bricks);
        self.admission_deferred_total += tick.admission_deferred;
        if tick.required_over_budget {
            self.required_over_budget_ticks += 1;
        }

        // T23 / G3 row 7 item 3: sample the live resident dense-byte total
        // every tick, the same way the brick-count ceiling above is tracked.
        let dense_bytes = total_resident_dense_bytes(world);
        self.dense_bytes_min = self.dense_bytes_min.min(dense_bytes);
        self.dense_bytes_max = self.dense_bytes_max.max(dense_bytes);
        self.dense_bytes_final = dense_bytes;

        // Decay reload grace *after* this tick used it, so a brick reloaded
        // this tick (via `note_pipeline_reloads`, called before `run`) is
        // fully pinned for this tick's decision and remains pinned for
        // exactly `PIN_GRACE_TICKS - 1` more.
        self.reload_grace.retain(|_, ticks_left| {
            *ticks_left -= 1;
            *ticks_left > 0
        });

        tick
    }

    /// T23 / G3 row 7 follow-up (post-merge review P2/P3), made incremental in
    /// increment 15: captures a checkpoint that includes evicted terrain read
    /// straight from the durable backing, instead of `reload_all`-ing every
    /// evicted brick back into the live world first just to satisfy
    /// `persist::capture`'s resident-only brick walk. That old path defeated
    /// the point of residency during every periodic checkpoint: memory would
    /// spike back up to the full world right before capture, then evict back
    /// down again — this keeps the live world's resident set (and its memory)
    /// untouched throughout. `world_hash` is unaffected either way — it has
    /// been the logical (resident ∪ evicted-digest) hash since increment 6,
    /// so this only changes what `capture` does to produce a checkpoint whose
    /// `bricks` actually reproduce that hash on recovery.
    ///
    /// **Incremental (increment 15):** every checkpoint's `bricks` list still
    /// covers the *complete* logical (resident ∪ evicted) terrain set — a
    /// checkpoint must stand alone for recovery, since older ones are pruned
    /// (`serve.rs`'s `RETAIN_CHECKPOINTS`) — but a brick whose revision has
    /// not changed since [`Self::checkpoint_cache`] last recorded it reuses
    /// that prior [`StoredBrick`] record verbatim: no fresh live snapshot (for
    /// a resident brick) or backing load (for an evicted one), and no
    /// re-encode. Only a brick whose revision differs (an edit, a fresh
    /// eviction/reload at a new revision, or one the cache has never seen) is
    /// actually re-captured. Revision equality implies content equality
    /// within one world/session generation — the same assumption
    /// `EvictedBricks::verify_reload` and the rest of the digest lifecycle
    /// already rely on (`docs/reports/G3-residency-hash.md`'s digest
    /// lifecycle table). The cache is seeded at install time
    /// ([`Self::install_with_backing`]) so even a run's first real checkpoint
    /// already skips every brick untouched since world creation, not only
    /// later checkpoints comparing against an earlier real one.
    ///
    /// An evicted brick absent from the backing fails the whole capture
    /// (`PersistError::EvictedBrickUnavailable`) rather than silently
    /// publishing a checkpoint whose `world_hash` claims geometry its
    /// `bricks` do not carry — recovery's rebuilt-hash check would catch that
    /// anyway, but failing here is the earlier, clearer signal. In practice
    /// this should not happen: every terrain brick is captured into the
    /// backing on install and again on every commit that touches it. A failed
    /// capture leaves [`Self::checkpoint_cache`] untouched (updated only after
    /// every brick in this call succeeded), matching the rest of this pass's
    /// fail-closed convention.
    pub fn capture_checkpoint(
        &mut self,
        sim: &Simulation,
        cfg: &PersistConfig,
        journal_cursor: u64,
    ) -> Result<Checkpoint, PersistError> {
        let world = sim.world();
        let mut new_cache = BTreeMap::new();
        let mut terrain_bricks = Vec::new();
        let mut captured = 0u64;

        for coord in world.terrain().volume.resident_brick_coords() {
            let snap = world
                .terrain()
                .volume
                .snapshot_brick(coord)
                .ok()
                .flatten()
                .expect("coord came from the resident set");
            let revision = snap.revision();
            let stored = match self.checkpoint_cache.get(&coord) {
                Some((cached_rev, cached)) if *cached_rev == revision => cached.clone(),
                _ => {
                    captured += 1;
                    stored_brick_from_snapshot(self.terrain, coord, &snap)?
                }
            };
            new_cache.insert(coord, (revision, stored.clone()));
            terrain_bricks.push(stored);
        }

        for (coord, digest) in world.evicted(self.terrain).iter() {
            let revision = digest.revision;
            let stored = match self.checkpoint_cache.get(&coord) {
                Some((cached_rev, cached)) if *cached_rev == revision => cached.clone(),
                _ => {
                    captured += 1;
                    let brick = match self.backing.load(self.terrain, coord) {
                        BackingBrick::Loaded(brick) => brick,
                        BackingBrick::KnownEmpty { revision, edited } => {
                            let air = vec![MaterialId::AIR; CELLS_PER_BRICK];
                            Brick::restored(&air, revision, edited)
                        }
                        BackingBrick::Unavailable => {
                            return Err(PersistError::EvictedBrickUnavailable {
                                volume: self.terrain.get(),
                                coord: [coord.x, coord.y, coord.z],
                            });
                        }
                    };
                    stored_brick_from_backing(self.terrain, coord, &brick)?
                }
            };
            new_cache.insert(coord, (revision, stored.clone()));
            terrain_bricks.push(stored);
        }

        let logical_total = terrain_bricks.len() as u64;
        let checkpoint = capture_with_terrain_bricks(sim, cfg, journal_cursor, terrain_bricks)?;

        // Only commit the new cache / counters once every brick in this call
        // succeeded — a failed capture (the `EvictedBrickUnavailable` early
        // return above) must not leave the pass believing bricks it never
        // actually re-verified are still accurately cached.
        self.checkpoint_cache = new_cache;
        self.checkpoint_bricks_captured_total += captured;
        self.checkpoint_bricks_logical_total += logical_total;

        Ok(checkpoint)
    }

    pub fn stats(&self) -> ResidencyStats {
        ResidencyStats {
            evictions_total: self.evictions_total,
            reloads_total: self.reloads_total,
            resident_terrain_bricks_min: self.resident_min,
            resident_terrain_bricks_max: self.resident_max,
            resident_terrain_bricks_final: self.resident_final,
            budget_miss_ticks: self.budget_miss_ticks,
            resident_dense_bytes_min: self.dense_bytes_min,
            resident_dense_bytes_max: self.dense_bytes_max,
            resident_dense_bytes_final: self.dense_bytes_final,
            pinned_bricks_max: self.pinned_max,
            admission_deferred_total: self.admission_deferred_total,
            required_over_budget_ticks: self.required_over_budget_ticks,
            digest_bytes_final: 0,
            backing_bytes_final: self.backing.resident_bytes(),
            checkpoint_bricks_captured_total: self.checkpoint_bricks_captured_total,
            checkpoint_bricks_logical_total: self.checkpoint_bricks_logical_total,
        }
    }

    /// [`ResidencyStats`] plus the digest-metadata bytes, which need live
    /// `SimWorld` access `stats()` alone does not have.
    pub fn stats_with_world(&self, world: &SimWorld) -> ResidencyStats {
        ResidencyStats {
            digest_bytes_final: digest_bytes(world, self.terrain),
            ..self.stats()
        }
    }

    /// The durable backing's on-disk footprint, when it is a real disk-backed
    /// store (`None` for the in-process `MemoryBacking` default, which has
    /// none). T23 / G3 row 7 item 3's durable-side counterpart to
    /// `resident_dense_bytes_*`.
    pub fn backing_disk_bytes(&self) -> Option<u64> {
        self.backing.disk_bytes()
    }

    fn interest_bricks(&self, player_feet_m: &[(u64, [f64; 3])]) -> BTreeSet<BrickCoord> {
        let r = self.limits.interest_radius_bricks;
        let mut set = BTreeSet::new();
        for &(_, feet) in player_feet_m {
            let b = self.brick_of(feet);
            for dz in -r..=r {
                for dy in -r..=r {
                    for dx in -r..=r {
                        set.insert(BrickCoord::new(b.x + dx, b.y + dy, b.z + dz));
                    }
                }
            }
        }
        set
    }

    fn brick_of(&self, point_m: [f64; 3]) -> BrickCoord {
        let cell = GlobalCell::new(
            (point_m[0] / self.cell_m).floor() as i64,
            (point_m[1] / self.cell_m).floor() as i64,
            (point_m[2] / self.cell_m).floor() as i64,
        );
        cell.split().0
    }

    /// Every brick a candidate box between two world points (grown by
    /// `margin_m`) touches, in this volume's terrain cell size.
    fn segment_bricks(&self, a_m: [f64; 3], b_m: [f64; 3], margin_m: f64) -> BTreeSet<BrickCoord> {
        let margin = margin_m.max(0.0);
        let mut lo = [0i64; 3];
        let mut hi = [0i64; 3];
        for axis in 0..3 {
            lo[axis] = ((a_m[axis].min(b_m[axis]) - margin) / self.cell_m).floor() as i64;
            hi[axis] = ((a_m[axis].max(b_m[axis]) + margin) / self.cell_m).floor() as i64;
        }
        let (min_b, _) = GlobalCell::new(lo[0], lo[1], lo[2]).split();
        let (max_b, _) = GlobalCell::new(hi[0], hi[1], hi[2]).split();
        let mut set = BTreeSet::new();
        for z in min_b.z..=max_b.z {
            for y in min_b.y..=max_b.y {
                for x in min_b.x..=max_b.x {
                    set.insert(BrickCoord::new(x, y, z));
                }
            }
        }
        set
    }

    /// T23 / G3 row 7 increment 13: the union of every pin source for this
    /// tick — pending-edit dependencies, pipeline reload grace, and
    /// swept-collision paths for players and active bodies — updating the
    /// previous-tick position trackers as a side effect.
    fn pinned_bricks(
        &mut self,
        world: &SimWorld,
        player_feet_m: &[(u64, [f64; 3])],
        pending_edit_bricks: &HashSet<BrickCoord>,
    ) -> BTreeSet<BrickCoord> {
        let mut pinned: BTreeSet<BrickCoord> = pending_edit_bricks.iter().copied().collect();
        pinned.extend(self.reload_grace.keys().copied());

        // Player swept path: the segment from last tick's feet to this
        // tick's, one cell of margin either side (the capsule radius is a
        // fraction of a cell; a whole extra cell of slack is intentionally
        // generous rather than importing the exact character radius here).
        // Keyed by the caller's stable id, never by list position -- a late
        // joiner inserted anywhere in the slice must not be misattributed to
        // a different player's last-known position.
        let mut next_feet = BTreeMap::new();
        for &(id, feet) in player_feet_m {
            let prev = self.prev_player_feet.get(&id).copied().unwrap_or(feet);
            pinned.extend(self.segment_bricks(prev, feet, self.cell_m));
            next_feet.insert(id, feet);
        }
        self.prev_player_feet = next_feet;

        // Active-body swept bounding sphere: every non-sleeping, non-dormant
        // body's collider region sweeps between its previous and current
        // world-space bounding sphere. Sleeping/dormant bodies do not move
        // and need no swept pin; a stale entry for a body no longer iterated
        // (retired/tombstoned) is simply not carried forward.
        let mut next_spheres = BTreeMap::new();
        for body in world.bodies() {
            let Some(entity) = body.entity else { continue };
            if body.sleeping || body.dormant {
                continue;
            }
            let (centre, radius) = body.world_bounding_sphere();
            let key = entity.get();
            let prev = self
                .prev_body_spheres
                .get(&key)
                .copied()
                .unwrap_or((centre, radius));
            pinned.extend(self.segment_bricks(prev.0, centre, radius.max(prev.1)));
            next_spheres.insert(key, (centre, radius));
        }
        self.prev_body_spheres = next_spheres;

        pinned
    }
}
