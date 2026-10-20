use super::*;

impl PhysicsWorld {
    /// Captures one live physical component for bounded immutable fracture
    /// planning. After the initial split every capture targets one child ID.
    pub fn capture_dynamic_corpse_fracture_snapshot(
        &self,
        id: ArticulatedBodyId,
        corpse_key: u128,
        component_key: Option<DerivedFractureComponentId>,
    ) -> Result<FractureSnapshot, FractureError> {
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse
            || corpse_key == 0
            || self
                .fracture_owner_keys
                .get(&corpse_key)
                .is_some_and(|owner| *owner != id.0)
            || assembly
                .fracture_owner_key
                .is_some_and(|key| key != corpse_key)
            || component_key.is_some_and(|key| {
                key.owner_key != corpse_key
                    || key.serial == 0
                    || key.serial >= assembly.next_component_serial
            })
        {
            return Err(FractureError::StaleOwner);
        }
        let selected = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == component_key)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return Err(FractureError::InvalidSnapshot);
        }
        let mut part_ids = HashSet::new();
        let mut volume_ids = HashSet::new();
        if selected
            .iter()
            .any(|part| !part_ids.insert(part.part_id) || !volume_ids.insert(part.volume_id))
        {
            return Err(FractureError::InvalidSnapshot);
        }
        Ok(FractureSnapshot {
            token: FractureSnapshotToken {
                corpse_key,
                topology_revision: assembly.topology_token,
                pose_epoch: self.step_count,
                source_generation: assembly.source_generation,
                component_key,
            },
            parts: selected
                .into_iter()
                .map(|part| FracturePartSnapshot {
                    source: SourcePart {
                        part_id: part.part_id,
                        parent_id: None,
                        volume_id: part.volume_id,
                        source_revision: part.source_revision,
                        grid: part.grid.clone(),
                        cell_m: part.cell_m,
                        local_transform: part.assembly_pose,
                    },
                    root_local_pose: part.assembly_pose,
                })
                .collect(),
        })
    }

    /// Captures all current components of one corpse as a value-only cold-
    /// restore DTO. `canonical_sources` must come from the owning Simulation;
    /// physics retains only the live shards and source-volume IDs after
    /// fracture, so it cannot reconstruct original hierarchy or empty source
    /// bindings. For an owner not yet bound in the physics fracture-key index
    /// (the intact checked-death case), `expected.owner_key` is the caller's
    /// authoritative key; this method validates that it is unclaimed and does
    /// not bind or otherwise mutate it.
    ///
    /// The source transforms supplied here are the current frozen local
    /// transforms expected by `DynamicCorpseRestore`, not authored rest data.
    /// Authored/rest metadata remains owned by Simulation and is not captured
    /// or reconstructed by this physics API. Canonical source descriptors must
    /// be sorted by stable part ID, matching the portable owner record.
    pub fn capture_dynamic_corpse_owner(
        &self,
        id: ArticulatedBodyId,
        expected: crate::articulated::DynamicCorpseOwnerFence,
        canonical_sources: &[SourcePart],
    ) -> Result<crate::articulated::DynamicCorpseRestore, crate::articulated::ArticulatedError>
    {
        self.capture_dynamic_corpse_owner_at_epoch(
            id,
            expected,
            canonical_sources,
            expected.pose_token,
        )
    }

    /// Captures against the actual runtime authored token while mapping the
    /// restored assembly to its host's authoritative logical pose epoch.
    /// Initial runtime token zero is valid; the portable epoch stays nonzero.
    pub fn capture_dynamic_corpse_owner_at_epoch(
        &self,
        id: ArticulatedBodyId,
        expected: crate::articulated::DynamicCorpseOwnerFence,
        canonical_sources: &[SourcePart],
        portable_pose_epoch: u64,
    ) -> Result<crate::articulated::DynamicCorpseRestore, crate::articulated::ArticulatedError>
    {
        use crate::articulated::{
            ArticulatedError, DynamicCorpseComponentRestore, DynamicCorpseRestore,
            MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_PARTS, MAX_ARTICULATED_SOLID_CELLS,
        };
        use std::collections::{BTreeMap, BTreeSet};

        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::InvalidRestore)?;
        if !assembly.dynamic_corpse
            || expected.owner_key == 0
            || expected.source_generation == 0
            || expected.topology_revision == 0
            || portable_pose_epoch == 0
            || expected.next_component_serial == 0
            || match assembly.fracture_owner_key {
                Some(key) => {
                    key != expected.owner_key || self.fracture_owner_keys.get(&key) != Some(&id.0)
                }
                None => self.fracture_owner_keys.contains_key(&expected.owner_key),
            }
            || assembly.source_generation != expected.source_generation
            || assembly.topology_token != expected.topology_revision
            || assembly.pose_token != expected.pose_token
            || assembly.next_component_serial != expected.next_component_serial
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
            || canonical_sources.is_empty()
            || canonical_sources.len() > MAX_ARTICULATED_PARTS
            || assembly.parts.is_empty()
            || assembly.parts.len() > MAX_ARTICULATED_PARTS
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        // Count all source and retained-shard work before building lookup maps,
        // output cell vectors, or cloning any grids into the returned DTO.
        let mut source_dense = 0_u64;
        let mut source_roots = 0_usize;
        let mut previous_part_id = None;
        for part in canonical_sources {
            if part.part_id == 0
                || previous_part_id.is_some_and(|previous| previous >= part.part_id)
                || part.source_revision == 0
                || !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || !part.local_transform.is_valid()
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            previous_part_id = Some(part.part_id);
            source_roots += usize::from(part.parent_id.is_none());
            let dims = part.grid.dims();
            source_dense = source_dense
                .checked_add(
                    u64::from(dims[0])
                        .checked_mul(u64::from(dims[1]))
                        .and_then(|v| v.checked_mul(u64::from(dims[2])))
                        .ok_or(ArticulatedError::WorkBudget)?,
                )
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        let mut shard_dense = 0_u64;
        for part in &assembly.parts {
            let dims = part.grid.dims();
            shard_dense = shard_dense
                .checked_add(
                    u64::from(dims[0])
                        .checked_mul(u64::from(dims[1]))
                        .and_then(|v| v.checked_mul(u64::from(dims[2])))
                        .ok_or(ArticulatedError::WorkBudget)?,
                )
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if source_dense > MAX_ARTICULATED_DENSE_CELLS
            || shard_dense > MAX_ARTICULATED_DENSE_CELLS
            || source_dense
                .checked_add(shard_dense)
                .is_none_or(|total| total > MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS)
        {
            return Err(ArticulatedError::WorkBudget);
        }

        let mut source_solids = 0_u64;
        let mut shard_solids = 0_u64;
        for part in canonical_sources {
            source_solids = source_solids
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        for part in &assembly.parts {
            shard_solids = shard_solids
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        let output_mask_cells = shard_solids;
        if source_solids == 0
            || source_roots != 1
            || source_solids > MAX_ARTICULATED_SOLID_CELLS
            || shard_solids > MAX_ARTICULATED_SOLID_CELLS
            || source_solids
                .checked_add(shard_solids)
                .and_then(|total| total.checked_add(output_mask_cells))
                .is_none_or(|total| total > MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS)
        {
            return Err(ArticulatedError::WorkBudget);
        }
        if shard_solids != source_solids {
            return Err(ArticulatedError::InvalidRestore);
        }

        let mut source_input_ids = BTreeSet::new();
        let mut source_volumes = BTreeSet::new();
        for part in canonical_sources {
            if !source_input_ids.insert(part.part_id) || !source_volumes.insert(part.volume_id) {
                return Err(ArticulatedError::InvalidRestore);
            }
        }
        if source_volumes != assembly.source_volume_ids {
            return Err(ArticulatedError::InvalidRestore);
        }
        if canonical_sources.iter().any(|part| {
            part.parent_id
                .is_some_and(|parent| !source_input_ids.contains(&parent))
        }) {
            return Err(ArticulatedError::Hierarchy);
        }

        // Cold restore canonicalizes each local quaternion before flattening;
        // capture must use the same canonical source transforms for equality.
        let mut normalized_sources = canonical_sources.to_vec();
        for part in &mut normalized_sources {
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidRestore)?;
        }
        let mut source_parts_by_id = BTreeMap::new();
        for part in &normalized_sources {
            source_parts_by_id.insert(part.part_id, part);
        }
        let mut shard_part_ids = BTreeSet::new();
        let mut shard_component_keys = BTreeSet::new();
        for part in &assembly.parts {
            if !shard_part_ids.insert((part.part_id, part.component_id)) {
                return Err(ArticulatedError::InvalidRestore);
            }
            if let Some(component_id) = part.component_id {
                if component_id.owner_key != expected.owner_key
                    || component_id.serial == 0
                    || component_id.serial >= assembly.next_component_serial
                {
                    return Err(ArticulatedError::InvalidRestore);
                }
                shard_component_keys.insert(component_id);
            }
        }
        if shard_component_keys.len() > crate::fracture::MAX_FRACTURE_COMPONENTS {
            return Err(ArticulatedError::WorkBudget);
        }

        let frozen_poses = flatten_source_parts(&normalized_sources)?;
        if frozen_poses
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let frozen_by_part = normalized_sources
            .iter()
            .zip(frozen_poses)
            .map(|(part, pose)| (part.part_id, pose))
            .collect::<BTreeMap<_, _>>();

        // Build the canonical provenance index only after the cumulative
        // source/shard bounds above have been checked.
        let mut expected_cells = BTreeSet::new();
        for part in &normalized_sources {
            let origin = part.grid.origin();
            let mut range_error = false;
            part.grid.for_each_solid(|x, y, z, material| {
                let coords = [
                    origin.x.checked_add(i64::from(x)),
                    origin.y.checked_add(i64::from(y)),
                    origin.z.checked_add(i64::from(z)),
                ];
                let [Some(x), Some(y), Some(z)] =
                    coords.map(|v| v.and_then(|coordinate| i32::try_from(coordinate).ok()))
                else {
                    range_error = true;
                    return;
                };
                if !expected_cells.insert(SourceCellRef {
                    volume_id: part.volume_id,
                    part_id: part.part_id,
                    cell: [x, y, z],
                    material,
                    source_revision: part.source_revision,
                }) {
                    range_error = true;
                }
            });
            if range_error {
                return Err(ArticulatedError::InvalidRestore);
            }
        }

        let density_manifest = assembly
            .density_manifest
            .as_ref()
            .ok_or(ArticulatedError::InvalidDensity)?;
        let mut assigned = BTreeSet::new();
        let mut component_parts =
            BTreeMap::<Option<DerivedFractureComponentId>, Vec<&ArticulatedPartEntry>>::new();
        for part in &assembly.parts {
            let Some(source) = source_parts_by_id.get(&part.part_id).copied() else {
                return Err(ArticulatedError::InvalidRestore);
            };
            let Some(expected_frozen_pose) = frozen_by_part.get(&part.part_id) else {
                return Err(ArticulatedError::InvalidRestore);
            };
            if source.volume_id != part.volume_id
                || source.source_revision != part.source_revision
                || source.cell_m.to_bits() != part.cell_m.to_bits()
                || part.assembly_pose.translation_m != expected_frozen_pose.translation_m
                || part.assembly_pose.rotation_xyzw != expected_frozen_pose.rotation_xyzw
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            component_parts
                .entry(part.component_id)
                .or_default()
                .push(part);
        }
        if component_parts.is_empty()
            || (component_parts.len() == 1
                && component_parts.keys().next().is_some_and(Option::is_some)
                && assembly.next_component_serial == 1)
            || (component_parts.len() == 1
                && component_parts.keys().next().is_some_and(Option::is_none)
                && assembly.next_component_serial != 1)
            || (component_parts.len() > 1 && component_parts.contains_key(&None))
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        let mut components = Vec::with_capacity(component_parts.len());
        let entry_body = self
            .entries
            .get(entry_index(id.0, self.id_namespace))
            .ok_or(ArticulatedError::InvalidRestore)?
            .body;
        let mut component_bodies = HashSet::new();
        let mut primary_component = None;
        for (component_id, parts) in component_parts {
            let first = parts.first().ok_or(ArticulatedError::InvalidRestore)?;
            if parts.iter().any(|part| part.body != first.body)
                || !component_bodies.insert(first.body)
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            if first.body == entry_body {
                primary_component = Some(component_id);
            }
            let body = self
                .bodies
                .get(first.body)
                .ok_or(ArticulatedError::InvalidRestore)?;
            if !body.is_dynamic() {
                return Err(ArticulatedError::InvalidRestore);
            }
            let translation = body.translation();
            let rotation = body.rotation();
            let linear = body.linvel();
            let angular = body.angvel();
            let translation_m = [translation.x, translation.y, translation.z];
            let rotation_xyzw = [rotation.x, rotation.y, rotation.z, rotation.w];
            let linear_velocity_m_s = [linear.x, linear.y, linear.z];
            let angular_velocity_rad_s = [angular.x, angular.y, angular.z];
            if !translation_m
                .iter()
                .chain(&rotation_xyzw)
                .chain(&linear_velocity_m_s)
                .chain(&angular_velocity_rad_s)
                .all(|value| value.is_finite())
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            if !articulated_translation_in_range(translation_m)
                || !(PartTransform {
                    translation_m: [0.0; 3],
                    rotation_xyzw,
                })
                .is_valid()
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            let body_rotation = normalize(rotation_xyzw);
            let mut cells = Vec::with_capacity(
                parts
                    .iter()
                    .map(|part| part.grid.solid_count() as usize)
                    .sum(),
            );
            let mut packed_dense = 0_u64;
            for part in parts {
                let source = source_parts_by_id
                    .get(&part.part_id)
                    .copied()
                    .ok_or(ArticulatedError::InvalidRestore)?;
                let origin = part.grid.origin();
                let mut packed_bounds = None::<([i32; 3], [i32; 3])>;
                let mut invalid = false;
                part.grid.for_each_solid(|x, y, z, material| {
                    let coords = [
                        origin.x.checked_add(i64::from(x)),
                        origin.y.checked_add(i64::from(y)),
                        origin.z.checked_add(i64::from(z)),
                    ];
                    let [Some(x), Some(y), Some(z)] =
                        coords.map(|v| v.and_then(|coordinate| i32::try_from(coordinate).ok()))
                    else {
                        invalid = true;
                        return;
                    };
                    let cell = SourceCellRef {
                        volume_id: part.volume_id,
                        part_id: part.part_id,
                        cell: [x, y, z],
                        material,
                        source_revision: part.source_revision,
                    };
                    if source.volume_id != cell.volume_id
                        || source.source_revision != cell.source_revision
                        || !expected_cells.contains(&cell)
                        || !assigned.insert(cell)
                        || density_manifest.get(&material).is_none_or(|bits| {
                            !f64::from_bits(*bits).is_finite() || f64::from_bits(*bits) <= 0.0
                        })
                    {
                        invalid = true;
                        return;
                    }
                    packed_bounds = Some(match packed_bounds {
                        Some((minimum, maximum)) => (
                            [minimum[0].min(x), minimum[1].min(y), minimum[2].min(z)],
                            [maximum[0].max(x), maximum[1].max(y), maximum[2].max(z)],
                        ),
                        None => ([x, y, z], [x, y, z]),
                    });
                    cells.push(cell);
                });
                if invalid {
                    return Err(ArticulatedError::InvalidRestore);
                }
                let (minimum, maximum) = packed_bounds.ok_or(ArticulatedError::InvalidRestore)?;
                let dims = [
                    i64::from(maximum[0]) - i64::from(minimum[0]) + 1,
                    i64::from(maximum[1]) - i64::from(minimum[1]) + 1,
                    i64::from(maximum[2]) - i64::from(minimum[2]) + 1,
                ];
                let dense = dims
                    .into_iter()
                    .try_fold(1_u64, |total, dimension| {
                        total.checked_mul(u64::try_from(dimension).ok()?)
                    })
                    .ok_or(ArticulatedError::WorkBudget)?;
                packed_dense = packed_dense
                    .checked_add(dense)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if packed_dense > MAX_ARTICULATED_DENSE_CELLS {
                    return Err(ArticulatedError::WorkBudget);
                }
                let frozen_pose = frozen_by_part
                    .get(&part.part_id)
                    .ok_or(ArticulatedError::InvalidRestore)?;
                let packed_origin_m = [
                    minimum[0] as f32 * source.cell_m,
                    minimum[1] as f32 * source.cell_m,
                    minimum[2] as f32 * source.cell_m,
                ];
                let local_offset = rotate(frozen_pose.rotation_xyzw, packed_origin_m);
                let local_translation = [
                    frozen_pose.translation_m[0] + local_offset[0],
                    frozen_pose.translation_m[1] + local_offset[1],
                    frozen_pose.translation_m[2] + local_offset[2],
                ];
                let world_offset = rotate(body_rotation, local_translation);
                let world_translation = [
                    translation_m[0] + world_offset[0],
                    translation_m[1] + world_offset[1],
                    translation_m[2] + world_offset[2],
                ];
                if !articulated_translation_in_range(local_translation)
                    || !articulated_translation_in_range(world_translation)
                {
                    return Err(ArticulatedError::GeometryRange);
                }
            }
            if cells.is_empty() {
                return Err(ArticulatedError::InvalidRestore);
            }
            cells.sort_unstable();
            components.push(DynamicCorpseComponentRestore {
                component_id,
                translation_m,
                rotation_xyzw,
                linear_velocity_m_s,
                angular_velocity_rad_s,
                cells,
            });
        }
        if assigned != expected_cells || primary_component.is_none() {
            return Err(ArticulatedError::InvalidRestore);
        }
        // Cold restore treats components[0] as the logical primary owner and
        // installs its body in the existing BodyId slot. Preserve the actual
        // current primary rather than assuming the lowest serial is primary.
        components.sort_by_key(|component| {
            (
                component.component_id != primary_component.unwrap(),
                component.component_id,
            )
        });

        Ok(DynamicCorpseRestore {
            owner_key: expected.owner_key,
            source_generation: expected.source_generation,
            topology_revision: expected.topology_revision,
            pose_epoch: portable_pose_epoch,
            next_component_serial: expected.next_component_serial,
            source_parts: normalized_sources,
            components,
        })
    }

    /// Atomically replaces exactly one frozen physical component with its
    /// prepared connected components. Source volumes stay under this one
    /// logical owner; each collider shard preserves original cell provenance.
    pub fn commit_dynamic_corpse_fracture(
        &mut self,
        id: ArticulatedBodyId,
        snapshot: &FractureSnapshot,
        plan: &PreparedFracturePlan,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<FractureCommitOutcome, FractureError> {
        let started = Instant::now();
        let body_id = id.0;
        let current = self.capture_dynamic_corpse_fracture_snapshot(
            id,
            snapshot.token.corpse_key,
            snapshot.token.component_key,
        )?;
        if snapshot.token != current.token || !fracture_snapshots_equal(snapshot, &current) {
            return Err(FractureError::StaleOwner);
        }
        if plan.version != FRACTURE_PLAN_VERSION
            || plan.token != snapshot.token
            || plan.components.len() < 2
        {
            return Err(FractureError::InvalidPlan);
        }
        let canonical = prepare_fracture(snapshot)?;
        if canonical.source_cells != plan.source_cells || canonical.components != plan.components {
            return Err(FractureError::InvalidPlan);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(FractureError::StaleOwner)?;
        let manifest = assembly
            .density_manifest
            .clone()
            .ok_or(FractureError::InvalidMass)?;
        if snapshot.parts.iter().any(|part| {
            let mut mismatch = false;
            part.source.grid.for_each_solid(|_, _, _, material| {
                mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
            mismatch
        }) {
            return Err(FractureError::InvalidMass);
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let target = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == snapshot.token.component_key)
            .cloned()
            .collect::<Vec<_>>();
        if assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || target.len() != snapshot.parts.len()
            || target.iter().zip(&snapshot.parts).any(|(part, snapshot)| {
                part.part_id != snapshot.source.part_id || part.body != target[0].body
            })
            || assembly
                .fracture_owner_key
                .is_some_and(|key| key != snapshot.token.corpse_key)
            || self.articulated.iter().any(|(other_id, other)| {
                *other_id != body_id && other.fracture_owner_key == Some(snapshot.token.corpse_key)
            })
            || self
                .fracture_owner_keys
                .get(&snapshot.token.corpse_key)
                .is_some_and(|owner| *owner != body_id)
        {
            return Err(FractureError::StaleOwner);
        }
        let next_serial = assembly
            .next_component_serial
            .checked_add(plan.components.len() as u64)
            .ok_or(FractureError::WorkBudget)?;
        let next_source_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(FractureError::WorkBudget)?;
        let next_topology_revision = assembly
            .topology_token
            .checked_add(1)
            .ok_or(FractureError::WorkBudget)?;
        let component_ids = (0..plan.components.len())
            .map(|offset| DerivedFractureComponentId {
                owner_key: snapshot.token.corpse_key,
                serial: assembly.next_component_serial + offset as u64,
            })
            .collect::<Vec<_>>();
        if component_ids.iter().any(|component| component.serial == 0) {
            return Err(FractureError::InvalidPlan);
        }
        let live_components = assembly
            .parts
            .iter()
            .filter_map(|part| part.component_id)
            .collect::<HashSet<_>>()
            .len()
            .max(1);
        if live_components
            .checked_sub(1)
            .and_then(|count| count.checked_add(plan.components.len()))
            .is_none_or(|count| count > crate::fracture::MAX_FRACTURE_COMPONENTS)
        {
            return Err(FractureError::WorkBudget);
        }
        let entry_idx = entry_index(body_id, self.id_namespace);
        let entry = &self.entries[entry_idx];
        let representation = entry.representation;
        let query_only = entry.query_only;
        let old_body = target[0].body;
        let old_pose = *self.bodies[old_body].position();
        let old_sources = target
            .iter()
            .map(|part| SourcePart {
                part_id: part.part_id,
                parent_id: None,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid.clone(),
                cell_m: part.cell_m,
                local_transform: part.assembly_pose,
            })
            .collect::<Vec<_>>();
        let old_poses = old_sources
            .iter()
            .map(|part| part.local_transform)
            .collect::<Vec<_>>();
        let old_mass =
            crate::mass::articulated_mass_properties(&old_sources, &old_poses, stable_density)
                .ok_or(FractureError::InvalidMass)?
                .to_body_properties();
        validate_body_mass_properties(old_mass).map_err(|_| FractureError::InvalidMass)?;
        // Re-derive from the canonical source cells. Rapier's cached COM can
        // be uninitialized on a newly-created aggregate and is not a source
        // ownership oracle.
        let old_com = old_pose.transform_point(Vector::new(
            old_mass.local_com_m[0],
            old_mass.local_com_m[1],
            old_mass.local_com_m[2],
        ));
        let old_linear = self.bodies[old_body].linvel();
        let old_angular = self.bodies[old_body].angvel();

        struct PreparedShard {
            part: ArticulatedPartEntry,
            collider: Collider,
        }
        struct PreparedChild {
            component_id: DerivedFractureComponentId,
            mass: BodyMassProperties,
            linear: Vector,
            angular: Vector,
            shards: Vec<PreparedShard>,
        }
        let mut prepared = Vec::with_capacity(plan.components.len());
        let mut primitive_count = 0usize;
        let mut derived_dense_count = 0usize;
        for (component_index, component) in plan.components.iter().enumerate() {
            let refs = component.cells.iter().copied().collect::<HashSet<_>>();
            if refs.len() != component.cells.len() || refs.is_empty() {
                return Err(FractureError::InvalidPlan);
            }
            let mut sources = Vec::new();
            let mut poses = Vec::new();
            let mut shards = Vec::new();
            for (part_index, old_part) in target.iter().enumerate() {
                let source = &snapshot.parts[part_index].source;
                let origin = old_part.grid.origin();
                let mut owned = Vec::new();
                old_part.grid.for_each_solid(|x, y, z, material| {
                    let coord = [
                        origin.x.checked_add(i64::from(x)),
                        origin.y.checked_add(i64::from(y)),
                        origin.z.checked_add(i64::from(z)),
                    ];
                    let Ok(cx) = i32::try_from(coord[0].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let Ok(cy) = i32::try_from(coord[1].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let Ok(cz) = i32::try_from(coord[2].unwrap_or(i64::MIN)) else {
                        return;
                    };
                    let cell = SourceCellRef {
                        volume_id: old_part.volume_id,
                        part_id: old_part.part_id,
                        cell: [cx, cy, cz],
                        material,
                        source_revision: old_part.source_revision,
                    };
                    if refs.contains(&cell) {
                        owned.push((x, y, z, material));
                    }
                });
                if owned.is_empty() {
                    continue;
                }
                let min = std::array::from_fn::<_, 3, _>(|axis| {
                    owned
                        .iter()
                        .map(|cell| [cell.0, cell.1, cell.2][axis])
                        .min()
                        .unwrap()
                });
                let max = std::array::from_fn::<_, 3, _>(|axis| {
                    owned
                        .iter()
                        .map(|cell| [cell.0, cell.1, cell.2][axis])
                        .max()
                        .unwrap()
                });
                let dims = [
                    max[0] - min[0] + 1,
                    max[1] - min[1] + 1,
                    max[2] - min[2] + 1,
                ];
                let dense = (dims[0] as usize)
                    .checked_mul(dims[1] as usize)
                    .and_then(|v| v.checked_mul(dims[2] as usize))
                    .ok_or(FractureError::WorkBudget)?;
                derived_dense_count = derived_dense_count
                    .checked_add(dense)
                    .ok_or(FractureError::WorkBudget)?;
                if derived_dense_count > crate::fracture::MAX_FRACTURE_DENSE_CELLS {
                    return Err(FractureError::WorkBudget);
                }
                let mut mask = vec![false; dense];
                let mut materials = vec![MaterialId::AIR; dense];
                for (x, y, z, material) in owned {
                    let local = [x - min[0], y - min[1], z - min[2]];
                    let index = local[0] as usize
                        + dims[0] as usize
                            * (local[1] as usize + dims[1] as usize * local[2] as usize);
                    mask[index] = true;
                    materials[index] = material;
                }
                let cropped_origin = GlobalCell::new(
                    origin.x + i64::from(min[0]),
                    origin.y + i64::from(min[1]),
                    origin.z + i64::from(min[2]),
                );
                let grid = OccupancyGrid::from_solid_mask(cropped_origin, dims, mask, materials)
                    .map_err(|_| FractureError::InvalidPlan)?;
                let source_part = SourcePart {
                    part_id: source.part_id,
                    parent_id: None,
                    volume_id: source.volume_id,
                    source_revision: source.source_revision,
                    grid: grid.clone(),
                    cell_m: source.cell_m,
                    local_transform: snapshot.parts[part_index].root_local_pose,
                };
                sources.push(source_part);
                poses.push(snapshot.parts[part_index].root_local_pose);
                let built = build_collider(&grid, old_part.cell_m, representation);
                primitive_count = primitive_count
                    .checked_add(built.primitives)
                    .ok_or(FractureError::WorkBudget)?;
                if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
                    || primitive_count > 65_536
                {
                    return Err(FractureError::WorkBudget);
                }
                let old_collider = self
                    .colliders
                    .get(old_part.collider)
                    .ok_or(FractureError::StaleOwner)?;
                let origin = grid.origin();
                let origin_m = [
                    origin.x as f32 * old_part.cell_m,
                    origin.y as f32 * old_part.cell_m,
                    origin.z as f32 * old_part.cell_m,
                ];
                let pose = snapshot.parts[part_index].root_local_pose;
                let offset = rotate(pose.rotation_xyzw, origin_m);
                let local = Pose::new(
                    Vector::new(
                        pose.translation_m[0] + offset[0],
                        pose.translation_m[1] + offset[1],
                        pose.translation_m[2] + offset[2],
                    ),
                    Rotation::from_xyzw(
                        pose.rotation_xyzw[0],
                        pose.rotation_xyzw[1],
                        pose.rotation_xyzw[2],
                        pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local)
                    .friction(old_collider.friction())
                    .active_collision_types(old_collider.active_collision_types())
                    .sensor(old_collider.is_sensor())
                    .build();
                collider.set_restitution(old_collider.restitution());
                collider.set_restitution_combine_rule(old_collider.restitution_combine_rule());
                collider.set_friction_combine_rule(old_collider.friction_combine_rule());
                collider.set_collision_groups(old_collider.collision_groups());
                collider.set_solver_groups(if query_only {
                    query_only_solver_groups()
                } else {
                    old_collider.solver_groups()
                });
                let part_surface_radius_m =
                    source_surface_radius_m(&grid, old_part.cell_m, [0.0; 3], None);
                let root_surface_radius_m =
                    source_surface_radius_m(&grid, old_part.cell_m, [0.0; 3], Some(pose));
                shards.push(PreparedShard {
                    part: ArticulatedPartEntry {
                        part_id: old_part.part_id,
                        parent_id: None,
                        volume_id: old_part.volume_id,
                        source_revision: old_part.source_revision,
                        grid,
                        cell_m: old_part.cell_m,
                        local_pose: pose,
                        assembly_pose: pose,
                        part_surface_radius_m,
                        root_surface_radius_m,
                        body: old_body,
                        collider: old_part.collider,
                        primitives: built.primitives,
                        component_id: Some(component_ids[component_index]),
                    },
                    collider,
                });
            }
            let mass = crate::mass::articulated_mass_properties(&sources, &poses, stable_density)
                .ok_or(FractureError::InvalidMass)?
                .to_body_properties();
            validate_body_mass_properties(mass).map_err(|_| FractureError::InvalidMass)?;
            let com = old_pose.transform_point(Vector::new(
                mass.local_com_m[0],
                mass.local_com_m[1],
                mass.local_com_m[2],
            ));
            let linear = old_linear + old_angular.cross(com - old_com);
            if shards.is_empty()
                || !linear.x.is_finite()
                || !linear.y.is_finite()
                || !linear.z.is_finite()
            {
                return Err(FractureError::InvalidMass);
            }
            prepared.push(PreparedChild {
                component_id: component_ids[component_index],
                mass,
                linear,
                angular: old_angular,
                shards,
            });
        }

        let mut total_parts = prepared
            .iter()
            .map(|child| child.shards.len())
            .sum::<usize>();
        let mut total_dense = derived_dense_count as u64;
        let mut total_solid = plan.source_cells.len() as u64;
        let mut total_primitives = primitive_count;
        for part in assembly
            .parts
            .iter()
            .filter(|part| part.component_id != snapshot.token.component_key)
        {
            total_parts = total_parts
                .checked_add(1)
                .ok_or(FractureError::WorkBudget)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|v| v.checked_mul(u64::from(dims[2])))
                .ok_or(FractureError::WorkBudget)?;
            total_dense = total_dense
                .checked_add(dense)
                .ok_or(FractureError::WorkBudget)?;
            total_solid = total_solid
                .checked_add(part.grid.solid_count())
                .ok_or(FractureError::WorkBudget)?;
            total_primitives = total_primitives
                .checked_add(part.primitives)
                .ok_or(FractureError::WorkBudget)?;
        }
        if total_parts > MAX_ARTICULATED_PARTS
            || total_dense > crate::articulated::MAX_ARTICULATED_DENSE_CELLS
            || total_solid > crate::articulated::MAX_ARTICULATED_SOLID_CELLS
            || total_primitives > 65_536
        {
            return Err(FractureError::WorkBudget);
        }

        // All partitions, shapes, masses, transforms, and stable IDs are now
        // validated. Only infallible Rapier insert/remove operations follow.
        let old_colliders = target.iter().map(|part| part.collider).collect::<Vec<_>>();
        let old_bodies = target.iter().map(|part| part.body).collect::<HashSet<_>>();
        let entry_body_was_replaced = old_bodies.contains(&self.entries[entry_idx].body);
        let mut committed = Vec::with_capacity(prepared.len());
        for child in prepared {
            let child_linear = child.linear;
            let child_angular = child.angular;
            let body = self.bodies.insert(
                RigidBodyBuilder::dynamic()
                    .pose(old_pose)
                    .additional_mass_properties(rapier_mass_properties(child.mass, [0.0; 3]))
                    .linvel(child.linear)
                    .angvel(child.angular)
                    .build(),
            );
            let mut parts = Vec::with_capacity(child.shards.len());
            for shard in child.shards {
                let handle =
                    self.colliders
                        .insert_with_parent(shard.collider, body, &mut self.bodies);
                self.tag_collider(handle, body_id);
                self.pending_modified.push(handle);
                let mut part = shard.part;
                part.body = body;
                part.collider = handle;
                parts.push(part);
            }
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            // Collider insertion/recomputation may shift Rapier's COM and
            // adjust the body's origin velocity. Install the prepared COM
            // velocity field only after the final mass frame is known.
            self.bodies[body].set_linvel(child_linear, true);
            self.bodies[body].set_angvel(child_angular, true);
            committed.push((child.component_id, child.mass, body, parts));
        }
        for collider in old_colliders {
            self.colliders
                .remove(collider, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(collider);
        }
        for body in old_bodies {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        if entry_body_was_replaced {
            self.entries[entry_idx].body = committed[0].2;
            self.entries[entry_idx].collider = committed[0].3[0].collider;
            self.entries[entry_idx].mass_properties = Some(committed[0].1);
        }
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("validated assembly");
        assembly
            .parts
            .retain(|part| part.component_id != snapshot.token.component_key);
        let mut outcomes = Vec::with_capacity(committed.len());
        for (component_id, mass, _, parts) in committed {
            assembly.parts.extend(parts);
            outcomes.push((component_id, f64::from(mass.mass_kg)));
        }
        assembly.fracture_owner_key = Some(snapshot.token.corpse_key);
        assembly.next_component_serial = next_serial;
        assembly.dynamic_corpse = true;
        assembly.kinematic_part_bodies = false;
        assembly.source_generation = next_source_generation;
        assembly.topology_token = next_topology_revision;
        self.fracture_owner_keys
            .insert(snapshot.token.corpse_key, body_id);
        self.ccd_solver = CCDSolver::new();
        Ok(FractureCommitOutcome {
            elapsed: started.elapsed(),
            source_cells: plan.source_cells.len(),
            components: outcomes,
        })
    }
}
