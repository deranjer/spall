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
//! Body volumes are never evicted. This pass owns the `Arc<MemoryBacking>` and
//! hands the same handle to [`SimWorld::set_backing`], so its `on_commit`
//! updates and the pipeline's reloads see one store.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spall_core::{BrickCoord, CELLS_PER_BRICK, GlobalCell, MaterialId, VolumeId};
use spall_sim::{BackingBrick, BrickBacking, MemoryBacking, SimWorld, Simulation};
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
}

pub struct ResidencyPass {
    limits: ResidencyLimits,
    terrain: VolumeId,
    cell_m: f64,
    backing: Arc<MemoryBacking>,
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
}

impl ResidencyPass {
    /// Builds the pass, seeds the backing from the current terrain, and installs
    /// that backing on `world`.
    pub fn install(world: &mut SimWorld, limits: ResidencyLimits) -> Self {
        let terrain = world.terrain_volume_id();
        let cell_m = world.terrain().volume.cell_size().metres();
        let backing = Arc::new(MemoryBacking::from_volume(&world.terrain().volume));
        world.set_backing(backing.clone());
        let resident = world.terrain().volume.resident_brick_count();
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
        }
    }

    /// The durable backing this pass owns — hand it to the late-join / repair
    /// capture path so a baseline can fill an evicted brick.
    pub fn backing(&self) -> Arc<MemoryBacking> {
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
            if *waited >= EVICT_SETTLE_TICKS
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
        }
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
