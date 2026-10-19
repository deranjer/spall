//! Replica-side creature ownership store.
//!
//! Applies the ordered `CreatureLifecycle` operations of committed topology
//! transactions through the same protocol reducer the server uses, so a replica
//! converges on the authoritative portable records without any physics or
//! rendering dependency. Live motion arrives separately; this store holds the
//! ownership/topology records and the at-birth motion they were published with.
//!
//! The replica cannot know the server's exact shared volume counter (retired
//! volume ids leave gaps), so it enforces what it can observe: creature ids and
//! source volume ids must be monotone and unique. Exact allocator continuity is
//! the server's invariant and is checked by the reducer there.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use spall_core::{
    CREATURE_ALLOCATOR_SCHEMA, CreatureAllocatorSnapshotV1, CreatureKey, VolumeId, WorldId,
};
use spall_protocol::{
    CreatureLifecycleCommitV4, CreatureLifecycleContextV1, CreatureLifecycleOperationV4,
    CreatureOwnershipError, CreatureOwnershipRecordV4, CreatureStateError, OwnedCreatureCatalog,
    TopologyOp, TopologyTransaction,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReplicaCreatureError {
    #[error("creature catalog: {0}")]
    Catalog(#[from] CreatureStateError),
    #[error("creature ownership: {0}")]
    Ownership(#[from] CreatureOwnershipError),
    #[error("creature belongs to a different world or epoch")]
    World,
    #[error("creature or source volume id is not newer than replica state")]
    NotMonotone,
    #[error("bulk lifecycle payload delivery is not supported yet")]
    BulkUnsupported,
}

/// One received root pose, stamped with its arrival time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MotionSample {
    at: Instant,
    server_tick: u64,
    translation_m: [f64; 3],
    rotation_xyzw: [f32; 4],
    linear_velocity_m_s: [f32; 3],
    locomotion_tag: u8,
    clip_phase_tick: u64,
}

#[derive(Debug, Clone, Copy)]
struct MotionTrack {
    previous: Option<MotionSample>,
    current: MotionSample,
}

/// A creature's presented pose at a render instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreaturePresentation {
    pub translation_m: [f64; 3],
    pub rotation_xyzw: [f32; 4],
    pub locomotion_tag: u8,
    /// Clip phase in ticks at the render instant.
    pub clip_phase_tick: f64,
}

/// How far behind the newest received sample creatures are drawn, so two
/// samples usually bracket the render time at the 20 Hz send rate.
pub const CREATURE_RENDER_DELAY: Duration = Duration::from_millis(110);
/// Longest a creature is extrapolated along its velocity past its newest sample.
const MAX_EXTRAPOLATION: Duration = Duration::from_millis(150);

#[derive(Debug, Clone)]
pub struct ReplicaCreatures {
    catalog: Arc<OwnedCreatureCatalog>,
    identity: Option<(WorldId, u64)>,
    next_creature_id: u64,
    next_volume_floor: u64,
    records: BTreeMap<CreatureKey, CreatureOwnershipRecordV4>,
    motion: BTreeMap<u64, MotionTrack>,
    motion_applied: u64,
    revision: u64,
}

impl ReplicaCreatures {
    pub fn new(catalog: Arc<OwnedCreatureCatalog>) -> Self {
        Self {
            catalog,
            identity: None,
            next_creature_id: 1,
            next_volume_floor: 1,
            records: BTreeMap::new(),
            motion: BTreeMap::new(),
            motion_applied: 0,
            revision: 0,
        }
    }

    /// Drops every record and identity, keeping the catalog. A full baseline
    /// replaces the world, so previous creatures must not survive it.
    pub fn reset(&mut self) {
        self.identity = None;
        self.next_creature_id = 1;
        self.next_volume_floor = 1;
        self.records.clear();
        self.motion.clear();
        self.revision = self.revision.saturating_add(1);
    }

    /// Pins the world identity before any record arrives (baseline/handshake).
    pub fn bind_world(&mut self, world_id: WorldId, epoch: u64) {
        self.identity = Some((world_id, epoch));
    }

    pub fn records(&self) -> impl Iterator<Item = &CreatureOwnershipRecordV4> {
        self.records.values()
    }

    pub fn get(&self, key: &CreatureKey) -> Option<&CreatureOwnershipRecordV4> {
        self.records.get(key)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Bumps on every applied lifecycle change so renderers can poll cheaply.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn has_lifecycle_ops(tx: &TopologyTransaction) -> bool {
        tx.ops.iter().any(|op| {
            matches!(
                op,
                TopologyOp::CreatureLifecycle { .. } | TopologyOp::CreatureLifecycleBulk { .. }
            )
        })
    }

    /// Applies a presentation frame. Entries for creatures this replica does not
    /// hold, from another world epoch, or not newer than the one already applied
    /// are ignored. Returns how many entries were applied.
    pub fn apply_motion(
        &mut self,
        frame: &spall_protocol::CreatureMotionFrame,
        now: Instant,
    ) -> usize {
        if let Some((_, epoch)) = self.identity
            && frame.world_epoch != epoch
        {
            return 0;
        }
        let mut applied = 0;
        for entry in &frame.entries {
            let Some(record) = self
                .records
                .values()
                .find(|r| r.state.key.creature_id.get() == entry.creature_id)
            else {
                continue;
            };
            if record.state.lifecycle != spall_protocol::CreatureLifecycleState::Alive {
                continue;
            }
            let sample = MotionSample {
                at: now,
                server_tick: frame.server_tick,
                translation_m: entry.translation_m,
                rotation_xyzw: entry.rotation_xyzw,
                linear_velocity_m_s: entry.linear_velocity_m_s,
                locomotion_tag: entry.locomotion_tag,
                clip_phase_tick: entry.clip_phase_tick,
            };
            match self.motion.get_mut(&entry.creature_id) {
                Some(track) if frame.server_tick <= track.current.server_tick => continue,
                Some(track) => {
                    track.previous = Some(track.current);
                    track.current = sample;
                }
                None => {
                    self.motion.insert(
                        entry.creature_id,
                        MotionTrack {
                            previous: None,
                            current: sample,
                        },
                    );
                }
            }
            applied += 1;
        }
        self.motion_applied += applied as u64;
        applied
    }

    /// The pose to draw for `record` at `now`: interpolated between the two
    /// newest samples, held or briefly extrapolated past the newest, and the
    /// record's own root when no motion has arrived yet.
    pub fn presentation(
        &self,
        record: &CreatureOwnershipRecordV4,
        now: Instant,
    ) -> CreaturePresentation {
        let state = &record.state;
        let fallback = CreaturePresentation {
            translation_m: state.root_translation_m,
            rotation_xyzw: state.root_rotation_xyzw,
            locomotion_tag: state.locomotion_tag,
            clip_phase_tick: state.clip_phase_tick as f64,
        };
        let Some(track) = self.motion.get(&state.key.creature_id.get()) else {
            return fallback;
        };
        let render_at = now.checked_sub(CREATURE_RENDER_DELAY).unwrap_or(now);
        let current = track.current;
        let phase_at = |sample: &MotionSample, at: Instant| {
            sample.clip_phase_tick as f64
                + at.saturating_duration_since(sample.at).as_secs_f64() * 60.0
        };
        if let Some(previous) = track.previous
            && render_at < current.at
        {
            let span = current
                .at
                .saturating_duration_since(previous.at)
                .as_secs_f64();
            let t = if span <= 0.0 {
                1.0
            } else {
                (render_at
                    .saturating_duration_since(previous.at)
                    .as_secs_f64()
                    / span)
                    .clamp(0.0, 1.0)
            };
            let lerp = |a: f64, b: f64| a + (b - a) * t;
            return CreaturePresentation {
                translation_m: [
                    lerp(previous.translation_m[0], current.translation_m[0]),
                    lerp(previous.translation_m[1], current.translation_m[1]),
                    lerp(previous.translation_m[2], current.translation_m[2]),
                ],
                rotation_xyzw: nlerp(previous.rotation_xyzw, current.rotation_xyzw, t as f32),
                locomotion_tag: current.locomotion_tag,
                clip_phase_tick: phase_at(&previous, render_at),
            };
        }
        let ahead = render_at
            .saturating_duration_since(current.at)
            .min(MAX_EXTRAPOLATION)
            .as_secs_f64();
        CreaturePresentation {
            translation_m: [
                current.translation_m[0] + f64::from(current.linear_velocity_m_s[0]) * ahead,
                current.translation_m[1] + f64::from(current.linear_velocity_m_s[1]) * ahead,
                current.translation_m[2] + f64::from(current.linear_velocity_m_s[2]) * ahead,
            ],
            rotation_xyzw: current.rotation_xyzw,
            locomotion_tag: current.locomotion_tag,
            clip_phase_tick: phase_at(&current, render_at),
        }
    }

    /// Largest horizontal distance any creature is presented from where its
    /// ownership record placed it. A rough "did anything move" measure.
    pub fn max_displacement_m(&self, now: Instant) -> f64 {
        self.records
            .values()
            .map(|record| {
                let presented = self.presentation(record, now).translation_m;
                let root = record.state.root_translation_m;
                (presented[0] - root[0]).hypot(presented[2] - root[2])
            })
            .fold(0.0, f64::max)
    }

    /// Motion entries applied so far.
    pub fn motion_entries_applied(&self) -> u64 {
        self.motion_applied
    }

    /// Installs one server-captured record at join time. Idempotent: an equal
    /// or newer record is kept, an older sync never rewinds a record the
    /// ordered transaction stream already advanced.
    pub fn install_sync(
        &mut self,
        sync: &spall_protocol::CreatureSync,
    ) -> Result<(), ReplicaCreatureError> {
        let key = sync.record.state.key;
        match self.identity {
            Some((world, epoch)) => {
                if key.world_id != world || key.world_epoch.get() != epoch {
                    return Err(ReplicaCreatureError::World);
                }
            }
            None => self.identity = Some((key.world_id, key.world_epoch.get())),
        }
        self.catalog
            .with_view(|catalog| sync.record.validate(catalog))??;
        if let Some(existing) = self.records.get(&key) {
            if existing.state.state_revision >= sync.record.state.state_revision {
                return Ok(());
            }
        } else if self.records.values().any(|other| {
            other.parts.iter().any(|part| {
                sync.record
                    .parts
                    .iter()
                    .any(|new| new.source_volume_id == part.source_volume_id)
            })
        }) {
            return Err(ReplicaCreatureError::NotMonotone);
        }
        self.next_creature_id = self.next_creature_id.max(sync.allocator.next_creature_id);
        self.next_volume_floor = self.next_volume_floor.max(sync.next_volume_id);
        self.records.insert(key, sync.record.clone());
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    /// Applies every lifecycle op of an actor-only transaction, all or nothing.
    /// `volume_floor` is the lowest source volume id the replica's geometry has
    /// not already used.
    pub fn apply_transaction(
        &mut self,
        tx: &TopologyTransaction,
        volume_floor: u64,
    ) -> Result<(), ReplicaCreatureError> {
        let mut staged: BTreeMap<CreatureKey, CreatureOwnershipRecordV4> = BTreeMap::new();
        let mut identity = self.identity;
        let mut next_creature_id = self.next_creature_id;
        let mut next_volume_floor = self.next_volume_floor.max(volume_floor);
        let mut applied = 0_u64;
        for op in &tx.ops {
            let commit = match op {
                TopologyOp::CreatureLifecycle { commit } => commit,
                TopologyOp::CreatureLifecycleBulk { .. } => {
                    return Err(ReplicaCreatureError::BulkUnsupported);
                }
                _ => continue,
            };
            let (key, first_volume) = match &commit.change.operation {
                CreatureLifecycleOperationV4::Birth { after }
                | CreatureLifecycleOperationV4::Death { after } => (
                    after.state.key,
                    after.parts.first().map(|part| part.source_volume_id),
                ),
            };
            match identity {
                Some((world, epoch)) => {
                    if key.world_id != world || key.world_epoch.get() != epoch {
                        return Err(ReplicaCreatureError::World);
                    }
                }
                None => identity = Some((key.world_id, key.world_epoch.get())),
            }
            let is_birth = matches!(
                commit.change.operation,
                CreatureLifecycleOperationV4::Birth { .. }
            );
            let current = staged.get(&key).or_else(|| self.records.get(&key));
            let allocator_next = if is_birth {
                if key.creature_id.get() < next_creature_id {
                    return Err(ReplicaCreatureError::NotMonotone);
                }
                key.creature_id.get()
            } else {
                next_creature_id
            };
            let context_volume = if is_birth {
                let start = first_volume.map_or(0, VolumeId::get);
                if start < next_volume_floor {
                    return Err(ReplicaCreatureError::NotMonotone);
                }
                start
            } else {
                let used = current
                    .map(|record| {
                        record
                            .parts
                            .iter()
                            .map(|part| part.source_volume_id.get())
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                next_volume_floor.max(used.saturating_add(1))
            };
            let existing_keys: Vec<CreatureKey> =
                self.records.keys().chain(staged.keys()).copied().collect();
            let existing_owner_keys: Vec<u128> = self
                .records
                .values()
                .chain(staged.values())
                .map(|record| record.owner_key)
                .collect();
            let existing_source_volumes: Vec<VolumeId> = self
                .records
                .values()
                .chain(staged.values())
                .flat_map(|record| record.parts.iter().map(|part| part.source_volume_id))
                .collect();
            let candidate = self.catalog.with_view(|catalog| {
                commit_reduce(
                    commit,
                    current,
                    CreatureLifecycleContextV1 {
                        transaction_id: tx.transaction_id,
                        operation_ordinal: commit.change.operation_ordinal,
                        replay_receipt: None,
                        allocator: CreatureAllocatorSnapshotV1 {
                            schema: CREATURE_ALLOCATOR_SCHEMA,
                            world_id: key.world_id,
                            world_epoch: key.world_epoch.get(),
                            next_creature_id: allocator_next,
                        },
                        next_volume_id: context_volume,
                        death_witness: None,
                        existing_keys: &existing_keys,
                        existing_owner_keys: &existing_owner_keys,
                        existing_source_volumes: &existing_source_volumes,
                    },
                    catalog,
                )
            })??;
            next_creature_id = next_creature_id.max(candidate.next_creature_id);
            next_volume_floor = next_volume_floor.max(candidate.next_volume_id);
            staged.insert(key, candidate.record);
            applied += 1;
        }
        // Everything validated: publish together.
        self.identity = identity;
        self.next_creature_id = next_creature_id;
        self.next_volume_floor = next_volume_floor;
        self.records.extend(staged);
        self.revision = self.revision.saturating_add(applied);
        Ok(())
    }
}

fn nlerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    // Take the shorter arc, then renormalize.
    let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
    let sign = if dot < 0.0 { -1.0 } else { 1.0 };
    let mut out = [0.0; 4];
    for i in 0..4 {
        out[i] = a[i] + (sign * b[i] - a[i]) * t;
    }
    let length = out.iter().map(|v| v * v).sum::<f32>().sqrt();
    if length <= f32::EPSILON {
        return a;
    }
    out.map(|v| v / length)
}

fn commit_reduce(
    commit: &CreatureLifecycleCommitV4,
    current: Option<&CreatureOwnershipRecordV4>,
    context: CreatureLifecycleContextV1<'_>,
    catalog: &spall_protocol::CreatureValidationCatalog<'_>,
) -> Result<spall_protocol::CreatureLifecycleCandidateV1, CreatureOwnershipError> {
    commit.validate_and_reduce(current, context, catalog)
}

#[cfg(test)]
#[path = "replica_creatures_tests.rs"]
mod tests;
