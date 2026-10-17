//! Owner-thread creature identity host and bounded scheduling proposal seam.
//!
//! This is deliberately not a lifecycle/spawn path. Registry insertion remains
//! crate-private until source ownership can be transferred atomically.
use std::collections::{BTreeMap, BTreeSet};

use spall_core::{
    CreatureAllocator, CreatureAllocatorSnapshotV1, CreatureId, CreatureKey, IdError, WorldEpoch,
    WorldId,
};
use spall_protocol::CreatureStateV1;
#[cfg(test)]
use spall_protocol::MAX_CREATURE_SOURCE_PARTS;

pub const MAX_CREATURE_HOST_RECORDS: usize = 1_024;
pub const MAX_CREATURE_SCHEDULE_SNAPSHOTS: usize = 64;
pub const MAX_CREATURE_SCHEDULE_BINDINGS: usize = 1_024;
pub const MAX_CREATURE_SCHEDULE_INTENTS: usize = 64;
#[cfg(test)]
const MAX_CREATURE_HOST_BINDINGS: usize = MAX_CREATURE_HOST_RECORDS * MAX_CREATURE_SOURCE_PARTS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreatureHostInitError {
    #[error("creature host is already initialized")]
    AlreadyInitialized,
    #[error(transparent)]
    Identity(#[from] IdError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatureScheduleContext {
    pub tick: u64,
    pub world_id: WorldId,
    pub world_epoch: WorldEpoch,
    pub terrain_revision: u64,
    pub water_frame_revision: Option<u64>,
    pub schedule_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatureScheduleToken {
    pub key: CreatureKey,
    pub state_revision: u64,
    pub topology_revision: u64,
    pub pose_epoch: u64,
    pub clip_epoch: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreatureScheduleSnapshot {
    pub token: CreatureScheduleToken,
    pub state: CreatureStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreatureIntentKind {
    AdvanceClipPhase { phase_tick: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatureIntent {
    pub context: CreatureScheduleContext,
    pub token: CreatureScheduleToken,
    pub kind: CreatureIntentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatureScheduleError {
    Capacity,
    InvalidBatch,
    Stale,
    RulesRejected,
    CounterExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatureRuleError {
    Capacity,
    Rejected,
}

/// Statically linked game policy receives only a bounded immutable batch.
/// It cannot mutate Simulation or registry state through this interface.
pub trait CreatureSchedulingRules: Send + Sync {
    fn propose(
        &self,
        context: CreatureScheduleContext,
        snapshots: &[CreatureScheduleSnapshot],
        output: &mut CreatureIntentWriter,
    ) -> Result<(), CreatureRuleError>;
}

pub struct CreatureIntentWriter {
    intents: Vec<CreatureIntent>,
    overflowed: bool,
}

impl CreatureIntentWriter {
    fn new() -> Self {
        Self {
            intents: Vec::with_capacity(MAX_CREATURE_SCHEDULE_INTENTS),
            overflowed: false,
        }
    }

    pub fn push(&mut self, intent: CreatureIntent) -> Result<(), CreatureRuleError> {
        if self.intents.len() >= MAX_CREATURE_SCHEDULE_INTENTS {
            self.overflowed = true;
            return Err(CreatureRuleError::Capacity);
        }
        self.intents.push(intent);
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreatureScheduleReport {
    pub snapshot_count: usize,
    pub binding_count: usize,
    pub accepted_intents: Vec<CreatureIntent>,
    pub rejected: Option<CreatureScheduleError>,
}

#[cfg_attr(test, derive(Clone))]
pub(crate) struct CreatureHost {
    allocator: CreatureAllocator,
    records: BTreeMap<CreatureId, CreatureStateV1>,
    schedule_cursor: Option<CreatureId>,
    schedule_epoch: u64,
}

impl CreatureHost {
    pub(crate) fn new(world_id: WorldId) -> Result<Self, IdError> {
        Ok(Self {
            allocator: CreatureAllocator::new(world_id)?,
            records: BTreeMap::new(),
            schedule_cursor: None,
            schedule_epoch: 1,
        })
    }

    pub(crate) fn from_allocator_snapshot(
        snapshot: CreatureAllocatorSnapshotV1,
    ) -> Result<Self, IdError> {
        Ok(Self {
            allocator: CreatureAllocator::restore(snapshot)?,
            records: BTreeMap::new(),
            schedule_cursor: None,
            schedule_epoch: 1,
        })
    }

    pub(crate) fn allocator_snapshot(&self) -> CreatureAllocatorSnapshotV1 {
        self.allocator.snapshot()
    }

    pub(crate) fn preview_reset_epoch(&self) -> Result<WorldEpoch, IdError> {
        self.schedule_epoch
            .checked_add(1)
            .ok_or(IdError::Exhausted)?;
        self.allocator.world_epoch().checked_next()
    }

    pub(crate) fn reset_epoch(&mut self) -> Result<(), IdError> {
        let next_world_epoch = self.allocator.world_epoch().checked_next()?;
        let next_schedule_epoch = self
            .schedule_epoch
            .checked_add(1)
            .ok_or(IdError::Exhausted)?;
        self.allocator.advance_world_epoch()?;
        debug_assert_eq!(self.allocator.world_epoch(), next_world_epoch);
        self.records.clear();
        self.schedule_cursor = None;
        self.schedule_epoch = next_schedule_epoch;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn world_id(&self) -> WorldId {
        self.allocator.world_id()
    }

    #[cfg(test)]
    pub(crate) fn world_epoch(&self) -> WorldEpoch {
        self.allocator.world_epoch()
    }

    /// Crate-private until the atomic source ownership slice introduces the
    /// production registration transaction.
    #[cfg(test)]
    pub(crate) fn register_validated(
        &mut self,
        state: CreatureStateV1,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureScheduleError> {
        state
            .validate(catalog)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        if state.key.world_id != self.allocator.world_id()
            || state.key.world_epoch != self.allocator.world_epoch()
            || state.key.creature_id.get() != self.allocator.next_creature_id()
            || self.records.len() >= MAX_CREATURE_HOST_RECORDS
            || self.records.contains_key(&state.key.creature_id)
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let binding_count = self
            .records
            .values()
            .try_fold(state.source_bindings.len(), |count, record| {
                count.checked_add(record.source_bindings.len())
            });
        if binding_count.is_none_or(|count| count > MAX_CREATURE_HOST_BINDINGS) {
            return Err(CreatureScheduleError::Capacity);
        }
        let mut volumes = BTreeSet::new();
        for existing in self.records.values() {
            volumes.extend(
                existing
                    .source_bindings
                    .iter()
                    .map(|binding| binding.source_volume_id),
            );
        }
        if state
            .source_bindings
            .iter()
            .any(|binding| !volumes.insert(binding.source_volume_id))
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let allocated = self
            .allocator
            .allocate()
            .map_err(|_| CreatureScheduleError::CounterExhausted)?;
        if allocated != state.key {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        self.records.insert(state.key.creature_id, state);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn replace_record_for_lifecycle_test(
        &mut self,
        state: CreatureStateV1,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureScheduleError> {
        state
            .validate(catalog)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        let Some(current) = self.records.get(&state.key.creature_id) else {
            return Err(CreatureScheduleError::InvalidBatch);
        };
        if current.key != state.key
            || current.lifecycle != spall_protocol::CreatureLifecycleState::Alive
            || state.lifecycle != spall_protocol::CreatureLifecycleState::Corpse
            || state.state_revision <= current.state_revision
            || state.source_bindings != current.source_bindings
            || state.aggregate_topology_revision != current.aggregate_topology_revision
        {
            return Err(CreatureScheduleError::Stale);
        }
        self.records.insert(state.key.creature_id, state);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn record(&self, id: CreatureId) -> Option<&CreatureStateV1> {
        self.records.get(&id)
    }

    pub(crate) fn schedule(
        &mut self,
        tick: u64,
        terrain_revision: u64,
        water_frame_revision: Option<u64>,
        rules: &dyn CreatureSchedulingRules,
    ) -> CreatureScheduleReport {
        let mut report = CreatureScheduleReport::default();
        if self.records.is_empty() {
            return report;
        }
        let keys = if let Some(cursor) = self.schedule_cursor {
            self.records
                .range((
                    std::ops::Bound::Excluded(cursor),
                    std::ops::Bound::Unbounded,
                ))
                .map(|(id, _)| *id)
                .chain(self.records.range(..=cursor).map(|(id, _)| *id))
                .take(MAX_CREATURE_SCHEDULE_SNAPSHOTS)
                .collect::<Vec<_>>()
        } else {
            self.records
                .keys()
                .take(MAX_CREATURE_SCHEDULE_SNAPSHOTS)
                .copied()
                .collect::<Vec<_>>()
        };
        let mut snapshots = Vec::with_capacity(keys.len());
        for id in &keys {
            let state = &self.records[id];
            let next_binding_count = report
                .binding_count
                .checked_add(state.source_bindings.len());
            if next_binding_count.is_none_or(|count| count > MAX_CREATURE_SCHEDULE_BINDINGS) {
                break;
            }
            report.binding_count = next_binding_count.expect("checked above");
            snapshots.push(CreatureScheduleSnapshot {
                token: token_for(state),
                state: state.clone(),
            });
        }
        report.snapshot_count = snapshots.len();
        if snapshots.is_empty() {
            return report;
        }
        let context = CreatureScheduleContext {
            tick,
            world_id: self.allocator.world_id(),
            world_epoch: self.allocator.world_epoch(),
            terrain_revision,
            water_frame_revision,
            schedule_epoch: self.schedule_epoch,
        };
        let mut writer = CreatureIntentWriter::new();
        if rules.propose(context, &snapshots, &mut writer).is_err() || writer.overflowed {
            report.rejected = Some(CreatureScheduleError::RulesRejected);
            return report;
        }
        if let Err(error) =
            validate_intent_batch(&self.records, context, &snapshots, &writer.intents)
        {
            report.rejected = Some(error);
            return report;
        }
        let Some(next_epoch) = self.schedule_epoch.checked_add(1) else {
            report.rejected = Some(CreatureScheduleError::CounterExhausted);
            return report;
        };
        self.schedule_cursor = keys.get(snapshots.len().saturating_sub(1)).copied();
        self.schedule_epoch = next_epoch;
        report.accepted_intents = writer.intents;
        report
    }
}

fn token_for(state: &CreatureStateV1) -> CreatureScheduleToken {
    CreatureScheduleToken {
        key: state.key,
        state_revision: state.state_revision,
        topology_revision: state.aggregate_topology_revision,
        pose_epoch: state.pose_epoch,
        clip_epoch: state.clip_epoch,
    }
}

fn validate_intent_batch(
    records: &BTreeMap<CreatureId, CreatureStateV1>,
    context: CreatureScheduleContext,
    snapshots: &[CreatureScheduleSnapshot],
    intents: &[CreatureIntent],
) -> Result<(), CreatureScheduleError> {
    if intents.len() > MAX_CREATURE_SCHEDULE_INTENTS || intents.len() > snapshots.len() {
        return Err(CreatureScheduleError::Capacity);
    }
    let snapshots = snapshots
        .iter()
        .map(|snapshot| (snapshot.token.key.creature_id, snapshot))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for intent in intents {
        if intent.context != context || !seen.insert(intent.token.key.creature_id) {
            return Err(CreatureScheduleError::Stale);
        }
        let Some(snapshot) = snapshots.get(&intent.token.key.creature_id) else {
            return Err(CreatureScheduleError::Stale);
        };
        let Some(current) = records.get(&intent.token.key.creature_id) else {
            return Err(CreatureScheduleError::Stale);
        };
        if intent.token != snapshot.token
            || intent.token != token_for(current)
            || intent.token.key != current.key
        {
            return Err(CreatureScheduleError::Stale);
        }
        match intent.kind {
            CreatureIntentKind::AdvanceClipPhase { phase_tick }
                if current.lifecycle == spall_protocol::CreatureLifecycleState::Alive
                    && current.clip_mode == spall_protocol::CreatureClipMode::Playing
                    && current.clip_phase_tick.checked_add(1) == Some(phase_tick) => {}
            _ => return Err(CreatureScheduleError::InvalidBatch),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use spall_core::{
        CreatureGeneration, CreatureId, CreatureKey, MaterialDef, MaterialFlags, MaterialId,
        MaterialManifest, RenderProps, SimProps, VolumeId,
    };
    use spall_protocol::{
        CreatureAssetCatalogEntry, CreatureAssetIdentity, CreatureClipMode, CreatureLifecycleState,
        CreatureMaterialMappingEntry, CreaturePartBinding, CreatureSpeciesCatalogEntry,
        CreatureStateV1, CreatureValidationCatalog, content_manifest_hash,
        creature_material_mapping_hash,
    };

    fn manifest() -> MaterialManifest {
        MaterialManifest::validated(vec![MaterialDef {
            id: MaterialId(1),
            name: "fur".into(),
            render: RenderProps {
                albedo: [0.5; 3],
                roughness: 0.5,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 1.0,
                friction: 0.5,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags(0),
            },
        }])
        .unwrap()
    }

    pub(crate) struct FixtureCatalog {
        materials: MaterialManifest,
        assets: [CreatureAssetCatalogEntry<'static>; 1],
        species: [CreatureSpeciesCatalogEntry<'static>; 1],
        mapping: [CreatureMaterialMappingEntry<'static>; 1],
    }

    impl FixtureCatalog {
        pub(crate) fn new() -> Self {
            Self::with_materials(manifest())
        }

        pub(crate) fn with_materials(materials: MaterialManifest) -> Self {
            Self {
                materials,
                assets: [CreatureAssetCatalogEntry {
                    uuid: [8; 16],
                    logical_hash: spall_protocol::Hash32([9; 32]),
                    part_ids: &[1],
                    clip_names: &["idle"],
                    portable_material_keys: &["fur"],
                }],
                species: [CreatureSpeciesCatalogEntry {
                    key: "deer",
                    version: 1,
                    enabled: true,
                    asset: CreatureAssetIdentity {
                        uuid: [8; 16],
                        logical_hash: spall_protocol::Hash32([9; 32]),
                    },
                }],
                mapping: [CreatureMaterialMappingEntry {
                    portable_key: "fur",
                    material_id: MaterialId(1),
                }],
            }
        }

        pub(crate) fn view(&self) -> CreatureValidationCatalog<'_> {
            CreatureValidationCatalog {
                assets: &self.assets,
                species: &self.species,
                materials: &self.materials,
                material_mapping_version: 1,
                material_mapping_hash: creature_material_mapping_hash(&self.mapping).unwrap(),
                material_mapping: &self.mapping,
            }
        }

        pub(crate) fn state(&self, id: u64, volume: u64) -> CreatureStateV1 {
            CreatureStateV1 {
                schema: spall_protocol::CREATURE_STATE_SCHEMA,
                key: CreatureKey::new(
                    WorldId::from_u128(10),
                    WorldEpoch::INITIAL,
                    CreatureId::new(id).unwrap(),
                    CreatureGeneration::INITIAL,
                )
                .unwrap(),
                state_revision: 1,
                lifecycle: CreatureLifecycleState::Alive,
                species_key: "deer".into(),
                species_version: 1,
                asset: CreatureAssetIdentity {
                    uuid: [8; 16],
                    logical_hash: spall_protocol::Hash32([9; 32]),
                },
                material_manifest_hash: content_manifest_hash(&self.materials),
                material_mapping_version: 1,
                material_mapping_hash: creature_material_mapping_hash(&self.mapping).unwrap(),
                clip_name: "idle".into(),
                clip_mode: CreatureClipMode::Playing,
                clip_phase_tick: 4,
                clip_subtick_remainder: 0,
                clip_epoch: 1,
                source_bindings: vec![CreaturePartBinding {
                    part_id: 1,
                    source_volume_id: VolumeId::new(volume).unwrap(),
                    source_revision: 1,
                }],
                aggregate_topology_revision: 2,
                pose_epoch: 3,
                root_translation_m: [0.0; 3],
                root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                linear_velocity_m_s: [0.0; 3],
                angular_velocity_rad_s: [0.0; 3],
                locomotion_tag: 0,
            }
        }
    }

    struct AdvanceAll;

    impl CreatureSchedulingRules for AdvanceAll {
        fn propose(
            &self,
            context: CreatureScheduleContext,
            snapshots: &[CreatureScheduleSnapshot],
            output: &mut CreatureIntentWriter,
        ) -> Result<(), CreatureRuleError> {
            for snapshot in snapshots {
                output.push(CreatureIntent {
                    context,
                    token: snapshot.token,
                    kind: CreatureIntentKind::AdvanceClipPhase {
                        phase_tick: snapshot.state.clip_phase_tick + 1,
                    },
                })?;
            }
            Ok(())
        }
    }

    struct StaleOne;

    impl CreatureSchedulingRules for StaleOne {
        fn propose(
            &self,
            context: CreatureScheduleContext,
            snapshots: &[CreatureScheduleSnapshot],
            output: &mut CreatureIntentWriter,
        ) -> Result<(), CreatureRuleError> {
            for (index, snapshot) in snapshots.iter().enumerate() {
                let mut token = snapshot.token;
                if index == 1 {
                    token.pose_epoch += 1;
                }
                output.push(CreatureIntent {
                    context,
                    token,
                    kind: CreatureIntentKind::AdvanceClipPhase {
                        phase_tick: snapshot.state.clip_phase_tick + 1,
                    },
                })?;
            }
            Ok(())
        }
    }

    struct Overflow;

    impl CreatureSchedulingRules for Overflow {
        fn propose(
            &self,
            context: CreatureScheduleContext,
            snapshots: &[CreatureScheduleSnapshot],
            output: &mut CreatureIntentWriter,
        ) -> Result<(), CreatureRuleError> {
            for _ in 0..=MAX_CREATURE_SCHEDULE_INTENTS {
                output.push(CreatureIntent {
                    context,
                    token: snapshots[0].token,
                    kind: CreatureIntentKind::AdvanceClipPhase { phase_tick: 5 },
                })?;
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Tamper {
        Generation,
        StateRevision,
        TopologyRevision,
        PoseEpoch,
        ClipEpoch,
        ForeignKey,
        Context,
        Duplicate,
    }

    struct TamperedBatch(Tamper);

    impl CreatureSchedulingRules for TamperedBatch {
        fn propose(
            &self,
            context: CreatureScheduleContext,
            snapshots: &[CreatureScheduleSnapshot],
            output: &mut CreatureIntentWriter,
        ) -> Result<(), CreatureRuleError> {
            for (index, snapshot) in snapshots.iter().enumerate() {
                if matches!(self.0, Tamper::Duplicate) && index > 0 {
                    continue;
                }
                let mut context = context;
                let mut token = snapshot.token;
                match self.0 {
                    Tamper::Generation => {
                        token.key.generation = spall_core::CreatureGeneration::new(2).unwrap();
                    }
                    Tamper::StateRevision => token.state_revision += 1,
                    Tamper::TopologyRevision => token.topology_revision += 1,
                    Tamper::PoseEpoch => token.pose_epoch += 1,
                    Tamper::ClipEpoch => token.clip_epoch += 1,
                    Tamper::ForeignKey => {
                        token.key.creature_id = CreatureId::new(999).unwrap();
                    }
                    Tamper::Context => context.tick += 1,
                    Tamper::Duplicate => {}
                }
                let intent = CreatureIntent {
                    context,
                    token,
                    kind: CreatureIntentKind::AdvanceClipPhase {
                        phase_tick: snapshot.state.clip_phase_tick + 1,
                    },
                };
                output.push(intent.clone())?;
                if matches!(self.0, Tamper::Duplicate) {
                    output.push(intent)?;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn scheduler_is_deterministic_bounded_and_round_robin() {
        let catalog = FixtureCatalog::new();
        let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
        for id in 1..=70 {
            host.register_validated(catalog.state(id, 1000 + id), &catalog.view())
                .unwrap();
        }
        let first = host.schedule(1, 9, Some(3), &AdvanceAll);
        assert_eq!(first.snapshot_count, MAX_CREATURE_SCHEDULE_SNAPSHOTS);
        assert_eq!(first.accepted_intents.len(), MAX_CREATURE_SCHEDULE_INTENTS);
        assert_eq!(first.accepted_intents[0].token.key.creature_id.get(), 1);
        assert_eq!(
            first
                .accepted_intents
                .last()
                .unwrap()
                .token
                .key
                .creature_id
                .get(),
            64
        );
        let second = host.schedule(2, 9, Some(3), &AdvanceAll);
        assert_eq!(second.snapshot_count, MAX_CREATURE_SCHEDULE_SNAPSHOTS);
        assert_eq!(second.accepted_intents[0].token.key.creature_id.get(), 65);
        assert_eq!(second.accepted_intents[5].token.key.creature_id.get(), 70);
        assert_eq!(second.accepted_intents[6].token.key.creature_id.get(), 1);
    }

    #[test]
    fn frozen_corpses_do_not_accept_clip_phase_intents() {
        let catalog = FixtureCatalog::new();
        let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
        let mut corpse = catalog.state(1, 1001);
        corpse.lifecycle = spall_protocol::CreatureLifecycleState::Corpse;
        corpse.clip_mode = spall_protocol::CreatureClipMode::Frozen;
        host.register_validated(corpse, &catalog.view()).unwrap();

        let report = host.schedule(1, 9, Some(3), &AdvanceAll);
        assert_eq!(report.rejected, Some(CreatureScheduleError::InvalidBatch));
        assert!(report.accepted_intents.is_empty());
    }

    #[test]
    fn stale_or_oversized_intent_batches_reject_all_without_actor_mutation() {
        let catalog = FixtureCatalog::new();
        let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
        for id in 1..=2 {
            host.register_validated(catalog.state(id, 1000 + id), &catalog.view())
                .unwrap();
        }
        let before = (
            host.record(CreatureId::new(1).unwrap()).unwrap().clone(),
            host.record(CreatureId::new(2).unwrap()).unwrap().clone(),
        );
        let stale = host.schedule(1, 9, None, &StaleOne);
        assert_eq!(stale.rejected, Some(CreatureScheduleError::Stale));
        assert!(stale.accepted_intents.is_empty());
        let overflow = host.schedule(1, 9, None, &Overflow);
        assert_eq!(
            overflow.rejected,
            Some(CreatureScheduleError::RulesRejected)
        );
        assert!(overflow.accepted_intents.is_empty());
        assert_eq!(host.record(CreatureId::new(1).unwrap()).unwrap(), &before.0);
        assert_eq!(host.record(CreatureId::new(2).unwrap()).unwrap(), &before.1);
    }

    #[test]
    fn every_token_context_and_duplicate_intent_error_rejects_the_whole_batch() {
        let catalog = FixtureCatalog::new();
        let mutations = [
            Tamper::Generation,
            Tamper::StateRevision,
            Tamper::TopologyRevision,
            Tamper::PoseEpoch,
            Tamper::ClipEpoch,
            Tamper::ForeignKey,
            Tamper::Context,
            Tamper::Duplicate,
        ];
        for mutation in mutations {
            let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
            for id in 1..=2 {
                host.register_validated(catalog.state(id, 1000 + id), &catalog.view())
                    .unwrap();
            }
            let before_snapshot = host.allocator_snapshot();
            let before_records = host.records.clone();
            let before_cursor = host.schedule_cursor;
            let before_schedule_epoch = host.schedule_epoch;
            let report = host.schedule(1, 9, None, &TamperedBatch(mutation));
            assert_eq!(report.rejected, Some(CreatureScheduleError::Stale));
            assert!(report.accepted_intents.is_empty());
            assert_eq!(host.allocator_snapshot(), before_snapshot);
            assert_eq!(host.records, before_records);
            assert_eq!(host.schedule_cursor, before_cursor);
            assert_eq!(host.schedule_epoch, before_schedule_epoch);
        }
    }

    #[test]
    fn source_ownership_is_unique_and_reset_drops_records_without_reusing_ids() {
        let catalog = FixtureCatalog::new();
        let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
        host.register_validated(catalog.state(1, 1001), &catalog.view())
            .unwrap();
        assert_eq!(
            host.register_validated(catalog.state(2, 1001), &catalog.view()),
            Err(CreatureScheduleError::InvalidBatch)
        );
        let before = host.allocator_snapshot();
        host.reset_epoch().unwrap();
        assert_eq!(host.records.len(), 0);
        let after = host.allocator_snapshot();
        assert_eq!(after.world_epoch, before.world_epoch + 1);
        assert_eq!(after.next_creature_id, before.next_creature_id);
        let restored = CreatureHost::from_allocator_snapshot(after).unwrap();
        assert_eq!(restored.world_epoch().get(), after.world_epoch);
        assert_eq!(restored.world_id(), WorldId::from_u128(10));
    }

    #[test]
    fn registration_requires_the_next_unallocated_identity() {
        let catalog = FixtureCatalog::new();
        let mut host = CreatureHost::new(WorldId::from_u128(10)).unwrap();
        let before = host.allocator_snapshot();
        assert_eq!(
            host.register_validated(catalog.state(2, 1002), &catalog.view()),
            Err(CreatureScheduleError::InvalidBatch)
        );
        assert_eq!(host.allocator_snapshot(), before);
        host.register_validated(catalog.state(1, 1001), &catalog.view())
            .unwrap();
        assert_eq!(host.allocator_snapshot().next_creature_id, 2);
    }
}
