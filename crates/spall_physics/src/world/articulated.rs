use super::*;

impl PhysicsWorld {
    /// Adds one position-controlled assembly root and one exact collider per
    /// source part. Parent transforms are flattened into assembly-local poses;
    /// source grids remain separate even when their integer coordinates overlap.
    pub fn add_articulated_body(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        self.add_articulated_body_mode(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            false,
            None,
        )
    }

    /// Experimental feasibility path: each exact source-part collider has an
    /// independent position-controlled body, while all parts retain one
    /// conceptual owner and source identity. No joints are inferred from the
    /// authored part hierarchy. This remains a bounded movement/contact rig,
    /// not a dynamic ragdoll or corpse representation.
    pub fn add_articulated_kinematic_parts(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let birth = self.prepare_articulated_birth(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            true,
            None,
        )?;
        let prepared = PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes: vec![PreparedArticulatedChange::AliveBirth {
                operation_tag: 0,
                birth,
            }],
        };
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .expect("singleton kinematic birth yields one result")
            .owner_id)
    }

    /// Bounded corpse candidate: exact frozen per-source-part colliders under
    /// one dynamic solver body. PART hierarchy defines pose only, never bonds.
    pub fn add_dynamic_corpse_aggregate(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let mut materials = std::collections::BTreeSet::new();
        for part in &parts {
            part.grid.for_each_solid(|_, _, _, material| {
                materials.insert(material);
            });
        }
        let mut manifest = HashMap::new();
        for material in materials {
            let rho = density(material);
            if !rho.is_finite() || rho <= 0.0 {
                return Err(crate::articulated::ArticulatedError::InvalidDensity);
            }
            manifest.insert(material, rho.to_bits());
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let birth = self.prepare_articulated_birth(
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            representation,
            false,
            Some(&stable_density),
        )?;
        let prepared = PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes: vec![PreparedArticulatedChange::DynamicCorpseBirth {
                operation_tag: 0,
                birth,
            }],
        };
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .expect("singleton birth yields one result")
            .owner_id)
    }

    /// Reconstructs one exact owner using the same preparation/publication
    /// boundary as a multi-owner cold restore.
    pub fn restore_dynamic_corpse(
        &mut self,
        restore: crate::articulated::DynamicCorpseRestore,
        representation: Representation,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        let prepared =
            self.prepare_dynamic_corpse_restore_batch(vec![restore], representation, density)?;
        let mut owners = self.commit_dynamic_corpse_restore_batch(prepared)?;
        Ok(owners
            .pop()
            .expect("singleton restore prepares exactly one owner"))
    }

    /// Prepares all sources, masks, shapes, masses and identities without
    /// inserting any solver object. The returned value is bound to this world.
    /// Input order is the order of the returned owner handles at commit.
    pub fn prepare_dynamic_corpse_restore_batch(
        &self,
        restores: Vec<crate::articulated::DynamicCorpseRestore>,
        representation: Representation,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<PreparedDynamicCorpseBatch, crate::articulated::ArticulatedError> {
        self.prepare_articulated_batch(
            restores
                .into_iter()
                .enumerate()
                .map(|(index, restore)| ArticulatedBatchOperation::ColdRestore {
                    operation_tag: index as u64,
                    restore,
                    representation,
                })
                .collect(),
            density,
        )
    }

    /// Prepares all supported articulated lifecycle operations without
    /// inserting solver objects. Density values are sampled once per material
    /// for the complete batch. Operation tags must be unique.
    pub fn prepare_articulated_batch(
        &self,
        operations: Vec<ArticulatedBatchOperation>,
        density: impl Fn(MaterialId) -> f64,
    ) -> Result<PreparedArticulatedBatch, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if operations.is_empty() || operations.len() > MAX_CORPSE_RESTORE_BATCH_OWNERS {
            return Err(ArticulatedError::WorkBudget);
        }
        let mut dense = 0_u64;
        let mut solids = 0_u64;
        let mut mask_cells = 0_usize;
        let mut materials = std::collections::BTreeSet::new();
        let mut tags = std::collections::BTreeSet::new();
        for operation in &operations {
            let tag = match operation {
                ArticulatedBatchOperation::AliveBirth { operation_tag, .. }
                | ArticulatedBatchOperation::AliveRestore { operation_tag, .. }
                | ArticulatedBatchOperation::CheckedDeathTransfer { operation_tag, .. }
                | ArticulatedBatchOperation::ColdRestore { operation_tag, .. } => *operation_tag,
            };
            if !tags.insert(tag) {
                return Err(ArticulatedError::StalePreparedBatch);
            }
            let parts = match operation {
                ArticulatedBatchOperation::AliveBirth { parts, .. } => parts.as_slice(),
                ArticulatedBatchOperation::AliveRestore { restore, .. } => {
                    if restore.parts.len() > MAX_ARTICULATED_PARTS
                        || restore.part_motion.len() != restore.parts.len()
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                    restore.parts.as_slice()
                }
                ArticulatedBatchOperation::ColdRestore { restore, .. } => {
                    if restore.components.len() > crate::fracture::MAX_FRACTURE_COMPONENTS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    for component in &restore.components {
                        mask_cells = mask_cells
                            .checked_add(component.cells.len())
                            .ok_or(ArticulatedError::WorkBudget)?;
                    }
                    if mask_cells as u64 > MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    restore.source_parts.as_slice()
                }
                ArticulatedBatchOperation::CheckedDeathTransfer { id, .. } => {
                    let assembly = self
                        .articulated
                        .get(&id.0)
                        .ok_or(ArticulatedError::Hierarchy)?;
                    let source = assembly
                        .parts
                        .iter()
                        .map(|part| SourcePart {
                            part_id: part.part_id,
                            parent_id: part.parent_id,
                            volume_id: part.volume_id,
                            source_revision: part.source_revision,
                            grid: part.grid.clone(),
                            cell_m: part.cell_m,
                            local_transform: part.local_pose,
                        })
                        .collect::<Vec<_>>();
                    if source.len() > MAX_ARTICULATED_PARTS {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    for part in &source {
                        add_batch_grid_budget(&part.grid, &mut dense, &mut solids, &mut materials)?;
                    }
                    continue;
                }
            };
            if parts.len() > MAX_ARTICULATED_PARTS {
                return Err(ArticulatedError::WorkBudget);
            }
            for part in parts {
                add_batch_grid_budget(&part.grid, &mut dense, &mut solids, &mut materials)?;
            }
        }
        // Snapshot each material density once for the complete batch. Shapes
        // and mass preparation cannot observe different catalog values per owner.
        let mut material_densities = HashMap::new();
        for material in materials {
            let rho = density(material);
            if !rho.is_finite() || rho <= 0.0 {
                return Err(ArticulatedError::InvalidDensity);
            }
            material_densities.insert(material, rho.to_bits());
        }
        let stable_density = |material| manifest_density(&material_densities, material);
        let mut changes = Vec::with_capacity(operations.len());
        let mut owner_keys = std::collections::BTreeSet::new();
        let mut volume_ids = std::collections::BTreeSet::new();
        let mut primitives = 0_usize;
        let mut derived_dense = 0_u64;
        let mut transfer_ids = HashSet::new();
        for operation in operations {
            match operation {
                ArticulatedBatchOperation::AliveBirth {
                    operation_tag,
                    translation_m,
                    rotation_xyzw,
                    topology_token,
                    parts,
                    representation,
                } => {
                    let birth = self.prepare_articulated_birth(
                        translation_m,
                        rotation_xyzw,
                        topology_token,
                        parts,
                        representation,
                        true,
                        Some(&stable_density),
                    )?;
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| !volume_ids.insert(*id))
                    {
                        return Err(ArticulatedError::DuplicateVolume);
                    }
                    primitives = primitives
                        .checked_add(birth.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::AliveBirth {
                        operation_tag,
                        birth,
                    });
                }
                ArticulatedBatchOperation::AliveRestore {
                    operation_tag,
                    restore,
                    representation,
                } => {
                    if restore.owner_key == 0
                        || restore.source_generation == 0
                        || restore.topology_token == 0
                        || restore.next_component_serial != 1
                        || !restore
                            .root_linear_velocity_m_s
                            .iter()
                            .chain(&restore.root_angular_velocity_rad_s)
                            .all(|value| value.is_finite())
                        || restore.parts.iter().any(|part| part.source_revision == 0)
                        || self.fracture_owner_keys.contains_key(&restore.owner_key)
                        || self
                            .articulated
                            .values()
                            .any(|owner| owner.fracture_owner_key == Some(restore.owner_key))
                        || !owner_keys.insert(restore.owner_key)
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                    let expected_part_ids = restore
                        .parts
                        .iter()
                        .map(|part| part.part_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let motion_part_ids = restore
                        .part_motion
                        .iter()
                        .map(|motion| motion.part_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    if expected_part_ids.len() != restore.parts.len()
                        || motion_part_ids.len() != restore.part_motion.len()
                        || motion_part_ids != expected_part_ids
                        || restore.part_motion.iter().any(|motion| {
                            !motion
                                .linear_velocity_m_s
                                .iter()
                                .chain(&motion.angular_velocity_rad_s)
                                .all(|value| value.is_finite())
                        })
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                    let birth = self.prepare_articulated_birth(
                        restore.root_translation_m,
                        restore.root_rotation_xyzw,
                        restore.topology_token,
                        restore.parts,
                        representation,
                        true,
                        Some(&stable_density),
                    )?;
                    let mut part_motion = restore.part_motion;
                    part_motion.sort_by_key(|motion| motion.part_id);
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| !volume_ids.insert(*id))
                    {
                        return Err(ArticulatedError::DuplicateVolume);
                    }
                    primitives = primitives
                        .checked_add(birth.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::AliveRestore {
                        operation_tag,
                        birth,
                        restore: PreparedAliveArticulatedMetadata {
                            pose_token: restore.pose_token,
                            source_generation: restore.source_generation,
                            next_component_serial: restore.next_component_serial,
                            root_linear_velocity_m_s: restore.root_linear_velocity_m_s,
                            root_angular_velocity_rad_s: restore.root_angular_velocity_rad_s,
                            part_motion,
                            owner_key: restore.owner_key,
                        },
                    });
                }
                ArticulatedBatchOperation::ColdRestore {
                    operation_tag,
                    restore,
                    representation,
                } => {
                    let owner = self.prepare_corpse_restore_owner(
                        restore,
                        representation,
                        &stable_density,
                    )?;
                    if !owner_keys.insert(owner.owner_key)
                        || owner
                            .source_volumes
                            .iter()
                            .any(|id| !volume_ids.insert(*id))
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                    primitives = primitives
                        .checked_add(owner.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    derived_dense = derived_dense
                        .checked_add(owner.derived_dense)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES
                        || derived_dense > MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
                    {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::ColdRestore {
                        operation_tag,
                        owner,
                    });
                }
                ArticulatedBatchOperation::CheckedDeathTransfer {
                    operation_tag,
                    id,
                    expected_topology_token,
                    expected_pose_token,
                } => {
                    if !transfer_ids.insert(id.0) {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                    let transfer = self.prepare_dynamic_corpse_transfer(
                        id,
                        expected_topology_token,
                        expected_pose_token,
                        stable_density,
                    )?;
                    if transfer
                        .source_volume_ids
                        .iter()
                        .any(|source| !volume_ids.insert(*source))
                    {
                        return Err(ArticulatedError::DuplicateVolume);
                    }
                    primitives = primitives
                        .checked_add(transfer.primitive_count)
                        .ok_or(ArticulatedError::WorkBudget)?;
                    if primitives > MAX_CORPSE_RESTORE_BATCH_PRIMITIVES {
                        return Err(ArticulatedError::WorkBudget);
                    }
                    changes.push(PreparedArticulatedChange::CheckedDeathTransfer {
                        operation_tag,
                        transfer,
                    });
                }
            }
        }
        Ok(PreparedArticulatedBatch {
            world_identity: self.preparation_identity.clone(),
            expected_step: self.step_count,
            expected_entries: self.entries.len(),
            changes,
        })
    }

    /// Revalidates the entire prepared batch before the first insertion.
    /// Stale or conflicting batches publish no owners and consume no handles.
    pub fn commit_dynamic_corpse_restore_batch(
        &mut self,
        prepared: PreparedDynamicCorpseBatch,
    ) -> Result<Vec<ArticulatedBodyId>, crate::articulated::ArticulatedError> {
        self.commit_articulated_batch(prepared).map(|outcomes| {
            outcomes
                .into_iter()
                .map(|outcome| outcome.owner_id)
                .collect()
        })
    }

    /// Revalidates every operation and cross-operation reservation before
    /// publishing any member. Publication has no fallible checks or Result
    /// paths after the first Rapier mutation.
    pub fn commit_articulated_batch(
        &mut self,
        prepared: PreparedArticulatedBatch,
    ) -> Result<Vec<ArticulatedBatchOutcome>, crate::articulated::ArticulatedError> {
        let validated = self.validate_articulated_batch(prepared)?;
        Ok(self.publish_validated_articulated_batch(validated))
    }

    /// Performs every fallible freshness, identity, source and cross-member
    /// check. The returned private token may be published after a separately
    /// prepared geometry batch mutates unrelated physics entries.
    pub fn validate_articulated_batch(
        &self,
        prepared: PreparedArticulatedBatch,
    ) -> Result<ValidatedArticulatedBatch, crate::articulated::ArticulatedError> {
        self.validate_articulated_batch_with_reserved_body_slots(prepared, 0)
    }

    /// Jointly checks this batch and an external reservation against the
    /// remaining logical `BodyId` space. `external_additions` is a bounded
    /// upper allowance for unrelated, already-prepared geometry entries that
    /// the caller will publish after all participants validate and before this
    /// articulated batch. It does not allocate or reserve IDs itself; the
    /// caller owns the other participants' registry, source and journal checks
    /// and must not publish more entries than the stated allowance.
    pub fn validate_articulated_batch_with_reserved_body_slots(
        &self,
        prepared: PreparedArticulatedBatch,
        external_additions: usize,
    ) -> Result<ValidatedArticulatedBatch, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if !std::sync::Arc::ptr_eq(&prepared.world_identity, &self.preparation_identity)
            || prepared.expected_step != self.step_count
            || prepared.expected_entries != self.entries.len()
        {
            return Err(ArticulatedError::StalePreparedBatch);
        }
        let final_entries = self
            .entries
            .len()
            .checked_add(prepared.additional_body_slots())
            .and_then(|entries| entries.checked_add(external_additions))
            .ok_or(ArticulatedError::WorkBudget)?;
        if final_entries > u32::MAX as usize {
            return Err(ArticulatedError::WorkBudget);
        }
        let live_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|owner| owner.source_volume_ids.iter().copied())
            .collect();
        let mut batch_owner_keys = std::collections::BTreeSet::new();
        let mut batch_volumes = std::collections::BTreeSet::new();
        for change in &prepared.changes {
            match change {
                PreparedArticulatedChange::ColdRestore { owner, .. } => {
                    if self.fracture_owner_keys.contains_key(&owner.owner_key)
                        || self
                            .articulated
                            .values()
                            .any(|live| live.fracture_owner_key == Some(owner.owner_key))
                        || owner
                            .source_volumes
                            .iter()
                            .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                        || !batch_owner_keys.insert(owner.owner_key)
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::AliveBirth { birth, .. } => {
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::AliveRestore { birth, restore, .. } => {
                    if self.fracture_owner_keys.contains_key(&restore.owner_key)
                        || self
                            .articulated
                            .values()
                            .any(|live| live.fracture_owner_key == Some(restore.owner_key))
                        || !batch_owner_keys.insert(restore.owner_key)
                        || birth
                            .source_volume_ids
                            .iter()
                            .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::InvalidRestore);
                    }
                }
                PreparedArticulatedChange::DynamicCorpseBirth { birth, .. } => {
                    if birth
                        .source_volume_ids
                        .iter()
                        .any(|id| live_volumes.contains(id) || !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
                PreparedArticulatedChange::CheckedDeathTransfer { transfer, .. } => {
                    if !self.validate_prepared_corpse_transfer(transfer) {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                    if transfer
                        .source_volume_ids
                        .iter()
                        .any(|id| !batch_volumes.insert(*id))
                    {
                        return Err(ArticulatedError::StalePreparedBatch);
                    }
                }
            }
        }
        Ok(ValidatedArticulatedBatch { prepared })
    }

    /// Publishes a previously validated batch without another fallible path.
    /// The token must be published on its creating world at the same tick,
    /// after joint validation and before unrelated mutation. When composing
    /// with geometry, only the already-validated geometry publication may
    /// intervene. Publishing on another world is programmer error and panics
    /// before this batch mutates either solver structures or owner indexes.
    pub fn publish_validated_articulated_batch(
        &mut self,
        validated: ValidatedArticulatedBatch,
    ) -> Vec<ArticulatedBatchOutcome> {
        assert!(
            std::sync::Arc::ptr_eq(
                &validated.prepared.world_identity,
                &self.preparation_identity
            ) && validated.prepared.expected_step == self.step_count,
            "validated articulated batch belongs to another PhysicsWorld or tick"
        );
        let prepared = validated.prepared;
        let mut outcomes = Vec::with_capacity(prepared.changes.len());
        // All bounds, reservations and freshness checks have completed.
        // Publication below contains no fallible preparation or validation.
        for change in prepared.changes {
            match change {
                PreparedArticulatedChange::AliveBirth {
                    operation_tag,
                    birth,
                } => {
                    let owner_id = self.publish_articulated_birth(birth);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::AliveRestore {
                    operation_tag,
                    birth,
                    restore,
                } => {
                    let owner_id = self.publish_alive_articulated_restore(birth, restore);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::DynamicCorpseBirth {
                    operation_tag,
                    birth,
                } => {
                    let owner_id = self.publish_articulated_birth(birth);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::ColdRestore {
                    operation_tag,
                    owner,
                } => {
                    let owner_id = self.publish_corpse_restore_owner(owner);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id,
                        transfer: None,
                    });
                }
                PreparedArticulatedChange::CheckedDeathTransfer {
                    operation_tag,
                    transfer,
                } => {
                    let outcome = self.publish_prepared_corpse_transfer(transfer);
                    outcomes.push(ArticulatedBatchOutcome {
                        operation_tag,
                        owner_id: outcome.0,
                        transfer: Some(outcome.1),
                    });
                }
            }
        }
        outcomes
    }

    fn prepare_articulated_birth(
        &self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        kinematic_part_bodies: bool,
        density: Option<&dyn Fn(MaterialId) -> f64>,
    ) -> Result<PreparedArticulatedBirth, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        if parts.is_empty()
            || parts.len() > MAX_ARTICULATED_PARTS
            || !articulated_translation_in_range(translation_m)
            || !(PartTransform {
                translation_m: [0.0; 3],
                rotation_xyzw,
            })
            .is_valid()
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let root_rotation = normalize(rotation_xyzw);
        let live_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|owner| owner.source_volume_ids.iter().copied())
            .collect();
        let mut sorted = parts;
        sorted.sort_by_key(|part| part.part_id);
        let mut ids = std::collections::BTreeSet::new();
        let mut source_volume_ids = std::collections::BTreeSet::new();
        let mut roots = 0usize;
        let mut dense = 0u64;
        let mut solids = 0u64;
        let cell_m = sorted[0].cell_m;
        for part in &mut sorted {
            if part.part_id == 0 || !ids.insert(part.part_id) {
                return Err(ArticulatedError::DuplicatePart);
            }
            if !source_volume_ids.insert(part.volume_id) || live_volumes.contains(&part.volume_id) {
                return Err(ArticulatedError::DuplicateVolume);
            }
            roots += usize::from(part.parent_id.is_none());
            if !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || (part.cell_m - cell_m).abs() > 1e-7
                || !part.local_transform.is_valid()
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            if !articulated_translation_in_range(part.local_transform.translation_m)
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::GeometryRange);
            }
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidTransform)?;
            let dims = part.grid.dims();
            let part_dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|n| n.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense = dense
                .checked_add(part_dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solids = solids
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
            if part.grid.solid_count() == 0 {
                return Err(ArticulatedError::EmptyPart(part.part_id));
            }
            if dense > MAX_ARTICULATED_DENSE_CELLS || solids > MAX_ARTICULATED_SOLID_CELLS {
                return Err(ArticulatedError::WorkBudget);
            }
        }
        if roots != 1
            || sorted
                .iter()
                .any(|part| part.parent_id.is_some_and(|parent| !ids.contains(&parent)))
        {
            return Err(ArticulatedError::Hierarchy);
        }
        let flattened = flatten_source_parts(&sorted)?;
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let manifest = density
            .map(|density| density_manifest(&sorted, density))
            .transpose()?;
        let mass = if kinematic_part_bodies {
            None
        } else {
            let manifest_ref = manifest.as_ref().ok_or(ArticulatedError::InvalidDensity)?;
            let stable_density = |material| manifest_density(manifest_ref, material);
            let mass =
                crate::mass::articulated_mass_properties(&sorted, &flattened, stable_density)
                    .ok_or(ArticulatedError::InvalidTransform)?
                    .to_body_properties();
            validate_body_mass_properties(mass)?;
            Some(mass)
        };
        let builds = sorted
            .iter()
            .map(|part| build_collider(&part.grid, cell_m, representation))
            .collect::<Vec<_>>();
        let primitive_count = builds.iter().map(|build| build.primitives).sum::<usize>();
        if builds
            .iter()
            .any(|build| build.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
            || primitive_count > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let collider_poses = sorted
            .iter()
            .zip(&flattened)
            .map(|(part, pose)| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                let offset = rotate(pose.rotation_xyzw, origin_m);
                (
                    [
                        pose.translation_m[0] + offset[0],
                        pose.translation_m[1] + offset[1],
                        pose.translation_m[2] + offset[2],
                    ],
                    pose.rotation_xyzw,
                )
            })
            .collect::<Vec<_>>();
        if collider_poses.iter().any(|(local, rotation)| {
            !articulated_translation_in_range(*local)
                || !rotation.iter().all(|v| v.is_finite())
                || {
                    let world = rotate(root_rotation, *local);
                    !articulated_translation_in_range([
                        translation_m[0] + world[0],
                        translation_m[1] + world[1],
                        translation_m[2] + world[2],
                    ])
                }
        }) {
            return Err(ArticulatedError::GeometryRange);
        }
        Ok(PreparedArticulatedBirth {
            translation_m,
            rotation_xyzw: root_rotation,
            topology_token,
            parts: sorted,
            flattened,
            collider_poses,
            builds,
            source_volume_ids,
            mass,
            manifest,
            kinematic_part_bodies,
            representation,
            primitive_count,
        })
    }

    fn publish_articulated_birth(&mut self, birth: PreparedArticulatedBirth) -> ArticulatedBodyId {
        let PreparedArticulatedBirth {
            translation_m,
            rotation_xyzw,
            topology_token,
            parts,
            flattened,
            collider_poses,
            builds,
            source_volume_ids,
            mass,
            manifest,
            kinematic_part_bodies,
            representation,
            ..
        } = birth;
        let pose = Pose::new(
            Vector::from(translation_m),
            Rotation::from_xyzw(
                rotation_xyzw[0],
                rotation_xyzw[1],
                rotation_xyzw[2],
                rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let root_builder = if let Some(mass) = mass {
            RigidBodyBuilder::dynamic()
                .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
        } else {
            RigidBodyBuilder::kinematic_position_based()
        };
        let root = self.bodies.insert(root_builder.pose(pose).build());
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let cell_m = parts[0].cell_m;
        let mut built_parts = Vec::with_capacity(parts.len());
        for (((part, assembly_pose), (local_translation, local_rotation)), build) in parts
            .into_iter()
            .zip(flattened)
            .zip(collider_poses)
            .zip(builds)
        {
            let local_rotation = Rotation::from_xyzw(
                local_rotation[0],
                local_rotation[1],
                local_rotation[2],
                local_rotation[3],
            );
            let (part_body, collider_pose) = if kinematic_part_bodies {
                let world_pose = compose(
                    PartTransform {
                        translation_m,
                        rotation_xyzw,
                    },
                    assembly_pose,
                );
                let part_body = self.bodies.insert(
                    RigidBodyBuilder::kinematic_position_based()
                        .pose(Pose::new(
                            Vector::from(world_pose.translation_m),
                            Rotation::from_xyzw(
                                world_pose.rotation_xyzw[0],
                                world_pose.rotation_xyzw[1],
                                world_pose.rotation_xyzw[2],
                                world_pose.rotation_xyzw[3],
                            )
                            .to_scaled_axis(),
                        ))
                        .build(),
                );
                let origin = part.grid.origin();
                (
                    part_body,
                    Pose::from_translation(Vector::new(
                        origin.x as f32 * cell_m,
                        origin.y as f32 * cell_m,
                        origin.z as f32 * cell_m,
                    )),
                )
            } else {
                (
                    root,
                    Pose::new(
                        Vector::from(local_translation),
                        local_rotation.to_scaled_axis(),
                    ),
                )
            };
            let mut collider_builder = ColliderBuilder::new(build.collider.shared_shape().clone())
                .density(0.0)
                .position(collider_pose);
            if kinematic_part_bodies {
                collider_builder = collider_builder.active_collision_types(
                    ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
                );
            }
            let collider = collider_builder.build();
            let handle = self
                .colliders
                .insert_with_parent(collider, part_body, &mut self.bodies);
            self.pending_modified.push(handle);
            let part_surface_radius_m = source_surface_radius_m(&part.grid, cell_m, [0.0; 3], None);
            let root_surface_radius_m =
                source_surface_radius_m(&part.grid, cell_m, [0.0; 3], Some(assembly_pose));
            built_parts.push(ArticulatedPartEntry {
                part_id: part.part_id,
                parent_id: part.parent_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid,
                cell_m,
                local_pose: part.local_transform,
                assembly_pose,
                part_surface_radius_m,
                root_surface_radius_m,
                body: part_body,
                collider: handle,
                primitives: build.primitives,
                component_id: None,
            });
        }
        let first_origin = built_parts[0].grid.origin();
        let primary = built_parts[0].collider;
        self.entries.push(Entry {
            body: root,
            collider: primary,
            cell_m,
            representation,
            density: 0.0,
            collider_offset_m: [
                first_origin.x as f32 * cell_m,
                first_origin.y as f32 * cell_m,
                first_origin.z as f32 * cell_m,
            ],
            mass_properties: mass,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        for part in &built_parts {
            self.tag_collider(part.collider, id);
        }
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token,
                pose_token: 0,
                root_pose: PartTransform {
                    translation_m,
                    rotation_xyzw,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies,
                velocity_based_restore: false,
                dynamic_corpse: mass.is_some(),
                source_generation: 1,
                fracture_owner_key: None,
                next_component_serial: 1,
                density_manifest: manifest,
                source_volume_ids,
                parts: built_parts,
            },
        );
        if let Some(mass) = mass {
            let pose = *self.bodies[root].position();
            self.bodies[root]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[root].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[root].set_position(pose, true);
        }
        ArticulatedBodyId(id)
    }

    fn publish_alive_articulated_restore(
        &mut self,
        birth: PreparedArticulatedBirth,
        restore: PreparedAliveArticulatedMetadata,
    ) -> ArticulatedBodyId {
        let PreparedAliveArticulatedMetadata {
            pose_token,
            source_generation,
            next_component_serial,
            root_linear_velocity_m_s,
            root_angular_velocity_rad_s,
            part_motion,
            owner_key,
        } = restore;
        let id = self.publish_articulated_birth(birth);
        let root_body = self.entries[entry_index(id.0, self.id_namespace)].body;
        self.bodies[root_body].set_body_type(RigidBodyType::KinematicVelocityBased, true);
        let root_linear = Vector::from(root_linear_velocity_m_s);
        let root_angular = Vector::from(root_angular_velocity_rad_s);
        self.bodies[root_body].set_linvel(root_linear, true);
        self.bodies[root_body].set_angvel(root_angular, true);

        let assembly = self
            .articulated
            .get_mut(&id.0)
            .expect("restore birth publication inserted its owner");
        assembly.pose_token = pose_token;
        assembly.source_generation = source_generation;
        assembly.next_component_serial = next_component_serial;
        assembly.fracture_owner_key = Some(owner_key);
        assembly.velocity_based_restore = true;
        assembly.pending_root_pose_token = None;
        assembly.pending_pose = None;
        self.restored_velocity_based_owners.push(id);
        assert_eq!(part_motion.len(), assembly.parts.len());
        for (part, motion) in assembly.parts.iter().zip(part_motion) {
            assert_eq!(part.part_id, motion.part_id);
            let body = self
                .bodies
                .get_mut(part.body)
                .expect("restore birth publication inserted each part body");
            body.set_body_type(RigidBodyType::KinematicVelocityBased, true);
            body.set_linvel(Vector::from(motion.linear_velocity_m_s), true);
            body.set_angvel(Vector::from(motion.angular_velocity_rad_s), true);
        }
        self.fracture_owner_keys.insert(owner_key, id.0);
        id
    }
}
