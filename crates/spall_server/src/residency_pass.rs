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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spall_core::{BrickCoord, CELLS_PER_BRICK, GlobalCell, MaterialId, VolumeId};
use spall_sim::{
    BackingBrick, BrickBacking, BrickBackingWriter, MemoryBacking, SimWorld, Simulation,
};
use spall_store::Checkpoint;
use spall_voxel::Brick;

use crate::persist::{PersistConfig, PersistError, stored_brick_from_backing};

/// Consecutive ticks a terrain brick must be resident *and* outside every
/// player's interest before the pass evicts it. The hysteresis stops the pass
/// from fighting the edit pipeline, which reloads a brick a queued edit needs
/// (`EvictedGeometryRequired`) and needs a few ticks to re-stage and commit it.
const EVICT_SETTLE_TICKS: u32 = 4;

/// Residency limits for a [`serve`](crate::serve) run.
#[derive(Debug, Clone, Copy)]
pub struct ResidencyLimits {
    /// Reported ceiling on resident terrain bricks. The pass does not force it
    /// below the interest set; a run whose interest set exceeds it records
    /// `budget_exceeded` rather than dropping geometry a player needs.
    pub budget_bricks: usize,
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
    evictions_total: u64,
    reloads_total: u64,
    resident_min: usize,
    resident_max: usize,
    resident_final: usize,
    budget_miss_ticks: u64,
    dense_bytes_min: u64,
    dense_bytes_max: u64,
    dense_bytes_final: u64,
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
    pub fn install_with_backing(
        world: &mut SimWorld,
        limits: ResidencyLimits,
        backing: Arc<dyn BrickBackingWriter>,
    ) -> Self {
        let terrain = world.terrain_volume_id();
        let cell_m = world.terrain().volume.cell_size().metres();
        for coord in world.terrain().volume.resident_brick_coords() {
            backing.capture(&world.terrain().volume, coord);
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
            evictions_total: 0,
            reloads_total: 0,
            resident_min: resident,
            resident_max: resident,
            resident_final: resident,
            budget_miss_ticks: 0,
            dense_bytes_min: dense_bytes,
            dense_bytes_max: dense_bytes,
            dense_bytes_final: dense_bytes,
        }
    }

    /// The durable backing this pass owns — hand it to the late-join / repair
    /// capture path so a baseline can fill an evicted brick. Read-only
    /// (`BrickBacking`, not `BrickBackingWriter`): baseline/repair capture
    /// only ever loads.
    pub fn backing(&self) -> Arc<dyn BrickBacking> {
        self.backing.clone()
    }

    /// Record the current geometry of every brick a committed transaction
    /// touched, so a later reload gets the right revision.
    pub fn on_commit(&self, world: &SimWorld, touched: impl IntoIterator<Item = BrickCoord>) {
        for coord in touched {
            self.backing.capture(&world.terrain().volume, coord);
        }
    }

    /// One post-tick residency pass over `player_feet_m` (world-space feet
    /// points). No-op stats when there are no players.
    pub fn run(&mut self, world: &mut SimWorld, player_feet_m: &[[f64; 3]]) -> PassTick {
        let interest = self.interest_bricks(player_feet_m);

        let resident: Vec<BrickCoord> = world.terrain().volume.resident_brick_coords();
        let evicted_now: Vec<BrickCoord> =
            world.evicted(self.terrain).iter().map(|(c, _)| c).collect();
        let resident_set: BTreeSet<BrickCoord> = resident.iter().copied().collect();

        let mut tick = PassTick::default();

        // The edit pipeline reloaded a brick we had evicted (it is resident and
        // in interest, or resident again without us asking): forget it so its
        // settle timer restarts.
        self.evicted_by_pass.retain(|c| !resident_set.contains(c));

        // Reload anything back in interest.
        for coord in evicted_now {
            if interest.contains(&coord)
                && matches!(world.reload_brick(self.terrain, coord), Ok(true))
            {
                tick.reloaded += 1;
                self.evicted_by_pass.remove(&coord);
                self.out_of_interest.remove(&coord);
            }
        }

        // Evict resident terrain bricks that have been out of interest for
        // `EVICT_SETTLE_TICKS` consecutive ticks.
        self.out_of_interest.retain(|c, _| resident_set.contains(c));
        for coord in resident {
            if interest.contains(&coord) {
                self.out_of_interest.remove(&coord);
                continue;
            }
            let waited = self.out_of_interest.entry(coord).or_insert(0);
            *waited += 1;
            // T23 / G3 row 7: ack-before-evict. A fresh, gated capture right
            // here guarantees the backing holds this exact revision before the
            // brick leaves the live cache -- the same contract T18's
            // `ResidencyController::enforce_budget` enforces ("persist dirty
            // candidates synchronously ... a backing error leaves geometry
            // resident"). A failed capture skips eviction this tick; `waited`
            // stays elevated so the very next tick retries.
            if *waited >= EVICT_SETTLE_TICKS
                && self.backing.capture(&world.terrain().volume, coord)
                && matches!(world.evict_brick(self.terrain, coord), Ok(true))
            {
                tick.evicted += 1;
                self.evicted_by_pass.insert(coord);
                self.out_of_interest.remove(&coord);
            }
        }

        let resident_bricks = world.terrain().volume.resident_brick_count();
        tick.resident_terrain_bricks = resident_bricks;
        tick.over_budget = resident_bricks > self.limits.budget_bricks;

        self.evictions_total += tick.evicted;
        self.reloads_total += tick.reloaded;
        self.resident_min = self.resident_min.min(resident_bricks);
        self.resident_max = self.resident_max.max(resident_bricks);
        self.resident_final = resident_bricks;
        if tick.over_budget {
            self.budget_miss_ticks += 1;
        }

        // T23 / G3 row 7 item 3: sample the live resident dense-byte total
        // every tick, the same way the brick-count ceiling above is tracked.
        let dense_bytes = total_resident_dense_bytes(world);
        self.dense_bytes_min = self.dense_bytes_min.min(dense_bytes);
        self.dense_bytes_max = self.dense_bytes_max.max(dense_bytes);
        self.dense_bytes_final = dense_bytes;

        tick
    }

    /// T23 / G3 row 7 follow-up (post-merge review P2/P3): captures a
    /// checkpoint that includes evicted terrain read straight from the
    /// durable backing, instead of `reload_all`-ing every evicted brick back
    /// into the live world first just to satisfy `persist::capture`'s
    /// resident-only brick walk. That old path defeated the point of
    /// residency during every periodic checkpoint: memory would spike back up
    /// to the full world right before capture, then evict back down again —
    /// this keeps the live world's resident set (and its memory) untouched
    /// throughout. `world_hash` is unaffected either way — it has been the
    /// logical (resident ∪ evicted-digest) hash since increment 6, so this
    /// only changes what `capture` does to produce a checkpoint whose
    /// `bricks` actually reproduce that hash on recovery.
    ///
    /// An evicted brick absent from the backing fails the whole capture
    /// (`PersistError::EvictedBrickUnavailable`) rather than silently
    /// publishing a checkpoint whose `world_hash` claims geometry its
    /// `bricks` do not carry — recovery's rebuilt-hash check would catch that
    /// anyway, but failing here is the earlier, clearer signal. In practice
    /// this should not happen: every terrain brick is captured into the
    /// backing on install and again on every commit that touches it.
    pub fn capture_checkpoint(
        &self,
        sim: &Simulation,
        cfg: &PersistConfig,
        journal_cursor: u64,
    ) -> Result<Checkpoint, PersistError> {
        let mut checkpoint = crate::persist::capture(sim, cfg, journal_cursor)?;
        for (coord, _digest) in sim.world().evicted(self.terrain).iter() {
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
            checkpoint
                .bricks
                .push(stored_brick_from_backing(self.terrain, coord, &brick)?);
        }
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
        }
    }

    /// The durable backing's on-disk footprint, when it is a real disk-backed
    /// store (`None` for the in-process `MemoryBacking` default, which has
    /// none). T23 / G3 row 7 item 3's durable-side counterpart to
    /// `resident_dense_bytes_*`.
    pub fn backing_disk_bytes(&self) -> Option<u64> {
        self.backing.disk_bytes()
    }

    fn interest_bricks(&self, player_feet_m: &[[f64; 3]]) -> BTreeSet<BrickCoord> {
        let r = self.limits.interest_radius_bricks;
        let mut set = BTreeSet::new();
        for feet in player_feet_m {
            let cell = GlobalCell::new(
                (feet[0] / self.cell_m).floor() as i64,
                (feet[1] / self.cell_m).floor() as i64,
                (feet[2] / self.cell_m).floor() as i64,
            );
            let b = cell.split().0;
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
}
