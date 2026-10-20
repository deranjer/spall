//! Creature lifecycle transactions and the creature host.

use super::*;

impl Simulation {
    /// Prepares one live server-owned lifecycle transaction. A staged geometry
    /// edit, when present, shares its already reserved transaction/journal IDs
    /// and candidate registry with the actor operations.
    pub(crate) fn prepare_creature_lifecycle_transaction(
        &self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<PreparedSimulationTransaction, String> {
        self.prepare_creature_lifecycle_transaction_with_transfers(
            tick,
            control_seq,
            intents,
            staged,
            catalog,
            CreatureTransactionTransfers::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_creature_lifecycle_transaction_with_transfers(
        &self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
        transfers: CreatureTransactionTransfers,
    ) -> Result<PreparedSimulationTransaction, String> {
        use spall_physics::{ArticulatedBatchOperation, OccupancyGrid, SourcePart};
        use spall_protocol::{
            CREATURE_CHANGE_SCHEMA, CREATURE_LIFECYCLE_COMMIT_SCHEMA, CreatureClipMode,
            CreatureComponentMotionV1, CreatureDeathWitnessV1, CreatureLifecycleChangeV4,
            CreatureLifecycleCommitV4, CreatureLifecycleContextV1, CreatureLifecycleOperationV4,
            CreatureLifecycleState, CreatureMotionOverlayV2, CreaturePartMotionV1,
            CreatureReductionOutcome, Record, TopologyOp, TopologyTransaction,
        };

        if intents.is_empty() {
            return Err("lifecycle transaction has no actor operations".into());
        }
        if intents.len() > spall_physics::MAX_CORPSE_RESTORE_BATCH_OWNERS {
            return Err("too many lifecycle owners in one transaction".into());
        }
        let kind_bits = spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT
            | spall_protocol::WORLD_RESET_TRANSFER_ID_BIT
            | spall_protocol::CREATURE_LIFECYCLE_TRANSFER_ID_BIT;
        let valid_id = |id: spall_protocol::TransferId, kind: u64| {
            id.0 & kind_bits == kind && id.0 & !kind_bits != 0
        };
        let mut seen = std::collections::BTreeSet::new();
        if transfers.lifecycle.len() > intents.len()
            || transfers.lifecycle.iter().any(|id| {
                !valid_id(*id, spall_protocol::CREATURE_LIFECYCLE_TRANSFER_ID_BIT)
                    || !seen.insert(id.0)
            })
            || transfers.geometry.is_some_and(|id| {
                staged.is_none() || !valid_id(id, spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT)
            })
        {
            return Err("invalid reserved lifecycle or geometry transfer IDs".into());
        }
        if staged.is_some_and(|edit| {
            matches!(edit.target, EditTarget::Body(entity) if self.world.body_is_dormant(entity))
        }) {
            return Err("mixed lifecycle commit requires staged dormant-body reactivation".into());
        }
        let live_host = self
            .creatures
            .as_ref()
            .ok_or_else(|| "creature host is not initialized".to_owned())?;
        if spall_protocol::content_manifest_hash(catalog.materials)
            != spall_protocol::content_manifest_hash(self.world.materials())
        {
            return Err("creature catalog material manifest differs from simulation".into());
        }
        let mut volumes_by_intent = Vec::with_capacity(intents.len());
        let mut all_source_volumes = Vec::new();
        for intent in &intents {
            let mut ids = Vec::new();
            match intent {
                AuthoritativeCreatureIntent::Birth { state, parts, .. } => {
                    if parts.len() > spall_protocol::MAX_CREATURE_OWNER_PARTS
                        || parts.len() != state.source_bindings.len()
                    {
                        return Err("birth source part count exceeds the owner bound".into());
                    }
                    for part in parts {
                        if ids.contains(&part.volume.id()) {
                            return Err("duplicate source volume in lifecycle transaction".into());
                        }
                        ids.push(part.volume.id());
                        all_source_volumes.push(&part.volume);
                    }
                }
                AuthoritativeCreatureIntent::Death { key, .. } => {
                    let state = live_host
                        .record(key.creature_id)
                        .filter(|state| state.key == *key)
                        .ok_or_else(|| "death owner is absent".to_owned())?;
                    if state.source_bindings.len() > spall_protocol::MAX_CREATURE_OWNER_PARTS {
                        return Err("death source part count exceeds the owner bound".into());
                    }
                    for binding in &state.source_bindings {
                        if ids.contains(&binding.source_volume_id) {
                            return Err("duplicate source volume in lifecycle transaction".into());
                        }
                        ids.push(binding.source_volume_id);
                        all_source_volumes.push(
                            self.world
                                .creature_source_part(*key, binding.part_id)
                                .ok_or_else(|| "death source part is absent".to_owned())?,
                        );
                    }
                }
            }
            volumes_by_intent.push(ids);
        }
        let mut source_walk_cells = 0_u64;
        for volume in &all_source_volumes {
            let slots = u64::try_from(volume.resident_brick_count())
                .map_err(|_| "source brick count overflow")?
                .checked_mul(32 * 32 * 32)
                .ok_or_else(|| "source cell work overflow".to_owned())?;
            source_walk_cells = source_walk_cells
                .checked_add(slots)
                .ok_or_else(|| "source cell work overflow".to_owned())?;
            if source_walk_cells
                > spall_protocol::creature_owner::MAX_CREATURE_OWNER_SET_DENSE_CELLS as u64
            {
                return Err("lifecycle source traversal exceeds the aggregate work budget".into());
            }
        }
        let mut source_work_by_volume = std::collections::BTreeMap::new();
        let mut aggregate_grid_cells = 0_u64;
        let mut aggregate_solid_cells = 0_u64;
        let mut aggregate_source_bytes = 0_u64;
        for volume in &all_source_volumes {
            let work = creature_source_work(volume)?;
            if source_work_by_volume.insert(volume.id(), work).is_some() {
                return Err("duplicate source volume in lifecycle transaction".into());
            }
            aggregate_grid_cells = aggregate_grid_cells
                .checked_add(work.grid_cells)
                .ok_or_else(|| "lifecycle grid work overflow".to_owned())?;
            aggregate_solid_cells = aggregate_solid_cells
                .checked_add(work.solid_cells)
                .ok_or_else(|| "lifecycle solid-cell work overflow".to_owned())?;
            aggregate_source_bytes = aggregate_source_bytes
                .checked_add(work.grid_cells.saturating_mul(2))
                .and_then(|bytes| bytes.checked_add(work.solid_cells.saturating_mul(4)))
                .ok_or_else(|| "lifecycle source byte estimate overflow".to_owned())?;
            if aggregate_grid_cells > spall_physics::MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
                || aggregate_solid_cells > spall_physics::MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS
                || aggregate_source_bytes
                    > spall_protocol::creature_owner::MAX_CREATURE_OWNER_SET_BYTES as u64
            {
                return Err("lifecycle source payload exceeds aggregate work limits".into());
            }
        }
        for ids in &volumes_by_intent {
            let mut record_grid_cells = 0_u64;
            let mut record_solid_cells = 0_u64;
            let mut record_bytes = 0_u64;
            let mut record_bricks = 0_usize;
            let mut record_masks = 0_usize;
            for id in ids {
                let work = source_work_by_volume
                    .get(id)
                    .ok_or_else(|| "source work preflight lost a volume".to_owned())?;
                record_grid_cells = record_grid_cells.saturating_add(work.grid_cells);
                record_solid_cells = record_solid_cells.saturating_add(work.solid_cells);
                record_bytes = record_bytes
                    .saturating_add(work.grid_cells.saturating_mul(2))
                    .saturating_add(work.solid_cells.saturating_mul(4));
                record_bricks = record_bricks.saturating_add(work.bricks);
                record_masks = record_masks.saturating_add(work.mask_bricks);
            }
            if record_grid_cells > spall_protocol::MAX_CREATURE_OWNER_DENSE_CELLS as u64
                || record_solid_cells > spall_protocol::MAX_CREATURE_OWNER_SOLID_CELLS as u64
                || record_solid_cells > spall_protocol::MAX_CREATURE_OWNER_MASK_CELLS as u64
                || record_bricks > spall_protocol::MAX_CREATURE_OWNER_BRICKS
                || record_masks > spall_protocol::MAX_CREATURE_OWNER_MASK_BRICKS
                || record_bytes > spall_protocol::MAX_CREATURE_OWNER_RECORD_BYTES as u64
            {
                return Err("lifecycle owner source payload exceeds record limits".into());
            }
        }
        let allocator_before = live_host.allocator_snapshot();
        let host_revision_before = live_host.revision();
        let registry_counters_before = self.world.registry().counters();
        let journal_cursor = self.journal.cursor();
        let mut geometry = if let Some(staged) = staged {
            match crate::commit::prepare_geometry_with_transfer_ids(
                &self.world,
                &self.journal,
                staged,
                tick,
                control_seq,
                &mut transfers.geometry.into_iter().collect(),
            )
            .map_err(|error| error.to_string())?
            {
                crate::commit::PrepareGeometryOutcome::Ready(geometry) => Some(geometry),
                crate::commit::PrepareGeometryOutcome::Stale(_) => {
                    return Err("staged geometry candidate is stale".into());
                }
            }
        } else {
            None
        };
        let mut candidate_registry = geometry
            .as_ref()
            .map(|geometry| geometry.registry.clone())
            .unwrap_or_else(|| self.world.registry().clone());
        let (transaction_id, journal_seq) = if let Some(geometry) = geometry.as_ref() {
            (geometry.transaction_id, geometry.journal_seq)
        } else {
            (
                candidate_registry
                    .allocate_transaction()
                    .map_err(|error| error.to_string())?,
                candidate_registry
                    .allocate_journal_seq()
                    .map_err(|error| error.to_string())?,
            )
        };
        let mut topology = geometry
            .as_ref()
            .map(|geometry| geometry.topology.clone())
            .unwrap_or(TopologyTransaction {
                transaction_id,
                server_tick: tick,
                control_seq,
                algorithm_version: crate::commit::ALGORITHM_VERSION,
                dependencies: Vec::new(),
                before: Vec::new(),
                after: Vec::new(),
                ops: Vec::new(),
                result_hashes: Vec::new(),
            });
        if topology.transaction_id != transaction_id
            || topology.server_tick != tick
            || topology.control_seq != control_seq
        {
            return Err("geometry candidate transaction framing differs".into());
        }

        enum PendingLifecycle {
            Birth {
                tag: u64,
                state: spall_protocol::CreatureStateV1,
                after: spall_protocol::CreatureOwnershipRecordV4,
                allocator_before: CreatureAllocatorSnapshotV1,
                next_volume_id: u64,
                next_volume_after: u64,
                parts: Vec<(
                    u32,
                    spall_voxel::Volume,
                    crate::world::CreatureSourceMetadata,
                )>,
            },
            Death {
                tag: u64,
                key: spall_core::CreatureKey,
                before: spall_protocol::CreatureOwnershipRecordV4,
                pre_motion: CreatureMotionOverlayV2,
                frozen_poses: Vec<(u32, spall_physics::PartTransform)>,
                witness_root_translation_m: [f64; 3],
                witness_root_rotation_xyzw: [f32; 4],
            },
        }

        let mut pending = Vec::with_capacity(intents.len());
        let mut planned_host = live_host.clone();
        let mut physics_operations = Vec::with_capacity(intents.len());
        let mut source_births = Vec::new();
        let mut death_source_fences = Vec::new();
        let mut source_ids_in_batch = std::collections::BTreeSet::new();
        let mut owner_keys_in_batch = std::collections::BTreeSet::new();
        for (index, intent) in intents.into_iter().enumerate() {
            let tag = u64::try_from(index).map_err(|_| "too many lifecycle operations")?;
            match intent {
                AuthoritativeCreatureIntent::Birth {
                    state,
                    parts,
                    representation,
                } => {
                    let state = *state;
                    state.validate(catalog).map_err(|error| error.to_string())?;
                    if state.lifecycle != CreatureLifecycleState::Alive
                        || state.clip_mode != CreatureClipMode::Playing
                        || state.linear_velocity_m_s != [0.0; 3]
                        || state.angular_velocity_rad_s != [0.0; 3]
                        || parts.len() != state.source_bindings.len()
                        || parts
                            .windows(2)
                            .any(|pair| pair[0].part_id >= pair[1].part_id)
                    {
                        return Err("birth source hierarchy or initial state is invalid".into());
                    }
                    let allocator_before = planned_host.allocator_snapshot();
                    let next_volume_id = candidate_registry.counters().1;
                    let owner_key = stable_creature_owner_key(state.key);
                    if !owner_keys_in_batch.insert(owner_key) {
                        return Err("creature owner-key hash collision in batch".into());
                    }
                    let asset = catalog
                        .assets
                        .iter()
                        .find(|asset| {
                            asset.uuid == state.asset.uuid
                                && asset.logical_hash == state.asset.logical_hash
                        })
                        .ok_or_else(|| {
                            "birth asset is absent from validation catalog".to_owned()
                        })?;
                    let portable_keys = asset
                        .portable_material_keys
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>();
                    let mapped_materials = catalog
                        .material_mapping
                        .iter()
                        .filter(|entry| portable_keys.contains(entry.portable_key))
                        .map(|entry| entry.material_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut owned_parts = Vec::with_capacity(parts.len());
                    let mut source_parts = Vec::with_capacity(parts.len());
                    for (input, binding) in parts.into_iter().zip(&state.source_bindings) {
                        let allocated = candidate_registry
                            .allocate_volume()
                            .map_err(|error| error.to_string())?;
                        if input.part_id != binding.part_id
                            || input.volume.id() != binding.source_volume_id
                            || allocated != binding.source_volume_id
                            || !source_ids_in_batch.insert(binding.source_volume_id)
                        {
                            return Err(
                                "birth source IDs differ from shared registry allocation".into()
                            );
                        }
                        let grid = OccupancyGrid::from_volume(&input.volume)
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "birth source has no solid occupancy".to_owned())?;
                        let mut materials_valid = true;
                        grid.for_each_solid(|_, _, _, material| {
                            materials_valid &= self.world.materials().contains(material)
                                && mapped_materials.contains(&material);
                        });
                        if !materials_valid {
                            return Err(
                                "birth source contains a material outside its asset map".into()
                            );
                        }
                        let metadata = creature_source_freshness(
                            &input.volume,
                            input.parent_id,
                            input.local_transform,
                        )
                        .ok_or_else(|| {
                            "birth source revision snapshot is unavailable".to_owned()
                        })?;
                        let cell_m = input.volume.cell_size().metres() as f32;
                        source_parts.push(SourcePart {
                            part_id: input.part_id,
                            parent_id: input.parent_id,
                            volume_id: binding.source_volume_id,
                            source_revision: binding.source_revision,
                            grid,
                            cell_m,
                            local_transform: input.local_transform,
                        });
                        owned_parts.push((input.part_id, input.volume, metadata));
                    }
                    let validation_parts = owned_parts
                        .iter()
                        .map(|(part_id, volume, _)| (*part_id, volume))
                        .collect::<Vec<_>>();
                    self.world
                        .validate_creature_sources(state.key, &validation_parts)
                        .map_err(str::to_owned)?;
                    let record_parts = owned_parts
                        .iter()
                        .map(|(part_id, volume, metadata)| (*part_id, volume, metadata))
                        .collect::<Vec<_>>();
                    let after = portable_birth_record(state.clone(), owner_key, &record_parts)
                        .map_err(|error| format!("birth record: {error:?}"))?;
                    after.validate(catalog).map_err(|error| error.to_string())?;
                    let translation = self
                        .world
                        .physics_translation(state.root_translation_m)
                        .map_err(|error| error.to_string())?;
                    physics_operations.push(ArticulatedBatchOperation::AliveBirth {
                        operation_tag: tag,
                        translation_m: translation,
                        rotation_xyzw: state.root_rotation_xyzw,
                        topology_token: state.aggregate_topology_revision,
                        parts: source_parts,
                        representation,
                    });
                    let next_volume_after = candidate_registry.counters().1;
                    planned_host
                        .register_validated(state.clone(), catalog)
                        .map_err(|error| format!("creature host registration: {error:?}"))?;
                    pending.push(PendingLifecycle::Birth {
                        tag,
                        state,
                        after,
                        allocator_before,
                        next_volume_id,
                        next_volume_after,
                        parts: owned_parts,
                    });
                }
                AuthoritativeCreatureIntent::Death {
                    key,
                    expected_state_revision,
                    expected_topology_token,
                    expected_pose_token,
                } => {
                    let state = self
                        .creatures
                        .as_ref()
                        .and_then(|host| host.record(key.creature_id))
                        .filter(|state| state.key == key)
                        .ok_or_else(|| "death owner is absent".to_owned())?;
                    if state.lifecycle != CreatureLifecycleState::Alive
                        || state.state_revision != expected_state_revision
                        || state.aggregate_topology_revision != expected_topology_token
                    {
                        return Err("death state or topology token is stale".into());
                    }
                    let owner_id = self
                        .world
                        .creature_physics_owner(key)
                        .ok_or_else(|| "death owner has no articulated physics body".to_owned())?;
                    let owner = self
                        .world
                        .physics()
                        .articulated_owner_snapshot(owner_id)
                        .ok_or_else(|| "death articulated owner snapshot is absent".to_owned())?;
                    let part_motion = self
                        .world
                        .physics()
                        .articulated_part_motion_snapshot(owner_id)
                        .ok_or_else(|| {
                            "death has pending or unavailable installed part motion".to_owned()
                        })?;
                    let poses = self
                        .world
                        .physics()
                        .articulated_pose_snapshot(owner_id)
                        .ok_or_else(|| "death articulated pose snapshot is absent".to_owned())?;
                    if owner.pending_pose
                        || owner.topology_token != expected_topology_token
                        || owner.pose_token != expected_pose_token
                        || poses.topology_token != expected_topology_token
                        || poses.pose_token != expected_pose_token
                        || poses
                            .parts
                            .iter()
                            .map(|(part_id, _)| *part_id)
                            .ne(state.source_bindings.iter().map(|binding| binding.part_id))
                    {
                        return Err("death has pending or stale articulated motion".into());
                    }
                    let asset = catalog
                        .assets
                        .iter()
                        .find(|asset| {
                            asset.uuid == state.asset.uuid
                                && asset.logical_hash == state.asset.logical_hash
                        })
                        .ok_or_else(|| {
                            "death asset is absent from validation catalog".to_owned()
                        })?;
                    let portable_keys = asset
                        .portable_material_keys
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>();
                    let mapped_materials = catalog
                        .material_mapping
                        .iter()
                        .filter(|entry| portable_keys.contains(entry.portable_key))
                        .map(|entry| entry.material_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut source_parts = Vec::with_capacity(state.source_bindings.len());
                    let mut source_refs = Vec::with_capacity(state.source_bindings.len());
                    let mut fenced_parts = Vec::with_capacity(state.source_bindings.len());
                    for binding in &state.source_bindings {
                        let volume = self
                            .world
                            .creature_source_part(key, binding.part_id)
                            .ok_or_else(|| "death source part is absent".to_owned())?;
                        let metadata = self
                            .world
                            .creature_source_metadata(key, binding.part_id)
                            .ok_or_else(|| "death source metadata is absent".to_owned())?;
                        if self.world.creature_source_owner(binding.source_volume_id)
                            != Some((key, binding.part_id))
                            || volume.id() != binding.source_volume_id
                            || metadata.state_stamp != volume.state_stamp()
                            || metadata.content_hash
                                != crate::world::volume_topology_hash_for(
                                    volume,
                                    spall_protocol::CanonicalOwner::Terrain,
                                )
                        {
                            return Err("death source ownership or content fence changed".into());
                        }
                        let grid = OccupancyGrid::from_volume(volume)
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "death source part has no solid occupancy".to_owned())?;
                        let current_revisions = volume
                            .resident_brick_coords()
                            .into_iter()
                            .map(|coord| {
                                volume
                                    .brick_revision(coord)
                                    .ok()
                                    .flatten()
                                    .map(|revision| (coord, revision.0))
                            })
                            .collect::<Option<Vec<_>>>()
                            .ok_or_else(|| {
                                "death source brick revision is unavailable".to_owned()
                            })?;
                        let mut materials_valid = true;
                        grid.for_each_solid(|_, _, _, material| {
                            materials_valid &= self.world.materials().contains(material)
                                && mapped_materials.contains(&material);
                        });
                        if metadata.brick_revisions != current_revisions || !materials_valid {
                            return Err(
                                "death source revision or asset material fence changed".into()
                            );
                        }
                        fenced_parts.push(CreatureDeathSourcePartFence {
                            part_id: binding.part_id,
                            volume_id: volume.id(),
                            state_stamp: volume.state_stamp(),
                            content_hash: crate::world::volume_topology_hash_for(
                                volume,
                                spall_protocol::CanonicalOwner::Terrain,
                            ),
                            brick_revisions: current_revisions.clone(),
                            metadata: metadata.clone(),
                        });
                        source_parts.push(SourcePart {
                            part_id: binding.part_id,
                            parent_id: metadata.parent_id,
                            volume_id: binding.source_volume_id,
                            source_revision: binding.source_revision,
                            cell_m: volume.cell_size().metres() as f32,
                            grid,
                            local_transform: metadata.initial_local_transform,
                        });
                        source_refs.push((binding.part_id, volume, metadata));
                    }
                    death_source_fences.push(CreatureDeathSourceFence {
                        key,
                        physics_owner: owner_id,
                        parts: fenced_parts,
                    });
                    let owner_key = stable_creature_owner_key(key);
                    let mut before = portable_birth_record(state.clone(), owner_key, &source_refs)
                        .map_err(|error| format!("death source record: {error:?}"))?;
                    let part_poses = poses
                        .parts
                        .iter()
                        .map(|(part_id, pose)| {
                            (
                                *part_id,
                                spall_protocol::CreatureLocalPoseV1 {
                                    translation_m: pose.translation_m,
                                    rotation_xyzw: pose.rotation_xyzw,
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    for part in &mut before.parts {
                        part.frozen_local_pose = part_poses
                            .iter()
                            .find(|(part_id, _)| *part_id == part.part_id)
                            .ok_or_else(|| "death pose part key mismatch".to_owned())?
                            .1;
                    }
                    let world_translation = self
                        .world
                        .physics_origin()
                        .to_world_f64(owner.root.translation_m);
                    before.state.root_translation_m = world_translation;
                    before.state.root_rotation_xyzw = owner.root.rotation;
                    before.state.linear_velocity_m_s = owner.root.linvel_m_s;
                    before.state.angular_velocity_rad_s = owner.root.angvel_rad_s;
                    before.components[0].translation_m = world_translation;
                    before.components[0].rotation_xyzw = owner.root.rotation;
                    before.components[0].linear_velocity_m_s = owner.root.linvel_m_s;
                    before.components[0].angular_velocity_rad_s = owner.root.angvel_rad_s;
                    before.part_velocities = part_motion
                        .iter()
                        .map(|motion| spall_protocol::CreaturePartVelocityV2 {
                            part_id: motion.part_id,
                            linear_velocity_m_s: motion.linear_velocity_m_s,
                            angular_velocity_rad_s: motion.angular_velocity_rad_s,
                        })
                        .collect();
                    let pre_motion = CreatureMotionOverlayV2 {
                        part_velocities: before.part_velocities.clone(),
                        state_revision: state.state_revision,
                        clip_mode: state.clip_mode,
                        clip_phase_tick: state.clip_phase_tick,
                        clip_subtick_remainder: state.clip_subtick_remainder,
                        clip_epoch: state.clip_epoch,
                        pose_epoch: state.pose_epoch,
                        root_translation_m: world_translation,
                        root_rotation_xyzw: owner.root.rotation,
                        linear_velocity_m_s: owner.root.linvel_m_s,
                        angular_velocity_rad_s: owner.root.angvel_rad_s,
                        locomotion_tag: state.locomotion_tag,
                        parts: part_poses
                            .iter()
                            .map(|(part_id, pose)| CreaturePartMotionV1 {
                                part_id: *part_id,
                                frozen_local_pose: *pose,
                            })
                            .collect(),
                        components: before
                            .components
                            .iter()
                            .map(|component| CreatureComponentMotionV1 {
                                owner_key: component.owner_key,
                                serial: component.serial,
                                translation_m: component.translation_m,
                                rotation_xyzw: component.rotation_xyzw,
                                linear_velocity_m_s: component.linear_velocity_m_s,
                                angular_velocity_rad_s: component.angular_velocity_rad_s,
                            })
                            .collect(),
                    };
                    physics_operations.push(ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: tag,
                        id: owner_id,
                        expected_topology_token,
                        expected_pose_token,
                    });
                    pending.push(PendingLifecycle::Death {
                        tag,
                        key,
                        before,
                        pre_motion,
                        frozen_poses: poses.parts,
                        witness_root_translation_m: world_translation,
                        witness_root_rotation_xyzw: owner.root.rotation,
                    });
                }
            }
        }

        let prepared_physics = self
            .world
            .physics()
            .prepare_articulated_batch(physics_operations, |material| self.world.density(material))
            .map_err(|error| error.to_string())?;
        if prepared_physics.additional_body_slots() > spall_physics::MAX_CORPSE_RESTORE_BATCH_OWNERS
        {
            return Err("actor body reservation exceeds physics batch bound".into());
        }

        let mut candidate_host = live_host.clone();
        let mut frozen_pose_updates = Vec::new();
        let mut death_witnesses = Vec::new();
        for operation in pending {
            let ordinal = u32::try_from(topology.ops.len())
                .map_err(|_| "too many topology operations for lifecycle ordinal")?;
            let (commit, owner_after, birth_parts, birth_allocator, birth_volume_counters) =
                match operation {
                    PendingLifecycle::Birth {
                        tag,
                        state,
                        after,
                        allocator_before,
                        next_volume_id,
                        next_volume_after,
                        parts,
                    } => {
                        let change = CreatureLifecycleChangeV4 {
                            schema: CREATURE_CHANGE_SCHEMA,
                            transaction_id,
                            operation_ordinal: ordinal,
                            expected_before_hash: spall_protocol::Hash32::ZERO,
                            expected_after_hash: after.canonical_hash(),
                            operation: CreatureLifecycleOperationV4::Birth {
                                after: after.clone(),
                            },
                        };
                        (
                            CreatureLifecycleCommitV4 {
                                schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
                                change,
                                expected_before_topology_hash: spall_protocol::Hash32::ZERO,
                                pre_motion: None,
                                death_witness: None,
                            },
                            Some(after),
                            Some((tag, state, parts)),
                            Some(allocator_before),
                            Some((next_volume_id, next_volume_after)),
                        )
                    }
                    PendingLifecycle::Death {
                        tag,
                        key,
                        before,
                        pre_motion,
                        frozen_poses,
                        witness_root_translation_m,
                        witness_root_rotation_xyzw,
                    } => {
                        let transfer = prepared_physics.transfer_outcome(tag).ok_or_else(|| {
                            "prepared death omitted its checked transfer witness".to_owned()
                        })?;
                        let witness = CreatureDeathWitnessV1 {
                            key,
                            source_generation: before.source_generation,
                            before_state_revision: before.state.state_revision,
                            before_topology_revision: before.state.aggregate_topology_revision,
                            before_pose_epoch: before.state.pose_epoch,
                            root_translation_m: witness_root_translation_m,
                            root_rotation_xyzw: witness_root_rotation_xyzw,
                            installed_linear_velocity_m_s: transfer
                                .linear_velocity_m_s
                                .map(|value| value as f32),
                            installed_angular_velocity_rad_s: transfer
                                .angular_velocity_rad_s
                                .map(|value| value as f32),
                            frozen_local_poses: frozen_poses
                                .iter()
                                .map(|(part_id, pose)| {
                                    (
                                        *part_id,
                                        spall_protocol::CreatureLocalPoseV1 {
                                            translation_m: pose.translation_m,
                                            rotation_xyzw: pose.rotation_xyzw,
                                        },
                                    )
                                })
                                .collect(),
                        };
                        let mut after = before.clone();
                        after.state.state_revision = after
                            .state
                            .state_revision
                            .checked_add(1)
                            .ok_or_else(|| "creature state revision exhausted".to_owned())?;
                        after.state.lifecycle = CreatureLifecycleState::Corpse;
                        after.part_velocities.clear();
                        after.state.clip_mode = CreatureClipMode::Frozen;
                        after.state.clip_epoch = after
                            .state
                            .clip_epoch
                            .checked_add(1)
                            .ok_or_else(|| "creature clip epoch exhausted".to_owned())?;
                        after.state.pose_epoch = after
                            .state
                            .pose_epoch
                            .checked_add(1)
                            .ok_or_else(|| "creature pose epoch exhausted".to_owned())?;
                        after.state.linear_velocity_m_s = witness.installed_linear_velocity_m_s;
                        after.state.angular_velocity_rad_s =
                            witness.installed_angular_velocity_rad_s;
                        for component in &mut after.components {
                            component.linear_velocity_m_s = witness.installed_linear_velocity_m_s;
                            component.angular_velocity_rad_s =
                                witness.installed_angular_velocity_rad_s;
                        }
                        let after_hash = after.canonical_hash();
                        let change = CreatureLifecycleChangeV4 {
                            schema: CREATURE_CHANGE_SCHEMA,
                            transaction_id,
                            operation_ordinal: ordinal,
                            expected_before_hash: before.canonical_hash(),
                            expected_after_hash: after_hash,
                            operation: CreatureLifecycleOperationV4::Death { after },
                        };
                        let commit = CreatureLifecycleCommitV4 {
                            schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
                            change,
                            expected_before_topology_hash: before.topology_hash(),
                            pre_motion: Some(pre_motion),
                            death_witness: Some(witness.clone()),
                        };
                        frozen_pose_updates.push((key, frozen_poses));
                        death_witnesses.push(witness);
                        (commit, Some(before), None, None, None)
                    }
                };

            let existing_keys = candidate_host
                .records()
                .map(|state| state.key)
                .collect::<Vec<_>>();
            let mut existing_owner_keys = Vec::with_capacity(existing_keys.len());
            let mut existing_source_volumes = Vec::new();
            for state in candidate_host.records() {
                existing_owner_keys.push(stable_creature_owner_key(state.key));
                existing_source_volumes.extend(
                    state
                        .source_bindings
                        .iter()
                        .map(|binding| binding.source_volume_id),
                );
            }
            let current = match &commit.change.operation {
                CreatureLifecycleOperationV4::Birth { .. } => None,
                CreatureLifecycleOperationV4::Death { .. } => owner_after.as_ref(),
            };
            let allocator = birth_allocator.unwrap_or_else(|| candidate_host.allocator_snapshot());
            let (next_volume_id, next_volume_after) = birth_volume_counters.unwrap_or((
                candidate_registry.counters().1,
                candidate_registry.counters().1,
            ));
            let witness = commit.death_witness.as_ref();
            let reduced = commit
                .validate_and_reduce(
                    current,
                    CreatureLifecycleContextV1 {
                        transaction_id,
                        operation_ordinal: ordinal,
                        replay_receipt: None,
                        allocator,
                        next_volume_id,
                        death_witness: witness,
                        existing_keys: &existing_keys,
                        existing_owner_keys: &existing_owner_keys,
                        existing_source_volumes: &existing_source_volumes,
                    },
                    catalog,
                )
                .map_err(|error| error.to_string())?;
            if reduced.outcome != CreatureReductionOutcome::Applied {
                return Err("live producer unexpectedly reduced as a replay".into());
            }
            if let Some((tag, state, parts)) = birth_parts {
                if reduced.record.state.key != state.key
                    || reduced.next_volume_id != next_volume_after
                {
                    return Err("birth reducer and shared candidate allocators disagree".into());
                }
                candidate_host
                    .register_validated(state, catalog)
                    .map_err(|error| format!("creature host registration: {error:?}"))?;
                source_births.push((tag, reduced.record.state.key, parts));
            } else {
                candidate_host
                    .replace_record_for_lifecycle(reduced.record.state.clone(), catalog)
                    .map_err(|error| format!("creature host lifecycle update: {error:?}"))?;
            }
            topology.ops.push(TopologyOp::CreatureLifecycle {
                commit: Box::new(commit),
            });
        }
        topology.validate().map_err(|error| error.to_string())?;
        let mut attachments = geometry
            .as_mut()
            .and_then(|geometry| geometry.attachments.take());
        if !crate::replication::inline_wire_fits(&topology) {
            let mut ids = transfers.lifecycle.into_iter();
            let set =
                attachments.get_or_insert_with(|| spall_protocol::TransactionAttachmentSetV1 {
                    schema: spall_protocol::TRANSACTION_ATTACHMENT_SET_SCHEMA,
                    transaction_id,
                    attachments: Vec::new(),
                });
            let inline_ops = std::mem::take(&mut topology.ops);
            for op in inline_ops {
                if let TopologyOp::CreatureLifecycle { commit } = op {
                    let transfer_id = ids.next().ok_or_else(|| "lifecycle transaction exceeds the inline control-record ceiling without reserved bulk IDs".to_owned())?;
                    let ordinal = commit.change.operation_ordinal;
                    let payload = spall_protocol::CreatureLifecycleBulkPayloadV4::new(
                        transaction_id,
                        ordinal,
                        *commit,
                    )
                    .map_err(|error| error.to_string())?;
                    topology.ops.push(TopologyOp::CreatureLifecycleBulk {
                        transfer_id: transfer_id.0,
                        operation_ordinal: ordinal,
                        payload_schema: spall_protocol::CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
                        commit_hash: payload.commit_hash,
                    });
                    set.attachments.push(
                        spall_protocol::TransactionAttachmentV1::CreatureLifecycle {
                            transfer_id: transfer_id.0,
                            payload: Box::new(payload),
                        },
                    );
                } else {
                    topology.ops.push(op);
                }
            }
            topology.validate().map_err(|error| error.to_string())?;
            if !crate::replication::inline_wire_fits(&topology) {
                return Err(
                    "bulk lifecycle marker transaction exceeds the control-record ceiling".into(),
                );
            }
        }
        if let Some(set) = &attachments {
            // Complete correlation, cumulative work and actual encoding must succeed
            // before the physics candidate can publish. No payload is patched later.
            set.encode_for_transaction(&topology)
                .map_err(|error| error.to_string())?;
        }
        if let Some(geometry) = geometry.as_mut() {
            geometry.registry = candidate_registry.clone();
            geometry.topology = topology.clone();
        }
        let participants = geometry
            .as_ref()
            .map_or_else(Vec::new, |geometry| geometry.participants.clone());
        let actor_only_physics_fence = if geometry.is_none() {
            Some(
                self.world
                    .physics()
                    .snapshot_body_commit_fence(&[])
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let actor_only_registry = if geometry.is_none() {
            Some(candidate_registry)
        } else {
            None
        };
        Ok(PreparedSimulationTransaction {
            world_identity: self.world.commit_identity.clone(),
            registry_counters_before,
            physics_origin: self.world.physics_origin(),
            topology_epoch: self.world.topology_epoch(),
            journal_cursor,
            actor_only_physics_fence,
            allocator_before,
            host_revision_before,
            catalog_hash: creature_catalog_fingerprint(catalog),
            candidate_host,
            geometry,
            actor_only_registry,
            physics: prepared_physics,
            source_births,
            death_source_fences,
            frozen_pose_updates,
            transaction_id,
            journal_seq,
            topology,
            attachments,
            participants,
            death_witnesses,
        })
    }

    pub(crate) fn validate_prepared_creature_lifecycle(
        &self,
        mut prepared: PreparedSimulationTransaction,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<ValidatedSimulationTransaction, String> {
        if !std::sync::Arc::ptr_eq(&prepared.world_identity, &self.world.commit_identity)
            || prepared.physics_origin != self.world.physics_origin()
            || prepared.topology_epoch != self.world.topology_epoch()
            || prepared.journal_cursor != self.journal.cursor()
            || prepared.catalog_hash != creature_catalog_fingerprint(catalog)
        {
            return Err("prepared lifecycle world, journal, or catalog fence changed".into());
        }
        let host = self
            .creatures
            .as_ref()
            .ok_or_else(|| "creature host disappeared before publication".to_owned())?;
        if host.revision() != prepared.host_revision_before
            || host.allocator_snapshot() != prepared.allocator_before
        {
            return Err("prepared lifecycle creature host changed".into());
        }
        if prepared.geometry.is_none()
            && (self.world.registry().counters() != prepared.registry_counters_before
                || prepared
                    .actor_only_physics_fence
                    .as_ref()
                    .is_none_or(|fence| !self.world.physics().validate_body_commit_fence(fence)))
        {
            return Err("prepared actor-only world fence changed".into());
        }
        for (_, key, parts) in &prepared.source_births {
            let borrowed = parts
                .iter()
                .map(|(part_id, volume, _)| (*part_id, volume))
                .collect::<Vec<_>>();
            self.world
                .validate_creature_sources(*key, &borrowed)
                .map_err(str::to_owned)?;
        }
        for fence in &prepared.death_source_fences {
            if self.world.creature_physics_owner(fence.key) != Some(fence.physics_owner) {
                return Err("prepared death articulated owner binding changed".into());
            }
            for part in &fence.parts {
                let volume = self
                    .world
                    .creature_source_part(fence.key, part.part_id)
                    .ok_or_else(|| "prepared death source part disappeared".to_owned())?;
                let metadata = self
                    .world
                    .creature_source_metadata(fence.key, part.part_id)
                    .ok_or_else(|| "prepared death source metadata disappeared".to_owned())?;
                let revisions = volume
                    .resident_brick_coords()
                    .into_iter()
                    .map(|coord| {
                        volume
                            .brick_revision(coord)
                            .ok()
                            .flatten()
                            .map(|revision| (coord, revision.0))
                    })
                    .collect::<Option<Vec<_>>>();
                if volume.id() != part.volume_id
                    || volume.state_stamp() != part.state_stamp
                    || crate::world::volume_topology_hash_for(
                        volume,
                        spall_protocol::CanonicalOwner::Terrain,
                    ) != part.content_hash
                    || revisions.as_deref() != Some(part.brick_revisions.as_slice())
                    || metadata != &part.metadata
                    || self.world.creature_source_owner(part.volume_id)
                        != Some((fence.key, part.part_id))
                {
                    return Err("prepared death source ownership or content fence changed".into());
                }
            }
        }
        let geometry_slots = prepared
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.additional_body_slots());
        let validated_physics = self
            .world
            .physics()
            .validate_articulated_batch_with_reserved_body_slots(prepared.physics, geometry_slots)
            .map_err(|error| error.to_string())?;
        let validated_geometry = prepared
            .geometry
            .take()
            .map(|geometry| {
                crate::commit::validate_prepared_geometry(&self.world, &self.journal, *geometry)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        Ok(ValidatedSimulationTransaction {
            geometry: validated_geometry,
            physics: validated_physics,
            candidate_host: prepared.candidate_host,
            actor_only_registry: prepared.actor_only_registry,
            source_births: prepared.source_births,
            frozen_pose_updates: prepared.frozen_pose_updates,
            transaction_id: prepared.transaction_id,
            journal_seq: prepared.journal_seq,
            topology: prepared.topology,
            attachments: prepared.attachments,
            participants: prepared.participants,
            death_witnesses: prepared.death_witnesses,
        })
    }

    /// Publishes a fully validated mixed candidate. Every operation below is
    /// infallible; the ordered journal is appended once after both owners.
    pub(crate) fn publish_validated_creature_lifecycle(
        &mut self,
        validated: ValidatedSimulationTransaction,
    ) -> CommittedCreatureLifecycle {
        let ValidatedSimulationTransaction {
            geometry,
            physics,
            candidate_host,
            actor_only_registry,
            source_births,
            frozen_pose_updates,
            transaction_id,
            journal_seq,
            topology,
            attachments,
            participants,
            death_witnesses,
        } = validated;
        // Finish journal ownership/copies before the first world publication.
        // The mixed parent owns the complete final set; no markers or payloads
        // are reconstructed from the geometry participant after it publishes.
        let entry = crate::journal::JournalEntry {
            seq: journal_seq,
            transaction: topology.clone(),
            participants,
            attachments: attachments.clone(),
        };
        if let Some(geometry) = geometry {
            crate::commit::publish_validated_geometry_without_journal(&mut self.world, geometry);
        } else {
            *self.world.registry_mut() = actor_only_registry
                .expect("actor-only prepared transaction owns its registry candidate");
        }
        let outcomes = self
            .world
            .physics_mut()
            .publish_validated_articulated_batch(physics);
        for (tag, key, parts) in source_births {
            self.world
                .publish_creature_sources_with_metadata(key, parts);
            let owner = outcomes
                .iter()
                .find(|outcome| outcome.operation_tag == tag)
                .expect("prepared birth has one physics outcome")
                .owner_id;
            self.world.set_creature_physics_owner(key, owner);
        }
        for (key, poses) in frozen_pose_updates {
            self.world.set_creature_frozen_poses(key, poses);
        }
        self.creatures = Some(candidate_host);
        self.journal.append(entry);
        CommittedCreatureLifecycle {
            transaction_id,
            journal_seq,
            topology,
            attachments,
            witnesses: death_witnesses,
        }
    }

    /// Commits lifecycle intents as one ordered transaction at the current
    /// tick, taking the next control sequence from the same counter as ordinary
    /// edits so replicas see a single monotone stream.
    pub fn commit_creature_lifecycle_now(
        &mut self,
        intents: Vec<AuthoritativeCreatureIntent>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<CommittedCreatureLifecycle, String> {
        let control_seq = spall_protocol::ControlSeq(self.next_control_seq);
        let committed = self.commit_creature_lifecycle_transaction(
            self.tick,
            control_seq,
            intents,
            None,
            catalog,
        )?;
        self.next_control_seq += 1;
        Ok(committed)
    }

    /// Next unallocated creature id and shared source-volume id, so a server
    /// director can build a birth batch without mutating the host.
    pub fn creature_birth_ids(&self) -> Option<(u64, u64)> {
        let next_creature = self.creature_allocator_snapshot()?.next_creature_id;
        Some((next_creature, self.world.registry().counters().1))
    }

    pub fn commit_creature_lifecycle_transaction(
        &mut self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<CommittedCreatureLifecycle, String> {
        let prepared = self.prepare_creature_lifecycle_transaction(
            tick,
            control_seq,
            intents,
            staged,
            catalog,
        )?;
        let validated = self.validate_prepared_creature_lifecycle(prepared, catalog)?;
        Ok(self.publish_validated_creature_lifecycle(validated))
    }

    /// Installs an empty, owner-thread creature host. Registration remains
    /// crate-private until the atomic lifecycle/source-ownership slice.
    pub fn initialize_creature_host(
        &mut self,
        world_id: WorldId,
    ) -> Result<(), crate::creatures::CreatureHostInitError> {
        if self.creatures.is_some() {
            return Err(crate::creatures::CreatureHostInitError::AlreadyInitialized);
        }
        self.creatures = Some(crate::creatures::CreatureHost::new(world_id)?);
        Ok(())
    }

    /// Restores only the in-memory actor ID/epoch counters. Actor records remain
    /// empty; this does not restore or publish creatures.
    pub fn restore_creature_allocator_snapshot(
        &mut self,
        snapshot: CreatureAllocatorSnapshotV1,
    ) -> Result<(), crate::creatures::CreatureHostInitError> {
        if self.creatures.is_some() {
            return Err(crate::creatures::CreatureHostInitError::AlreadyInitialized);
        }
        self.creatures = Some(crate::creatures::CreatureHost::from_allocator_snapshot(
            snapshot,
        )?);
        Ok(())
    }

    /// Captures the in-memory creature allocator only. This is not wired into
    /// any checkpoint or durable recovery schema.
    pub fn creature_allocator_snapshot(&self) -> Option<CreatureAllocatorSnapshotV1> {
        self.creatures
            .as_ref()
            .map(crate::creatures::CreatureHost::allocator_snapshot)
    }

    /// Test-only transaction seam for source ownership. It is intentionally
    /// absent from production builds until lifecycle replay and checkpoint
    /// publication can carry actor state.
    #[cfg(test)]
    pub(super) fn commit_creature_birth_for_test(
        &mut self,
        state: spall_protocol::CreatureStateV1,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
        parts: Vec<CreatureBirthPartInput>,
        representation: spall_physics::Representation,
    ) -> Result<spall_physics::ArticulatedBodyId, crate::creatures::CreatureScheduleError> {
        use crate::creatures::CreatureScheduleError;
        use spall_physics::{OccupancyGrid, SourcePart};
        use spall_protocol::{CreatureClipMode, CreatureLifecycleState, content_manifest_hash};

        if state.lifecycle != CreatureLifecycleState::Alive
            || state.clip_mode != CreatureClipMode::Playing
            || state.linear_velocity_m_s != [0.0; 3]
            || state.angular_velocity_rad_s != [0.0; 3]
            || content_manifest_hash(catalog.materials)
                != content_manifest_hash(self.world.materials())
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }

        let mut candidate_host = self
            .creatures
            .as_ref()
            .ok_or(CreatureScheduleError::InvalidBatch)?
            .clone();
        // Validate the immutable DTO and consume only a cloned creature counter.
        candidate_host.register_validated(state.clone(), catalog)?;

        if parts.len() != state.source_bindings.len()
            || parts
                .windows(2)
                .any(|pair| pair[0].part_id >= pair[1].part_id)
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let mut candidate_registry = self.world.registry().clone();
        let asset = catalog
            .assets
            .iter()
            .find(|asset| {
                asset.uuid == state.asset.uuid && asset.logical_hash == state.asset.logical_hash
            })
            .ok_or(CreatureScheduleError::InvalidBatch)?;
        let portable_keys: std::collections::BTreeSet<_> =
            asset.portable_material_keys.iter().copied().collect();
        let mapped_materials: std::collections::BTreeSet<_> = catalog
            .material_mapping
            .iter()
            .filter(|entry| portable_keys.contains(entry.portable_key))
            .map(|entry| entry.material_id)
            .collect();
        let mut source_volumes = Vec::with_capacity(parts.len());
        let mut physics_parts = Vec::with_capacity(parts.len());
        for (input, binding) in parts.into_iter().zip(&state.source_bindings) {
            let allocated = candidate_registry
                .allocate_volume()
                .map_err(|_| CreatureScheduleError::CounterExhausted)?;
            if input.part_id != binding.part_id
                || input.volume.id() != binding.source_volume_id
                || allocated != binding.source_volume_id
            {
                return Err(CreatureScheduleError::InvalidBatch);
            }
            let grid = OccupancyGrid::from_volume(&input.volume)
                .map_err(|_| CreatureScheduleError::InvalidBatch)?
                .ok_or(CreatureScheduleError::InvalidBatch)?;
            let mut materials_valid = true;
            grid.for_each_solid(|_, _, _, material| {
                materials_valid &= self.world.materials().contains(material)
                    && mapped_materials.contains(&material);
            });
            if !materials_valid {
                return Err(CreatureScheduleError::InvalidBatch);
            }
            let freshness =
                creature_source_freshness(&input.volume, input.parent_id, input.local_transform)
                    .ok_or(CreatureScheduleError::InvalidBatch)?;
            let cell_m = input.volume.cell_size().metres() as f32;
            physics_parts.push(SourcePart {
                part_id: input.part_id,
                parent_id: input.parent_id,
                volume_id: binding.source_volume_id,
                source_revision: binding.source_revision,
                grid,
                cell_m,
                local_transform: input.local_transform,
            });
            source_volumes.push((input.part_id, input.volume, freshness));
        }
        let source_volumes_for_validation: Vec<_> = source_volumes
            .iter()
            .map(|(part_id, volume, _)| (*part_id, volume.clone()))
            .collect();
        self.world
            .validate_creature_sources(state.key, &source_volumes_for_validation)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;

        let translation = self
            .world
            .physics_translation(state.root_translation_m)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        // The physics adapter preflights every part, collider and transform
        // before inserting any owner state. No fallible step follows success.
        let physics_owner = self
            .world
            .physics_mut()
            .add_articulated_kinematic_parts(
                translation,
                state.root_rotation_xyzw,
                state.aggregate_topology_revision,
                physics_parts,
                representation,
            )
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;

        self.world
            .publish_creature_sources_with_metadata(state.key, source_volumes);
        self.world
            .set_creature_physics_owner(state.key, physics_owner);
        self.world.registry_mut().clone_from(&candidate_registry);
        self.creatures = Some(candidate_host);
        Ok(physics_owner)
    }

    #[cfg(test)]
    pub(super) fn commit_creature_death_for_test(
        &mut self,
        key: spall_core::CreatureKey,
        expected_state_revision: u64,
        expected_topology_token: u64,
        expected_pose_token: u64,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<spall_physics::CorpseTransferOutcome, crate::creatures::CreatureScheduleError> {
        use crate::creatures::CreatureScheduleError;
        use spall_protocol::{CreatureClipMode, CreatureLifecycleState};

        let current = self
            .creatures
            .as_ref()
            .and_then(|host| host.record(key.creature_id))
            .filter(|state| state.key == key)
            .ok_or(CreatureScheduleError::Stale)?;
        if current.lifecycle != CreatureLifecycleState::Alive
            || current.state_revision != expected_state_revision
            || current.aggregate_topology_revision != expected_topology_token
            || spall_protocol::content_manifest_hash(catalog.materials)
                != spall_protocol::content_manifest_hash(self.world.materials())
        {
            return Err(CreatureScheduleError::Stale);
        }
        current
            .validate(catalog)
            .map_err(|_| CreatureScheduleError::Stale)?;
        let asset = catalog
            .assets
            .iter()
            .find(|asset| {
                asset.uuid == current.asset.uuid && asset.logical_hash == current.asset.logical_hash
            })
            .ok_or(CreatureScheduleError::Stale)?;
        let portable_keys: std::collections::BTreeSet<_> =
            asset.portable_material_keys.iter().copied().collect();
        let mapped_materials: std::collections::BTreeSet<_> = catalog
            .material_mapping
            .iter()
            .filter(|entry| portable_keys.contains(entry.portable_key))
            .map(|entry| entry.material_id)
            .collect();
        for binding in &current.source_bindings {
            let source = self
                .world
                .creature_source_part(key, binding.part_id)
                .ok_or(CreatureScheduleError::Stale)?;
            if self.world.creature_source_owner(binding.source_volume_id)
                != Some((key, binding.part_id))
                || source.id() != binding.source_volume_id
            {
                return Err(CreatureScheduleError::Stale);
            }
            let Some(metadata) = self.world.creature_source_metadata(key, binding.part_id) else {
                return Err(CreatureScheduleError::Stale);
            };
            let current_revisions: Vec<_> = source
                .resident_brick_coords()
                .into_iter()
                .map(|coord| {
                    source
                        .brick_revision(coord)
                        .ok()
                        .flatten()
                        .map(|revision| (coord, revision.0))
                })
                .collect::<Option<_>>()
                .ok_or(CreatureScheduleError::Stale)?;
            if metadata.state_stamp != source.state_stamp()
                || metadata.content_hash
                    != crate::world::volume_topology_hash_for(
                        source,
                        spall_protocol::CanonicalOwner::Terrain,
                    )
                || metadata.brick_revisions != current_revisions
            {
                return Err(CreatureScheduleError::Stale);
            }
            let grid = spall_physics::OccupancyGrid::from_volume(source)
                .map_err(|_| CreatureScheduleError::Stale)?
                .ok_or(CreatureScheduleError::Stale)?;
            let mut materials_valid = true;
            grid.for_each_solid(|_, _, _, material| {
                materials_valid &= self.world.materials().contains(material)
                    && mapped_materials.contains(&material);
            });
            if !materials_valid {
                return Err(CreatureScheduleError::Stale);
            }
        }
        let physics_owner = self
            .world
            .creature_physics_owner(key)
            .ok_or(CreatureScheduleError::Stale)?;
        let live = self
            .world
            .physics()
            .articulated_owner_snapshot(physics_owner)
            .ok_or(CreatureScheduleError::Stale)?;
        if live.pending_pose
            || live.topology_token != expected_topology_token
            || live.pose_token != expected_pose_token
        {
            return Err(CreatureScheduleError::Stale);
        }
        let live_parts = self
            .world
            .physics()
            .articulated_pose_snapshot(physics_owner)
            .ok_or(CreatureScheduleError::Stale)?;
        if live_parts.topology_token != expected_topology_token
            || live_parts.pose_token != expected_pose_token
            || live_parts
                .parts
                .iter()
                .map(|(part_id, _)| *part_id)
                .ne(current
                    .source_bindings
                    .iter()
                    .map(|binding| binding.part_id))
        {
            return Err(CreatureScheduleError::Stale);
        }

        let mut corpse = current.clone();
        corpse.state_revision = corpse
            .state_revision
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.lifecycle = CreatureLifecycleState::Corpse;
        corpse.clip_mode = CreatureClipMode::Frozen;
        corpse.clip_epoch = corpse
            .clip_epoch
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.pose_epoch = corpse
            .pose_epoch
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.root_translation_m = self
            .world
            .physics_origin()
            .to_world_f64(live.root.translation_m);
        corpse.root_rotation_xyzw = live.root.rotation;
        corpse.linear_velocity_m_s = live.root.linvel_m_s;
        corpse.angular_velocity_rad_s = live.root.angvel_rad_s;

        let mut candidate_host = self.creatures.as_ref().unwrap().clone();

        // Density is read from the Simulation's immutable material manifest,
        // and the exact materials present in source cells are preflighted
        // before the physics owner changes.
        let mut densities = std::collections::BTreeMap::new();
        for binding in &current.source_bindings {
            let volume = self
                .world
                .creature_source_part(key, binding.part_id)
                .ok_or(CreatureScheduleError::Stale)?;
            let grid = spall_physics::OccupancyGrid::from_volume(volume)
                .map_err(|_| CreatureScheduleError::InvalidBatch)?
                .ok_or(CreatureScheduleError::InvalidBatch)?;
            grid.for_each_solid(|_, _, _, material| {
                densities
                    .entry(material)
                    .or_insert_with(|| self.world.density(material));
            });
        }
        if densities
            .values()
            .any(|density| !density.is_finite() || *density <= 0.0)
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let density = |material| densities.get(&material).copied().unwrap_or(0.0);

        // The adapter invokes this closure after exact mass/resultant motion
        // and collider preparation, before changing physics ownership. It
        // validates the final DTO/host candidate so no fallible step follows
        // the physical transfer.
        let outcome = self
            .world
            .physics_mut()
            .transfer_articulated_to_dynamic_corpse_checked(
                physics_owner,
                expected_topology_token,
                expected_pose_token,
                density,
                |outcome| {
                    corpse.linear_velocity_m_s =
                        outcome.linear_velocity_m_s.map(|value| value as f32);
                    corpse.angular_velocity_rad_s =
                        outcome.angular_velocity_rad_s.map(|value| value as f32);
                    candidate_host
                        .replace_record_for_lifecycle_test(corpse, catalog)
                        .is_ok()
                },
            )
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        self.world.set_creature_frozen_poses(key, live_parts.parts);
        self.creatures = Some(candidate_host);
        Ok(outcome)
    }
}
