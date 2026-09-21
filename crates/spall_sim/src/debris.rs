//! Optional game-authorized retirement of small, continuously dormant debris.
//!
//! This is destruction, not dormancy: every removed cell goes through a normal
//! topology transaction and journal. Bodies are protected unless the game
//! explicitly approves their stable id. Approval and timers are session-local;
//! a restarted game must approve again and wait the full interval.

use std::collections::{BTreeMap, HashSet};

use spall_core::{EntityId, GlobalCell, LocalCell, MaterialId, Tick};
use spall_protocol::{
    BrickRevision, CanonicalOwner, ControlSeq, Record, TopologyOp, TopologyTransaction, VolumeHash,
};
use spall_voxel::EditPlan;

use crate::{Body, CommitError, Committed, JournalEntry, JournalSink, SimWorld};

/// Hard bounds for this small-fragment policy, independent of game thresholds.
pub const MAX_TRACKED_DEBRIS: usize = 256;
const MAX_FRAGMENT_CELLS: usize = 256;
const MAX_FRAGMENT_BRICKS: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct DebrisLifetimeConfig {
    /// Occupied volume, not the body's origin or enclosing box.
    pub max_solid_volume_m3: f64,
    /// Continuous dormant simulation ticks; offline time never expires debris.
    pub dormant_ticks: u64,
    /// Minimum distance beyond the fragment's bounding sphere to a player.
    pub player_clearance_m: f64,
    /// Conservative separation from every other body, including sleepers.
    pub body_clearance_m: f64,
    /// At most this many mature fragments are examined per tick (1..=32).
    pub max_candidates_per_tick: usize,
    /// At most this many bodies can be retired per tick.
    pub max_removals_per_tick: usize,
}

impl DebrisLifetimeConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.max_solid_volume_m3.is_finite()
            || self.max_solid_volume_m3 <= 0.0
            || self.dormant_ticks == 0
            || !self.player_clearance_m.is_finite()
            || self.player_clearance_m < 1.0
            || !self.body_clearance_m.is_finite()
            || self.body_clearance_m < 0.1
            || !(1..=32).contains(&self.max_candidates_per_tick)
            || self.max_removals_per_tick == 0
            || self.max_removals_per_tick > self.max_candidates_per_tick
        {
            return Err("invalid debris lifetime size, interval, clearance or work budget");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Approval {
    since: Option<u64>,
    dormant_generation: u64,
    revision: u64,
}

/// Owned by the game/host. Creating one never approves existing or future bodies.
pub struct DebrisLifetimePolicy {
    config: DebrisLifetimeConfig,
    approved: BTreeMap<EntityId, Approval>,
    last_tick: Option<u64>,
    cursor: Option<EntityId>,
}

impl DebrisLifetimePolicy {
    pub fn new(config: DebrisLifetimeConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            config,
            approved: BTreeMap::new(),
            last_tick: None,
            cursor: None,
        })
    }

    /// The game asserts this particular entity is expendable (not a structure,
    /// container, resource to preserve or player-owned object). Children never
    /// inherit approval. Repeated approval does not restart the timer.
    pub fn approve(&mut self, entity: EntityId) -> Result<(), &'static str> {
        if self.approved.contains_key(&entity) {
            return Ok(());
        }
        if self.approved.len() == MAX_TRACKED_DEBRIS {
            return Err("debris approval budget full");
        }
        self.approved.insert(entity, Approval::default());
        Ok(())
    }

    /// Revocation takes effect before the next retirement pass.
    pub fn protect(&mut self, entity: EntityId) {
        self.approved.remove(&entity);
    }

    pub(crate) fn is_approved(&self, entity: EntityId) -> bool {
        self.approved.contains_key(&entity)
    }

    pub(crate) fn candidates(
        &mut self,
        world: &SimWorld,
        tick: u64,
        targeted: &HashSet<EntityId>,
    ) -> Vec<EntityId> {
        if self.last_tick == Some(tick) {
            return Vec::new();
        }
        if self
            .last_tick
            .is_some_and(|t| t.checked_add(1) != Some(tick))
        {
            for a in self.approved.values_mut() {
                a.since = None;
            }
        }
        self.last_tick = Some(tick);
        self.approved.retain(|id, _| world.body(*id).is_some());
        let mut mature = Vec::new();
        for (&id, a) in &mut self.approved {
            let b = world.body(id).expect("retained above");
            let (centre, radius) = b.world_bounding_sphere();
            let near_player = world.players().any(|p| {
                let (lo, hi) = p.capsule_aabb_m();
                let gap2: f64 = (0..3)
                    .map(|i| (centre[i] - centre[i].clamp(lo[i], hi[i])).powi(2))
                    .sum();
                gap2 <= (radius + self.config.player_clearance_m).powi(2)
            });
            if !b.dormant
                || !b.sleeping
                || targeted.contains(&id)
                || near_player
                || b.linvel_m_s != [0.0; 3]
                || b.angvel_rad_s != [0.0; 3]
                || !centre.iter().all(|v| v.is_finite())
                || !radius.is_finite()
            {
                a.since = None;
                continue;
            }
            if a.dormant_generation != b.dormant_generation {
                a.since = None;
                a.dormant_generation = b.dormant_generation;
            }
            if a.revision != b.volume.next_revision().get() {
                a.since = None;
                a.revision = b.volume.next_revision().get();
            }
            let since = *a.since.get_or_insert(tick);
            if tick.saturating_sub(since) >= self.config.dormant_ticks {
                mature.push(id);
            }
        }
        // Round robin: permanently protected neighbours cannot starve later ids.
        if let Some(cursor) = self.cursor {
            let split = mature.partition_point(|id| *id <= cursor);
            mature.rotate_left(split);
        }
        mature.truncate(self.config.max_candidates_per_tick);
        if let Some(&last) = mature.last() {
            self.cursor = Some(last);
        }
        mature
    }

    pub(crate) fn max_removals(&self) -> usize {
        self.config.max_removals_per_tick
    }

    pub(crate) fn removal_plan(&self, world: &SimWorld, id: EntityId) -> Option<EditPlan> {
        let body = world.body(id)?;
        let (c, r) = body.world_bounding_sphere();
        // This intentionally protects all piles: even dormant bodies can support
        // neighbours. No contact graph is available for deactivated colliders.
        if world.bodies().any(|other| {
            if other.entity == Some(id) || other.entity.is_none() {
                return false;
            }
            let (oc, or) = other.world_bounding_sphere();
            let d2: f64 = (0..3).map(|i| (c[i] - oc[i]).powi(2)).sum();
            !d2.is_finite() || d2 <= (r + or + self.config.body_clearance_m).powi(2)
        }) {
            return None;
        }
        if !world.evicted(body.volume_id).is_empty()
            || body.volume.resident_brick_count() > MAX_FRAGMENT_BRICKS
        {
            return None;
        }
        let mut plan = EditPlan::new(body.volume_id);
        for coord in body.volume.resident_brick_coords() {
            let snap = body.volume.snapshot_brick(coord).ok()??;
            for index in 0..spall_core::CELLS_PER_BRICK as u16 {
                let local = LocalCell::from_linear_index(index)?;
                if !snap.get(local).is_air() {
                    plan.set(GlobalCell::from_parts(coord, local).ok()?, MaterialId::AIR);
                    if plan.writes.len() > MAX_FRAGMENT_CELLS {
                        return None;
                    }
                }
            }
        }
        let volume = plan.writes.len() as f64 * body.cell_size().metres().powi(3);
        (!plan.writes.is_empty() && volume <= self.config.max_solid_volume_m3).then_some(plan)
    }
}

/// Explicit material sink for telemetry. This is never reported as conservation
/// by transfer into another body.
#[derive(Debug, Clone)]
pub struct DebrisRetirement {
    pub entity: EntityId,
    pub transaction: spall_core::TransactionId,
    pub destroyed_cells: u64,
    pub solid_volume_m3: f64,
}

/// Candidate-first whole-fragment deletion using the existing CellRun protocol.
/// No physics rebuild, new wire tag or save format is needed for empty-body retirement.
pub(crate) fn retire(
    world: &mut SimWorld,
    journal: &mut JournalSink,
    id: EntityId,
    plan: &EditPlan,
    tick: Tick,
    seq: ControlSeq,
) -> Result<Committed, CommitError> {
    let body: &Body = world.body(id).expect("candidate checked by owner thread");
    let mut candidate = body.volume.clone();
    let outcome = candidate.apply_edit(plan)?;
    let mut reg = world.registry().clone();
    let transaction_id = reg.allocate_transaction()?;
    let journal_seq = reg.allocate_journal_seq()?;
    let topology = TopologyTransaction {
        transaction_id,
        server_tick: tick,
        control_seq: seq,
        algorithm_version: crate::commit::ALGORITHM_VERSION,
        dependencies: Vec::new(),
        before: outcome
            .bricks
            .iter()
            .map(|b| BrickRevision {
                volume: plan.volume,
                coord: b.coord,
                revision: b.before_revision,
            })
            .collect(),
        after: outcome
            .bricks
            .iter()
            .map(|b| BrickRevision {
                volume: plan.volume,
                coord: b.coord,
                revision: b.after_revision,
            })
            .collect(),
        ops: plan
            .writes
            .iter()
            .map(|w| TopologyOp::CellRun {
                volume: plan.volume,
                start: w.cell,
                len: 1,
                material: MaterialId::AIR,
            })
            .collect(),
        result_hashes: vec![VolumeHash {
            volume: plan.volume,
            hash: crate::world::volume_topology_hash_for(&candidate, CanonicalOwner::Body(id)),
        }],
    };
    topology.validate()?;
    if !crate::replication::inline_wire_fits(&topology) {
        // Unreachable under the 256-cell cap, but never panic the server thread.
        return Err(crate::replication::ReplicationError::SplitTooLarge {
            volume: plan.volume.get(),
            blob_bytes: 0,
            cap: spall_protocol::limits::MAX_SPLIT_BASELINE_BLOB,
        }
        .into());
    }
    // Publish after every fallible operation. The generic retirement path also
    // removes the dormant physics slot; replay/client prune the emptied volume.
    *world.registry_mut() = reg;
    world.retire_empty_volume(plan.volume);
    journal.append(JournalEntry {
        seq: journal_seq,
        transaction: topology.clone(),
        participants: Vec::new(),
        bulk_baseline: None,
    });
    Ok(Committed {
        transaction: transaction_id,
        journal_seq,
        topology,
        children: Vec::new(),
        bumped_epoch: false,
        bulk_baseline: None,
    })
}
